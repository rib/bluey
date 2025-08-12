# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Bluey is a cross-platform Rust library for Bluetooth Low Energy (BLE) communication. The API supports scanning for peripherals, connecting to GATT services, and accessing characteristics and descriptors across Windows, Android, Linux, and Web platforms.

## Architecture

### Core Components

- **Session**: Central state tracker and API entry point (`bluey/src/session.rs`)
- **Peripheral**: Represents BLE devices with address, name, RSSI, and GATT services
- **Service**: GATT services with characteristics and included services  
- **Characteristic**: GATT characteristics with descriptors and value operations
- **Descriptor**: GATT descriptors with read/write capabilities

### Backend Architecture

The library uses a stateless backend design where platform-specific implementations send events to the frontend for state tracking:

- **Windows**: WinRT backend (`bluey/src/winrt/`)
- **Android**: JNI backend (`bluey/src/android/`) with Java support classes
- **Linux**: BlueZ backend via `bluer` crate (`bluey/src/linux/`)
- **Web**: Web Bluetooth API (`bluey/src/web/`)
- **Fake**: Testing backend (`bluey/src/fake/`)

All backends implement the `BackendSession` trait and communicate via an event stream to the frontend session.

### Event-Driven Design

The API uses a unified event stream pattern with `Session::events()` providing notifications for:
- Peripheral discovery and property changes
- Connection/disconnection events  
- GATT service/characteristic/descriptor discovery
- Characteristic value notifications

## Project Structure

### Core Crates

- **`bluey/`**: Main library crate with cross-platform BLE API
- **`bluey-ui/`**: Cross-platform UI application using egui for testing/demonstration
- **`bluey-web/`**: WASM bindings for web usage

### Platform-Specific Code

- **Android**: Java support classes in `bluey/src/java/` built via Gradle
- **Windows**: WinRT bindings generated via `build.rs`
- **Linux**: BlueZ integration via `bluer` dependency

### Key Files

- `bluey/src/lib.rs`: Core types, addresses, errors, and events
- `bluey/src/session.rs`: Main API implementation and state management
- `bluey/examples/`: Usage examples including heart rate monitor and scanning

## Development Commands

### Building

```bash
# Main library (host platform)
cargo build

# All targets in workspace  
cargo build --workspace

# Android UI (requires Android SDK/NDK setup)
cd bluey-ui
export ANDROID_HOME="path/to/sdk"
export ANDROID_NDK_HOME="path/to/ndk"
rustup target add aarch64-linux-android
cargo install cargo-ndk
cargo ndk -t aarch64-linux-android -o app/src/main/jniLibs/ build
./gradlew build

# Desktop UI
cargo run --manifest-path bluey-ui/Cargo.toml --features=desktop

# Web build
cd bluey-web  
wasm-pack build --target web
```

### Testing

```bash
# Run tests (note: most require physical BLE hardware or will use fake backend)
cargo test

# Run examples
cargo run --example scan
cargo run --example heart-rate-monitor
```

### Code Quality

```bash
# Format code (uses custom rustfmt.toml settings)
cargo fmt

# Check compilation
cargo check --workspace
```

## Platform Support Matrix

| Feature | Windows | Android | Linux | Web |
|---------|---------|---------|-------|-----|
| Scanning | ✓ | ✓ | ✓ | ✓ |
| GATT Operations | ✓ | ✓ | ✓ | ✓ |
| Peripheral Selection | - | ✓ (Companion API) | - | - |
| Service Data | ✓ | - | ✓ | ✓ |

## Key Design Considerations

### State Management

- Frontend maintains all GATT state; backends are mostly stateless
- Peripheral handles are platform-specific u32 identifiers
- State objects use Arc<RwLock<>> for thread-safe access
- Weak references prevent circular dependencies between Session and Peripherals

### Android-Specific Notes

- Requires careful request serialization due to stack limitations
- Supports both traditional scanning and Companion API for device selection
- Java AAR built via Gradle and included in `bluey-ui/app/libs/`
- Location permissions required for traditional BLE scanning

### Cross-Platform Compatibility

- Address handling supports both MAC addresses and platform-specific strings
- Event ordering preserved where possible across platforms
- Characteristic/descriptor ordering maintained by handle values when available

## Common Patterns

### Basic Scanning
```rust
let session = SessionConfig::new().start().await?;
let events = session.events()?;
session.start_scanning(Filter::new()).await?;
// Process events stream for PeripheralFound notifications
```

### GATT Operations
```rust
peripheral.connect().await?;
peripheral.discover_services(None).await?;
let services = peripheral.services()?;
// Access characteristics and perform read/write/subscribe operations
```

### Testing
Use the fake backend for unit tests:
```rust
let session = SessionConfig::new()
    .set_backend(Backend::Fake)
    .start().await?;
```