use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use parking_lot::Mutex;
use reqwest::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::cli_discovery;
use crate::gemini_gate;
use crate::logging;
use crate::model_catalog;
use crate::model_catalog::CliProvider;
use crate::settings::AppSettings;

const CODE_ASSIST_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
const CODE_ASSIST_API_VERSION: &str = "v1internal";
const GEMINI_API_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta";
const OAUTH_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const OAUTH_CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";
const OAUTH_CLIENT_SECRET: &str = "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";
const SERVICE_NAME: &str = "gemini-cli-oauth";
const MAIN_ACCOUNT: &str = "main-account";
const HTTP_MAX_ATTEMPTS: usize = 3;
const EMPTY_RESPONSE_MAX_ATTEMPTS: usize = 2;
const EMPTY_RESPONSE_COOLDOWN_THRESHOLD: u32 = 2;
const EMPTY_RESPONSE_COOLDOWN: Duration = Duration::from_secs(5 * 60);

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
static AUTH_GATE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
static CACHED_CREDENTIAL: OnceLock<Mutex<Option<CachedCredential>>> = OnceLock::new();
static SETUP_CACHE: OnceLock<Mutex<HashMap<String, SetupCacheEntry>>> = OnceLock::new();
static FAST_HEALTH: OnceLock<Mutex<FastHealthState>> = OnceLock::new();
static GENERATE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct FastGenerationConfig {
    pub thinking_level: String,
    pub thinking_budget: i32,
}

impl FastGenerationConfig {
    pub fn from_settings(settings: &AppSettings) -> Self {
        Self {
            thinking_level: settings.gemini_fast_thinking_level.clone(),
            thinking_budget: settings.gemini_fast_thinking_budget,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FastGenerationOptions {
    pub model: String,
    pub prompt: String,
    pub timeout: Duration,
    pub config: FastGenerationConfig,
    pub respect_code_assist_cooldown: bool,
    pub timeout_includes_queue: bool,
    pub gate_wait_timeout: Option<Duration>,
    pub max_output_tokens: Option<u32>,
    pub bypass_generate_gate: bool,
    pub http_max_attempts: usize,
    pub empty_response_max_attempts: usize,
}

impl FastGenerationOptions {
    pub fn new(
        model: impl Into<String>,
        prompt: impl Into<String>,
        timeout: Duration,
        config: FastGenerationConfig,
    ) -> Self {
        let model = model.into();
        Self {
            model: model_catalog::normalize_cli_model_for_provider(&model, CliProvider::Gemini),
            prompt: prompt.into(),
            timeout: timeout.max(Duration::from_secs(1)),
            config,
            respect_code_assist_cooldown: true,
            timeout_includes_queue: true,
            gate_wait_timeout: None,
            max_output_tokens: Some(8192),
            bypass_generate_gate: false,
            http_max_attempts: HTTP_MAX_ATTEMPTS,
            empty_response_max_attempts: EMPTY_RESPONSE_MAX_ATTEMPTS,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FastGenerationResult {
    pub success: bool,
    pub text: String,
    pub source: String,
    pub error: String,
}

#[derive(Clone, Debug)]
pub struct FastProbeResult {
    pub cli_installed: bool,
    pub cli_detail: String,
    pub wrapper_ready: bool,
    pub abuse_or_policy_signal: bool,
    pub source: String,
    pub response_preview: String,
    pub error: String,
}

#[derive(Clone, Debug)]
pub struct FastModelAvailabilityResult {
    pub model: model_catalog::ModelOption,
    pub available: bool,
    pub source: String,
    pub preview: String,
    pub error: String,
}

#[derive(Clone, Debug)]
struct SetupCacheEntry {
    project_id: String,
    expires_at_utc: DateTime<Utc>,
}

#[derive(Default)]
struct FastHealthState {
    consecutive_empty_responses: u32,
    code_assist_cooldown_until_utc: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CredentialKind {
    OAuth,
    ApiKey,
}

#[derive(Clone, Debug)]
struct CachedCredential {
    kind: CredentialKind,
    access_token: String,
    refresh_token: String,
    api_key: String,
    expires_at_utc: Option<DateTime<Utc>>,
    source: String,
}

impl CachedCredential {
    fn from_oauth(
        access_token: impl Into<String>,
        refresh_token: impl Into<String>,
        expires_at_utc: Option<DateTime<Utc>>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            kind: CredentialKind::OAuth,
            access_token: access_token.into(),
            refresh_token: refresh_token.into(),
            api_key: String::new(),
            expires_at_utc,
            source: source.into(),
        }
    }

    fn from_api_key(api_key: impl Into<String>) -> Self {
        Self {
            kind: CredentialKind::ApiKey,
            access_token: String::new(),
            refresh_token: String::new(),
            api_key: api_key.into(),
            expires_at_utc: None,
            source: "env-gemini-api-key".to_owned(),
        }
    }

    fn is_usable(&self) -> bool {
        match self.kind {
            CredentialKind::ApiKey => !self.api_key.trim().is_empty(),
            CredentialKind::OAuth => {
                !self.access_token.trim().is_empty()
                    && self
                        .expires_at_utc
                        .map(|expires| expires > Utc::now() + ChronoDuration::minutes(2))
                        .unwrap_or(true)
            }
        }
    }
}

pub async fn try_generate(options: FastGenerationOptions) -> FastGenerationResult {
    let request_id = GENERATE_SEQUENCE.fetch_add(1, Ordering::SeqCst) + 1;
    println!(
        "[GeminiFast] #{request_id} request received (model={}, timeoutMs={}, cooldownCheck={}, maxOutputTokens={}, gate={}, thinking={}, httpAttempts={}, emptyAttempts={}, {})",
        options.model,
        options.timeout.as_millis(),
        options.respect_code_assist_cooldown,
        options
            .max_output_tokens
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unbounded".to_owned()),
        if options.bypass_generate_gate {
            "none"
        } else {
            "default"
        },
        describe_thinking_config(&options.model, &options.config),
        options.http_max_attempts.max(1),
        options.empty_response_max_attempts.max(1),
        logging::summarize_text(&options.prompt, 160)
    );

    let result = if options.timeout_includes_queue {
        match tokio::time::timeout(options.timeout, run_generate_with_optional_gate(&options)).await
        {
            Ok(result) => result,
            Err(_) => Err("fast wrapper timeout".to_owned()),
        }
    } else {
        run_generate_with_split_timeout(&options).await
    };

    match result {
        Ok(result) => result,
        Err(error) => {
            let sanitized = sanitize_error(&error);
            println!(
                "[GeminiFast] #{request_id} failure received ({})",
                logging::summarize_text(&sanitized, 180)
            );
            fail(sanitized)
        }
    }
}

pub async fn probe(
    model: &str,
    timeout: Duration,
    config: FastGenerationConfig,
) -> FastProbeResult {
    cli_discovery::reset_cache();
    let cli_detail = cli_discovery::try_find()
        .map(|installation| installation.display_source())
        .unwrap_or_else(|| {
            "공식 Gemini CLI(@google/gemini-cli)를 찾을 수 없습니다. npm install -g @google/gemini-cli 설치 후 다시 시도해주세요."
                .to_owned()
        });
    let cli_installed = cli_discovery::try_find().is_some();

    if !cli_installed {
        return FastProbeResult {
            cli_installed: false,
            cli_detail: cli_detail.clone(),
            wrapper_ready: false,
            abuse_or_policy_signal: false,
            source: String::new(),
            response_preview: String::new(),
            error: cli_detail,
        };
    }

    let mut options = FastGenerationOptions::new(model, "Reply with exactly OK.", timeout, config);
    options.respect_code_assist_cooldown = false;
    let result = try_generate(options).await;
    let combined = if result.success {
        &result.text
    } else {
        &result.error
    };
    let abuse_or_policy_signal = looks_like_abuse_or_policy_signal(combined);
    let wrapper_ready = result.success && result.text.to_ascii_uppercase().contains("OK");

    FastProbeResult {
        cli_installed: true,
        cli_detail,
        wrapper_ready,
        abuse_or_policy_signal,
        source: result.source,
        response_preview: logging::summarize_text(result.text, 160),
        error: if result.success {
            String::new()
        } else {
            result.error
        },
    }
}

pub async fn probe_models(
    models: &[model_catalog::ModelOption],
    per_model_timeout: Duration,
    config: FastGenerationConfig,
) -> Vec<FastModelAvailabilityResult> {
    let mut results = Vec::with_capacity(models.len());
    for model in models {
        let mut options = FastGenerationOptions::new(
            model.id,
            "Reply with exactly OK.",
            per_model_timeout,
            config.clone(),
        );
        options.respect_code_assist_cooldown = false;
        let result = try_generate(options).await;
        let combined = if result.success {
            &result.text
        } else {
            &result.error
        };
        let available = result.success
            && !looks_like_abuse_or_policy_signal(combined)
            && result.text.to_ascii_uppercase().contains("OK");

        results.push(FastModelAvailabilityResult {
            model: model.clone(),
            available,
            source: result.source,
            preview: logging::summarize_text(result.text, 120),
            error: if result.success {
                String::new()
            } else {
                result.error
            },
        });
    }
    results
}

async fn run_generate_with_optional_gate(
    options: &FastGenerationOptions,
) -> Result<FastGenerationResult, String> {
    let _permit = if options.bypass_generate_gate {
        None
    } else {
        Some(gemini_gate::acquire().await?)
    };
    generate_core(options).await
}

async fn run_generate_with_split_timeout(
    options: &FastGenerationOptions,
) -> Result<FastGenerationResult, String> {
    let _permit = if options.bypass_generate_gate {
        None
    } else if let Some(gate_wait_timeout) = options.gate_wait_timeout {
        Some(gemini_gate::acquire_with_timeout(gate_wait_timeout).await?)
    } else {
        Some(gemini_gate::acquire().await?)
    };

    match tokio::time::timeout(options.timeout, generate_core(options)).await {
        Ok(result) => result,
        Err(_) => Err("fast wrapper timeout".to_owned()),
    }
}

async fn generate_core(options: &FastGenerationOptions) -> Result<FastGenerationResult, String> {
    let credential = get_credential().await?.ok_or_else(|| {
        "no compatible Gemini CLI OAuth credential or GEMINI_API_KEY found".to_owned()
    })?;

    if credential.kind == CredentialKind::ApiKey {
        let text = generate_with_empty_response_retry(
            || {
                generate_with_gemini_api(
                    &credential,
                    &options.model,
                    &options.prompt,
                    options.max_output_tokens,
                    &options.config,
                    options.http_max_attempts,
                )
            },
            "Gemini API",
            options.empty_response_max_attempts,
        )
        .await?;
        return Ok(ok(text, "fast-gemini-api-key"));
    }

    if options.respect_code_assist_cooldown
        && let Some(remaining) = code_assist_cooldown_remaining()
    {
        return Err(format!(
            "Code Assist fast wrapper cooldown active ({:.0}s remaining after repeated empty responses)",
            remaining.as_secs_f64()
        ));
    }

    let setup = get_setup(&credential, &options.model, options.http_max_attempts).await?;
    let generated = generate_with_empty_response_retry(
        || {
            generate_with_code_assist(
                &credential,
                &setup,
                &options.model,
                &options.prompt,
                options.max_output_tokens,
                &options.config,
                options.http_max_attempts,
            )
        },
        "Code Assist",
        options.empty_response_max_attempts,
    )
    .await;

    match generated {
        Ok(text) => {
            record_code_assist_success();
            Ok(ok(text, "fast-codeassist-oauth"))
        }
        Err(error) => {
            if options.respect_code_assist_cooldown && looks_like_empty_response_error(&error) {
                record_code_assist_empty_response(&error);
            }
            Err(error)
        }
    }
}

async fn get_credential() -> Result<Option<CachedCredential>, String> {
    let _guard = AUTH_GATE
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;

    let cache = CACHED_CREDENTIAL.get_or_init(|| Mutex::new(None));
    if let Some(credential) = cache.lock().clone().filter(CachedCredential::is_usable) {
        return Ok(Some(credential));
    }

    if let Some(oauth) = load_oauth_credential().await? {
        let credential = refresh_if_needed(oauth).await?;
        *cache.lock() = Some(credential.clone());
        return Ok(Some(credential));
    }

    if let Ok(api_key) = std::env::var("GEMINI_API_KEY")
        && !api_key.trim().is_empty()
    {
        let credential = CachedCredential::from_api_key(api_key.trim());
        *cache.lock() = Some(credential.clone());
        return Ok(Some(credential));
    }

    Ok(None)
}

async fn load_oauth_credential() -> Result<Option<CachedCredential>, String> {
    if let Ok(access_token) = std::env::var("GOOGLE_CLOUD_ACCESS_TOKEN")
        && !access_token.trim().is_empty()
        && std::env::var("GOOGLE_GENAI_USE_GCA")
            .map(|value| value.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    {
        return Ok(Some(CachedCredential::from_oauth(
            access_token.trim(),
            "",
            Some(Utc::now() + ChronoDuration::minutes(30)),
            "env-google-cloud-access-token",
        )));
    }

    if let Some(credential) = try_load_oauth_credential_file().await {
        return Ok(Some(credential));
    }

    if let Some(credential) = try_load_windows_credential_manager() {
        return Ok(Some(credential));
    }

    if let Some(credential) = try_load_oauth_credential_via_node_keytar().await {
        return Ok(Some(credential));
    }

    Ok(None)
}

async fn try_load_oauth_credential_file() -> Option<CachedCredential> {
    for path in oauth_credential_file_path_candidates() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(credential) = try_parse_oauth_credential_json(
            &text,
            &format!("oauth_creds.json:{}", shorten_home_path(&path)),
        ) {
            return Some(credential);
        }
    }
    None
}

fn oauth_credential_file_path_candidates() -> Vec<PathBuf> {
    let mut seen = Vec::<PathBuf>::new();
    let mut out = Vec::new();
    for home in cli_home_candidates() {
        for path in [
            home.join(".gemini").join("oauth_creds.json"),
            home.join("oauth_creds.json"),
        ] {
            if !seen.iter().any(|existing| existing == &path) {
                seen.push(path.clone());
                out.push(path);
            }
        }
    }
    out
}

fn cli_home_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for raw in [
        std::env::var("GEMINI_CLI_HOME").ok(),
        std::env::var("USERPROFILE").ok(),
        std::env::var("HOME").ok(),
        combine_home_drive_path(),
        dirs::home_dir().map(|path| path.display().to_string()),
    ]
    .into_iter()
    .flatten()
    {
        let path = PathBuf::from(raw.trim().trim_matches('"'));
        if path.is_dir() && !out.iter().any(|existing: &PathBuf| existing == &path) {
            out.push(path);
        }
    }
    out
}

fn combine_home_drive_path() -> Option<String> {
    let drive = std::env::var("HOMEDRIVE").ok()?;
    let path = std::env::var("HOMEPATH").ok()?;
    if drive.trim().is_empty() || path.trim().is_empty() {
        None
    } else {
        Some(format!("{drive}{path}"))
    }
}

fn shorten_home_path(path: &std::path::Path) -> String {
    for env_name in ["USERPROFILE", "HOME"] {
        if let Ok(home) = std::env::var(env_name) {
            let home = PathBuf::from(home);
            if let Ok(stripped) = path.strip_prefix(&home) {
                return format!("~\\{}", stripped.display());
            }
        }
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned()
}

async fn try_load_oauth_credential_via_node_keytar() -> Option<CachedCredential> {
    let package_dir = cli_discovery::try_find()
        .map(|installation| installation.package_dir)
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let node_path = cli_discovery::try_find()
        .map(|installation| installation.node_path)
        .unwrap_or_else(|| PathBuf::from("node"));
    let script = r#"
(async () => {
  const service = 'gemini-cli-oauth';
  const account = 'main-account';

  function emit(value) {
    if (value) {
      process.stdout.write(value);
      return true;
    }
    return false;
  }

  async function tryNativeKeytar() {
    try {
      const mod = await import('@github/keytar');
      const keytar = mod.default || mod;
      if (emit(await keytar.getPassword(service, account))) return true;
      const all = await keytar.findCredentials(service);
      const hit = Array.isArray(all) ? all.find((x) => x && x.account === account && x.password) : null;
      return !!hit && emit(hit.password);
    } catch {
      return false;
    }
  }

  async function tryEncryptedFileFallback() {
    try {
      const fs = await import('node:fs/promises');
      const path = await import('node:path');
      const os = await import('node:os');
      const crypto = await import('node:crypto');
      const home = process.env.GEMINI_CLI_HOME || os.homedir();
      const file = path.join(home, '.gemini', 'gemini-credentials.json');
      const encryptedData = (await fs.readFile(file, 'utf8')).trim();
      const parts = encryptedData.split(':');
      if (parts.length !== 3) return false;
      const salt = `${os.hostname()}-${os.userInfo().username}-gemini-cli`;
      const key = crypto.scryptSync('gemini-cli-oauth', salt, 32);
      const decipher = crypto.createDecipheriv('aes-256-gcm', key, Buffer.from(parts[0], 'hex'));
      decipher.setAuthTag(Buffer.from(parts[1], 'hex'));
      let decrypted = decipher.update(parts[2], 'hex', 'utf8');
      decrypted += decipher.final('utf8');
      const data = JSON.parse(decrypted);
      return emit(data?.[service]?.[account]);
    } catch {
      return false;
    }
  }

  if (await tryNativeKeytar()) return;
  await tryEncryptedFileFallback();
})();
"#;

    let child = Command::new(node_path)
        .arg("-e")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("NO_COLOR", "1")
        .current_dir(package_dir)
        .spawn()
        .ok()?;

    let output = match tokio::time::timeout(Duration::from_secs(5), child.wait_with_output()).await
    {
        Ok(Ok(output)) => output,
        _ => return None,
    };

    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    try_parse_oauth_credential_json(&stdout, "node-keytar")
}

fn try_parse_oauth_credential_json(json_text: &str, source: &str) -> Option<CachedCredential> {
    if json_text.trim().is_empty() {
        return None;
    }

    let value: Value = serde_json::from_str(json_text).ok()?;
    if let Some(token) = value.get("token").and_then(Value::as_object) {
        let access_token = token
            .get("accessToken")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let refresh_token = token
            .get("refreshToken")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let expires_at = token
            .get("expiresAt")
            .and_then(as_i64)
            .and_then(from_unix_milliseconds);
        return Some(CachedCredential::from_oauth(
            access_token,
            refresh_token,
            expires_at,
            source,
        ));
    }

    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let refresh_token = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let expires_at = value
        .get("expiry_date")
        .and_then(as_i64)
        .and_then(from_unix_milliseconds);
    Some(CachedCredential::from_oauth(
        access_token,
        refresh_token,
        expires_at,
        source,
    ))
}

async fn refresh_if_needed(credential: CachedCredential) -> Result<CachedCredential, String> {
    if credential.is_usable() {
        return Ok(credential);
    }

    if credential.refresh_token.trim().is_empty() {
        return Err("OAuth access token expired and no refresh token is available".to_owned());
    }

    let response = http_client()
        .post(OAUTH_TOKEN_ENDPOINT)
        .form(&[
            ("client_id", OAUTH_CLIENT_ID),
            ("client_secret", OAUTH_CLIENT_SECRET),
            ("refresh_token", credential.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .map_err(|error| format!("OAuth refresh failed: {error}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("OAuth refresh failed ({})", status.as_u16()));
    }

    let value: Value = serde_json::from_str(&body)
        .map_err(|error| format!("OAuth refresh JSON parse failed: {error}"))?;
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let expires_in = value.get("expires_in").and_then(as_i64).unwrap_or(3600);
    if access_token.trim().is_empty() {
        return Err("OAuth refresh did not return an access token".to_owned());
    }

    Ok(CachedCredential::from_oauth(
        access_token,
        credential.refresh_token,
        Some(Utc::now() + ChronoDuration::seconds(expires_in.max(1))),
        format!("{}+refresh", credential.source),
    ))
}

async fn get_setup(
    credential: &CachedCredential,
    model: &str,
    http_max_attempts: usize,
) -> Result<SetupCacheEntry, String> {
    let cache_key = format!(
        "{}:{}",
        credential.source,
        hash_for_cache_key(&credential.access_token)
    );
    if let Some(entry) = SETUP_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .get(&cache_key)
        .cloned()
        .filter(|entry| entry.expires_at_utc > Utc::now())
    {
        return Ok(entry);
    }

    let project_id = get_cloud_project_id();
    let project_value = if project_id.trim().is_empty() {
        Value::Null
    } else {
        Value::String(project_id.clone())
    };
    let payload = json!({
        "cloudaicompanionProject": project_value,
        "metadata": {
            "ideType": "IDE_UNSPECIFIED",
            "platform": "PLATFORM_UNSPECIFIED",
            "pluginType": "GEMINI",
            "duetProject": if project_id.trim().is_empty() { Value::Null } else { Value::String(project_id.clone()) }
        }
    });

    let root = post_code_assist(
        credential,
        "loadCodeAssist",
        &payload,
        model,
        http_max_attempts,
    )
    .await?;
    let resolved_project = get_string(&root, "cloudaicompanionProject")
        .or_else(|| (!project_id.trim().is_empty()).then_some(project_id.clone()))
        .ok_or_else(|| {
            "Code Assist project is not available; CLI onboarding may be required".to_owned()
        })?;

    let has_tier = root
        .get("currentTier")
        .map(|value| !value.is_null())
        .unwrap_or(false);
    if !has_tier {
        return Err(
            "Code Assist user tier is not ready; CLI onboarding may be required".to_owned(),
        );
    }

    let entry = SetupCacheEntry {
        project_id: resolved_project,
        expires_at_utc: Utc::now() + ChronoDuration::minutes(5),
    };
    SETUP_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .insert(cache_key, entry.clone());
    Ok(entry)
}

async fn generate_with_code_assist(
    credential: &CachedCredential,
    setup: &SetupCacheEntry,
    model: &str,
    prompt: &str,
    max_output_tokens: Option<u32>,
    config: &FastGenerationConfig,
    http_max_attempts: usize,
) -> Result<String, String> {
    let prompt_id = uuid::Uuid::new_v4().simple().to_string();
    let payload = json!({
        "model": model,
        "project": setup.project_id,
        "user_prompt_id": prompt_id,
        "request": {
            "contents": [{
                "role": "user",
                "parts": [{ "text": prompt }]
            }],
            "generationConfig": build_generation_config(model, max_output_tokens, config),
            "session_id": prompt_id
        }
    });

    let root = post_code_assist(
        credential,
        "generateContent",
        &payload,
        model,
        http_max_attempts,
    )
    .await?;
    let text = extract_code_assist_text(&root);
    if text.trim().is_empty() {
        return Err(format!(
            "Code Assist returned an empty response: {}",
            describe_generation_response(&root)
        ));
    }
    Ok(text)
}

async fn generate_with_gemini_api(
    credential: &CachedCredential,
    model: &str,
    prompt: &str,
    max_output_tokens: Option<u32>,
    config: &FastGenerationConfig,
    http_max_attempts: usize,
) -> Result<String, String> {
    let url = format!(
        "{}/models/{}:generateContent?key={}",
        GEMINI_API_ENDPOINT,
        urlencoding::encode(model),
        urlencoding::encode(&credential.api_key)
    );
    let payload = json!({
        "contents": [{
            "role": "user",
            "parts": [{ "text": prompt }]
        }],
        "generationConfig": build_generation_config(model, max_output_tokens, config)
    });

    let root = post_json_with_retry(&url, &payload, None, model, http_max_attempts).await?;
    let text = extract_standard_gemini_text(&root);
    if text.trim().is_empty() {
        return Err(format!(
            "Gemini API returned an empty response: {}",
            describe_generation_response(&root)
        ));
    }
    Ok(text)
}

async fn generate_with_empty_response_retry<F, Fut>(
    mut generate: F,
    label: &str,
    max_attempts: usize,
) -> Result<String, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    let mut last_error = None;
    let max_attempts = max_attempts.clamp(1, 5);
    for attempt in 1..=max_attempts {
        match generate().await {
            Ok(text) => return Ok(text),
            Err(error) if looks_like_empty_response_error(&error) => {
                last_error = Some(error);
                if attempt >= max_attempts {
                    break;
                }
                tokio::time::sleep(compute_retry_delay(attempt)).await;
            }
            Err(error) => return Err(error),
        }
    }

    Err(last_error.unwrap_or_else(|| format!("{label} returned an empty response")))
}

fn build_generation_config(
    model: &str,
    max_output_tokens: Option<u32>,
    config: &FastGenerationConfig,
) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("temperature".to_owned(), json!(0.2));
    map.insert("topP".to_owned(), json!(1));
    if let Some(max_output_tokens) = max_output_tokens.filter(|value| *value > 0) {
        map.insert("maxOutputTokens".to_owned(), json!(max_output_tokens));
    }
    if model_catalog::is_gemini_3_or_newer(model) {
        map.insert("topK".to_owned(), json!(64));
    }
    if let Some(thinking_config) = build_thinking_config(model, config) {
        map.insert("thinkingConfig".to_owned(), thinking_config);
    }
    Value::Object(map)
}

fn build_thinking_config(model: &str, config: &FastGenerationConfig) -> Option<Value> {
    if model_catalog::supports_gemini3_thinking_level(model) {
        return Some(json!({
            "thinkingLevel": fast_thinking_level_for_model(model, &config.thinking_level)
        }));
    }

    if model_catalog::supports_gemini25_thinking_budget(model) {
        return Some(json!({
            "thinkingBudget": model_catalog::thinking_budget_for_model(
                model,
                &config.thinking_level,
                config.thinking_budget
            )
        }));
    }

    None
}

fn describe_thinking_config(model: &str, config: &FastGenerationConfig) -> String {
    let Some(thinking_config) = build_thinking_config(model, config) else {
        return "none".to_owned();
    };
    if let Some(level) = thinking_config
        .get("thinkingLevel")
        .and_then(Value::as_str)
        .filter(|level| !level.trim().is_empty())
    {
        return format!("level={level}");
    }
    if let Some(budget) = thinking_config
        .get("thinkingBudget")
        .and_then(Value::as_i64)
    {
        return format!("budget={budget}");
    }
    "unknown".to_owned()
}

fn fast_thinking_level_for_model(model: &str, requested_level: &str) -> String {
    let mut normalized = model_catalog::normalize_thinking_level(requested_level);
    if normalized == "OFF" {
        normalized = if model_catalog::supports_gemini3_minimal_thinking(model) {
            "MINIMAL".to_owned()
        } else {
            "LOW".to_owned()
        };
    }

    if normalized == "MINIMAL" && !model_catalog::supports_gemini3_minimal_thinking(model) {
        return "low".to_owned();
    }

    normalized.to_ascii_lowercase()
}

async fn post_code_assist(
    credential: &CachedCredential,
    method: &str,
    payload: &Value,
    model: &str,
    http_max_attempts: usize,
) -> Result<Value, String> {
    let endpoint =
        std::env::var("CODE_ASSIST_ENDPOINT").unwrap_or_else(|_| CODE_ASSIST_ENDPOINT.to_owned());
    let version = std::env::var("CODE_ASSIST_API_VERSION")
        .unwrap_or_else(|_| CODE_ASSIST_API_VERSION.to_owned());
    let base_url = normalize_code_assist_base_url(&endpoint, &version);
    let url = format!("{base_url}:{method}");
    post_json_with_retry(
        &url,
        payload,
        Some(&credential.access_token),
        model,
        http_max_attempts,
    )
    .await
}

fn normalize_code_assist_base_url(endpoint: &str, version: &str) -> String {
    let trimmed_endpoint = endpoint.trim().trim_end_matches('/');
    let trimmed_version = version.trim().trim_matches('/');
    let last_segment = trimmed_endpoint
        .rsplit('/')
        .next()
        .unwrap_or(trimmed_endpoint);
    if last_segment.len() > 1
        && last_segment.starts_with('v')
        && last_segment
            .chars()
            .nth(1)
            .map(|ch| ch.is_ascii_digit())
            .unwrap_or(false)
    {
        trimmed_endpoint.to_owned()
    } else {
        format!(
            "{}/{}",
            trimmed_endpoint,
            if trimmed_version.is_empty() {
                CODE_ASSIST_API_VERSION
            } else {
                trimmed_version
            }
        )
    }
}

async fn post_json_with_retry(
    url: &str,
    payload: &Value,
    bearer_token: Option<&str>,
    user_agent_model: &str,
    max_attempts: usize,
) -> Result<Value, String> {
    let mut last_error = None;
    let max_attempts = max_attempts.clamp(1, 5);
    for attempt in 1..=max_attempts {
        let mut request = http_client()
            .post(url)
            .header(
                reqwest::header::USER_AGENT,
                build_user_agent(user_agent_model),
            )
            .json(payload);
        if let Some(token) = bearer_token.filter(|token| !token.trim().is_empty()) {
            request = request.bearer_auth(token);
        }

        match request.send().await {
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                if status.is_success() {
                    return serde_json::from_str(&body)
                        .map_err(|error| format!("HTTP JSON parse failed: {error}"));
                }

                let error = format!("HTTP {}: {}", status.as_u16(), extract_error_message(&body));
                last_error = Some(error.clone());
                if !should_retry(status) || attempt == max_attempts {
                    break;
                }
            }
            Err(error) => {
                last_error = Some(error.to_string());
                if attempt == max_attempts {
                    break;
                }
            }
        }

        tokio::time::sleep(compute_retry_delay(attempt)).await;
    }

    Err(last_error.unwrap_or_else(|| "HTTP request failed".to_owned()))
}

fn should_retry(status: StatusCode) -> bool {
    status.as_u16() == 429 || status.as_u16() == 499 || status.as_u16() >= 500
}

fn compute_retry_delay(attempt: usize) -> Duration {
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| (duration.subsec_nanos() % 180) as u64)
        .unwrap_or(80)
        + 80;
    Duration::from_millis(650 * attempt as u64 + jitter)
}

fn extract_code_assist_text(root: &Value) -> String {
    if let Some(response) = root.get("response").filter(|value| value.is_object()) {
        return extract_standard_gemini_text(response);
    }
    extract_standard_gemini_text(root)
}

fn extract_standard_gemini_text(root: &Value) -> String {
    let mut out = String::new();
    let Some(candidates) = root.get("candidates").and_then(Value::as_array) else {
        return out;
    };
    for candidate in candidates {
        let Some(parts) = candidate
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                out.push_str(text);
            }
        }
    }
    out.trim().to_owned()
}

fn describe_generation_response(root: &Value) -> String {
    let response_root = root
        .get("response")
        .filter(|value| value.is_object())
        .unwrap_or(root);
    let Some(candidates) = response_root.get("candidates").and_then(Value::as_array) else {
        let keys = response_root
            .as_object()
            .map(|object| object.keys().take(8).cloned().collect::<Vec<_>>().join(","))
            .unwrap_or_default();
        return format!("candidates=missing; keys={keys}");
    };
    if candidates.is_empty() {
        return "candidates=0".to_owned();
    }

    let mut parts = Vec::new();
    for (index, candidate) in candidates.iter().take(2).enumerate() {
        let finish = get_string(candidate, "finishReason").unwrap_or_else(|| "-".to_owned());
        let message = get_string(candidate, "finishMessage").unwrap_or_else(|| "-".to_owned());
        let part_count = candidate
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        parts.push(format!(
            "candidate[{index}]: finish={finish}, message={}, parts={part_count}",
            logging::summarize_text(message, 80)
        ));
    }

    format!("candidates={}; {}", candidates.len(), parts.join("; "))
}

fn extract_error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|root| {
            root.get("error")
                .and_then(|error| get_string(error, "message"))
        })
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| logging::summarize_text(body, 300))
}

fn http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(120))
            .pool_max_idle_per_host(64)
            .tcp_nodelay(true)
            .build()
            .expect("reqwest client")
    })
}

fn build_user_agent(model: &str) -> String {
    let arch = std::env::consts::ARCH;
    let os = match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        _ => "linux",
    };
    let model_part = if model.trim().is_empty() {
        model_catalog::DEFAULT_GEMINI_CLI_MODEL_ID
    } else {
        model.trim()
    };
    format!("GeminiCLI-ruster/1.0/{model_part} ({os}; {arch}; desktop)")
}

fn get_cloud_project_id() -> String {
    std::env::var("GOOGLE_CLOUD_PROJECT")
        .or_else(|_| std::env::var("GOOGLE_CLOUD_PROJECT_ID"))
        .unwrap_or_default()
}

fn code_assist_cooldown_remaining() -> Option<Duration> {
    let mut health = FAST_HEALTH
        .get_or_init(|| Mutex::new(FastHealthState::default()))
        .lock();
    let cooldown_until = health.code_assist_cooldown_until_utc?;
    let now = Utc::now();
    if cooldown_until > now {
        return (cooldown_until - now).to_std().ok();
    }

    health.code_assist_cooldown_until_utc = None;
    health.consecutive_empty_responses = 0;
    None
}

fn record_code_assist_success() {
    let mut health = FAST_HEALTH
        .get_or_init(|| Mutex::new(FastHealthState::default()))
        .lock();
    health.consecutive_empty_responses = 0;
    health.code_assist_cooldown_until_utc = None;
}

fn record_code_assist_empty_response(detail: &str) {
    let mut health = FAST_HEALTH
        .get_or_init(|| Mutex::new(FastHealthState::default()))
        .lock();
    health.consecutive_empty_responses += 1;
    if health.consecutive_empty_responses >= EMPTY_RESPONSE_COOLDOWN_THRESHOLD {
        health.code_assist_cooldown_until_utc =
            Some(Utc::now() + ChronoDuration::from_std(EMPTY_RESPONSE_COOLDOWN).unwrap());
        println!(
            "[GeminiFast] Code Assist empty responses reached {}; cooldown started ({})",
            health.consecutive_empty_responses,
            logging::summarize_text(detail, 160)
        );
    }
}

pub fn looks_like_abuse_or_policy_signal(text: &str) -> bool {
    let value = text.to_ascii_lowercase();
    [
        "abuse",
        "misuse",
        "suspicious",
        "unusual traffic",
        "policy violation",
        "blocked due to policy",
        "violates",
        "restricted due to",
    ]
    .iter()
    .any(|marker| value.contains(marker))
}

pub fn looks_like_empty_response_error(text: &str) -> bool {
    let value = text.to_ascii_lowercase();
    value.contains("empty response")
        || value.contains("empty response text")
        || value.contains("빈 응답")
}

fn get_string(value: &Value, property: &str) -> Option<String> {
    value
        .get(property)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.parse::<i64>().ok()))
}

fn from_unix_milliseconds(value: i64) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_millis(value)
}

fn ok(text: impl Into<String>, source: impl Into<String>) -> FastGenerationResult {
    FastGenerationResult {
        success: true,
        text: text.into(),
        source: source.into(),
        error: String::new(),
    }
}

fn fail(error: impl Into<String>) -> FastGenerationResult {
    FastGenerationResult {
        success: false,
        text: String::new(),
        source: "fast-wrapper".to_owned(),
        error: error.into(),
    }
}

fn sanitize_error(value: &str) -> String {
    let mut out = value.to_owned();
    if let Ok(api_key) = std::env::var("GEMINI_API_KEY")
        && !api_key.trim().is_empty()
    {
        out = out.replace(api_key.trim(), "[redacted]");
    }
    logging::summarize_text(out, 500)
}

fn hash_for_cache_key(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect()
}

#[cfg(windows)]
fn try_load_windows_credential_manager() -> Option<CachedCredential> {
    for target in [
        SERVICE_NAME.to_owned(),
        format!("{SERVICE_NAME}/{MAIN_ACCOUNT}"),
        format!("{SERVICE_NAME}:{MAIN_ACCOUNT}"),
        MAIN_ACCOUNT.to_owned(),
    ] {
        if let Some(json) = windows_credentials::read_generic_credential(&target, MAIN_ACCOUNT)
            && let Some(credential) =
                try_parse_oauth_credential_json(&json, "windows-credential-manager")
        {
            return Some(credential);
        }
    }

    for json in windows_credentials::enumerate_gemini_credentials(SERVICE_NAME, MAIN_ACCOUNT) {
        if let Some(credential) =
            try_parse_oauth_credential_json(&json, "windows-credential-manager")
        {
            return Some(credential);
        }
    }

    None
}

#[cfg(not(windows))]
fn try_load_windows_credential_manager() -> Option<CachedCredential> {
    None
}

#[cfg(windows)]
mod windows_credentials {
    use std::ptr::null_mut;

    use windows::Win32::Security::Credentials::{
        CRED_TYPE_GENERIC, CREDENTIALW, CredEnumerateW, CredFree, CredReadW,
    };
    use windows::core::{HSTRING, PCWSTR, PWSTR};

    pub fn read_generic_credential(target_name: &str, expected_user_name: &str) -> Option<String> {
        let mut credential_ptr: *mut CREDENTIALW = null_mut();
        let target = HSTRING::from(target_name);
        let read_result =
            unsafe { CredReadW(&target, CRED_TYPE_GENERIC, None, &mut credential_ptr) };
        if read_result.is_err() || credential_ptr.is_null() {
            return None;
        }

        let result = unsafe { read_credential_blob(credential_ptr, expected_user_name) };
        unsafe { CredFree(credential_ptr.cast()) };
        result
    }

    pub fn enumerate_gemini_credentials(
        service_name: &str,
        expected_user_name: &str,
    ) -> Vec<String> {
        let mut count = 0u32;
        let mut credentials_ptr: *mut *mut CREDENTIALW = null_mut();
        let enumerate_result =
            unsafe { CredEnumerateW(PCWSTR::null(), None, &mut count, &mut credentials_ptr) };
        if enumerate_result.is_err() || credentials_ptr.is_null() {
            return Vec::new();
        }

        let mut out = Vec::new();
        unsafe {
            let credentials = std::slice::from_raw_parts(credentials_ptr, count as usize);
            for &credential_ptr in credentials {
                if credential_ptr.is_null() {
                    continue;
                }
                let credential = &*credential_ptr;
                let target = pwstr_to_string(credential.TargetName);
                let user_name = pwstr_to_string(credential.UserName);
                if !target
                    .to_ascii_lowercase()
                    .contains(&service_name.to_ascii_lowercase())
                    && !user_name.eq_ignore_ascii_case(expected_user_name)
                {
                    continue;
                }
                if let Some(blob) =
                    decode_credential_blob(credential.CredentialBlob, credential.CredentialBlobSize)
                {
                    out.push(blob);
                }
            }
            CredFree(credentials_ptr.cast());
        }

        out
    }

    unsafe fn read_credential_blob(
        credential_ptr: *mut CREDENTIALW,
        expected_user_name: &str,
    ) -> Option<String> {
        if credential_ptr.is_null() {
            return None;
        }
        let credential = unsafe { &*credential_ptr };
        let user_name = pwstr_to_string(credential.UserName);
        if !expected_user_name.trim().is_empty()
            && !user_name.trim().is_empty()
            && !user_name.eq_ignore_ascii_case(expected_user_name)
        {
            return None;
        }

        decode_credential_blob(credential.CredentialBlob, credential.CredentialBlobSize)
    }

    fn decode_credential_blob(blob_ptr: *mut u8, blob_size: u32) -> Option<String> {
        if blob_ptr.is_null() || blob_size == 0 {
            return None;
        }

        let bytes = unsafe { std::slice::from_raw_parts(blob_ptr, blob_size as usize) };
        if let Ok(text) = std::str::from_utf8(bytes) {
            let text = text.trim_end_matches('\0').trim();
            if looks_like_json(text) {
                return Some(text.to_owned());
            }
        }

        if bytes.len() % 2 == 0 {
            let wide: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect();
            if let Ok(text) = String::from_utf16(&wide) {
                let text = text.trim_end_matches('\0').trim();
                if looks_like_json(text) {
                    return Some(text.to_owned());
                }
            }
        }

        None
    }

    fn pwstr_to_string(value: PWSTR) -> String {
        if value.is_null() {
            return String::new();
        }
        unsafe { value.to_string().unwrap_or_default() }
    }

    fn looks_like_json(value: &str) -> bool {
        !value.trim().is_empty() && !value.contains('\0') && value.trim_start().starts_with('{')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_wrapper_policy_markers_match_expected_set() {
        assert!(looks_like_abuse_or_policy_signal("blocked due to policy"));
        assert!(looks_like_abuse_or_policy_signal(
            "unusual traffic detected"
        ));
        assert!(!looks_like_abuse_or_policy_signal("quota exceeded"));
    }

    #[test]
    fn fast_wrapper_empty_response_markers_match_expected_set() {
        assert!(looks_like_empty_response_error(
            "Code Assist returned an empty response"
        ));
        assert!(looks_like_empty_response_error("빈 응답"));
        assert!(!looks_like_empty_response_error("model not found"));
    }

    #[test]
    fn code_assist_base_url_keeps_existing_version_segment() {
        assert_eq!(
            normalize_code_assist_base_url("https://example.test/v1internal", "v1beta"),
            "https://example.test/v1internal"
        );
        assert_eq!(
            normalize_code_assist_base_url("https://example.test", "v1internal"),
            "https://example.test/v1internal"
        );
    }

    #[test]
    fn generation_config_includes_thinking_for_gemini25() {
        let config = FastGenerationConfig {
            thinking_level: "LOW".to_owned(),
            thinking_budget: 2048,
        };
        let value = build_generation_config("gemini-2.5-flash", Some(8192), &config);
        assert_eq!(value["maxOutputTokens"], json!(8192));
        assert!(value["thinkingConfig"]["thinkingBudget"].is_number());
    }

    #[test]
    fn generation_config_resolves_model_specific_thinking() {
        let minimal = FastGenerationConfig {
            thinking_level: "MINIMAL".to_owned(),
            thinking_budget: 2048,
        };
        let low = FastGenerationConfig {
            thinking_level: "LOW".to_owned(),
            thinking_budget: 2048,
        };

        let gemini3_flash = build_generation_config("gemini-3-flash-preview", Some(8192), &minimal);
        assert_eq!(
            gemini3_flash["thinkingConfig"]["thinkingLevel"],
            json!("minimal")
        );

        let gemini3_pro = build_generation_config("gemini-3.1-pro-preview", Some(8192), &minimal);
        assert_eq!(gemini3_pro["thinkingConfig"]["thinkingLevel"], json!("low"));

        let gemini25_flash = build_generation_config("gemini-2.5-flash", Some(8192), &minimal);
        assert_eq!(gemini25_flash["thinkingConfig"]["thinkingBudget"], json!(0));

        let gemini25_pro = build_generation_config("gemini-2.5-pro", Some(8192), &minimal);
        assert_eq!(gemini25_pro["thinkingConfig"]["thinkingBudget"], json!(128));

        let gemma = build_generation_config("gemma-4-31b-it", Some(8192), &low);
        assert!(gemma.get("thinkingConfig").is_none());
    }

    #[test]
    fn fast_generation_options_default_to_reference_retry_shape() {
        let options = FastGenerationOptions::new(
            "gemini-3.1-pro-preview",
            "hello",
            Duration::from_secs(10),
            FastGenerationConfig {
                thinking_level: "LOW".to_owned(),
                thinking_budget: 2048,
            },
        );

        assert_eq!(options.http_max_attempts, 3);
        assert_eq!(options.empty_response_max_attempts, 2);
        assert_eq!(
            describe_thinking_config(&options.model, &options.config),
            "level=low"
        );
    }
}
