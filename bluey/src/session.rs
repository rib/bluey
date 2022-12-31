use async_trait::async_trait;
use dashmap::DashMap;
use futures::{stream, Stream, StreamExt};
use log::{info, trace, warn};
use std::borrow::BorrowMut;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::marker::PhantomData;
use std::ops::{Add, Deref};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::RwLock as StdRwLock;
use std::sync::{Arc, Weak};
use std::{char, default};
use tokio::sync::Mutex;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

use crate::characteristic::{Characteristic, CharacteristicProperties, WriteType, self};
use crate::descriptor::Descriptor;
use crate::peripheral::{self, Peripheral, WeakPeripheral};
use crate::service::{self, Service};
use crate::{fake, Address, AddressType, BackendPeripheralProperty, Error, Result, MAC, GattError, DescriptorHandle};
use crate::{BackendEvent, Event};
use crate::{
    CacheMode, CharacteristicHandle, MacAddressType, PeripheralHandle, PeripheralPropertyId,
    ServiceHandle,
};

#[cfg(target_os = "windows")]
use crate::winrt;

#[cfg(target_os = "android")]
use crate::android;

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
impl Hash for Session {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::ptr::hash(Arc::<SessionInner>::as_ptr(&self.inner), state);
    }
}
impl Deref for Session {
    type Target = SessionInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[cfg(not(target_os = "android"))]
#[tokio::test]
async fn session_eq() {
    let session0 = SessionConfig::new().start().await.unwrap();
    let session1 = SessionConfig::new().start().await.unwrap();
    assert_ne!(session0, session1);
    assert_eq!(session0, session0.clone());
}

// public for the sake of implementing Deref for ergonomics but since
// no members are public and there's not public API for SessionInner
// we don't really leak anything
#[derive(Debug)]
pub struct SessionInner {
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

    // App-facing peripherals are ref-counted so we can see when the app no longer
    // cares about a peripheral. (and disconnect/free state)
    //
    // We hold weak references to the peripherals which we upgrade whenever we need
    // to hand out a peripheral reference to the application (or create a new
    // app-facing peripheral if we don't have a a weak reference yet)
    //
    // NB: we hold weak references because an app-facing peripheral reference
    // also includes a reference to the Session - so we don't want a ref cycle.
    app_peripherals: DashMap<PeripheralHandle, WeakPeripheral>
}


// Note the entry points into the backend generally provide the full heirarchy
// of associated handles (such as peripheral -> service -> characteristic -> descriptor)
// in case that may simplify state tracking in the backend (by not having to
// track those relationshipts itself)
#[async_trait]
pub(crate) trait BackendSession {
    fn start_scanning(&self, filter: &Filter) -> Result<()>;
    fn stop_scanning(&self) -> Result<()>;

    fn declare_peripheral(&self, address: Address, name: String) -> Result<PeripheralHandle>;

    async fn peripheral_connect(&self, peripheral_handle: PeripheralHandle) -> Result<()>;

    //fn peripheral_is_connected(&self, peripheral_handle: PeripheralHandle) -> Result<bool>;

    async fn peripheral_disconnect(&self, peripheral_handle: PeripheralHandle) -> Result<()>;

    /// When an application drops all its refererences to a Peripheral then
    /// we automatically close any gatt connection, but unlike `peripheral_disconnect`
    /// this API is synchronous (called within Peripheral drop()) and the application
    /// isn't waiting to see the status.
    ///
    /// Note: the actual disconnect doesn't need to happen synchronously, and ideally
    /// it should happen asynchronously.
    fn peripheral_drop_gatt_state(&self, peripheral_handle: PeripheralHandle);

    async fn peripheral_read_rssi(&self, peripheral_handle: PeripheralHandle) -> Result<i16>;

    async fn peripheral_discover_gatt_services(&self, peripheral_handle: PeripheralHandle,
                                               of_interest_hint: Option<Vec<Uuid>>)
                                               -> Result<()>;

    async fn gatt_service_discover_includes(&self, peripheral_handle: PeripheralHandle,
                                            service_handle: ServiceHandle)
                                            -> Result<()>;
    async fn gatt_service_discover_characteristics(&self, peripheral_handle: PeripheralHandle,
                                                   service_handle: ServiceHandle)
                                                   -> Result<()>;

    async fn gatt_characteristic_read(&self, peripheral_handle: PeripheralHandle,
                                      service_handle: ServiceHandle,
                                      characteristic_handle: CharacteristicHandle,
                                      cache_mode: CacheMode)
                                      -> Result<Vec<u8>>;
    async fn gatt_characteristic_write(&self, peripheral_handle: PeripheralHandle,
                                       service_handle: ServiceHandle,
                                       characteristic_handle: CharacteristicHandle,
                                       write_type: WriteType, data: &[u8])
                                       -> Result<()>;

    // We explicitly pass the backend the service_handle here because we want
    // characteristic value notifications sent from the backend to include
    // the service but the backend isn't otherwise expected to have to track
    // relationships between handles
    async fn gatt_characteristic_subscribe(&self, peripheral_handle: PeripheralHandle,
                                           service_handle: ServiceHandle,
                                           characteristic_handle: CharacteristicHandle)
                                           -> Result<()>;
    async fn gatt_characteristic_unsubscribe(&self, peripheral_handle: PeripheralHandle,
                                             service_handle: ServiceHandle,
                                             characteristic_handle: CharacteristicHandle)
                                             -> Result<()>;

    async fn gatt_characteristic_discover_descriptors(&self, peripheral_handle: PeripheralHandle,
                                                      service_handle: ServiceHandle,
                                                      characteristic_handle: CharacteristicHandle)
                                                   -> Result<()>;

    async fn gatt_descriptor_read(&self, peripheral_handle: PeripheralHandle,
                                  service_handle: ServiceHandle,
                                  characteristic_handle: CharacteristicHandle,
                                  descriptor_handle: DescriptorHandle,
                                  cache_mode: CacheMode)
                                  -> Result<Vec<u8>>;
    async fn gatt_descriptor_write(&self, peripheral_handle: PeripheralHandle,
                                   service_handle: ServiceHandle,
                                   characteristic_handle: CharacteristicHandle,
                                   descriptor_handle: DescriptorHandle,
                                   data: &[u8])
                                   -> Result<()>;

    // To make it possible for the frontend cache to be cleared then we need to
    // be able to cope with receiving service or characteristic handles with
    // no context. When recreating state objects we at least need to know the
    // uuids...
    // XXX: for now there's no way to arbitrarily clear cached state so we don't
    // actually need these
    //fn gatt_service_uuid(&self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle)
    //                     -> Result<Uuid>;
    //fn gatt_characteristic_uuid(&self, peripheral_handle: PeripheralHandle,
    //                            characteristic_handle: CharacteristicHandle)
    //                            -> Result<Uuid>;

    fn flush(&self, id: u32) -> Result<()>;
}

#[derive(Debug)]
enum BackendSessionImpl {
    #[cfg(target_os = "windows")]
    Winrt(winrt::session::WinrtSession),
    #[cfg(target_os = "android")]
    Android(android::session::AndroidSession),
    Fake(fake::session::FakeSession),
}
impl BackendSessionImpl {
    fn api(&self) -> &dyn BackendSession {
        match self {
            #[cfg(target_os = "windows")]
            BackendSessionImpl::Winrt(winrt) => winrt,
            #[cfg(target_os = "android")]
            BackendSessionImpl::Android(android) => android,
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
impl Deref for PeripheralState {
    type Target = Arc<StdRwLock<PeripheralStateInner>>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
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
    pub(crate) gatt_services_by_uuid: DashMap<Uuid, ServiceHandle>, // (primary and secondary)
    pub(crate) gatt_primary_services: Vec<ServiceHandle>,
    //pub(crate) gatt_primary_services_complete: bool,
    pub(crate) gatt_characteristics: DashMap<CharacteristicHandle, CharacteristicState>,
    pub(crate) gatt_descriptors: DashMap<DescriptorHandle, DescriptorState>,
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
                gatt_services_by_uuid: DashMap::new(),
                gatt_characteristics: DashMap::new(),
                gatt_descriptors: DashMap::new(),
                gatt_primary_services: vec![],
                //gatt_primary_services_complete: false
            })),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ServiceState {
    pub(crate) inner: Arc<StdRwLock<ServiceStateInner>>,
}
impl ServiceState {
    fn new(uuid: Uuid, is_primary: bool) -> Self {
        Self { inner: Arc::new(StdRwLock::new(ServiceStateInner {
            is_primary,
            uuid,
            included_services: vec![],
            //included_services_complete: false,
            characteristics: vec![],
            //characteristics_complete: false
        })) }
    }
}
impl Deref for ServiceState {
    type Target = Arc<StdRwLock<ServiceStateInner>>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[derive(Debug)]
pub(crate) struct ServiceStateInner {
    pub(crate) is_primary: bool,
    pub(crate) uuid: Uuid,
    pub(crate) included_services: Vec<ServiceHandle>,
    //pub(crate) included_services_complete: bool,

    pub(crate) characteristics: Vec<CharacteristicHandle>,
    //pub(crate) characteristics_complete: bool,
}


#[derive(Clone, Debug)]
pub(crate) struct CharacteristicState {
    pub(crate) inner: Arc<StdRwLock<CharacteristicStateInner>>,
}
impl CharacteristicState {
    fn new(service: ServiceHandle, uuid: Uuid, properties: CharacteristicProperties) -> Self {
        Self {
            inner: Arc::new(StdRwLock::new(CharacteristicStateInner {
                service,
                uuid,
                properties,

                descriptors: vec![],
                //descriptors_complete: false
            }))
        }
    }
}
impl Deref for CharacteristicState {
    type Target = Arc<StdRwLock<CharacteristicStateInner>>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[derive(Debug)]
pub(crate) struct CharacteristicStateInner {
    pub(crate) service: ServiceHandle,
    pub(crate) uuid: Uuid,
    pub(crate) properties: CharacteristicProperties,

    pub(crate) descriptors: Vec<DescriptorHandle>,
    //pub(crate) descriptors_complete: bool,
}


#[derive(Clone, Debug)]
pub(crate) struct DescriptorState {
    pub(crate) inner: Arc<StdRwLock<DescriptorStateInner>>,
}
impl DescriptorState {
    fn new(characteristic: CharacteristicHandle, uuid: Uuid) -> Self {
        Self {
            inner: Arc::new(StdRwLock::new(DescriptorStateInner {
                characteristic,
                uuid,
            }))
        }
    }
}
impl Deref for DescriptorState {
    type Target = Arc<StdRwLock<DescriptorStateInner>>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[derive(Debug)]
pub(crate) struct DescriptorStateInner {
    pub(crate) characteristic: CharacteristicHandle,
    pub(crate) uuid: Uuid,
}

pub struct Filter {
    pub(crate) service_uuids: HashSet<Uuid>
}
impl Filter {
    pub fn new() -> Self {
        Self {
            service_uuids: HashSet::new()
        }
    }
    /*
    pub fn by_address(&mut self, address: Address) -> &mut Self {
        todo!();
        self
    }
    */
    pub fn add_service(&mut self, uuid: Uuid) -> &mut Self {
        self.service_uuids.insert(uuid);

        self
    }
}

impl Default for Filter {
    fn default() -> Self {
        Self::new()
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

#[cfg(target_os="android")]
pub struct AndroidConfig<'a> {
    pub jni_env: jni::JNIEnv<'a>,
    pub activity: jni::objects::JObject<'a>,
    pub companion_chooser_request_code: Option<u32>,

    //lifetime: PhantomData<&'a ()>,
}

/*
impl<'a> AndroidConfig<'a> {
    pub fn new(env: jni::JNIEnv<'a>,
               activity: jni::objects::JObject<'a>,
               companion_chooser_request_code: Option<u32>) -> AndroidConfig<'a> {
        AndroidConfig {
            jni_env: env,
            activity,
            companion_chooser_request_code
            //lifetime: PhantomData::new()
        }
    }
}*/

pub struct SessionConfig<'a> {
    backend: Backend,

    #[cfg(target_os="android")]
    pub android: Option<AndroidConfig<'a>>,

    lifetime: PhantomData<&'a ()>,
}

impl<'a> SessionConfig<'a> {

    #[cfg(not(target_os="android"))]
    pub fn new() -> SessionConfig<'a> {

        SessionConfig {
            backend: Backend::SystemDefault,

            lifetime: PhantomData
        }
    }

    #[cfg(target_os="android")]
    pub fn android_new(env: jni::JNIEnv<'a>,
                       activity: jni::objects::JObject<'a>,
                       companion_chooser_request_code: Option<u32>) -> SessionConfig<'a> {
        SessionConfig {
            backend: Backend::SystemDefault,

            android: Some(AndroidConfig {
                jni_env: env,
                activity,
                companion_chooser_request_code
            }),

            lifetime: PhantomData
        }
    }

    pub fn set_backend(&mut self, backend: Backend) -> &mut Self {
        self.backend = backend;
        self
    }

    /*
    pub fn init_android(&mut self, config: AndroidConfig<'a>) {
        self.android = Some(config);
        self
    }
    */

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

    async fn start(config: SessionConfig<'_>) -> Result<Self> {
        let (broadcast_sender, _) = broadcast::channel(16);

        // Each per-backend backend is responsible for feeding the backend event bus
        // and then we handle state tracking and forwarding corresponding events to
        // the application as necessary
        let (backend_bus_tx, backend_bus_rx) = mpsc::unbounded_channel();
        let backend = match config.backend {
            #[cfg(target_os = "windows")]
            Backend::SystemDefault => {
                let implementation =
                    winrt::session::WinrtSession::new(&config, backend_bus_tx)?;
                BackendSessionImpl::Winrt(implementation)
            }
            #[cfg(target_os = "android")]
            Backend::SystemDefault => {
                let implementation =
                    android::session::AndroidSession::new(&config, backend_bus_tx)?;
                BackendSessionImpl::Android(implementation)
            }
            #[cfg(target_arch = "wasm32")]
            Backend::SystemDefault => {
                let implementation =
                    fake::session::FakeSession::new(&config, backend_bus_tx)?;
                BackendSessionImpl::Fake(implementation)
            }
            Backend::Fake => {
                let implementation =
                    fake::session::FakeSession::new(&config, backend_bus_tx)?;
                BackendSessionImpl::Fake(implementation)
            }
        };
        let session =
            Session { inner: Arc::new(SessionInner { event_bus: broadcast_sender,
                                                     next_flush_index: AtomicU32::new(0),
                                                     //backend_bus: backend_bus_rx,
                                                     backend,
                                                     is_scanning: Mutex::new(false),
                                                     //scan_subscriptions: AtomicU32::new(0),
                                                     peripherals: DashMap::new(),
                                                     app_peripherals: DashMap::new(),
                                                    }) };

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

    pub(crate) fn ensure_peripheral_state(&self, peripheral_handle: PeripheralHandle)
                                          -> PeripheralState {
        match self.peripherals.get(&peripheral_handle) {
            Some(peripheral_state) => peripheral_state.clone(),
            None => {
                let peripheral_state = PeripheralState::new(peripheral_handle);
                self.peripherals
                    .insert(peripheral_handle, peripheral_state.clone());
                peripheral_state
            }
        }
    }

    pub(crate) fn get_peripheral_state(&self, peripheral_handle: PeripheralHandle)
                                       -> PeripheralState {
        match self.peripherals.get(&peripheral_handle) {
            Some(peripheral_state) => peripheral_state.clone(),
            None => {
                log::error!("Spurious notification of a new peripheral {:?}",
                            peripheral_handle);

                // Handle this gracefully by just creating peripheral state on the fly (this would imply
                // a backend bug, not an application bug and it's not obvious what the best way of handling
                // this case would be)
                self.ensure_peripheral_state(peripheral_handle)
            }
        }
    }

    fn get_application_peripheral(&self, peripheral_handle: PeripheralHandle) -> Peripheral {
        let peripheral = match self.app_peripherals.get(&peripheral_handle) {
            Some(weak_peripheral) => {
                weak_peripheral.upgrade()
            }
            None => {
                None
            }
        };

        match peripheral {
            Some(peripheral) => {
                peripheral
            },
            None => {
                let peripheral = Peripheral::new(self.clone(), peripheral_handle);
                let weak_peripheral = peripheral.downgrade();
                self.app_peripherals.insert(peripheral_handle, weak_peripheral);

                peripheral
            }
        }
    }

    pub(crate) fn ensure_gatt_service_state(
        &self,
        peripheral_state: &mut PeripheralStateInner,
        service_handle: ServiceHandle,
        uuid: Uuid,
        is_primary: bool
    ) -> (ServiceState, bool) {
        match peripheral_state.gatt_services.get(&service_handle) {
            Some(service_state) => (service_state.clone(), false),
            None => {
                log::trace!("Creating state for unknown Service {:?}", service_handle);
                //let uuid =
                //    self.backend_api()
                //        .gatt_service_uuid(peripheral_state.peripheral_handle, service_handle)?;
                let service_state = ServiceState::new(uuid, is_primary);
                peripheral_state.gatt_services
                    .insert(service_handle, service_state.clone());
                peripheral_state.gatt_services_by_uuid
                    .insert(uuid, service_handle);

                if is_primary {
                    peripheral_state.gatt_primary_services = peripheral_state.gatt_services
                        .iter()
                        .filter_map(|kv| if kv.value().inner.read().unwrap().is_primary { Some(*kv.key()) } else { None })
                        .collect();
                    peripheral_state.gatt_primary_services.sort();
                }
                (service_state, true)
            }
        }
    }

    pub(crate) fn get_gatt_service_state(&self, peripheral_state: &PeripheralStateInner,
                                         service_handle: ServiceHandle)
                                         -> Result<(ServiceState, bool)> {
        match peripheral_state.gatt_services.get(&service_handle) {
            Some(service_state) => Ok((service_state.clone(), false)),
            None => Err(Error::InvalidStateReference)
        }
    }

    pub(crate) fn ensure_gatt_characteristic_state(&self, peripheral_state: &PeripheralStateInner,
                                                   service_handle: ServiceHandle,
                                                   characteristic_handle: CharacteristicHandle,
                                                   uuid: Uuid,
                                                   properties: CharacteristicProperties)
                                                   -> (CharacteristicState, bool) {
        log::debug!("ensure_gatt_characteristic_state: char_handle={characteristic_handle:?}");
        match peripheral_state.gatt_characteristics.get(&characteristic_handle) {
            Some(characteristic_state) => (characteristic_state.clone(), false),
            None => {
                log::debug!("Creating state for unknown Characteristic {:?}",
                            characteristic_handle);
                //let uuid = self.backend_api()
                //               .gatt_characteristic_uuid(peripheral_state.peripheral_handle,
                //                                         characteristic_handle)?;
                let characteristic_state = CharacteristicState::new(service_handle, uuid, properties);
                peripheral_state.gatt_characteristics
                    .insert(characteristic_handle, characteristic_state.clone());
                (characteristic_state, true)
            }
        }
    }

    pub(crate) fn get_gatt_characteristic_state(&self, peripheral_state: &PeripheralStateInner,
                                                characteristic_handle: CharacteristicHandle)
                                                -> Result<(CharacteristicState, bool)> {
        match peripheral_state.gatt_characteristics.get(&characteristic_handle) {
            Some(characteristic_state) => Ok((characteristic_state.clone(), false)),
            None => Err(Error::InvalidStateReference)
        }
    }

    pub(crate) fn ensure_gatt_descriptor_state(&self, peripheral_state: &PeripheralStateInner,
                                               characteristic_handle: CharacteristicHandle,
                                               descriptor_handle: DescriptorHandle,
                                               uuid: Uuid)
                                               -> (DescriptorState, bool) {
        match peripheral_state.gatt_descriptors.get(&descriptor_handle) {
            Some(descriptor_state) => (descriptor_state.clone(), false),
            None => {
                log::trace!("Creating state for unknown Characteristic {:?}",
                            characteristic_handle);
                //let uuid = self.backend_api()
                //               .gatt_descriptor_uuid(peripheral_state.peripheral_handle,
                //                                     characteristic_handle)?;
                let descriptor_state = DescriptorState::new(characteristic_handle, uuid);
                peripheral_state.gatt_descriptors
                    .insert(descriptor_handle, descriptor_state.clone());
                (descriptor_state, true)
            }
        }
    }

    pub(crate) fn get_gatt_descriptor_state(&self, peripheral_state: &PeripheralStateInner,
                                            descriptor_handle: DescriptorHandle)
                                            -> Result<(DescriptorState, bool)> {
        match peripheral_state.gatt_descriptors.get(&descriptor_handle) {
            Some(descriptor_state) => Ok((descriptor_state.clone(), false)),
            None => Err(Error::InvalidStateReference)
        }
    }

    fn on_peripheral_connected(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);
        let mut state_guard = peripheral_state.write().unwrap();

        if !state_guard.is_connected {
            trace!("PeripheralConnected: handle={}/{}",
                   peripheral_handle.0,
                   state_guard.address
                              .as_ref()
                              .unwrap_or(&Address::MAC(MAC(0)))
                              .to_string());
            state_guard.is_connected = true;

            trace!("Notifying peripheral {} connected",
                   state_guard.address.as_ref().unwrap().to_string());
            let peripheral = self.get_application_peripheral(peripheral_handle);
            let _ = self.event_bus
                .send(Event::PeripheralConnected { peripheral } );
        } else {
            warn!("Spurious, unbalanced/redundant PeripheralConnected notification from backend");
        }

        Ok(())
    }

    fn clear_gatt_state_for_peripheral(state_guard: &mut std::sync::RwLockWriteGuard<PeripheralStateInner>) {
        trace!("Invalidating GATT state");
        state_guard.gatt_services.clear();
        state_guard.gatt_services_by_uuid.clear();
        state_guard.gatt_primary_services.clear();
        state_guard.gatt_characteristics.clear();
        state_guard.gatt_descriptors.clear();
    }

    fn on_peripheral_disconnected(&self, peripheral_handle: PeripheralHandle, error: Option<GattError>) -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);
        let mut state_guard = peripheral_state.write().unwrap();

        trace!("PeripheralDisconnected: handle={}/{}",
               peripheral_handle.0,
               state_guard.address
                          .as_ref()
                          .unwrap_or(&Address::MAC(MAC(0)))
                          .to_string());

        Session::clear_gatt_state_for_peripheral(&mut state_guard);

        // XXX: it's possible that the backend could send redundant disconnect events
        // if it has multiple orthogonal indicators of a disconnect happening (e.g. explicit
        // disconnect callback from OS vs observed IO failure) so we try to normalize this for
        // the application...
        if state_guard.is_connected {
            state_guard.is_connected = false;

            trace!("Notifying peripheral {} disconnected",
                   state_guard.address.as_ref().unwrap().to_string());
            let peripheral = self.get_application_peripheral(peripheral_handle);
            let _ = self.event_bus.send(Event::PeripheralDisconnected {
                peripheral,
                error
            });
        } else {
            warn!("Spurious, unbalanced/redundant PeripheralDisconnected notification from backend");
        }

        Ok(())
    }

    fn on_peripheral_property_set(&self, peripheral_handle: PeripheralHandle,
                                  property: BackendPeripheralProperty)
                                  -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);
        let peripheral = self.get_application_peripheral(peripheral_handle);

        //
        // XXX: BEWARE: This is a std::sync lock so we need to avoid awaiting or taking too long
        // while we currently have a broad scope for convenience here...
        //
        let mut state_guard = peripheral_state.write().unwrap();

        let mut changed_prop = None;
        //let mut changed_connection_state = false;

        trace!("PeripheralPropertySet: handle={}/{}: {:?}",
               peripheral_handle.0,
               state_guard.address
                          .as_ref()
                          .unwrap_or(&Address::MAC(MAC(0)))
                          .to_string(),
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
                if state_guard.tx_power.is_none() || state_guard.tx_power.unwrap() != tx_power {
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
        if !state_guard.advertised
           && state_guard.name.is_some()
           && state_guard.address.is_some()
        {
            let _ =
                self.event_bus
                    .send(Event::PeripheralFound { peripheral: peripheral.clone(),
                                                   address: state_guard.address
                                                                       .as_ref()
                                                                       .unwrap()
                                                                       .to_owned(),
                                                   name: state_guard.name
                                                                    .as_ref()
                                                                    .unwrap()
                                                                    .clone() });
            state_guard.advertised = true;

            // Also notify the app about any other properties we we're already tracking for the peripheral...

            //let _ = session.event_bus.send(Event::DevicePropertyChanged(Peripheral::wrap(session.clone(), peripheral_handle), PeripheralPropertyId::Address));
            let _ = self.event_bus.send(Event::PeripheralPropertyChanged {
                peripheral: peripheral.clone(),
                property_id: PeripheralPropertyId::Name
            });
            if state_guard.address_type.is_some() {
                let _ = self.event_bus.send(Event::PeripheralPropertyChanged {
                    peripheral: peripheral.clone(),
                    property_id: PeripheralPropertyId::AddressType
                });
            }
            if state_guard.tx_power.is_some() {
                let _ = self.event_bus.send(Event::PeripheralPropertyChanged {
                    peripheral: peripheral.clone(),
                    property_id: PeripheralPropertyId::TxPower
                });
            }
            if state_guard.rssi.is_some() {
                let _ = self.event_bus.send(Event::PeripheralPropertyChanged {
                    peripheral: peripheral.clone(),
                    property_id: PeripheralPropertyId::Rssi
                });
            }
            if !state_guard.manufacturer_data.is_empty() {
                let _ = self.event_bus.send(Event::PeripheralPropertyChanged {
                    peripheral: peripheral.clone(),
                    property_id: PeripheralPropertyId::ManufacturerData
                });
            }
            if !state_guard.service_data.is_empty() {
                let _ = self.event_bus.send(Event::PeripheralPropertyChanged {
                    peripheral: peripheral.clone(),
                    property_id: PeripheralPropertyId::ServiceData
                });
            }
            if !state_guard.service_ids.is_empty() {
                let _ = self.event_bus.send(Event::PeripheralPropertyChanged {
                    peripheral: peripheral.clone(),
                    property_id: PeripheralPropertyId::ServiceIds
                });
            }
        }
        /*
        if changed_connection_state {
            if state_guard.is_connected {
                trace!("Notifying peripheral {} connected", state_guard.address.as_ref().unwrap().to_string());
                let _ = self.event_bus.send(Event::PeripheralConnected(Peripheral::wrap(self.clone(), peripheral_handle)));
            } else {
                trace!("Notifying peripheral {} disconnected", state_guard.address.as_ref().unwrap().to_string());
                let _ = self.event_bus.send(Event::PeripheralDisconnected(Peripheral::wrap(self.clone(), peripheral_handle)));
            }
        }
        */
        if let Some(changed_prop) = changed_prop {
            trace!("Notifying property {:?} changed", changed_prop);
            let _ = self.event_bus.send(Event::PeripheralPropertyChanged {
                peripheral,
                property_id: changed_prop,
            });
        }

        Ok(())
    }

    fn on_primary_gatt_service_notification(&self, peripheral_handle: PeripheralHandle,
                                            service_handle: ServiceHandle, uuid: Uuid)
                                            -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);

        //
        // XXX: BEWARE: This is a std::sync lock so we need to avoid awaiting or taking too
        //
        let mut state_guard = peripheral_state.write().unwrap();

        let (service_state, is_gatt_service_new) =
            self.ensure_gatt_service_state(state_guard.borrow_mut(), service_handle, uuid, true /* is_primary */);

        if is_gatt_service_new {
            let peripheral = self.get_application_peripheral(peripheral_handle);
            let service = Service::wrap(peripheral.clone(), service_handle);
            let _ = self.event_bus
                        .send(Event::PeripheralPrimaryGattService { peripheral,
                                                                    service,
                                                                    uuid });
        }

        // Also register the service uuid
        // FIXME: avoid duplicating this snippet of code...
        let mut new_service_id = false;
        if !state_guard.service_ids.contains(&uuid) {
            state_guard.service_ids.insert(uuid);
            new_service_id = true;
        }
        if new_service_id {
            trace!("Notifying property {:?} changed", PeripheralPropertyId::ServiceIds);
            let peripheral = self.get_application_peripheral(peripheral_handle);
            let _ = self
                .event_bus
                .send(Event::PeripheralPropertyChanged {
                    peripheral,
                    property_id: PeripheralPropertyId::ServiceIds,
                });
        }

        Ok(())
    }

    fn on_primary_gatt_services_complete(&self, peripheral_handle: PeripheralHandle, error: Option<GattError>) -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);
        let mut state_guard = peripheral_state.write().unwrap();
        let peripheral = self.get_application_peripheral(peripheral_handle);
        let _ = self.event_bus.send(
            Event::PeripheralPrimaryGattServicesComplete {
                peripheral,
            },
        );
        Ok(())
    }

    fn on_included_gatt_service_notification(&self, peripheral_handle: PeripheralHandle,
                                             parent_service_handle: ServiceHandle,
                                             included_service_handle: ServiceHandle, uuid: Uuid)
                                             -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);
        let mut peripheral_state_guard = peripheral_state.write().unwrap();

        match self.get_gatt_service_state(&peripheral_state_guard, parent_service_handle) {
            Ok((parent_service_state, _)) => {
                let mut parent_service_state_guard = parent_service_state.write().unwrap();

                // XXX: we don't consider whether this is the first time we've seen this
                // service when deciding whether to notify the app, rather we check to see
                // if we've already seen that this service is included by the given parent...
                let (_service_state, _is_gatt_service_new) =
                    self.ensure_gatt_service_state(&mut peripheral_state_guard, included_service_handle, uuid, false /* is_primary */);

                // XXX: ideally we should try and preserve the order of the includes from the device
                // but it's tricky to preserve the original order seen by the backend in case piecemeal
                // queries are made, matching given uuids
                //
                // For now we just try and preserve the order of events from the backend
                //
                // XXX: we expect fairly tiny arrays so the contains checks should be fine
                if !parent_service_state_guard.included_services
                                              .contains(&included_service_handle)
                {
                    parent_service_state_guard.included_services
                                              .push(included_service_handle);

                    let peripheral = self.get_application_peripheral(peripheral_handle);
                    let parent_service = Service::wrap(peripheral.clone(), parent_service_handle);
                    let service = Service::wrap(peripheral.clone(), included_service_handle);
                    let _ = self.event_bus
                                .send(Event::ServiceIncludedGattService { peripheral,
                                                                          parent_service,
                                                                          service,
                                                                          uuid });
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
                    let peripheral = self.get_application_peripheral(peripheral_handle);
                    let _ = self.event_bus.send(Event::PeripheralPropertyChanged {
                        peripheral,
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

    fn on_included_gatt_services_complete(&self, peripheral_handle: PeripheralHandle,
                                          service_handle: ServiceHandle,
                                          error: Option<GattError>)
                                          -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);
        let peripheral_state_guard = peripheral_state.read().unwrap();

        match self.get_gatt_service_state(&peripheral_state_guard, service_handle) {
            Ok((service_state, _)) => {
                let mut service_state_guard = service_state.write().unwrap();
                let peripheral = self.get_application_peripheral(peripheral_handle);
                let service = Service::wrap(peripheral.clone(), service_handle);
                let _ = self.event_bus
                            .send(Event::ServiceIncludedGattServicesComplete { peripheral,
                                                                                service });
            }
            Err(_) => {
                log::error!("Spurious service handle {:?}", service_handle);
            }
        }
        Ok(())
    }

    fn on_gatt_characteristic_notification(&self, peripheral_handle: PeripheralHandle,
                                           service_handle: ServiceHandle,
                                           characteristic_handle: CharacteristicHandle,
                                           uuid: Uuid,
                                           properties: CharacteristicProperties)
                                           -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);
        let peripheral_state_guard = peripheral_state.read().unwrap();

        match self.get_gatt_service_state(&peripheral_state_guard, service_handle) {
            Ok((service_state, _)) => {
                let mut service_state_guard = service_state.write().unwrap();

                let (characteristic_state, is_gatt_characteristic_new) =
                    self.ensure_gatt_characteristic_state(&peripheral_state_guard,
                        service_handle, characteristic_handle, uuid, properties);

                // We maintain the order of characteristics, according to their handle
                // in case it might be significant for the device / application. This
                // is only done on a best effort basis considering that some backends
                // do not preserve the order.
                if let Err(i) = service_state_guard.characteristics
                                                .binary_search(&characteristic_handle)
                {
                    service_state_guard.characteristics
                                    .insert(i, characteristic_handle);

                    let peripheral = self.get_application_peripheral(peripheral_handle);
                    let service = Service::wrap(peripheral.clone(), service_handle);
                    let characteristic =
                        Characteristic::wrap(peripheral.clone(), characteristic_handle);
                    log::debug!("Notifying app of new characteristic {characteristic_handle:?}");
                    let _ = self.event_bus
                                .send(Event::ServiceGattCharacteristic {
                                    peripheral,
                                    service,
                                    characteristic,
                                    uuid
                                });
                }
            }
            Err(_) => {
                log::error!("Spurious service handle {:?}", service_handle);
            }
        }
        Ok(())
    }

    fn on_gatt_characteristics_complete(&self, peripheral_handle: PeripheralHandle,
                                        service_handle: ServiceHandle,
                                        error: Option<GattError>)
                                        -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);
        let peripheral_state_guard = peripheral_state.read().unwrap();

        match self.get_gatt_service_state(&peripheral_state_guard, service_handle) {
            Ok((service_state, _)) => {
                let mut service_state_guard = service_state.write().unwrap();
                let peripheral = self.get_application_peripheral(peripheral_handle);
                let service = Service::wrap(peripheral.clone(), service_handle);
                let _ = self.event_bus
                            .send(Event::ServiceGattCharacteristicsComplete { peripheral,
                                                                                service });
            }
            Err(_) => {
                log::error!("Spurious service handle {:?}", service_handle);
            }
        }
        Ok(())
    }

    fn on_gatt_characteristic_value_change_notification(&self,
                                                        peripheral_handle: PeripheralHandle,
                                                        service_handle: ServiceHandle,
                                                        characteristic_handle: CharacteristicHandle,
                                                        value: Vec<u8>)
                                                        -> Result<()> {
        let peripheral = self.get_application_peripheral(peripheral_handle);
        let service = Service::wrap(peripheral.clone(), service_handle);
        let characteristic = Characteristic::wrap(peripheral.clone(), characteristic_handle);
        let _ = self.event_bus
                    .send(Event::ServiceGattCharacteristicValueNotify { peripheral,
                                                                        service,
                                                                        characteristic,
                                                                        value });

        // TODO: maintain a cache of the attribute value
        Ok(())
    }

    fn on_gatt_descriptor_notification(&self, peripheral_handle: PeripheralHandle,
                                       service_handle: ServiceHandle,
                                       characteristic_handle: CharacteristicHandle,
                                       descriptor_handle: DescriptorHandle,
                                       uuid: Uuid)
                                       -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);
        let peripheral_state_guard = peripheral_state.read().unwrap();

        match self.get_gatt_characteristic_state(&peripheral_state_guard, characteristic_handle) {
            Ok((characteristic_state, _)) => {
                let mut characteristic_state_guard = characteristic_state.write().unwrap();

                let (descriptor_state, is_gatt_descriptor_new) =
                    self.ensure_gatt_descriptor_state(&peripheral_state_guard,
                        characteristic_handle, descriptor_handle, uuid);

                // We maintain the order of descriptors, according to their handle
                // in case it might be significant for the device / application. This
                // is only done on a best effort basis considering that some backends
                // do not preserve the order.
                if let Err(i) = characteristic_state_guard.descriptors
                                                .binary_search(&descriptor_handle)
                {
                    characteristic_state_guard.descriptors.insert(i, descriptor_handle);

                    let peripheral = self.get_application_peripheral(peripheral_handle);
                    let service = Service::wrap(peripheral.clone(), service_handle);
                    let characteristic = Characteristic::wrap(peripheral.clone(), characteristic_handle);
                    let descriptor = Descriptor::wrap(peripheral.clone(), descriptor_handle);
                    let _ = self.event_bus
                                .send(Event::ServiceGattDescriptor { peripheral,
                                                                     service,
                                                                     characteristic,
                                                                     descriptor,
                                                                     uuid });
                }
            }
            Err(_) => {
                log::error!("Spurious service handle {:?}", service_handle);
            }
        }
        Ok(())
    }

    fn on_gatt_descriptors_complete(&self, peripheral_handle: PeripheralHandle,
                                    service_handle: ServiceHandle,
                                    characteristic_handle: CharacteristicHandle,
                                    error: Option<GattError>)
                                    -> Result<()> {
        let peripheral_state = self.get_peripheral_state(peripheral_handle);
        let peripheral_state_guard = peripheral_state.read().unwrap();

        match self.get_gatt_characteristic_state(&peripheral_state_guard, characteristic_handle) {
            Ok((characteristic_state, _)) => {
                let mut characteristic_state_guard = characteristic_state.write().unwrap();
                let peripheral = self.get_application_peripheral(peripheral_handle);
                let service = Service::wrap(peripheral.clone(), service_handle);
                let characteristic = Characteristic::wrap(peripheral.clone(), characteristic_handle);
                let _ = self.event_bus
                            .send(Event::ServiceGattDescriptorsComplete {
                                peripheral,
                                service,
                                characteristic
                            });
            }
            Err(_) => {
                log::error!("Spurious service handle {:?}", service_handle);
            }
        }
        Ok(())
    }

    fn on_connect_failed(&self, peripheral_handle: PeripheralHandle, error: Option<GattError>) -> Result<()> {
        let peripheral = self.get_application_peripheral(peripheral_handle);
        let _ = self.event_bus
            .send(Event::PeripheralFailedToConnect {
                peripheral,
                error
            });

        Ok(())
    }

    // If all strong references to a Peripheral (app-facing) are dropped then we can
    // assume the app is no longer interested in the Peripheral and potentially
    // free cached state or disconnect the peripheral from Gatt if necessary...
    //
    // Note: If we are scanning then we probably don't want to free any cached state
    // even if the app isn't showing an interest in a peripheral since they may only
    // start showing an interest after it advertises some service they care about.
    //
    // For now we just use this callback to close any Gatt connection
    pub(crate) fn on_peripheral_drop(&self, peripheral_handle: PeripheralHandle) {
        trace!("Peripheral {:?} dropped by application", peripheral_handle);

        /*
        let peripheral_state = self.get_peripheral_state(peripheral_handle);
        let is_connected = {
            let guard =  peripheral_state.read().unwrap();
            guard.is_connected
        };
        if is_connected {
            self.backend_api()
                .peripheral_disconnect(peripheral_handle);
        }
        */
        self.backend_api()
            .peripheral_drop_gatt_state(peripheral_handle);
    }

    async fn run_backend_task(weak_session_inner: Weak<SessionInner>,
                              backend_bus: mpsc::UnboundedReceiver<BackendEvent>) {
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
                BackendEvent::PeripheralDisconnected { peripheral_handle, error } => {
                    if let Err(err) = session.on_peripheral_disconnected(peripheral_handle, error) {
                        log::error!("Error handling peripheral disconnect event: {:?}", err);
                    }
                }
                BackendEvent::PeripheralPropertySet { peripheral_handle,
                                                      property, } => {
                    if let Err(err) =
                        session.on_peripheral_property_set(peripheral_handle, property)
                    {
                        log::error!("Error handling peripheral property set event: {:?}", err);
                    }
                }
                BackendEvent::GattService { peripheral_handle,
                                                             service_handle,
                                                             uuid, } => {
                    if let Err(err) =
                        session.on_primary_gatt_service_notification(peripheral_handle,
                                                                     service_handle,
                                                                     uuid)
                    {
                        log::error!("Error handling primary gatt service notification: {:?}",
                                    err);
                    }
                }
                BackendEvent::GattServicesComplete { peripheral_handle, error } => {
                    if let Err(err) = session.on_primary_gatt_services_complete(peripheral_handle, error) {
                        log::error!("Error handling primary gatt services complete notification: {:?}", err);
                    }
                }
                BackendEvent::GattIncludedService { peripheral_handle,
                                                           parent_service_handle,
                                                           included_service_handle,
                                                           uuid,
                                                           .. } => {
                    if let Err(err) =
                        session.on_included_gatt_service_notification(peripheral_handle,
                                                                      parent_service_handle,
                                                                      included_service_handle,
                                                                      uuid)
                    {
                        log::error!("Error handling included gatt service notification: {:?}",
                                    err);
                    }
                }
                BackendEvent::GattIncludedServicesComplete { peripheral_handle,
                                                                    service_handle,
                                                                    error } => {
                    if let Err(err) = session.on_included_gatt_services_complete(peripheral_handle,
                                                                                        service_handle,
                                                                                        error)
                    {
                        log::error!("Error handling included gatt services complete notification: {:?}", err);
                    }
                }
                BackendEvent::GattCharacteristic { peripheral_handle,
                                                          service_handle,
                                                          characteristic_handle,
                                                          uuid,
                                                          properties,
                                                          .. } => {
                    if let Err(err) =
                        session.on_gatt_characteristic_notification(peripheral_handle,
                                                                    service_handle,
                                                                    characteristic_handle,
                                                                    uuid,
                                                                    properties)
                    {
                        log::error!("Error handling gatt characteristic notification: {:?}", err);
                    }
                }
                BackendEvent::GattCharacteristicsComplete { peripheral_handle,
                                                                   service_handle,
                                                                   error } => {
                    if let Err(err) =
                        session.on_gatt_characteristics_complete(peripheral_handle, service_handle, error)
                    {
                        log::error!("Error handling gatt characteristics complete notification: {:?}", err);
                    }
                }
                BackendEvent::GattCharacteristicNotify { peripheral_handle,
                                                                service_handle,
                                                                characteristic_handle,
                                                                value, } => {
                    if let Err(err) = session.on_gatt_characteristic_value_change_notification(
                        peripheral_handle, service_handle, characteristic_handle, value)
                    {
                        log::error!("Error handling gatt characteristic value change notification: {:?}", err);
                    }
                }
                BackendEvent::GattDescriptor { peripheral_handle,
                                                      service_handle,
                                                      characteristic_handle,
                                                      descriptor_handle,
                                                      uuid,
                                                      .. } => {
                    if let Err(err) =
                        session.on_gatt_descriptor_notification(peripheral_handle,
                                                                service_handle,
                                                                characteristic_handle,
                                                                descriptor_handle,
                                                                uuid)
                    {
                        log::error!("Error handling gatt characteristic notification: {:?}", err);
                    }
                }
                BackendEvent::GattDescriptorsComplete { peripheral_handle,
                                                               service_handle,
                                                               characteristic_handle,
                                                               error } => {
                    if let Err(err) =
                        session.on_gatt_descriptors_complete(peripheral_handle, service_handle, characteristic_handle, error)
                    {
                        log::error!("Error handling gatt characteristics complete notification: {:?}", err);
                    }
                }
                BackendEvent::Flush(id) => {
                    trace!("backend flush {} received", id);
                    let _ = session.event_bus.send(Event::Flush(id));
                }
                BackendEvent::PeripheralConnectionFailed { peripheral_handle, error } => {
                    if let Err(err) = session.on_connect_failed(peripheral_handle, error) {
                        log::error!("Error handling connection failure notification: {err:?}");
                    }
                },
                BackendEvent::GattCharacteristicWriteNotify { peripheral_handle, service_handle, characteristic_handle } => todo!(),
                BackendEvent::GattDescriptorWriteNotify { peripheral_handle, service_handle, characteristic_handle, descriptor_handle } => todo!(),
                BackendEvent::GattDescriptorNotify { peripheral_handle, service_handle, characteristic_handle, descriptor_handle, value } => todo!(),
            }
        }

        trace!("Finished task processing backend events from the backend_bus");
    }

    /// Returns a stream of bluetooth system events, including peripheral discovery
    /// notifications, property change notifications and connect/disconnect events.
    /// Also see `peripheral_events` which may be convenient when you are only
    /// interested in events related to a single peripheral.
    pub fn events(&self) -> Result<impl Stream<Item = Event>> {
        let receiver = self.event_bus.subscribe();
        Ok(BroadcastStream::new(receiver).filter_map(|x| async move {
                                             if let Ok(x) = x {
                                                 Some(x)
                                             } else {
                                                 None
                                             }
                                         }))
    }

    /// As a convenience this provides a filtered stream of events that guarantees
    /// any peripheral events will only related to the specified peripheral. Other
    /// events unrelated to peripherals will be delivered, unfiltered.
    #[rustfmt::skip]
    pub fn peripheral_events(&self, peripheral: &Peripheral) -> Result<impl Stream<Item = Event>> {
        let filter_handle = peripheral.peripheral_handle;

        Ok(self.events()?.filter_map(move |event| async move {
            match event {
                Event::PeripheralFound{..} => None,
                Event::PeripheralConnected { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::PeripheralFailedToConnect { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::PeripheralDisconnected { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::PeripheralPropertyChanged { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::PeripheralPrimaryGattService { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::PeripheralPrimaryGattServicesComplete { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::ServiceIncludedGattService { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::ServiceIncludedGattServicesComplete { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::ServiceGattCharacteristic { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::ServiceGattCharacteristicsComplete { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::ServiceGattCharacteristicValueNotify { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::ServiceGattDescriptor { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
                Event::ServiceGattDescriptorsComplete { ref peripheral, .. } => { if peripheral.peripheral_handle == filter_handle { Some(event) } else { None } },
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
        let mut is_scanning_guard = self.is_scanning.lock().await;

        if *is_scanning_guard {
            return Err(Error::Other(anyhow!("Already scanning")));
        }

        self.inner.backend.api().start_scanning(&filter)?;
        *is_scanning_guard = true;

        Ok(())
    }

    pub async fn stop_scanning(&self) -> Result<()> {
        let mut is_scanning_guard = self.is_scanning.lock().await;
        if !*is_scanning_guard {
            return Err(Error::Other(anyhow!("Not currently scanning")));
        }

        self.backend.api().stop_scanning()?;
        *is_scanning_guard = false;

        Ok(())
    }

    pub fn peripherals(&self) -> Result<Vec<Peripheral>> {
        Ok(self.peripherals
               .iter()
               .map(|item| self.get_application_peripheral(*item.key()) )
               .collect())
    }

    pub fn declare_peripheral(&self, address: Address, name: String) -> Result<Peripheral> {
        let peripheral_handle = self.backend_api().declare_peripheral(address, name)?;

        Ok(self.get_application_peripheral(peripheral_handle))
    }

    pub(crate) fn backend_api(&self) -> &dyn BackendSession {
        self.backend.api()
    }
}
