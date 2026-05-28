use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::cli_discovery;
use crate::fast_client::{self, FastGenerationConfig, FastGenerationOptions};
use crate::gemini_gate;
use crate::logging;
use crate::model_catalog;
use crate::settings::AppSettings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliErrorType {
    NotInstalled,
    AuthExpired,
    RateLimited,
    NetworkError,
    Timeout,
    ProcessCrash,
    UpdateTransient,
    EmptyResponse,
    ModelUnavailable,
    Unknown,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct GeminiCliError {
    pub error_type: CliErrorType,
    pub message: String,
    pub suggested_http_status: u16,
    pub retryable: bool,
}

#[derive(Clone)]
pub struct GeminiCliClient {
    model: String,
    timeout: Duration,
    working_dir: PathBuf,
    use_fast_wrapper: bool,
    fast_config: FastGenerationConfig,
    max_output_tokens: Option<u32>,
    bypass_request_gate: bool,
    respect_fast_wrapper_cooldown: bool,
    retry_attempts: u8,
    fast_wrapper_native_fallback: bool,
    fast_wrapper_http_max_attempts: usize,
    fast_wrapper_empty_response_max_attempts: usize,
}

#[derive(Clone, Debug)]
pub struct GeminiModelAvailabilityResult {
    pub model: model_catalog::ModelOption,
    pub available: bool,
    pub source: String,
    pub error: String,
}

struct ProcessResult {
    status_code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl GeminiCliClient {
    pub fn new(model: String, timeout_seconds: u64) -> Self {
        Self {
            model: model_catalog::normalize_cli_model(&model),
            timeout: Duration::from_secs(timeout_seconds.clamp(5, 600)),
            working_dir: std::env::temp_dir().join("ruster").join("gemini-cli"),
            use_fast_wrapper: false,
            fast_config: FastGenerationConfig {
                thinking_level: "LOW".to_owned(),
                thinking_budget: 2048,
            },
            max_output_tokens: Some(8192),
            bypass_request_gate: false,
            respect_fast_wrapper_cooldown: true,
            retry_attempts: 2,
            fast_wrapper_native_fallback: true,
            fast_wrapper_http_max_attempts: 3,
            fast_wrapper_empty_response_max_attempts: 2,
        }
    }

    pub fn with_working_dir(mut self, working_dir: impl Into<PathBuf>) -> Self {
        self.working_dir = working_dir.into();
        self
    }

    pub fn with_fast_wrapper_from_settings(mut self, settings: &AppSettings) -> Self {
        self.use_fast_wrapper = settings.gemini_cli_use_fast_wrapper;
        self.fast_config = FastGenerationConfig::from_settings(settings);
        self
    }

    pub fn with_max_output_tokens(mut self, max_output_tokens: Option<u32>) -> Self {
        self.max_output_tokens = max_output_tokens;
        self
    }

    pub fn with_bypass_request_gate(mut self, bypass_request_gate: bool) -> Self {
        self.bypass_request_gate = bypass_request_gate;
        self
    }

    pub fn with_respect_fast_wrapper_cooldown(
        mut self,
        respect_fast_wrapper_cooldown: bool,
    ) -> Self {
        self.respect_fast_wrapper_cooldown = respect_fast_wrapper_cooldown;
        self
    }

    pub fn with_retry_attempts(mut self, retry_attempts: u8) -> Self {
        self.retry_attempts = retry_attempts.clamp(1, 3);
        self
    }

    pub fn with_fast_wrapper_native_fallback(mut self, enabled: bool) -> Self {
        self.fast_wrapper_native_fallback = enabled;
        self
    }

    pub fn with_fast_wrapper_http_max_attempts(mut self, max_attempts: usize) -> Self {
        self.fast_wrapper_http_max_attempts = max_attempts.clamp(1, 5);
        self
    }

    pub fn with_fast_wrapper_empty_response_max_attempts(mut self, max_attempts: usize) -> Self {
        self.fast_wrapper_empty_response_max_attempts = max_attempts.clamp(1, 5);
        self
    }

    pub async fn validate_readiness(&self) -> Result<String, GeminiCliError> {
        let version = self
            .run_process(&["--version"], Duration::from_secs(5), None)
            .await?;
        let auth_text = self.send_prompt("Reply with exactly OK.").await?;

        if !auth_text.to_ascii_uppercase().contains("OK") {
            return Err(runtime_error(
                CliErrorType::AuthExpired,
                format!(
                    "Gemini CLI 인증 확인 실패: {}",
                    logging::summarize_text(&auth_text, 160)
                ),
            ));
        }

        Ok(format!(
            "Gemini CLI 준비 완료 (v{}, model={})",
            version.stdout.trim(),
            self.model
        ))
    }

    pub async fn send_prompt(&self, prompt: &str) -> Result<String, GeminiCliError> {
        let _slot = if self.bypass_request_gate {
            None
        } else {
            Some(gemini_gate::acquire().await.map_err(|error| {
                runtime_error(
                    CliErrorType::Timeout,
                    format!("Gemini CLI gate 실패: {error}"),
                )
            })?)
        };

        let mut last_error = None;
        let retry_attempts = self.retry_attempts.max(1);
        for attempt in 1..=retry_attempts {
            let result = self.run_preferred_prompt(prompt, attempt).await;

            match result {
                Ok(output) => {
                    let cleaned = clean_output(&output);
                    if cleaned.trim().is_empty() {
                        last_error = Some(runtime_error(
                            CliErrorType::EmptyResponse,
                            "Gemini CLI가 빈 응답을 반환했습니다.".to_owned(),
                        ));
                        if attempt < retry_attempts {
                            tokio::time::sleep(Duration::from_millis(250)).await;
                            continue;
                        }
                        break;
                    }
                    return Ok(cleaned);
                }
                Err(error) if error.retryable && attempt < retry_attempts => {
                    last_error = Some(error);
                    tokio::time::sleep(Duration::from_millis(700)).await;
                }
                Err(error) => return Err(error),
            }
        }

        Err(last_error.unwrap_or_else(|| {
            runtime_error(CliErrorType::Unknown, "Gemini CLI 처리 실패".to_owned())
        }))
    }

    async fn run_preferred_prompt(
        &self,
        prompt: &str,
        attempt: u8,
    ) -> Result<String, GeminiCliError> {
        if self.use_fast_wrapper {
            println!(
                "[GeminiCli] fast wrapper send (attempt={attempt}, model={}, thinkingLevel={}, thinkingBudget={}, nativeFallback={}, httpAttempts={}, emptyAttempts={})",
                self.model,
                self.fast_config.thinking_level,
                self.fast_config.thinking_budget,
                self.fast_wrapper_native_fallback,
                self.fast_wrapper_http_max_attempts,
                self.fast_wrapper_empty_response_max_attempts
            );
            let mut options = FastGenerationOptions::new(
                self.model.clone(),
                prompt.to_owned(),
                self.timeout,
                self.fast_config.clone(),
            );
            options.max_output_tokens = self.max_output_tokens;
            options.respect_code_assist_cooldown = self.respect_fast_wrapper_cooldown;
            options.bypass_generate_gate = true;
            options.http_max_attempts = self.fast_wrapper_http_max_attempts;
            options.empty_response_max_attempts = self.fast_wrapper_empty_response_max_attempts;
            let fast_result = fast_client::try_generate(options).await;
            if fast_result.success && !fast_result.text.trim().is_empty() {
                println!(
                    "[GeminiCli] fast wrapper received (attempt={attempt}, source={}, {})",
                    fast_result.source,
                    logging::summarize_text(&fast_result.text, 160)
                );
                return Ok(fast_result.text);
            }

            println!(
                "[GeminiCli] fast wrapper failed (attempt={attempt}, error={})",
                logging::summarize_text(&fast_result.error, 180)
            );
            let error_type = classify_error(&fast_result.error, None);
            if error_type == CliErrorType::ModelUnavailable {
                return Err(runtime_error(
                    CliErrorType::ModelUnavailable,
                    format!("Gemini fast wrapper 모델 사용 불가: {}", fast_result.error),
                ));
            }
            if !self.fast_wrapper_native_fallback {
                return Err(runtime_error(
                    error_type,
                    format!("Gemini fast wrapper 실패: {}", fast_result.error),
                ));
            }
            println!(
                "[GeminiCli] fast wrapper failed - native CLI fallback ({})",
                logging::summarize_text(&fast_result.error, 180)
            );
        } else {
            println!(
                "[GeminiCli] fast wrapper skipped - native CLI direct (attempt={attempt}, model={})",
                self.model
            );
        }

        let output = self
            .run_process(
                &[
                    "--model",
                    &self.model,
                    "--prompt",
                    prompt,
                    "--output-format",
                    "text",
                ],
                self.timeout,
                Some(attempt),
            )
            .await?;
        Ok(output.stdout)
    }

    pub async fn probe_native_models(
        models: &[model_catalog::ModelOption],
        per_model_timeout: Duration,
    ) -> Vec<GeminiModelAvailabilityResult> {
        let source = cli_discovery::try_find()
            .map(|installation| installation.display_source())
            .unwrap_or_default();
        let mut out = Vec::with_capacity(models.len());
        for model in models {
            let client = GeminiCliClient::new(model.id.to_owned(), per_model_timeout.as_secs());
            match client
                .run_process(
                    &[
                        "--model",
                        model.id,
                        "--prompt",
                        "Reply with exactly OK.",
                        "--output-format",
                        "text",
                    ],
                    per_model_timeout,
                    None,
                )
                .await
            {
                Ok(result) if result.stdout.to_ascii_uppercase().contains("OK") => {
                    out.push(GeminiModelAvailabilityResult {
                        model: model.clone(),
                        available: true,
                        source: source.clone(),
                        error: String::new(),
                    });
                }
                Ok(result) => out.push(GeminiModelAvailabilityResult {
                    model: model.clone(),
                    available: false,
                    source: source.clone(),
                    error: format!(
                        "검증 응답에 OK가 없습니다: {}",
                        logging::summarize_text(merge_output(&result.stdout, &result.stderr), 160)
                    ),
                }),
                Err(error) => out.push(GeminiModelAvailabilityResult {
                    model: model.clone(),
                    available: false,
                    source: source.clone(),
                    error: describe_error(&error),
                }),
            }
        }
        out
    }

    async fn run_process(
        &self,
        args: &[&str],
        timeout: Duration,
        attempt: Option<u8>,
    ) -> Result<ProcessResult, GeminiCliError> {
        let installation = cli_discovery::find()
            .map_err(|message| runtime_error(CliErrorType::NotInstalled, message))?;

        let _ = std::fs::create_dir_all(&self.working_dir);
        let mut command = Command::new(&installation.file_name);
        command.args(&installation.prefix_args);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("NO_COLOR", "1")
            .current_dir(&self.working_dir);

        let mut child = command.spawn().map_err(|error| {
            runtime_error(
                CliErrorType::ProcessCrash,
                format!("Gemini CLI 프로세스 시작 실패: {error}"),
            )
        })?;

        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");
        let stdout_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf).await;
            String::from_utf8_lossy(&buf).to_string()
        });
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf).await;
            String::from_utf8_lossy(&buf).to_string()
        });

        let status = tokio::select! {
            status = child.wait() => status.map_err(|error| {
                runtime_error(CliErrorType::ProcessCrash, format!("Gemini CLI 대기 실패: {error}"))
            })?,
            _ = tokio::time::sleep(timeout) => {
                let _ = child.kill().await;
                return Err(runtime_error(
                    CliErrorType::Timeout,
                    format!("Gemini CLI {}초 타임아웃", timeout.as_secs()),
                ));
            }
        };

        let stdout = stdout_task.await.unwrap_or_default();
        let stderr = stderr_task.await.unwrap_or_default();
        let result = ProcessResult {
            status_code: status.code(),
            stdout,
            stderr,
        };

        if !status.success() {
            let detail = merge_output(&result.stdout, &result.stderr);
            let error_type = classify_error(&detail, result.status_code);
            return Err(runtime_error(
                error_type,
                format!(
                    "Gemini CLI 실패{}: {}",
                    attempt
                        .map(|a| format!(" (attempt={a})"))
                        .unwrap_or_default(),
                    first_meaningful_line(&detail)
                ),
            ));
        }

        Ok(result)
    }
}

fn runtime_error(error_type: CliErrorType, message: String) -> GeminiCliError {
    GeminiCliError {
        error_type,
        suggested_http_status: map_error_type_to_status(error_type),
        retryable: is_retryable(error_type),
        message,
    }
}

fn map_error_type_to_status(error_type: CliErrorType) -> u16 {
    match error_type {
        CliErrorType::AuthExpired => 401,
        CliErrorType::RateLimited => 429,
        CliErrorType::Timeout => 504,
        CliErrorType::NetworkError
        | CliErrorType::NotInstalled
        | CliErrorType::ProcessCrash
        | CliErrorType::UpdateTransient => 503,
        CliErrorType::EmptyResponse => 502,
        CliErrorType::ModelUnavailable => 400,
        CliErrorType::Unknown => 500,
    }
}

fn is_retryable(error_type: CliErrorType) -> bool {
    matches!(
        error_type,
        CliErrorType::Timeout
            | CliErrorType::RateLimited
            | CliErrorType::NetworkError
            | CliErrorType::ProcessCrash
            | CliErrorType::UpdateTransient
            | CliErrorType::EmptyResponse
    )
}

fn classify_error(message: &str, code: Option<i32>) -> CliErrorType {
    let msg = message.to_ascii_lowercase();
    if msg.contains("empty response") || msg.contains("빈 응답") {
        CliErrorType::EmptyResponse
    } else if msg.contains("timeout")
        || msg.contains("time out")
        || msg.contains("timed out")
        || msg.contains("타임아웃")
        || msg.contains("시간이 초과")
    {
        CliErrorType::Timeout
    } else if looks_like_rate_limit(&msg) {
        CliErrorType::RateLimited
    } else if msg.contains("requested entity was not found")
        || msg.contains("model not found")
        || msg.contains("not found for model")
        || (msg.contains("404") && msg.contains("model"))
    {
        CliErrorType::ModelUnavailable
    } else if msg.contains("not recognized")
        || msg.contains("not found")
        || msg.contains("파일을 찾을 수 없습니다")
        || msg.contains("command not found")
    {
        CliErrorType::NotInstalled
    } else if msg.contains("auth")
        || msg.contains("api key")
        || msg.contains("api_key")
        || msg.contains("login")
        || msg.contains("authentication")
        || msg.contains("failed to open browser")
        || msg.contains("fatalauthenticationerror")
        || msg.contains("please set an auth method")
    {
        CliErrorType::AuthExpired
    } else if msg.contains("network")
        || msg.contains("connect")
        || msg.contains("dns")
        || msg.contains("connection")
    {
        CliErrorType::NetworkError
    } else if msg.contains("automatic update failed")
        || msg.contains("please try updating manually")
        || msg.contains("failed to relaunch")
        || msg.contains("update")
        || msg.contains("relaunch")
    {
        CliErrorType::UpdateTransient
    } else if code.map(|c| !(0..=2).contains(&c)).unwrap_or(false) {
        CliErrorType::ProcessCrash
    } else {
        CliErrorType::Unknown
    }
}

fn looks_like_rate_limit(msg: &str) -> bool {
    msg.contains("no capacity available")
        || msg.contains("capacity issues")
        || msg.contains("http 429")
        || msg.contains("status 429")
        || msg.contains("rate limit")
        || msg.contains("resource exhausted")
        || msg.contains("resource_exhausted")
        || msg.contains("quota")
        || msg.contains("limit hit")
        || msg.contains("limit exceeded")
        || msg.contains("too many requests")
        || msg.contains("요청 제한")
        || msg.contains("요청 한도")
        || msg.contains("사용량 한도")
        || msg.contains("쿼타")
}

fn clean_output(raw: &str) -> String {
    let mut cleaned = raw.trim().to_owned();
    let fence_start = regex::Regex::new(r"(?m)^```\w*\s*\n?").unwrap();
    let fence_end = regex::Regex::new(r"(?m)\n?```\s*$").unwrap();
    let ansi = regex::Regex::new(r"\x1B\[[0-9;]*m").unwrap();
    cleaned = fence_start.replace_all(&cleaned, "").to_string();
    cleaned = fence_end.replace_all(&cleaned, "").to_string();
    cleaned = ansi.replace_all(&cleaned, "").to_string();
    cleaned.trim().to_owned()
}

fn merge_output(stdout: &str, stderr: &str) -> String {
    match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
        (false, false) => format!("{}\n{}", stderr.trim(), stdout.trim()),
        (_, false) => stderr.trim().to_owned(),
        (false, _) => stdout.trim().to_owned(),
        _ => "stdout/stderr 없음".to_owned(),
    }
}

fn first_meaningful_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("at "))
        .unwrap_or("원인 미상")
        .to_owned()
}

pub fn describe_error(error: &GeminiCliError) -> String {
    format!(
        "{} (type={:?}, status={}, retryable={})",
        logging::summarize_text(&error.message, 240),
        error.error_type,
        error.suggested_http_status,
        error.retryable
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_rate_limit_classifier_matches_ruster_markers() {
        for message in [
            "rate limit exceeded",
            "RESOURCE_EXHAUSTED",
            "status 429 from upstream",
            "no capacity available",
            "사용량 한도 초과",
            "쿼타 초과",
        ] {
            assert_eq!(
                classify_error(message, Some(1)),
                CliErrorType::RateLimited,
                "{message}"
            );
        }
    }

    #[test]
    fn cli_classifier_keeps_auth_and_update_transient_parity_markers() {
        assert_eq!(
            classify_error("FatalAuthenticationError: failed to open browser", Some(1)),
            CliErrorType::AuthExpired
        );
        assert_eq!(
            classify_error(
                "automatic update failed; please try updating manually",
                Some(1)
            ),
            CliErrorType::UpdateTransient
        );
    }

    #[test]
    fn cli_fast_lane_options_disable_internal_limits() {
        let client = GeminiCliClient::new(model_catalog::DEFAULT_CLI_MODEL_ID.to_owned(), 120)
            .with_bypass_request_gate(true)
            .with_respect_fast_wrapper_cooldown(false)
            .with_retry_attempts(1)
            .with_fast_wrapper_native_fallback(false)
            .with_fast_wrapper_http_max_attempts(1)
            .with_fast_wrapper_empty_response_max_attempts(1);

        assert!(client.bypass_request_gate);
        assert!(!client.respect_fast_wrapper_cooldown);
        assert_eq!(client.retry_attempts, 1);
        assert!(!client.fast_wrapper_native_fallback);
        assert_eq!(client.fast_wrapper_http_max_attempts, 1);
        assert_eq!(client.fast_wrapper_empty_response_max_attempts, 1);
    }
}
