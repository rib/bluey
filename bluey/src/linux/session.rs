use core::ops::Deref;

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::convert::TryInto;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::RwLock as StdRwLock;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use futures::{pin_mut, stream, Stream, StreamExt};

use log::{debug, error, info, trace, warn};
use tokio::sync::Mutex;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::BroadcastStream;

use uuid::Uuid;

use crate::characteristic::{CharacteristicProperties, WriteType};
use crate::session::{BackendSession, Filter, SessionConfig};
use crate::{
    linux, try_u64_from_mac48_str, Address, BackendEvent, BackendPeripheralProperty,
    CharacteristicHandle, DescriptorHandle, Error, GattError, PeripheralHandle, Result,
    ServiceHandle, MAC,
};
use anyhow::anyhow;
use bluer::{Adapter, AdapterEvent, Device, DeviceEvent};

impl From<bluer::Error> for Error {
    fn from(value: bluer::Error) -> Self {
        match value.kind {
            bluer::ErrorKind::AlreadyConnected => Error::InvalidStateReference,
            bluer::ErrorKind::NotReady => Error::PeripheralUnreachable,
            bluer::ErrorKind::NotAuthorized => Error::PeripheralAccessDenied,
            bluer::ErrorKind::NotPermitted => {
                Error::PeripheralGattProtocolError(GattError::WriteNotPermitted)
            }
            bluer::ErrorKind::NotSupported => Error::Unsupported,
            _ => Error::Other(anyhow::anyhow!(value)),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LinuxSession {
    inner: Arc<LinuxSessionInner>,
}

#[derive(Debug)]
pub(crate) struct LinuxSessionInner {
    backend_bus: mpsc::UnboundedSender<BackendEvent>,
    session: bluer::Session,
    adapter: Adapter,
    device_scanner_task: Mutex<Option<(JoinHandle<()>, watch::Sender<()>)>>,
    peripherals_by_mac: DashMap<u64, PeripheralHandle>,
    peripherals_by_handle: DashMap<PeripheralHandle, LinuxPeripheral>,
    next_handle: AtomicU32,
}

#[derive(Clone, Debug)]
struct LinuxPeripheral {
    address: bluer::Address,
    peripheral_handle: PeripheralHandle,
    device: Device,
    inner: Arc<StdRwLock<LinuxPeripheralInner>>,
}

#[derive(Debug)]
struct LinuxPeripheralInner {
    gatt_services: DashMap<ServiceHandle, bluer::gatt::remote::Service>,
    gatt_characteristics: DashMap<CharacteristicHandle, bluer::gatt::remote::Characteristic>,
    gatt_descriptors: DashMap<DescriptorHandle, bluer::gatt::remote::Descriptor>,
}

impl Deref for LinuxSession {
    type Target = Arc<LinuxSessionInner>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl LinuxSession {
    pub async fn new(
        config: &SessionConfig<'_>, backend_bus: mpsc::UnboundedSender<BackendEvent>,
    ) -> Result<Self> {
        let session = bluer::Session::new().await?;
        let adapter = session.default_adapter().await?;
        adapter.set_powered(true).await?;

        Ok(LinuxSession {
            inner: Arc::new(LinuxSessionInner {
                backend_bus,
                session,
                adapter,
                device_scanner_task: Mutex::new(None),
                peripherals_by_mac: DashMap::new(),
                peripherals_by_handle: DashMap::new(),
                next_handle: AtomicU32::new(1),
            }),
        })
    }

    fn peripheral_from_mac(&self, address: u64) -> Result<PeripheralHandle> {
        match self.inner.peripherals_by_mac.get(&address) {
            None => {
                let mut addr_bytes = [0u8; 6];
                addr_bytes.copy_from_slice(&address.to_le_bytes()[0..6]);
                let bluer_addr = bluer::Address::new(addr_bytes);

                let device = self.inner.adapter.device(bluer_addr)?;
                let backend_bus = self.inner.backend_bus.clone();

                let peripheral_id = self.inner.next_handle.fetch_add(1, Ordering::SeqCst);
                let peripheral_handle = PeripheralHandle(peripheral_id);

                let linux_peripheral = LinuxPeripheral {
                    address: bluer_addr,
                    peripheral_handle,
                    device,
                    inner: Arc::new(StdRwLock::new(LinuxPeripheralInner {
                        gatt_services: DashMap::new(),
                        gatt_characteristics: DashMap::new(),
                        gatt_descriptors: DashMap::new(),
                    })),
                };
                self.inner
                    .peripherals_by_mac
                    .insert(address, peripheral_handle);
                self.inner
                    .peripherals_by_handle
                    .insert(peripheral_handle, linux_peripheral);

                let _ = backend_bus.send(BackendEvent::PeripheralFound { peripheral_handle });

                let _ = backend_bus.send(BackendEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: BackendPeripheralProperty::Address(Address::MAC(MAC(address))),
                });

                Ok(peripheral_handle)
            }
            Some(peripheral_handle) => Ok(*peripheral_handle.value()),
        }
    }

    fn linux_peripheral_from_handle(
        &self, peripheral_handle: PeripheralHandle,
    ) -> Result<LinuxPeripheral> {
        match self.inner.peripherals_by_handle.get(&peripheral_handle) {
            Some(p) => Ok(p.clone()),
            None => {
                log::error!(
                    "Spurious request with unknown peripheral handle {:?}",
                    peripheral_handle
                );
                Err(Error::InvalidStateReference)
            }
        }
    }

    async fn update_all_properties(
        &self, peripheral_handle: PeripheralHandle, device: &Device,
    ) -> Result<()> {
        let backend_bus = self.inner.backend_bus.clone();

        if let Ok(Some(name)) = device.name().await {
            let _ = backend_bus.send(BackendEvent::PeripheralPropertySet {
                peripheral_handle,
                property: BackendPeripheralProperty::Name(name),
            });
        }
        if let Ok(Some(rssi)) = device.rssi().await {
            let _ = backend_bus.send(BackendEvent::PeripheralPropertySet {
                peripheral_handle,
                property: BackendPeripheralProperty::Rssi(rssi),
            });
        }
        if let Ok(Some(tx_power)) = device.tx_power().await {
            let _ = backend_bus.send(BackendEvent::PeripheralPropertySet {
                peripheral_handle,
                property: BackendPeripheralProperty::TxPower(tx_power),
            });
        }
        if let Ok(Some(uuids)) = device.uuids().await {
            let _ = backend_bus.send(BackendEvent::PeripheralPropertySet {
                peripheral_handle,
                property: BackendPeripheralProperty::ServiceIds(uuids.into_iter().collect()),
            });
        }
        if let Ok(Some(manufacturer_data)) = device.manufacturer_data().await {
            let _ = backend_bus.send(BackendEvent::PeripheralPropertySet {
                peripheral_handle,
                property: BackendPeripheralProperty::ManufacturerData(manufacturer_data),
            });
        }
        if let Ok(Some(service_data)) = device.service_data().await {
            let _ = backend_bus.send(BackendEvent::PeripheralPropertySet {
                peripheral_handle,
                property: BackendPeripheralProperty::ServiceData(service_data),
            });
        }

        Ok(())
    }
}

#[async_trait]
impl BackendSession for LinuxSession {
    async fn start_scanning(&self, filter: &Filter) -> Result<()> {
        let mut guard = self.device_scanner_task.lock().await;
        if guard.is_some() {
            return Err(Error::Other(anyhow!("Scanning already in progress")));
        }

        let bluer_filter = bluer::DiscoveryFilter {
            uuids: filter.service_uuids.clone(),
            ..Default::default()
        };
        self.inner
            .adapter
            .set_discovery_filter(bluer_filter)
            .await?;

        let (tx, mut rx) = watch::channel(());

        let session = self.clone();
        log::debug!("Starting device scanner task");
        let task = tokio::spawn(async move {
            let mut back_off = 1u64;

            loop {
                tokio::select! {
                    _ = rx.changed() => {
                        debug!("Scan task cancelled");
                        break;
                    }
                    res = session.adapter.discover_devices_with_changes() => {
                        match res {
                            Ok(mut discovery_stream) => {
                                back_off = 1;
                                loop {
                                    tokio::select! {
                                        _ = rx.changed() => {
                                            debug!("Scan task cancelled during discovery");
                                            return;
                                        }
                                        event = discovery_stream.next() => {
                                            match event {
                                                Some(AdapterEvent::DeviceAdded(addr)) => {
                                                    log::debug!("Device added: {}", addr);
                                                    let mut addr_bytes = [0u8; 8];
                                                    addr_bytes[0..6].copy_from_slice(&addr.0);
                                                    let mac = u64::from_le_bytes(addr_bytes);
                                                    if let Ok(ph) = session.peripheral_from_mac(mac) {
                                                        if let Ok(device) = session.adapter.device(addr) {
                                                            let session_clone = session.clone();
                                                            tokio::spawn(async move {
                                                                if let Err(e) = session_clone.update_all_properties(ph, &device).await {
                                                                    warn!("Failed to update properties for device {}: {}", addr, e);
                                                                }
                                                            });
                                                        }
                                                    }
                                                },
                                                Some(AdapterEvent::DeviceRemoved(addr)) => {
                                                    log::debug!("Device removed: {} - TODO", addr);
                                                    // TODO
                                                },
                                                Some(AdapterEvent::PropertyChanged(prop)) => {
                                                    log::debug!("Property changed: {:?} - TODO", prop);
                                                    // TODO
                                                }
                                                None => {
                                                    debug!("Discovery stream ended");
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                            },
                            Err(err) => {
                                error!("Failed to start discovery: {}", err);
                                tokio::time::sleep(Duration::from_secs(back_off)).await;
                                back_off = u64::min(back_off * 2, 16);
                            }
                        }
                    }
                }
            }
        });

        *guard = Some((task, tx));
        Ok(())
    }

    async fn stop_scanning(&self) -> Result<()> {
        let mut guard = self.device_scanner_task.lock().await;
        if let Some((task, cancel)) = guard.take() {
            let _ = cancel.send(());
            task.await.ok();
        }
        Ok(())
    }

    fn declare_peripheral(&self, address: Address, name: String) -> Result<PeripheralHandle> {
        let mac = match address {
            Address::MAC(MAC(mac)) => mac,
            Address::String(address_str) => match try_u64_from_mac48_str(&address_str) {
                Some(mac) => mac,
                None => {
                    return Err(Error::Other(anyhow!(
                        "Unsupported device address format: {}",
                        address_str
                    )))
                }
            },
            _ => {
                return Err(Error::Other(anyhow!(
                    "Unsupported device address format: {:?}",
                    address
                )))
            }
        };

        let peripheral_handle = self.peripheral_from_mac(mac)?;
        let _ = self
            .inner
            .backend_bus
            .send(BackendEvent::PeripheralPropertySet {
                peripheral_handle,
                property: BackendPeripheralProperty::Name(name.to_string()),
            });

        Ok(peripheral_handle)
    }

    async fn peripheral_connect(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        let peripheral = self.linux_peripheral_from_handle(peripheral_handle)?;
        peripheral.device.connect().await?;
        let _ = self
            .backend_bus
            .send(BackendEvent::PeripheralConnected { peripheral_handle });
        Ok(())
    }
    async fn peripheral_disconnect(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        let peripheral = self.linux_peripheral_from_handle(peripheral_handle)?;
        peripheral.device.disconnect().await?;
        let _ = self.backend_bus.send(BackendEvent::PeripheralDisconnected {
            peripheral_handle,
            error: None,
        });
        Ok(())
    }
    fn peripheral_drop_gatt_state(&self, peripheral_handle: PeripheralHandle) {
        // TODO
    }

    async fn peripheral_read_rssi(&self, peripheral_handle: PeripheralHandle) -> Result<i16> {
        let peripheral = self.linux_peripheral_from_handle(peripheral_handle)?;
        Ok(peripheral.device.rssi().await?.unwrap_or(0))
    }

    async fn peripheral_discover_gatt_services(
        &self, peripheral_handle: PeripheralHandle, of_interest_hint: Option<Vec<Uuid>>,
    ) -> Result<()> {
        let peripheral = self.linux_peripheral_from_handle(peripheral_handle)?;
        let services = peripheral.device.services().await?;
        for service in services {
            let uuid = service.uuid().await?;
            let service_handle = ServiceHandle(service.id() as u32);
            peripheral
                .inner
                .read()
                .unwrap()
                .gatt_services
                .insert(service_handle, service);
            let _ = self.backend_bus.send(BackendEvent::GattService {
                peripheral_handle,
                service_handle,
                uuid,
            });
        }
        let _ = self.backend_bus.send(BackendEvent::GattServicesComplete {
            peripheral_handle,
            error: None,
        });
        Ok(())
    }
    async fn gatt_service_discover_includes(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
    ) -> Result<()> {
        // bluer discovers included services along with primary services.
        // This is a no-op.
        let _ = self
            .backend_bus
            .send(BackendEvent::GattIncludedServicesComplete {
                peripheral_handle,
                service_handle,
                error: None,
            });
        Ok(())
    }
    async fn gatt_service_discover_characteristics(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
    ) -> Result<()> {
        let peripheral = self.linux_peripheral_from_handle(peripheral_handle)?;
        log::debug!(
            "Discovering characteristics for service {:?} on peripheral {:?}",
            service_handle,
            peripheral_handle
        );
        let service = peripheral
            .inner
            .read()
            .unwrap()
            .gatt_services
            .get(&service_handle)
            .ok_or(Error::InvalidStateReference)?
            .clone();
        log::debug!("Service: {:?}", service);

        let characteristics = service.characteristics().await?;
        for char in characteristics {
            let uuid = char.uuid().await?;
            let properties = char.flags().await?.into();
            let characteristic_handle = CharacteristicHandle(char.id() as u32);
            peripheral
                .inner
                .read()
                .unwrap()
                .gatt_characteristics
                .insert(characteristic_handle, char);
            let _ = self.backend_bus.send(BackendEvent::GattCharacteristic {
                peripheral_handle,
                service_handle,
                characteristic_handle,
                uuid,
                properties,
            });
        }
        let _ = self
            .backend_bus
            .send(BackendEvent::GattCharacteristicsComplete {
                peripheral_handle,
                service_handle,
                error: None,
            });
        Ok(())
    }

    async fn gatt_characteristic_read(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, cache_mode: crate::CacheMode,
    ) -> Result<Vec<u8>> {
        let peripheral = self.linux_peripheral_from_handle(peripheral_handle)?;
        let characteristic = peripheral
            .inner
            .read()
            .unwrap()
            .gatt_characteristics
            .get(&characteristic_handle)
            .ok_or(Error::InvalidStateReference)?
            .clone();
        Ok(characteristic.read().await?)
    }

    async fn gatt_characteristic_write(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, write_type: crate::characteristic::WriteType,
        data: &[u8],
    ) -> Result<()> {
        let peripheral = self.linux_peripheral_from_handle(peripheral_handle)?;
        let characteristic = peripheral
            .inner
            .read()
            .unwrap()
            .gatt_characteristics
            .get(&characteristic_handle)
            .ok_or(Error::InvalidStateReference)?
            .clone();
        let write_req = bluer::gatt::remote::CharacteristicWriteRequest {
            op_type: match write_type {
                WriteType::WithResponse => bluer::gatt::WriteOp::Request,
                WriteType::WithoutResponse => bluer::gatt::WriteOp::Command,
            },
            ..Default::default()
        };
        characteristic.write_ext(data, &write_req).await?;
        Ok(())
    }

    async fn gatt_characteristic_subscribe(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
    ) -> Result<()> {
        let peripheral = self.linux_peripheral_from_handle(peripheral_handle)?;
        let characteristic = peripheral
            .inner
            .read()
            .unwrap()
            .gatt_characteristics
            .get(&characteristic_handle)
            .ok_or(Error::InvalidStateReference)?
            .clone();
        let notify_stream = characteristic.notify().await?;
        let backend_bus = self.backend_bus.clone();
        tokio::spawn(async move {
            pin_mut!(notify_stream);
            while let Some(value) = notify_stream.next().await {
                let _ = backend_bus.send(BackendEvent::GattCharacteristicNotify {
                    peripheral_handle,
                    service_handle,
                    characteristic_handle,
                    value,
                });
            }
        });
        Ok(())
    }

    async fn gatt_characteristic_unsubscribe(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
    ) -> Result<()> {
        // Dropping the stream from notify() stops notifications.
        // We need a way to manage the lifetime of the notification stream task.
        // For now, this is a no-op.
        Ok(())
    }

    async fn gatt_characteristic_discover_descriptors(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
    ) -> Result<()> {
        let peripheral = self.linux_peripheral_from_handle(peripheral_handle)?;
        let characteristic = peripheral
            .inner
            .read()
            .unwrap()
            .gatt_characteristics
            .get(&characteristic_handle)
            .ok_or(Error::InvalidStateReference)?
            .clone();
        let descriptors = characteristic.descriptors().await?;
        for desc in descriptors {
            let uuid = desc.uuid().await?;
            let descriptor_handle = DescriptorHandle(desc.id() as u32);
            peripheral
                .inner
                .read()
                .unwrap()
                .gatt_descriptors
                .insert(descriptor_handle, desc);
            let _ = self.backend_bus.send(BackendEvent::GattDescriptor {
                peripheral_handle,
                service_handle,
                characteristic_handle,
                descriptor_handle,
                uuid,
            });
        }
        let _ = self
            .backend_bus
            .send(BackendEvent::GattDescriptorsComplete {
                peripheral_handle,
                service_handle,
                characteristic_handle,
                error: None,
            });
        Ok(())
    }

    async fn gatt_descriptor_read(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, descriptor_handle: DescriptorHandle,
        cache_mode: crate::CacheMode,
    ) -> Result<Vec<u8>> {
        let peripheral = self.linux_peripheral_from_handle(peripheral_handle)?;
        let descriptor = peripheral
            .inner
            .read()
            .unwrap()
            .gatt_descriptors
            .get(&descriptor_handle)
            .ok_or(Error::InvalidStateReference)?
            .clone();
        Ok(descriptor.read().await?)
    }
    async fn gatt_descriptor_write(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, descriptor_handle: DescriptorHandle,
        data: &[u8],
    ) -> Result<()> {
        let peripheral = self.linux_peripheral_from_handle(peripheral_handle)?;
        let descriptor = peripheral
            .inner
            .read()
            .unwrap()
            .gatt_descriptors
            .get(&descriptor_handle)
            .ok_or(Error::InvalidStateReference)?
            .clone();
        descriptor.write(data).await?;
        Ok(())
    }

    fn flush(&self, id: u32) -> Result<()> {
        let _ = self.backend_bus.send(BackendEvent::Flush(id));
        Ok(())
    }
}

impl From<bluer::gatt::CharacteristicFlags> for CharacteristicProperties {
    fn from(flags: bluer::gatt::CharacteristicFlags) -> Self {
        let mut props = CharacteristicProperties::NONE;
        if flags.broadcast {
            props |= CharacteristicProperties::BROADCAST;
        }
        if flags.read {
            props |= CharacteristicProperties::READ;
        }
        if flags.write_without_response {
            props |= CharacteristicProperties::WRITE_WITHOUT_RESPONSE;
        }
        if flags.write {
            props |= CharacteristicProperties::WRITE;
        }
        if flags.notify {
            props |= CharacteristicProperties::NOTIFY;
        }
        if flags.indicate {
            props |= CharacteristicProperties::INDICATE;
        }
        if flags.authenticated_signed_writes {
            props |= CharacteristicProperties::AUTHENTICATED_SIGNED_WRITES;
        }
        if flags.extended_properties {
            props |= CharacteristicProperties::EXTENDED_PROPERTIES;
        }
        if flags.reliable_write {
            props |= CharacteristicProperties::RELIABLE_WRITES;
        }
        if flags.writable_auxiliaries {
            props |= CharacteristicProperties::WRITABLE_AUXILIARIES;
        }
        props
    }
}
