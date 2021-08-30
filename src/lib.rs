#![allow(unused_imports)]

use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use ::uuid::Uuid;
use anyhow::anyhow;
use serde::{Serialize, Deserialize};
use std::fmt;
use std::str::FromStr;
use std::iter::Extend;
use arrayvec::ArrayVec;

mod uuid;

pub mod peripheral;
use peripheral::Peripheral;

pub mod session;

#[cfg(target_os = "windows")]
mod winrt;

mod fake;

#[derive(Clone, Copy, Debug)]
pub enum MacAddressType {
    Public,
    Random,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MAC(u64);
impl fmt::Display for MAC {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let bytes = u64::to_le_bytes(self.0);
        write!(f, "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5])
    }
}

/// A platform-specific unique identifier for Bluetooth devices
///
/// The underlying hardware MAC address is directly exposed on platforms where
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
        let mut bytes =  [0u8; 8];
        for i in 0..6 {
            bytes[i] = match u8::from_str_radix(parts[i], 16) {
                Ok(v) => v,
                Err(_e) => { return None; }
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
            None => Ok(Address::String(s.to_string()))
        }
    }
}

#[test]
fn mac_two_way() {
    let addr = Address::from_str("01:02:03:04:05:06").unwrap();
    let str = addr.to_string();
    assert_eq!(str, "01:02:03:04:05:06");

    let addr = Address::from_str("18c2a267-a539-4423-aecc-edeeb2784bcc").unwrap();
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
struct PlatformPeripheralHandle(u32);

#[derive(Clone, Debug)]
enum PlatformPeripheralProperty {
    Name(String),
    Address(Address),
    AddressType(AddressType),
    Rssi(i16),
    TxPower(i16),
    ManufacturerData(HashMap<u16, Vec<u8>>),
    ServiceData(HashMap<Uuid, Vec<u8>>),
    Services(Vec<Uuid>),
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
#[derive(Clone, Copy, Debug)]
pub enum PeripheralPropertyId {
    Name,
    //Address,
    AddressType,
    Rssi,
    TxPower,
    ManufacturerData,
    ServiceData,
    Services,
}

#[derive(Clone, Debug)]
pub(crate) enum PlatformEvent {
    PeripheralFound {
        peripheral_handle: PlatformPeripheralHandle,
    },
    PeripheralPropertySet {
        peripheral_handle: PlatformPeripheralHandle,
        property: PlatformPeripheralProperty,
    },
    PeripheralConnected {
        peripheral_handle: PlatformPeripheralHandle,
    },
    PeripheralDisconnected {
        peripheral_handle: PlatformPeripheralHandle,
    },
}

#[non_exhaustive]
#[derive(Clone)]
pub enum Event {
    #[non_exhaustive]
    PeripheralFound {
        peripheral: Peripheral,
        address: Address,
        name: String
    },
    PeripheralConnected(Peripheral),
    PeripheralDisconnected(Peripheral),
    PeripheralPropertyChanged(Peripheral, PeripheralPropertyId),
}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::PeripheralFound { peripheral: _, address, name } => {
                f.debug_tuple("PeripheralFound")
                    .field(&address.to_string())
                    .field(name)
                    .finish()
            },
            Event::PeripheralConnected(peripheral) => {
                f.debug_tuple("PeripheralConnected")
                    .field(&peripheral.address().to_string())
                    .finish()
            }
            Event::PeripheralDisconnected(peripheral) => {
                f.debug_tuple("PeripheralDisconnected")
                    .field(&peripheral.address().to_string())
                    .finish()
            }
            Event::PeripheralPropertyChanged(peripheral, id) => {
                f.debug_tuple("PeripheralPropertyChanged")
                    .field(&peripheral.address().to_string())
                    .field(id)
                    .finish()
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("The system is unable to communicate with this peripheral currently")]
    PeripheralUnreachable,

    #[error("There was a GATT communication protocol error")]
    PeripheralGattProtocolError,

    #[error("Access Denied")]
    PeripheralAccessDenied,

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

