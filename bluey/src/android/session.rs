#![allow(unused)]

use core::fmt;
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::atomic::{AtomicIsize, Ordering, AtomicU32, AtomicBool};
use std::sync::{Weak, Arc, Once};
use std::marker::PhantomData;
use std::sync::Mutex as StdMutex;
use std::sync::RwLock as StdRwLock;
use std::time::{Instant, Duration};
use const_format::concatcp;

use async_trait::async_trait;
use dashmap::lock::RwLockReadGuard;
use futures::stream::Fuse;
use futures::{stream, Stream, StreamExt};

use function_name::named;

use dashmap::DashMap;

use jni::{sys::*, NativeMethod};
use tokio::sync::{OwnedMutexGuard, Mutex};
use tokio::sync::{broadcast, mpsc, mpsc::UnboundedSender};
use tokio_stream::wrappers::{BroadcastStream, UnboundedReceiverStream};

use log::{debug, info, trace, warn, error};
use anyhow::anyhow;

use jni::objects::{JObject, JClass, GlobalRef, JMethodID, JList, JValue, JString};
use jni::JNIEnv;
use uuid::Uuid;
use crate::android::jni::*;

use crate::service::Service;
use crate::session::{BackendSession, Filter, Session, SessionConfig};
use crate::{characteristic, peripheral, GattError, DescriptorHandle, try_u64_from_mac48_str};
use crate::{
    fake, uuid::BluetoothUuid, AddressType, BackendPeripheralProperty, Error,
    MacAddressType, Result,
};
use crate::{Address, MAC};
use crate::{BackendEvent, CacheMode, CharacteristicHandle, PeripheralHandle, ServiceHandle};
use crate::characteristic::{WriteType, CharacteristicProperties};

// NB usefull references:
// https://android.googlesource.com/platform/frameworks/base/+/refs/heads/android10-release/core/java/android/bluetooth/BluetoothGatt.java
// https://android.googlesource.com/platform/packages/apps/Bluetooth/+/refs/heads/android10-mainline-release/src/com/android/bluetooth/gatt/GattService.java


static REGISTER_NATIVE_METHODS: Once = Once::new();

const BLE_SESSION_CLASS_NAME: &'static str = "co.bluey.BleSession";
const BLE_DEVICE_CLASS_NAME: &'static str = "co.bluey.BleDevice";
const BLE_DEVICE_JNI_TYPE: &'static str = "Lco/bluey/BleDevice;";

impl From<jni::errors::Error> for Error {
    fn from(jerr: jni::errors::Error) -> Self {
        Error::Other(anyhow!(jerr))
    }
}

impl<T> From<tokio::sync::mpsc::error::SendError<T>> for Error {
    fn from(err: tokio::sync::mpsc::error::SendError<T>) -> Self {
        Error::Other(anyhow!("IO Error: {}", err))
    }
}

impl From<tokio::sync::oneshot::error::RecvError> for Error {
    fn from(err: tokio::sync::oneshot::error::RecvError) -> Self {
        Error::Other(anyhow!(err))
    }
}

impl From<uuid::Error> for Error {
    fn from(err: uuid::Error) -> Self {
        Error::Other(anyhow!(err))
    }
}

impl From<AndroidGattStatus> for Option<GattError> {
    fn from(status: AndroidGattStatus) -> Self {
        match status {
            AndroidGattStatus::Success => None,
            AndroidGattStatus::InsufAuthentication => Some(GattError::InsufficientAuthentication),
            AndroidGattStatus::InsufAuthorization => Some(GattError::InsufficientAuthorization),
            AndroidGattStatus::InsufEncryption => Some(GattError::InsufficientEncryption),
            AndroidGattStatus::WriteNotPermit => Some(GattError::WriteNotPermitted),
            AndroidGattStatus::ReadNotPermit => Some(GattError::ReadNotPermitted),
            AndroidGattStatus::Congested => Some(GattError::Congested),
            _ => Some(GattError::GeneralFailure(format!("{:?}", status))),
        }
    }
}

impl Into<jni::sys::jvalue> for PeripheralHandle {
    fn into(self) -> jni::sys::jvalue {
        jni::sys::jvalue { j: self.0 as i64 }
    }
}

impl Into<jvalue> for WriteType {
    fn into(self) -> jvalue {
        match self {
            WriteType::WithoutResponse => { jvalue { i: 1 }},
            WriteType::WithResponse => { jvalue { i: 2 }},
            //WriteType::Signed => { jvalue { i: 4 }},
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AndroidSession {
    inner: Arc<AndroidSessionInner>,
}

// Compile time assert that the AndroidSession is Send + Sync, considering we
// expect asynchronous callbacks from Java that may come from unknown threads
const _: () = {
    fn ensure_send_sync<T: Send + Sync>() {}
    fn assert() {
        ensure_send_sync::<AndroidSession>();
    }
};

impl Deref for AndroidSession {
    type Target = Arc<AndroidSessionInner>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// Allow us to maintain a (weak) reference to the AndroidSessionInner state that
/// we can pass as a `long` handle over JNI.
///
/// NB: Even though it's a weak reference we still need to make sure we
/// explicitly drop the handle when dropping the AndroidSessionInner otherwise
/// the backing storage for the Arc won't actually be freed!
///
/// NB: Considering that an AndroidSession will initially be associated with
/// a ::default (null) handle, we are careful to consider the possibility of a
/// null handle that's not associated with any weak reference.
impl IntoJHandle for AndroidSession {
    unsafe fn into_weak_handle(this: Self) -> JHandle<Self> {
        let weak = Arc::downgrade(&this.inner);
        let inner_ptr = weak.into_raw();
        let inner_handle = inner_ptr as jni::sys::jlong;
        JHandle { handle: inner_handle, phantom: PhantomData }
    }

    unsafe fn clone_from_weak_handle(handle: JHandle<Self>) -> Option<Self> {
        let inner_ptr = handle.handle as *const AndroidSessionInner;
        if inner_ptr.is_null() {
            return None
        }

        let weak = Weak::from_raw(inner_ptr);

        let ret = match (&weak).upgrade() {
            Some(arc) => {
                let session = Self {
                    inner: arc,
                };
                //debug!("refs within clone_from_weak_handle, after Arc::from_raw(): strong = {}, weak = {}", Arc::strong_count(&session.inner), Arc::weak_count(&session.inner));
                Some(session)
            }
            None => None
        };


        // Convert back into a ptr since we don't want the weak reference to be dropped yet...
        let _inner_ptr = weak.into_raw();

        if let Some(session) = ret {
            //debug!("refs within clone_from_weak_handle, after weak.into_raw(): strong = {}, weak = {}", Arc::strong_count(&session.inner), Arc::weak_count(&session.inner));
            Some(session)
        } else {
            None
        }
    }

    unsafe fn drop_weak_handle(handle: JHandle<Self>) {
        let inner_ptr = handle.handle as *const AndroidSessionInner;
        if ! inner_ptr.is_null() {
            let weak = Weak::from_raw(inner_ptr);

            //debug!("AndroidSession ref count after weak handle dropped = {}", weak.strong_count());
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum AndroidBondState {
    None, // 10 / 0xa
    Bonding, // 11 / 0xb
    Bonded, // 12 // 0xc
}
impl From<i32> for AndroidBondState {
    fn from(val: i32) -> Self {
        match val {
            10 => AndroidBondState::None,
            11 => AndroidBondState::Bonding,
            12 => AndroidBondState::Bonded,
            _ => unreachable!()
        }
    }
}

//#[derive(Debug)]
pub(crate) struct AndroidSessionInner {
    jvm: jni::JavaVM,
    activity_ref: jni::objects::GlobalRef,
    jsession: jni::objects::GlobalRef,
    weak_handle: StdRwLock<JHandle<AndroidSession>>,

    device_class: jni::objects::GlobalRef,

    backend_bus: mpsc::UnboundedSender<BackendEvent>,
    peripheral_handles_by_mac: DashMap<u64, PeripheralHandle>,
    peripherals_by_handle: DashMap<PeripheralHandle, AndroidPeripheral>,
    next_peripheral_handle: AtomicU32,

    // Note we have to cache the method IDs with our own wrapper type (See
    // android::jni::JMethodID) because jni-rs adds a lifetime to its
    // jni::object::JMethodID to stop it getting GC'd (which looks like a
    // copy and paste for how JObject is wrapped, without considering that
    // a method ID isn't an object managed by the GC)
    set_native_handle_method: JMethodID,

    scanner_config_reset_method: JMethodID,
    scanner_config_add_service_uuid_method: JMethodID,
    start_scanning_method: JMethodID,
    stop_scanning_method: JMethodID,

    scan_result_get_local_name_method: JMethodID,
    scan_result_get_specific_manufacturer_data_count_method: JMethodID,
    scan_result_get_specific_manufacturer_data_id_method: JMethodID,
    scan_result_get_specific_manufacturer_data_method: JMethodID,
    scan_result_get_service_uuids_count_method: JMethodID,
    scan_result_get_nth_service_uuid_method: JMethodID,
    scan_result_get_tx_power_method: JMethodID,
    scan_result_get_rssi_method: JMethodID,

    device_get_address_method: JMethodID, // XXX: Takes BluetoothDevice not BleDevice!

    get_device_for_address_method: JMethodID, // Returns a BleDevice

    connect_device_gatt_method: JMethodID,
    reconnect_device_gatt_method: JMethodID,
    disconnect_device_gatt_method: JMethodID,
    close_device_gatt_method: JMethodID,

    get_device_name_method: JMethodID,
    get_device_bond_state_method: JMethodID,
    read_device_rssi_method: JMethodID,
    discover_device_services_method: JMethodID,
    get_device_services_method: JMethodID,
    clear_device_gatt_state_method: JMethodID,

    get_service_instance_id_method: JMethodID,
    get_service_uuid_method: JMethodID,
    get_service_includes_method: JMethodID,
    get_service_characteristics_method: JMethodID,

    get_characteristic_instance_id_method: JMethodID,
    get_characteristic_uuid_method: JMethodID,
    get_characteristic_properties_method: JMethodID,
    get_characteristic_descriptors_method: JMethodID,

    characteristic_read_method: JMethodID,
    characteristic_write_method: JMethodID,
    characteristic_subscribe_method: JMethodID,
    characteristic_unsubscribe_method: JMethodID,

    //add_descriptor_handle_method: JMethodID,
    get_descriptor_id_method: JMethodID,
    get_descriptor_uuid_method: JMethodID,

    descriptor_read_method: JMethodID,
    descriptor_write_method: JMethodID,
}

impl Drop for AndroidSessionInner {
    fn drop(&mut self) {
        debug!("Dropping AndroidSessionInner");

        // Just in case there is anything asynchronous happening in Java that might
        // keep our Java BleSession alive longer than the native session we
        // explicitly detach the native handle...
        if let Ok(jenv) = self.jvm.get_env() {
            try_call_void_method(jenv, self.jsession.as_obj(), self.set_native_handle_method, &[jni::objects::JValue::Long(0).to_jni()]);
            debug!("Explicitly detached native handle from BleSession");
        }

        let weak_handle = *self.weak_handle.read().unwrap();

        // We need to explicitly convert the weak handle that we gave to Java
        // back into a Weak<AndroidSessionInner> and let that drop to ensure
        // the backing storage for the Arc itself will really be freed...
        //
        // Note: this will gracefully handle the situation where the weak_handle hasn't
        // yet been allocated (is_null()) in case there is some error while initializing
        // the AndroidSession
        unsafe { IntoJHandle::drop_weak_handle(weak_handle); }
    }
}


impl fmt::Debug for AndroidSessionInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AndroidSessionInner")
            //.field("ctx_ref", &self.ctx_ref)
            //.field("jsession", &self.jsession)
            .finish()
    }
}

#[derive(Clone, Debug)]
struct AndroidPeripheral {
    inner: Arc<AndroidPeripheralInner>,
}
impl Deref for AndroidPeripheral {
    type Target = AndroidPeripheralInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[derive(Debug)]
enum IOCmd {
    RequestConnect {
        //result_tx: tokio::sync::oneshot::Sender<Result<()>>,
    },

    ConnectionStatusNotify {
        connected: bool,
        status: AndroidGattStatus
    },
    BondingStateNotify {
        bond_state: AndroidBondState,
    },
    ReadRSSI {
        result_tx: tokio::sync::oneshot::Sender<Result<i16>>,
    },
    FinishReadRSSI {
        rssi: i16,
        status: AndroidGattStatus
    },
    // Note: we sent a result after starting to discover services so we
    // can report any error with starting the request - we don't wait
    // until all the services are discovered (there will be a separate
    // event delivered for that)
    RequestDiscoverServices {
        //result_tx: tokio::sync::oneshot::Sender<Result<()>>,
    },
    FinishDiscoverServices {
        status: AndroidGattStatus
    },

    RequestReadCharacteristic {
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        result_tx: tokio::sync::oneshot::Sender<Result<Vec<u8>>>,
    },
    FinishReadCharacteristic {
        characteristic_handle: CharacteristicHandle,
        status: AndroidGattStatus,
        value: Vec<u8>
    },

    RequestWriteCharacteristic {
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        value: Vec<u8>,
        write_type: crate::characteristic::WriteType,
        //result_tx: tokio::sync::oneshot::Sender<Result<()>>,
    },
    FinishWriteCharacteristic {
        characteristic_handle: CharacteristicHandle,
        status: AndroidGattStatus
    },

    RequestSubscribeCharacteristic {
        characteristic_handle: CharacteristicHandle,
        //result_tx: tokio::sync::oneshot::Sender<Result<()>>,
    },
    RequestUnsubscribeCharacteristic {
        characteristic_handle: CharacteristicHandle,
        //result_tx: tokio::sync::oneshot::Sender<Result<()>>,
    },
    CharacteristicNotify {
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        value: Vec<u8>
    },

    RequestReadDescriptor {
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        descriptor_handle: DescriptorHandle,
        result_tx: tokio::sync::oneshot::Sender<Result<Vec<u8>>>,
    },
    FinishReadDescriptor {
        descriptor_handle: DescriptorHandle,
        status: AndroidGattStatus,
        value: Vec<u8>
    },

    RequestWriteDescriptor {
        service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
        descriptor_handle: DescriptorHandle,
        value: Vec<u8>,
        //result_tx: tokio::sync::oneshot::Sender<Result<()>>,
    },
    FinishWriteDescriptor {
        descriptor_handle: DescriptorHandle,
        status: AndroidGattStatus
    },
}


#[derive(Debug)]
struct AndroidPeripheralState {
    address: u64,

    // Only used when an application explicitly connects to the peripheral...
    ble_device: Option<GlobalRef>,

    // We we first instantiate a BleDevice in Java it doesn't have an associated
    // BluetoothGatt instance. It's only after we initiate a request to connect
    // that we register callbacks and get a BluetoothGatt instance. (Note:
    // the connection doesn't need to complete before we have a BluetoothGatt
    // instance - and it can be used to cancel a connection request.)
    has_gatt_interface: bool,

    connected: bool,

    // NB: we don't index services by uuid since it's possible for a device to
    // instantiate a service multiple times - each with the same uuid - but
    // with a different ATT handle.
    //
    // FIXME: switch this to a regular HashMap since we anyway have to lock
    // the peripheral inner to access
    gatt_services: DashMap<ServiceHandle, GlobalRef>,

    // Similar to services, it's possible for a characteristic to be instantiated
    // multiple times per service by a device so we differentiate them by their
    // ATT handle.
    //
    // Note: the handles shouldn't be duplicated across services, so we don't
    // need to explicitly track the relationship between characteristics and
    // services here. (The frontend will be tracking the connection between
    // characteristics and services - we just need to be able to lookup the
    // BluetoothGattCharacteristic for a given handle in the future)
    gatt_characteristics: DashMap<CharacteristicHandle, GlobalRef>,

    gatt_descriptors: DashMap<DescriptorHandle, GlobalRef>,

    // Note: these are a clone of the .io state, so it's possible to send
    // IO requests or cancel the IO task from a synchronous context but
    // the authorative ownership of these is under
    // AndroidPeripheralIOTask
    io_bus_tx: Option<mpsc::UnboundedSender<IOCmd>>,

    // Note: since the cancellation completes asynchronously and their may
    // soon be nothing holding a reference to the session (e.g. if all
    // peripheral references are dropped) then the cancellation itself is
    // given a temporary reference to the session to ensure it stays alive
    // until complete (otherwise the weak session reference that the IO
    // task hold could fail to upgrade and then the task won't exit
    // cleanly)
    io_task_cancellation: Option<tokio::sync::oneshot::Sender<(AndroidSession)>>,
}

#[derive(Debug)]
enum IOTaskRunState {
    Running(UnboundedSender<BackendEvent>),
    Closing(UnboundedSender<BackendEvent>),
    Closed
}

#[derive(Debug)]
struct AndroidPeripheralIOTask {
    // Most Android Bluetooth GATT requests complete asynchronously with an
    // initial Java method to request something and then a callback that
    // notifies completion (or an error status) and it's implicitly necessary
    // to serialize most of these requests to a single peripheral (although
    // this isn't documented, it's needed in practice not least in case
    // of repeat requests for the same thing where it wouldn't be possible
    // to distinguish results/errors)
    //
    // To handle serializing GATT requests we spawn a dedicated I/O task
    // when connecting a Peripheral to a GATT server and thereafter all
    // Android GATT requests are made via this task.
    //
    // Note: To allow awaiting on the completion of I/O requests then some
    // requests are associated with a tokio oneshot.
    //
    // Note: there are three distinct states for the IO task:
    // 1) io_bus_tx = Some() and io_task_cancellation = Some(): RUNNING
    // 2) io_bus_tx = Some() and io_task_cancellation = None: CLOSING
    // 3) io_bus_tx = None and io_task_cancellation = None: CLOSED
    //
    // Note: To reduce complexity then all code that deals with checking the
    // state of the IO task and potentially changing the state (such as
    // spawning a new task) must take this scheduler lock.
    //task_scheduler_lock: Mutex,

    io_bus_tx: Option<mpsc::UnboundedSender<IOCmd>>,

    // To stop the IO task reliably, but also block a new IO task from being
    // spawned until it has really exit then we can't just drop the io_bus_tx
    // and trust that the IO task will exit.
    //
    // We could potentially have an IORequest::Quit command but in case there
    // might be a backlog of commands we really just want to abort processing
    // any more commands.
    //
    // The IO task should be cancelled by first triggering this cancellation
    // oneshot, setting it to None then clone io_bus_tx and (outside of the
    // AndroidPeripheralInner lock) call io_bus_tx.closed().await to wait for
    // confirmation that the task has stopped. Finally retake the
    // AndroidPeripheralInner lock and if io_bus_tx.is_some() and
    // io_task_cancellation.is_none() then set io_bus_tx = None. (It's important
    // to double check these states in case something else might have also
    // been waiting and then immediately spawned a new IO task)
    //
    io_task_cancellation_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

#[derive(Debug)]
struct AndroidPeripheralInner {
    // Descriptors don't have an instance ID like services and characteristics
    // so we have to allocate unique IDs for descriptors...
    next_descriptor_handle: AtomicU32,
    state: StdRwLock<AndroidPeripheralState>,
    io: tokio::sync::Mutex<AndroidPeripheralIOTask>,
}

#[derive(Clone, Debug)]
struct AndroidCharacteristic {
    jcharacteristic: GlobalRef,
}

#[derive(Clone, Debug)]
struct AndroidDescriptor {
    jdescriptor: GlobalRef,
}

struct IOProcessorState {
    pub queue: VecDeque<IOCmd>,

    pub pending: Option<IOCmd>, // last issued request which we expect a callback for

    // For timeouts we track a start time and duration in case we may need to pause
    // the timeout to allow for bonding/pairing (and reset the started_at time)
    pub pending_started_at: Option<Instant>,
    pub pending_timeout_duration: Duration,

    // We pause the processing of IO requests if we're waiting for a device to bond
    // (could involve user interaction)
    pub waiting_for_bond: bool,

    pub quit: bool,

    // If we're quitting due to the completion of an explicit disconnect request
    // then there's no need to call disconnect before close()
    pub skip_disconnect_on_quit: bool
}

fn check_jni_exception(jenv: JNIEnv) -> Result<()> {
    if jenv.exception_check()? {
        error!("Exception was thrown!");
        let throwable = jenv.exception_occurred()?;
        jenv.exception_clear();

        let klass = jenv.get_object_class(throwable)?;
        let get_message_method = jenv.get_method_id(klass, "getMessage", "()Ljava/lang/String;")?;

        let message = try_call_string_method(jenv, *throwable, get_message_method, &[])?;
        if let Some(message) = message {
            error!("Java Exception: {message}");
        }
    }

    Ok(())
}

fn resolve_method_id(jenv: JNIEnv, klass: JClass, name: &str, signature: &str) -> Result<JMethodID> {
    trace!("Resolving method {name}...");
    match jenv.get_method_id(klass, name, signature) {
        Ok(method_id) => Ok(method_id),
        Err(err) => {
            error!("Failed to resolve method {name}");
            check_jni_exception(jenv)?;
            Err(Error::Other(anyhow!(err)))
        }
    }
}

struct BleSessionNative;

impl BleSessionNative {

    /// Whenever the user selects a device via the Companion API chooser UI then this callback is
    /// called to notify us of the selected device...
    #[named]
    extern "C" fn on_companion_device_select(
        env: JNIEnv,
        session: JObject,
        session_handle: JHandle<AndroidSession>, // (jlong wrapper)
        device: JObject, // BluetoothDevice
        address: JString,
        name: JString
    ) {
        debug!("Notify companion device, handle = {:?}", session_handle);

        let result = if let Some(session) = unsafe { IntoJHandle::clone_from_weak_handle(session_handle) } {
            (|| -> Result<()> { // Like a try{} block since the JNI func doesn't return Result

                let address = env
                    .get_string(address)?
                    .to_str().map_err(|err| { Error::Other(anyhow!("JNI: invalid utf8 for returned String: {:?}", err))})?
                    .to_string();

                let name = env
                    .get_string(name)?
                    .to_str().map_err(|err| { Error::Other(anyhow!("JNI: invalid utf8 for returned String: {:?}", err))})?
                    .to_string();

                let _peripheral_handle = session.declare_peripheral(Address::String(address), name)?;

                Ok(())
            })() // call closure to catch Result
        } else {
            Err(Error::Other(anyhow!("Java callback failed to get native session from handle")))
        };

        if let Err(err) = result {
            error!("Failed to handle companion device notification: {}", err);
            env.throw(format!("{}", err));
        }
    }

    #[named]
    extern "C" fn on_scan_result(
        env: JNIEnv,
        session: JObject,
        session_handle: JHandle<AndroidSession>, // (jlong wrapper)
        callback_type: jint,
        scan_result: JObject,
        address: JString
    ) {
        trace!("On Scan Result");

        let result = if let Some(session) = unsafe { IntoJHandle::clone_from_weak_handle(session_handle) } {
            (|| -> Result<()> { // Like a try{} block since the JNI func doesn't return Result
                let address_str = env
                    .get_string(address)?;
                let address_str = address_str
                    .to_str().map_err(|err| { Error::Other(anyhow!("JNI: invalid utf8 for returned String: {:?}", err))})?;
                let mac = match try_u64_from_mac48_str(&address_str) {
                    Some(mac) => mac,
                    None => return Err(Error::Other(anyhow!("Spurious device address format: {}", address_str)))
                };

                let peripheral_handle = session.peripheral_from_mac(mac)?;

                if let Some(local_name) = try_call_string_method(env, session.jsession.as_obj(), session.scan_result_get_local_name_method, &[
                    JValue::Object(scan_result).to_jni()
                ])? {
                    let _ = session.backend_bus
                        .send(BackendEvent::PeripheralPropertySet {
                            peripheral_handle,
                            property: BackendPeripheralProperty::Name(local_name),
                        });
                }
                let tx_power = try_call_int_method(env, session.jsession.as_obj(), session.scan_result_get_tx_power_method, &[
                    JValue::Object(scan_result).to_jni()
                ])?;
                let _ = session.backend_bus
                    .send(BackendEvent::PeripheralPropertySet {
                        peripheral_handle,
                        property: BackendPeripheralProperty::TxPower(tx_power as i16),
                    });
                let rssi = try_call_int_method(env, session.jsession.as_obj(), session.scan_result_get_rssi_method, &[
                    JValue::Object(scan_result).to_jni()
                ])?;
                let _ = session.backend_bus
                    .send(BackendEvent::PeripheralPropertySet {
                        peripheral_handle,
                        property: BackendPeripheralProperty::Rssi(rssi as i16),
                    });

                let n_service_uuids = try_call_int_method(env, session.jsession.as_obj(), session.scan_result_get_service_uuids_count_method, &[
                    JValue::Object(scan_result).to_jni()
                ])?;
                if n_service_uuids > 0 {
                    let mut service_uuids = vec![];
                    for i in 0..n_service_uuids {
                        if let Some(service_uuid) = try_call_string_method(env, session.jsession.as_obj(), session.scan_result_get_nth_service_uuid_method, &[
                            JValue::Object(scan_result).to_jni(),
                            JValue::Int(i).to_jni()
                        ])? {
                            if let Ok(uuid) = Uuid::parse_str(&service_uuid) {
                                service_uuids.push(uuid);
                            }
                        }
                    }

                    if !service_uuids.is_empty() {
                        let _ = session.backend_bus.send(BackendEvent::PeripheralPropertySet {
                            peripheral_handle,
                            property: BackendPeripheralProperty::ServiceIds(service_uuids),
                        });
                    }
                }

                let n = try_call_int_method(env, session.jsession.as_obj(), session.scan_result_get_specific_manufacturer_data_count_method, &[
                    JValue::Object(scan_result).to_jni()
                ])?;
                if n > 0 {
                    let mut data_map = HashMap::new();
                    for i in 0..n {
                        let manfacturer_id = try_call_int_method(env, session.jsession.as_obj(), session.scan_result_get_specific_manufacturer_data_id_method, &[
                            JValue::Object(scan_result).to_jni(),
                            JValue::Int(i).to_jni()
                        ])? as u16;
                        let data = try_call_object_method(env, session.jsession.as_obj(), session.scan_result_get_specific_manufacturer_data_method, &[
                            JValue::Object(scan_result).to_jni(),
                            JValue::Int(i).to_jni()
                        ])?;
                        if !data.is_null() {
                            let data_vec = env.convert_byte_array(*data)?;
                            data_map.insert(manfacturer_id, data_vec);
                        }
                    }
                    if !data_map.is_empty() {
                        let _ = session.backend_bus.send(BackendEvent::PeripheralPropertySet {
                            peripheral_handle,
                            property: BackendPeripheralProperty::ManufacturerData(data_map),
                        });
                    }
                }

                Ok(())
            })()
        } else {
            Err(Error::Other(anyhow!("Java callback failed to get native session from handle")))
        };

        if let Err(err) = result {
            error!("Failed to handle scan result: {:?}", err);
        }
    }

    #[named]
    extern "C" fn on_device_connection_state_change(
        env: JNIEnv,
        session: JObject,
        session_handle: JHandle<AndroidSession>, // (jlong wrapper)
        device_handle: jlong, // u32 peripheral handle
        connected: jboolean,
        status: jint,
    ) {
        notify_io_callback_from_jni(env, session_handle, device_handle, function_name!(),
                                    |session, peripheral_handle| {
            let status = map_gatt_status(status, true);
            let connected = connected != 0;
            Ok(IOCmd::ConnectionStatusNotify { connected, status })
        });

    }

    #[named]
    extern "C" fn on_device_bonding_state_change(
        env: JNIEnv,
        session: JObject,
        session_handle: JHandle<AndroidSession>, // (jlong wrapper)
        device_handle: jlong, // u32 peripheral handle
        prev_state: jint,
        new_state: jint,
    ) {
        notify_io_callback_from_jni(env, session_handle, device_handle, function_name!(),
                                    |session, peripheral_handle| {
            let bond_state = AndroidBondState::from(new_state);
            Ok(IOCmd::BondingStateNotify { bond_state })
        });
    }

    #[named]
    extern "C" fn on_device_services_discovered(
        env: JNIEnv,
        session: JObject,
        session_handle: JHandle<AndroidSession>, // (jlong wrapper)
        device_handle: jlong, // u32 peripheral handle
        status: jint
    ) {
        notify_io_callback_from_jni(env, session_handle, device_handle, function_name!(),
                                    |session, peripheral_handle| {
            let status = map_gatt_status(status, true);
            Ok(IOCmd::FinishDiscoverServices { status })
        });
    }

    #[named]
    extern "C" fn on_device_read_remote_rssi(
        env: JNIEnv,
        session: JObject,
        session_handle: JHandle<AndroidSession>, // (jlong wrapper)
        device_handle: jlong, // u32 peripheral handle
        rssi: jint,
        status: jint,
    ) {
        notify_io_callback_from_jni(env, session_handle, device_handle, function_name!(),
                                    |session, peripheral_handle| {
            let status = map_gatt_status(status, true);
            Ok(IOCmd::FinishReadRSSI { rssi: rssi as i16, status })
        });
    }

    #[named]
    extern "C" fn on_characteristic_read(
        env: JNIEnv,
        session: JObject,
        session_handle: JHandle<AndroidSession>, // (jlong wrapper)
        device_handle: jlong, // u32 peripheral handle
        characteristic_instance_id: jint,
        value: jbyteArray,
        status: jint
    ) {
        notify_io_callback_from_jni(env, session_handle, device_handle, function_name!(),
                                    |session, peripheral_handle| {
            let status = map_gatt_status(status, true);
            let characteristic_handle = CharacteristicHandle(characteristic_instance_id as u32);
            let value = env.convert_byte_array(value)?;
            Ok(IOCmd::FinishReadCharacteristic { characteristic_handle, status, value })
        });
    }

    #[named]
    extern "C" fn on_characteristic_write(
        env: JNIEnv,
        session: JObject,
        session_handle: JHandle<AndroidSession>, // (jlong wrapper)
        device_handle: jlong, // u32 peripheral handle
        characteristic_instance_id: jint,
        status: jint,
    ) {
        notify_io_callback_from_jni(env, session_handle, device_handle, function_name!(),
                                |session, peripheral_handle| {
            let status = map_gatt_status(status, true);
            let characteristic_handle = CharacteristicHandle(characteristic_instance_id as u32);
            Ok(IOCmd::FinishWriteCharacteristic { characteristic_handle, status })
        });
    }

    #[named]
    extern "C" fn on_characteristic_changed(
        env: JNIEnv,
        session: JObject,
        session_handle: JHandle<AndroidSession>, // (jlong wrapper)
        device_handle: jlong, // u32 peripheral handle
        service_instance_id: jint,
        characteristic_instance_id: jint,
        value: jbyteArray,
    ) {
        notify_io_callback_from_jni(env, session_handle, device_handle, function_name!(),
                                    |session, peripheral_handle| {
            let service_handle = ServiceHandle(service_instance_id as u32);
            let characteristic_handle = CharacteristicHandle(characteristic_instance_id as u32);
            let value = env.convert_byte_array(value)?;
            Ok(IOCmd::CharacteristicNotify { service_handle, characteristic_handle, value })
        });
    }

    #[named]
    extern "C" fn on_descriptor_read(
        env: JNIEnv,
        session: JObject,
        session_handle: JHandle<AndroidSession>, // (jlong wrapper)
        device_handle: jlong, // u32 peripheral handle
        descriptor_id: jint,
        value: jbyteArray,
        status: jint,
    ) {
        notify_io_callback_from_jni(env, session_handle, device_handle, function_name!(),
                                    |session, peripheral_handle| {
            let status = map_gatt_status(status, true);
            let descriptor_handle = DescriptorHandle(descriptor_id as u32);
            let value = env.convert_byte_array(value)?;
            Ok(IOCmd::FinishReadDescriptor { descriptor_handle, status, value })
        });
    }

    #[named]
    extern "C" fn on_descriptor_write(
        env: JNIEnv,
        session: JObject,
        session_handle: JHandle<AndroidSession>, // (jlong wrapper)
        device_handle: jlong, // u32 peripheral handle
        descriptor_id: jint,
        status: jint,
    ) {
        notify_io_callback_from_jni(env, session_handle, device_handle, function_name!(),
                                    |session, peripheral_handle| {
            let status = map_gatt_status(status, true);
            let descriptor_handle = DescriptorHandle(descriptor_id as u32);
            Ok(IOCmd::FinishWriteDescriptor { descriptor_handle, status })
        });
    }

    fn register_native_methods(env: &JNIEnv, session_class: JClass) -> Result<()>
    {
        debug!("Calling env.register_native_methods...");
        env.register_native_methods(
            session_class,
            &[
                NativeMethod {
                    name: "onCompanionDeviceSelect".into(),
                    sig: "(JLandroid/bluetooth/BluetoothDevice;Ljava/lang/String;Ljava/lang/String;)V".into(),
                    fn_ptr: Self::on_companion_device_select as *mut c_void,
                },
                NativeMethod {
                    name: "onScanResult".into(),
                    sig: "(JILandroid/bluetooth/le/ScanResult;Ljava/lang/String;)V".into(),
                    fn_ptr: Self::on_scan_result as *mut c_void,
                },
                NativeMethod {
                    name: "onDeviceConnectionStateChange".into(),
                    sig: "(JJZI)V".into(),
                    fn_ptr: Self::on_device_connection_state_change as *mut c_void,
                },
                NativeMethod {
                    name: "onDeviceBondingStateChange".into(),
                    sig: "(JJII)V".into(),
                    fn_ptr: Self::on_device_bonding_state_change as *mut c_void,
                },
                NativeMethod {
                    name: "onDeviceServicesDiscovered".into(),
                    sig: "(JJI)V".into(),
                    fn_ptr: Self::on_device_services_discovered as *mut c_void,
                },
                NativeMethod {
                    name: "onDeviceReadRemoteRssi".into(),
                    sig: "(JJII)V".into(),
                    fn_ptr: Self::on_device_read_remote_rssi as *mut c_void,
                },
                NativeMethod {
                    name: "onCharacteristicRead".into(),
                    sig: "(JJI[BI)V".into(),
                    fn_ptr: Self::on_characteristic_read as *mut c_void,
                },
                NativeMethod {
                    name: "onCharacteristicWrite".into(),
                    sig: "(JJII)V".into(),
                    fn_ptr: Self::on_characteristic_write as *mut c_void,
                },
                NativeMethod {
                    name: "onCharacteristicChanged".into(),
                    sig: "(JJII[B)V".into(),
                    fn_ptr: Self::on_characteristic_changed as *mut c_void,
                },
                NativeMethod {
                    name: "onDescriptorRead".into(),
                    sig: "(JJI[BI)V".into(),
                    fn_ptr: Self::on_descriptor_read as *mut c_void,
                },
                NativeMethod {
                    name: "onDescriptorWrite".into(),
                    sig: "(JJII)V".into(),
                    fn_ptr: Self::on_descriptor_write as *mut c_void,
                },
            ],
        )?;
        Ok(())
    }
}

impl AndroidSession {
    // For the sake of avoiding circular references then we may choose to
    // capture weak references to the WinrtSessionInner for various
    // callbacks and after upgrading back to an Arc we can re-wrap as a
    // bona fide WinrtSession again for the duration of the callback...
    fn wrap_inner(inner: Arc<AndroidSessionInner>) -> Self {
        Self { inner }
    }

    pub fn new(config: &SessionConfig<'_>, backend_bus: mpsc::UnboundedSender<BackendEvent>)
                     -> Result<Self> {

        let thread_id = std::thread::current().id();
        trace!("AndroidSession::new() (thread id = {thread_id:?}");

        let android_config = config.android.as_ref().expect("Missing AndroidConfig on Android");
        let env = android_config.jni_env;
        let jvm = env.get_java_vm()?;

        // Since we likely aren't running on the Java main thread we need to consider
        // that env.find_class() will fallback to the system classLoader which won't find
        // our BleSession class.
        //
        // Instead of using env.find_class() we lookup the ClassLoader associated with
        // the Activity that we've been given a reference to, which should be able to
        // find the BleSession class.
        //
        let activity = android_config.activity;
        let activity_class = env.get_object_class(activity)?;

        let get_class_loader_method = resolve_method_id(env, activity_class, "getClassLoader", "()Ljava/lang/ClassLoader;")?;

        trace!("Calling activity.getClassLoader()");
        let loader = try_call_object_method(env, activity, get_class_loader_method, &[])?;
        let loader_class: JObject = env.find_class("java/lang/ClassLoader")?.into();
        let load_class_method = resolve_method_id(env, loader_class.into(), "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;")?;
        let ble_session_class_name: JObject = env.new_string(BLE_SESSION_CLASS_NAME)?.into();
        let session_class: JClass = try_call_object_method(env, loader, load_class_method, &[
            JValue::Object(ble_session_class_name).to_jni()
        ])?.into();

        trace!("AndroidSession::new(): find_class(BluetoothDevice)");
        let android_device_class = match env.find_class("android/bluetooth/BluetoothDevice") {
            Ok(s) => s,
            Err(err) => { error!("Failed to find BluetoothDevice class: {:?}", err); return Err(err)?; }
        };

        trace!("AndroidSession::new(): find_class(BleDevice)");
        let ble_device_class_name: JObject = env.new_string(BLE_DEVICE_CLASS_NAME)?.into();
        let ble_device_class: JClass = try_call_object_method(env, loader, load_class_method, &[
            JValue::Object(ble_device_class_name).to_jni()
        ])?.into();

        trace!("AndroidSession::new(): construct new BleSession");
        let activity_val = jni::objects::JValue::Object(android_config.activity);
        let chooser_request_code = match android_config.companion_chooser_request_code {
            Some(code) => jni::objects::JValue::Int(code as i32),
            None => jni::objects::JValue::Int(-1),
        };
        let jsession = match env.new_object(session_class, "(Landroid/app/Activity;I)V", &[activity_val, chooser_request_code]) {
            Ok(s) => s,
            Err(err) => { error!("Failed to construct Java BleSession instance {:?}", err); return Err(err)?; }
        };

        REGISTER_NATIVE_METHODS.call_once(|| {
            BleSessionNative::register_native_methods(&env, session_class).expect("Failed to initialize native methods for BleSession");
        });

        let mut session = AndroidSession {
            inner: Arc::new(AndroidSessionInner {
                jvm,
                activity_ref: env.new_global_ref(android_config.activity)?,
                jsession: env.new_global_ref(jsession)?,
                weak_handle: StdRwLock::new(JHandle::<AndroidSession>::default()),

                device_class: env.new_global_ref(ble_device_class)?,
                backend_bus,

                peripheral_handles_by_mac: DashMap::new(),
                peripherals_by_handle: DashMap::new(),
                next_peripheral_handle: AtomicU32::new(1),

                // BleSession methods...
                //
                set_native_handle_method: resolve_method_id(env, session_class, "setNativeSessionHandle", "(J)V")?,

                scanner_config_reset_method: resolve_method_id(env, session_class, "scannerConfigReset", "()V")?,
                scanner_config_add_service_uuid_method: resolve_method_id(env, session_class, "scannerConfigAddServiceUuid", "(Ljava/lang/String;)V")?,
                start_scanning_method: resolve_method_id(env, session_class, "startScanning", "()V")?,
                stop_scanning_method: resolve_method_id(env, session_class, "stopScanning", "()V")?,

                scan_result_get_local_name_method: resolve_method_id(env, session_class,
                    "scanResultGetLocalName", "(Landroid/bluetooth/le/ScanResult;)Ljava/lang/String;")?,
                scan_result_get_specific_manufacturer_data_count_method: resolve_method_id(env, session_class,
                    "scanResultGetSpecificManufacturerDataCount", "(Landroid/bluetooth/le/ScanResult;)I")?,
                scan_result_get_specific_manufacturer_data_id_method: resolve_method_id(env, session_class,
                    "scanResultGetSpecificManufacturerDataId", "(Landroid/bluetooth/le/ScanResult;I)I")?,
                scan_result_get_specific_manufacturer_data_method: resolve_method_id(env, session_class,
                    "scanResultGetSpecificManufacturerData", "(Landroid/bluetooth/le/ScanResult;I)[B")?,
                scan_result_get_service_uuids_count_method: resolve_method_id(env, session_class,
                    "scanResultGetServiceUuidsCount", "(Landroid/bluetooth/le/ScanResult;)I")?,
                scan_result_get_nth_service_uuid_method: resolve_method_id(env, session_class,
                    "scanResultGetNthServiceUuid", "(Landroid/bluetooth/le/ScanResult;I)Ljava/lang/String;")?,
                scan_result_get_tx_power_method: resolve_method_id(env, session_class,
                    "scanResultGetTxPowerLevel", "(Landroid/bluetooth/le/ScanResult;)I")?,
                scan_result_get_rssi_method: resolve_method_id(env, session_class,
                    "scanResultGetRssi", "(Landroid/bluetooth/le/ScanResult;)I")?,

                get_device_for_address_method: resolve_method_id(env, session_class, "getDeviceForAddress", concatcp!("(Ljava/lang/String;J)", BLE_DEVICE_JNI_TYPE))?,

                connect_device_gatt_method: resolve_method_id(env, session_class, "connectDeviceGatt", concatcp!("(", BLE_DEVICE_JNI_TYPE, "JZ)V"))?,
                reconnect_device_gatt_method: resolve_method_id(env, session_class, "reconnectDeviceGatt", concatcp!("(", BLE_DEVICE_JNI_TYPE, ")Z"))?,
                disconnect_device_gatt_method: resolve_method_id(env, session_class, "disconnectDeviceGatt", concatcp!("(", BLE_DEVICE_JNI_TYPE, ")V"))?,
                close_device_gatt_method: resolve_method_id(env, session_class, "closeDeviceGatt", concatcp!("(", BLE_DEVICE_JNI_TYPE, ")V"))?,

                get_device_bond_state_method: resolve_method_id(env, session_class, "getDeviceBondState", concatcp!("(", BLE_DEVICE_JNI_TYPE, ")I"))?,
                get_device_name_method: resolve_method_id(env, session_class, "getDeviceName", concatcp!("(", BLE_DEVICE_JNI_TYPE, ")Ljava/lang/String;"))?,
                read_device_rssi_method: resolve_method_id(env, session_class, "readDeviceRssi", concatcp!("(", BLE_DEVICE_JNI_TYPE, ")Z"))?,
                discover_device_services_method: resolve_method_id(env, session_class, "discoverDeviceServices", concatcp!("(", BLE_DEVICE_JNI_TYPE, ")Z"))?,
                get_device_services_method: resolve_method_id(env, session_class, "getDeviceServices", concatcp!("(", BLE_DEVICE_JNI_TYPE, ")Ljava/util/List;"))?,
                clear_device_gatt_state_method: resolve_method_id(env, session_class, "clearDeviceGattState", concatcp!("(", BLE_DEVICE_JNI_TYPE, ")V"))?,

                get_service_instance_id_method: resolve_method_id(env, session_class, "getServiceInstanceId", "(Landroid/bluetooth/BluetoothGattService;)I")?,
                get_service_uuid_method: resolve_method_id(env, session_class, "getServiceUuid", "(Landroid/bluetooth/BluetoothGattService;)Ljava/lang/String;")?,
                get_service_includes_method: resolve_method_id(env, session_class, "getServiceIncludes", "(Landroid/bluetooth/BluetoothGattService;)Ljava/util/List;")?,
                get_service_characteristics_method: resolve_method_id(env, session_class, "getServiceCharacteristics", "(Landroid/bluetooth/BluetoothGattService;)Ljava/util/List;")?,

                get_characteristic_instance_id_method: resolve_method_id(env, session_class, "getCharacteristicInstanceId", "(Landroid/bluetooth/BluetoothGattCharacteristic;)I")?,
                get_characteristic_uuid_method: resolve_method_id(env, session_class, "getCharacteristicUuid", "(Landroid/bluetooth/BluetoothGattCharacteristic;)Ljava/lang/String;")?,
                get_characteristic_properties_method: resolve_method_id(env, session_class, "getCharacteristicProperties", "(Landroid/bluetooth/BluetoothGattCharacteristic;)I")?,
                get_characteristic_descriptors_method: resolve_method_id(env, session_class, "getCharacteristicDescriptors", "(Landroid/bluetooth/BluetoothGattCharacteristic;)Ljava/util/List;")?,
                characteristic_read_method: resolve_method_id(env, session_class, "requestReadCharacteristic", concatcp!("(", BLE_DEVICE_JNI_TYPE, "Landroid/bluetooth/BluetoothGattCharacteristic;)Z"))?,
                characteristic_write_method: resolve_method_id(env, session_class, "requestWriteCharacteristic", concatcp!("(", BLE_DEVICE_JNI_TYPE, "Landroid/bluetooth/BluetoothGattCharacteristic;[BI)Z"))?,
                characteristic_subscribe_method: resolve_method_id(env, session_class, "requestSubscribeCharacteristic", concatcp!("(", BLE_DEVICE_JNI_TYPE, "Landroid/bluetooth/BluetoothGattCharacteristic;)Z"))?,
                characteristic_unsubscribe_method: resolve_method_id(env, session_class, "requestUnsubscribeCharacteristic", concatcp!("(", BLE_DEVICE_JNI_TYPE, "Landroid/bluetooth/BluetoothGattCharacteristic;)Z"))?,

                //add_descriptor_handle_method: env.get_method_id(session_class, "addDescriptorHandle", "(", BLE_DEVICE_JNI_TYPE, "Landroid/bluetooth/BluetoothGattDescriptor;J)")?.into(),
                get_descriptor_id_method: resolve_method_id(env, session_class, "getDescriptorId", concatcp!("(", BLE_DEVICE_JNI_TYPE, "Landroid/bluetooth/BluetoothGattDescriptor;)I"))?,
                get_descriptor_uuid_method: resolve_method_id(env, session_class, "getDescriptorUuid", "(Landroid/bluetooth/BluetoothGattDescriptor;)Ljava/lang/String;")?,
                descriptor_read_method: resolve_method_id(env, session_class, "requestReadDescriptor", concatcp!("(", BLE_DEVICE_JNI_TYPE, "Landroid/bluetooth/BluetoothGattDescriptor;)Z"))?,
                descriptor_write_method: resolve_method_id(env, session_class, "requestWriteDescriptor", concatcp!("(", BLE_DEVICE_JNI_TYPE, "Landroid/bluetooth/BluetoothGattDescriptor;[B)Z"))?,

                // BluetoothDevice methods...
                //
                device_get_address_method: resolve_method_id(env, android_device_class, "getAddress", "()Ljava/lang/String;")?,
            })
        };


        trace!("Creating native handle...: refs beforehand: strong = {}, weak = {}", Arc::strong_count(&session.inner), Arc::weak_count(&session.inner));
        // Create a handle to a weak reference that can be passed over JNI and store the handle itself
        // in AndroidSessionInner so that the handle can be mapped back into a Weak reference and properly
        // dropped when AndroidSessionInner gets dropped
        let weak_handle = unsafe { IntoJHandle::into_weak_handle(session.clone()) };
        {
            let mut guard = session.weak_handle.write().unwrap();
            *guard = weak_handle;
        }

        trace!("AndroidSession::new(): setting native handle = {:?}", session.weak_handle);
        try_call_void_method(env, jsession, session.set_native_handle_method, &[weak_handle.to_jni()])?;

        trace!("AndroidSession::new(): done: refs = strong = {}, weak = {}", Arc::strong_count(&session.inner), Arc::weak_count(&session.inner));

        Ok(session)
    }

    fn peripheral_from_mac(&self, address: u64) -> Result<PeripheralHandle> {
        match self.peripheral_handles_by_mac.get(&address) {
            None => {
                trace!("New peripheral for mac {}", MAC(address));
                // Note: we only lookup the corresponding BluetoothDevice if the user
                // tries to connect to the Peripheral, so initially ble_device will
                // be set to None

                let backend_bus = self.backend_bus.clone();

                let peripheral_id = self.next_peripheral_handle.fetch_add(1, Ordering::SeqCst);
                let peripheral_handle = PeripheralHandle(peripheral_id);

                let android_peripheral = AndroidPeripheral {
                    //inner: Arc::new(StdRwLock::new(AndroidPeripheralInner {
                    inner: Arc::new(AndroidPeripheralInner {
                        next_descriptor_handle: AtomicU32::new(1),
                        state: StdRwLock::new(AndroidPeripheralState {
                            address,
                            //peripheral_handle,
                            ble_device: None,
                            connected: false,
                            has_gatt_interface: false,

                            gatt_services: DashMap::new(),
                            gatt_characteristics: DashMap::new(),
                            gatt_descriptors: DashMap::new(),

                            io_bus_tx: None,
                            io_task_cancellation: None,
                        }),
                        io: tokio::sync::Mutex::new(AndroidPeripheralIOTask {
                            io_bus_tx: None,
                            io_task_cancellation_tx: None,
                        })
                    }),
                };
                self.peripheral_handles_by_mac
                    .insert(address, peripheral_handle);
                self.peripherals_by_handle
                    .insert(peripheral_handle, android_peripheral);

                let _ = backend_bus.send(BackendEvent::PeripheralFound { peripheral_handle });

                let _ = backend_bus.send(BackendEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: BackendPeripheralProperty::Address(Address::MAC(MAC(address))),
                });

                Ok(peripheral_handle)
            }
            Some(peripheral_handle) => Ok(*peripheral_handle),
        }
    }

    fn android_peripheral_from_handle(&self, peripheral_handle: PeripheralHandle)
                                      -> Result<AndroidPeripheral> {
        match self.peripherals_by_handle.get(&peripheral_handle) {
            Some(p) => Ok(p.clone()),
            None => {
                log::error!("Spurious request with unknown peripheral handle {:?}",
                            peripheral_handle);
                Err(Error::InvalidStateReference)
            }
        }
    }

    async fn handle_io_notification(
        state: &mut IOProcessorState,
        session: AndroidSession,
        cmd: IOCmd,
        peripheral_handle: PeripheralHandle,
        ble_device: GlobalRef) -> Result<()>
    {
        trace!("Handling peripheral IO notification = {cmd:?}");

        let jenv = session.jvm.get_env()?;
        match cmd {
            IOCmd::RequestConnect { .. } |
            //IOCmd::RequestDisconnect { .. } |
            IOCmd::ReadRSSI { .. } |
            IOCmd::RequestDiscoverServices { .. } |
            IOCmd::RequestReadCharacteristic { .. } |
            IOCmd::RequestWriteCharacteristic { .. } |
            IOCmd::RequestSubscribeCharacteristic { .. } |
            IOCmd::RequestUnsubscribeCharacteristic { .. } |
            IOCmd::RequestReadDescriptor { .. } |
            IOCmd::RequestWriteDescriptor { .. }
            => {
                unreachable!();
            },

            IOCmd::BondingStateNotify { bond_state } => {
                trace!("Bonding state notification = {:?}", bond_state);
                state.waiting_for_bond = matches!(bond_state, AndroidBondState::Bonding);

                Ok(())
            },
            IOCmd::ConnectionStatusNotify { connected, status } => {

                {
                    let android_peripheral = session.android_peripheral_from_handle(peripheral_handle)?;
                    let mut guard = android_peripheral.state.write().unwrap();
                    guard.connected = connected;
                }

                match state.pending {
                    Some(IOCmd::RequestConnect { .. }) => {
                        match status {
                            AndroidGattStatus::Success if connected == true => {
                                // If Android says the device is in the process of bonding after notifying
                                // us of a connect then we want to delay any follow up requests, such
                                // as service discovery until the bonding is complete...
                                trace!("Querying device bond state");
                                let bond_state = try_call_int_method(jenv, session.jsession.as_obj(), session.get_device_bond_state_method,
                                                                &[JValue::Object(ble_device.as_obj()).to_jni()])?;
                                let bond_state = AndroidBondState::from(bond_state);
                                trace!("Bonding state after connection = {:?}", bond_state);
                                state.waiting_for_bond = matches!(bond_state, AndroidBondState::Bonding);

                                let _ = session
                                    .backend_bus
                                    .send(BackendEvent::PeripheralConnected { peripheral_handle });
                            },
                            _ => {
                                let error = Option::<GattError>::from(status);
                                let _ = session
                                    .backend_bus
                                    .send(BackendEvent::PeripheralConnectionFailed {
                                        peripheral_handle,
                                        error
                                    } );
                            }
                        };

                        state.pending = None;
                    }
                    /*
                    Some(IOCmd::RequestDisconnect { .. }) => {
                        if connected == false {
                            let error = Option::<GattError>::from(status);
                            let _ = session
                                .backend_bus
                                .send(BackendEvent::PeripheralDisconnected { peripheral_handle, error } );

                            state.pending = None;
                        } else {
                            // If we are asked to cancel a pending connection I suppose it's possible we might get
                            // a connect notification if we connect just before the request to disconnect and
                            // we should still expect a future disconnect notification (so we don't clear
                            // state.pending here)
                            warn!("Spurious connect notification after requesting disconnect");
                        }
                    }*/
                    _ => {
                        // NB: In this case we didn't explicitly ask for a connect / disconnect
                        // and so only disconnect notifications are expected here!

                        if connected {
                            // XXX: what should we do with a spurious connection notification?
                            error!("Spurious peripheral connection notification");
                        } else {
                            let error = Option::<GattError>::from(status);
                            session.notify_disconnect(peripheral_handle, error);
                        }
                    }
                }

                if !connected {
                    state.quit = true;
                    // If we've just been notified of a disconnect we don't need to
                    // explicitly disconnect before closing.
                    state.skip_disconnect_on_quit = true;
                }

                Ok(())
            }
            IOCmd::FinishReadRSSI { rssi, status } => {
                if matches!(state.pending, Some(IOCmd::ReadRSSI { .. })) {
                    let finished = std::mem::take(&mut state.pending).unwrap();
                    let result_tx = if let IOCmd::ReadRSSI { result_tx, .. } = finished { result_tx } else { unreachable!() };

                    match status {
                        AndroidGattStatus::Success => {
                            let _ = session
                                .backend_bus
                                .send(BackendEvent::PeripheralPropertySet { peripheral_handle, property: BackendPeripheralProperty::Rssi(rssi as i16) });

                            let _ = result_tx.send(Ok(rssi));
                        },
                        _ => {
                            let gatt_error = Option::<GattError>::from(status).unwrap();
                            let _ = result_tx.send(Err(Error::from(gatt_error)));
                        }
                    };

                    Ok(())
                } else {
                    Err(Error::Other(anyhow!("Spurious characteristic read notification")))
                }
            },
            IOCmd::FinishDiscoverServices { status } => {
                if matches!(state.pending, Some(IOCmd::RequestDiscoverServices { .. })) {
                    let finished = std::mem::take(&mut state.pending).unwrap();

                    if matches!(status, AndroidGattStatus::Success) {

                        let android_peripheral = session.android_peripheral_from_handle(peripheral_handle)?;

                        let services = try_call_object_method(jenv, session.jsession.as_obj(), session.get_device_services_method,
                                                                    &[JValue::Object(ble_device.as_obj()).to_jni()])?;
                        let services: JList = JList::from_env(&jenv, services)?;

                        session.index_services_list(&android_peripheral, &services)?;

                        for service in services.iter()? {
                            let service_handle = session.get_service_handle(jenv, &android_peripheral, service)?;
                            let uuid = session.get_service_uuid(jenv, &android_peripheral, service)?;
                            let _ = session.backend_bus.send(BackendEvent::GattService {
                                peripheral_handle,
                                service_handle,
                                uuid
                            });
                        }
                    }
                    let error = Option::<GattError>::from(status);
                    let _ = session
                        .backend_bus
                        .send(BackendEvent::GattServicesComplete { peripheral_handle, error });

                    Ok(())
                } else {
                    Err(Error::Other(anyhow!("Spurious discover services notification")))
                }
            },

            IOCmd::FinishReadCharacteristic { characteristic_handle, status, value } => {
                if matches!(state.pending, Some(IOCmd::RequestReadCharacteristic { .. })) {
                    let finished = std::mem::take(&mut state.pending).unwrap();

                    // Bit annoying to have to repeat this pattern match for the sake of being able to take state.pending
                    // without upsetting the borrow checker.
                    if let IOCmd::RequestReadCharacteristic { result_tx, service_handle, .. } = finished {

                        match status {
                            AndroidGattStatus::Success => {
                                let _ = session
                                    .backend_bus
                                    .send(BackendEvent::GattCharacteristicNotify { peripheral_handle, service_handle, characteristic_handle, value: value.clone() } );

                                let _ = result_tx.send(Ok(value));

                                Ok(())
                            },
                            _ => {
                                let gatt_error = Option::<GattError>::from(status).unwrap();
                                //let _ = result_tx.send(Err(Error::from(gatt_error)));
                                Err(Error::from(gatt_error))
                            }
                        }

                    } else {
                        unreachable!()
                    }
                } else {
                    Err(Error::Other(anyhow!("Spurious characteristic read notification")))
                }
            },
            IOCmd::CharacteristicNotify { service_handle, characteristic_handle, value } => {
                let _ = session
                    .backend_bus
                    .send(BackendEvent::GattCharacteristicNotify { peripheral_handle, service_handle, characteristic_handle, value: value.clone() } );

                Ok(())
            }
            IOCmd::FinishWriteCharacteristic { characteristic_handle, status } => {
                if matches!(state.pending, Some(IOCmd::RequestWriteCharacteristic { .. })) {
                    let finished = std::mem::take(&mut state.pending).unwrap();

                    // Bit annoying to have to repeat this pattern match for the sake of being able to take state.pending
                    // without upsetting the borrow checker.
                    if let IOCmd::RequestWriteCharacteristic { service_handle, characteristic_handle, .. } = finished {

                        match status {
                            AndroidGattStatus::Success => {
                                let _ = session
                                    .backend_bus
                                    .send(BackendEvent::GattCharacteristicWriteNotify { peripheral_handle, service_handle, characteristic_handle } );

                                //let _ = result_tx.send(Ok(value));
                                Ok(())
                            },
                            _ => {
                                let gatt_error = Option::<GattError>::from(status).unwrap();
                                //let _ = result_tx.send(Err(Error::from(gatt_error)));
                                Err(Error::from(gatt_error))
                            }
                        }

                    } else {
                        unreachable!()
                    }
                } else {
                    Err(Error::Other(anyhow!("Spurious characteristic write notification")))
                }
            }
            IOCmd::FinishReadDescriptor { descriptor_handle, status, value } => {
                if matches!(state.pending, Some(IOCmd::RequestReadDescriptor { .. })) {
                    let finished = std::mem::take(&mut state.pending).unwrap();

                    // Bit annoying to have to repeat this pattern match for the sake of being able to take state.pending
                    // without upsetting the borrow checker.
                    if let IOCmd::RequestReadDescriptor { result_tx, service_handle, characteristic_handle, .. } = finished {

                        match status {
                            AndroidGattStatus::Success => {
                                let _ = session
                                    .backend_bus
                                    .send(BackendEvent::GattDescriptorNotify { peripheral_handle, service_handle, characteristic_handle, descriptor_handle, value: value.clone() } );

                                let _ = result_tx.send(Ok(value));
                            },
                            _ => {
                                let gatt_error = Option::<GattError>::from(status).unwrap();
                                let _ = result_tx.send(Err(Error::from(gatt_error)));
                            }
                        };

                        Ok(())
                    } else {
                        unreachable!()
                    }
                } else {
                    Err(Error::Other(anyhow!("Spurious descriptor read notification")))
                }
            },
            IOCmd::FinishWriteDescriptor { descriptor_handle, status } => {
                if matches!(state.pending, Some(IOCmd::RequestWriteDescriptor { .. })) {
                    let finished = std::mem::take(&mut state.pending).unwrap();

                    // Bit annoying to have to repeat this pattern match for the sake of being able to take state.pending
                    // without upsetting the borrow checker.
                    if let IOCmd::RequestWriteDescriptor { service_handle, characteristic_handle, .. } = finished {

                        match status {
                            AndroidGattStatus::Success => {
                                let _ = session
                                    .backend_bus
                                    .send(BackendEvent::GattDescriptorWriteNotify { peripheral_handle, service_handle, characteristic_handle, descriptor_handle } );

                                //let _ = result_tx.send(Ok(value));
                                Ok(())
                            },
                            _ => {
                                let gatt_error = Option::<GattError>::from(status).unwrap();
                                //let _ = result_tx.send(Err(Error::from(gatt_error)));
                                Err(Error::from(gatt_error))
                            }
                        }
                    } else {
                        unreachable!()
                    }
                } else {
                    Err(Error::Other(anyhow!("Spurious descriptor read notification")))
                }
            }
        }
    }

    async fn handle_io_request(
        state: &mut IOProcessorState,
        session: AndroidSession,
        request: IOCmd,
        peripheral_handle: PeripheralHandle,
        ble_device: GlobalRef) -> Result<()>
    {
        trace!("Handling peripheral IO request = {request:?}");

        let jenv = session.jvm.get_env()?;
        match request {
            IOCmd::ConnectionStatusNotify { .. } |
            IOCmd::BondingStateNotify { .. } |
            IOCmd::FinishReadRSSI { .. } |
            IOCmd::FinishDiscoverServices { .. } |
            IOCmd::FinishReadCharacteristic { .. } |
            IOCmd::FinishWriteCharacteristic { .. } |
            IOCmd::CharacteristicNotify { .. } |
            IOCmd::FinishReadDescriptor { .. } |
            IOCmd::FinishWriteDescriptor { .. }
            => {
                // These notifications are expected to be handled immediately,  not queued.
                unreachable!();
            }

            IOCmd::RequestConnect { /*result_tx*/ } => {

                // XXX: theoretically we could differentiate between a first connect and a reconnect
                // where we could re-use the existing BluetoothGatt from a previous connection but
                // considering 1) the added complexity, 2) the reduced control with
                // BluetoothGatt::connect() and 3) no documented benefit to reconnecting with connect()
                // vs connectGatt() we currently always use connectGatt().
                //
                // (This also implies that whenever we disconnect we also immediately close() the device
                //  and drop our BluetoothGatt reference.)
                //
                // Note: some developers report that there can be problems on some versions of Android
                // with trying to connect to uncached devices using autoconnect = true. Since this
                // sounds like it may relate to older versions of Android, and considering this isn't
                // a documented limitation of connectGatt, then we currently ignore this potential
                // issue. If we need to support older versions of Android (or if it's still an issue
                // with newer versions) we may need to fallback to manually retrying to connect via
                // autoconnect=false for uncached devices.

                let try_result = || -> Result<()> { // try {
                    trace!("IO Task: requesting device connect...");
                    try_call_void_method(jenv, session.jsession.as_obj(), session.connect_device_gatt_method,
                        &[JValue::Object(ble_device.as_obj()).to_jni(),
                                peripheral_handle.into(),
                                JValue::Bool(1).to_jni() /* autoconnect=true */])?;
                        //JValue::Long(android_peripheral.peripheral_handle.0 as jlong)])?;

                    {
                        let android_peripheral = session.android_peripheral_from_handle(peripheral_handle)?;
                        let mut guard = android_peripheral.state.write().unwrap();
                        guard.has_gatt_interface = true;
                    }
                    trace!("IO Task: peripheral now associated with BluetoothGatt instance");

                    Ok(())
                }();
                if let Err(err) = try_result {
                    session
                        .backend_bus
                        .send(BackendEvent::PeripheralConnectionFailed {
                            peripheral_handle,
                            error: Some(GattError::GeneralFailure(format!("Failed to initiate connection request")))
                        });
                }

                // Considering that we might not have previously enabled scanning and
                // the peripheral might have been declared by the application based
                // on a saved Address so this might be the first point where we learn
                // the device's name...
                trace!("IO Task: querying device name...");
                let name = try_call_string_method(jenv, session.jsession.as_obj(), session.get_device_name_method,
                                                                &[JValue::Object(ble_device.as_obj()).to_jni()])?;
                if let Some(name) = name {
                    debug!("IO Task: got device name = {}", &name);
                    let _ = session
                        .backend_bus
                        .send(BackendEvent::PeripheralPropertySet {
                            peripheral_handle,
                            property: BackendPeripheralProperty::Name(name.to_string()),
                        });
                }

                state.pending = Some(request);

                Ok(())
            },

            IOCmd::ReadRSSI { .. } => {
                if !try_call_bool_method(jenv, session.jsession.as_obj(), session.read_device_rssi_method,
                                    &[JValue::Object(ble_device.as_obj()).to_jni()])?
                {
                    return Err(Error::Other(anyhow!("Failed to initiate RSSI read")));
                }

                // Note: the saved request includes a oneshot which we will
                // use to signal the completion of this request...
                state.pending = Some(request);

                Ok(())
            },

            IOCmd::RequestDiscoverServices { } => {
                if !try_call_bool_method(jenv, session.jsession.as_obj(), session.discover_device_services_method,
                    &[JValue::Object(ble_device.as_obj()).to_jni()])?
                {
                    return Err(Error::Other(anyhow!("Failed to initiate services discovery")));
                }

                state.pending = Some(request);

                Ok(())
            }

            IOCmd::RequestReadCharacteristic { characteristic_handle, .. } => {
                let android_peripheral = session.android_peripheral_from_handle(peripheral_handle)?;
                let jcharacteristic = session.gatt_characteristic_from_handle(&android_peripheral, characteristic_handle)?;

                if !try_call_bool_method(jenv, session.jsession.as_obj(), session.characteristic_read_method,
                                         &[JValue::Object(ble_device.as_obj()).to_jni(),
                                                JValue::Object(jcharacteristic.as_obj()).to_jni()])?
                {
                    return Err(Error::Other(anyhow!("Failed to initiate characteristic read")));
                }

                // Note: the saved request includes a oneshot which we will
                // use to signal the completion of this request...
                state.pending = Some(request);

                Ok(())
            },

            IOCmd::RequestWriteCharacteristic { service_handle, characteristic_handle, value, write_type } => {
                let android_peripheral = session.android_peripheral_from_handle(peripheral_handle)?;
                let jcharacteristic = session.gatt_characteristic_from_handle(&android_peripheral, characteristic_handle)?;

                let value_bytes = jenv.byte_array_from_slice(&value)?;
                if !try_call_bool_method(jenv, session.jsession.as_obj(), session.characteristic_write_method,
                                         &[JValue::Object(ble_device.as_obj()).to_jni(),
                                                 JValue::Object(jcharacteristic.as_obj()).to_jni(),
                                                 jvalue { l: value_bytes },
                                                 write_type.into()])?
                {
                    return Err(Error::Other(anyhow!("Failed to initiate characteristic write")));
                }

                // For the sake of tracking the pending request, we don't want / need to keep
                // a copy of the value/data which was moved above, so we reconstuct the request
                // without the data...
                state.pending = Some(IOCmd::RequestWriteCharacteristic { service_handle, characteristic_handle, value: vec![], write_type });

                Ok(())
            },

            IOCmd::RequestSubscribeCharacteristic { characteristic_handle } => {
                let android_peripheral = session.android_peripheral_from_handle(peripheral_handle)?;
                let jcharacteristic = session.gatt_characteristic_from_handle(&android_peripheral, characteristic_handle)?;
                if !try_call_bool_method(jenv, session.jsession.as_obj(), session.characteristic_subscribe_method,
                                         &[JValue::Object(ble_device.as_obj()).to_jni(),
                                                 JValue::Object(jcharacteristic.as_obj()).to_jni()])? {
                    return Err(Error::Other(anyhow!("Failed to initiate subscription to characteristic")));
                }

                state.pending = Some(request);

                Ok(())
            },

            IOCmd::RequestUnsubscribeCharacteristic { characteristic_handle } => {
                let android_peripheral = session.android_peripheral_from_handle(peripheral_handle)?;
                let jcharacteristic = session.gatt_characteristic_from_handle(&android_peripheral, characteristic_handle)?;
                if !try_call_bool_method(jenv, session.jsession.as_obj(), session.characteristic_unsubscribe_method,
                                         &[JValue::Object(ble_device.as_obj()).to_jni(),
                                                 JValue::Object(jcharacteristic.as_obj()).to_jni()])? {

                    return Err(Error::Other(anyhow!("Failed to initiate unsubscribe from characteristic")));
                }

                state.pending = Some(request);

                Ok(())
            },

            IOCmd::RequestReadDescriptor { service_handle, characteristic_handle, descriptor_handle, .. } => {
                let android_peripheral = session.android_peripheral_from_handle(peripheral_handle)?;
                let jdescriptor = session.gatt_descriptor_from_handle(&android_peripheral, descriptor_handle)?;

                if !try_call_bool_method(jenv, session.jsession.as_obj(), session.descriptor_read_method,
                                         &[JValue::Object(ble_device.as_obj()).to_jni(),
                                                JValue::Object(jdescriptor.as_obj()).to_jni()])?
                {
                    return Err(Error::Other(anyhow!("Failed to initiate descriptor read")));
                }

                // Note: the saved request includes a oneshot which we will
                // use to signal the completion of this request...
                state.pending = Some(request);

                Ok(())
            },

            IOCmd::RequestWriteDescriptor { service_handle, characteristic_handle, descriptor_handle, value } => {
                let android_peripheral = session.android_peripheral_from_handle(peripheral_handle)?;
                let jdescriptor = session.gatt_descriptor_from_handle(&android_peripheral, descriptor_handle)?;

                let value_bytes = jenv.byte_array_from_slice(&value)?;
                if !try_call_bool_method(jenv, session.jsession.as_obj(), session.descriptor_write_method,
                                         &[JValue::Object(ble_device.as_obj()).to_jni(),
                                                 JValue::Object(jdescriptor.as_obj()).to_jni(),
                                                 jvalue { l: value_bytes }])?
                {
                    return Err(Error::Other(anyhow!("Failed to initiate descriptor read")));
                }

                // For the sake of tracking the pending request, we don't want / need to keep
                // a copy of the value/data which was moved above, so we reconstuct the request
                // without the data...
                state.pending = Some(IOCmd::RequestWriteDescriptor { service_handle, characteristic_handle, descriptor_handle, value: vec![] });

                Ok(())
            }
        }
    }

    async fn run_peripheral_io_task_command_loop(
        peripheral_handle: PeripheralHandle,
        //io_bus: mpsc::UnboundedReceiver<IORequest>,
        mut cmd_stream: Pin<&mut Fuse<UnboundedReceiverStream<IOCmd>>>,
        weak_session_inner: Weak<AndroidSessionInner>,
        ble_device: GlobalRef,
        mut cancellation: tokio::sync::oneshot::Receiver<AndroidSession>) -> (Pin<&mut Fuse<UnboundedReceiverStream<IOCmd>>>, Result<()>)
    {
        let mut state = IOProcessorState {
            queue: VecDeque::new(),

            pending: None,
            pending_started_at: None,
            pending_timeout_duration: Duration::from_secs(5),

            waiting_for_bond: false,

            quit: false,
            skip_disconnect_on_quit: false
        };

        let timeout = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(timeout);

        //let cmd_stream = tokio_stream::wrappers::UnboundedReceiverStream::new(io_bus).fuse();
        //tokio::pin!(cmd_stream);
        loop {
            // TODO: support request timeouts

            tokio::select! {
                Some(cmd) = cmd_stream.next() => {
                    let session =  {
                        let android_session_inner = weak_session_inner.upgrade();
                        if android_session_inner.is_none() {
                            //let io_bus = cmd_stream.into_inner().into_inner();
                            return (cmd_stream, Err(Error::Other(anyhow!("peripheral IO task failed to upgrade session reference"))));
                        }

                        AndroidSession::wrap_inner(android_session_inner.unwrap())
                    };

                    // Requests just get queued to ensure they are processed sequentially and
                    // notifications (which may complete an in-progress) request are handled
                    // immediately...
                    match cmd {
                        IOCmd::RequestConnect { .. } |
                        //IOCmd::RequestDisconnect { .. } |
                        IOCmd::ReadRSSI { .. } |
                        IOCmd::RequestDiscoverServices { .. } |
                        IOCmd::RequestReadCharacteristic { .. } |
                        IOCmd::RequestWriteCharacteristic { .. } |
                        IOCmd::RequestSubscribeCharacteristic { .. } |
                        IOCmd::RequestUnsubscribeCharacteristic { .. } |
                        IOCmd::RequestReadDescriptor { .. } |
                        IOCmd::RequestWriteDescriptor { .. }
                        => {
                            state.queue.push_back(cmd);
                        },
                        IOCmd::ConnectionStatusNotify { .. } |
                        IOCmd::BondingStateNotify { .. } |
                        IOCmd::FinishReadRSSI { .. } |
                        IOCmd::FinishDiscoverServices { .. } |
                        IOCmd::FinishReadCharacteristic { .. } |
                        IOCmd::FinishWriteCharacteristic { .. } |
                        IOCmd::CharacteristicNotify { .. } |
                        IOCmd::FinishReadDescriptor { .. } |
                        IOCmd::FinishWriteDescriptor { .. }
                        => {
                            if let Err(err) = AndroidSession::handle_io_notification(&mut state, session.clone(), cmd, peripheral_handle, ble_device.clone()).await {
                                error!("IO Task: Failed to handle callback / notification: {err}");
                                // XXX: we should probably check for certain errors and in some cases
                                // abort the IO loop and disconnect / close the device
                            }
                        }
                    }

                }
                () = &mut timeout => {

                }
                cancel = &mut cancellation => {
                    match cancel {
                        Err(err) => {
                            //debug!("Peripheral IO task has been cancelled (cancellation TX dropped)");
                            //let io_bus = cmd_stream.into_inner().into_inner();
                            return (cmd_stream, Err(Error::Other(anyhow!("Peripheral IO task: cancellation TX was dropped"))));
                        },
                        Ok(_) => {
                            debug!("Peripheral IO task has been cancelled (explicitly)");
                            //let io_bus = cmd_stream.into_inner().into_inner();
                            return (cmd_stream, Ok(()));
                        }
                    }
                    //break;
                }
                else => {
                    //debug!("Peripheral IO task stream ended");
                    //let io_bus = cmd_stream.into_inner().into_inner();
                    return (cmd_stream, Err(Error::Other(anyhow!("Peripheral IO task: command stream ended"))));
                    //break;
                }
            }

            loop {
                if state.pending.is_some() {
                    break;
                }
                if state.waiting_for_bond {
                    break;
                }
                let request = match state.queue.pop_front() {
                    Some(r) => r,
                    None => break,
                };

                let session =  {
                    let android_session_inner = weak_session_inner.upgrade();
                    if android_session_inner.is_none() {
                        //let io_bus = cmd_stream.into_inner().into_inner();
                        return (cmd_stream, Err(Error::Other(anyhow!("peripheral IO task failed to upgrade session reference"))));
                    }

                    AndroidSession::wrap_inner(android_session_inner.unwrap())
                };

                error!("IO: Checking get_env() before calling handle_io_request()");
                if let Err(err) = session.jvm.get_env() {
                    error!("IO: can't query session JVM: {err:?}");
                }
                error!("IO: Calling handle_io_request(): {request:?}");
                if let Err(err) = AndroidSession::handle_io_request(&mut state, session, request, peripheral_handle, ble_device.clone()).await {
                    error!("IO Task: Failed to execute IO request: {err}");
                    // XXX: we should probably check for certain errors and in some cases
                    // abort the IO loop and disconnect / close the device
                }
            }
        }

    }

    fn notify_disconnect(&self, peripheral_handle: PeripheralHandle, error: Option<GattError>) {
        trace!("notify_disconnect");

        self.peripheral_drop_gatt_state(peripheral_handle);
        let _ = self.inner
                    .backend_bus
                    .send(BackendEvent::PeripheralDisconnected { peripheral_handle, error });
    }

    async fn disconnect_and_close(
        peripheral_handle: PeripheralHandle,
        weak_session_inner: Weak<AndroidSessionInner>,
        mut cmd_stream: Pin<&mut Fuse<UnboundedReceiverStream<IOCmd>>>,
        ble_device: GlobalRef,
    ) -> Result<()> {
        let session =  {
            let android_session_inner = weak_session_inner.upgrade();
            if android_session_inner.is_none() {
                return Err(Error::Other(anyhow!("peripheral IO task failed to upgrade session reference for GATT clean up")));
            }

            AndroidSession::wrap_inner(android_session_inner.unwrap())
        };


        let (has_gatt_interface, device_connected) = {
            let android_peripheral = session.android_peripheral_from_handle(peripheral_handle)?;
            let mut guard = android_peripheral.state.read().unwrap();
            (guard.has_gatt_interface, guard.connected)
        };

        if has_gatt_interface {
            // Note: we call BluetoothGatt.disconnect() even if the device is not connected,
            // to potentially cancel any pending connection request, or in case the device has
            // connected and we just haven't recieved the notification yet.

            {
                let jenv = session.jvm.get_env()?;
                try_call_void_method(jenv, session.jsession.as_obj(), session.disconnect_device_gatt_method,
                                    &[JValue::Object(ble_device.as_obj()).to_jni()])?;
            }

            // Wait for the disconnect notification...
            // XXX: In case Android doesn't send a notification if we are already disconnected
            // we have a timeout
            let timeout = tokio::time::sleep(Duration::from_secs(15));
            tokio::pin!(timeout);

            loop {
                trace!("Waiting for disconnect notification before closing device");

                tokio::select! {
                    Some(request) = cmd_stream.next() => {
                        match request {
                            IOCmd::ConnectionStatusNotify { connected, status } => {
                                if connected == false {
                                    let error = Option::<GattError>::from(status);
                                    session.notify_disconnect(peripheral_handle, error);
                                } else {
                                    // If we are asked to cancel a pending connection I suppose it's possible we might get
                                    // a connect notification if we connect just before the request to disconnect and
                                    // we should still expect a future disconnect notification (so we don't clear
                                    // state.pending here)
                                    warn!("Spurious connect notification after requesting disconnect");
                                }
                            }
                            _ => {
                                warn!("Ignoring IO {:?} while waiting for disconnect", request);
                            }
                        }
                    }
                    () = &mut timeout => {
                        warn!("Timeout waiting for peripheral disconnect");
                        session.notify_disconnect(peripheral_handle, None);
                        break;
                    }
                    else => {
                        //debug!("Peripheral IO task stream ended");
                        error!("IO Task: couldn't wait for disconnect (IO channel closed)");
                        session.notify_disconnect(peripheral_handle, None);
                        break;
                    }
                }
            }
        }

        let jenv = session.jvm.get_env()?;
        try_call_void_method(jenv, session.jsession.as_obj(), session.close_device_gatt_method,
                                &[JValue::Object(ble_device.as_obj()).to_jni()])?;

        {
            let android_peripheral = session.android_peripheral_from_handle(peripheral_handle)?;
            let mut guard = android_peripheral.state.write().unwrap();
            guard.has_gatt_interface = false;
        }
        debug!("IO Task: peripheral has closed BluetoothGatt instance");

        Ok(())
    }

    // Note: this task is not cancellation safe (since we rely on a clean exit to
    // handle disconnecting / closing the device GATT) so we don't use tokio's
    // JoinHandle::abort() API when we want to stop the task, instead we use a
    // oneshot to signal that we want to cancel the task.
    async fn run_peripheral_io_task(peripheral_handle: PeripheralHandle,
        io_bus: mpsc::UnboundedReceiver<IOCmd>,
        weak_session_inner: Weak<AndroidSessionInner>,
        ble_device: GlobalRef,
        cancellation: tokio::sync::oneshot::Receiver<AndroidSession>)
    {
        trace!("Starting I/O task for peripheral {:?}", peripheral_handle);
        {
            let session =  {
                let android_session_inner = weak_session_inner.upgrade().unwrap();
                AndroidSession::wrap_inner(android_session_inner)
            };
            let jenv = match session.jvm.get_env() {
                Ok(jenv) => jenv,
                Err(err) => {
                    let thread_id = std::thread::current().id();
                    error!("Failed to query JNI env in new IO task {err}, thread id = {thread_id:?}");

                    return;
                }
            };
        }

        let cmd_stream = tokio_stream::wrappers::UnboundedReceiverStream::new(io_bus).fuse();
        tokio::pin!(cmd_stream);

        let cmd_stream = match AndroidSession::run_peripheral_io_task_command_loop(peripheral_handle,
            cmd_stream, weak_session_inner.clone(), ble_device.clone(), cancellation).await
        {
            (cmd_stream, Ok(_)) => {
                debug!("Peripheral IO task loop finished cleanly");
                cmd_stream
            }
            (cmd_stream, Err(err)) => {
                error!("Peripheral IO task loop quit with an error: {}", err);
                cmd_stream
            }
        };

        trace!("Finished I/O task processing for peripheral {:?}", peripheral_handle);
        trace!("Closing device GATT");
        match AndroidSession::disconnect_and_close(peripheral_handle, weak_session_inner, cmd_stream, ble_device).await {
            Ok(_) => { debug!("Cleaned up GATT state for peripheral") },
            Err(err) => { error!("Failed to gracefully clean up GATT state for peripheral: {}", err) }
        }
    }

    fn cancel_peripheral_io_task(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        let android_peripheral = self.android_peripheral_from_handle(peripheral_handle)?;

        let mut guard = android_peripheral.state.write().unwrap();

        // Tell any IO task to quit. This will complete asynchronously. Before the IO task really
        // quits then it will effectively call BluetoothGatt.close() to disconnect and remove
        // our gatt callbacks.
        //
        // Note: the authorative io_bus_tx is actually stored under android_peripheral.io and in case
        // we need to start a new IO task to re-connect we will be able to use the authoritive state
        // to also confirm that any old task has finished before starting a new one.
        if let Some(cancellation) = std::mem::take(&mut guard.io_task_cancellation) {

            trace!("Cancelling IO task for peripheral {peripheral_handle:?}");
            // Send the cancellation a temporary reference to the session to ensure it stays
            // alive until the IO task has cleaned up + quit...
            cancellation.send(self.clone());

            guard.io_bus_tx = None;
        }

        Ok(())
    }

    // Make sure we've got a BluetoothDevice for our peripheral and a spawned IO task
    // that will manage a queue of IO requests to ensure we serialize GATT requests
    // via the Android API
    //
    // (NB: we don't keep BluetoothDevice references for every peripheral discovered via
    // advertising packets - we only need a device once an application is interested in
    // connecting to the peripheral)
    //
    // Note: This doesn't initiate a connectGatt, so any newly constructed BleDevice
    // won't yet have any associated `connectedGatt` state/callbacks.
    //
    async fn ensure_peripheral_binding_and_io_task(&self, peripheral_handle: PeripheralHandle)
        -> Result<mpsc::UnboundedSender<IOCmd>>
    {
        let android_peripheral = self.android_peripheral_from_handle(peripheral_handle)?;

        debug!("ensure_peripheral_binding_and_io_task");

        // NB: An IO task is represented by an unbounded channel and a oneshot for
        // cancellation. Referenced as io_bus_tx and io_task_cancellation_tx.
        //
        // We keep _two_ clones of the the TX/sender ends of both of
        // these; one pair stored under android_peripheral.state with a synchronous
        // lock and one pair store under android_peripheral.io with an aync lock.
        //
        // The .state pair are accessible from a synchronous context so that we
        // can request the task to quit from a synchronous context (Drop). At the
        // same time as cancelling, then the .state pair are cleared (set to None)
        // (See peripheral_drop_gatt_state)
        //
        // The .io pair aren't accessible in a synchronous context and aren't cleared
        // when an application peripheral is dropped. This means if the application
        // re-discoveres and decides to re-use the same peripheral we can recognise that
        // the peripheral was previously associated with an IO task, and we can
        // confirm that it has really closed (or wait if necessary) before starting
        // a new IO task.

        let mut io_guard = android_peripheral.io.lock().await;

        // There are essentially three states we need to consider here:
        //
        // RUNNING: state.io_bus_tx.is_some() (also implies io.io_bus_tx.is_some())
        // CLOSING: state.io_bus_tx.is_none() and io.io_bus_tx.is_some()
        // CLOSED: io.io_bus_tx.is_none() (also implies state.io_bus_tx.is_none())

        let mac = {
            let state_guard = android_peripheral.state.read().unwrap();

            if let Some(ref io_bus_tx) = state_guard.io_bus_tx { // RUNNING
                return Ok(io_bus_tx.clone());
            }

            state_guard.address
        };

        if let Some(ref io_bus_tx) = io_guard.io_bus_tx { // CLOSING
            // We assume that at this point something else has already requested any
            // pre-existing IO task to exit via io_task_cancellation_tx.send(())
            debug!("Waiting for IO task to exit...");
            io_bus_tx.closed().await;
            io_guard.io_bus_tx = None;
            io_guard.io_task_cancellation_tx = None;
            // CLOSED
        }

        // If not, look up a device, register a connection handler and associate both
        // with the android_peripheral...

        let jenv = self.jvm.get_env()?;
        let address_str = MAC(mac).to_string(); // Will format in uppercase as Android expects
        debug!("calling get_device_for_address with addr = {}", &address_str);

        let address_jstr = jenv.new_string(address_str)?;

        let ble_device_ref = try_call_object_method(jenv, self.jsession.as_obj(), self.get_device_for_address_method,
            &[JValue::Object(*address_jstr).to_jni(),
                    peripheral_handle.into()])?;
                    //JValue::Long(android_peripheral.peripheral_handle.0 as jlong).to_jni()])?;
        let global_device_ref = jenv.new_global_ref(ble_device_ref)?;

        let (io_bus_tx, io_bus_rx) = mpsc::unbounded_channel();
        let (io_cancellation_tx, io_cancellation_rx) = tokio::sync::oneshot::channel();

        {
            let mut android_peripheral_guard = android_peripheral.state.write().unwrap();

            android_peripheral_guard.ble_device = Some(global_device_ref.clone());
            android_peripheral_guard.io_bus_tx = Some(io_bus_tx.clone());
            android_peripheral_guard.io_task_cancellation = Some(io_cancellation_tx);

            debug!("associated BluetoothDevice with peripheral and spawning IO task");
        }

        // Only give the IO task a weak reference to the session to avoid a ref cycle...

        let thread_id = std::thread::current().id();
        error!("About to spawn new IO task from thread id = {thread_id:?}");
        let weak_session_inner = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            AndroidSession::run_peripheral_io_task(peripheral_handle,
                io_bus_rx,
                weak_session_inner,
                global_device_ref,
                io_cancellation_rx).await
        });

        Ok(io_bus_tx)
    }

    fn get_peripheral_io_task_bus(&self, peripheral_handle: PeripheralHandle)
        -> Result<mpsc::UnboundedSender<IOCmd>>
    {
        let android_peripheral = self.android_peripheral_from_handle(peripheral_handle)?;

        debug!("get_peripheral_io_task_bus");

        let state_guard = android_peripheral.state.read().unwrap();

        if let Some(ref io_bus_tx) = state_guard.io_bus_tx { // RUNNING
            Ok(io_bus_tx.clone())
        } else {
            // XXX: note quite sure what error to throw here?
            // If the IO task was shut down gracefully and reported as disconnected then the
            // frontend shouldn't call through to us for GATT requests when it knows we're
            // disconnected.
            Err(Error::InvalidStateReference)
        }
    }

    fn get_ble_device(&self, android_peripheral: &AndroidPeripheral) -> Result<GlobalRef>
    {
        debug!("get_ble_device");

        let state_guard = android_peripheral.state.read().unwrap();
        match state_guard.ble_device {
            Some(ref ble_device) => {
                Ok(ble_device.clone())
            },
            None => {
                // Would likely represent an internal bug, since we shouldn't be dealing with
                // requests that need a BleDevice if we are disconnected
                Err(Error::InvalidStateReference)
            }
        }
    }

    fn gatt_service_from_handle(&self, android_peripheral: &AndroidPeripheral,
                                service_handle: ServiceHandle)
                                -> Result<GlobalRef> {
        // Only need a 'read' lock since the DashMap has interior mutability.
        let guard = android_peripheral.state.read().unwrap();

        let gatt_service = guard.gatt_services.get(&service_handle);
        match gatt_service {
            Some(gatt_service) => Ok(gatt_service.clone()),
            None => {
                warn!("Request made with invalid service handle");
                Err(Error::InvalidStateReference)
            }
        }
    }

    fn gatt_characteristic_from_handle(&self, android_peripheral: &AndroidPeripheral,
                                       characteristic_handle: CharacteristicHandle)
                                       -> Result<GlobalRef> {
        // Only need a 'read' lock since the DashMap has interior mutability.
        let guard = android_peripheral.state.read().unwrap();

        let result = match guard.gatt_characteristics.get(&characteristic_handle) {
            Some(gatt_characteristic) => Ok(gatt_characteristic.clone()),
            None => {
                warn!("Request made with invalid characteristic handle");
                Err(Error::InvalidStateReference)
            }
        };

        // NB: we explicitly assign to `result` to ensure the temporary lock guard
        // is dropped before returning the result (will get a compiler error otherwise)
        result
    }

    fn gatt_descriptor_from_handle(&self, android_peripheral: &AndroidPeripheral,
                                   descriptor_handle: DescriptorHandle)
                                   -> Result<GlobalRef> {
        // Only need a 'read' lock since the DashMap has interior mutability.
        let guard = android_peripheral.state.read().unwrap();

        let result = match guard.gatt_descriptors.get(&descriptor_handle) {
            Some(gatt_descriptor) => Ok(gatt_descriptor.clone()),
            None => {
                warn!("Request made with invalid descriptor handle");
                Err(Error::InvalidStateReference)
            }
        };

        // NB: we explicitly assign to `result` to ensure the temporary lock guard
        // is dropped before returning the result (will get a compiler error otherwise)
        result
    }

    fn get_service_handle<'a>(&self, jenv: JNIEnv<'a>, android_peripheral: &AndroidPeripheral, service: JObject<'a>) -> Result<ServiceHandle> {
        let id = try_call_int_method(jenv, self.jsession.as_obj(), self.get_service_instance_id_method,
            &[JValue::Object(service).to_jni()])?;
        Ok(ServiceHandle(id as u32))
    }

    fn get_service_uuid<'a>(&self, jenv: JNIEnv<'a>, android_peripheral: &AndroidPeripheral, service: JObject<'a>) -> Result<uuid::Uuid> {
        match try_call_string_method(jenv, self.jsession.as_obj(), self.get_service_uuid_method,
            &[JValue::Object(service).to_jni()])?
        {
            Some(uuid_str) => {
                let uuid = uuid::Uuid::parse_str(&uuid_str)?;
                Ok(uuid)
            }
            None => {
                Err(Error::Other(anyhow!("Failed to get BluetoothGattService Uuid")))
            }
        }
    }

    fn index_services_list(&self, android_peripheral: &AndroidPeripheral, services: &JList)
        -> Result<()>
    {
        let jenv = self.jvm.get_env()?;

        // Only need a 'read' lock since the DashMap has interior mutability.
        let guard = android_peripheral.state.read().unwrap();

        for service in services.iter()? {
            let service_handle = self.get_service_handle(jenv, android_peripheral, service)?;
            if !guard.gatt_services.contains_key(&service_handle) {
                let global_ref = jenv.new_global_ref(service)?;
                guard.gatt_services.insert(service_handle, global_ref);
            }
        }

        Ok(())
    }

    fn get_characteristic_handle<'a>(&self, jenv: JNIEnv<'a>, android_peripheral: &AndroidPeripheral, characteristic: JObject<'a>) -> Result<CharacteristicHandle> {
        let id = try_call_int_method(jenv, self.jsession.as_obj(), self.get_characteristic_instance_id_method,
            &[JValue::Object(characteristic).to_jni()])?;
        debug!("Created new characterisitc handle :{id}");
        Ok(CharacteristicHandle(id as u32))
    }

    fn get_characteristic_uuid<'a>(&self, jenv: JNIEnv<'a>, android_peripheral: &AndroidPeripheral, characteristic: JObject<'a>) -> Result<uuid::Uuid> {
        match try_call_string_method(jenv, self.jsession.as_obj(), self.get_characteristic_uuid_method,
            &[JValue::Object(characteristic).to_jni()])?
        {
            Some(uuid_str) => {
                let uuid = uuid::Uuid::parse_str(&uuid_str)?;
                Ok(uuid)
            }
            None => {
                Err(Error::Other(anyhow!("Failed to get BluetoothGattCharacteristic Uuid")))

            }
        }
    }

    fn get_characteristic_properties<'a>(&self, jenv: JNIEnv<'a>, android_peripheral: &AndroidPeripheral, characteristic: JObject<'a>) -> Result<CharacteristicProperties> {
        let props = try_call_int_method(jenv, self.jsession.as_obj(), self.get_characteristic_properties_method,
            &[JValue::Object(characteristic).to_jni()])?;
        Ok(CharacteristicProperties::from_bits_truncate(props as u32))
    }

    // Note: Since descriptors don't have an instance ID (like services and characteristics)
    // we ask Java to associate a unique ID with each BluetoothGattDescriptor that can be
    // used with JNI
    fn get_descriptor_handle<'a>(&self, jenv: JNIEnv<'a>, ble_device: JObject<'a>, descriptor: JObject<'a>) -> Result<DescriptorHandle> {
        let id = try_call_int_method(jenv, self.jsession.as_obj(), self.get_descriptor_id_method,
            &[JValue::Object(ble_device).to_jni(),
                    JValue::Object(descriptor).to_jni()])?;
        Ok(DescriptorHandle(id as u32))
    }

    fn get_descriptor_uuid<'a>(&self, jenv: JNIEnv<'a>, android_peripheral: &AndroidPeripheral, descriptor: JObject<'a>) -> Result<uuid::Uuid> {
        match try_call_string_method(jenv, self.jsession.as_obj(), self.get_descriptor_uuid_method,
            &[JValue::Object(descriptor).to_jni()])?
        {
            Some(uuid_str) => {
                let uuid = uuid::Uuid::parse_str(&uuid_str)?;
                Ok(uuid)
            }
            None => {
                Err(Error::Other(anyhow!("Failed to get BluetoothGattDescriptor Uuid")))

            }
        }
    }
}

/*
Ref: https://android.googlesource.com/platform/external/bluetooth/bluedroid/+/adc9f28ad418356cb81640059b59eee4d862e6b4/stack/include/gatt_api.h#54

#define  GATT_SUCCESS                        0x00
#define  GATT_INVALID_HANDLE                 0x01
#define  GATT_READ_NOT_PERMIT                0x02
#define  GATT_WRITE_NOT_PERMIT               0x03
#define  GATT_INVALID_PDU                    0x04
#define  GATT_INSUF_AUTHENTICATION           0x05
#define  GATT_REQ_NOT_SUPPORTED              0x06
#define  GATT_INVALID_OFFSET                 0x07
#define  GATT_INSUF_AUTHORIZATION            0x08
#define  GATT_PREPARE_Q_FULL                 0x09
#define  GATT_NOT_FOUND                      0x0a
#define  GATT_NOT_LONG                       0x0b
#define  GATT_INSUF_KEY_SIZE                 0x0c
#define  GATT_INVALID_ATTR_LEN               0x0d
#define  GATT_ERR_UNLIKELY                   0x0e
#define  GATT_INSUF_ENCRYPTION               0x0f
#define  GATT_UNSUPPORT_GRP_TYPE             0x10
#define  GATT_INSUF_RESOURCE                 0x11
#define  GATT_ILLEGAL_PARAMETER              0x87
#define  GATT_NO_RESOURCES                   0x80
#define  GATT_INTERNAL_ERROR                 0x81
#define  GATT_WRONG_STATE                    0x82
#define  GATT_DB_FULL                        0x83
#define  GATT_BUSY                           0x84
#define  GATT_ERROR                          0x85
#define  GATT_CMD_STARTED                    0x86
#define  GATT_PENDING                        0x88
#define  GATT_AUTH_FAIL                      0x89
#define  GATT_MORE                           0x8a
#define  GATT_INVALID_CFG                    0x8b
#define  GATT_SERVICE_STARTED                0x8c
#define  GATT_ENCRYPED_MITM                  GATT_SUCCESS
#define  GATT_ENCRYPED_NO_MITM               0x8d
#define  GATT_NOT_ENCRYPTED                  0x8e
#define  GATT_CONGESTED                      0x8f
                                             /* 0xE0 ~ 0xFC reserved for future use */
#define  GATT_CCC_CFG_ERR                    0xFD /* Client Characteristic Configuration Descriptor Improperly Configured */
#define  GATT_PRC_IN_PROGRESS                0xFE /* Procedure Already in progress */
#define  GATT_OUT_OF_RANGE                   0xFF /* Attribute value out of range */

*/

#[derive(Debug)]
enum AndroidGattStatus {
    Success, // 0x00
    InvalidHandle, // 0x01
    ReadNotPermit, // 0x02
    WriteNotPermit, // 0x03
    InvalidPdu, // 0x04
    InsufAuthentication, // 0x05
    ReqNotSupported, // 0x06
    InvalidOffset, // 0x07
    InsufAuthorization, // 0x08
    PrepareQueueFull, // 0x09
    NotFound, // 0x0a
    NotLong, // 0x0b
    InsufKeySize, // 0x0c
    InvalidAttrLen, // 0x0d
    ErrUnlikely, // 0x0e
    InsufEncryption, // 0x0f
    UnsupportedGrpType, // 0x10
    InsufResource, // 0x11
    IllegalParameter, // 0x87
    NoResources, // 0x80
    InternalError, // 0x81
    WrongState, // 0x82
    DbFull, // 0x83
    Busy, // 0x84
    Error, // 0x85
    CmdStarted, // 0x86
    Pending, // 0x88
    AuthFail, // 0x89
    More, // 0x8a
    InvalidCfg, // 0x8b
    ServiceStarted, // 0x8c
    EncrypedMitm, // GTT_SUCCESS (XXX: indistinguishable!)
    EncrypedNoMitm, // 0x8d
    NotEncrypted, // 0x8e
    Congested, // 0x8f

    /*, //FC reserved for future use */

    DescriptorCfgErr, // 0xFD /* Client Characteristic Configuration Descriptor Improperly Configured */
    PrcInProgress, // 0xFE /* Procedure Already in progress */
    OutOfRange, // 0xFF /* Attribute value out of range */

    UnknownError,
}

const fn map_gatt_status(status: jint, zero_success: bool) -> AndroidGattStatus {
    match status {
        0x00 => { if zero_success { AndroidGattStatus::Success } else { AndroidGattStatus::EncrypedMitm } },

        0x01 => AndroidGattStatus::InvalidHandle,
        0x02 => AndroidGattStatus::ReadNotPermit,
        0x03 => AndroidGattStatus::WriteNotPermit,
        0x04 => AndroidGattStatus::InvalidPdu,
        0x05 => AndroidGattStatus::InsufAuthentication,
        0x06 => AndroidGattStatus::ReqNotSupported,
        0x07 => AndroidGattStatus::InvalidOffset,
        0x08 => AndroidGattStatus::InsufAuthorization,
        0x09 => AndroidGattStatus::PrepareQueueFull,
        0x0a => AndroidGattStatus::NotFound,
        0x0b => AndroidGattStatus::NotLong,
        0x0c => AndroidGattStatus::InsufKeySize,
        0x0d => AndroidGattStatus::InvalidAttrLen,
        0x0e => AndroidGattStatus::ErrUnlikely,
        0x0f => AndroidGattStatus::InsufEncryption,
        0x10 => AndroidGattStatus::UnsupportedGrpType,
        0x11 => AndroidGattStatus::InsufResource,
        0x87 => AndroidGattStatus::IllegalParameter,
        0x80 => AndroidGattStatus::NoResources,
        0x81 => AndroidGattStatus::InternalError,
        0x82 => AndroidGattStatus::WrongState,
        0x83 => AndroidGattStatus::DbFull,
        0x84 => AndroidGattStatus::Busy,
        0x85 => AndroidGattStatus::Error,
        0x86 => AndroidGattStatus::CmdStarted,
        0x88 => AndroidGattStatus::Pending,
        0x89 => AndroidGattStatus::AuthFail,
        0x8a => AndroidGattStatus::More,
        0x8b => AndroidGattStatus::InvalidCfg,
        0x8c => AndroidGattStatus::ServiceStarted,
        0x8d => AndroidGattStatus::EncrypedNoMitm,
        0x8e => AndroidGattStatus::NotEncrypted,
        0x8f => AndroidGattStatus::Congested,

        /*, //FC reserved for future use */

        0xFD => AndroidGattStatus::DescriptorCfgErr,
        0xFE => AndroidGattStatus::PrcInProgress,
        0xFF => AndroidGattStatus::OutOfRange,

        _ => AndroidGattStatus::UnknownError
    }
}

fn notify_io_callback_from_jni<F>(
    env: JNIEnv,
    session_handle: JHandle<AndroidSession>,
    device_handle: jlong, // u32 peripheral handle
    function_name_debug: &str,
    func: F)
    where
        F: FnOnce(AndroidSession, PeripheralHandle) -> Result<IOCmd>
{
    // Note: we currently avoid using env.throw() to propogate errors as exceptions
    // to Jave, to avoid additional complexity having to handle callback exceptions
    // in Java
    //
    // Should we also catch panics here?

    if let Some(session) = unsafe { IntoJHandle::clone_from_weak_handle(session_handle) } {
            let peripheral_handle = PeripheralHandle(device_handle as u32);
            let io_bus_tx = match session.get_peripheral_io_task_bus(peripheral_handle) {
                Ok(io_bus_tx) => io_bus_tx,
                Err(err) => {
                    error!("JNI callback {function_name_debug} failed to map device handle to peripheral");
                    return;
                }
            };
        match func(session, peripheral_handle) {
            Ok(cmd) => {
                trace!("Sending IO notification ({cmd:?}) from {function_name_debug}");

                if let Err(send_err) = io_bus_tx.send(cmd) {
                    error!("JNI callback {function_name_debug} failed to send notification due to IO bus send error: {send_err:?}");

                    // XXX: If there's no running IO task should we try to trigger a disconnect/close somehow?
                }
            }
            Err(err) => {
                error!("JNI callback {function_name_debug} failed to prepare notification: {err:?}");
            }
        }
    } else {
        error!("Java callback failed to get native session from handle");
    }
}


#[async_trait]
impl BackendSession for AndroidSession {
    async fn start_scanning(&self, filter: &Filter) -> Result<()> {
        debug!("BLE: backend: start_scanning");
        let jenv = self.jvm.get_env()?;
        let ble_session = self.jsession.as_obj();

        try_call_void_method(jenv, ble_session, self.scanner_config_reset_method, &[])?;
        for uuid in filter.service_uuids.iter() {
            let uuid_str = uuid.to_string();
            let uuid_jstr: JObject = jenv.new_string(uuid_str)?.into();
            try_call_void_method(jenv, ble_session, self.scanner_config_add_service_uuid_method, &[
                JValue::Object(uuid_jstr).to_jni()
            ])?;
        }
        try_call_void_method(jenv, ble_session, self.start_scanning_method, &[])?;
        debug!("BLE: backend: start_scanning: jni done");

        Ok(())
    }

    async fn stop_scanning(&self) -> Result<()> {
        debug!("BLE: backend: stop_scanning");
        let jenv = self.jvm.get_env()?;
        let ble_session = self.jsession.as_obj();

        try_call_void_method(jenv, ble_session, self.stop_scanning_method, &[])?;
        debug!("BLE: backend: stop_scanning: jni done");

        Ok(())
    }

    fn declare_peripheral(&self, address: Address, name: String) -> Result<PeripheralHandle> {

        // On Android bluetooth addresses are MAC48 addresses stored as uppercase
        // hex octets separated by colons like: "F0:E1:D2:C3:B4:A5"
        //
        // It looks like we can rely on their format being a stable part of the API
        // (we don't have to treat them as opaque strings) considering that there
        // is also a BluetoothAdapater.checkBluetoothAddress() which also documents that it
        // expects this format, and there is a BluetoothAdapter.getRemoteDevice() API
        // that optionally takes a 6 byte[] array for the address, instead of a
        // String.

        let mac = match address {
            Address::MAC(MAC(mac)) => mac,
            Address::String(address_str) => {
                match try_u64_from_mac48_str(&address_str) {
                    Some(mac) => mac,
                    None => return Err(Error::Other(anyhow!("Unsupported device address format: {}", address_str)))
                }
            }
            _ => return Err(Error::Other(anyhow!("Unsupported device address format: {:?}", address)))
        };

        let peripheral_handle = self.peripheral_from_mac(mac)?;
        let _ = self
            .backend_bus
            .send(BackendEvent::PeripheralPropertySet {
                peripheral_handle,
                property: BackendPeripheralProperty::Name(name.to_string()),
            });
        Ok(peripheral_handle)
    }

    #[named]
    async fn peripheral_connect(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        trace!(function_name!());

        let io_bus_tx = self.ensure_peripheral_binding_and_io_task(peripheral_handle).await?;

        //let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        io_bus_tx.send(IOCmd::RequestConnect { })?;

        //let status = oneshot_rx.await?;
        //status
        Ok(())
    }

    async fn peripheral_disconnect(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        // TODO: add a try_peripheral_drop_gatt_state method which returns
        // a Result
        self.peripheral_drop_gatt_state(peripheral_handle);
        Ok(())
    }

    fn peripheral_drop_gatt_state(&self, peripheral_handle: PeripheralHandle) {
        if let Err(err) = self.cancel_peripheral_io_task(peripheral_handle) {
            error!("Failed to cancel IO task for peripheral {peripheral_handle:?}: {err}");
        }

        if let Ok(android_peripheral) = self.android_peripheral_from_handle(peripheral_handle) {
            let peripheral_state = android_peripheral.inner.state.read().unwrap();
            peripheral_state.gatt_services.clear();
            peripheral_state.gatt_characteristics.clear();
            peripheral_state.gatt_descriptors.clear();
        }
    }

    async fn peripheral_read_rssi(&self, peripheral_handle: PeripheralHandle) -> Result<i16> {
        let io_bus_tx = self.get_peripheral_io_task_bus(peripheral_handle)?;

        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        io_bus_tx.send(IOCmd::ReadRSSI { result_tx: oneshot_tx })?;

        oneshot_rx.await?
    }

    async fn peripheral_discover_gatt_services(&self, peripheral_handle: PeripheralHandle,
                                               of_interest_hint: Option<Vec<Uuid>>)
                                               -> Result<()> {
        let io_bus_tx = self.get_peripheral_io_task_bus(peripheral_handle)?;

        //let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        io_bus_tx.send(IOCmd::RequestDiscoverServices { })?;

        //oneshot_rx.await?
        Ok(())
    }

    async fn gatt_service_discover_includes(&self, peripheral_handle: PeripheralHandle,
                                            service_handle: ServiceHandle)
                                            -> Result<()> {
        debug!("gatt_service_discover_includes");
        let android_peripheral = self.android_peripheral_from_handle(peripheral_handle)?;
        let gatt_service = self.gatt_service_from_handle(&android_peripheral, service_handle)?;

        // Android doesn't explicitly expose include / characteristic discovery, since they
        // are also discovered as part of service discovery, so we can simply enumerate
        // the includes and forward them to the frontend...
        //
        // (Since this doesn't directly interact with the GATT server we don't handle this
        //  via the IO task.)

        let jenv = self.jvm.get_env()?;

        debug!("calling JNI getServiceIncludes()");
        let includes = try_call_object_method(jenv, self.jsession.as_obj(), self.get_service_includes_method,
            &[JValue::Object(gatt_service.as_obj()).to_jni()])?;
        let includes: JList = JList::from_env(&jenv, includes)?;
        debug!("enumerating includes...");
        for included_service in includes.iter()? {
            let included_uuid = self.get_service_uuid(jenv, &android_peripheral, included_service)?;
            let included_service_handle = self.get_service_handle(jenv, &android_peripheral, included_service)?;
            debug!("notifying include: uuid = {}", &included_uuid);
            let _ = self.backend_bus.send(BackendEvent::GattIncludedService {
                peripheral_handle,
                parent_service_handle: service_handle,
                included_service_handle,
                uuid: included_uuid
            } );
        }

        // XXX: For better consistency with other platforms maybe we should
        // queue the completion to be delivered asynchronously?

        let _ = self
            .backend_bus
            .send(BackendEvent::GattIncludedServicesComplete { peripheral_handle, service_handle, error: None } );

        Ok(())
    }

    async fn gatt_service_discover_characteristics(&self, peripheral_handle: PeripheralHandle,
                                                   service_handle: ServiceHandle)
                                                   -> Result<()> {
        debug!("gatt_service_discover_characteristics");

        let android_peripheral = self.android_peripheral_from_handle(peripheral_handle)?;
        let peripheral_state = android_peripheral.inner.state.read().unwrap();
        let gatt_service = self.gatt_service_from_handle(&android_peripheral, service_handle)?;

        // Android doesn't explicitly expose include / characteristic discovery, since they
        // are also discovered as part of service discovery, so we can simply enumerate
        // the characteristics and forward them to the frontend...
        //
        // (Since this doesn't directly interact with the GATT server we don't handle this
        //  via the IO task.)

        let jenv = self.jvm.get_env()?;

        debug!("calling JNI getServiceCharacteristics()");
        let characteristics = try_call_object_method(jenv, self.jsession.as_obj(), self.get_service_characteristics_method,
            &[JValue::Object(gatt_service.as_obj()).to_jni()])?;
        let characteristics: JList = JList::from_env(&jenv, characteristics)?;
        debug!("enumerating characteristics...");
        for characteristic in characteristics.iter()? {
            let characteristic_uuid = self.get_characteristic_uuid(jenv, &android_peripheral, characteristic)?;
            let characteristic_handle = self.get_characteristic_handle(jenv, &android_peripheral, characteristic)?;
            let characteristic_global_ref = jenv.new_global_ref(characteristic)?;
            peripheral_state.gatt_characteristics.insert(characteristic_handle, characteristic_global_ref);

            let properties = self.get_characteristic_properties(jenv, &android_peripheral, characteristic)?;
            debug!("notifying characteristics: uuid = {}", &characteristic_uuid);
            let _ = self.backend_bus.send(BackendEvent::GattCharacteristic {
                peripheral_handle,
                service_handle,
                characteristic_handle,
                uuid: characteristic_uuid,
                properties
            } );
        }

        // XXX: For better consistency with other platforms maybe we should
        // queue the completion to be delivered asynchronously?

        let _ = self
            .backend_bus
            .send(BackendEvent::GattCharacteristicsComplete { peripheral_handle, service_handle, error: None } );

        Ok(())
    }

    async fn gatt_characteristic_read(&self, peripheral_handle: PeripheralHandle,
                                      service_handle: ServiceHandle,
                                      characteristic_handle: CharacteristicHandle,
                                      cache_mode: crate::CacheMode)
                                      -> Result<Vec<u8>> {
        let io_bus_tx = self.get_peripheral_io_task_bus(peripheral_handle)?;

        // Unlike most requests that notify their completion only via events we currently
        // support awaiting on the completion of a GATT characteristic read. We pass
        // a oneshot over to the IO task that will be signaled on completion
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        io_bus_tx.send(IOCmd::RequestReadCharacteristic { service_handle, characteristic_handle, result_tx: oneshot_tx })?;

        // Note we recieve a Result through the oneshot, and we are just unwrapping any
        // potential Recv error with the '?' here...
        oneshot_rx.await?
    }

    async fn gatt_characteristic_write(&self, peripheral_handle: PeripheralHandle,
                                       service_handle: ServiceHandle,
                                       characteristic_handle: CharacteristicHandle,
                                       write_type: crate::characteristic::WriteType, data: &[u8])
                                       -> Result<()> {
        let io_bus_tx = self.get_peripheral_io_task_bus(peripheral_handle)?;

        //let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        io_bus_tx.send(IOCmd::RequestWriteCharacteristic { service_handle, characteristic_handle, value: data.into(), write_type })?;

        //let status = oneshot_rx.await?;
        //status
        Ok(())
    }

    async fn gatt_characteristic_subscribe(&self, peripheral_handle: PeripheralHandle,
                                           service_handle: ServiceHandle,
                                           characteristic_handle: CharacteristicHandle)
                                           -> Result<()> {
        let io_bus_tx = self.get_peripheral_io_task_bus(peripheral_handle)?;
        io_bus_tx.send(IOCmd::RequestSubscribeCharacteristic { characteristic_handle })?;
        Ok(())
    }

    async fn gatt_characteristic_unsubscribe(&self, peripheral_handle: PeripheralHandle,
                                             service_handle: ServiceHandle,
                                             characteristic_handle: CharacteristicHandle)
                                             -> Result<()> {
        let io_bus_tx = self.get_peripheral_io_task_bus(peripheral_handle)?;
        io_bus_tx.send(IOCmd::RequestUnsubscribeCharacteristic { characteristic_handle })?;
        Ok(())
    }

    async fn gatt_characteristic_discover_descriptors(&self, peripheral_handle: PeripheralHandle,
                                                      service_handle: ServiceHandle,
                                                      characteristic_handle: CharacteristicHandle)
                                                   -> Result<()> {
        let android_peripheral = self.android_peripheral_from_handle(peripheral_handle)?;
        let peripheral_state = android_peripheral.inner.state.read().unwrap();
        let ble_device = self.get_ble_device(&android_peripheral)?;
        let gatt_characteristic = self.gatt_characteristic_from_handle(&android_peripheral, characteristic_handle)?;

        // Android doesn't explicitly expose descriptor discovery, since they
        // are also discovered as part of service discovery, so we can simply enumerate
        // the descriptors and forward them to the frontend...
        //
        // (Since this doesn't directly interact with the GATT server we don't handle this
        //  via the IO task.)

        let jenv = self.jvm.get_env()?;

        debug!("calling JNI getCharacteristicDescriptors()");
        let descriptors = try_call_object_method(jenv, self.jsession.as_obj(), self.get_characteristic_descriptors_method,
            &[JValue::Object(gatt_characteristic.as_obj()).to_jni()])?;
        let descriptors: JList = JList::from_env(&jenv, descriptors)?;
        debug!("enumerating descriptors...");
        for descriptor in descriptors.iter()? {
            let descriptor_uuid = self.get_descriptor_uuid(jenv, &android_peripheral, descriptor)?;
            // This will call through to Java to allocate a new ID for the descriptor that
            // can be passed over JNI
            let descriptor_handle = self.get_descriptor_handle(jenv, ble_device.as_obj(), descriptor)?;
            let descriptor_global_ref = jenv.new_global_ref(descriptor)?;
            peripheral_state.gatt_descriptors.insert(descriptor_handle, descriptor_global_ref);
            debug!("notifying descriptors: uuid = {}", &descriptor_uuid);
            let _ = self.backend_bus.send(BackendEvent::GattDescriptor {
                peripheral_handle,
                service_handle,
                characteristic_handle,
                descriptor_handle,
                uuid: descriptor_uuid
            } );
        }

        // XXX: For better consistency with other platforms maybe we should
        // queue the completion to be delivered asynchronously?

        let _ = self
            .backend_bus
            .send(BackendEvent::GattDescriptorsComplete { peripheral_handle, service_handle, characteristic_handle, error: None } );

        Ok(())
    }

    async fn gatt_descriptor_read(&self, peripheral_handle: PeripheralHandle,
                                  service_handle: ServiceHandle,
                                  characteristic_handle: CharacteristicHandle,
                                  descriptor_handle: DescriptorHandle,
                                  cache_mode: CacheMode)
                                  -> Result<Vec<u8>> {
        let io_bus_tx = self.get_peripheral_io_task_bus(peripheral_handle)?;

        // Unlike most requests that notify their completion only via events we currently
        // support awaiting on the completion of a GATT characteristic read. We pass
        // a oneshot over to the IO task that will be signaled on completion
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        io_bus_tx.send(IOCmd::RequestReadDescriptor { service_handle, characteristic_handle, descriptor_handle, result_tx: oneshot_tx })?;

        // Note we recieve a Result through the oneshot, and we are just unwrapping any
        // potential Recv error with the '?' here...
        oneshot_rx.await?
    }

    async fn gatt_descriptor_write(&self, peripheral_handle: PeripheralHandle,
                                   service_handle: ServiceHandle,
                                   characteristic_handle: CharacteristicHandle,
                                   descriptor_handle: DescriptorHandle,
                                   data: &[u8])
                                   -> Result<()> {
        let io_bus_tx = self.get_peripheral_io_task_bus(peripheral_handle)?;

        //let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        io_bus_tx.send(IOCmd::RequestWriteDescriptor { service_handle, characteristic_handle, descriptor_handle, value: data.into() })?;

        //oneshot_rx.await?
        Ok(())
    }

    fn flush(&self, id: u32) -> Result<()> {
        error!("TODO: flush");
        Err(Error::Other(anyhow!("TODO: flush")))
    }
}
