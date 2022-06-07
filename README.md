# 🐋🦷 Bluey McBluetooth[*](#name) 🦷🐋

# Overview

Bluey is a cross-platform Rust API for accessing Bluetooth Low Energy devices.

Bluey supports scanning and connecting to GATT services and currently supports
Windows and Android (with the plan to support macOS, iOS, Linux and Web Bluetooth
too).

The general design mirrors Core Bluetooth in many ways, with an event stream
instead of delegate callbacks.

The library was originally created to help enable connectivity with health and fitness
wearables and equipment in [RealFit](http://realfit.co).

The aim is to be able to implement support for different devices against a single
API instead of needing to implement that same support repeatedly for different
platforms.

# Bluey UI

For testing the library there is a cross-platform (Egui based) application
that support scanning for peripherals, and then connecting to them and
browsing their services, characteristics and descriptors - including
reading and subscribing to characteristic values and reading descriptor
values.

![Screenshot of Bluey-UI showing heart rate monitor](https://github.com/rib/bluey/blob/android-support/bluey-ui/docs/images/bluey_ui_hrm.png)

# Features

## Scanner State

This is the state that can be discovered while scanning for peripherals (without connecting):

| State | Windows | Android |
|-------|---------|---------|
| Address | ✓ | ✓ |
| Address Type | ✓ | ✓ |
| Name | ✓ | ✓ |
| Tx Power | ✓ | ✓ |
| RSSI | ✓ | ✓ |
| Service Uuids | ✓ | ✓ |
| Service Data | ✓ |   |
| Manufacturer Data | ✓ | ✓ |

On Android Bluey optionally supports using the Companion API to select a peripheral instead of scanning.
Although the Companion API hides scanning state from the application this has the advantage of not
requiring "fine location" permissions on Android.


## GATT Features

|Feature| Windows | Android |
|-------|---------|---------|
| Discover Services | ✓ | ✓ |
| ┗ Discover Service Includes | ✓ | ✓ |
| ┗ Discover Characteristics | ✓ | ✓ |
|   ┗ Read Characteristics | ✓ | ✓ |
|   ┗ Write Characteristics | ✓ | ✓ |
|   ┗ Subscribe Characteristic Notifications | ✓ | ✓ |
|   ┗ Discover Descriptors | ✓ | ✓ |
|     ┗ Read Descriptors | ✓ | ✓ |
|     ┗ Write Descriptors | ✓ | ✓ |

_Note: Bluey also supports there being multiple instances of a service or characteristic that may
have the same Uuid, differentiated by their underlying AT handle. Where possible Bluey will also
preserve the on-device ordering of services and characteristics._

## Peripheral, Service, Characteristic and Descriptor State

| Peripheral Property | Windows | Android |
|---------------------|---------|---------|
| Address (MAC 48 or String) | ✓ | ✓ |
| Address Type | ✓ | ✓ |
| Name | ✓ | ✓ |
| Tx Power Level | ✓ | ✓ |
| Rssi | ✓ | ✓ |
| Service Uuids | ✓ | ✓ |
| (Primary) Services | ✓ | ✓ |
| (Primary + Secondary) Services | ✓ | ✓ |
| Service Data | ✓ |  |
| Manufacturer Data | ✓ | ✓ |

| Service Property | Windows | Android |
|------------------|---------|---------|
| Uuid | ✓ | ✓ |
| Included Services | ✓ | ✓ |
| Characteristics | ✓ | ✓ |

| Characteristic Property | Windows | Android |
|-------------------------|---------|---------|
| Uuid | ✓ | ✓ |
| Descriptors | ✓ | ✓ |
| Value (Read/Write/Subscribe) | ✓ | ✓ |

| Descriptor Property | Windows | Android |
|-------------------------|---------|---------|
| Uuid | ✓ | ✓ |
| Value (Read/Write) | ✓ | ✓ |

## Limitations

The API currently only supports central mode (i.e. connecting to peripherals, not
advertising peripherals)

It would be good to support peripheral mode in the future as a potential means to
help test the library via fake peripherals.

There's currently no support for L2CAP sockets.

# API

The entry point into the API is through the creation of a `Session` which is the
overall state tracker for the library.

`Peripheral`s can be found via scanning and then connecting to a `Peripheral`
enables you to discover `Service`s, `Characteristic`s and `Descriptor`s.

Each `Session` provides an `events()` stream that notifies applications of
state changes (such as discovering peripherals, or disconnecting peripherals)
and the completion of Gatt requests. This design allows for IO with devices to
complete asynchronously and is practical to integrate into the event loop of a
higher-level application.

By default, session events may relate to any number of peripherals but if it's
convenient for applications they can also create filtered streams for a single
peripheral that the application is interested in.

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

# Design

## (mostly) Stateless backends.

One of the notable things I've been aiming for with this implementation is to
keep all higher-level state tracking in common code instead of requiring
backends to handle state tracking / caching.

Backends implement a `BackendSession` trait for accepting requests from the
frontend and then all state updates are delivered to the frontend via an
internal stream of events.

This stream/event based approach should lend itself to both tracing and testing.

One future goal is to support synthetic peripherals via a "fake" backend that
make it possible to perform high-level testing of applications against a variety
of emulated devices in a way that's more practical than always needing to
test with physical hardware. To this end, it should also be possible to implement
common tracing features that make it easy to define fake test peripherals based
on real device data.

# Alternatives

## Btleplug

[btleplug](https://github.com/deviceplug/btleplug) also provides a
cross-platform Bluetooth LE Rust API.

These are a few of the technical differences:
- Session wide event stream in Bluey, which can be (optionally) filtered down to
per-peripheral events. Btleplug provides a separate event stream per-peripheral and
separate adapter events.
- Individual property getters in Bluey (for things like address, name, rssi, tx power
etc). Btleplug provides an aggregated property structure so you query (copy)
all properties at the same time.
- Bluey events can notify individual peripheral property changes.
- Bluey events directly include handles to any relevant peripheral, characteristic
or descriptor (which can be used to make API calls against) without needing to
e.g. indirectly map peripheral IDs from events to peripherals.
- Bluey is designed to support peripherals that may advertise multiple instances
of a service of characteristic with the same uuid (differentiated by their AT handle).
Btleplug notifies characteristic changes by uuid which means it wouldn't be
able to differentiate multiple instances of a characteristic.
- Btleplug supports macOS, iOS and Linux

## Servo Web Bluetooth

Although no longer developed or maintained, the Servo web browser originally implemented
various web bluetooth backends in Rust which are linked [here](https://szeged.github.io/servo/)

# Name
_The hardest part with any new project is the naming; so big thanks to the
 [team](https://www.bbc.co.uk/news/uk-36225652) that voted on our new name!_