use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex as ParkingMutex;
use serde_json::{Value, json};
use tokio::runtime::Handle;
use tokio::sync::Mutex;

use crate::host::TranslationMode;
use crate::logging::{LogBuffer, summarize_text};
use crate::settings::AppSettings;
use crate::webview2_native::{NativeWebView2Health, NativeWebView2Session};

const GEMINI_URL: &str = "https://gemini.google.com/app";
const CHATGPT_URL: &str = "https://chatgpt.com/";
const READY_WAIT: Duration = Duration::from_secs(75);
const MAX_ATTEMPTS: u8 = 2;
const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(2500);
const MIN_USAGE_REFRESH_COOLDOWN: Duration = Duration::from_secs(30);
const SESSION_REFRESH_COOLDOWN: Duration = Duration::from_secs(12);
const PURE_QUALITY_SESSION_REFRESH_COOLDOWN: Duration = Duration::from_secs(2);
const SESSION_REFRESH_READY_CHECK: Duration = Duration::from_secs(5);
const SESSION_REFRESH_STABILIZE: Duration = Duration::from_millis(1200);
const PROTECTION_BLOCK_COOLDOWN: Duration = Duration::from_secs(60);
const MAX_PROFILE_DELETE_ATTEMPTS: u8 = 10;
const PROFILE_RESET_SHUTDOWN_WAIT: Duration = Duration::from_millis(350);

#[derive(Clone)]
pub struct WebAutomationBackend {
    logs: LogBuffer,
    profile_root: PathBuf,
    current_request_id: Arc<AtomicU64>,
    idle_generation: Arc<AtomicU64>,
    session: Arc<Mutex<Option<BrowserSession>>>,
    launch_lock: Arc<Mutex<()>>,
    last_mode: Arc<ParkingMutex<Option<TranslationMode>>>,
    refresh_state: Arc<ParkingMutex<WebRefreshState>>,
}

struct BrowserSession {
    mode: TranslationMode,
    engine: WebView2Engine,
}

#[derive(Clone, Copy)]
enum WebWindowVisibility {
    Show,
    Hide,
    Toggle,
}

#[derive(Default)]
struct WebRefreshState {
    translation_count: u32,
    active_request_count: u32,
    last_usage_refresh: Option<Instant>,
    last_session_refresh: Option<Instant>,
    last_request_at: Option<Instant>,
    last_throttle_at: Option<Instant>,
    protection_blocked_until: Option<Instant>,
    idle_refresh_armed: bool,
    is_refreshing: bool,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct WebBackendError {
    pub message: String,
    pub status: u16,
    pub retryable: bool,
}

#[derive(Clone, Debug)]
pub struct WebProfileResetResult {
    pub deleted_existing_data: bool,
    pub webview_data_dir: PathBuf,
}

impl WebAutomationBackend {
    pub fn new(logs: LogBuffer, profile_root: PathBuf) -> Self {
        Self {
            logs,
            profile_root,
            current_request_id: Arc::new(AtomicU64::new(0)),
            idle_generation: Arc::new(AtomicU64::new(0)),
            session: Arc::new(Mutex::new(None)),
            launch_lock: Arc::new(Mutex::new(())),
            last_mode: Arc::new(ParkingMutex::new(None)),
            refresh_state: Arc::new(ParkingMutex::new(WebRefreshState::default())),
        }
    }

    pub async fn start(&self, mode: TranslationMode) -> Result<(), WebBackendError> {
        self.ensure_session(mode).await?;
        Ok(())
    }

    pub async fn reset_profiles(
        &self,
        restart_mode: Option<TranslationMode>,
    ) -> Result<WebProfileResetResult, WebBackendError> {
        let restart_mode = restart_mode.filter(|mode| Provider::from_mode(*mode).is_ok());

        let deleted_existing_data = {
            let _launch = self.launch_lock.lock().await;
            {
                let mut guard = self.session.lock().await;
                if guard.is_some() {
                    self.logs
                        .push("[WebView] profile reset: closing active session");
                    *guard = None;
                }
            }
            *self.last_mode.lock() = None;
            *self.refresh_state.lock() = WebRefreshState::default();

            tokio::time::sleep(PROFILE_RESET_SHUTDOWN_WAIT).await;
            reset_profile_root(&self.profile_root, &self.logs).await?
        };

        if let Some(mode) = restart_mode {
            self.ensure_session(mode).await?;
        }

        Ok(WebProfileResetResult {
            deleted_existing_data,
            webview_data_dir: self.profile_root.clone(),
        })
    }

    pub fn activate_request_guard(&self, source: &str, remaining: Duration) {
        self.current_request_id.fetch_add(1, Ordering::SeqCst);
        self.idle_generation.fetch_add(1, Ordering::SeqCst);
        {
            let mut state = self.refresh_state.lock();
            state.idle_refresh_armed = false;
            state.active_request_count = 0;
        }
        self.logs.push(format!(
            "[WebView] request guard 전파: current/pending 요청 취소 ({source})"
        ));

        if let Ok(handle) = Handle::try_current() {
            let this = self.clone();
            let source = source.to_owned();
            handle.spawn(async move {
                if let Err(error) = this.notify_page_request_guard(&source, remaining).await {
                    this.logs.push(format!(
                        "[WebView] request guard 페이지 알림 실패: {}",
                        error.message
                    ));
                }
            });
        } else {
            self.logs
                .push("[WebView] request guard 페이지 알림은 런타임 밖 호출이라 생략했습니다.");
        }
    }

    pub async fn send_prompt(
        &self,
        mode: TranslationMode,
        prompt: &str,
        timeout: Duration,
        settings: &AppSettings,
    ) -> Result<String, WebBackendError> {
        let request_id = self.current_request_id.fetch_add(1, Ordering::SeqCst) + 1;
        let raw_mode = settings.raw_prompt_mode || settings.web_view_raw_mode;
        self.logs.push(format!(
            "[WebView#{request_id}] {} 요청 시작 (raw={}, len={}, {})",
            mode.label(),
            raw_mode,
            prompt.len(),
            summarize_text(prompt, 100)
        ));

        if let Some(remaining) = self.protection_cooldown_remaining() {
            return Err(web_error(
                format!(
                    "{} 보호/요청 제한 cooldown 중입니다. {:.1}s 후 다시 시도하세요.",
                    mode.label(),
                    remaining.as_secs_f32()
                ),
                429,
                false,
            ));
        }

        let deadline = Instant::now() + timeout.max(Duration::from_secs(5));
        self.begin_request_scope().await;
        let result = async {
            self.refresh_for_idle_if_needed(mode, settings).await;
            self.refresh_for_usage_if_needed(mode, settings).await;

            let mut last_error = None;
            for attempt in 0..MAX_ATTEMPTS {
                self.ensure_session(mode).await?;
                let result = self
                    .send_prompt_once(mode, request_id, prompt, deadline, raw_mode, attempt)
                    .await;

                match result {
                    Ok(text) => {
                        self.logs.push(format!(
                            "[WebView#{request_id}] {} 응답 수신 (len={})",
                            mode.label(),
                            text.len()
                        ));
                        return Ok(text);
                    }
                    Err(error) if error.retryable && attempt + 1 < MAX_ATTEMPTS => {
                        self.logs.push(format!(
                            "[WebView#{request_id}] 재시도 준비: {}",
                            error.message
                        ));
                        let reset = error.status == 503
                            || error.message.contains("WebView2")
                            || error.message.contains("I/O");
                        last_error = Some(error);
                        if reset {
                            let _ = self
                                .restart_session(mode, "retryable backend failure")
                                .await;
                        } else {
                            let _ = self.reload(mode).await;
                        }
                    }
                    Err(error) => return Err(error),
                }
            }

            Err(last_error.unwrap_or_else(|| web_error("WebView 요청 실패", 503, true)))
        }
        .await;
        self.end_request_scope(mode, settings);
        result
    }

    pub async fn recover_session(&self, mode: TranslationMode) -> bool {
        match self.force_reload_with_state(mode).await {
            Ok(()) => true,
            Err(error) => {
                self.logs.push(format!(
                    "[WebView] {} reload 복구 실패: {}",
                    mode.label(),
                    error.message
                ));
                self.restart_session_with_cache_clear(mode, "manual recovery fallback")
                    .await
                    .is_ok()
            }
        }
    }

    pub async fn show_window(&self, mode: TranslationMode) -> Result<bool, WebBackendError> {
        self.set_window_visibility(mode, WebWindowVisibility::Show)
            .await
    }

    pub async fn hide_window(&self, mode: TranslationMode) -> Result<bool, WebBackendError> {
        self.set_window_visibility(mode, WebWindowVisibility::Hide)
            .await
    }

    pub async fn toggle_window(&self, mode: TranslationMode) -> Result<bool, WebBackendError> {
        self.set_window_visibility(mode, WebWindowVisibility::Toggle)
            .await
    }

    async fn send_prompt_once(
        &self,
        mode: TranslationMode,
        request_id: u64,
        prompt: &str,
        deadline: Instant,
        raw_mode: bool,
        attempt: u8,
    ) -> Result<String, WebBackendError> {
        let provider = Provider::from_mode(mode)?;
        let mut guard = self.session.lock().await;
        let session = guard
            .as_mut()
            .filter(|session| session.mode == mode)
            .ok_or_else(|| web_error("WebView 세션이 준비되지 않았습니다.", 503, true))?;

        session.engine.ensure_healthy(provider).await?;
        let _ = session.engine.call("Page.bringToFront", json!({})).await;
        session.engine.evaluate(&provider.ready_script()).await?;
        let baseline = session.engine.snapshot(provider).await?;
        if baseline.protection_blocked {
            self.mark_protection_blocked(provider, "send-baseline");
            return Err(web_error(
                format!("{} 보호/요청 제한 상태가 감지되었습니다.", provider.label()),
                429,
                false,
            ));
        }
        session
            .engine
            .evaluate(&provider.clear_input_script())
            .await?;
        session
            .engine
            .evaluate(&provider.inject_prompt_script(prompt))
            .await?;
        let send_result = session.engine.evaluate(&provider.send_script()).await?;
        let send_status = send_result.as_str().unwrap_or_default().to_owned();
        if !send_status.starts_with("sent_") {
            return Err(web_error(
                format!("WebView 전송 실패: {send_status}"),
                503,
                true,
            ));
        }

        self.logs.push(format!(
            "[WebView#{request_id}] {} 전송 완료 (attempt={}, status={send_status})",
            provider.label(),
            attempt + 1
        ));

        self.wait_for_response(session, provider, request_id, baseline, deadline, raw_mode)
            .await
    }

    async fn set_window_visibility(
        &self,
        mode: TranslationMode,
        visibility: WebWindowVisibility,
    ) -> Result<bool, WebBackendError> {
        let provider = Provider::from_mode(mode)?;
        self.ensure_session(mode).await?;
        let mut guard = self.session.lock().await;
        let session = guard
            .as_mut()
            .filter(|session| session.mode == mode)
            .ok_or_else(|| web_error("WebView 세션이 준비되지 않았습니다.", 503, true))?;
        let visible = session.engine.set_visibility(visibility).await?;
        self.logs.push(format!(
            "[WebView] {} 표시 상태 변경: {}",
            provider.label(),
            if visible { "visible" } else { "offscreen" }
        ));
        Ok(visible)
    }

    async fn begin_request_scope(&self) {
        let sleep_for = {
            let mut state = self.refresh_state.lock();
            state.active_request_count = state.active_request_count.saturating_add(1);
            state.idle_refresh_armed = true;
            state.last_request_at = Some(Instant::now());
            self.idle_generation.fetch_add(1, Ordering::SeqCst);

            let now = Instant::now();
            let wait = state.last_throttle_at.and_then(|last| {
                MIN_REQUEST_INTERVAL.checked_sub(now.saturating_duration_since(last))
            });
            state.last_throttle_at = Some(now + wait.unwrap_or_default());
            wait
        };

        if let Some(duration) = sleep_for
            && !duration.is_zero()
        {
            self.logs.push(format!(
                "[WebView] 요청 pacing 대기 ({:.1}s)",
                duration.as_secs_f32()
            ));
            tokio::time::sleep(duration).await;
        }
    }

    fn end_request_scope(&self, mode: TranslationMode, settings: &AppSettings) {
        let should_schedule_idle = {
            let mut state = self.refresh_state.lock();
            state.active_request_count = state.active_request_count.saturating_sub(1);
            state.last_request_at = Some(Instant::now());
            state.idle_refresh_armed
                && state.active_request_count == 0
                && !state.is_refreshing
                && !settings.web_view_raw_mode
        };

        if should_schedule_idle {
            self.schedule_idle_refresh(
                mode,
                settings.web_view_idle_refresh_seconds,
                settings.web_view_pure_quality_mode,
            );
        }
    }

    fn protection_cooldown_remaining(&self) -> Option<Duration> {
        let mut state = self.refresh_state.lock();
        let until = state.protection_blocked_until?;
        let now = Instant::now();
        if until <= now {
            state.protection_blocked_until = None;
            None
        } else {
            Some(until.saturating_duration_since(now))
        }
    }

    fn mark_protection_blocked(&self, provider: Provider, reason: &str) {
        let until = Instant::now() + PROTECTION_BLOCK_COOLDOWN;
        self.refresh_state.lock().protection_blocked_until = Some(until);
        self.current_request_id.fetch_add(1, Ordering::SeqCst);
        self.idle_generation.fetch_add(1, Ordering::SeqCst);
        self.logs.push(format!(
            "[WebView] {} 보호/요청 제한 cooldown 시작 ({reason}, {:.1}s)",
            provider.label(),
            PROTECTION_BLOCK_COOLDOWN.as_secs_f32()
        ));
    }

    async fn refresh_for_usage_if_needed(&self, mode: TranslationMode, settings: &AppSettings) {
        if settings.web_view_raw_mode {
            return;
        }

        let should_refresh = {
            let mut state = self.refresh_state.lock();
            let interval = usage_refresh_interval(settings);
            state.translation_count = state.translation_count.saturating_add(1);
            if state.translation_count < interval {
                false
            } else {
                let now = Instant::now();
                if interval > 1
                    && state
                        .last_usage_refresh
                        .map(|last| {
                            now.saturating_duration_since(last) < MIN_USAGE_REFRESH_COOLDOWN
                        })
                        .unwrap_or(false)
                {
                    state.translation_count = interval.saturating_sub(1);
                    false
                } else {
                    state.translation_count = 0;
                    state.last_usage_refresh = Some(now);
                    true
                }
            }
        };

        if should_refresh {
            self.logs.push(format!(
                "[WebView] {} 사용량 기준 세션 새로고침",
                mode.label()
            ));
            if let Err(error) = self
                .reload_with_state(mode, settings.web_view_pure_quality_mode)
                .await
            {
                self.logs.push(format!(
                    "[WebView] 사용량 기준 새로고침 생략/실패: {}",
                    error.message
                ));
            }
        }
    }

    async fn refresh_for_idle_if_needed(&self, mode: TranslationMode, settings: &AppSettings) {
        if settings.web_view_raw_mode {
            return;
        }

        let idle =
            Duration::from_secs(settings.web_view_idle_refresh_seconds.clamp(10, 600) as u64);
        let should_refresh = {
            let mut state = self.refresh_state.lock();
            if !state.idle_refresh_armed || state.active_request_count > 1 || state.is_refreshing {
                false
            } else {
                let elapsed = state
                    .last_request_at
                    .map(|last| last.elapsed())
                    .unwrap_or_default();
                if elapsed >= idle {
                    state.idle_refresh_armed = false;
                    true
                } else {
                    false
                }
            }
        };

        if should_refresh {
            self.logs
                .push(format!("[WebView] {} 유휴 세션 새로고침", mode.label()));
            if let Err(error) = self
                .reload_with_state(mode, settings.web_view_pure_quality_mode)
                .await
            {
                self.logs.push(format!(
                    "[WebView] 유휴 새로고침 생략/실패: {}",
                    error.message
                ));
            }
        }
    }

    fn schedule_idle_refresh(&self, mode: TranslationMode, idle_seconds: u32, pure_quality: bool) {
        let generation = self.idle_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let idle = Duration::from_secs(idle_seconds.clamp(10, 600) as u64);
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(idle).await;
            if this.idle_generation.load(Ordering::SeqCst) != generation {
                return;
            }

            let should_refresh = {
                let mut state = this.refresh_state.lock();
                if !state.idle_refresh_armed
                    || state.active_request_count > 0
                    || state.is_refreshing
                {
                    false
                } else {
                    let elapsed = state
                        .last_request_at
                        .map(|last| last.elapsed())
                        .unwrap_or_default();
                    if elapsed >= idle {
                        state.idle_refresh_armed = false;
                        true
                    } else {
                        false
                    }
                }
            };

            if should_refresh {
                this.logs.push(format!(
                    "[WebView] {} 유휴 타이머 세션 새로고침",
                    mode.label()
                ));
                if let Err(error) = this.reload_with_state(mode, pure_quality).await {
                    this.logs.push(format!(
                        "[WebView] 유휴 타이머 새로고침 실패: {}",
                        error.message
                    ));
                }
            }
        });
    }

    async fn wait_for_response(
        &self,
        session: &mut BrowserSession,
        provider: Provider,
        request_id: u64,
        baseline: ResponseSnapshot,
        deadline: Instant,
        raw_mode: bool,
    ) -> Result<String, WebBackendError> {
        let mut last_text = String::new();
        let mut last_change = Instant::now();
        let mut stable_count = 0u8;
        let mut new_response = baseline.text.is_empty();
        let mut poll = Duration::from_millis(180);
        let mut pending_resend_attempts = 0u8;
        let mut first_pending_prompt_at: Option<Instant> = None;
        let mut last_pending_resend_at: Option<Instant> = None;
        let mut login_recovery_attempts = 0u8;

        while Instant::now() < deadline {
            tokio::time::sleep(poll).await;
            if self.current_request_id.load(Ordering::SeqCst) != request_id {
                let _ = session
                    .engine
                    .evaluate(&provider.clear_input_script())
                    .await;
                return Ok("Retry_Stale".to_owned());
            }

            let snapshot = session.engine.snapshot(provider).await?;
            if snapshot.protection_blocked {
                self.mark_protection_blocked(provider, "wait-response");
                return Err(web_error(
                    format!("{} 보호/요청 제한 상태가 감지되었습니다.", provider.label()),
                    429,
                    false,
                ));
            }
            if snapshot.login_needed {
                if login_recovery_attempts < 2 {
                    login_recovery_attempts += 1;
                    match self
                        .handle_login_prompt(session, provider, request_id)
                        .await?
                    {
                        LoginPromptHandling::SoftDismissed | LoginPromptHandling::HardRecovered => {
                            last_change = Instant::now();
                            tokio::time::sleep(Duration::from_millis(450)).await;
                            continue;
                        }
                        LoginPromptHandling::HardRecoveryTriggered => {
                            return Err(web_error(
                                format!(
                                    "{} 로그인/세션 복구를 위해 reload가 필요합니다.",
                                    provider.label()
                                ),
                                503,
                                true,
                            ));
                        }
                        LoginPromptHandling::None => {
                            if matches!(
                                session.engine.evaluate(&provider.ready_script()).await,
                                Ok(Value::Bool(true))
                            ) {
                                continue;
                            }
                        }
                        LoginPromptHandling::LoginRequired => {}
                    }
                }

                return Err(web_error(
                    format!("{} 로그인 또는 세션 복구가 필요합니다.", provider.label()),
                    401,
                    false,
                ));
            }
            if snapshot.has_error && !snapshot.is_generating && snapshot.text.trim().is_empty() {
                return Err(web_error(
                    format!("{} 페이지 오류 상태가 감지되었습니다.", provider.label()),
                    503,
                    true,
                ));
            }
            if snapshot.is_stopped && !snapshot.is_generating {
                let _ = session
                    .engine
                    .evaluate(&provider.clear_input_script())
                    .await;
                return Err(web_error(
                    format!("{} 응답 중지 상태가 감지되었습니다.", provider.label()),
                    504,
                    true,
                ));
            }
            if !new_response {
                let count_increased = snapshot.count > baseline.count;
                let text_changed = !snapshot.text.is_empty() && snapshot.text != baseline.text;
                if !count_increased && !text_changed {
                    let prompt_state = session.engine.prompt_state(provider).await?;
                    if prompt_state.pending_send {
                        let pending_since =
                            *first_pending_prompt_at.get_or_insert_with(Instant::now);
                        let resend_due = last_pending_resend_at
                            .map(|last| last.elapsed() >= Duration::from_millis(1200))
                            .unwrap_or(true);
                        if provider == Provider::Gemini && pending_resend_attempts < 2 && resend_due
                        {
                            pending_resend_attempts += 1;
                            last_pending_resend_at = Some(Instant::now());
                            self.logs.push(format!(
                                "[WebView#{request_id}] pending prompt 재전송 {}/2 (len={}, preview={})",
                                pending_resend_attempts,
                                prompt_state.text_length,
                                summarize_text(&prompt_state.preview, 80)
                            ));
                            let _ = session.engine.evaluate(&provider.send_script()).await;
                            last_change = Instant::now();
                            continue;
                        }

                        if pending_since.elapsed() > Duration::from_secs(5) {
                            let _ = session
                                .engine
                                .evaluate(&provider.clear_input_script())
                                .await;
                            return Err(web_error(
                                "WebView 전송 수락 미완료로 요청을 재시도해야 합니다.",
                                504,
                                true,
                            ));
                        }
                    } else {
                        first_pending_prompt_at = None;
                    }

                    if !snapshot.is_generating && last_change.elapsed() > Duration::from_secs(30) {
                        return Err(web_error(
                            "WebView 새 응답을 감지하지 못했습니다.",
                            504,
                            true,
                        ));
                    }
                    continue;
                }
                new_response = true;
                last_change = Instant::now();
            }

            let text_changed = snapshot.text != last_text;
            if text_changed {
                last_text = snapshot.text.clone();
                last_change = Instant::now();
                stable_count = 0;
                poll = Duration::from_millis(180);
            } else if !snapshot.text.trim().is_empty() && !snapshot.is_generating {
                stable_count = stable_count.saturating_add(1);
            } else if snapshot.is_generating && last_change.elapsed() > Duration::from_secs(5) {
                poll = Duration::from_millis(900);
            }

            if snapshot.is_generating {
                if last_change.elapsed() > Duration::from_secs(75) {
                    return Err(web_error("WebView 생성이 정체되었습니다.", 504, true));
                }
                continue;
            }

            let required_stable = if raw_mode {
                1
            } else {
                adaptive_stable_count(&snapshot.text)
            };
            if stable_count >= required_stable && !snapshot.text.trim().is_empty() {
                let _ = session
                    .engine
                    .evaluate(&provider.clear_input_script())
                    .await;
                return Ok(sanitize_response(&snapshot.text));
            }

            if !snapshot.text.trim().is_empty() && last_change.elapsed() > Duration::from_secs(45) {
                let _ = session
                    .engine
                    .evaluate(&provider.clear_input_script())
                    .await;
                return Ok(sanitize_response(&snapshot.text));
            }
        }

        Err(web_error(
            "WebView 응답 대기 시간이 초과되었습니다.",
            504,
            true,
        ))
    }

    async fn handle_login_prompt(
        &self,
        session: &mut BrowserSession,
        provider: Provider,
        request_id: u64,
    ) -> Result<LoginPromptHandling, WebBackendError> {
        let state = session.engine.login_prompt_state(provider).await?;
        if !state.has_any_prompt {
            return Ok(if state.input_visible {
                LoginPromptHandling::None
            } else {
                LoginPromptHandling::LoginRequired
            });
        }

        self.logs.push(format!(
            "[WebView#{request_id}] {} 로그인/세션 프롬프트 감지 (signedOut={}, hard={}, nudge={}, loginButton={}, overlay={}, inputBlocked={}, detail={})",
            provider.label(),
            state.signed_out_dialog,
            state.needs_hard_recovery,
            state.login_nudge_banner,
            state.login_button,
            state.blocking_overlay,
            state.input_blocked,
            summarize_text(&state.detail, 120)
        ));

        if state.signed_out_dialog || state.needs_hard_recovery || state.input_blocked {
            let value = session
                .engine
                .evaluate(&provider.recover_signed_out_script())
                .await?;
            let success = value
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let action = value
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("none");
            self.logs.push(format!(
                "[WebView#{request_id}] {} hard recovery 결과 (success={}, action={})",
                provider.label(),
                success,
                action
            ));

            if success && !action.eq_ignore_ascii_case("needs_reload") {
                return Ok(LoginPromptHandling::HardRecovered);
            }
            return Ok(LoginPromptHandling::HardRecoveryTriggered);
        }

        let value = session
            .engine
            .evaluate(&provider.dismiss_login_prompts_script())
            .await?;
        self.logs.push(format!(
            "[WebView#{request_id}] {} 로그인 방해 요소 dismiss 결과: {}",
            provider.label(),
            summarize_text(value.to_string(), 160)
        ));
        Ok(LoginPromptHandling::SoftDismissed)
    }

    async fn reload(&self, mode: TranslationMode) -> Result<(), WebBackendError> {
        let provider = Provider::from_mode(mode)?;
        self.ensure_session(mode).await?;
        let mut guard = self.session.lock().await;
        let session = guard
            .as_mut()
            .filter(|session| session.mode == mode)
            .ok_or_else(|| web_error("WebView 세션이 준비되지 않았습니다.", 503, true))?;
        self.logs
            .push(format!("[WebView] {} 세션 새로고침", mode.label()));
        session
            .engine
            .call("Page.navigate", json!({ "url": provider.url() }))
            .await?;
        let _ = session.engine.call("Page.bringToFront", json!({})).await;
        session
            .engine
            .wait_until_ready(provider, READY_WAIT)
            .await?;
        Ok(())
    }

    async fn reload_with_state(
        &self,
        mode: TranslationMode,
        pure_quality: bool,
    ) -> Result<(), WebBackendError> {
        self.reload_with_state_inner(mode, pure_quality, false)
            .await
    }

    async fn force_reload_with_state(&self, mode: TranslationMode) -> Result<(), WebBackendError> {
        self.reload_with_state_inner(mode, false, true).await
    }

    async fn reload_with_state_inner(
        &self,
        mode: TranslationMode,
        pure_quality: bool,
        force: bool,
    ) -> Result<(), WebBackendError> {
        let cooldown = session_refresh_cooldown(pure_quality);
        let cooldown_skip = {
            let mut state = self.refresh_state.lock();
            if state.is_refreshing {
                return Ok(());
            }
            let mut skip = None;
            if !force && let Some(last) = state.last_session_refresh {
                let elapsed = last.elapsed();
                if elapsed < cooldown {
                    skip = Some(cooldown.saturating_sub(elapsed));
                }
            }
            if skip.is_none() {
                state.is_refreshing = true;
                state.last_session_refresh = Some(Instant::now());
            }
            skip
        };

        if let Some(remain) = cooldown_skip {
            self.logs.push(format!(
                "[WebView] {} 세션 새로고침 쿨다운 - reload 생략 ({:.1}s 남음)",
                mode.label(),
                remain.as_secs_f32()
            ));
            return self
                .check_ready_until(mode, SESSION_REFRESH_READY_CHECK)
                .await;
        }

        let result = self.reload(mode).await;
        if result.is_ok() {
            tokio::time::sleep(SESSION_REFRESH_STABILIZE).await;
            let ready_result = self
                .check_ready_until(mode, SESSION_REFRESH_READY_CHECK)
                .await;
            let mut state = self.refresh_state.lock();
            state.translation_count = 0;
            state.last_usage_refresh = Some(Instant::now());
            state.last_request_at = Some(Instant::now());
            if ready_result.is_err() {
                self.logs.push(format!(
                    "[WebView] {} 세션 새로고침 후 준비 확인 실패: {}",
                    mode.label(),
                    ready_result
                        .as_ref()
                        .err()
                        .map(|e| e.message.as_str())
                        .unwrap_or("")
                ));
            }
        } else if self
            .check_ready_until(mode, SESSION_REFRESH_READY_CHECK)
            .await
            .is_ok()
        {
            self.logs.push(format!(
                "[WebView] {} reload 오류 후 입력창 감지 - 세션 준비 상태로 복구",
                mode.label()
            ));
            self.refresh_state.lock().is_refreshing = false;
            return Ok(());
        }

        self.refresh_state.lock().is_refreshing = false;
        result
    }

    async fn check_ready_until(
        &self,
        mode: TranslationMode,
        timeout: Duration,
    ) -> Result<(), WebBackendError> {
        let provider = Provider::from_mode(mode)?;
        let deadline = Instant::now() + timeout;
        let mut last_error = None;
        while Instant::now() < deadline {
            let ready = {
                let mut guard = self.session.lock().await;
                let Some(session) = guard.as_mut().filter(|session| session.mode == mode) else {
                    return Err(web_error("WebView 세션이 준비되지 않았습니다.", 503, true));
                };
                match session.engine.evaluate(&provider.ready_script()).await {
                    Ok(Value::Bool(true)) => return Ok(()),
                    Ok(_) => false,
                    Err(error) => {
                        last_error = Some(error);
                        false
                    }
                }
            };
            if ready {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(last_error.unwrap_or_else(|| {
            web_error(
                format!("{} 입력창 준비 확인 시간이 초과되었습니다.", mode.label()),
                503,
                true,
            )
        }))
    }

    async fn restart_session(
        &self,
        mode: TranslationMode,
        reason: &str,
    ) -> Result<(), WebBackendError> {
        self.logs
            .push(format!("[WebView] {} 세션 재생성 ({reason})", mode.label()));
        {
            let mut guard = self.session.lock().await;
            if guard.as_ref().map(|s| s.mode == mode).unwrap_or(false) {
                *guard = None;
            }
        }
        self.ensure_session(mode).await
    }

    async fn restart_session_with_cache_clear(
        &self,
        mode: TranslationMode,
        reason: &str,
    ) -> Result<(), WebBackendError> {
        self.logs.push(format!(
            "[WebView] {} 세션 hard reset ({reason})",
            mode.label()
        ));
        {
            let mut guard = self.session.lock().await;
            if let Some(session) = guard.as_mut().filter(|session| session.mode == mode) {
                match session.engine.clear_cache().await {
                    Ok(()) => self.logs.push(format!(
                        "[WebView] {} WebView2 cache clear 완료",
                        mode.label()
                    )),
                    Err(error) => self.logs.push(format!(
                        "[WebView] {} WebView2 cache clear 실패: {}",
                        mode.label(),
                        error.message
                    )),
                }
                *guard = None;
            }
        }
        self.ensure_session(mode).await
    }

    async fn notify_page_request_guard(
        &self,
        source: &str,
        remaining: Duration,
    ) -> Result<(), WebBackendError> {
        let mode = self.last_mode.lock().unwrap_or(TranslationMode::GeminiCli);
        if Provider::from_mode(mode).is_err() {
            return Ok(());
        }
        let mut guard = self.session.lock().await;
        let Some(session) = guard.as_mut().filter(|session| session.mode == mode) else {
            return Ok(());
        };
        let until_ms = chrono::Utc::now().timestamp_millis()
            + i64::try_from(remaining.as_millis()).unwrap_or(1000);
        self.logs.push(format!(
            "[WebView] {} request guard 페이지 적용 ({source})",
            mode.label()
        ));
        let _ = session.engine.activate_request_guard(until_ms as u64).await;
        let _ = session
            .engine
            .evaluate(&format!(
                "window.__rusterRequestGuardActivate?.({until_ms}); void 0;"
            ))
            .await;
        let provider = Provider::from_mode(mode)?;
        let _ = session
            .engine
            .evaluate(&provider.clear_input_script())
            .await;
        let _ = session.engine.call("Page.stopLoading", json!({})).await;
        Ok(())
    }

    async fn ensure_session(&self, mode: TranslationMode) -> Result<(), WebBackendError> {
        if Provider::from_mode(mode).is_err() {
            return Ok(());
        }

        {
            let mut guard = self.session.lock().await;
            if let Some(session) = guard.as_mut().filter(|session| session.mode == mode) {
                match session.engine.health().await {
                    Ok(health) if health.is_operational() => return Ok(()),
                    Ok(health) => {
                        self.logs.push(format!(
                            "[WebView] {} health check 실패 - 세션 재생성 ({})",
                            mode.label(),
                            health.summary()
                        ));
                        *guard = None;
                    }
                    Err(error) => {
                        self.logs.push(format!(
                            "[WebView] {} health check 오류 - 세션 재생성 ({})",
                            mode.label(),
                            error.message
                        ));
                        *guard = None;
                    }
                }
            }
        }

        let _launch = self.launch_lock.lock().await;
        {
            let mut guard = self.session.lock().await;
            if let Some(session) = guard.as_mut().filter(|session| session.mode == mode) {
                match session.engine.health().await {
                    Ok(health) if health.is_operational() => return Ok(()),
                    Ok(health) => {
                        self.logs.push(format!(
                            "[WebView] {} launch lock 이후 health 실패 - 세션 재생성 ({})",
                            mode.label(),
                            health.summary()
                        ));
                        *guard = None;
                    }
                    Err(error) => {
                        self.logs.push(format!(
                            "[WebView] {} launch lock 이후 health 오류 - 세션 재생성 ({})",
                            mode.label(),
                            error.message
                        ));
                        *guard = None;
                    }
                }
            }
        }

        let provider = Provider::from_mode(mode)?;
        let session = BrowserSession::launch(
            provider,
            &self.profile_root.join(provider.profile_name()),
            &self.logs,
        )
        .await?;

        *self.last_mode.lock() = Some(mode);
        *self.session.lock().await = Some(session);
        Ok(())
    }
}

impl BrowserSession {
    async fn launch(
        provider: Provider,
        profile_dir: &Path,
        logs: &LogBuffer,
    ) -> Result<Self, WebBackendError> {
        let engine = WebView2Engine::launch(provider, profile_dir, logs).await?;
        Ok(Self {
            mode: provider.mode(),
            engine,
        })
    }
}

struct WebView2Engine {
    native: NativeWebView2Session,
}

impl WebView2Engine {
    async fn launch(
        provider: Provider,
        profile_dir: &Path,
        logs: &LogBuffer,
    ) -> Result<Self, WebBackendError> {
        let native = NativeWebView2Session::launch(
            provider.label(),
            provider.url(),
            profile_dir.to_path_buf(),
            REQUEST_GUARD_BOOTSTRAP_JS,
            logs.clone(),
        )
        .map_err(|error| web_error(format!("WebView2 시작 실패: {error}"), 503, true))?;

        let mut engine = Self { native };
        engine.wait_until_ready(provider, READY_WAIT).await?;
        let _ = engine.evaluate(REQUEST_GUARD_BOOTSTRAP_JS).await;
        Ok(engine)
    }

    async fn wait_until_ready(
        &mut self,
        provider: Provider,
        timeout: Duration,
    ) -> Result<(), WebBackendError> {
        let deadline = Instant::now() + timeout;
        let mut last_error = None;
        while Instant::now() < deadline {
            match self.evaluate(&provider.ready_script()).await {
                Ok(Value::Bool(true)) => return Ok(()),
                Ok(_) => {}
                Err(error) => last_error = Some(error),
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Err(last_error.unwrap_or_else(|| {
            web_error(
                format!("{} 입력창 준비 시간이 초과되었습니다.", provider.label()),
                503,
                true,
            )
        }))
    }

    async fn snapshot(&mut self, provider: Provider) -> Result<ResponseSnapshot, WebBackendError> {
        let value = self.evaluate(&provider.snapshot_script()).await?;
        Ok(ResponseSnapshot {
            text: value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            count: value.get("count").and_then(Value::as_u64).unwrap_or(0) as usize,
            is_generating: value
                .get("isGenerating")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            login_needed: value
                .get("loginNeeded")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            protection_blocked: value
                .get("protectionBlocked")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            has_error: value
                .get("hasError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            is_stopped: value
                .get("isStopped")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    async fn prompt_state(
        &mut self,
        provider: Provider,
    ) -> Result<PromptInputState, WebBackendError> {
        let value = self.evaluate(&provider.prompt_state_script()).await?;
        let input_visible = value
            .get("inputVisible")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let has_text = value
            .get("hasText")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let send_enabled = value
            .get("sendEnabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let generating = value
            .get("generating")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(PromptInputState {
            pending_send: input_visible && has_text && send_enabled && !generating,
            text_length: value.get("textLength").and_then(Value::as_u64).unwrap_or(0) as usize,
            preview: value
                .get("preview")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
    }

    async fn login_prompt_state(
        &mut self,
        provider: Provider,
    ) -> Result<LoginPromptState, WebBackendError> {
        let value = self.evaluate(&provider.login_prompt_state_script()).await?;
        Ok(LoginPromptState::from_value(&value))
    }

    async fn health(&mut self) -> Result<NativeWebView2Health, WebBackendError> {
        self.native
            .health()
            .await
            .map_err(|error| web_error(format!("WebView2 health check 실패: {error}"), 503, true))
    }

    async fn ensure_healthy(&mut self, provider: Provider) -> Result<(), WebBackendError> {
        let health = self.health().await?;
        if health.is_operational() {
            return Ok(());
        }
        Err(web_error(
            format!(
                "{} WebView2 health check 실패: {}",
                provider.label(),
                health.summary()
            ),
            503,
            true,
        ))
    }

    async fn clear_cache(&mut self) -> Result<(), WebBackendError> {
        self.native
            .clear_cache()
            .await
            .map_err(|error| web_error(format!("WebView2 cache clear 실패: {error}"), 503, true))
    }

    async fn activate_request_guard(&mut self, until_unix_ms: u64) -> Result<(), WebBackendError> {
        self.native
            .activate_request_guard(until_unix_ms)
            .await
            .map_err(|error| {
                web_error(
                    format!("WebView2 native request guard 적용 실패: {error}"),
                    503,
                    true,
                )
            })
    }

    async fn evaluate(&mut self, expression: &str) -> Result<Value, WebBackendError> {
        self.native
            .evaluate(expression)
            .await
            .map_err(|error| web_error(format!("WebView2 script error: {error}"), 500, true))
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value, WebBackendError> {
        match method {
            "Page.bringToFront" => {
                self.native.bring_to_front().await.map_err(|error| {
                    web_error(format!("WebView2 foreground 실패: {error}"), 503, true)
                })?;
                Ok(Value::Null)
            }
            "Page.stopLoading" => {
                self.native.stop_loading().await.map_err(|error| {
                    web_error(format!("WebView2 stop 실패: {error}"), 503, true)
                })?;
                Ok(Value::Null)
            }
            "Page.navigate" => {
                let url = params
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| web_error("WebView2 navigate URL이 없습니다.", 500, false))?;
                self.native.navigate(url).await.map_err(|error| {
                    web_error(format!("WebView2 navigate 실패: {error}"), 503, true)
                })?;
                Ok(Value::Null)
            }
            _ => Err(web_error(
                format!("지원하지 않는 WebView2 명령: {method}"),
                500,
                false,
            )),
        }
    }

    async fn set_visibility(
        &mut self,
        visibility: WebWindowVisibility,
    ) -> Result<bool, WebBackendError> {
        let result = match visibility {
            WebWindowVisibility::Show => self.native.show_window().await,
            WebWindowVisibility::Hide => self.native.hide_window().await,
            WebWindowVisibility::Toggle => self.native.toggle_window().await,
        };
        result
            .map_err(|error| web_error(format!("WebView2 표시 상태 변경 실패: {error}"), 503, true))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Provider {
    Gemini,
    ChatGpt,
}

impl Provider {
    fn from_mode(mode: TranslationMode) -> Result<Self, WebBackendError> {
        match mode {
            TranslationMode::WebView => Ok(Self::Gemini),
            TranslationMode::ChatGptWebView => Ok(Self::ChatGpt),
            TranslationMode::GeminiCli => Err(web_error(
                "CLI 모드는 WebView backend가 아닙니다.",
                500,
                false,
            )),
        }
    }

    fn mode(self) -> TranslationMode {
        match self {
            Self::Gemini => TranslationMode::WebView,
            Self::ChatGpt => TranslationMode::ChatGptWebView,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Gemini => "Gemini WebView",
            Self::ChatGpt => "ChatGPT WebView",
        }
    }

    fn url(self) -> &'static str {
        match self {
            Self::Gemini => GEMINI_URL,
            Self::ChatGpt => CHATGPT_URL,
        }
    }

    fn profile_name(self) -> &'static str {
        match self {
            Self::Gemini => "Gemini",
            Self::ChatGpt => "ChatGPT",
        }
    }

    fn input_selector(self) -> &'static str {
        match self {
            Self::Gemini => {
                "rich-textarea .ql-editor, .ql-editor[contenteditable=\"true\"], [role=\"textbox\"][contenteditable=\"true\"], div[contenteditable=\"true\"], textarea[aria-label], textarea"
            }
            Self::ChatGpt => {
                "#prompt-textarea, textarea[data-testid=\"prompt-textarea\"], [data-testid=\"prompt-textarea\"], [data-testid=\"composer-text-input\"], rich-textarea .ql-editor, .ql-editor[contenteditable=\"true\"], [role=\"textbox\"][contenteditable=\"true\"], div[contenteditable=\"true\"], textarea[aria-label], textarea"
            }
        }
    }

    fn send_selector(self) -> &'static str {
        match self {
            Self::Gemini => {
                "[data-test-id=\"send-button-container\"] button.send-button:not(.stop), .send-button-container button.send-button:not(.stop), button.send-button[aria-label*=\"메시지 보내기\"]:not(.stop), button.send-button[aria-label*=\"Send message\"]:not(.stop), button.send-button:not(.stop), button[data-test-id=\"send-button\"]:not(.stop)"
            }
            Self::ChatGpt => {
                "[data-testid=\"send-button\"]:not(.stop), button[data-testid=\"send-button\"]:not(.stop), button[aria-label*=\"프롬프트 보내기\"]:not(.stop), button[aria-label*=\"보내기\"]:not(.stop), button[aria-label*=\"Send\"]:not(.stop), button[aria-label*=\"Send prompt\"]:not(.stop), button[aria-label*=\"Send message\"]:not(.stop), form button[type=\"submit\"]:not(.stop)"
            }
        }
    }

    fn stop_selector(self) -> &'static str {
        match self {
            Self::Gemini => {
                ".send-button.stop, button.send-button.stop, button[aria-label*=\"중지\"], button[aria-label*=\"Stop\"], button[aria-label*=\"stop\"]"
            }
            Self::ChatGpt => {
                "[data-testid=\"stop-button\"], button[data-testid=\"stop-button\"], .send-button.stop, button.send-button.stop, button[aria-label*=\"중지\"], button[aria-label*=\"Stop\"], button[aria-label*=\"stop\"]"
            }
        }
    }

    fn response_selector(self) -> &'static str {
        match self {
            Self::Gemini => {
                "model-response message-content, response-container message-content, [data-message-author-role=\"model\"] message-content, [data-test-id*=\"model\"] message-content, model-response .model-response-text, response-container .model-response-text, structured-content-container.model-response-text, message-content.model-response-text, .model-response-text, [data-test-id=\"model-response-text\"], message-content"
            }
            Self::ChatGpt => {
                "[data-message-author-role=\"assistant\"] .markdown, [data-message-author-role=\"assistant\"] [data-testid=\"conversation-turn-content\"], [data-message-author-role=\"assistant\"], article[data-testid*=\"conversation-turn\"] .markdown, article .markdown, .markdown"
            }
        }
    }

    fn new_chat_selector(self) -> &'static str {
        match self {
            Self::Gemini => {
                "button[aria-label*=\"새 채팅\"], button[aria-label*=\"New chat\"], a[aria-label*=\"새 채팅\"], a[aria-label*=\"New chat\"], [role=\"button\"][aria-label*=\"새 채팅\"], [role=\"button\"][aria-label*=\"New chat\"], [data-test-id*=\"new-chat\"], [data-testid*=\"new-chat\"]"
            }
            Self::ChatGpt => {
                "button[aria-label*=\"새 채팅\"], button[aria-label*=\"New chat\"], a[aria-label*=\"새 채팅\"], a[aria-label*=\"New chat\"], [role=\"button\"][aria-label*=\"새 채팅\"], [role=\"button\"][aria-label*=\"New chat\"], [data-testid*=\"new-chat\"], [data-test-id*=\"new-chat\"]"
            }
        }
    }

    fn ready_script(self) -> String {
        format!(
            r#"(function() {{
const isVisible = {is_visible};
const input = Array.from(document.querySelectorAll({input:?})).find(isVisible);
if (!isVisible(input)) return false;
const body = (document.body && (document.body.innerText || document.body.textContent) || '').toLowerCase();
if (body.includes('session expired') || body.includes('login required') || body.includes('signed out') || body.includes('not logged in') || body.includes('로그인이 필요') || body.includes('다시 로그인')) return false;
const dialogs = Array.from(document.querySelectorAll('signed-out-dialog, mat-dialog-container, .mat-mdc-dialog-container, [role="dialog"], [aria-modal="true"]')).filter(isVisible);
for (const dialog of dialogs) {{
  const text = ((dialog.innerText || dialog.textContent || '') + '').toLowerCase();
  if (text.includes('session expired') || text.includes('login required') || text.includes('signed out') || text.includes('sign in') || text.includes('로그인이 필요') || text.includes('다시 로그인')) return false;
  const loginBtn = dialog.querySelector('button[aria-label*="로그인"], button[aria-label*="Sign in"], a[href*="ServiceLogin"], a[href*="accounts.google.com"]');
  if (isVisible(loginBtn)) return false;
}}
return true;
}})()"#,
            is_visible = IS_VISIBLE_JS,
            input = self.input_selector()
        )
    }

    fn clear_input_script(self) -> String {
        format!(
            r#"(function() {{
const input = Array.from(document.querySelectorAll({input:?})).find(isVisible);
if (!input) return 'no_input';
input.focus();
try {{
  document.execCommand('selectAll', false, null);
  document.execCommand('delete', false, null);
}} catch (e) {{}}
if ('value' in input) input.value = '';
if (input.isContentEditable) {{
  if (input.classList && input.classList.contains('ql-editor')) {{
    input.innerHTML = '<p><br></p>';
    input.classList.add('ql-blank');
  }} else {{
    input.textContent = '';
  }}
}}
input.dispatchEvent(new InputEvent('input', {{bubbles:true, inputType:'deleteContentBackward', data:null}}));
input.dispatchEvent(new Event('change', {{bubbles:true}}));
return 'cleared';
}})()"#,
            input = self.input_selector()
        )
    }

    fn inject_prompt_script(self, prompt: &str) -> String {
        let prompt_json = serde_json::to_string(prompt).unwrap_or_else(|_| "\"\"".to_owned());
        format!(
            r#"(function() {{
const isVisible = {is_visible};
const input = Array.from(document.querySelectorAll({input:?})).find(isVisible);
if (!isVisible(input)) return 'no_input';
const text = {prompt};
input.focus();
try {{
  document.execCommand('selectAll', false, null);
  document.execCommand('delete', false, null);
}} catch (e) {{}}
if ('value' in input && !input.isContentEditable) {{
  input.value = text;
}} else {{
  const lines = String(text).replace(/\r\n/g, '\n').replace(/\r/g, '\n').split('\n');
  input.replaceChildren(...lines.map(line => {{
    const p = document.createElement('p');
    if (line.length) p.appendChild(document.createTextNode(line));
    else p.appendChild(document.createElement('br'));
    return p;
  }}));
  if (input.classList) input.classList.remove('ql-blank');
}}
input.dispatchEvent(new InputEvent('beforeinput', {{bubbles:true, cancelable:true, inputType:'insertText', data:text}}));
input.dispatchEvent(new InputEvent('input', {{bubbles:true, inputType:'insertText', data:text}}));
input.dispatchEvent(new Event('change', {{bubbles:true}}));
return 'injected:' + String(text).length;
}})()"#,
            is_visible = IS_VISIBLE_JS,
            input = self.input_selector(),
            prompt = prompt_json
        )
    }

    fn send_script(self) -> String {
        format!(
            r#"(function() {{
const isVisible = {is_visible};
const normalize = text => String(text || '').replace(/\s+/g, ' ').trim();
const input = Array.from(document.querySelectorAll({input:?})).find(isVisible);
const disabled = el => el.disabled === true || (el.getAttribute('aria-disabled') || '').toLowerCase() === 'true' || !!el.closest('[disabled], .disabled, .send-button-container.disabled');
const isStopLike = el => {{
  const aria = normalize(el?.getAttribute('aria-label') || '').toLowerCase();
  const cls = String(el?.className || '').toLowerCase();
  return cls.includes('stop') || aria.includes('stop') || aria.includes('중지');
}};
const stop = Array.from(document.querySelectorAll({stop:?})).find(btn => isVisible(btn) && !disabled(btn));
if (stop) return 'busy_stop';
if (!isVisible(input)) return 'no_input';
const text = normalize(input.value || input.innerText || input.textContent || '');
if (!text) return 'no_text';
const scope = input.closest('input-area-v2, input-container, form, .input-area, [role="group"]') || document;
const roots = scope === document ? [document] : [scope, document];
for (const root of roots) {{
  const button = Array.from(root.querySelectorAll({send:?})).find(btn => isVisible(btn) && !disabled(btn) && !isStopLike(btn));
  if (button) {{
    try {{
      button.click();
      return 'sent_click';
    }} catch (e) {{
      button.dispatchEvent(new MouseEvent('click', {{ bubbles:true, cancelable:true }}));
      return 'sent_click_event';
    }}
  }}
}}
for (const type of ['keydown','keypress','keyup']) {{
  input.dispatchEvent(new KeyboardEvent(type, {{key:'Enter', code:'Enter', keyCode:13, which:13, bubbles:true, cancelable:true, composed:true}}));
}}
return 'sent_enter';
}})()"#,
            is_visible = IS_VISIBLE_JS,
            input = self.input_selector(),
            stop = self.stop_selector(),
            send = self.send_selector()
        )
    }

    fn snapshot_script(self) -> String {
        format!(
            r#"(function() {{
const isVisible = {is_visible};
const normalize = text => String(text || '').replace(/\u00a0/g, ' ').replace(/[ \t]+\n/g, '\n').replace(/\n{{3,}}/g, '\n\n').trim();
const stop = Array.from(document.querySelectorAll({stop:?})).some(el => isVisible(el));
const busy = Array.from(document.querySelectorAll('.markdown[aria-busy="true"], [aria-busy="true"], mat-progress-spinner, .loading, .thinking')).some(isVisible);
const input = Array.from(document.querySelectorAll({input:?})).find(isVisible);
const disabled = el => el.disabled === true || (el.getAttribute('aria-disabled') || '').toLowerCase() === 'true' || !!el.closest('[disabled], .disabled, .send-button-container.disabled');
const sendEnabled = Array.from(document.querySelectorAll({send:?})).some(btn => isVisible(btn) && !disabled(btn));
const nodes = Array.from(document.querySelectorAll({responses:?}))
  .filter(el => isVisible(el) && (!input || !input.contains(el)))
  .filter(el => !el.closest('[data-message-author-role="user"], [data-test-id*="user"], user-query, .input-area, input-area-v2, input-container, rich-textarea, .ql-editor, div[contenteditable="true"]'))
  .map(el => normalize(el.innerText || el.textContent || ''))
  .filter(text => text.length > 0)
  .filter((text, index, arr) => arr.indexOf(text) === index);
const body = normalize(document.body?.innerText || document.body?.textContent || '').toLowerCase();
const loginNeeded = body.includes('session expired') ||
  body.includes('login required') ||
  body.includes('signed out') ||
  body.includes('not logged in') ||
  body.includes('sign in') ||
  body.includes('log in') ||
  body.includes('로그인이 필요') ||
  body.includes('다시 로그인');
const protectionBlocked = body.includes('unusual traffic') ||
  body.includes('automated traffic') ||
  body.includes('automated queries') ||
  body.includes('captcha') ||
  body.includes('recaptcha') ||
  body.includes('abuse') ||
  body.includes('too many requests') ||
  body.includes('rate limit') ||
  body.includes('temporarily blocked') ||
  body.includes('suspicious activity') ||
  body.includes('보안문자') ||
  body.includes('비정상적인 트래픽') ||
  body.includes('자동화') ||
  body.includes('요청이 너무 많') ||
  body.includes('요청 한도') ||
  body.includes('사용량 한도') ||
  body.includes('잠시 후 다시');
const hasError = body.includes('something went wrong') ||
  body.includes('try again') ||
  body.includes('오류가 발생') ||
  body.includes('다시 시도');
const lastText = nodes.length ? nodes[nodes.length - 1].toLowerCase() : '';
const stoppedTerms = [
  'response stopped', 'generation stopped', 'stopped generating',
  'message stopped', 'content policy', 'policy violation',
  '응답이 중지', '답변이 중지', '생성이 중지', '콘텐츠 정책'
];
const isStopped = !stop && !busy && lastText.length > 0 && stoppedTerms.some(term => lastText.includes(term));
return {{
  text: nodes.length ? nodes[nodes.length - 1] : '',
  count: nodes.length,
  responseCount: nodes.length,
  isGenerating: stop || busy,
  readyForNextInput: !!input && !stop && !busy,
  inputVisible: !!input,
  sendEnabled,
  loginNeeded,
  protectionBlocked,
  hasError,
  isStopped
}};
}})()"#,
            is_visible = IS_VISIBLE_JS,
            stop = self.stop_selector(),
            input = self.input_selector(),
            send = self.send_selector(),
            responses = self.response_selector()
        )
    }

    fn prompt_state_script(self) -> String {
        format!(
            r#"(function() {{
const isVisible = {is_visible};
const normalize = text => String(text || '').replace(/\s+/g, ' ').trim();
const input = Array.from(document.querySelectorAll({input:?})).find(isVisible);
const stop = Array.from(document.querySelectorAll({stop:?})).some(isVisible);
if (!isVisible(input)) return {{ inputVisible:false, hasText:false, sendEnabled:false, generating:stop, textLength:0, preview:'' }};
const text = normalize(input.value || input.innerText || input.textContent || '');
const editable = (input.getAttribute('contenteditable') || '').toLowerCase() === 'true' || (input.tagName || '').toLowerCase() === 'textarea' && input.disabled !== true && input.readOnly !== true;
const disabled = el => el.disabled === true || (el.getAttribute('aria-disabled') || '').toLowerCase() === 'true' || !!el.closest('[disabled], .disabled, .send-button-container.disabled');
const sendEnabled = Array.from(document.querySelectorAll({send:?})).some(btn => isVisible(btn) && !disabled(btn));
return {{
  inputVisible: true,
  inputEditable: editable,
  hasText: text.length > 0,
  sendEnabled: editable && sendEnabled,
  generating: stop,
  textLength: text.length,
  preview: text.slice(0, 160)
}};
}})()"#,
            is_visible = IS_VISIBLE_JS,
            input = self.input_selector(),
            stop = self.stop_selector(),
            send = self.send_selector()
        )
    }

    fn login_prompt_state_script(self) -> String {
        format!(
            r#"(function() {{
const isVisible = {is_visible};
const normalize = text => String(text || '').replace(/\s+/g, ' ').trim();
const lower = text => normalize(text).toLowerCase();
const hasKeyword = (text, keywords) => {{
  const value = lower(text);
  return keywords.some(keyword => value.includes(String(keyword).toLowerCase()));
}};
const strong = ['로그아웃된 상태', '다시 로그인', '로그인이 필요', '세션 만료', 'signed out', 'sign back in', 'session expired', 'login required', 'not logged in'];
const weak = ['로그인', 'sign in', 'login', 'session', 'auth'];
const result = {{
  hasAnyPrompt: false,
  signedOutDialog: false,
  loginNudgeBanner: false,
  loginButton: false,
  modalLoginButton: false,
  blockingOverlay: false,
  loginTextHit: false,
  inputVisible: false,
  inputBlocked: false,
  needsHardRecovery: false,
  details: {{}}
}};
const input = Array.from(document.querySelectorAll({input:?})).find(isVisible);
result.inputVisible = isVisible(input);
const dialogs = Array.from(document.querySelectorAll('signed-out-dialog, mat-dialog-container, .mat-mdc-dialog-container, [role="dialog"], [aria-modal="true"]')).filter(isVisible);
let longestDialogText = '';
for (const dialog of dialogs) {{
  const dialogText = normalize(dialog.innerText || dialog.textContent || '');
  if (dialogText.length > longestDialogText.length) longestDialogText = dialogText;
  const loginAction = Array.from(dialog.querySelectorAll('button, a, [role="button"]')).find(el => {{
    if (!isVisible(el)) return false;
    const label = normalize((el.innerText || el.textContent || '') + ' ' + (el.getAttribute('aria-label') || ''));
    return hasKeyword(label, ['로그인', 'sign in', 'login']);
  }});
  if (loginAction) result.modalLoginButton = true;
  if (hasKeyword(dialogText, strong) || (hasKeyword(dialogText, weak) && !!loginAction)) {{
    result.signedOutDialog = true;
  }}
}}
if (longestDialogText.length > 0) result.details.dialogText = longestDialogText.substring(0, 220);
const nudge = Array.from(document.querySelectorAll('sign-in-nudge, [data-testid*="sign-in"], [data-test-id*="sign-in"], .sign-in-nudge')).find(isVisible);
if (nudge) {{
  result.loginNudgeBanner = true;
  const text = normalize(nudge.innerText || nudge.textContent || '');
  if (text.length > 0) result.details.bannerText = text.substring(0, 140);
}}
const globalLoginButton = Array.from(document.querySelectorAll('button, a, [role="button"]')).find(el => {{
  if (!isVisible(el)) return false;
  const label = normalize((el.innerText || el.textContent || '') + ' ' + (el.getAttribute('aria-label') || ''));
  if (!hasKeyword(label, ['로그인', 'sign in', 'login'])) return false;
  const href = (el.getAttribute('href') || '').toLowerCase();
  const testId = ((el.getAttribute('data-testid') || '') + ' ' + (el.getAttribute('data-test-id') || '')).toLowerCase();
  return href.includes('servicelogin') || href.includes('accounts.google.com') || testId.includes('sign') || testId.includes('login') || label.length <= 36;
}});
result.loginButton = !!globalLoginButton;
const overlays = Array.from(document.querySelectorAll('.cdk-overlay-backdrop, .cdk-overlay-container, .cdk-overlay-pane, [aria-modal="true"]')).filter(isVisible);
result.blockingOverlay = overlays.length > 0;
const bodyText = normalize(document.body ? document.body.innerText : '');
result.loginTextHit = hasKeyword(bodyText.substring(0, 2000), strong);
result.inputBlocked = !result.inputVisible && (result.blockingOverlay || result.signedOutDialog);
result.needsHardRecovery = result.signedOutDialog || result.inputBlocked || (result.blockingOverlay && result.modalLoginButton);
result.hasAnyPrompt = result.needsHardRecovery || result.loginNudgeBanner || (result.loginButton && (result.blockingOverlay || result.loginTextHit));
return result;
}})()"#,
            is_visible = IS_VISIBLE_JS,
            input = self.input_selector()
        )
    }

    fn recover_signed_out_script(self) -> String {
        format!(
            r#"(function() {{
const isVisible = {is_visible};
const result = {{ success: false, action: 'none', message: '' }};
try {{
  const overlays = document.querySelectorAll('.cdk-overlay-container, .cdk-overlay-backdrop, .cdk-overlay-pane, mat-dialog-container, .mat-mdc-dialog-container, signed-out-dialog, [role="dialog"], [aria-modal="true"]');
  overlays.forEach(el => el.remove());
  if (document.body) {{
    document.body.style.overflow = '';
    document.body.classList.remove('cdk-global-scrollblock');
  }}
  if (document.documentElement) {{
    document.documentElement.style.overflow = '';
    document.documentElement.classList.remove('cdk-global-scrollblock');
  }}
  const input = Array.from(document.querySelectorAll({input:?})).find(isVisible);
  if (!input) {{
    result.action = 'needs_reload';
    result.message = 'input not found after login prompt removal';
    return result;
  }}
  input.focus();
  const newChat = Array.from(document.querySelectorAll({new_chat:?})).find(isVisible);
  if (newChat) {{
    newChat.click();
    result.action = 'new_chat_started';
    result.message = 'new chat started';
  }} else {{
    result.action = 'needs_reload';
    result.message = 'new chat button not found';
  }}
  result.success = true;
}} catch (e) {{
  result.action = 'error';
  result.message = e && e.message ? e.message : 'unknown';
}}
return result;
}})()"#,
            is_visible = IS_VISIBLE_JS,
            input = self.input_selector(),
            new_chat = self.new_chat_selector()
        )
    }

    fn dismiss_login_prompts_script(self) -> String {
        r#"(function() {
const result = { dialogDismissed: false, bannerDismissed: false, overlayRemoved: false, scrollRestored: false };
const dialogs = document.querySelectorAll('signed-out-dialog, mat-dialog-container, .mat-mdc-dialog-container, [role="dialog"], [aria-modal="true"]');
dialogs.forEach(el => { el.remove(); result.dialogDismissed = true; });
const overlays = document.querySelectorAll('.cdk-overlay-container, .cdk-overlay-backdrop, .cdk-overlay-pane');
overlays.forEach(el => { el.remove(); result.overlayRemoved = true; });
if (document.body && (document.body.style.overflow === 'hidden' || document.body.classList.contains('cdk-global-scrollblock'))) {
  document.body.style.overflow = '';
  document.body.classList.remove('cdk-global-scrollblock');
  result.scrollRestored = true;
}
if (document.documentElement && (document.documentElement.style.overflow === 'hidden' || document.documentElement.classList.contains('cdk-global-scrollblock'))) {
  document.documentElement.style.overflow = '';
  document.documentElement.classList.remove('cdk-global-scrollblock');
  result.scrollRestored = true;
}
const nudges = document.querySelectorAll('sign-in-nudge, [data-testid*="sign-in"], [data-test-id*="sign-in"], .sign-in-nudge');
nudges.forEach(el => { el.remove(); result.bannerDismissed = true; });
return result;
})()"#.to_string()
    }
}

const IS_VISIBLE_JS: &str = r#"(el) => {
  if (!el) return false;
  const style = window.getComputedStyle(el);
  if (!style || style.display === 'none' || style.visibility === 'hidden' || style.opacity === '0') return false;
  const rect = el.getBoundingClientRect();
  return rect.width > 2 && rect.height > 2;
}"#;

const REQUEST_GUARD_BOOTSTRAP_JS: &str = r#"
(() => {
  const KEY = "__rusterRequestGuard";
  if (window[KEY]?.initialized) return;

  const state = window[KEY] = {
    initialized: true,
    until: 0,
    fetch: window.fetch?.bind(window),
    xhrOpen: XMLHttpRequest.prototype.open,
    xhrSend: XMLHttpRequest.prototype.send,
    activeControllers: new Set(),
    activeXhrs: new Set()
  };

  const isGuarded = () => Date.now() < state.until;

  window.__rusterRequestGuardActivate = (until) => {
    const nextUntil = Number(until) || (Date.now() + 1000);
    state.until = Math.max(state.until, nextUntil);
    for (const controller of Array.from(state.activeControllers)) {
      try { controller.abort(); } catch (_) { }
    }
    for (const xhr of Array.from(state.activeXhrs)) {
      try { xhr.abort(); } catch (_) { }
    }
    try { window.stop(); } catch (_) { }
  };

  document.addEventListener("keydown", (event) => {
    const isF12 = event.key === "F12" || event.code === "F12" || event.keyCode === 123;
    const isF5 = event.key === "F5" || event.code === "F5" || event.keyCode === 116;
    if (isF12 || isF5) {
      event.preventDefault();
      event.stopPropagation();
      event.stopImmediatePropagation();
      window.__rusterRequestGuardActivate(Date.now() + 1000);
    }
  }, true);

  if (state.fetch) {
    window.fetch = function(resource, init) {
      if (isGuarded()) {
        return Promise.reject(new DOMException("Blocked by request guard", "AbortError"));
      }
      const controller = new AbortController();
      const nextInit = init ? Object.assign({}, init) : {};
      if (init?.signal) {
        if (AbortSignal.any) {
          nextInit.signal = AbortSignal.any([init.signal, controller.signal]);
        } else {
          if (init.signal.aborted) controller.abort();
          else init.signal.addEventListener("abort", () => controller.abort(), { once: true });
          nextInit.signal = controller.signal;
        }
      } else {
        nextInit.signal = controller.signal;
      }
      state.activeControllers.add(controller);
      return state.fetch(resource, nextInit).finally(() => {
        state.activeControllers.delete(controller);
      });
    };
  }

  XMLHttpRequest.prototype.open = function(method, url) {
    this.__rusterRequestGuardUrl = url;
    return state.xhrOpen.apply(this, arguments);
  };

  XMLHttpRequest.prototype.send = function(body) {
    if (isGuarded()) {
      setTimeout(() => { try { this.abort(); } catch (_) { } }, 0);
      return undefined;
    }
    state.activeXhrs.add(this);
    this.addEventListener("loadend", () => state.activeXhrs.delete(this), { once: true });
    return state.xhrSend.apply(this, arguments);
  };

  try {
    const originalBeacon = navigator.sendBeacon?.bind(navigator);
    if (originalBeacon) {
      navigator.sendBeacon = (url, data) => isGuarded() ? true : originalBeacon(url, data);
    }
  } catch (_) { }
})();
"#;

#[derive(Clone, Debug)]
struct ResponseSnapshot {
    text: String,
    count: usize,
    is_generating: bool,
    login_needed: bool,
    protection_blocked: bool,
    has_error: bool,
    is_stopped: bool,
}

#[derive(Clone, Debug, Default)]
struct PromptInputState {
    pending_send: bool,
    text_length: usize,
    preview: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoginPromptHandling {
    None,
    SoftDismissed,
    HardRecovered,
    HardRecoveryTriggered,
    LoginRequired,
}

#[derive(Clone, Debug, Default)]
struct LoginPromptState {
    has_any_prompt: bool,
    signed_out_dialog: bool,
    needs_hard_recovery: bool,
    login_nudge_banner: bool,
    login_button: bool,
    blocking_overlay: bool,
    input_visible: bool,
    input_blocked: bool,
    detail: String,
}

impl LoginPromptState {
    fn from_value(value: &Value) -> Self {
        let details = value.get("details").unwrap_or(&Value::Null);
        let detail = details
            .get("dialogText")
            .or_else(|| details.get("bannerText"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        Self {
            has_any_prompt: value
                .get("hasAnyPrompt")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            signed_out_dialog: value
                .get("signedOutDialog")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            needs_hard_recovery: value
                .get("needsHardRecovery")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            login_nudge_banner: value
                .get("loginNudgeBanner")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            login_button: value
                .get("loginButton")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            blocking_overlay: value
                .get("blockingOverlay")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            input_visible: value
                .get("inputVisible")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            input_blocked: value
                .get("inputBlocked")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            detail,
        }
    }
}

fn adaptive_stable_count(text: &str) -> u8 {
    if text.len() < 300 {
        2
    } else if text.len() < 1200 {
        3
    } else {
        4
    }
}

fn usage_refresh_interval(settings: &AppSettings) -> u32 {
    if settings.web_view_pure_quality_mode {
        1
    } else {
        settings.web_view_refresh_every_requests.clamp(1, 200)
    }
}

fn session_refresh_cooldown(pure_quality: bool) -> Duration {
    if pure_quality {
        PURE_QUALITY_SESSION_REFRESH_COOLDOWN
    } else {
        SESSION_REFRESH_COOLDOWN
    }
}

fn sanitize_response(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim_matches(['\r', '\n'])
        .to_owned()
}

fn web_error(message: impl Into<String>, status: u16, retryable: bool) -> WebBackendError {
    WebBackendError {
        message: message.into(),
        status,
        retryable,
    }
}

async fn reset_profile_root(root: &Path, logs: &LogBuffer) -> Result<bool, WebBackendError> {
    let root = full_path(root).map_err(|error| {
        web_error(
            format!("Failed to resolve WebView profile folder: {error}"),
            500,
            false,
        )
    })?;

    if !is_safe_webview_root(&root) {
        return Err(web_error(
            format!("Unsafe WebView data path: {}", root.display()),
            500,
            false,
        ));
    }

    let deleted_existing_data = directory_has_entries(&root).map_err(|error| {
        web_error(
            format!("Failed to inspect WebView profile folder: {error}"),
            503,
            true,
        )
    })?;

    if root.exists() {
        delete_directory_with_retry(&root).await.map_err(|error| {
            web_error(
                format!(
                    "Failed to delete WebView profile folder. A WebView/browser process may still be shutting down: {error}"
                ),
                503,
                true,
            )
        })?;
    }

    fs::create_dir_all(&root).map_err(|error| {
        web_error(
            format!("Failed to recreate empty WebView profile folder: {error}"),
            503,
            true,
        )
    })?;

    logs.push(format!(
        "[WebView] profile reset complete (deletedExistingData={}, path={})",
        deleted_existing_data,
        root.display()
    ));
    Ok(deleted_existing_data)
}

fn full_path(path: &Path) -> io::Result<PathBuf> {
    if path.exists() {
        fs::canonicalize(path)
    } else if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn is_safe_webview_root(root: &Path) -> bool {
    root.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("WebView2"))
        && root.parent().is_some()
}

fn directory_has_entries(root: &Path) -> io::Result<bool> {
    if !root.exists() {
        return Ok(false);
    }
    let mut entries = fs::read_dir(root)?;
    match entries.next() {
        Some(Ok(_)) => Ok(true),
        Some(Err(error)) => Err(error),
        None => Ok(false),
    }
}

async fn delete_directory_with_retry(root: &Path) -> io::Result<()> {
    let mut last_error = None;

    for attempt in 1..=MAX_PROFILE_DELETE_ATTEMPTS {
        if !root.exists() {
            return Ok(());
        }

        normalize_attributes(root);
        match fs::remove_dir_all(root) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => last_error = Some(error),
        }

        tokio::time::sleep(profile_delete_retry_delay(attempt)).await;
    }

    Err(last_error.unwrap_or_else(|| io::Error::other("delete retry failed")))
}

fn profile_delete_retry_delay(attempt: u8) -> Duration {
    Duration::from_millis((100 + u64::from(attempt) * 160).min(1200))
}

fn normalize_attributes(path: &Path) {
    if !path.exists() {
        return;
    }

    if path.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                normalize_attributes(&entry.path());
            }
        }
    }

    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        if permissions.readonly() {
            permissions.set_readonly(false);
            let _ = fs::set_permissions(path, permissions);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn profile_reset_deletes_existing_webview_root_and_recreates_empty_dir() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("ruster-tests")
            .join(uuid::Uuid::new_v4().to_string())
            .join("WebView2");
        let profile = root.join("Gemini");
        fs::create_dir_all(&profile).unwrap();
        let cookie_file = profile.join("Cookies");
        fs::write(&cookie_file, "stale").unwrap();
        let mut permissions = fs::metadata(&cookie_file).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&cookie_file, permissions).unwrap();

        let logs = LogBuffer::new();
        logs.set_stdout_enabled(false);
        let deleted = reset_profile_root(&root, &logs).await.unwrap();

        assert!(deleted);
        assert!(root.exists());
        assert!(!directory_has_entries(&root).unwrap());

        let _ = fs::remove_dir_all(root.parent().unwrap());
    }

    #[tokio::test]
    async fn profile_reset_creates_missing_webview_root_without_deleted_flag() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("ruster-tests")
            .join(uuid::Uuid::new_v4().to_string())
            .join("WebView2");
        let logs = LogBuffer::new();
        logs.set_stdout_enabled(false);

        let deleted = reset_profile_root(&root, &logs).await.unwrap();

        assert!(!deleted);
        assert!(root.exists());
        assert!(!directory_has_entries(&root).unwrap());

        let _ = fs::remove_dir_all(root.parent().unwrap());
    }

    #[tokio::test]
    async fn profile_reset_rejects_non_webview_root() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("ruster-tests")
            .join(uuid::Uuid::new_v4().to_string())
            .join("profiles");
        let logs = LogBuffer::new();
        logs.set_stdout_enabled(false);

        let error = reset_profile_root(&root, &logs).await.unwrap_err();

        assert!(error.message.contains("Unsafe WebView data path"));
    }

    #[test]
    fn pure_quality_refreshes_every_request() {
        let settings = AppSettings {
            web_view_pure_quality_mode: true,
            web_view_refresh_every_requests: 99,
            ..Default::default()
        };
        assert_eq!(usage_refresh_interval(&settings), 1);
    }

    #[test]
    fn configured_refresh_interval_is_clamped() {
        let mut settings = AppSettings {
            web_view_pure_quality_mode: false,
            web_view_refresh_every_requests: 250,
            ..Default::default()
        };
        assert_eq!(usage_refresh_interval(&settings), 200);
        settings.web_view_refresh_every_requests = 0;
        assert_eq!(usage_refresh_interval(&settings), 1);
    }

    #[test]
    fn session_refresh_cooldown_matches_ruster_policy() {
        assert_eq!(session_refresh_cooldown(false), Duration::from_secs(12));
        assert_eq!(session_refresh_cooldown(true), Duration::from_secs(2));
    }

    #[test]
    fn login_prompt_state_reads_hard_recovery_details() {
        let value = json!({
            "hasAnyPrompt": true,
            "signedOutDialog": true,
            "needsHardRecovery": true,
            "loginNudgeBanner": false,
            "loginButton": true,
            "blockingOverlay": true,
            "inputVisible": false,
            "inputBlocked": true,
            "details": {"dialogText": "Session expired"}
        });
        let state = LoginPromptState::from_value(&value);

        assert!(state.has_any_prompt);
        assert!(state.signed_out_dialog);
        assert!(state.needs_hard_recovery);
        assert!(state.login_button);
        assert!(state.blocking_overlay);
        assert!(state.input_blocked);
        assert_eq!(state.detail, "Session expired");
    }

    #[test]
    fn provider_login_recovery_scripts_include_expected_selectors() {
        let gemini = Provider::Gemini.login_prompt_state_script();
        let chatgpt = Provider::ChatGpt.recover_signed_out_script();

        assert!(gemini.contains("sign-in-nudge"));
        assert!(gemini.contains("signed-out-dialog"));
        assert!(chatgpt.contains("data-testid"));
        assert!(chatgpt.contains("new-chat"));
    }

    #[test]
    fn response_snapshot_script_exposes_provider_state_fields() {
        let gemini = Provider::Gemini.snapshot_script();
        let chatgpt = Provider::ChatGpt.snapshot_script();

        for script in [gemini, chatgpt] {
            assert!(script.contains("responseCount"));
            assert!(script.contains("isGenerating"));
            assert!(script.contains("readyForNextInput"));
            assert!(script.contains("sendEnabled"));
            assert!(script.contains("isStopped"));
        }
    }
}
