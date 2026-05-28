use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use eframe::egui::{self, Color32, CornerRadius, Margin, Rect, RichText, Stroke};
use parking_lot::{Mutex as ParkingMutex, RwLock};
use tokio::runtime::Handle;
use tokio::sync::oneshot;

use crate::app_icon::rtr_egui_icon_data;
use crate::app_paths::AppPaths;
use crate::cli::{GeminiCliClient, describe_error};
use crate::cli_discovery;
use crate::cli_setup;
use crate::fast_client::{self, FastGenerationConfig};
use crate::host::{TranslationMode, TranslatorHost};
use crate::logging::LogBuffer;
use crate::model_catalog;
use crate::prompt_config::PromptConfig;
use crate::settings::{AppSettings, generate_local_api_key, normalize_theme_mode};
use crate::usage_metrics::{UsageBucket, UsageMetrics, UsageSnapshot, UsageStatsPeriod};
use crate::windows_startup;

static DARK_UI: AtomicBool = AtomicBool::new(false);
const DEVELOPER_NAME: &str = "hohofught";
const DEVELOPER_GITHUB: &str = "https://github.com/hohofught";
const DEVELOPER_TELEGRAM: &str = "@username_6974";
const DEVELOPER_TELEGRAM_URL: &str = "https://t.me/username_6974";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuiExitAction {
    Exit,
    ServerMode(TranslationMode),
}

pub fn run(paths: AppPaths, settings: AppSettings, logs: LogBuffer) -> Result<GuiExitAction> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ruster")
            .with_icon(rtr_egui_icon_data())
            .with_transparent(true)
            .with_inner_size([1080.0, 720.0])
            .with_min_inner_size([920.0, 640.0]),
        ..Default::default()
    };
    let handle = Handle::current();
    let exit_action = Arc::new(ParkingMutex::new(GuiExitAction::Exit));
    let exit_action_for_app = exit_action.clone();
    eframe::run_native(
        "ruster",
        native_options,
        Box::new(move |cc| {
            let native_hwnd = native_window_handle(cc);
            configure_app_style(&cc.egui_ctx, &logs, &settings.theme_mode);
            Ok(Box::new(RusterApp::new(
                paths.clone(),
                settings.clone(),
                logs.clone(),
                handle.clone(),
                exit_action_for_app.clone(),
                native_hwnd,
            )))
        }),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(*exit_action.lock())
}

fn configure_app_style(ctx: &egui::Context, logs: &LogBuffer, theme_mode: &str) {
    ctx.set_style_of(egui::Theme::Light, style_for_theme(false));
    ctx.set_style_of(egui::Theme::Dark, style_for_theme(true));
    apply_theme_preference(ctx, theme_mode);
    DARK_UI.store(ctx.theme() == egui::Theme::Dark, Ordering::Relaxed);
    configure_korean_fonts(ctx, logs);
}

fn style_for_theme(dark: bool) -> egui::Style {
    let mut style = if dark {
        egui::Theme::Dark.default_style()
    } else {
        egui::Theme::Light.default_style()
    };

    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.panel_fill = app_bg_for(dark);
    visuals.window_fill = surface_for(dark);
    visuals.window_stroke = Stroke::new(1.0, border_for(dark));
    visuals.window_corner_radius = CornerRadius::same(8);
    visuals.extreme_bg_color = app_bg_for(dark);
    visuals.text_edit_bg_color = Some(input_bg_for(dark));
    visuals.code_bg_color = surface_raised_for(dark);
    visuals.hyperlink_color = accent_light_for(dark);
    visuals.selection.bg_fill = accent_for(dark);
    visuals.selection.stroke = Stroke::new(1.0, accent_strong_for(dark));
    visuals.faint_bg_color = surface_raised_for(dark);
    visuals.warn_fg_color = warning_for(dark);
    visuals.error_fg_color = danger_for(dark);
    visuals.button_frame = true;
    visuals.widgets.noninteractive.bg_fill = surface_for(dark);
    visuals.widgets.noninteractive.weak_bg_fill = surface_raised_for(dark);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, border_for(dark));
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, text_for(dark));
    visuals.widgets.noninteractive.corner_radius = CornerRadius::same(7);
    visuals.widgets.inactive.bg_fill = surface_raised_for(dark);
    visuals.widgets.inactive.weak_bg_fill = surface_raised_for(dark);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, border_for(dark));
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, text_for(dark));
    visuals.widgets.inactive.corner_radius = CornerRadius::same(7);
    visuals.widgets.hovered.bg_fill = accent_soft_for(dark);
    visuals.widgets.hovered.weak_bg_fill = accent_soft_for(dark);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, accent_for(dark));
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, text_for(dark));
    visuals.widgets.hovered.corner_radius = CornerRadius::same(7);
    visuals.widgets.active.bg_fill = accent_for(dark);
    visuals.widgets.active.weak_bg_fill = accent_soft_for(dark);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, accent_strong_for(dark));
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, text_for(dark));
    visuals.widgets.active.corner_radius = CornerRadius::same(7);
    visuals.widgets.open = visuals.widgets.hovered;
    style.visuals = visuals;

    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.interact_size = egui::vec2(34.0, 32.0);
    style.spacing.slider_width = 180.0;
    style.spacing.combo_width = 220.0;
    style.spacing.text_edit_width = 280.0;
    style.spacing.window_margin = Margin::symmetric(12, 12);
    style
}

fn apply_theme_preference(ctx: &egui::Context, theme_mode: &str) {
    match normalize_theme_mode(theme_mode).as_str() {
        "Dark" => ctx.set_theme(egui::ThemePreference::Dark),
        "Light" => ctx.set_theme(egui::ThemePreference::Light),
        _ => ctx.set_theme(egui::ThemePreference::System),
    }
}

fn configure_korean_fonts(ctx: &egui::Context, logs: &LogBuffer) {
    let candidates = [
        r"C:\Windows\Fonts\malgun.ttf",
        r"C:\Windows\Fonts\malgunbd.ttf",
        r"C:\Windows\Fonts\gulim.ttc",
    ];
    let Some((path, bytes)) = candidates
        .iter()
        .find_map(|path| std::fs::read(path).ok().map(|bytes| (*path, bytes)))
    else {
        logs.push("[GUI] 한글 폰트를 찾지 못했습니다. Windows Fonts 폴더를 확인하세요.");
        return;
    };

    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "ruster-korean".to_owned(),
        egui::FontData::from_owned(bytes).into(),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "ruster-korean".to_owned());
    }
    ctx.set_fonts(fonts);
    logs.push(format!("[GUI] 한글 폰트 적용: {path}"));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AppPage {
    Home,
    Runtime,
    Proxy,
    Stats,
    Prompt,
}

impl AppPage {
    const ALL: [Self; 5] = [
        Self::Home,
        Self::Runtime,
        Self::Proxy,
        Self::Stats,
        Self::Prompt,
    ];

    fn nav_title(self) -> &'static str {
        match self {
            Self::Home => "홈",
            Self::Runtime => "실행 설정",
            Self::Proxy => "프록시",
            Self::Stats => "통계",
            Self::Prompt => "프롬프트",
        }
    }
}

#[derive(Clone, Debug)]
struct CliSetupPanelState {
    phase: String,
    summary: String,
    detail: String,
}

impl Default for CliSetupPanelState {
    fn default() -> Self {
        Self {
            phase: "대기".to_owned(),
            summary: "환경 진단 전".to_owned(),
            detail:
                "Gemini CLI 초기설정을 실행하면 설치, 로그인, 모델 검증 상태가 여기에 표시됩니다."
                    .to_owned(),
        }
    }
}

struct RusterApp {
    paths: AppPaths,
    settings: Arc<RwLock<AppSettings>>,
    draft: AppSettings,
    logs: LogBuffer,
    runtime: Handle,
    host: Arc<TranslatorHost>,
    selected_mode: TranslationMode,
    server_shutdown: Option<oneshot::Sender<()>>,
    page: AppPage,
    scroll_target: Option<AppPage>,
    usage_period: UsageStatsPeriod,
    status_message: String,
    cli_setup_inflight: Arc<AtomicBool>,
    cli_setup_panel: Arc<RwLock<CliSetupPanelState>>,
    last_cli_setup_phase: String,
    cli_model_verified_notice: Option<String>,
    cli_start_after_setup: bool,
    prompt_editor_text: String,
    confirm_usage_reset: bool,
    show_developer_info: bool,
    exit_action: Arc<ParkingMutex<GuiExitAction>>,
    close_requested: bool,
    native_hwnd: Option<isize>,
    last_mica_dark: Option<bool>,
}

impl RusterApp {
    fn new(
        paths: AppPaths,
        settings: AppSettings,
        logs: LogBuffer,
        runtime: Handle,
        exit_action: Arc<ParkingMutex<GuiExitAction>>,
        native_hwnd: Option<isize>,
    ) -> Self {
        let mut settings = settings;
        if windows_startup::is_registered() {
            settings.start_with_windows = true;
            settings.run_in_tray = true;
        }
        let settings_arc = Arc::new(RwLock::new(settings.clone()));
        let selected_mode = TranslationMode::WebView;
        let host = Arc::new(TranslatorHost::new(
            settings_arc.clone(),
            logs.clone(),
            selected_mode,
            paths.webview_user_data_dir("profiles"),
            paths.ivlyrics_study_limit_guard_path(),
        ));
        logs.push(format!(
            "[GUI] ruster 시작. 설정 위치: {}",
            paths.settings_path().display()
        ));
        let prompt_editor_text = PromptConfig::load(&paths, &logs).editable_document();
        Self {
            paths,
            settings: settings_arc,
            draft: settings,
            logs,
            runtime,
            host,
            selected_mode,
            server_shutdown: None,
            page: AppPage::Home,
            scroll_target: None,
            usage_period: UsageStatsPeriod::Daily,
            status_message: "gemini와 chatgpt를 지원합니다.".to_string(),
            cli_setup_inflight: Arc::new(AtomicBool::new(false)),
            cli_setup_panel: Arc::new(RwLock::new(CliSetupPanelState::default())),
            last_cli_setup_phase: String::new(),
            cli_model_verified_notice: None,
            cli_start_after_setup: false,
            prompt_editor_text,
            confirm_usage_reset: false,
            show_developer_info: false,
            exit_action,
            close_requested: false,
            native_hwnd,
            last_mica_dark: None,
        }
    }

    fn save_settings(&mut self) {
        self.enforce_startup_dependencies();
        self.draft = self.draft.clone().normalized();
        {
            let current = self.settings.read();
            if self.draft.gemini_cli_verified_model_ids.is_empty()
                && !current.gemini_cli_verified_model_ids.is_empty()
            {
                self.draft.gemini_cli_verified_model_ids =
                    current.gemini_cli_verified_model_ids.clone();
                self.draft.gemini_cli_verified_at_utc = current.gemini_cli_verified_at_utc;
                self.draft.gemini_cli_verified_source = current.gemini_cli_verified_source.clone();
                self.draft.gemini_cli_verified_wrapper_source =
                    current.gemini_cli_verified_wrapper_source.clone();
                let selected = model_catalog::normalize_cli_model(&self.draft.gemini_cli_model);
                if !current
                    .gemini_cli_verified_model_ids
                    .iter()
                    .any(|model| model.eq_ignore_ascii_case(&selected))
                {
                    self.draft.gemini_cli_model = current.gemini_cli_model.clone();
                    self.draft.iv_lyrics_quiz_cli_model = current.iv_lyrics_quiz_cli_model.clone();
                }
            }
        }
        *self.settings.write() = self.draft.clone();
        match self.draft.save(&self.paths) {
            Ok(()) => self.logs.push("[GUI] 설정 저장 완료"),
            Err(error) => self.logs.push(format!("[GUI] 설정 저장 실패: {error}")),
        }
        if let Err(error) = windows_startup::apply(self.draft.start_with_windows) {
            self.logs
                .push(format!("[GUI] Windows 자동 실행 설정 실패: {error}"));
            self.status_message = format!("Windows 자동 실행 설정 실패: {error}");
        }
    }

    fn enforce_startup_dependencies(&mut self) {
        if self.draft.start_with_windows {
            self.draft.run_in_tray = true;
        }
    }

    fn set_status(&mut self, text: impl Into<String>) {
        self.status_message = text.into();
    }

    fn sync_cli_setup_notice(&mut self) {
        let state = self.cli_setup_panel.read().clone();
        if state.phase == self.last_cli_setup_phase {
            return;
        }

        if state.phase == "초기설정 완료" {
            let detail = if state.detail.trim().is_empty() {
                "사용 가능한 Gemini CLI 모델을 확인했습니다.".to_owned()
            } else {
                state.detail.clone()
            };
            self.cli_model_verified_notice =
                Some(format!("모델 확인이 완료되었습니다.\n\n{detail}"));
            self.set_status("Gemini CLI 모델 확인이 완료되었습니다.");
            self.logs
                .push(format!("[GeminiCli] 모델 확인 완료 알림 표시: {detail}"));
        }

        self.last_cli_setup_phase = state.phase;
    }

    fn set_cli_setup_panel(
        panel: &Arc<RwLock<CliSetupPanelState>>,
        phase: impl Into<String>,
        summary: impl Into<String>,
        detail: impl Into<String>,
    ) {
        let mut state = panel.write();
        state.phase = phase.into();
        state.summary = summary.into();
        state.detail = detail.into();
    }

    fn refresh_cli_setup_environment(&mut self) {
        let panel = self.cli_setup_panel.clone();
        Self::set_cli_setup_panel(&panel, "환경 진단 중", "확인 중...", "");
        self.runtime.spawn(async move {
            let status = tokio::task::spawn_blocking(cli_setup::get_environment_status)
                .await
                .unwrap_or_default();
            Self::set_cli_setup_panel(&panel, "환경 진단 완료", status.summary(), "");
        });
        self.set_status("Gemini CLI 환경 진단을 시작했습니다.");
    }

    fn reload_prompt_editor(&mut self) {
        self.prompt_editor_text = PromptConfig::load(&self.paths, &self.logs).editable_document();
        self.set_status("프롬프트 설정을 다시 읽었습니다.");
    }

    fn load_default_prompt_editor(&mut self) {
        self.prompt_editor_text = PromptConfig::default_editable_document();
        self.set_status("기본 prompts.json을 불러왔습니다. 저장해야 적용됩니다.");
    }

    fn save_prompt_editor(&mut self) {
        match PromptConfig::save_user_override_document(&self.paths, &self.prompt_editor_text) {
            Ok(config) => {
                self.prompt_editor_text = config.editable_document();
                let path = self.paths.prompt_override_path();
                self.logs.push(format!(
                    "[PromptConfig] 사용자 프롬프트 설정 저장: {}",
                    path.display()
                ));
                self.set_status(format!("프롬프트를 저장했습니다: {}", path.display()));
            }
            Err(error) => {
                self.logs
                    .push(format!("[PromptConfig] 프롬프트 저장 실패: {error}"));
                self.set_status(format!("프롬프트 저장 실패: {error}"));
            }
        }
    }

    fn apply_gui_theme(&mut self, ctx: &egui::Context) {
        self.draft.theme_mode = normalize_theme_mode(&self.draft.theme_mode);
        apply_theme_preference(ctx, &self.draft.theme_mode);
        let dark = ctx.theme() == egui::Theme::Dark;
        DARK_UI.store(dark, Ordering::Relaxed);
        if self.last_mica_dark != Some(dark) {
            apply_windows_mica_backdrop(self.native_hwnd, dark, &self.logs);
            self.last_mica_dark = Some(dark);
        }
    }

    fn sync_cli_cache_from_settings(&mut self) {
        let settings = self.settings.read().clone();
        let cache_changed = self.draft.gemini_cli_verified_model_ids
            != settings.gemini_cli_verified_model_ids
            || self.draft.gemini_cli_verified_at_utc != settings.gemini_cli_verified_at_utc
            || self.draft.gemini_cli_verified_source != settings.gemini_cli_verified_source
            || self.draft.gemini_cli_verified_wrapper_source
                != settings.gemini_cli_verified_wrapper_source;
        if !cache_changed {
            return;
        }

        self.draft.gemini_cli_verified_model_ids = settings.gemini_cli_verified_model_ids.clone();
        self.draft.gemini_cli_verified_at_utc = settings.gemini_cli_verified_at_utc;
        self.draft.gemini_cli_verified_source = settings.gemini_cli_verified_source.clone();
        self.draft.gemini_cli_verified_wrapper_source =
            settings.gemini_cli_verified_wrapper_source.clone();
        let selected = model_catalog::normalize_cli_model(&self.draft.gemini_cli_model);
        if !self
            .draft
            .gemini_cli_verified_model_ids
            .iter()
            .any(|model| model.eq_ignore_ascii_case(&selected))
            && !settings.gemini_cli_model.is_empty()
        {
            self.draft.gemini_cli_model = settings.gemini_cli_model;
            self.draft.iv_lyrics_quiz_cli_model = settings.iv_lyrics_quiz_cli_model;
            self.ensure_thinking_for_selected_model();
        }
    }

    fn ensure_thinking_for_selected_model(&mut self) {
        self.draft.gemini_cli_model =
            model_catalog::normalize_cli_model(&self.draft.gemini_cli_model);
        self.draft.gemini_fast_thinking_level = model_catalog::normalize_thinking_level_for_model(
            &self.draft.gemini_cli_model,
            &self.draft.gemini_fast_thinking_level,
        );
        self.draft.gemini_fast_thinking_budget = model_catalog::thinking_budget_for_model(
            &self.draft.gemini_cli_model,
            &self.draft.gemini_fast_thinking_level,
            self.draft.gemini_fast_thinking_budget,
        );
        self.draft.iv_lyrics_quiz_cli_model = self.draft.gemini_cli_model.clone();
    }

    fn finish_cli_auto_start_after_setup(&mut self) {
        if !self.cli_start_after_setup || self.cli_setup_inflight.load(Ordering::SeqCst) {
            return;
        }
        if self.cli_model_verified_notice.is_some() {
            return;
        }

        self.sync_cli_cache_from_settings();
        if self.selected_mode_ready() {
            self.cli_start_after_setup = false;
            self.set_status(format!(
                "Gemini CLI 준비 완료. 시작합니다: {}",
                self.draft.gemini_cli_model
            ));
            self.enter_server_mode();
        } else {
            self.cli_start_after_setup = false;
            self.page = AppPage::Runtime;
            self.set_status("Gemini CLI 초기설정이 완료되지 않았습니다.");
        }
    }

    fn start_mode_from_home(&mut self, mode: TranslationMode) {
        self.selected_mode = mode;
        self.draft.last_translation_mode = translation_mode_setting_value(mode).to_owned();
        if mode == TranslationMode::GeminiCli && !self.selected_mode_ready() {
            self.page = AppPage::Runtime;
            self.cli_start_after_setup = true;
            self.set_status("CLI 검증 캐시가 없어 초기설정을 시작합니다.");
            self.launch_cli_setup_flow();
            return;
        }

        self.enter_server_mode();
    }

    fn stop_server(&mut self) {
        if let Some(tx) = self.server_shutdown.take() {
            let _ = tx.send(());
        }
    }

    fn enter_server_mode(&mut self) {
        if self.close_requested {
            return;
        }
        if !self.selected_mode_has_backend() {
            self.log_missing_webview_backend();
            self.page = AppPage::Runtime;
            self.set_status("선택한 모드를 시작할 수 없습니다.");
            return;
        }
        if !self.selected_mode_ready() {
            self.log_missing_cli_verification_cache();
            self.page = AppPage::Runtime;
            self.set_status("Gemini CLI 모델 검증이 필요합니다.");
            return;
        }
        self.save_settings();
        self.stop_server();
        *self.exit_action.lock() = GuiExitAction::ServerMode(self.selected_mode);
        let backend_label = if self.draft.run_in_tray {
            "트레이 백엔드"
        } else {
            "콘솔 백엔드"
        };
        self.logs.push(format!(
            "[GUI] 서버 모드 전환: {}. GUI를 닫고 {}만 유지합니다.",
            self.selected_mode.label(),
            backend_label
        ));
        self.set_status(format!(
            "{} {} 시작",
            self.selected_mode.label(),
            backend_label
        ));
        self.close_requested = true;
    }

    fn selected_mode_has_backend(&self) -> bool {
        true
    }

    fn selected_mode_ready(&self) -> bool {
        if self.selected_mode != TranslationMode::GeminiCli {
            return true;
        }
        let selected = model_catalog::normalize_cli_model(&self.draft.gemini_cli_model);
        let settings = self.settings.read();
        settings.has_gemini_cli_verification_cache()
            && settings
                .gemini_cli_verified_model_ids
                .iter()
                .any(|model| model.eq_ignore_ascii_case(&selected))
    }

    fn log_missing_webview_backend(&self) {
        self.logs.push(format!(
            "[Parity] {} 모드를 시작할 수 없습니다.",
            self.selected_mode.label()
        ));
    }

    fn log_missing_cli_verification_cache(&self) {
        self.logs.push(
            "[GeminiCli] 검증된 CLI 모델 캐시가 없습니다. CLI 초기설정 후 모델 검증을 먼저 실행하세요.",
        );
    }

    fn launch_cli_setup_flow(&mut self) {
        if self.cli_setup_inflight.swap(true, Ordering::SeqCst) {
            self.logs
                .push("[GeminiCli] CLI 초기설정 확인이 이미 진행 중입니다.");
            self.set_status("Gemini CLI 초기설정이 이미 진행 중입니다.");
            return;
        }

        self.save_settings();
        let logs = self.logs.clone();
        let inflight = self.cli_setup_inflight.clone();
        let panel = self.cli_setup_panel.clone();
        let paths = self.paths.clone();
        let settings = self.settings.clone();
        let settings_snapshot = self.draft.clone().normalized();
        let selected_model = model_catalog::normalize_cli_model(&self.draft.gemini_cli_model);
        let timeout = self.draft.gemini_cli_timeout_seconds.max(120);
        self.logs.push("[GeminiCli] CLI 초기설정 환경 진단 시작");
        self.set_status("Gemini CLI 상태 확인 중...");
        Self::set_cli_setup_panel(&panel, "환경 진단 중", "확인 중...", "");
        self.runtime.spawn(async move {
            let status = tokio::task::spawn_blocking(cli_setup::get_environment_status)
                .await
                .unwrap_or_default();
            let summary = status.summary();
            Self::set_cli_setup_panel(&panel, "환경 진단 완료", summary.clone(), "");
            logs.push(format!("[GeminiCli] CLI 초기설정 환경 진단:\n{}", summary));

            let result = if status.has_gemini() {
                Self::set_cli_setup_panel(&panel, "로그인/온보딩 창 실행", summary.clone(), "");
                cli_setup::launch_login_terminal()
            } else {
                Self::set_cli_setup_panel(&panel, "설치/로그인 창 실행", summary.clone(), "");
                cli_setup::launch_install_terminal()
            };
            match result {
                Ok(()) => {
                    logs.push("[GeminiCli] CLI 초기설정 PowerShell 창을 열었습니다.");
                    Self::set_cli_setup_panel(
                        &panel,
                        "로그인/온보딩 대기 중",
                        summary.clone(),
                        "PowerShell 창에서 설치 또는 로그인을 완료하면 자동 감지를 계속합니다.",
                    );
                }
                Err(error) => {
                    logs.push(format!("[GeminiCli] CLI 초기설정 실행 실패: {error}"));
                    Self::set_cli_setup_panel(
                        &panel,
                        "초기설정 실행 실패",
                        summary,
                        error.to_string(),
                    );
                    inflight.store(false, Ordering::SeqCst);
                    return;
                }
            }

            logs.push("[GeminiCli] 로그인/초기화 완료 자동 감지를 시작합니다. (최대 10분)");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
            let mut last_error = String::new();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(8)).await;
                cli_discovery::reset_cache();
                let client = GeminiCliClient::new(selected_model.clone(), timeout)
                    .with_fast_wrapper_from_settings(&settings_snapshot);
                match client.validate_readiness().await {
                    Ok(message) => {
                        logs.push(format!("[GeminiCli] 자동 감지 성공: {message}"));
                        Self::set_cli_setup_panel(
                            &panel,
                            "모델 검증 중",
                            summary.clone(),
                            message.clone(),
                        );
                        let stored = probe_and_store_cli_models(
                            paths.clone(),
                            settings.clone(),
                            logs.clone(),
                            selected_model.clone(),
                        )
                        .await;
                        if stored {
                            let cached = settings.read().gemini_cli_verified_model_ids.join(", ");
                            Self::set_cli_setup_panel(
                                &panel,
                                "초기설정 완료",
                                summary.clone(),
                                format!("검증된 모델: {cached}"),
                            );
                        } else {
                            Self::set_cli_setup_panel(
                                &panel,
                                "모델 검증 실패",
                                summary.clone(),
                                "사용 가능한 CLI 모델을 확인하지 못했습니다.",
                            );
                        }
                        break;
                    }
                    Err(error) => {
                        let summary = describe_error(&error);
                        if summary != last_error {
                            logs.push(format!(
                                "[GeminiCli] 자동 감지 대기 중: {}",
                                crate::logging::summarize_text(&summary, 220)
                            ));
                            last_error = summary;
                            Self::set_cli_setup_panel(
                                &panel,
                                "로그인/온보딩 대기 중",
                                status.summary(),
                                crate::logging::summarize_text(&last_error, 220),
                            );
                        }
                        if std::time::Instant::now() >= deadline {
                            logs.push("[GeminiCli] CLI 초기설정 자동 감지 시간이 초과되었습니다.");
                            Self::set_cli_setup_panel(
                                &panel,
                                "초기설정 시간 초과",
                                status.summary(),
                                "10분 안에 CLI 준비 상태를 확인하지 못했습니다.",
                            );
                            break;
                        }
                    }
                }
            }
            inflight.store(false, Ordering::SeqCst);
        });
    }

    fn draw_sidebar(&mut self, ui: &mut egui::Ui) {
        ui.set_width(ui.available_width());
        ui.add_space(2.0);
        fixed_badge(
            ui,
            egui::vec2(44.0, 44.0),
            "RTR",
            13.0,
            accent_soft(),
            accent_strong(),
        );
        ui.add_space(12.0);

        for page in AppPage::ALL {
            let selected = self.page == page;
            let button = egui::Button::new(RichText::new(page.nav_title()).size(14.0))
                .selected(selected)
                .frame(true)
                .corner_radius(8)
                .fill(if selected { nav_selected() } else { nav_bg() });
            let response = ui.add_sized([ui.available_width(), 40.0], button);
            if selected {
                let rail = egui::Rect::from_min_max(
                    egui::pos2(response.rect.left(), response.rect.top() + 8.0),
                    egui::pos2(response.rect.left() + 3.0, response.rect.bottom() - 8.0),
                );
                ui.painter().rect_filled(rail, 2.0, accent_strong());
            }
            if response.clicked() {
                self.page = page;
                self.scroll_target = Some(page);
            }
        }

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            compact_card_frame(badge_bg(), border()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(&self.status_message).color(muted_text()));
                ui.add_space(8.0);
                if ui.button("개발자 정보").clicked() {
                    self.show_developer_info = true;
                }
            });
        });
    }

    fn draw_header(&mut self, ui: &mut egui::Ui) {
        self.section_anchor(ui, AppPage::Home);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("번역 모드를 선택하세요")
                        .size(30.0)
                        .strong()
                        .color(text()),
                );
                ui.label(
                    RichText::new(
                        "호환 API, OpenAI, Gemini 프록시 설정은 ruster 실행 경로에서 유지됩니다.",
                    )
                    .color(muted_text()),
                );
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut theme_changed = false;
                let selected_theme = normalize_theme_mode(&self.draft.theme_mode);
                egui::ComboBox::from_id_salt("theme_combo")
                    .width(132.0)
                    .selected_text(selected_theme.clone())
                    .show_ui(ui, |ui| {
                        for theme in ["System", "Light", "Dark"] {
                            theme_changed |= ui
                                .selectable_value(
                                    &mut self.draft.theme_mode,
                                    theme.to_owned(),
                                    theme,
                                )
                                .changed();
                        }
                    });
                ui.label(RichText::new("테마").color(muted_text()));

                if theme_changed {
                    self.draft.theme_mode = normalize_theme_mode(&self.draft.theme_mode);
                    self.apply_gui_theme(ui.ctx());
                    self.save_settings();
                    self.set_status("테마 설정을 저장했습니다.");
                }
            });
        });
        ui.add_space(18.0);
    }

    fn section_anchor(&mut self, ui: &mut egui::Ui, page: AppPage) {
        let response =
            ui.allocate_response(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
        if self.scroll_target == Some(page) {
            response.scroll_to_me(Some(egui::Align::Min));
            self.scroll_target = None;
        }
    }

    fn draw_overview_section(&mut self, ui: &mut egui::Ui) {
        self.draw_start_mode_cards(ui);
    }

    fn draw_start_mode_cards(&mut self, ui: &mut egui::Ui) {
        let mut selected = None;
        ui.columns(3, |columns| {
            if mode_start_card(
                &mut columns[0],
                "Gemini WebView",
                "브라우저 세션 번역",
                "G",
                accent_light(),
            ) {
                selected = Some(TranslationMode::WebView);
            }
            if mode_start_card(
                &mut columns[1],
                "Gemini CLI",
                "2026-06-18 이후 보증 불가",
                "CLI",
                success(),
            ) {
                selected = Some(TranslationMode::GeminiCli);
            }
            if mode_start_card(
                &mut columns[2],
                "ChatGPT WebView",
                "ChatGPT 세션 번역",
                "C",
                Color32::from_rgb(2, 132, 199),
            ) {
                selected = Some(TranslationMode::ChatGptWebView);
            }
        });

        if let Some(mode) = selected {
            self.start_mode_from_home(mode);
        }
    }

    fn draw_runtime_section(&mut self, ui: &mut egui::Ui) {
        self.section_anchor(ui, AppPage::Runtime);
        draw_card(ui, "WebView / CLI", |ui| {
            ui.columns(2, |columns| {
                let ui = &mut columns[0];
                ui.add_space(4.0);
                let run_in_tray_response =
                    toggle_row(ui, &mut self.draft.run_in_tray, "트레이 모드로 실행")
                        .on_hover_text(
                            "서버 모드 실행 중 Windows 알림 영역에서 로그, 통계, WebView 제어, 종료를 처리합니다.",
                        );
                if run_in_tray_response.changed() && self.draft.start_with_windows {
                    self.draft.run_in_tray = true;
                    self.set_status("Windows 자동 실행은 트레이 모드가 필요합니다.");
                }
                let start_response = toggle_row(
                    ui,
                    &mut self.draft.start_with_windows,
                    "Windows 시작 시 자동 실행",
                );
                if start_response.changed() {
                    if self.draft.start_with_windows {
                        self.draft.run_in_tray = true;
                    }
                    self.save_settings();
                    self.set_status(if self.draft.start_with_windows {
                        "Windows 시작 시 자동 실행을 켰습니다."
                    } else {
                        "Windows 시작 시 자동 실행을 껐습니다."
                    });
                }
                toggle_row(ui, &mut self.draft.verbose_logs, "디버그 로그");
                toggle_row(
                    ui,
                    &mut self.draft.web_view_pure_quality_mode,
                    "WebView 최고 품질 모드",
                );
                toggle_row(ui, &mut self.draft.web_view_raw_mode, "WebView Raw 모드");
                ui.add_space(18.0);
                egui::Grid::new("webview_runtime_grid")
                    .num_columns(3)
                    .spacing([14.0, 10.0])
                    .show(ui, |ui| {
                        ui.label("새로고침 주기");
                        ui.add_sized(
                            [90.0, 32.0],
                            egui::DragValue::new(
                                &mut self.draft.web_view_refresh_every_requests,
                            )
                            .range(1..=200)
                            .speed(1),
                        );
                        ui.label(RichText::new("요청마다").color(muted_text()));
                        ui.end_row();

                        ui.label("유휴 새로고침");
                        ui.add_sized(
                            [90.0, 32.0],
                            egui::DragValue::new(&mut self.draft.web_view_idle_refresh_seconds)
                                .range(10..=600)
                                .speed(1),
                        );
                        ui.label(RichText::new("초").color(muted_text()));
                        ui.end_row();
                    });

                let ui = &mut columns[1];
                ui.add_space(4.0);
                let before_model = self.draft.gemini_cli_model.clone();
                ui.label(RichText::new("CLI 모델").color(text()));
                egui::ComboBox::from_id_salt("cli_model_combo")
                    .width(ui.available_width())
                    .selected_text(self.draft.gemini_cli_model.clone())
                    .show_ui(ui, |ui| {
                        for model in model_catalog::CLI_MODELS {
                            ui.selectable_value(
                                &mut self.draft.gemini_cli_model,
                                model.id.to_owned(),
                                format!("{} ({})", model.display_name, model.id),
                            );
                        }
                    });
                if before_model != self.draft.gemini_cli_model {
                    self.ensure_thinking_for_selected_model();
                    self.set_status("CLI 모델 설정을 변경했습니다.");
                }

                let thinking_options =
                    model_catalog::thinking_options_for_model(&self.draft.gemini_cli_model);
                ui.add_space(8.0);
                ui.label(RichText::new("Wrapper 생각").color(text()));
                egui::ComboBox::from_id_salt("cli_thinking_combo")
                    .width(ui.available_width())
                    .selected_text(self.draft.gemini_fast_thinking_level.clone())
                    .show_ui(ui, |ui| {
                        for level in thinking_options {
                            ui.selectable_value(
                                &mut self.draft.gemini_fast_thinking_level,
                                level.to_owned(),
                                level,
                            );
                        }
                    });
                self.ensure_thinking_for_selected_model();

                let before_fast = self.draft.gemini_cli_use_fast_wrapper;
                toggle_row(
                    ui,
                    &mut self.draft.gemini_cli_use_fast_wrapper,
                    "Fast wrapper 사용",
                )
                .on_hover_text(
                    "Code Assist/GEMINI_API_KEY fast path를 먼저 시도하고 실패 시 native Gemini CLI로 fallback합니다.",
                );
                if before_fast != self.draft.gemini_cli_use_fast_wrapper {
                    self.draft.clear_gemini_cli_verification_cache();
                    self.settings.write().clear_gemini_cli_verification_cache();
                    self.set_status("fast wrapper 설정 변경으로 CLI 검증 캐시를 초기화했습니다.");
                }
                ui.add_space(8.0);
                let setting_up = self.cli_setup_inflight.load(Ordering::SeqCst);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !setting_up,
                            egui::Button::new(if setting_up {
                                "초기설정 확인 중..."
                            } else {
                                "CLI 초기설정"
                            }),
                        )
                        .clicked()
                    {
                        self.launch_cli_setup_flow();
                    }
                    if ui.button("환경 진단").clicked() {
                        self.refresh_cli_setup_environment();
                    }
                    ui.label(RichText::new("2026-06-18 이후 보증 불가").color(muted_text()));
                });
                ui.add_space(8.0);
                self.draw_cli_cache_summary(ui);
                ui.add_space(8.0);
                self.draw_cli_setup_panel(ui);
            });
        });
    }

    fn draw_proxy_section(&mut self, ui: &mut egui::Ui) {
        self.section_anchor(ui, AppPage::Proxy);
        draw_card(ui, "서버 / 프록시", |ui| {
            egui::Grid::new("server_grid")
                .num_columns(2)
                .spacing([14.0, 10.0])
                .show(ui, |ui| {
                    ui.label("Base URL");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.draft.base_url)
                            .desired_width(ui.available_width()),
                    );
                    ui.end_row();
                });

            ui.add_space(12.0);
            toggle_row(
                ui,
                &mut self.draft.open_ai_proxy_enabled,
                "OpenAI 호환 프록시 활성화",
            );
            toggle_row(
                ui,
                &mut self.draft.gemini_proxy_enabled,
                "Gemini 호환 프록시 활성화",
            );
            toggle_row(
                ui,
                &mut self.draft.require_proxy_api_key,
                "로컬 API 키 인증 요구",
            );

            ui.add_space(12.0);
            egui::Grid::new("api_key_grid")
                .num_columns(2)
                .spacing([14.0, 10.0])
                .show(ui, |ui| {
                    ui.label("로컬 API 키");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.draft.local_api_key)
                                .desired_width((ui.available_width() - 156.0).max(160.0)),
                        );
                        if ui.button("생성").clicked() {
                            self.draft.local_api_key = generate_local_api_key();
                            self.draft.require_proxy_api_key = true;
                            ui.ctx()
                                .copy_text(self.draft.local_api_key.trim().to_owned());
                            self.set_status("새 로컬 API 키를 생성하고 복사했습니다.");
                        }
                        if ui.button("복사").clicked() {
                            ui.ctx()
                                .copy_text(self.draft.local_api_key.trim().to_owned());
                            self.logs.push("[GUI] 로컬 API 키를 복사했습니다.");
                            self.set_status("로컬 API 키를 복사했습니다.");
                        }
                    });
                    ui.end_row();
                });
        });
    }

    fn draw_stats_section(&mut self, ui: &mut egui::Ui) {
        self.section_anchor(ui, AppPage::Stats);
        card_frame(surface(), border()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("요청 통계").size(18.0).strong().color(text()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("초기화").clicked() {
                        self.confirm_usage_reset = true;
                    }
                    if ui.button("새로고침").clicked() {
                        self.set_status("요청 통계를 갱신했습니다.");
                    }
                    egui::ComboBox::from_id_salt("usage_period_combo")
                        .width(104.0)
                        .selected_text(period_label(self.usage_period))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.usage_period,
                                UsageStatsPeriod::Daily,
                                "일별",
                            );
                            ui.selectable_value(
                                &mut self.usage_period,
                                UsageStatsPeriod::Weekly,
                                "주간",
                            );
                            ui.selectable_value(
                                &mut self.usage_period,
                                UsageStatsPeriod::Monthly,
                                "월별",
                            );
                        });
                });
            });
            ui.add_space(14.0);
            self.draw_usage_panel(ui);
        });
    }

    fn draw_prompt_section(&mut self, ui: &mut egui::Ui) {
        self.section_anchor(ui, AppPage::Prompt);
        draw_card(ui, "프롬프트 / ivLyrics", |ui| {
            toggle_row(ui, &mut self.draft.raw_prompt_mode, "Raw Prompt 모드");
            ui.horizontal(|ui| {
                ui.add_space(22.0);
                ui.label(
                    RichText::new("호환 API/루트 요청 본문을 번역 래핑 없이 그대로 보냅니다.")
                        .color(muted_text()),
                );
            });
            ui.add_space(8.0);
            toggle_row(
                ui,
                &mut self.draft.iv_lyrics_study_cli_direct_enabled,
                "ivLyrics 학습/퀴즈 CLI fast lane",
            );
            ui.horizontal(|ui| {
                ui.add_space(22.0);
                ui.label(
                    RichText::new(
                        "학습/퀴즈 요청은 원본 prompt 그대로 CLI 제한 게이트를 우회해 즉시 보내고, CLI 한도류 실패 시에만 WebView를 직렬 fallback으로 사용합니다.",
                    )
                    .color(muted_text()),
                );
            });
        });

        ui.add_space(14.0);
        draw_card(ui, "프롬프트 편집", |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "저장 위치: {}",
                            self.paths.prompt_override_path().display()
                        ))
                        .color(muted_text()),
                    );
                    ui.label(
                        RichText::new(
                            "프롬프트 문자열을 줄바꿈 그대로 편집합니다. 저장 시 prompts.json으로 변환됩니다.",
                        )
                        .color(muted_text()),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                    if ui.button("저장").clicked() {
                        self.save_prompt_editor();
                    }
                    if ui.button("다시 읽기").clicked() {
                        self.reload_prompt_editor();
                    }
                    if ui.button("기본값 불러오기").clicked() {
                        self.load_default_prompt_editor();
                    }
                });
            });
            ui.add_space(10.0);
            ui.add_sized(
                [ui.available_width(), 430.0],
                egui::TextEdit::multiline(&mut self.prompt_editor_text)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .lock_focus(true),
            );
        });
    }

    fn draw_cli_cache_summary(&mut self, ui: &mut egui::Ui) {
        let settings = self.settings.read().clone();
        let cached = settings.cached_gemini_cli_model_options();
        if cached.is_empty() {
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "CLI 직접 시작은 ruster 초기설정에서 모델 검증을 완료한 뒤 활성화됩니다.",
                    )
                    .color(muted_text()),
                )
                .wrap(),
            );
        } else {
            let text = format!(
                "검증된 CLI 모델: {}",
                cached
                    .iter()
                    .map(|model| model.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            draw_single_line_scroll_text(ui, "cli_cache_summary", &text, muted_text());
        }
    }

    fn draw_cli_setup_panel(&self, ui: &mut egui::Ui) {
        let state = self.cli_setup_panel.read().clone();
        ui.separator();
        ui.label(RichText::new("CLI 초기설정 상태").strong().color(text()));
        ui.label(RichText::new(state.phase).color(accent_light()));
        draw_multiline_scroll_text(ui, "cli_setup_summary", &state.summary);
        if !state.detail.trim().is_empty() {
            ui.add(egui::Label::new(RichText::new(state.detail).color(muted_text())).wrap());
        }
    }

    fn draw_usage_panel(&mut self, ui: &mut egui::Ui) {
        let usage = UsageMetrics::new(&self.paths, self.logs.clone());
        let snapshot = usage.snapshot();
        let buckets = usage.buckets(self.usage_period);

        ui.columns(4, |columns| {
            metric_tile(
                &mut columns[0],
                "총 요청",
                &format_count(snapshot.total_requests),
                &format!("성공률 {:.1}%", snapshot.success_rate()),
                text(),
            );
            metric_tile(
                &mut columns[1],
                "성공",
                &format_count(snapshot.succeeded_requests),
                "완료된 요청",
                success(),
            );
            metric_tile(
                &mut columns[2],
                "실패 / 취소",
                &format!(
                    "{} / {}",
                    format_count(snapshot.failed_requests),
                    format_count(snapshot.cancelled_requests)
                ),
                "오류 및 stale",
                danger(),
            );
            metric_tile(
                &mut columns[3],
                "입력 / 출력 토큰",
                &format!(
                    "{} / {}",
                    format_count(snapshot.input_tokens),
                    format_count(snapshot.successful_output_tokens)
                ),
                "로컬 추정값",
                accent_light(),
            );
        });
        ui.add_space(12.0);

        draw_usage_chart(ui, &buckets);
        ui.add_space(12.0);

        draw_usage_detail(ui, &self.paths, &snapshot, &buckets);
        if !snapshot.last_failure.is_empty() {
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!("마지막 실패: {}", snapshot.last_failure)).color(warning()),
            );
        }
    }

    fn draw_usage_reset_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_usage_reset {
            return;
        }

        let mut close = false;
        egui::Window::new("통계 초기화")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label("저장된 요청 통계를 초기화할까요?");
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("초기화").clicked() {
                        UsageMetrics::new(&self.paths, self.logs.clone()).reset();
                        self.set_status("요청 통계를 초기화했습니다.");
                        close = true;
                    }
                    if ui.button("취소").clicked() {
                        close = true;
                    }
                });
            });

        if close {
            self.confirm_usage_reset = false;
        }
    }

    fn draw_cli_model_verified_dialog(&mut self, ctx: &egui::Context) {
        let Some(message) = self.cli_model_verified_notice.clone() else {
            return;
        };

        let mut close = false;
        egui::Window::new("Gemini CLI 모델 확인")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.label(
                    RichText::new("모델 확인이 완료되었습니다.")
                        .strong()
                        .color(success()),
                );
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(message).color(text())).wrap());
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("확인").clicked() {
                        close = true;
                    }
                    if self.cli_start_after_setup {
                        ui.label(
                            RichText::new("닫으면 Gemini CLI 모드를 시작합니다.")
                                .color(muted_text()),
                        );
                    }
                });
            });

        if close {
            self.cli_model_verified_notice = None;
        }
    }

    fn draw_developer_info_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_developer_info {
            return;
        }

        let mut close = false;
        egui::Window::new("개발자 정보")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.label(RichText::new("ruster").size(20.0).strong().color(text()));
                ui.add_space(8.0);
                ui.label(RichText::new(format!("개발자: {DEVELOPER_NAME}")).color(text()));
                ui.hyperlink_to(DEVELOPER_GITHUB, DEVELOPER_GITHUB);
                ui.hyperlink_to(
                    format!("Telegram: {DEVELOPER_TELEGRAM} ({DEVELOPER_TELEGRAM_URL})"),
                    DEVELOPER_TELEGRAM_URL,
                );
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("GitHub 복사").clicked() {
                        ui.ctx().copy_text(DEVELOPER_GITHUB.to_owned());
                        self.set_status("GitHub 주소를 복사했습니다.");
                    }
                    if ui.button("Telegram 링크 복사").clicked() {
                        ui.ctx().copy_text(DEVELOPER_TELEGRAM_URL.to_owned());
                        self.set_status("Telegram 링크를 복사했습니다.");
                    }
                    if ui.button("닫기").clicked() {
                        close = true;
                    }
                });
            });

        if close {
            self.show_developer_info = false;
        }
    }

    fn draw_main_sections(&mut self, ui: &mut egui::Ui) {
        self.draw_header(ui);
        self.draw_overview_section(ui);
        ui.add_space(18.0);
        self.draw_runtime_section(ui);
        ui.add_space(16.0);
        self.draw_proxy_section(ui);
        ui.add_space(16.0);
        self.draw_stats_section(ui);
        ui.add_space(16.0);
        self.draw_prompt_section(ui);
    }
}

fn period_label(period: UsageStatsPeriod) -> &'static str {
    match period {
        UsageStatsPeriod::Daily => "일별",
        UsageStatsPeriod::Weekly => "주간",
        UsageStatsPeriod::Monthly => "월별",
    }
}

fn translation_mode_setting_value(mode: TranslationMode) -> &'static str {
    match mode {
        TranslationMode::WebView => "WebView",
        TranslationMode::GeminiCli => "GeminiCli",
        TranslationMode::ChatGptWebView => "ChatGptWebView",
    }
}

fn draw_usage_chart(ui: &mut egui::Ui, buckets: &[UsageBucket]) {
    let desired = egui::vec2(ui.available_width().max(320.0), 268.0);
    let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 8.0, input_bg());
    painter.rect_stroke(
        rect,
        8.0,
        Stroke::new(1.0, border()),
        egui::StrokeKind::Inside,
    );

    let area_left = rect.left() + 10.0;
    let area_top = rect.top() + 8.0;
    let area_right = rect.right() - 10.0;
    let area_bottom = rect.bottom() - 8.0;
    let plot_left = area_left + 52.0;
    let plot_top = area_top + 38.0;
    let plot_right = area_right - 18.0;
    let plot_bottom = area_bottom - 42.0;
    let plot_width = (plot_right - plot_left).max(1.0);
    let plot_height = (plot_bottom - plot_top).max(1.0);

    painter.text(
        egui::pos2(area_left + 2.0, area_top + 2.0),
        egui::Align2::LEFT_TOP,
        "요청 수",
        egui::FontId::proportional(13.0),
        text(),
    );
    draw_chart_legend(&painter, area_right - 190.0, area_top + 2.0);

    let has_data = buckets.iter().any(|bucket| bucket.total_requests > 0);
    if !has_data {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "표시할 요청 통계가 없습니다.",
            egui::FontId::proportional(14.0),
            muted_text(),
        );
        return;
    }

    let max_total = buckets
        .iter()
        .map(|bucket| bucket.total_requests)
        .max()
        .unwrap_or(1)
        .max(1);
    let grid_stroke = Stroke::new(1.0, border());
    for i in 0..=4 {
        let y = plot_bottom - plot_height * i as f32 / 4.0;
        painter.line_segment(
            [egui::pos2(plot_left, y), egui::pos2(plot_right, y)],
            grid_stroke,
        );
        let value = (max_total as f32 * i as f32 / 4.0).round() as u64;
        painter.text(
            egui::pos2(plot_left - 8.0, y - 8.0),
            egui::Align2::RIGHT_TOP,
            format_count(value),
            egui::FontId::proportional(11.0),
            muted_text(),
        );
    }

    painter.line_segment(
        [
            egui::pos2(plot_left, plot_top),
            egui::pos2(plot_left, plot_bottom),
        ],
        Stroke::new(1.0, muted_text()),
    );
    painter.line_segment(
        [
            egui::pos2(plot_left, plot_bottom),
            egui::pos2(plot_right, plot_bottom),
        ],
        Stroke::new(1.0, muted_text()),
    );

    let count = buckets.len().max(1) as f32;
    let gap = 8.0;
    let bar_width = ((plot_width - gap * (count + 1.0)) / count).max(10.0);
    for (index, bucket) in buckets.iter().enumerate() {
        let x = plot_left + gap + index as f32 * (bar_width + gap);
        let total_height = plot_height * bucket.total_requests as f32 / max_total as f32;
        let y = plot_bottom - total_height;
        let mut cursor = y + total_height;
        draw_bar_segment(
            &painter,
            x,
            bar_width,
            &mut cursor,
            total_height * bucket.succeeded_requests as f32 / bucket.total_requests.max(1) as f32,
            success(),
        );
        draw_bar_segment(
            &painter,
            x,
            bar_width,
            &mut cursor,
            total_height * bucket.failed_requests as f32 / bucket.total_requests.max(1) as f32,
            danger(),
        );
        let cancelled_height = (cursor - y).max(0.0);
        draw_bar_segment(
            &painter,
            x,
            bar_width,
            &mut cursor,
            cancelled_height,
            muted_text(),
        );

        painter.text(
            egui::pos2(x + bar_width / 2.0, plot_bottom + 6.0),
            egui::Align2::CENTER_TOP,
            shorten_bucket_label(&bucket.label),
            egui::FontId::proportional(11.0),
            muted_text(),
        );
        if bucket.total_requests > 0 {
            painter.text(
                egui::pos2(x + bar_width / 2.0, (y - 18.0).max(plot_top)),
                egui::Align2::CENTER_TOP,
                format_count(bucket.total_requests),
                egui::FontId::proportional(11.0),
                text(),
            );
        }
    }
}

fn draw_chart_legend(painter: &egui::Painter, left: f32, top: f32) {
    draw_legend_item(painter, left, top, success(), "성공");
    draw_legend_item(painter, left + 64.0, top, danger(), "실패");
    draw_legend_item(painter, left + 128.0, top, muted_text(), "취소");
}

fn draw_legend_item(painter: &egui::Painter, left: f32, top: f32, color: Color32, label: &str) {
    let swatch = egui::Rect::from_min_size(egui::pos2(left, top + 4.0), egui::vec2(11.0, 11.0));
    painter.rect_filled(swatch, 2.0, color);
    painter.text(
        egui::pos2(left + 16.0, top),
        egui::Align2::LEFT_TOP,
        label,
        egui::FontId::proportional(11.0),
        text(),
    );
}

fn draw_bar_segment(
    painter: &egui::Painter,
    x: f32,
    width: f32,
    cursor: &mut f32,
    height: f32,
    color: Color32,
) {
    if height <= 0.0 {
        return;
    }

    *cursor -= height;
    let rect =
        egui::Rect::from_min_size(egui::pos2(x, *cursor), egui::vec2(width, height.max(1.0)));
    painter.rect_filled(rect, 2.0, color);
}

fn draw_usage_detail(
    ui: &mut egui::Ui,
    paths: &AppPaths,
    snapshot: &UsageSnapshot,
    buckets: &[UsageBucket],
) {
    egui::Frame::NONE
        .fill(input_bg())
        .stroke(Stroke::new(1.0, border()))
        .corner_radius(8)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(format!(
                    "Gemini {}  /  OpenAI {}  /  호환 API {}  /  기타 {}",
                    format_count(snapshot.gemini_requests),
                    format_count(snapshot.open_ai_requests),
                    format_count(snapshot.mort_requests),
                    format_count(snapshot.other_requests)
                ))
                .color(muted_text()),
            );
            ui.label(
                RichText::new(format!(
                    "집계 시작 {}  /  최근 업데이트 {}",
                    empty_dash(&snapshot.started_at_local),
                    empty_dash(&snapshot.last_updated_at_local)
                ))
                .color(muted_text()),
            );
            ui.label(RichText::new("토큰은 로컬 문자열 기반 추정값입니다.").color(muted_text()));
            ui.add_space(8.0);
            ui.monospace(format!(
                "저장 위치: {}",
                paths.usage_metrics_path().display()
            ));
            ui.monospace(format!(
                "입력 문자: {}",
                format_count(snapshot.input_characters)
            ));
            ui.monospace(format!(
                "성공 출력 문자: {}",
                format_count(snapshot.successful_output_characters)
            ));
            ui.monospace(format!("최근 실패: {}", empty_dash(&snapshot.last_failure)));
            ui.add_space(6.0);
            if buckets.is_empty() {
                ui.monospace("기간별 통계: -");
            } else {
                for bucket in buckets {
                    ui.monospace(format!(
                        "{}  요청 {}, 성공 {} ({:.1}%), 실패 {}, 취소 {}, G/O/호환/기타 {}/{}/{}/{}, 입력 {}, 출력 {}",
                        bucket.label,
                        format_count(bucket.total_requests),
                        format_count(bucket.succeeded_requests),
                        bucket.success_rate(),
                        format_count(bucket.failed_requests),
                        format_count(bucket.cancelled_requests),
                        format_count(bucket.gemini_requests),
                        format_count(bucket.open_ai_requests),
                        format_count(bucket.mort_requests),
                        format_count(bucket.other_requests),
                        format_count(bucket.input_tokens),
                        format_count(bucket.successful_output_tokens)
                    ));
                }
            }
        });
}

fn shorten_bucket_label(label: &str) -> String {
    if label.len() <= 10 {
        label.to_owned()
    } else {
        label[label.len().saturating_sub(5)..].to_owned()
    }
}

fn format_count(value: u64) -> String {
    let raw = value.to_string();
    let mut out = String::with_capacity(raw.len() + raw.len() / 3);
    for (index, ch) in raw.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn empty_dash(value: &str) -> &str {
    if value.trim().is_empty() { "-" } else { value }
}

async fn probe_and_store_cli_models(
    paths: AppPaths,
    settings: Arc<RwLock<AppSettings>>,
    logs: LogBuffer,
    selected_model: String,
) -> bool {
    let settings_snapshot = settings.read().clone().normalized();
    let use_fast_wrapper = settings_snapshot.gemini_cli_use_fast_wrapper;
    logs.push(format!(
        "[GeminiCli] 사용 가능 CLI 모델 검증 시작 (preferred={selected_model}, mode={})",
        if use_fast_wrapper {
            "fast-wrapper"
        } else {
            "native-cli"
        }
    ));

    let mut available = Vec::new();
    let mut wrapper_sources = Vec::<String>::new();
    let cli_source = cli_discovery::try_find()
        .map(|installation| installation.display_source())
        .unwrap_or_default();

    if use_fast_wrapper {
        let results = fast_client::probe_models(
            model_catalog::CLI_MODELS,
            std::time::Duration::from_secs(25),
            FastGenerationConfig::from_settings(&settings_snapshot),
        )
        .await;
        for result in &results {
            if result.available {
                let preview = if result.preview.trim().is_empty() {
                    String::new()
                } else {
                    format!(
                        ", preview={}",
                        crate::logging::summarize_text(&result.preview, 80)
                    )
                };
                logs.push(format!(
                    "[GeminiCli] 모델 사용 가능: {} ({}{})",
                    result.model.id, result.source, preview
                ));
                available.push(result.model.id.to_owned());
                if !result.source.trim().is_empty()
                    && !wrapper_sources
                        .iter()
                        .any(|source| source.eq_ignore_ascii_case(&result.source))
                {
                    wrapper_sources.push(result.source.clone());
                }
            } else {
                logs.push(format!(
                    "[GeminiCli] 모델 제외: {} ({})",
                    result.model.id,
                    crate::logging::summarize_text(&result.error, 180)
                ));
            }
        }
    } else {
        let results = GeminiCliClient::probe_native_models(
            model_catalog::CLI_MODELS,
            std::time::Duration::from_secs(25),
        )
        .await;
        for result in &results {
            if result.available {
                logs.push(format!(
                    "[GeminiCli] 모델 사용 가능: {} ({})",
                    result.model.id, result.source
                ));
                available.push(result.model.id.to_owned());
                if !result.source.trim().is_empty()
                    && !wrapper_sources
                        .iter()
                        .any(|source| source.eq_ignore_ascii_case(&result.source))
                {
                    wrapper_sources.push(result.source.clone());
                }
            } else {
                logs.push(format!(
                    "[GeminiCli] 모델 제외: {} ({})",
                    result.model.id,
                    crate::logging::summarize_text(&result.error, 180)
                ));
            }
        }
    }

    if available.is_empty() {
        logs.push("[GeminiCli] 사용 가능한 CLI 모델을 확인하지 못했습니다.");
        return false;
    }

    {
        let mut guard = settings.write();
        guard.store_gemini_cli_verification_cache(
            available.clone(),
            &selected_model,
            &cli_source,
            &wrapper_sources.join(", "),
        );
        if let Err(error) = guard.save(&paths) {
            logs.push(format!("[GeminiCli] 모델 검증 캐시 저장 실패: {error}"));
            return false;
        }
    }

    logs.push(format!(
        "[GeminiCli] 모델 검증 캐시 저장: {}",
        available.join(", ")
    ));
    true
}

impl eframe::App for RusterApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.apply_gui_theme(ctx);
        self.sync_cli_cache_from_settings();
        self.sync_cli_setup_notice();
        self.finish_cli_auto_start_after_setup();

        if ctx.input(|input| input.key_pressed(egui::Key::F12)) {
            let host = self.host.clone();
            self.runtime.spawn(async move {
                let _ = host.activate_request_guard_with_recovery("gui-f12").await;
            });
            self.set_status("현재 요청을 취소하고 WebView 세션 복구를 준비합니다.");
        }
        if ctx.input(|input| input.key_pressed(egui::Key::F5)) {
            let host = self.host.clone();
            self.runtime.spawn(async move {
                let _ = host.request_session_recovery().await;
            });
            self.set_status("WebView 세션 복구를 요청했습니다.");
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(500));

        egui::SidePanel::left("ruster_nav")
            .resizable(false)
            .exact_width(230.0)
            .frame(egui::Frame::NONE.fill(nav_bg()).inner_margin(Margin {
                left: 18,
                right: 18,
                top: 20,
                bottom: 18,
            }))
            .show(ctx, |ui| self.draw_sidebar(ui));

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(app_bg()).inner_margin(Margin {
                left: 30,
                right: 34,
                top: 24,
                bottom: 32,
            }))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.draw_main_sections(ui));
            });

        self.draw_usage_reset_dialog(ctx);
        self.draw_cli_model_verified_dialog(ctx);
        self.draw_developer_info_dialog(ctx);

        if self.close_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_server();
        self.save_settings();
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        Color32::from_rgba_unmultiplied(0, 0, 0, 0).to_normalized_gamma_f32()
    }
}

fn draw_card(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
    card_frame(surface(), border()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(title).size(18.0).strong().color(text()));
        ui.add_space(14.0);
        add_contents(ui);
    });
}

fn metric_tile(ui: &mut egui::Ui, title: &str, value: &str, caption: &str, color: Color32) {
    card_frame(surface_raised(), border()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(title).color(muted_text()));
        ui.add_space(4.0);
        ui.label(RichText::new(value).size(18.0).strong().color(color));
        ui.label(RichText::new(caption).color(muted_text()));
    });
}

fn draw_single_line_scroll_text(
    ui: &mut egui::Ui,
    id: &'static str,
    text_value: &str,
    color: Color32,
) {
    egui::ScrollArea::horizontal()
        .id_salt(id)
        .auto_shrink([false, true])
        .max_height(24.0)
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(RichText::new(text_value).color(color))
                    .wrap_mode(egui::TextWrapMode::Extend),
            );
        });
}

fn draw_multiline_scroll_text(ui: &mut egui::Ui, id: &'static str, text_value: &str) {
    let line_count = text_value.lines().count().clamp(1, 6) as f32;
    let height = line_count * 18.0 + 14.0;
    egui::Frame::NONE
        .fill(input_bg())
        .stroke(Stroke::new(1.0, border()))
        .corner_radius(7)
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::horizontal()
                .id_salt(id)
                .auto_shrink([false, false])
                .max_height(height)
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(text_value).monospace().color(muted_text()))
                            .wrap_mode(egui::TextWrapMode::Extend),
                    );
                });
        });
}

fn mode_start_card(
    ui: &mut egui::Ui,
    title: &str,
    caption: &str,
    badge: &str,
    color: Color32,
) -> bool {
    let mut clicked = false;
    let frame = mode_card_frame(surface(), border()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.set_min_height(124.0);
        fixed_badge(
            ui,
            egui::vec2(34.0, 34.0),
            badge,
            if badge.len() > 1 { 12.0 } else { 14.0 },
            badge_bg(),
            color,
        );
        ui.add_space(8.0);
        ui.label(RichText::new(title).size(20.0).strong().color(color));
        ui.label(RichText::new(caption).color(muted_text()));
        ui.add_space(12.0);
        ui.label(RichText::new("시작").strong().color(text()));
    });
    let response = ui
        .interact(
            frame.response.rect,
            ui.make_persistent_id(("mode_start_card", title)),
            egui::Sense::click(),
        )
        .on_hover_text("설정을 저장하고 이 창을 닫은 뒤 선택한 백엔드를 시작합니다.");
    if response.hovered() {
        ui.painter().rect_stroke(
            frame.response.rect,
            8.0,
            Stroke::new(1.0, accent()),
            egui::StrokeKind::Inside,
        );
    }
    if response.clicked() {
        clicked = true;
    }
    clicked
}

fn card_frame(fill: Color32, stroke: Color32) -> egui::Frame {
    egui::Frame::NONE
        .fill(fill)
        .stroke(Stroke::new(1.0, stroke))
        .corner_radius(8)
        .inner_margin(Margin::same(20))
}

fn compact_card_frame(fill: Color32, stroke: Color32) -> egui::Frame {
    egui::Frame::NONE
        .fill(fill)
        .stroke(Stroke::new(1.0, stroke))
        .corner_radius(8)
        .inner_margin(Margin::same(12))
}

fn mode_card_frame(fill: Color32, stroke: Color32) -> egui::Frame {
    egui::Frame::NONE
        .fill(fill)
        .stroke(Stroke::new(1.0, stroke))
        .corner_radius(8)
        .inner_margin(Margin::same(18))
}

fn fixed_badge(
    ui: &mut egui::Ui,
    size: egui::Vec2,
    label: &str,
    font_size: f32,
    fill: Color32,
    color: Color32,
) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter().rect_filled(rect, 8.0, fill);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(font_size),
        color,
    );
}

fn toggle_row(ui: &mut egui::Ui, value: &mut bool, label: &str) -> egui::Response {
    let height = 28.0;
    let (rect, mut response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::click(),
    );
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }

    let track_rect = Rect::from_min_size(
        egui::pos2(rect.left(), rect.center().y - 11.0),
        egui::vec2(42.0, 22.0),
    );
    let track_fill = if *value { accent_strong() } else { border() };
    let track_stroke = if *value { accent_strong() } else { border() };
    ui.painter().rect_filled(track_rect, 11.0, track_fill);
    ui.painter().rect_stroke(
        track_rect,
        11.0,
        Stroke::new(1.0, track_stroke),
        egui::StrokeKind::Inside,
    );

    let thumb_x = if *value {
        track_rect.right() - 11.0
    } else {
        track_rect.left() + 11.0
    };
    ui.painter()
        .circle_filled(egui::pos2(thumb_x, track_rect.center().y), 9.0, surface());
    ui.painter().text(
        egui::pos2(track_rect.right() + 10.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(14.0),
        text(),
    );

    if response.hovered() {
        ui.painter().rect_stroke(
            track_rect.expand(1.0),
            12.0,
            Stroke::new(1.0, accent()),
            egui::StrokeKind::Inside,
        );
    }

    response
}

fn app_bg() -> Color32 {
    app_bg_for(is_dark_ui())
}

fn nav_bg() -> Color32 {
    nav_bg_for(is_dark_ui())
}

fn nav_selected() -> Color32 {
    nav_selected_for(is_dark_ui())
}

fn surface() -> Color32 {
    surface_for(is_dark_ui())
}

fn surface_raised() -> Color32 {
    surface_raised_for(is_dark_ui())
}

fn input_bg() -> Color32 {
    input_bg_for(is_dark_ui())
}

fn badge_bg() -> Color32 {
    badge_bg_for(is_dark_ui())
}

fn border() -> Color32 {
    border_for(is_dark_ui())
}

fn text() -> Color32 {
    text_for(is_dark_ui())
}

fn muted_text() -> Color32 {
    muted_text_for(is_dark_ui())
}

fn accent_light() -> Color32 {
    accent_light_for(is_dark_ui())
}

fn accent() -> Color32 {
    accent_for(is_dark_ui())
}

fn accent_soft() -> Color32 {
    accent_soft_for(is_dark_ui())
}

fn accent_strong() -> Color32 {
    accent_strong_for(is_dark_ui())
}

fn success() -> Color32 {
    success_for(is_dark_ui())
}

fn warning() -> Color32 {
    warning_for(is_dark_ui())
}

fn danger() -> Color32 {
    danger_for(is_dark_ui())
}

fn is_dark_ui() -> bool {
    DARK_UI.load(Ordering::Relaxed)
}

fn app_bg_for(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgba_unmultiplied(24, 24, 27, 204)
    } else {
        Color32::from_rgba_unmultiplied(247, 249, 252, 221)
    }
}

fn nav_bg_for(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgba_unmultiplied(32, 33, 38, 184)
    } else {
        Color32::from_rgba_unmultiplied(239, 243, 249, 191)
    }
}

fn nav_selected_for(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgba_unmultiplied(42, 45, 53, 210)
    } else {
        Color32::from_rgb(255, 255, 255)
    }
}

fn surface_for(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgba_unmultiplied(36, 38, 45, 217)
    } else {
        Color32::from_rgba_unmultiplied(255, 255, 255, 239)
    }
}

fn surface_raised_for(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgba_unmultiplied(48, 51, 59, 204)
    } else {
        Color32::from_rgba_unmultiplied(244, 247, 252, 223)
    }
}

fn input_bg_for(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgba_unmultiplied(31, 33, 39, 238)
    } else {
        Color32::from_rgb(255, 255, 255)
    }
}

fn badge_bg_for(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgba_unmultiplied(37, 42, 52, 170)
    } else {
        Color32::from_rgb(239, 246, 255)
    }
}

fn border_for(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgba_unmultiplied(69, 73, 85, 138)
    } else {
        Color32::from_rgba_unmultiplied(214, 223, 235, 191)
    }
}

fn text_for(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(244, 244, 245)
    } else {
        Color32::from_rgb(27, 36, 48)
    }
}

fn muted_text_for(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(181, 186, 197)
    } else {
        Color32::from_rgb(101, 113, 132)
    }
}

fn accent_for(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(96, 165, 250)
    } else {
        Color32::from_rgb(59, 130, 246)
    }
}

fn accent_light_for(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(96, 165, 250)
    } else {
        Color32::from_rgb(37, 99, 235)
    }
}

fn accent_soft_for(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgba_unmultiplied(59, 38, 50, 64)
    } else {
        Color32::from_rgb(248, 220, 232)
    }
}

fn accent_strong_for(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(59, 130, 246)
    } else {
        Color32::from_rgb(29, 78, 216)
    }
}

fn success_for(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(74, 222, 128)
    } else {
        Color32::from_rgb(22, 163, 74)
    }
}

fn warning_for(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(251, 191, 36)
    } else {
        Color32::from_rgb(180, 83, 9)
    }
}

fn danger_for(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(248, 113, 113)
    } else {
        Color32::from_rgb(220, 38, 38)
    }
}

#[cfg(windows)]
fn native_window_handle(cc: &eframe::CreationContext<'_>) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

#[cfg(not(windows))]
fn native_window_handle(_cc: &eframe::CreationContext<'_>) -> Option<isize> {
    None
}

#[cfg(windows)]
fn apply_windows_mica_backdrop(hwnd: Option<isize>, dark: bool, logs: &LogBuffer) {
    use std::ffi::c_void;
    use std::mem::size_of;

    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DWMSBT_MAINWINDOW, DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_SYSTEMBACKDROP_TYPE,
        DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
        DWMWCP_ROUND, DwmSetWindowAttribute,
    };

    let Some(hwnd) = hwnd else {
        return;
    };
    let hwnd = HWND(hwnd as *mut _);
    let dark_value: i32 = i32::from(dark);
    let backdrop = DWMSBT_MAINWINDOW.0;
    let caption = color_to_dwm_color(app_bg_for(dark));
    let border = color_to_dwm_color(border_for(dark));
    let text = color_to_dwm_color(text_for(dark));
    let corner = DWMWCP_ROUND.0;
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark_value as *const _ as *const c_void,
            size_of::<i32>() as u32,
        );
        let backdrop_result = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &backdrop as *const _ as *const c_void,
            size_of::<i32>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            &caption as *const _ as *const c_void,
            size_of::<u32>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &border as *const _ as *const c_void,
            size_of::<u32>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TEXT_COLOR,
            &text as *const _ as *const c_void,
            size_of::<u32>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const c_void,
            size_of::<i32>() as u32,
        );
        if let Err(error) = backdrop_result {
            logs.push(format!("[GUI] Mica backdrop 적용 실패: {error}"));
        }
    }
}

#[cfg(not(windows))]
fn apply_windows_mica_backdrop(_hwnd: Option<isize>, _dark: bool, _logs: &LogBuffer) {}

fn color_to_dwm_color(color: Color32) -> u32 {
    u32::from(color.r()) | (u32::from(color.g()) << 8) | (u32::from(color.b()) << 16)
}
