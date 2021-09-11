use async_trait::async_trait;
use dashmap::DashMap;
use futures::{stream, Stream, StreamExt};
use log::{info, trace, warn};
use std::borrow::BorrowMut;
use std::collections::{HashMap, HashSet};
use std::{char, default};
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::RwLock as StdRwLock;
use std::sync::{Arc, Weak};
use tokio::sync::Mutex;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

use crate::characteristic::{Characteristic, WriteType};
use crate::peripheral::{self, Peripheral};
use crate::service::{self, Service};
use crate::{fake, winrt, Address, AddressType, Error, BackendPeripheralProperty, Result, MAC};
use crate::{
    CacheMode, CharacteristicHandle, MacAddressType, PeripheralPropertyId,
    PeripheralHandle, ServiceHandle,
};
use crate::{Event, BackendEvent};

use anyhow::anyhow;

#[derive(Clone, Debug)]
pub struct Session {
    inner: Arc<SessionInner>,
}
impl PartialEq for Session {
    fn eq(&self, other: &Session) -> bool {
        Arc::<SessionInner>::ptr_eq(&self.inner, &other.inner)
    }
}
impl Eq for Session {}

#[derive(Debug)]
struct SessionInner {
    // The public-facing event stream
    event_bus: broadcast::Sender<Event>,
    next_flush_index: AtomicU32,

    // A union/enum of all the available backend implementations.
    // In general for a given backend this will likely only include
    // one for the current OS and one 'fake' backend that can be
    // chosen when configuring a session before start()ing it.
    backend: BackendSessionImpl,

    // There is also a 'backend_bus' that serves as a stream of events
    // from the backend backend and a task associated with this frontend
    // which gets spawned during `start()`. One end is handed directly to
    // the backend and the other is passed into the task that will process
    // backend events, so we don't actually store the RX end here.

    // Note: we have a (tokio) mutex here to synchronize while starting/stopping
    // scanning, not just for maintaining this is_scanning itself, so this can't
    // just be an AtomicBool
    is_scanning: Mutex<bool>,

    // All the state tracking for peripherals
    peripherals: DashMap<PeripheralHandle, PeripheralState>,
}

#[async_trait]
pub(crate) trait BackendSession {
    async fn start_scanning(&self, filter: &Filter) -> Result<()>;
    async fn stop_scanning(&self) -> Result<()>;

    async fn peripheral_connect(&self, peripheral_handle: PeripheralHandle) -> Result<()>;

    async fn peripheral_discover_gatt_services(
        &self,
        peripheral_handle: PeripheralHandle,
    ) -> Result<()>;

    async fn gatt_service_discover_includes(
        &self,
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
    ) -> Result<()>;
    async fn gatt_service_discover_characteristics(
        &self,
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
    ) -> Result<()>;

    async fn gatt_characteristic_read(
        &self,
        peripheral_handle: PeripheralHandle,
        characteristic_handle: CharacteristicHandle,
        cache_mode: CacheMode,
    ) -> Result<Vec<u8>>;
    async fn gatt_characteristic_write(
        &self,
        peripheral_handle: PeripheralHandle,
        characteristic_handle: CharacteristicHandle,
        write_type: WriteType,
        data: &[u8],
    ) -> Result<()>;

    // We explicitly pass the backend the service_handle here because we want
    // characteristic value notifications sent from the backend to include
    // the service but the backend isn't otherwise expected to have to track
    // relationships between handles
    async fn gatt_characteristic_subscribe(
        &self,
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
    ) -> Result<()>;
    async fn gatt_characteristic_unsubscribe(
        &self,
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
    ) -> Result<()>;

    // To make it possible for the frontend cache to be cleared then we need to
    // be able to cope with receiving service or characteristic handles with
    // no context. When recreating state objects we at least need to know the
    // uuids...
    fn gatt_service_uuid(&self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle) -> Result<Uuid>;
    fn gatt_characteristic_uuid(&self, peripheral_handle: PeripheralHandle, characteristic_handle: CharacteristicHandle) -> Result<Uuid>;

    fn flush(&self, id: u32) -> Result<()>;
}

#[derive(Debug)]
enum BackendSessionImpl {
    #[cfg(target_os = "windows")]
    Winrt(winrt::session::WinrtSession),
    Fake(fake::session::FakeSession),
}
impl BackendSessionImpl {
    fn api(&self) -> &dyn BackendSession {
        match self {
            BackendSessionImpl::Winrt(winrt) => winrt,
            BackendSessionImpl::Fake(fake) => fake,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PeripheralState {
    // Note we use a std::sync RwLock instead of a tokio RwLock while
    // we don't really expect any significant contention and so we can
    // support a simpler, synchronous api for reading peripheral
    // properties instead of needing to await for every property read
    pub(crate) inner: Arc<StdRwLock<PeripheralStateInner>>,
}

#[derive(Debug)]
pub(crate) struct PeripheralStateInner {
    peripheral_handle: PeripheralHandle,

    // We wait until we have an address and name before we advertise
    // a Peripheral to applications.
    advertised: bool,

    // (mostly) GAP advertised state
    //
    // The index of service_ids will also be updated based on GATT
    // state services are queried after connecting to Gatt)
    //
    pub(crate) address: Option<Address>,
    pub(crate) name: Option<String>,
    pub(crate) address_type: Option<MacAddressType>,
    pub(crate) tx_power: Option<i16>,
    pub(crate) rssi: Option<i16>,
    pub(crate) manufacturer_data: HashMap<u16, Vec<u8>>,
    pub(crate) service_data: HashMap<Uuid, Vec<u8>>,
    pub(crate) service_ids: HashSet<Uuid>,

    // GATT state
    //
    pub(crate) is_connected: bool,

    pub(crate) gatt_services: DashMap<ServiceHandle, ServiceState>, // (primary and secondary)
    pub(crate) gatt_characteristics: DashMap<CharacteristicHandle, CharacteristicState>,

    pub(crate) primary_gatt_services: Vec<ServiceHandle>,
    pub(crate) primary_gatt_services_complete: bool,
}

impl PeripheralState {
    fn new(peripheral_handle: PeripheralHandle) -> Self {
        PeripheralState {
            inner: Arc::new(StdRwLock::new(PeripheralStateInner {
                peripheral_handle,
                advertised: false,
                address: None,
                name: None,
                address_type: None,
                tx_power: None,
                rssi: None,
                manufacturer_data: HashMap::new(),
                service_data: HashMap::new(),
                service_ids: HashSet::new(),
                is_connected: false,
                gatt_services: DashMap::new(),
                gatt_characteristics: DashMap::new(),
                primary_gatt_services: vec![],
                primary_gatt_services_complete: false
            })),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ServiceState {
    pub(crate) inner: Arc<StdRwLock<ServiceStateInner>>,
}
impl ServiceState {
    fn new(uuid: Uuid) -> Self {
        Self {
            inner: Arc::new(StdRwLock::new(ServiceStateInner {
                uuid,
                included_services: vec![],
                included_services_complete: false,
                characteristics: vec![],
                characteristics_complete: false,
            })),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ServiceStateInner {
    pub(crate) uuid: Uuid,
    pub(crate) included_services: Vec<ServiceHandle>,
    pub(crate) included_services_complete: bool,

    pub(crate) characteristics: Vec<CharacteristicHandle>,
    pub(crate) characteristics_complete: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct CharacteristicState {
    pub(crate) uuid: Uuid,
}
impl CharacteristicState {
    fn new(uuid: Uuid) -> Self {
        Self { uuid }
    }
}

pub struct Filter {}
impl Filter {
    pub fn new() -> Self {
        Filter {}
    }
    pub fn by_address(&mut self, address: Address) -> &mut Self {
        todo!();
        self
    }
    pub fn by_services(&mut self, uuids: HashSet<Uuid>) -> &mut Self {
        todo!();
        self
    }
}

/*
Note: the initial scanning API supported ref-counting subscriptions to
scan and returned a ScanSubscription that would drop the ref count when
it got dropped. That sounded like a neat idea but ended up feeling
kinda awkward to use. Maybe I'll revisit this later though.

It's a bit fiddly that different backends may handle attempts
to start multiple scans differently and potentially it'd be good to
enforce consistent behaviour by ensuring we only ever ask the
backend to scan with one filter which would be the union of filters
if the app wants to create multiple scanners.

From an application POV though I'm not sure there's really much
need to have multiple scans, and it could just be fine to enforce
at this level that it's an error to try and start multiple scans.

pub struct ScanSubscription {
    session: Session,
}
impl Drop for ScanSubscription {
    fn drop(&mut self) {
    }
}
*/

pub enum Backend {
    SystemDefault,
    Fake,
}
pub struct SessionConfig {
    backend: Backend,
}

impl SessionConfig {
    pub fn new() -> SessionConfig {
        SessionConfig {
            backend: Backend::SystemDefault,
        }
    }

    pub fn set_backend(&mut self, backend: Backend) -> &mut Self {
        self.backend = backend;
        self
    }

    pub async fn start(self) -> Result<Session> {
        Session::start(self).await
    }
}

impl Session {
    // In situations where need to pass around a reference to a Session internally but need
    // to avoid creating a circular reference (such as for the task spawned to process
    // backend events) we can instead share a Weak<> reference to the SessionInner and then
    // on-demand (when processing a backend event) we can `upgrade` the reference to an `Arc`
    // and then use this api to re`wrap()` the SessionInner into a bona fide `Session`.
    //
    // Note: we follow the same Weak ref + upgrade->wrap() pattern in the backends too when
    // we need to register callbacks with the OS that will need to reference backend
    // session state but shouldn't create a circular reference that makes it impossible to
    // drop the backend session state
    fn wrap(inner: Arc<SessionInner>) -> Self {
        Self { inner }
    }

    async fn start(config: SessionConfig) -> Result<Self> {
        let (broadcast_sender, _) = broadcast::channel(16);

        // Each per-backend backend is responsible for feeding the backend event bus
        // and then we handle state tracking and forwarding corresponding events to
        // the application as necessary
        let (backend_bus_tx, backend_bus_rx) = mpsc::unbounded_channel();
        let backend = match config.backend {
            #[cfg(target_os = "windows")]
            Backend::SystemDefault => {
                let implementation =
                    winrt::session::WinrtSession::new(&config, backend_bus_tx.clone()).await?;
                BackendSessionImpl::Winrt(implementation)
            }
            Backend::Fake => {
                let implementation =
                    fake::session::FakeSession::new(&config, backend_bus_tx.clone()).await?;
                BackendSessionImpl::Fake(implementation)
            }
        };
        let session = Session {
            inner: Arc::new(SessionInner {
                event_bus: broadcast_sender,
                next_flush_index: AtomicU32::new(0),
                //backend_bus: backend_bus_rx,
                backend,
                is_scanning: Mutex::new(false),
                //scan_subscriptions: AtomicU32::new(0),
                peripherals: DashMap::new(),
            }),
        };

        // XXX: This task (which will be responsible for processing all backend
        // events) is only given a Weak reference to the session, otherwise
        // it would introduce a circular reference and it wouldn't be possible to
        // drop a Session. The task will temporarily upgrade this to a strong
        // reference only while actually processing a backend event, and the
        // task will also be able to recognise when the TX end of the backend_bus
        // closes.
        let weak_session = Arc::downgrade(&session.inner);
        tokio::spawn(async move { Session::run_backend_task(weak_session, backend_bus_rx).await });

        Ok(session)
    }

    pub(crate) fn ensure_peripheral_state(
        &self,
        peripheral_handle: PeripheralHandle,
    ) -> PeripheralState {
        match self.inner.peripherals.get(&peripheral_handle) {
            Some(peripheral_state) => peripheral_state.clone(),
            None => {
                let peripheral_state = PeripheralState::new(peripheral_handle);
                self.inner
                    .peripherals
                    .insert(peripheral_handle, peripheral_state.clone());
                peripheral_state
            }
        }
    }

    pub(crate) fn get_peripheral_state(
        &self,
        peripheral_handle: PeripheralHandle,
    ) -> PeripheralState {
        match self.inner.peripherals.get(&peripheral_handle) {
            Some(peripheral_state) => peripheral_state.clone(),
            None => {
                log::error!(
                    "Spurious notification of a new peripheral {:?}",
                    peripheral_handle
                );

                // Handle this gracefully by just creating peripheral state on the fly (this would imply
                // a backend bug, not an application bug and it's not obvious what the best way of handling
                // this case would be)
                self.ensure_peripheral_state(peripheral_handle)
            }
        }
    }

    /*
    pub(crate) fn ensure_gatt_service_state(
        &self,
        peripheral_inner: &PeripheralStateInner,
        service_handle: ServiceHandle,
        uuid: Uuid,
    ) -> (ServiceState, bool) {
        match peripheral_inner.gatt_services.get(&service_handle) {
            Some(service_state) => (service_state.clone(), false),
            None => {
                let service_state = ServiceState::new(service_handle, uuid);
                peripheral_inner
                    .gatt_services
                    .insert(service_handle, service_state.clone());
                (service_state, true)
            }
        }
    }
    */

    pub(crate) fn get_gatt_service_state(
        &self,
        peripheral_state: &PeripheralStateInner,
        service_handle: ServiceHandle,
    ) -> Result<(ServiceState, bool)> {
        match peripheral_state.gatt_services.get(&service_handle) {
            Some(service_state) => Ok((service_state.clone(), false)),
            None => {
                log::trace!("Creating state for unknown Service {:?}", service_handle);
                let uuid = self.backend_api().gatt_service_uuid(peripheral_state.peripheral_handle, service_handle)?;
                let service_state = ServiceState::new(uuid);
                peripheral_state
                    .gatt_services
                    .insert(service_handle, service_state.clone());
                Ok((service_state, true))
            }
        }
    }

    pub(crate) fn get_gatt_characteristic_state(
        &self,
        peripheral_state: &PeripheralStateInner,
        characteristic_handle: CharacteristicHandle,
    ) -> Result<(CharacteristicState, bool)> {
        match peripheral_state
            .gatt_characteristics
            .get(&characteristic_handle)
        {
            Some(characteristic_state) => Ok((characteristic_state.clone(), false)),
            None => {
                log::trace!("Creating state for unknown Characteristic {:?}", characteristic_handle);
                let uuid = self.backend_api().gatt_characteristic_uuid(peripheral_state.peripheral_handle, characteristic_handle)?;
                let characteristic_state = CharacteristicState::new(uuid);
                peripheral_state
                    .gatt_characteristics
                    .insert(characteristic_handle, characteristic_state.clone());
                Ok((characteristic_state, true))
            }
        }
    }

    fn on_peripheral_connected(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);
        let mut state_guard = peripheral_state.inner.write().unwrap();

        if state_guard.is_connected != true {
            trace!("PeripheralConnected: handle={}/{}",
                    peripheral_handle.0,
                    state_guard.address.as_ref().unwrap_or(&Address::MAC(MAC(0))).to_string());
            state_guard.is_connected = true;

            trace!(
                "Notifying peripheral {} connected",
                state_guard.address.as_ref().unwrap().to_string()
            );
            let _ = self.inner.event_bus.send(Event::PeripheralConnected {
                peripheral: Peripheral::wrap(self.clone(), peripheral_handle),
            });
        } else {
            warn!("Spurious, unbalanced/redundant PeripheralConnected notification from backend");
        }

        Ok(())
    }

    fn on_peripheral_disconnected(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);
        let mut state_guard = peripheral_state.inner.write().unwrap();

        trace!("PeripheralDisconnected: handle={}/{}",
                peripheral_handle.0,
                state_guard.address.as_ref().unwrap_or(&Address::MAC(MAC(0))).to_string());

        trace!("Invalidating GATT state");
        state_guard.gatt_services.clear();
        state_guard.gatt_characteristics.clear();
        state_guard.primary_gatt_services.clear();

        // XXX: it's possible that the backend could send redundant disconnect events
        // if it has multiple orthogonal indicators of a disconnect happening (e.g. explicit
        // disconnect callback from OS vs observed IO failure) so we try to normalize this for
        // the application...
        if state_guard.is_connected != false {
            state_guard.is_connected = false;

            trace!("Notifying peripheral {} disconnected",
                state_guard.address.as_ref().unwrap().to_string());
            let _ = self.inner.event_bus.send(Event::PeripheralDisconnected {
                peripheral: Peripheral::wrap(self.clone(), peripheral_handle),
            });
        } else {
            warn!("Spurious, unbalanced/redundant PeripheralDisconnected notification from backend");
        }

        Ok(())
    }

    fn on_peripheral_property_set(&self, peripheral_handle: PeripheralHandle, property: BackendPeripheralProperty) -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);

        //
        // XXX: BEWARE: This is a std::sync lock so we need to avoid awaiting or taking too long
        // while we currently have a broad scope for convenience here...
        //
        let mut state_guard = peripheral_state.inner.write().unwrap();

        let mut changed_prop = None;
        //let mut changed_connection_state = false;

        trace!("PeripheralPropertySet: handle={}/{}: {:?}",
                peripheral_handle.0,
                state_guard.address.as_ref().unwrap_or(&Address::MAC(MAC(0))).to_string(),
                property);

        match property {
            BackendPeripheralProperty::Address(address) => {
                // XXX: in terms of the public API the address isn't expected to change for a
                // specific Peripheral so this won't be reported as a PropertyChange
                // Once we have an address and name though we will advertise the Peripheral
                // to the application.
                match &state_guard.address {
                    None => {
                        state_guard.address = Some(address);
                    }
                    Some(existing) => {
                        if &address != existing {
                            log::error!("Spurious change of peripheral address by backend!");
                        }
                    }
                }
            }
            BackendPeripheralProperty::AddressType(address_type) => {
                let new_mac_address_type = match address_type {
                    AddressType::PublicMAC => Some(MacAddressType::Public),
                    AddressType::RandomMAC => Some(MacAddressType::Random),
                    AddressType::String => None,
                };

                let changed = state_guard.address_type == new_mac_address_type;
                if changed {
                    state_guard.address_type = new_mac_address_type;
                    changed_prop = Some(PeripheralPropertyId::AddressType);
                }
            }
            BackendPeripheralProperty::Name(name) => {
                let changed = match &state_guard.name {
                    Some(current_name) => current_name != &name,
                    None => true,
                };
                if changed {
                    state_guard.name = Some(name);
                    changed_prop = Some(PeripheralPropertyId::Name);
                }
            }
            BackendPeripheralProperty::TxPower(tx_power) => {
                if state_guard.tx_power.is_none()
                    || state_guard.tx_power.unwrap() != tx_power
                {
                    state_guard.tx_power = Some(tx_power);
                    changed_prop = Some(PeripheralPropertyId::TxPower);
                }
            }
            BackendPeripheralProperty::Rssi(rssi) => {
                if state_guard.rssi.is_none() || state_guard.rssi.unwrap() != rssi {
                    state_guard.rssi = Some(rssi);
                    changed_prop = Some(PeripheralPropertyId::Rssi);
                }
            }
            BackendPeripheralProperty::ManufacturerData(data) => {
                if !data.is_empty() {
                    // Assume something may have changed without doing a detailed
                    // comparison of state...
                    state_guard.manufacturer_data.extend(data);
                    changed_prop = Some(PeripheralPropertyId::ManufacturerData);
                }
            }
            BackendPeripheralProperty::ServiceData(data) => {
                if !data.is_empty() {
                    // Assume something may have changed without doing a detailed
                    // comparison of state...
                    state_guard.service_data.extend(data);
                    changed_prop = Some(PeripheralPropertyId::ServiceData);
                }
            }
            BackendPeripheralProperty::ServiceIds(uuids) => {
                let mut new_services = false;
                for uuid in uuids {
                    if !state_guard.service_ids.contains(&uuid) {
                        state_guard.service_ids.insert(uuid);
                        new_services = true;
                    }
                }
                if new_services {
                    changed_prop = Some(PeripheralPropertyId::ServiceIds);
                }
            }
        }

        // We wait until we have and address and a name before advertising peripherals
        // to applications...
        if state_guard.advertised == false && state_guard.name != None && state_guard.address != None {
            let _ = self.inner.event_bus.send(Event::PeripheralFound {
                peripheral: Peripheral::wrap(self.clone(), peripheral_handle),
                address: state_guard.address.as_ref().unwrap().to_owned(),
                name: state_guard.name.as_ref().unwrap().clone()
            });
            state_guard.advertised = true;

            // Also notify the app about any other properties we we're already tracking for the peripheral...

            //let _ = session.inner.event_bus.send(Event::DevicePropertyChanged(Peripheral::wrap(session.clone(), peripheral_handle), PeripheralPropertyId::Address));
            let _ = self.inner.event_bus.send(Event::PeripheralPropertyChanged {
                peripheral: Peripheral::wrap(self.clone(), peripheral_handle),
                property_id: PeripheralPropertyId::Name
            });
            if state_guard.address_type.is_some() {
                let _ = self.inner.event_bus.send(Event::PeripheralPropertyChanged {
                    peripheral: Peripheral::wrap(self.clone(), peripheral_handle),
                    property_id: PeripheralPropertyId::AddressType
                });
            }
            if state_guard.tx_power.is_some() {
                let _ = self.inner.event_bus.send(Event::PeripheralPropertyChanged {
                    peripheral: Peripheral::wrap(self.clone(), peripheral_handle),
                    property_id: PeripheralPropertyId::TxPower
                });
            }
            if state_guard.rssi.is_some() {
                let _ = self.inner.event_bus.send(Event::PeripheralPropertyChanged {
                    peripheral: Peripheral::wrap(self.clone(), peripheral_handle),
                    property_id: PeripheralPropertyId::Rssi
                });
            }
            if !state_guard.manufacturer_data.is_empty() {
                let _ = self.inner.event_bus.send(Event::PeripheralPropertyChanged {
                    peripheral: Peripheral::wrap(self.clone(), peripheral_handle),
                    property_id: PeripheralPropertyId::ManufacturerData
                });
            }
            if !state_guard.service_data.is_empty() {
                let _ = self.inner.event_bus.send(Event::PeripheralPropertyChanged {
                    peripheral: Peripheral::wrap(self.clone(), peripheral_handle),
                    property_id: PeripheralPropertyId::ServiceData
                });
            }
            if !state_guard.service_ids.is_empty() {
                let _ = self.inner.event_bus.send(Event::PeripheralPropertyChanged {
                    peripheral: Peripheral::wrap(self.clone(), peripheral_handle),
                    property_id: PeripheralPropertyId::ServiceIds
                });
            }
        }
        /*
        if changed_connection_state {
            if state_guard.is_connected {
                trace!("Notifying peripheral {} connected", state_guard.address.as_ref().unwrap().to_string());
                let _ = self.inner.event_bus.send(Event::PeripheralConnected(Peripheral::wrap(self.clone(), peripheral_handle)));
            } else {
                trace!("Notifying peripheral {} disconnected", state_guard.address.as_ref().unwrap().to_string());
                let _ = self.inner.event_bus.send(Event::PeripheralDisconnected(Peripheral::wrap(self.clone(), peripheral_handle)));
            }
        }
        */
        if let Some(changed_prop) = changed_prop {
            trace!("Notifying property {:?} changed", changed_prop);
            let _ = self.inner.event_bus.send(Event::PeripheralPropertyChanged {
                peripheral: Peripheral::wrap(self.clone(), peripheral_handle),
                property_id: changed_prop,
            });
        }

        Ok(())
    }

    fn on_primary_gatt_service_notification(&self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle, uuid: Uuid) -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);

        //
        // XXX: BEWARE: This is a std::sync lock so we need to avoid awaiting or taking too
        //
        let mut state_guard = peripheral_state.inner.write().unwrap();

        if let Ok((service_state, is_gatt_service_new)) =
            self.get_gatt_service_state(state_guard.borrow_mut(), service_handle) {

            if is_gatt_service_new {
                let peripheral = Peripheral::wrap(self.clone(), peripheral_handle);
                let service = Service::wrap(peripheral.clone(), service_handle);
                let _ = self.inner.event_bus.send(Event::PeripheralPrimaryGattService {
                    peripheral,
                    service,
                    uuid
                });
            }

            // Also register the service uuid
            // FIXME: avoid duplicating this snippet of code...
            let mut new_service_id = false;
            if !state_guard.service_ids.contains(&uuid) {
                state_guard.service_ids.insert(uuid);
                new_service_id = true;
            }
            if new_service_id {
                trace!(
                    "Notifying property {:?} changed",
                    PeripheralPropertyId::ServiceIds
                );
                let _ = self
                    .inner
                    .event_bus
                    .send(Event::PeripheralPropertyChanged {
                        peripheral: Peripheral::wrap(self.clone(), peripheral_handle),
                        property_id: PeripheralPropertyId::ServiceIds,
                    });
            }
        }

        Ok(())
    }

    fn on_primary_gatt_services_complete(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);
        let mut state_guard = peripheral_state.inner.write().unwrap();
        if !state_guard.primary_gatt_services_complete {
            state_guard.primary_gatt_services_complete = true;
            let _ = self.inner.event_bus.send(
                Event::PeripheralPrimaryGattServicesComplete {
                    peripheral: Peripheral::wrap(self.clone(), peripheral_handle),
                },
            );
        }
        Ok(())
    }

    fn on_included_gatt_service_notification(
        &self,
        peripheral_handle: PeripheralHandle,
        parent_service_handle: ServiceHandle,
        included_service_handle: ServiceHandle,
        uuid: Uuid) -> Result<()>
    {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);
        let mut peripheral_state_guard = peripheral_state.inner.write().unwrap();

        match self
            .get_gatt_service_state(&peripheral_state_guard, parent_service_handle)
        {
            Ok((parent_service_state, _)) => {
                let mut parent_service_state_guard =
                    parent_service_state.inner.write().unwrap();

                // XXX: we don't consider whether this is the first time we've seen this
                // service when deciding whether to notify the app, rather we check to see
                // if we've already seen that this service is included by the given parent...

                // XXX: ideally we should try and preserve the order of the includes from the device
                // but it's tricky to preserve the original order seen by the backend in case piecemeal
                // queries are made, matching given uuids
                //
                // For now we just try and preserve the order of events from the backend
                //
                // XXX: we expect fairly tiny arrays so the contains checks should be fine
                if !parent_service_state_guard.included_services.contains(&included_service_handle) {
                    parent_service_state_guard.included_services.push(included_service_handle);

                    let peripheral = Peripheral::wrap(self.clone(), peripheral_handle);
                    let parent_service = Service::wrap(peripheral.clone(), parent_service_handle);
                    let service = Service::wrap(peripheral.clone(), included_service_handle);
                    let _ = self.inner.event_bus.send(Event::ServiceIncludedGattService {
                        peripheral,
                        parent_service,
                        service,
                        uuid
                    });
                }

                // Also register the service uuid
                // FIXME: avoid duplicating this snippet of code...
                let mut new_service_id = false;
                if !peripheral_state_guard.service_ids.contains(&uuid) {
                    peripheral_state_guard.service_ids.insert(uuid);
                    new_service_id = true;
                }
                if new_service_id {
                    trace!("Notifying property {:?} changed", PeripheralPropertyId::ServiceIds);
                    let _ = self.inner.event_bus.send(Event::PeripheralPropertyChanged {
                        peripheral: Peripheral::wrap(self.clone(), peripheral_handle),
                        property_id: PeripheralPropertyId::ServiceIds,
                    });
                }
            }
            Err(_) => {
                log::error!("Spurious service handle {:?}", parent_service_handle);
            }
        }
        Ok(())
    }

    fn on_included_gatt_services_complete(&self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle) -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);
        let peripheral_state_guard = peripheral_state.inner.read().unwrap();

        match self.get_gatt_service_state(&peripheral_state_guard, service_handle) {
            Ok((service_state, _)) => {
                let mut service_state_guard = service_state.inner.write().unwrap();
                if !service_state_guard.included_services_complete {
                    service_state_guard.included_services_complete = true;
                    let peripheral =
                        Peripheral::wrap(self.clone(), peripheral_handle);
                    let service = Service::wrap(peripheral.clone(), service_handle);
                    let _ = self.inner.event_bus.send(
                        Event::ServiceIncludedGattServicesComplete {
                            peripheral,
                            service,
                        },
                    );
                }
            }
            Err(_) => {
                log::error!("Spurious service handle {:?}", service_handle);
            }
        }
        Ok(())
    }

    fn on_gatt_characteristic_notification(
        &self,
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        uuid: Uuid
    ) -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);
        let peripheral_state_guard = peripheral_state.inner.read().unwrap();

        match self.get_gatt_service_state(&peripheral_state_guard, service_handle) {
            Ok((service_state, _)) => {
                let mut service_state_guard = service_state.inner.write().unwrap();

                // We maintain the order of characteristics, according to their handle
                // in case it might be significant for the device / application. This
                // is only done on a best effort basis considering that some backends
                // not not preserve the order.
                if let Err(i) = service_state_guard
                    .characteristics
                    .binary_search(&characteristic_handle)
                {
                    service_state_guard
                        .characteristics
                        .insert(i, characteristic_handle);

                    let peripheral =
                        Peripheral::wrap(self.clone(), peripheral_handle);
                    let service = Service::wrap(peripheral.clone(), service_handle);
                    let characteristic =
                        Characteristic::wrap(service.clone(), characteristic_handle);
                    let _ = self.inner.event_bus.send(
                        Event::ServiceGattCharacteristic {
                            peripheral,
                            service,
                            characteristic,
                            uuid,
                        },
                    );
                }
            }
            Err(_) => {
                log::error!("Spurious service handle {:?}", service_handle);
            }
        }
        Ok(())
    }

    fn on_gatt_characteristics_complete(&self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle) -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);
        let peripheral_state_guard = peripheral_state.inner.read().unwrap();

        match self.get_gatt_service_state(&peripheral_state_guard, service_handle) {
            Ok((service_state, _)) => {
                let mut service_state_guard = service_state.inner.write().unwrap();
                if !service_state_guard.characteristics_complete {
                    service_state_guard.characteristics_complete = true;
                    let peripheral =
                        Peripheral::wrap(self.clone(), peripheral_handle);
                    let service = Service::wrap(peripheral.clone(), service_handle);
                    let _ = self.inner.event_bus.send(
                        Event::ServiceGattCharacteristicsComplete {
                            peripheral,
                            service,
                        },
                    );
                }
            }
            Err(_) => {
                log::error!("Spurious service handle {:?}", service_handle);
            }
        }
        Ok(())
    }

    fn on_gatt_characteristic_value_change_notification(
        &self,
        peripheral_handle: PeripheralHandle,
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        value: Vec<u8>
    ) -> Result<()> {
        let peripheral = Peripheral::wrap(self.clone(), peripheral_handle);
        let service = Service::wrap(peripheral.clone(), service_handle);
        let characteristic = Characteristic::wrap(service.clone(), characteristic_handle);
        let _ = self.inner.event_bus.send(Event::ServiceGattCharacteristicValueNotify {
            peripheral,
            service,
            characteristic,
            value
        });

        // TODO: maintain a cache of the attribute value
        Ok(())
    }

    async fn run_backend_task(
        weak_session_inner: Weak<SessionInner>,
        backend_bus: mpsc::UnboundedReceiver<BackendEvent>,
    ) {
        trace!("Starting task to process backend events from the backend_bus...");

        let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(backend_bus);
        tokio::pin!(stream);
        while let Some(event) = stream.next().await {
            // We only hold a strong reference back to the Session while we're
            // processing a backend event otherwise we would be holding a circular reference...
            let session = match weak_session_inner.upgrade() {
                Some(strong_inner) => Session::wrap(strong_inner),
                None => {
                    trace!("Exiting backend event processor task since Session has been dropped");
                    break;
                }
            };

            match event {
                BackendEvent::PeripheralFound { peripheral_handle } => {
                    session.ensure_peripheral_state(peripheral_handle);
                }
                BackendEvent::PeripheralConnected { peripheral_handle } => {
                    if let Err(err) = session.on_peripheral_connected(peripheral_handle) {
                        log::error!("Error handling peripheral connection event: {:?}", err);
                    }
                }
                BackendEvent::PeripheralDisconnected { peripheral_handle } => {
                    if let Err(err) = session.on_peripheral_disconnected(peripheral_handle) {
                        log::error!("Error handling peripheral disconnect event: {:?}", err);
                    }
                }
                BackendEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property,
                } => {
                    if let Err(err) = session.on_peripheral_property_set(peripheral_handle, property) {
                        log::error!("Error handling peripheral property set event: {:?}", err);
                    }
                }
                BackendEvent::PeripheralPrimaryGattService {
                    peripheral_handle,
                    service_handle,
                    uuid,
                } => {
                    if let Err(err) = session.on_primary_gatt_service_notification(peripheral_handle, service_handle, uuid) {
                        log::error!("Error handling primary gatt service notification: {:?}", err);
                    }
                }
                BackendEvent::PeripheralPrimaryGattServicesComplete {
                    peripheral_handle, ..
                } => {
                    if let Err(err) = session.on_primary_gatt_services_complete(peripheral_handle) {
                        log::error!("Error handling primary gatt services complete notification: {:?}", err);
                    }
                }
                BackendEvent::ServiceIncludedGattService {
                    peripheral_handle,
                    parent_service_handle,
                    included_service_handle,
                    uuid,
                    ..
                } => {
                    if let Err(err) = session.on_included_gatt_service_notification(
                            peripheral_handle, parent_service_handle, included_service_handle, uuid)
                    {
                        log::error!("Error handling included gatt service notification: {:?}", err);
                    }
                }
                BackendEvent::ServiceIncludedGattServicesComplete {
                    peripheral_handle,
                    service_handle,
                    ..
                } => {
                    if let Err(err) = session.on_included_gatt_services_complete(peripheral_handle, service_handle) {
                        log::error!("Error handling included gatt services complete notification: {:?}", err);
                    }
                }
                BackendEvent::ServiceGattCharacteristic {
                    peripheral_handle,
                    service_handle,
                    characteristic_handle,
                    uuid,
                    ..
                } => {
                    if let Err(err) = session.on_gatt_characteristic_notification(
                        peripheral_handle, service_handle, characteristic_handle, uuid)
                    {
                        log::error!("Error handling gatt characteristic notification: {:?}", err);
                    }
                }
                BackendEvent::ServiceGattCharacteristicsComplete {
                    peripheral_handle,
                    service_handle,
                } => {
                    if let Err(err) = session.on_gatt_characteristics_complete(peripheral_handle, service_handle) {
                        log::error!("Error handling gatt characteristics complete notification: {:?}", err);
                    }
                }
                BackendEvent::ServiceGattCharacteristicNotify {
                    peripheral_handle,
                    service_handle,
                    characteristic_handle,
                    value
                } => {
                    if let Err(err) = session.on_gatt_characteristic_value_change_notification(
                        peripheral_handle, service_handle, characteristic_handle, value)
                    {
                        log::error!("Error handling gatt characteristic value change notification: {:?}", err);
                    }
                }
                BackendEvent::Flush(id) => {
                    trace!("backend flush {} received", id);
                    let _ = session.inner.event_bus.send(Event::Flush(id));
                }
            }
        }

        trace!("Finished task processing backend events from the backend_bus");
    }

    /// Returns a stream of bluetooth system events, including peripheral discovery
    /// notifications, property change notifications and connect/disconnect events.
    /// Also see `peripheral_events` which may be convenient when you are only
    /// interested in events related to a single peripheral.
    pub fn events(&self) -> Result<impl Stream<Item = Event>> {
        let receiver = self.inner.event_bus.subscribe();
        Ok(BroadcastStream::new(receiver).filter_map(|x| async move {
            if x.is_ok() {
                Some(x.unwrap())
            } else {
                None
            }
        }))
    }

    /// As a convenience this provides a filtered stream of events that guarantees
    /// any peripheral events will only related to the specified peripheral. Other
    /// events unrelated to peripherals will be delivered, unfiltered.
    pub fn peripheral_events(&self, peripheral: &Peripheral) -> Result<impl Stream<Item = Event>> {
        let filter_handle = peripheral.peripheral_handle;

        Ok(self.events()?.filter_map(move |event| async move {
            match event {
                Event::PeripheralFound{..} => None,
                Event::PeripheralConnected { ref peripheral } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::PeripheralDisconnected { ref peripheral } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::PeripheralPropertyChanged { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::PeripheralPrimaryGattService { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::PeripheralPrimaryGattServicesComplete { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::ServiceIncludedGattService { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::ServiceIncludedGattServicesComplete { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::ServiceGattCharacteristic { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::ServiceGattCharacteristicsComplete { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::ServiceGattCharacteristicValueNotify { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::Flush(_) => Some(event)
            }
        }))
    }

    /// Starts scanning for Bluetooth devices, according to the given filter
    ///
    /// Note: It's an error to try and initiate multiple scans in parallel
    /// considering the varied ways different backends will try to handle
    /// such requests.
    pub async fn start_scanning(&self, filter: Filter) -> Result<()> {
        let mut is_scanning_guard = self.inner.is_scanning.lock().await;

        if *is_scanning_guard {
            return Err(Error::Other(anyhow!("Already scanning")));
        }

        self.inner.backend.api().start_scanning(&filter).await?;
        *is_scanning_guard = true;

        Ok(())
    }

    pub async fn stop_scanning(&self) -> Result<()> {
        let mut is_scanning_guard = self.inner.is_scanning.lock().await;
        if !*is_scanning_guard {
            return Err(Error::Other(anyhow!("Not currently scanning")));
        }

        self.inner.backend.api().stop_scanning().await?;
        *is_scanning_guard = false;

        Ok(())
    }

    pub fn peripherals(&self) -> Result<Vec<Peripheral>> {
        Ok(self.inner.peripherals.iter().map(|item| {
            Peripheral::wrap(self.clone(), *item.key())
        }).collect())
    }

    pub(crate) fn backend_api(&self) -> &dyn BackendSession {
        self.inner.backend.api()
    }
}
