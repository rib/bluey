use async_trait::async_trait;
use std::borrow::Cow;
use std::char;
use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::sync::atomic::compiler_fence;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::RwLock as StdRwLock;

use super::uuid::WinrtUuid;
use bindings::Windows::Devices::Bluetooth::Advertisement::*;
use bindings::Windows::Devices::Bluetooth::BluetoothDeviceId;
use bindings::Windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristic, GattCharacteristicProperties, GattCharacteristicsResult,
    GattClientCharacteristicConfigurationDescriptorValue, GattCommunicationStatus,
    GattDeviceService, GattDeviceServicesResult, GattValueChangedEventArgs, GattWriteOption,
    GattProtocolError, GattSession, GattSessionStatusChangedEventArgs, GattSessionStatus,
    GattDescriptorsResult, GattDescriptor
};
use bindings::Windows::Devices::Bluetooth::{BluetoothAddressType, BluetoothCacheMode};
use bindings::Windows::Devices::Bluetooth::{BluetoothConnectionStatus, BluetoothLEDevice};
use bindings::Windows::Foundation::{EventRegistrationToken, IReference, TypedEventHandler};
use bindings::Windows::Storage::Streams::{DataReader, DataWriter, IBuffer};
use bindings::Windows::Win32::Foundation::RO_E_CLOSED;
use windows::Guid;

use anyhow::anyhow;
use dashmap::DashMap;
use futures::{stream, Stream, StreamExt};
use log::{debug, info, trace, warn, error};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;
use lazy_static::lazy_static;

use crate::characteristic::CharacteristicProperties;
use crate::descriptor::Descriptor;
use crate::service::Service;
use crate::session::{BackendSession, Filter, Session, SessionConfig};
use crate::winrt::bindings;
use crate::{characteristic, peripheral, DescriptorHandle, try_u64_from_mac48_str};
use crate::{
    fake, uuid::BluetoothUuid, winrt, AddressType, BackendPeripheralProperty, Error,
    MacAddressType, Result,
    GattError
};
use crate::{Address, MAC};
use crate::{BackendEvent, CacheMode, CharacteristicHandle, PeripheralHandle, ServiceHandle};

const SERVICE_DATA_16_BIT_UUID: u8 = 0x16;
const SERVICE_DATA_32_BIT_UUID: u8 = 0x20;
const SERVICE_DATA_128_BIT_UUID: u8 = 0x21;

#[derive(Clone, Debug)]
pub(crate) struct WinrtSession {
    inner: Arc<WinrtSessionInner>,
}

lazy_static! {
    static ref GATT_PROTOCOL_ERRORS: HashMap<u8, GattError> = {
        let mut map = HashMap::new();
        map.insert(GattProtocolError::InsufficientAuthentication().unwrap(),  GattError::InsufficientAuthentication);
        map.insert(GattProtocolError::InsufficientAuthorization().unwrap(),  GattError::InsufficientAuthorization);
        map.insert(GattProtocolError::InsufficientEncryption().unwrap(),  GattError::InsufficientEncryption);
        map.insert(GattProtocolError::ReadNotPermitted().unwrap(),  GattError::ReadNotPermitted);
        map.insert(GattProtocolError::WriteNotPermitted().unwrap(),  GattError::WriteNotPermitted);
        map.insert(GattProtocolError::RequestNotSupported().unwrap(),  GattError::Unsupported);

        map.insert(GattProtocolError::AttributeNotFound().unwrap(),  GattError::GeneralFailure("AttributeNotFound".to_string()));
        map.insert(GattProtocolError::AttributeNotLong().unwrap(),  GattError::GeneralFailure("AttributeNotLong".to_string()));
        map.insert(GattProtocolError::InsufficientEncryptionKeySize().unwrap(),  GattError::GeneralFailure("InsufficientEncryptionKeySize".to_string()));
        map.insert(GattProtocolError::InsufficientResources().unwrap(),  GattError::GeneralFailure("InsufficientResources".to_string()));
        map.insert(GattProtocolError::InvalidAttributeValueLength().unwrap(),  GattError::GeneralFailure("InvalidAttributeValueLength".to_string()));
        map.insert(GattProtocolError::InvalidHandle().unwrap(),  GattError::GeneralFailure("InvalidHandle".to_string()));
        map.insert(GattProtocolError::InvalidOffset().unwrap(),  GattError::GeneralFailure("InvalidOffset".to_string()));
        map.insert(GattProtocolError::InvalidPdu().unwrap(),  GattError::GeneralFailure("InvalidPdu".to_string()));
        map.insert(GattProtocolError::PrepareQueueFull().unwrap(),  GattError::GeneralFailure("PrepareQueueFull".to_string()));
        map.insert(GattProtocolError::UnlikelyError().unwrap(),  GattError::GeneralFailure("UnlikelyError".to_string()));
        map.insert(GattProtocolError::UnsupportedGroupType().unwrap(),  GattError::GeneralFailure("UnsupportedGroupType".to_string()));

        map
    };
}

#[derive(Debug)]
struct WinrtSessionInner {
    watcher: BluetoothLEAdvertisementWatcher,
    backend_bus: mpsc::UnboundedSender<BackendEvent>,
    peripherals_by_mac: DashMap<u64, WinrtPeripheral>,
    peripherals_by_handle: DashMap<PeripheralHandle, WinrtPeripheral>,
    next_handle: AtomicU32,
}

#[derive(Clone, Debug)]
struct WinrtPeripheral {
    address: u64,
    peripheral_handle: PeripheralHandle,
    inner: Arc<StdRwLock<WinrtPeripheralInner>>,
}

#[derive(Debug)]
struct WinrtPeripheralInner {
    // Only used when an application explicitly connects to the peripheral...
    ble_device: Option<BluetoothLEDevice>,
    //connection_status_handler: Option<EventRegistrationToken>,

    gatt_session: Option<GattSession>,
    gatt_session_status: GattSessionStatus,
    gatt_session_status_handler: Option<EventRegistrationToken>,

    // NB: we don't index services by uuid since it's possible for a device to
    // instantiate a service multiple times - each with the same uuid - but
    // with a different ATT handle.
    //
    // FIXME: switch this to a regular HashMap since we anyway have to lock
    // the peripheral inner to access
    gatt_services: DashMap<ServiceHandle, GattDeviceService>,

    // Similar to services, it's possible for a characteristic to be instantiated
    // multiple times per service by a device so we differentiate them by their
    // ATT handle.
    //
    // Note: the handles shouldn't be duplicated across services, so we don't
    // need to explicitly track the relationship between characteristics and
    // services here. (The frontend will be tracking the connection between
    // characteristics and services - we just need to be able to lookup the
    // GattCharacteristic for a given handle in the future)
    gatt_characteristics: DashMap<CharacteristicHandle, WinrtCharacteristic>,

    gatt_descriptors: DashMap<DescriptorHandle, GattDescriptor>,
}

impl WinrtPeripheralInner {

    /*
    fn remove_status_changed_handler(&mut self) {
        if let Some(device) = &self.ble_device {
            match device.RemoveConnectionStatusChanged(&self.connection_status_handler) {
                Ok(()) => {
                    trace!("Drop: removed connection status change handler");
                }
                Err(err) => {
                    log::error!("Failed to remove connection handler for device {:?}", err);
                }
            }
        }
    }
    */

    fn clear_gatt_state(&self) -> Result<()> {
        // Note: by de-indexing all GATT state handles then any requests that hit
        // get_service_from_handle() or get_characteristic_from_handle() will return
        // and InvalidStateReference error back to the caller...
        self.gatt_services.clear();
        self.gatt_characteristics.clear();
        self.gatt_descriptors.clear();

        Ok(())
    }

    fn close_gatt_session(&mut self) {
        if let Some(device) = &self.ble_device {
            if let Some(ref gatt_session) = self.gatt_session {
                match gatt_session.RemoveSessionStatusChanged(&self.gatt_session_status_handler) {
                Ok(()) => {
                        trace!("Drop: removed GattSession status change handler");
                    }
                    Err(err) => {
                        log::error!("Failed to remove GattSession status change handler: {:?}", err);
                    }
                }
                let _ = gatt_session.SetMaintainConnection(false);
                gatt_session.Close();
                self.gatt_session = None;
                self.gatt_session_status = GattSessionStatus::Closed;
            }
        }
    }

    fn close_device(&mut self) {
        if let Some(device) = &self.ble_device {
            device.Close();
            self.ble_device = None;
        }
    }
}

impl Drop for WinrtPeripheralInner {
    fn drop(&mut self) {
        //self.remove_status_changed_handler();
        self.close_gatt_session();
        self.close_device();
    }
}

#[derive(Clone, Debug)]
struct WinrtCharacteristic {
    gatt_characteristic: GattCharacteristic,
    notifications_handler: Option<EventRegistrationToken>,
}

impl TryFrom<&IBuffer> for Vec<u8> {
    type Error = Error;

    fn try_from(buf: &IBuffer) -> Result<Vec<u8>> {
        let reader = DataReader::FromBuffer(buf)?;
        let len = reader.UnconsumedBufferLength()? as usize;
        let mut data = vec![0u8; len];
        reader.ReadBytes(&mut data)?;

        Ok(data)
    }
}

impl WinrtSession {
    // For the sake of avoiding circular references then we may choose to
    // capture weak references to the WinrtSessionInner for various
    // callbacks and after upgrading back to an Arc we can re-wrap as a
    // bona fide WinrtSession again for the duration of the callback...
    fn wrap_inner(inner: Arc<WinrtSessionInner>) -> Self {
        Self { inner }
    }

    #[rustfmt::skip] // Visual indent really screws up the struct initialization
    pub fn new(_config: &SessionConfig<'_>, backend_bus: mpsc::UnboundedSender<BackendEvent>)
                     -> Result<Self> {
        let filter = BluetoothLEAdvertisementFilter::new().map_err(|e| Error::Other(anyhow!(e)))?;
        let watcher =
            BluetoothLEAdvertisementWatcher::Create(&filter).map_err(|e| Error::Other(anyhow!(e)))?;

        Ok(WinrtSession {
            inner: Arc::new(WinrtSessionInner {
                watcher,
                backend_bus,
                peripherals_by_mac: DashMap::new(),
                peripherals_by_handle: DashMap::new(),
                next_handle: AtomicU32::new(1)
            })
        })
    }

    fn peripheral_from_mac(&self, address: u64) -> PeripheralHandle {
        match self.inner.peripherals_by_mac.get(&address) {
            None => {
                let backend_bus = self.inner.backend_bus.clone();

                let peripheral_id = self.inner.next_handle.fetch_add(1, Ordering::SeqCst);
                let peripheral_handle = PeripheralHandle(peripheral_id);

                let winrt_peripheral = WinrtPeripheral {
                    address: address,
                    peripheral_handle,
                    inner: Arc::new(StdRwLock::new(WinrtPeripheralInner {
                        ble_device: None,
                        //connection_status_handler: None,
                        gatt_session: None,
                        gatt_session_status: GattSessionStatus::Closed,
                        gatt_session_status_handler: None,
                        gatt_services: DashMap::new(),
                        gatt_characteristics: DashMap::new(),
                        gatt_descriptors: DashMap::new(),
                    })),
                };
                self.inner
                    .peripherals_by_mac
                    .insert(address, winrt_peripheral.clone());
                self.inner
                    .peripherals_by_handle
                    .insert(peripheral_handle, winrt_peripheral);

                let _ = backend_bus.send(BackendEvent::PeripheralFound { peripheral_handle });

                let _ = backend_bus.send(BackendEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: BackendPeripheralProperty::Address(Address::MAC(MAC(address))),
                });

                peripheral_handle
            }
            Some(winrt_peripheral) => winrt_peripheral.peripheral_handle,
        }
    }

    fn on_advertisement_received(&self, args: &BluetoothLEAdvertisementReceivedEventArgs) {
        let advertisement = args.Advertisement().unwrap();

        let backend_bus = self.inner.backend_bus.clone();

        let address = match args.BluetoothAddress() {
            Ok(addr) => addr,
            Err(_e) => {
                return;
            }
        };

        let peripheral_handle = self.peripheral_from_mac(address);

        if let Ok(address_type) = args.BluetoothAddressType() {
            let address_type = match address_type {
                BluetoothAddressType::Public => Some(AddressType::PublicMAC),
                BluetoothAddressType::Random => Some(AddressType::RandomMAC),
                _ => None,
            };
            if let Some(address_type) = address_type {
                let _ = backend_bus.send(BackendEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: BackendPeripheralProperty::AddressType(address_type),
                });
            }
        }
        if let Ok(name) = advertisement.LocalName() {
            if !name.is_empty() {
                let _ = backend_bus.send(BackendEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: BackendPeripheralProperty::Name(name.to_string()),
                });
            }
        }

        if let Ok(manufacturer_data) = advertisement.ManufacturerData() {
            let mut data_map = HashMap::new();
            for data in manufacturer_data {
                if let Ok(manfacturer_id) = data.CompanyId() {
                    if let Ok(buf) = data.Data() {
                        if let Ok(vec) = Vec::<u8>::try_from(&buf) {
                            data_map.insert(manfacturer_id, vec);
                        }
                    }
                }
            }
            if !data_map.is_empty() {
                let _ = backend_bus.send(BackendEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: BackendPeripheralProperty::ManufacturerData(data_map),
                });
            }
        }

        // We have to parse service data manually from the data sections...
        if let Ok(data_sections) = advertisement.DataSections() {
            let mut data_map = HashMap::new();
            for section in &data_sections {
                if let Ok(buf) = &section.Data() {
                    if let Ok(data) = Vec::<u8>::try_from(buf) {
                        match section.DataType() {
                            Ok(SERVICE_DATA_16_BIT_UUID) => {
                                if data.len() >= 2 {
                                    let (uuid, data) = data.split_at(2);
                                    let uuid: [u8; 2] = uuid.try_into().unwrap();
                                    let uuid = Uuid::from_u16(u16::from_le_bytes(uuid));
                                    data_map.insert(uuid, data.to_owned());
                                }
                            }
                            Ok(SERVICE_DATA_32_BIT_UUID) => {
                                if data.len() >= 4 {
                                    let (uuid, data) = data.split_at(4);
                                    let uuid: [u8; 4] = uuid.try_into().unwrap();
                                    let uuid = Uuid::from_u32(u32::from_le_bytes(uuid));
                                    data_map.insert(uuid, data.to_owned());
                                }
                            }
                            Ok(SERVICE_DATA_128_BIT_UUID) => {
                                if data.len() >= 16 {
                                    let (uuid, data) = data.split_at(16);
                                    let uuid = Uuid::from_slice(uuid).unwrap();
                                    data_map.insert(uuid, data.to_owned());
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            if !data_map.is_empty() {
                let _ = backend_bus.send(BackendEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: BackendPeripheralProperty::ServiceData(data_map),
                });
            }
        }

        if let Ok(services) = advertisement.ServiceUuids() {
            let uuids: Vec<Uuid> = services.into_iter()
                                           .filter_map(|guid| {
                                               if let Ok(uuid) = Uuid::try_from_guid(&guid) {
                                                   Some(uuid)
                                               } else {
                                                   None
                                               }
                                           })
                                           .collect();

            if !uuids.is_empty() {
                let _ = backend_bus.send(BackendEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: BackendPeripheralProperty::ServiceIds(uuids),
                });
            }
        }

        if let Ok(tx_reference) = args.TransmitPowerLevelInDBm() {
            // IReference is (ironically) a crazy foot gun in Rust since it very easily
            // panics if you look at it wrong. Calling GetInt16(), IsNumericScalar() or Type()
            // all panic here without returning a Result as documented.
            // Value() is apparently the _right_ way to extract something from an IReference<T>...
            if let Ok(tx) = tx_reference.Value() {
                let _ = backend_bus.send(BackendEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: BackendPeripheralProperty::TxPower(tx),
                });
            }
        }

        if let Ok(rssi) = args.RawSignalStrengthInDBm() {
            let _ = backend_bus.send(BackendEvent::PeripheralPropertySet {
                peripheral_handle,
                property: BackendPeripheralProperty::Rssi(rssi),
            });
        }
    }

    fn winrt_peripheral_from_handle(&self, peripheral_handle: PeripheralHandle)
                                    -> Result<WinrtPeripheral> {
        match self.inner.peripherals_by_handle.get(&peripheral_handle) {
            Some(p) => Ok(p.clone()),
            None => {
                log::error!("Spurious request with unknown peripheral handle {:?}",
                            peripheral_handle);
                Err(Error::InvalidStateReference)
            }
        }
    }

    fn check_gatt_communication_status(&self,
                                       status: std::result::Result<GattCommunicationStatus,
                                                           windows::Error>,
                                       protocol_error: std::result::Result<IReference<u8>,
                                                           windows::Error>)
                                       -> Result<()> {
        let status = status?;
        match status {
            GattCommunicationStatus::AccessDenied => Err(Error::PeripheralAccessDenied),
            GattCommunicationStatus::ProtocolError => {
                let protocol_error = protocol_error?.Value()?;
                let gatt_error = GATT_PROTOCOL_ERRORS.get(&protocol_error)
                    .map_or(GattError::GeneralFailure(format!("{protocol_error}")), |v| v.clone());
                Err(Error::PeripheralGattProtocolError(gatt_error.clone()))
            }
            GattCommunicationStatus::Unreachable => Err(Error::PeripheralUnreachable),
            GattCommunicationStatus::Success => Ok(()),
            _ => {
                warn!("Spurious GATT communication status: {:?}", status);
                Err(Error::Other(anyhow!("Spurious GATT communication status")))
            }
        }
    }

    // NB: The WinRT docs explicitly clarify that passing `Cached` to `GetGattServicesAsync`
    // as a cache mode "means try looking in the cache first and, if not there, then retrieve
    // from the device"
    async fn get_gatt_services(&self, device: &BluetoothLEDevice, cache_mode: BluetoothCacheMode)
                               -> Result<GattDeviceServicesResult> {
        let gatt_service_result = device.GetGattServicesWithCacheModeAsync(cache_mode)?
                                        .await?;
        self.check_gatt_communication_status(gatt_service_result.Status(),
                                             gatt_service_result.ProtocolError())?;
        Ok(gatt_service_result)
    }

    fn notify_disconnect(&self, peripheral_handle: PeripheralHandle) {
        trace!("notify_disconnect");

        self.peripheral_drop_gatt_state(peripheral_handle);
        let _ = self.inner
                    .backend_bus
                    .send(BackendEvent::PeripheralDisconnected { peripheral_handle, error: None });
    }

    /*
    fn on_connection_status_change(&self, status: BluetoothConnectionStatus,
                                   peripheral_handle: PeripheralHandle,
                                   peripheral_address_debug: &str)
                                   -> Result<()> {
        match status {
            BluetoothConnectionStatus::Connected => {
                trace!("Peripheral connected: handle={:?}/{}",
                       peripheral_handle,
                       peripheral_address_debug);
                let _ = self.inner
                            .backend_bus
                            .send(BackendEvent::PeripheralConnected { peripheral_handle });
            }
            BluetoothConnectionStatus::Disconnected => {
                trace!("Peripheral disconnected: handle={:?}/{}",
                       peripheral_handle,
                       peripheral_address_debug);

                self.notify_disconnect(peripheral_handle);
            }
            status => {
                log::error!("Spurious bluetooth connection status: {:?}: handle={:?}/{}",
                            status,
                            peripheral_handle,
                            peripheral_address_debug);
            }
        }

        Ok(())
    }*/

    fn on_session_status_change(&self, status: GattSessionStatus,
                                peripheral_handle: PeripheralHandle) -> Result<()> {
        let winrt_peripheral = self.winrt_peripheral_from_handle(peripheral_handle)?;
        let prev_status = winrt_peripheral.inner.write().unwrap().gatt_session_status;

        trace!("GattSessionStatusChanged: {:?} (prev status = {:?}", status,  prev_status);
        // Can get redundant notifications
        if status == prev_status {
            debug!("Ignoring GattSessionStatusChanged (unchanged)");
            return Ok(());
        }

        let peripheral_address = winrt_peripheral.address;
        let peripheral_address_debug = MAC(peripheral_address).to_string();

        match status {
            GattSessionStatus::Active => {
                trace!("Peripheral ({:?}/{}) Gatt Session is Active", peripheral_handle, peripheral_address_debug);
                let _ = self.inner
                            .backend_bus
                            .send(BackendEvent::PeripheralConnected { peripheral_handle });
            },
            GattSessionStatus::Closed => {
                trace!("Peripheral ({:?}/{}) Gatt Session is Closed", peripheral_handle, peripheral_address_debug);

                self.notify_disconnect(peripheral_handle);
            },
            status => {
                log::error!("Spurious bluetooth connection status: {:?}: handle={:?}/{}",
                            status,
                            peripheral_handle,
                            peripheral_address_debug);
            }
        }

        let mut guard = winrt_peripheral.inner.write().unwrap();
        guard.gatt_session_status = status;

        Ok(())
    }

    /*
    async fn attach_device_with_connection_handler(&self, winrt_peripheral: &WinrtPeripheral)
                                                   -> Result<BluetoothLEDevice> {
        let device = BluetoothLEDevice::FromBluetoothAddressAsync(winrt_peripheral.address)
            .map_err(|_| Error::PeripheralUnreachable)?
            .await
            .map_err(|_| Error::PeripheralUnreachable)?;

        // XXX: I thought non-lexical lifetimes were suppose to fix this kind of issue... :/
        //
        // This scope has been added because otherwise the compiler complains that
        // the connection_status_handler (which is not Send safe) might be used after the
        // final await... the await that is at the end of the function where it should
        // surely be possible for the compiler to recognise that this doesn't need to live
        // that long?
        //
        // Also the handler is moved into device.ConnectionStatusChanged() so I also don't
        // understand why the lifetime isn't seen as ending there?
        //
        // So apparently this isn't technically an 'NLL' issue, the compiler is too
        // grabby with capturing for generators:
        // https://github.com/rust-lang/rust/issues/69663
        //
        {
            // XXX: be careful not to introduce a ref cycle here...
            // (notably we don't pass a winrt session reference to the status handler)
            //let backend_bus = self.inner.backend_bus.clone();
            let peripheral_handle = winrt_peripheral.peripheral_handle;
            let peripheral_address = winrt_peripheral.address;
            let peripheral_address_debug = MAC(peripheral_address).to_string();

            // Give the handler a weak reference to the session to avoid a ref cycle...
            let weak_session_inner = Arc::downgrade(&self.inner);

            let connection_status_handler =
                TypedEventHandler::new(move |sender: &Option<BluetoothLEDevice>, _| {
                    if let Some(sender) = sender {
                        let winrt_session_inner = weak_session_inner.upgrade();
                        if let Some(winrt_session_inner) = winrt_session_inner {
                            let winrt_session = WinrtSession::wrap_inner(winrt_session_inner);

                            match sender.ConnectionStatus() {
                                Ok(status) => {
                                    if let Err(err) = winrt_session.on_connection_status_change(
                                        status, peripheral_handle, &peripheral_address_debug
                                    ) {
                                        log::error!("Error while handling connection status change callback: {:?}", err);
                                    }
                                }
                                Err(err) => {
                                    log::error!("Failure while querying bluetooth connection status: {:?}: handle={:?}/{}",
                                        err, peripheral_handle, &peripheral_address_debug);
                                }
                            }
                        }
                    }

                    Ok(())
                });

            let mut winrt_peripheral_guard = winrt_peripheral.inner.write().unwrap();
            winrt_peripheral_guard.connection_status_handler =
                Some(device.ConnectionStatusChanged(connection_status_handler)
                           .map_err(|_| {
                               Error::Other(anyhow!("Could not add connection status handler"))
                           })?);
            winrt_peripheral_guard.ble_device = Some(device.clone());
        }

        Ok(device)
    }*/

    // Make sure we've got a BluetoothLEDevice for our peripheral (we don't look up
    // a device for every peripheral discovered via advertising packets - we only
    // need a device once an application is interested in connecting to the
    // peripheral)
    //
    // Returns a reference to the device
    /*
    async fn ensure_device_with_connection_status_handler(&self,
                                                          winrt_peripheral: &WinrtPeripheral)
                                                          -> Result<BluetoothLEDevice> {
        // First speculatively take a read lock and see if we already have an
        // associated device with a connection handler...
        {
            let guard = winrt_peripheral.inner.read().unwrap();
            if let Some(ref ble_device) = guard.ble_device {
                return Ok(ble_device.clone());
            }
        }

        // If not, look up a device, register a connection handler and associate both
        // with the winrt_peripheral...
        self.attach_device_with_connection_handler(winrt_peripheral)
            .await
    }*/

    fn get_ble_device(&self, winrt_peripheral: &WinrtPeripheral) -> Result<BluetoothLEDevice>
    {
        debug!("get_ble_device");

        let state_guard = winrt_peripheral.inner.read().unwrap();
        match state_guard.ble_device {
            Some(ref ble_device) => {
                Ok(ble_device.clone())
            },
            None => {
                // Would likely represent an internal bug, since we shouldn't be dealing with
                // requests that need a BleDevice if we are disconnected
                error!("Failed to lookup expected BluetoothLEDevice for peripheral!");
                Err(Error::InvalidStateReference)
            }
        }
    }

    fn gatt_service_from_handle(&self, winrt_peripheral: &WinrtPeripheral,
                                service_handle: ServiceHandle)
                                -> Result<GattDeviceService> {
        // Only need a 'read' lock since the DashMap has interior mutability.
        let guard = winrt_peripheral.inner.read().unwrap();

        let gatt_service = guard.gatt_services.get(&service_handle);
        match gatt_service {
            Some(gatt_service) => Ok(gatt_service.clone()),
            None => {
                warn!("Request made with invalid service handle");
                Err(Error::InvalidStateReference)
            }
        }
    }

    fn gatt_characteristic_from_handle(&self, winrt_peripheral: &WinrtPeripheral,
                                       characteristic_handle: CharacteristicHandle)
                                       -> Result<GattCharacteristic> {
        // Only need a 'read' lock since the DashMap has interior mutability.
        let guard = winrt_peripheral.inner.read().unwrap();

        let winrt_characteristic = guard.gatt_characteristics.get(&characteristic_handle);
        match winrt_characteristic {
            Some(winrt_characteristic) => Ok(winrt_characteristic.gatt_characteristic.clone()),
            None => {
                warn!("Request made with invalid characteristic handle");
                Err(Error::InvalidStateReference)
            }
        }
    }

    fn gatt_descriptor_from_handle(&self, winrt_peripheral: &WinrtPeripheral,
                                   descriptor_handle: DescriptorHandle)
                                   -> Result<GattDescriptor> {
        // Only need a 'read' lock since the DashMap has interior mutability.
        let guard = winrt_peripheral.inner.read().unwrap();

        let gatt_descriptor = guard.gatt_descriptors.get(&descriptor_handle);
        match gatt_descriptor {
            Some(gatt_descriptor) => Ok(gatt_descriptor.clone()),
            None => {
                warn!("Request made with invalid descriptor handle");
                Err(Error::InvalidStateReference)
            }
        }
    }

    fn index_services_result(&self, winrt_peripheral: &WinrtPeripheral,
                             services_result: &GattDeviceServicesResult)
                             -> Result<()> {
        // Only need a 'read' lock since the DashMap has interior mutability.
        let guard = winrt_peripheral.inner.read().unwrap();

        let services = services_result.Services()?;

        for service in services {
            let attribute = service.AttributeHandle()?;
            let service_handle = ServiceHandle(attribute as u32);
            guard.gatt_services.insert(service_handle, service);
        }

        Ok(())
    }

    fn index_characteristics_result(&self, winrt_peripheral: &WinrtPeripheral,
                                    characteristics_result: &GattCharacteristicsResult)
                                    -> Result<()> {
        // Only need a 'read' lock since the DashMap has interior mutability.
        let guard = winrt_peripheral.inner.read().unwrap();

        let characteristics = characteristics_result.Characteristics()?;

        for characteristic in characteristics {
            let attribute = characteristic.AttributeHandle()?;
            let characteristic_handle = CharacteristicHandle(attribute as u32);
            let winrt_characteristic = WinrtCharacteristic { gatt_characteristic: characteristic,
                                                             notifications_handler: None };
            guard.gatt_characteristics
                 .insert(characteristic_handle, winrt_characteristic);
        }

        Ok(())
    }

    fn index_descriptors_result(&self, winrt_peripheral: &WinrtPeripheral,
                                descriptors_result: &GattDescriptorsResult)
                                -> Result<()> {
        // Only need a 'read' lock since the DashMap has interior mutability.
        let guard = winrt_peripheral.inner.read().unwrap();

        let descriptors = descriptors_result.Descriptors()?;

        for descriptor in descriptors {
            let attribute = descriptor.AttributeHandle()?;
            let descriptor_handle = DescriptorHandle(attribute as u32);
            guard.gatt_descriptors
                 .insert(descriptor_handle, descriptor);
        }

        Ok(())
    }
}

#[async_trait]
impl BackendSession for WinrtSession {
    async fn start_scanning(&self, filter: &Filter) -> Result<()> {
        trace!("winrt: start scanning");

        self.inner
            .watcher
            .SetScanningMode(BluetoothLEScanningMode::Active)?;

        // XXX: we're careful here to avoid giving a strong (circular) reference to
        // the watcher callback, otherwise it won't be possible to ever drop a
        // WinrtSession after starting scanning.
        let weak_inner = Arc::downgrade(&self.inner);

        let handler: TypedEventHandler<
            BluetoothLEAdvertisementWatcher,
            BluetoothLEAdvertisementReceivedEventArgs,
        > = TypedEventHandler::new(
            move |_sender, args: &Option<BluetoothLEAdvertisementReceivedEventArgs>| {
                if let Some(args) = args {
                    if let Some(inner) = weak_inner.upgrade() {
                        let session = WinrtSession::wrap_inner(inner);
                        session.on_advertisement_received(args);
                    }
                }
                Ok(())
            },
        );

        self.inner.watcher.Received(&handler)?;
        self.inner.watcher.Start()?;
        Ok(())
    }

    async fn stop_scanning(&self) -> Result<()> {
        trace!("winrt: stop scanning");
        self.inner.watcher.Stop()?;
        Ok(())
    }

    fn declare_peripheral(&self, address: Address, name: String) -> Result<PeripheralHandle> {
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

        let peripheral_handle = self.peripheral_from_mac(mac);
        let _ = self.inner
            .backend_bus
            .send(BackendEvent::PeripheralPropertySet {
                peripheral_handle,
                property: BackendPeripheralProperty::Name(name.to_string()),
            });

        Ok(peripheral_handle)
    }

    async fn peripheral_connect(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        let winrt_peripheral = self.winrt_peripheral_from_handle(peripheral_handle)?;

        trace!("peripheral_connect");

        // Make sure we've got a BluetoothLEDevice for our peripheral (we don't look up
        // a device for every peripheral discovered via advertising packets - we only
        // need a device once an application is interested in connecting to the
        // peripheral)
        let device = BluetoothLEDevice::FromBluetoothAddressAsync(winrt_peripheral.address)
            .map_err(|_| Error::PeripheralUnreachable)?
            .await
            .map_err(|_| Error::PeripheralUnreachable)?;

        //let device = self.ensure_device_with_connection_status_handler(&winrt_peripheral).await?;
        //let device = self.get_ble_device(&winrt_peripheral)?;

        // Considering that we might not have previously enabled scanning and
        // the peripheral might have been declared by the application based
        // on a saved Address so this might be the first point where we learn
        // the device's name...
        let name = device.Name()?.to_string();
        let backend_bus = self.inner.backend_bus.clone();
        let _ = backend_bus.send(BackendEvent::PeripheralPropertySet {
            peripheral_handle,
            property: BackendPeripheralProperty::Name(name.to_string()),
        });

        let id = device.DeviceId()?;
        let id = BluetoothDeviceId::FromId(id)?;
        trace!("Calling GattSession::FromDeviceIdAsync()...");
        let gatt_session = GattSession::FromDeviceIdAsync(id)?.await?;

        // Start listening for GattSession state changes to know when we have connected
        // to the Gatt Server
        {
            // XXX: be careful not to introduce a ref cycle here...
            // (notably we don't pass a winrt session reference to the status handler)
            //let backend_bus = self.inner.backend_bus.clone();
            let peripheral_address = winrt_peripheral.address;
            let peripheral_address_debug = MAC(peripheral_address).to_string();

            // Give the handler a weak reference to the session to avoid a ref cycle...
            let weak_session_inner = Arc::downgrade(&self.inner);

            let session_status_handler =
                TypedEventHandler::new(move |gatt_session: &Option<GattSession>, args: &Option<GattSessionStatusChangedEventArgs> | {
                    if let Some(gatt_session) = gatt_session {
                        if let Some(args) = args {
                            let winrt_session_inner = weak_session_inner.upgrade();
                            if let Some(winrt_session_inner) = winrt_session_inner {
                                let winrt_session = WinrtSession::wrap_inner(winrt_session_inner);

                                match args.Status() {
                                    Ok(status) => {
                                        if let Err(err) = winrt_session.on_session_status_change(
                                            status, peripheral_handle
                                        ) {
                                            log::error!("Error while handling connection status change callback: {:?}", err);
                                        }
                                    }
                                    Err(err) => {
                                        log::error!("Failure while querying bluetooth connection status: {:?}: handle={:?}/{}",
                                            err, peripheral_handle, &peripheral_address_debug);
                                    }
                                }
                            }
                        }
                    }

                    Ok(())
                });

            {
                let mut winrt_peripheral_guard = winrt_peripheral.inner.write().unwrap();
                winrt_peripheral_guard.gatt_session_status_handler =
                    Some(gatt_session.SessionStatusChanged(session_status_handler)
                            .map_err(|_| {
                                Error::Other(anyhow!("Could not add gatt session status handler"))
                            })?);
                winrt_peripheral_guard.ble_device = Some(device.clone());

                winrt_peripheral_guard.gatt_session = Some(gatt_session.clone());
                winrt_peripheral_guard.gatt_session_status = GattSessionStatus::Closed;
            }

            let initial_status = gatt_session.SessionStatus()?;
            trace!("Created GattSession: Initial State = {:?}", initial_status);
            self.on_session_status_change(initial_status, peripheral_handle);
        }

        gatt_session.SetMaintainConnection(true)?;


        //let io_bus_tx = self.ensure_peripheral_watcher_and_connect_task(peripheral_handle).await?;

        // XXX: We used to initiate a connection by requesting gatt services
        // (which would implicitly need to connect internally) but we now
        // rely on `gatt_session.SetMaintainConnection(true)` which doesn't
        // block us indefinitely waiting for a connection.
        //
        // Windows doesn't actually expose a connection-oriented API so to
        // 'connect' we just have to perform an action that we know requires
        // a connection. By default winrt queries for gatt services and
        // characteristics etc happen via a system cache which can avoid needing
        // to actually connect to the device. By explicitly bypassing the cache
        // we know the system will have to really connect to the device...
        //
        // XXX: this is actually liable to wait until we have really connected
        // to the device, incase this is our first connection and we haven't
        // yet been able to call `session.SetMaintainConnection(true)`. This
        // backend API is only support to _initiate_ a connection request and
        // isn't expected to wait indefinitely for the connection to happen.
        //let services_result = self.get_gatt_services(&device, BluetoothCacheMode::Uncached)
        //    .await?;

        // For consistency with the Android autoconnect = true behaviour and
        // the auto connect behaviour with Core Bluetooth we also want to
        // ensure that if we disconnect from the peripheral then we should
        // try to automatically reconnect...
        //
        // Note: since we have a chicken/egg requirement to make a connection
        // before we can configure the autoconnect = true then this is most
        // similar to first connecting with autoconnect = false on Android
        // and then automatically connecting with autoconnect = true after
        // the first disconnect.
        //
        // Emulating Core Bluetooth behaviour for potentially better consistency
        // would require us to run get_gatt_services() in a loop here until
        // we succeed.

        /*
        let services = services_result.Services()?;
        if let Some(first_service) = services.into_iter().next() {
            let session = first_service.Session()?;
        } else {
            // On Windows afik we need access to a Service to get a GattSession
            // so if there was somehow a device that had no services, (and
            // get_gatt_services() didn't already treat that as an error) then we
            // wouldn't be able to configure the session to autoconnect.
            error!("Can't ask OS to maintain bluetooth connection to device with no services.");
        }
        */

        Ok(())
    }

    async fn peripheral_disconnect(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        self.peripheral_drop_gatt_state(peripheral_handle);

        // Since we've closed the session then we won't get any callback for the peripheral
        // disconnecting, so we just optimistically inform the frontend that the
        // peripheral is disconnected
        let _ = self.inner
                    .backend_bus
                    .send(BackendEvent::PeripheralDisconnected { peripheral_handle, error: None });
        Ok(())
    }

    fn peripheral_drop_gatt_state(&self, peripheral_handle: PeripheralHandle) {
        if let Ok(winrt_peripheral) = self.winrt_peripheral_from_handle(peripheral_handle) {
            let mut guard = winrt_peripheral.inner.write().unwrap();
            guard.close_gatt_session();
            guard.close_device();
            guard.clear_gatt_state();
        }
    }

    //fn peripheral_is_connected(&self, peripheral_handle: PeripheralHandle) -> Result<bool> {
    //    todo!();
    //}

    async fn peripheral_read_rssi(&self, peripheral_handle: PeripheralHandle) -> Result<i16> {
        // Currently windows only exposes RSSI via advertising packets
        Err(Error::Unsupported)
    }

    async fn peripheral_discover_gatt_services(&self,
            peripheral_handle: PeripheralHandle,
            of_interest_hint: Option<Vec<Uuid>>)
            -> Result<()>
    {
        debug!("winrt: peripheral_discover_gatt_services");
        let winrt_peripheral = self.winrt_peripheral_from_handle(peripheral_handle)?;
        //let device = self.ensure_device_with_connection_status_handler(&winrt_peripheral).await?;
        let device = self.get_ble_device(&winrt_peripheral)?;

        // Note in this case `Cached` "means try looking in the cache first and, if not there,
        // then retrieve from the device" - so it's valid to use for 'discovery' here in case
        // the services have not yet been queried.
        let services_result = self.get_gatt_services(&device, BluetoothCacheMode::Cached).await?;

        self.index_services_result(&winrt_peripheral, &services_result)?;

        let services = services_result.Services()?;
        let backend_bus = self.inner.backend_bus.clone();

        for service in services {
            debug!("winrt: notify gatt_services");
            let attribute = service.AttributeHandle()?;
            let service_handle = ServiceHandle(attribute as u32);
            let uuid: Uuid = Uuid::try_from_guid(&service.Uuid()?)?;
            let _ =
                backend_bus.send(BackendEvent::GattService { peripheral_handle,
                                                                      service_handle,
                                                                      uuid });
        }

        debug!("winrt: notify gatt_services complete");
        // In this case we queried all the services for the device so we can
        // tell the frontend that it now knows about all of the peripheral's
        // primary services...
        let _ = backend_bus
            .send(BackendEvent::GattServicesComplete { peripheral_handle, error: None });

        Ok(())
    }

    async fn gatt_service_discover_includes(&self, peripheral_handle: PeripheralHandle,
                                            service_handle: ServiceHandle)
                                            -> Result<()> {
        let winrt_peripheral = self.winrt_peripheral_from_handle(peripheral_handle)?;
        let gatt_service = self.gatt_service_from_handle(&winrt_peripheral, service_handle)?;

        let services_result = gatt_service.GetIncludedServicesAsync()?.await?;
        self.check_gatt_communication_status(services_result.Status(),
                                             services_result.ProtocolError())?;

        self.index_services_result(&winrt_peripheral, &services_result)?;

        let services = services_result.Services()?;
        let peripheral_handle = winrt_peripheral.peripheral_handle;
        let backend_bus = self.inner.backend_bus.clone();

        for service in services {
            let attribute = service.AttributeHandle()?;
            let included_service_handle = ServiceHandle(attribute as u32);
            let uuid: Uuid = Uuid::try_from_guid(&service.Uuid()?)?;
            let _ = backend_bus.send(BackendEvent::GattIncludedService {
                peripheral_handle,
                parent_service_handle: service_handle,
                included_service_handle,
                uuid,
            });
        }

        // In this case we queried all the included services for the service so we can
        // tell the frontend that it now knows about all of the service's
        // included services...
        let _ = backend_bus
            .send(BackendEvent::GattIncludedServicesComplete {
                peripheral_handle,
                service_handle,
                error: None
            });

        Ok(())
    }

    async fn gatt_service_discover_characteristics(&self, peripheral_handle: PeripheralHandle,
                                                   service_handle: ServiceHandle)
                                                   -> Result<()> {
        let winrt_peripheral = self.winrt_peripheral_from_handle(peripheral_handle)?;
        let gatt_service = self.gatt_service_from_handle(&winrt_peripheral, service_handle)?;

        let characteristics_result = gatt_service.GetCharacteristicsAsync()?.await?;
        self.check_gatt_communication_status(characteristics_result.Status(),
                                             characteristics_result.ProtocolError())?;

        self.index_characteristics_result(&winrt_peripheral, &characteristics_result)?;

        let characteristics = characteristics_result.Characteristics()?;
        let backend_bus = self.inner.backend_bus.clone();

        for characteristic in characteristics {
            let attribute = characteristic.AttributeHandle()?;
            let characteristic_handle = CharacteristicHandle(attribute as u32);
            let uuid: Uuid = Uuid::try_from_guid(&characteristic.Uuid()?)?;
            let properties = CharacteristicProperties::from(characteristic.CharacteristicProperties()?);
            let _ = backend_bus.send(BackendEvent::GattCharacteristic {
                peripheral_handle,
                service_handle,
                characteristic_handle,
                uuid,
                properties
            });
        }

        // In this case we queried all the characteristics for the service so we can
        // tell the frontend that it now knows about all of the service's
        // characteristics...
        let _ = backend_bus
            .send(BackendEvent::GattCharacteristicsComplete {
                peripheral_handle,
                service_handle,
                error: None
            });
        Ok(())
    }

    async fn gatt_characteristic_read(&self, peripheral_handle: PeripheralHandle,
                                      service_handle: ServiceHandle,
                                      characteristic_handle: CharacteristicHandle,
                                      cache_mode: CacheMode)
                                      -> Result<Vec<u8>> {
        let winrt_peripheral = self.winrt_peripheral_from_handle(peripheral_handle)?;
        let gatt_characteristic =
            self.gatt_characteristic_from_handle(&winrt_peripheral, characteristic_handle)?;
        let cache_mode = match cache_mode {
            CacheMode::Cached => BluetoothCacheMode::Cached,
            CacheMode::Uncached => BluetoothCacheMode::Uncached,
        };
        let result = gatt_characteristic.ReadValueWithCacheModeAsync(cache_mode)?
                                        .await?;
        self.check_gatt_communication_status(result.Status(), result.ProtocolError())?;

        let buf = Vec::<u8>::try_from(&result.Value()?)?;
        Ok(buf)
    }

    async fn gatt_characteristic_write(&self, peripheral_handle: PeripheralHandle,
                                       service_handle: ServiceHandle,
                                       characteristic_handle: CharacteristicHandle,
                                       write_type: characteristic::WriteType, data: &[u8])
                                       -> Result<()> {
        let winrt_peripheral = self.winrt_peripheral_from_handle(peripheral_handle)?;
        let gatt_characteristic =
            self.gatt_characteristic_from_handle(&winrt_peripheral, characteristic_handle)?;

        let write_option = match write_type {
            characteristic::WriteType::WithResponse => GattWriteOption::WriteWithResponse,
            characteristic::WriteType::WithoutResponse => GattWriteOption::WriteWithoutResponse,
        };

        let writer = DataWriter::new()?;
        writer.WriteBytes(data)?;
        let fut = gatt_characteristic.WriteValueWithResultAndOptionAsync(writer.DetachBuffer()?,
                                                                         write_option)?;

        // XXX: Rust _really_ needs to improve how it captures for generators, it's like before non-lexical lifetimes
        // where you have to do awkward rearrangements purely for the sake of spelling out to the compiler that what
        // you're doing is fine.
        //
        // Here the compiler is having a fit about the IBuffer from `writer.DetachBuffer()?` not being Send, even
        // though it's moved straight into `WriteValueWithResultAndOptionAsync` and clearly doesn't need to be
        // captured by the future for this function to survive beyond this await.
        //
        // We have to split this into a separate statement avoid capturing the IBuffer
        let result = fut.await?;

        self.check_gatt_communication_status(result.Status(), result.ProtocolError())?;

        Ok(())
    }

    async fn gatt_characteristic_subscribe(&self, peripheral_handle: PeripheralHandle,
                                           service_handle: ServiceHandle,
                                           characteristic_handle: CharacteristicHandle)
                                           -> Result<()> {
        let winrt_peripheral = self.winrt_peripheral_from_handle(peripheral_handle)?;
        let gatt_characteristic =
            self.gatt_characteristic_from_handle(&winrt_peripheral, characteristic_handle)?;

        let props = gatt_characteristic.CharacteristicProperties()?;
        if props & GattCharacteristicProperties::Notify != GattCharacteristicProperties::Notify {
            return Err(Error::Other(anyhow!(
                "Attribute doesn't support notifications"
            )));
        }

        {
            let guard = winrt_peripheral.inner.read().unwrap();

            let winrt_characteristic = guard.gatt_characteristics.get_mut(&characteristic_handle);
            let mut winrt_characteristic = match winrt_characteristic {
                Some(winrt_characteristic) => winrt_characteristic,
                None => {
                    warn!("Spurious request with unknown characteristic handle");
                    return Err(Error::Other(anyhow!("Unknown characteristic handle")));
                }
            };

            // First make sure to remove any old handler...
            if winrt_characteristic.notifications_handler.is_some() {
                gatt_characteristic.RemoveValueChanged(winrt_characteristic.notifications_handler)?;
                winrt_characteristic.notifications_handler = None;
            }

            // XXX: be careful not to introduce a ref cycle here...
            // (notably we don't pass a winrt session reference to the status handler)
            let backend_bus = self.inner.backend_bus.clone();

            let value_changed_handler = TypedEventHandler::new(
                move |_: &Option<GattCharacteristic>, args: &Option<GattValueChangedEventArgs>| {
                    if let Some(args) = args {
                        match Vec::<u8>::try_from(&args.CharacteristicValue()?) {
                            Ok(buf) => {
                                let _ = backend_bus.send(
                                    BackendEvent::GattCharacteristicNotify {
                                        peripheral_handle,
                                        service_handle,
                                        characteristic_handle,
                                        value: buf,
                                    },
                                );
                            }
                            Err(err) => {
                                log::error!("Failed to convert notified characteristic value to Vec<u8>: {:?}", err);
                            }
                        }
                    }
                    Ok(())
                },
            );
            winrt_characteristic.notifications_handler =
                Some(gatt_characteristic.ValueChanged(&value_changed_handler)?);
        }

        let config = GattClientCharacteristicConfigurationDescriptorValue::Notify;
        let fut = gatt_characteristic
            .WriteClientCharacteristicConfigurationDescriptorWithResultAsync(config)?;

        match fut.await {
            Ok(result) => {
                self.check_gatt_communication_status(result.Status(), result.ProtocolError())?;
                trace!("Subscribed to notifications for characteristic {:?}",
                       gatt_characteristic.Uuid());
            }
            Err(err) => {
                if err.code() == RO_E_CLOSED {
                    // ObjectDisposed
                    // Device disconnection will invalidate GattCharacteristic instances
                    // and since device disconnection happens asynchronously it's possible
                    // that this is the first we'll know of a disconnect happening

                    warn!("Inferred disconnect from IO error for peripheral {:?}",
                          peripheral_handle);
                    self.notify_disconnect(peripheral_handle);
                }
            }
        }

        Ok(())
    }

    async fn gatt_characteristic_unsubscribe(&self, peripheral_handle: PeripheralHandle,
                                             _service_handle: ServiceHandle,
                                             characteristic_handle: CharacteristicHandle)
                                             -> Result<()> {
        let winrt_peripheral = self.winrt_peripheral_from_handle(peripheral_handle)?;

        let gatt_characteristic = {
            let peripheral_guard = winrt_peripheral.inner.read().unwrap();

            let winrt_characteristic = peripheral_guard.gatt_characteristics
                                                       .get_mut(&characteristic_handle);
            let mut winrt_characteristic = match winrt_characteristic {
                Some(winrt_characteristic) => winrt_characteristic,
                None => {
                    warn!("Spurious request with unknown characteristic handle");
                    return Err(Error::Other(anyhow!("Unknown characteristic handle")));
                }
            };

            let gatt_characteristic = winrt_characteristic.gatt_characteristic.clone();

            if winrt_characteristic.notifications_handler.is_some() {
                gatt_characteristic.RemoveValueChanged(winrt_characteristic.notifications_handler)?;
                winrt_characteristic.notifications_handler = None;
            } else {
                warn!("No value changed handler found when unsubscribing from characteristic notifications (not subscribed?)");
            }

            gatt_characteristic
        };

        let config = GattClientCharacteristicConfigurationDescriptorValue::None;
        let fut = gatt_characteristic
            .WriteClientCharacteristicConfigurationDescriptorWithResultAsync(config)?;
        match fut.await {
            Ok(result) => {
                self.check_gatt_communication_status(result.Status(), result.ProtocolError())?;
                trace!("Unsubscribed from notifications for characteristic {:?}",
                       gatt_characteristic.Uuid());
            }
            Err(err) => {
                if err.code() == RO_E_CLOSED {
                    // ObjectDisposed
                    // Device disconnection will invalidate GattCharacteristic instances
                    // and since device disconnection happens asynchronously it's possible
                    // that this is the first we'll know of a disconnect happening

                    warn!("Inferred disconnect from IO error for peripheral {:?}",
                          peripheral_handle);
                    self.notify_disconnect(peripheral_handle);
                }
            }
        }

        Ok(())
    }

    async fn gatt_characteristic_discover_descriptors(&self, peripheral_handle: PeripheralHandle,
                                                      service_handle: ServiceHandle,
                                                      characteristic_handle: CharacteristicHandle)
                                                   -> Result<()> {
        let winrt_peripheral = self.winrt_peripheral_from_handle(peripheral_handle)?;
        let gatt_service = self.gatt_service_from_handle(&winrt_peripheral, service_handle)?;
        let gatt_characteristic = self.gatt_characteristic_from_handle(&winrt_peripheral, characteristic_handle)?;

        let descriptors_result = gatt_characteristic.GetDescriptorsAsync()?.await?;
        self.check_gatt_communication_status(descriptors_result.Status(),
                                             descriptors_result.ProtocolError())?;

        self.index_descriptors_result(&winrt_peripheral, &descriptors_result)?;

        let descriptors = descriptors_result.Descriptors()?;
        let backend_bus = self.inner.backend_bus.clone();

        for descriptor in descriptors {
            let attribute = descriptor.AttributeHandle()?;
            let descriptor_handle = DescriptorHandle(attribute as u32);
            let uuid: Uuid = Uuid::try_from_guid(&descriptor.Uuid()?)?;
            let _ = backend_bus.send(BackendEvent::GattDescriptor {
                peripheral_handle,
                service_handle,
                characteristic_handle,
                descriptor_handle,
                uuid
            });
        }

        // In this case we queried all the characteristics for the service so we can
        // tell the frontend that it now knows about all of the service's
        // characteristics...
        let _ = backend_bus
            .send(BackendEvent::GattDescriptorsComplete {
                peripheral_handle,
                service_handle,
                characteristic_handle,
                error: None
            });
        Ok(())
    }

    async fn gatt_descriptor_read(&self, peripheral_handle: PeripheralHandle,
                                  service_handle: ServiceHandle,
                                  characteristic_handle: CharacteristicHandle,
                                  descriptor_handle: DescriptorHandle,
                                  cache_mode: CacheMode)
                                  -> Result<Vec<u8>> {
        let winrt_peripheral = self.winrt_peripheral_from_handle(peripheral_handle)?;
        let gatt_descriptor =
            self.gatt_descriptor_from_handle(&winrt_peripheral, descriptor_handle)?;
        let cache_mode = match cache_mode {
            CacheMode::Cached => BluetoothCacheMode::Cached,
            CacheMode::Uncached => BluetoothCacheMode::Uncached,
        };
        let result = gatt_descriptor.ReadValueWithCacheModeAsync(cache_mode)?
                                        .await?;
        self.check_gatt_communication_status(result.Status(), result.ProtocolError())?;

        let buf = Vec::<u8>::try_from(&result.Value()?)?;
        Ok(buf)
    }

    async fn gatt_descriptor_write(&self, peripheral_handle: PeripheralHandle,
                                   service_handle: ServiceHandle,
                                   characteristic_handle: CharacteristicHandle,
                                   descriptor_handle: DescriptorHandle,
                                   data: &[u8])
                                   -> Result<()> {
        let winrt_peripheral = self.winrt_peripheral_from_handle(peripheral_handle)?;
        let gatt_descriptor =
            self.gatt_descriptor_from_handle(&winrt_peripheral, descriptor_handle)?;

        let writer = DataWriter::new()?;
        writer.WriteBytes(data)?;
        let fut = gatt_descriptor.WriteValueWithResultAsync(writer.DetachBuffer()?)?;

        // XXX: Rust _really_ needs to improve how it captures for generators, it's like before non-lexical lifetimes
        // where you have to do awkward rearrangements purely for the sake of spelling out to the compiler that what
        // you're doing is fine.
        //
        // Here the compiler is having a fit about the IBuffer from `writer.DetachBuffer()?` not being Send, even
        // though it's moved straight into `WriteValueWithResultAndOptionAsync` and clearly doesn't need to be
        // captured by the future for this function to survive beyond this await.
        //
        // We have to split this into a separate statement avoid capturing the IBuffer
        let result = fut.await?;

        self.check_gatt_communication_status(result.Status(), result.ProtocolError())?;

        Ok(())
    }

    //fn gatt_service_uuid(&self, peripheral_handle: PeripheralHandle,
    //                     service_handle: ServiceHandle)
    //                     -> Result<Uuid> {
    //    let winrt_peripheral = self.winrt_peripheral_from_handle(peripheral_handle)?;
    //    let gatt_service = self.gatt_service_from_handle(&winrt_peripheral, service_handle)?;
    //    Ok(Uuid::try_from_guid(&gatt_service.Uuid()?)?)
    //}

    //fn gatt_characteristic_uuid(&self, peripheral_handle: PeripheralHandle,
    //                            characteristic_handle: CharacteristicHandle)
    //                            -> Result<Uuid> {
    //    let winrt_peripheral = self.winrt_peripheral_from_handle(peripheral_handle)?;
    //    let gatt_characteristic =
    //        self.gatt_characteristic_from_handle(&winrt_peripheral, characteristic_handle)?;
    //    Ok(Uuid::try_from_guid(&gatt_characteristic.Uuid()?)?)
    //}

    fn flush(&self, id: u32) -> Result<()> {
        let _ = self.inner.backend_bus.send(BackendEvent::Flush(id));
        Ok(())
    }
}

impl From<windows::Error> for Error {
    fn from(err: windows::Error) -> Error {
        Error::Other(anyhow!(err))
    }
}

impl From<GattCharacteristicProperties> for CharacteristicProperties {
    fn from(gatt_props: GattCharacteristicProperties) -> Self {
        let mut props = CharacteristicProperties::NONE;
        if gatt_props & GattCharacteristicProperties::Broadcast != GattCharacteristicProperties::None {
            props |= CharacteristicProperties::BROADCAST;
        }
        if gatt_props & GattCharacteristicProperties::Read != GattCharacteristicProperties::None {
            props |= CharacteristicProperties::READ;
        }
        if gatt_props & GattCharacteristicProperties::WriteWithoutResponse != GattCharacteristicProperties::None {
            props |= CharacteristicProperties::WRITE_WITHOUT_RESPONSE;
        }
        if gatt_props & GattCharacteristicProperties::Write != GattCharacteristicProperties::None {
            props |= CharacteristicProperties::WRITE;
        }
        if gatt_props & GattCharacteristicProperties::Notify != GattCharacteristicProperties::None {
            props |= CharacteristicProperties::NOTIFY;
        }
        if gatt_props & GattCharacteristicProperties::Indicate != GattCharacteristicProperties::None {
            props |= CharacteristicProperties::INDICATE;
        }
        if gatt_props & GattCharacteristicProperties::AuthenticatedSignedWrites != GattCharacteristicProperties::None {
            props |= CharacteristicProperties::AUTHENTICATED_SIGNED_WRITES;
        }
        if gatt_props & GattCharacteristicProperties::ExtendedProperties != GattCharacteristicProperties::None {
            props |= CharacteristicProperties::EXTENDED_PROPERTIES;
        }
        if gatt_props & GattCharacteristicProperties::ReliableWrites != GattCharacteristicProperties::None {
            props |= CharacteristicProperties::RELIABLE_WRITES;
        }
        if gatt_props & GattCharacteristicProperties::WritableAuxiliaries != GattCharacteristicProperties::None {
            props |= CharacteristicProperties::WRITABLE_AUXILIARIES;
        }

        props
    }
}