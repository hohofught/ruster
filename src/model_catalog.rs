use serde::{Deserialize, Serialize};

pub const BOOTSTRAP_PROBE_MODEL_ID: &str = "gemini-3-flash-preview";
pub const DEFAULT_CLI_MODEL_ID: &str = "gemini-2.5-flash";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelOption {
    pub id: &'static str,
    pub display_name: &'static str,
    pub is_preview: bool,
    pub input_token_limit: u32,
    pub output_token_limit: u32,
}

impl ModelOption {
    pub const fn new(id: &'static str, display_name: &'static str, is_preview: bool) -> Self {
        Self {
            id,
            display_name,
            is_preview,
            input_token_limit: 1_048_576,
            output_token_limit: 65_536,
        }
    }
}

pub const CLI_MODELS: &[ModelOption] = &[
    ModelOption::new("gemini-3.1-pro-preview", "Gemini 3.1 Pro Preview", true),
    ModelOption::new("gemini-3-flash-preview", "Gemini 3 Flash Preview", true),
    ModelOption::new(
        "gemini-3.1-flash-lite-preview",
        "Gemini 3.1 Flash-Lite Preview",
        true,
    ),
    ModelOption::new("gemini-2.5-pro", "Gemini 2.5 Pro", false),
    ModelOption::new("gemini-2.5-flash", "Gemini 2.5 Flash", false),
    ModelOption::new("gemini-2.5-flash-lite", "Gemini 2.5 Flash-Lite", false),
    ModelOption::new("gemma-4-31b-it", "Gemma 4 31B IT", false),
    ModelOption::new("gemma-4-26b-a4b-it", "Gemma 4 26B A4B IT", false),
];

pub fn api_models() -> Vec<ModelOption> {
    let mut models = vec![ModelOption::new(
        "gemini-3.5-flash",
        "Gemini 3.5 Flash",
        false,
    )];
    models.extend(
        CLI_MODELS
            .iter()
            .filter(|m| m.id.starts_with("gemini-"))
            .cloned(),
    );
    models
}

pub fn find_cli(model_id: &str) -> Option<ModelOption> {
    CLI_MODELS
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(model_id.trim()))
        .cloned()
}

pub fn normalize_cli_model(model: &str) -> String {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return DEFAULT_CLI_MODEL_ID.to_owned();
    }

    let mapped = match trimmed.to_ascii_lowercase().as_str() {
        "auto" => DEFAULT_CLI_MODEL_ID,
        "auto-gemini-3" => BOOTSTRAP_PROBE_MODEL_ID,
        "auto-gemini-2.5" => "gemini-2.5-flash",
        "flash" => DEFAULT_CLI_MODEL_ID,
        "thinking" | "pro" => "gemini-3.1-pro-preview",
        "flash-lite" => "gemini-3.1-flash-lite-preview",
        "gemini-3.0-flash" => "gemini-3-flash-preview",
        "gemini-3.0-pro" | "gemini-3.0-pro-thinking" | "gemini-3-pro-preview" => {
            "gemini-3.1-pro-preview"
        }
        "gemini-2.0-flash" => DEFAULT_CLI_MODEL_ID,
        "gemini-2.0-flash-lite" => "gemini-3.1-flash-lite-preview",
        _ => trimmed,
    };

    find_cli(mapped)
        .map(|m| m.id.to_owned())
        .unwrap_or_else(|| DEFAULT_CLI_MODEL_ID.to_owned())
}

pub fn is_gemini_3_or_newer(model: &str) -> bool {
    normalize_model_text(model).starts_with("gemini-3")
}

pub fn supports_gemini3_minimal_thinking(model: &str) -> bool {
    let normalized = normalize_model_text(model);
    normalized.starts_with("gemini-3-flash")
        || normalized.starts_with("gemini-3.1-flash-lite")
        || normalized.starts_with("gemini-3.5-flash")
}

pub fn supports_gemini3_thinking_level(model: &str) -> bool {
    is_gemini_3_or_newer(model)
}

pub fn supports_gemini25_thinking_budget(model: &str) -> bool {
    let normalized = normalize_model_text(model);
    normalized.starts_with("gemini-2.5-")
        || normalized.starts_with("gemini-2-5-")
        || normalized.starts_with("robotics-er-1.")
}

pub fn is_gemini25_pro(model: &str) -> bool {
    let normalized = normalize_model_text(model);
    normalized.starts_with("gemini-2.5-pro") || normalized.starts_with("gemini-2-5-pro")
}

pub fn is_gemini25_flash_lite(model: &str) -> bool {
    let normalized = normalize_model_text(model);
    (normalized.starts_with("gemini-2.5-") || normalized.starts_with("gemini-2-5-"))
        && normalized.contains("flash-lite")
}

pub fn supports_gemini_thinking_off(model: &str) -> bool {
    supports_gemini25_thinking_budget(model) && !is_gemini25_pro(model)
}

#[allow(dead_code)]
pub fn supports_thinking_controls(model: &str) -> bool {
    supports_gemini3_thinking_level(model) || supports_gemini25_thinking_budget(model)
}

pub fn thinking_options_for_model(model: &str) -> Vec<&'static str> {
    if !supports_thinking_controls(model) {
        return vec!["LOW"];
    }

    let mut options = Vec::new();
    if supports_gemini_thinking_off(model) {
        options.push("OFF");
    }
    if supports_gemini3_minimal_thinking(model) || supports_gemini25_thinking_budget(model) {
        options.push("MINIMAL");
    }
    options.extend(["LOW", "MEDIUM", "HIGH"]);
    options
}

pub fn normalize_thinking_level_for_model(model: &str, value: &str) -> String {
    let normalized = normalize_thinking_level(value);
    let options = thinking_options_for_model(model);
    if options
        .iter()
        .any(|option| option.eq_ignore_ascii_case(&normalized))
    {
        normalized
    } else {
        options.first().copied().unwrap_or("LOW").to_owned()
    }
}

pub fn thinking_budget_for_model(model: &str, level: &str, fallback: i32) -> i32 {
    let normalized = normalize_thinking_level(level);
    if is_gemini25_pro(model) {
        return match normalized.as_str() {
            "OFF" | "MINIMAL" => 128,
            "LOW" => 2048,
            "MEDIUM" => 8192,
            "HIGH" => 32768,
            _ => 2048,
        };
    }

    if is_gemini25_flash_lite(model) {
        return match normalized.as_str() {
            "OFF" | "MINIMAL" => 0,
            "LOW" => 512,
            "MEDIUM" => 4096,
            "HIGH" => 24576,
            _ => 512,
        };
    }

    match normalized.as_str() {
        "OFF" | "MINIMAL" => 0,
        "LOW" => 1024,
        "MEDIUM" => 4096,
        "HIGH" => 24576,
        _ => clamp_thinking_budget(fallback),
    }
}

pub fn clamp_thinking_budget(value: i32) -> i32 {
    if value == -1 || value == 0 {
        value
    } else if value <= 0 {
        2048
    } else {
        value.clamp(128, 32768)
    }
}

pub fn normalize_thinking_level(value: &str) -> String {
    match value.trim().to_ascii_uppercase().as_str() {
        "OFF" | "NONE" | "DISABLED" | "DISABLE" => "OFF",
        "MINIMAL" | "MIN" => "MINIMAL",
        "LOW" => "LOW",
        "MID" | "MEDIUM" => "MEDIUM",
        "HIGH" => "HIGH",
        _ => "LOW",
    }
    .to_owned()
}

fn normalize_model_text(model: &str) -> String {
    model.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_model_list_uses_catalog_shape() {
        let models = api_models();
        let ids = models.iter().map(|model| model.id).collect::<Vec<_>>();

        assert_eq!(ids.first().copied(), Some("gemini-3.5-flash"));
        assert!(ids.contains(&"gemini-3.1-pro-preview"));
        assert!(ids.contains(&"gemini-2.5-flash"));
        assert!(!ids.iter().any(|id| id.starts_with("gemma-")));
        assert!(models.iter().all(|model| {
            model.input_token_limit == 1_048_576 && model.output_token_limit == 65_536
        }));
    }

    #[test]
    fn cli_model_aliases_use_expected_normalization() {
        assert_eq!(normalize_cli_model(""), DEFAULT_CLI_MODEL_ID);
        assert_eq!(
            normalize_cli_model("auto-gemini-3"),
            BOOTSTRAP_PROBE_MODEL_ID
        );
        assert_eq!(normalize_cli_model("thinking"), "gemini-3.1-pro-preview");
        assert_eq!(
            normalize_cli_model("gemini-3.0-pro-thinking"),
            "gemini-3.1-pro-preview"
        );
        assert_eq!(
            normalize_cli_model("gemini-2.0-flash"),
            DEFAULT_CLI_MODEL_ID
        );
        assert_eq!(normalize_cli_model("unknown-model"), DEFAULT_CLI_MODEL_ID);
    }

    #[test]
    fn thinking_options_and_budgets_use_model_rules() {
        assert_eq!(
            thinking_options_for_model("gemini-2.5-flash"),
            vec!["OFF", "MINIMAL", "LOW", "MEDIUM", "HIGH"]
        );
        assert_eq!(
            thinking_options_for_model("gemini-2.5-pro"),
            vec!["MINIMAL", "LOW", "MEDIUM", "HIGH"]
        );
        assert_eq!(
            thinking_budget_for_model("gemini-2.5-pro", "LOW", 999),
            2048
        );
        assert_eq!(
            thinking_budget_for_model("gemini-2.5-flash-lite", "HIGH", 999),
            24576
        );
        assert_eq!(
            thinking_budget_for_model("gemini-3-flash-preview", "LOW", 999),
            1024
        );
        assert_eq!(clamp_thinking_budget(-1), -1);
        assert_eq!(clamp_thinking_budget(1), 128);
        assert_eq!(clamp_thinking_budget(50000), 32768);
    }
}
