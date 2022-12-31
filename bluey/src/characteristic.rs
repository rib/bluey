use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;

use bitflags::bitflags;
use log::{debug, trace};
use thiserror::Error;
use uuid::Uuid;

use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;

use crate::descriptor::Descriptor;
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
    peripheral: Peripheral,
    //service: Service,
    characteristic_handle: CharacteristicHandle,
}

bitflags! {
    pub struct CharacteristicProperties: u32 {
        const NONE = 0;

        const BROADCAST = 0x01;
        const READ = 0x02;
        const WRITE_WITHOUT_RESPONSE = 0x04;
        const WRITE = 0x08;
        const NOTIFY = 0x10;
        const INDICATE = 0x20;
        const AUTHENTICATED_SIGNED_WRITES = 0x40;
        const EXTENDED_PROPERTIES = 0x80;
        const RELIABLE_WRITES = 0x100;
        const WRITABLE_AUXILIARIES = 0x200;
    }
}

impl Characteristic {
    pub(crate) fn wrap(
        peripheral: Peripheral, characteristic_handle: CharacteristicHandle,
    ) -> Self {
        Self {
            peripheral,
            characteristic_handle,
        }
    }

    fn get_characteristic_state(&self) -> Result<CharacteristicState> {
        let session = &self.peripheral.session;
        let peripheral_handle = self.peripheral.peripheral_handle;
        let peripheral_state = session.get_peripheral_state(peripheral_handle);

        let peripheral_state_guard = peripheral_state.inner.read().unwrap();

        let (characteristic_state, _) = session
            .get_gatt_characteristic_state(&peripheral_state_guard, self.characteristic_handle)?;

        Ok(characteristic_state)
    }

    pub fn uuid(&self) -> Result<Uuid> {
        let characteristic_state = self.get_characteristic_state()?;
        let state_guard = characteristic_state.inner.read().unwrap();
        let uuid = state_guard.uuid;
        Ok(uuid)
    }

    pub fn properties(&self) -> Result<CharacteristicProperties> {
        let characteristic_state = self.get_characteristic_state()?;
        let state_guard = characteristic_state.inner.read().unwrap();
        let properties = state_guard.properties;
        Ok(properties)
    }

    pub async fn read_value(&self, cache_mode: CacheMode) -> Result<Vec<u8>> {
        let session = &self.peripheral.session;
        let peripheral_handle = self.peripheral.peripheral_handle;

        let characteristic_state = self.get_characteristic_state()?;
        let service_handle = characteristic_state.inner.read().unwrap().service;

        session
            .backend_api()
            .gatt_characteristic_read(
                peripheral_handle,
                service_handle,
                self.characteristic_handle,
                cache_mode,
            )
            .await
    }

    pub async fn write_value(&self, write_type: WriteType, data: &[u8]) -> Result<()> {
        let session = &self.peripheral.session;
        let peripheral_handle = self.peripheral.peripheral_handle;

        let characteristic_state = self.get_characteristic_state()?;
        let service_handle = characteristic_state.inner.read().unwrap().service;

        session
            .backend_api()
            .gatt_characteristic_write(
                peripheral_handle,
                service_handle,
                self.characteristic_handle,
                write_type,
                data,
            )
            .await
    }

    pub async fn subscribe(&self) -> Result<()> {
        trace!("subscribe()");

        let session = &self.peripheral.session;
        let peripheral_handle = self.peripheral.peripheral_handle;

        let characteristic_state = self.get_characteristic_state()?;
        let service_handle = characteristic_state.inner.read().unwrap().service;

        session
            .backend_api()
            .gatt_characteristic_subscribe(
                peripheral_handle,
                service_handle,
                self.characteristic_handle,
            )
            .await
    }

    pub async fn unsubscribe(&self) -> Result<()> {
        trace!("unsubscribe()");

        let session = &self.peripheral.session;
        let peripheral_handle = self.peripheral.peripheral_handle;

        let characteristic_state = self.get_characteristic_state()?;
        let service_handle = characteristic_state.inner.read().unwrap().service;

        session
            .backend_api()
            .gatt_characteristic_unsubscribe(
                peripheral_handle,
                service_handle,
                self.characteristic_handle,
            )
            .await
    }

    pub async fn discover_descriptors(&self) -> Result<()> {
        debug!("discover_descriptors()");
        let session = &self.peripheral.session;

        let characteristic_state = self.get_characteristic_state()?;
        let service_handle = characteristic_state.inner.read().unwrap().service;

        debug!("discover_descriptors() calling catt_characteristic_discover_descriptors");
        session
            .backend_api()
            .gatt_characteristic_discover_descriptors(
                self.peripheral.peripheral_handle,
                service_handle,
                self.characteristic_handle,
            )
            .await?;
        Ok(())
    }

    pub fn descriptors(&self) -> Result<Vec<Descriptor>> {
        let characteristic_state = self.get_characteristic_state()?;

        let service_state_guard = characteristic_state.inner.read().unwrap();

        Ok(service_state_guard
            .descriptors
            .iter()
            .map(|handle| Descriptor::wrap(self.peripheral.clone(), *handle))
            .collect())
    }
}

#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum WriteType {
    WithResponse,
    WithoutResponse,
}
