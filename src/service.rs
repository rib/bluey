use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;
use log::{trace};

use crate::characteristic::Characteristic;
use crate::peripheral::Peripheral;
use crate::session::{Session, ServiceState};
use crate::{fake, winrt, Address, Error, MacAddressType, Result, ServiceHandle, MAC};
//use crate::ServicePropertyId;

// For the public API a Service is just a thin wrapper over a
// Peripheral and a backend service handle. NB: we don't use
// a Uuid as a unique key for a service since it's possible
// for devices to expose the same service (with the same uuid)
// multiple times, differentiated with an attribute handle.
//
// It's notably not used internally since it holds a reference to the
// Peripheral which indirectly holds a reference to the Session and
// we need to be careful to avoid any circular references.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Service {
    pub(crate) peripheral: Peripheral,
    pub(crate) service_handle: ServiceHandle,
}

impl Service {
    pub(crate) fn wrap(peripheral: Peripheral, service_handle: ServiceHandle) -> Self {
        Service {
            peripheral,
            service_handle,
        }
    }

    fn get_service_state(&self) -> Result<ServiceState> {
        let session = &self.peripheral.session;
        let peripheral_handle = self.peripheral.peripheral_handle;
        let peripheral_state = session.get_peripheral_state(peripheral_handle);

        let peripheral_state_guard = peripheral_state.inner.read().unwrap();

        let (service_state, _) =
            session.get_gatt_service_state(&peripheral_state_guard, self.service_handle)?;

        Ok(service_state)
    }

    pub fn uuid(&self) -> Result<Uuid> {
        let service_state = self.get_service_state()?;
        let service_state_guard = service_state.inner.read().unwrap();

        Ok(service_state_guard.uuid)
    }

    pub async fn discover_included_services(&self) -> Result<()> {
        trace!("discover_included_services()");
        let session = &self.peripheral.session;
        session
            .backend_api()
            .gatt_service_discover_includes(self.peripheral.peripheral_handle, self.service_handle)
            .await?;
        Ok(())
    }

    pub fn included_services(&self) -> Result<Vec<Service>> {
        let service_state = self.get_service_state()?;
        let service_state_guard = service_state.inner.read().unwrap();

        Ok(service_state_guard
            .included_services
            .iter()
            .map(|handle| Service::wrap(self.peripheral.clone(), *handle))
            .collect())
    }

    pub async fn discover_characteristics(&self) -> Result<()> {
        trace!("discover_characteristics()");
        let session = &self.peripheral.session;
        session
            .backend_api()
            .gatt_service_discover_characteristics(
                self.peripheral.peripheral_handle,
                self.service_handle,
            )
            .await?;
        Ok(())
    }

    pub fn characteristics(&self) -> Result<Vec<Characteristic>> {
        let service_state = self.get_service_state()?;

        let service_state_guard = service_state.inner.read().unwrap();

        Ok(service_state_guard
            .characteristics
            .iter()
            .map(|handle| Characteristic::wrap(self.clone(), *handle))
            .collect())
    }
}
