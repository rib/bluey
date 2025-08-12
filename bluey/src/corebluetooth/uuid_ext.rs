use crate::uuid::BluetoothUuid;
use objc2_core_bluetooth::CBUUID;
use uuid::Uuid;

/// Extension trait for UUID conversion from CBUUID
pub trait UuidExt {
    fn from_cbuuid(cbuuid: &CBUUID) -> Uuid;
}

impl UuidExt for Uuid {
    fn from_cbuuid(cbuuid: &CBUUID) -> Uuid {
        let data = unsafe { cbuuid.data() };
        let bytes = unsafe { data.as_bytes_unchecked() };

        match bytes.len() {
            2 => {
                // 16-bit UUID short form
                let uuid_16 = u16::from_be_bytes([bytes[0], bytes[1]]);
                Uuid::from_u16(uuid_16)
            }
            4 => {
                // 32-bit UUID short form
                let uuid_32 = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                Uuid::from_u32(uuid_32)
            }
            16 => {
                // Full 128-bit UUID
                let mut uuid_bytes = [0u8; 16];
                uuid_bytes.copy_from_slice(bytes);
                Uuid::from_bytes(uuid_bytes)
            }
            _ => {
                // Fallback - this shouldn't happen with valid CBUUIDs
                log::warn!("Unexpected CBUUID data length: {}", bytes.len());
                Uuid::nil()
            }
        }
    }
}
