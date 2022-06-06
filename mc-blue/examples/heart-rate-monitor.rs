#![cfg(not(target_os="android"))]

use bmb::uuid::uuid_from_u16;
use bmb::{
    self, characteristic::Characteristic, peripheral::Peripheral, service::Service,
    PeripheralPropertyId,
};
use bmb::{characteristic, session};
use futures::FutureExt;
use log::{info, trace, warn};
use std::pin::Pin;
use std::time::Duration;
use tokio::signal;
use tokio_stream::wrappers::{IntervalStream, UnboundedReceiverStream};
use tokio_stream::{Stream, StreamExt, StreamMap};
use uuid::Uuid;

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
enum EventSource {
    Bluetooth,
    Interrupt, // Ctrl-C handling
    Poll,      // One second poll for re-trying connect until connected
    Waker,     // On-demand state updates
}

#[derive(Debug, Clone)]
enum Event {
    BtEvent(bmb::Event),
    Interrupt,
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::builder().filter_level(log::LevelFilter::Warn) // Default Log Level
                         .parse_default_env()
                         .format(pretty_env_logger::formatter)
                         .init();

    let session = session::SessionConfig::new().start().await?;
    let events = session.events()?;

    let filter = session::Filter::new();
    session.start_scanning(filter).await?;

    let mut mainloop = StreamMap::new();

    let ctrl_c_stream: Pin<Box<dyn Stream<Item = Event>>> =
        Box::pin(signal::ctrl_c().into_stream().map(|_| {
                                                   println!("Ctrl-C");
                                                   Event::Interrupt
                                               }));
    mainloop.insert(EventSource::Interrupt, ctrl_c_stream);

    let bt_event_stream: Pin<Box<dyn Stream<Item = Event>>> =
        Box::pin(events.map(|bt_event| Event::BtEvent(bt_event)));
    mainloop.insert(EventSource::Bluetooth, bt_event_stream);

    let mut hr_monitor = None;
    let mut hr_service: Option<Service> = None;
    let mut hr_characteristic: Option<Characteristic> = None;

    // Scan for a heart rate monitor to connect to...
    //
    while let Some((_, event)) = mainloop.next().await {
        match event {
            Event::BtEvent(event) => match event {
                bmb::Event::PeripheralFound { peripheral: _, address, name, .. } => {
                    println!("Discovered peripheral: {} / {}", name, address.to_string());
                }
                bmb::Event::PeripheralPropertyChanged { peripheral,
                                                        property_id,
                                                        .. } => {
                    if property_id == PeripheralPropertyId::ServiceIds {
                        println!("Got notified of new service IDs: {:?}",
                                 peripheral.service_ids());
                    }
                    if hr_monitor.is_none() && property_id == bmb::PeripheralPropertyId::ServiceIds
                    {
                        if peripheral.has_service_id(HEART_RATE_SERVICE_UUID) {
                            println!("Found heart rate monitor");
                            hr_monitor = Some(peripheral);
                            break;
                        } else {
                            println!("Not found heart rate monitor yet");
                        }
                    }
                }
                _ => {
                    println!("EVENT: {:?}", &event);
                }
            },
            Event::Interrupt => {
                println!("Interrupt received!");
                break;
            }
            _ => {
                println!("Unknown event");
            }
        }
    }

    session.stop_scanning().await?;

    if let Some(hr_monitor) = hr_monitor {
        // Lets switch to a filtered event stream now to discard any peripheral events
        // we don't care about...
        mainloop.remove(&EventSource::Bluetooth);
        let events = session.peripheral_events(&hr_monitor)?;
        let bt_event_stream: Pin<Box<dyn Stream<Item = Event>>> =
            Box::pin(events.map(|bt_event| Event::BtEvent(bt_event)));
        mainloop.insert(EventSource::Bluetooth, bt_event_stream);

        // While we're waiting to connect and fetch device characteristics we enable
        // periodic updates for polling...
        let poll_events = create_poll_stream();
        mainloop.insert(EventSource::Poll, poll_events);

        // This channel gives us a way to immediately queue events for ourself from within the loop
        // (e.g. to trigger updates for state transitions without needing to wait)
        let (wake_tx, wake_rx) = tokio::sync::mpsc::unbounded_channel();
        let wake_rx_stream = UnboundedReceiverStream::new(wake_rx);
        let wake_rx_events: Pin<Box<dyn Stream<Item = Event> + Send>> = Box::pin(wake_rx_stream);
        mainloop.insert(EventSource::Waker, wake_rx_events);

        let mut state = HrmState::Connecting;

        // Monitoring loop
        while let Some((_, event)) = mainloop.next().await {
            match event {
                Event::Update => {
                    match state {
                        HrmState::Connecting => {
                            println!("Connecting to heart rate monitor {} ...",
                                     hr_monitor.address());
                            match hr_monitor.connect().await {
                                Ok(()) => {
                                    println!("Connected to heart rate monitor");
                                    state = HrmState::WaitingForCharacteristics;
                                    let _ = wake_tx.send(Event::Update); // Don't wait 1 second for the next update
                                }
                                Err(err) => {
                                    println!("Failed to connect to heart rate monitor: {:?}", err);
                                }
                            }
                        }
                        HrmState::WaitingForCharacteristics => {
                            if let Some(hr_service) = &hr_service {
                                if let Some(hr_characteristic) = &hr_characteristic {
                                    println!("Subscribing to characteristic notifications...");

                                    if let Err(err) = hr_characteristic.subscribe().await {
                                        println!("Failed to subscribe to characteristic notifications: {:?}", err);
                                    } else {
                                        state = HrmState::Monitoring;
                                        let _ = wake_tx.send(Event::Update); // Don't wait 1 second for the next update
                                    }
                                } else {
                                    println!("Initiating Gatt characteristic discovery...");
                                    if let Err(err) = hr_service.discover_characteristics().await {
                                        println!("Failed to initiate discovery of service characteristics: {:?}", err)
                                    }
                                }
                            } else {
                                // Although we know the device supports the service we want, we
                                // still need to discover a specific instance of that service ...
                                println!("Initiating Gatt service discovery...");
                                if let Err(err) = hr_monitor.discover_services(None).await {
                                    println!(
                                        "Failed to initiate discovery of peripheral services: {:?}",
                                        err
                                    )
                                }
                            }
                        }
                        HrmState::Monitoring => {
                            if mainloop.contains_key(&EventSource::Poll) {
                                mainloop.remove(&EventSource::Poll);
                            }
                        }
                    }
                }
                Event::BtEvent(event) => {
                    match event {
                        bmb::Event::PeripheralPrimaryGattService { service, uuid, .. } => {
                            if uuid == HEART_RATE_SERVICE_UUID {
                                println!("Discovered Heart Rate service");
                                hr_service = Some(service);
                                let _ = wake_tx.send(Event::Update); // Don't wait 1 second for the next update
                            }
                        }
                        bmb::Event::ServiceGattCharacteristic { characteristic,
                                                                uuid,
                                                                .. } => {
                            if uuid == HEART_RATE_MEASUREMENT_CHARACTERISTIC_UUID {
                                println!("Discovered Heart Rate Measurement characteristic");
                                hr_characteristic = Some(characteristic);
                                let _ = wake_tx.send(Event::Update); // Don't wait 1 second for the next update
                            }
                        }
                        bmb::Event::PeripheralDisconnected { .. } => {
                            println!("Heart Rate Monitor Disconnected!");
                            // All GATT state becomes invalid after a disconnect...
                            hr_service = None;
                            hr_characteristic = None;

                            state = HrmState::Connecting;
                            if !mainloop.contains_key(&EventSource::Poll) {
                                mainloop.insert(EventSource::Poll, create_poll_stream());
                            }
                        }
                        bmb::Event::ServiceGattCharacteristicValueNotify { characteristic,
                                                                           value,
                                                                           .. } => {
                            match &hr_characteristic {
                                Some(hr_characteristic) if hr_characteristic == &characteristic => {
                                    let data = value;
                                    println!("HR: Notify! {:?}", &data);
                                    let mut u16_format = false;
                                    let mut rr_start = 2;
                                    if data[0] & 0x1 == 0x1 {
                                        u16_format = true;
                                        println!("> Format = UINT16");
                                        rr_start += 1;
                                    } else {
                                        println!("> Format = UINT8");
                                    }
                                    if data[0] & 0x3 == 0x3 {
                                        println!("> Contact detection: SUPPORTED");
                                        if data[0] & 0x2 == 0x2 {
                                            println!(">> Contact status: IN-CONTACT");
                                        } else {
                                            println!(">> Contact status: NOT IN-CONTACT");
                                        }
                                    } else {
                                        println!("> Contact detection: NOT SUPPORTED");
                                        if data[0] & 0x2 == 0x2 {
                                            println!(">> Contact status: DEFAULT = IN-CONTACT");
                                        } else {
                                            println!(">> Contact status: SPURIOUS: NOT IN-CONTACT");
                                        }
                                    }
                                    if data[0] & 0x8 == 0x8 {
                                        println!("> Energy Expenditure: PRESENT");
                                        rr_start += 2;
                                    } else {
                                        println!("> Energy Expenditure: NOT PRESENT");
                                    }
                                    if data[0] & 0x10 == 0x10 {
                                        println!("> RR: PRESENT")
                                    } else {
                                        println!("> RR: NOT PRESENT")
                                    }
                                    if data[0] & 0x60 != 0 {
                                        println!("> Reserved bits set!");
                                    }
                                    let mut hr = data[1] as u16;
                                    if u16_format {
                                        hr = u16::from_le_bytes([data[1], data[2]]);
                                    }
                                    println!("> Heart Rate: {}", hr);

                                    // each RR value is 2 bytes
                                    let n_rrs = (data.len() - rr_start) / 2;
                                    for i in 0..n_rrs {
                                        let pos = rr_start + 2 * i;
                                        let rr_fixed =
                                            u16::from_le_bytes([data[pos], data[pos + 1]]);
                                        let rr_seconds: f32 = rr_fixed as f32 / 1024.0f32;
                                        println!("> RR[{}] = {}", i, rr_seconds);
                                    }
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
                Event::Interrupt => {
                    println!("Interrupt received!");
                    break;
                }
            }
        }
    }

    Ok(())
}
