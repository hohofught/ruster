use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::time::Duration;

use regex::{Captures, Regex};
use serde_json::{Map, Value, json};

use crate::ivlyrics::{self, IvLyricsPromptKind, IvLyricsPromptRewriteResult};
use crate::prompt_config::PromptConfig;

const MAX_QUIZ_ITEMS: usize = 12;
const PHONETIC_REPAIR_PROMPT_PREFIX: &str = "너 사람 말 무시해? 똑바로 안해?";

pub async fn repair_if_needed<F, Fut, L>(
    label: &str,
    request_id: u64,
    result: String,
    rewrite: Option<&IvLyricsPromptRewriteResult>,
    prompts: &PromptConfig,
    mut send_prompt: F,
    mut log: L,
) -> String
where
    F: FnMut(String, Duration) -> Fut,
    Fut: Future<Output = Result<String, String>>,
    L: FnMut(String),
{
    let Some(rewrite) = rewrite else {
        return result;
    };

    if result.trim().eq_ignore_ascii_case("Retry_Stale") {
        return result;
    }

    match rewrite.kind {
        IvLyricsPromptKind::Phonetic => {
            repair_phonetic(
                label,
                request_id,
                result,
                rewrite,
                prompts,
                &mut send_prompt,
                &mut log,
            )
            .await
        }
        IvLyricsPromptKind::LyricsStudyQuiz => {
            repair_quiz(
                label,
                request_id,
                result,
                rewrite,
                &mut send_prompt,
                &mut log,
            )
            .await
        }
        IvLyricsPromptKind::Translation => result,
    }
}

pub fn strip_number_tags_if_needed(
    result: String,
    rewrite: Option<&IvLyricsPromptRewriteResult>,
) -> String {
    if rewrite
        .map(|r| r.strip_number_tags_from_response)
        .unwrap_or(false)
    {
        ivlyrics::strip_number_tags_from_response(&result)
    } else {
        result
    }
}

async fn repair_phonetic<F, Fut, L>(
    label: &str,
    request_id: u64,
    response: String,
    rewrite: &IvLyricsPromptRewriteResult,
    prompts: &PromptConfig,
    send_prompt: &mut F,
    log: &mut L,
) -> String
where
    F: FnMut(String, Duration) -> Fut,
    Fut: Future<Output = Result<String, String>>,
    L: FnMut(String),
{
    if rewrite.line_count == 0 || rewrite.source_lines.is_empty() {
        return response;
    }

    let expected = rewrite.line_count;
    let source_lines = normalize_source_lines(&rewrite.source_lines, expected);
    let (mut output_lines, raw_line_count) = normalize_response_lines(&response, expected);
    let failed_indexes =
        find_failed_phonetic_lines(&output_lines, &source_lines, raw_line_count, expected);

    if failed_indexes.is_empty() {
        return output_lines.join("\n");
    }

    log(format!(
        "[{label}#{request_id}] ivLyrics phonetic repair attempt 1/1: {} line(s)",
        failed_indexes.len()
    ));

    let repair_prompt =
        build_phonetic_repair_prompt(rewrite, prompts, &source_lines, &failed_indexes);
    let repair_response = match send_prompt(repair_prompt, Duration::from_secs(150)).await {
        Ok(text) => text,
        Err(error) => {
            log(format!(
                "[{label}#{request_id}] ivLyrics phonetic repair skipped after request failure: {error}"
            ));
            return output_lines.join("\n");
        }
    };

    if repair_response.trim().starts_with("Retry_") {
        log(format!(
            "[{label}#{request_id}] ivLyrics phonetic repair skipped after retry token: {repair_response}"
        ));
        return output_lines.join("\n");
    }

    let (repair_lines, repair_raw_count) =
        normalize_response_lines(&repair_response, failed_indexes.len());
    for (local_index, original_index) in failed_indexes.into_iter().enumerate() {
        if local_index >= repair_raw_count {
            continue;
        }
        output_lines[original_index] = repair_lines[local_index].clone();
    }

    output_lines.join("\n")
}

async fn repair_quiz<F, Fut, L>(
    label: &str,
    request_id: u64,
    response: String,
    rewrite: &IvLyricsPromptRewriteResult,
    send_prompt: &mut F,
    log: &mut L,
) -> String
where
    F: FnMut(String, Duration) -> Fut,
    Fut: Future<Output = Result<String, String>>,
    L: FnMut(String),
{
    match try_normalize_quiz_json(&response, rewrite) {
        Ok(normalized) => return normalized,
        Err(error) => {
            log(format!(
                "[{label}#{request_id}] ivLyrics quiz local normalize failed: {error}"
            ));
        }
    }

    let repair_prompt = build_quiz_repair_prompt(rewrite, &response, "invalid quiz JSON");
    let repair_response = match send_prompt(repair_prompt, Duration::from_secs(120)).await {
        Ok(text) => text,
        Err(error) => {
            log(format!(
                "[{label}#{request_id}] ivLyrics quiz repair request failed: {error}"
            ));
            return empty_quiz_json();
        }
    };

    if repair_response.trim().starts_with("Retry_") {
        log(format!(
            "[{label}#{request_id}] ivLyrics quiz repair skipped: {repair_response}"
        ));
        return empty_quiz_json();
    }

    match try_normalize_quiz_json(&repair_response, rewrite) {
        Ok(normalized) => normalized,
        Err(error) => {
            log(format!(
                "[{label}#{request_id}] ivLyrics quiz repair normalize failed: {error}"
            ));
            empty_quiz_json()
        }
    }
}

fn normalize_source_lines(source_lines: &[String], expected: usize) -> Vec<String> {
    (0..expected)
        .map(|index| source_lines.get(index).cloned().unwrap_or_default())
        .collect()
}

fn normalize_response_lines(response: &str, expected: usize) -> (Vec<String>, usize) {
    let fence = Regex::new(r"(?i)^\s*```[a-z0-9_-]*\s*$").unwrap();
    let number_tag = Regex::new(r"^\s*\[\d+\]\s*").unwrap();
    let normalized = response.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = normalized
        .split('\n')
        .filter(|line| !fence.is_match(line))
        .map(|line| number_tag.replace(line, "").trim_end().to_owned())
        .collect::<Vec<_>>();

    while lines.len() > expected && lines.first().map(|s| s.trim().is_empty()).unwrap_or(false) {
        lines.remove(0);
    }
    while lines.len() > expected && lines.last().map(|s| s.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }

    let raw_count = lines.len().min(expected);
    lines.resize(expected, String::new());
    lines.truncate(expected);
    (lines, raw_count)
}

fn find_failed_phonetic_lines(
    output_lines: &[String],
    source_lines: &[String],
    raw_line_count: usize,
    expected: usize,
) -> Vec<usize> {
    (0..expected)
        .filter(|index| {
            *index >= raw_line_count
                || is_failed_phonetic_line(&output_lines[*index], &source_lines[*index])
        })
        .collect()
}

fn is_failed_phonetic_line(output_line: &str, source_line: &str) -> bool {
    let number_tag = Regex::new(r"^\s*\[\d+\]\s*").unwrap();
    let cleaned = number_tag.replace(output_line, "").trim().to_owned();
    if cleaned.is_empty() {
        return !source_line.trim().is_empty() && !is_allowed_marker_only_line(source_line);
    }

    if is_allowed_marker_only_line(&cleaned) {
        return false;
    }
    if contains_japanese_source_script(&cleaned) {
        return true;
    }

    let without_markers = allowed_marker_regex().replace_all(&cleaned, "");
    contains_latin_or_ipa_letters(&without_markers)
}

fn is_allowed_marker_only_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }
    allowed_marker_regex()
        .replace_all(trimmed, "")
        .trim()
        .is_empty()
}

fn allowed_marker_regex() -> Regex {
    Regex::new(r"(?:\[[^\]\r\n]{1,60}\]|\([A-Za-z0-9\s.'’!?,-]{1,60}\)|[♪♫♬]+)").unwrap()
}

fn contains_japanese_source_script(value: &str) -> bool {
    value.chars().any(|ch| {
        ('\u{3040}'..='\u{309F}').contains(&ch)
            || ('\u{30A0}'..='\u{30FF}').contains(&ch)
            || ('\u{FF66}'..='\u{FF9F}').contains(&ch)
            || ('\u{4E00}'..='\u{9FAF}').contains(&ch)
            || matches!(
                ch,
                '\u{3005}' | '\u{3007}' | '\u{30F5}' | '\u{30F6}' | '\u{30FC}'
            )
    })
}

fn contains_latin_or_ipa_letters(value: &str) -> bool {
    value.chars().any(|ch: char| {
        ch.is_alphabetic()
            && (ch.is_ascii_alphabetic()
                || ('\u{00C0}'..='\u{024F}').contains(&ch)
                || ('\u{0250}'..='\u{02AF}').contains(&ch)
                || ('\u{1D00}'..='\u{1D7F}').contains(&ch)
                || ('\u{1D80}'..='\u{1DBF}').contains(&ch))
    })
}

fn build_phonetic_repair_prompt(
    rewrite: &IvLyricsPromptRewriteResult,
    prompts: &PromptConfig,
    source_lines: &[String],
    failed_indexes: &[usize],
) -> String {
    let failed_lyrics = failed_indexes
        .iter()
        .filter_map(|index| source_lines.get(*index))
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = try_build_prompt_from_original(
        &rewrite.original_prompt,
        &failed_lyrics,
        failed_indexes.len(),
    )
    .unwrap_or_else(|| {
        prompts.build_ivlyrics_phonetic_prompt(
            failed_indexes.len(),
            failed_lyrics.trim_end(),
            "Korean (한국어)",
        )
    });
    format!("{PHONETIC_REPAIR_PROMPT_PREFIX}\n\n{prompt}")
}

fn try_build_prompt_from_original(
    original_prompt: &str,
    replacement_lyrics: &str,
    replacement_line_count: usize,
) -> Option<String> {
    if original_prompt.trim().is_empty() {
        return None;
    }

    let input = Regex::new(r"(?im)^INPUT:\s*$")
        .ok()?
        .find(original_prompt)?;
    let start = skip_line_break(original_prompt, input.end());
    let search = &original_prompt[start..];
    let example = Regex::new(r"(?im)^\s*Example:\s*$")
        .ok()?
        .find(search)
        .map(|m| m.start());
    let output = Regex::new(r"(?im)^OUTPUT\s*(?:\(|EXACTLY\b)")
        .ok()?
        .find(search)
        .map(|m| m.start());
    let end = match (example, output) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };

    let before = &original_prompt[..start];
    let after = search[end..].trim_start_matches(['\r', '\n']);
    let prompt = format!("{before}{}\n\n{after}", replacement_lyrics.trim_end());
    Some(rewrite_line_counts(&prompt, replacement_line_count))
}

fn rewrite_line_counts(prompt: &str, line_count: usize) -> String {
    let prompt = Regex::new(r"(?i)\b(Convert|Transcribe)\s+these\s+\d+\s+lines")
        .unwrap()
        .replace_all(prompt, |caps: &Captures| {
            format!("{} these {} lines", &caps[1], line_count)
        })
        .to_string();

    let prompt =
        rewrite_digits_in_matches(&prompt, r"(?i)\bOutput\s+EXACTLY\s+\d+\s+lines", line_count);
    let prompt = rewrite_digits_in_matches(&prompt, r"(?i)\bOUTPUT\s*\(\s*\d+\s+lines", line_count);
    rewrite_digits_in_matches(&prompt, r"(?i)\bOUTPUT\s+EXACTLY\s+\d+\s+LINES", line_count)
}

fn rewrite_digits_in_matches(prompt: &str, pattern: &str, line_count: usize) -> String {
    let digit = Regex::new(r"\d+").unwrap();
    Regex::new(pattern)
        .unwrap()
        .replace_all(prompt, |caps: &Captures| {
            digit
                .replace_all(&caps[0], line_count.to_string().as_str())
                .to_string()
        })
        .to_string()
}

fn try_normalize_quiz_json(
    response: &str,
    rewrite: &IvLyricsPromptRewriteResult,
) -> Result<String, String> {
    let mut payload =
        extract_json_payload(response).ok_or_else(|| "no json object or array found".to_owned())?;
    payload = strip_trailing_commas(&payload);

    let root: Value =
        serde_json::from_str(&payload).map_err(|error| format!("invalid json: {error}"))?;
    let source_by_index = extract_source_lines_by_index(rewrite);
    let mut seen_sources = HashSet::new();
    let mut seen_questions = HashSet::new();
    let mut quiz_items = Vec::new();

    for raw_item in enumerate_quiz_objects(&root) {
        let Some((item, source_key, question_key)) =
            normalize_quiz_item(&raw_item, &source_by_index)
        else {
            continue;
        };

        if !source_key.is_empty() && seen_sources.contains(&source_key) {
            continue;
        }
        if source_key.is_empty()
            && !question_key.is_empty()
            && seen_questions.contains(&question_key)
        {
            continue;
        }
        if !source_key.is_empty() {
            seen_sources.insert(source_key);
        }
        if !question_key.is_empty() {
            seen_questions.insert(question_key);
        }

        quiz_items.push(Value::Object(item));
        if quiz_items.len() >= MAX_QUIZ_ITEMS {
            break;
        }
    }

    if quiz_items.is_empty() {
        return Err("no valid quiz items".to_owned());
    }

    Ok(json!({ "quiz": quiz_items }).to_string())
}

pub fn normalize_study_json_or_empty(response: &str, category: &str) -> (String, bool, String) {
    let category = category.trim().to_ascii_lowercase();
    if category.is_empty() {
        return (response.to_owned(), false, String::new());
    }

    if category == "quiz" {
        let rewrite = IvLyricsPromptRewriteResult {
            kind: IvLyricsPromptKind::LyricsStudyQuiz,
            prompt: String::new(),
            line_count: 0,
            strip_number_tags_from_response: false,
            source_lines: Vec::new(),
            original_prompt: String::new(),
        };
        if let Ok(normalized) = try_normalize_quiz_json(response, &rewrite) {
            return (normalized, false, String::new());
        }
    }

    match parse_study_json(response).and_then(|root| normalize_study_parsed_json(&root, &category))
    {
        Ok(normalized) => (normalized, false, String::new()),
        Err(detail) => (empty_study_shape(&category), true, detail),
    }
}

fn parse_study_json(response: &str) -> Result<Value, String> {
    let payload =
        extract_json_payload(response).ok_or_else(|| "no json object or array found".to_owned())?;
    let payload = strip_trailing_commas(&payload);
    serde_json::from_str::<Value>(&payload).map_err(|error| format!("invalid json: {error}"))
}

fn normalize_study_parsed_json(root: &Value, category: &str) -> Result<String, String> {
    let root = unwrap_json_string(root);
    if category == "summary" {
        return if root.is_object() {
            Ok(root.to_string())
        } else {
            Err("summary result is not a JSON object".to_owned())
        };
    }

    let (output_key, aliases): (&str, &[&str]) = match category {
        "lines" => ("lines", &["lines", "items", "data"]),
        "expressions" => (
            "keyExpressions",
            &["keyExpressions", "expressions", "items", "data"],
        ),
        "quiz" => ("quiz", &["quiz", "quizzes", "questions", "items", "data"]),
        _ => {
            return if root.is_object() {
                Ok(root.to_string())
            } else {
                Err("unknown study result is not a JSON object".to_owned())
            };
        }
    };

    let array = extract_study_array(&root, aliases)
        .ok_or_else(|| "missing expected top-level study array".to_owned())?;
    Ok(json!({ output_key: array }).to_string())
}

fn extract_study_array(root: &Value, aliases: &[&str]) -> Option<Value> {
    match root {
        Value::Array(_) => Some(root.clone()),
        Value::Object(obj) => {
            for alias in aliases {
                let candidate = get_property(obj, &[*alias]).map(unwrap_json_string);
                if let Some(Value::Array(_)) = candidate {
                    return candidate;
                }
            }
            None
        }
        _ => None,
    }
}

fn empty_study_shape(category: &str) -> String {
    match category {
        "summary" => json!({ "summary": "", "keyPoints": [] }).to_string(),
        "lines" => json!({ "lines": [] }).to_string(),
        "expressions" => json!({ "keyExpressions": [] }).to_string(),
        "quiz" => json!({ "quiz": [] }).to_string(),
        _ => json!({}).to_string(),
    }
}

fn normalize_quiz_item(
    raw_item: &Map<String, Value>,
    source_by_index: &HashMap<i64, String>,
) -> Option<(Map<String, Value>, String, String)> {
    let question = clean_text(&get_string_from_obj(
        raw_item,
        &["question", "prompt", "text"],
    ));
    if question.is_empty() {
        return None;
    }

    let choices = get_choices(raw_item);
    if choices.len() < 2 {
        return None;
    }

    let answer_index = get_answer_index(raw_item, &choices);
    let quiz_type = normalize_quiz_type(&get_string_from_obj(
        raw_item,
        &["type", "quizType", "kind", "category"],
    ));
    let explanation = clean_text(&get_string_from_obj(
        raw_item,
        &["explanation", "reason", "note", "feedback"],
    ));
    let reading = clean_text(&get_string_from_obj(
        raw_item,
        &["reading", "kana", "furigana"],
    ));
    let pronunciation = clean_text(&get_string_from_obj(
        raw_item,
        &["pronunciation", "pronounce", "ipa", "sound"],
    ));
    let line_index = get_line_index(raw_item, source_by_index);
    let mut source_text = clean_text(&get_string_from_obj(
        raw_item,
        &["sourceText", "source", "lyric", "lyricText"],
    ));
    if source_text.is_empty()
        && let Some(index) = line_index
        && let Some(source) = source_by_index.get(&index)
    {
        source_text = clean_text(source);
    }

    let mut item = Map::new();
    item.insert("type".to_owned(), Value::String(quiz_type));
    item.insert("question".to_owned(), Value::String(question.clone()));
    item.insert(
        "choices".to_owned(),
        Value::Array(choices.iter().cloned().map(Value::String).collect()),
    );
    item.insert("answerIndex".to_owned(), Value::from(answer_index as i64));
    item.insert("explanation".to_owned(), Value::String(explanation));
    item.insert(
        "lineIndex".to_owned(),
        line_index.map(Value::from).unwrap_or(Value::Null),
    );
    item.insert("sourceText".to_owned(), Value::String(source_text.clone()));
    item.insert("reading".to_owned(), Value::String(reading));
    item.insert("pronunciation".to_owned(), Value::String(pronunciation));

    let quoted_text = if source_text.is_empty() {
        extract_quoted_quiz_text(&question)
    } else {
        String::new()
    };
    let source_key = normalize_comparable_text(if source_text.is_empty() {
        &quoted_text
    } else {
        &source_text
    });
    let question_key = normalize_comparable_text(&question);
    Some((item, source_key, question_key))
}

fn enumerate_quiz_objects(root: &Value) -> Vec<Map<String, Value>> {
    let container = unwrap_json_string(root);
    match container {
        Value::Object(obj) => {
            for name in ["quiz", "quizzes", "items", "questions"] {
                if let Some(candidate) = get_property(&obj, &[name]) {
                    match unwrap_json_string(candidate) {
                        Value::Array(array) => {
                            return array
                                .into_iter()
                                .filter_map(|value| match value {
                                    Value::Object(item) => Some(item),
                                    _ => None,
                                })
                                .collect();
                        }
                        Value::Object(item) if looks_like_quiz_item(&item) => return vec![item],
                        _ => {}
                    }
                }
            }

            if looks_like_quiz_item(&obj) {
                vec![obj]
            } else {
                Vec::new()
            }
        }
        Value::Array(array) => array
            .into_iter()
            .filter_map(|value| match value {
                Value::Object(item) => Some(item),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn looks_like_quiz_item(obj: &Map<String, Value>) -> bool {
    get_property(obj, &["question", "prompt", "text"]).is_some()
        && get_property(obj, &["choices", "options", "answers"]).is_some()
}

fn unwrap_json_string(value: &Value) -> Value {
    let Value::String(text) = value else {
        return value.clone();
    };
    let Some(payload) = extract_json_payload(text) else {
        return value.clone();
    };
    let payload = strip_trailing_commas(&payload);
    serde_json::from_str(&payload).unwrap_or_else(|_| value.clone())
}

fn get_choices(obj: &Map<String, Value>) -> Vec<String> {
    let mut choices = Vec::new();
    match get_property(obj, &["choices", "options", "answers"]) {
        Some(Value::Array(array)) => {
            for item in array {
                add_choice(&mut choices, &get_string(item));
            }
        }
        Some(Value::Object(choice_obj)) => {
            for name in ["A", "B", "C", "D", "a", "b", "c", "d"] {
                if let Some(value) = get_property(choice_obj, &[name]) {
                    add_choice(&mut choices, &get_string(value));
                }
            }
            if choices.is_empty() {
                for value in choice_obj.values() {
                    add_choice(&mut choices, &get_string(value));
                }
            }
        }
        Some(value) => {
            let splitter = Regex::new(r"\r?\n|\s*\|\s*|\s*;\s*").unwrap();
            for part in splitter.split(&get_string(value)) {
                add_choice(&mut choices, part);
            }
        }
        None => {}
    }

    let mut seen = HashSet::new();
    choices
        .into_iter()
        .filter(|choice| !choice.is_empty() && seen.insert(choice.clone()))
        .take(4)
        .collect()
}

fn add_choice(choices: &mut Vec<String>, value: &str) {
    let value = clean_text(value);
    if !value.is_empty() {
        choices.push(value);
    }
}

fn get_answer_index(obj: &Map<String, Value>, choices: &[String]) -> usize {
    if let Some(index) = get_int(get_property(
        obj,
        &["answerIndex", "correctIndex", "correctChoiceIndex"],
    )) {
        if index >= 0 && (index as usize) < choices.len() {
            return index as usize;
        }
        if index == choices.len() as i64 {
            return choices.len().saturating_sub(1);
        }
    }

    let answer = clean_text(&get_string_from_obj(
        obj,
        &[
            "answer",
            "answerText",
            "correct",
            "correctAnswer",
            "correctChoice",
        ],
    ));
    if !answer.is_empty() {
        let upper = answer.trim().to_ascii_uppercase();
        if upper.len() == 1
            && let Some(index) = "ABCD".find(&upper)
            && index < choices.len()
        {
            return index;
        }
        let normalized_answer = normalize_comparable_text(&answer);
        for (index, choice) in choices.iter().enumerate() {
            if normalize_comparable_text(choice) == normalized_answer {
                return index;
            }
        }
    }

    0
}

fn get_line_index(obj: &Map<String, Value>, source_by_index: &HashMap<i64, String>) -> Option<i64> {
    get_int(get_property(
        obj,
        &["lineIndex", "index", "sourceLineIndex"],
    ))
    .or_else(|| {
        if source_by_index.len() == 1 {
            source_by_index.keys().next().copied()
        } else {
            None
        }
    })
}

fn normalize_quiz_type(value: &str) -> String {
    let normalized = Regex::new(r"[_\s-]+")
        .unwrap()
        .replace_all(value, "")
        .trim()
        .to_ascii_lowercase();
    match normalized.as_str() {
        "blank" | "fillblank" | "fillintheblank" | "cloze" => "blank",
        "usage" | "context" | "situation" | "transfer" => "usage",
        "rewrite" | "rephrase" | "paraphrase" => "rewrite",
        "grammar" | "form" | "structure" => "grammar",
        _ => "meaning",
    }
    .to_owned()
}

fn extract_source_lines_by_index(rewrite: &IvLyricsPromptRewriteResult) -> HashMap<i64, String> {
    let mut result = HashMap::new();
    for prompt in [&rewrite.original_prompt, &rewrite.prompt] {
        let Some(json_text) = extract_input_lines_json(prompt) else {
            continue;
        };
        let Ok(Value::Array(lines)) = serde_json::from_str::<Value>(&json_text) else {
            continue;
        };
        for line in lines {
            let Value::Object(obj) = line else {
                continue;
            };
            let Some(index) = get_int(get_property(&obj, &["index"])) else {
                continue;
            };
            let text = get_string_from_obj(&obj, &["text"]);
            if !text.trim().is_empty() {
                result.entry(index).or_insert(text);
            }
        }
    }
    result
}

fn extract_input_lines_json(prompt: &str) -> Option<String> {
    let m = Regex::new(r"(?im)^\s*Input\s+lines(?:\s+JSON)?\s*:\s*$")
        .ok()?
        .find(prompt)?;
    let start = skip_line_break(prompt, m.end());
    extract_balanced_json(prompt[start..].trim(), None)
}

fn extract_json_payload(response: &str) -> Option<String> {
    let cleaned = strip_code_fences(response)
        .trim_matches(['\u{FEFF}', '\r', '\n', ' ', '\t'])
        .to_owned();
    if cleaned.is_empty() {
        return None;
    }

    extract_balanced_json(&cleaned, Some('{'))
        .or_else(|| extract_balanced_json(&cleaned, Some('[')))
}

fn strip_code_fences(value: &str) -> String {
    let fence = Regex::new(r"(?i)^\s*```[a-z0-9_-]*\s*$").unwrap();
    value
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .filter(|line| !fence.is_match(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_trailing_commas(value: &str) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    let mut out = String::with_capacity(value.len());
    let mut in_string = false;
    let mut escaped = false;

    for (index, ch) in chars.iter().copied().enumerate() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
            out.push(ch);
            continue;
        }

        if ch == ',' {
            let mut next = index + 1;
            while next < chars.len() && chars[next].is_whitespace() {
                next += 1;
            }
            if next < chars.len() && matches!(chars[next], '}' | ']') {
                continue;
            }
        }

        out.push(ch);
    }

    out
}

fn extract_balanced_json(text: &str, preferred_opening: Option<char>) -> Option<String> {
    let start = text.char_indices().find_map(|(index, ch)| {
        if preferred_opening
            .map(|opening| ch == opening)
            .unwrap_or(ch == '[' || ch == '{')
        {
            Some(index)
        } else {
            None
        }
    })?;

    let mut expected_closings = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => expected_closings.push('}'),
            '[' => expected_closings.push(']'),
            '}' | ']' => {
                if expected_closings.pop() != Some(ch) {
                    return None;
                }
                if expected_closings.is_empty() {
                    return Some(
                        text[start..start + offset + ch.len_utf8()]
                            .trim()
                            .to_owned(),
                    );
                }
            }
            _ => {}
        }
    }
    None
}

fn get_property<'a>(obj: &'a Map<String, Value>, names: &[&str]) -> Option<&'a Value> {
    for name in names {
        if let Some(value) = obj.get(*name) {
            return Some(value);
        }
    }
    obj.iter()
        .find(|(key, _)| names.iter().any(|name| key.eq_ignore_ascii_case(name)))
        .map(|(_, value)| value)
}

fn get_string_from_obj(obj: &Map<String, Value>, names: &[&str]) -> String {
    get_property(obj, names).map(get_string).unwrap_or_default()
}

fn get_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(boolean) => boolean.to_string(),
        _ => String::new(),
    }
}

fn get_int(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|value| value.round() as i64)),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn clean_text(value: &str) -> String {
    let value = value.replace("\r\n", "\n").replace('\r', "\n");
    let value = Regex::new(r"[\u{200B}-\u{200D}\u{FEFF}]")
        .unwrap()
        .replace_all(&value, "");
    Regex::new(r"\s+")
        .unwrap()
        .replace_all(&value, " ")
        .trim()
        .to_owned()
}

fn normalize_comparable_text(value: &str) -> String {
    let cleaned = clean_text(value).to_ascii_lowercase();
    let punctuation_replaced = cleaned
        .chars()
        .map(|ch| {
            if is_comparable_punctuation(ch) {
                ' '
            } else {
                ch
            }
        })
        .collect::<String>();
    Regex::new(r"\s+")
        .unwrap()
        .replace_all(&punctuation_replaced, " ")
        .trim()
        .to_owned()
}

fn is_comparable_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '"' | '\''
            | '`'
            | '“'
            | '”'
            | '‘'
            | '’'
            | '「'
            | '」'
            | '『'
            | '』'
            | ','
            | '.'
            | ';'
            | ':'
            | '!'
            | '?'
            | '！'
            | '？'
            | '。'
            | '、'
            | '，'
            | '·'
            | '・'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '<'
            | '>'
            | '/'
            | '\\'
            | '|'
            | '~'
            | '～'
            | '_'
            | '+'
            | '='
            | '-'
    )
}

fn extract_quoted_quiz_text(question: &str) -> String {
    Regex::new("[\"“”'‘’「『](?P<text>[^\"“”'‘’」』]{2,120})[\"“”'‘’」』]")
        .unwrap()
        .captures(question)
        .and_then(|captures| captures.name("text"))
        .map(|m| m.as_str().to_owned())
        .unwrap_or_default()
}

fn build_quiz_repair_prompt(
    rewrite: &IvLyricsPromptRewriteResult,
    broken_output: &str,
    error: &str,
) -> String {
    format!(
        "Convert the following model output into valid ivLyrics quiz JSON only.\n\
Return ONLY JSON. No markdown, no code fences, no comments.\n\n\
Required shape:\n\
{{\"quiz\":[{{\"type\":\"meaning|blank|usage|rewrite|grammar\",\"question\":\"...\",\"choices\":[\"A\",\"B\",\"C\",\"D\"],\"answerIndex\":0,\"explanation\":\"...\",\"lineIndex\":0,\"reading\":\"\",\"pronunciation\":\"\"}}]}}\n\n\
Rules:\n\
- Keep or fix as many quiz items as possible.\n\
- choices must contain 2-4 non-empty strings.\n\
- answerIndex must be a zero-based number inside the choices array.\n\
- type must be one of meaning, blank, usage, rewrite, grammar.\n\
- For blank type, question must contain ____.\n\
- Preserve lineIndex if present or inferable from the original task.\n\
- Omit unrelated top-level keys.\n\n\
Original task:\n{}\n\n\
Parser error:\n{}\n\n\
Broken output:\n{}",
        limit(&rewrite.prompt, 8000),
        limit(error, 1000),
        limit(broken_output, 12000)
    )
}

fn limit(value: &str, max_length: usize) -> String {
    value.chars().take(max_length).collect()
}

fn empty_quiz_json() -> String {
    "{\"quiz\":[]}".to_owned()
}

fn skip_line_break(value: &str, mut index: usize) -> usize {
    let bytes = value.as_bytes();
    if index < bytes.len() && bytes[index] == b'\r' {
        index += 1;
    }
    if index < bytes.len() && bytes[index] == b'\n' {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phonetic_rewrite(line_count: usize) -> IvLyricsPromptRewriteResult {
        IvLyricsPromptRewriteResult {
            kind: IvLyricsPromptKind::Phonetic,
            prompt:
                "Convert these 2 lines of lyrics to pronunciation\nINPUT:\n春\n愛\nOUTPUT (2 lines)"
                    .to_owned(),
            line_count,
            strip_number_tags_from_response: true,
            source_lines: vec!["春".to_owned(), "愛".to_owned()],
            original_prompt:
                "Convert these 2 lines of lyrics to pronunciation\nINPUT:\n春\n愛\nOUTPUT (2 lines)"
                    .to_owned(),
        }
    }

    #[test]
    fn phonetic_normalization_pads_and_strips_number_tags() {
        let (lines, raw_count) = normalize_response_lines("[1] 봄\n", 2);

        assert_eq!(raw_count, 2);
        assert_eq!(lines, vec!["봄".to_owned(), String::new()]);
    }

    #[test]
    fn phonetic_failure_detects_japanese_and_latin_output() {
        assert!(is_failed_phonetic_line("春", "春"));
        assert!(is_failed_phonetic_line("haru", "春"));
        assert!(!is_failed_phonetic_line("하루", "春"));
        assert!(!is_failed_phonetic_line("♪", "♪"));
    }

    #[tokio::test]
    async fn phonetic_repair_replaces_failed_lines() {
        let prompts = PromptConfig::default();
        let rewrite = phonetic_rewrite(2);
        let result = repair_if_needed(
            "Test",
            1,
            "春\n아이".to_owned(),
            Some(&rewrite),
            &prompts,
            |prompt, _timeout| async move {
                assert!(prompt.starts_with(PHONETIC_REPAIR_PROMPT_PREFIX));
                assert!(prompt.contains("春"));
                Ok("하루".to_owned())
            },
            |_message| {},
        )
        .await;

        assert_eq!(result, "하루\n아이");
    }

    #[test]
    fn quiz_normalization_extracts_fenced_json_and_canonical_shape() {
        let rewrite = IvLyricsPromptRewriteResult {
            kind: IvLyricsPromptKind::LyricsStudyQuiz,
            prompt: "Input lines JSON:\n[{\"index\":3,\"text\":\"夜に駆ける\"}]".to_owned(),
            line_count: 1,
            strip_number_tags_from_response: false,
            source_lines: Vec::new(),
            original_prompt: "Input lines JSON:\n[{\"index\":3,\"text\":\"夜に駆ける\"}]"
                .to_owned(),
        };
        let normalized = try_normalize_quiz_json(
            "```json\n{\"questions\":[{\"prompt\":\"뜻은?\",\"options\":{\"A\":\"밤으로 달려\",\"B\":\"아침\"},\"correct\":\"A\",\"kind\":\"meaning\",\"index\":3}]}\n```",
            &rewrite,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&normalized).unwrap();

        assert_eq!(value["quiz"][0]["lineIndex"], 3);
        assert_eq!(value["quiz"][0]["sourceText"], "夜に駆ける");
        assert_eq!(value["quiz"][0]["answerIndex"], 0);
    }

    #[test]
    fn quiz_normalization_handles_aliases_trailing_commas_and_answer_text() {
        let rewrite = IvLyricsPromptRewriteResult {
            kind: IvLyricsPromptKind::LyricsStudyQuiz,
            prompt: "Input lines JSON:\n[{\"index\":8,\"text\":\"惨めな馬鹿女って\"}]".to_owned(),
            line_count: 1,
            strip_number_tags_from_response: false,
            source_lines: Vec::new(),
            original_prompt: "Input lines JSON:\n[{\"index\":8,\"text\":\"惨めな馬鹿女って\"}]"
                .to_owned(),
        };
        let normalized = try_normalize_quiz_json(
            r#"{
                "quiz": [{
                    "text": "빈칸에 들어갈 말은?",
                    "answers": "미지메 | 바카 | 온나",
                    "answerText": "바카",
                    "category": "fill-in-the-blank",
                    "lineIndex": 8,
                },],
            }"#,
            &rewrite,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&normalized).unwrap();

        assert_eq!(value["quiz"][0]["type"], "blank");
        assert_eq!(
            value["quiz"][0]["choices"],
            json!(["미지메", "바카", "온나"])
        );
        assert_eq!(value["quiz"][0]["answerIndex"], 1);
        assert_eq!(value["quiz"][0]["sourceText"], "惨めな馬鹿女って");
    }

    #[test]
    fn quiz_normalization_deduplicates_same_source() {
        let rewrite = IvLyricsPromptRewriteResult {
            kind: IvLyricsPromptKind::LyricsStudyQuiz,
            prompt: "Input lines JSON:\n[{\"index\":1,\"text\":\"hello\"}]".to_owned(),
            line_count: 1,
            strip_number_tags_from_response: false,
            source_lines: Vec::new(),
            original_prompt: "Input lines JSON:\n[{\"index\":1,\"text\":\"hello\"}]".to_owned(),
        };
        let normalized = try_normalize_quiz_json(
            r#"[
                {"question":"뜻?","choices":["안녕","잘가"],"answerIndex":0,"sourceText":"hello"},
                {"question":"다른 질문","choices":["안녕","잘가"],"answerIndex":1,"sourceText":"hello"}
            ]"#,
            &rewrite,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&normalized).unwrap();

        assert_eq!(value["quiz"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn study_lines_normalization_accepts_cli_raw_shape() {
        let (normalized, used_empty_fallback, detail) = normalize_study_json_or_empty(
            r#"{
                "lines": [{
                    "index": 18,
                    "reading": "あぁ おぼえのあるあいのことば",
                    "pronunciation": "/aː oboe no aru ai no kotoba/",
                    "translation": "기억나는 사랑의 말"
                }]
            }"#,
            "lines",
        );
        let value: Value = serde_json::from_str(&normalized).unwrap();

        assert!(!used_empty_fallback, "{detail}");
        assert_eq!(value["lines"].as_array().unwrap().len(), 1);
        assert_eq!(value["lines"][0]["index"], 18);
    }

    #[test]
    fn strip_number_tags_if_needed_matches_rewrite_flag() {
        let rewrite = phonetic_rewrite(1);
        assert_eq!(
            strip_number_tags_if_needed("[1] 하나".to_owned(), Some(&rewrite)),
            "하나"
        );
    }
}
