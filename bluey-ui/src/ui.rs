// There's some kind of compiler bug going on, causing a crazy amount of false
// positives atm :(
#![allow(dead_code)]

use std::{collections::{HashSet, HashMap, VecDeque}, time::{Instant, Duration}};

use bluey::{peripheral::Peripheral, uuid::BluetoothUuid, service::Service, characteristic::{Characteristic, CharacteristicProperties}, descriptor::Descriptor};
use log::{info, debug};
use serde::{de, Deserialize};
use uuid::Uuid;
use lazy_static::lazy_static;

use tokio::sync::mpsc::UnboundedSender;
use egui::{self, RichText, Color32, Ui};
use crate::ble::{self, BleRequest};

const HEART_RATE_MEASUREMENT_CHARACTERISTIC_UUID: Uuid = bluey::uuid::uuid_from_u16(0x2A37);

const SERVICES_DB_JSON: &str = include_str!("../bluetooth-numbers-database/service_uuids.json");
const CHARACTERISTICS_DB_JSON: &str = include_str!("../bluetooth-numbers-database/characteristic_uuids.json");
const DESCRIPTORS_DB_JSON: &str = include_str!("../bluetooth-numbers-database/descriptor_uuids.json");
const COMPANIES_DB_JSON: &str = include_str!("../bluetooth-numbers-database/company_ids.json");

pub fn deserialize_bluetooth_db_uuid<'de, D>(deserializer: D) -> Result<Uuid, D::Error>
where
    D: de::Deserializer<'de>,
{
    let s: String = de::Deserialize::deserialize(deserializer)?;

    match s.parse::<Uuid>() {
        Ok(uuid) => Ok(uuid),
        Err(_) => match u16::from_str_radix(&s, 16) {
            Ok(short) => Ok(Uuid::from_u16(short)),
            Err(_) => {
                Ok(Uuid::from_u32(u32::from_str_radix(&s, 16).map_err(de::Error::custom)?))
            }
        }
    }
}

#[derive(Deserialize, Debug)]
struct BluetoothUuidInfo {
    name: String,
    identifier: String,
    #[serde(deserialize_with="deserialize_bluetooth_db_uuid")]
    uuid: Uuid,
    source: String
}

#[derive(Deserialize, Debug)]
struct CharacteristicDBEntry {
    name: String,
    identifier: String,
    #[serde(deserialize_with="deserialize_bluetooth_db_uuid")]
    uuid: Uuid,
    source: String
}

#[derive(Deserialize, Debug)]
struct DescriptorDBEntry {
    name: String,
    identifier: String,
    #[serde(deserialize_with="deserialize_bluetooth_db_uuid")]
    uuid: Uuid,
    source: String
}

#[derive(Deserialize, Debug)]
struct CompanyName {
    code: u16,
    name: String,
}

fn index_uuids(uuids: &'static Vec<BluetoothUuidInfo>) -> HashMap<Uuid, &'static BluetoothUuidInfo> {
    let mut index = HashMap::new();

    for i in 0..uuids.len() {
        index.insert(uuids[i].uuid, &uuids[i]);
    }
    index
}

pub fn index_companies() -> HashMap<u16, &'static String> {
    let mut index = HashMap::new();

    for i in 0..COMPANIES_DB.len() {
        index.insert(COMPANIES_DB[i].code, &COMPANIES_DB[i].name);
    }
    index
}

lazy_static! {
    static ref SERVICES_DB: Vec<BluetoothUuidInfo> = serde_json::from_str(&SERVICES_DB_JSON).unwrap();
    static ref CHARACTERISTICS_DB: Vec<BluetoothUuidInfo> = serde_json::from_str(&CHARACTERISTICS_DB_JSON).unwrap();
    static ref DESCRIPTORS_DB: Vec<BluetoothUuidInfo> = serde_json::from_str(&DESCRIPTORS_DB_JSON).unwrap();
    static ref COMPANIES_DB: Vec<CompanyName> = serde_json::from_str(&COMPANIES_DB_JSON).unwrap();

    static ref SERVICE_UUIDS: HashMap<Uuid, &'static BluetoothUuidInfo> = index_uuids(&SERVICES_DB);
    static ref CHARACTERISTIC_UUIDS: HashMap<Uuid, &'static BluetoothUuidInfo> = index_uuids(&CHARACTERISTICS_DB);
    static ref DESCRIPTOR_UUIDS: HashMap<Uuid, &'static BluetoothUuidInfo> = index_uuids(&DESCRIPTORS_DB);
    static ref COMPANY_NAMES: HashMap<u16, &'static String> = index_companies();
}

#[derive(Debug, Clone)]
pub enum Event {
    RequestRedraw,
    Ble(bluey::Event),

    UpdateCharacteristicValue(Characteristic, Vec<u8>),
    UpdateDescriptorValue(Descriptor, Vec<u8>),

    // Show a notice to the user...
    ShowText(log::Level, String),
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum BleState {
    Idle,
    Scanning,
    Connecting,
    Connected
}
impl Default for BleState {
    fn default() -> Self { BleState::Idle }
}

struct Notice {
    level: log::Level,
    text: String,
    timestamp: Instant
}

const NOTICE_TIMEOUT_SECS: u64 = 7;

#[derive(Debug)]
pub struct PeripheralState {
    //peripheral: Peripheral,

    connected: bool,

    name: String,
    back_label: String,

    address: String,
    address_type: String,
    //tx_power: String,
    //rssi: String,
    service_ids: Vec<Uuid>,
    services: Vec<Service>,
    manufacturer_data: Option<HashMap<u16, Vec<u8>>>,
}

#[derive(Debug)]
pub struct ServiceState {
    uuid: Uuid,
    uuid_str: String,

    name: String,
    back_label: String,

    included: Vec<Service>,
    characteristics: Vec<Characteristic>
}

#[derive(Debug)]
pub struct CharacteristicState {
    uuid: Uuid,
    uuid_str: String,

    name: String,
    back_label: String,

    properties: Vec<String>,
    readable: bool,
    subscribable: bool,
    last_read: Option<String>,
    //last_read_val: Option<Vec<u8>>,
    extra_props: Vec<(String, String)>,
    descriptors: Vec<Descriptor>
}

#[derive(Debug)]
pub struct DescriptorState {
    uuid: Uuid,
    uuid_str: String,
    name: String,
    last_read: Option<String>,
}

#[derive(Default)]
pub struct State {
    state: BleState,

    notices: VecDeque<Notice>,
    peripherals: HashSet<Peripheral>,
    //selected_peripheral: Option<Peripheral>,
    //connected_peripheral: Option<Peripheral>,

    selected_peripheral: Option<Peripheral>,
    selected_peripheral_state: Option<PeripheralState>,
    selected_service_id: Option<Uuid>,
    selected_service: Option<Service>,
    selected_service_state: Option<ServiceState>,
    selected_characteristic: Option<Characteristic>,
    selected_characteristic_state: Option<CharacteristicState>,
    selected_descriptor: Option<Descriptor>,
    selected_descriptor_state: Option<DescriptorState>,
}

impl State {

    fn update_selected_peripheral_property(&mut self, peripheral: &Peripheral, property_id: bluey::PeripheralPropertyId) {
        match property_id {
            bluey::PeripheralPropertyId::Name => {
                let name = peripheral.name().unwrap_or(format!("Unknown"));
                self.selected_peripheral_state.as_mut().unwrap().back_label = format!("< {name}");
                self.selected_peripheral_state.as_mut().unwrap().name = name;
            },
            bluey::PeripheralPropertyId::AddressType => {
                self.selected_peripheral_state.as_mut().unwrap().address_type = peripheral.address_type().map_or("Unknown".to_string(),
                    |addr_type| format!("{addr_type:?}"));
            }
            bluey::PeripheralPropertyId::Rssi => { /* queried each frame */},
            bluey::PeripheralPropertyId::TxPower => { /* queried each frame */},
            bluey::PeripheralPropertyId::ManufacturerData => {
                self.selected_peripheral_state.as_mut().unwrap().manufacturer_data = peripheral.all_manufacturer_data();
            },
            bluey::PeripheralPropertyId::ServiceData => {

            },
            bluey::PeripheralPropertyId::ServiceIds => {
                self.selected_peripheral_state.as_mut().unwrap().service_ids = peripheral.service_ids();
            },
            bluey::PeripheralPropertyId::PrimaryServices => {},
            _ => {},
        }
    }

    fn update_selected_peripheral_services(&mut self, peripheral: &Peripheral) {
        self.selected_peripheral_state.as_mut().unwrap().services = peripheral.primary_services();
    }

    fn update_selected_service_includes(&mut self, _peripheral: &Peripheral, service: &Service) -> Result<(), anyhow::Error> {
        self.selected_service_state.as_mut().unwrap().included = service.included_services()?;
        Ok(())
    }

    fn update_selected_service_characteristics(&mut self, _peripheral: &Peripheral, service: &Service) -> Result<(), anyhow::Error> {
        self.selected_service_state.as_mut().unwrap().characteristics = service.characteristics()?;
        Ok(())
    }

     fn update_selected_characteristic_descriptors(&mut self, _peripheral: &Peripheral, _service: &Service, characteristic: &Characteristic) -> Result<(), anyhow::Error> {
        self.selected_characteristic_state.as_mut().unwrap().descriptors = characteristic.descriptors()?;
        Ok(())
    }

    fn select_adapter(&mut self, request_peripheral_disconnect: bool, ble_tx: &UnboundedSender<ble::BleRequest>) {
        debug!("select_adapter");
        if request_peripheral_disconnect {
            if let Some(peripheral) = self.selected_peripheral.as_ref() {
                let _ = ble_tx.send(BleRequest::Disconnect(peripheral.clone()));
            }
        }
        self.selected_peripheral = None;
        self.selected_service = None;
        self.selected_characteristic = None;
        self.selected_descriptor = None;
        self.state = BleState::Idle;
    }

    fn select_peripheral(&mut self, peripheral: &Peripheral, connected: bool, ble_tx: &UnboundedSender<ble::BleRequest>) {
        let peripheral_state = PeripheralState {
            //peripheral: peripheral.clone(),
            name: "Unknown".to_string(),
            back_label: "< Unknown".to_string(),
            connected,
            address: peripheral.address().to_string(),
            address_type: "Unknown".to_string(),
            service_ids: peripheral.service_ids(),
            services: peripheral.primary_services(),
            manufacturer_data: peripheral.all_manufacturer_data()
        };
        self.selected_peripheral = Some(peripheral.clone());
        self.selected_peripheral_state = Some(peripheral_state);
        self.update_selected_peripheral_property(peripheral, bluey::PeripheralPropertyId::Name);
        self.update_selected_peripheral_property(peripheral, bluey::PeripheralPropertyId::AddressType);

        self.selected_service = None;
        self.selected_characteristic = None;
        self.selected_descriptor = None;

        if connected {
            let _ = ble_tx.send(BleRequest::DiscoverGattServices(peripheral.clone()));
        }
    }

    fn select_service(&mut self, service: &Service, ble_tx: &UnboundedSender<ble::BleRequest>) {

        let uuid = service.uuid().unwrap();
        let name = if let Some(service_info) = SERVICE_UUIDS.get(&uuid) {
            service_info.name.clone()
        } else {
            uuid.to_string()
        };
        let back_label = format!("< {name}");
        let service_state = ServiceState {
            name,
            back_label,
            uuid,
            uuid_str: uuid.to_string(),
            included: service.included_services().unwrap_or(vec![]),
            characteristics: service.characteristics().unwrap_or(vec![]),
        };
        self.selected_service = Some(service.clone());
        self.selected_service_state = Some(service_state);
        self.selected_characteristic = None;
        self.selected_descriptor = None;

        let _ = ble_tx.send(BleRequest::DiscoverGattIncludes(service.clone()));
        let _ = ble_tx.send(BleRequest::DiscoverGattCharacteristics(service.clone()));
    }

    fn select_characteristic(&mut self, characteristic: &Characteristic, ble_tx: &UnboundedSender<ble::BleRequest>) {

        let uuid = characteristic.uuid().unwrap();
        let name = if let Some(info) = CHARACTERISTIC_UUIDS.get(&uuid) {
            info.name.clone()
        } else {
            uuid.to_string()
        };
        let back_label = format!("< {name}");

        let mut properties = vec![];
        let mut readable = false;
        let mut subscribable = false;

        if let Ok(props) = characteristic.properties() {
            readable = if props & CharacteristicProperties::READ != CharacteristicProperties::NONE { true } else { false };
            subscribable = if props & (CharacteristicProperties::INDICATE | CharacteristicProperties::NOTIFY) != CharacteristicProperties::NONE {
                true
            } else {
                false
            };

            if props & CharacteristicProperties::BROADCAST != CharacteristicProperties::NONE {
                properties.push("Broadcast".to_string());
            }
            if props & CharacteristicProperties::READ != CharacteristicProperties::NONE {
                properties.push("Read".to_string());
            }
            if props & CharacteristicProperties::WRITE != CharacteristicProperties::NONE {
                properties.push("Write".to_string());
            }
            if props & CharacteristicProperties::NOTIFY != CharacteristicProperties::NONE {
                properties.push("Notify".to_string());
            }
            if props & CharacteristicProperties::INDICATE != CharacteristicProperties::NONE {
                properties.push("Indicate".to_string());
            }
            if props & CharacteristicProperties::AUTHENTICATED_SIGNED_WRITES != CharacteristicProperties::NONE {
                properties.push("Authenticated Signed Writes".to_string());
            }
            if props & CharacteristicProperties::EXTENDED_PROPERTIES != CharacteristicProperties::NONE {
                properties.push("Extended Properties".to_string());
            }
            if props & CharacteristicProperties::RELIABLE_WRITES != CharacteristicProperties::NONE {
                properties.push("Reliable Writes".to_string());
            }
            if props & CharacteristicProperties::WRITABLE_AUXILIARIES != CharacteristicProperties::NONE {
                properties.push("Writable Auxiliaries".to_string());
            }
        }

        let state = CharacteristicState {
            name,
            back_label,
            uuid,
            uuid_str: uuid.to_string(),
            descriptors: characteristic.descriptors().unwrap_or(vec![]),
            properties,
            readable,
            subscribable,
            last_read: None,
            //last_read_val: None,
            extra_props: vec![],
        };
        self.selected_characteristic = Some(characteristic.clone());
        self.selected_characteristic_state = Some(state);
        self.selected_descriptor = None;

        let _ = ble_tx.send(BleRequest::DiscoverGattDescriptors(characteristic.clone()));
    }

    fn update_characteristic_value(&mut self, characteristic: &Characteristic, value: Vec<u8>) {
        if let Some(selected) = &self.selected_characteristic {
            if selected == characteristic {
                let state = self.selected_characteristic_state.as_mut().unwrap();
                state.last_read = Some(format!("{value:02x?}"));
                //state.last_read_val = Some(value);

                let mut props = vec![];
                if let Ok(uuid) = characteristic.uuid() {
                    if uuid == HEART_RATE_MEASUREMENT_CHARACTERISTIC_UUID {
                        let data = value;
                        debug!("HR: Notify! {:?}", &data);
                        let mut u16_format = false;
                        let mut rr_start = 2;
                        if data[0] & 0x1 == 0x1 {
                            u16_format = true;
                            debug!("> Format = UINT16");
                            props.push(("Format".to_string(), "UINT16".to_string()));
                            rr_start += 1;
                        } else {
                            props.push(("Format".to_string(), "UINT8".to_string()));
                            debug!("> Format = UINT8");
                        }
                        if data[0] & 0x3 == 0x3 {
                            props.push(("Contact Detection".to_string(), "SUPPORTED".to_string()));
                            debug!("> Contact detection: SUPPORTED");
                            if data[0] & 0x2 == 0x2 {
                                props.push(("Contact Status".to_string(), "IN-CONTACT".to_string()));
                                debug!(">> Contact status: IN-CONTACT");
                            } else {
                                props.push(("Contact Status".to_string(), "NOT IN-CONTACT".to_string()));
                                debug!(">> Contact status: NOT IN-CONTACT");
                            }
                        } else {
                            props.push(("Contact Detection".to_string(), "NOT SUPPORTED".to_string()));
                            debug!("> Contact detection: NOT SUPPORTED");
                            if data[0] & 0x2 == 0x2 {
                                debug!(">> Contact status: DEFAULT = IN-CONTACT");
                            } else {
                                debug!(">> Contact status: SPURIOUS: NOT IN-CONTACT");
                            }
                        }
                        if data[0] & 0x8 == 0x8 {
                            props.push(("Energy Expenditure".to_string(), "PRESENT".to_string()));
                            debug!("> Energy Expenditure: PRESENT");
                            rr_start += 2;
                        } else {
                            props.push(("Energy Expenditure".to_string(), "NOT PRESENT".to_string()));
                            debug!("> Energy Expenditure: NOT PRESENT");
                        }
                        if data[0] & 0x10 == 0x10 {
                            props.push(("RR".to_string(), "PRESENT".to_string()));
                            debug!("> RR: PRESENT")
                        } else {
                            props.push(("RR".to_string(), "NOT PRESENT".to_string()));
                            debug!("> RR: NOT PRESENT")
                        }
                        if data[0] & 0x60 != 0 {
                            debug!("> Reserved bits set!");
                        }
                        let mut hr = data[1] as u16;
                        if u16_format {
                            hr = u16::from_le_bytes([data[1], data[2]]);
                        }
                        props.push(("Heart Rate".to_string(), format!("{hr}")));
                        debug!("> Heart Rate: {}", hr);

                        // each RR value is 2 bytes
                        let n_rrs = (data.len() - rr_start) / 2;
                        for i in 0..n_rrs {
                            let pos = rr_start + 2 * i;
                            let rr_fixed =
                                u16::from_le_bytes([data[pos], data[pos + 1]]);
                            let rr_seconds: f32 = rr_fixed as f32 / 1024.0f32;
                            props.push((format!("RR[{i}]"), format!("{rr_seconds}")));
                            debug!("> RR[{}] = {}", i, rr_seconds);
                        }
                    }
                }
                state.extra_props = props;
            }
        }
    }

    fn select_descriptor(&mut self, descriptor: &Descriptor, _ble_tx: &UnboundedSender<ble::BleRequest>) {
        let uuid = descriptor.uuid().unwrap();
        let name = if let Some(info) = DESCRIPTOR_UUIDS.get(&uuid) {
            info.name.clone()
        } else {
            uuid.to_string()
        };
        let state = DescriptorState {
            name,
            uuid,
            uuid_str: uuid.to_string(),
            last_read: None,
        };
        self.selected_descriptor = Some(descriptor.clone());
        self.selected_descriptor_state = Some(state);
    }

    fn update_descriptor_value(&mut self, descriptor: &Descriptor, value: Vec<u8>) {
        if let Some(selected) = &self.selected_descriptor {
            if selected == descriptor {
                let state = self.selected_descriptor_state.as_mut().unwrap();
                state.last_read = Some(format!("{value:02x?}"));
            }
        }
    }

    pub fn draw_notices_header(&mut self, ui: &mut Ui) {
        //ui.heading("Bluey-UI");

        while self.notices.len() > 0 {
            let ts = self.notices.front().unwrap().timestamp;
            if Instant::now() - ts > Duration::from_secs(NOTICE_TIMEOUT_SECS) {
                self.notices.pop_front();
            } else {
                break;
            }
        }

        if self.notices.len() > 0 {
            for notice in self.notices.iter() {
                let mut rt = RichText::new(notice.text.clone())
                    .strong();
                let (fg, bg) = match notice.level {
                    log::Level::Warn => (Color32::YELLOW, Color32::DARK_GRAY),
                    log::Level::Error => (Color32::WHITE, Color32::DARK_RED),
                    _ => (Color32::TRANSPARENT, Color32::BLACK)
                };
                rt = rt.color(fg).background_color(bg);
                ui.label(rt);
            }
        }
    }

    pub fn draw_peripherals_list(&mut self, ui: &mut Ui, ble_tx: &UnboundedSender<ble::BleRequest>) {
        egui::Grid::new("devices").show(ui, |ui| {

            let mut newly_selected_peripheral = None;
            for peripheral in self.peripherals.iter() {
                if let Some(name) = peripheral.name() {
                    //let address = peripheral.address();
                    //let address_str = address.to_string();
                    //ui.label(name);
                    if ui.selectable_value(&mut self.selected_peripheral, Some(peripheral.clone()), name).changed() {
                        newly_selected_peripheral = Some(peripheral.clone());
                    }
                    //ui.label(address_str);

                    if ui.button("Connect").clicked() {
                        debug!("Connect...");
                        let _ = ble_tx.send(BleRequest::Connect(peripheral.clone()));
                        self.state = BleState::Connecting;
                    }
                    ui.end_row();
                }
            }
            if let Some(peripheral) = newly_selected_peripheral {
                self.select_peripheral(&peripheral, false, ble_tx);
            }
        });
    }


    pub fn draw_nav_header(&mut self, ui: &mut Ui, ble_tx: &UnboundedSender<ble::BleRequest>) {
        ui.horizontal(|ui| {
            if ui.button("< Adapter").clicked() {
                //let peripheral = self.selected_peripheral.as_ref().unwrap();
                self.select_adapter(true, ble_tx);
                /*
                let _ = ble_tx.send(BleRequest::Disconnect(peripheral.clone()));
                self.state = BleState::Idle;
                self.selected_peripheral = None;
                self.selected_service = None;
                self.selected_characteristic = None;
                self.selected_descriptor = None;
                */
            }
            if self.selected_service.is_some() {
                let state = self.selected_peripheral_state.as_ref().unwrap();
                if ui.button(&state.back_label).clicked() {
                    self.selected_service = None;
                    self.selected_characteristic = None;
                    self.selected_descriptor = None;
                }

                if self.selected_characteristic.is_some() {
                    let state = self.selected_service_state.as_ref().unwrap();
                    if ui.button(&state.back_label).clicked() {
                        self.selected_characteristic = None;
                        self.selected_descriptor = None;
                    }

                    if self.selected_descriptor.is_some() {
                        let state = self.selected_characteristic_state.as_ref().unwrap();
                        if ui.button(&state.back_label).clicked() {
                            self.selected_descriptor = None;
                        }

                        //let descriptor_state = self.selected_descriptor_state.as_ref().unwrap();
                        //ui.label(&descriptor_state.name);
                    } else {
                        //let state = self.selected_characteristic_state.as_ref().unwrap();
                        //ui.label(&state.name);
                    }
                } else {
                    //let state = self.selected_service_state.as_ref().unwrap();
                    //ui.label(&state.name);
                }

            } else {
                //let state = self.selected_peripheral_state.as_ref().unwrap();
                //ui.label(&state.name);
            }

        });
    }

    pub fn draw_peripheral_page(&mut self, ui: &mut Ui,
        ble_tx: &UnboundedSender<ble::BleRequest>)
    {
        let peripheral = self.selected_peripheral.as_ref().unwrap();
        let state = self.selected_peripheral_state.as_ref().unwrap();

        egui::Grid::new("peripheral_state").show(ui, |ui| {
            ui.heading("Peripheral:");
            ui.heading(&state.name);
            ui.end_row();

            ui.label("Address:");
            ui.label(&state.address);
            ui.end_row();

            ui.label("TX Power:");
            if let Some(tx) = peripheral.tx_power() {
                ui.label(format!("{} dBm", tx));
            } else {
                ui.label(format!("Unknown dBm"));
            }
            ui.end_row();

            ui.label("RSSI:");
            if let Some(rssi) = peripheral.rssi() {
                ui.label(format!("{} dBm", rssi));
            } else {
                ui.label(format!("Unknown dBm"));
            }
            ui.end_row();

        });

        if state.services.len() > 0 {
            let mut newly_selected_service = None;

            egui::CollapsingHeader::new("Services").show(ui, |ui| {
                for service in state.services.iter() {
                    if let Ok(uuid) = service.uuid() {
                        if let Some(service_info) = SERVICE_UUIDS.get(&uuid) {
                            if ui.selectable_value(&mut self.selected_service, Some(service.clone()), &service_info.name).changed() {
                                newly_selected_service = Some(service.clone());
                            }
                        } else {
                            if ui.selectable_value(&mut self.selected_service, Some(service.clone()), uuid.to_string()).changed() {
                                newly_selected_service = Some(service.clone());
                            }
                        }
                    }
                }
            });
            if let Some(selected_service) = newly_selected_service {
                self.select_service(&selected_service, ble_tx);
            }
        } else if state.service_ids.len() > 0 {
            let mut newly_selected_service = None;

            egui::CollapsingHeader::new("Services").show(ui, |ui| {
                for uuid in state.service_ids.iter() {
                    if let Some(service_info) = SERVICE_UUIDS.get(uuid) {
                        if ui.selectable_value(&mut self.selected_service_id, Some(*uuid), &service_info.name).changed() {
                            newly_selected_service = Some(*uuid);
                        }
                    } else {
                        if ui.selectable_value(&mut self.selected_service_id, Some(*uuid), uuid.to_string()).changed() {
                            newly_selected_service = Some(*uuid);
                        }
                    }
                }
            });
            if let Some(service_id) = &self.selected_service_id {
                if let Some(service_info) = SERVICE_UUIDS.get(&service_id) {
                    ui.group(|ui| {
                        egui::Grid::new("service_id_info").show(ui, |ui| {
                            ui.label("Name:");
                            ui.label(&service_info.name);
                            ui.end_row();
                            ui.label("Uuid:");
                            ui.label(service_id.to_string());
                            ui.end_row();
                            ui.label("Identifier:");
                            ui.label(&service_info.identifier);
                            ui.end_row();
                            ui.label("Source:");
                            ui.label(&service_info.source);
                            ui.end_row();
                        });
                    });
                }
            }
        }
    }

    pub fn draw_service_page(&mut self, ui: &mut Ui, ble_tx: &UnboundedSender<ble::BleRequest>)
    {
        //let service = self.selected_service.as_ref().unwrap();
        let state = self.selected_service_state.as_ref().unwrap();

        egui::Grid::new("service_state").show(ui, |ui| {
            ui.heading("Service:");
            ui.heading(&state.name);
            ui.end_row();

            ui.label("Uuid:");
            ui.label(&state.uuid_str);
            ui.end_row();
        });
        if let Some(info) = SERVICE_UUIDS.get(&state.uuid) {
            egui::Grid::new("service_id_info").show(ui, |ui| {
                ui.label("Identifier:");
                ui.label(&info.identifier);
                ui.end_row();
                ui.label("Source:");
                ui.label(&info.source);
                ui.end_row();
            });
        }

        let mut newly_selected_service = None;

        if state.included.len() > 0 {
            egui::CollapsingHeader::new("Included Services").show(ui, |ui| {
                for service in state.included.iter() {
                    if let Ok(uuid) = service.uuid() {
                        if let Some(service_info) = SERVICE_UUIDS.get(&uuid) {
                            if ui.selectable_value(&mut newly_selected_service, Some(service.clone()), &service_info.name).changed() {
                                newly_selected_service = Some(service.clone());
                            }
                        } else {
                            if ui.selectable_value(&mut newly_selected_service, Some(service.clone()), uuid.to_string()).changed() {
                                newly_selected_service = Some(service.clone());
                            }
                        }
                    }
                }
            });
        }

        let mut newly_selected_characteristic = None;

        egui::CollapsingHeader::new("Characteristics").show(ui, |ui| {
            for characteristic in state.characteristics.iter() {
                if let Ok(uuid) = characteristic.uuid() {
                    if let Some(characteristic_info) = CHARACTERISTIC_UUIDS.get(&uuid) {
                        if ui.selectable_value(&mut self.selected_characteristic, Some(characteristic.clone()), &characteristic_info.name).changed() {
                            newly_selected_characteristic = Some(characteristic.clone());
                        }
                    } else {
                        if ui.selectable_value(&mut self.selected_characteristic, Some(characteristic.clone()), uuid.to_string()).changed() {
                            newly_selected_characteristic = Some(characteristic.clone());
                        }
                    }
                }
            }
        });

        if let Some(selected_characteristic) = newly_selected_characteristic {
            self.select_characteristic(&selected_characteristic, ble_tx);
        }

        if let Some(selected_service) = newly_selected_service {
            self.select_service(&selected_service, ble_tx);
        }
    }

    pub fn draw_characteristic_page(&mut self, ui: &mut Ui,
        ble_tx: &UnboundedSender<ble::BleRequest>)
    {
        let characteristic = self.selected_characteristic.as_ref().unwrap();
        let state = self.selected_characteristic_state.as_ref().unwrap();

        egui::Grid::new("characteristic_state").show(ui, |ui| {
            ui.heading("Characteristic:");
            ui.heading(&state.name);
            ui.end_row();

            ui.label("Uuid:");
            ui.label(&state.uuid_str);
            ui.end_row();
        });
        if let Some(info) = DESCRIPTOR_UUIDS.get(&state.uuid) {
            egui::Grid::new("characteristic_id_info").show(ui, |ui| {
                ui.label("Identifier:");
                ui.label(&info.identifier);
                ui.end_row();
                ui.label("Source:");
                ui.label(&info.source);
                ui.end_row();
            });
        }
        ui.heading("Properties:");
        for prop in state.properties.iter() {
            ui.label(prop);
        }

        ui.horizontal(|ui| {
            if state.readable {
                if ui.button("Read").clicked() {
                    let _ = ble_tx.send(BleRequest::ReadGattCharacteristic(characteristic.clone()));
                }
            }
            if state.subscribable {
                if ui.button("Subscribe").clicked() {
                    let _ = ble_tx.send(BleRequest::SubscribeGattCharacteristic(characteristic.clone()));
                }
                if ui.button("Unsubscribe").clicked() {
                    let _ = ble_tx.send(BleRequest::UnsubscribeGattCharacteristic(characteristic.clone()));
                }
            }
        });

        let mut newly_selected_descriptor = None;
        egui::CollapsingHeader::new("Descriptors").show(ui, |ui| {
            for descriptor in state.descriptors.iter() {
                if let Ok(uuid) = descriptor.uuid() {
                    if let Some(info) = DESCRIPTOR_UUIDS.get(&uuid) {
                        if ui.selectable_value(&mut self.selected_descriptor, Some(descriptor.clone()), &info.name).changed() {
                            newly_selected_descriptor = Some(descriptor.clone());
                        }
                    } else {
                        if ui.selectable_value(&mut self.selected_descriptor, Some(descriptor.clone()), uuid.to_string()).changed() {
                            newly_selected_descriptor = Some(descriptor.clone());
                        }
                    }
                }
            }
        });

        if let Some(val) = &state.last_read {
            ui.heading("Value:");
            ui.label(val);
        }
        if state.extra_props.len() > 0 {
            ui.group(|ui| {
                ui.heading("Decoded:");
                egui::Grid::new("characteristic_extra_props").show(ui, |ui| {
                    for prop in state.extra_props.iter() {
                        ui.label(&prop.0);
                        ui.label(&prop.1);
                        ui.end_row();
                    }
                });
            });
        }

        if let Some(selected_descriptor) = newly_selected_descriptor {
            self.select_descriptor(&selected_descriptor, ble_tx);
        }
    }

    pub fn draw_descriptor_page(&self, ui: &mut Ui,
        ble_tx: &UnboundedSender<ble::BleRequest>)
    {
        let descriptor = self.selected_descriptor.as_ref().unwrap();
        let state = self.selected_descriptor_state.as_ref().unwrap();
        egui::Grid::new("descriptor_state").show(ui, |ui| {
            ui.heading("Descriptor:");
            ui.heading(&state.name);
            ui.end_row();

            ui.label("Uuid:");
            ui.label(&state.uuid_str);
            ui.end_row();
        });
        if let Some(info) = DESCRIPTOR_UUIDS.get(&state.uuid) {
            egui::Grid::new("descriptor_id_info").show(ui, |ui| {
                ui.label("Identifier:");
                ui.label(&info.identifier);
                ui.end_row();
                ui.label("Source:");
                ui.label(&info.source);
                ui.end_row();
            });
        }

        if ui.button("Read").clicked() {
            let _ = ble_tx.send(BleRequest::ReadGattDescriptor(descriptor.clone()));
        }

        if let Some(val) = &state.last_read {
            ui.label("Value:");
            ui.label(val);
        }
    }

    pub fn draw(&mut self, ctx: &egui::Context, ble_tx: &UnboundedSender<ble::BleRequest>) {
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            self.draw_notices_header(ui);

            match self.state {
                BleState::Idle => {
                    if ui.button("Scan").clicked() {
                        let _ = ble_tx.send(BleRequest::StartScanning);
                        self.state = BleState::Scanning;
                    }
                }
                BleState::Scanning => {
                    ui.horizontal(|ui| {
                        if ui.button("Stop Scanning").clicked() {
                            let _ = ble_tx.send(BleRequest::StopScanning);
                            self.state = BleState::Idle;
                        }
                        ui.spinner();
                    });
                }
                BleState::Connecting => {
                    ui.horizontal(|ui| {
                        ui.label("Connecting...");
                        ui.spinner();
                    });
                }
                BleState::Connected  => {
                    self.draw_nav_header(ui, ble_tx);
                }
            }
        });

        if cfg!(not(target_os="android")) {
            if self.state == BleState::Idle || self.state == BleState::Scanning {
                egui::SidePanel::left("devices")
                    .resizable(false)
                    .default_width(200.0)
                    .show(ctx, |ui| {
                        self.draw_peripherals_list(ui, ble_tx);
                    });
            }
        }
        egui::CentralPanel::default().show(ctx, |ui| {

            if cfg!(target_os="android") {
                self.draw_peripherals_list(ui, ble_tx);
            }

            if self.selected_descriptor.is_some() {
                assert!(self.selected_descriptor_state.is_some());
                self.draw_descriptor_page(ui, ble_tx);
            } else if self.selected_characteristic.is_some() {
                assert!(self.selected_characteristic_state.is_some());
                self.draw_characteristic_page(ui, ble_tx);
            } else if self.selected_service.is_some() {
                assert!(self.selected_service_state.is_some());
                self.draw_service_page(ui, ble_tx);
            } else if self.selected_peripheral.is_some() {
                assert!(self.selected_peripheral_state.is_some());
                self.draw_peripheral_page(ui, ble_tx);
            }

        });
    }

    pub fn handle_event(&mut self, event: Event, ble_tx: &UnboundedSender<ble::BleRequest>) {
        match event {
            Event::Ble(bluey::Event::PeripheralFound { peripheral, name, address, ..}) => {
                if !self.peripherals.contains(&peripheral) {
                    self.peripherals.insert(peripheral);
                    info!("UI: peripheral found {name}: {address}");
                }
            },
            Event::Ble(bluey::Event::PeripheralPropertyChanged { ref peripheral, property_id, ..}) => {
                match &self.selected_peripheral {
                    Some(selected) if selected == peripheral => {
                        self.update_selected_peripheral_property(peripheral, property_id);
                    }
                    _ => {}
                }
            }
            Event::Ble(bluey::Event::PeripheralConnected { peripheral, ..}) => {
                self.select_peripheral(&peripheral, true, ble_tx);
                self.state = BleState::Connected;
                let _ = ble_tx.send(BleRequest::DiscoverGattServices(peripheral.clone()));
            }
            Event::Ble(bluey::Event::PeripheralDisconnected { peripheral, ..}) => {
                if let Some(selected) = self.selected_peripheral.as_ref() {
                    if &peripheral == selected {
                        self.select_adapter(false, ble_tx);
                        self.notices.push_back(Notice { level: log::Level::Error, text: format!("Peripheral Disconnected"), timestamp: Instant::now() });
                    }
                }
            }
            Event::Ble(bluey::Event::PeripheralPrimaryGattServicesComplete { peripheral, .. })=> {
                match &self.selected_peripheral {
                    Some(selected) if selected == &peripheral => {
                        self.update_selected_peripheral_services(&peripheral);
                    }
                    _ => {}
                }
            }
            Event::Ble(bluey::Event::ServiceIncludedGattServicesComplete { peripheral, service, .. }) => {
                match &self.selected_service {
                    Some(selected) if selected == &service => {
                        self.update_selected_service_includes(&peripheral, &service);
                    }
                    _ => {}
                }
            }
            Event::Ble(bluey::Event::ServiceGattCharacteristicsComplete { peripheral, service, .. }) => {
                match &self.selected_service {
                    Some(selected) if selected == &service => {
                        self.update_selected_service_characteristics(&peripheral, &service);
                    }
                    _ => {}
                }
            }
            Event::Ble(bluey::Event::ServiceGattDescriptorsComplete { peripheral, service, characteristic, .. }) => {
                match &self.selected_characteristic {
                    Some(selected) if selected == &characteristic => {
                        self.update_selected_characteristic_descriptors(&peripheral, &service, &characteristic);
                    }
                    _ => {}
                }
            }
            Event::Ble(bluey::Event::ServiceGattCharacteristicValueNotify { characteristic, value, .. }) => {
                self.update_characteristic_value(&characteristic, value);
            }
            Event::UpdateCharacteristicValue(characteristic, value) => {
                self.update_characteristic_value(&characteristic, value);
            }
            Event::UpdateDescriptorValue(descriptor, value) => {
                self.update_descriptor_value(&descriptor, value);
            }
            Event::ShowText(level, text) => {
                self.notices.push_back(Notice { level, text, timestamp: Instant::now() });
            }
            _ => {

            }
        }
    }
}