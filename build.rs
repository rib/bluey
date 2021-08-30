fn main() {
    #[cfg(target_os = "windows")]
    windows::build!(
        Windows::Devices::Bluetooth::*,
        Windows::Devices::Bluetooth::Advertisement::*,
        Windows::Devices::Bluetooth::GenericAttributeProfile::*,
        Windows::Foundation::{
            IAsyncOperation,
            IReference,
            EventRegistrationToken,
            TypedEventHandler,
        },
        Windows::Foundation::Collections::{
            IVector,
            IVectorView,
        },
        Windows::Storage::Streams::{
            IBuffer,
            DataReader,
        },
    );
}
