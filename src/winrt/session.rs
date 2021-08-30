use std::borrow::Cow;
use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::RwLock as StdRwLock;
use std::sync::atomic::compiler_fence;
use async_trait::async_trait;

use crate::winrt::bindings;
use bindings::Windows::Foundation::{EventRegistrationToken, TypedEventHandler};
use bindings::Windows::Storage::Streams::{DataReader, IBuffer};
use bindings::Windows::Devices::Bluetooth::Advertisement::*;
use bindings::Windows::Devices::Bluetooth::BluetoothAddressType;
use bindings::Windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristic, GattCommunicationStatus, GattDeviceService, GattDeviceServicesResult,
};
use bindings::Windows::Devices::Bluetooth::{BluetoothConnectionStatus, BluetoothLEDevice};
use windows::Guid;
use super::uuid::WinrtUuid;

use futures::{stream, Stream, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;
use log::{info, trace, warn};
use anyhow::anyhow;
use uuid::Uuid;

use crate::peripheral;
use crate::session::{Session, PlatformSession, SessionConfig, Filter};
use crate::{PlatformEvent, PlatformPeripheralHandle};
use crate::{
    fake, uuid::BluetoothUuid, winrt, AddressType, Error, MacAddressType,
    PlatformPeripheralProperty, Result,
};
use crate::{Address, MAC};
use dashmap::DashMap;


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
    platform_bus: mpsc::UnboundedSender<PlatformEvent>,
    peripherals_by_mac: DashMap<u64, WinrtPeripheral>,
    peripherals_by_handle: DashMap<PlatformPeripheralHandle, WinrtPeripheral>,
    next_handle: AtomicU32,
}

impl Drop for WinrtSessionInner {
    fn drop(&mut self) {
        // XXX: I wasn't 100% sure sure whether the winrt bindings would automatically
        // just do the right thing or we have to remove the handler. I _think_ actually
        // we can rely on the windows-rs bindings here but should try to double check
        // this
    }
}


#[derive(Clone, Debug)]
struct WinrtPeripheral
{
    address: u64,
    peripheral_handle: PlatformPeripheralHandle,
    inner: Arc<StdRwLock<WinrtPeripheralInner>>
}

#[derive(Debug)]
struct WinrtPeripheralInner
{
    ble_device: Option<BluetoothLEDevice>,
    connection_status_handler: Option<EventRegistrationToken>,
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

    pub async fn new(
        _config: &SessionConfig,
        platform_bus: mpsc::UnboundedSender<PlatformEvent>,
    ) -> Result<Self> {
        let filter = BluetoothLEAdvertisementFilter::new().map_err(|e| Error::Other(anyhow!(e)))?;
        let watcher = BluetoothLEAdvertisementWatcher::Create(&filter)
            .map_err(|e| Error::Other(anyhow!(e)))?;
        Ok(WinrtSession {
            inner: Arc::new(WinrtSessionInner {
                watcher,
                platform_bus,
                peripherals_by_mac: DashMap::new(),
                peripherals_by_handle: DashMap::new(),
                next_handle: AtomicU32::new(1),
            }),
        })
    }

    fn on_advertisement_received(&self, args: &BluetoothLEAdvertisementReceivedEventArgs) {
        let advertisement = args.Advertisement().unwrap();

        let platform_bus = self.inner.platform_bus.clone();

        let address = match args.BluetoothAddress() {
            Ok(addr) => addr,
            Err(_e) => {
                return;
            }
        };

        let peripheral_handle = {

            let peripheral_handle = match self.inner.peripherals_by_mac.get(&address) {
                None => {
                    let peripheral_id = self.inner.next_handle.fetch_add(1, Ordering::SeqCst);
                    let peripheral_handle = PlatformPeripheralHandle(peripheral_id);

                    let winrt_peripheral = WinrtPeripheral {
                        address: address,
                        peripheral_handle,
                        inner: Arc::new(StdRwLock::new(WinrtPeripheralInner {
                            ble_device: None,
                            connection_status_handler: None
                        }))
                    };
                    self.inner.peripherals_by_mac.insert(address, winrt_peripheral.clone());
                    self.inner.peripherals_by_handle.insert(peripheral_handle, winrt_peripheral);

                    let _ = platform_bus.send(PlatformEvent::PeripheralFound { peripheral_handle });
                    peripheral_handle
                }
                Some(winrt_peripheral) => {
                    winrt_peripheral.peripheral_handle
                }
            };

            peripheral_handle
        };

        let _ = platform_bus.send(PlatformEvent::PeripheralPropertySet {
            peripheral_handle,
            property: PlatformPeripheralProperty::Address(Address::MAC(MAC(address))),
        });

        if let Ok(address_type) = args.BluetoothAddressType() {
            let address_type = match address_type {
                BluetoothAddressType::Public => Some(AddressType::PublicMAC),
                BluetoothAddressType::Random => Some(AddressType::RandomMAC),
                _ => None,
            };
            if let Some(address_type) = address_type {
                let _ = platform_bus.send(PlatformEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: PlatformPeripheralProperty::AddressType(address_type),
                });
            }
        }
        if let Ok(name) = advertisement.LocalName() {
            if !name.is_empty() {
                let _ = platform_bus.send(PlatformEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: PlatformPeripheralProperty::Name(name.to_string()),
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
                let _ = platform_bus.send(PlatformEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: PlatformPeripheralProperty::ManufacturerData(data_map),
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
                let _ = platform_bus.send(PlatformEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: PlatformPeripheralProperty::ServiceData(data_map),
                });
            }
        }

        if let Ok(services) = advertisement.ServiceUuids() {
            let uuids: Vec<Uuid> = services
                .into_iter()
                .filter_map(|guid| {
                    if let Ok(uuid) = Uuid::try_from_guid(&guid) {
                        Some(uuid)
                    } else {
                        None
                    }
                })
                .collect();

            if !uuids.is_empty() {
                let _ = platform_bus.send(PlatformEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: PlatformPeripheralProperty::Services(uuids),
                });
            }
        }

        if let Ok(tx_reference) = args.TransmitPowerLevelInDBm() {
            // IReference is (ironically) a crazy foot gun in Rust since it very easily
            // panics if you look at it wrong. Calling GetInt16(), IsNumericScalar() or Type()
            // all panic here without returning a Result as documented.
            // Value() is apparently the _right_ way to extract something from an IReference<T>...
            if let Ok(tx) = tx_reference.Value() {
                let _ = platform_bus.send(PlatformEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: PlatformPeripheralProperty::TxPower(tx),
                });
            }
        }

        if let Ok(rssi) = args.RawSignalStrengthInDBm() {
            let _ = platform_bus.send(PlatformEvent::PeripheralPropertySet {
                peripheral_handle,
                property: PlatformPeripheralProperty::Rssi(rssi),
            });
        }
    }

    async fn get_gatt_services(&self, device: &BluetoothLEDevice) -> Result<()> {
        let gatt_service_result = device.GetGattServicesAsync()?.await?;
        let status = gatt_service_result.Status().map_err(|_| Error::Other(anyhow!("Unable to check GATT query status")))?;
        match status {
            GattCommunicationStatus::AccessDenied => Err(Error::PeripheralAccessDenied),
            GattCommunicationStatus::ProtocolError => Err(Error::PeripheralGattProtocolError),
            GattCommunicationStatus::Unreachable => Err(Error::PeripheralUnreachable),
            GattCommunicationStatus::Success => Ok(()),
            _ => {
                warn!("Spurious GATT query status: {:?}", status);
                Err(Error::Other(anyhow!("Spurious GATT query status")))
            }
        }
    }
}

#[async_trait]
impl PlatformSession for WinrtSession {
    async fn start_scanning(&self, filter: &Filter) -> Result<()> {
        trace!("winrt: start scanning");

        self.inner
            .watcher
            .SetScanningMode(BluetoothLEScanningMode::Active)?;

        // XXX: we're careful here to avoid giving a strong (circular) reference to
        // the watcher callback, otherwise it won't be possible to ever drop a
        // WinrtSession after starting scannnig.
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

    async fn connect_peripheral(&self, peripheral_handle: PlatformPeripheralHandle) -> Result<()> {
        let winrt_peripheral = match self.inner.peripherals_by_handle.get(&peripheral_handle) {
            Some(p) => p,
            None => {
                log::error!("Spurious connection request with unknown peripheral handle {:?}", peripheral_handle);
                return Err(Error::Other(anyhow!("Unknown peripheral")));
            }
        };

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
            let platform_bus = self.inner.platform_bus.clone();
            let peripheral_address = winrt_peripheral.address;

            let connection_status_handler =
                TypedEventHandler::new(move |sender: &Option<BluetoothLEDevice>, _| {
                    if let Some(sender) = sender {
                        match sender.ConnectionStatus() {
                            Ok(BluetoothConnectionStatus::Connected) => {
                                trace!("Peripheral connected: handle={:?}/{}", peripheral_handle, MAC(peripheral_address).to_string());
                                let _ = platform_bus.send(PlatformEvent::PeripheralConnected { peripheral_handle } );
                            },
                            Ok(BluetoothConnectionStatus::Disconnected) => {
                                trace!("Peripheral connected: handle={:?}/{}", peripheral_handle, MAC(peripheral_address).to_string());
                                let _ = platform_bus.send(PlatformEvent::PeripheralDisconnected { peripheral_handle } );
                            },
                            Ok(status) => {
                                log::error!("Spurious bluetooth connection status: {:?}: handle={:?}/{}", status, peripheral_handle, MAC(peripheral_address).to_string());
                            }
                            Err(err) => {
                                log::error!("Failure while querying bluetooth connection status: {:?}: handle={:?}/{}",
                                            err, peripheral_handle, MAC(peripheral_address).to_string())
                            }
                        }
                    }

                    Ok(())
                });

            let mut winrt_peripheral_guard = winrt_peripheral.inner.write().unwrap();
            winrt_peripheral_guard.connection_status_handler = Some(device.ConnectionStatusChanged(connection_status_handler)
                .map_err(|_| Error::Other(anyhow!("Could not add connection status handler")))?);
        }

        self.get_gatt_services(&device).await
    }
}

impl From<windows::Error> for Error {
    fn from(err: windows::Error) -> Error {
        Error::Other(anyhow!(err))
    }
}
