use async_trait::async_trait;
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::{Rc, Weak};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, LazyLock, RwLock,
};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, extern_class, msg_send, AnyThread, ClassType, DeclaredClass, Message};
use std::ptr;

use crate::corebluetooth::advertisement::{
    AdvertisementDataKey, AdvertisementDataParser, AdvertisementValue,
};
use crate::corebluetooth::bridge::{event_bridge_task, BridgeRequest};
use crate::corebluetooth::dispatch::{
    BluetoothDispatchState, CoreBluetoothDelegates, InternalEvent,
};
use crate::corebluetooth::dispatch_specific::{DispatchExt, DispatchSpecific};
use crate::corebluetooth::uuid_ext::UuidExt;
use crate::uuid::BluetoothUuid;

use objc2_core_bluetooth::{
    CBCentralManager, CBCentralManagerDelegate, CBCharacteristic, CBManagerState, CBPeripheral,
    CBPeripheralDelegate, CBService, CBUUID,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSData, NSDictionary, NSNumber, NSObject, NSObjectProtocol, NSString,
};

use crate::characteristic::WriteType;
use crate::session::{BackendSession, Filter};
use crate::{
    Address, AddressType, BackendEvent, BackendPeripheralProperty, CacheMode, CharacteristicHandle,
    DescriptorHandle, Error, PeripheralHandle, Result, ServiceHandle, State,
};
use anyhow;

#[derive(Debug)]
pub struct CoreBluetoothSession {
    queue: DispatchRetained<DispatchQueue>,
    state_rx: watch::Receiver<CBManagerState>,
    bridge_request_tx: mpsc::UnboundedSender<BridgeRequest>,
}

impl CoreBluetoothSession {
    pub fn new(backend_bus: mpsc::UnboundedSender<BackendEvent>) -> Self {
        // Create dedicated queue for Core Bluetooth operations
        //
        // Considering that the Core Bluetooth framework is not thread-safe,
        // all interactions with it must be serialized. Therefore,
        // we create a dedicated serial queue for all Core Bluetooth operations.
        let queue = DispatchQueue::new("co.bluey.corebluetooth", None);

        // Create internal event channel, bridge request channel, and state watcher
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let (bridge_request_tx, bridge_request_rx) = mpsc::unbounded_channel();
        let (state_tx, state_rx) = watch::channel(CBManagerState::Unknown);

        // Spawn the event bridge task
        let queue_clone_for_bridge = queue.clone();
        tokio::spawn(async move {
            // For now, just handle internal events. We'll add bridge request handling later.
            event_bridge_task(internal_rx, backend_bus, state_tx).await;
        });

        log::info!("Initializing CoreBluetooth dispatch queue state");

        // Hoist the initialization code so it doesn't all get marked as `unsafe {}` when
        // calling `::attach()`
        let queue_clone = queue.clone();
        let state_init = move || {
            // Create delegate on the CoreBluetooth queue
            let delegate: Retained<CoreBluetoothDelegates> =
                unsafe { msg_send![CoreBluetoothDelegates::alloc(), init] };

            // Create central manager with delegate on current queue
            let central_manager = unsafe {
                CBCentralManager::initWithDelegate_queue_options(
                    CBCentralManager::alloc(),
                    Some(ProtocolObject::from_ref(&*delegate)),
                    Some(&queue_clone),
                    None, // No options
                )
            };
            BluetoothDispatchState::new(internal_tx, central_manager, delegate)
        };

        // Initialize and attach the dispatch-specific state
        //
        // Safety:
        // This is a SERIAL queue and this is the first time we're attaching this state
        let attached =
            unsafe { DispatchSpecific::<BluetoothDispatchState>::attach(&queue, state_init) };
        if !attached {
            panic!("Failed to attach BluetoothDispatchState to queue, which should never happen for a newly created queue");
        }

        Self {
            queue,
            state_rx,
            bridge_request_tx,
        }
    }
}

#[async_trait]
impl BackendSession for CoreBluetoothSession {
    fn supports_scanning(&self) -> bool {
        true
    }

    fn supports_select_peripheral(&self) -> bool {
        false
    }

    fn supports_declare_peripheral(&self) -> bool {
        false
    }

    fn has_scan_permission(&self) -> bool {
        true
    }

    async fn start_scanning(&self, filter: &Filter) -> Result<()> {
        let filter = filter.clone();

        // Wait for the central manager to be in PoweredOn state
        let mut state_rx = self.state_rx.clone();
        let timeout = tokio::time::timeout(
            Duration::from_secs(10),
            state_rx.wait_for(|state| *state == CBManagerState::PoweredOn),
        );

        match timeout.await {
            Ok(Ok(_)) => {
                log::debug!("Central manager is powered on, starting scan...");
            }
            Ok(Err(_)) => {
                return Err(Error::Other(anyhow::anyhow!("State watcher was dropped")));
            }
            Err(_) => {
                return Err(Error::Other(anyhow::anyhow!(
                    "Timeout waiting for Bluetooth to power on"
                )));
            }
        }

        self.queue
            .safe_exec_async(move || -> Result<()> {
                DispatchSpecific::<BluetoothDispatchState>::with_mut(|state| {
                    state.handle_start_scanning(filter)
                })
            })
            .await?;
        Ok(())
    }

    async fn stop_scanning(&self) -> Result<()> {
        self.queue
            .safe_exec_async(move || -> Result<()> {
                DispatchSpecific::<BluetoothDispatchState>::with_mut(|state| {
                    state.handle_stop_scanning()
                })
            })
            .await?;
        Ok(())
    }

    async fn select_peripheral(&self, _filter: &Filter) -> Result<PeripheralHandle> {
        Err(Error::Unsupported)
    }

    fn declare_peripheral(&self, _address: Address, _name: String) -> Result<PeripheralHandle> {
        Err(Error::Unsupported)
    }

    async fn peripheral_connect(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();

        self.queue
            .safe_exec_async(move || -> Result<()> {
                DispatchSpecific::<BluetoothDispatchState>::with_mut(|state| {
                    state.handle_connect(peripheral_handle, response_tx)
                })
            })
            .await?;

        response_rx
            .await
            .map_err(|_| Error::Other(anyhow::anyhow!("Connection request was cancelled")))?
    }

    async fn peripheral_disconnect(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();

        self.queue
            .safe_exec_async(move || -> Result<()> {
                DispatchSpecific::<BluetoothDispatchState>::with_mut(|state| {
                    state.handle_disconnect(peripheral_handle, response_tx)
                })
            })
            .await?;

        response_rx
            .await
            .map_err(|_| Error::Other(anyhow::anyhow!("Disconnection request was cancelled")))?
    }

    fn peripheral_drop_gatt_state(&self, _peripheral_handle: PeripheralHandle) {
        log::debug!("Dropped GATT state for peripheral {:?}", _peripheral_handle);
    }

    async fn peripheral_read_rssi(&self, peripheral_handle: PeripheralHandle) -> Result<i16> {
        let (response_tx, response_rx) = oneshot::channel();

        self.queue
            .safe_exec_async(move || -> Result<()> {
                DispatchSpecific::<BluetoothDispatchState>::with_mut(|state| {
                    state.handle_read_rssi(peripheral_handle, response_tx)
                })
            })
            .await?;

        response_rx
            .await
            .map_err(|_| Error::Other(anyhow::anyhow!("RSSI request was cancelled")))?
    }

    async fn peripheral_discover_gatt_services(
        &self, peripheral_handle: PeripheralHandle, of_interest_hint: Option<Vec<Uuid>>,
    ) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();

        self.queue
            .safe_exec_async(move || -> Result<()> {
                DispatchSpecific::<BluetoothDispatchState>::with_mut(|state| {
                    state.handle_discover_services(peripheral_handle, of_interest_hint, response_tx)
                })
            })
            .await?;

        response_rx
            .await
            .map_err(|_| Error::Other(anyhow::anyhow!("Service discovery request was cancelled")))?
    }

    async fn gatt_service_discover_characteristics(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
    ) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();

        self.queue
            .safe_exec_async(move || -> Result<()> {
                DispatchSpecific::<BluetoothDispatchState>::with_mut(|state| {
                    state.handle_discover_characteristics(
                        peripheral_handle,
                        service_handle,
                        response_tx,
                    )
                })
            })
            .await?;

        response_rx.await.map_err(|_| {
            Error::Other(anyhow::anyhow!(
                "Characteristic discovery request was cancelled"
            ))
        })?
    }

    async fn gatt_service_discover_includes(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
    ) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();

        self.queue
            .safe_exec_async(move || -> Result<()> {
                DispatchSpecific::<BluetoothDispatchState>::with_mut(|state| {
                    state.handle_discover_included_services(
                        peripheral_handle,
                        service_handle,
                        response_tx,
                    )
                })
            })
            .await?;

        response_rx.await.map_err(|_| {
            Error::Other(anyhow::anyhow!(
                "Included service discovery request was cancelled"
            ))
        })?
    }

    async fn gatt_characteristic_read(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, _cache_mode: CacheMode,
    ) -> Result<Vec<u8>> {
        let (response_tx, response_rx) = oneshot::channel();

        self.queue
            .safe_exec_async(move || -> Result<()> {
                DispatchSpecific::<BluetoothDispatchState>::with_mut(|state| {
                    state.handle_read_characteristic(
                        peripheral_handle,
                        service_handle,
                        characteristic_handle,
                        response_tx,
                    )
                })
            })
            .await?;

        response_rx.await.map_err(|_| {
            Error::Other(anyhow::anyhow!("Characteristic read request was cancelled"))
        })?
    }

    async fn gatt_characteristic_write(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, write_type: WriteType, data: &[u8],
    ) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        let data_vec = data.to_vec();

        self.queue
            .safe_exec_async(move || -> Result<()> {
                DispatchSpecific::<BluetoothDispatchState>::with_mut(|state| {
                    state.handle_write_characteristic(
                        peripheral_handle,
                        service_handle,
                        characteristic_handle,
                        data_vec,
                        write_type,
                        response_tx,
                    )
                })
            })
            .await?;

        response_rx.await.map_err(|_| {
            Error::Other(anyhow::anyhow!(
                "Characteristic write request was cancelled"
            ))
        })?
    }

    async fn gatt_characteristic_subscribe(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
    ) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();

        // Send the request directly to the dispatch queue
        self.queue
            .safe_exec_async(move || -> Result<()> {
                DispatchSpecific::<BluetoothDispatchState>::with_mut(|state| {
                    state.handle_subscribe_characteristic(
                        peripheral_handle,
                        service_handle,
                        characteristic_handle,
                        response_tx,
                    )
                })
            })
            .await?;

        // Await the response
        response_rx.await.map_err(|_| {
            Error::Other(anyhow::anyhow!(
                "Characteristic subscription request was cancelled"
            ))
        })?
    }

    async fn gatt_characteristic_unsubscribe(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
    ) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();

        self.queue
            .safe_exec_async(move || -> Result<()> {
                DispatchSpecific::<BluetoothDispatchState>::with_mut(|state| {
                    state.handle_unsubscribe_characteristic(
                        peripheral_handle,
                        service_handle,
                        characteristic_handle,
                        response_tx,
                    )
                })
            })
            .await?;

        response_rx.await.map_err(|_| {
            Error::Other(anyhow::anyhow!(
                "Characteristic unsubscription request was cancelled"
            ))
        })?
    }

    async fn gatt_characteristic_discover_descriptors(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
    ) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();

        self.queue
            .safe_exec_async(move || -> Result<()> {
                DispatchSpecific::<BluetoothDispatchState>::with_mut(|state| {
                    state.handle_discover_descriptors(
                        peripheral_handle,
                        service_handle,
                        characteristic_handle,
                        response_tx,
                    )
                })
            })
            .await?;

        response_rx.await.map_err(|_| {
            Error::Other(anyhow::anyhow!(
                "Descriptor discovery request was cancelled"
            ))
        })?
    }

    async fn gatt_descriptor_read(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, descriptor_handle: DescriptorHandle,
        _cache_mode: CacheMode,
    ) -> Result<Vec<u8>> {
        let (response_tx, response_rx) = oneshot::channel();

        self.queue
            .safe_exec_async(move || -> Result<()> {
                DispatchSpecific::<BluetoothDispatchState>::with_mut(|state| {
                    state.handle_read_descriptor(
                        peripheral_handle,
                        service_handle,
                        characteristic_handle,
                        descriptor_handle,
                        response_tx,
                    )
                })
            })
            .await?;

        response_rx
            .await
            .map_err(|_| Error::Other(anyhow::anyhow!("Descriptor read request was cancelled")))?
    }

    async fn gatt_descriptor_write(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, descriptor_handle: DescriptorHandle,
        data: &[u8],
    ) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        let data_vec = data.to_vec();

        self.queue
            .safe_exec_async(move || -> Result<()> {
                DispatchSpecific::<BluetoothDispatchState>::with_mut(|state| {
                    state.handle_write_descriptor(
                        peripheral_handle,
                        service_handle,
                        characteristic_handle,
                        descriptor_handle,
                        data_vec,
                        response_tx,
                    )
                })
            })
            .await?;

        response_rx
            .await
            .map_err(|_| Error::Other(anyhow::anyhow!("Descriptor write request was cancelled")))?
    }

    fn flush(&self, _id: u32) -> Result<()> {
        Ok(())
    }
}
