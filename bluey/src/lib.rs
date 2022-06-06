#![allow(unused_imports)]
#![allow(unused)]

//#[cfg(target_os = "android")]
//use jni::sys::jvalue;

use ::uuid::Uuid;
use anyhow::anyhow;
use arrayvec::ArrayVec;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::fmt;
use std::iter::Extend;
use std::str::FromStr;

pub mod uuid;

pub mod session;

pub mod peripheral;
use peripheral::Peripheral;

pub mod service;
use service::Service;

pub mod characteristic;
use characteristic::{Characteristic, CharacteristicProperties};

pub mod descriptor;
use descriptor::Descriptor;

#[cfg(target_os = "windows")]
mod winrt;

#[cfg(target_os = "android")]
mod android;

mod fake;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacAddressType {
    Public,
    Random,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MAC(u64);
impl fmt::Display for MAC {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let bytes = u64::to_le_bytes(self.0);
        write!(f,
               "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
               bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5])
    }
}

/// A backend-specific unique identifier for Bluetooth devices
///
/// The underlying hardware MAC address is directly exposed on backends where
/// this is supported.
///
/// An address can be serialized/deserialized such that it's possible for
/// applications to save the address of a known device and later connect
/// back to the same device without having to re-scan
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Address {
    MAC(MAC),
    String(String),
}
impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Address::MAC(mac) => {
                write!(f, "{}", mac)
            }
            Address::String(s) => {
                write!(f, "{}", s)
            }
        }
    }
}
impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Address::MAC(mac) => {
                write!(f, "MAC:{}", mac)
            }
            Address::String(s) => {
                write!(f, "String:{}", s)
            }
        }
    }
}

// XXX: should maybe return Result if made public somehow but we don't
// really want any allocations in the 'error' path considering that a valid address
// might not be a MAC address.
fn try_u64_from_mac48_str(s: &str) -> Option<u64> {
    if s.contains(':') {
        let mut parts = ArrayVec::<_, 6>::new();
        for part in s.split(':') {
            if let Err(_e) = parts.try_push(part) {
                return None;
            }
        }
        if parts.len() != 6 {
            return None;
        }
        let mut bytes = [0u8; 8];
        for i in 0..6 {
            bytes[i] = match u8::from_str_radix(parts[i], 16) {
                Ok(v) => v,
                Err(_e) => {
                    return None;
                }
            };
        }
        Some(u64::from_le_bytes(bytes))
    } else {
        None
    }
}

impl FromStr for Address {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> std::result::Result<Self, std::convert::Infallible> {
        match try_u64_from_mac48_str(s) {
            Some(val) => Ok(Address::MAC(MAC(val))),
            None => Ok(Address::String(s.to_string())),
        }
    }
}

#[test]
fn mac_two_way() {
    let addr = Address::from_str("F1:E2:D3:C4:B5:A6").unwrap();
    assert!(matches!(addr, Address::MAC(_)));
    let str = addr.to_string();
    // Note: we are also intentionally checking that we format the address octets
    // as uppercase considering that Android is very particular about this and
    // we rely on this to format address strings on Android.
    assert_eq!(str, "F1:E2:D3:C4:B5:A6");

    let addr = Address::from_str("18c2a267-a539-4423-aecc-edeeb2784bcc").unwrap();
    assert!(matches!(addr, Address::String(_)));
    let str = addr.to_string();
    assert_eq!(str, "18c2a267-a539-4423-aecc-edeeb2784bcc");
}

#[derive(Clone, Copy, Debug)]
pub enum AddressType {
    PublicMAC,
    RandomMAC,
    String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct PeripheralHandle(u32);


#[derive(Clone, Debug)]
enum BackendPeripheralProperty {
    Name(String),
    Address(Address),
    AddressType(AddressType),
    Rssi(i16),
    TxPower(i16),
    ManufacturerData(HashMap<u16, Vec<u8>>),
    ServiceData(HashMap<Uuid, Vec<u8>>),
    ServiceIds(Vec<Uuid>),
}

/*
#[derive(Clone)]
enum PeripheralProperty {
    Name(String),
    Address(Address),
    Rssi(i16),
    TxPower(i16),
    ManufacturerData(HashMap<u16, Vec<u8>>),
    ServiceData(HashMap<Uuid, Vec<u8>>),
    Services(HashSet<Uuid>),
    Connected(bool),
    Paired(bool),
}
*/

#[non_exhaustive]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PeripheralPropertyId {
    Name,
    //Address,
    AddressType,
    Rssi,
    TxPower,
    ManufacturerData,
    ServiceData,
    ServiceIds,
    PrimaryServices,
}

// On backends where it's supported then a ServiceHandle should
// correspond to the underlying ATT attribute handle, or else
// a similar, sortable value that represents the ordering of services
// as they are on the device if known.
//
// Note: the handle should only be considered for ordering primary
// services. For included services we want to preserve the order
// of the include attributes themselves, not the referenced
// services.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct ServiceHandle(u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct CharacteristicHandle(u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct DescriptorHandle(u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CacheMode {
    Cached,
    Uncached,
}

#[derive(Debug, thiserror::Error, Clone)]
pub enum GattError {
    #[error("Insufficient Authentication")]
    InsufficientAuthentication,

    #[error("Insufficient Authorization")]
    InsufficientAuthorization,

    #[error("Insufficient Encryption")]
    InsufficientEncryption,

    #[error("Read Not Permitted")]
    ReadNotPermitted,

    #[error("Read Not Permitted")]
    WriteNotPermitted,

    #[error("Unsupported request")]
    Unsupported,

    #[error("Congested")]
    Congested,

    #[error("General Failure")]
    GeneralFailure(String),
}

#[derive(Clone, Debug)]
pub(crate) enum BackendEvent {
    PeripheralFound {
        peripheral_handle: PeripheralHandle,
    },
    PeripheralPropertySet {
        peripheral_handle: PeripheralHandle,
        property: BackendPeripheralProperty,
    },
    PeripheralConnected {
        peripheral_handle: PeripheralHandle,
    },
    PeripheralConnectionFailed {
        peripheral_handle: PeripheralHandle,
        error: Option<GattError>
    },
    PeripheralDisconnected {
        peripheral_handle: PeripheralHandle,
        error: Option<GattError>
    },
    GattService {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        uuid: Uuid,
    },
    GattServicesComplete {
        peripheral_handle: PeripheralHandle,
        error: Option<GattError>
    },
    GattIncludedService {
        peripheral_handle: PeripheralHandle,
        parent_service_handle: ServiceHandle,
        included_service_handle: ServiceHandle,
        uuid: Uuid,
    },
    GattIncludedServicesComplete {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        error: Option<GattError>
    },
    GattCharacteristic {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        uuid: Uuid,
        properties: CharacteristicProperties
    },
    GattCharacteristicsComplete {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        error: Option<GattError>
    },
    GattCharacteristicWriteNotify {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
    },
    GattCharacteristicNotify {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        value: Vec<u8>,
    },
    GattDescriptor {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        descriptor_handle: DescriptorHandle,
        uuid: Uuid,
    },
    GattDescriptorsComplete {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        error: Option<GattError>
    },
    GattDescriptorWriteNotify {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        descriptor_handle: DescriptorHandle,
    },
    GattDescriptorNotify {
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        descriptor_handle: DescriptorHandle,
        value: Vec<u8>,
    },
    Flush(u32),
}

#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum Event {
    #[non_exhaustive]
    PeripheralFound {
        peripheral: Peripheral,
        address: Address,
        name: String,
    },

    #[non_exhaustive]
    PeripheralConnected {
        peripheral: Peripheral,
    },

    #[non_exhaustive]
    PeripheralFailedToConnect {
        peripheral: Peripheral,
        error: Option<GattError>
    },

    /// Indicates that a peripheral has disconnected.
    ///
    /// Once a peripheral has disconnected then all previously associated
    /// `Service`s and `Characteristic`s become invalid and requests
    /// against these objects will start to report `InvalidStateReference`
    /// errors.
    #[non_exhaustive]
    PeripheralDisconnected {
        peripheral: Peripheral,
        error: Option<GattError>
    },
    #[non_exhaustive]
    PeripheralPropertyChanged {
        peripheral: Peripheral,
        property_id: PeripheralPropertyId,
    },
    #[non_exhaustive]
    PeripheralPrimaryGattService {
        peripheral: Peripheral,
        service: Service,
        uuid: Uuid, // just for convenience
    },
    #[non_exhaustive]
    PeripheralPrimaryGattServicesComplete {
        peripheral: Peripheral,
    },
    #[non_exhaustive]
    ServiceIncludedGattService {
        peripheral: Peripheral,
        parent_service: Service,
        service: Service,
        uuid: Uuid, // just for convenience
    },
    #[non_exhaustive]
    ServiceIncludedGattServicesComplete {
        peripheral: Peripheral,
        service: Service,
    },
    #[non_exhaustive]
    ServiceGattCharacteristic {
        peripheral: Peripheral,
        service: Service,
        characteristic: Characteristic,
        uuid: Uuid, // just for convenience
    },
    #[non_exhaustive]
    ServiceGattCharacteristicsComplete {
        peripheral: Peripheral,
        service: Service,
    },
    #[non_exhaustive]
    ServiceGattCharacteristicValueNotify {
        peripheral: Peripheral,
        service: Service,
        characteristic: Characteristic,
        value: Vec<u8>,
    },
    #[non_exhaustive]
    ServiceGattDescriptor {
        peripheral: Peripheral,
        service: Service,
        characteristic: Characteristic,
        descriptor: Descriptor,
        uuid: Uuid, // just for convenience
    },
    #[non_exhaustive]
    ServiceGattDescriptorsComplete {
        peripheral: Peripheral,
        service: Service,
        characteristic: Characteristic,
    },
    Flush(u32),
}
/*
impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::PeripheralFound { peripheral: _, address, name } => {
                f.debug_tuple("PeripheralFound")
                    .field(&address.to_string())
                    .field(name)
                    .finish()
            },
            Event::PeripheralConnected { peripheral } => {
                f.debug_tuple("PeripheralConnected")
                    .field(&peripheral.address().to_string())
                    .finish()
            }
            Event::PeripheralDisconnected { peripheral } => {
                f.debug_tuple("PeripheralDisconnected")
                    .field(&peripheral.address().to_string())
                    .finish()
            }
            Event::PeripheralPropertyChanged { peripheral, property_id } => {
                f.debug_tuple("PeripheralPropertyChanged")
                    .field(&peripheral.address().to_string())
                    .field(property_id)
                    .finish()
            },
            Event::PeripheralPrimaryGattService {peripheral, service} => {
                f.debug_tuple("PeripheralPrimaryGattService")
                    .field(&peripheral.address().to_string())
                    .field(&service.uuid().unwrap_or(Uuid::default()).to_string())
                    .finish()
            },
            Event::ServiceIncludedGattService { peripheral, parent_service, service } => {
                f.debug_tuple("ServiceIncludedGattService")
                    .field(&peripheral.address().to_string())
                    .field(&parent_service.uuid().unwrap_or(Uuid::default()).to_string())
                    .field(&service.uuid().unwrap_or(Uuid::default()).to_string())
                    .finish()
            },
        }
    }
}*/

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("The system is unable to communicate with this peripheral currently")]
    PeripheralUnreachable,

    #[error("There was a GATT communication protocol error")]
    PeripheralGattProtocolError(#[from] GattError),

    #[error("Access Denied")]
    PeripheralAccessDenied,

    #[error("Invalid State Reference")]
    InvalidStateReference,

    #[error("The system doesn't support this request / operation")]
    Unsupported,

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
