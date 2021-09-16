use std::borrow::Cow;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::{stream, Stream, StreamExt};

use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;

use crate::session::{BackendSession, Filter, SessionConfig};
use crate::{Address, BackendEvent, CharacteristicHandle, Error, PeripheralHandle, Result, ServiceHandle, fake};

#[derive(Debug)]
pub(crate) struct FakeSession {}
impl FakeSession {
    pub async fn new(config: &SessionConfig, backend_bus: mpsc::UnboundedSender<BackendEvent>)
                     -> Result<Self> {
        Ok(FakeSession {})
    }
}

#[async_trait]
impl BackendSession for FakeSession {
    async fn start_scanning(&self, filter: &Filter) -> Result<()> {
        todo!();
    }
    async fn stop_scanning(&self) -> Result<()> {
        todo!();
    }

    fn declare_peripheral(&self, address: Address) -> Result<PeripheralHandle> {
        todo!();
    }

    async fn peripheral_connect(&self, peripheral_handle: PeripheralHandle) -> Result<()> {
        todo!();
    }
    async fn peripheral_discover_gatt_services(&self, peripheral_handle: PeripheralHandle)
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
                                      characteristic_handle: CharacteristicHandle,
                                      cache_mode: crate::CacheMode)
                                      -> Result<Vec<u8>> {
        todo!()
    }

    async fn gatt_characteristic_write(&self, peripheral_handle: PeripheralHandle,
                                       characteristic_handle: CharacteristicHandle,
                                       write_type: crate::characteristic::WriteType, data: &[u8])
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

    fn gatt_characteristic_uuid(&self, peripheral_handle: PeripheralHandle,
                                characteristic_handle: CharacteristicHandle)
                                -> Result<uuid::Uuid> {
        todo!()
    }

    fn gatt_service_uuid(&self, peripheral_handle: PeripheralHandle,
                         service_handle: ServiceHandle)
                         -> Result<uuid::Uuid> {
        todo!()
    }

    fn flush(&self, id: u32) -> Result<()> {
        todo!()
    }
}
