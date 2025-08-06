use bluey::session::{Filter, SessionConfig};
use bluey::uuid::uuid_from_u16;
use bluey::{characteristic::Characteristic, peripheral::Peripheral, service::Service};
use futures::{pin_mut, StreamExt};
use log::{error, info};
use std::cell::RefCell;
use std::rc::Rc;
use uuid::Uuid;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::HtmlButtonElement;

const HEART_RATE_SERVICE_UUID: Uuid = uuid_from_u16(0x180D);
const HEART_RATE_MEASUREMENT_CHARACTERISTIC_UUID: Uuid = uuid_from_u16(0x2A37);

#[derive(PartialEq, Eq, Clone, Copy)]
enum HrmState {
    Idle,
    DeviceSelection,
    Connecting,
    WaitingForCharacteristics,
    Monitoring,
}

struct TestState {
    state: HrmState,
    hr_monitor: Option<Peripheral>,
    hr_service: Option<Service>,
    hr_characteristic: Option<Characteristic>,
}

impl TestState {
    fn new() -> Self {
        Self {
            state: HrmState::Idle,
            hr_monitor: None,
            hr_service: None,
            hr_characteristic: None,
        }
    }

    fn reset(&mut self) {
        self.state = HrmState::Idle;
        self.hr_monitor = None;
        self.hr_service = None;
        self.hr_characteristic = None;
    }
}

fn setup_console_logging() -> Result<(), JsValue> {
    console_log::init_with_level(log::Level::Info).unwrap();
    info!("Console logging initialized");
    Ok(())
}

fn log_to_page(message: &str) {
    let window = web_sys::window().unwrap();
    let document = window.document().unwrap();

    let log_element = document.get_element_by_id("log")
        .unwrap_or_else(|| {
            let log_div = document.create_element("div").unwrap();
            log_div.set_id("log");
            log_div.set_attribute("style", 
                "display: block; border: 1px solid #ccc; padding: 10px; margin: 10px 0; height: 300px; overflow-y: scroll; font-family: monospace; font-size: 12px; background: #f9f9f9; border-radius: 4px;"
            ).unwrap();
            document.body().unwrap().append_child(&log_div).unwrap();
            log_div
        });

    // Make sure it's visible
    log_element.set_attribute("style", 
        "display: block; border: 1px solid #ccc; padding: 10px; margin: 10px 0; height: 300px; overflow-y: scroll; font-family: monospace; font-size: 12px; background: #f9f9f9; border-radius: 4px;"
    ).unwrap();

    let time = js_sys::Date::new_0().to_iso_string();
    let time_str = time.as_string().unwrap_or_else(|| "unknown".to_string());
    let log_line = format!("[{}] {}\n", time_str, message);

    let current_content = log_element.text_content().unwrap_or_default();
    log_element.set_text_content(Some(&format!("{}{}", current_content, log_line)));

    // Auto-scroll to bottom
    log_element.set_scroll_top(log_element.scroll_height());
}

async fn start_hrm_test() -> Result<(), JsValue> {
    log_to_page("🔵 Starting Heart Rate Monitor Test...");

    let test_state = Rc::new(RefCell::new(TestState::new()));

    log_to_page("🔧 Creating session config...");

    // Create session with web backend
    let session = SessionConfig::new().start().await.map_err(|e| {
        let error_msg = format!("Failed to start session: {:?}", e);
        log_to_page(&format!("❌ {}", error_msg));
        JsValue::from_str(&error_msg)
    })?;

    log_to_page("✅ Session started");

    // Create filter for heart rate service - this is crucial for Web Bluetooth permissions
    let mut filter = Filter::new();
    filter.add_service(HEART_RATE_SERVICE_UUID);

    log_to_page("🔍 Requesting Heart Rate device selection...");
    log_to_page(&format!(
        "   - Looking for service: {}",
        HEART_RATE_SERVICE_UUID
    ));
    test_state.borrow_mut().state = HrmState::DeviceSelection;

    // Select peripheral (this will show the browser's device selection dialog)
    // Web Bluetooth will only show devices that advertise the heart rate service
    let peripheral = session.select_peripheral(filter).await.map_err(|e| {
        let error_msg = format!("Failed to select peripheral: {:?}", e);
        log_to_page(&format!("❌ {}", error_msg));
        JsValue::from_str(&error_msg)
    })?;

    log_to_page(&format!("✅ Selected device: {}", peripheral.address()));
    test_state.borrow_mut().hr_monitor = Some(peripheral.clone());
    test_state.borrow_mut().state = HrmState::Connecting;

    // Get event stream for this peripheral
    let events = session
        .peripheral_events(&peripheral)
        .map_err(|e| JsValue::from_str(&format!("Failed to get events: {:?}", e)))?;

    log_to_page("🔗 Connecting to device...");

    // Connect to the device
    peripheral.connect().await.map_err(|e| {
        let error_msg = format!("Failed to connect: {:?}", e);
        log_to_page(&format!("❌ {}", error_msg));
        JsValue::from_str(&error_msg)
    })?;

    log_to_page("✅ Connected! Waiting for connection to stabilize...");
    test_state.borrow_mut().state = HrmState::WaitingForCharacteristics;

    // Wait for the connection event to be processed before trying service discovery
    // This should be handled by the event loop below, so let's not do service discovery here

    // Process events
    let test_state_for_events = test_state.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let events = events;
        pin_mut!(events);
        while let Some(event) = events.next().await {
            match event {
                bluey::Event::PeripheralConnected { peripheral, .. } => {
                    log_to_page("🔗 Connection confirmed! Now discovering services...");

                    // Now that we're definitely connected, start service discovery
                    match peripheral
                        .discover_services(Some(vec![HEART_RATE_SERVICE_UUID]))
                        .await
                    {
                        Ok(()) => {
                            log_to_page("🔍 Service discovery initiated");
                        }
                        Err(e) => {
                            let error_msg = format!("Failed to discover services: {:?}", e);
                            log_to_page(&format!("❌ {}", error_msg));
                            log_to_page(
                                "💡 Make sure your device is advertising the Heart Rate service",
                            );
                        }
                    }
                }
                bluey::Event::PeripheralPrimaryGattService { service, uuid, .. } => {
                    if uuid == HEART_RATE_SERVICE_UUID {
                        log_to_page("💓 Discovered Heart Rate service");
                        test_state_for_events.borrow_mut().hr_service = Some(service.clone());

                        // Discover characteristics
                        if let Err(e) = service.discover_characteristics().await {
                            error!("Failed to discover characteristics: {:?}", e);
                            log_to_page(&format!("❌ Failed to discover characteristics: {:?}", e));
                        }
                    }
                }
                bluey::Event::ServiceGattCharacteristic {
                    characteristic,
                    uuid,
                    ..
                } => {
                    if uuid == HEART_RATE_MEASUREMENT_CHARACTERISTIC_UUID {
                        log_to_page("📡 Discovered Heart Rate Measurement characteristic");
                        test_state_for_events.borrow_mut().hr_characteristic =
                            Some(characteristic.clone());

                        // Subscribe to notifications
                        log_to_page("🔔 Subscribing to heart rate notifications...");
                        match characteristic.subscribe().await {
                            Ok(()) => {
                                log_to_page("✅ Subscribed! Monitoring heart rate...");
                                test_state_for_events.borrow_mut().state = HrmState::Monitoring;
                            }
                            Err(e) => {
                                error!("Failed to subscribe: {:?}", e);
                                log_to_page(&format!("❌ Failed to subscribe: {:?}", e));
                            }
                        }
                    }
                }
                bluey::Event::ServiceGattCharacteristicValueNotify {
                    characteristic,
                    value,
                    ..
                } => {
                    log_to_page(&format!(
                        "📡 Received notification from characteristic: {} bytes: {:02x?}",
                        value.len(),
                        value
                    ));

                    let is_hrm_characteristic = test_state_for_events
                        .borrow()
                        .hr_characteristic
                        .as_ref()
                        .map(|c| c == &characteristic)
                        .unwrap_or(false);

                    if is_hrm_characteristic {
                        log_to_page("❤️ This is from our heart rate characteristic!");
                        log_to_page(&format!(
                            "🔍 Frontend received exactly {} bytes: {:02x?}",
                            value.len(),
                            value
                        ));
                        parse_heart_rate_data(&value);
                    } else {
                        log_to_page("⚠️ Notification from unknown characteristic");
                    }
                }
                bluey::Event::PeripheralDisconnected { .. } => {
                    log_to_page("💔 Device disconnected!");
                    test_state_for_events.borrow_mut().reset();
                }
                _ => {
                    // Log other events for debugging
                    log_to_page(&format!("📋 Event: {:?}", event));
                }
            }
        }
    });

    Ok(())
}

fn parse_heart_rate_data(data: &[u8]) {
    log_to_page(&format!(
        "🔍 HR: Notify! {} bytes: {:02x?}",
        data.len(),
        data
    ));

    if data.is_empty() {
        log_to_page("⚠️ Empty heart rate data received");
        return;
    }

    // Ensure we have at least the first byte for flags
    if data.len() < 1 {
        log_to_page("⚠️ Heart rate data too short (no flags byte)");
        return;
    }

    let mut u16_format = false;
    let mut rr_start = 2;

    if data[0] & 0x1 == 0x1 {
        u16_format = true;
        log_to_page("> Format = UINT16");
        rr_start += 1;
    } else {
        log_to_page("> Format = UINT8");
    }

    if data[0] & 0x3 == 0x3 {
        log_to_page("> Contact detection: SUPPORTED");
        if data[0] & 0x2 == 0x2 {
            log_to_page(">> Contact status: IN-CONTACT");
        } else {
            log_to_page(">> Contact status: NOT IN-CONTACT");
        }
    } else {
        log_to_page("> Contact detection: NOT SUPPORTED");
        if data[0] & 0x2 == 0x2 {
            log_to_page(">> Contact status: DEFAULT = IN-CONTACT");
        } else {
            log_to_page(">> Contact status: SPURIOUS: NOT IN-CONTACT");
        }
    }

    if data[0] & 0x8 == 0x8 {
        log_to_page("> Energy Expenditure: PRESENT");
        rr_start += 2;
    } else {
        log_to_page("> Energy Expenditure: NOT PRESENT");
    }

    if data[0] & 0x10 == 0x10 {
        log_to_page("> RR: PRESENT")
    } else {
        log_to_page("> RR: NOT PRESENT")
    }

    if data[0] & 0x60 != 0 {
        log_to_page("> Reserved bits set!");
    }

    // Parse heart rate value with bounds checking
    if data.len() < 2 {
        log_to_page("⚠️ Heart rate data too short (no heart rate value)");
        return;
    }

    let mut hr = data[1] as u16;
    if u16_format {
        if data.len() < 3 {
            log_to_page("⚠️ Heart rate data too short for UINT16 format (need 3 bytes minimum)");
            log_to_page(&format!(
                "⚠️ Only have {} bytes, treating as UINT8",
                data.len()
            ));
            // Fall back to UINT8 format
            hr = data[1] as u16;
        } else {
            hr = u16::from_le_bytes([data[1], data[2]]);
        }
    }
    log_to_page(&format!("> Heart Rate: {}", hr));

    // Parse RR intervals with bounds checking
    if data.len() > rr_start {
        let n_rrs = (data.len() - rr_start) / 2;
        for i in 0..n_rrs {
            let pos = rr_start + 2 * i;
            if pos + 1 < data.len() {
                let rr_fixed = u16::from_le_bytes([data[pos], data[pos + 1]]);
                let rr_seconds: f32 = rr_fixed as f32 / 1024.0f32;
                log_to_page(&format!("> RR[{}] = {}", i, rr_seconds));
            } else {
                log_to_page(&format!(
                    "⚠️ Incomplete RR interval at index {}, skipping",
                    i
                ));
                break;
            }
        }
    } else {
        log_to_page("> No RR interval data");
    }
}

#[wasm_bindgen(start)]
pub fn main() {
    setup_console_logging().unwrap();

    let window = web_sys::window().unwrap();
    let document = window.document().unwrap();

    // Create and setup the test button
    let button = document
        .create_element("button")
        .unwrap()
        .dyn_into::<HtmlButtonElement>()
        .unwrap();

    button.set_inner_text("Start Heart Rate Monitor Test");
    button.set_attribute("style", 
        "padding: 10px 20px; font-size: 16px; background: #007cba; color: white; border: none; border-radius: 4px; cursor: pointer; margin: 10px;"
    ).unwrap();

    // Add click handler
    let button_clone = button.clone();
    let closure = Closure::wrap(Box::new(move |_event: web_sys::Event| {
        // First, just test that we can log
        log_to_page("🔘 Button clicked! Starting test...");
        web_sys::console::log_1(&"Button clicked from console".into());

        let button_ref = button_clone.clone();
        button_ref.set_disabled(true);
        button_ref.set_inner_text("Testing...");

        // Start the actual test
        wasm_bindgen_futures::spawn_local(async move {
            log_to_page("⚡ Async task started");

            // Simple test first - just try to create a session
            match start_hrm_test().await {
                Ok(()) => {
                    log_to_page("🎉 Test completed successfully!");
                }
                Err(e) => {
                    log_to_page(&format!("❌ Test failed: {:?}", e));
                }
            }

            button_ref.set_disabled(false);
            button_ref.set_inner_text("Start Heart Rate Monitor Test");
        });
    }) as Box<dyn FnMut(_)>);

    button
        .add_event_listener_with_callback("click", closure.as_ref().unchecked_ref())
        .unwrap();
    closure.forget();

    // Add elements to page
    document.body().unwrap().append_child(&button).unwrap();

    // Add title
    let title = document.create_element("h1").unwrap();
    title.set_inner_html("Bluey Web Bluetooth Heart Rate Monitor Test");
    title
        .set_attribute(
            "style",
            "color: #333; font-family: Arial, sans-serif; margin: 20px 10px;",
        )
        .unwrap();
    document
        .body()
        .unwrap()
        .insert_before(&title, Some(&button))
        .unwrap();

    // Add instructions
    let instructions = document.create_element("p").unwrap();
    instructions.set_inner_html(
        "Click the button below to start the heart rate monitor test. You'll need a Bluetooth LE heart rate monitor to test with."
    );
    instructions
        .set_attribute(
            "style",
            "font-family: Arial, sans-serif; margin: 10px; color: #666;",
        )
        .unwrap();
    document
        .body()
        .unwrap()
        .insert_before(&instructions, Some(&button))
        .unwrap();

    log_to_page("🚀 Bluey Web Test initialized. Click the button to start!");
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console)]
    fn log(s: &str);
}

// Re-export for convenience
pub use wasm_bindgen_futures;
pub use web_sys;
