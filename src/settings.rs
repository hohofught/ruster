use serde::{Deserialize, Serialize};

use crate::app_paths::AppPaths;
use crate::model_catalog;
use chrono::{DateTime, Utc};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AppSettings {
    pub base_url: String,
    pub show_console: bool,
    pub run_in_tray: bool,
    pub start_with_windows: bool,
    pub last_translation_mode: String,
    pub open_ai_proxy_enabled: bool,
    pub gemini_proxy_enabled: bool,
    pub require_proxy_api_key: bool,
    pub local_api_key: String,
    pub raw_prompt_mode: bool,
    pub mort_cli_raw_mode: bool,
    pub gemini_cli_model: String,
    pub gemini_cli_timeout_seconds: u64,
    pub gemini_cli_verified_model_ids: Vec<String>,
    pub gemini_cli_verified_at_utc: Option<DateTime<Utc>>,
    pub gemini_cli_verified_source: String,
    pub gemini_cli_verified_wrapper_source: String,
    pub iv_lyrics_quiz_enabled: bool,
    pub iv_lyrics_quiz_detailed_enabled: bool,
    pub iv_lyrics_study_cli_direct_enabled: bool,
    pub iv_lyrics_quiz_cli_model: String,
    pub gemini_cli_use_fast_wrapper: bool,
    pub iv_lyrics_quiz_fast_cli_wrapper_enabled: bool,
    pub gemini_fast_thinking_level: String,
    pub gemini_fast_thinking_budget: i32,
    pub verbose_logs: bool,
    pub web_view_refresh_every_requests: u32,
    pub web_view_pure_quality_mode: bool,
    pub web_view_raw_mode: bool,
    pub web_view_idle_refresh_seconds: u32,
    pub theme_mode: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:5000".to_owned(),
            show_console: true,
            run_in_tray: false,
            start_with_windows: false,
            last_translation_mode: "WebView".to_owned(),
            open_ai_proxy_enabled: true,
            gemini_proxy_enabled: true,
            require_proxy_api_key: true,
            local_api_key: generate_local_api_key(),
            raw_prompt_mode: false,
            mort_cli_raw_mode: true,
            gemini_cli_model: model_catalog::DEFAULT_CLI_MODEL_ID.to_owned(),
            gemini_cli_timeout_seconds: 120,
            gemini_cli_verified_model_ids: Vec::new(),
            gemini_cli_verified_at_utc: None,
            gemini_cli_verified_source: String::new(),
            gemini_cli_verified_wrapper_source: String::new(),
            iv_lyrics_quiz_enabled: true,
            iv_lyrics_quiz_detailed_enabled: false,
            iv_lyrics_study_cli_direct_enabled: false,
            iv_lyrics_quiz_cli_model: model_catalog::DEFAULT_CLI_MODEL_ID.to_owned(),
            gemini_cli_use_fast_wrapper: true,
            iv_lyrics_quiz_fast_cli_wrapper_enabled: true,
            gemini_fast_thinking_level: "LOW".to_owned(),
            gemini_fast_thinking_budget: 2048,
            verbose_logs: true,
            web_view_refresh_every_requests: 1,
            web_view_pure_quality_mode: true,
            web_view_raw_mode: false,
            web_view_idle_refresh_seconds: 60,
            theme_mode: "System".to_owned(),
        }
    }
}

impl AppSettings {
    pub fn load(paths: &AppPaths) -> Self {
        let primary_exists = paths.settings_path().exists();
        let primary = load_settings_file(&paths.settings_path());
        let legacy_portable = load_settings_file(&paths.legacy_settings_path());

        let settings = primary.or(legacy_portable).unwrap_or_default().normalized();
        if !primary_exists {
            let _ = settings.save(paths);
        }
        settings
    }

    pub fn save(&self, paths: &AppPaths) -> anyhow::Result<()> {
        paths.ensure_data_dir();
        let normalized = self.clone().normalized();
        let json = serde_json::to_string_pretty(&normalized)?;
        std::fs::write(paths.settings_path(), json)?;
        Ok(())
    }

    pub fn port(&self) -> u16 {
        url::Url::parse(&self.base_url)
            .ok()
            .and_then(|u| u.port())
            .unwrap_or(5000)
    }

    pub fn host(&self) -> String {
        url::Url::parse(&self.base_url)
            .ok()
            .and_then(|u| u.host_str().map(ToOwned::to_owned))
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_owned())
    }

    pub fn proxy_api_key_required(&self) -> bool {
        self.require_proxy_api_key && !normalize_local_api_key(&self.local_api_key).is_empty()
    }

    pub fn has_gemini_cli_verification_cache(&self) -> bool {
        !self.gemini_cli_verified_model_ids.is_empty()
    }

    pub fn cached_gemini_cli_model_options(&self) -> Vec<model_catalog::ModelOption> {
        normalize_gemini_cli_verified_model_ids(&self.gemini_cli_verified_model_ids)
            .into_iter()
            .filter_map(|model| model_catalog::find_cli(&model))
            .collect()
    }

    pub fn store_gemini_cli_verification_cache(
        &mut self,
        model_ids: impl IntoIterator<Item = String>,
        selected_model: &str,
        cli_source: &str,
        wrapper_source: &str,
    ) {
        let normalized = normalize_gemini_cli_verified_model_ids(model_ids);
        if normalized.is_empty() {
            return;
        }

        let selected = model_catalog::normalize_cli_model(selected_model);
        let selected = if normalized
            .iter()
            .any(|model| model.eq_ignore_ascii_case(&selected))
        {
            selected
        } else {
            normalized[0].clone()
        };

        self.gemini_cli_verified_model_ids = normalized;
        self.gemini_cli_verified_at_utc = Some(Utc::now());
        self.gemini_cli_verified_source = cli_source.trim().to_owned();
        self.gemini_cli_verified_wrapper_source = wrapper_source.trim().to_owned();
        self.gemini_cli_model = selected.clone();
        self.iv_lyrics_quiz_cli_model = selected;
    }

    pub fn clear_gemini_cli_verification_cache(&mut self) {
        self.gemini_cli_verified_model_ids.clear();
        self.gemini_cli_verified_at_utc = None;
        self.gemini_cli_verified_source.clear();
        self.gemini_cli_verified_wrapper_source.clear();
    }

    pub fn normalized(mut self) -> Self {
        self.gemini_cli_model = model_catalog::normalize_cli_model(&self.gemini_cli_model);
        self.iv_lyrics_quiz_cli_model =
            model_catalog::normalize_cli_model(&self.iv_lyrics_quiz_cli_model);
        self.gemini_fast_thinking_level = model_catalog::normalize_thinking_level_for_model(
            &self.gemini_cli_model,
            &self.gemini_fast_thinking_level,
        );
        self.gemini_fast_thinking_budget = model_catalog::thinking_budget_for_model(
            &self.gemini_cli_model,
            &self.gemini_fast_thinking_level,
            self.gemini_fast_thinking_budget,
        );
        self.gemini_cli_timeout_seconds = self.gemini_cli_timeout_seconds.clamp(5, 600);
        self.gemini_cli_verified_model_ids =
            normalize_gemini_cli_verified_model_ids(&self.gemini_cli_verified_model_ids);
        self.web_view_refresh_every_requests = self.web_view_refresh_every_requests.clamp(1, 200);
        self.web_view_idle_refresh_seconds = self.web_view_idle_refresh_seconds.clamp(10, 600);
        self.theme_mode = normalize_theme_mode(&self.theme_mode);
        self.local_api_key = normalize_local_api_key(&self.local_api_key);
        if self.local_api_key.is_empty() {
            self.local_api_key = generate_local_api_key();
        }
        self.last_translation_mode = normalize_translation_mode(&self.last_translation_mode);
        if self.start_with_windows {
            self.run_in_tray = true;
        }
        self.show_console = self.verbose_logs;
        self.iv_lyrics_quiz_enabled = true;
        self.iv_lyrics_quiz_detailed_enabled = false;
        self.iv_lyrics_quiz_fast_cli_wrapper_enabled = self.gemini_cli_use_fast_wrapper;
        self
    }
}

fn load_settings_file(path: &std::path::Path) -> Option<AppSettings> {
    if !path.exists() {
        return None;
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<AppSettings>(&text).ok())
}

fn normalize_gemini_cli_verified_model_ids(
    model_ids: impl IntoIterator<Item = impl AsRef<str>>,
) -> Vec<String> {
    let mut out = Vec::new();
    for model in model_ids {
        let normalized = model_catalog::normalize_cli_model(model.as_ref());
        if model_catalog::find_cli(&normalized).is_none() {
            continue;
        }
        if out
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(&normalized))
        {
            continue;
        }
        out.push(normalized);
    }
    out
}

pub fn normalize_local_api_key(api_key: &str) -> String {
    api_key.trim().to_owned()
}

pub fn normalize_theme_mode(theme_mode: &str) -> String {
    match theme_mode.trim().to_ascii_lowercase().as_str() {
        "light" => "Light",
        "dark" => "Dark",
        _ => "System",
    }
    .to_owned()
}

pub fn normalize_translation_mode(mode: &str) -> String {
    match mode
        .trim()
        .replace(['-', '_', ' '], "")
        .to_ascii_lowercase()
        .as_str()
    {
        "geminicli" | "cli" => "GeminiCli",
        "chatgptwebview" | "chatgpt" | "chatgptweb" => "ChatGptWebView",
        _ => "WebView",
    }
    .to_owned()
}

pub fn generate_local_api_key() -> String {
    let left = uuid::Uuid::new_v4();
    let right = uuid::Uuid::new_v4();
    let mut bytes = [0u8; 24];
    bytes[..16].copy_from_slice(left.as_bytes());
    bytes[16..].copy_from_slice(&right.as_bytes()[..8]);
    format!("rst-{}", base64_url_no_pad(&bytes))
}

fn base64_url_no_pad(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut index = 0;
    while index + 3 <= bytes.len() {
        let chunk = ((bytes[index] as u32) << 16)
            | ((bytes[index + 1] as u32) << 8)
            | bytes[index + 2] as u32;
        out.push(TABLE[((chunk >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((chunk >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((chunk >> 6) & 0x3f) as usize] as char);
        out.push(TABLE[(chunk & 0x3f) as usize] as char);
        index += 3;
    }

    match bytes.len() - index {
        1 => {
            let chunk = (bytes[index] as u32) << 16;
            out.push(TABLE[((chunk >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((chunk >> 12) & 0x3f) as usize] as char);
        }
        2 => {
            let chunk = ((bytes[index] as u32) << 16) | ((bytes[index + 1] as u32) << 8);
            out.push(TABLE[((chunk >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((chunk >> 12) & 0x3f) as usize] as char);
            out.push(TABLE[((chunk >> 6) & 0x3f) as usize] as char);
        }
        _ => {}
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_local_api_key_uses_ruster_base64url_format() {
        let key = generate_local_api_key();

        assert!(key.starts_with("rst-"));
        assert_eq!(key.len(), "rst-".len() + 32);
        assert!(
            key["rst-".len()..]
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        );
        assert!(!key.contains('='));
    }

    #[test]
    fn normalized_settings_preserve_ruster_field_synchronization() {
        let settings = AppSettings {
            verbose_logs: false,
            show_console: true,
            iv_lyrics_quiz_enabled: false,
            iv_lyrics_quiz_detailed_enabled: true,
            gemini_cli_use_fast_wrapper: false,
            iv_lyrics_quiz_fast_cli_wrapper_enabled: true,
            gemini_cli_timeout_seconds: 0,
            web_view_refresh_every_requests: 0,
            web_view_idle_refresh_seconds: 1,
            local_api_key: "  rst-local  ".to_owned(),
            theme_mode: "dark".to_owned(),
            ..Default::default()
        }
        .normalized();

        assert!(!settings.show_console);
        assert!(!settings.run_in_tray);
        assert!(!settings.start_with_windows);
        assert_eq!(settings.last_translation_mode, "WebView");
        assert!(settings.iv_lyrics_quiz_enabled);
        assert!(!settings.iv_lyrics_quiz_detailed_enabled);
        assert!(!settings.iv_lyrics_quiz_fast_cli_wrapper_enabled);
        assert_eq!(settings.gemini_cli_timeout_seconds, 5);
        assert_eq!(settings.web_view_refresh_every_requests, 1);
        assert_eq!(settings.web_view_idle_refresh_seconds, 10);
        assert_eq!(settings.local_api_key, "rst-local");
        assert_eq!(settings.theme_mode, "Dark");
    }

    #[test]
    fn run_in_tray_uses_pascal_case_setting_contract() {
        let settings: AppSettings =
            serde_json::from_str(r#"{"RunInTray":true,"LocalApiKey":"rst-test"}"#).unwrap();

        assert!(settings.run_in_tray);
        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains(r#""RunInTray":true"#));
    }

    #[test]
    fn start_with_windows_enforces_startup_dependency_contract() {
        let settings: AppSettings = serde_json::from_str(
            r#"{"StartWithWindows":true,"RunInTray":false,"LastTranslationMode":"gemini-cli","LocalApiKey":"rst-test"}"#,
        )
        .unwrap();
        let settings = settings.normalized();

        assert!(settings.start_with_windows);
        assert!(settings.run_in_tray);
        assert_eq!(settings.last_translation_mode, "GeminiCli");
        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains(r#""StartWithWindows":true"#));
        assert!(json.contains(r#""LastTranslationMode":"GeminiCli""#));
    }

    #[test]
    fn ivlyrics_study_cli_direct_uses_ruster_setting_field_name() {
        let settings: AppSettings =
            serde_json::from_str(r#"{"IvLyricsStudyCliDirectEnabled":true}"#).unwrap();
        assert!(settings.iv_lyrics_study_cli_direct_enabled);

        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains(r#""IvLyricsStudyCliDirectEnabled":true"#));
    }

    #[test]
    fn mort_cli_raw_mode_defaults_on_and_uses_pascal_case_contract() {
        let settings: AppSettings = serde_json::from_str(r#"{}"#).unwrap();
        assert!(settings.mort_cli_raw_mode);

        let settings: AppSettings = serde_json::from_str(r#"{"MortCliRawMode":false}"#).unwrap();
        assert!(!settings.mort_cli_raw_mode);
        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains(r#""MortCliRawMode":false"#));
    }

    #[test]
    fn store_cli_verification_cache_uses_selected_model_fallback() {
        let mut settings = AppSettings::default();
        settings.store_gemini_cli_verification_cache(
            vec![
                "gemini-2.5-flash".to_owned(),
                "gemini-2.5-flash".to_owned(),
                "gemma-4-31b-it".to_owned(),
            ],
            "gemini-3.1-pro-preview",
            " cli ",
            " wrapper ",
        );

        assert_eq!(
            settings.gemini_cli_verified_model_ids,
            vec!["gemini-2.5-flash".to_owned(), "gemma-4-31b-it".to_owned()]
        );
        assert_eq!(settings.gemini_cli_model, "gemini-2.5-flash");
        assert_eq!(settings.iv_lyrics_quiz_cli_model, "gemini-2.5-flash");
        assert_eq!(settings.gemini_cli_verified_source, "cli");
        assert_eq!(settings.gemini_cli_verified_wrapper_source, "wrapper");
        assert!(settings.gemini_cli_verified_at_utc.is_some());
    }
}
