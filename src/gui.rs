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
use crate::i18n::{UiLanguage, normalize_ui_language};
use crate::logging::LogBuffer;
use crate::model_catalog;
use crate::prompt_config::{PromptConfig, PromptPresetInfo};
use crate::settings::{AppSettings, generate_local_api_key, normalize_theme_mode};
use crate::update_check::{self, UpdateInfo};
use crate::usage_metrics::{UsageBucket, UsageMetrics, UsageSnapshot, UsageStatsPeriod};
use crate::windows_startup;

static DARK_UI: AtomicBool = AtomicBool::new(false);
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
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

    fn nav_title(self, language: UiLanguage) -> &'static str {
        page_nav_title(language, self)
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
        Self::for_language(UiLanguage::Korean)
    }
}

impl CliSetupPanelState {
    fn for_language(language: UiLanguage) -> Self {
        Self {
            phase: cli_phase_idle(language).to_owned(),
            summary: cli_summary_before_diagnostics(language).to_owned(),
            detail: cli_detail_before_diagnostics(language).to_owned(),
        }
    }

    fn reset_if_idle(&mut self, language: UiLanguage) {
        if is_cli_idle_phase(&self.phase) {
            *self = Self::for_language(language);
        }
    }
}

#[derive(Clone, Debug)]
struct UpdateCheckPanelState {
    phase: String,
    summary: String,
    detail: String,
    release_url: String,
    primary_asset_name: String,
    primary_asset_download_url: String,
    update_available: bool,
    finished: bool,
}

impl Default for UpdateCheckPanelState {
    fn default() -> Self {
        Self::for_language(UiLanguage::Korean)
    }
}

impl UpdateCheckPanelState {
    fn for_language(language: UiLanguage) -> Self {
        Self {
            phase: update_phase_idle(language).to_owned(),
            summary: current_version_summary(language),
            detail: update_detail_uses_latest_release(language).to_owned(),
            release_url: String::new(),
            primary_asset_name: String::new(),
            primary_asset_download_url: String::new(),
            update_available: false,
            finished: false,
        }
    }

    fn reset_if_idle(&mut self, language: UiLanguage) {
        if is_update_idle_phase(&self.phase) {
            *self = Self::for_language(language);
        }
    }
}

fn update_check_checking_state(language: UiLanguage) -> UpdateCheckPanelState {
    UpdateCheckPanelState {
        phase: update_phase_checking(language).to_owned(),
        summary: current_version_summary(language),
        detail: update_detail_checking_latest_release(language).to_owned(),
        ..UpdateCheckPanelState::for_language(language)
    }
}

fn update_check_success_state(language: UiLanguage, info: UpdateInfo) -> UpdateCheckPanelState {
    let published = info
        .published_at
        .map(|value| value.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "-".to_owned());
    let summary = if info.update_available {
        update_summary_available(language, &info.latest_tag, &info.current_version)
    } else {
        update_summary_current(language, &info.current_version, &info.latest_tag)
    };

    UpdateCheckPanelState {
        phase: update_phase_done(language).to_owned(),
        summary,
        detail: update_detail_success(language, &info.latest_version, &published),
        release_url: info.release_url,
        primary_asset_name: info.primary_asset_name.unwrap_or_default(),
        primary_asset_download_url: info.primary_asset_download_url.unwrap_or_default(),
        update_available: info.update_available,
        finished: true,
    }
}

fn update_check_error_state(language: UiLanguage, error: String) -> UpdateCheckPanelState {
    UpdateCheckPanelState {
        phase: update_phase_failed(language).to_owned(),
        summary: update_summary_failed(language).to_owned(),
        detail: error,
        finished: true,
        ..UpdateCheckPanelState::for_language(language)
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
    webview_profile_reset_inflight: Arc<AtomicBool>,
    webview_profile_reset_status: Arc<RwLock<Option<String>>>,
    confirm_webview_profile_reset: bool,
    show_mort_preset_guide: bool,
    update_check_inflight: Arc<AtomicBool>,
    update_download_inflight: Arc<AtomicBool>,
    update_check_panel: Arc<RwLock<UpdateCheckPanelState>>,
    last_update_check_marker: String,
    show_update_check_dialog: bool,
    prompt_editor_text: String,
    prompt_presets: Vec<PromptPresetInfo>,
    selected_prompt_preset_id: String,
    confirm_usage_reset: bool,
    show_developer_info: bool,
    exit_action: Arc<ParkingMutex<GuiExitAction>>,
    close_requested: bool,
    native_hwnd: Option<isize>,
    last_mica_dark: Option<bool>,
    selected_local_api_key: Option<String>,
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
            paths.webview_data_dir(),
            paths.ivlyrics_study_limit_guard_path(),
        ));
        logs.push(format!(
            "[GUI] ruster 시작. 설정 위치: {}",
            paths.settings_path().display()
        ));
        let prompt_editor_text = PromptConfig::load(&paths, &logs).editable_document();
        let prompt_presets = PromptConfig::get_prompt_presets(&paths);
        let language = UiLanguage::from_setting(&settings.ui_language);
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
            status_message: status_ready(language).to_owned(),
            cli_setup_inflight: Arc::new(AtomicBool::new(false)),
            cli_setup_panel: Arc::new(RwLock::new(CliSetupPanelState::for_language(language))),
            last_cli_setup_phase: String::new(),
            cli_model_verified_notice: None,
            cli_start_after_setup: false,
            webview_profile_reset_inflight: Arc::new(AtomicBool::new(false)),
            webview_profile_reset_status: Arc::new(RwLock::new(None)),
            confirm_webview_profile_reset: false,
            show_mort_preset_guide: false,
            update_check_inflight: Arc::new(AtomicBool::new(false)),
            update_download_inflight: Arc::new(AtomicBool::new(false)),
            update_check_panel: Arc::new(RwLock::new(UpdateCheckPanelState::for_language(
                language,
            ))),
            last_update_check_marker: String::new(),
            show_update_check_dialog: false,
            prompt_editor_text,
            prompt_presets,
            selected_prompt_preset_id: "current".to_owned(),
            confirm_usage_reset: false,
            show_developer_info: false,
            exit_action,
            close_requested: false,
            native_hwnd,
            last_mica_dark: None,
            selected_local_api_key: None,
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
            let language = self.language();
            self.logs
                .push(format!("[GUI] Windows 자동 실행 설정 실패: {error}"));
            self.status_message = windows_startup_failed_status(language, &error.to_string());
        }
    }

    fn language(&self) -> UiLanguage {
        UiLanguage::from_setting(&self.draft.ui_language)
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

        if is_cli_setup_done_phase(&state.phase) {
            let language = self.language();
            let detail = if state.detail.trim().is_empty() {
                cli_models_checked_detail(language).to_owned()
            } else {
                state.detail.clone()
            };
            self.cli_model_verified_notice = Some(cli_model_verified_message(language, &detail));
            self.set_status(cli_model_verified_status(language));
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

    fn set_update_check_panel(
        panel: &Arc<RwLock<UpdateCheckPanelState>>,
        state: UpdateCheckPanelState,
    ) {
        *panel.write() = state;
    }

    fn sync_update_check_status(&mut self) {
        let state = self.update_check_panel.read().clone();
        let marker = format!("{}|{}", state.phase, state.summary);
        if marker == self.last_update_check_marker {
            return;
        }

        if state.finished {
            self.logs.push(format!(
                "[Update] {} {}",
                state.phase,
                crate::logging::summarize_text(&state.summary, 180)
            ));
            self.set_status(state.summary.clone());
        }
        self.last_update_check_marker = marker;
    }

    fn launch_update_check(&mut self) {
        let language = self.language();
        self.show_update_check_dialog = true;
        if self.update_check_inflight.swap(true, Ordering::SeqCst) {
            self.set_status(update_status_already_checking(language));
            return;
        }

        let panel = self.update_check_panel.clone();
        let inflight = self.update_check_inflight.clone();
        let logs = self.logs.clone();
        Self::set_update_check_panel(&panel, update_check_checking_state(language));
        self.set_status(update_status_checking(language));
        logs.push("[Update] GitHub 최신 릴리스 확인 시작");
        self.runtime.spawn(async move {
            let state = match update_check::check_latest_release(APP_VERSION).await {
                Ok(info) => update_check_success_state(language, info),
                Err(error) => update_check_error_state(language, error.to_string()),
            };
            Self::set_update_check_panel(&panel, state);
            inflight.store(false, Ordering::SeqCst);
        });
    }

    fn launch_update_download(&mut self) {
        let language = self.language();
        let state = self.update_check_panel.read().clone();
        if state.primary_asset_name.trim().is_empty()
            || state.primary_asset_download_url.trim().is_empty()
        {
            self.set_status(update_status_no_asset(language));
            return;
        }
        if self.update_download_inflight.swap(true, Ordering::SeqCst) {
            self.set_status(update_status_download_in_progress(language));
            return;
        }

        let asset_name = state.primary_asset_name.clone();
        let download_url = state.primary_asset_download_url.clone();
        let panel = self.update_check_panel.clone();
        let inflight = self.update_download_inflight.clone();
        let logs = self.logs.clone();
        let mut progress = state.clone();
        progress.phase = update_phase_downloading(language).to_owned();
        progress.summary = update_download_summary(language, &asset_name);
        progress.detail = update_download_detail(language).to_owned();
        progress.finished = false;
        Self::set_update_check_panel(&panel, progress);
        self.set_status(update_status_downloading(language));
        logs.push(format!(
            "[Update] 릴리스 EXE 다운로드/실행 시작: {asset_name}"
        ));

        self.runtime.spawn(async move {
            let mut finished = state;
            match update_check::download_and_launch_primary_asset(&asset_name, &download_url).await
            {
                Ok(path) => {
                    finished.phase = update_phase_launched(language).to_owned();
                    finished.summary = update_launch_summary(language, &asset_name);
                    finished.detail = saved_path_detail(language, &path.display().to_string());
                    logs.push(format!(
                        "[Update] 릴리스 EXE 다운로드/실행 완료: {}",
                        path.display()
                    ));
                }
                Err(error) => {
                    finished.phase = update_phase_launch_failed(language).to_owned();
                    finished.summary = update_launch_failed_summary(language).to_owned();
                    finished.detail = error.to_string();
                    logs.push(format!("[Update] 릴리스 EXE 다운로드/실행 실패: {error}"));
                }
            }
            finished.finished = true;
            Self::set_update_check_panel(&panel, finished);
            inflight.store(false, Ordering::SeqCst);
        });
    }

    fn refresh_cli_setup_environment(&mut self) {
        let language = self.language();
        let panel = self.cli_setup_panel.clone();
        Self::set_cli_setup_panel(
            &panel,
            cli_phase_checking_environment(language),
            checking_label(language),
            "",
        );
        self.runtime.spawn(async move {
            let status = tokio::task::spawn_blocking(cli_setup::get_environment_status)
                .await
                .unwrap_or_default();
            Self::set_cli_setup_panel(
                &panel,
                cli_phase_environment_done(language),
                status.summary(),
                "",
            );
        });
        self.set_status(cli_environment_check_started(language));
    }

    fn sync_webview_profile_reset_status(&mut self) {
        let status = self.webview_profile_reset_status.write().take();
        if let Some(status) = status {
            self.set_status(status);
        }
    }

    fn launch_webview_profile_reset(&mut self) {
        let language = self.language();
        if self
            .webview_profile_reset_inflight
            .swap(true, Ordering::SeqCst)
        {
            self.set_status(webview_profile_reset_busy_status(language));
            return;
        }

        let host = self.host.clone();
        let inflight = self.webview_profile_reset_inflight.clone();
        let status_slot = self.webview_profile_reset_status.clone();
        self.set_status(webview_profile_resetting_status(language));
        self.runtime.spawn(async move {
            let status = match host.reset_webview_profiles().await {
                Ok(result) => webview_profile_reset_result_status(
                    language,
                    result.deleted_existing_data,
                    &result.webview_data_dir.display().to_string(),
                ),
                Err(error) => webview_profile_reset_error_status(language, &error.to_string()),
            };
            *status_slot.write() = Some(status);
            inflight.store(false, Ordering::SeqCst);
        });
    }

    fn reload_prompt_editor(&mut self) {
        self.refresh_prompt_presets("current");
        self.prompt_editor_text = PromptConfig::load(&self.paths, &self.logs).editable_document();
        self.selected_prompt_preset_id = "current".to_owned();
        self.set_status(prompt_reloaded_status(self.language()));
    }

    fn load_default_prompt_editor(&mut self) {
        self.refresh_prompt_presets("default");
        self.prompt_editor_text = PromptConfig::default_editable_document();
        self.selected_prompt_preset_id = "default".to_owned();
        self.set_status(prompt_default_loaded_status(self.language()));
    }

    fn refresh_prompt_presets(&mut self, preferred_id: &str) {
        self.prompt_presets = PromptConfig::get_prompt_presets(&self.paths);
        let fallback_id = self
            .prompt_presets
            .iter()
            .find(|preset| preset.id == "current")
            .or_else(|| self.prompt_presets.first())
            .map(|preset| preset.id.clone())
            .unwrap_or_else(|| "current".to_owned());
        self.selected_prompt_preset_id = self
            .prompt_presets
            .iter()
            .find(|preset| preset.id.eq_ignore_ascii_case(preferred_id))
            .map(|preset| preset.id.clone())
            .unwrap_or(fallback_id);
    }

    fn selected_prompt_preset(&self) -> Option<PromptPresetInfo> {
        self.prompt_presets
            .iter()
            .find(|preset| preset.id == self.selected_prompt_preset_id)
            .cloned()
            .or_else(|| self.prompt_presets.first().cloned())
    }

    fn selected_prompt_preset_label(&self) -> String {
        self.selected_prompt_preset()
            .map(|preset| preset.display_name)
            .unwrap_or_else(|| no_preset_label(self.language()).to_owned())
    }

    fn load_selected_prompt_preset(&mut self) {
        let language = self.language();
        let Some(preset) = self.selected_prompt_preset() else {
            self.set_status(no_preset_to_load_status(language));
            return;
        };
        match PromptConfig::load_prompt_preset_document(&self.paths, &preset) {
            Ok(document) => {
                self.prompt_editor_text = document;
                self.selected_prompt_preset_id = preset.id.clone();
                self.set_status(preset_loaded_status(language, &preset.display_name));
            }
            Err(error) => {
                self.logs
                    .push(format!("[PromptConfig] 프리셋 불러오기 실패: {error}"));
                self.set_status(preset_load_failed_status(language, &error.to_string()));
            }
        }
    }

    fn save_prompt_preset(&mut self) {
        let language = self.language();
        match PromptConfig::save_user_preset_document(&self.paths, &self.prompt_editor_text) {
            Ok(preset) => {
                let preset_id = preset.id.clone();
                let path = preset
                    .source_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| preset.display_name.clone());
                self.refresh_prompt_presets(&preset_id);
                self.logs
                    .push(format!("[PromptConfig] 사용자 프리셋 저장: {path}"));
                self.set_status(preset_saved_status(language, &path));
            }
            Err(error) => {
                self.logs
                    .push(format!("[PromptConfig] 사용자 프리셋 저장 실패: {error}"));
                self.set_status(preset_save_failed_status(language, &error.to_string()));
            }
        }
    }

    fn delete_selected_prompt_preset(&mut self) {
        let language = self.language();
        let Some(preset) = self.selected_prompt_preset() else {
            self.set_status(no_preset_to_delete_status(language));
            return;
        };
        if PromptConfig::delete_user_preset(&self.paths, &preset) {
            self.logs.push(format!(
                "[PromptConfig] 사용자 프리셋 삭제: {}",
                preset.display_name
            ));
            self.refresh_prompt_presets("current");
            self.set_status(preset_deleted_status(language, &preset.display_name));
        } else {
            self.set_status(only_user_presets_delete_status(language));
        }
    }

    fn save_prompt_editor(&mut self) {
        let language = self.language();
        match PromptConfig::save_user_override_document(&self.paths, &self.prompt_editor_text) {
            Ok(config) => {
                self.prompt_editor_text = config.editable_document();
                self.refresh_prompt_presets("current");
                let path = self.paths.prompt_override_path();
                self.logs.push(format!(
                    "[PromptConfig] 사용자 프롬프트 설정 저장: {}",
                    path.display()
                ));
                self.set_status(prompt_saved_status(language, &path.display().to_string()));
            }
            Err(error) => {
                self.logs
                    .push(format!("[PromptConfig] 프롬프트 저장 실패: {error}"));
                self.set_status(prompt_save_failed_status(language, &error.to_string()));
            }
        }
    }

    fn apply_gui_theme(&mut self, ctx: &egui::Context) {
        self.draft.theme_mode = normalize_theme_mode(&self.draft.theme_mode);
        self.draft.ui_language = normalize_ui_language(&self.draft.ui_language);
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
            let language = self.language();
            self.cli_start_after_setup = false;
            self.set_status(cli_ready_starting_status(
                language,
                &self.draft.gemini_cli_model,
            ));
            self.enter_server_mode();
        } else {
            let language = self.language();
            self.cli_start_after_setup = false;
            self.page = AppPage::Runtime;
            self.set_status(cli_setup_not_completed_status(language));
        }
    }

    fn start_mode_from_home(&mut self, mode: TranslationMode) {
        let language = self.language();
        self.selected_mode = mode;
        self.draft.last_translation_mode = translation_mode_setting_value(mode).to_owned();
        if mode == TranslationMode::GeminiCli && !self.selected_mode_ready() {
            self.page = AppPage::Runtime;
            self.cli_start_after_setup = true;
            self.set_status(cli_cache_missing_start_setup_status(language));
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
            self.set_status(mode_start_unavailable_status(self.language()));
            return;
        }
        if !self.selected_mode_ready() {
            self.log_missing_cli_verification_cache();
            self.page = AppPage::Runtime;
            self.set_status(cli_model_verification_required_status(self.language()));
            return;
        }
        self.save_settings();
        self.stop_server();
        *self.exit_action.lock() = GuiExitAction::ServerMode(self.selected_mode);
        let language = self.language();
        let backend_label = if self.draft.run_in_tray {
            tray_backend_label(language)
        } else {
            console_backend_label(language)
        };
        self.logs.push(format!(
            "[GUI] 서버 모드 전환: {}. GUI를 닫고 {}만 유지합니다.",
            self.selected_mode.label(),
            backend_label
        ));
        self.set_status(server_mode_started_status(
            language,
            self.selected_mode.label(),
            backend_label,
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
        let language = self.language();
        if self.cli_setup_inflight.swap(true, Ordering::SeqCst) {
            self.logs
                .push("[GeminiCli] CLI 초기설정 확인이 이미 진행 중입니다.");
            self.set_status(cli_setup_already_running_status(language));
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
        self.set_status(cli_status_checking_status(language));
        Self::set_cli_setup_panel(
            &panel,
            cli_phase_checking_environment(language),
            checking_label(language),
            "",
        );
        self.runtime.spawn(async move {
            let status = tokio::task::spawn_blocking(cli_setup::get_environment_status)
                .await
                .unwrap_or_default();
            let summary = status.summary();
            Self::set_cli_setup_panel(
                &panel,
                cli_phase_environment_done(language),
                summary.clone(),
                "",
            );
            logs.push(format!("[GeminiCli] CLI 초기설정 환경 진단:\n{}", summary));

            let result = if status.has_gemini() {
                Self::set_cli_setup_panel(
                    &panel,
                    cli_phase_launching_login(language),
                    summary.clone(),
                    "",
                );
                cli_setup::launch_login_terminal()
            } else {
                Self::set_cli_setup_panel(
                    &panel,
                    cli_phase_launching_install(language),
                    summary.clone(),
                    "",
                );
                cli_setup::launch_install_terminal()
            };
            match result {
                Ok(()) => {
                    logs.push("[GeminiCli] CLI 초기설정 PowerShell 창을 열었습니다.");
                    Self::set_cli_setup_panel(
                        &panel,
                        cli_phase_waiting_login(language),
                        summary.clone(),
                        cli_waiting_login_detail(language),
                    );
                }
                Err(error) => {
                    logs.push(format!("[GeminiCli] CLI 초기설정 실행 실패: {error}"));
                    Self::set_cli_setup_panel(
                        &panel,
                        cli_phase_setup_launch_failed(language),
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
                            cli_phase_verifying_models(language),
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
                                cli_phase_setup_done(language),
                                summary.clone(),
                                verified_models_detail(language, &cached),
                            );
                        } else {
                            Self::set_cli_setup_panel(
                                &panel,
                                cli_phase_model_verification_failed(language),
                                summary.clone(),
                                cli_no_available_models_detail(language),
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
                                cli_phase_waiting_login(language),
                                status.summary(),
                                crate::logging::summarize_text(&last_error, 220),
                            );
                        }
                        if std::time::Instant::now() >= deadline {
                            logs.push("[GeminiCli] CLI 초기설정 자동 감지 시간이 초과되었습니다.");
                            Self::set_cli_setup_panel(
                                &panel,
                                cli_phase_setup_timeout(language),
                                status.summary(),
                                cli_setup_timeout_detail(language),
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
        let language = self.language();
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
            let button = egui::Button::new(RichText::new(page.nav_title(language)).size(14.0))
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
                ui.add(
                    egui::Label::new(RichText::new(&self.status_message).color(muted_text()))
                        .wrap(),
                );
            });
        });
    }

    fn draw_header(&mut self, ui: &mut egui::Ui) {
        let language = self.language();
        self.section_anchor(ui, AppPage::Home);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(header_title(language))
                        .size(30.0)
                        .strong()
                        .color(text()),
                );
                ui.label(RichText::new(header_subtitle(language)).color(muted_text()));
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut theme_changed = false;
                let selected_theme = normalize_theme_mode(&self.draft.theme_mode);
                egui::ComboBox::from_id_salt("theme_combo")
                    .width(132.0)
                    .selected_text(theme_display(language, &selected_theme))
                    .show_ui(ui, |ui| {
                        for theme in ["System", "Light", "Dark"] {
                            theme_changed |= ui
                                .selectable_value(
                                    &mut self.draft.theme_mode,
                                    theme.to_owned(),
                                    theme_display(language, theme),
                                )
                                .changed();
                        }
                    });
                ui.label(RichText::new(theme_label(language)).color(muted_text()));

                if theme_changed {
                    self.draft.theme_mode = normalize_theme_mode(&self.draft.theme_mode);
                    self.apply_gui_theme(ui.ctx());
                    self.save_settings();
                    self.set_status(theme_saved_status(self.language()));
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
        let language = self.language();
        let mut selected = None;
        ui.columns(3, |columns| {
            if mode_start_card(
                &mut columns[0],
                "Gemini WebView",
                mode_caption_webview(language),
                "G",
                accent_light(),
                language,
            ) {
                selected = Some(TranslationMode::WebView);
            }
            if mode_start_card(
                &mut columns[1],
                "Antigravity CLI",
                antigravity_priority_caption(language),
                "CLI",
                success(),
                language,
            ) {
                selected = Some(TranslationMode::GeminiCli);
            }
            if mode_start_card(
                &mut columns[2],
                "ChatGPT WebView",
                mode_caption_chatgpt(language),
                "C",
                Color32::from_rgb(2, 132, 199),
                language,
            ) {
                selected = Some(TranslationMode::ChatGptWebView);
            }
        });

        if let Some(mode) = selected {
            self.start_mode_from_home(mode);
        }
    }

    fn draw_runtime_section(&mut self, ui: &mut egui::Ui) {
        let language = self.language();
        self.section_anchor(ui, AppPage::Runtime);
        draw_card(ui, "WebView / CLI", |ui| {
            ui.columns(2, |columns| {
                let ui = &mut columns[0];
                ui.add_space(4.0);
                let run_in_tray_response =
                    toggle_row(ui, &mut self.draft.run_in_tray, run_in_tray_label(language))
                        .on_hover_text(run_in_tray_hover(language));
                if run_in_tray_response.changed() && self.draft.start_with_windows {
                    self.draft.run_in_tray = true;
                    self.set_status(startup_requires_tray_status(language));
                }
                let start_response = toggle_row(
                    ui,
                    &mut self.draft.start_with_windows,
                    windows_startup_label(language),
                );
                if start_response.changed() {
                    if self.draft.start_with_windows {
                        self.draft.run_in_tray = true;
                    }
                    self.save_settings();
                    self.set_status(if self.draft.start_with_windows {
                        windows_startup_enabled_status(language)
                    } else {
                        windows_startup_disabled_status(language)
                    });
                }
                toggle_row(ui, &mut self.draft.verbose_logs, debug_logs_label(language));
                toggle_row(
                    ui,
                    &mut self.draft.web_view_pure_quality_mode,
                    webview_quality_label(language),
                );
                toggle_row(
                    ui,
                    &mut self.draft.web_view_raw_mode,
                    webview_raw_label(language),
                );
                toggle_row(
                    ui,
                    &mut self.draft.maximum_usage_mode_enabled,
                    maximum_usage_label(language),
                );
                ui.add_space(18.0);
                egui::Grid::new("webview_runtime_grid")
                    .num_columns(3)
                    .spacing([14.0, 10.0])
                    .show(ui, |ui| {
                        ui.label(refresh_every_label(language));
                        ui.add_sized(
                            [90.0, 32.0],
                            egui::DragValue::new(&mut self.draft.web_view_refresh_every_requests)
                                .range(1..=200)
                                .speed(1),
                        );
                        ui.label(RichText::new(requests_unit_label(language)).color(muted_text()));
                        ui.end_row();

                        ui.label(idle_refresh_label(language));
                        ui.add_sized(
                            [90.0, 32.0],
                            egui::DragValue::new(&mut self.draft.web_view_idle_refresh_seconds)
                                .range(10..=600)
                                .speed(1),
                        );
                        ui.label(RichText::new(seconds_unit_label(language)).color(muted_text()));
                        ui.end_row();

                        ui.label(webview_instance_count_label(language));
                        ui.add_sized(
                            [90.0, 32.0],
                            egui::DragValue::new(&mut self.draft.web_view_instance_count)
                                .range(1..=3)
                                .speed(1),
                        );
                        ui.label(
                            RichText::new(webview_instance_count_hint(language))
                                .size(12.0)
                                .color(muted_text()),
                        );
                        ui.end_row();
                    });

                ui.add_space(14.0);
                let reset_busy = self.webview_profile_reset_inflight.load(Ordering::SeqCst);
                if ui
                    .add_enabled(
                        !reset_busy,
                        egui::Button::new(if reset_busy {
                            webview_profile_resetting_button(language)
                        } else {
                            reset_webview_profile_button(language)
                        }),
                    )
                    .clicked()
                {
                    self.confirm_webview_profile_reset = true;
                }
                ui.add_space(8.0);
                if ui
                    .add_sized(
                        [190.0, 32.0],
                        egui::Button::new(mort_preset_guide_button(language)),
                    )
                    .clicked()
                {
                    self.show_mort_preset_guide = true;
                }

                let ui = &mut columns[1];
                ui.add_space(4.0);
                let before_model = self.draft.gemini_cli_model.clone();
                ui.label(RichText::new(cli_model_label(language)).color(text()));
                egui::ComboBox::from_id_salt("cli_model_combo")
                    .width(ui.available_width())
                    .selected_text(self.draft.gemini_cli_model.clone())
                    .show_ui(ui, |ui| {
                        for model in model_catalog::cli_models_for_current_provider() {
                            ui.selectable_value(
                                &mut self.draft.gemini_cli_model,
                                model.id.to_owned(),
                                format!("{} ({})", model.display_name, model.id),
                            );
                        }
                    });
                if before_model != self.draft.gemini_cli_model {
                    self.ensure_thinking_for_selected_model();
                    self.set_status(cli_model_changed_status(language));
                }

                let thinking_options =
                    model_catalog::thinking_options_for_model(&self.draft.gemini_cli_model);
                ui.add_space(8.0);
                ui.label(RichText::new(wrapper_thinking_label(language)).color(text()));
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
                    fast_wrapper_label(language),
                )
                .on_hover_text(fast_wrapper_hover(language));
                if before_fast != self.draft.gemini_cli_use_fast_wrapper {
                    self.draft.clear_gemini_cli_verification_cache();
                    self.settings.write().clear_gemini_cli_verification_cache();
                    self.set_status(fast_wrapper_cache_cleared_status(language));
                }
                ui.add_space(8.0);
                let setting_up = self.cli_setup_inflight.load(Ordering::SeqCst);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !setting_up,
                            egui::Button::new(if setting_up {
                                cli_setup_checking_button(language)
                            } else {
                                cli_setup_button(language)
                            }),
                        )
                        .clicked()
                    {
                        self.launch_cli_setup_flow();
                    }
                    if ui
                        .button(environment_diagnostics_button(language))
                        .clicked()
                    {
                        self.refresh_cli_setup_environment();
                    }
                    ui.label(RichText::new(cli_warranty_caption(language)).color(muted_text()));
                });
                ui.add_space(8.0);
                self.draw_cli_cache_summary(ui);
                ui.add_space(8.0);
                self.draw_cli_setup_panel(ui);
            });
        });
    }

    fn draw_proxy_section(&mut self, ui: &mut egui::Ui) {
        let language = self.language();
        self.section_anchor(ui, AppPage::Proxy);
        draw_card(ui, proxy_section_title(language), |ui| {
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
                openai_proxy_label(language),
            );
            toggle_row(
                ui,
                &mut self.draft.gemini_proxy_enabled,
                gemini_proxy_label(language),
            );
            toggle_row(
                ui,
                &mut self.draft.require_proxy_api_key,
                local_api_key_required_label(language),
            );

            ui.add_space(12.0);
            self.draw_local_api_keys(ui, language);
        });
    }

    fn draw_local_api_keys(&mut self, ui: &mut egui::Ui, language: UiLanguage) {
        self.sync_local_api_key_selection();
        let keys = self.draft.local_api_keys.clone();

        enum KeyAction {
            None,
            Select(String),
            Add,
            Delete,
            CopySelected,
            CopyAll,
        }
        let mut action = KeyAction::None;

        egui::Grid::new("api_key_grid")
            .num_columns(2)
            .spacing([14.0, 10.0])
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.set_min_width(150.0);
                    ui.label(local_api_key_label(language));
                    ui.label(
                        RichText::new(local_api_key_all_valid_label(language))
                            .size(12.0)
                            .color(muted_text()),
                    );
                });
                ui.horizontal(|ui| {
                    let button_width = 100.0;
                    let list_width = (ui.available_width() - button_width - 12.0).max(240.0);
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_min_size(egui::vec2(list_width, 108.0));
                        egui::ScrollArea::both()
                            .id_salt("local_api_key_list")
                            .max_height(108.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_min_width(list_width - 18.0);
                                for key in &keys {
                                    let selected = self.selected_local_api_key.as_deref()
                                        == Some(key.as_str());
                                    if ui.selectable_label(selected, key.as_str()).clicked() {
                                        action = KeyAction::Select(key.clone());
                                    }
                                }
                            });
                    });
                    ui.add_space(12.0);
                    ui.vertical(|ui| {
                        if ui
                            .add_sized(
                                [button_width, 32.0],
                                egui::Button::new(add_key_button(language)),
                            )
                            .clicked()
                        {
                            action = KeyAction::Add;
                        }
                        let can_delete =
                            self.selected_local_api_key.is_some() && keys.len() > 1;
                        if ui
                            .add_enabled_ui(can_delete, |ui| {
                                ui.add_sized(
                                    [button_width, 32.0],
                                    egui::Button::new(delete_key_button(language)),
                                )
                            })
                            .inner
                            .clicked()
                        {
                            action = KeyAction::Delete;
                        }
                        let can_copy = self.selected_local_api_key.is_some();
                        if ui
                            .add_enabled_ui(can_copy, |ui| {
                                ui.add_sized(
                                    [button_width, 32.0],
                                    egui::Button::new(copy_selected_button(language)),
                                )
                            })
                            .inner
                            .clicked()
                        {
                            action = KeyAction::CopySelected;
                        }
                        if ui
                            .add_sized(
                                [button_width, 32.0],
                                egui::Button::new(copy_all_button(language)),
                            )
                            .clicked()
                        {
                            action = KeyAction::CopyAll;
                        }
                    });
                });
                ui.end_row();
            });

        match action {
            KeyAction::None => {}
            KeyAction::Select(key) => self.selected_local_api_key = Some(key),
            KeyAction::Add => self.add_local_api_key(language),
            KeyAction::Delete => self.delete_selected_local_api_key(language),
            KeyAction::CopySelected => {
                self.copy_selected_local_api_key(ui.ctx().clone(), language)
            }
            KeyAction::CopyAll => self.copy_all_local_api_keys(ui.ctx().clone(), language),
        }
    }

    /// draft 키 풀에 최소 1개를 보장하고, 선택 항목이 유효하지 않으면 첫 키로 맞춘다.
    fn sync_local_api_key_selection(&mut self) {
        if self.draft.local_api_keys.is_empty() {
            let keys = self.draft.local_api_key_list();
            self.draft.set_local_api_keys(keys);
        }
        let valid = self
            .selected_local_api_key
            .as_ref()
            .map(|sel| self.draft.local_api_keys.iter().any(|key| key == sel))
            .unwrap_or(false);
        if !valid {
            self.selected_local_api_key = self.draft.local_api_keys.first().cloned();
        }
    }

    fn add_local_api_key(&mut self, language: UiLanguage) {
        let new_key = generate_local_api_key();
        let mut keys = self.draft.local_api_keys.clone();
        keys.push(new_key.clone());
        self.draft.set_local_api_keys(keys);
        self.selected_local_api_key = Some(new_key);
        self.save_settings();
        self.set_status(local_api_key_added_status(language));
    }

    fn delete_selected_local_api_key(&mut self, language: UiLanguage) {
        let Some(selected) = self.selected_local_api_key.clone() else {
            return;
        };
        if self.draft.local_api_keys.len() <= 1 {
            return;
        }
        let Some(index) = self
            .draft
            .local_api_keys
            .iter()
            .position(|key| key == &selected)
        else {
            return;
        };
        let mut keys = self.draft.local_api_keys.clone();
        keys.remove(index);
        let next = keys[index.min(keys.len() - 1)].clone();
        self.draft.set_local_api_keys(keys);
        self.selected_local_api_key = Some(next);
        self.save_settings();
        self.set_status(local_api_key_deleted_status(language));
    }

    fn copy_selected_local_api_key(&mut self, ctx: egui::Context, language: UiLanguage) {
        let Some(selected) = self.selected_local_api_key.clone() else {
            return;
        };
        ctx.copy_text(selected.trim().to_owned());
        self.logs.push("[GUI] 선택한 로컬 API 키를 복사했습니다.");
        self.set_status(local_api_key_selected_copied_status(language));
    }

    fn copy_all_local_api_keys(&mut self, ctx: egui::Context, language: UiLanguage) {
        let keys = self.draft.local_api_keys.clone();
        ctx.copy_text(keys.join("\n"));
        self.logs
            .push(format!("[GUI] 로컬 API 키 {}개를 복사했습니다.", keys.len()));
        self.set_status(local_api_key_all_copied_status(language, keys.len()));
    }

    fn draw_stats_section(&mut self, ui: &mut egui::Ui) {
        let language = self.language();
        self.section_anchor(ui, AppPage::Stats);
        card_frame(surface(), border()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(request_stats_title(language))
                        .size(18.0)
                        .strong()
                        .color(text()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(reset_button(language)).clicked() {
                        self.confirm_usage_reset = true;
                    }
                    if ui.button(refresh_button(language)).clicked() {
                        self.set_status(stats_refreshed_status(language));
                    }
                    egui::ComboBox::from_id_salt("usage_period_combo")
                        .width(104.0)
                        .selected_text(period_label(language, self.usage_period))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.usage_period,
                                UsageStatsPeriod::Daily,
                                period_label(language, UsageStatsPeriod::Daily),
                            );
                            ui.selectable_value(
                                &mut self.usage_period,
                                UsageStatsPeriod::Weekly,
                                period_label(language, UsageStatsPeriod::Weekly),
                            );
                            ui.selectable_value(
                                &mut self.usage_period,
                                UsageStatsPeriod::Monthly,
                                period_label(language, UsageStatsPeriod::Monthly),
                            );
                        });
                });
            });
            ui.add_space(14.0);
            self.draw_usage_panel(ui, language);
        });
    }

    fn draw_prompt_section(&mut self, ui: &mut egui::Ui) {
        let language = self.language();
        self.section_anchor(ui, AppPage::Prompt);
        draw_card(ui, prompt_ivlyrics_title(language), |ui| {
            toggle_row(
                ui,
                &mut self.draft.mort_cli_raw_mode,
                mort_cli_raw_label(language),
            );
            ui.horizontal(|ui| {
                ui.add_space(22.0);
                ui.label(RichText::new(mort_cli_raw_detail(language)).color(muted_text()));
            });
            ui.add_space(8.0);
            toggle_row(
                ui,
                &mut self.draft.raw_prompt_mode,
                raw_prompt_mode_label(language),
            );
            ui.horizontal(|ui| {
                ui.add_space(22.0);
                ui.label(RichText::new(raw_prompt_mode_detail(language)).color(muted_text()));
            });
            ui.add_space(8.0);
            toggle_row(
                ui,
                &mut self.draft.iv_lyrics_study_cli_direct_enabled,
                ivlyrics_study_cli_label(language),
            );
            ui.horizontal(|ui| {
                ui.add_space(22.0);
                ui.label(RichText::new(ivlyrics_study_cli_detail(language)).color(muted_text()));
            });
            ui.add_space(8.0);
            toggle_row(
                ui,
                &mut self.draft.iv_lyrics_auto_prompt_selection_enabled,
                ivlyrics_auto_prompt_label(language),
            );
            ui.horizontal(|ui| {
                ui.add_space(22.0);
                ui.label(RichText::new(ivlyrics_auto_prompt_detail(language)).color(muted_text()));
            });
            ui.add_space(8.0);
            toggle_row(
                ui,
                &mut self.draft.iv_lyrics_phonetic_use_cli_wrapper_enabled,
                ivlyrics_phonetic_cli_label(language),
            );
            ui.horizontal(|ui| {
                ui.add_space(22.0);
                ui.label(RichText::new(ivlyrics_phonetic_cli_detail(language)).color(muted_text()));
            });

            ui.add_space(16.0);
            compact_card_frame(surface_raised(), border()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(prompt_current_title(language))
                                .size(16.0)
                                .strong()
                                .color(text()),
                        );
                        ui.label(
                            RichText::new(saved_path_detail(
                                language,
                                &self.paths.prompt_override_path().display().to_string(),
                            ))
                            .size(12.0)
                            .color(muted_text()),
                        );
                        ui.label(
                            RichText::new(prompt_editor_detail(language))
                                .size(12.0)
                                .color(muted_text()),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                        if ui
                            .add_sized([72.0, 32.0], egui::Button::new(save_button(language)))
                            .clicked()
                        {
                            self.save_prompt_editor();
                        }
                        if ui
                            .add_sized([86.0, 32.0], egui::Button::new(reload_button(language)))
                            .clicked()
                        {
                            self.reload_prompt_editor();
                        }
                        if ui
                            .add_sized(
                                [118.0, 32.0],
                                egui::Button::new(load_defaults_button(language)),
                            )
                            .clicked()
                        {
                            self.load_default_prompt_editor();
                        }
                    });
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(preset_label(language)).color(muted_text()));
                    let selected_label = self.selected_prompt_preset_label();
                    let presets = self.prompt_presets.clone();
                    let mut preset_changed = false;
                    egui::ComboBox::from_id_salt("prompt_preset_combo")
                        .width((ui.available_width() - 210.0).max(260.0))
                        .selected_text(selected_label)
                        .show_ui(ui, |ui| {
                            for preset in presets {
                                preset_changed |= ui
                                    .selectable_value(
                                        &mut self.selected_prompt_preset_id,
                                        preset.id,
                                        preset.display_name,
                                    )
                                    .changed();
                            }
                        });
                    if preset_changed {
                        self.load_selected_prompt_preset();
                    }
                    if ui
                        .add_sized(
                            [112.0, 32.0],
                            egui::Button::new(save_preset_button(language)),
                        )
                        .clicked()
                    {
                        self.save_prompt_preset();
                    }
                    let delete_enabled = self
                        .selected_prompt_preset()
                        .map(|preset| preset.is_user_preset)
                        .unwrap_or(false);
                    if ui
                        .add_enabled_ui(delete_enabled, |ui| {
                            ui.add_sized(
                                [68.0, 32.0],
                                egui::Button::new(delete_preset_button(language)),
                            )
                        })
                        .inner
                        .clicked()
                    {
                        self.delete_selected_prompt_preset();
                    }
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
        });
    }

    fn draw_cli_cache_summary(&mut self, ui: &mut egui::Ui) {
        let language = self.language();
        let settings = self.settings.read().clone();
        let cached = settings.cached_gemini_cli_model_options();
        if cached.is_empty() {
            ui.add(
                egui::Label::new(
                    RichText::new(cli_cache_empty_detail(language)).color(muted_text()),
                )
                .wrap(),
            );
        } else {
            let text = verified_models_detail(
                language,
                &cached
                    .iter()
                    .map(|model| model.id)
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            draw_single_line_scroll_text(ui, "cli_cache_summary", &text, muted_text());
        }
    }

    fn draw_cli_setup_panel(&self, ui: &mut egui::Ui) {
        let language = self.language();
        let state = self.cli_setup_panel.read().clone();
        ui.separator();
        ui.label(
            RichText::new(cli_setup_status_title(language))
                .strong()
                .color(text()),
        );
        ui.label(RichText::new(state.phase).color(accent_light()));
        draw_multiline_scroll_text(ui, "cli_setup_summary", &state.summary);
        if !state.detail.trim().is_empty() {
            ui.add(egui::Label::new(RichText::new(state.detail).color(muted_text())).wrap());
        }
    }

    fn draw_usage_panel(&mut self, ui: &mut egui::Ui, language: UiLanguage) {
        let usage = UsageMetrics::new(&self.paths, self.logs.clone());
        let snapshot = usage.snapshot();
        let buckets = usage.buckets(self.usage_period);

        ui.columns(4, |columns| {
            metric_tile(
                &mut columns[0],
                total_requests_label(language),
                &format_count(snapshot.total_requests),
                &success_rate_caption(language, snapshot.success_rate()),
                text(),
            );
            metric_tile(
                &mut columns[1],
                success_label(language),
                &format_count(snapshot.succeeded_requests),
                completed_requests_caption(language),
                success(),
            );
            metric_tile(
                &mut columns[2],
                failed_cancelled_label(language),
                &format!(
                    "{} / {}",
                    format_count(snapshot.failed_requests),
                    format_count(snapshot.cancelled_requests)
                ),
                errors_stale_caption(language),
                danger(),
            );
            metric_tile(
                &mut columns[3],
                input_output_tokens_label(language),
                &format!(
                    "{} / {}",
                    format_count(snapshot.input_tokens),
                    format_count(snapshot.successful_output_tokens)
                ),
                local_estimate_caption(language),
                accent_light(),
            );
        });
        ui.add_space(12.0);

        draw_usage_chart(ui, &buckets, language);
        ui.add_space(12.0);

        draw_usage_detail(ui, &self.paths, &snapshot, &buckets, language);
        if !snapshot.last_failure.is_empty() {
            ui.add_space(8.0);
            ui.label(
                RichText::new(last_failure_status(language, &snapshot.last_failure))
                    .color(warning()),
            );
        }
    }

    fn draw_usage_reset_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_usage_reset {
            return;
        }

        let language = self.language();
        let mut close = false;
        egui::Window::new(reset_stats_window_title(language))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(reset_stats_confirm(language));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(reset_button(language)).clicked() {
                        UsageMetrics::new(&self.paths, self.logs.clone()).reset();
                        self.set_status(stats_reset_status(language));
                        close = true;
                    }
                    if ui.button(cancel_button(language)).clicked() {
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

        let language = self.language();
        let mut close = false;
        egui::Window::new(cli_model_verified_window_title(language))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.label(
                    RichText::new(cli_model_verified_heading(language))
                        .strong()
                        .color(success()),
                );
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(message).color(text())).wrap());
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button(ok_button(language)).clicked() {
                        close = true;
                    }
                    if self.cli_start_after_setup {
                        ui.label(
                            RichText::new(close_starts_cli_note(language)).color(muted_text()),
                        );
                    }
                });
            });

        if close {
            self.cli_model_verified_notice = None;
        }
    }

    fn draw_update_check_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_update_check_dialog {
            return;
        }

        let state = self.update_check_panel.read().clone();
        let checking = self.update_check_inflight.load(Ordering::SeqCst);
        let downloading = self.update_download_inflight.load(Ordering::SeqCst);
        let mut close = false;
        let mut retry = false;
        let mut download = false;
        let language = self.language();
        egui::Window::new(update_check_window_title(language))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_min_width(420.0);
                let phase_color = if downloading {
                    accent_light()
                } else if state.update_available {
                    warning()
                } else if state.finished {
                    success()
                } else {
                    accent_light()
                };
                ui.label(
                    RichText::new(&state.phase)
                        .size(18.0)
                        .strong()
                        .color(phase_color),
                );
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(&state.summary).color(text())).wrap());
                if !state.detail.trim().is_empty() {
                    ui.add_space(8.0);
                    draw_multiline_scroll_text(ui, "update_check_detail", &state.detail);
                }
                if !state.release_url.trim().is_empty() {
                    ui.add_space(8.0);
                    ui.hyperlink_to(open_github_release_link(language), &state.release_url);
                }
                if !state.primary_asset_download_url.trim().is_empty() {
                    ui.hyperlink_to(
                        download_link_label(language, &state.primary_asset_name),
                        &state.primary_asset_download_url,
                    );
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !checking && !downloading,
                            egui::Button::new(if checking {
                                checking_label(language)
                            } else {
                                recheck_button(language)
                            }),
                        )
                        .clicked()
                    {
                        retry = true;
                    }
                    if !state.primary_asset_download_url.trim().is_empty()
                        && ui
                            .add_enabled(
                                !checking && !downloading,
                                egui::Button::new(if downloading {
                                    downloading_label(language)
                                } else {
                                    download_and_run_button(language)
                                }),
                            )
                            .clicked()
                    {
                        download = true;
                    }
                    if !state.release_url.trim().is_empty()
                        && ui.button(copy_release_link_button(language)).clicked()
                    {
                        ui.ctx().copy_text(state.release_url.clone());
                        self.set_status(release_link_copied_status(language));
                    }
                    if ui.button(close_button(language)).clicked() {
                        close = true;
                    }
                });
            });

        if retry {
            self.launch_update_check();
        }
        if download {
            self.launch_update_download();
        }
        if close {
            self.show_update_check_dialog = false;
        }
    }

    fn draw_developer_info_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_developer_info {
            return;
        }

        let language = self.language();
        let mut close = false;
        egui::Window::new(developer_info_window_title(language))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.label(RichText::new("ruster").size(20.0).strong().color(text()));
                ui.add_space(8.0);
                ui.label(RichText::new(version_label(language, APP_VERSION)).color(text()));
                ui.label(RichText::new(developer_label(language, DEVELOPER_NAME)).color(text()));
                ui.hyperlink_to(DEVELOPER_GITHUB, DEVELOPER_GITHUB);
                ui.hyperlink_to(
                    format!("Telegram: {DEVELOPER_TELEGRAM} ({DEVELOPER_TELEGRAM_URL})"),
                    DEVELOPER_TELEGRAM_URL,
                );
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button(copy_github_button(language)).clicked() {
                        ui.ctx().copy_text(DEVELOPER_GITHUB.to_owned());
                        self.set_status(github_copied_status(language));
                    }
                    if ui.button(copy_telegram_button(language)).clicked() {
                        ui.ctx().copy_text(DEVELOPER_TELEGRAM_URL.to_owned());
                        self.set_status(telegram_copied_status(language));
                    }
                    if ui.button(close_button(language)).clicked() {
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

fn translation_mode_setting_value(mode: TranslationMode) -> &'static str {
    match mode {
        TranslationMode::WebView => "WebView",
        TranslationMode::GeminiCli => "GeminiCli",
        TranslationMode::ChatGptWebView => "ChatGptWebView",
    }
}

fn tr(language: UiLanguage, ko: &'static str, en: &'static str) -> &'static str {
    match language {
        UiLanguage::Korean => ko,
        UiLanguage::English => en,
    }
}

fn page_nav_title(language: UiLanguage, page: AppPage) -> &'static str {
    match page {
        AppPage::Home => tr(language, "홈", "Home"),
        AppPage::Runtime => tr(language, "실행 설정", "Runtime"),
        AppPage::Proxy => tr(language, "프록시", "Proxy"),
        AppPage::Stats => tr(language, "통계", "Stats"),
        AppPage::Prompt => tr(language, "프롬프트", "Prompts"),
    }
}

fn is_cli_idle_phase(phase: &str) -> bool {
    phase == cli_phase_idle(UiLanguage::Korean) || phase == cli_phase_idle(UiLanguage::English)
}

fn is_cli_setup_done_phase(phase: &str) -> bool {
    phase == cli_phase_setup_done(UiLanguage::Korean)
        || phase == cli_phase_setup_done(UiLanguage::English)
}

fn is_update_idle_phase(phase: &str) -> bool {
    phase == update_phase_idle(UiLanguage::Korean)
        || phase == update_phase_idle(UiLanguage::English)
}

fn status_ready(language: UiLanguage) -> &'static str {
    tr(
        language,
        "gemini와 chatgpt를 지원합니다.",
        "Gemini and ChatGPT are supported.",
    )
}

fn checking_label(language: UiLanguage) -> &'static str {
    tr(language, "확인 중...", "Checking...")
}

fn cli_phase_idle(language: UiLanguage) -> &'static str {
    tr(language, "대기", "Idle")
}

fn cli_summary_before_diagnostics(language: UiLanguage) -> &'static str {
    tr(language, "환경 진단 전", "Environment check not run")
}

fn cli_detail_before_diagnostics(language: UiLanguage) -> &'static str {
    tr(
        language,
        "Gemini CLI 초기설정을 실행하면 설치, 로그인, 모델 검증 상태가 여기에 표시됩니다.",
        "Run Gemini CLI setup to show installation, login, and model verification status here.",
    )
}

fn cli_phase_checking_environment(language: UiLanguage) -> &'static str {
    tr(language, "환경 진단 중", "Checking environment")
}

fn cli_phase_environment_done(language: UiLanguage) -> &'static str {
    tr(language, "환경 진단 완료", "Environment check complete")
}

fn cli_phase_launching_login(language: UiLanguage) -> &'static str {
    tr(
        language,
        "로그인/온보딩 창 실행",
        "Opening login/onboarding",
    )
}

fn cli_phase_launching_install(language: UiLanguage) -> &'static str {
    tr(language, "설치/로그인 창 실행", "Opening install/login")
}

fn cli_phase_waiting_login(language: UiLanguage) -> &'static str {
    tr(
        language,
        "로그인/온보딩 대기 중",
        "Waiting for login/onboarding",
    )
}

fn cli_phase_setup_launch_failed(language: UiLanguage) -> &'static str {
    tr(language, "초기설정 실행 실패", "Setup launch failed")
}

fn cli_phase_verifying_models(language: UiLanguage) -> &'static str {
    tr(language, "모델 검증 중", "Verifying models")
}

fn cli_phase_setup_done(language: UiLanguage) -> &'static str {
    tr(language, "초기설정 완료", "Setup complete")
}

fn cli_phase_model_verification_failed(language: UiLanguage) -> &'static str {
    tr(language, "모델 검증 실패", "Model verification failed")
}

fn cli_phase_setup_timeout(language: UiLanguage) -> &'static str {
    tr(language, "초기설정 시간 초과", "Setup timed out")
}

fn cli_waiting_login_detail(language: UiLanguage) -> &'static str {
    tr(
        language,
        "PowerShell 창에서 설치 또는 로그인을 완료하면 자동 감지를 계속합니다.",
        "Complete installation or login in the PowerShell window; detection will continue automatically.",
    )
}

fn cli_setup_timeout_detail(language: UiLanguage) -> &'static str {
    tr(
        language,
        "10분 안에 CLI 준비 상태를 확인하지 못했습니다.",
        "CLI readiness was not confirmed within 10 minutes.",
    )
}

fn cli_no_available_models_detail(language: UiLanguage) -> &'static str {
    tr(
        language,
        "사용 가능한 CLI 모델을 확인하지 못했습니다.",
        "No available CLI models were found.",
    )
}

fn cli_models_checked_detail(language: UiLanguage) -> &'static str {
    tr(
        language,
        "사용 가능한 Gemini CLI 모델을 확인했습니다.",
        "Available Gemini CLI models were verified.",
    )
}

fn verified_models_detail(language: UiLanguage, models: &str) -> String {
    match language {
        UiLanguage::Korean => format!("검증된 모델: {models}"),
        UiLanguage::English => format!("Verified models: {models}"),
    }
}

fn cli_model_verified_message(language: UiLanguage, detail: &str) -> String {
    match language {
        UiLanguage::Korean => format!("모델 확인이 완료되었습니다.\n\n{detail}"),
        UiLanguage::English => format!("Model verification is complete.\n\n{detail}"),
    }
}

fn cli_model_verified_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "Gemini CLI 모델 확인이 완료되었습니다.",
        "Gemini CLI model verification is complete.",
    )
}

fn update_phase_idle(language: UiLanguage) -> &'static str {
    tr(language, "대기", "Idle")
}

fn update_phase_checking(language: UiLanguage) -> &'static str {
    tr(language, "확인 중", "Checking")
}

fn update_phase_done(language: UiLanguage) -> &'static str {
    tr(language, "확인 완료", "Check complete")
}

fn update_phase_failed(language: UiLanguage) -> &'static str {
    tr(language, "확인 실패", "Check failed")
}

fn update_phase_downloading(language: UiLanguage) -> &'static str {
    tr(language, "다운로드 중", "Downloading")
}

fn update_phase_launched(language: UiLanguage) -> &'static str {
    tr(language, "실행 완료", "Launched")
}

fn update_phase_launch_failed(language: UiLanguage) -> &'static str {
    tr(language, "실행 실패", "Launch failed")
}

fn current_version_summary(language: UiLanguage) -> String {
    match language {
        UiLanguage::Korean => format!("현재 버전: v{APP_VERSION}"),
        UiLanguage::English => format!("Current version: v{APP_VERSION}"),
    }
}

fn update_detail_uses_latest_release(language: UiLanguage) -> &'static str {
    tr(
        language,
        "GitHub 최신 릴리스 기준으로 확인합니다.",
        "Checks against the latest GitHub release.",
    )
}

fn update_detail_checking_latest_release(language: UiLanguage) -> &'static str {
    tr(
        language,
        "GitHub 최신 릴리스를 조회하고 있습니다.",
        "Checking the latest GitHub release.",
    )
}

fn update_summary_available(
    language: UiLanguage,
    latest_tag: &str,
    current_version: &str,
) -> String {
    match language {
        UiLanguage::Korean => format!("새 버전이 있습니다: {latest_tag} (현재 v{current_version})"),
        UiLanguage::English => {
            format!("A new version is available: {latest_tag} (current v{current_version})")
        }
    }
}

fn update_summary_current(language: UiLanguage, current_version: &str, latest_tag: &str) -> String {
    match language {
        UiLanguage::Korean => {
            format!("최신 버전입니다: 현재 v{current_version} / GitHub {latest_tag}")
        }
        UiLanguage::English => {
            format!("You are up to date: current v{current_version} / GitHub {latest_tag}")
        }
    }
}

fn update_detail_success(language: UiLanguage, latest_version: &str, published: &str) -> String {
    match language {
        UiLanguage::Korean => {
            format!("GitHub 최신 버전: {latest_version}\n릴리스 날짜: {published}")
        }
        UiLanguage::English => {
            format!("Latest GitHub version: {latest_version}\nRelease date: {published}")
        }
    }
}

fn update_summary_failed(language: UiLanguage) -> &'static str {
    tr(
        language,
        "GitHub 최신 릴리스를 확인하지 못했습니다.",
        "Could not check the latest GitHub release.",
    )
}

fn update_download_summary(language: UiLanguage, asset: &str) -> String {
    match language {
        UiLanguage::Korean => format!("릴리스 EXE 다운로드 후 실행 중: {asset}"),
        UiLanguage::English => format!("Downloading and launching release EXE: {asset}"),
    }
}

fn update_download_detail(language: UiLanguage) -> &'static str {
    tr(
        language,
        "GitHub 릴리스 EXE를 임시 폴더에 저장한 뒤 실행합니다.",
        "The GitHub release EXE will be saved to a temporary folder and launched.",
    )
}

fn update_launch_summary(language: UiLanguage, asset: &str) -> String {
    match language {
        UiLanguage::Korean => format!("다운로드한 EXE를 실행했습니다: {asset}"),
        UiLanguage::English => format!("Launched downloaded EXE: {asset}"),
    }
}

fn update_launch_failed_summary(language: UiLanguage) -> &'static str {
    tr(
        language,
        "릴리스 EXE 다운로드 또는 실행에 실패했습니다.",
        "Failed to download or launch the release EXE.",
    )
}

fn header_title(language: UiLanguage) -> &'static str {
    tr(
        language,
        "번역 모드를 선택하세요",
        "Choose a Translation Mode",
    )
}

fn header_subtitle(language: UiLanguage) -> &'static str {
    tr(
        language,
        "호환 API, OpenAI, Gemini 프록시 설정은 ruster 실행 경로에서 유지됩니다.",
        "Compatible API, OpenAI, and Gemini proxy settings are kept in the ruster runtime path.",
    )
}

fn language_label(language: UiLanguage) -> &'static str {
    tr(language, "언어", "Language")
}

fn theme_label(language: UiLanguage) -> &'static str {
    tr(language, "테마", "Theme")
}

fn theme_display(language: UiLanguage, theme: &str) -> &'static str {
    match normalize_theme_mode(theme).as_str() {
        "Light" => tr(language, "라이트", "Light"),
        "Dark" => tr(language, "다크", "Dark"),
        _ => tr(language, "시스템", "System"),
    }
}

fn language_saved_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "언어 설정을 저장했습니다.",
        "Language setting saved.",
    )
}

fn theme_saved_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "테마 설정을 저장했습니다.",
        "Theme setting saved.",
    )
}

fn mode_caption_webview(language: UiLanguage) -> &'static str {
    tr(
        language,
        "브라우저 세션 번역",
        "Browser session translation",
    )
}

fn mode_caption_chatgpt(language: UiLanguage) -> &'static str {
    tr(language, "ChatGPT 세션 번역", "ChatGPT session translation")
}

fn cli_warranty_caption(language: UiLanguage) -> &'static str {
    tr(
        language,
        "2026-06-18 이후 보증 불가",
        "Not guaranteed after 2026-06-18",
    )
}

fn run_in_tray_label(language: UiLanguage) -> &'static str {
    tr(language, "트레이 모드로 실행", "Run in tray mode")
}

fn run_in_tray_hover(language: UiLanguage) -> &'static str {
    tr(
        language,
        "서버 모드 실행 중 Windows 알림 영역에서 로그, 통계, WebView 제어, 종료를 처리합니다.",
        "While server mode is running, the Windows notification area handles logs, stats, WebView control, and exit.",
    )
}

fn windows_startup_label(language: UiLanguage) -> &'static str {
    tr(
        language,
        "Windows 시작 시 자동 실행",
        "Run at Windows startup",
    )
}

fn debug_logs_label(language: UiLanguage) -> &'static str {
    tr(language, "디버그 로그", "Debug logs")
}

fn webview_quality_label(language: UiLanguage) -> &'static str {
    tr(
        language,
        "WebView 최고 품질 모드",
        "WebView highest quality mode",
    )
}

fn webview_raw_label(language: UiLanguage) -> &'static str {
    tr(language, "WebView Raw 모드", "WebView raw mode")
}

fn maximum_usage_label(language: UiLanguage) -> &'static str {
    tr(
        language,
        "\u{CD5C}\u{B300} \u{C0AC}\u{C6A9}\u{B7C9} \u{BAA8}\u{B4DC}",
        "Maximum usage mode",
    )
}

fn webview_parallel_label(language: UiLanguage) -> &'static str {
    tr(
        language,
        "WebView \u{BCD1}\u{B82C} \u{CC98}\u{B9AC}",
        "WebView parallel processing",
    )
}

fn webview_instance_count_label(language: UiLanguage) -> &'static str {
    tr(
        language,
        "WebView \u{C778}\u{C2A4}\u{D134}\u{C2A4} \u{C218}",
        "WebView instances",
    )
}

fn webview_instance_count_hint(language: UiLanguage) -> &'static str {
    tr(
        language,
        "provider당 최대 3, 총 6",
        "max 3 per provider, 6 total",
    )
}

fn refresh_every_label(language: UiLanguage) -> &'static str {
    tr(language, "새로고침 주기", "Refresh interval")
}

fn requests_unit_label(language: UiLanguage) -> &'static str {
    tr(language, "요청마다", "requests")
}

fn idle_refresh_label(language: UiLanguage) -> &'static str {
    tr(language, "유휴 새로고침", "Idle refresh")
}

fn seconds_unit_label(language: UiLanguage) -> &'static str {
    tr(language, "초", "sec")
}

fn cli_model_label(language: UiLanguage) -> &'static str {
    tr(language, "CLI 모델", "CLI model")
}

fn wrapper_thinking_label(language: UiLanguage) -> &'static str {
    tr(language, "Wrapper 생각", "Wrapper thinking")
}

fn fast_wrapper_label(language: UiLanguage) -> &'static str {
    tr(language, "Fast wrapper 사용", "Use fast wrapper")
}

fn fast_wrapper_hover(language: UiLanguage) -> &'static str {
    tr(
        language,
        "Code Assist/GEMINI_API_KEY fast path를 먼저 시도하고 실패 시 native Gemini CLI로 fallback합니다.",
        "Tries the Code Assist/GEMINI_API_KEY fast path first, then falls back to native Gemini CLI if it fails.",
    )
}

fn cli_setup_button(language: UiLanguage) -> &'static str {
    tr(language, "CLI 초기설정", "CLI setup")
}

fn cli_setup_checking_button(language: UiLanguage) -> &'static str {
    tr(language, "초기설정 확인 중...", "Checking setup...")
}

fn environment_diagnostics_button(language: UiLanguage) -> &'static str {
    tr(language, "환경 진단", "Diagnostics")
}

fn proxy_section_title(language: UiLanguage) -> &'static str {
    tr(language, "서버 / 프록시", "Server / Proxy")
}

fn openai_proxy_label(language: UiLanguage) -> &'static str {
    tr(
        language,
        "OpenAI 호환 프록시 활성화",
        "Enable OpenAI-compatible proxy",
    )
}

fn gemini_proxy_label(language: UiLanguage) -> &'static str {
    tr(
        language,
        "Gemini 호환 프록시 활성화",
        "Enable Gemini-compatible proxy",
    )
}

fn local_api_key_required_label(language: UiLanguage) -> &'static str {
    tr(
        language,
        "로컬 API 키 인증 요구",
        "Require local API key authentication",
    )
}

fn local_api_key_label(language: UiLanguage) -> &'static str {
    tr(language, "로컬 API 키", "Local API key")
}

fn local_api_key_all_valid_label(language: UiLanguage) -> &'static str {
    tr(language, "전부 유효", "All valid")
}

fn add_key_button(language: UiLanguage) -> &'static str {
    tr(language, "키 추가", "Add key")
}

fn delete_key_button(language: UiLanguage) -> &'static str {
    tr(language, "삭제", "Delete")
}

fn copy_selected_button(language: UiLanguage) -> &'static str {
    tr(language, "선택 복사", "Copy selected")
}

fn copy_all_button(language: UiLanguage) -> &'static str {
    tr(language, "전체 복사", "Copy all")
}

fn request_stats_title(language: UiLanguage) -> &'static str {
    tr(language, "요청 통계", "Request Stats")
}

fn reset_button(language: UiLanguage) -> &'static str {
    tr(language, "초기화", "Reset")
}

fn refresh_button(language: UiLanguage) -> &'static str {
    tr(language, "새로고침", "Refresh")
}

fn period_label(language: UiLanguage, period: UsageStatsPeriod) -> &'static str {
    match period {
        UsageStatsPeriod::Daily => tr(language, "일별", "Daily"),
        UsageStatsPeriod::Weekly => tr(language, "주간", "Weekly"),
        UsageStatsPeriod::Monthly => tr(language, "월별", "Monthly"),
    }
}

fn prompt_ivlyrics_title(language: UiLanguage) -> &'static str {
    tr(language, "프롬프트 / ivLyrics", "Prompts / ivLyrics")
}

fn mort_cli_raw_label(language: UiLanguage) -> &'static str {
    tr(language, "MORT CLI Raw 사용", "Use MORT CLI raw")
}

fn mort_cli_raw_detail(language: UiLanguage) -> &'static str {
    tr(
        language,
        "MORT/root/custom 요청은 WebView 대신 Gemini CLI raw 경로로 처리합니다.",
        "MORT/root/custom requests are handled through the Gemini CLI raw path instead of WebView.",
    )
}

fn raw_prompt_mode_label(language: UiLanguage) -> &'static str {
    tr(language, "Raw Prompt 모드", "Raw prompt mode")
}

fn raw_prompt_mode_detail(language: UiLanguage) -> &'static str {
    tr(
        language,
        "호환 API/루트 요청 본문을 번역 래핑 없이 그대로 보냅니다.",
        "Sends compatible API/root request bodies as-is without translation wrapping.",
    )
}

fn ivlyrics_study_cli_label(language: UiLanguage) -> &'static str {
    tr(
        language,
        "ivLyrics 학습/퀴즈 CLI 직접 처리",
        "Handle ivLyrics study/quiz directly via CLI",
    )
}

fn ivlyrics_study_cli_detail(language: UiLanguage) -> &'static str {
    tr(
        language,
        "학습/퀴즈 요청은 원본 prompt 그대로 CLI 제한 게이트를 우회해 보내고, CLI 한도류 실패 시 WebView를 직렬 fallback으로 사용합니다.",
        "Study/quiz requests are sent directly to CLI with the original prompt, bypassing limit gates; WebView is used as a serial fallback for CLI limit failures.",
    )
}

fn ivlyrics_phonetic_cli_label(language: UiLanguage) -> &'static str {
    tr(
        language,
        "ivLyrics 발음 CLI wrapper 사용",
        "Use CLI wrapper for ivLyrics pronunciation",
    )
}

fn ivlyrics_phonetic_cli_detail(language: UiLanguage) -> &'static str {
    tr(
        language,
        "발음/문자별 발음 요청을 C# LWT와 같이 CLI wrapper 경로로 직접 처리합니다.",
        "Routes pronunciation/character-pronunciation requests directly through the CLI wrapper path, matching C# LWT.",
    )
}

fn ivlyrics_auto_prompt_label(language: UiLanguage) -> &'static str {
    tr(
        language,
        "ivLyrics 번역 자동 프롬프트 판정",
        "Auto-detect ivLyrics translation prompt",
    )
}

fn ivlyrics_auto_prompt_detail(language: UiLanguage) -> &'static str {
    tr(
        language,
        "ivLyrics 번역 요청을 A/C/D/E 유형으로 판정해 내장 프리셋과 추가 지시를 자동 선택합니다.",
        "Classifies ivLyrics translation requests into A/C/D/E types and automatically selects built-in presets and extra instructions.",
    )
}

fn prompt_editor_title(language: UiLanguage) -> &'static str {
    tr(language, "프롬프트 편집", "Prompt Editor")
}

fn prompt_editor_detail(language: UiLanguage) -> &'static str {
    tr(
        language,
        "프롬프트 문자열을 줄바꿈 그대로 편집합니다. 저장 시 prompts.json으로 변환됩니다.",
        "Edit prompt strings with line breaks preserved. Saving converts them to prompts.json.",
    )
}

fn save_button(language: UiLanguage) -> &'static str {
    tr(language, "저장", "Save")
}

fn reload_button(language: UiLanguage) -> &'static str {
    tr(language, "다시 읽기", "Reload")
}

fn load_defaults_button(language: UiLanguage) -> &'static str {
    tr(language, "기본값 불러오기", "Load defaults")
}

fn preset_label(language: UiLanguage) -> &'static str {
    tr(language, "프리셋", "Preset")
}

fn load_button(language: UiLanguage) -> &'static str {
    tr(language, "불러오기", "Load")
}

fn save_preset_button(language: UiLanguage) -> &'static str {
    tr(language, "프리셋 저장", "Save preset")
}

fn delete_preset_button(language: UiLanguage) -> &'static str {
    tr(language, "프리셋 삭제", "Delete preset")
}

fn cli_setup_status_title(language: UiLanguage) -> &'static str {
    tr(language, "CLI 초기설정 상태", "CLI Setup Status")
}

fn total_requests_label(language: UiLanguage) -> &'static str {
    tr(language, "총 요청", "Total Requests")
}

fn success_label(language: UiLanguage) -> &'static str {
    tr(language, "성공", "Success")
}

fn failed_label(language: UiLanguage) -> &'static str {
    tr(language, "실패", "Failed")
}

fn cancelled_label(language: UiLanguage) -> &'static str {
    tr(language, "취소", "Cancelled")
}

fn failed_cancelled_label(language: UiLanguage) -> &'static str {
    tr(language, "실패 / 취소", "Failed / Cancelled")
}

fn input_output_tokens_label(language: UiLanguage) -> &'static str {
    tr(language, "입력 / 출력 토큰", "Input / Output Tokens")
}

fn completed_requests_caption(language: UiLanguage) -> &'static str {
    tr(language, "완료된 요청", "Completed requests")
}

fn errors_stale_caption(language: UiLanguage) -> &'static str {
    tr(language, "오류 및 stale", "Errors and stale requests")
}

fn local_estimate_caption(language: UiLanguage) -> &'static str {
    tr(language, "로컬 추정값", "Local estimate")
}

fn success_rate_caption(language: UiLanguage, rate: f64) -> String {
    match language {
        UiLanguage::Korean => format!("성공률 {rate:.1}%"),
        UiLanguage::English => format!("Success rate {rate:.1}%"),
    }
}

fn request_count_axis_label(language: UiLanguage) -> &'static str {
    tr(language, "요청 수", "Requests")
}

fn no_usage_stats_label(language: UiLanguage) -> &'static str {
    tr(
        language,
        "표시할 요청 통계가 없습니다.",
        "No request stats to display.",
    )
}

fn token_estimate_detail(language: UiLanguage) -> &'static str {
    tr(
        language,
        "토큰은 로컬 문자열 기반 추정값입니다.",
        "Token counts are local string-based estimates.",
    )
}

fn provider_usage_summary(
    language: UiLanguage,
    gemini: String,
    openai: String,
    compat: String,
    other: String,
) -> String {
    match language {
        UiLanguage::Korean => {
            format!("Gemini {gemini}  /  OpenAI {openai}  /  호환 API {compat}  /  기타 {other}")
        }
        UiLanguage::English => format!(
            "Gemini {gemini}  /  OpenAI {openai}  /  Compatible API {compat}  /  Other {other}"
        ),
    }
}

fn updated_summary(language: UiLanguage, started: &str, updated: &str) -> String {
    match language {
        UiLanguage::Korean => format!("집계 시작 {started}  /  최근 업데이트 {updated}"),
        UiLanguage::English => format!("Started {started}  /  Last updated {updated}"),
    }
}

fn saved_path_detail(language: UiLanguage, path: &str) -> String {
    match language {
        UiLanguage::Korean => format!("저장 위치: {path}"),
        UiLanguage::English => format!("Saved path: {path}"),
    }
}

fn input_chars_detail(language: UiLanguage, value: &str) -> String {
    match language {
        UiLanguage::Korean => format!("입력 문자: {value}"),
        UiLanguage::English => format!("Input chars: {value}"),
    }
}

fn success_output_chars_detail(language: UiLanguage, value: &str) -> String {
    match language {
        UiLanguage::Korean => format!("성공 출력 문자: {value}"),
        UiLanguage::English => format!("Successful output chars: {value}"),
    }
}

fn last_failure_detail(language: UiLanguage, value: &str) -> String {
    match language {
        UiLanguage::Korean => format!("최근 실패: {value}"),
        UiLanguage::English => format!("Last failure: {value}"),
    }
}

fn last_failure_status(language: UiLanguage, value: &str) -> String {
    match language {
        UiLanguage::Korean => format!("마지막 실패: {value}"),
        UiLanguage::English => format!("Last failure: {value}"),
    }
}

fn period_stats_empty(language: UiLanguage) -> &'static str {
    tr(language, "기간별 통계: -", "Period stats: -")
}

fn bucket_detail_line(
    language: UiLanguage,
    label: &str,
    total: &str,
    succeeded: &str,
    success_rate: f64,
    failed: &str,
    cancelled: &str,
    gemini: &str,
    openai: &str,
    compat: &str,
    other: &str,
    input: &str,
    output: &str,
) -> String {
    match language {
        UiLanguage::Korean => format!(
            "{label}  요청 {total}, 성공 {succeeded} ({success_rate:.1}%), 실패 {failed}, 취소 {cancelled}, G/O/호환/기타 {gemini}/{openai}/{compat}/{other}, 입력 {input}, 출력 {output}"
        ),
        UiLanguage::English => format!(
            "{label}  requests {total}, success {succeeded} ({success_rate:.1}%), failed {failed}, cancelled {cancelled}, G/O/compat/other {gemini}/{openai}/{compat}/{other}, input {input}, output {output}"
        ),
    }
}

fn reset_stats_window_title(language: UiLanguage) -> &'static str {
    tr(language, "통계 초기화", "Reset Stats")
}

fn reset_stats_confirm(language: UiLanguage) -> &'static str {
    tr(
        language,
        "저장된 요청 통계를 초기화할까요?",
        "Reset saved request stats?",
    )
}

fn cancel_button(language: UiLanguage) -> &'static str {
    tr(language, "취소", "Cancel")
}

fn ok_button(language: UiLanguage) -> &'static str {
    tr(language, "확인", "OK")
}

fn close_button(language: UiLanguage) -> &'static str {
    tr(language, "닫기", "Close")
}

fn cli_model_verified_window_title(language: UiLanguage) -> &'static str {
    tr(language, "Gemini CLI 모델 확인", "Gemini CLI Model Check")
}

fn cli_model_verified_heading(language: UiLanguage) -> &'static str {
    tr(
        language,
        "모델 확인이 완료되었습니다.",
        "Model verification is complete.",
    )
}

fn close_starts_cli_note(language: UiLanguage) -> &'static str {
    tr(
        language,
        "닫으면 Gemini CLI 모드를 시작합니다.",
        "Closing this starts Gemini CLI mode.",
    )
}

fn update_check_window_title(language: UiLanguage) -> &'static str {
    tr(language, "업데이트 확인", "Check for Updates")
}

fn update_check_button(language: UiLanguage) -> &'static str {
    tr(language, "업데이트 확인", "Check updates")
}

fn open_github_release_link(language: UiLanguage) -> &'static str {
    tr(language, "GitHub 릴리스 열기", "Open GitHub release")
}

fn download_link_label(language: UiLanguage, asset: &str) -> String {
    match language {
        UiLanguage::Korean => format!("다운로드: {asset}"),
        UiLanguage::English => format!("Download: {asset}"),
    }
}

fn recheck_button(language: UiLanguage) -> &'static str {
    tr(language, "다시 확인", "Check again")
}

fn downloading_label(language: UiLanguage) -> &'static str {
    tr(language, "다운로드 중...", "Downloading...")
}

fn download_and_run_button(language: UiLanguage) -> &'static str {
    tr(language, "EXE 다운로드 후 실행", "Download and run EXE")
}

fn copy_release_link_button(language: UiLanguage) -> &'static str {
    tr(language, "릴리스 링크 복사", "Copy release link")
}

fn developer_info_button(language: UiLanguage) -> &'static str {
    tr(language, "개발자 정보", "Developer Info")
}

fn developer_info_window_title(language: UiLanguage) -> &'static str {
    tr(language, "개발자 정보", "Developer Info")
}

fn version_label(language: UiLanguage, version: &str) -> String {
    match language {
        UiLanguage::Korean => format!("버전: v{version}"),
        UiLanguage::English => format!("Version: v{version}"),
    }
}

fn developer_label(language: UiLanguage, developer: &str) -> String {
    match language {
        UiLanguage::Korean => format!("개발자: {developer}"),
        UiLanguage::English => format!("Developer: {developer}"),
    }
}

fn copy_github_button(language: UiLanguage) -> &'static str {
    tr(language, "GitHub 복사", "Copy GitHub")
}

fn copy_telegram_button(language: UiLanguage) -> &'static str {
    tr(language, "Telegram 링크 복사", "Copy Telegram link")
}

fn start_button(language: UiLanguage) -> &'static str {
    tr(language, "시작", "Start")
}

fn start_card_hover(language: UiLanguage) -> &'static str {
    tr(
        language,
        "설정을 저장하고 이 창을 닫은 뒤 선택한 백엔드를 시작합니다.",
        "Saves settings, closes this window, and starts the selected backend.",
    )
}

fn windows_startup_failed_status(language: UiLanguage, error: &str) -> String {
    match language {
        UiLanguage::Korean => format!("Windows 자동 실행 설정 실패: {error}"),
        UiLanguage::English => format!("Failed to apply Windows startup setting: {error}"),
    }
}

fn update_status_already_checking(language: UiLanguage) -> &'static str {
    tr(
        language,
        "업데이트 확인이 이미 진행 중입니다.",
        "An update check is already running.",
    )
}

fn update_status_checking(language: UiLanguage) -> &'static str {
    tr(
        language,
        "GitHub 최신 릴리스를 확인 중입니다.",
        "Checking the latest GitHub release.",
    )
}

fn update_status_no_asset(language: UiLanguage) -> &'static str {
    tr(
        language,
        "다운로드할 Windows EXE 릴리스 자산이 없습니다.",
        "There is no Windows EXE release asset to download.",
    )
}

fn update_status_download_in_progress(language: UiLanguage) -> &'static str {
    tr(
        language,
        "릴리스 EXE 다운로드가 이미 진행 중입니다.",
        "Release EXE download is already running.",
    )
}

fn update_status_downloading(language: UiLanguage) -> &'static str {
    tr(
        language,
        "릴리스 EXE를 다운로드하고 있습니다.",
        "Downloading release EXE.",
    )
}

fn cli_environment_check_started(language: UiLanguage) -> &'static str {
    tr(
        language,
        "Gemini CLI 환경 진단을 시작했습니다.",
        "Started Gemini CLI environment diagnostics.",
    )
}

fn prompt_reloaded_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "프롬프트 설정을 다시 읽었습니다.",
        "Prompt settings reloaded.",
    )
}

fn prompt_default_loaded_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "기본 prompts.json을 불러왔습니다. 저장해야 적용됩니다.",
        "Loaded the default prompts.json. Save to apply it.",
    )
}

fn no_preset_label(language: UiLanguage) -> &'static str {
    tr(language, "프리셋 없음", "No preset")
}

fn no_preset_to_load_status(language: UiLanguage) -> &'static str {
    tr(language, "불러올 프리셋이 없습니다.", "No preset to load.")
}

fn preset_loaded_status(language: UiLanguage, preset: &str) -> String {
    match language {
        UiLanguage::Korean => format!("프리셋을 불러왔습니다: {preset}"),
        UiLanguage::English => format!("Loaded preset: {preset}"),
    }
}

fn preset_load_failed_status(language: UiLanguage, error: &str) -> String {
    match language {
        UiLanguage::Korean => format!("프리셋 불러오기 실패: {error}"),
        UiLanguage::English => format!("Failed to load preset: {error}"),
    }
}

fn preset_saved_status(language: UiLanguage, path: &str) -> String {
    match language {
        UiLanguage::Korean => format!("사용자 프리셋을 저장했습니다: {path}"),
        UiLanguage::English => format!("Saved user preset: {path}"),
    }
}

fn preset_save_failed_status(language: UiLanguage, error: &str) -> String {
    match language {
        UiLanguage::Korean => format!("사용자 프리셋 저장 실패: {error}"),
        UiLanguage::English => format!("Failed to save user preset: {error}"),
    }
}

fn no_preset_to_delete_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "삭제할 사용자 프리셋이 없습니다.",
        "No user preset to delete.",
    )
}

fn preset_deleted_status(language: UiLanguage, preset: &str) -> String {
    match language {
        UiLanguage::Korean => format!("사용자 프리셋을 삭제했습니다: {preset}"),
        UiLanguage::English => format!("Deleted user preset: {preset}"),
    }
}

fn only_user_presets_delete_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "사용자 프리셋만 삭제할 수 있습니다.",
        "Only user presets can be deleted.",
    )
}

fn prompt_saved_status(language: UiLanguage, path: &str) -> String {
    match language {
        UiLanguage::Korean => format!("프롬프트를 저장했습니다: {path}"),
        UiLanguage::English => format!("Saved prompts: {path}"),
    }
}

fn prompt_save_failed_status(language: UiLanguage, error: &str) -> String {
    match language {
        UiLanguage::Korean => format!("프롬프트 저장 실패: {error}"),
        UiLanguage::English => format!("Failed to save prompts: {error}"),
    }
}

fn cli_ready_starting_status(language: UiLanguage, model: &str) -> String {
    match language {
        UiLanguage::Korean => format!("Gemini CLI 준비 완료. 시작합니다: {model}"),
        UiLanguage::English => format!("Gemini CLI is ready. Starting: {model}"),
    }
}

fn cli_setup_not_completed_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "Gemini CLI 초기설정이 완료되지 않았습니다.",
        "Gemini CLI setup has not completed.",
    )
}

fn cli_cache_missing_start_setup_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "CLI 검증 캐시가 없어 초기설정을 시작합니다.",
        "CLI verification cache is missing. Starting setup.",
    )
}

fn mode_start_unavailable_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "선택한 모드를 시작할 수 없습니다.",
        "The selected mode cannot be started.",
    )
}

fn cli_model_verification_required_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "Gemini CLI 모델 검증이 필요합니다.",
        "Gemini CLI model verification is required.",
    )
}

fn tray_backend_label(language: UiLanguage) -> &'static str {
    tr(language, "트레이 백엔드", "tray backend")
}

fn console_backend_label(language: UiLanguage) -> &'static str {
    tr(language, "콘솔 백엔드", "console backend")
}

fn server_mode_started_status(language: UiLanguage, mode: &str, backend: &str) -> String {
    match language {
        UiLanguage::Korean => format!("{mode} {backend} 시작"),
        UiLanguage::English => format!("Started {mode} {backend}"),
    }
}

fn cli_setup_already_running_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "Gemini CLI 초기설정이 이미 진행 중입니다.",
        "Gemini CLI setup is already running.",
    )
}

fn cli_status_checking_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "Gemini CLI 상태 확인 중...",
        "Checking Gemini CLI status...",
    )
}

fn startup_requires_tray_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "Windows 자동 실행은 트레이 모드가 필요합니다.",
        "Windows startup requires tray mode.",
    )
}

fn windows_startup_enabled_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "Windows 시작 시 자동 실행을 켰습니다.",
        "Windows startup has been enabled.",
    )
}

fn windows_startup_disabled_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "Windows 시작 시 자동 실행을 껐습니다.",
        "Windows startup has been disabled.",
    )
}

fn cli_model_changed_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "CLI 모델 설정을 변경했습니다.",
        "CLI model setting changed.",
    )
}

fn fast_wrapper_cache_cleared_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "fast wrapper 설정 변경으로 CLI 검증 캐시를 초기화했습니다.",
        "CLI verification cache was cleared because the fast wrapper setting changed.",
    )
}

fn local_api_key_added_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "새 로컬 API 키를 추가했습니다.",
        "Added a new local API key.",
    )
}

fn local_api_key_deleted_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "선택한 로컬 API 키를 삭제했습니다.",
        "Deleted the selected local API key.",
    )
}

fn local_api_key_selected_copied_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "선택한 로컬 API 키를 복사했습니다.",
        "Copied the selected local API key.",
    )
}

fn local_api_key_all_copied_status(language: UiLanguage, count: usize) -> String {
    match language {
        UiLanguage::Korean => format!("로컬 API 키 {count}개를 복사했습니다."),
        UiLanguage::English => format!("Copied {count} local API key(s)."),
    }
}

fn stats_refreshed_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "요청 통계를 갱신했습니다.",
        "Request stats refreshed.",
    )
}

fn stats_reset_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "요청 통계를 초기화했습니다.",
        "Request stats reset.",
    )
}

fn release_link_copied_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "GitHub 릴리스 링크를 복사했습니다.",
        "GitHub release link copied.",
    )
}

fn github_copied_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "GitHub 주소를 복사했습니다.",
        "GitHub URL copied.",
    )
}

fn telegram_copied_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "Telegram 링크를 복사했습니다.",
        "Telegram link copied.",
    )
}

fn cli_cache_empty_detail(language: UiLanguage) -> &'static str {
    tr(
        language,
        "CLI 직접 시작은 ruster 초기설정에서 모델 검증을 완료한 뒤 활성화됩니다.",
        "Direct CLI start is enabled after model verification is completed in ruster setup.",
    )
}

fn request_cancel_recovery_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "현재 요청을 취소하고 WebView 세션 복구를 준비합니다.",
        "Cancelling the current request and preparing WebView session recovery.",
    )
}

fn webview_recovery_status(language: UiLanguage) -> &'static str {
    tr(
        language,
        "WebView 세션 복구를 준비합니다.",
        "Preparing WebView session recovery.",
    )
}

fn draw_usage_chart(ui: &mut egui::Ui, buckets: &[UsageBucket], language: UiLanguage) {
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
        request_count_axis_label(language),
        egui::FontId::proportional(13.0),
        text(),
    );
    draw_chart_legend(&painter, area_right - 190.0, area_top + 2.0, language);

    let has_data = buckets.iter().any(|bucket| bucket.total_requests > 0);
    if !has_data {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            no_usage_stats_label(language),
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

fn draw_chart_legend(painter: &egui::Painter, left: f32, top: f32, language: UiLanguage) {
    draw_legend_item(painter, left, top, success(), success_label(language));
    draw_legend_item(painter, left + 64.0, top, danger(), failed_label(language));
    draw_legend_item(
        painter,
        left + 128.0,
        top,
        muted_text(),
        cancelled_label(language),
    );
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
    language: UiLanguage,
) {
    egui::Frame::NONE
        .fill(input_bg())
        .stroke(Stroke::new(1.0, border()))
        .corner_radius(8)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(provider_usage_summary(
                    language,
                    format_count(snapshot.gemini_requests),
                    format_count(snapshot.open_ai_requests),
                    format_count(snapshot.mort_requests),
                    format_count(snapshot.other_requests),
                ))
                .color(muted_text()),
            );
            ui.label(
                RichText::new(updated_summary(
                    language,
                    empty_dash(&snapshot.started_at_local),
                    empty_dash(&snapshot.last_updated_at_local),
                ))
                .color(muted_text()),
            );
            ui.label(RichText::new(token_estimate_detail(language)).color(muted_text()));
            ui.add_space(8.0);
            ui.monospace(saved_path_detail(
                language,
                &paths.usage_metrics_path().display().to_string(),
            ));
            ui.monospace(input_chars_detail(
                language,
                &format_count(snapshot.input_characters),
            ));
            ui.monospace(success_output_chars_detail(
                language,
                &format_count(snapshot.successful_output_characters),
            ));
            ui.monospace(last_failure_detail(
                language,
                empty_dash(&snapshot.last_failure),
            ));
            ui.add_space(6.0);
            if buckets.is_empty() {
                ui.monospace(period_stats_empty(language));
            } else {
                for bucket in buckets {
                    ui.monospace(bucket_detail_line(
                        language,
                        &bucket.label,
                        &format_count(bucket.total_requests),
                        &format_count(bucket.succeeded_requests),
                        bucket.success_rate(),
                        &format_count(bucket.failed_requests),
                        &format_count(bucket.cancelled_requests),
                        &format_count(bucket.gemini_requests),
                        &format_count(bucket.open_ai_requests),
                        &format_count(bucket.mort_requests),
                        &format_count(bucket.other_requests),
                        &format_count(bucket.input_tokens),
                        &format_count(bucket.successful_output_tokens),
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
    let use_fast_wrapper = settings_snapshot.gemini_cli_use_fast_wrapper
        && !cli_discovery::should_use_antigravity_fast_backend();
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
            model_catalog::cli_models_for_current_provider(),
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
            model_catalog::cli_models_for_current_provider(),
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
        self.sync_update_check_status();
        self.finish_cli_auto_start_after_setup();

        if ctx.input(|input| input.key_pressed(egui::Key::F12)) {
            let host = self.host.clone();
            self.runtime.spawn(async move {
                let _ = host.activate_request_guard_with_recovery("gui-f12").await;
            });
            self.set_status(request_cancel_recovery_status(self.language()));
        }
        if ctx.input(|input| input.key_pressed(egui::Key::F5)) {
            let host = self.host.clone();
            self.runtime.spawn(async move {
                let _ = host.activate_request_guard_with_recovery("gui-f5").await;
            });
            self.set_status(webview_recovery_status(self.language()));
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
        self.draw_update_check_dialog(ctx);
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
    language: UiLanguage,
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
        ui.label(RichText::new(start_button(language)).strong().color(text()));
    });
    let response = ui
        .interact(
            frame.response.rect,
            ui.make_persistent_id(("mode_start_card", title)),
            egui::Sense::click(),
        )
        .on_hover_text(start_card_hover(language));
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
