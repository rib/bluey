use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;
//use std::borrow::Cow;
use crate::{Error, PlatformPeripheralHandle, Result, fake, winrt, Address, MAC, MacAddressType};
use crate::session::Session;
use std::sync::atomic::{AtomicIsize, Ordering};
//use crate::PeripheralPropertyId;
use uuid::Uuid;
use std::collections::HashMap;

// For the public API a Peripheral is just a thin wrapper over a
// backend handle associated with a peripheral-specific API.
//
// It's notably not used internally since it holds a reference to the
// Session and we need to be careful to avoid any circular references
// considering that the Session itself maintains an index of
// peripheral state.

#[derive(Clone, Debug)]
pub struct Peripheral {
    session: Session,
    peripheral_handle: PlatformPeripheralHandle
}

impl Peripheral {
    pub(crate) fn wrap(session: Session, peripheral_handle: PlatformPeripheralHandle) -> Self {
        Peripheral {
            session,
            peripheral_handle
        }
    }

    pub async fn connect(&self) -> Result<()> {
        self.session.connect_peripheral(self.peripheral_handle).await?;
        Ok(())
    }

    pub fn address(&self) -> Address {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        // NB: we wait until we have an address before a peripheral is advertised
        // to the application so we shouldn't ever report an address of 0 here...
        state_guard.address.as_ref().unwrap_or(&Address::MAC(MAC(0))).clone()
    }

    pub fn address_type(&self) -> Option<MacAddressType> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        state_guard.address_type
    }

    pub fn name(&self) -> String {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        // NB: we wait until we have a name before a peripheral is advertised
        // to the application so we shouldn't ever report 'Unknown' here...
        state_guard.name.as_ref().unwrap_or(&format!("Unknown")).clone()
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

    pub fn has_service(&self, id: Uuid) -> bool {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        state_guard.services.contains(&id)
    }

    pub fn all_services(&self) -> Vec<Uuid> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        state_guard.services.iter().map(|uuid| *uuid).collect()
    }

    pub fn service_data(&self, id: Uuid) -> Option<Vec<u8>> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        match state_guard.service_data.get(&id) {
            Some(data) => Some(data.to_owned()),
            None => None
        }
    }

    pub fn all_service_data(&self) -> Option<HashMap<Uuid,Vec<u8>>> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        if !state_guard.service_data.is_empty() {
            Some(state_guard.service_data.iter().map(|(uuid, data)| (*uuid, data.to_owned())).collect())
        } else {
            None
        }
    }

    pub fn manufacturer_data(&self, id: u16) -> Option<Vec<u8>> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        match state_guard.manufacturer_data.get(&id) {
            Some(data) => Some(data.to_owned()),
            None => None
        }
    }

    pub fn all_manufacturer_data(&self) -> Option<HashMap<u16,Vec<u8>>> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        if !state_guard.manufacturer_data.is_empty() {
            Some(state_guard.manufacturer_data.iter().map(|(id, data)| (*id, data.to_owned())).collect())
        } else {
            None
        }
    }
}

