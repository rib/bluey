use crate::{Error, Result};
use std::str::FromStr;

use anyhow::anyhow;
use uuid::Uuid;
use windows::{Abi, Guid};

/// Guid doesn't expose its value publicly, except via the Debug trait!
/// windows::Guid is a repr(C) struct and we've copied the layout
/// so we can transmute and access the data...
/*
#[repr(C)]
struct CGuid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}
*/

pub trait WinrtUuid {
    fn as_guid(uuid: &Uuid) -> Guid;

    fn try_from_guid(guid: &Guid) -> Result<Uuid>;
}

impl WinrtUuid for Uuid {
    fn as_guid(uuid: &Uuid) -> Guid {
        let (data1, data2, data3, data4) = uuid.as_fields();
        Guid::from_values(data1, data2, data3, data4.to_owned())
    }

    // Urgh, Guid doesn't expose its value publicly, except via the Debug trait!
    fn try_from_guid(guid: &Guid) -> Result<Uuid> {
        //let cguid: CGuid = unsafe { std::mem::transmute(guid) };
        //Ok(Uuid::from_fields(cguid.data1, cguid.data2, cguid.data3, &cguid.data4).map_err(|err|Error::Other(anyhow!(err)))?)
        /*
        let (data1, data2, data3, data4) = unsafe {
            let ptr = guid as *const Guid;
            let ptr: *const CGuid = ptr as *const CGuid;
            let cguid: &CGuid = &*ptr as &CGuid;
            (cguid.data1, cguid.data2, cguid.data3, cguid.data4)
        };
        */
        // XXX: Pretty hacky but but I guess it at least avoids unsafe code :/ ...
        let s = format!("{:?}", guid);
        Ok(Uuid::from_str(&s).map_err(|err| Error::Other(anyhow!(err)))?)
    }
}
