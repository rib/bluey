use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;

use log::trace;
use thiserror::Error;
use uuid::Uuid;

use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;

use crate::peripheral::Peripheral;
use crate::session::{CharacteristicState, Session};
use crate::{
    fake, Address, CacheMode, CharacteristicHandle, Error, MacAddressType, Result, Service, MAC,
};

// For the public API a Characteristic is just a thin wrapper over a
// Service and a backend characteristic handle.
//
// NB: we don't use a Uuid as a unique key for a characteristic
// since it's possible for devices to expose the same characteristic
// (with the same uuid) multiple times, differentiated with an attribute
// handle.
//
// It's notably not used internally since it holds a reference to the
// Peripheral which indirectly holds a reference to the Session and
// we need to be careful to avoid any circular references.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Characteristic {
    service: Service,
    characteristic_handle: CharacteristicHandle,
}

impl Characteristic {
    pub(crate) fn wrap(service: Service, characteristic_handle: CharacteristicHandle) -> Self {
        Self { service,
               characteristic_handle }
    }

    fn get_characteristic_state(&self) -> Result<CharacteristicState> {
        let session = &self.service.peripheral.session;
        let peripheral_handle = self.service.peripheral.peripheral_handle;
        let peripheral_state = session.get_peripheral_state(peripheral_handle);

        let peripheral_state_guard = peripheral_state.inner.read().unwrap();

        let (characteristic_state, _) =
            session.get_gatt_characteristic_state(&peripheral_state_guard,
                                                  self.characteristic_handle)?;

        Ok(characteristic_state)
    }

    pub fn uuid(&self) -> Result<Uuid> {
        let characteristic_state = self.get_characteristic_state()?;
        Ok(characteristic_state.uuid)
    }

    pub async fn read_value(&self, cache_mode: CacheMode) -> Result<Vec<u8>> {
        let session = &self.service.peripheral.session;
        let peripheral_handle = self.service.peripheral.peripheral_handle;

        session.backend_api()
               .gatt_characteristic_read(peripheral_handle, self.characteristic_handle, cache_mode)
               .await
    }

    pub async fn write_value(&self, write_type: WriteType, data: &[u8]) -> Result<()> {
        let session = &self.service.peripheral.session;
        let peripheral_handle = self.service.peripheral.peripheral_handle;

        session.backend_api()
               .gatt_characteristic_write(peripheral_handle,
                                          self.characteristic_handle,
                                          write_type,
                                          data)
               .await
    }

    pub async fn subscribe(&self) -> Result<()> {
        let session = &self.service.peripheral.session;
        let peripheral_handle = self.service.peripheral.peripheral_handle;
        let service_handle = self.service.service_handle;

        trace!("subscribe()");
        session.backend_api()
               .gatt_characteristic_subscribe(peripheral_handle,
                                              service_handle,
                                              self.characteristic_handle)
               .await
    }

    pub async fn unsubscribe(&self) -> Result<()> {
        let session = &self.service.peripheral.session;
        let peripheral_handle = self.service.peripheral.peripheral_handle;
        let service_handle = self.service.service_handle;

        trace!("unsubscribe()");
        session.backend_api()
               .gatt_characteristic_unsubscribe(peripheral_handle,
                                                service_handle,
                                                self.characteristic_handle)
               .await
    }
}

#[non_exhaustive]
pub enum WriteType {
    WithResponse,
    WithoutResponse,
}
