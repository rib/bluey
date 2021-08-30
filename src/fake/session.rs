use crate::session::{PlatformSession, SessionConfig, Filter};
use crate::{fake, winrt, Error, Result, PlatformEvent, PlatformPeripheralHandle};
use async_trait::async_trait;
use futures::{stream, Stream, StreamExt};
use std::borrow::Cow;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;

#[derive(Debug)]
pub(crate) struct FakeSession {}
impl FakeSession {
    pub async fn new(
        config: &SessionConfig,
        platform_bus: mpsc::UnboundedSender<PlatformEvent>,
    ) -> Result<Self> {
        Ok(FakeSession {})
    }
}

#[async_trait]
impl PlatformSession for FakeSession {
    async fn start_scanning(&self, filter: &Filter) -> Result<()> {
        todo!();
        Ok(())
    }
    async fn stop_scanning(&self) -> Result<()> {
        todo!();
        Ok(())
    }
    async fn connect_peripheral(&self, peripheral_handle: PlatformPeripheralHandle) -> Result<()> {
        todo!();
        Ok(())
    }
}
