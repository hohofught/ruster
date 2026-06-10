#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiLanguage {
    Korean,
    English,
}

impl UiLanguage {
    pub const ALL: [Self; 2] = [Self::Korean, Self::English];

    pub fn from_setting(value: &str) -> Self {
        match value
            .trim()
            .replace(['-', '_', ' '], "")
            .to_ascii_lowercase()
            .as_str()
        {
            "english" | "en" | "enus" | "eng" => Self::English,
            _ => Self::Korean,
        }
    }

    pub fn setting_value(self) -> &'static str {
        match self {
            Self::Korean => "Korean",
            Self::English => "English",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Korean => "한국어",
            Self::English => "English",
        }
    }
}

pub fn normalize_ui_language(value: &str) -> String {
    UiLanguage::from_setting(value).setting_value().to_owned()
}
