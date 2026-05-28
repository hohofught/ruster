use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const MAX_CONCURRENT_REQUESTS: usize = 5;

static REQUEST_GATE: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn gate() -> Arc<Semaphore> {
    REQUEST_GATE
        .get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)))
        .clone()
}

pub async fn acquire() -> Result<OwnedSemaphorePermit, String> {
    gate()
        .acquire_owned()
        .await
        .map_err(|_| "Gemini request gate is closed".to_owned())
}

pub async fn acquire_with_timeout(timeout: Duration) -> Result<OwnedSemaphorePermit, String> {
    match tokio::time::timeout(timeout, acquire()).await {
        Ok(result) => result,
        Err(_) => Err("fast wrapper queue timeout".to_owned()),
    }
}
