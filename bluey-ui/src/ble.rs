use log::Level;
use log::{error, warn, info, debug, trace};

use std::thread;
use winit::event_loop::EventLoopProxy;
use crate::tokio_runtime::*;
use crate::ui;

use futures::FutureExt;
//use futures::stream::StreamExt;
use std::pin::Pin;
use std::time::Duration;
use tokio_stream::wrappers::{IntervalStream, UnboundedReceiverStream};
use tokio_stream::{Stream, StreamExt, StreamMap};
use uuid::Uuid;


use bluey::uuid::uuid_from_u16;
use bluey::{
    self, descriptor::Descriptor, characteristic::Characteristic, peripheral::Peripheral, service::Service,
    PeripheralPropertyId,
};
use bluey::{characteristic, session};

#[derive(Debug, Clone)]
pub enum BleRequest {
    StartScanning,
    StopScanning,
    Connect(Peripheral),
    Disconnect(Peripheral),
    DiscoverGattServices(Peripheral),
    DiscoverGattIncludes(Service),
    DiscoverGattCharacteristics(Service),
    DiscoverGattDescriptors(Characteristic),
    ReadGattCharacteristic(Characteristic),
    SubscribeGattCharacteristic(Characteristic),
    UnsubscribeGattCharacteristic(Characteristic),
    ReadGattDescriptor(Descriptor),
}


#[derive(PartialEq, Eq, Hash, Clone, Copy)]
enum EventSource {
    Bluetooth,
    Poll,      // One second poll for re-trying connect until connected
    Waker,     // On-demand state updates
    UI,        // Requests from UI
}

#[derive(Debug, Clone)]
enum Event {
    BtEvent(bluey::Event),
    UiRequest(BleRequest),
    Update,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum HrmState {
    Connecting,
    WaitingForCharacteristics,
    Monitoring,
}

const HEART_RATE_SERVICE_UUID: Uuid = uuid_from_u16(0x180D);
const HEART_RATE_MEASUREMENT_CHARACTERISTIC_UUID: Uuid = uuid_from_u16(0x2A37);

fn create_poll_stream() -> Pin<Box<dyn Stream<Item = Event> + Send>> {
    let interval = {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval
    };
    let poll_stream = IntervalStream::new(interval);

    Box::pin(poll_stream.map(|_i| Event::Update))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Scanning,
    Connected,
    Connecting
}

pub struct BleService {
    service_handle: thread::JoinHandle<anyhow::Result<()>>,
    //session: bluey::session::Session
    //ble_task_handle: tokio::task::JoinHandle<anyhow::Result<()>>
}

struct ServiceImpl {
    state: State,
}

impl ServiceImpl {
     fn new() -> Self {
         Self {
             state: State::Idle
         }
     }

}

impl BleService {
    pub fn new(event_proxy: EventLoopProxy<crate::ui::Event>,
               ui_requests: tokio::sync::mpsc::UnboundedReceiver<BleRequest>,
               companion_chooser_request_code: Option<u32>) -> Self {

        let runtime = build_tokio_runtime().unwrap();

        let service_handle = thread::spawn(move || {
            let _guard = runtime.enter();
            let result = runtime.block_on(async move {
                BleService::bluetooth_service(event_proxy, ui_requests, companion_chooser_request_code).await
            });
            warn!("Bluetooth Service Finished: {:?}", result);
            result
        });

        BleService {
            service_handle
        }
    }

    // XXX: ideally this should probably be run as a proper Android Service
    pub async fn bluetooth_service(event_proxy: EventLoopProxy<crate::ui::Event>,
        ui_requests: tokio::sync::mpsc::UnboundedReceiver<BleRequest>,
        companion_chooser_request_code: Option<u32>) -> anyhow::Result<()> {

        #[cfg(target_os="android")]
        let session = {
            let ctx = ndk_context::android_context();
            let jvm_ptr = ctx.vm();
            let jvm = unsafe { jni::JavaVM::from_raw(jvm_ptr.cast()).expect("Expected to find JVM via ndk_context crate") };
            let activity_ptr = ctx.context();
            //let activity = ;
            let activity = jni::objects::JObject::from(activity_ptr as jni::sys::jobject);
            //let activity = env.new_global_ref()?;
            let env = jvm.attach_current_thread_permanently().unwrap();

            //env: jni::JNIEnv<'a>,
            //                        activity: jni::objects::JObject<'a>,
            //                        companion_chooser_request_code: Option<u32>) -> Result<BluetoothSession, Box<dyn std::error::Error>> {
            //let android_config = session::AndroidConfig::new(env, context);
            let config = session::SessionConfig::android_new(env, activity, companion_chooser_request_code);
            config.start().await?
        };

        #[cfg(not(target_os="android"))]
        let session = bluey::session::SessionConfig::new().start().await?;

        //let scanner = session.start_scanning(filter)
        //let scanner = bluetooth_scanner_new(runtime, session);
        let events = session.events()?;


        let mut mainloop = StreamMap::new();

        let bt_event_stream: Pin<Box<dyn Stream<Item = Event>>> =
            Box::pin(events.map(|bt_event| Event::BtEvent(bt_event)));
        mainloop.insert(EventSource::Bluetooth, bt_event_stream);

        let ui_request_stream = tokio_stream::wrappers::UnboundedReceiverStream::new(ui_requests);
        let ui_request_stream: Pin<Box<dyn Stream<Item = Event>>> =
            Box::pin(ui_request_stream.map(|req| Event::UiRequest(req)));
        mainloop.insert(EventSource::UI, ui_request_stream);

        // This channel gives us a way to immediately queue events for ourself from within the loop
        // (e.g. to trigger updates for state transitions without needing to wait)
        let (wake_tx, wake_rx) = tokio::sync::mpsc::unbounded_channel();
        let wake_rx_stream = UnboundedReceiverStream::new(wake_rx);
        let wake_rx_events: Pin<Box<dyn Stream<Item = Event> + Send>> = Box::pin(wake_rx_stream);
        mainloop.insert(EventSource::Waker, wake_rx_events);

        // While we're waiting to connect and fetch device characteristics we enable
        // periodic updates for polling...
        let poll_events = create_poll_stream();
        mainloop.insert(EventSource::Poll, poll_events);

        let mut hr_monitor = None;
        let mut hr_service: Option<Service> = None;
        let mut hr_characteristic: Option<Characteristic> = None;

        let mut state = State::Idle;

        // Scan for a heart rate monitor to connect to...
        //
        while let Some((_, event)) = mainloop.next().await {
            match event {
                Event::UiRequest(req) => {
                    match req {
                        BleRequest::StartScanning => {
                            match state {
                                Idle => {
                                    let filter = session::Filter::new();
                                    trace!("Starting scanning...");
                                    session.start_scanning(filter).await?; // FIXME: don't quit service on error
                                    state = State::Scanning;
                                }
                                _ => {
                                    error!("Can only start scanning when idle");
                                }
                            }
                        }
                        BleRequest::StopScanning => {
                            match state {
                                Scanning => {
                                    trace!("Stopping scanning...");
                                    session.stop_scanning().await?; // FIXME: don't quit service on error
                                    state = State::Idle;
                                }
                                _ => {
                                    trace!("Ignoring redundant request to stop scanning");
                                }
                            }
                        }
                        BleRequest::Connect(peripheral) => {
                            debug!("Ble: request connect...");
                            if state == State::Scanning {
                                session.stop_scanning().await?; // FIXME: don't quit service on error
                                state = State::Idle;
                            }

                            match peripheral.connect().await {
                                Ok(()) => {
                                    state = State::Connecting;
                                },
                                Err(err) => {
                                    let _ = event_proxy.send_event(ui::Event::ShowText(Level::Error, format!("Failed to connect: {:#?}", err)));
                                }
                            }
                        }
                        BleRequest::Disconnect(peripheral) => {
                            trace!("Disconnecting...");
                            peripheral.disconnect().await?; // FIXME: don't quit service on error

                            state = State::Idle;
                        }
                        BleRequest::DiscoverGattServices(peripheral) => {
                            if let Err(err) = peripheral.discover_services(None).await {
                                let _ = event_proxy.send_event(ui::Event::ShowText(Level::Error, format!("Couldn't initiate service discovery: {:#?}", err)));
                            }
                        }
                        BleRequest::DiscoverGattIncludes(service) => {
                            if let Err(err) = service.discover_included_services().await {
                                let _ = event_proxy.send_event(ui::Event::ShowText(Level::Error, format!("Couldn't initiate service includes discovery: {:#?}", err)));
                            }
                        }
                        BleRequest::DiscoverGattCharacteristics(service) => {
                            if let Err(err) = service.discover_characteristics().await {
                                let _ = event_proxy.send_event(ui::Event::ShowText(Level::Error, format!("Couldn't initiate characteristic discovery: {:#?}", err)));
                            }
                        }
                        BleRequest::DiscoverGattDescriptors(characteristic) => {
                            debug!("Calling characteristic.discover_descriptors()... (c={characteristic:?}");
                            if let Err(err) = characteristic.discover_descriptors().await {
                                let _ = event_proxy.send_event(ui::Event::ShowText(Level::Error, format!("Couldn't initiate descriptor discovery: {:#?}", err)));
                            }
                        }
                        BleRequest::ReadGattCharacteristic(characteristic) => {
                            match characteristic.read_value(bluey::CacheMode::Uncached).await {
                                Ok(value) => {
                                    debug!("Read Gatt Characteristic: {value:?}");
                                    let _ = event_proxy.send_event(ui::Event::UpdateCharacteristicValue(characteristic, value));
                                }
                                Err(err) => {
                                    let _ = event_proxy.send_event(ui::Event::ShowText(Level::Error, format!("Failed to read characteristic: {err:#?}")));
                                }
                            }
                        }
                        BleRequest::SubscribeGattCharacteristic(characteristic) => {
                            debug!("Subscribing to characteristic");
                            if let Err(err) = characteristic.subscribe().await {
                                let _ = event_proxy.send_event(ui::Event::ShowText(Level::Error, format!("Failed to subscribe to characteristic: {err:#?}")));
                            }
                        }
                        BleRequest::UnsubscribeGattCharacteristic(characteristic) => {
                            debug!("Unsubscribing to characteristic");
                            if let Err(err) = characteristic.unsubscribe().await {
                                let _ = event_proxy.send_event(ui::Event::ShowText(Level::Error, format!("Failed to unsubscribe from characteristic: {err:#?}")));
                            }
                        }
                        BleRequest::ReadGattDescriptor(descriptor) => {
                            match descriptor.read_value(bluey::CacheMode::Uncached).await {
                                Ok(value) => {
                                    debug!("Read Gatt Descriptor: {value:?}");
                                    let _ = event_proxy.send_event(ui::Event::UpdateDescriptorValue(descriptor, value));
                                }
                                Err(err) => {
                                    let _ = event_proxy.send_event(ui::Event::ShowText(Level::Error, format!("Failed to read descriptor: {err:#?}")));
                                }
                            }
                        }
                    }
                }
                Event::Update => {

                }
                Event::BtEvent(event) => {
                    match &event {
                        bluey::Event::PeripheralFound { peripheral, address, name, .. } => {
                            info!("Discovered peripheral: {} / {}", name, address.to_string());
                        }
                        bluey::Event::PeripheralPropertyChanged { peripheral,
                                                                property_id,
                                                                .. } => {
                            if *property_id == PeripheralPropertyId::ServiceIds {
                                info!("Got notified of new service IDs: {:?}",
                                        peripheral.service_ids());
                            }
                            if hr_monitor.is_none() && *property_id == bluey::PeripheralPropertyId::ServiceIds
                            {
                                if peripheral.has_service_id(HEART_RATE_SERVICE_UUID) {
                                    info!("Found heart rate monitor");
                                    hr_monitor = Some(peripheral.clone());
                                } else {
                                    info!("Not found heart rate monitor yet");
                                }
                            }
                        }
                        /*
                        bluey::Event::PeripheralPrimaryGattServicesComplete { peripheral, .. } => {
                            let services = peripheral.primary_services();
                            for service in services.iter() {
                                if let Err(err) = service.discover_characteristics().await {
                                    let _ = event_proxy.send_event(ui::Event::ShowText(Level::Error, format!("Couldn't initiate characteristic discovery: {:?}", err)));
                                    break;
                                }
                                if let Err(err) = service.discover_included_services().await {
                                    let _ = event_proxy.send_event(ui::Event::ShowText(Level::Error, format!("Couldn't initiate included service discovery: {:?}", err)));
                                    break;
                                }
                            }
                        }
                        bluey::Event::ServiceIncludedGattServicesComplete { peripheral, service, .. } => {
                            let services = service.included_services().unwrap();
                            for service in services.iter() {
                                if let Err(err) = service.discover_characteristics().await {
                                    let _ = event_proxy.send_event(ui::Event::ShowText(Level::Error, format!("Couldn't initiate characteristic discovery: {:?}", err)));
                                    break;
                                }
                                if let Err(err) = service.discover_included_services().await {
                                    let _ = event_proxy.send_event(ui::Event::ShowText(Level::Error, format!("Couldn't initiate included service discovery: {:?}", err)));
                                    break;
                                }
                            }
                        }
                        bluey::Event::ServiceGattCharacteristicsComplete { peripheral, service, .. } => {
                            let characteristics = service.characteristics().unwrap();
                            for characteristic in characteristics.iter() {
                                if let Err(err) = characteristic.discover_descriptors().await {
                                    let _ = event_proxy.send_event(ui::Event::ShowText(Level::Error, format!("Couldn't initiate descriptor discovery: {:?}", err)));
                                    break;
                                }
                            }
                        }
                        */
                        _ => {
                            trace!("EVENT: {:?}", event);
                        }
                    }

                    let _ = event_proxy.send_event(ui::Event::Ble(event));
                },
                _ => {
                    error!("Unhandled event");
                }
            }
        }



        Ok(())
    }

}