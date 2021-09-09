use uuid::Uuid;

const BLUETOOTH_BASE_UUID: u128 = 0x00000000_0000_1000_8000_00805f9b34fb;
const BLUETOOTH_BASE_MASK_32: u128 = 0x00000000_ffff_ffff_ffff_ffffffffffff;
const BLUETOOTH_BASE_MASK_16: u128 = 0xffff0000_ffff_ffff_ffff_ffffffffffff;

pub trait BluetoothUuid {
    fn as_u16(&self) -> Option<u16>;
    fn as_u32(&self) -> Option<u32>;
    fn from_u16(v: u16) -> Uuid;
    fn from_u32(v: u32) -> Uuid;
}

impl BluetoothUuid for Uuid {
    fn as_u16(&self) -> Option<u16> {
        let value = self.as_u128();
        if value & BLUETOOTH_BASE_MASK_16 == BLUETOOTH_BASE_UUID {
            Some((value >> 96) as u16)
        } else {
            None
        }
    }

    fn as_u32(&self) -> Option<u32> {
        let value = self.as_u128();
        if value & BLUETOOTH_BASE_MASK_32 == BLUETOOTH_BASE_UUID {
            Some((value >> 96) as u32)
        } else {
            None
        }
    }

    fn from_u16(v: u16) -> Uuid {
        uuid_from_u16(v)
    }

    fn from_u32(v: u32) -> Uuid {
        uuid_from_u32(v)
    }
}

// It's useful to have const functions so apps can declare const Uuids but
// unfortunately we can't have const functions in traits yet

pub const fn uuid_from_u16(v: u16) -> Uuid {
    Uuid::from_u128(BLUETOOTH_BASE_UUID | ((v as u128) << 96))
}
pub const fn uuid_from_u32(v: u32) -> Uuid {
    Uuid::from_u128(BLUETOOTH_BASE_UUID | ((v as u128) << 96))
}
