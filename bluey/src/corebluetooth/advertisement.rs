use objc2_core_bluetooth::{
    CBAdvertisementDataIsConnectable, CBAdvertisementDataLocalNameKey,
    CBAdvertisementDataManufacturerDataKey, CBAdvertisementDataOverflowServiceUUIDsKey,
    CBAdvertisementDataServiceDataKey, CBAdvertisementDataServiceUUIDsKey,
    CBAdvertisementDataSolicitedServiceUUIDsKey, CBAdvertisementDataTxPowerLevelKey, CBUUID,
};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSNumber, NSString};
use std::collections::HashMap;
use uuid::Uuid;

use super::uuid_ext::UuidExt;

// Advertisement data keys for type-safe access
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AdvertisementDataKey {
    LocalName,
    ManufacturerData,
    ServiceData(Uuid), // UUID-specific service data
    ServiceUuids,
    TxPowerLevel,
    IsConnectable,
    SolicitedServiceUuids,
    OverflowServiceUuids,
}

// Advertisement data values that can be extracted from CoreBluetooth
#[derive(Debug, Clone)]
pub enum AdvertisementValue {
    LocalName(String),
    ManufacturerData(Vec<u8>),
    ServiceData { uuid: Uuid, data: Vec<u8> },
    ServiceUuids(Vec<Uuid>),
    TxPowerLevel(i32),
    IsConnectable(bool),
    SolicitedServiceUuids(Vec<Uuid>),
    OverflowServiceUuids(Vec<Uuid>),
    Raw(Vec<u8>), // For any unrecognized data
}

/// Parser for CoreBluetooth advertisement data
pub struct AdvertisementDataParser;

impl AdvertisementDataParser {
    /// Extract advertisement data from CoreBluetooth NSDictionary
    pub fn parse(ad_data: &NSDictionary) -> HashMap<AdvertisementDataKey, AdvertisementValue> {
        let mut advertisement_data = HashMap::new();

        log::debug!("Extracting advertisement data from NSDictionary");

        unsafe {
            // Get values by directly using the CoreBluetooth constants as keys

            // CBAdvertisementDataLocalNameKey
            if let Some(local_name) = ad_data.objectForKey(CBAdvertisementDataLocalNameKey) {
                if let Some(name_str) = local_name.downcast_ref::<NSString>() {
                    advertisement_data.insert(
                        AdvertisementDataKey::LocalName,
                        AdvertisementValue::LocalName(name_str.to_string()),
                    );
                    log::debug!("Found LocalName: {}", name_str);
                }
            }

            // CBAdvertisementDataManufacturerDataKey
            if let Some(manufacturer_data) =
                ad_data.objectForKey(CBAdvertisementDataManufacturerDataKey)
            {
                if let Some(data) = manufacturer_data.downcast_ref::<NSData>() {
                    let length = data.length() as usize;
                    if length > 0 {
                        // Extract raw bytes from NSData
                        let bytes = data.to_vec();

                        advertisement_data.insert(
                            AdvertisementDataKey::ManufacturerData,
                            AdvertisementValue::ManufacturerData(bytes.clone()),
                        );

                        log::debug!(
                            "Found ManufacturerData: {} bytes: {:02x?}",
                            length,
                            bytes.iter().take(16).map(|b| *b).collect::<Vec<u8>>()
                        );
                    } else {
                        log::debug!("Found empty ManufacturerData");
                    }
                }
            }

            // CBAdvertisementDataServiceUUIDsKey
            if let Some(service_uuids_obj) =
                ad_data.objectForKey(CBAdvertisementDataServiceUUIDsKey)
            {
                if let Some(service_uuids_array) = service_uuids_obj.downcast_ref::<NSArray>() {
                    let uuids = Self::parse_uuid_array(service_uuids_array);
                    if !uuids.is_empty() {
                        log::debug!("Found ServiceUUIDs: {:?}", uuids);
                        advertisement_data.insert(
                            AdvertisementDataKey::ServiceUuids,
                            AdvertisementValue::ServiceUuids(uuids),
                        );
                    }
                }
            }

            // CBAdvertisementDataTxPowerLevelKey
            if let Some(tx_power_obj) = ad_data.objectForKey(CBAdvertisementDataTxPowerLevelKey) {
                if let Some(tx_power_num) = tx_power_obj.downcast_ref::<NSNumber>() {
                    let tx_power = tx_power_num.intValue();
                    advertisement_data.insert(
                        AdvertisementDataKey::TxPowerLevel,
                        AdvertisementValue::TxPowerLevel(tx_power),
                    );
                    log::debug!("Found TxPowerLevel: {}", tx_power);
                }
            }

            // CBAdvertisementDataIsConnectable
            if let Some(connectable_obj) = ad_data.objectForKey(CBAdvertisementDataIsConnectable) {
                if let Some(connectable_num) = connectable_obj.downcast_ref::<NSNumber>() {
                    let is_connectable = connectable_num.boolValue();
                    advertisement_data.insert(
                        AdvertisementDataKey::IsConnectable,
                        AdvertisementValue::IsConnectable(is_connectable),
                    );
                    log::debug!("Found IsConnectable: {}", is_connectable);
                }
            }

            // CBAdvertisementDataServiceDataKey
            if let Some(service_data_dict) = ad_data.objectForKey(CBAdvertisementDataServiceDataKey)
            {
                if let Some(service_data) = service_data_dict.downcast_ref::<NSDictionary>() {
                    Self::parse_service_data(service_data, &mut advertisement_data);
                }
            }

            // CBAdvertisementDataSolicitedServiceUUIDsKey
            if let Some(solicited_uuids_obj) =
                ad_data.objectForKey(CBAdvertisementDataSolicitedServiceUUIDsKey)
            {
                if let Some(solicited_uuids_array) = solicited_uuids_obj.downcast_ref::<NSArray>() {
                    let uuids = Self::parse_uuid_array(solicited_uuids_array);
                    if !uuids.is_empty() {
                        log::debug!("Found SolicitedServiceUUIDs: {:?}", uuids);
                        advertisement_data.insert(
                            AdvertisementDataKey::SolicitedServiceUuids,
                            AdvertisementValue::SolicitedServiceUuids(uuids),
                        );
                    }
                }
            }

            // CBAdvertisementDataOverflowServiceUUIDsKey
            if let Some(overflow_uuids_obj) =
                ad_data.objectForKey(CBAdvertisementDataOverflowServiceUUIDsKey)
            {
                if let Some(overflow_uuids_array) = overflow_uuids_obj.downcast_ref::<NSArray>() {
                    let uuids = Self::parse_uuid_array(overflow_uuids_array);
                    if !uuids.is_empty() {
                        log::debug!("Found OverflowServiceUUIDs: {:?}", uuids);
                        advertisement_data.insert(
                            AdvertisementDataKey::OverflowServiceUuids,
                            AdvertisementValue::OverflowServiceUuids(uuids),
                        );
                    }
                }
            }
        }

        log::debug!(
            "Extracted {} advertisement data fields",
            advertisement_data.len()
        );
        advertisement_data
    }

    /// Parse UUID array from NSArray<CBUUID>
    fn parse_uuid_array(array: &NSArray) -> Vec<Uuid> {
        let mut uuids = Vec::new();
        for i in 0..array.count() {
            let cbuuid_obj = array.objectAtIndex(i);
            if let Some(cbuuid) = cbuuid_obj.downcast_ref::<CBUUID>() {
                let uuid = Uuid::from_cbuuid(cbuuid);
                uuids.push(uuid);
            }
        }
        uuids
    }

    /// Parse service data from NSDictionary
    fn parse_service_data(
        service_data: &NSDictionary,
        advertisement_data: &mut HashMap<AdvertisementDataKey, AdvertisementValue>,
    ) {
        log::debug!(
            "Parsing service data dictionary with {} entries",
            service_data.count()
        );

        unsafe {
            // Iterate through service data entries
            let all_keys = unsafe { service_data.allKeys() };
            for i in 0..all_keys.count() {
                let key = all_keys.objectAtIndex(i);
                let value = unsafe { service_data.objectForKey(&key) };
                log::debug!("Processing service data entry");

                // The key should be a CBUUID
                if let Some(cbuuid) = key.downcast_ref::<CBUUID>() {
                    let service_uuid = Uuid::from_cbuuid(cbuuid);

                    // The value should be NSData
                    if let Some(value_obj) = value {
                        if let Some(ns_data) = value_obj.downcast_ref::<NSData>() {
                            let data_bytes = ns_data.to_vec();

                            log::debug!(
                                "Found service data for UUID {}: {} bytes",
                                service_uuid,
                                data_bytes.len()
                            );

                            advertisement_data.insert(
                                AdvertisementDataKey::ServiceData(service_uuid),
                                AdvertisementValue::ServiceData {
                                    uuid: service_uuid,
                                    data: data_bytes.clone(),
                                },
                            );

                            if data_bytes.len() <= 32 {
                                log::debug!("Service data content: {:02x?}", data_bytes);
                            } else {
                                log::debug!(
                                    "Service data content (first 32 bytes): {:02x?}...",
                                    &data_bytes[..32]
                                );
                            }
                        } else {
                            log::warn!("Service data value is not NSData");
                        }
                    } else {
                        log::warn!("Service data value is None");
                    }
                } else {
                    log::warn!("Service data key is not CBUUID");
                }
            }
        }
    }
}
