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

// Bridge requests for communication between Backend and bridge task
#[derive(Debug)]
pub enum BridgeRequest {
    ReadRssi {
        peripheral_handle: PeripheralHandle,
        response_tx: oneshot::Sender<Result<i16>>,
    },
    Connect {
        peripheral_handle: PeripheralHandle,
        response_tx: oneshot::Sender<Result<()>>,
    },
    Disconnect {
        peripheral_handle: PeripheralHandle,
        response_tx: oneshot::Sender<Result<()>>,
    },
    DiscoverServices {
        peripheral_handle: PeripheralHandle,
        service_uuids: Option<Vec<Uuid>>,
        response_tx: oneshot::Sender<Result<()>>,
    },
    DiscoverCharacteristics {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        response_tx: oneshot::Sender<Result<()>>,
    },
    DiscoverIncludedServices {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        response_tx: oneshot::Sender<Result<()>>,
    },
    ReadCharacteristic {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        response_tx: oneshot::Sender<Result<Vec<u8>>>,
    },
    WriteCharacteristic {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        data: Vec<u8>,
        write_type: WriteType,
        response_tx: oneshot::Sender<Result<()>>,
    },
    SubscribeCharacteristic {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        response_tx: oneshot::Sender<Result<()>>,
    },
    UnsubscribeCharacteristic {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        response_tx: oneshot::Sender<Result<()>>,
    },
    DiscoverDescriptors {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        response_tx: oneshot::Sender<Result<()>>,
    },
    ReadDescriptor {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        descriptor_handle: DescriptorHandle,
        response_tx: oneshot::Sender<Result<Vec<u8>>>,
    },
    WriteDescriptor {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        descriptor_handle: DescriptorHandle,
        data: Vec<u8>,
        response_tx: oneshot::Sender<Result<()>>,
    },
}

// Bridge task that converts internal events to backend events
pub async fn event_bridge_task(
    mut internal_rx: mpsc::UnboundedReceiver<InternalEvent>,
    backend_bus: mpsc::UnboundedSender<BackendEvent>, state_tx: watch::Sender<CBManagerState>,
) {
    // State for managing peripheral mappings
    let mut next_peripheral_id = 1u32;
    let mut peripherals_by_identifier: HashMap<String, PeripheralHandle> = HashMap::new();

    while let Some(event) = internal_rx.recv().await {
        match event {
            InternalEvent::StateChanged(state) => {
                log::debug!("Bridge task: Central manager state changed to {:?}", state);
                let _ = state_tx.send(state);
            }
            InternalEvent::PeripheralDiscovered {
                identifier,
                name,
                rssi,
                advertisement_data,
            } => {
                // Get or create peripheral handle
                let peripheral_handle =
                    if let Some(&existing_handle) = peripherals_by_identifier.get(&identifier) {
                        existing_handle
                    } else {
                        let handle = PeripheralHandle(next_peripheral_id);
                        next_peripheral_id += 1;
                        peripherals_by_identifier.insert(identifier.clone(), handle);

                        // Send PeripheralFound event for new peripherals
                        if let Err(_) = backend_bus.send(BackendEvent::PeripheralFound {
                            peripheral_handle: handle,
                        }) {
                            log::warn!("Failed to send PeripheralFound event");
                            continue;
                        }

                        handle
                    };

                // Send Address property
                let address = Address::String(identifier.clone());
                if let Err(_) = backend_bus.send(BackendEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: BackendPeripheralProperty::Address(address),
                }) {
                    log::warn!("Failed to send Address property");
                    continue;
                }

                // Send AddressType property
                if let Err(_) = backend_bus.send(BackendEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: BackendPeripheralProperty::AddressType(AddressType::String),
                }) {
                    log::warn!("Failed to send AddressType property");
                    continue;
                }

                // Send Name property if available (prefer advertisement data local name)
                let effective_name = advertisement_data
                    .get(&AdvertisementDataKey::LocalName)
                    .and_then(|ad_val| match ad_val {
                        AdvertisementValue::LocalName(local_name) => Some(local_name.clone()),
                        _ => None,
                    })
                    .or_else(|| name);

                if let Some(ref name_str) = effective_name {
                    if let Err(_) = backend_bus.send(BackendEvent::PeripheralPropertySet {
                        peripheral_handle,
                        property: BackendPeripheralProperty::Name(name_str.clone()),
                    }) {
                        log::warn!("Failed to send Name property");
                        continue;
                    }
                }

                // Send RSSI property
                if let Err(_) = backend_bus.send(BackendEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: BackendPeripheralProperty::Rssi(rssi as i16),
                }) {
                    log::warn!("Failed to send RSSI property");
                    continue;
                }

                // Send TxPower property if available from advertisement data
                if let Some(AdvertisementValue::TxPowerLevel(tx_power)) =
                    advertisement_data.get(&AdvertisementDataKey::TxPowerLevel)
                {
                    if let Err(_) = backend_bus.send(BackendEvent::PeripheralPropertySet {
                        peripheral_handle,
                        property: BackendPeripheralProperty::TxPower(*tx_power as i16),
                    }) {
                        log::warn!("Failed to send TxPower property");
                        continue;
                    }
                }

                // Send ManufacturerData property if available
                if let Some(AdvertisementValue::ManufacturerData(manufacturer_data)) =
                    advertisement_data.get(&AdvertisementDataKey::ManufacturerData)
                {
                    // Convert Vec<u8> to the expected manufacturer data format
                    // For now, we'll create a simple HashMap with a placeholder ID
                    let mut manufacturer_map = HashMap::new();
                    if manufacturer_data.len() >= 2 {
                        let company_id =
                            u16::from_le_bytes([manufacturer_data[0], manufacturer_data[1]]);
                        let data = manufacturer_data[2..].to_vec();
                        manufacturer_map.insert(company_id, data);

                        if let Err(_) = backend_bus.send(BackendEvent::PeripheralPropertySet {
                            peripheral_handle,
                            property: BackendPeripheralProperty::ManufacturerData(manufacturer_map),
                        }) {
                            log::warn!("Failed to send ManufacturerData property");
                            continue;
                        }
                    }
                }

                // Send ServiceIds property if available from advertisement data
                if let Some(AdvertisementValue::ServiceUuids(service_uuids)) =
                    advertisement_data.get(&AdvertisementDataKey::ServiceUuids)
                {
                    if let Err(_) = backend_bus.send(BackendEvent::PeripheralPropertySet {
                        peripheral_handle,
                        property: BackendPeripheralProperty::ServiceIds(service_uuids.clone()),
                    }) {
                        log::warn!("Failed to send ServiceIds property");
                        continue;
                    }
                }

                // Send ServiceData property if available from advertisement data
                let mut service_data_map = HashMap::new();
                for (key, value) in &advertisement_data {
                    if let AdvertisementDataKey::ServiceData(_) = key {
                        if let AdvertisementValue::ServiceData { uuid, data } = value {
                            service_data_map.insert(*uuid, data.clone());
                        }
                    }
                }
                if !service_data_map.is_empty() {
                    if let Err(_) = backend_bus.send(BackendEvent::PeripheralPropertySet {
                        peripheral_handle,
                        property: BackendPeripheralProperty::ServiceData(service_data_map),
                    }) {
                        log::warn!("Failed to send ServiceData property");
                        continue;
                    }
                }

                // Additional advertisement data that we log but don't currently map to backend properties
                if let Some(AdvertisementValue::SolicitedServiceUuids(solicited_uuids)) =
                    advertisement_data.get(&AdvertisementDataKey::SolicitedServiceUuids)
                {
                    log::debug!("Device is soliciting services: {:?}", solicited_uuids);
                }

                if let Some(AdvertisementValue::OverflowServiceUuids(overflow_uuids)) =
                    advertisement_data.get(&AdvertisementDataKey::OverflowServiceUuids)
                {
                    log::debug!("Device has overflow service UUIDs: {:?}", overflow_uuids);
                }

                if let Some(AdvertisementValue::IsConnectable(is_connectable)) =
                    advertisement_data.get(&AdvertisementDataKey::IsConnectable)
                {
                    log::debug!("Device is connectable: {}", is_connectable);
                }

                log::debug!(
                    "Bridge task: Peripheral discovered: {} ({}) with handle {:?}, advertisement data keys: {:?}",
                    effective_name.as_deref().unwrap_or("Unknown"),
                    identifier,
                    peripheral_handle,
                    advertisement_data.keys().collect::<Vec<_>>()
                );
            }
            InternalEvent::PeripheralConnected { identifier } => {
                if let Some(&peripheral_handle) = peripherals_by_identifier.get(&identifier) {
                    if let Err(_) =
                        backend_bus.send(BackendEvent::PeripheralConnected { peripheral_handle })
                    {
                        log::warn!("Failed to send PeripheralConnected event");
                    }
                }
            }
            InternalEvent::PeripheralDisconnected { identifier, error } => {
                if let Some(&peripheral_handle) = peripherals_by_identifier.get(&identifier) {
                    if let Err(_) = backend_bus.send(BackendEvent::PeripheralDisconnected {
                        peripheral_handle,
                        error: None, // TODO: Map error properly if needed
                    }) {
                        log::warn!("Failed to send PeripheralDisconnected event");
                    }
                }
            }
            InternalEvent::PeripheralConnectionFailed { identifier, error } => {
                log::warn!(
                    "Connection failed for peripheral {}: {:?}",
                    identifier,
                    error
                );
                // Could send a connection failed event if the backend supports it
            }
            InternalEvent::PeripheralRssiRead {
                identifier,
                rssi,
                error,
            } => {
                log::debug!(
                    "Bridge task: RSSI read for peripheral {}: {:?}",
                    identifier,
                    rssi
                );
                // For now, we don't send RSSI read events to the backend
                // The response is handled through the pending request mechanism in dispatch state
            }
            InternalEvent::ServiceDiscovered {
                identifier,
                service_uuid,
            } => {
                log::debug!(
                    "Bridge task: Service discovered for peripheral {}: {}",
                    identifier,
                    service_uuid
                );

                // Look up peripheral handle by identifier
                if let Some(&peripheral_handle) = peripherals_by_identifier.get(&identifier) {
                    // Generate a service handle - use UUID hash for consistency
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    service_uuid.hash(&mut hasher);
                    let service_handle = ServiceHandle(hasher.finish() as u32);

                    // Send GATT service event to backend
                    if let Err(_) = backend_bus.send(BackendEvent::GattService {
                        peripheral_handle,
                        service_handle,
                        uuid: service_uuid,
                    }) {
                        log::warn!("Failed to send GattService event");
                    }
                } else {
                    log::warn!("Service discovered for unknown peripheral: {}", identifier);
                }
            }
            InternalEvent::ServicesDiscoveryComplete { identifier, error } => {
                log::debug!(
                    "Bridge task: Services discovery complete for peripheral {}: {:?}",
                    identifier,
                    error
                );

                // Look up peripheral handle by identifier
                if let Some(&peripheral_handle) = peripherals_by_identifier.get(&identifier) {
                    // Send GattServicesComplete event to backend
                    if let Err(_) = backend_bus.send(BackendEvent::GattServicesComplete {
                        peripheral_handle,
                        error: None, // For now, we don't map CoreBluetooth errors to specific GattErrors
                    }) {
                        log::warn!("Failed to send GattServicesComplete event");
                    }
                } else {
                    log::warn!(
                        "Services discovery complete for unknown peripheral: {}",
                        identifier
                    );
                }
            }
            InternalEvent::CharacteristicDiscovered {
                identifier,
                service_uuid,
                characteristic_uuid,
                properties,
            } => {
                log::debug!(
                    "Bridge task: Characteristic discovered for peripheral {}, service {}: {} (properties: {})",
                    identifier,
                    service_uuid,
                    characteristic_uuid,
                    properties
                );

                // Look up peripheral handle by identifier
                if let Some(&peripheral_handle) = peripherals_by_identifier.get(&identifier) {
                    // Generate handles using UUID hashes for consistency
                    let mut service_hasher = std::collections::hash_map::DefaultHasher::new();
                    service_uuid.hash(&mut service_hasher);
                    let service_handle = ServiceHandle(service_hasher.finish() as u32);

                    let mut char_hasher = std::collections::hash_map::DefaultHasher::new();
                    characteristic_uuid.hash(&mut char_hasher);
                    let characteristic_handle = CharacteristicHandle(char_hasher.finish() as u32);

                    // Convert properties to CharacteristicProperties
                    let char_properties =
                        crate::characteristic::CharacteristicProperties::from_bits_truncate(
                            properties,
                        );

                    // Send GATT characteristic event to backend
                    if let Err(_) = backend_bus.send(BackendEvent::GattCharacteristic {
                        peripheral_handle,
                        service_handle,
                        characteristic_handle,
                        uuid: characteristic_uuid,
                        properties: char_properties,
                    }) {
                        log::warn!("Failed to send GattCharacteristic event");
                    }
                } else {
                    log::warn!(
                        "Characteristic discovered for unknown peripheral: {}",
                        identifier
                    );
                }
            }
            InternalEvent::CharacteristicsDiscoveryComplete {
                identifier,
                service_uuid,
                error,
            } => {
                log::debug!(
                    "Bridge task: Characteristics discovery complete for peripheral {}, service {}: {:?}",
                    identifier,
                    service_uuid,
                    error
                );

                // Look up peripheral handle by identifier
                if let Some(&peripheral_handle) = peripherals_by_identifier.get(&identifier) {
                    // Generate service handle using UUID hash for consistency
                    let mut service_hasher = std::collections::hash_map::DefaultHasher::new();
                    service_uuid.hash(&mut service_hasher);
                    let service_handle = ServiceHandle(service_hasher.finish() as u32);

                    // Send GattCharacteristicsComplete event to backend
                    if let Err(_) = backend_bus.send(BackendEvent::GattCharacteristicsComplete {
                        peripheral_handle,
                        service_handle,
                        error: None, // For now, we don't map CoreBluetooth errors to specific GattErrors
                    }) {
                        log::warn!("Failed to send GattCharacteristicsComplete event");
                    }
                } else {
                    log::warn!(
                        "Characteristics discovery complete for unknown peripheral: {}",
                        identifier
                    );
                }
            }
            InternalEvent::IncludedServiceDiscovered {
                identifier,
                parent_service_uuid,
                included_service_uuid,
            } => {
                log::debug!(
                    "Bridge task: Included service {} discovered for peripheral {}, parent service {}",
                    included_service_uuid,
                    identifier,
                    parent_service_uuid
                );

                // Look up peripheral handle by identifier
                if let Some(&peripheral_handle) = peripherals_by_identifier.get(&identifier) {
                    // Generate parent service handle using UUID hash for consistency
                    let mut parent_service_hasher =
                        std::collections::hash_map::DefaultHasher::new();
                    parent_service_uuid.hash(&mut parent_service_hasher);
                    let parent_service_handle =
                        ServiceHandle(parent_service_hasher.finish() as u32);

                    // Generate included service handle using UUID hash for consistency
                    let mut included_service_hasher =
                        std::collections::hash_map::DefaultHasher::new();
                    included_service_uuid.hash(&mut included_service_hasher);
                    let included_service_handle =
                        ServiceHandle(included_service_hasher.finish() as u32);

                    // Send GATT included service event to backend
                    if let Err(_) = backend_bus.send(BackendEvent::GattIncludedService {
                        peripheral_handle,
                        parent_service_handle,
                        included_service_handle,
                        uuid: included_service_uuid,
                    }) {
                        log::warn!("Failed to send GattIncludedService event");
                    }
                } else {
                    log::warn!(
                        "Included service discovered for unknown peripheral: {}",
                        identifier
                    );
                }
            }
            InternalEvent::IncludedServicesDiscoveryComplete {
                identifier,
                service_uuid,
                error,
            } => {
                log::debug!(
                    "Bridge task: Included services discovery complete for peripheral {}, service {}: {:?}",
                    identifier,
                    service_uuid,
                    error
                );

                // Look up peripheral handle by identifier
                if let Some(&peripheral_handle) = peripherals_by_identifier.get(&identifier) {
                    // Generate service handle using UUID hash for consistency
                    let mut service_hasher = std::collections::hash_map::DefaultHasher::new();
                    service_uuid.hash(&mut service_hasher);
                    let service_handle = ServiceHandle(service_hasher.finish() as u32);

                    // Send GattIncludedServicesComplete event to backend
                    if let Err(_) = backend_bus.send(BackendEvent::GattIncludedServicesComplete {
                        peripheral_handle,
                        service_handle,
                        error: None, // For now, we don't map CoreBluetooth errors to specific GattErrors
                    }) {
                        log::warn!("Failed to send GattIncludedServicesComplete event");
                    }
                } else {
                    log::warn!(
                        "Included services discovery complete for unknown peripheral: {}",
                        identifier
                    );
                }
            }
            InternalEvent::CharacteristicRead {
                identifier,
                service_uuid,
                characteristic_uuid,
                data,
                error,
            } => {
                log::debug!(
                    "Bridge task: Characteristic read for peripheral {}, service {}, characteristic {}: {} bytes, error: {:?}",
                    identifier,
                    service_uuid,
                    characteristic_uuid,
                    data.len(),
                    error
                );

                // Look up peripheral handle by identifier
                if let Some(&peripheral_handle) = peripherals_by_identifier.get(&identifier) {
                    // Generate service handle using UUID hash for consistency
                    let mut service_hasher = std::collections::hash_map::DefaultHasher::new();
                    service_uuid.hash(&mut service_hasher);
                    let service_handle = ServiceHandle(service_hasher.finish() as u32);

                    // Generate characteristic handle using UUID hash for consistency
                    let mut characteristic_hasher =
                        std::collections::hash_map::DefaultHasher::new();
                    characteristic_uuid.hash(&mut characteristic_hasher);
                    let characteristic_handle =
                        CharacteristicHandle(characteristic_hasher.finish() as u32);

                    // Send GattCharacteristicNotify event to backend (for both reads and notifications)
                    if let Err(_) = backend_bus.send(BackendEvent::GattCharacteristicNotify {
                        peripheral_handle,
                        service_handle,
                        characteristic_handle,
                        value: data,
                    }) {
                        log::warn!("Failed to send GattCharacteristicNotify event");
                    }
                } else {
                    log::warn!("Characteristic read for unknown peripheral: {}", identifier);
                }
            }
            InternalEvent::CharacteristicWriteComplete {
                identifier,
                service_uuid,
                characteristic_uuid,
                error,
            } => {
                log::debug!(
                    "Bridge task: Characteristic write complete for peripheral {}, service {}, characteristic {}: error: {:?}",
                    identifier,
                    service_uuid,
                    characteristic_uuid,
                    error
                );

                // For now, we don't send specific write completion events to the backend
                // The write completion is already handled in the delegate by completing the pending request
                // This event is mainly for logging and potential future use
            }
            InternalEvent::CharacteristicSubscriptionChanged {
                identifier,
                service_uuid,
                characteristic_uuid,
                subscribed,
                error,
            } => {
                log::debug!(
                    "Bridge task: Characteristic subscription changed for peripheral {}, service {}, characteristic {}: subscribed={}, error: {:?}",
                    identifier,
                    service_uuid,
                    characteristic_uuid,
                    subscribed,
                    error
                );

                // For now, we don't send specific subscription change events to the backend
                // The subscription completion is already handled in the delegate by completing the pending request
                // However, notifications from subscribed characteristics will be sent as GattCharacteristicNotify events
                // This event is mainly for logging and potential future subscription state tracking
            }
            InternalEvent::DescriptorDiscovered {
                identifier,
                service_uuid,
                characteristic_uuid,
                descriptor_uuid,
            } => {
                log::debug!(
                    "Bridge task: Descriptor {} discovered for peripheral {}, service {}, characteristic {}",
                    descriptor_uuid,
                    identifier,
                    service_uuid,
                    characteristic_uuid
                );

                // Look up peripheral handle by identifier
                if let Some(&peripheral_handle) = peripherals_by_identifier.get(&identifier) {
                    // Generate service handle using UUID hash for consistency
                    let mut service_hasher = std::collections::hash_map::DefaultHasher::new();
                    service_uuid.hash(&mut service_hasher);
                    let service_handle = ServiceHandle(service_hasher.finish() as u32);

                    // Generate characteristic handle using UUID hash for consistency
                    let mut characteristic_hasher =
                        std::collections::hash_map::DefaultHasher::new();
                    characteristic_uuid.hash(&mut characteristic_hasher);
                    let characteristic_handle =
                        CharacteristicHandle(characteristic_hasher.finish() as u32);

                    // Generate descriptor handle using UUID hash for consistency
                    let mut descriptor_hasher = std::collections::hash_map::DefaultHasher::new();
                    descriptor_uuid.hash(&mut descriptor_hasher);
                    let descriptor_handle = DescriptorHandle(descriptor_hasher.finish() as u32);

                    // Send GATT descriptor event to backend
                    if let Err(_) = backend_bus.send(BackendEvent::GattDescriptor {
                        peripheral_handle,
                        service_handle,
                        characteristic_handle,
                        descriptor_handle,
                        uuid: descriptor_uuid,
                    }) {
                        log::warn!("Failed to send GattDescriptor event");
                    }
                } else {
                    log::warn!(
                        "Descriptor discovered for unknown peripheral: {}",
                        identifier
                    );
                }
            }
            InternalEvent::DescriptorsDiscoveryComplete {
                identifier,
                service_uuid,
                characteristic_uuid,
                error,
            } => {
                log::debug!(
                    "Bridge task: Descriptors discovery complete for peripheral {}, service {}, characteristic {}: error: {:?}",
                    identifier,
                    service_uuid,
                    characteristic_uuid,
                    error
                );

                // Look up peripheral handle by identifier
                if let Some(&peripheral_handle) = peripherals_by_identifier.get(&identifier) {
                    // Generate service handle using UUID hash for consistency
                    let mut service_hasher = std::collections::hash_map::DefaultHasher::new();
                    service_uuid.hash(&mut service_hasher);
                    let service_handle = ServiceHandle(service_hasher.finish() as u32);

                    // Generate characteristic handle using UUID hash for consistency
                    let mut characteristic_hasher =
                        std::collections::hash_map::DefaultHasher::new();
                    characteristic_uuid.hash(&mut characteristic_hasher);
                    let characteristic_handle =
                        CharacteristicHandle(characteristic_hasher.finish() as u32);

                    // Send GattDescriptorsComplete event to backend
                    if let Err(_) = backend_bus.send(BackendEvent::GattDescriptorsComplete {
                        peripheral_handle,
                        service_handle,
                        characteristic_handle,
                        error: None, // For now, we don't map CoreBluetooth errors to specific GattErrors
                    }) {
                        log::warn!("Failed to send GattDescriptorsComplete event");
                    }
                } else {
                    log::warn!(
                        "Descriptors discovery complete for unknown peripheral: {}",
                        identifier
                    );
                }
            }
            InternalEvent::DescriptorRead {
                identifier,
                service_uuid,
                characteristic_uuid,
                descriptor_uuid,
                data,
                error,
            } => {
                log::debug!(
                    "Bridge task: Descriptor read for peripheral {}, service {}, characteristic {}, descriptor {}: {} bytes, error: {:?}",
                    identifier,
                    service_uuid,
                    characteristic_uuid,
                    descriptor_uuid,
                    data.len(),
                    error
                );

                // For now, we don't send specific descriptor read events to the backend
                // The descriptor read completion is already handled in the delegate by completing the pending request
                // This event is mainly for logging and potential future use
            }
            InternalEvent::DescriptorWriteComplete {
                identifier,
                service_uuid,
                characteristic_uuid,
                descriptor_uuid,
                error,
            } => {
                log::debug!(
                    "Bridge task: Descriptor write complete for peripheral {}, service {}, characteristic {}, descriptor {}: error: {:?}",
                    identifier,
                    service_uuid,
                    characteristic_uuid,
                    descriptor_uuid,
                    error
                );

                // For now, we don't send specific descriptor write completion events to the backend
                // The descriptor write completion is already handled in the delegate by completing the pending request
                // This event is mainly for logging and potential future use
            }
        }
    }

    log::info!("Event bridge task shutting down");
}
