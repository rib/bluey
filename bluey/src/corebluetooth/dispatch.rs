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

// Internal events for communication between dispatch queue and async bridge task
#[derive(Debug)]
pub enum InternalEvent {
    StateChanged(CBManagerState),
    PeripheralDiscovered {
        identifier: String,
        name: Option<String>,
        rssi: i32,
        advertisement_data: HashMap<AdvertisementDataKey, AdvertisementValue>,
    },
    PeripheralConnected {
        identifier: String,
    },
    PeripheralDisconnected {
        identifier: String,
        error: Option<String>,
    },
    PeripheralConnectionFailed {
        identifier: String,
        error: Option<String>,
    },
    PeripheralRssiRead {
        identifier: String,
        rssi: i16,
        error: Option<String>,
    },
    ServiceDiscovered {
        identifier: String,
        service_uuid: Uuid,
    },
    ServicesDiscoveryComplete {
        identifier: String,
        error: Option<String>,
    },
    CharacteristicDiscovered {
        identifier: String,
        service_uuid: Uuid,
        characteristic_uuid: Uuid,
        properties: u32,
    },
    CharacteristicsDiscoveryComplete {
        identifier: String,
        service_uuid: Uuid,
        error: Option<String>,
    },
    IncludedServiceDiscovered {
        identifier: String,
        parent_service_uuid: Uuid,
        included_service_uuid: Uuid,
    },
    IncludedServicesDiscoveryComplete {
        identifier: String,
        service_uuid: Uuid,
        error: Option<String>,
    },
    CharacteristicRead {
        identifier: String,
        service_uuid: Uuid,
        characteristic_uuid: Uuid,
        data: Vec<u8>,
        error: Option<String>,
    },
    CharacteristicWriteComplete {
        identifier: String,
        service_uuid: Uuid,
        characteristic_uuid: Uuid,
        error: Option<String>,
    },
    CharacteristicSubscriptionChanged {
        identifier: String,
        service_uuid: Uuid,
        characteristic_uuid: Uuid,
        subscribed: bool,
        error: Option<String>,
    },
    DescriptorDiscovered {
        identifier: String,
        service_uuid: Uuid,
        characteristic_uuid: Uuid,
        descriptor_uuid: Uuid,
    },
    DescriptorsDiscoveryComplete {
        identifier: String,
        service_uuid: Uuid,
        characteristic_uuid: Uuid,
        error: Option<String>,
    },
    DescriptorRead {
        identifier: String,
        service_uuid: Uuid,
        characteristic_uuid: Uuid,
        descriptor_uuid: Uuid,
        data: Vec<u8>,
        error: Option<String>,
    },
    DescriptorWriteComplete {
        identifier: String,
        service_uuid: Uuid,
        characteristic_uuid: Uuid,
        descriptor_uuid: Uuid,
        error: Option<String>,
    },
}

// Core Bluetooth delegates (CBCentralManagerDelegate and CBPeripheralDelegate)
define_class!(
    #[derive(Debug)]
    #[unsafe(super(NSObject))]
    pub struct CoreBluetoothDelegates;

    unsafe impl CBCentralManagerDelegate for CoreBluetoothDelegates {
        #[unsafe(method(centralManagerDidUpdateState:))]
        fn central_manager_did_update_state(&self, central: &CBCentralManager) {
            let state = unsafe { central.state() };
            log::info!("CoreBluetooth central manager state updated: {:?}", state);

            // Send state change event to bridge task
            DispatchSpecific::<BluetoothDispatchState>::with(|dispatch_state| {
                if let Err(_) = dispatch_state
                    .internal_event_tx
                    .send(InternalEvent::StateChanged(state))
                {
                    log::warn!("Failed to send state change event to bridge task");
                }
            });
        }

        #[unsafe(method(centralManager:didDiscoverPeripheral:advertisementData:RSSI:))]
        fn central_manager_did_discover_peripheral(
            &self, _central: &CBCentralManager, peripheral: &CBPeripheral,
            advertisement_data: &NSDictionary, rssi: &NSNumber,
        ) {
            let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
            let name = unsafe { peripheral.name().map(|n| n.to_string()) };
            let rssi_value = unsafe { rssi.intValue() };

            let ad_data = AdvertisementDataParser::parse(advertisement_data);

            // Send peripheral discovered event to bridge task
            DispatchSpecific::<BluetoothDispatchState>::with_mut(|dispatch_state| {
                // Store the peripheral for later use (connection, RSSI reads, etc.)
                // Check if we already have this peripheral
                if !dispatch_state
                    .peripherals_by_identifier
                    .contains_key(&identifier)
                {
                    let handle = PeripheralHandle(
                        dispatch_state
                            .next_peripheral_handle
                            .fetch_add(1, Ordering::SeqCst),
                    );
                    // Store a retained reference to the peripheral
                    let peripheral_retained = peripheral.retain();
                    dispatch_state
                        .peripherals_by_handle
                        .insert(handle, peripheral_retained);
                    dispatch_state
                        .peripherals_by_identifier
                        .insert(identifier.clone(), handle);
                    log::debug!("Stored peripheral {} with handle {:?}", identifier, handle);
                }

                let event = InternalEvent::PeripheralDiscovered {
                    identifier,
                    name,
                    rssi: rssi_value,
                    advertisement_data: ad_data,
                };
                if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                    log::warn!("Failed to send peripheral discovered event to bridge task");
                }
            });
        }

        #[unsafe(method(centralManager:didConnectPeripheral:))]
        fn central_manager_did_connect_peripheral(
            &self, _central: &CBCentralManager, peripheral: &CBPeripheral,
        ) {
            let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
            log::info!("Connected to peripheral: {}", identifier);

            // Complete pending connect request and send connection event to bridge task
            DispatchSpecific::<BluetoothDispatchState>::with_mut(|dispatch_state| {
                // Complete pending connect request if exists
                if let Some(response_tx) =
                    dispatch_state.pending_connect_requests.remove(&identifier)
                {
                    let _ = response_tx.send(Ok(()));
                } else {
                    log::warn!("Received connection success for peripheral {} but no pending request found", identifier);
                }

                // Send connection event to bridge task
                let event = InternalEvent::PeripheralConnected { identifier };
                if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                    log::warn!("Failed to send peripheral connected event to bridge task");
                }
            });
        }

        #[unsafe(method(centralManager:didDisconnectPeripheral:error:))]
        fn central_manager_did_disconnect_peripheral(
            &self, _central: &CBCentralManager, peripheral: &CBPeripheral,
            error: Option<&objc2_foundation::NSError>,
        ) {
            let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
            let error_string = error.map(|e| e.localizedDescription().to_string());

            if let Some(ref err) = error_string {
                log::warn!(
                    "Disconnected from peripheral {} with error: {}",
                    identifier,
                    err
                );
            } else {
                log::info!("Disconnected from peripheral: {}", identifier);
            }

            // Complete pending disconnect request and send disconnection event to bridge task
            DispatchSpecific::<BluetoothDispatchState>::with_mut(|dispatch_state| {
                // Complete pending disconnect request if exists
                if let Some(response_tx) = dispatch_state
                    .pending_disconnect_requests
                    .remove(&identifier)
                {
                    let result = if error_string.is_some() {
                        // Note: In CoreBluetooth, disconnection with error might still be considered successful
                        // depending on whether it was initiated by us or the remote device
                        Ok(())
                    } else {
                        Ok(())
                    };
                    let _ = response_tx.send(result);
                } else {
                    log::debug!("Received disconnection for peripheral {} (no pending request, likely initiated by remote)", identifier);
                }

                // Send disconnection event to bridge task
                let event = InternalEvent::PeripheralDisconnected {
                    identifier,
                    error: error_string,
                };
                if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                    log::warn!("Failed to send peripheral disconnected event to bridge task");
                }
            });
        }

        #[unsafe(method(centralManager:didFailToConnectPeripheral:error:))]
        fn central_manager_did_fail_to_connect_peripheral(
            &self, _central: &CBCentralManager, peripheral: &CBPeripheral,
            error: Option<&objc2_foundation::NSError>,
        ) {
            let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
            let error_string = error.map(|e| e.localizedDescription().to_string());

            if let Some(ref err) = error_string {
                log::error!("Failed to connect to peripheral {}: {}", identifier, err);
            }

            // Complete pending connect request and send connection failed event to bridge task
            DispatchSpecific::<BluetoothDispatchState>::with_mut(|dispatch_state| {
                // Complete pending connect request if exists
                if let Some(response_tx) =
                    dispatch_state.pending_connect_requests.remove(&identifier)
                {
                    let result = if let Some(err) = &error_string {
                        Err(Error::Other(anyhow::anyhow!("Connection failed: {}", err)))
                    } else {
                        Err(Error::Other(anyhow::anyhow!(
                            "Connection failed: Unknown error"
                        )))
                    };
                    let _ = response_tx.send(result);
                } else {
                    log::warn!("Received connection failure for peripheral {} but no pending request found", identifier);
                }

                // Send connection failed event to bridge task
                let event = InternalEvent::PeripheralConnectionFailed {
                    identifier,
                    error: error_string,
                };
                if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                    log::warn!("Failed to send peripheral connection failed event to bridge task");
                }
            });
        }
    }

    unsafe impl CBPeripheralDelegate for CoreBluetoothDelegates {
        #[unsafe(method(peripheral:didReadRSSI:error:))]
        fn peripheral_did_read_rssi(
            &self, peripheral: &CBPeripheral, rssi: &NSNumber,
            error: Option<&objc2_foundation::NSError>,
        ) {
            let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
            let rssi_value = unsafe { rssi.intValue() } as i16;
            let error_string = error.map(|e| e.localizedDescription().to_string());

            if let Some(ref err) = error_string {
                log::warn!("RSSI read failed for peripheral {}: {}", identifier, err);
            } else {
                log::debug!(
                    "RSSI read successful for peripheral {}: {}",
                    identifier,
                    rssi_value
                );
            }

            // Complete pending RSSI request and optionally send event to bridge task
            DispatchSpecific::<BluetoothDispatchState>::with_mut(|dispatch_state| {
                // Complete pending request if exists
                if let Some(response_tx) = dispatch_state.pending_rssi_requests.remove(&identifier)
                {
                    let result = if let Some(err) = &error_string {
                        Err(Error::Other(anyhow::anyhow!("RSSI read failed: {}", err)))
                    } else {
                        Ok(rssi_value)
                    };
                    let _ = response_tx.send(result);
                } else {
                    log::warn!(
                        "Received RSSI response for peripheral {} but no pending request found",
                        identifier
                    );
                }

                // Optionally send event to bridge task for logging
                let event = InternalEvent::PeripheralRssiRead {
                    identifier,
                    rssi: rssi_value,
                    error: error_string,
                };
                if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                    log::warn!("Failed to send peripheral RSSI read event to bridge task");
                }
            });
        }

        #[unsafe(method(peripheral:didDiscoverServices:))]
        fn peripheral_did_discover_services(
            &self, peripheral: &CBPeripheral, error: Option<&objc2_foundation::NSError>,
        ) {
            let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
            let error_string = error.map(|e| e.localizedDescription().to_string());

            if let Some(ref err) = error_string {
                log::warn!(
                    "Service discovery failed for peripheral {}: {}",
                    identifier,
                    err
                );
            } else {
                log::debug!("Service discovery completed for peripheral {}", identifier);
            }

            // Complete pending service discovery request and optionally send event to bridge task
            DispatchSpecific::<BluetoothDispatchState>::with_mut(|dispatch_state| {
                // Complete pending request if exists
                if let Some(response_tx) = dispatch_state
                    .pending_service_discovery_requests
                    .remove(&identifier)
                {
                    let result = if let Some(err) = &error_string {
                        Err(Error::Other(anyhow::anyhow!(
                            "Service discovery failed: {}",
                            err
                        )))
                    } else {
                        Ok(())
                    };
                    let _ = response_tx.send(result);
                } else {
                    log::warn!("Received service discovery response for peripheral {} but no pending request found", identifier);
                }

                // Send discovered services to backend
                if error_string.is_none() {
                    if let Some(services) = unsafe { peripheral.services() } {
                        for i in 0..services.count() {
                            let service = services.objectAtIndex(i);
                            if let Some(cb_service) = service.downcast_ref::<CBService>() {
                                let service_uuid = Uuid::from_cbuuid(unsafe { &cb_service.UUID() });
                                {
                                    // Generate service handle using UUID hash for consistency
                                    let mut service_hasher = DefaultHasher::new();
                                    service_uuid.hash(&mut service_hasher);
                                    let service_handle =
                                        ServiceHandle(service_hasher.finish() as u32);

                                    // Store service by handle for characteristic discovery
                                    dispatch_state
                                        .services_by_handle
                                        .insert(service_handle, cb_service.retain());

                                    // Send service discovery event to bridge task
                                    let event = InternalEvent::ServiceDiscovered {
                                        identifier: identifier.clone(),
                                        service_uuid,
                                    };
                                    if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                                        log::warn!("Failed to send service discovered event to bridge task");
                                    }
                                }
                            }
                        }
                    }

                    // Send services discovery complete event
                    let event = InternalEvent::ServicesDiscoveryComplete {
                        identifier,
                        error: None,
                    };
                    if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                        log::warn!(
                            "Failed to send services discovery complete event to bridge task"
                        );
                    }
                } else {
                    // Send services discovery complete with error
                    let event = InternalEvent::ServicesDiscoveryComplete {
                        identifier,
                        error: error_string,
                    };
                    if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                        log::warn!(
                            "Failed to send services discovery complete event to bridge task"
                        );
                    }
                }
            });
        }

        #[unsafe(method(peripheral:didDiscoverCharacteristicsForService:error:))]
        fn peripheral_did_discover_characteristics_for_service(
            &self, peripheral: &CBPeripheral, service: &CBService,
            error: Option<&objc2_foundation::NSError>,
        ) {
            let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
            let service_uuid = Uuid::from_cbuuid(unsafe { &service.UUID() });
            let error_string = error.map(|e| e.localizedDescription().to_string());

            if let Some(ref err) = error_string {
                log::warn!(
                    "Characteristic discovery failed for peripheral {} service {}: {}",
                    identifier,
                    service_uuid,
                    err
                );
            } else {
                log::debug!(
                    "Characteristic discovery completed for peripheral {} service {}",
                    identifier,
                    service_uuid
                );
            }

            // Complete pending characteristic discovery request and send events to bridge task
            DispatchSpecific::<BluetoothDispatchState>::with_mut(|dispatch_state| {
                // Complete pending request if exists
                let request_key = format!("{}:{}", identifier, service_uuid);
                if let Some(response_tx) = dispatch_state
                    .pending_characteristic_discovery_requests
                    .remove(&request_key)
                {
                    let result = if let Some(err) = &error_string {
                        Err(Error::Other(anyhow::anyhow!(
                            "Characteristic discovery failed: {}",
                            err
                        )))
                    } else {
                        Ok(())
                    };
                    let _ = response_tx.send(result);
                } else {
                    log::warn!("Received characteristic discovery response for peripheral {} service {} but no pending request found", identifier, service_uuid);
                }

                // Send discovered characteristics to backend
                if error_string.is_none() {
                    if let Some(characteristics) = unsafe { service.characteristics() } {
                        for i in 0..characteristics.count() {
                            let characteristic = characteristics.objectAtIndex(i);
                            if let Some(cb_characteristic) =
                                characteristic.downcast_ref::<CBCharacteristic>()
                            {
                                let characteristic_uuid =
                                    Uuid::from_cbuuid(unsafe { &cb_characteristic.UUID() });
                                {
                                    // Get characteristic properties and convert to u32
                                    let cb_properties = unsafe { cb_characteristic.properties() };
                                    let properties = cb_properties.0 as u32; // Convert to u32

                                    // Generate characteristic handle using UUID hash for consistency
                                    let mut characteristic_hasher =
                                        std::collections::hash_map::DefaultHasher::new();
                                    characteristic_uuid.hash(&mut characteristic_hasher);
                                    let characteristic_handle =
                                        CharacteristicHandle(characteristic_hasher.finish() as u32);

                                    // Store the characteristic by handle for later read/write operations
                                    dispatch_state
                                        .characteristics_by_handle
                                        .insert(characteristic_handle, cb_characteristic.retain());

                                    // Send characteristic discovery event to bridge task
                                    let event = InternalEvent::CharacteristicDiscovered {
                                        identifier: identifier.clone(),
                                        service_uuid,
                                        characteristic_uuid,
                                        properties,
                                    };
                                    if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                                        log::warn!("Failed to send characteristic discovered event to bridge task");
                                    }
                                }
                            }
                        }
                    }
                }

                // Send characteristics discovery complete event
                let event = InternalEvent::CharacteristicsDiscoveryComplete {
                    identifier,
                    service_uuid,
                    error: error_string,
                };
                if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                    log::warn!(
                        "Failed to send characteristics discovery complete event to bridge task"
                    );
                }
            });
        }

        #[unsafe(method(peripheral:didDiscoverIncludedServicesForService:error:))]
        fn peripheral_did_discover_included_services_for_service(
            &self, peripheral: &CBPeripheral, service: &CBService,
            error: Option<&objc2_foundation::NSError>,
        ) {
            let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
            let service_uuid = Uuid::from_cbuuid(unsafe { &service.UUID() });
            let error_string = error.map(|e| e.localizedDescription().to_string());

            if let Some(ref err) = error_string {
                log::warn!(
                    "Included service discovery failed for peripheral {} service {}: {}",
                    identifier,
                    service_uuid,
                    err
                );
            } else {
                log::debug!(
                    "Included service discovery completed for peripheral {} service {}",
                    identifier,
                    service_uuid
                );
            }

            // Complete pending included service discovery request and send events to bridge task
            DispatchSpecific::<BluetoothDispatchState>::with_mut(|dispatch_state| {
                // Complete pending request if exists
                let request_key = format!("{}:{}", identifier, service_uuid);
                if let Some(response_tx) = dispatch_state
                    .pending_included_service_discovery_requests
                    .remove(&request_key)
                {
                    let result = if let Some(err) = &error_string {
                        Err(Error::Other(anyhow::anyhow!(
                            "Included service discovery failed: {}",
                            err
                        )))
                    } else {
                        Ok(())
                    };
                    let _ = response_tx.send(result);
                } else {
                    log::warn!("Received included service discovery response for peripheral {} service {} but no pending request found", identifier, service_uuid);
                }

                // Send discovered included services to backend
                if error_string.is_none() {
                    if let Some(included_services) = unsafe { service.includedServices() } {
                        for i in 0..included_services.count() {
                            let included_service = included_services.objectAtIndex(i);
                            if let Some(cb_included_service) =
                                included_service.downcast_ref::<CBService>()
                            {
                                let included_service_uuid =
                                    Uuid::from_cbuuid(unsafe { &cb_included_service.UUID() });
                                {
                                    // Send included service discovery event to bridge task
                                    let event = InternalEvent::IncludedServiceDiscovered {
                                        identifier: identifier.clone(),
                                        parent_service_uuid: service_uuid,
                                        included_service_uuid,
                                    };
                                    if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                                        log::warn!("Failed to send included service discovered event to bridge task");
                                    }
                                }
                            }
                        }
                    }
                }

                // Send included services discovery complete event
                let event = InternalEvent::IncludedServicesDiscoveryComplete {
                    identifier,
                    service_uuid,
                    error: error_string,
                };
                if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                    log::warn!(
                        "Failed to send included services discovery complete event to bridge task"
                    );
                }
            });
        }

        #[unsafe(method(peripheral:didUpdateValueForCharacteristic:error:))]
        fn peripheral_did_update_value_for_characteristic(
            &self, peripheral: &CBPeripheral, characteristic: &CBCharacteristic,
            error: Option<&objc2_foundation::NSError>,
        ) {
            let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
            let characteristic_uuid = Uuid::from_cbuuid(unsafe { &characteristic.UUID() });
            let service_uuid = Uuid::from_cbuuid(unsafe {
                &characteristic
                    .service()
                    .expect("Characteristic must have a service")
                    .UUID()
            });
            let error_string = error.map(|e| e.localizedDescription().to_string());

            if let Some(ref err) = error_string {
                log::warn!(
                    "Characteristic read failed for peripheral {} service {} characteristic {}: {}",
                    identifier,
                    service_uuid,
                    characteristic_uuid,
                    err
                );
            } else {
                log::debug!(
                    "Characteristic read completed for peripheral {} service {} characteristic {}",
                    identifier,
                    service_uuid,
                    characteristic_uuid
                );
            }

            // Get characteristic data
            let data = if error_string.is_none() {
                if let Some(value) = unsafe { characteristic.value() } {
                    value.to_vec()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };

            // Complete pending characteristic read request and send event to bridge task
            DispatchSpecific::<BluetoothDispatchState>::with_mut(|dispatch_state| {
                // Complete pending request if exists
                let request_key =
                    format!("{}:{}:{}", identifier, service_uuid, characteristic_uuid);
                if let Some(response_tx) = dispatch_state
                    .pending_characteristic_read_requests
                    .remove(&request_key)
                {
                    let result = if let Some(err) = &error_string {
                        Err(Error::Other(anyhow::anyhow!(
                            "Characteristic read failed: {}",
                            err
                        )))
                    } else {
                        Ok(data.clone())
                    };
                    let _ = response_tx.send(result);
                } else {
                    log::debug!("Received characteristic read response for peripheral {} service {} characteristic {} but no pending request found (likely notification)", identifier, service_uuid, characteristic_uuid);
                }

                // Send characteristic read event to bridge task
                let event = InternalEvent::CharacteristicRead {
                    identifier,
                    service_uuid,
                    characteristic_uuid,
                    data,
                    error: error_string,
                };
                if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                    log::warn!("Failed to send characteristic read event to bridge task");
                }
            });
        }

        #[unsafe(method(peripheral:didWriteValueForCharacteristic:error:))]
        fn peripheral_did_write_value_for_characteristic(
            &self, peripheral: &CBPeripheral, characteristic: &CBCharacteristic,
            error: Option<&objc2_foundation::NSError>,
        ) {
            let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
            let characteristic_uuid = Uuid::from_cbuuid(unsafe { &characteristic.UUID() });
            let service_uuid = Uuid::from_cbuuid(unsafe {
                &characteristic
                    .service()
                    .expect("Characteristic must have a service")
                    .UUID()
            });
            let error_string = error.map(|e| e.localizedDescription().to_string());

            if let Some(ref err) = error_string {
                log::warn!("Characteristic write failed for peripheral {} service {} characteristic {}: {}", identifier, service_uuid, characteristic_uuid, err);
            } else {
                log::debug!(
                    "Characteristic write completed for peripheral {} service {} characteristic {}",
                    identifier,
                    service_uuid,
                    characteristic_uuid
                );
            }

            // Complete pending characteristic write request and send event to bridge task
            DispatchSpecific::<BluetoothDispatchState>::with_mut(|dispatch_state| {
                // Complete pending request if exists
                let request_key =
                    format!("{}:{}:{}", identifier, service_uuid, characteristic_uuid);
                if let Some(response_tx) = dispatch_state
                    .pending_characteristic_write_requests
                    .remove(&request_key)
                {
                    let result = if let Some(err) = &error_string {
                        Err(Error::Other(anyhow::anyhow!(
                            "Characteristic write failed: {}",
                            err
                        )))
                    } else {
                        Ok(())
                    };
                    let _ = response_tx.send(result);
                } else {
                    log::warn!("Received characteristic write response for peripheral {} service {} characteristic {} but no pending request found", identifier, service_uuid, characteristic_uuid);
                }

                // Send characteristic write complete event to bridge task
                let event = InternalEvent::CharacteristicWriteComplete {
                    identifier,
                    service_uuid,
                    characteristic_uuid,
                    error: error_string,
                };
                if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                    log::warn!("Failed to send characteristic write complete event to bridge task");
                }
            });
        }

        #[unsafe(method(peripheral:didUpdateNotificationStateForCharacteristic:error:))]
        fn peripheral_did_update_notification_state_for_characteristic(
            &self, peripheral: &CBPeripheral, characteristic: &CBCharacteristic,
            error: Option<&objc2_foundation::NSError>,
        ) {
            let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
            let characteristic_uuid = Uuid::from_cbuuid(unsafe { &characteristic.UUID() });
            let service_uuid = Uuid::from_cbuuid(unsafe {
                &characteristic
                    .service()
                    .expect("Characteristic must have a service")
                    .UUID()
            });
            let error_string = error.map(|e| e.localizedDescription().to_string());
            let is_notifying = unsafe { characteristic.isNotifying() };

            if let Some(ref err) = error_string {
                log::warn!("Characteristic subscription change failed for peripheral {} service {} characteristic {}: {}", identifier, service_uuid, characteristic_uuid, err);
            } else {
                log::debug!("Characteristic subscription changed for peripheral {} service {} characteristic {}: notifying={}", identifier, service_uuid, characteristic_uuid, is_notifying);
            }

            // Complete pending characteristic subscription request and send event to bridge task
            DispatchSpecific::<BluetoothDispatchState>::with_mut(|dispatch_state| {
                // Complete pending request if exists (both subscribe and unsubscribe use same key format)
                let request_key =
                    format!("{}:{}:{}", identifier, service_uuid, characteristic_uuid);

                // Check for subscribe request first
                if let Some(response_tx) = dispatch_state
                    .pending_characteristic_subscribe_requests
                    .remove(&request_key)
                {
                    let result = if let Some(err) = &error_string {
                        Err(Error::Other(anyhow::anyhow!(
                            "Characteristic subscription failed: {}",
                            err
                        )))
                    } else if is_notifying {
                        Ok(())
                    } else {
                        Err(Error::Other(anyhow::anyhow!("Characteristic subscription failed: not notifying after subscribe request")))
                    };
                    let _ = response_tx.send(result);
                }
                // Check for unsubscribe request
                else if let Some(response_tx) = dispatch_state
                    .pending_characteristic_unsubscribe_requests
                    .remove(&request_key)
                {
                    let result = if let Some(err) = &error_string {
                        Err(Error::Other(anyhow::anyhow!(
                            "Characteristic unsubscription failed: {}",
                            err
                        )))
                    } else if !is_notifying {
                        Ok(())
                    } else {
                        Err(Error::Other(anyhow::anyhow!("Characteristic unsubscription failed: still notifying after unsubscribe request")))
                    };
                    let _ = response_tx.send(result);
                } else {
                    log::debug!("Received characteristic notification state change for peripheral {} service {} characteristic {} but no pending request found", identifier, service_uuid, characteristic_uuid);
                }

                // Send characteristic subscription state change event to bridge task
                let event = InternalEvent::CharacteristicSubscriptionChanged {
                    identifier,
                    service_uuid,
                    characteristic_uuid,
                    subscribed: is_notifying,
                    error: error_string,
                };
                if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                    log::warn!(
                        "Failed to send characteristic subscription changed event to bridge task"
                    );
                }
            });
        }

        #[unsafe(method(peripheral:didDiscoverDescriptorsForCharacteristic:error:))]
        fn peripheral_did_discover_descriptors_for_characteristic(
            &self, peripheral: &CBPeripheral, characteristic: &CBCharacteristic,
            error: Option<&objc2_foundation::NSError>,
        ) {
            let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
            let characteristic_uuid = Uuid::from_cbuuid(unsafe { &characteristic.UUID() });
            let service_uuid = Uuid::from_cbuuid(unsafe {
                &characteristic
                    .service()
                    .expect("Characteristic must have a service")
                    .UUID()
            });
            let error_string = error.map(|e| e.localizedDescription().to_string());

            if let Some(ref err) = error_string {
                log::warn!("Descriptor discovery failed for peripheral {} service {} characteristic {}: {}", identifier, service_uuid, characteristic_uuid, err);
            } else {
                log::debug!(
                    "Descriptor discovery completed for peripheral {} service {} characteristic {}",
                    identifier,
                    service_uuid,
                    characteristic_uuid
                );
            }

            // Complete pending descriptor discovery request and send events to bridge task
            DispatchSpecific::<BluetoothDispatchState>::with_mut(|dispatch_state| {
                // Complete pending request if exists
                let request_key =
                    format!("{}:{}:{}", identifier, service_uuid, characteristic_uuid);
                if let Some(response_tx) = dispatch_state
                    .pending_descriptor_discovery_requests
                    .remove(&request_key)
                {
                    let result = if let Some(err) = &error_string {
                        Err(Error::Other(anyhow::anyhow!(
                            "Descriptor discovery failed: {}",
                            err
                        )))
                    } else {
                        Ok(())
                    };
                    let _ = response_tx.send(result);
                } else {
                    log::warn!("Received descriptor discovery response for peripheral {} service {} characteristic {} but no pending request found", identifier, service_uuid, characteristic_uuid);
                }

                // Send discovered descriptors to backend
                if error_string.is_none() {
                    if let Some(descriptors) = unsafe { characteristic.descriptors() } {
                        for i in 0..descriptors.count() {
                            let descriptor = descriptors.objectAtIndex(i);
                            if let Some(cb_descriptor) =
                                descriptor.downcast_ref::<objc2_core_bluetooth::CBDescriptor>()
                            {
                                let descriptor_uuid =
                                    Uuid::from_cbuuid(unsafe { &cb_descriptor.UUID() });
                                {
                                    // Generate descriptor handle using UUID hash for consistency
                                    let mut descriptor_hasher =
                                        std::collections::hash_map::DefaultHasher::new();
                                    descriptor_uuid.hash(&mut descriptor_hasher);
                                    let descriptor_handle =
                                        DescriptorHandle(descriptor_hasher.finish() as u32);

                                    // Store the descriptor by handle for later read/write operations
                                    dispatch_state
                                        .descriptors_by_handle
                                        .insert(descriptor_handle, cb_descriptor.retain());

                                    // Send descriptor discovery event to bridge task
                                    let event = InternalEvent::DescriptorDiscovered {
                                        identifier: identifier.clone(),
                                        service_uuid,
                                        characteristic_uuid,
                                        descriptor_uuid,
                                    };
                                    if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                                        log::warn!("Failed to send descriptor discovered event to bridge task");
                                    }
                                }
                            }
                        }
                    }
                }

                // Send descriptor discovery complete event
                let event = InternalEvent::DescriptorsDiscoveryComplete {
                    identifier,
                    service_uuid,
                    characteristic_uuid,
                    error: error_string,
                };
                if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                    log::warn!("Failed to send descriptor discovery complete event to bridge task");
                }
            });
        }

        #[unsafe(method(peripheral:didUpdateValueForDescriptor:error:))]
        fn peripheral_did_update_value_for_descriptor(
            &self, peripheral: &CBPeripheral, descriptor: &objc2_core_bluetooth::CBDescriptor,
            error: Option<&objc2_foundation::NSError>,
        ) {
            let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
            let descriptor_uuid = Uuid::from_cbuuid(unsafe { &descriptor.UUID() });
            let characteristic_uuid = Uuid::from_cbuuid(unsafe {
                &descriptor
                    .characteristic()
                    .expect("Descriptor must have a characteristic")
                    .UUID()
            });
            let service_uuid = Uuid::from_cbuuid(unsafe {
                &descriptor
                    .characteristic()
                    .expect("Descriptor must have a characteristic")
                    .service()
                    .expect("Characteristic must have a service")
                    .UUID()
            });
            let error_string = error.map(|e| e.localizedDescription().to_string());

            if let Some(ref err) = error_string {
                log::warn!("Descriptor read failed for peripheral {} service {} characteristic {} descriptor {}: {}", identifier, service_uuid, characteristic_uuid, descriptor_uuid, err);
            } else {
                log::debug!("Descriptor read completed for peripheral {} service {} characteristic {} descriptor {}", identifier, service_uuid, characteristic_uuid, descriptor_uuid);
            }

            // Get descriptor data
            let data = if error_string.is_none() {
                if let Some(value) = unsafe { descriptor.value() } {
                    if let Some(ns_data) = value.downcast_ref::<objc2_foundation::NSData>() {
                        ns_data.to_vec()
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };

            // Complete pending descriptor read request and send event to bridge task
            DispatchSpecific::<BluetoothDispatchState>::with_mut(|dispatch_state| {
                // Complete pending request if exists
                let request_key = format!(
                    "{}:{}:{}:{}",
                    identifier, service_uuid, characteristic_uuid, descriptor_uuid
                );
                if let Some(response_tx) = dispatch_state
                    .pending_descriptor_read_requests
                    .remove(&request_key)
                {
                    let result = if let Some(err) = &error_string {
                        Err(Error::Other(anyhow::anyhow!(
                            "Descriptor read failed: {}",
                            err
                        )))
                    } else {
                        Ok(data.clone())
                    };
                    let _ = response_tx.send(result);
                } else {
                    log::warn!("Received descriptor read response for peripheral {} service {} characteristic {} descriptor {} but no pending request found", identifier, service_uuid, characteristic_uuid, descriptor_uuid);
                }

                // Send descriptor read event to bridge task
                let event = InternalEvent::DescriptorRead {
                    identifier,
                    service_uuid,
                    characteristic_uuid,
                    descriptor_uuid,
                    data,
                    error: error_string,
                };
                if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                    log::warn!("Failed to send descriptor read event to bridge task");
                }
            });
        }

        #[unsafe(method(peripheral:didWriteValueForDescriptor:error:))]
        fn peripheral_did_write_value_for_descriptor(
            &self, peripheral: &CBPeripheral, descriptor: &objc2_core_bluetooth::CBDescriptor,
            error: Option<&objc2_foundation::NSError>,
        ) {
            let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
            let descriptor_uuid = Uuid::from_cbuuid(unsafe { &descriptor.UUID() });
            let characteristic_uuid = Uuid::from_cbuuid(unsafe {
                &descriptor
                    .characteristic()
                    .expect("Descriptor must have a characteristic")
                    .UUID()
            });
            let service_uuid = Uuid::from_cbuuid(unsafe {
                &descriptor
                    .characteristic()
                    .expect("Descriptor must have a characteristic")
                    .service()
                    .expect("Characteristic must have a service")
                    .UUID()
            });
            let error_string = error.map(|e| e.localizedDescription().to_string());

            if let Some(ref err) = error_string {
                log::warn!("Descriptor write failed for peripheral {} service {} characteristic {} descriptor {}: {}", identifier, service_uuid, characteristic_uuid, descriptor_uuid, err);
            } else {
                log::debug!("Descriptor write completed for peripheral {} service {} characteristic {} descriptor {}", identifier, service_uuid, characteristic_uuid, descriptor_uuid);
            }

            // Complete pending descriptor write request and send event to bridge task
            DispatchSpecific::<BluetoothDispatchState>::with_mut(|dispatch_state| {
                // Complete pending request if exists
                let request_key = format!(
                    "{}:{}:{}:{}",
                    identifier, service_uuid, characteristic_uuid, descriptor_uuid
                );
                if let Some(response_tx) = dispatch_state
                    .pending_descriptor_write_requests
                    .remove(&request_key)
                {
                    let result = if let Some(err) = &error_string {
                        Err(Error::Other(anyhow::anyhow!(
                            "Descriptor write failed: {}",
                            err
                        )))
                    } else {
                        Ok(())
                    };
                    let _ = response_tx.send(result);
                } else {
                    log::warn!("Received descriptor write response for peripheral {} service {} characteristic {} descriptor {} but no pending request found", identifier, service_uuid, characteristic_uuid, descriptor_uuid);
                }

                // Send descriptor write complete event to bridge task
                let event = InternalEvent::DescriptorWriteComplete {
                    identifier,
                    service_uuid,
                    characteristic_uuid,
                    descriptor_uuid,
                    error: error_string,
                };
                if let Err(_) = dispatch_state.internal_event_tx.send(event) {
                    log::warn!("Failed to send descriptor write complete event to bridge task");
                }
            });
        }
    }

    unsafe impl NSObjectProtocol for CoreBluetoothDelegates {}
);

// Queue-specific state that contains CBCentralManager and internal event channel
pub struct BluetoothDispatchState {
    central_manager: Retained<CBCentralManager>,
    delegate: Retained<CoreBluetoothDelegates>,
    internal_event_tx: mpsc::UnboundedSender<InternalEvent>,
    next_peripheral_handle: AtomicU32,
    pending_rssi_requests: HashMap<String, oneshot::Sender<Result<i16>>>,
    pending_connect_requests: HashMap<String, oneshot::Sender<Result<()>>>,
    pending_disconnect_requests: HashMap<String, oneshot::Sender<Result<()>>>,
    pending_service_discovery_requests: HashMap<String, oneshot::Sender<Result<()>>>,
    pending_characteristic_discovery_requests: HashMap<String, oneshot::Sender<Result<()>>>, // Key: "identifier:service_uuid"
    pending_included_service_discovery_requests: HashMap<String, oneshot::Sender<Result<()>>>, // Key: "identifier:service_uuid"
    pending_characteristic_read_requests: HashMap<String, oneshot::Sender<Result<Vec<u8>>>>, // Key: "identifier:service_uuid:characteristic_uuid"
    pending_characteristic_write_requests: HashMap<String, oneshot::Sender<Result<()>>>, // Key: "identifier:service_uuid:characteristic_uuid"
    pending_characteristic_subscribe_requests: HashMap<String, oneshot::Sender<Result<()>>>, // Key: "identifier:service_uuid:characteristic_uuid"
    pending_characteristic_unsubscribe_requests: HashMap<String, oneshot::Sender<Result<()>>>, // Key: "identifier:service_uuid:characteristic_uuid"
    pending_descriptor_discovery_requests: HashMap<String, oneshot::Sender<Result<()>>>, // Key: "identifier:service_uuid:characteristic_uuid"
    pending_descriptor_read_requests: HashMap<String, oneshot::Sender<Result<Vec<u8>>>>, // Key: "identifier:service_uuid:characteristic_uuid:descriptor_uuid"
    pending_descriptor_write_requests: HashMap<String, oneshot::Sender<Result<()>>>, // Key: "identifier:service_uuid:characteristic_uuid:descriptor_uuid"
    peripherals_by_handle: HashMap<PeripheralHandle, Retained<CBPeripheral>>,
    services_by_handle: HashMap<ServiceHandle, Retained<CBService>>, // For characteristic discovery
    characteristics_by_handle: HashMap<CharacteristicHandle, Retained<CBCharacteristic>>, // For characteristic read/write
    descriptors_by_handle: HashMap<DescriptorHandle, Retained<objc2_core_bluetooth::CBDescriptor>>, // For descriptor read/write
    peripherals_by_identifier: HashMap<String, PeripheralHandle>,
}

impl BluetoothDispatchState {
    pub fn new(
        internal_event_tx: mpsc::UnboundedSender<InternalEvent>,
        central_manager: Retained<CBCentralManager>, delegate: Retained<CoreBluetoothDelegates>,
    ) -> Self {
        Self {
            central_manager,
            delegate,
            internal_event_tx,
            next_peripheral_handle: AtomicU32::new(1),
            pending_rssi_requests: HashMap::new(),
            pending_connect_requests: HashMap::new(),
            pending_disconnect_requests: HashMap::new(),
            pending_service_discovery_requests: HashMap::new(),
            pending_characteristic_discovery_requests: HashMap::new(),
            pending_included_service_discovery_requests: HashMap::new(),
            pending_characteristic_read_requests: HashMap::new(),
            pending_characteristic_write_requests: HashMap::new(),
            pending_characteristic_subscribe_requests: HashMap::new(),
            pending_characteristic_unsubscribe_requests: HashMap::new(),
            pending_descriptor_discovery_requests: HashMap::new(),
            pending_descriptor_read_requests: HashMap::new(),
            pending_descriptor_write_requests: HashMap::new(),
            peripherals_by_handle: HashMap::new(),
            services_by_handle: HashMap::new(),
            characteristics_by_handle: HashMap::new(),
            descriptors_by_handle: HashMap::new(),
            peripherals_by_identifier: HashMap::new(),
        }
    }

    fn central_manager(&self) -> &Retained<CBCentralManager> {
        &self.central_manager
    }

    fn convert_filter_to_service_uuids(
        filter: &Filter,
    ) -> Result<Option<Retained<NSArray<CBUUID>>>> {
        if filter.service_uuids.is_empty() {
            return Ok(None);
        }

        let mut cbuuids = Vec::new();

        for uuid in &filter.service_uuids {
            let uuid_string = NSString::from_str(&uuid.to_string());
            let cbuuid = unsafe { CBUUID::UUIDWithString(&uuid_string) };
            cbuuids.push(cbuuid);
        }

        // Convert Vec to NSArray properly
        let cbuuid_refs: Vec<&CBUUID> = cbuuids.iter().map(|r| r.as_ref()).collect();
        let ns_array = NSArray::from_slice(&cbuuid_refs[..]);
        Ok(Some(ns_array))
    }

    pub fn handle_start_scanning(&mut self, filter: Filter) -> Result<()> {
        let central_manager = self.central_manager();

        // Check central manager state
        let state = unsafe { central_manager.state() };
        match state {
            CBManagerState::PoweredOn => {
                log::trace!("CoreBluetooth: Bluetooth is powered on and ready");
            }
            CBManagerState::PoweredOff => {
                log::warn!("CoreBluetooth: Bluetooth is powered off");
                return Err(Error::Unavailable(State::Disabled));
            }
            CBManagerState::Unauthorized => {
                log::warn!("CoreBluetooth: Bluetooth is unauthorized");
                return Err(Error::Unavailable(State::Unauthorized));
            }
            CBManagerState::Resetting => {
                log::debug!("CoreBluetooth: Bluetooth is resetting");
                return Err(Error::Unavailable(State::Resetting));
            }
            CBManagerState::Unsupported => {
                log::debug!("CoreBluetooth: Bluetooth is unsupported on this device");
                return Err(Error::Unavailable(State::Unsupported));
            }
            CBManagerState::Unknown | _ => {
                log::trace!("CoreBluetooth: Bluetooth manager in unknown state");
                return Err(Error::Unavailable(State::Unknown));
            }
        }

        // Convert filter to CoreBluetooth parameters
        let service_uuids = Self::convert_filter_to_service_uuids(&filter)?;

        // Start scanning
        unsafe {
            central_manager.scanForPeripheralsWithServices_options(
                service_uuids.as_deref(),
                None, // No scan options for now
            );
        }

        log::info!("Started CoreBluetooth scanning on dedicated queue");

        Ok(())
    }

    pub fn handle_stop_scanning(&mut self) -> Result<()> {
        let central_manager = self.central_manager();
        unsafe {
            central_manager.stopScan();
        }

        log::info!("Stopped CoreBluetooth scanning");
        Ok(())
    }

    pub fn handle_read_rssi(
        &mut self, peripheral_handle: PeripheralHandle, response_tx: oneshot::Sender<Result<i16>>,
    ) -> Result<()> {
        // Look up peripheral by handle
        if let Some(peripheral) = self.peripherals_by_handle.get(&peripheral_handle) {
            let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };

            // Store pending request
            self.pending_rssi_requests.insert(identifier, response_tx);

            // Set delegate and trigger RSSI read
            unsafe {
                peripheral.setDelegate(Some(ProtocolObject::from_ref(&*self.delegate)));
                peripheral.readRSSI();
            }

            log::debug!("Initiated RSSI read for peripheral {:?}", peripheral_handle);
            Ok(())
        } else {
            let _ = response_tx.send(Err(Error::InvalidStateReference));
            Ok(())
        }
    }

    pub fn handle_connect(
        &mut self, peripheral_handle: PeripheralHandle, response_tx: oneshot::Sender<Result<()>>,
    ) -> Result<()> {
        // Look up peripheral by handle
        if let Some(peripheral) = self.peripherals_by_handle.get(&peripheral_handle) {
            let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };

            // Store pending request
            self.pending_connect_requests
                .insert(identifier, response_tx);

            // Set delegate and initiate connection
            unsafe {
                peripheral.setDelegate(Some(ProtocolObject::from_ref(&*self.delegate)));
                self.central_manager
                    .connectPeripheral_options(peripheral, None);
            }

            log::debug!("Initiated connection to peripheral {:?}", peripheral_handle);
            Ok(())
        } else {
            let _ = response_tx.send(Err(Error::InvalidStateReference));
            Ok(())
        }
    }

    pub fn handle_disconnect(
        &mut self, peripheral_handle: PeripheralHandle, response_tx: oneshot::Sender<Result<()>>,
    ) -> Result<()> {
        // Look up peripheral by handle
        if let Some(peripheral) = self.peripherals_by_handle.get(&peripheral_handle) {
            let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };

            // Store pending request
            self.pending_disconnect_requests
                .insert(identifier, response_tx);

            // Initiate disconnection
            unsafe {
                self.central_manager.cancelPeripheralConnection(peripheral);
            }

            log::debug!(
                "Initiated disconnection from peripheral {:?}",
                peripheral_handle
            );
            Ok(())
        } else {
            let _ = response_tx.send(Err(Error::InvalidStateReference));
            Ok(())
        }
    }

    pub fn handle_discover_services(
        &mut self, peripheral_handle: PeripheralHandle, service_uuids: Option<Vec<Uuid>>,
        response_tx: oneshot::Sender<Result<()>>,
    ) -> Result<()> {
        // Look up peripheral by handle
        if let Some(peripheral) = self.peripherals_by_handle.get(&peripheral_handle) {
            let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };

            // Store pending request
            self.pending_service_discovery_requests
                .insert(identifier, response_tx);

            // Convert service UUIDs to CoreBluetooth format if provided
            let service_array = if let Some(uuids) = service_uuids {
                let mut cb_uuids = Vec::new();
                for uuid in &uuids {
                    let uuid_string = NSString::from_str(&uuid.to_string());
                    let cb_uuid = unsafe { CBUUID::UUIDWithString(&uuid_string) };
                    cb_uuids.push(cb_uuid);
                }
                let cb_uuid_refs: Vec<&CBUUID> = cb_uuids.iter().map(|r| r.as_ref()).collect();
                Some(NSArray::from_slice(&cb_uuid_refs[..]))
            } else {
                None
            };

            // Set delegate and initiate service discovery
            unsafe {
                peripheral.setDelegate(Some(ProtocolObject::from_ref(&*self.delegate)));
                peripheral.discoverServices(service_array.as_deref());
            }

            log::debug!(
                "Initiated service discovery for peripheral {:?}",
                peripheral_handle
            );
            Ok(())
        } else {
            let _ = response_tx.send(Err(Error::InvalidStateReference));
            Ok(())
        }
    }

    pub fn handle_discover_characteristics(
        &mut self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        response_tx: oneshot::Sender<Result<()>>,
    ) -> Result<()> {
        // Look up peripheral and service by handles
        let peripheral = match self.peripherals_by_handle.get(&peripheral_handle) {
            Some(p) => p,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };

        let service = match self.services_by_handle.get(&service_handle) {
            Some(s) => s,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };

        let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
        let service_uuid = Uuid::from_cbuuid(unsafe { &service.UUID() });

        // Store pending request with composite key
        let request_key = format!("{}:{}", identifier, service_uuid);
        self.pending_characteristic_discovery_requests
            .insert(request_key, response_tx);

        // Set delegate and initiate characteristic discovery
        unsafe {
            peripheral.setDelegate(Some(ProtocolObject::from_ref(&*self.delegate)));
            peripheral.discoverCharacteristics_forService(None, service); // Discover all characteristics
        }

        log::debug!(
            "Initiated characteristic discovery for peripheral {:?} service {:?}",
            peripheral_handle,
            service_handle
        );
        Ok(())
    }

    pub fn handle_discover_included_services(
        &mut self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        response_tx: oneshot::Sender<Result<()>>,
    ) -> Result<()> {
        // Look up peripheral and service by handles
        let peripheral = match self.peripherals_by_handle.get(&peripheral_handle) {
            Some(p) => p,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };
        let service = match self.services_by_handle.get(&service_handle) {
            Some(s) => s,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };
        let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
        let service_uuid = Uuid::from_cbuuid(unsafe { &service.UUID() });

        // Store pending request with composite key
        let request_key = format!("{}:{}", identifier, service_uuid);
        self.pending_included_service_discovery_requests
            .insert(request_key, response_tx);

        // Set delegate and initiate included service discovery
        unsafe {
            peripheral.setDelegate(Some(ProtocolObject::from_ref(&*self.delegate)));
            peripheral.discoverIncludedServices_forService(None, service); // Discover all included services
        }

        log::debug!(
            "Initiated included service discovery for peripheral {:?} service {:?}",
            peripheral_handle,
            service_handle
        );
        Ok(())
    }

    pub fn handle_read_characteristic(
        &mut self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, response_tx: oneshot::Sender<Result<Vec<u8>>>,
    ) -> Result<()> {
        // Look up peripheral, service, and characteristic by handles
        let peripheral = match self.peripherals_by_handle.get(&peripheral_handle) {
            Some(p) => p,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };
        let service = match self.services_by_handle.get(&service_handle) {
            Some(s) => s,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };
        let characteristic = match self.characteristics_by_handle.get(&characteristic_handle) {
            Some(c) => c,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };

        let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
        let service_uuid = Uuid::from_cbuuid(unsafe { &service.UUID() });
        let characteristic_uuid = Uuid::from_cbuuid(unsafe { &characteristic.UUID() });

        // Store pending request with composite key
        let request_key = format!("{}:{}:{}", identifier, service_uuid, characteristic_uuid);
        self.pending_characteristic_read_requests
            .insert(request_key, response_tx);

        // Set delegate and initiate characteristic read
        unsafe {
            peripheral.setDelegate(Some(ProtocolObject::from_ref(&*self.delegate)));
            peripheral.readValueForCharacteristic(characteristic);
        }

        log::debug!(
            "Initiated characteristic read for peripheral {:?} service {:?} characteristic {:?}",
            peripheral_handle,
            service_handle,
            characteristic_handle
        );
        Ok(())
    }

    pub fn handle_write_characteristic(
        &mut self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, data: Vec<u8>, write_type: WriteType,
        response_tx: oneshot::Sender<Result<()>>,
    ) -> Result<()> {
        // Look up peripheral, service, and characteristic by handles
        let peripheral = match self.peripherals_by_handle.get(&peripheral_handle) {
            Some(p) => p,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };
        let service = match self.services_by_handle.get(&service_handle) {
            Some(s) => s,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };
        let characteristic = match self.characteristics_by_handle.get(&characteristic_handle) {
            Some(c) => c,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };

        let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
        let service_uuid = Uuid::from_cbuuid(unsafe { &service.UUID() });
        let characteristic_uuid = Uuid::from_cbuuid(unsafe { &characteristic.UUID() });

        // Convert write type to CoreBluetooth type
        let cb_write_type = match write_type {
            WriteType::WithResponse => {
                objc2_core_bluetooth::CBCharacteristicWriteType::WithResponse
            }
            WriteType::WithoutResponse => {
                objc2_core_bluetooth::CBCharacteristicWriteType::WithoutResponse
            }
        };

        // Create NSData from the data
        let ns_data = unsafe { objc2_foundation::NSData::with_bytes(&data) };

        // Handle the request based on write type
        match write_type {
            WriteType::WithResponse => {
                // For write with response, store pending request before initiating write
                let request_key =
                    format!("{}:{}:{}", identifier, service_uuid, characteristic_uuid);
                self.pending_characteristic_write_requests
                    .insert(request_key, response_tx);

                // Set delegate and initiate characteristic write
                unsafe {
                    peripheral.setDelegate(Some(ProtocolObject::from_ref(&*self.delegate)));
                    peripheral.writeValue_forCharacteristic_type(
                        &ns_data,
                        characteristic,
                        cb_write_type,
                    );
                }
            }
            WriteType::WithoutResponse => {
                // Set delegate and initiate characteristic write
                unsafe {
                    peripheral.setDelegate(Some(ProtocolObject::from_ref(&*self.delegate)));
                    peripheral.writeValue_forCharacteristic_type(
                        &ns_data,
                        characteristic,
                        cb_write_type,
                    );
                }

                // For write without response, complete immediately
                let _ = response_tx.send(Ok(()));
            }
        }

        log::debug!("Initiated characteristic write for peripheral {:?} service {:?} characteristic {:?} with {:?}", peripheral_handle, service_handle, characteristic_handle, write_type);
        Ok(())
    }

    pub fn handle_subscribe_characteristic(
        &mut self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, response_tx: oneshot::Sender<Result<()>>,
    ) -> Result<()> {
        // Look up peripheral, service, and characteristic by handles
        let peripheral = match self.peripherals_by_handle.get(&peripheral_handle) {
            Some(p) => p,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };
        let service = match self.services_by_handle.get(&service_handle) {
            Some(s) => s,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };
        let characteristic = match self.characteristics_by_handle.get(&characteristic_handle) {
            Some(c) => c,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };

        let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
        let service_uuid = Uuid::from_cbuuid(unsafe { &service.UUID() });
        let characteristic_uuid = Uuid::from_cbuuid(unsafe { &characteristic.UUID() });

        // Store pending request with composite key
        let request_key = format!("{}:{}:{}", identifier, service_uuid, characteristic_uuid);
        self.pending_characteristic_subscribe_requests
            .insert(request_key, response_tx);

        // Set delegate and initiate characteristic subscription
        unsafe {
            peripheral.setDelegate(Some(ProtocolObject::from_ref(&*self.delegate)));
            peripheral.setNotifyValue_forCharacteristic(true, characteristic);
        }

        log::debug!("Initiated characteristic subscription for peripheral {:?} service {:?} characteristic {:?}", peripheral_handle, service_handle, characteristic_handle);
        Ok(())
    }

    pub fn handle_unsubscribe_characteristic(
        &mut self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, response_tx: oneshot::Sender<Result<()>>,
    ) -> Result<()> {
        // Look up peripheral, service, and characteristic by handles
        let peripheral = match self.peripherals_by_handle.get(&peripheral_handle) {
            Some(p) => p,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };
        let service = match self.services_by_handle.get(&service_handle) {
            Some(s) => s,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };
        let characteristic = match self.characteristics_by_handle.get(&characteristic_handle) {
            Some(c) => c,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };

        let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
        let service_uuid = Uuid::from_cbuuid(unsafe { &service.UUID() });
        let characteristic_uuid = Uuid::from_cbuuid(unsafe { &characteristic.UUID() });

        // Store pending request with composite key
        let request_key = format!("{}:{}:{}", identifier, service_uuid, characteristic_uuid);
        self.pending_characteristic_unsubscribe_requests
            .insert(request_key, response_tx);

        // Set delegate and initiate characteristic unsubscription
        unsafe {
            peripheral.setDelegate(Some(ProtocolObject::from_ref(&*self.delegate)));
            peripheral.setNotifyValue_forCharacteristic(false, characteristic);
        }

        log::debug!("Initiated characteristic unsubscription for peripheral {:?} service {:?} characteristic {:?}", peripheral_handle, service_handle, characteristic_handle);
        Ok(())
    }

    pub fn handle_discover_descriptors(
        &mut self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, response_tx: oneshot::Sender<Result<()>>,
    ) -> Result<()> {
        // Look up peripheral, service, and characteristic by handles
        let peripheral = match self.peripherals_by_handle.get(&peripheral_handle) {
            Some(p) => p,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };
        let service = match self.services_by_handle.get(&service_handle) {
            Some(s) => s,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };
        let characteristic = match self.characteristics_by_handle.get(&characteristic_handle) {
            Some(c) => c,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };

        let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
        let service_uuid = Uuid::from_cbuuid(unsafe { &service.UUID() });
        let characteristic_uuid = Uuid::from_cbuuid(unsafe { &characteristic.UUID() });

        // Store pending request with composite key
        let request_key = format!("{}:{}:{}", identifier, service_uuid, characteristic_uuid);
        self.pending_descriptor_discovery_requests
            .insert(request_key, response_tx);

        // Set delegate and initiate descriptor discovery
        unsafe {
            peripheral.setDelegate(Some(ProtocolObject::from_ref(&*self.delegate)));
            peripheral.discoverDescriptorsForCharacteristic(characteristic);
        }

        log::debug!(
            "Initiated descriptor discovery for peripheral {:?} service {:?} characteristic {:?}",
            peripheral_handle,
            service_handle,
            characteristic_handle
        );
        Ok(())
    }

    pub fn handle_read_descriptor(
        &mut self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, descriptor_handle: DescriptorHandle,
        response_tx: oneshot::Sender<Result<Vec<u8>>>,
    ) -> Result<()> {
        // Look up peripheral, service, characteristic, and descriptor by handles
        let peripheral = match self.peripherals_by_handle.get(&peripheral_handle) {
            Some(p) => p,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };
        let service = match self.services_by_handle.get(&service_handle) {
            Some(s) => s,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };
        let characteristic = match self.characteristics_by_handle.get(&characteristic_handle) {
            Some(c) => c,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };
        let descriptor = match self.descriptors_by_handle.get(&descriptor_handle) {
            Some(d) => d,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };

        let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
        let service_uuid = Uuid::from_cbuuid(unsafe { &service.UUID() });
        let characteristic_uuid = Uuid::from_cbuuid(unsafe { &characteristic.UUID() });
        let descriptor_uuid = Uuid::from_cbuuid(unsafe { &descriptor.UUID() });

        // Store pending request with composite key
        let request_key = format!(
            "{}:{}:{}:{}",
            identifier, service_uuid, characteristic_uuid, descriptor_uuid
        );
        self.pending_descriptor_read_requests
            .insert(request_key, response_tx);

        // Set delegate and initiate descriptor read
        unsafe {
            peripheral.setDelegate(Some(ProtocolObject::from_ref(&*self.delegate)));
            peripheral.readValueForDescriptor(descriptor);
        }

        log::debug!("Initiated descriptor read for peripheral {:?} service {:?} characteristic {:?} descriptor {:?}", peripheral_handle, service_handle, characteristic_handle, descriptor_handle);
        Ok(())
    }

    pub fn handle_write_descriptor(
        &mut self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, descriptor_handle: DescriptorHandle,
        data: Vec<u8>, response_tx: oneshot::Sender<Result<()>>,
    ) -> Result<()> {
        // Look up peripheral, service, characteristic, and descriptor by handles
        let peripheral = match self.peripherals_by_handle.get(&peripheral_handle) {
            Some(p) => p,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };
        let service = match self.services_by_handle.get(&service_handle) {
            Some(s) => s,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };
        let characteristic = match self.characteristics_by_handle.get(&characteristic_handle) {
            Some(c) => c,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };
        let descriptor = match self.descriptors_by_handle.get(&descriptor_handle) {
            Some(d) => d,
            None => {
                let _ = response_tx.send(Err(Error::InvalidStateReference));
                return Ok(());
            }
        };

        let identifier = unsafe { peripheral.identifier().UUIDString().to_string() };
        let service_uuid = Uuid::from_cbuuid(unsafe { &service.UUID() });
        let characteristic_uuid = Uuid::from_cbuuid(unsafe { &characteristic.UUID() });
        let descriptor_uuid = Uuid::from_cbuuid(unsafe { &descriptor.UUID() });

        // Store pending request with composite key
        let request_key = format!(
            "{}:{}:{}:{}",
            identifier, service_uuid, characteristic_uuid, descriptor_uuid
        );
        self.pending_descriptor_write_requests
            .insert(request_key, response_tx);

        // Create NSData from the data
        let ns_data = unsafe { objc2_foundation::NSData::with_bytes(&data) };

        // Set delegate and initiate descriptor write
        unsafe {
            peripheral.setDelegate(Some(ProtocolObject::from_ref(&*self.delegate)));
            peripheral.writeValue_forDescriptor(&ns_data, descriptor);
        }

        log::debug!("Initiated descriptor write for peripheral {:?} service {:?} characteristic {:?} descriptor {:?}", peripheral_handle, service_handle, characteristic_handle, descriptor_handle);
        Ok(())
    }
}
