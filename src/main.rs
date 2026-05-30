mod app_icon;
mod app_paths;
mod cli;
mod cli_discovery;
mod cli_setup;
mod console_hotkeys;
mod console_window;
mod custom_api;
mod diagnostics;
mod fast_client;
mod gemini_gate;
mod gui;
mod host;
mod http_server;
mod ivlyrics;
mod ivlyrics_gate;
mod ivlyrics_repair;
mod logging;
mod model_catalog;
mod prompt_config;
mod proxy_dedup;
mod request_guard;
mod settings;
mod tray;
mod update_check;
mod usage_metrics;
mod web_backend;
mod webview2_native;
mod windows_startup;

use std::sync::Arc;

use anyhow::Result;
use parking_lot::{Mutex, RwLock};
use tokio::sync::oneshot;

use crate::app_paths::AppPaths;
use crate::fast_client::FastGenerationConfig;
use crate::gui::GuiExitAction;
use crate::host::{TranslationMode, TranslatorHost};
use crate::logging::LogBuffer;
use crate::settings::AppSettings;

type ShutdownSignal = Arc<Mutex<Option<oneshot::Sender<()>>>>;
const DETACHED_TRAY_ARGUMENT: &str = "--detached-tray-child";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let paths = AppPaths::resolve();
    let settings = AppSettings::load(&paths);
    let logs = LogBuffer::new();
    let detached_tray_child = args
        .iter()
        .any(|arg| arg.eq_ignore_ascii_case(DETACHED_TRAY_ARGUMENT));

    if args
        .iter()
        .any(|arg| arg.eq_ignore_ascii_case("--probe-cli-wrapper"))
    {
        return run_cli_probe(settings).await;
    }

    if args.iter().any(|arg| {
        arg.eq_ignore_ascii_case("--smoke-webview")
            || arg.eq_ignore_ascii_case("--webview-smoke")
            || arg.eq_ignore_ascii_case("--smoke-webview2")
    }) {
        let mode = args
            .iter()
            .find_map(|arg| parse_mode_arg(arg))
            .unwrap_or(TranslationMode::WebView);
        let smoke_prompt = args
            .iter()
            .find_map(|arg| {
                arg.strip_prefix("--smoke-prompt=")
                    .or_else(|| arg.strip_prefix("/smoke-prompt="))
            })
            .map(ToOwned::to_owned);
        let recovery = args.iter().any(|arg| {
            arg.eq_ignore_ascii_case("--smoke-recovery")
                || arg.eq_ignore_ascii_case("/smoke-recovery")
        });
        return run_webview_smoke(paths, settings, logs, mode, smoke_prompt, recovery).await;
    }

    if args
        .iter()
        .any(|arg| arg.eq_ignore_ascii_case(windows_startup::STARTUP_ARGUMENT))
    {
        if !settings.start_with_windows {
            let _ = windows_startup::apply(false);
            return Ok(());
        }

        let mut settings = settings;
        settings.run_in_tray = true;
        let mode = args
            .iter()
            .find_map(|arg| parse_mode_arg(arg))
            .unwrap_or_else(|| mode_from_settings(&settings));
        return run_headless(paths, settings, logs, mode).await;
    }

    if args
        .iter()
        .any(|arg| arg.eq_ignore_ascii_case("--headless"))
    {
        let mut settings = settings;
        if detached_tray_child {
            settings.run_in_tray = true;
        }
        let mode = args
            .iter()
            .find_map(|arg| parse_mode_arg(arg))
            .unwrap_or(TranslationMode::GeminiCli);
        return run_headless(paths, settings, logs, mode).await;
    }

    match gui::run(paths.clone(), settings, logs.clone())? {
        GuiExitAction::Exit => Ok(()),
        GuiExitAction::ServerMode(mode) => {
            logs.push(format!("[GUI] 서버 모드 시작 준비 완료: {}", mode.label()));
            tokio::time::sleep(std::time::Duration::from_millis(350)).await;
            let settings = AppSettings::load(&paths);
            if settings.run_in_tray
                && !detached_tray_child
                && relaunch_detached_tray_backend(mode, &logs)
            {
                return Ok(());
            }
            run_headless(paths, settings, logs, mode).await
        }
    }
}

fn parse_mode_arg(arg: &str) -> Option<TranslationMode> {
    let value = arg
        .strip_prefix("--mode=")
        .or_else(|| arg.strip_prefix("/mode="))?
        .trim()
        .to_owned();
    parse_mode_value(&value)
}

fn mode_from_settings(settings: &AppSettings) -> TranslationMode {
    parse_mode_value(&settings.last_translation_mode).unwrap_or(TranslationMode::WebView)
}

fn parse_mode_value(value: &str) -> Option<TranslationMode> {
    match value
        .trim()
        .replace(['-', '_', ' '], "")
        .to_ascii_lowercase()
        .as_str()
    {
        "webview" | "geminiwebview" | "gemini" => Some(TranslationMode::WebView),
        "chatgpt" | "chatgptwebview" => Some(TranslationMode::ChatGptWebView),
        "cli" | "geminicli" => Some(TranslationMode::GeminiCli),
        _ => None,
    }
}

async fn run_cli_probe(settings: AppSettings) -> Result<()> {
    let model = model_catalog::normalize_cli_model(&settings.gemini_cli_model);
    println!("[CLI Probe] Gemini Rust wrapper probe start (model={model})");

    let result = fast_client::probe(
        &model,
        std::time::Duration::from_secs(60),
        FastGenerationConfig::from_settings(&settings),
    )
    .await;
    println!("[CLI Probe] CLI installed: {}", result.cli_installed);
    println!("[CLI Probe] CLI detail: {}", result.cli_detail);
    println!("[CLI Probe] Wrapper ready: {}", result.wrapper_ready);
    println!("[CLI Probe] Source: {}", result.source);
    println!(
        "[CLI Probe] Abuse/policy signal: {}",
        result.abuse_or_policy_signal
    );
    if !result.response_preview.trim().is_empty() {
        println!("[CLI Probe] Response: {}", result.response_preview);
    }
    if !result.error.trim().is_empty() {
        println!("[CLI Probe] Error: {}", result.error);
    }
    if result.wrapper_ready && !result.abuse_or_policy_signal {
        Ok(())
    } else {
        std::process::exit(2);
    }
}

async fn run_webview_smoke(
    paths: AppPaths,
    settings: AppSettings,
    logs: LogBuffer,
    mode: TranslationMode,
    smoke_prompt: Option<String>,
    recovery: bool,
) -> Result<()> {
    if mode == TranslationMode::GeminiCli {
        anyhow::bail!("WebView smoke requires --mode=webview or --mode=chatgpt");
    }

    println!("[WebView Smoke] start (mode={})", mode.label());
    let settings = Arc::new(RwLock::new(settings));
    let host = Arc::new(TranslatorHost::new(
        settings.clone(),
        logs.clone(),
        mode,
        paths.webview_user_data_dir("profiles"),
        paths.ivlyrics_study_limit_guard_path(),
    ));

    host.start().await?;
    println!("[WebView Smoke] launch/readiness: ok");

    if let Some(prompt) = smoke_prompt.filter(|value| !value.trim().is_empty()) {
        println!(
            "[WebView Smoke] prompt send: start (len={})",
            prompt.chars().count()
        );
        let response = host
            .send_raw_prompt(&prompt, std::time::Duration::from_secs(120))
            .await?;
        println!(
            "[WebView Smoke] prompt response: ok (len={}, preview={})",
            response.chars().count(),
            logging::summarize_text(&response, 180)
        );
    } else {
        println!("[WebView Smoke] prompt send: skipped (pass --smoke-prompt=...)");
    }

    let guard_window = host.activate_request_guard("webview-smoke");
    println!(
        "[WebView Smoke] request guard propagation: armed ({:.1}s)",
        guard_window.as_secs_f32()
    );

    if recovery {
        let recovered = host.request_session_recovery().await;
        println!("[WebView Smoke] recovery reload: {recovered}");
        if !recovered {
            std::process::exit(2);
        }
    } else {
        println!("[WebView Smoke] recovery reload: skipped (pass --smoke-recovery)");
    }

    println!("[WebView Smoke] complete");
    Ok(())
}

async fn run_headless(
    paths: AppPaths,
    settings: AppSettings,
    logs: LogBuffer,
    mode: TranslationMode,
) -> Result<()> {
    let settings = Arc::new(RwLock::new(settings));
    let host = Arc::new(TranslatorHost::new(
        settings.clone(),
        logs.clone(),
        mode,
        paths.webview_user_data_dir("profiles"),
        paths.ivlyrics_study_limit_guard_path(),
    ));
    host.start().await?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let shutdown_signal = Arc::new(Mutex::new(Some(shutdown_tx)));
    let run_in_tray = settings.read().run_in_tray;
    let tray_handle = if run_in_tray {
        tray::spawn(
            paths.clone(),
            settings.clone(),
            host.clone(),
            logs.clone(),
            tokio::runtime::Handle::current(),
            shutdown_signal.clone(),
        )
    } else {
        None
    };
    if tray_handle.is_some() {
        logs.push("[Console] 트레이 모드: 콘솔 창을 숨기고 트레이 로그로 전환합니다.");
        console_window::detach_for_tray(&logs);
    } else {
        console_hotkeys::spawn(
            host.clone(),
            logs.clone(),
            tokio::runtime::Handle::current(),
        );
    }
    let _tray = tray_handle;

    let server = http_server::serve(paths, settings, host, logs, shutdown_rx);

    tokio::select! {
        result = server => result?,
        _ = tokio::signal::ctrl_c() => {
            send_shutdown(&shutdown_signal);
        }
    }

    Ok(())
}

fn send_shutdown(signal: &ShutdownSignal) {
    if let Some(tx) = signal.lock().take() {
        let _ = tx.send(());
    }
}

#[cfg(windows)]
fn relaunch_detached_tray_backend(mode: TranslationMode, logs: &LogBuffer) -> bool {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    use windows::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
    };

    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            logs.push(format!(
                "[Tray] 분리 실행 실패: 현재 실행 파일 경로를 확인할 수 없습니다: {error}"
            ));
            return false;
        }
    };
    let flags = DETACHED_PROCESS.0 | CREATE_NO_WINDOW.0 | CREATE_NEW_PROCESS_GROUP.0;
    match Command::new(exe)
        .arg("--headless")
        .arg(DETACHED_TRAY_ARGUMENT)
        .arg(format!("--mode={}", translation_mode_arg_value(mode)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(flags)
        .spawn()
    {
        Ok(child) => {
            logs.push(format!(
                "[Tray] 분리된 트레이 백엔드를 시작했습니다. pid={}",
                child.id()
            ));
            true
        }
        Err(error) => {
            logs.push(format!(
                "[Tray] 분리 실행 실패, 현재 프로세스에서 계속 실행합니다: {error}"
            ));
            false
        }
    }
}

#[cfg(not(windows))]
fn relaunch_detached_tray_backend(_mode: TranslationMode, _logs: &LogBuffer) -> bool {
    false
}

fn translation_mode_arg_value(mode: TranslationMode) -> &'static str {
    match mode {
        TranslationMode::WebView => "webview",
        TranslationMode::ChatGptWebView => "chatgpt",
        TranslationMode::GeminiCli => "cli",
    }
}
