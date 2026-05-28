use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex as ParkingMutex;
use tokio::sync::{Mutex, Notify};

use crate::host::HostError;
use crate::logging::LogBuffer;

const RECENT_RESULT_TTL: Duration = Duration::from_secs(5);
const MAX_RECENT_RESULTS: usize = 64;

#[derive(Clone)]
pub struct ProxyDeduplicator {
    inner: Arc<Mutex<DedupState>>,
    logs: LogBuffer,
}

struct DedupState {
    inflight: HashMap<String, Arc<InflightRequest>>,
    recent: HashMap<String, RecentResult>,
}

struct InflightRequest {
    notify: Notify,
    result: ParkingMutex<Option<Result<String, HostError>>>,
}

struct RecentResult {
    text: String,
    created_at: Instant,
}

impl ProxyDeduplicator {
    pub fn new(logs: LogBuffer) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DedupState {
                inflight: HashMap::new(),
                recent: HashMap::new(),
            })),
            logs,
        }
    }

    pub async fn run<F, Fut>(
        &self,
        key: String,
        label: &str,
        request_id: u64,
        source: &str,
        operation: F,
    ) -> Result<String, HostError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<String, HostError>>,
    {
        if let Some((text, age)) = self.try_get_recent(&key).await {
            self.logs.push(format!(
                "[{label}#{request_id}] 동일 프롬프트 최근 결과 재사용 (ageMs={})",
                age.as_millis()
            ));
            return Ok(text);
        }

        let (entry, is_owner) = {
            let mut state = self.inner.lock().await;
            if let Some(entry) = state.inflight.get(&key) {
                (entry.clone(), false)
            } else {
                let entry = Arc::new(InflightRequest {
                    notify: Notify::new(),
                    result: ParkingMutex::new(None),
                });
                state.inflight.insert(key.clone(), entry.clone());
                (entry, true)
            }
        };

        if !is_owner {
            self.logs.push(format!(
                "[{label}#{request_id}] 동일 프롬프트 처리 중 - 기존 요청에 합류 (source={source})"
            ));
            let notified = entry.notify.notified();
            if let Some(result) = entry.result.lock().clone() {
                return result;
            }
            notified.await;
            return entry.result.lock().clone().ok_or_else(|| {
                HostError::Internal("deduplicated request ended without result".to_owned())
            })?;
        }

        let result = operation().await;
        {
            *entry.result.lock() = Some(result.clone());
            entry.notify.notify_waiters();
        }

        let mut state = self.inner.lock().await;
        if state
            .inflight
            .get(&key)
            .map(|active| Arc::ptr_eq(active, &entry))
            .unwrap_or(false)
        {
            state.inflight.remove(&key);
        }
        if let Ok(text) = &result {
            store_recent_result(&mut state, key, text);
        }

        result
    }

    async fn try_get_recent(&self, key: &str) -> Option<(String, Duration)> {
        let mut state = self.inner.lock().await;
        let now = Instant::now();
        if let Some(recent) = state.recent.get(key) {
            let age = now.saturating_duration_since(recent.created_at);
            if age <= RECENT_RESULT_TTL {
                return Some((recent.text.clone(), age));
            }
        }
        state.recent.remove(key);
        None
    }
}

fn store_recent_result(state: &mut DedupState, key: String, text: &str) {
    if text.trim().is_empty() || text.starts_with("Retry_") {
        return;
    }

    prune_recent_results(state);
    if state.recent.len() >= MAX_RECENT_RESULTS {
        state.recent.clear();
    }
    state.recent.insert(
        key,
        RecentResult {
            text: text.to_owned(),
            created_at: Instant::now(),
        },
    );
}

fn prune_recent_results(state: &mut DedupState) {
    let now = Instant::now();
    state
        .recent
        .retain(|_, recent| now.saturating_duration_since(recent.created_at) <= RECENT_RESULT_TTL);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::logging::LogBuffer;

    #[tokio::test]
    async fn reuses_recent_successful_result() {
        let dedup = ProxyDeduplicator::new(LogBuffer::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let key = "recent-key".to_owned();

        let first_calls = calls.clone();
        let first = dedup
            .run(key.clone(), "Test", 1, "unit", move || async move {
                first_calls.fetch_add(1, Ordering::SeqCst);
                Ok::<String, HostError>("ok".to_owned())
            })
            .await
            .unwrap();

        let second_calls = calls.clone();
        let second = dedup
            .run(key, "Test", 2, "unit", move || async move {
                second_calls.fetch_add(1, Ordering::SeqCst);
                Ok::<String, HostError>("miss".to_owned())
            })
            .await
            .unwrap();

        assert_eq!(first, "ok");
        assert_eq!(second, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
