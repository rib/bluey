use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::fmt;
use std::ops::Deref;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Weak};

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
//
// The application-facing Peripheral is also ref-counted so we
// can infer that the application is no longer interested in a
// device if they drop all references (and implicitly disconnect
// + free cached state if necessary)

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct PeripheralInner {
    pub(crate) session: Session,
    pub(crate) peripheral_handle: PeripheralHandle,
}
impl Drop for PeripheralInner {
    fn drop(&mut self) {
        self.session.on_peripheral_drop(self.peripheral_handle);
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Peripheral {
    pub(crate) inner: Arc<PeripheralInner>,
}

impl Deref for Peripheral {
    type Target = PeripheralInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[derive(Debug)]
pub(crate) struct WeakPeripheral {
    pub(crate) inner: Weak<PeripheralInner>,
}
impl WeakPeripheral {
    pub(crate) fn upgrade(&self) -> Option<Peripheral> {
        self.inner.upgrade().map(|strong_inner| Peripheral {
            inner: strong_inner,
        })
    }
}

impl fmt::Debug for Peripheral {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let addr_string = match peripheral_state.inner.try_read() {
            Ok(inner) => {
                let address = inner
                    .address
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
    pub(crate) fn new(session: Session, peripheral_handle: PeripheralHandle) -> Self {
        Peripheral {
            inner: Arc::new(PeripheralInner {
                session,
                peripheral_handle,
            }),
        }
    }

    pub(crate) fn downgrade(&self) -> WeakPeripheral {
        let weak_inner = Arc::downgrade(&self.inner);
        WeakPeripheral { inner: weak_inner }
    }

    pub(crate) fn check_connected(&self) -> Result<()> {
        //let connected = self.session
        //    .backend_api()
        //    .peripheral_is_connected(self.peripheral_handle)?;

        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        if state_guard.is_connected {
            Ok(())
        } else {
            Err(Error::PeripheralAccessDenied)
        }
    }

    /// Requests to connect to the GATT server for this peripheral.
    ///
    /// A successful result means that your request to connect has been _initiated_, and
    /// you should then listen for a subsequent [PeripheralConnected] event to handle a
    /// successful connection.
    ///
    /// If there is some failure while trying to connect to the device you will get a
    /// [PeripheralFailedToConnect] event.
    ///
    /// You can cancel a request to connect by calling [`.disconnect()`][disconnect]
    ///
    /// [PeripheralConnected]: crate::Event::PeripheralConnected
    /// [PeripheralFailedToConnect]: crate::Event::PeripheralFailedToConnect
    /// [disconnect]: Self::disconnect
    ///
    /// # Portability
    ///
    /// On Windows the underlying system API for bluetooth does not expose explicit connections
    /// and so the backend will take an action that implicitly needs to to connect to the peripheral.
    /// This also means that it's possible that the system might have already connected to the
    /// peripheral.
    ///
    pub async fn connect(&self) -> Result<()> {
        self.session
            .backend_api()
            .peripheral_connect(self.peripheral_handle)
            .await
    }

    pub async fn disconnect(&self) -> Result<()> {
        self.session
            .backend_api()
            .peripheral_disconnect(self.peripheral_handle)
            .await
    }

    pub fn address(&self) -> Address {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        // NB: we wait until we have an address before a peripheral is advertised
        // to the application so we shouldn't ever report an address of 0 here...
        state_guard
            .address
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

    /// Returns any cached TX power level
    ///
    /// This API won't initiate any IO but if a TX power level has been advertised
    /// while scanning this will return the latest value.
    pub fn tx_power(&self) -> Option<i16> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        state_guard.tx_power
    }

    /// Returns any cached RSSI value
    ///
    /// This API won't initiate any IO but if an RSSI value has been advertised
    /// while scanning or read while connected (via `read_rssi()`) this will
    /// return the latest cached RSSI value.
    pub fn rssi(&self) -> Option<i16> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        state_guard.rssi
    }

    /// Reads the peripherals RSSI while connected.
    ///
    /// A successfull read will also updated the cached RSSI value available via `rssi()`
    ///
    /// # Portability
    /// It's currently not possible to explicitly read a peripheral's RSSI once connected to GATT
    /// on Windows, so this API will return `Error::Unsupported`. Alternatively RSSI can be
    /// got via scanning on Windows.
    pub async fn read_rssi(&self) -> Result<i16> {
        self.check_connected()?;
        let rssi = self
            .session
            .backend_api()
            .peripheral_read_rssi(self.peripheral_handle)
            .await?;
        Ok(rssi)
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
        state_guard.service_ids.iter().copied().collect()
    }

    /// Queries all the available services for the peripheral.
    ///
    /// When complete an [`ServicesDiscovered`] event will be delivered and the
    /// services will be accessible via the `services()` method.
    ///
    /// On some platforms the discovery can optionally be optimized by
    /// giving a list of `of_interest_hint` services so the system can just
    /// query for these services only. (The hint may be ignored on some
    /// platforms so applications may also see additional services
    /// advertised)
    ///
    /// Although this method is async, its completion is not guaranteed to
    /// correspond with the completed discovery of all services. On some
    /// platforms this might be the case and on other platforms the function
    /// completes after dispatching the request. For portability your
    /// application should wait for a `PeripheralPrimaryGattServicesComplete`
    /// event.
    ///
    /// # Errors
    ///
    /// The peripheral should be in a connected state when calling this API and
    /// will return `Error::PeripheralAccessDenied` if not.
    ///
    pub async fn discover_services(&self, of_interest_hint: Option<Vec<Uuid>>) -> Result<()> {
        trace!("discover_services()");
        self.check_connected()?;
        self.session
            .backend_api()
            .peripheral_discover_gatt_services(self.peripheral_handle, of_interest_hint)
            .await?;
        Ok(())
    }

    /// Get a peripheral service by its `uuid`
    ///
    /// This may return primary or secondary (included) services.
    ///
    /// There is no guaranteed order for the returned services. If your application needs to
    /// know the on-device order of the services then it can call [`Self::primary_services()`]
    ///
    /// In case a peripheral advertises multiple services with the same Uuid then it's undefined
    /// which specific instance will be returned by this API. The [`Self::primary_services()`]
    /// API can be used in this case to access multiple services with the same Uuid and with
    /// their on-device order preserved.
    ///
    /// This won't initiate any IO, so it will only return a service that has been previously
    /// discovered.
    pub fn service(&self, uuid: Uuid) -> Option<Service> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();

        state_guard
            .gatt_services_by_uuid
            .get(&uuid)
            .map(|r| Service::wrap(self.clone(), *r))
    }

    /// Fetch all the services that have been discovered for this `Peripheral`
    ///
    /// This will return primary and secondary (included) services.
    ///
    /// This won't initiate any IO, so it will only return a service that has been previously
    /// discovered.
    pub fn services(&self) -> Vec<Service> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();

        state_guard
            .gatt_services
            .iter()
            .map(|kv| Service::wrap(self.clone(), *kv.key()))
            .collect()
    }

    /// Fetch all the primary services that have been discovered for this `Peripheral`
    ///
    /// The services will be ordered as they are on the device if the operating system preserves
    /// that information.
    pub fn primary_services(&self) -> Vec<Service> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();

        state_guard
            .gatt_primary_services
            .iter()
            .map(|handle| Service::wrap(self.clone(), *handle))
            .collect()
    }

    pub fn service_data(&self, id: Uuid) -> Option<Vec<u8>> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        state_guard
            .service_data
            .get(&id)
            .map(|data| data.to_owned())
    }

    pub fn all_service_data(&self) -> Option<HashMap<Uuid, Vec<u8>>> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        if !state_guard.service_data.is_empty() {
            Some(
                state_guard
                    .service_data
                    .iter()
                    .map(|(uuid, data)| (*uuid, data.to_owned()))
                    .collect(),
            )
        } else {
            None
        }
    }

    pub fn manufacturer_data(&self, id: u16) -> Option<Vec<u8>> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        state_guard
            .manufacturer_data
            .get(&id)
            .map(|data| data.to_owned())
    }

    pub fn all_manufacturer_data(&self) -> Option<HashMap<u16, Vec<u8>>> {
        let peripheral_state = self.session.get_peripheral_state(self.peripheral_handle);
        let state_guard = peripheral_state.inner.read().unwrap();
        if !state_guard.manufacturer_data.is_empty() {
            Some(
                state_guard
                    .manufacturer_data
                    .iter()
                    .map(|(id, data)| (*id, data.to_owned()))
                    .collect(),
            )
        } else {
            None
        }
    }
}
