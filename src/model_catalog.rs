use serde::{Deserialize, Serialize};

pub const BOOTSTRAP_PROBE_MODEL_ID: &str = "gemini-3-flash-preview";
pub const DEFAULT_GEMINI_CLI_MODEL_ID: &str = "gemini-2.5-flash";
pub const DEFAULT_ANTIGRAVITY_CLI_MODEL_ID: &str = "Gemini 3.5 Flash (Medium)";
pub const DEFAULT_CLI_MODEL_ID: &str = DEFAULT_ANTIGRAVITY_CLI_MODEL_ID;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliProvider {
    Antigravity,
    Gemini,
}

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

pub const ANTIGRAVITY_CLI_MODELS: &[ModelOption] = &[
    ModelOption::new(
        "Gemini 3.5 Flash (Medium)",
        "Gemini 3.5 Flash (Medium)",
        false,
    ),
    ModelOption::new("Gemini 3.5 Flash (High)", "Gemini 3.5 Flash (High)", false),
    ModelOption::new("Gemini 3.5 Flash (Low)", "Gemini 3.5 Flash (Low)", false),
    ModelOption::new("Gemini 3.1 Pro (Low)", "Gemini 3.1 Pro (Low)", false),
    ModelOption::new("Gemini 3.1 Pro (High)", "Gemini 3.1 Pro (High)", false),
    ModelOption::new(
        "Claude Sonnet 4.6 (Thinking)",
        "Claude Sonnet 4.6 (Thinking)",
        false,
    ),
    ModelOption::new(
        "Claude Opus 4.6 (Thinking)",
        "Claude Opus 4.6 (Thinking)",
        false,
    ),
    ModelOption::new("GPT-OSS 120B (Medium)", "GPT-OSS 120B (Medium)", false),
];

pub const GEMINI_CLI_MODELS: &[ModelOption] = &[
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

pub const CLI_MODELS: &[ModelOption] = &[
    ModelOption::new(
        "Gemini 3.5 Flash (Medium)",
        "Gemini 3.5 Flash (Medium)",
        false,
    ),
    ModelOption::new("Gemini 3.5 Flash (High)", "Gemini 3.5 Flash (High)", false),
    ModelOption::new("Gemini 3.5 Flash (Low)", "Gemini 3.5 Flash (Low)", false),
    ModelOption::new("Gemini 3.1 Pro (Low)", "Gemini 3.1 Pro (Low)", false),
    ModelOption::new("Gemini 3.1 Pro (High)", "Gemini 3.1 Pro (High)", false),
    ModelOption::new(
        "Claude Sonnet 4.6 (Thinking)",
        "Claude Sonnet 4.6 (Thinking)",
        false,
    ),
    ModelOption::new(
        "Claude Opus 4.6 (Thinking)",
        "Claude Opus 4.6 (Thinking)",
        false,
    ),
    ModelOption::new("GPT-OSS 120B (Medium)", "GPT-OSS 120B (Medium)", false),
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
        .find(|m| {
            m.id.eq_ignore_ascii_case(model_id.trim())
                || m.display_name.eq_ignore_ascii_case(model_id.trim())
        })
        .cloned()
}

pub fn normalize_cli_model(model: &str) -> String {
    let provider = current_cli_provider();
    normalize_cli_model_for_provider(model, provider)
}

pub fn normalize_cli_model_for_provider(model: &str, provider: CliProvider) -> String {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return default_cli_model(provider).to_owned();
    }

    let is_antigravity = provider == CliProvider::Antigravity;
    let default_model = default_cli_model(provider);
    let mapped = match trimmed.to_ascii_lowercase().as_str() {
        "auto" => default_model,
        "auto-gemini-3" => {
            if is_antigravity {
                DEFAULT_ANTIGRAVITY_CLI_MODEL_ID
            } else {
                BOOTSTRAP_PROBE_MODEL_ID
            }
        }
        "auto-gemini-2.5" => {
            if is_antigravity {
                DEFAULT_ANTIGRAVITY_CLI_MODEL_ID
            } else {
                "gemini-2.5-flash"
            }
        }
        "flash" => default_model,
        "flash-low" | "low" => {
            if is_antigravity {
                "Gemini 3.5 Flash (Low)"
            } else {
                default_model
            }
        }
        "flash-medium" | "medium" => default_model,
        "flash-high" | "high" => {
            if is_antigravity {
                "Gemini 3.5 Flash (High)"
            } else {
                default_model
            }
        }
        "thinking" | "pro" | "pro-high" => {
            if is_antigravity {
                "Gemini 3.1 Pro (High)"
            } else {
                "gemini-3.1-pro-preview"
            }
        }
        "pro-low" => {
            if is_antigravity {
                "Gemini 3.1 Pro (Low)"
            } else {
                "gemini-3.1-pro-preview"
            }
        }
        "sonnet" | "claude" => {
            if is_antigravity {
                "Claude Sonnet 4.6 (Thinking)"
            } else {
                default_model
            }
        }
        "opus" => {
            if is_antigravity {
                "Claude Opus 4.6 (Thinking)"
            } else {
                default_model
            }
        }
        "gpt-oss" | "gpt oss" => {
            if is_antigravity {
                "GPT-OSS 120B (Medium)"
            } else {
                default_model
            }
        }
        "gemini-3.5-flash" | "gemini-3.5-flash-medium" => {
            if is_antigravity {
                DEFAULT_ANTIGRAVITY_CLI_MODEL_ID
            } else {
                default_model
            }
        }
        "gemini-3.5-flash-low" => {
            if is_antigravity {
                "Gemini 3.5 Flash (Low)"
            } else {
                default_model
            }
        }
        "gemini-3.5-flash-high" => {
            if is_antigravity {
                "Gemini 3.5 Flash (High)"
            } else {
                default_model
            }
        }
        "gemini-3.1-pro"
        | "gemini-3.0-pro"
        | "gemini-3.0-pro-thinking"
        | "gemini-3-pro-preview" => {
            if is_antigravity {
                "Gemini 3.1 Pro (High)"
            } else {
                "gemini-3.1-pro-preview"
            }
        }
        "flash-lite" => {
            if is_antigravity {
                DEFAULT_ANTIGRAVITY_CLI_MODEL_ID
            } else {
                "gemini-3.1-flash-lite-preview"
            }
        }
        "gemini-3-flash-preview" | "gemini-3.0-flash" => {
            if is_antigravity {
                DEFAULT_ANTIGRAVITY_CLI_MODEL_ID
            } else {
                "gemini-3-flash-preview"
            }
        }
        "gemini-2.5-flash" | "gemini-2.0-flash" => {
            if is_antigravity {
                DEFAULT_ANTIGRAVITY_CLI_MODEL_ID
            } else {
                DEFAULT_GEMINI_CLI_MODEL_ID
            }
        }
        "gemini-2.0-flash-lite" => {
            if is_antigravity {
                DEFAULT_ANTIGRAVITY_CLI_MODEL_ID
            } else {
                "gemini-3.1-flash-lite-preview"
            }
        }
        _ => trimmed,
    };

    cli_models_for_provider(provider)
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(mapped) || m.display_name.eq_ignore_ascii_case(mapped))
        .map(|m| m.id.to_owned())
        .or_else(|| find_cli(mapped).map(|m| m.id.to_owned()))
        .unwrap_or_else(|| default_cli_model(provider).to_owned())
}

pub fn is_gemini_3_or_newer(model: &str) -> bool {
    normalize_model_text(model).starts_with("gemini-3")
        || normalize_model_text(model).starts_with("gemini 3")
}

pub fn supports_gemini3_minimal_thinking(model: &str) -> bool {
    let normalized = normalize_model_text(model);
    normalized.starts_with("gemini-3-flash")
        || normalized.starts_with("gemini 3.5 flash")
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
    supports_antigravity_thinking_level(model)
        || supports_gemini3_thinking_level(model)
        || supports_gemini25_thinking_budget(model)
}

pub fn thinking_options_for_model(model: &str) -> Vec<&'static str> {
    if is_antigravity_flash_model(model) {
        return vec!["LOW", "MEDIUM", "HIGH"];
    }
    if is_antigravity_pro_model(model) {
        return vec!["LOW", "HIGH"];
    }
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
    let embedded = embedded_antigravity_thinking_level(model);
    if !embedded.is_empty() {
        return embedded.to_owned();
    }
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

pub fn apply_cli_thinking_level(model: &str, thinking_level: &str) -> String {
    let provider = current_cli_provider();
    apply_cli_thinking_level_for_provider(model, thinking_level, provider)
}

pub fn apply_cli_thinking_level_for_provider(
    model: &str,
    thinking_level: &str,
    provider: CliProvider,
) -> String {
    let normalized = normalize_cli_model_for_provider(model, provider);
    if provider != CliProvider::Antigravity {
        return normalized;
    }

    let requested_level = normalize_thinking_level(thinking_level);
    let options = thinking_options_for_model(&normalized);
    let level = if options
        .iter()
        .any(|option| option.eq_ignore_ascii_case(&requested_level))
    {
        requested_level
    } else {
        let embedded = embedded_antigravity_thinking_level(&normalized);
        if !embedded.is_empty()
            && options
                .iter()
                .any(|option| option.eq_ignore_ascii_case(embedded))
        {
            embedded.to_owned()
        } else {
            options.first().copied().unwrap_or("LOW").to_owned()
        }
    };
    if is_antigravity_flash_model(&normalized) {
        return match level.as_str() {
            "LOW" => "Gemini 3.5 Flash (Low)".to_owned(),
            "HIGH" => "Gemini 3.5 Flash (High)".to_owned(),
            _ => DEFAULT_ANTIGRAVITY_CLI_MODEL_ID.to_owned(),
        };
    }

    if is_antigravity_pro_model(&normalized) {
        return if level == "HIGH" {
            "Gemini 3.1 Pro (High)".to_owned()
        } else {
            "Gemini 3.1 Pro (Low)".to_owned()
        };
    }

    normalized
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

pub fn default_cli_model(provider: CliProvider) -> &'static str {
    match provider {
        CliProvider::Antigravity => DEFAULT_ANTIGRAVITY_CLI_MODEL_ID,
        CliProvider::Gemini => DEFAULT_GEMINI_CLI_MODEL_ID,
    }
}

pub fn cli_models_for_provider(provider: CliProvider) -> &'static [ModelOption] {
    match provider {
        CliProvider::Antigravity => ANTIGRAVITY_CLI_MODELS,
        CliProvider::Gemini => GEMINI_CLI_MODELS,
    }
}

pub fn current_cli_provider() -> CliProvider {
    if crate::cli_discovery::should_use_antigravity_fast_backend() {
        CliProvider::Antigravity
    } else {
        CliProvider::Gemini
    }
}

pub fn cli_models_for_current_provider() -> &'static [ModelOption] {
    cli_models_for_provider(current_cli_provider())
}

pub fn is_antigravity_cli_model(model: &str) -> bool {
    let normalized = model.trim();
    ANTIGRAVITY_CLI_MODELS.iter().any(|option| {
        option.id.eq_ignore_ascii_case(normalized)
            || option.display_name.eq_ignore_ascii_case(normalized)
    })
}

fn supports_antigravity_thinking_level(model: &str) -> bool {
    is_antigravity_flash_model(model) || is_antigravity_pro_model(model)
}

fn is_antigravity_flash_model(model: &str) -> bool {
    let normalized = normalize_model_text(model);
    normalized.starts_with("gemini 3.5 flash") || normalized.starts_with("gemini-3.5-flash")
}

fn is_antigravity_pro_model(model: &str) -> bool {
    let normalized = normalize_model_text(model);
    normalized.starts_with("gemini 3.1 pro") || normalized.starts_with("gemini-3.1-pro")
}

fn embedded_antigravity_thinking_level(model: &str) -> &'static str {
    let normalized = normalize_model_text(model);
    if normalized.contains("(low)") {
        "LOW"
    } else if normalized.contains("(medium)") {
        "MEDIUM"
    } else if normalized.contains("(high)") || normalized.contains("(thinking)") {
        "HIGH"
    } else {
        ""
    }
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
        assert_eq!(
            normalize_cli_model_for_provider("", CliProvider::Gemini),
            DEFAULT_GEMINI_CLI_MODEL_ID
        );
        assert_eq!(
            normalize_cli_model_for_provider("auto-gemini-3", CliProvider::Gemini),
            BOOTSTRAP_PROBE_MODEL_ID
        );
        assert_eq!(
            normalize_cli_model_for_provider("thinking", CliProvider::Gemini),
            "gemini-3.1-pro-preview"
        );
        assert_eq!(
            normalize_cli_model_for_provider("gemini-3.0-pro-thinking", CliProvider::Gemini),
            "gemini-3.1-pro-preview"
        );
        assert_eq!(
            normalize_cli_model_for_provider("gemini-2.0-flash", CliProvider::Gemini),
            DEFAULT_GEMINI_CLI_MODEL_ID
        );
        assert_eq!(
            normalize_cli_model_for_provider("unknown-model", CliProvider::Gemini),
            DEFAULT_GEMINI_CLI_MODEL_ID
        );
        assert_eq!(
            normalize_cli_model_for_provider("flash-high", CliProvider::Antigravity),
            "Gemini 3.5 Flash (High)"
        );
        assert_eq!(
            normalize_cli_model_for_provider("sonnet", CliProvider::Antigravity),
            "Claude Sonnet 4.6 (Thinking)"
        );
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
        assert_eq!(
            thinking_options_for_model("Gemini 3.5 Flash (Medium)"),
            vec!["LOW", "MEDIUM", "HIGH"]
        );
        assert_eq!(
            apply_cli_thinking_level_for_provider(
                "Gemini 3.5 Flash (Medium)",
                "HIGH",
                CliProvider::Antigravity
            ),
            "Gemini 3.5 Flash (High)"
        );
    }
}
