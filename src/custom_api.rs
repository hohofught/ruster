use std::collections::HashMap;
use std::path::PathBuf;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    build_default_translation_prompt: bool,
) -> String {
    let Some(preset) = preset else {
        if build_default_translation_prompt {
            return build_default_mort_translation_prompt(original_text, source_code, result_code);
        }
        return original_text.to_owned();
    };
    if preset.request_template.trim().is_empty() {
        return original_text.to_owned();
    }

    let rendered = render_request_template(
        &preset.request_template,
        original_text,
        source_code,
        result_code,
    );
    try_extract_prompt_from_rendered_request(&rendered).unwrap_or(rendered)
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

    let rendered = render_response_template(
        &preset.response_template,
        original_text,
        result,
        source_code,
        result_code,
    );
    if serde_json::from_str::<Value>(&rendered).is_ok() {
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

fn render_request_template(
    template: &str,
    ocr_text: &str,
    source_code: &str,
    result_code: &str,
) -> String {
    if !looks_like_structured_template(template) {
        return replace_plain_tokens(template, ocr_text, "", source_code, result_code, ocr_text);
    }

    let rendered =
        replace_json_template_tokens(template, ocr_text, "", source_code, result_code, ocr_text);
    normalize_object_template_to_json(&rendered)
}

fn render_response_template(
    template: &str,
    ocr_text: &str,
    result_text: &str,
    source_code: &str,
    result_code: &str,
) -> String {
    let rendered = replace_json_template_tokens(
        template,
        ocr_text,
        result_text,
        source_code,
        result_code,
        ocr_text,
    );
    normalize_object_template_to_json(&rendered)
}

fn replace_plain_tokens(
    template: &str,
    ocr_text: &str,
    result_text: &str,
    source_code: &str,
    result_code: &str,
    raw_prompt: &str,
) -> String {
    template
        .replace("{OCR_TEXT}", ocr_text)
        .replace("{RESULT_TEXT}", result_text)
        .replace("{SOURCE_CODE}", source_code)
        .replace("{RESULT_CODE}", result_code)
        .replace("{RAW_PROMPT}", raw_prompt)
}

fn replace_json_template_tokens(
    template: &str,
    ocr_text: &str,
    result_text: &str,
    source_code: &str,
    result_code: &str,
    raw_prompt: &str,
) -> String {
    let mut out = template.to_owned();
    for (token, value) in [
        ("{OCR_TEXT}", ocr_text),
        ("{RESULT_TEXT}", result_text),
        ("{SOURCE_CODE}", source_code),
        ("{RESULT_CODE}", result_code),
        ("{RAW_PROMPT}", raw_prompt),
    ] {
        out = replace_json_template_token(&out, token, value);
    }
    out
}

fn replace_json_template_token(template: &str, token: &str, value: &str) -> String {
    if template.is_empty() || !template.contains(token) {
        return template.to_owned();
    }

    let mut out = String::with_capacity(template.len() + value.len().min(256));
    let mut search_start = 0;
    while let Some(offset) = template[search_start..].find(token) {
        let index = search_start + offset;
        out.push_str(&template[search_start..index]);
        if is_inside_quoted_string(template, index) {
            out.push_str(&escape_json_string_content(value));
        } else {
            out.push_str(&serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned()));
        }
        search_start = index + token.len();
    }
    out.push_str(&template[search_start..]);
    out
}

fn escape_json_string_content(value: &str) -> String {
    let json = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned());
    json.strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or("")
        .to_owned()
}

fn is_inside_quoted_string(value: &str, token_index: usize) -> bool {
    let mut in_string = false;
    let mut escaped = false;
    for ch in value[..token_index].chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            _ => {}
        }
    }
    in_string
}

fn looks_like_structured_template(template: &str) -> bool {
    let trimmed = template.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return true;
    }

    Regex::new(r#"(?s)^\s*"?[A-Za-z0-9_ .-]+"?\s*[:=]"#)
        .ok()
        .is_some_and(|re| re.is_match(template))
}

fn normalize_object_template_to_json(template: &str) -> String {
    let mut content = template.trim().to_owned();
    if content.is_empty() {
        return "{}".to_owned();
    }

    if (content.starts_with('{') || content.starts_with('[')) && parse_json_value(&content).is_ok()
    {
        return content;
    }

    if !content.starts_with('{') && !content.starts_with('[') {
        content = format!("{{{content}}}");
        if parse_json_value(&content).is_ok() {
            return content;
        }
    }

    if content.starts_with('[') {
        return content;
    }

    let inner = content
        .strip_prefix('{')
        .and_then(|v| v.strip_suffix('}'))
        .unwrap_or(&content);
    let parts = split_top_level(inner, ',');
    let mut json_parts = Vec::new();
    for property in parts {
        if let Some((key, value)) = parse_template_property(&property) {
            if !key.trim().is_empty() {
                let key_json =
                    serde_json::to_string(key.trim()).unwrap_or_else(|_| "\"\"".to_owned());
                json_parts.push(format!("{key_json}: {value}"));
            }
        }
    }

    format!("{{{}}}", json_parts.join(", "))
}

fn parse_template_property(property: &str) -> Option<(String, String)> {
    let property = property.trim();
    if property.is_empty() {
        return None;
    }

    for separator in [':', '='] {
        if let Some(index) = find_top_level_separator(property, separator) {
            let key = property[..index].trim().trim_matches('"').to_owned();
            let value = normalize_template_value(property[index + 1..].trim());
            return Some((key, value));
        }
    }

    None
}

fn normalize_template_value(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return "\"\"".to_owned();
    }
    if parse_json_value(value).is_ok() {
        return value.to_owned();
    }
    if value.starts_with('[') && value.ends_with(']') {
        let inner = &value[1..value.len() - 1];
        let values = split_top_level(inner, ',')
            .into_iter()
            .map(|value| normalize_template_value(&value))
            .collect::<Vec<_>>();
        return format!("[{}]", values.join(", "));
    }
    if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("false") {
        return value.to_ascii_lowercase();
    }
    if value.eq_ignore_ascii_case("null") {
        return "null".to_owned();
    }
    if value.parse::<f64>().is_ok() {
        return value.to_owned();
    }

    serde_json::to_string(value.trim_matches('"')).unwrap_or_else(|_| "\"\"".to_owned())
}

fn parse_json_value(value: &str) -> serde_json::Result<Value> {
    serde_json::from_str::<Value>(value)
}

fn find_top_level_separator(value: &str, separator: char) -> Option<usize> {
    let mut in_string = false;
    let mut escaped = false;
    let mut depth = 0i32;
    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && in_string {
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match ch {
            '[' | '{' => depth += 1,
            ']' | '}' => depth -= 1,
            _ if ch == separator && depth == 0 => return Some(index),
            _ => {}
        }
    }
    None
}

fn split_top_level(value: &str, separator: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut depth = 0i32;

    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && in_string {
            current.push(ch);
            escaped = true;
            continue;
        }
        if ch == '"' {
            current.push(ch);
            in_string = !in_string;
            continue;
        }
        if !in_string {
            match ch {
                '[' | '{' => depth += 1,
                ']' | '}' => depth -= 1,
                _ if ch == separator && depth == 0 => {
                    if !current.trim().is_empty() {
                        out.push(current.trim().to_owned());
                    }
                    current.clear();
                    continue;
                }
                _ => {}
            }
        }
        current.push(ch);
    }

    if !current.trim().is_empty() {
        out.push(current.trim().to_owned());
    }
    out
}

fn try_extract_prompt_from_rendered_request(rendered: &str) -> Option<String> {
    let root = serde_json::from_str::<Value>(rendered).ok()?;
    first_string(&root, &["text", "prompt", "q", "input"])
        .or_else(|| extract_openai_prompt(&root))
        .or_else(|| extract_gemini_prompt(&root))
}

fn build_default_mort_translation_prompt(
    original_text: &str,
    source_code: &str,
    result_code: &str,
) -> String {
    let source = normalize_language_label(source_code, "auto-detected language");
    let target = normalize_language_label(result_code, "Korean");
    format!(
        "You are a translation engine for OCR/game text. Translate from {source} to {target}.\n\
Return only the translated text. Preserve line breaks and do not add explanations.\n\n\
TEXT:\n{original_text}"
    )
}

fn normalize_language_label(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return fallback.to_owned();
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "ko" | "kor" | "kr" | "ko-kr" => "Korean".to_owned(),
        "ja" | "jp" | "jpn" | "ja-jp" => "Japanese".to_owned(),
        "en" | "eng" | "en-us" | "en-gb" => "English".to_owned(),
        "zh" | "zho" | "zh-cn" | "cn" => "Chinese".to_owned(),
        "zh-tw" | "tw" => "Traditional Chinese".to_owned(),
        "fr" | "fra" => "French".to_owned(),
        "de" | "deu" => "German".to_owned(),
        "es" | "spa" => "Spanish".to_owned(),
        "ru" | "rus" => "Russian".to_owned(),
        _ => trimmed.to_owned(),
    }
}

fn first_string(root: &serde_json::Value, names: &[&str]) -> Option<String> {
    for name in names {
        if let Some(value) = get_property_ignore_case(root, name) {
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

fn get_property_ignore_case<'a>(root: &'a Value, name: &str) -> Option<&'a Value> {
    let Value::Object(obj) = root else {
        return None;
    };
    obj.iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value)
}

pub fn extract_openai_prompt(root: &serde_json::Value) -> Option<String> {
    let messages = get_property_ignore_case(root, "messages")?.as_array()?;
    for message in messages.iter().rev() {
        if message
            .as_object()
            .and_then(|_| get_property_ignore_case(message, "role"))
            .and_then(|v| v.as_str())
            .map(|role| !role.eq_ignore_ascii_case("user"))
            .unwrap_or(false)
        {
            continue;
        }
        if let Some(text) = extract_content_text(get_property_ignore_case(message, "content")?)
            && !text.trim().is_empty()
        {
            return Some(text);
        }
    }
    None
}

pub fn extract_gemini_prompt(root: &serde_json::Value) -> Option<String> {
    let contents = get_property_ignore_case(root, "contents")?.as_array()?;
    for content in contents.iter().rev() {
        let Some(parts) = get_property_ignore_case(content, "parts").and_then(|v| v.as_array())
        else {
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
        serde_json::Value::Object(_) => get_property_ignore_case(value, "text")
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
    fn custom_request_template_extracts_openai_user_prompt_after_json_render() {
        let preset = CustomApiPreset {
            request_template:
                r#"{"messages":[{"role":"system","content":"ignore"},{"role":"user","content":"{OCR_TEXT}"}]}"#
                    .to_owned(),
            ..Default::default()
        };

        let prompt = build_prompt("line \"one\"\nline two", Some(&preset), "ja", "ko", false);

        assert_eq!(prompt, "line \"one\"\nline two");
    }

    #[test]
    fn custom_request_template_normalizes_object_fragment_and_extracts_text() {
        let preset = CustomApiPreset {
            request_template: r#""text":"{OCR_TEXT}","source":"{SOURCE_CODE}""#.to_owned(),
            ..Default::default()
        };

        let prompt = build_prompt("번역 대상", Some(&preset), "ja", "ko", false);

        assert_eq!(prompt, "번역 대상");
    }

    #[test]
    fn mort_cli_raw_default_prompt_wraps_plain_text_for_translation() {
        let prompt = build_prompt("こんにちは", None, "ja", "ko", true);

        assert!(prompt.contains("Translate from Japanese to Korean"));
        assert!(prompt.contains("TEXT:\nこんにちは"));
    }

    #[test]
    fn invalid_custom_response_template_falls_back_to_mort_json() {
        let preset = CustomApiPreset {
            response_template: "[".to_owned(),
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
