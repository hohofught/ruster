use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::watch;

#[derive(Clone, Debug, thiserror::Error)]
#[error("F12 request guard active ({remaining_secs}s remaining)")]
pub struct RequestGuardError {
    pub remaining_secs: u64,
}

#[derive(Clone)]
pub struct RequestGuard {
    inner: Arc<Mutex<RequestGuardInner>>,
    cancel_tx: watch::Sender<u64>,
}

struct RequestGuardInner {
    guard_until: Option<Instant>,
    generation: u64,
}

impl Default for RequestGuard {
    fn default() -> Self {
        let (cancel_tx, _) = watch::channel(0);
        Self {
            inner: Arc::new(Mutex::new(RequestGuardInner {
                guard_until: None,
                generation: 0,
            })),
            cancel_tx,
        }
    }
}

impl RequestGuard {
    pub const DURATION: Duration = Duration::from_secs(1);

    pub fn activate(&self, _source: &str) -> Duration {
        let mut inner = self.inner.lock();
        let now = Instant::now();
        let requested_until = now + Self::DURATION;
        if inner
            .guard_until
            .is_none_or(|until| requested_until > until)
        {
            inner.guard_until = Some(requested_until);
        }
        inner.generation = inner.generation.saturating_add(1);
        let _ = self.cancel_tx.send(inner.generation);
        inner
            .guard_until
            .unwrap_or(requested_until)
            .saturating_duration_since(now)
    }

    pub fn is_active(&self) -> Option<Duration> {
        let mut inner = self.inner.lock();
        let now = Instant::now();
        match inner.guard_until {
            Some(until) if until > now => Some(until.saturating_duration_since(now)),
            _ => {
                inner.guard_until = None;
                None
            }
        }
    }

    pub fn throw_if_active(&self) -> Result<(), RequestGuardError> {
        if let Some(remaining) = self.is_active() {
            return Err(RequestGuardError {
                remaining_secs: remaining.as_secs().max(1),
            });
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn subscribe_cancel(&self) -> watch::Receiver<u64> {
        self.cancel_tx.subscribe()
    }
}
