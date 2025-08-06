# Bluey Web Bluetooth Heart Rate Monitor Test

This crate provides a web-based test application for the Bluey library's Web Bluetooth backend. It demonstrates connecting to a Bluetooth LE heart rate monitor and displaying real-time heart rate data.

## Prerequisites

- [Trunk](https://trunkrs.dev/) - Install with `cargo install trunk`
- A Bluetooth LE heart rate monitor (chest strap, smartwatch, fitness tracker, etc.)
- A modern browser supporting Web Bluetooth (Chrome, Edge, Opera)
- HTTPS connection (required by Web Bluetooth API)

## Running the Test

1. **Install Trunk** (if not already installed):
   ```bash
   cargo install trunk
   ```

2. **Build and serve the application**:
   ```bash
   cd bluey-web-test
   trunk serve
   ```

3. **Open your browser**: 
   - Trunk will automatically open `https://localhost:8080`
   - Accept the self-signed certificate warning (required for HTTPS)

4. **Start the test**:
   - Click the "Start Heart Rate Monitor Test" button
   - Your browser will show a device selection dialog
   - Select your heart rate monitor from the list
   - Watch the log for real-time heart rate data!

## Features Tested

- **Device Selection**: Uses Web Bluetooth's `requestDevice()` API
- **GATT Connection**: Connects to the selected device
- **Service Discovery**: Discovers the Heart Rate Service (0x180D)
- **Characteristic Discovery**: Finds the Heart Rate Measurement characteristic (0x2A37)
- **Notifications**: Subscribes to heart rate notifications
- **Data Parsing**: Parses and displays:
  - Heart rate in BPM
  - Data format (UINT8 vs UINT16)
  - Contact detection status
  - Energy expenditure (if present)
  - RR intervals (if present)

## Supported Heart Rate Monitors

This test should work with any Bluetooth LE device that implements the standard Heart Rate Service, including:

- Polar chest straps (H7, H9, H10, etc.)
- Garmin heart rate monitors
- Wahoo TICKR series
- Apple Watch (when in workout mode)
- Many fitness trackers and smartwatches

## Web Bluetooth Limitations

- **No scanning**: Web Bluetooth doesn't support traditional scanning. Instead, users select devices from a browser dialog.
- **HTTPS required**: Web Bluetooth only works over HTTPS connections.
- **User gesture required**: Bluetooth access must be initiated by user interaction (button click).
- **Limited device access**: Only devices matching the service filter are shown in the selection dialog.

## Development

The crate structure follows the standard Rust library pattern but builds to WebAssembly:

- `src/lib.rs` - Main application logic
- `index.html` - HTML template with styling
- `Cargo.toml` - Dependencies including web-sys bindings
- `Trunk.toml` - Build configuration for Trunk

To modify the test:

1. Edit `src/lib.rs` for logic changes
2. Edit `index.html` for UI/styling changes  
3. Run `trunk serve` to see changes live (with auto-reload)

## Building for Production

```bash
trunk build --release
```

The built files will be in the `dist/` directory and can be served by any static file server with HTTPS support.