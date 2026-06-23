use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Local, Utc};
use parking_lot::{Mutex as ParkingMutex, RwLock};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::cli::{CliErrorType, GeminiCliClient, GeminiCliError, describe_error};
use crate::fast_client::{self, FastGenerationConfig, FastGenerationOptions};
use crate::ivlyrics;
use crate::logging::{LogBuffer, summarize_text};
use crate::model_catalog;
use crate::request_guard::{RequestGuard, RequestGuardError};
use crate::settings::AppSettings;
use crate::web_backend::{WebAutomationBackend, WebBackendError, WebProfileResetResult};

const GENERAL_CLI_PROMPT_TOKEN_BUDGET: usize = 8000;
const APPROX_CHARS_PER_TOKEN: usize = 4;
const GENERAL_CLI_PROMPT_CHAR_BUDGET: usize =
    GENERAL_CLI_PROMPT_TOKEN_BUDGET * APPROX_CHARS_PER_TOKEN;
const INPUT_LINES_START_MARKER: &str = "INPUT_LINES_START";
const INPUT_LINES_END_MARKER: &str = "INPUT_LINES_END";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranslationMode {
    WebView,
    GeminiCli,
    ChatGptWebView,
}

impl TranslationMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::WebView => "Gemini WebView",
            Self::GeminiCli => "Gemini CLI",
            Self::ChatGptWebView => "ChatGPT WebView",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebViewProvider {
    Current,
    Gemini,
    ChatGpt,
}

impl WebViewProvider {
    fn mode(self, current: TranslationMode) -> TranslationMode {
        match self {
            Self::Current => current,
            Self::Gemini => TranslationMode::WebView,
            Self::ChatGpt => TranslationMode::ChatGptWebView,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Gemini => "gemini",
            Self::ChatGpt => "chatgpt",
        }
    }
}

fn should_fallback_ivlyrics_study_to_webview(error: &GeminiCliError) -> bool {
    IvLyricsStudyCliLimitGuard::should_fallback_to_webview(error)
}

fn should_preload_ivlyrics_study_cli(settings: &AppSettings, mode: TranslationMode) -> bool {
    settings.iv_lyrics_study_cli_direct_enabled
        && matches!(
            mode,
            TranslationMode::WebView | TranslationMode::ChatGptWebView
        )
}

struct IvLyricsStudyCliLimitGuard {
    path: Option<PathBuf>,
    inner: ParkingMutex<IvLyricsStudyCliLimitState>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct IvLyricsStudyCliLimitState {
    recent_hits_utc: VecDeque<DateTime<Utc>>,
    cooldown_until_utc: Option<DateTime<Utc>>,
    last_reason: String,
    total_hits: u64,
}

impl IvLyricsStudyCliLimitGuard {
    const HIT_WINDOW_MINUTES: i64 = 10;
    const TRANSIENT_COOLDOWN_MINUTES: i64 = 5;
    const DAILY_QUOTA_COOLDOWN_HOURS: i64 = 6;

    fn new(path: PathBuf, logs: &LogBuffer) -> Self {
        let state = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<IvLyricsStudyCliLimitState>(&text).ok())
            .unwrap_or_default();
        if state.cooldown_until_utc.is_some() {
            logs.push(format!(
                "[IvLyricsStudyCliLimitGuard] 저장된 CLI limit 상태 로드 (path={})",
                path.display()
            ));
        }
        Self {
            path: Some(path),
            inner: ParkingMutex::new(state),
        }
    }

    #[cfg(test)]
    fn in_memory() -> Self {
        Self {
            path: None,
            inner: ParkingMutex::new(IvLyricsStudyCliLimitState::default()),
        }
    }

    fn try_get_fallback_cooldown(&self) -> Option<(Duration, String)> {
        let now = Utc::now();
        let state = self.inner.lock();
        let cooldown_until = state.cooldown_until_utc?;
        if cooldown_until <= now {
            return None;
        }

        let remaining = (cooldown_until - now)
            .to_std()
            .unwrap_or_else(|_| Duration::from_secs(0));
        Some((remaining, state.last_reason.clone()))
    }

    fn record_success(&self) {
        let snapshot = {
            let mut state = self.inner.lock();
            let now = Utc::now();
            state.recent_hits_utc.clear();
            if state
                .cooldown_until_utc
                .map(|cooldown_until| cooldown_until <= now)
                .unwrap_or(true)
            {
                state.cooldown_until_utc = None;
                state.last_reason.clear();
            }
            state.clone()
        };
        self.save_snapshot(&snapshot);
    }

    fn record_rate_limit(&self, reason: &str, logs: &LogBuffer) {
        let now = Utc::now();
        let reason = if reason.trim().is_empty() {
            "Gemini CLI rate limit"
        } else {
            reason.trim()
        };
        let daily_quota = Self::looks_like_daily_quota(reason);
        let cooldown_until = now
            + if daily_quota {
                ChronoDuration::hours(Self::DAILY_QUOTA_COOLDOWN_HOURS)
            } else {
                ChronoDuration::minutes(Self::TRANSIENT_COOLDOWN_MINUTES)
            };

        let (hits10m, total_hits, cooldown_until_local, reason_summary, snapshot) = {
            let mut state = self.inner.lock();
            Self::prune_old_hits(&mut state, now);
            state.recent_hits_utc.push_back(now);
            state.total_hits += 1;
            if state
                .cooldown_until_utc
                .map(|current| cooldown_until > current)
                .unwrap_or(true)
            {
                state.cooldown_until_utc = Some(cooldown_until);
            }
            state.last_reason = reason.to_owned();

            let cooldown_until_local = state
                .cooldown_until_utc
                .unwrap_or(cooldown_until)
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string();
            (
                state.recent_hits_utc.len(),
                state.total_hits,
                cooldown_until_local,
                summarize_text(&state.last_reason, 160),
                state.clone(),
            )
        };
        self.save_snapshot(&snapshot);

        logs.push(format!(
            "[IvLyricsStudyCliLimitGuard] CLI limit hit 기록 (daily={}, hits10m={}, total={}, cooldownUntilLocal={}, reason={})",
            daily_quota, hits10m, total_hits, cooldown_until_local, reason_summary
        ));
    }

    fn should_fallback_to_webview(error: &GeminiCliError) -> bool {
        if error.error_type == CliErrorType::RateLimited {
            return true;
        }

        Self::looks_like_rate_limit(&error.message)
    }

    fn looks_like_rate_limit(text: &str) -> bool {
        Self::contains_any(
            text,
            &[
                "rate limit",
                "too many requests",
                "http 429",
                "status 429",
                " 429",
                "resource exhausted",
                "resource_exhausted",
                "quota",
                "limit hit",
                "limit exceeded",
                "no capacity available",
                "capacity issues",
                "요청 제한",
                "요청 한도",
                "사용량 한도",
                "쿼타",
            ],
        )
    }

    fn looks_like_daily_quota(text: &str) -> bool {
        Self::contains_any(
            text,
            &[
                "daily",
                "per day",
                "perday",
                "requests/day",
                "requests per day",
                "request per day",
                "daily limit",
                "일일",
                "하루",
            ],
        )
    }

    fn prune_old_hits(state: &mut IvLyricsStudyCliLimitState, now: DateTime<Utc>) {
        let cutoff = now - ChronoDuration::minutes(Self::HIT_WINDOW_MINUTES);
        while state
            .recent_hits_utc
            .front()
            .map(|hit| *hit < cutoff)
            .unwrap_or(false)
        {
            state.recent_hits_utc.pop_front();
        }
    }

    fn contains_any(text: &str, markers: &[&str]) -> bool {
        if text.trim().is_empty() {
            return false;
        }

        let text = text.to_lowercase();
        markers.iter().any(|marker| text.contains(marker))
    }

    fn save_snapshot(&self, snapshot: &IvLyricsStudyCliLimitState) {
        let Some(path) = &self.path else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        if let Ok(json) = serde_json::to_string_pretty(snapshot) {
            let _ = std::fs::write(path, json);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
enum HostRequestPriority {
    Normal,
    High,
    Immediate,
}

struct PriorityGate {
    state: ParkingMutex<PriorityGateState>,
}

#[derive(Default)]
struct PriorityGateState {
    active: bool,
    next_waiter_id: u64,
    granted_waiter_id: Option<u64>,
    immediate_queue: VecDeque<PriorityGateWaiterEntry>,
    high_queue: VecDeque<PriorityGateWaiterEntry>,
    normal_queue: VecDeque<PriorityGateWaiterEntry>,
}

struct PriorityGateWaiterEntry {
    id: u64,
    notify: Arc<Notify>,
}

struct PriorityGatePermit {
    gate: Arc<PriorityGate>,
}

struct PriorityGateWaiter {
    gate: Arc<PriorityGate>,
    priority: HostRequestPriority,
    id: u64,
    notify: Arc<Notify>,
    queued: bool,
}

impl PriorityGate {
    fn new() -> Self {
        Self {
            state: ParkingMutex::new(PriorityGateState::default()),
        }
    }

    async fn acquire(self: &Arc<Self>, priority: HostRequestPriority) -> PriorityGatePermit {
        let Some((id, notify)) = self.add_waiter(priority) else {
            return PriorityGatePermit {
                gate: Arc::clone(self),
            };
        };

        let mut waiter = PriorityGateWaiter {
            gate: Arc::clone(self),
            priority,
            id,
            notify,
            queued: true,
        };

        loop {
            waiter.notify.notified().await;
            if self.accept_grant(waiter.id) {
                waiter.queued = false;
                return PriorityGatePermit {
                    gate: Arc::clone(self),
                };
            }
        }
    }

    fn add_waiter(&self, priority: HostRequestPriority) -> Option<(u64, Arc<Notify>)> {
        let mut state = self.state.lock();
        if !state.active {
            state.active = true;
            return None;
        }

        let id = state.next_waiter_id;
        state.next_waiter_id = state.next_waiter_id.wrapping_add(1);
        let notify = Arc::new(Notify::new());
        let entry = PriorityGateWaiterEntry {
            id,
            notify: Arc::clone(&notify),
        };
        match priority {
            HostRequestPriority::Immediate => state.immediate_queue.push_back(entry),
            HostRequestPriority::High => state.high_queue.push_back(entry),
            HostRequestPriority::Normal => state.normal_queue.push_back(entry),
        }
        Some((id, notify))
    }

    fn accept_grant(&self, id: u64) -> bool {
        let mut state = self.state.lock();
        if state.granted_waiter_id == Some(id) {
            state.granted_waiter_id = None;
            return true;
        }
        false
    }

    fn release(&self) {
        let notify = {
            let mut state = self.state.lock();
            if let Some(entry) = Self::pop_next_waiter_locked(&mut state) {
                state.granted_waiter_id = Some(entry.id);
                Some(entry.notify)
            } else {
                state.active = false;
                None
            }
        };

        if let Some(notify) = notify {
            notify.notify_one();
        }
    }

    fn cancel_waiter(&self, priority: HostRequestPriority, id: u64) -> bool {
        let mut release_needed = false;
        {
            let mut state = self.state.lock();
            if Self::remove_waiter_locked(&mut state, priority, id) {
                return false;
            }
            if state.granted_waiter_id == Some(id) {
                state.granted_waiter_id = None;
                release_needed = true;
            }
        }
        release_needed
    }

    fn pop_next_waiter_locked(state: &mut PriorityGateState) -> Option<PriorityGateWaiterEntry> {
        state
            .immediate_queue
            .pop_front()
            .or_else(|| state.high_queue.pop_front())
            .or_else(|| state.normal_queue.pop_front())
    }

    fn remove_waiter_locked(
        state: &mut PriorityGateState,
        priority: HostRequestPriority,
        id: u64,
    ) -> bool {
        let queue = match priority {
            HostRequestPriority::Immediate => &mut state.immediate_queue,
            HostRequestPriority::High => &mut state.high_queue,
            HostRequestPriority::Normal => &mut state.normal_queue,
        };
        let Some(index) = queue.iter().position(|entry| entry.id == id) else {
            return false;
        };
        queue.remove(index);
        true
    }
}

impl Drop for PriorityGatePermit {
    fn drop(&mut self) {
        self.gate.release();
    }
}

impl Drop for PriorityGateWaiter {
    fn drop(&mut self) {
        if self.queued && self.gate.cancel_waiter(self.priority, self.id) {
            self.gate.release();
        }
    }
}

#[derive(Clone)]
pub struct TranslatorHost {
    settings: Arc<RwLock<AppSettings>>,
    logs: LogBuffer,
    mode: Arc<RwLock<TranslationMode>>,
    request_guard: RequestGuard,
    translate_lock: Arc<PriorityGate>,
    ivlyrics_limit_guard: Arc<IvLyricsStudyCliLimitGuard>,
    ivlyrics_study_cli_preload_inflight: Arc<AtomicBool>,
    web_backend: WebAutomationBackend,
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum HostError {
    #[error("{0}")]
    Cli(#[from] GeminiCliError),
    #[error("{0}")]
    Guard(#[from] RequestGuardError),
    #[error("{0}")]
    Web(#[from] WebBackendError),
    #[error("{0}")]
    Internal(String),
}

impl HostError {
    pub fn status_code(&self) -> u16 {
        match self {
            Self::Cli(error) => error.suggested_http_status,
            Self::Guard(_) => 503,
            Self::Web(error) => error.status,
            Self::Internal(_) => 500,
        }
    }
}

impl TranslatorHost {
    pub fn new(
        settings: Arc<RwLock<AppSettings>>,
        logs: LogBuffer,
        mode: TranslationMode,
        webview_profile_root: PathBuf,
        ivlyrics_limit_guard_path: PathBuf,
    ) -> Self {
        Self {
            settings,
            logs: logs.clone(),
            mode: Arc::new(RwLock::new(mode)),
            request_guard: RequestGuard::default(),
            translate_lock: Arc::new(PriorityGate::new()),
            ivlyrics_limit_guard: Arc::new(IvLyricsStudyCliLimitGuard::new(
                ivlyrics_limit_guard_path,
                &logs,
            )),
            ivlyrics_study_cli_preload_inflight: Arc::new(AtomicBool::new(false)),
            web_backend: WebAutomationBackend::new(logs, webview_profile_root),
        }
    }

    pub async fn start(&self) -> anyhow::Result<()> {
        let mode = *self.mode.read();
        self.logs.push(format!("[Host] {} 모드 시작", mode.label()));
        if matches!(
            mode,
            TranslationMode::WebView | TranslationMode::ChatGptWebView
        ) {
            self.web_backend.start(mode).await?;
        }
        if mode == TranslationMode::GeminiCli && self.settings.read().maximum_usage_mode_enabled {
            self.web_backend.start(TranslationMode::WebView).await?;
        }
        self.start_ivlyrics_study_cli_preload_if_needed(mode);
        Ok(())
    }

    pub async fn reset_webview_profiles(&self) -> Result<WebProfileResetResult, HostError> {
        let mode = self.mode();
        let restart_mode = matches!(
            mode,
            TranslationMode::WebView | TranslationMode::ChatGptWebView
        )
        .then_some(mode);

        if restart_mode.is_some() {
            self.activate_request_guard("webview-profile-reset");
        }

        self.logs.push(format!(
            "[Host] WebView profile reset start (restartMode={})",
            restart_mode.map(TranslationMode::label).unwrap_or("none")
        ));

        let _permit = tokio::time::timeout(
            Duration::from_secs(30),
            self.translate_lock.acquire(HostRequestPriority::Immediate),
        )
        .await
        .map_err(|_| {
            HostError::Internal(
                "진행 중인 WebView 요청이 끝나지 않아 프로필 초기화를 중단했습니다.".to_owned(),
            )
        })?;

        let result = self.web_backend.reset_profiles(restart_mode).await?;
        self.logs.push(format!(
            "[Host] WebView profile reset complete (deletedExistingData={}, path={})",
            result.deleted_existing_data,
            result.webview_data_dir.display()
        ));
        Ok(result)
    }

    pub fn mode(&self) -> TranslationMode {
        *self.mode.read()
    }

    pub fn raw_prompt_mode(&self) -> bool {
        self.settings.read().raw_prompt_mode
    }

    #[allow(dead_code)]
    pub async fn translate(&self, text: &str, timeout: Duration) -> Result<String, HostError> {
        if self.raw_prompt_mode() {
            return self.send_raw_prompt(text, timeout).await;
        }
        self.forward_to_backend(text, timeout, None).await
    }

    #[allow(dead_code)]
    pub async fn translate_with_model(
        &self,
        text: &str,
        timeout: Duration,
        model_override: Option<&str>,
    ) -> Result<String, HostError> {
        if self.raw_prompt_mode() {
            return self
                .send_raw_prompt_with_model(text, timeout, model_override)
                .await;
        }
        self.forward_to_backend(text, timeout, model_override).await
    }

    pub async fn send_raw_prompt(
        &self,
        prompt: &str,
        timeout: Duration,
    ) -> Result<String, HostError> {
        self.forward_to_backend(prompt, timeout, None).await
    }

    pub async fn send_raw_prompt_with_model(
        &self,
        prompt: &str,
        timeout: Duration,
        model_override: Option<&str>,
    ) -> Result<String, HostError> {
        self.forward_to_backend(prompt, timeout, model_override)
            .await
    }

    pub async fn send_raw_prompt_to_webview_provider(
        &self,
        provider: WebViewProvider,
        prompt: &str,
        timeout: Duration,
    ) -> Result<String, HostError> {
        if provider == WebViewProvider::Current {
            return self.send_raw_prompt(prompt, timeout).await;
        }

        self.request_guard.throw_if_active()?;
        let _guard = self
            .translate_lock
            .acquire(HostRequestPriority::Normal)
            .await;
        self.request_guard.throw_if_active()?;

        let settings = self.settings.read().clone();
        let mode = provider.mode(self.mode());
        self.logs.push(format!(
            "[Host] forced WebView provider route provider={} mode={}",
            provider.label(),
            mode.label()
        ));
        self.send_web_backend_once(mode, prompt, timeout, &settings)
            .await
    }

    pub async fn send_cli_raw_prompt(
        &self,
        prompt: &str,
        timeout: Duration,
        model_override: Option<&str>,
    ) -> Result<String, HostError> {
        self.request_guard.throw_if_active()?;
        let _guard = self
            .translate_lock
            .acquire(HostRequestPriority::Normal)
            .await;
        self.request_guard.throw_if_active()?;

        let settings = self.settings.read().clone();
        let selected_model = model_override
            .and_then(model_catalog::find_cli)
            .map(|model| model.id.to_owned())
            .unwrap_or_else(|| model_catalog::normalize_cli_model(&settings.gemini_cli_model));
        let client = GeminiCliClient::new(
            selected_model,
            timeout.as_secs().max(settings.gemini_cli_timeout_seconds),
        )
        .with_fast_wrapper_from_settings(&settings)
        .with_max_output_tokens(ivlyrics_raw_max_output_tokens(prompt));

        let result = client.send_prompt(prompt).await;
        match &result {
            Ok(text) => self.logs.push(format!(
                "[Host] MORT/custom CLI raw 응답 수신 (len={})",
                text.len()
            )),
            Err(error) => self.logs.push(format!(
                "[Host] MORT/custom CLI raw 오류: {}",
                describe_error(error)
            )),
        }
        Ok(result?)
    }

    #[allow(dead_code)]
    pub async fn send_ivlyrics_study_prompt_with_webview_limit_fallback(
        &self,
        prompt: &str,
        timeout: Duration,
    ) -> Result<String, HostError> {
        self.send_ivlyrics_direct_prompt_with_webview_limit_fallback(
            prompt,
            timeout,
            ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz,
        )
        .await
    }

    pub async fn send_ivlyrics_direct_prompt_with_webview_limit_fallback(
        &self,
        prompt: &str,
        timeout: Duration,
        kind: ivlyrics::IvLyricsPromptKind,
    ) -> Result<String, HostError> {
        self.request_guard.throw_if_active()?;

        let settings = self.settings.read().clone();
        let mode = self.mode();
        let route_label = ivlyrics_direct_route_label(kind);
        if let Some((remaining, reason)) = self.ivlyrics_limit_guard.try_get_fallback_cooldown() {
            self.logs.push(format!(
                "[Host] {route_label} CLI cooldown 중 - WebView fallback 직접 사용 ({:.0}s 남음, reason={})",
                remaining.as_secs_f32(),
                summarize_text(&reason, 160)
            ));
            return self
                .send_ivlyrics_study_prompt_via_serial_webview_fallback(
                    mode, prompt, timeout, &settings,
                )
                .await;
        }

        let timeout_seconds = timeout
            .as_secs()
            .max(settings.gemini_cli_timeout_seconds)
            .max(180);
        let client = GeminiCliClient::new(
            model_catalog::normalize_cli_model(&settings.gemini_cli_model),
            timeout_seconds,
        )
        .with_fast_wrapper_from_settings(&settings)
        .with_max_output_tokens(ivlyrics_study_max_output_tokens(prompt))
        .with_bypass_request_gate(true)
        .with_respect_fast_wrapper_cooldown(false)
        .with_retry_attempts(1)
        .with_fast_wrapper_native_fallback(false)
        .with_fast_wrapper_http_max_attempts(1)
        .with_fast_wrapper_empty_response_max_attempts(1)
        .with_working_dir(ivlyrics_study_cli_working_dir());

        self.logs.push(format!(
            "[Host] {route_label} fast lane -> Gemini CLI raw 직접 호출 (mode={}, timeout={}s, gates=off, cooldown=guarded, wrapperRetry=off, nativeFallback=off, WebViewFallback=limit)",
            mode.label(),
            timeout_seconds
        ));
        let result = client.send_prompt(prompt).await;
        match result {
            Ok(text) => {
                self.ivlyrics_limit_guard.record_success();
                self.logs.push(format!(
                    "[Host] {route_label} CLI 응답 수신 (len={})",
                    text.len()
                ));
                Ok(text)
            }
            Err(error) if should_fallback_ivlyrics_study_to_webview(&error) => {
                self.ivlyrics_limit_guard
                    .record_rate_limit(&error.message, &self.logs);
                if mode == TranslationMode::GeminiCli {
                    self.logs.push(format!(
                        "[Host] {route_label} CLI limit 감지 - 현재 Gemini CLI 모드라 WebView fallback 생략 ({})",
                        describe_error(&error)
                    ));
                    Err(error.into())
                } else {
                    self.logs.push(format!(
                        "[Host] {route_label} CLI limit 감지 - WebView fallback 시작 ({})",
                        describe_error(&error)
                    ));
                    self.send_ivlyrics_study_prompt_via_serial_webview_fallback(
                        mode, prompt, timeout, &settings,
                    )
                    .await
                }
            }
            Err(error) => {
                self.logs.push(format!(
                    "[Host] {route_label} CLI 오류: {}",
                    describe_error(&error)
                ));
                Err(error.into())
            }
        }
    }

    fn start_ivlyrics_study_cli_preload_if_needed(&self, mode: TranslationMode) {
        let settings = self.settings.read().clone();
        if !should_preload_ivlyrics_study_cli(&settings, mode) {
            return;
        }

        if self
            .ivlyrics_study_cli_preload_inflight
            .swap(true, Ordering::SeqCst)
        {
            return;
        }

        let host = self.clone();
        tokio::spawn(async move {
            host.preload_ivlyrics_study_cli(settings).await;
            host.ivlyrics_study_cli_preload_inflight
                .store(false, Ordering::SeqCst);
        });
    }

    async fn preload_ivlyrics_study_cli(&self, settings: AppSettings) {
        let model = model_catalog::normalize_cli_model(&settings.gemini_cli_model);
        let timeout_seconds = settings.gemini_cli_timeout_seconds.max(180);
        if let Some((remaining, reason)) = self.ivlyrics_limit_guard.try_get_fallback_cooldown() {
            self.logs.push(format!(
                "[Host] ivLyrics study CLI preload 생략 - cooldown 중 ({:.0}s 남음, reason={})",
                remaining.as_secs_f32(),
                summarize_text(&reason, 160)
            ));
            return;
        }

        self.logs.push(format!(
            "[Host] ivLyrics study CLI preload 시작 (model={}, fastWrapper={})",
            model, settings.gemini_cli_use_fast_wrapper
        ));

        if settings.gemini_cli_use_fast_wrapper {
            let mut options = FastGenerationOptions::new(
                model.clone(),
                "Reply with OK only.",
                Duration::from_secs(timeout_seconds.min(120)),
                FastGenerationConfig::from_settings(&settings),
            );
            options.respect_code_assist_cooldown = true;
            options.bypass_generate_gate = true;
            options.max_output_tokens = Some(128);
            options.http_max_attempts = 1;
            options.empty_response_max_attempts = 1;

            let fast_result = fast_client::try_generate(options).await;
            if fast_result.success && !fast_result.text.trim().is_empty() {
                self.logs.push(format!(
                    "[Host] ivLyrics study fast wrapper preload 완료 (source={})",
                    fast_result.source
                ));
            } else {
                self.logs.push(format!(
                    "[Host] ivLyrics study fast wrapper preload 실패: {}",
                    summarize_text(&fast_result.error, 180)
                ));
            }
        }

        let client = GeminiCliClient::new(model, timeout_seconds)
            .with_max_output_tokens(Some(16))
            .with_bypass_request_gate(true)
            .with_respect_fast_wrapper_cooldown(false)
            .with_retry_attempts(1)
            .with_fast_wrapper_native_fallback(false)
            .with_fast_wrapper_http_max_attempts(1)
            .with_fast_wrapper_empty_response_max_attempts(1)
            .with_working_dir(ivlyrics_study_cli_working_dir());

        match client.validate_readiness().await {
            Ok(message) => self.logs.push(format!(
                "[Host] ivLyrics study CLI preload 완료 ({message})"
            )),
            Err(error) => self.logs.push(format!(
                "[Host] ivLyrics study CLI preload 실패: {}",
                describe_error(&error)
            )),
        }
    }

    async fn send_ivlyrics_study_prompt_via_serial_webview_fallback(
        &self,
        mode: TranslationMode,
        prompt: &str,
        timeout: Duration,
        settings: &AppSettings,
    ) -> Result<String, HostError> {
        let _guard = self
            .translate_lock
            .acquire(HostRequestPriority::Immediate)
            .await;
        self.logs
            .push("[Host] ivLyrics study WebView fallback priority=immediate");
        self.send_ivlyrics_study_prompt_via_webview_fallback(mode, prompt, timeout, settings)
            .await
    }

    async fn send_ivlyrics_study_prompt_via_webview_fallback(
        &self,
        mode: TranslationMode,
        prompt: &str,
        timeout: Duration,
        settings: &AppSettings,
    ) -> Result<String, HostError> {
        if mode == TranslationMode::GeminiCli {
            return Err(HostError::Cli(GeminiCliError {
                error_type: CliErrorType::RateLimited,
                message: "ivLyrics study Gemini CLI limit hit: 현재 Gemini CLI 모드라 WebView fallback을 사용할 수 없습니다."
                    .to_owned(),
                suggested_http_status: 429,
                retryable: true,
            }));
        }

        self.request_guard.throw_if_active()?;
        let web_timeout = Duration::from_secs(150).max(timeout);
        let web_result = self
            .web_backend
            .send_prompt(mode, prompt, web_timeout, settings)
            .await;
        match web_result {
            Ok(text) if !text.trim().is_empty() => {
                self.logs.push(format!(
                    "[Host] ivLyrics study WebView fallback 완료 (mode={}, len={})",
                    mode.label(),
                    text.len()
                ));
                Ok(text)
            }
            Ok(_) => Err(HostError::Cli(GeminiCliError {
                error_type: CliErrorType::EmptyResponse,
                message: "ivLyrics study WebView fallback 처리 실패: 빈 응답을 받았습니다."
                    .to_owned(),
                suggested_http_status: 503,
                retryable: true,
            })),
            Err(web_error) => Err(HostError::Web(web_error)),
        }
    }

    pub fn request_guard(&self) -> RequestGuard {
        self.request_guard.clone()
    }

    pub fn activate_request_guard(&self, source: &str) -> Duration {
        let remaining = self.request_guard.activate(source);
        if matches!(
            self.mode(),
            TranslationMode::WebView | TranslationMode::ChatGptWebView
        ) {
            self.web_backend.activate_request_guard(source, remaining);
        }
        self.logs.push(format!(
            "[RequestGuard] F12 guard 활성화 ({:.1}s)",
            remaining.as_secs_f32()
        ));
        remaining
    }

    pub async fn activate_request_guard_with_recovery(&self, source: &str) -> Duration {
        let mode = self.mode();
        let remaining = self.activate_request_guard(source);
        if matches!(
            mode,
            TranslationMode::WebView | TranslationMode::ChatGptWebView
        ) {
            self.logs.push(format!(
                "[RequestGuard] 현재 요청 취소 후 {:.1}s 대기, WebView 세션 복구 준비 ({})",
                remaining.as_secs_f32(),
                mode.label()
            ));
            tokio::time::sleep(remaining).await;
            let recovered = self.web_backend.recover_session(mode).await;
            self.logs.push(format!(
                "[RequestGuard] WebView 세션 복구 결과 (mode={}): {}",
                mode.label(),
                recovered
            ));
        }
        remaining
    }

    pub async fn request_session_recovery(&self) -> bool {
        let mode = self.mode();
        self.logs
            .push(format!("[Host] 세션 복구 요청 ({})", mode.label()));
        if matches!(
            mode,
            TranslationMode::WebView | TranslationMode::ChatGptWebView
        ) {
            self.web_backend.recover_session(mode).await
        } else {
            true
        }
    }

    pub async fn show_webview(&self) -> bool {
        self.set_webview_visibility(WebViewVisibilityAction::Show)
            .await
    }

    pub async fn hide_webview(&self) -> bool {
        self.set_webview_visibility(WebViewVisibilityAction::Hide)
            .await
    }

    pub async fn toggle_webview_visibility(&self) -> bool {
        self.set_webview_visibility(WebViewVisibilityAction::Toggle)
            .await
    }

    async fn set_webview_visibility(&self, action: WebViewVisibilityAction) -> bool {
        let mode = self.mode();
        if !matches!(
            mode,
            TranslationMode::WebView | TranslationMode::ChatGptWebView
        ) {
            return false;
        }

        match action.apply(&self.web_backend, mode).await {
            Ok(visible) => visible,
            Err(error) => {
                self.logs.push(format!(
                    "[Host] WebView 표시 상태 변경 실패 (mode={}): {}",
                    mode.label(),
                    error.message
                ));
                false
            }
        }
    }

    async fn forward_to_backend(
        &self,
        prompt: &str,
        timeout: Duration,
        model_override: Option<&str>,
    ) -> Result<String, HostError> {
        self.request_guard.throw_if_active()?;
        let _guard = self
            .translate_lock
            .acquire(HostRequestPriority::Normal)
            .await;
        self.request_guard.throw_if_active()?;

        let settings = self.settings.read().clone();
        let mode = self.mode();
        let maximum_usage = settings.maximum_usage_mode_enabled
            && matches!(mode, TranslationMode::WebView | TranslationMode::GeminiCli);

        if matches!(
            mode,
            TranslationMode::WebView | TranslationMode::ChatGptWebView
        ) {
            let result = self
                .send_web_backend_once(mode, prompt, timeout, &settings)
                .await;
            if maximum_usage && is_maximum_usage_fallback_candidate(result.as_ref().err()) {
                if let Err(error) = &result {
                    self.logs.push(format!(
                        "[Host] maximum usage WebView fallback to CLI: {error}"
                    ));
                }
                return self
                    .send_cli_backend_once(prompt, timeout, model_override, &settings)
                    .await;
            }
            return result;
        }

        let result = self
            .send_cli_backend_once(prompt, timeout, model_override, &settings)
            .await;
        if maximum_usage && is_maximum_usage_fallback_candidate(result.as_ref().err()) {
            if let Err(error) = &result {
                self.logs.push(format!(
                    "[Host] maximum usage CLI fallback to WebView: {error}"
                ));
            }
            return self
                .send_web_backend_once(TranslationMode::WebView, prompt, timeout, &settings)
                .await;
        }
        result
    }

    async fn send_web_backend_once(
        &self,
        mode: TranslationMode,
        prompt: &str,
        timeout: Duration,
        settings: &AppSettings,
    ) -> Result<String, HostError> {
        let result = self
            .web_backend
            .send_prompt(mode, prompt, timeout, settings)
            .await;
        match &result {
            Ok(text) => self.logs.push(format!(
                "[Host] WebView response received (mode={}, len={})",
                mode.label(),
                text.len()
            )),
            Err(error) => self.logs.push(format!(
                "[Host] WebView backend error (mode={}): {}",
                mode.label(),
                error.message
            )),
        }
        Ok(result?)
    }

    async fn send_cli_backend_once(
        &self,
        prompt: &str,
        timeout: Duration,
        model_override: Option<&str>,
        settings: &AppSettings,
    ) -> Result<String, HostError> {
        let selected_model = model_override
            .and_then(model_catalog::find_cli)
            .map(|model| model.id.to_owned())
            .unwrap_or_else(|| model_catalog::normalize_cli_model(&settings.gemini_cli_model));
        let client = GeminiCliClient::new(
            selected_model,
            timeout.as_secs().max(settings.gemini_cli_timeout_seconds),
        )
        .with_fast_wrapper_from_settings(settings)
        .with_max_output_tokens(ivlyrics_raw_max_output_tokens(prompt));

        if let Some(prompt_chunks) = try_build_general_cli_prompt_chunks(prompt) {
            let operation_timeout = scale_timeout(timeout, prompt_chunks.len());
            self.logs.push(format!(
                "[Host] general CLI prompt chunking start (chunks={}, budgetTokens~{}, originalTokens~{}, timeoutSec={:.0})",
                prompt_chunks.len(),
                GENERAL_CLI_PROMPT_TOKEN_BUDGET,
                estimate_tokens(prompt),
                operation_timeout.as_secs_f64()
            ));

            let send_chunks = async {
                let mut results = Vec::with_capacity(prompt_chunks.len());
                for (index, chunk) in prompt_chunks.iter().enumerate() {
                    self.logs.push(format!(
                        "[Host] general CLI chunk {}/{} send (tokens~{}, len={})",
                        index + 1,
                        prompt_chunks.len(),
                        estimate_tokens(chunk),
                        utf16_len(chunk)
                    ));
                    results.push(client.send_prompt(chunk).await?);
                }
                Ok::<_, GeminiCliError>(join_cli_chunk_results(&results))
            };

            let result = tokio::time::timeout(operation_timeout, send_chunks)
                .await
                .unwrap_or_else(|_| Err(gemini_cli_timeout_error()));
            match &result {
                Ok(text) => self.logs.push(format!(
                    "[Host] CLI chunked response received (mode={}, chunks={}, len={})",
                    self.mode().label(),
                    prompt_chunks.len(),
                    text.len()
                )),
                Err(error) => self.logs.push(format!(
                    "[Host] CLI chunked backend error: {}",
                    describe_error(error)
                )),
            }
            return Ok(result?);
        }

        let result = client.send_prompt(prompt).await;
        match &result {
            Ok(text) => self.logs.push(format!(
                "[Host] CLI response received (mode={}, len={})",
                self.mode().label(),
                text.len()
            )),
            Err(error) => self.logs.push(format!(
                "[Host] CLI backend error: {}",
                describe_error(error)
            )),
        }
        Ok(result?)
    }
}

fn gemini_cli_timeout_error() -> GeminiCliError {
    GeminiCliError {
        error_type: CliErrorType::Timeout,
        message: "Gemini CLI timeout".to_owned(),
        suggested_http_status: 504,
        retryable: true,
    }
}

fn scale_timeout(timeout: Duration, chunk_count: usize) -> Duration {
    if chunk_count <= 1 || timeout.is_zero() {
        return timeout;
    }

    let multiplier = chunk_count.min(12) as u32;
    timeout.saturating_mul(multiplier)
}

fn try_build_general_cli_prompt_chunks(prompt: &str) -> Option<Vec<String>> {
    if prompt.trim().is_empty()
        || estimate_tokens(prompt) <= GENERAL_CLI_PROMPT_TOKEN_BUDGET
        || ivlyrics::try_detect_lyrics_study_category(prompt).is_some()
    {
        return None;
    }

    let chunks = try_split_input_lines_prompt(prompt)?;
    if chunks.len() > 1 { Some(chunks) } else { None }
}

fn try_split_input_lines_prompt(prompt: &str) -> Option<Vec<String>> {
    let start_marker_index = prompt.find(INPUT_LINES_START_MARKER)?;
    let input_start = skip_line_break(prompt, start_marker_index + INPUT_LINES_START_MARKER.len());
    let relative_end = prompt[input_start..].find(INPUT_LINES_END_MARKER)?;
    let end_marker_index = input_start + relative_end;
    if end_marker_index <= input_start {
        return None;
    }

    let header = ensure_ends_with_line_break(&prompt[..input_start]);
    let footer = &prompt[end_marker_index..];
    let used_chars = utf16_len(&header) + utf16_len(footer);
    let available_chars = GENERAL_CLI_PROMPT_CHAR_BUDGET.checked_sub(used_chars)?;
    if available_chars < 1000 {
        return None;
    }

    let lines_block = trim_trailing_line_breaks(&prompt[input_start..end_marker_index]);
    let normalized = lines_block.replace("\r\n", "\n").replace('\r', "\n");
    let lines = normalized.split('\n').collect::<Vec<_>>();
    if lines.len() <= 1 {
        return None;
    }

    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut current_chars = 0usize;
    for line in lines {
        let line_chars = utf16_len(line) + 1;
        if !current.is_empty() && current_chars + line_chars > available_chars {
            chunks.push(build_input_lines_chunk(&header, footer, &current));
            current.clear();
            current_chars = 0;
        }

        current.push(line.to_owned());
        current_chars += line_chars;
    }

    if !current.is_empty() {
        chunks.push(build_input_lines_chunk(&header, footer, &current));
    }

    Some(chunks)
}

fn build_input_lines_chunk(header: &str, footer: &str, lines: &[String]) -> String {
    format!("{}{}\n{}", header, lines.join("\n"), footer)
}

fn join_cli_chunk_results(results: &[String]) -> String {
    results
        .iter()
        .map(|result| result.trim_end_matches(['\r', '\n']))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end_matches(['\r', '\n'])
        .to_owned()
}

fn estimate_tokens(text: &str) -> usize {
    let len = utf16_len(text);
    if len == 0 {
        0
    } else {
        len.div_ceil(APPROX_CHARS_PER_TOKEN).max(1)
    }
}

fn skip_line_break(value: &str, mut index: usize) -> usize {
    let bytes = value.as_bytes();
    if index < bytes.len() && bytes[index] == b'\r' {
        index += 1;
    }
    if index < bytes.len() && bytes[index] == b'\n' {
        index += 1;
    }
    index
}

fn ensure_ends_with_line_break(value: &str) -> String {
    if value.ends_with('\n') || value.ends_with('\r') {
        value.to_owned()
    } else {
        format!("{value}\n")
    }
}

fn trim_trailing_line_breaks(value: &str) -> &str {
    value.trim_end_matches(['\r', '\n'])
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn is_maximum_usage_fallback_candidate(error: Option<&HostError>) -> bool {
    match error {
        Some(HostError::Cli(error)) => matches!(
            error.error_type,
            CliErrorType::RateLimited
                | CliErrorType::Timeout
                | CliErrorType::ProcessCrash
                | CliErrorType::UpdateTransient
                | CliErrorType::EmptyResponse
                | CliErrorType::Unknown
        ),
        Some(HostError::Web(error)) => matches!(error.status, 429 | 503 | 504),
        Some(HostError::Internal(_)) => true,
        Some(HostError::Guard(_)) | None => false,
    }
}

fn ivlyrics_study_max_output_tokens(prompt: &str) -> Option<u32> {
    match ivlyrics::try_detect_lyrics_study_category(prompt).as_deref() {
        Some("summary") => Some(8192),
        Some("quiz" | "expressions" | "lines") => Some(16384),
        Some(_) => Some(16384),
        None => Some(16384),
    }
}

fn ivlyrics_direct_route_label(kind: ivlyrics::IvLyricsPromptKind) -> &'static str {
    match kind {
        ivlyrics::IvLyricsPromptKind::Phonetic => "ivLyrics pronunciation",
        ivlyrics::IvLyricsPromptKind::CharacterPronunciation => "ivLyrics character pronunciation",
        ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz => "ivLyrics study",
        ivlyrics::IvLyricsPromptKind::Translation => "ivLyrics translation",
    }
}

fn ivlyrics_study_cli_working_dir() -> PathBuf {
    std::env::temp_dir()
        .join("ruster")
        .join("ivlyrics-study-cli")
}

fn ivlyrics_raw_max_output_tokens(prompt: &str) -> Option<u32> {
    match ivlyrics::try_detect_kind(prompt) {
        Some(ivlyrics::IvLyricsPromptKind::Translation)
        | Some(ivlyrics::IvLyricsPromptKind::Phonetic)
        | Some(ivlyrics::IvLyricsPromptKind::CharacterPronunciation) => Some(16384),
        _ => Some(8192),
    }
}

enum WebViewVisibilityAction {
    Show,
    Hide,
    Toggle,
}

impl WebViewVisibilityAction {
    async fn apply(
        &self,
        backend: &WebAutomationBackend,
        mode: TranslationMode,
    ) -> Result<bool, WebBackendError> {
        match self {
            Self::Show => backend.show_window(mode).await,
            Self::Hide => backend.hide_window(mode).await,
            Self::Toggle => backend.toggle_window(mode).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ivlyrics_study_fallback_only_accepts_limit_like_cli_errors() {
        let limited = GeminiCliError {
            error_type: CliErrorType::RateLimited,
            message: "resource exhausted".to_owned(),
            suggested_http_status: 429,
            retryable: true,
        };
        let auth = GeminiCliError {
            error_type: CliErrorType::AuthExpired,
            message: "login required".to_owned(),
            suggested_http_status: 401,
            retryable: false,
        };

        assert!(should_fallback_ivlyrics_study_to_webview(&limited));
        assert!(!should_fallback_ivlyrics_study_to_webview(&auth));
    }

    #[test]
    fn ivlyrics_study_fallback_accepts_rate_limit_marker_set() {
        let marker_error = GeminiCliError {
            error_type: CliErrorType::Unknown,
            message: "HTTP 429: 요청 한도 초과, 쿼타 확인 필요".to_owned(),
            suggested_http_status: 500,
            retryable: true,
        };
        let unrelated_error = GeminiCliError {
            error_type: CliErrorType::Unknown,
            message: "login required".to_owned(),
            suggested_http_status: 401,
            retryable: false,
        };

        assert!(should_fallback_ivlyrics_study_to_webview(&marker_error));
        assert!(!should_fallback_ivlyrics_study_to_webview(&unrelated_error));
    }

    #[test]
    fn ivlyrics_study_cli_preload_matches_webview_direct_setting() {
        let mut settings = AppSettings::default();

        assert!(!should_preload_ivlyrics_study_cli(
            &settings,
            TranslationMode::WebView
        ));

        settings.iv_lyrics_study_cli_direct_enabled = true;
        assert!(should_preload_ivlyrics_study_cli(
            &settings,
            TranslationMode::WebView
        ));
        assert!(should_preload_ivlyrics_study_cli(
            &settings,
            TranslationMode::ChatGptWebView
        ));
        assert!(!should_preload_ivlyrics_study_cli(
            &settings,
            TranslationMode::GeminiCli
        ));
    }

    #[test]
    fn ivlyrics_limit_guard_records_transient_cooldown_and_success_keeps_active_cooldown() {
        let guard = IvLyricsStudyCliLimitGuard::in_memory();
        assert!(guard.try_get_fallback_cooldown().is_none());

        guard.record_rate_limit("HTTP 429 too many requests", &LogBuffer::new());
        let (remaining, reason) = guard.try_get_fallback_cooldown().unwrap();
        assert!(remaining <= Duration::from_secs(5 * 60));
        assert!(remaining > Duration::from_secs(4 * 60));
        assert_eq!(reason, "HTTP 429 too many requests");

        guard.record_success();
        assert!(guard.try_get_fallback_cooldown().is_some());
    }

    #[test]
    fn ivlyrics_limit_guard_uses_long_cooldown_for_daily_quota() {
        let guard = IvLyricsStudyCliLimitGuard::in_memory();
        guard.record_rate_limit("daily requests per day quota exceeded", &LogBuffer::new());

        let (remaining, _) = guard.try_get_fallback_cooldown().unwrap();
        assert!(remaining > Duration::from_secs(5 * 60 * 60));
    }

    #[test]
    fn ivlyrics_limit_guard_persists_cooldown_state() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("ruster-tests")
            .join(uuid::Uuid::new_v4().to_string());
        let path = dir.join("ivlyrics-study-cli-limit.json");
        let logs = LogBuffer::new();

        let guard = IvLyricsStudyCliLimitGuard::new(path.clone(), &logs);
        guard.record_rate_limit("HTTP 429 too many requests", &logs);

        let loaded = IvLyricsStudyCliLimitGuard::new(path.clone(), &logs);
        let (remaining, reason) = loaded.try_get_fallback_cooldown().unwrap();
        assert!(remaining > Duration::from_secs(4 * 60));
        assert_eq!(reason, "HTTP 429 too many requests");

        loaded.record_success();
        let cleared = IvLyricsStudyCliLimitGuard::new(path, &logs);
        assert!(cleared.try_get_fallback_cooldown().is_some());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn general_cli_prompt_chunking_matches_input_lines_contract() {
        let line = "0123456789".repeat(200);
        let lines = (0..40)
            .map(|index| format!("{index}:{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "HEADER\n{INPUT_LINES_START_MARKER}\n{lines}\n{INPUT_LINES_END_MARKER}\nFOOTER"
        );

        let chunks = try_build_general_cli_prompt_chunks(&prompt).unwrap();
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.starts_with("HEADER\nINPUT_LINES_START\n"))
        );
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.ends_with("INPUT_LINES_END\nFOOTER"))
        );
        assert!(
            chunks
                .iter()
                .all(|chunk| estimate_tokens(chunk) <= GENERAL_CLI_PROMPT_TOKEN_BUDGET + 1)
        );
    }

    #[test]
    fn general_cli_prompt_chunking_keeps_short_or_unknown_shapes_single() {
        assert!(try_build_general_cli_prompt_chunks("short").is_none());
        assert!(
            try_build_general_cli_prompt_chunks(&"x".repeat(GENERAL_CLI_PROMPT_CHAR_BUDGET + 10))
                .is_none()
        );
    }

    #[test]
    fn general_cli_chunk_join_trims_like_csharp_host() {
        let joined =
            join_cli_chunk_results(&["a\r\n".to_owned(), "b\n\n".to_owned(), "c".to_owned()]);

        assert_eq!(joined, "a\nb\nc");
    }

    #[tokio::test]
    async fn priority_gate_releases_immediate_then_high_then_normal() {
        let gate = Arc::new(PriorityGate::new());
        let first = gate.acquire(HostRequestPriority::Normal).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_normal_tx, release_normal_rx) = tokio::sync::oneshot::channel();
        let (release_high_tx, release_high_rx) = tokio::sync::oneshot::channel();
        let (release_immediate_tx, release_immediate_rx) = tokio::sync::oneshot::channel();

        spawn_gate_waiter(
            Arc::clone(&gate),
            HostRequestPriority::Normal,
            "normal",
            tx.clone(),
            release_normal_rx,
        );
        spawn_gate_waiter(
            Arc::clone(&gate),
            HostRequestPriority::High,
            "high",
            tx.clone(),
            release_high_rx,
        );
        spawn_gate_waiter(
            Arc::clone(&gate),
            HostRequestPriority::Immediate,
            "immediate",
            tx,
            release_immediate_rx,
        );
        tokio::task::yield_now().await;

        drop(first);
        assert_eq!(rx.recv().await, Some("immediate"));
        let _ = release_immediate_tx.send(());
        assert_eq!(rx.recv().await, Some("high"));
        let _ = release_high_tx.send(());
        assert_eq!(rx.recv().await, Some("normal"));
        let _ = release_normal_tx.send(());
    }

    #[tokio::test]
    async fn priority_gate_cancelled_waiter_does_not_block_next() {
        let gate = Arc::new(PriorityGate::new());
        let first = gate.acquire(HostRequestPriority::Normal).await;
        let cancelled = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move {
                let _permit = gate.acquire(HostRequestPriority::Immediate).await;
            }
        });
        tokio::task::yield_now().await;
        cancelled.abort();
        let _ = cancelled.await;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        spawn_gate_waiter(
            Arc::clone(&gate),
            HostRequestPriority::Normal,
            "next",
            tx,
            release_rx,
        );
        tokio::task::yield_now().await;

        drop(first);
        assert_eq!(rx.recv().await, Some("next"));
        let _ = release_tx.send(());
    }

    fn spawn_gate_waiter(
        gate: Arc<PriorityGate>,
        priority: HostRequestPriority,
        label: &'static str,
        tx: tokio::sync::mpsc::UnboundedSender<&'static str>,
        release_rx: tokio::sync::oneshot::Receiver<()>,
    ) {
        tokio::spawn(async move {
            let _permit = gate.acquire(priority).await;
            tx.send(label).unwrap();
            let _ = release_rx.await;
        });
    }
}
