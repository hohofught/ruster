use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::app_paths::AppPaths;
use crate::logging::LogBuffer;

pub const MODE_TRANSLATE: &str = "translate";
pub const MODE_RAW_PROMPT: &str = "raw";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct CustomApiPreset {
    pub name: String,
    pub url: String,
    pub method: String,
    pub headers: Vec<String>,
    pub request_template: String,
    pub response_template: String,
    pub result_path: String,
    pub mode: String,
    pub timeout_seconds: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct CustomApiPresetList {
    presets: Vec<CustomApiPreset>,
}

#[derive(Clone)]
pub struct CustomApiPresetService {
    dir: PathBuf,
    logs: LogBuffer,
}

#[derive(Clone, Debug)]
pub struct IncomingText {
    pub text: String,
    pub source_code: String,
    pub result_code: String,
}

impl CustomApiPresetService {
    pub fn new(paths: &AppPaths, logs: LogBuffer) -> Self {
        Self {
            dir: paths.custom_api_preset_dir(),
            logs,
        }
    }

    pub fn get_all(&self) -> Vec<CustomApiPreset> {
        let mut presets = self.load_all();
        presets.sort_by(|a, b| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        });
        presets
    }

    pub fn find(&self, name: &str) -> Option<CustomApiPreset> {
        let needle = name.trim();
        self.load_all()
            .into_iter()
            .find(|preset| preset.name.eq_ignore_ascii_case(needle))
    }

    fn load_all(&self) -> Vec<CustomApiPreset> {
        let _ = std::fs::create_dir_all(&self.dir);
        let mut loaded = HashMap::<String, CustomApiPreset>::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
                continue;
            };
            if !ext.eq_ignore_ascii_case("json") && !ext.eq_ignore_ascii_case("txt") {
                continue;
            }
            self.load_file(&path, &mut loaded);
        }

        loaded.into_values().collect()
    }

    fn load_file(&self, path: &PathBuf, loaded: &mut HashMap<String, CustomApiPreset>) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let text = text.trim();
        if text.is_empty() {
            return;
        }

        let value = match serde_json::from_str::<serde_json::Value>(text) {
            Ok(value) => value,
            Err(error) => {
                self.logs.push(format!(
                    "[CustomApi] 프리셋 JSON 파싱 실패 ({}): {error}",
                    path.file_name().and_then(|s| s.to_str()).unwrap_or("?")
                ));
                return;
            }
        };

        if value.get("presets").is_some() || value.get("Presets").is_some() {
            if let Ok(list) = serde_json::from_value::<CustomApiPresetList>(value) {
                for preset in list.presets {
                    add_preset(preset, loaded);
                }
            }
        } else if let Ok(preset) = serde_json::from_str::<CustomApiPreset>(text) {
            add_preset(preset, loaded);
        }
    }
}

fn add_preset(mut preset: CustomApiPreset, loaded: &mut HashMap<String, CustomApiPreset>) {
    preset.name = preset.name.trim().to_owned();
    if preset.name.is_empty() {
        return;
    }
    preset.method = if preset.method.trim().is_empty() {
        "POST".to_owned()
    } else {
        preset.method.trim().to_ascii_uppercase()
    };
    preset.mode = if preset.mode.eq_ignore_ascii_case(MODE_RAW_PROMPT)
        || preset.mode.eq_ignore_ascii_case("rawPrompt")
    {
        MODE_RAW_PROMPT.to_owned()
    } else {
        MODE_TRANSLATE.to_owned()
    };
    preset.timeout_seconds = preset.timeout_seconds.clamp(5, 300);
    preset.headers = preset
        .headers
        .into_iter()
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .collect();

    loaded
        .entry(preset.name.to_ascii_lowercase())
        .or_insert(preset);
}

pub fn build_prompt(
    original_text: &str,
    preset: Option<&CustomApiPreset>,
    source_code: &str,
    result_code: &str,
) -> String {
    let Some(preset) = preset else {
        return original_text.to_owned();
    };
    if preset.request_template.trim().is_empty() {
        return original_text.to_owned();
    }

    replace_plain_tokens(
        &preset.request_template,
        original_text,
        "",
        source_code,
        result_code,
    )
}

pub fn build_mort_json_response(result: &str, error_message: &str, error_code: &str) -> String {
    serde_json::json!({
        "result": result,
        "errorMessage": error_message,
        "errorCode": error_code,
    })
    .to_string()
}

pub fn build_custom_json_response(
    preset: Option<&CustomApiPreset>,
    result: &str,
    original_text: &str,
    source_code: &str,
    result_code: &str,
) -> String {
    let Some(preset) = preset else {
        return build_mort_json_response(result, "", "0");
    };
    if preset.response_template.trim().is_empty() {
        return build_mort_json_response(result, "", "0");
    }

    let mut template = preset.response_template.trim().to_owned();
    if !template.starts_with('{') && !template.starts_with('[') {
        template = format!("{{{template}}}");
    }
    let rendered =
        replace_json_value_tokens(&template, original_text, result, source_code, result_code);
    if serde_json::from_str::<serde_json::Value>(&rendered).is_ok() {
        rendered
    } else {
        build_mort_json_response(result, "", "0")
    }
}

pub fn extract_incoming_text(body: &str) -> IncomingText {
    let trimmed = body.trim_start();
    if trimmed.starts_with('{') {
        if let Ok(root) = serde_json::from_str::<serde_json::Value>(body) {
            let source = first_string(&root, &["source", "from", "sourceCode"]);
            let target = first_string(&root, &["target", "to", "targetCode", "resultCode"]);
            let text = first_string(&root, &["text", "prompt", "q", "input"])
                .or_else(|| extract_openai_prompt(&root))
                .or_else(|| extract_gemini_prompt(&root))
                .unwrap_or_default();
            if !text.trim().is_empty() {
                return IncomingText {
                    text,
                    source_code: source.unwrap_or_default(),
                    result_code: target.unwrap_or_default(),
                };
            }
        }
    } else if trimmed.starts_with('"')
        && let Ok(text) = serde_json::from_str::<String>(body)
        && !text.trim().is_empty()
    {
        return IncomingText {
            text,
            source_code: String::new(),
            result_code: String::new(),
        };
    }

    IncomingText {
        text: body.to_owned(),
        source_code: String::new(),
        result_code: String::new(),
    }
}

fn replace_plain_tokens(
    template: &str,
    ocr_text: &str,
    result_text: &str,
    source_code: &str,
    result_code: &str,
) -> String {
    template
        .replace("{OCR_TEXT}", ocr_text)
        .replace("{RESULT_TEXT}", result_text)
        .replace("{SOURCE_CODE}", source_code)
        .replace("{RESULT_CODE}", result_code)
        .replace("{RAW_PROMPT}", ocr_text)
}

fn replace_json_value_tokens(
    template: &str,
    ocr_text: &str,
    result_text: &str,
    source_code: &str,
    result_code: &str,
) -> String {
    let mut out = template.to_owned();
    for (token, value) in [
        ("{OCR_TEXT}", ocr_text),
        ("{RESULT_TEXT}", result_text),
        ("{SOURCE_CODE}", source_code),
        ("{RESULT_CODE}", result_code),
        ("{RAW_PROMPT}", ocr_text),
    ] {
        let json = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned());
        out = out.replace(&format!("\"{token}\""), &json);
        out = out.replace(token, &json);
    }
    out
}

fn first_string(root: &serde_json::Value, names: &[&str]) -> Option<String> {
    for name in names {
        if let Some(value) = root.get(*name) {
            if let Some(text) = value.as_str() {
                return Some(text.to_owned());
            }
            if value.is_number() || value.is_boolean() {
                return Some(value.to_string());
            }
        }
    }
    None
}

pub fn extract_openai_prompt(root: &serde_json::Value) -> Option<String> {
    let messages = root.get("messages")?.as_array()?;
    for message in messages.iter().rev() {
        if message
            .get("role")
            .and_then(|v| v.as_str())
            .map(|role| !role.eq_ignore_ascii_case("user"))
            .unwrap_or(false)
        {
            continue;
        }
        if let Some(text) = extract_content_text(message.get("content")?)
            && !text.trim().is_empty()
        {
            return Some(text);
        }
    }
    None
}

pub fn extract_gemini_prompt(root: &serde_json::Value) -> Option<String> {
    let contents = root.get("contents")?.as_array()?;
    for content in contents.iter().rev() {
        let Some(parts) = content.get("parts").and_then(|v| v.as_array()) else {
            continue;
        };
        let text = parts
            .iter()
            .filter_map(extract_content_text)
            .collect::<Vec<_>>()
            .join("");
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    None
}

pub fn extract_content_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Object(obj) => obj
            .get("text")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned),
        serde_json::Value::Array(items) => Some(
            items
                .iter()
                .filter_map(extract_content_text)
                .collect::<Vec<_>>()
                .join(""),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn mort_json_response_matches_ruster_contract() {
        let body = build_mort_json_response("번역", "오류", "503");
        let value: Value = serde_json::from_str(&body).unwrap();

        assert_eq!(value["result"], "번역");
        assert_eq!(value["errorMessage"], "오류");
        assert_eq!(value["errorCode"], "503");
        assert_eq!(value.as_object().unwrap().len(), 3);
    }

    #[test]
    fn custom_response_template_wraps_fragment_and_json_escapes_tokens() {
        let preset = CustomApiPreset {
            response_template:
                "\"translated\":\"{RESULT_TEXT}\",\"source\":\"{OCR_TEXT}\",\"from\":\"{SOURCE_CODE}\""
                    .to_owned(),
            ..Default::default()
        };

        let body =
            build_custom_json_response(Some(&preset), "line \"one\"\nline two", "원문", "ja", "ko");
        let value: Value = serde_json::from_str(&body).unwrap();

        assert_eq!(value["translated"], "line \"one\"\nline two");
        assert_eq!(value["source"], "원문");
        assert_eq!(value["from"], "ja");
    }

    #[test]
    fn invalid_custom_response_template_falls_back_to_mort_json() {
        let preset = CustomApiPreset {
            response_template: "\"translated\":\"{RESULT_TEXT}\",".to_owned(),
            ..Default::default()
        };

        let body = build_custom_json_response(Some(&preset), "ok", "src", "ja", "ko");
        let value: Value = serde_json::from_str(&body).unwrap();

        assert_eq!(value["result"], "ok");
        assert_eq!(value["errorMessage"], "");
        assert_eq!(value["errorCode"], "0");
    }

    #[test]
    fn incoming_text_extracts_mort_json_aliases_and_scalar_codes() {
        let incoming = extract_incoming_text(r#"{"source":12,"resultCode":true,"q":"번역 대상"}"#);

        assert_eq!(incoming.text, "번역 대상");
        assert_eq!(incoming.source_code, "12");
        assert_eq!(incoming.result_code, "true");
    }

    #[test]
    fn incoming_text_falls_back_to_raw_body_on_malformed_json() {
        let body = r#"{"text":"깨진 JSON""#;
        let incoming = extract_incoming_text(body);

        assert_eq!(incoming.text, body);
        assert_eq!(incoming.source_code, "");
        assert_eq!(incoming.result_code, "");
    }

    #[test]
    fn mort_openai_prompt_extraction_uses_content_rules() {
        let root: Value = serde_json::from_str(
            r#"{
                "messages": [
                    {"role":"assistant","content":"무시"},
                    {"role":"user","content":[
                        "A",
                        {"text":"B"},
                        7,
                        true,
                        {"type":"image_url","image_url":{"url":"x"}}
                    ]}
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(extract_openai_prompt(&root).unwrap(), "AB");
    }

    #[test]
    fn mort_gemini_prompt_extraction_uses_latest_non_empty_parts_without_extra_separators() {
        let root: Value = serde_json::from_str(
            r#"{
                "contents": [
                    {"parts":[{"text":"old"}]},
                    {"parts":[{"text":"새"},{"text":"문장"},{"inlineData":{}}]}
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(extract_gemini_prompt(&root).unwrap(), "새문장");
    }
}
