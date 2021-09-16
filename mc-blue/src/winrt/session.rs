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
use bindings::Windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristic, GattCharacteristicProperties, GattCharacteristicsResult,
    GattClientCharacteristicConfigurationDescriptorValue, GattCommunicationStatus,
    GattDeviceService, GattDeviceServicesResult, GattValueChangedEventArgs, GattWriteOption,
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
use log::{debug, info, trace, warn};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

use crate::service::Service;
use crate::session::{BackendSession, Filter, Session, SessionConfig};
use crate::winrt::bindings;
use crate::{characteristic, peripheral};
use crate::{
    fake, uuid::BluetoothUuid, winrt, AddressType, BackendPeripheralProperty, Error,
    MacAddressType, Result,
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
    connection_status_handler: Option<EventRegistrationToken>,

    // NB: we don't index services by uuid since it's possible for a device to
    // instantiate a service multiple times - each with the same uuid - but
    // with a different ATT handle.
    //
    // FIXME: switch this to a regular HashMap since we anyway have to
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
}

impl Drop for WinrtPeripheralInner {
    fn drop(&mut self) {
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
    pub async fn new(_config: &SessionConfig, backend_bus: mpsc::UnboundedSender<BackendEvent>)
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
                        connection_status_handler: None,
                        gatt_services: DashMap::new(),
                        gatt_characteristics: DashMap::new(),
                    })),
                };
                self.inner
                    .peripherals_by_mac
                    .insert(address, winrt_peripheral.clone());
                self.inner
                    .peripherals_by_handle
                    .insert(peripheral_handle, winrt_peripheral);

                let _ = backend_bus.send(BackendEvent::PeripheralFound { peripheral_handle });
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

        let _ = backend_bus.send(BackendEvent::PeripheralPropertySet {
            peripheral_handle,
            property: BackendPeripheralProperty::Address(Address::MAC(MAC(address))),
        });

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
                Err(Error::PeripheralGattProtocolError(protocol_error))
            }
            GattCommunicationStatus::Unreachable => Err(Error::PeripheralUnreachable),
            GattCommunicationStatus::Success => Ok(()),
            _ => {
                warn!("Spurious GATT communication status: {:?}", status);
                Err(Error::Other(anyhow!("Spurious GATT communication status")))
            }
        }
    }

    async fn get_gatt_services(&self, device: &BluetoothLEDevice, cache_mode: BluetoothCacheMode)
                               -> Result<GattDeviceServicesResult> {
        let gatt_service_result = device.GetGattServicesWithCacheModeAsync(cache_mode)?
                                        .await?;
        self.check_gatt_communication_status(gatt_service_result.Status(),
                                             gatt_service_result.ProtocolError())?;
        Ok(gatt_service_result)
    }

    fn notify_disconnect(&self, peripheral_handle: PeripheralHandle) {
        match self.invalidate_gatt_state(peripheral_handle) {
            Ok(()) => {
                trace!("Invalidated GATT state for peripheral");
            }
            Err(err) => {
                warn!("Failed to invalidate GATT state for peripheral: {:?}", err);
            }
        }

        let _ = self.inner
                    .backend_bus
                    .send(BackendEvent::PeripheralDisconnected { peripheral_handle });
    }

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
    }

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
    }

    // Make sure we've got a BluetoothLEDevice for our peripheral (we don't look up
    // a device for every peripheral discovered via advertising packets - we only
    // need a device once an application is interested in connecting to the
    // peripheral)
    //
    // Returns a reference to the device
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

    fn index_services_result(&self, winrt_peripheral: &WinrtPeripheral,
                             services_result: &GattDeviceServicesResult)
                             -> Result<()> {
        // Only need a 'read' lock since the DashMap has interior mutability.
        let guard = winrt_peripheral.inner.read().unwrap();

        let services = services_result.Services()?;

        for service in services {
            let attribute = service.AttributeHandle()?;
            let service_handle = ServiceHandle(attribute);
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
            let characteristic_handle = CharacteristicHandle(attribute);
            let winrt_characteristic = WinrtCharacteristic { gatt_characteristic: characteristic,
                                                             notifications_handler: None };
            guard.gatt_characteristics
                 .insert(characteristic_handle, winrt_characteristic);
        }

        Ok(())
    }

    fn invalidate_gatt_state(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        let winrt_peripheral = self.winrt_peripheral_from_handle(peripheral_handle)?;
        let guard = winrt_peripheral.inner.read().unwrap();

        // Note: by de-indexing all GATT state handles then any requests that hit
        // get_service_from_handle() or get_characteristic_from_handle() will return
        // and InvalidStateReference error back to the caller...
        guard.gatt_services.clear();
        guard.gatt_characteristics.clear();

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

    fn declare_peripheral(&self, address: Address) -> Result<PeripheralHandle> {
        if let Address::MAC(MAC(mac)) = address {
            Ok(self.peripheral_from_mac(mac))
        } else {
            Err(Error::Other(anyhow!("Unsupported declaration address")))
        }
    }

    async fn peripheral_connect(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        let winrt_peripheral = match self.inner.peripherals_by_handle.get(&peripheral_handle) {
            Some(p) => p,
            None => {
                log::error!("Spurious connection request with unknown peripheral handle {:?}",
                            peripheral_handle);
                return Err(Error::Other(anyhow!("Unknown peripheral")));
            }
        };

        let device = self.ensure_device_with_connection_status_handler(&winrt_peripheral)
                         .await?;

        // Windows doesn't actually expose a connection-oriented API so to
        // 'connect' we just have to perform an action that we know requires
        // a connection. By default winrt queries for gatt services and
        // characteristics etc happen via a system cache which can avoid needing
        // to actually connect to the device. By explicitly bypassing the cache
        // we know the system will have to really connect to the device...
        self.get_gatt_services(&device, BluetoothCacheMode::Uncached)
            .await?;

        Ok(())
    }

    async fn peripheral_discover_gatt_services(&self, peripheral_handle: PeripheralHandle)
                                               -> Result<()> {
        let winrt_peripheral = self.winrt_peripheral_from_handle(peripheral_handle)?;

        let device = self.ensure_device_with_connection_status_handler(&winrt_peripheral)
                         .await?;
        let services_result = self.get_gatt_services(&device, BluetoothCacheMode::Cached)
                                  .await?;
        self.check_gatt_communication_status(services_result.Status(),
                                             services_result.ProtocolError())?;

        self.index_services_result(&winrt_peripheral, &services_result)?;

        let services = services_result.Services()?;
        let peripheral_handle = winrt_peripheral.peripheral_handle;
        let backend_bus = self.inner.backend_bus.clone();

        for service in services {
            let attribute = service.AttributeHandle()?;
            let service_handle = ServiceHandle(attribute);
            let uuid: Uuid = Uuid::try_from_guid(&service.Uuid()?)?;
            let _ =
                backend_bus.send(BackendEvent::PeripheralPrimaryGattService { peripheral_handle,
                                                                              service_handle,
                                                                              uuid });
        }

        // In this case we queried all the services for the device so we can
        // tell the frontend that it now knows about all of the peripheral's
        // primary services...
        let _ = backend_bus
            .send(BackendEvent::PeripheralPrimaryGattServicesComplete { peripheral_handle });

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
            let included_service_handle = ServiceHandle(attribute);
            let uuid: Uuid = Uuid::try_from_guid(&service.Uuid()?)?;
            let _ = backend_bus.send(BackendEvent::ServiceIncludedGattService {
                peripheral_handle,
                parent_service_handle: service_handle,
                included_service_handle,
                uuid,
            });
        }

        // In this case we queried all the included services for the service so we can
        // tell the frontend that it now knows about all of the service's
        // included services...
        let _ =
            backend_bus.send(BackendEvent::ServiceIncludedGattServicesComplete { peripheral_handle,
                                                                                 service_handle });

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
        let peripheral_handle = winrt_peripheral.peripheral_handle;
        let backend_bus = self.inner.backend_bus.clone();

        for characteristic in characteristics {
            let attribute = characteristic.AttributeHandle()?;
            let characteristic_handle = CharacteristicHandle(attribute);
            let uuid: Uuid = Uuid::try_from_guid(&characteristic.Uuid()?)?;
            let _ = backend_bus.send(BackendEvent::ServiceGattCharacteristic { peripheral_handle,
                                                                    service_handle,
                                                                    characteristic_handle,
                                                                    uuid });
        }

        // In this case we queried all the characteristics for the service so we can
        // tell the frontend that it now knows about all of the service's
        // characteristics...
        let _ =
            backend_bus.send(BackendEvent::ServiceGattCharacteristicsComplete { peripheral_handle,
                                                                                service_handle });
        Ok(())
    }

    async fn gatt_characteristic_read(&self, peripheral_handle: PeripheralHandle,
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
        // captured by the future for this function to survive beyond this this await.
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
                                    BackendEvent::ServiceGattCharacteristicNotify {
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

    fn gatt_service_uuid(&self, peripheral_handle: PeripheralHandle,
                         service_handle: ServiceHandle)
                         -> Result<Uuid> {
        let winrt_peripheral = self.winrt_peripheral_from_handle(peripheral_handle)?;
        let gatt_service = self.gatt_service_from_handle(&winrt_peripheral, service_handle)?;
        Ok(Uuid::try_from_guid(&gatt_service.Uuid()?)?)
    }

    fn gatt_characteristic_uuid(&self, peripheral_handle: PeripheralHandle,
                                characteristic_handle: CharacteristicHandle)
                                -> Result<Uuid> {
        let winrt_peripheral = self.winrt_peripheral_from_handle(peripheral_handle)?;
        let gatt_characteristic =
            self.gatt_characteristic_from_handle(&winrt_peripheral, characteristic_handle)?;
        Ok(Uuid::try_from_guid(&gatt_characteristic.Uuid()?)?)
    }

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
