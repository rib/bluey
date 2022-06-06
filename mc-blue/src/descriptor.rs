use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;

use log::trace;
use thiserror::Error;
use uuid::Uuid;

use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;

use crate::characteristic::{Characteristic, self};
use crate::peripheral::Peripheral;
use crate::session::{DescriptorState, Session, CharacteristicState};
use crate::{
    fake, Address, CacheMode, DescriptorHandle, Error, MacAddressType, Result, Service, MAC, CharacteristicHandle,
};

// For the public API a Descriptor is just a thin wrapper over a
// Service and a backend descriptor handle.
//
// NB: we don't use a Uuid as a unique key for a descriptor
// since it's possible for devices to expose the same descriptor
// (with the same uuid) multiple times, differentiated with an attribute
// handle.
//
// It's notably not used internally since it holds a reference to the
// Peripheral which indirectly holds a reference to the Session and
// we need to be careful to avoid any circular references.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Descriptor {
    // To keep the size of this transient struct compact we don't maintain
    // the full chain of Descriptor -> Characteristic -> Service -> Peripheral -> Session
    // dependencies here. The characteristic can be found via the session state
    // if needed.
    //
    // We include the Peripheral for the sake of ensuring the peripheral isn't dropped
    // (and potentially disconnected) so long as the application is holding a
    // descriptor reference.
    peripheral: Peripheral,
    //characteristic: Characteristic,
    descriptor_handle: DescriptorHandle,
}

impl Descriptor {
    pub(crate) fn wrap(peripheral: Peripheral, descriptor_handle: DescriptorHandle) -> Self {
        Self { peripheral, descriptor_handle }
    }

    fn get_descriptor_state(&self) -> Result<(DescriptorState, CharacteristicState, CharacteristicHandle)> {
        let session = &self.peripheral.session;
        let peripheral_handle = self.peripheral.peripheral_handle;
        let peripheral_state = session.get_peripheral_state(peripheral_handle);

        let peripheral_state_guard = peripheral_state.inner.read().unwrap();

        let (descriptor_state, _) =
            session.get_gatt_descriptor_state(&peripheral_state_guard,
                                              self.descriptor_handle)?;

        let characteristic_handle = descriptor_state.inner.read().unwrap().characteristic;
        let (characteristic_state, _) =
            session.get_gatt_characteristic_state(&peripheral_state_guard,
                                                  characteristic_handle)?;

        Ok((descriptor_state, characteristic_state, characteristic_handle))
    }

    pub fn uuid(&self) -> Result<Uuid> {
        let (descriptor_state, _, _) = self.get_descriptor_state()?;
        let state_guard = descriptor_state.inner.read().unwrap();
        let uuid = state_guard.uuid;
        Ok(uuid)
    }

    pub async fn read_value(&self, cache_mode: CacheMode) -> Result<Vec<u8>> {
        let session = &self.peripheral.session;
        let peripheral_handle = self.peripheral.peripheral_handle;

        let (descriptor_state, characteristic_state, characteristic_handle) = self.get_descriptor_state()?;
        let service_handle = characteristic_state.inner.read().unwrap().service;

        session.backend_api()
               .gatt_descriptor_read(peripheral_handle, service_handle, characteristic_handle, self.descriptor_handle, cache_mode)
               .await
    }

    pub async fn write_value(&self, write_type: WriteType, data: &[u8]) -> Result<()> {
        let session = &self.peripheral.session;
        let peripheral_handle = self.peripheral.peripheral_handle;

        let (descriptor_state, characteristic_state, characteristic_handle) = self.get_descriptor_state()?;
        let service_handle = characteristic_state.inner.read().unwrap().service;

        session.backend_api()
               .gatt_descriptor_write(peripheral_handle,
                                      service_handle,
                                      characteristic_handle,
                                      self.descriptor_handle,
                                      data)
               .await
    }
}

#[non_exhaustive]
pub enum WriteType {
    WithResponse,
    WithoutResponse,
}