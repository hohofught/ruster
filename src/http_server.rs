use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, Response, StatusCode, Uri};
use axum::routing::any;
use parking_lot::RwLock;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, oneshot};

use crate::app_paths::AppPaths;
use crate::auto_prompt;
use crate::custom_api::{self, CustomApiPresetService};
use crate::diagnostics;
use crate::host::{HostError, TranslationMode, TranslatorHost, WebViewProvider};
use crate::ivlyrics;
use crate::ivlyrics_gate::IvLyricsGate;
use crate::ivlyrics_repair;
use crate::logging::{LogBuffer, summarize_text};
use crate::model_catalog;
use crate::prompt_config::PromptConfig;
use crate::proxy_dedup::ProxyDeduplicator;
use crate::settings::AppSettings;
use crate::usage_metrics::UsageMetrics;

const MAX_REQUEST_BODY_BYTES: usize = 1_000_000;
const MAX_SERVER_INFLIGHT_REQUESTS: usize = 256;
const RUSTER_MODEL_OWNER: &str = "ruster";
const RUSTER_OPENAI_ERROR_TYPE: &str = "ruster_error";
const RUSTER_GEMINI_MODEL_DESCRIPTION: &str = "ruster local Gemini bridge model";
const CUSTOM_API_PROVIDER: &str = "CustomApi";
const LISTENER_RETRY_DELAY: Duration = Duration::from_secs(3);
const LISTENER_MAX_RETRIES: usize = 5;

#[derive(Clone)]
pub struct ServerState {
    paths: AppPaths,
    settings: Arc<RwLock<AppSettings>>,
    host: Arc<TranslatorHost>,
    logs: LogBuffer,
    prompts: Arc<PromptConfig>,
    usage: UsageMetrics,
    custom_presets: CustomApiPresetService,
    slots: Arc<Semaphore>,
    dedup: ProxyDeduplicator,
    iv_gate: IvLyricsGate,
}

pub async fn serve(
    paths: AppPaths,
    settings: Arc<RwLock<AppSettings>>,
    host: Arc<TranslatorHost>,
    logs: LogBuffer,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<()> {
    let prompt_config = PromptConfig::load(&paths, &logs);
    let usage = UsageMetrics::new(&paths, logs.clone());
    let custom_presets = CustomApiPresetService::new(&paths, logs.clone());
    let state = ServerState {
        paths: paths.clone(),
        settings: settings.clone(),
        host,
        logs: logs.clone(),
        prompts: Arc::new(prompt_config),
        usage,
        custom_presets,
        slots: Arc::new(Semaphore::new(MAX_SERVER_INFLIGHT_REQUESTS)),
        dedup: ProxyDeduplicator::new(logs.clone()),
        iv_gate: IvLyricsGate::new(logs.clone()),
    };

    let (host_name, port) = {
        let settings = settings.read();
        (settings.host(), settings.port())
    };
    let ip = resolve_bind_ip(&host_name);
    let addr = SocketAddr::new(ip, port);
    let listener = bind_listener_with_retry(addr, &host_name, &logs, &mut shutdown).await?;
    let local_addr = listener.local_addr()?;
    logs.push(format!(
        "[HTTP] ruster listening on http://{}:{}/",
        host_name,
        local_addr.port()
    ));
    logs.push(format!(
        "[HTTP] Data: {} ({})",
        paths.data_dir.display(),
        if paths.using_portable_data {
            "portable"
        } else {
            "AppData"
        }
    ));

    let app = Router::new()
        .route("/{*path}", any(dispatch))
        .fallback(dispatch)
        .with_state(state);

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = shutdown.await;
        })
        .await?;
    logs.push("[HTTP] 서버 중지");
    Ok(())
}

async fn dispatch(
    State(state): State<ServerState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let path = uri.path().to_owned();
    let query = parse_query(uri.query().unwrap_or(""));

    if method == Method::OPTIONS {
        return empty_response(StatusCode::NO_CONTENT);
    }

    if let Some(response) = reject_unauthorized_before_slot(&state, &headers, &query, &path) {
        return response;
    }

    if let Some(remaining) = state.host.request_guard().is_active() {
        return request_guard_response(&path, remaining);
    }

    let Ok(_permit) = state.slots.acquire().await else {
        return proxy_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_busy",
            "server busy",
        );
    };

    if let Some(remaining) = state.host.request_guard().is_active() {
        return request_guard_response(&path, remaining);
    }

    let prefer_gemini_route = {
        let settings = state.settings.read().clone();
        should_prefer_gemini_route(&settings, &headers, &query, &path)
    };

    if prefer_gemini_route
        && state.settings.read().gemini_proxy_enabled
        && let Some(response) = handle_gemini(&state, &method, &path, &headers, &query, &body).await
    {
        return response;
    }

    if state.settings.read().open_ai_proxy_enabled
        && let Some(response) = handle_openai(&state, &method, &path, &headers, &query, &body).await
    {
        return response;
    }

    if !prefer_gemini_route
        && state.settings.read().gemini_proxy_enabled
        && let Some(response) = handle_gemini(&state, &method, &path, &headers, &query, &body).await
    {
        return response;
    }

    if let Some(response) = disabled_proxy_response(&state, &path) {
        return response;
    }

    handle_mort(&state, &method, &path, &headers, &query, &body).await
}

fn resolve_bind_ip(host: &str) -> IpAddr {
    if host.eq_ignore_ascii_case("localhost") {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        host.parse().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
    }
}

async fn bind_listener_with_retry(
    addr: SocketAddr,
    configured_host: &str,
    logs: &LogBuffer,
    shutdown: &mut oneshot::Receiver<()>,
) -> Result<TcpListener> {
    let max_attempts = LISTENER_MAX_RETRIES + 1;
    let mut last_error = None;

    for attempt in 1..=max_attempts {
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                if attempt > 1 {
                    logs.push(format!(
                        "[HTTP] 리스너 시작 성공 (재시도 {}/{})",
                        attempt - 1,
                        LISTENER_MAX_RETRIES
                    ));
                }
                return Ok(listener);
            }
            Err(error) => {
                log_listener_bind_error(configured_host, addr.port(), &error, logs);
                last_error = Some(error);
            }
        }

        if attempt > LISTENER_MAX_RETRIES {
            break;
        }

        logs.push(format!(
            "[HTTP] {}초 후 자동 재시도합니다. ({}/{})",
            LISTENER_RETRY_DELAY.as_secs(),
            attempt,
            LISTENER_MAX_RETRIES
        ));
        tokio::select! {
            _ = tokio::time::sleep(LISTENER_RETRY_DELAY) => {}
            _ = &mut *shutdown => {
                anyhow::bail!("[HTTP] 리스너 시작 대기 중 종료 신호를 받아 중단합니다.");
            }
        }
    }

    let message = format!(
        "http://{}:{}/ 리스너 시작 실패 (자동 재시도 {}회 소진)",
        configured_host,
        addr.port(),
        LISTENER_MAX_RETRIES
    );
    match last_error {
        Some(error) => Err(anyhow::anyhow!("{message}: {error}")),
        None => Err(anyhow::anyhow!(message)),
    }
}

fn log_listener_bind_error(configured_host: &str, port: u16, error: &io::Error, logs: &LogBuffer) {
    if error.kind() == io::ErrorKind::AddrInUse {
        logs.push(format!(
            "[HTTP] 포트 충돌: http://{}:{}/ 가 이미 사용 중입니다.",
            configured_host, port
        ));
        return;
    }

    if error.kind() == io::ErrorKind::PermissionDenied {
        logs.push(format!(
            "[HTTP] 액세스 거부: http://{}:{}/ 바인드 권한이 없습니다.",
            configured_host, port
        ));
        return;
    }

    logs.push(format!(
        "[HTTP] 리스너 시작 실패: {} (kind={:?}, raw_os_error={:?})",
        error,
        error.kind(),
        error.raw_os_error()
    ));
}

fn parse_query(query: &str) -> HashMap<String, Vec<String>> {
    url::form_urlencoded::parse(query.as_bytes()).fold(HashMap::new(), |mut map, (k, v)| {
        map.entry(k.to_string()).or_default().push(v.to_string());
        map
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpenAiRoute {
    Models,
    ChatCompletions,
    Responses,
    Completions,
}

fn openai_route_kind(path: &str) -> Option<OpenAiRoute> {
    let path = path.trim_end_matches('/').to_ascii_lowercase();
    match path.as_str() {
        "/v1/models" | "/models" => Some(OpenAiRoute::Models),
        "/v1/chat/completions"
        | "/chat/completions"
        | "/custom/chat/completions"
        | "/v1/custom/chat/completions" => Some(OpenAiRoute::ChatCompletions),
        "/v1/responses" | "/responses" => Some(OpenAiRoute::Responses),
        "/v1/completions" | "/completions" => Some(OpenAiRoute::Completions),
        _ => None,
    }
}

fn is_openai_path(path: &str) -> bool {
    openai_route_kind(path).is_some()
}

fn is_models_path(path: &str, allow_v1_models_path: bool) -> bool {
    path.eq_ignore_ascii_case("/models")
        || path.eq_ignore_ascii_case("/v2/models")
        || path.eq_ignore_ascii_case("/v1beta/models")
        || (allow_v1_models_path && path.eq_ignore_ascii_case("/v1/models"))
}

fn is_potential_gemini_path(path: &str, allow_v1_models_path: bool) -> bool {
    let path_lower = path.to_ascii_lowercase();
    is_models_path(path, allow_v1_models_path)
        || path_lower.starts_with("/models/")
        || path_lower.starts_with("/v1/models/")
        || path_lower.starts_with("/v2/models/")
        || path_lower.starts_with("/v1beta/models/")
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum GeminiRoute {
    Models,
    GenerateContent { model: String, stream: bool },
    UnsupportedAction { model: String, action: String },
}

fn gemini_route_kind(path: &str, allow_v1_models_path: bool) -> Option<GeminiRoute> {
    let path = path.trim_end_matches('/');
    if is_models_path(path, allow_v1_models_path) {
        return Some(GeminiRoute::Models);
    }

    let path_lower = path.to_ascii_lowercase();
    let prefixes = ["/models/", "/v1/models/", "/v2/models/", "/v1beta/models/"];
    for prefix in prefixes {
        if !path_lower.starts_with(prefix) {
            continue;
        }
        let rest = &path[prefix.len()..];
        let rest_lower = &path_lower[prefix.len()..];
        if let Some(model_lower) = rest_lower.strip_suffix(":generatecontent") {
            let model = &rest[..model_lower.len()];
            return Some(GeminiRoute::GenerateContent {
                model: urlencoding::decode(model).ok()?.to_string(),
                stream: false,
            });
        }
        if let Some(model_lower) = rest_lower.strip_suffix(":streamgeneratecontent") {
            let model = &rest[..model_lower.len()];
            return Some(GeminiRoute::GenerateContent {
                model: urlencoding::decode(model).ok()?.to_string(),
                stream: true,
            });
        }
        if let Some(colon_index) = rest_lower.rfind(':') {
            let model = &rest[..colon_index];
            let action = &rest[colon_index + 1..];
            return Some(GeminiRoute::UnsupportedAction {
                model: urlencoding::decode(model).ok()?.to_string(),
                action: action.to_owned(),
            });
        }
    }
    None
}

fn should_prefer_gemini_route(
    settings: &AppSettings,
    headers: &HeaderMap,
    query: &HashMap<String, Vec<String>>,
    path: &str,
) -> bool {
    settings.gemini_proxy_enabled
        && is_potential_gemini_path(path, true)
        && has_gemini_api_key_shape(headers, query)
}

fn has_gemini_api_key_shape(headers: &HeaderMap, query: &HashMap<String, Vec<String>>) -> bool {
    header_to_str(headers, "x-goog-api-key")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
        || query
            .get("key")
            .map(|values| values.iter().any(|value| !value.trim().is_empty()))
            .unwrap_or(false)
}

fn reject_unauthorized_before_slot(
    state: &ServerState,
    headers: &HeaderMap,
    query: &HashMap<String, Vec<String>>,
    path: &str,
) -> Option<Response<Body>> {
    let settings = state.settings.read().clone();
    if !settings.proxy_api_key_required() {
        return None;
    }

    let protected = if should_prefer_gemini_route(&settings, headers, query, path) {
        Some("gemini")
    } else if settings.open_ai_proxy_enabled && is_openai_path(path) {
        Some("openai")
    } else if settings.gemini_proxy_enabled
        && is_potential_gemini_path(path, !settings.open_ai_proxy_enabled)
    {
        Some("gemini")
    } else if is_protected_mort_path(path) {
        Some("mort")
    } else {
        None
    }?;

    if validate_api_key(&settings, headers, query) {
        return None;
    }

    let saw_candidate = !candidates(headers, query).is_empty();
    let message = if saw_candidate {
        "Invalid local API key"
    } else {
        "Local API key is required"
    };
    state.logs.push(format!(
        "[HTTP] 인증 실패: path={path}, route={}, reason={}",
        protected_route_log_label(protected),
        if saw_candidate { "invalid" } else { "missing" }
    ));
    state.usage.record_failure(
        protected_provider_label(protected),
        "auth",
        None,
        401,
        message,
    );
    let mut response = match protected {
        "gemini" => json_response(
            StatusCode::UNAUTHORIZED,
            &json!({"error":{"code":401,"message":message,"status":"UNAUTHENTICATED"}}),
        ),
        "mort" => raw_json_response(
            StatusCode::UNAUTHORIZED,
            custom_api::build_mort_json_response("", message, "401"),
        ),
        _ => openai_error_response(401, message),
    };
    response.headers_mut().insert(
        "WWW-Authenticate",
        HeaderValue::from_static("Bearer realm=\"ruster\""),
    );
    Some(response)
}

fn protected_provider_label(value: &str) -> &'static str {
    match value {
        "openai" => "OpenAI",
        "gemini" => "Gemini",
        "mort" => CUSTOM_API_PROVIDER,
        _ => "Unknown",
    }
}

fn protected_route_log_label(value: &str) -> &str {
    match value {
        "mort" => "customapi",
        value => value,
    }
}

fn is_protected_mort_path(path: &str) -> bool {
    if path.eq_ignore_ascii_case("/custom/presets") || path.eq_ignore_ascii_case("/custom/presets/")
    {
        return true;
    }
    path.to_ascii_lowercase().starts_with("/custom/")
        && !path.eq_ignore_ascii_case("/custom/chat/completions")
        && !path.eq_ignore_ascii_case("/v1/custom/chat/completions")
}

fn validate_api_key(
    settings: &AppSettings,
    headers: &HeaderMap,
    query: &HashMap<String, Vec<String>>,
) -> bool {
    if !settings.proxy_api_key_required() {
        return true;
    }
    candidates(headers, query)
        .into_iter()
        .any(|candidate| settings.matches_any_local_api_key(&candidate))
}

fn candidates(headers: &HeaderMap, query: &HashMap<String, Vec<String>>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(value) = header_to_str(headers, "authorization") {
        for raw in value.split(',') {
            let candidate = raw.trim();
            let token = if candidate
                .get(.."Bearer ".len())
                .map(|prefix| prefix.eq_ignore_ascii_case("Bearer "))
                .unwrap_or(false)
            {
                candidate.get("Bearer ".len()..).unwrap_or("")
            } else {
                candidate
            };
            push_candidate(&mut out, token);
        }
    }
    for name in ["x-api-key", "api-key", "x-goog-api-key"] {
        if let Some(value) = header_to_str(headers, name) {
            for candidate in value.split(',') {
                push_candidate(&mut out, candidate);
            }
        }
    }
    for name in ["key", "api_key", "access_token"] {
        if let Some(values) = query.get(name) {
            for candidate in values {
                push_candidate(&mut out, candidate);
            }
        }
    }
    out
}

fn push_candidate(out: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if !value.is_empty() {
        out.push(value.to_owned());
    }
}

fn header_to_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn disabled_proxy_response(state: &ServerState, path: &str) -> Option<Response<Body>> {
    let settings = state.settings.read();
    let openai_only = matches!(
        openai_route_kind(path),
        Some(OpenAiRoute::ChatCompletions | OpenAiRoute::Responses | OpenAiRoute::Completions)
    );

    if openai_only && !settings.open_ai_proxy_enabled {
        return Some(proxy_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "openai_proxy_disabled",
            "OpenAI compatible proxy is disabled",
        ));
    }

    if path.eq_ignore_ascii_case("/v1/models")
        && !settings.open_ai_proxy_enabled
        && !settings.gemini_proxy_enabled
    {
        return Some(proxy_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "proxy_disabled",
            "Both OpenAI and Gemini proxies are disabled",
        ));
    }

    if is_potential_gemini_path(path, !settings.open_ai_proxy_enabled)
        && !settings.gemini_proxy_enabled
    {
        return Some(proxy_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "gemini_proxy_disabled",
            "Gemini compatible proxy is disabled",
        ));
    }

    None
}

async fn forward_prompt_with_options(
    state: &ServerState,
    prompt: &str,
    raw: bool,
    timeout: Duration,
    cli_route: Option<ivlyrics::IvLyricsPromptKind>,
    model_override: Option<&str>,
    translation_target: Option<&str>,
    preferred_provider: WebViewProvider,
) -> Result<String, HostError> {
    if let Some(kind) = cli_route {
        let request_id = diagnostics::next_request_id();
        let route_label = ivlyrics_direct_route_label(kind);
        state.logs.push(format!(
            "[Forward#{request_id}] {route_label} fast lane: dedup/merge 우회, raw CLI 전송 (len={}, hash={})",
            prompt.len(),
            diagnostics::fingerprint(prompt)
        ));
        return state
            .host
            .send_ivlyrics_direct_prompt_with_webview_limit_fallback(prompt, timeout, kind)
            .await;
    }

    let backend_prompt = if raw {
        prompt.to_owned()
    } else {
        build_translation_forward_prompt(prompt, translation_target)
    };
    let settings_snapshot = state.settings.read().clone();
    let backend_scope = format!(
        "cli:{}",
        model_catalog::normalize_cli_model(&settings_snapshot.gemini_cli_model)
    );
    let mode = if raw { "raw" } else { "translate" };
    let model_scope = model_override
        .and_then(model_catalog::find_cli)
        .map(|model| model.id.to_owned())
        .unwrap_or_else(|| "selected".to_owned());
    let key = format!(
        "forward:{mode}:{backend_scope}:{model_scope}:{}:{}",
        backend_prompt.len(),
        diagnostics::fingerprint(&backend_prompt)
    );
    let request_id = diagnostics::next_request_id();
    let host = state.host.clone();
    let model_override = model_override.map(ToOwned::to_owned);
    if preferred_provider != WebViewProvider::Current {
        let provider = preferred_provider;
        let key = format!("{key}:provider:{}", provider.label());
        return state
            .dedup
            .run(key, "Forward", request_id, mode, move || async move {
                host.send_raw_prompt_to_webview_provider(provider, &backend_prompt, timeout)
                    .await
            })
            .await;
    }

    state
        .dedup
        .run(key, "Forward", request_id, mode, move || async move {
            host.send_raw_prompt_with_model(&backend_prompt, timeout, model_override.as_deref())
                .await
        })
        .await
}

async fn forward_cli_raw_prompt(
    state: &ServerState,
    prompt: &str,
    timeout: Duration,
    model_override: Option<&str>,
) -> Result<String, HostError> {
    let settings_snapshot = state.settings.read().clone();
    let backend_scope = format!(
        "cli:{}",
        model_catalog::normalize_cli_model(&settings_snapshot.gemini_cli_model)
    );
    let model_scope = model_override
        .and_then(model_catalog::find_cli)
        .map(|model| model.id.to_owned())
        .unwrap_or_else(|| "selected".to_owned());
    let key = format!(
        "forward:cli-raw:{backend_scope}:{model_scope}:{}:{}",
        prompt.len(),
        diagnostics::fingerprint(prompt)
    );
    let request_id = diagnostics::next_request_id();
    let host = state.host.clone();
    let prompt = prompt.to_owned();
    let model_override = model_override.map(ToOwned::to_owned);

    state
        .dedup
        .run(key, "Forward", request_id, "cli-raw", move || async move {
            host.send_cli_raw_prompt(&prompt, timeout, model_override.as_deref())
                .await
        })
        .await
}

fn build_translation_forward_prompt(text: &str, target: Option<&str>) -> String {
    let target = normalize_translation_target(target);
    format!(
        "Translate the following text to {target}.\n\n\
Rules:\n\
- Output only the translated text.\n\
- Preserve line breaks and simple formatting.\n\
- Do not add explanations, summaries, quotes, Markdown, or code fences.\n\
- If a line is already in the target language, keep it natural and do not invent new content.\n\n\
INPUT_TEXT_START\n{}\nINPUT_TEXT_END\nOUTPUT_TEXT_START",
        text.trim_end()
    )
}

fn normalize_translation_target(target: Option<&str>) -> String {
    let value = target.unwrap_or("").trim();
    if value.is_empty() {
        return "Korean (한국어)".to_owned();
    }

    match value.to_ascii_lowercase().as_str() {
        "ko" | "kor" | "ko-kr" | "korean" => "Korean (한국어)".to_owned(),
        "en" | "eng" | "en-us" | "en-gb" | "english" => "English".to_owned(),
        "ja" | "jpn" | "jp" | "ja-jp" | "japanese" => "Japanese (日本語)".to_owned(),
        "zh" | "zho" | "zh-cn" | "zh-tw" | "chinese" => "Chinese (中文)".to_owned(),
        _ => value.to_owned(),
    }
}

async fn prepare_prompt_for_forwarding(
    state: &ServerState,
    prompt: &str,
    allow_ivlyrics_rewrite: bool,
) -> (
    String,
    Option<ivlyrics::IvLyricsPromptRewriteResult>,
    Option<ivlyrics::IvLyricsPromptKind>,
) {
    let detected_kind = ivlyrics::try_detect_kind(prompt);
    if allow_ivlyrics_rewrite {
        if detected_kind == Some(ivlyrics::IvLyricsPromptKind::Translation)
            && let Some(input) = ivlyrics::try_extract_translation_rewrite_input(prompt)
        {
            let settings = state.settings.read().clone();
            let rewritten = auto_prompt::build_translation_prompt(
                &state.paths,
                &state.host,
                &settings,
                &state.prompts,
                &input,
                &state.logs,
            )
            .await;
            let result = ivlyrics::IvLyricsPromptRewriteResult {
                kind: ivlyrics::IvLyricsPromptKind::Translation,
                prompt: rewritten.prompt,
                line_count: input.line_count,
                strip_number_tags_from_response: true,
                source_lines: Vec::new(),
                original_prompt: prompt.to_owned(),
                preferred_provider: rewritten.preferred_provider,
            };
            return (result.prompt.clone(), Some(result), detected_kind);
        }
        if detected_kind == Some(ivlyrics::IvLyricsPromptKind::Phonetic)
            && let Some(result) = ivlyrics::try_rewrite_phonetic(prompt, &state.prompts)
        {
            return (result.prompt.clone(), Some(result), detected_kind);
        }
        if detected_kind == Some(ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz)
            && let Some(result) = ivlyrics::try_rewrite_lyrics_study_quiz(prompt)
        {
            return (result.prompt.clone(), Some(result), detected_kind);
        }
    }
    (prompt.to_owned(), None, detected_kind)
}

fn detect_ivlyrics_cli_prompt(
    primary: &str,
    fallback: &str,
) -> Option<(ivlyrics::IvLyricsPromptKind, String, String)> {
    if let Some(category) = ivlyrics::try_detect_lyrics_study_category(primary) {
        return Some((
            ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz,
            category,
            primary.to_owned(),
        ));
    }
    if let Some(kind) = ivlyrics::try_detect_kind(primary)
        && ivlyrics::is_cli_direct_prompt_kind(Some(kind))
    {
        return Some((kind, String::new(), primary.to_owned()));
    }
    if !fallback.trim().is_empty() && primary != fallback {
        if let Some(category) = ivlyrics::try_detect_lyrics_study_category(fallback) {
            return Some((
                ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz,
                category,
                fallback.to_owned(),
            ));
        }
        if let Some(kind) = ivlyrics::try_detect_kind(fallback)
            && ivlyrics::is_cli_direct_prompt_kind(Some(kind))
        {
            return Some((kind, String::new(), fallback.to_owned()));
        }
    }
    None
}

fn direct_cli_route_for_kind(
    settings: &AppSettings,
    kind: Option<ivlyrics::IvLyricsPromptKind>,
) -> Option<ivlyrics::IvLyricsPromptKind> {
    match kind {
        Some(ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz)
            if settings.iv_lyrics_study_cli_direct_enabled =>
        {
            Some(ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz)
        }
        Some(ivlyrics::IvLyricsPromptKind::Phonetic)
        | Some(ivlyrics::IvLyricsPromptKind::CharacterPronunciation)
            if settings.iv_lyrics_phonetic_use_cli_wrapper_enabled =>
        {
            kind
        }
        _ => None,
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

fn detect_ivlyrics_study_category(primary: &str, fallback: &str) -> Option<String> {
    ivlyrics::try_detect_lyrics_study_category(primary).or_else(|| {
        if !fallback.trim().is_empty() && primary != fallback {
            ivlyrics::try_detect_lyrics_study_category(fallback)
        } else {
            None
        }
    })
}

fn should_route_ivlyrics_study_to_cli(
    settings: &AppSettings,
    primary: &str,
    fallback: &str,
) -> Option<String> {
    if !settings.iv_lyrics_study_cli_direct_enabled {
        return None;
    }
    detect_ivlyrics_study_category(primary, fallback)
}

fn select_ivlyrics_study_raw_prompt<'a>(primary: &'a str, fallback: &'a str) -> &'a str {
    if detect_ivlyrics_study_category(primary, "").is_some() {
        primary
    } else {
        fallback
    }
}

fn preferred_provider_for_forward(
    state: &ServerState,
    rewrite: Option<&ivlyrics::IvLyricsPromptRewriteResult>,
    cli_route: Option<ivlyrics::IvLyricsPromptKind>,
) -> WebViewProvider {
    if cli_route.is_some() || state.host.mode() == TranslationMode::GeminiCli {
        return WebViewProvider::Current;
    }

    rewrite
        .filter(|rewrite| rewrite.kind == ivlyrics::IvLyricsPromptKind::Translation)
        .map(|rewrite| rewrite.preferred_provider)
        .unwrap_or(WebViewProvider::Current)
}

fn map_host_error(
    provider: &str,
    route: &str,
    input: &str,
    state: &ServerState,
    error: HostError,
) -> Response<Body> {
    let status = error.status_code();
    state
        .usage
        .record_failure(provider, route, Some(input), status, &error.to_string());
    match provider {
        "Gemini" => {
            gemini_error_response(status, status_to_gemini_code(status), &error.to_string())
        }
        CUSTOM_API_PROVIDER => raw_json_response(
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            custom_api::build_mort_json_response("", &error.to_string(), &status.to_string()),
        ),
        _ => openai_error_response(status, &error.to_string()),
    }
}

fn is_retry_stale(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case("Retry_Stale")
}

fn retry_stale_response(
    provider: &str,
    route: &str,
    input: &str,
    state: &ServerState,
) -> Response<Body> {
    let message = "Request cancelled - newer request exists";
    state
        .usage
        .record_failure(provider, route, Some(input), 499, message);
    if provider == "Gemini" {
        gemini_error_response(
            499,
            "CANCELLED",
            "Request stale - cancelled by newer request",
        )
    } else {
        openai_error_response(499, message)
    }
}

fn status_to_gemini_code(status: u16) -> &'static str {
    match status {
        400 => "INVALID_ARGUMENT",
        401 => "UNAUTHENTICATED",
        403 => "PERMISSION_DENIED",
        404 => "NOT_FOUND",
        429 => "RESOURCE_EXHAUSTED",
        499 => "CANCELLED",
        503 => "UNAVAILABLE",
        504 => "DEADLINE_EXCEEDED",
        _ => "INTERNAL",
    }
}

fn openai_models_payload() -> Value {
    let mut data = vec![json!({
        "id": "gpt-4o-mini",
        "object": "model",
        "created": 0,
        "owned_by": RUSTER_MODEL_OWNER
    })];
    data.extend(model_catalog::api_models().into_iter().map(|m| {
        json!({
            "id": m.id,
            "object": "model",
            "created": 0,
            "owned_by": RUSTER_MODEL_OWNER
        })
    }));
    json!({"object":"list","data":data})
}

async fn handle_openai(
    state: &ServerState,
    method: &Method,
    path: &str,
    _headers: &HeaderMap,
    _query: &HashMap<String, Vec<String>>,
    body: &Bytes,
) -> Option<Response<Body>> {
    let route = openai_route_kind(path)?;

    if method == Method::GET && route == OpenAiRoute::Models {
        return Some(json_response(StatusCode::OK, &openai_models_payload()));
    }

    if method != Method::POST || route == OpenAiRoute::Models {
        return Some(openai_error_response(
            405,
            "Method not allowed for OpenAI route",
        ));
    }

    if body.len() > MAX_REQUEST_BODY_BYTES {
        state
            .usage
            .record_failure("OpenAI", path, None, 413, "Request body too large");
        return Some(openai_error_response(413, "Request body too large"));
    }

    let body_text = String::from_utf8_lossy(body).to_string();
    if body_text.trim().is_empty() {
        state
            .usage
            .record_failure("OpenAI", path, Some(&body_text), 400, "Empty request body");
        return Some(openai_error_response(400, "Empty request body"));
    }

    let root = match serde_json::from_str::<Value>(&body_text) {
        Ok(root) => root,
        Err(error) => {
            let message = invalid_json_message(error);
            state
                .usage
                .record_failure("OpenAI", path, Some(&body_text), 400, &message);
            return Some(openai_error_response(400, &message));
        }
    };
    Some(match route {
        OpenAiRoute::ChatCompletions => handle_openai_chat(state, path, &root).await,
        OpenAiRoute::Responses => handle_openai_responses(state, &root).await,
        OpenAiRoute::Completions => handle_openai_completions(state, &root).await,
        OpenAiRoute::Models => openai_error_response(405, "Method not allowed for OpenAI route"),
    })
}

async fn handle_openai_chat(state: &ServerState, path: &str, root: &Value) -> Response<Body> {
    let request_id = diagnostics::next_request_id();
    let request_started_at = Instant::now();
    let stream = root.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let model = root
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("gemini-raw")
        .to_owned();

    let Some(messages) = root.get("messages").and_then(Value::as_array) else {
        state
            .usage
            .record_failure("OpenAI", "chat", None, 400, "No messages provided");
        return openai_error_response(400, "No messages provided");
    };

    let (mut prompt, latest_user_text) = build_openai_chat_prompt(messages);
    if prompt.trim().is_empty() {
        state
            .usage
            .record_failure("OpenAI", "chat", None, 400, "No user message found");
        return openai_error_response(400, "No user message found");
    }

    let raw_mode = state.host.raw_prompt_mode();
    let custom_translate_route = path.eq_ignore_ascii_case("/custom/chat/completions")
        || path.eq_ignore_ascii_case("/v1/custom/chat/completions");
    let raw_chat_prompt = prompt.clone();
    let direct_candidate = detect_ivlyrics_cli_prompt(&latest_user_text, &raw_chat_prompt);
    let direct_kind = direct_candidate.as_ref().map(|(kind, _, _)| *kind);
    let detected_study = direct_candidate
        .as_ref()
        .and_then(|(kind, category, _)| {
            (*kind == ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz).then(|| category.clone())
        })
        .filter(|category| !category.is_empty());
    let cli_route = {
        let settings = state.settings.read();
        direct_cli_route_for_kind(&settings, direct_kind)
    };
    let route_ivlyrics_direct_to_cli = cli_route.is_some();
    let route_ivlyrics_pronunciation_to_cli = matches!(
        cli_route,
        Some(
            ivlyrics::IvLyricsPromptKind::Phonetic
                | ivlyrics::IvLyricsPromptKind::CharacterPronunciation
        )
    );
    if !route_ivlyrics_direct_to_cli {
        prompt = apply_openai_response_format_instruction(prompt, root.get("response_format"));
    }
    let (prompt_to_send, rewrite, iv_kind) = if route_ivlyrics_direct_to_cli {
        let (_, _, selected_prompt) = direct_candidate.unwrap_or((
            cli_route.unwrap_or(ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz),
            String::new(),
            raw_chat_prompt.clone(),
        ));
        (selected_prompt, None, cli_route)
    } else {
        prepare_prompt_for_forwarding(state, &prompt, true).await
    };
    let gate_kind = iv_kind.or(if detected_study.is_some() {
        Some(ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz)
    } else {
        None
    });

    state.logs.push(format!(
        "[OpenAIProxy#{request_id}] chat 요청 (path={path}, stream={stream}, raw={raw_mode}, ivKind={:?}, ivStudy={}, ivStudyCliDirect={}, ivPhoneticCli={}, rewrite={}, backend={}, promptHash={}, sendHash={}, {})",
        iv_kind,
        detected_study.as_deref().unwrap_or("None"),
        route_ivlyrics_direct_to_cli,
        route_ivlyrics_pronunciation_to_cli,
        rewrite.is_some(),
        cli_route.map(ivlyrics_direct_route_label).unwrap_or("selected"),
        diagnostics::fingerprint(&prompt),
        diagnostics::fingerprint(&prompt_to_send),
        summarize_text(&prompt, 100)
    ));

    let _iv_gate = state
        .iv_gate
        .begin("OpenAIProxy", request_id, gate_kind, &prompt)
        .await;
    let raw_forward = rewrite.is_some() || !custom_translate_route || raw_mode;
    let result = match forward_prompt_with_options(
        state,
        &prompt_to_send,
        raw_forward,
        Duration::from_secs(150),
        cli_route,
        Some(&model),
        None,
        preferred_provider_for_forward(state, rewrite.as_ref(), cli_route),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return map_host_error("OpenAI", "chat", &prompt_to_send, state, error),
    };
    if is_retry_stale(&result) {
        return retry_stale_response("OpenAI", "chat", &prompt_to_send, state);
    }
    let result = normalize_forwarded_ivlyrics_result(
        state,
        "OpenAIProxy",
        request_id,
        result,
        rewrite.as_ref(),
        if cli_route == Some(ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz) {
            detected_study.as_deref().unwrap_or("")
        } else {
            ""
        },
    )
    .await;

    state
        .usage
        .record_success("OpenAI", "chat", Some(&prompt_to_send), Some(&result));
    log_response_ready(
        state,
        ResponseReadyLog {
            label: "OpenAIProxy",
            request_id,
            route: "chat",
            stream,
            status: StatusCode::OK,
            started_at: request_started_at,
            prompt: &prompt_to_send,
            result: &result,
        },
    );
    if stream {
        openai_chat_stream_response(&result, &model)
    } else {
        json_response(
            StatusCode::OK,
            &json!({
                "id": format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
                "object": "chat.completion",
                "created": chrono::Utc::now().timestamp(),
                "model": model,
                "choices": [{
                    "index": 0,
                    "message": {"role":"assistant","content":result},
                    "finish_reason":"stop"
                }],
                "usage": {
                    "prompt_tokens": prompt.len(),
                    "completion_tokens": result.len(),
                    "total_tokens": prompt.len() + result.len()
                }
            }),
        )
    }
}

async fn handle_openai_responses(state: &ServerState, root: &Value) -> Response<Body> {
    let request_id = diagnostics::next_request_id();
    let request_started_at = Instant::now();
    let stream = root.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let model = root
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("gemini-raw")
        .to_owned();
    let mut prompt = build_responses_prompt(root);
    if prompt.trim().is_empty() {
        state
            .usage
            .record_failure("OpenAI", "responses", None, 400, "No input provided");
        return openai_error_response(400, "No input provided");
    }
    let format = root
        .get("response_format")
        .or_else(|| root.get("text").and_then(|t| t.get("format")));
    let raw_mode = state.host.raw_prompt_mode();
    let raw_response_prompt = prompt.clone();
    let direct_candidate = detect_ivlyrics_cli_prompt(&raw_response_prompt, "");
    let direct_kind = direct_candidate.as_ref().map(|(kind, _, _)| *kind);
    let detected_study = direct_candidate
        .as_ref()
        .and_then(|(kind, category, _)| {
            (*kind == ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz).then(|| category.clone())
        })
        .filter(|category| !category.is_empty());
    let cli_route = {
        let settings = state.settings.read();
        direct_cli_route_for_kind(&settings, direct_kind)
    };
    let route_ivlyrics_direct_to_cli = cli_route.is_some();
    let route_ivlyrics_pronunciation_to_cli = matches!(
        cli_route,
        Some(
            ivlyrics::IvLyricsPromptKind::Phonetic
                | ivlyrics::IvLyricsPromptKind::CharacterPronunciation
        )
    );
    if !route_ivlyrics_direct_to_cli {
        prompt = apply_openai_response_format_instruction(prompt, format);
    }
    let (prompt_to_send, rewrite, iv_kind) = if route_ivlyrics_direct_to_cli {
        let (_, _, selected_prompt) = direct_candidate.unwrap_or((
            cli_route.unwrap_or(ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz),
            String::new(),
            raw_response_prompt,
        ));
        (selected_prompt, None, cli_route)
    } else {
        prepare_prompt_for_forwarding(state, &prompt, true).await
    };
    let gate_kind = iv_kind.or(if detected_study.is_some() {
        Some(ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz)
    } else {
        None
    });
    state.logs.push(format!(
        "[OpenAIProxy#{request_id}] responses 요청 (stream={stream}, raw={raw_mode}, ivKind={:?}, ivStudy={}, ivStudyCliDirect={}, ivPhoneticCli={}, rewrite={}, promptHash={}, sendHash={})",
        iv_kind,
        detected_study.as_deref().unwrap_or("None"),
        route_ivlyrics_direct_to_cli,
        route_ivlyrics_pronunciation_to_cli,
        rewrite.is_some(),
        diagnostics::fingerprint(&prompt),
        diagnostics::fingerprint(&prompt_to_send)
    ));
    let _iv_gate = state
        .iv_gate
        .begin("OpenAIProxy", request_id, gate_kind, &prompt)
        .await;
    let result = match forward_prompt_with_options(
        state,
        &prompt_to_send,
        true,
        Duration::from_secs(150),
        cli_route,
        Some(&model),
        None,
        preferred_provider_for_forward(state, rewrite.as_ref(), cli_route),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return map_host_error("OpenAI", "responses", &prompt_to_send, state, error),
    };
    if is_retry_stale(&result) {
        return retry_stale_response("OpenAI", "responses", &prompt_to_send, state);
    }
    let result = normalize_forwarded_ivlyrics_result(
        state,
        "OpenAIProxy",
        request_id,
        result,
        rewrite.as_ref(),
        if cli_route == Some(ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz) {
            detected_study.as_deref().unwrap_or("")
        } else {
            ""
        },
    )
    .await;
    state
        .usage
        .record_success("OpenAI", "responses", Some(&prompt_to_send), Some(&result));
    log_response_ready(
        state,
        ResponseReadyLog {
            label: "OpenAIProxy",
            request_id,
            route: "responses",
            stream,
            status: StatusCode::OK,
            started_at: request_started_at,
            prompt: &prompt_to_send,
            result: &result,
        },
    );
    if stream {
        openai_responses_stream_response(&result, &model)
    } else {
        json_response(
            StatusCode::OK,
            &json!({
                "id": format!("resp_{}", uuid::Uuid::new_v4().simple()),
                "object": "response",
                "created_at": chrono::Utc::now().timestamp(),
                "status": "completed",
                "model": model,
                "output": [{
                    "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type":"output_text","text": result,"annotations": []}]
                }],
                "output_text": result,
                "usage": {"input_tokens": 0, "output_tokens": result.len(), "total_tokens": result.len()}
            }),
        )
    }
}

async fn handle_openai_completions(state: &ServerState, root: &Value) -> Response<Body> {
    let request_id = diagnostics::next_request_id();
    let request_started_at = Instant::now();
    let stream = root.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let model = root
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("gemini-raw")
        .to_owned();
    let prompt = extract_prompt_value(root.get("prompt"));
    if prompt.trim().is_empty() {
        return openai_error_response(400, "No prompt provided");
    }
    let raw_mode = state.host.raw_prompt_mode();
    let direct_candidate = detect_ivlyrics_cli_prompt(&prompt, "");
    let direct_kind = direct_candidate.as_ref().map(|(kind, _, _)| *kind);
    let detected_study = direct_candidate
        .as_ref()
        .and_then(|(kind, category, _)| {
            (*kind == ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz).then(|| category.clone())
        })
        .filter(|category| !category.is_empty());
    let cli_route = {
        let settings = state.settings.read();
        direct_cli_route_for_kind(&settings, direct_kind)
    };
    let route_ivlyrics_direct_to_cli = cli_route.is_some();
    let route_ivlyrics_pronunciation_to_cli = matches!(
        cli_route,
        Some(
            ivlyrics::IvLyricsPromptKind::Phonetic
                | ivlyrics::IvLyricsPromptKind::CharacterPronunciation
        )
    );
    let (prompt_to_send, rewrite, iv_kind) = if route_ivlyrics_direct_to_cli {
        let (_, _, selected_prompt) = direct_candidate.unwrap_or((
            cli_route.unwrap_or(ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz),
            String::new(),
            prompt.clone(),
        ));
        (selected_prompt, None, cli_route)
    } else {
        prepare_prompt_for_forwarding(state, &prompt, true).await
    };
    let gate_kind = iv_kind.or(if detected_study.is_some() {
        Some(ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz)
    } else {
        None
    });
    state.logs.push(format!(
        "[OpenAIProxy#{request_id}] completions 요청 (stream={stream}, raw={raw_mode}, ivKind={:?}, ivStudy={}, ivStudyCliDirect={}, ivPhoneticCli={}, rewrite={}, promptHash={}, sendHash={})",
        iv_kind,
        detected_study.as_deref().unwrap_or("None"),
        route_ivlyrics_direct_to_cli,
        route_ivlyrics_pronunciation_to_cli,
        rewrite.is_some(),
        diagnostics::fingerprint(&prompt),
        diagnostics::fingerprint(&prompt_to_send)
    ));
    let _iv_gate = state
        .iv_gate
        .begin("OpenAIProxy", request_id, gate_kind, &prompt)
        .await;
    let result = match forward_prompt_with_options(
        state,
        &prompt_to_send,
        true,
        Duration::from_secs(150),
        cli_route,
        Some(&model),
        None,
        preferred_provider_for_forward(state, rewrite.as_ref(), cli_route),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            return map_host_error("OpenAI", "completions", &prompt_to_send, state, error);
        }
    };
    if is_retry_stale(&result) {
        return retry_stale_response("OpenAI", "completions", &prompt_to_send, state);
    }
    let result = normalize_forwarded_ivlyrics_result(
        state,
        "OpenAIProxy",
        request_id,
        result,
        rewrite.as_ref(),
        if cli_route == Some(ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz) {
            detected_study.as_deref().unwrap_or("")
        } else {
            ""
        },
    )
    .await;
    state.usage.record_success(
        "OpenAI",
        "completions",
        Some(&prompt_to_send),
        Some(&result),
    );
    log_response_ready(
        state,
        ResponseReadyLog {
            label: "OpenAIProxy",
            request_id,
            route: "completions",
            stream,
            status: StatusCode::OK,
            started_at: request_started_at,
            prompt: &prompt_to_send,
            result: &result,
        },
    );
    if stream {
        openai_completion_stream_response(&result, &model)
    } else {
        json_response(
            StatusCode::OK,
            &json!({
                "id": format!("cmpl-{}", uuid::Uuid::new_v4().simple()),
                "object": "text_completion",
                "created": chrono::Utc::now().timestamp(),
                "model": model,
                "choices": [{"text": result, "index": 0, "logprobs": null, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": prompt.len(), "completion_tokens": result.len(), "total_tokens": prompt.len() + result.len()}
            }),
        )
    }
}

fn gemini_models_payload() -> Value {
    let models: Vec<_> = model_catalog::api_models()
        .into_iter()
        .map(|m| {
            json!({
                "name": format!("models/{}", m.id),
                "displayName": m.display_name,
                "description": RUSTER_GEMINI_MODEL_DESCRIPTION,
                "supportedGenerationMethods": ["generateContent", "streamGenerateContent"],
                "inputTokenLimit": m.input_token_limit,
                "outputTokenLimit": m.output_token_limit
            })
        })
        .collect();
    json!({"models": models})
}

fn invalid_json_message(error: serde_json::Error) -> String {
    format!("Invalid JSON: {error}")
}

fn gemini_unsupported_action_message(action: &str) -> String {
    format!("Gemini action '{action}' is not implemented by this proxy")
}

async fn handle_gemini(
    state: &ServerState,
    method: &Method,
    path: &str,
    _headers: &HeaderMap,
    query: &HashMap<String, Vec<String>>,
    body: &Bytes,
) -> Option<Response<Body>> {
    let allow_v1_models = !state.settings.read().open_ai_proxy_enabled;
    let route = gemini_route_kind(path, allow_v1_models);
    if route.is_none() && !is_potential_gemini_path(path, allow_v1_models) {
        return None;
    }

    if method == Method::GET && route == Some(GeminiRoute::Models) {
        return Some(json_response(StatusCode::OK, &gemini_models_payload()));
    }

    if method != Method::POST {
        return Some(gemini_error_response(
            405,
            "METHOD_NOT_ALLOWED",
            "Method not allowed for Gemini route",
        ));
    }

    let Some(route) = route else {
        return Some(gemini_error_response(
            404,
            "NOT_FOUND",
            "Gemini route not found",
        ));
    };
    let (requested_model, stream) = match route {
        GeminiRoute::GenerateContent { model, stream } => (model, stream),
        GeminiRoute::UnsupportedAction { action, .. } => {
            let message = gemini_unsupported_action_message(&action);
            state
                .usage
                .record_failure("Gemini", &action, None, 501, &message);
            return Some(gemini_error_response(501, "UNIMPLEMENTED", &message));
        }
        GeminiRoute::Models => {
            return Some(gemini_error_response(
                405,
                "METHOD_NOT_ALLOWED",
                "Method not allowed for Gemini route",
            ));
        }
    };
    if stream
        && query
            .get("alt")
            .and_then(|v| v.first())
            .map(|alt| !alt.eq_ignore_ascii_case("sse"))
            .unwrap_or(false)
    {
        return Some(gemini_error_response(
            501,
            "UNIMPLEMENTED",
            "Only alt=sse is supported for streamGenerateContent",
        ));
    }

    if body.len() > MAX_REQUEST_BODY_BYTES {
        state.usage.record_failure(
            "Gemini",
            "generateContent",
            None,
            413,
            "Request body too large",
        );
        return Some(gemini_error_response(
            413,
            "INVALID_ARGUMENT",
            "Request body too large",
        ));
    }

    let body_text = String::from_utf8_lossy(body).to_string();
    if body_text.trim().is_empty() {
        state
            .usage
            .record_failure("Gemini", "generateContent", None, 400, "Empty request body");
        return Some(gemini_error_response(
            400,
            "INVALID_ARGUMENT",
            "Empty request body",
        ));
    }
    let root = match serde_json::from_str::<Value>(&body_text) {
        Ok(root) => root,
        Err(error) => {
            let message = invalid_json_message(error);
            state.usage.record_failure(
                "Gemini",
                "generateContent",
                Some(&body_text),
                400,
                &message,
            );
            return Some(gemini_error_response(400, "INVALID_ARGUMENT", &message));
        }
    };
    let prompt = extract_gemini_generate_prompt(&root);
    if prompt.trim().is_empty() {
        state.usage.record_failure(
            "Gemini",
            "generateContent",
            Some(&body_text),
            400,
            "No user prompt found in contents",
        );
        return Some(gemini_error_response(
            400,
            "INVALID_ARGUMENT",
            "No user prompt found in contents",
        ));
    }
    let raw_mode = state.host.raw_prompt_mode();
    let request_id = diagnostics::next_request_id();
    let request_started_at = Instant::now();
    let direct_candidate = detect_ivlyrics_cli_prompt(&prompt, "");
    let direct_kind = direct_candidate.as_ref().map(|(kind, _, _)| *kind);
    let detected_study = direct_candidate
        .as_ref()
        .and_then(|(kind, category, _)| {
            (*kind == ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz).then(|| category.clone())
        })
        .filter(|category| !category.is_empty());
    let cli_route = {
        let settings = state.settings.read();
        direct_cli_route_for_kind(&settings, direct_kind)
    };
    let route_ivlyrics_direct_to_cli = cli_route.is_some();
    let route_ivlyrics_pronunciation_to_cli = matches!(
        cli_route,
        Some(
            ivlyrics::IvLyricsPromptKind::Phonetic
                | ivlyrics::IvLyricsPromptKind::CharacterPronunciation
        )
    );
    let (prompt_to_send, rewrite, iv_kind) = if route_ivlyrics_direct_to_cli {
        let (_, _, selected_prompt) = direct_candidate.unwrap_or((
            cli_route.unwrap_or(ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz),
            String::new(),
            prompt.clone(),
        ));
        (selected_prompt, None, cli_route)
    } else {
        prepare_prompt_for_forwarding(state, &prompt, true).await
    };
    let gate_kind = iv_kind.or(if detected_study.is_some() {
        Some(ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz)
    } else {
        None
    });
    state.logs.push(format!(
        "[GeminiProxy#{request_id}] 요청 (model={requested_model}, stream={stream}, raw={raw_mode}, ivKind={:?}, ivStudy={}, ivStudyCliDirect={}, ivPhoneticCli={}, rewrite={}, backend={}, promptHash={}, sendHash={})",
        iv_kind,
        detected_study.as_deref().unwrap_or("None"),
        route_ivlyrics_direct_to_cli,
        route_ivlyrics_pronunciation_to_cli,
        rewrite.is_some(),
        cli_route.map(ivlyrics_direct_route_label).unwrap_or("selected"),
        diagnostics::fingerprint(&prompt),
        diagnostics::fingerprint(&prompt_to_send)
    ));
    let _iv_gate = state
        .iv_gate
        .begin("GeminiProxy", request_id, gate_kind, &prompt)
        .await;
    let result = match forward_prompt_with_options(
        state,
        &prompt_to_send,
        true,
        Duration::from_secs(150),
        cli_route,
        Some(&requested_model),
        None,
        preferred_provider_for_forward(state, rewrite.as_ref(), cli_route),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            return Some(map_host_error(
                "Gemini",
                "generateContent",
                &prompt_to_send,
                state,
                error,
            ));
        }
    };
    if is_retry_stale(&result) {
        return Some(retry_stale_response(
            "Gemini",
            "generateContent",
            &prompt_to_send,
            state,
        ));
    }
    let result = normalize_forwarded_ivlyrics_result(
        state,
        "GeminiProxy",
        request_id,
        result,
        rewrite.as_ref(),
        if cli_route == Some(ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz) {
            detected_study.as_deref().unwrap_or("")
        } else {
            ""
        },
    )
    .await;
    state.usage.record_success(
        "Gemini",
        "generateContent",
        Some(&prompt_to_send),
        Some(&result),
    );
    log_response_ready(
        state,
        ResponseReadyLog {
            label: "GeminiProxy",
            request_id,
            route: "generateContent",
            stream,
            status: StatusCode::OK,
            started_at: request_started_at,
            prompt: &prompt_to_send,
            result: &result,
        },
    );
    Some(if stream {
        gemini_stream_response(&result, &requested_model)
    } else {
        json_response(
            StatusCode::OK,
            &json!({
                "candidates": [{
                    "content": {"role":"model","parts":[{"text": result}]},
                    "finishReason": "STOP",
                    "index": 0
                }],
                "usageMetadata": {
                    "promptTokenCount": prompt.len(),
                    "candidatesTokenCount": result.len(),
                    "totalTokenCount": prompt.len() + result.len()
                },
                "modelVersion": requested_model
            }),
        )
    })
}

async fn handle_mort(
    state: &ServerState,
    method: &Method,
    path: &str,
    _headers: &HeaderMap,
    _query: &HashMap<String, Vec<String>>,
    body: &Bytes,
) -> Response<Body> {
    if path.eq_ignore_ascii_case("/custom/presets") || path.eq_ignore_ascii_case("/custom/presets/")
    {
        if method != Method::GET {
            return raw_json_response(
                StatusCode::METHOD_NOT_ALLOWED,
                custom_api::build_mort_json_response("", "Method Not Allowed", "405"),
            );
        }
        let presets: Vec<_> = state
            .custom_presets
            .get_all()
            .into_iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "mode": p.mode,
                    "timeoutSeconds": p.timeout_seconds,
                    "hasRequestTemplate": !p.request_template.trim().is_empty(),
                    "hasResponseTemplate": !p.response_template.trim().is_empty()
                })
            })
            .collect();
        return json_response(StatusCode::OK, &json!({"presets": presets}));
    }

    if method != Method::POST {
        return raw_json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            custom_api::build_mort_json_response("", "Method Not Allowed", "405"),
        );
    }

    if body.len() > MAX_REQUEST_BODY_BYTES {
        state.usage.record_failure(
            CUSTOM_API_PROVIDER,
            path,
            None,
            413,
            "Request body too large",
        );
        return raw_json_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            custom_api::build_mort_json_response("", "요청 본문이 너무 큽니다.", "413"),
        );
    }

    let body_text = String::from_utf8_lossy(body).to_string();
    if body_text.trim().is_empty() {
        return raw_json_response(
            StatusCode::BAD_REQUEST,
            custom_api::build_mort_json_response("", "요청 본문에 텍스트를 넣어주세요.", "400"),
        );
    }
    let incoming = custom_api::extract_incoming_text(&body_text);
    if incoming.text.trim().is_empty() {
        return raw_json_response(
            StatusCode::BAD_REQUEST,
            custom_api::build_mort_json_response("", "번역할 텍스트를 찾을 수 없습니다.", "400"),
        );
    }

    let preset_name = custom_preset_name(path);
    let preset = match preset_name.as_deref() {
        Some(name) => match state.custom_presets.find(name) {
            Some(preset) => Some(preset),
            None => {
                return raw_json_response(
                    StatusCode::NOT_FOUND,
                    custom_api::build_mort_json_response(
                        "",
                        &format!("커스텀 API 프리셋을 찾을 수 없습니다: {name}"),
                        "404",
                    ),
                );
            }
        },
        None => None,
    };
    let settings_snapshot = state.settings.read().clone();
    let host_raw_mode = state.host.raw_prompt_mode();
    let use_mort_cli_raw =
        settings_snapshot.mort_cli_raw_mode && state.host.mode() == TranslationMode::GeminiCli;
    let prompt = custom_api::build_prompt(
        &incoming.text,
        preset.as_ref(),
        &incoming.source_code,
        &incoming.result_code,
        use_mort_cli_raw && preset.is_none() && !host_raw_mode,
    );
    let preset_builds_prompt = preset
        .as_ref()
        .map(|p| !p.request_template.trim().is_empty())
        .unwrap_or(false);
    let raw_prompt = preset
        .as_ref()
        .map(|p| p.mode.eq_ignore_ascii_case(custom_api::MODE_RAW_PROMPT))
        .unwrap_or(false)
        || host_raw_mode
        || preset_builds_prompt;
    let timeout = Duration::from_secs(preset.as_ref().map(|p| p.timeout_seconds).unwrap_or(60));
    let direct_candidate = detect_ivlyrics_cli_prompt(&incoming.text, &prompt);
    let direct_kind = direct_candidate.as_ref().map(|(kind, _, _)| *kind);
    let detected_study = direct_candidate
        .as_ref()
        .and_then(|(kind, category, _)| {
            (*kind == ivlyrics::IvLyricsPromptKind::LyricsStudyQuiz).then(|| category.clone())
        })
        .filter(|category| !category.is_empty());
    let cli_route = {
        let settings = state.settings.read();
        direct_cli_route_for_kind(&settings, direct_kind)
    };
    let route_ivlyrics_direct_to_cli = cli_route.is_some();
    let route_ivlyrics_pronunciation_to_cli = matches!(
        cli_route,
        Some(
            ivlyrics::IvLyricsPromptKind::Phonetic
                | ivlyrics::IvLyricsPromptKind::CharacterPronunciation
        )
    );
    let forward_input = if route_ivlyrics_direct_to_cli {
        direct_candidate
            .as_ref()
            .map(|(_, _, selected_prompt)| selected_prompt.as_str())
            .unwrap_or(prompt.as_str())
    } else {
        prompt.as_str()
    };
    let prompt_mode = if route_ivlyrics_direct_to_cli {
        cli_route
            .map(ivlyrics_direct_route_label)
            .unwrap_or("iv-direct-cli")
    } else if use_mort_cli_raw {
        if preset.is_some() {
            "custom-cli-raw"
        } else if host_raw_mode {
            "raw-body-cli-raw"
        } else {
            "default-translate-cli-raw"
        }
    } else if raw_prompt {
        "raw"
    } else {
        "translate"
    };

    let request_id = diagnostics::next_request_id();
    state.logs.push(format!(
        "[CustomApi#{request_id}] 요청 (path={path}, preset={}, mode={prompt_mode}, raw={raw_prompt}, mortCliRaw={use_mort_cli_raw}, ivKind={:?}, ivStudy={}, ivStudyCliDirect={}, ivPhoneticCli={}, backend={}, textHash={}, promptHash={}, sendHash={}, {})",
        preset.as_ref().map(|p| p.name.as_str()).unwrap_or(""),
        direct_kind,
        detected_study.as_deref().unwrap_or("None"),
        route_ivlyrics_direct_to_cli,
        route_ivlyrics_pronunciation_to_cli,
        cli_route.map(ivlyrics_direct_route_label).unwrap_or("selected"),
        diagnostics::fingerprint(&incoming.text),
        diagnostics::fingerprint(&prompt),
        diagnostics::fingerprint(forward_input),
        summarize_text(&incoming.text, 100)
    ));

    let forwarded = if use_mort_cli_raw && !route_ivlyrics_direct_to_cli {
        forward_cli_raw_prompt(state, forward_input, timeout, None).await
    } else {
        forward_prompt_with_options(
            state,
            forward_input,
            raw_prompt || route_ivlyrics_direct_to_cli,
            timeout,
            cli_route,
            None,
            Some(&incoming.result_code),
            WebViewProvider::Current,
        )
        .await
    };
    let translation = match forwarded {
        Ok(result) => result,
        Err(error) => {
            return map_host_error(CUSTOM_API_PROVIDER, path, forward_input, state, error);
        }
    };
    let json = if preset.is_some() {
        custom_api::build_custom_json_response(
            preset.as_ref(),
            &translation,
            &incoming.text,
            &incoming.source_code,
            &incoming.result_code,
        )
    } else {
        custom_api::build_mort_json_response(&translation, "", "0")
    };
    state.usage.record_success(
        CUSTOM_API_PROVIDER,
        path,
        Some(forward_input),
        Some(&translation),
    );
    raw_json_response(StatusCode::OK, json)
}

fn extract_gemini_generate_prompt(root: &Value) -> String {
    let system = root
        .get("systemInstruction")
        .and_then(|s| s.get("parts"))
        .and_then(Value::as_array)
        .map(|parts| join_gemini_parts(parts))
        .unwrap_or_default();

    let mut prompt = String::new();
    if let Some(contents) = root.get("contents").and_then(Value::as_array) {
        for content in contents.iter().rev() {
            if content
                .get("role")
                .and_then(Value::as_str)
                .map(|role| !role.eq_ignore_ascii_case("user"))
                .unwrap_or(false)
            {
                continue;
            }
            prompt = content
                .get("parts")
                .and_then(Value::as_array)
                .map(|parts| join_gemini_parts(parts))
                .unwrap_or_default();
            if !prompt.trim().is_empty() {
                break;
            }
        }

        if prompt.trim().is_empty() {
            for content in contents.iter().rev() {
                prompt = content
                    .get("parts")
                    .and_then(Value::as_array)
                    .map(|parts| join_gemini_parts(parts))
                    .unwrap_or_default();
                if !prompt.trim().is_empty() {
                    break;
                }
            }
        }
    }

    if system.trim().is_empty() {
        prompt
    } else if prompt.trim().is_empty() {
        system
    } else {
        format!("[system]\n{}\n\n[user]\n{}", system.trim(), prompt)
    }
}

fn join_gemini_parts(parts: &[Value]) -> String {
    parts
        .iter()
        .filter_map(|part| {
            part.get("text")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .collect::<Vec<_>>()
        .join("")
}

fn custom_preset_name(path: &str) -> Option<String> {
    let lower = path.to_ascii_lowercase();
    if !lower.starts_with("/custom/") {
        return None;
    }
    let rest = path.get("/custom/".len()..)?.trim_matches('/');
    if rest.is_empty() || rest.eq_ignore_ascii_case("presets") {
        return None;
    }
    rest.split('/')
        .next()
        .and_then(|s| urlencoding::decode(s).ok())
        .map(|s| s.to_string())
}

async fn normalize_forwarded_ivlyrics_result(
    state: &ServerState,
    label: &str,
    request_id: u64,
    result: String,
    rewrite: Option<&ivlyrics::IvLyricsPromptRewriteResult>,
    study_category: &str,
) -> String {
    if is_retry_stale(&result) {
        return result;
    }

    if !study_category.trim().is_empty() {
        let (normalized, used_empty_fallback, detail) =
            ivlyrics_repair::normalize_study_json_or_empty(&result, study_category);
        if used_empty_fallback {
            state.logs.push(format!(
                "[{label}#{request_id}] ivLyrics study {study_category} invalid JSON - empty shape returned ({detail})"
            ));
        } else {
            state.logs.push(format!(
                "[{label}#{request_id}] ivLyrics study {study_category} JSON 정규화 완료 (resultLen={}, resultHash={})",
                normalized.len(),
                diagnostics::fingerprint(&normalized)
            ));
        }
        return normalized;
    }

    let repair_state = state.clone();
    let repaired = ivlyrics_repair::repair_if_needed(
        label,
        request_id,
        result,
        rewrite,
        state.prompts.as_ref(),
        move |repair_prompt, timeout| {
            let repair_state = repair_state.clone();
            async move {
                forward_prompt_with_options(
                    &repair_state,
                    &repair_prompt,
                    true,
                    timeout,
                    None,
                    None,
                    None,
                    WebViewProvider::Current,
                )
                .await
                .map_err(|error| error.to_string())
            }
        },
        |message| state.logs.push(message),
    )
    .await;

    ivlyrics_repair::strip_number_tags_if_needed(repaired, rewrite)
}

struct ResponseReadyLog<'a> {
    label: &'a str,
    request_id: u64,
    route: &'a str,
    stream: bool,
    status: StatusCode,
    started_at: Instant,
    prompt: &'a str,
    result: &'a str,
}

fn log_response_ready(state: &ServerState, event: ResponseReadyLog<'_>) {
    state.logs.push(format!(
        "[{label}#{request_id}] {route} 응답 전송 완료 (status={}, stream={}, totalMs={}, sendHash={}, resultHash={}, resultLen={})",
        event.status.as_u16(),
        event.stream,
        event.started_at.elapsed().as_millis(),
        diagnostics::fingerprint(event.prompt),
        diagnostics::fingerprint(event.result),
        event.result.len(),
        label = event.label,
        request_id = event.request_id,
        route = event.route
    ));
}

fn apply_openai_response_format_instruction(
    mut prompt: String,
    response_format: Option<&Value>,
) -> String {
    let Some(format) = response_format else {
        return prompt;
    };
    let Some(kind) = format.get("type").and_then(Value::as_str) else {
        return prompt;
    };
    if kind.eq_ignore_ascii_case("json_object") {
        prompt.push_str("\n\nReturn only a valid JSON object. Do not wrap it in markdown.");
    } else if kind.eq_ignore_ascii_case("json_schema") {
        let schema = format
            .get("json_schema")
            .or_else(|| format.get("schema"))
            .map(Value::to_string)
            .unwrap_or_default();
        if schema.is_empty() {
            prompt.push_str(
                "\n\nReturn only JSON matching the requested schema. Do not wrap it in markdown.",
            );
        } else {
            prompt.push_str(
                "\n\nReturn only JSON matching this schema. Do not wrap it in markdown.\n",
            );
            prompt.push_str(&schema);
        }
    }
    prompt
}

fn build_openai_chat_prompt(messages: &[Value]) -> (String, String) {
    let mut normalized = Vec::new();
    let mut latest_user_text = String::new();

    for message in messages {
        let text = message
            .get("content")
            .map(extract_openai_message_content)
            .unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }

        let role = message
            .get("role")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|role| !role.is_empty())
            .unwrap_or("user")
            .to_owned();
        if role.eq_ignore_ascii_case("user") {
            latest_user_text = text.clone();
        }
        normalized.push((role, text));
    }

    if normalized.is_empty() {
        return (String::new(), latest_user_text);
    }

    if normalized.len() == 1 && normalized[0].0.eq_ignore_ascii_case("user") {
        return (normalized.remove(0).1, latest_user_text);
    }

    let mut out = String::new();
    out.push_str("You are processing an OpenAI-compatible chat completion request.\n");
    out.push_str("Preserve the intent of each role and answer the latest user request.\n\n");
    for (role, text) in normalized {
        out.push('[');
        out.push_str(&role);
        out.push_str("]\n");
        out.push_str(&text);
        out.push_str("\n\n");
    }
    while out.ends_with('\n') {
        out.pop();
    }

    (out, latest_user_text)
}

fn extract_openai_message_content(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                match part {
                    Value::String(text) => out.push_str(text),
                    Value::Object(obj) => {
                        if let Some(text) = obj.get("text").and_then(Value::as_str) {
                            out.push_str(text);
                            continue;
                        }

                        let Some(part_type) = obj.get("type").and_then(Value::as_str) else {
                            continue;
                        };
                        if matches!(
                            part_type.to_ascii_lowercase().as_str(),
                            "text" | "input_text" | "output_text"
                        ) && let Some(text) = obj.get("text").and_then(Value::as_str)
                        {
                            out.push_str(text);
                            continue;
                        }

                        if part_type.to_ascii_lowercase().contains("image") {
                            out.push('\n');
                            out.push_str(
                                "[image input omitted: this local text proxy cannot inspect image content]",
                            );
                            out.push('\n');
                        }
                    }
                    _ => {}
                }
            }
            out
        }
        Value::Object(obj) => obj
            .get("text")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn build_responses_prompt(root: &Value) -> String {
    let mut items = Vec::new();
    if let Some(instructions) = root.get("instructions").and_then(Value::as_str)
        && !instructions.trim().is_empty()
    {
        items.push(format!("[instructions]\n{}", instructions.trim()));
    }
    if let Some(input) = root.get("input") {
        let input_text = extract_openai_responses_input(input);
        if !input_text.trim().is_empty() {
            items.push(input_text);
        }
    }
    items.join("\n\n").trim().to_owned()
}

fn extract_openai_responses_input(input: &Value) -> String {
    match input {
        Value::String(text) => text.clone(),
        Value::Array(array) => array
            .iter()
            .filter_map(|item| match item {
                Value::String(text) => Some(text.clone()),
                Value::Object(obj) => {
                    let role = obj.get("role").and_then(Value::as_str).unwrap_or("user");
                    let content = obj.get("content").map(extract_openai_message_content)?;
                    if content.trim().is_empty() {
                        None
                    } else {
                        Some(format!("[{role}]\n{content}"))
                    }
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(obj) => {
            if let Some(content) = obj.get("content") {
                extract_openai_message_content(content)
            } else {
                obj.get("text")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .unwrap_or_default()
            }
        }
        _ => String::new(),
    }
}

fn extract_prompt_value(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(array)) => array
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(_) | Value::Bool(_) => Some(v.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Number(_) | Value::Bool(_)) => value.unwrap().to_string(),
        Some(Value::Null) => String::new(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn openai_chat_stream_response(text: &str, model: &str) -> Response<Body> {
    let mut out = String::new();
    for chunk in chunks(text, 900) {
        out.push_str("data: ");
        out.push_str(
            &json!({
                "id": format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
                "object": "chat.completion.chunk",
                "created": chrono::Utc::now().timestamp(),
                "model": model,
                "choices": [{"index":0,"delta":{"content":chunk},"finish_reason": null}]
            })
            .to_string(),
        );
        out.push_str("\n\n");
    }
    out.push_str("data: ");
    out.push_str(
        &json!({
            "id": format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            "object": "chat.completion.chunk",
            "created": chrono::Utc::now().timestamp(),
            "model": model,
            "choices": [{"index":0,"delta":{},"finish_reason":"stop"}]
        })
        .to_string(),
    );
    out.push_str("\n\ndata: [DONE]\n\n");
    sse_response(out)
}

fn openai_responses_stream_response(text: &str, model: &str) -> Response<Body> {
    let response_id = format!("resp_{}", uuid::Uuid::new_v4().simple());
    let mut out = String::new();
    out.push_str("event: response.created\n");
    out.push_str("data: ");
    out.push_str(&json!({"type":"response.created","response":{"id":response_id,"object":"response","created_at":chrono::Utc::now().timestamp(),"status":"in_progress","model":model}}).to_string());
    out.push_str("\n\n");
    for (index, chunk) in chunks(text, 900).into_iter().enumerate() {
        out.push_str("event: response.output_text.delta\n");
        out.push_str("data: ");
        out.push_str(&json!({"type":"response.output_text.delta","response_id":response_id,"output_index":0,"content_index":0,"item_id":format!("msg_{}", response_id.trim_start_matches("resp_")),"delta":chunk,"sequence_number":index}).to_string());
        out.push_str("\n\n");
    }
    out.push_str("event: response.completed\n");
    out.push_str("data: ");
    out.push_str(&json!({"type":"response.completed","response":{"id":response_id,"object":"response","created_at":chrono::Utc::now().timestamp(),"status":"completed","model":model,"output_text":text}}).to_string());
    out.push_str("\n\ndata: [DONE]\n\n");
    sse_response(out)
}

fn openai_completion_stream_response(text: &str, model: &str) -> Response<Body> {
    let mut out = String::new();
    for chunk in chunks(text, 900) {
        out.push_str("data: ");
        out.push_str(
            &json!({
                "id": format!("cmpl-{}", uuid::Uuid::new_v4().simple()),
                "object": "text_completion",
                "created": chrono::Utc::now().timestamp(),
                "model": model,
                "choices": [{"text": chunk, "index": 0, "logprobs": null, "finish_reason": null}]
            })
            .to_string(),
        );
        out.push_str("\n\n");
    }
    out.push_str("data: [DONE]\n\n");
    sse_response(out)
}

fn gemini_stream_response(text: &str, model: &str) -> Response<Body> {
    let mut out = String::new();
    for chunk in gemini_stream_chunks(text) {
        out.push_str("data: ");
        out.push_str(
            &json!({
                "candidates": [{"content":{"role":"model","parts":[{"text": chunk}]},"index":0}],
                "modelVersion": model
            })
            .to_string(),
        );
        out.push_str("\n\n");
    }
    if text.is_empty() {
        out.push_str("data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"\"}]},\"index\":0}]}\n\n");
    }
    out.push_str("data: ");
    out.push_str(
        &json!({
            "candidates": [{"finishReason":"STOP","index":0}],
            "modelVersion": model
        })
        .to_string(),
    );
    out.push_str("\n\n");
    sse_response(out)
}

fn gemini_stream_chunks(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }

    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let lines = normalized.split('\n').collect::<Vec<_>>();
    let last = lines.len().saturating_sub(1);
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            if index < last {
                format!("{line}\n")
            } else {
                (*line).to_owned()
            }
        })
        .collect()
}

fn chunks(text: &str, size: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if current.len() >= size {
            chunks.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn request_guard_response(path: &str, remaining: Duration) -> Response<Body> {
    let seconds = duration_seconds_ceiling(remaining);
    let message = format!("F12 request guard active. Retry after {seconds}s.");
    let mut response = if is_openai_path(path) {
        json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({"error":{"message":message,"type":"request_guard_active","code":"request_guard_active"}}),
        )
    } else if is_potential_gemini_path(path, true) {
        json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({"error":{"code":503,"message":message,"status":"UNAVAILABLE"}}),
        )
    } else {
        json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({"result":"","errorMessage":message,"errorCode":"503"}),
        )
    };
    response.headers_mut().insert(
        "Retry-After",
        HeaderValue::from_str(&seconds.to_string()).unwrap(),
    );
    response
}

fn duration_seconds_ceiling(value: Duration) -> u64 {
    let seconds = value.as_secs() + u64::from(value.subsec_nanos() > 0);
    seconds.max(1)
}

fn proxy_error_response(status: StatusCode, code: &str, message: &str) -> Response<Body> {
    json_response(status, &json!({"error":{"code":code,"message":message}}))
}

fn openai_error_response(status: u16, message: &str) -> Response<Body> {
    json_response(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        &json!({"error":{"message":message,"type":RUSTER_OPENAI_ERROR_TYPE,"code":status}}),
    )
}

fn gemini_error_response(status: u16, code: &str, message: &str) -> Response<Body> {
    json_response(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        &json!({"error":{"code":status,"message":message,"status":code}}),
    )
}

fn json_response<T: Serialize>(status: StatusCode, payload: &T) -> Response<Body> {
    let json = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_owned());
    raw_json_response(status, json)
}

fn raw_json_response(status: StatusCode, json: String) -> Response<Body> {
    let content_length = json.len().to_string();
    let mut response = Response::builder()
        .status(status)
        .header("content-type", "application/json; charset=utf-8")
        .header("content-length", content_length)
        .body(Body::from(json))
        .unwrap();
    add_cors(response.headers_mut());
    response
}

fn sse_response(body: String) -> Response<Body> {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream; charset=utf-8")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .header("x-accel-buffering", "no")
        .body(Body::from(body))
        .unwrap();
    add_cors(response.headers_mut());
    response
}

fn empty_response(status: StatusCode) -> Response<Body> {
    let mut response = Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap();
    add_cors(response.headers_mut());
    response
}

fn add_cors(headers: &mut HeaderMap) {
    headers.insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));
    headers.insert(
        "Access-Control-Allow-Methods",
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        "Access-Control-Allow-Headers",
        HeaderValue::from_static(
            "Content-Type, Authorization, Accept, X-Api-Key, x-api-key, api-key, X-Goog-Api-Key, x-goog-api-key",
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_models_with_key_prefers_gemini_even_when_openai_is_enabled() {
        let settings = AppSettings::default();
        let headers = HeaderMap::new();
        let query = parse_query("key=AIza-test");

        assert!(settings.open_ai_proxy_enabled);
        assert!(is_openai_path("/models"));
        assert!(should_prefer_gemini_route(
            &settings, &headers, &query, "/models"
        ));
    }

    #[test]
    fn openai_models_without_gemini_key_stays_openai() {
        let settings = AppSettings::default();
        let headers = HeaderMap::new();
        let query = parse_query("");

        assert!(is_openai_path("/models"));
        assert!(!should_prefer_gemini_route(
            &settings, &headers, &query, "/models"
        ));
    }

    #[test]
    fn x_goog_api_key_prefers_gemini_route() {
        let settings = AppSettings::default();
        let mut headers = HeaderMap::new();
        headers.insert("x-goog-api-key", HeaderValue::from_static("AIza-test"));
        let query = parse_query("");

        assert!(should_prefer_gemini_route(
            &settings,
            &headers,
            &query,
            "/v1beta/models/gemini-2.5-flash:generateContent"
        ));
    }

    #[test]
    fn openai_route_matching_is_case_insensitive() {
        assert_eq!(
            openai_route_kind("/V1/CHAT/COMPLETIONS/"),
            Some(OpenAiRoute::ChatCompletions)
        );
        assert_eq!(
            openai_route_kind("/CUSTOM/CHAT/COMPLETIONS"),
            Some(OpenAiRoute::ChatCompletions)
        );
    }

    #[test]
    fn gemini_route_matching_is_case_insensitive() {
        assert_eq!(
            gemini_route_kind("/V1BETA/MODELS/gemini-2.5-flash:generateContent", true),
            Some(GeminiRoute::GenerateContent {
                model: "gemini-2.5-flash".to_owned(),
                stream: false,
            })
        );
        assert_eq!(
            gemini_route_kind(
                "/v1beta/models/gemini-2.5-flash:STREAMGENERATECONTENT",
                true
            ),
            Some(GeminiRoute::GenerateContent {
                model: "gemini-2.5-flash".to_owned(),
                stream: true,
            })
        );
    }

    #[test]
    fn gemini_unsupported_actions_are_detected_before_json_parsing() {
        assert_eq!(
            gemini_route_kind("/v1beta/models/gemini-2.5-flash:countTokens", true),
            Some(GeminiRoute::UnsupportedAction {
                model: "gemini-2.5-flash".to_owned(),
                action: "countTokens".to_owned(),
            })
        );
        assert_eq!(
            gemini_unsupported_action_message("countTokens"),
            "Gemini action 'countTokens' is not implemented by this proxy"
        );
    }

    #[test]
    fn openai_compatible_route_rejects_wrong_bearer_as_local_auth_candidate() {
        let settings = AppSettings {
            require_proxy_api_key: true,
            ..Default::default()
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer sk-not-a-ruster-local-key"),
        );
        let query = parse_query("");

        assert!(!validate_api_key(&settings, &headers, &query));
    }

    #[test]
    fn auth_candidates_match_ruster_trimming_rules() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("BEARER rst-local, bearer second"),
        );
        headers.insert("x-api-key", HeaderValue::from_static(" , third "));
        let query = parse_query("key=%20&api_key=fourth");

        assert_eq!(
            candidates(&headers, &query),
            vec![
                "rst-local".to_owned(),
                "second".to_owned(),
                "third".to_owned(),
                "fourth".to_owned()
            ]
        );
    }

    #[test]
    fn mort_custom_route_matching_is_case_insensitive() {
        assert!(is_protected_mort_path("/CUSTOM/PRESETS"));
        assert!(is_protected_mort_path("/CUSTOM/foo"));
        assert!(!is_protected_mort_path("/CUSTOM/chat/completions"));
        assert_eq!(
            custom_preset_name("/CUSTOM/Foo%20Bar"),
            Some("Foo Bar".to_owned())
        );
        assert_eq!(custom_preset_name("/custom/presets"), None);
    }

    #[test]
    fn ivlyrics_study_cli_direct_setting_enables_cli_first_route() {
        let prompt = "You are a language learning tutor inside a lyrics app.\n\
Category: quiz\n\
Build one category.\n\
Input lines:\n[{\"index\":0,\"text\":\"hello\"}]";
        let mut settings = AppSettings::default();

        assert_eq!(
            detect_ivlyrics_study_category(prompt, ""),
            Some("quiz".to_owned())
        );
        assert!(should_route_ivlyrics_study_to_cli(&settings, prompt, "").is_none());

        settings.iv_lyrics_study_cli_direct_enabled = true;
        assert_eq!(
            should_route_ivlyrics_study_to_cli(&settings, prompt, ""),
            Some("quiz".to_owned())
        );
    }

    #[test]
    fn ivlyrics_study_direct_prefers_latest_raw_user_prompt() {
        let study = "You are a language learning tutor inside a lyrics app.\n\
Category: quiz\n\
Build one category.\n\
Input lines:\n[{\"index\":0,\"text\":\"hello\"}]";
        let wrapped = format!(
            "You are processing an OpenAI-compatible chat completion request.\n\n[system]\nrule\n\n[user]\n{study}"
        );

        assert_eq!(select_ivlyrics_study_raw_prompt(study, &wrapped), study);
        assert_eq!(select_ivlyrics_study_raw_prompt("", &wrapped), wrapped);
    }

    #[test]
    fn retry_stale_detection_is_case_insensitive_and_trimmed() {
        assert!(is_retry_stale(" Retry_Stale\r\n"));
        assert!(!is_retry_stale("Retry_Stale extra"));
    }

    #[test]
    fn translation_forward_prompt_wraps_plain_text_for_translate_mode() {
        let prompt = build_translation_forward_prompt("hello\nworld\n", Some("ko"));

        assert!(prompt.contains("Translate the following text to Korean (한국어)."));
        assert!(prompt.contains("INPUT_TEXT_START\nhello\nworld\nINPUT_TEXT_END"));
        assert!(prompt.contains("Output only the translated text."));
    }

    #[test]
    fn openai_single_user_chat_prompt_uses_raw_shortcut() {
        let messages = vec![json!({"role":"user","content":"안녕"})];
        let (prompt, latest_user) = build_openai_chat_prompt(&messages);

        assert_eq!(prompt, "안녕");
        assert_eq!(latest_user, "안녕");
    }

    #[test]
    fn openai_multi_role_chat_prompt_uses_role_preamble() {
        let messages = vec![
            json!({"role":"system","content":"규칙"}),
            json!({"role":"user","content":"첫 요청"}),
            json!({"role":"assistant","content":"이전 답"}),
            json!({"role":"user","content":"최신 요청"}),
        ];
        let (prompt, latest_user) = build_openai_chat_prompt(&messages);

        assert_eq!(latest_user, "최신 요청");
        assert_eq!(
            prompt,
            "You are processing an OpenAI-compatible chat completion request.\n\
Preserve the intent of each role and answer the latest user request.\n\n\
[system]\n\
규칙\n\n\
[user]\n\
첫 요청\n\n\
[assistant]\n\
이전 답\n\n\
[user]\n\
최신 요청"
        );
    }

    #[test]
    fn openai_message_content_handles_multimodal_contract() {
        let content = json!([
            "A",
            {"type":"input_text","text":"B"},
            7,
            true,
            {"type":"image_url","image_url":{"url":"data:"}},
            {"type":"output_text","text":"C"}
        ]);

        assert_eq!(
            extract_openai_message_content(&content),
            "AB\n[image input omitted: this local text proxy cannot inspect image content]\nC"
        );
    }

    #[test]
    fn openai_responses_prompt_uses_expected_extraction_rules() {
        let root = json!({
            "instructions": "  system rule  ",
            "input": [
                "loose text",
                {"role":"assistant","content":"assistant text"},
                {"role":"user","text":"ignored by array extractor"},
                {"role":"user","content":[{"text":"final"}, {"type":"image_url"}]},
                123
            ]
        });

        assert_eq!(
            build_responses_prompt(&root),
            "[instructions]\n\
system rule\n\n\
loose text\n\n\
[assistant]\n\
assistant text\n\n\
[user]\n\
final\n[image input omitted: this local text proxy cannot inspect image content]"
        );
    }

    #[test]
    fn openai_response_format_instruction_handles_json_schema_variants() {
        let with_schema = apply_openai_response_format_instruction(
            "프롬프트".to_owned(),
            Some(&json!({"type":"json_schema","schema":{"type":"object"}})),
        );
        assert_eq!(
            with_schema,
            "프롬프트\n\nReturn only JSON matching this schema. Do not wrap it in markdown.\n{\"type\":\"object\"}"
        );

        let without_schema = apply_openai_response_format_instruction(
            "프롬프트".to_owned(),
            Some(&json!({"type":"json_schema"})),
        );
        assert_eq!(
            without_schema,
            "프롬프트\n\nReturn only JSON matching the requested schema. Do not wrap it in markdown."
        );
    }

    #[test]
    fn openai_completion_prompt_value_handles_scalar_and_array_rules() {
        assert_eq!(extract_prompt_value(Some(&json!("a"))), "a");
        assert_eq!(
            extract_prompt_value(Some(&json!(["a", 1, true, {"x": 1}]))),
            "a\n1\ntrue"
        );
        assert_eq!(extract_prompt_value(Some(&json!({"x": 1}))), "{\"x\":1}");
        assert_eq!(extract_prompt_value(Some(&Value::Null)), "");
    }

    #[test]
    fn gemini_generate_prompt_prioritizes_user_and_joins_system() {
        let root = json!({
            "systemInstruction": {"parts": [{"text":"sys1"}, {"text":"sys2"}]},
            "contents": [
                {"role":"model","parts":[{"text":"old"}]},
                {"role":"user","parts":[{"text":"first"}]},
                {"role":"user","parts":[{"text":"last"}, {"inlineData":{}}]}
            ]
        });

        assert_eq!(
            extract_gemini_generate_prompt(&root),
            "[system]\nsys1sys2\n\n[user]\nlast"
        );
    }

    #[test]
    fn gemini_stream_chunks_preserve_split_newline_behavior() {
        assert_eq!(
            gemini_stream_chunks("a\r\nb\n"),
            vec!["a\n".to_owned(), "b\n".to_owned(), String::new()]
        );
        assert_eq!(gemini_stream_chunks(""), vec![String::new()]);
    }

    #[tokio::test]
    async fn request_guard_response_ceilings_retry_after() {
        let response = request_guard_response("/custom/foo", Duration::from_millis(1501));
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get("retry-after").unwrap(), "2");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            value["errorMessage"],
            "F12 request guard active. Retry after 2s."
        );
    }

    #[tokio::test]
    async fn openai_unauthorized_error_shape_uses_ruster_contract() {
        let response = openai_error_response(401, "Invalid local API key");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(value["error"]["message"], "Invalid local API key");
        assert_eq!(value["error"]["type"], "ruster_error");
        assert_eq!(value["error"]["code"], 401);
    }

    #[tokio::test]
    async fn openai_error_shape_matches_ruster() {
        let response = openai_error_response(400, "Invalid JSON: expected value");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(value["error"]["message"], "Invalid JSON: expected value");
        assert_eq!(value["error"]["type"], "ruster_error");
        assert_eq!(value["error"]["code"], 400);
    }

    #[tokio::test]
    async fn json_response_sets_content_length() {
        let response = json_response(StatusCode::OK, &json!({"result":"ok"}));
        assert_eq!(response.status(), StatusCode::OK);
        let length = response
            .headers()
            .get("content-length")
            .unwrap()
            .to_str()
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();

        assert_eq!(length, body.len());
    }

    #[test]
    fn openai_models_owner_matches_ruster() {
        let payload = openai_models_payload();
        let models = payload["data"].as_array().unwrap();

        assert!(!models.is_empty());
        assert!(
            models
                .iter()
                .all(|model| { model["object"] == "model" && model["owned_by"] == "ruster" })
        );
    }

    #[test]
    fn gemini_models_description_matches_ruster() {
        let payload = gemini_models_payload();
        let models = payload["models"].as_array().unwrap();

        assert!(!models.is_empty());
        assert!(models.iter().all(|model| {
            model["description"] == "ruster local Gemini bridge model"
                && model["supportedGenerationMethods"]
                    == json!(["generateContent", "streamGenerateContent"])
        }));
    }

    #[test]
    fn invalid_json_message_keeps_parser_detail() {
        let error = serde_json::from_str::<Value>("{").unwrap_err();
        let message = invalid_json_message(error);

        assert!(message.starts_with("Invalid JSON: "));
        assert!(message.len() > "Invalid JSON: ".len());
    }

    #[tokio::test]
    async fn stream_chat_response_uses_openai_sse_contract() {
        let response = openai_chat_stream_response("hello", "test-model");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/event-stream; charset=utf-8"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();

        assert!(text.contains("\"object\":\"chat.completion.chunk\""));
        assert!(text.contains("\"delta\":{\"content\":\"hello\"}"));
        assert!(text.ends_with("data: [DONE]\n\n"));
    }
}
