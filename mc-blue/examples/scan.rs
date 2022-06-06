#![cfg(not(target_os="android"))]

use bmb;
use bmb::session;
use futures::FutureExt;
use std::io::Write;
use std::pin::Pin;
use tokio::signal;
use tokio_stream::{Stream, StreamExt, StreamMap};

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
enum EventSource {
    Bluetooth,
    Interrupt,
}

#[derive(Debug, Clone)]
enum Event {
    BtEvent(bmb::Event),
    Interrupt,
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

    while let Some((_, event)) = mainloop.next().await {
        match event {
            Event::BtEvent(event) => match event {
                bmb::Event::PeripheralFound { peripheral: _,
                                              name,
                                              address,
                                              .. } => {
                    println!("Peripheral Found: {}: {}", name, address);
                }
                bmb::Event::PeripheralPropertyChanged { peripheral,
                                                        property_id,
                                                        .. } => {
                    println!("Peripheral Property Changed: {}: {:?}",
                             peripheral.address(),
                             property_id);
                }
                _ => {
                    println!("Other Event: {:?}", &event);
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

    Ok(())
}
