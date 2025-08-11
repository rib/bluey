use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use js_sys::{Array, Object, Promise, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Bluetooth, BluetoothDevice, BluetoothLeScanFilterInit, BluetoothRemoteGattCharacteristic,
    BluetoothRemoteGattDescriptor, BluetoothRemoteGattServer, BluetoothRemoteGattService,
    Navigator, RequestDeviceOptions, Window,
};

use uuid::Uuid;

use crate::characteristic::{CharacteristicProperties, WriteType};
use crate::session::{BackendSession, Filter, SessionConfig};
use crate::{
    Address, BackendEvent, BackendPeripheralProperty, CharacteristicHandle, DescriptorHandle,
    Error, PeripheralHandle, Result, ServiceHandle,
};

#[derive(Debug)]
struct DeviceState {
    device: BluetoothDevice,
    server: Option<BluetoothRemoteGattServer>,
    services: HashMap<ServiceHandle, BluetoothRemoteGattService>,
    characteristics: HashMap<CharacteristicHandle, BluetoothRemoteGattCharacteristic>,
    descriptors: HashMap<DescriptorHandle, BluetoothRemoteGattDescriptor>,
}
/// Safety: The web backend is only supported on the wasm32 target which is single-threaded.
#[cfg(target_arch = "wasm32")]
unsafe impl Send for DeviceState {}
#[cfg(target_arch = "wasm32")]
unsafe impl Sync for DeviceState {}

#[derive(Debug)]
pub(crate) struct WebSessionInner {
    backend_bus: tokio::sync::mpsc::UnboundedSender<BackendEvent>,
    next_peripheral_handle: AtomicU32,
    next_service_handle: AtomicU32,
    next_characteristic_handle: AtomicU32,
    next_descriptor_handle: AtomicU32,
    devices: DashMap<PeripheralHandle, DeviceState>,
}

#[derive(Debug, Clone)]
pub(crate) struct WebSession {
    inner: Arc<WebSessionInner>,
}

impl From<JsValue> for Error {
    fn from(value: JsValue) -> Self {
        Error::Other(anyhow::anyhow!("JS Error: {:?}", value))
    }
}

impl WebSession {
    pub fn new(
        config: &SessionConfig<'_>, backend_bus: tokio::sync::mpsc::UnboundedSender<BackendEvent>,
    ) -> Result<Self> {
        let inner = WebSessionInner {
            backend_bus,
            next_peripheral_handle: AtomicU32::new(1),
            next_service_handle: AtomicU32::new(1),
            next_characteristic_handle: AtomicU32::new(1),
            next_descriptor_handle: AtomicU32::new(1),
            devices: DashMap::new(),
        };

        Ok(WebSession {
            inner: Arc::new(inner),
        })
    }

    fn get_bluetooth_api() -> Result<Bluetooth> {
        let window: Window = web_sys::window().ok_or(Error::Unsupported)?;
        let navigator: Navigator = window.navigator();
        let bluetooth: Bluetooth = navigator.bluetooth().ok_or(Error::Unsupported)?;
        Ok(bluetooth)
    }
}

#[async_trait(?Send)]
impl BackendSession for WebSession {
    fn supports_scanning(&self) -> bool {
        false
    }
    fn supports_select_peripheral(&self) -> bool {
        true
    }
    fn supports_declare_peripheral(&self) -> bool {
        false
    }
    fn has_scan_permission(&self) -> bool {
        false // Web platform doesn't support scanning
    }

    async fn start_scanning(&self, filter: &Filter) -> Result<()> {
        Err(Error::Unsupported)
    }
    async fn stop_scanning(&self) -> Result<()> {
        Err(Error::Unsupported)
    }

    async fn select_peripheral(&self, filter: &Filter) -> Result<PeripheralHandle> {
        let bluetooth = Self::get_bluetooth_api()?;

        let filters = Array::new();
        for uuid in &filter.service_uuids {
            let mut scan_filter = BluetoothLeScanFilterInit::new();
            scan_filter.services(&Array::of1(&JsValue::from(uuid.to_string())));
            filters.push(&scan_filter.into());
        }

        let mut options = RequestDeviceOptions::new();
        options.filters(&filters);

        let device_promise = bluetooth.request_device(&options);
        let device_js = JsFuture::from(device_promise).await?;
        let device: BluetoothDevice = device_js.into();

        let id = device.id();
        let name = device.name();

        let peripheral_id = self
            .inner
            .next_peripheral_handle
            .fetch_add(1, Ordering::SeqCst);
        let peripheral_handle = PeripheralHandle(peripheral_id);

        // Store device state
        let device_ref = DeviceState {
            device: device.clone(),
            server: None,
            services: HashMap::new(),
            characteristics: HashMap::new(),
            descriptors: HashMap::new(),
        };
        self.inner.devices.insert(peripheral_handle, device_ref);

        web_sys::console::log_1(
            &format!(
                "Web backend: Sending PeripheralFound for handle {:?}",
                peripheral_handle
            )
            .into(),
        );
        let _ = self
            .inner
            .backend_bus
            .send(BackendEvent::PeripheralFound { peripheral_handle });

        web_sys::console::log_1(
            &format!(
                "Web backend: Sending address property for handle {:?}",
                peripheral_handle
            )
            .into(),
        );
        let _ = self
            .inner
            .backend_bus
            .send(BackendEvent::PeripheralPropertySet {
                peripheral_handle,
                property: BackendPeripheralProperty::Address(Address::String(id)),
            });
        if let Some(name) = name {
            web_sys::console::log_1(
                &format!(
                    "Web backend: Sending name property for handle {:?}",
                    peripheral_handle
                )
                .into(),
            );
            let _ = self
                .inner
                .backend_bus
                .send(BackendEvent::PeripheralPropertySet {
                    peripheral_handle,
                    property: BackendPeripheralProperty::Name(name),
                });
        }

        Ok(peripheral_handle)
    }

    fn declare_peripheral(&self, address: Address, name: String) -> Result<PeripheralHandle> {
        Err(Error::Unsupported)
    }

    async fn peripheral_connect(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        let mut device_ref = self
            .inner
            .devices
            .get_mut(&peripheral_handle)
            .ok_or(Error::InvalidStateReference)?;

        let gatt_promise = device_ref.device.gatt().unwrap().connect();
        let server_js = JsFuture::from(gatt_promise).await?;
        let server: BluetoothRemoteGattServer = server_js.into();

        device_ref.server = Some(server);

        web_sys::console::log_1(
            &format!(
                "Web backend: Sending PeripheralConnected for handle {:?}",
                peripheral_handle
            )
            .into(),
        );
        let _ = self
            .inner
            .backend_bus
            .send(BackendEvent::PeripheralConnected { peripheral_handle });

        Ok(())
    }
    async fn peripheral_disconnect(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        let mut device_ref = self
            .inner
            .devices
            .get_mut(&peripheral_handle)
            .ok_or(Error::InvalidStateReference)?;

        if let Some(server) = &device_ref.server {
            server.disconnect();
        }

        // Clear GATT state
        device_ref.server = None;
        device_ref.services.clear();
        device_ref.characteristics.clear();
        device_ref.descriptors.clear();

        let _ = self
            .inner
            .backend_bus
            .send(BackendEvent::PeripheralDisconnected {
                peripheral_handle,
                error: None,
            });

        Ok(())
    }

    fn peripheral_drop_gatt_state(&self, peripheral_handle: PeripheralHandle) {
        if let Some(mut device_ref) = self.inner.devices.get_mut(&peripheral_handle) {
            if let Some(server) = &device_ref.server {
                server.disconnect();
            }
            device_ref.server = None;
            device_ref.services.clear();
            device_ref.characteristics.clear();
            device_ref.descriptors.clear();
        }
    }

    async fn peripheral_read_rssi(&self, peripheral_handle: PeripheralHandle) -> Result<i16> {
        // Web Bluetooth API doesn't provide RSSI reading after connection
        Err(Error::Unsupported)
    }

    async fn peripheral_discover_gatt_services(
        &self, peripheral_handle: PeripheralHandle, of_interest_hint: Option<Vec<Uuid>>,
    ) -> Result<()> {
        let server = {
            let device_ref = self
                .inner
                .devices
                .get(&peripheral_handle)
                .ok_or(Error::InvalidStateReference)?;
            device_ref
                .server
                .as_ref()
                .ok_or(Error::PeripheralUnreachable)?
                .clone()
        };

        let services_promise = if let Some(uuids) = &of_interest_hint {
            if uuids.len() == 1 {
                server.get_primary_service_with_str(&uuids[0].to_string())
            } else {
                server.get_primary_services()
            }
        } else {
            server.get_primary_services()
        };

        let services_js = JsFuture::from(services_promise).await?;

        if of_interest_hint.is_some() && of_interest_hint.as_ref().unwrap().len() == 1 {
            // Single service returned
            let service: BluetoothRemoteGattService = services_js.into();
            let service_handle = ServiceHandle(
                self.inner
                    .next_service_handle
                    .fetch_add(1, Ordering::SeqCst),
            );

            let uuid_str = service.uuid();
            let uuid = Uuid::parse_str(&uuid_str).unwrap_or_default();

            if let Some(mut device_ref) = self.inner.devices.get_mut(&peripheral_handle) {
                device_ref.services.insert(service_handle, service);
            }

            let _ = self.inner.backend_bus.send(BackendEvent::GattService {
                peripheral_handle,
                service_handle,
                uuid,
            });
        } else {
            // Multiple services returned as array
            let services_array: js_sys::Array = services_js.into();

            for i in 0..services_array.length() {
                let service_js = services_array.get(i);
                let service: BluetoothRemoteGattService = service_js.into();
                let service_handle = ServiceHandle(
                    self.inner
                        .next_service_handle
                        .fetch_add(1, Ordering::SeqCst),
                );

                let uuid_str = service.uuid();
                let uuid = Uuid::parse_str(&uuid_str).unwrap_or_default();

                if let Some(mut device_ref) = self.inner.devices.get_mut(&peripheral_handle) {
                    device_ref.services.insert(service_handle, service);
                }

                let _ = self.inner.backend_bus.send(BackendEvent::GattService {
                    peripheral_handle,
                    service_handle,
                    uuid,
                });
            }
        }

        let _ = self
            .inner
            .backend_bus
            .send(BackendEvent::GattServicesComplete {
                peripheral_handle,
                error: None,
            });

        Ok(())
    }
    async fn gatt_service_discover_includes(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
    ) -> Result<()> {
        // Web Bluetooth API doesn't support included services discovery
        let _ = self
            .inner
            .backend_bus
            .send(BackendEvent::GattIncludedServicesComplete {
                peripheral_handle,
                service_handle,
                error: None,
            });
        Ok(())
    }
    async fn gatt_service_discover_characteristics(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
    ) -> Result<()> {
        let mut device_ref = self
            .inner
            .devices
            .get_mut(&peripheral_handle)
            .ok_or(Error::InvalidStateReference)?;

        let service = device_ref
            .services
            .get(&service_handle)
            .ok_or(Error::InvalidStateReference)?
            .clone();

        let characteristics_promise = service.get_characteristics();
        let characteristics_js = JsFuture::from(characteristics_promise).await?;
        let characteristics_array: js_sys::Array = characteristics_js.into();

        for i in 0..characteristics_array.length() {
            let characteristic_js = characteristics_array.get(i);
            let characteristic: BluetoothRemoteGattCharacteristic = characteristic_js.into();
            let characteristic_handle = CharacteristicHandle(
                self.inner
                    .next_characteristic_handle
                    .fetch_add(1, Ordering::SeqCst),
            );

            let uuid_str = characteristic.uuid();
            let uuid = Uuid::parse_str(&uuid_str).unwrap_or_default();

            // Convert Web Bluetooth properties to our properties
            let properties_obj = characteristic.properties();
            let mut properties = CharacteristicProperties::NONE;

            if js_sys::Reflect::get(&properties_obj, &"read".into())
                .unwrap()
                .as_bool()
                .unwrap_or(false)
            {
                properties |= CharacteristicProperties::READ;
            }
            if js_sys::Reflect::get(&properties_obj, &"write".into())
                .unwrap()
                .as_bool()
                .unwrap_or(false)
            {
                properties |= CharacteristicProperties::WRITE;
            }
            if js_sys::Reflect::get(&properties_obj, &"writeWithoutResponse".into())
                .unwrap()
                .as_bool()
                .unwrap_or(false)
            {
                properties |= CharacteristicProperties::WRITE_WITHOUT_RESPONSE;
            }
            if js_sys::Reflect::get(&properties_obj, &"notify".into())
                .unwrap()
                .as_bool()
                .unwrap_or(false)
            {
                properties |= CharacteristicProperties::NOTIFY;
            }
            if js_sys::Reflect::get(&properties_obj, &"indicate".into())
                .unwrap()
                .as_bool()
                .unwrap_or(false)
            {
                properties |= CharacteristicProperties::INDICATE;
            }

            device_ref
                .characteristics
                .insert(characteristic_handle, characteristic);

            let _ = self
                .inner
                .backend_bus
                .send(BackendEvent::GattCharacteristic {
                    peripheral_handle,
                    service_handle,
                    characteristic_handle,
                    uuid,
                    properties,
                });
        }

        let _ = self
            .inner
            .backend_bus
            .send(BackendEvent::GattCharacteristicsComplete {
                peripheral_handle,
                service_handle,
                error: None,
            });

        Ok(())
    }

    async fn gatt_characteristic_read(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, cache_mode: crate::CacheMode,
    ) -> Result<Vec<u8>> {
        let device_ref = self
            .inner
            .devices
            .get(&peripheral_handle)
            .ok_or(Error::InvalidStateReference)?;

        let characteristic = device_ref
            .characteristics
            .get(&characteristic_handle)
            .ok_or(Error::InvalidStateReference)?;

        let read_promise = characteristic.read_value();
        let value_js = JsFuture::from(read_promise).await?;

        // Convert ArrayBuffer to Vec<u8>
        let array_buffer: js_sys::ArrayBuffer = value_js.into();
        let uint8_array = js_sys::Uint8Array::new(&array_buffer);
        let mut buffer = vec![0u8; uint8_array.length() as usize];
        uint8_array.copy_to(&mut buffer);

        Ok(buffer)
    }

    async fn gatt_characteristic_write(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, write_type: WriteType, data: &[u8],
    ) -> Result<()> {
        let device_ref = self
            .inner
            .devices
            .get(&peripheral_handle)
            .ok_or(Error::InvalidStateReference)?;

        let characteristic = device_ref
            .characteristics
            .get(&characteristic_handle)
            .ok_or(Error::InvalidStateReference)?;

        let mut data_slice = data.to_vec();

        let write_promise = match write_type {
            WriteType::WithResponse => {
                characteristic.write_value_with_response_with_u8_array(&mut data_slice)
            }
            WriteType::WithoutResponse => {
                characteristic.write_value_without_response_with_u8_array(&mut data_slice)
            }
        };

        JsFuture::from(write_promise).await?;
        Ok(())
    }

    async fn gatt_characteristic_subscribe(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
    ) -> Result<()> {
        let device_ref = self
            .inner
            .devices
            .get(&peripheral_handle)
            .ok_or(Error::InvalidStateReference)?;

        let characteristic = device_ref
            .characteristics
            .get(&characteristic_handle)
            .ok_or(Error::InvalidStateReference)?;

        // Create event handler for characteristic value changes
        let backend_bus = self.inner.backend_bus.clone();

        // Capture the handles for the closure
        let p_handle = peripheral_handle;
        let s_handle = service_handle;
        let c_handle = characteristic_handle;

        let closure = Closure::wrap(Box::new(move |event: web_sys::Event| {
            web_sys::console::log_1(&format!("Web backend: Event type: {}", event.type_()).into());

            let target = event.target().unwrap();
            let characteristic: BluetoothRemoteGattCharacteristic =
                target.clone().dyn_into().unwrap();

            // Try multiple approaches to get the notification data
            web_sys::console::log_1(
                &"Web backend: Trying different approaches to get notification data".into(),
            );

            // Log characteristic details
            web_sys::console::log_1(
                &format!(
                    "Web backend: Characteristic UUID: {}",
                    characteristic.uuid()
                )
                .into(),
            );
            let service = characteristic.service();
            web_sys::console::log_1(
                &format!("Web backend: Service UUID: {}", service.uuid()).into(),
            );

            // Check if the characteristic actually has a value
            web_sys::console::log_1(
                &format!(
                    "Web backend: Characteristic has value: {}",
                    characteristic.value().is_some()
                )
                .into(),
            );

            // Approach 1: Try to get the data from the event itself
            // Some browsers might put the data in the event.detail or other properties
            if let Ok(detail) = js_sys::Reflect::get(&event, &"detail".into()) {
                if !detail.is_undefined() && !detail.is_null() {
                    web_sys::console::log_1(
                        &format!("Web backend: Event has detail property: {:?}", detail).into(),
                    );
                }
            }

            // Approach 2: Check if the event has a 'value' property directly
            if let Ok(event_value) = js_sys::Reflect::get(&event, &"value".into()) {
                if !event_value.is_undefined() && !event_value.is_null() {
                    web_sys::console::log_1(
                        &format!("Web backend: Event has value property: {:?}", event_value).into(),
                    );
                }
            }

            // Approach 3: Try casting the event to a more specific type
            // Check if it's actually a BluetoothCharacteristicValueChangedEvent
            if let Ok(specific_event) = event.dyn_into::<web_sys::Event>() {
                web_sys::console::log_1(&"Web backend: Successfully cast to specific event".into());
                // Log all enumerable properties of the event
                let object: &js_sys::Object = specific_event.unchecked_ref();
                let keys = js_sys::Object::keys(object);
                web_sys::console::log_1(&format!("Web backend: Event keys: {:?}", keys).into());
            }

            // Approach 4: Get the value from the characteristic target directly
            let value_from_target = characteristic.value();

            if let Some(value) = value_from_target {
                // The value is already a DataView
                let data_view_length = value.byte_length() as usize;
                web_sys::console::log_1(
                    &format!("Web backend: DataView byte length: {}", data_view_length).into(),
                );

                // Try to get underlying ArrayBuffer and create Uint8Array
                let array_buffer = value.buffer();
                let uint8_array = js_sys::Uint8Array::new(&array_buffer);
                let buffer_length = uint8_array.length() as usize;

                web_sys::console::log_1(
                    &format!(
                        "Web backend: Got notification, Uint8Array buffer length: {}",
                        buffer_length
                    )
                    .into(),
                );
                web_sys::console::log_1(
                    &format!(
                        "Web backend: ArrayBuffer byte length: {}",
                        array_buffer.byte_length()
                    )
                    .into(),
                );

                if buffer_length > 0 || data_view_length > 0 {
                    let actual_length = std::cmp::max(buffer_length, data_view_length);
                    let mut buffer = vec![0u8; actual_length];

                    if buffer_length > 0 {
                        uint8_array.copy_to(&mut buffer);
                        web_sys::console::log_1(
                            &format!("Web backend: Data via Uint8Array: {:02x?}", buffer).into(),
                        );
                    } else if data_view_length > 0 {
                        // Read byte by byte using DataView
                        for i in 0..data_view_length {
                            let byte = value.get_uint8(i);
                            buffer[i] = byte;
                        }
                        web_sys::console::log_1(
                            &format!("Web backend: Data via DataView: {:02x?}", buffer).into(),
                        );
                    }

                    web_sys::console::log_1(
                        &format!(
                            "Web backend: About to send {} bytes to backend bus: {:02x?}",
                            buffer.len(),
                            buffer
                        )
                        .into(),
                    );
                    let _ = backend_bus.send(BackendEvent::GattCharacteristicNotify {
                        peripheral_handle: p_handle,
                        service_handle: s_handle,
                        characteristic_handle: c_handle,
                        value: buffer,
                    });
                    web_sys::console::log_1(
                        &"Web backend: Notification sent to backend bus".into(),
                    );
                } else {
                    web_sys::console::log_1(
                        &"Web backend: Buffer length is 0 - trying alternative approaches".into(),
                    );

                    // Approach 5: Try to access the ArrayBuffer directly from the event
                    let mut found_data = false;

                    // Check if the event target has a different value property
                    if let Ok(target_value) = js_sys::Reflect::get(&target, &"value".into()) {
                        if target_value.is_instance_of::<js_sys::ArrayBuffer>() {
                            web_sys::console::log_1(
                                &"Web backend: Found ArrayBuffer in target.value".into(),
                            );
                            let array_buffer: js_sys::ArrayBuffer = target_value.into();
                            let uint8_array = js_sys::Uint8Array::new(&array_buffer);
                            let buffer_length = uint8_array.length() as usize;

                            if buffer_length > 0 {
                                let mut buffer = vec![0u8; buffer_length];
                                uint8_array.copy_to(&mut buffer);

                                web_sys::console::log_1(&format!("Web backend: Found data via target.value: {} bytes: {:02x?}", buffer.len(), buffer).into());

                                let _ = backend_bus.send(BackendEvent::GattCharacteristicNotify {
                                    peripheral_handle: p_handle,
                                    service_handle: s_handle,
                                    characteristic_handle: c_handle,
                                    value: buffer,
                                });
                                found_data = true;
                            }
                        }
                    }

                    if !found_data {
                        // Approach 6: Try manual read with a small delay
                        let read_promise = characteristic.read_value();
                        let backend_bus_clone = backend_bus.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            // Add a small delay before reading
                            let delay_promise = js_sys::Promise::resolve(&JsValue::from(1));
                            let _ = wasm_bindgen_futures::JsFuture::from(delay_promise).await;

                            match wasm_bindgen_futures::JsFuture::from(read_promise).await {
                                Ok(value_js) => {
                                    let array_buffer: js_sys::ArrayBuffer = value_js.into();
                                    let uint8_array = js_sys::Uint8Array::new(&array_buffer);
                                    let mut buffer = vec![0u8; uint8_array.length() as usize];
                                    uint8_array.copy_to(&mut buffer);

                                    web_sys::console::log_1(
                                        &format!(
                                            "Web backend: Manual read got {} bytes: {:02x?}",
                                            buffer.len(),
                                            buffer
                                        )
                                        .into(),
                                    );

                                    if !buffer.is_empty() {
                                        let _ = backend_bus_clone.send(
                                            BackendEvent::GattCharacteristicNotify {
                                                peripheral_handle: p_handle,
                                                service_handle: s_handle,
                                                characteristic_handle: c_handle,
                                                value: buffer,
                                            },
                                        );
                                    }
                                }
                                Err(e) => {
                                    web_sys::console::log_1(
                                        &format!("Web backend: Manual read failed: {:?}", e).into(),
                                    );
                                }
                            }
                        });
                    }
                }
            } else {
                web_sys::console::log_1(
                    &"Web backend: No value from characteristic.value()".into(),
                );
            }
        }) as Box<dyn FnMut(_)>);

        // Add debugging before event listener attachment
        web_sys::console::log_1(&"Web backend: About to add event listener".into());

        match characteristic.add_event_listener_with_callback(
            "characteristicvaluechanged",
            closure.as_ref().unchecked_ref(),
        ) {
            Ok(()) => {
                web_sys::console::log_1(&"Web backend: Event listener added successfully".into())
            }
            Err(e) => web_sys::console::log_1(
                &format!("Web backend: Failed to add event listener: {:?}", e).into(),
            ),
        }

        closure.forget(); // Keep the closure alive

        web_sys::console::log_1(&"Web backend: About to start notifications".into());
        let start_promise = characteristic.start_notifications();
        match JsFuture::from(start_promise).await {
            Ok(_) => {
                web_sys::console::log_1(
                    &"Web backend: start_notifications() completed successfully".into(),
                );

                // Verify that notifications are actually started
                web_sys::console::log_1(
                    &"Web backend: Checking if notifications are active...".into(),
                );

                // Let's also try to trigger a test read to see if the characteristic is working
                web_sys::console::log_1(
                    &"Web backend: Attempting test read of characteristic".into(),
                );
                let test_read_promise = characteristic.read_value();
                match JsFuture::from(test_read_promise).await {
                    Ok(value_js) => {
                        let array_buffer: js_sys::ArrayBuffer = value_js.into();
                        let uint8_array = js_sys::Uint8Array::new(&array_buffer);
                        let mut test_buffer = vec![0u8; uint8_array.length() as usize];
                        uint8_array.copy_to(&mut test_buffer);
                        web_sys::console::log_1(
                            &format!(
                                "Web backend: Test read successful, {} bytes: {:02x?}",
                                test_buffer.len(),
                                test_buffer
                            )
                            .into(),
                        );
                    }
                    Err(e) => {
                        web_sys::console::log_1(
                            &format!("Web backend: Test read failed: {:?}", e).into(),
                        );
                    }
                }
            }
            Err(e) => {
                web_sys::console::log_1(
                    &format!("Web backend: start_notifications() failed: {:?}", e).into(),
                );
                return Err(e.into());
            }
        }

        Ok(())
    }

    async fn gatt_characteristic_unsubscribe(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
    ) -> Result<()> {
        let device_ref = self
            .inner
            .devices
            .get(&peripheral_handle)
            .ok_or(Error::InvalidStateReference)?;

        let characteristic = device_ref
            .characteristics
            .get(&characteristic_handle)
            .ok_or(Error::InvalidStateReference)?;

        let stop_promise = characteristic.stop_notifications();
        JsFuture::from(stop_promise).await?;

        Ok(())
    }

    async fn gatt_characteristic_discover_descriptors(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle,
    ) -> Result<()> {
        let mut device_ref = self
            .inner
            .devices
            .get_mut(&peripheral_handle)
            .ok_or(Error::InvalidStateReference)?;

        let characteristic = device_ref
            .characteristics
            .get(&characteristic_handle)
            .ok_or(Error::InvalidStateReference)?;

        let descriptors_promise = characteristic.get_descriptors();
        let descriptors_js = JsFuture::from(descriptors_promise).await?;
        let descriptors_array: js_sys::Array = descriptors_js.into();

        for i in 0..descriptors_array.length() {
            let descriptor_js = descriptors_array.get(i);
            let descriptor: BluetoothRemoteGattDescriptor = descriptor_js.into();
            let descriptor_handle = DescriptorHandle(
                self.inner
                    .next_descriptor_handle
                    .fetch_add(1, Ordering::SeqCst),
            );

            let uuid_str = descriptor.uuid();
            let uuid = Uuid::parse_str(&uuid_str).unwrap_or_default();

            device_ref.descriptors.insert(descriptor_handle, descriptor);

            let _ = self.inner.backend_bus.send(BackendEvent::GattDescriptor {
                peripheral_handle,
                service_handle,
                characteristic_handle,
                descriptor_handle,
                uuid,
            });
        }

        let _ = self
            .inner
            .backend_bus
            .send(BackendEvent::GattDescriptorsComplete {
                peripheral_handle,
                service_handle,
                characteristic_handle,
                error: None,
            });

        Ok(())
    }

    async fn gatt_descriptor_read(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, descriptor_handle: DescriptorHandle,
        cache_mode: crate::CacheMode,
    ) -> Result<Vec<u8>> {
        let device_ref = self
            .inner
            .devices
            .get(&peripheral_handle)
            .ok_or(Error::InvalidStateReference)?;

        let descriptor = device_ref
            .descriptors
            .get(&descriptor_handle)
            .ok_or(Error::InvalidStateReference)?;

        let read_promise = descriptor.read_value();
        let value_js = JsFuture::from(read_promise).await?;

        // Convert ArrayBuffer to Vec<u8>
        let array_buffer: js_sys::ArrayBuffer = value_js.into();
        let uint8_array = js_sys::Uint8Array::new(&array_buffer);
        let mut buffer = vec![0u8; uint8_array.length() as usize];
        uint8_array.copy_to(&mut buffer);

        Ok(buffer)
    }

    async fn gatt_descriptor_write(
        &self, peripheral_handle: PeripheralHandle, service_handle: ServiceHandle,
        characteristic_handle: CharacteristicHandle, descriptor_handle: DescriptorHandle,
        data: &[u8],
    ) -> Result<()> {
        let device_ref = self
            .inner
            .devices
            .get(&peripheral_handle)
            .ok_or(Error::InvalidStateReference)?;

        let descriptor = device_ref
            .descriptors
            .get(&descriptor_handle)
            .ok_or(Error::InvalidStateReference)?;

        let mut data_slice = data.to_vec();

        let write_promise = descriptor.write_value_with_u8_array(&mut data_slice);
        JsFuture::from(write_promise).await?;

        Ok(())
    }
    //fn gatt_service_uuid(&self, peripheral_handle: PeripheralHandle,
    //                     service_handle: ServiceHandle)
    //                     -> Result<uuid::Uuid> {
    //    todo!()
    //}

    //fn gatt_characteristic_uuid(&self, peripheral_handle: PeripheralHandle,
    //                            characteristic_handle: CharacteristicHandle)
    //                            -> Result<uuid::Uuid> {
    //    todo!()
    //}

    fn flush(&self, id: u32) -> Result<()> {
        let _ = self.inner.backend_bus.send(BackendEvent::Flush(id));
        Ok(())
    }
}
