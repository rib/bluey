use std::borrow::Cow;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::{stream, Stream, StreamExt};

use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;

use uuid::Uuid;

use crate::session::{BackendSession, Filter, SessionConfig};
use crate::{Address, BackendEvent, CharacteristicHandle, Error, PeripheralHandle, Result, ServiceHandle, fake, DescriptorHandle};

#[derive(Debug)]
pub(crate) struct FakeSession {}
impl FakeSession {
    pub fn new(config: &SessionConfig<'_>, backend_bus: mpsc::UnboundedSender<BackendEvent>)
                     -> Result<Self> {
        Ok(FakeSession {})
    }
}

#[async_trait]
impl BackendSession for FakeSession {
    fn start_scanning(&self, filter: &Filter) -> Result<()> {
        todo!();
    }
    fn stop_scanning(&self) -> Result<()> {
        todo!();
    }

    fn declare_peripheral(&self, address: Address, name: String) -> Result<PeripheralHandle> {
        todo!();
    }

    async fn peripheral_connect(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        todo!();
    }
    async fn peripheral_disconnect(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        todo!();
    }
    fn peripheral_drop_gatt_state(&self, peripheral_handle: PeripheralHandle) {
        todo!();
    }

    async fn peripheral_read_rssi(&self, peripheral_handle: PeripheralHandle) -> Result<i16> {
        todo!();
    }

    async fn peripheral_discover_gatt_services(&self, peripheral_handle: PeripheralHandle,
                                               of_interest_hint: Option<Vec<Uuid>>)
                                               -> Result<()> {
        todo!()
    }
    async fn gatt_service_discover_includes(&self, peripheral_handle: PeripheralHandle,
                                            service_handle: ServiceHandle)
                                            -> Result<()> {
        todo!()
    }
    async fn gatt_service_discover_characteristics(&self, peripheral_handle: PeripheralHandle,
                                                   service_handle: ServiceHandle)
                                                   -> Result<()> {
        todo!()
    }

    async fn gatt_characteristic_read(&self, peripheral_handle: PeripheralHandle,
                                      service_handle: ServiceHandle,
                                      characteristic_handle: CharacteristicHandle,
                                      cache_mode: crate::CacheMode)
                                      -> Result<Vec<u8>> {
        todo!()
    }

    async fn gatt_characteristic_write(&self, peripheral_handle: PeripheralHandle,
                                       service_handle: ServiceHandle,
                                       characteristic_handle: CharacteristicHandle,
                                       write_type: crate::characteristic::WriteType,
                                       data: &[u8])
                                       -> Result<()> {
        todo!()
    }

    async fn gatt_characteristic_subscribe(&self, peripheral_handle: PeripheralHandle,
                                           service_handle: ServiceHandle,
                                           characteristic_handle: CharacteristicHandle)
                                           -> Result<()> {
        todo!()
    }

    async fn gatt_characteristic_unsubscribe(&self, peripheral_handle: PeripheralHandle,
                                             service_handle: ServiceHandle,
                                             characteristic_handle: CharacteristicHandle)
                                             -> Result<()> {
        todo!()
    }

    async fn gatt_characteristic_discover_descriptors(&self, peripheral_handle: PeripheralHandle,
                                                      service_handle: ServiceHandle,
                                                      characteristic_handle: CharacteristicHandle)
                                                   -> Result<()> {
        todo!()
    }

    async fn gatt_descriptor_read(&self, peripheral_handle: PeripheralHandle,
                                  service_handle: ServiceHandle,
                                  characteristic_handle: CharacteristicHandle,
                                  descriptor_handle: DescriptorHandle,
                                  cache_mode: crate::CacheMode)
                                  -> Result<Vec<u8>> {
        todo!()
    }
    async fn gatt_descriptor_write(&self, peripheral_handle: PeripheralHandle,
                                   service_handle: ServiceHandle,
                                   characteristic_handle: CharacteristicHandle,
                                   descriptor_handle: DescriptorHandle,
                                   data: &[u8])
                                   -> Result<()> {
        todo!()
    }
    //fn gatt_service_uuid(&self, peripheral_handle: PeripheralHandle,
    //                     service_handle: ServiceHandle)
    //                     -> Result<uuid::Uuid> {
    //    todo!()
    //}

    //fn gatt_characteristic_uuid(&self, peripheral_handle: PeripheralHandle,
    //                            characteristic_handle: CharacteristicHandle)
    //                            -> Result<uuid::Uuid> {
    //    todo!()
    //}

    fn flush(&self, id: u32) -> Result<()> {
        todo!()
    }
}
