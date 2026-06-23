use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub executable_dir: PathBuf,
    pub data_dir: PathBuf,
    pub using_portable_data: bool,
}

impl AppPaths {
    pub fn resolve() -> Self {
        let executable_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let portable = is_portable_dir(&executable_dir) && can_write_to_dir(&executable_dir);
        let data_dir = if portable {
            executable_dir.clone()
        } else {
            dirs::data_local_dir()
                .unwrap_or_else(|| executable_dir.clone())
                .join("ruster")
        };

        Self {
            executable_dir,
            using_portable_data: portable,
            data_dir,
        }
    }

    pub fn ensure_data_dir(&self) {
        let _ = std::fs::create_dir_all(&self.data_dir);
    }

    pub fn settings_path(&self) -> PathBuf {
        self.data_dir.join("settings.json")
    }

    pub fn legacy_settings_path(&self) -> PathBuf {
        self.executable_dir.join("settings.json")
    }

    pub fn usage_metrics_path(&self) -> PathBuf {
        self.data_dir.join("usage-metrics.json")
    }

    pub fn ivlyrics_study_limit_guard_path(&self) -> PathBuf {
        self.data_dir.join("ivlyrics-study-cli-limit.json")
    }

    pub fn custom_api_preset_dir(&self) -> PathBuf {
        self.data_dir.join("CustomApi")
    }

    pub fn prompt_override_path(&self) -> PathBuf {
        self.data_dir.join("prompts.json")
    }

    pub fn prompt_preset_dir(&self) -> PathBuf {
        self.data_dir.join("PromptPresets")
    }

    pub fn webview_data_dir(&self) -> PathBuf {
        self.data_dir.join("WebView2")
    }

    #[allow(dead_code)]
    pub fn webview_user_data_dir(&self, profile_name: &str) -> PathBuf {
        self.webview_data_dir()
            .join(sanitize_path_segment(profile_name))
    }
}

fn is_portable_dir(dir: &Path) -> bool {
    dir.join("ruster.portable").exists() || dir.join("settings.json").exists()
}

fn can_write_to_dir(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }

    let probe = dir.join(format!(".ruster-write-probe-{}.tmp", uuid::Uuid::new_v4()));
    match std::fs::write(&probe, []) {
        Ok(()) => {
            let _ = std::fs::remove_file(probe);
            true
        }
        Err(_) => false,
    }
}

#[allow(dead_code)]
fn sanitize_path_segment(value: &str) -> String {
    let invalid = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if invalid.contains(&ch) || ch.is_control() {
                '_'
            } else {
                ch
            }
        })
        .collect::<String>()
        .trim()
        .to_owned();

    if sanitized.is_empty() {
        "Default".to_owned()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_marker_matches_ruster_marker_and_settings_file() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("ruster-tests")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir).unwrap();

        assert!(!is_portable_dir(&dir));
        std::fs::write(dir.join("ruster.portable"), "").unwrap();
        assert!(is_portable_dir(&dir));
        std::fs::remove_file(dir.join("ruster.portable")).unwrap();
        std::fs::write(dir.join("settings.json"), "{}").unwrap();
        assert!(is_portable_dir(&dir));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sanitize_path_segment_replaces_windows_invalid_characters() {
        assert_eq!(
            sanitize_path_segment(" Gemini:ChatGPT? "),
            "Gemini_ChatGPT_"
        );
        assert_eq!(sanitize_path_segment("   "), "Default");
    }

    #[test]
    fn webview_paths_match_csharp_profile_layout() {
        let paths = AppPaths {
            executable_dir: PathBuf::from("app"),
            data_dir: PathBuf::from("data"),
            using_portable_data: false,
        };

        assert_eq!(
            paths.webview_data_dir(),
            PathBuf::from("data").join("WebView2")
        );
        assert_eq!(
            paths.webview_user_data_dir("Gemini"),
            PathBuf::from("data").join("WebView2").join("Gemini")
        );
    }
}
