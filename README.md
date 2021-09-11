# 🐋🦷 Bluey McBluetooth 🦷🐋

_The hardest part with any new project is the naming; so big thanks to the
[team](https://www.bbc.co.uk/news/uk-36225652) that voted on our new name!_

# ☠ WIP ☠

_This is still a work in progress._ It currently supports:
* Windows only so far
* Scanning for devices and delivering advertising data, such as service IDs,
service data, tx, rssi etc
* Basic GATT state; exporting Services => Included Services => Characteristics (It doesn't expose descriptors yet).
* Reading/Writing GATT characteristics and subscribing to notifications.

# Overview

McBlue is a cross-platform Bluetooth library, focused on Bluetooth Low Energy
peripheral support via GAP advertising packets for scanning and connected
GATT services.

For now the library is focused on enabling Windows support and I'll most likely
enable Android after that.

My main interest is in supporting access to BLE heart rate monitors in VR
(for [RealFit VR](https://realfit.co/))

Is a library _really_ cross-platform when it only actually works on one
platform?  Probably not, but at this stage I'm holding off from enabling other
platforms, besides keeping aware of some of the different platform-specific
quirks/limitations, so that it doesn't get in the way of first getting one
platform (Windows) working well. I'm generally trying to keep familiar with
Corebluetooth, Bluez, Android and Web Bluetooth APIs as I go.

# API

I tend to like libraries that provide an opinionated API that can make 99% of
real-world use cases a breeze but that also offer (clearly delineated)
lower-level entry points in case you need to poke at unusual corner cases.
So that's hopefully what I'll end up doing here.

Different to some of the other Rust Bluetooth libraries McBlue provides
a unified event stream that covers central scanning data as well as
per-peripheral updates. In my experience so far I find this is much more
convenient than having to juggle streams coming and going for different
peripherals.

When you only care about a single peripheral though the API can filter
events for you.

A simple app that starts scanning currently looks something like this:
```rust
    let session = session::SessionConfig::new().start().await?;
    let events = session.events()?;

    let filter = session::Filter::new();
    session.start_scanning(filter).await?;

    let mut mainloop = StreamMap::new();

    let ctrl_c_stream: Pin<Box<dyn Stream<Item=MyEvent>>> = Box::pin(signal::ctrl_c().into_stream().map(|_| { MyEvent::Interrupt }));
    mainloop.insert(EventSource::Interrupt, ctrl_c_stream);

    let bt_event_stream: Pin<Box<dyn Stream<Item=MyEvent>>> = Box::pin(events.map(|bt_event| MyEvent::BlueyEvent(bt_event)));
    mainloop.insert(EventSource::Bluetooth, bt_event_stream);

    while let Some((_, event)) = mainloop.next().await {
         match event {
            MyEvent::BlueyEvent(event) => {
                match event {
                    bmb::Event::PeripheralFound { peripheral, name, address, .. } => {
                        // do stuff
                    }
                    bmb::Event::PeripheralConnected(peripheral) => {
                        // do stuff
                    }
                    bmb::Event::PeripheralDisonnected(peripheral) => {
                        // do stuff
                    }
                    bmb::Event::PeripheralPropertyChanged(peripheral, property_id) => {
                        // query some changed properties...
                        let tx = peripheral.tx_power();
                    }
                    _ => {
                        // ¯\_(ツ)_/¯
                    }
                }
            }
            MyEvent::Interrupt => {
                break;
            }
        }
    }

    session.stop_scanning().await?;
```

The API currently exposes the name, address, TX power, RSSI, manufacturer data,
service data and service uuids for peripherals that can be got from the `Peripheral`
that you're passed via the event stream:

```rust
let name = peripheral.name();
let address = peripheral.address();
let tx = peripheral.tx_power();
let rssi = peripheral.rssi();
...
```

A Peripheral `Address` is either an `Address::MAC` if the platform exposes the
underlying MAC address for bluetooth devices or otherwise it's a String
(`Address::String`). In either case an `Address` can always be converted
to and from a `String` so it's possible for an application to save/serialize
an address for being able to use with the API again in the future. (E.g. so
an application can remember a user's choice of device to speed up reconnecting
again in the future).

# (mostly) Stateless backends.

One of the notable things I've been aiming for with this implementation is to
keep all higher-level state tracking in common code instead of requiring
backends to handle state tracking.

Besides having some traits for being able to issue requests to the backends
they have a very narrow interface for delivering a stream of events to the
frontend state tracker.

This stream/event based approach should lend itself to both tracing and testing.
The idea is to have a 'fake' backend which can be selected at runtime for unit
tests to be able to inject a synthetic stream of data. (otherwise
if all testing relies on testing with real hardware it's going to get really
impractical to test)

Tracing by capturing data sent from the backend can be implemented in one place
and if this data can then be replayed via the fake backend that should help
with higher-level integration testing for the state tracking code as well as
for applications - again without always needing to use real hardware.

# Alternatives

The only other (Rust) Bluetooth library I found that was supporting Windows and
aiming to be cross-platform was [btleplug](https://github.com/deviceplug/btleplug)
which is what I first started using. Although I've been contributing various
changes upstream I also ended up wanting to investigate higher level design choices
without necessarily knowing that my ideas would work out as envisioned. Definitely
check it out if need an _actually_ cross-platform Bluetooth library now!
