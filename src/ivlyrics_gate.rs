use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::ivlyrics::{self, IvLyricsPromptKind};
use crate::logging::LogBuffer;

const PHONETIC_TRANSLATION_WAIT: Duration = Duration::from_secs(3);
const PHONETIC_TRANSLATION_POLL: Duration = Duration::from_millis(50);

#[derive(Clone)]
pub struct IvLyricsGate {
    inner: Arc<Mutex<GateState>>,
    logs: LogBuffer,
}

struct GateState {
    translations: HashMap<String, Arc<TranslationMarker>>,
}

struct TranslationMarker {
    done: AtomicBool,
    notify: Notify,
}

pub struct IvLyricsGateRequest {
    gate: Option<IvLyricsGate>,
    scope_key: String,
    marker: Option<Arc<TranslationMarker>>,
}

impl IvLyricsGate {
    pub fn new(logs: LogBuffer) -> Self {
        Self {
            inner: Arc::new(Mutex::new(GateState {
                translations: HashMap::new(),
            })),
            logs,
        }
    }

    pub async fn begin(
        &self,
        label: &str,
        request_id: u64,
        kind: Option<IvLyricsPromptKind>,
        prompt: &str,
    ) -> IvLyricsGateRequest {
        let Some(kind) = kind else {
            return IvLyricsGateRequest::empty();
        };
        let identity = ivlyrics::build_scope_identity(kind, prompt);

        match kind {
            IvLyricsPromptKind::Translation => {
                let marker = Arc::new(TranslationMarker {
                    done: AtomicBool::new(false),
                    notify: Notify::new(),
                });
                self.inner
                    .lock()
                    .translations
                    .insert(identity.key.clone(), marker.clone());
                self.logs.push(format!(
                    "[{label}#{request_id}] ivLyrics 번역 scope 시작 ({})",
                    identity.description
                ));
                IvLyricsGateRequest {
                    gate: Some(self.clone()),
                    scope_key: identity.key,
                    marker: Some(marker),
                }
            }
            IvLyricsPromptKind::Phonetic => {
                self.wait_for_matching_translation(
                    label,
                    request_id,
                    &identity.key,
                    &identity.description,
                )
                .await;
                IvLyricsGateRequest::empty()
            }
            IvLyricsPromptKind::LyricsStudyQuiz => IvLyricsGateRequest::empty(),
        }
    }

    async fn wait_for_matching_translation(
        &self,
        label: &str,
        request_id: u64,
        scope_key: &str,
        scope_description: &str,
    ) {
        let deadline = Instant::now() + PHONETIC_TRANSLATION_WAIT;
        loop {
            let marker = self.inner.lock().translations.get(scope_key).cloned();
            if let Some(marker) = marker {
                if marker.done.load(Ordering::SeqCst) {
                    return;
                }
                self.logs.push(format!(
                    "[{label}#{request_id}] ivLyrics 발음 요청 대기 - 같은 곡 번역 완료 후 처리 ({scope_description})"
                ));
                let notified = marker.notify.notified();
                if marker.done.load(Ordering::SeqCst) {
                    return;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return;
                }
                tokio::select! {
                    _ = notified => {}
                    _ = tokio::time::sleep(remaining) => {}
                }
                return;
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.logs.push(format!(
                    "[{label}#{request_id}] ivLyrics 발음 요청: 3000ms 내 같은 곡 번역 요청 없음 - 발음 먼저 처리 ({scope_description})"
                ));
                return;
            }
            tokio::time::sleep(remaining.min(PHONETIC_TRANSLATION_POLL)).await;
        }
    }
}

impl IvLyricsGateRequest {
    fn empty() -> Self {
        Self {
            gate: None,
            scope_key: String::new(),
            marker: None,
        }
    }
}

impl Drop for IvLyricsGateRequest {
    fn drop(&mut self) {
        let Some(gate) = &self.gate else {
            return;
        };
        let Some(marker) = &self.marker else {
            return;
        };
        marker.done.store(true, Ordering::SeqCst);
        marker.notify.notify_waiters();

        let mut state = gate.inner.lock();
        if state
            .translations
            .get(&self.scope_key)
            .map(|active| Arc::ptr_eq(active, marker))
            .unwrap_or(false)
        {
            state.translations.remove(&self.scope_key);
        }
    }
}
