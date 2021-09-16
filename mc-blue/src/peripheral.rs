use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;

use log::{info, trace, warn};
use thiserror::Error;
use uuid::Uuid;

use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;

use crate::service::Service;
use crate::session::Session;
use crate::{fake, Address, Error, MacAddressType, PeripheralHandle, Result, MAC};

// For the public API a Peripheral is just a thin wrapper over a
// backend handle associated with a peripheral-specific API.
//
// It's notably not used internally since it holds a reference to the
// Session and we need to be careful to avoid any circular references
// considering that the Session itself maintains an index of
// peripheral state.

#[derive(Clone, PartialEq, Eq)]
pub struct Peripheral {
    pub(crate) session: Session,
    pub(crate) peripheral_handle: PeripheralHandle,
}

impl fmt::Debug for Peripheral {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let addr_string = match peripheral_state.inner.try_read() {
            Ok(inner) => {
                let address = inner.address
                                   .as_ref()
                                   .unwrap_or(&Address::MAC(MAC(0)))
                                   .clone();
                address.to_string()
            }
            Err(_) => "<locked>".to_string(),
        };
        f.debug_struct("Peripheral")
         .field("address", &addr_string)
         .field("handle", &self.peripheral_handle)
         .finish()
    }
}

impl Peripheral {
    pub(crate) fn wrap(session: Session, peripheral_handle: PeripheralHandle) -> Self {
        Peripheral { session,
                     peripheral_handle }
    }

    pub async fn connect(&self) -> Result<()> {
        self.session
            .backend_api()
            .peripheral_connect(self.peripheral_handle)
            .await?;
        Ok(())
    }

    pub fn address(&self) -> Address {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        // NB: we wait until we have an address before a peripheral is advertised
        // to the application so we shouldn't ever report an address of 0 here...
        state_guard.address
                   .as_ref()
                   .unwrap_or(&Address::MAC(MAC(0)))
                   .clone()
    }

    pub fn address_type(&self) -> Option<MacAddressType> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        state_guard.address_type
    }

    pub fn name(&self) -> Option<String> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        // NB: we wait until we have a name before a peripheral is advertised
        // to the application so we shouldn't ever report 'Unknown' here...
        state_guard.name.clone()
    }

    pub fn tx_power(&self) -> Option<i16> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        state_guard.tx_power
    }

    pub fn rssi(&self) -> Option<i16> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        state_guard.rssi
    }

    /// Queries whether a particular service UUID has been discovered for
    /// this peripheral. It's possible that if a service has not been discovered
    /// yet that it could be advertised or discovered by connecting to the
    /// device in the future.
    ///
    /// A complete list of primary and secondary services can only be reliably
    /// discovered after connecting to the device.
    pub fn has_service_id(&self, id: Uuid) -> bool {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        state_guard.service_ids.contains(&id)
    }

    /// A (potentially incomplete) list of service UUIDs associated with this
    /// peripheral.
    ///
    /// A complete list of primary and secondary services can only be reliably
    /// discovered after connecting to the device.
    pub fn service_ids(&self) -> Vec<Uuid> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        state_guard.service_ids.iter().map(|uuid| *uuid).collect()
    }

    pub async fn discover_services(&self) -> Result<()> {
        trace!("discover_services()");
        self.session
            .backend_api()
            .peripheral_discover_gatt_services(self.peripheral_handle)
            .await?;
        Ok(())
    }

    pub fn services(&self) -> Vec<Service> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();

        state_guard.primary_gatt_services
                   .iter()
                   .map(|handle| Service::wrap(self.clone(), *handle))
                   .collect()
    }

    pub fn service_data(&self, id: Uuid) -> Option<Vec<u8>> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        match state_guard.service_data.get(&id) {
            Some(data) => Some(data.to_owned()),
            None => None,
        }
    }

    pub fn all_service_data(&self) -> Option<HashMap<Uuid, Vec<u8>>> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        if !state_guard.service_data.is_empty() {
            Some(state_guard.service_data
                            .iter()
                            .map(|(uuid, data)| (*uuid, data.to_owned()))
                            .collect())
        } else {
            None
        }
    }

    pub fn manufacturer_data(&self, id: u16) -> Option<Vec<u8>> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        match state_guard.manufacturer_data.get(&id) {
            Some(data) => Some(data.to_owned()),
            None => None,
        }
    }

    pub fn all_manufacturer_data(&self) -> Option<HashMap<u16, Vec<u8>>> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        if !state_guard.manufacturer_data.is_empty() {
            Some(state_guard.manufacturer_data
                            .iter()
                            .map(|(id, data)| (*id, data.to_owned()))
                            .collect())
        } else {
            None
        }
    }
}
