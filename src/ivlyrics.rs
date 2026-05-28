use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::diagnostics;
use crate::prompt_config::PromptConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IvLyricsPromptKind {
    Translation,
    Phonetic,
    LyricsStudyQuiz,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct IvLyricsPromptRewriteResult {
    pub kind: IvLyricsPromptKind,
    pub prompt: String,
    pub line_count: usize,
    pub strip_number_tags_from_response: bool,
    pub source_lines: Vec<String>,
    pub original_prompt: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IvLyricsScopeIdentity {
    pub key: String,
    pub description: String,
}

pub fn build_scope_identity(kind: IvLyricsPromptKind, prompt: &str) -> IvLyricsScopeIdentity {
    let song = extract_learning_field(prompt, "Song").unwrap_or_default();
    let artist = extract_learning_field(prompt, "Artist").unwrap_or_default();
    if !song.trim().is_empty() || !artist.trim().is_empty() {
        let normalized_song = clean_inline(&song);
        let normalized_artist = clean_inline(&artist);
        let description = if normalized_artist.is_empty() {
            normalized_song.clone()
        } else if normalized_song.is_empty() {
            normalized_artist.clone()
        } else {
            format!("{normalized_artist} - {normalized_song}")
        };
        return IvLyricsScopeIdentity {
            key: format!(
                "meta:{}|{}",
                normalize_key(&normalized_artist),
                normalize_key(&normalized_song)
            ),
            description,
        };
    }

    if let Some(lyrics) = extract_lyrics(prompt) {
        let normalized = normalize_lyrics(&lyrics);
        if !normalized.is_empty() {
            let hash = diagnostics::fingerprint(&normalized);
            return IvLyricsScopeIdentity {
                key: format!("lyrics:{hash}"),
                description: format!("{kind:?} lyrics {hash}"),
            };
        }
    }

    if let Some(lines) = extract_learning_input_lines_json(prompt) {
        let normalized = normalize_lyrics(&lines);
        if !normalized.is_empty() {
            let hash = diagnostics::fingerprint(&normalized);
            return IvLyricsScopeIdentity {
                key: format!("study-lines:{hash}"),
                description: format!("study lines {hash}"),
            };
        }
    }

    let hash = diagnostics::fingerprint(prompt);
    IvLyricsScopeIdentity {
        key: format!("prompt:{hash}"),
        description: format!("{kind:?} prompt {hash}"),
    }
}

pub fn try_rewrite_translation(
    prompt: &str,
    config: &PromptConfig,
) -> Option<IvLyricsPromptRewriteResult> {
    if !looks_like_translation_prompt(prompt) {
        return None;
    }

    let mut lyrics = extract_lyrics(prompt)?;
    let line_count =
        normalize_line_count_and_pad(&mut lyrics, extract_translation_line_count(prompt));
    let target_language = extract_translation_target(prompt);
    let numbered = build_numbered_input_lines(&lyrics);
    let rewritten = if is_korean_translation_target(&target_language) {
        config.build_translation_prompt(&target_language, &numbered)
    } else {
        config.build_translation_prompt_english_instructions(&target_language, &numbered)
    };

    Some(IvLyricsPromptRewriteResult {
        kind: IvLyricsPromptKind::Translation,
        prompt: rewritten,
        line_count,
        strip_number_tags_from_response: true,
        source_lines: Vec::new(),
        original_prompt: prompt.to_owned(),
    })
}

pub fn try_rewrite_phonetic(prompt: &str) -> Option<IvLyricsPromptRewriteResult> {
    if !looks_like_phonetic_prompt(prompt) {
        return None;
    }

    let mut lyrics = extract_lyrics(prompt)?;
    let line_count = normalize_line_count_and_pad(&mut lyrics, extract_phonetic_line_count(prompt));
    let source_lines = split_lines_like_js(&lyrics);
    Some(IvLyricsPromptRewriteResult {
        kind: IvLyricsPromptKind::Phonetic,
        prompt: prompt.to_owned(),
        line_count,
        strip_number_tags_from_response: true,
        source_lines,
        original_prompt: prompt.to_owned(),
    })
}

#[allow(dead_code)]
pub fn try_rewrite_lyrics_study_quiz(prompt: &str) -> Option<IvLyricsPromptRewriteResult> {
    if !is_lyrics_study_quiz_prompt(prompt) {
        return None;
    }

    let input_lines_json = extract_learning_input_lines_json(prompt)?;
    let line_count = count_learning_input_lines(&input_lines_json);
    if line_count == 0 {
        return None;
    }

    let target_language = extract_learning_field(prompt, "Target explanation language")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Korean (한국어)".to_owned());
    let source_language = extract_learning_field(prompt, "Detected/source language")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "auto".to_owned());
    let song = extract_learning_field(prompt, "Song").unwrap_or_default();
    let artist = extract_learning_field(prompt, "Artist").unwrap_or_default();
    let difficulty = extract_learning_field(prompt, "Difficulty")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Normal".to_owned());
    let chunk = extract_learning_field(prompt, "Chunk")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "1/1".to_owned());

    let rewritten = format!(
        "You are a language-learning quiz generator for ivLyrics.\n\n\
Return ONLY valid JSON. No markdown, no comments, no code fences, no extra text.\n\
Output exactly this top-level shape:\n\
{{\"quiz\":[{{\"type\":\"meaning|blank|usage|rewrite|grammar\",\"question\":\"...\",\"choices\":[\"A\",\"B\",\"C\",\"D\"],\"answerIndex\":0,\"explanation\":\"...\",\"lineIndex\":0,\"reading\":\"\",\"pronunciation\":\"\"}}]}}\n\n\
Metadata:\n\
- Target explanation language: {target_language}\n\
- Source language: {source_language}\n\
- Song: {song}\n\
- Artist: {artist}\n\
- Difficulty: {difficulty}\n\
- Chunk: {chunk}\n\n\
Rules:\n\
- Create only 2-4 choice-based quiz items from the input lyric lines.\n\
- Write every human-readable question, choice, and explanation in {target_language}.\n\
- Use only these type values: meaning, blank, usage, rewrite, grammar.\n\
- Each item must have question, choices, answerIndex, explanation, and lineIndex.\n\
- choices must contain 2-4 non-empty strings. answerIndex must be a number from 0 to choices.length - 1.\n\
- Preserve the exact numeric lineIndex from the input line used by the item.\n\
- Show an actual short lyric phrase when a lyric matters. Do not say \"line 3\", \"3rd line\", or \"N번째 줄\".\n\
- For blank type, put ____ directly in the question and make choices short words or phrases.\n\
- Vary answerIndex. Do not place every correct answer at choices[0].\n\
- Vary type when the lyrics support it. Do not make all items meaning questions.\n\
- Include practical usage/rewrite questions when possible, but only when supported by the lyrics.\n\
- Repeated lyric phrases should produce at most one quiz item.\n\
- Add reading only for Japanese/kanji text when useful; otherwise use \"\".\n\
- Add pronunciation only when it helps; otherwise use \"\".\n\
- Omit unrelated top-level keys.\n\n\
Input lines JSON:\n{input_lines_json}"
    );

    Some(IvLyricsPromptRewriteResult {
        kind: IvLyricsPromptKind::LyricsStudyQuiz,
        prompt: rewritten,
        line_count,
        strip_number_tags_from_response: false,
        source_lines: Vec::new(),
        original_prompt: prompt.to_owned(),
    })
}

pub fn try_detect_kind(prompt: &str) -> Option<IvLyricsPromptKind> {
    if looks_like_translation_prompt(prompt) {
        Some(IvLyricsPromptKind::Translation)
    } else if looks_like_phonetic_prompt(prompt) {
        Some(IvLyricsPromptKind::Phonetic)
    } else if is_lyrics_study_quiz_prompt(prompt) {
        Some(IvLyricsPromptKind::LyricsStudyQuiz)
    } else {
        None
    }
}

pub fn try_detect_lyrics_study_category(prompt: &str) -> Option<String> {
    let lower = prompt.to_ascii_lowercase();
    let looks_like = lower.contains("build one category")
        || lower.contains("compact study pack")
        || lower.contains("ivlyrics study")
        || lower.contains("batched input requests json");

    if !lower.contains("language learning tutor inside a lyrics app") || !looks_like {
        return None;
    }

    let regex = Regex::new(r"(?im)^\s*Category:\s*(?P<category>[a-z][a-z0-9_-]*)\s*$").ok()?;
    regex
        .captures(prompt)
        .and_then(|c| c.name("category"))
        .map(|m| m.as_str().trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
}

pub fn is_lyrics_study_quiz_prompt(prompt: &str) -> bool {
    try_detect_lyrics_study_category(prompt)
        .map(|category| category.eq_ignore_ascii_case("quiz"))
        .unwrap_or(false)
}

pub fn strip_number_tags_from_response(response: &str) -> String {
    let fence = Regex::new(r"(?im)^\s*```[a-z0-9_-]*\s*$").unwrap();
    let tag = Regex::new(r"^\s*\[\d+\]\s*").unwrap();
    let normalized = response.replace("\r\n", "\n").replace('\r', "\n");
    let normalized = fence.replace_all(&normalized, "");
    normalized
        .split('\n')
        .map(|line| tag.replace(line, "").to_string())
        .collect::<Vec<_>>()
        .join("\n")
        .trim_matches(['\r', '\n'])
        .to_owned()
}

fn looks_like_translation_prompt(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    if lower.contains("you are a lyrics translator") && lower.contains("this is a translation task")
    {
        return true;
    }
    let line_count = Regex::new(
        r"(?is)\bTranslate\s+these\s+\d+\s+lines\s+of\s+song\s+lyrics\s+(?:into|to)\s+.+?\(.+?\)",
    )
    .unwrap();
    line_count.is_match(prompt)
        && Regex::new(r"(?im)^INPUT:\s*$").unwrap().is_match(prompt)
        && Regex::new(r"(?im)^OUTPUT\s*\(").unwrap().is_match(prompt)
        && !prompt.to_ascii_lowercase().contains("pronunciation")
}

fn looks_like_phonetic_prompt(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    if lower.contains("you are a pronunciation converter")
        && lower.contains("this is a pronunciation task")
    {
        return true;
    }
    let full = Regex::new(
        r"(?is)\b(?:Convert|Transcribe)\s+these\s+\d+\s+lines\s+of\s+lyrics\s+into\s+how\s+they\s+SOUND\b",
    )
    .unwrap();
    let simple = Regex::new(
        r"(?is)\b(?:Convert|Transcribe)\s+these\s+\d+\s+lines\s+of\s+lyrics\s+to\s+pronunciation\b",
    )
    .unwrap();
    (full.is_match(prompt) || simple.is_match(prompt))
        && Regex::new(r"(?im)^INPUT:\s*$").unwrap().is_match(prompt)
        && Regex::new(r"(?im)^OUTPUT\s*\(").unwrap().is_match(prompt)
}

fn extract_lyrics(prompt: &str) -> Option<String> {
    let input = Regex::new(r"(?im)^INPUT:\s*$").ok()?.find(prompt)?;
    let start = skip_line_break(prompt, input.end());
    let search = &prompt[start..];
    let example = Regex::new(r"(?im)^\s*Example:\s*$")
        .ok()?
        .find(search)
        .map(|m| m.start());
    let output = Regex::new(r"(?im)^OUTPUT\s*\(")
        .ok()?
        .find(search)
        .map(|m| m.start());
    let end = match (example, output) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    Some(search[..end].trim_end_matches(['\r', '\n']).to_owned())
}

fn extract_translation_line_count(prompt: &str) -> usize {
    capture_usize(
        prompt,
        r"(?is)\bTranslate\s+these\s+(?P<count>\d+)\s+lines\s+of\s+song\s+lyrics\s+(?:into|to)\s+.+?\(.+?\)",
    )
    .or_else(|| capture_usize(prompt, r"(?is)\bOUTPUT\s*\(\s*(?P<count>\d+)\s+lines\s+in\s+[^)]+\)"))
    .unwrap_or(0)
}

fn extract_phonetic_line_count(prompt: &str) -> usize {
    capture_usize(
        prompt,
        r"(?is)\b(?:Convert|Transcribe)\s+these\s+(?P<count>\d+)\s+lines\s+of\s+lyrics\s+into\s+how\s+they\s+SOUND\b",
    )
    .or_else(|| capture_usize(prompt, r"(?is)\bOUTPUT\s*\(\s*(?P<count>\d+)\s+lines\s*\)"))
    .unwrap_or(0)
}

fn extract_translation_target(prompt: &str) -> String {
    let header = Regex::new(
        r"(?is)\bTranslate\s+these\s+\d+\s+lines\s+of\s+song\s+lyrics\s+(?:into|to)\s+(?P<name>.+?)\s*\((?P<native>.+?)\)",
    )
    .unwrap();
    if let Some(caps) = header.captures(prompt) {
        let name = clean_inline(caps.name("name").map(|m| m.as_str()).unwrap_or(""));
        let native = clean_inline(caps.name("native").map(|m| m.as_str()).unwrap_or(""));
        if !name.is_empty() && !native.is_empty() {
            return format!("{name} ({native})");
        }
        if !native.is_empty() {
            return native;
        }
    }

    let output =
        Regex::new(r"(?is)\bOUTPUT\s*\(\s*\d+\s+lines\s+in\s+(?P<target>[^)]+)\)").unwrap();
    output
        .captures(prompt)
        .and_then(|c| c.name("target"))
        .map(|m| clean_inline(m.as_str()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Korean (한국어)".to_owned())
}

fn is_korean_translation_target(target: &str) -> bool {
    let normalized = clean_inline(target).to_ascii_lowercase();
    normalized.is_empty()
        || normalized == "ko"
        || normalized == "ko-kr"
        || normalized.contains("korean")
        || normalized.contains("한국")
        || normalized.contains("조선")
}

fn build_numbered_input_lines(lyrics: &str) -> String {
    split_lines_like_js(lyrics)
        .into_iter()
        .enumerate()
        .map(|(index, line)| format!("[{}] {}", index + 1, line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_line_count_and_pad(lyrics: &mut String, parsed_line_count: usize) -> usize {
    let mut actual = split_lines_like_js(lyrics).len();
    let line_count = if parsed_line_count > 0 {
        parsed_line_count
    } else {
        actual
    };
    while actual < line_count {
        lyrics.push('\n');
        actual += 1;
    }
    line_count
}

fn split_lines_like_js(value: &str) -> Vec<String> {
    value
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(ToOwned::to_owned)
        .collect()
}

fn extract_learning_field(prompt: &str, field: &str) -> Option<String> {
    let pattern = format!(
        r"(?im)^\s*{}\s*:\s*(?P<value>.*?)\s*$",
        regex::escape(field)
    );
    Regex::new(&pattern)
        .ok()?
        .captures(prompt)
        .and_then(|c| c.name("value"))
        .map(|m| clean_inline(m.as_str()))
}

fn extract_learning_input_lines_json(prompt: &str) -> Option<String> {
    for heading in [
        r"Input\s+lines(?:\s+JSON)?",
        r"Batched\s+input\s+requests\s+JSON",
    ] {
        let pattern = format!(r"(?im)^\s*{heading}\s*:\s*$");
        let Some(m) = Regex::new(&pattern).ok()?.find(prompt) else {
            continue;
        };
        let start = skip_line_break(prompt, m.end());
        let tail = prompt[start..].trim();
        let extracted = extract_balanced_json(tail).unwrap_or_else(|| tail.to_owned());
        return normalize_learning_input_lines_json(&extracted).or(Some(extracted));
    }
    None
}

fn count_learning_input_lines(input_lines_json: &str) -> usize {
    let Ok(serde_json::Value::Array(lines)) =
        serde_json::from_str::<serde_json::Value>(input_lines_json)
    else {
        return 0;
    };

    lines
        .iter()
        .filter(|line| looks_like_learning_line_object(line))
        .count()
}

fn normalize_learning_input_lines_json(json_text: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(json_text).ok()?;
    let mut lines = Vec::new();
    collect_learning_line_objects(&value, &mut lines);
    if lines.is_empty() {
        None
    } else {
        Some(serde_json::Value::Array(lines).to_string())
    }
}

fn collect_learning_line_objects(value: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
    match value {
        serde_json::Value::Array(items) => {
            if items.iter().all(looks_like_learning_line_object) {
                out.extend(items.iter().filter_map(canonical_learning_line_object));
                return;
            }
            for item in items {
                collect_learning_line_objects(item, out);
            }
        }
        serde_json::Value::Object(obj) => {
            if looks_like_learning_line_object(value) {
                if let Some(line) = canonical_learning_line_object(value) {
                    out.push(line);
                }
                return;
            }
            for name in ["inputLines", "input_lines", "lines", "items", "lyrics"] {
                if let Some(nested) = obj.get(name) {
                    collect_learning_line_objects(nested, out);
                }
            }
        }
        _ => {}
    }
}

fn looks_like_learning_line_object(value: &serde_json::Value) -> bool {
    let serde_json::Value::Object(obj) = value else {
        return false;
    };
    let has_index = obj.get("index").and_then(read_json_number).is_some()
        || obj.get("lineIndex").and_then(read_json_number).is_some()
        || obj.get("line_index").and_then(read_json_number).is_some();
    let has_text = obj
        .get("text")
        .or_else(|| obj.get("lyric"))
        .or_else(|| obj.get("sourceText"))
        .and_then(serde_json::Value::as_str)
        .map(|text| !text.trim().is_empty())
        .unwrap_or(false);
    has_index && has_text
}

fn canonical_learning_line_object(value: &serde_json::Value) -> Option<serde_json::Value> {
    let serde_json::Value::Object(obj) = value else {
        return None;
    };
    let index = obj
        .get("index")
        .and_then(read_json_number)
        .or_else(|| obj.get("lineIndex").and_then(read_json_number))
        .or_else(|| obj.get("line_index").and_then(read_json_number))?;
    let text = obj
        .get("text")
        .or_else(|| obj.get("lyric"))
        .or_else(|| obj.get("sourceText"))
        .and_then(serde_json::Value::as_str)?
        .trim();
    if text.is_empty() {
        return None;
    }

    let mut next = obj.clone();
    if !next.contains_key("index") {
        let index_value = if index.fract() == 0.0 {
            serde_json::Value::Number(serde_json::Number::from(index as i64))
        } else {
            serde_json::Number::from_f64(index)
                .map(serde_json::Value::Number)
                .unwrap_or_else(|| serde_json::Value::Number(serde_json::Number::from(0)))
        };
        next.insert("index".to_owned(), index_value);
    }
    if !next.contains_key("text") {
        next.insert(
            "text".to_owned(),
            serde_json::Value::String(text.to_owned()),
        );
    }
    Some(serde_json::Value::Object(next))
}

fn read_json_number(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
}

fn extract_balanced_json(text: &str) -> Option<String> {
    let start = text.find(['[', '{'])?;
    let chars: Vec<char> = text[start..].chars().collect();
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for (i, ch) in chars.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if *ch == '\\' {
                escaped = true;
            } else if *ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                if stack.pop() != Some(*ch) {
                    return None;
                }
                if stack.is_empty() {
                    return Some(chars[..=i].iter().collect());
                }
            }
            _ => {}
        }
    }
    None
}

fn capture_usize(text: &str, pattern: &str) -> Option<usize> {
    Regex::new(pattern)
        .ok()?
        .captures(text)
        .and_then(|c| c.name("count"))
        .and_then(|m| m.as_str().parse::<usize>().ok())
        .filter(|value| *value > 0)
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

fn clean_inline(value: &str) -> String {
    Regex::new(r"\s+")
        .unwrap()
        .replace_all(value, " ")
        .trim()
        .to_owned()
}

fn normalize_lyrics(value: &str) -> String {
    split_lines_like_js(value)
        .into_iter()
        .map(|line| clean_inline(&line))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_key(value: &str) -> String {
    clean_inline(value).to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_lyrics_study_quiz_prompt_to_canonical_json_task() {
        let prompt = "You are a language learning tutor inside a lyrics app.\n\
Category: quiz\n\
Build one category.\n\
Target explanation language: Korean (한국어)\n\
Detected/source language: Japanese\n\
Song: Test Song\n\
Artist: Test Artist\n\
Difficulty: Hard\n\
Chunk: 2/3\n\
Input lines:\n\
[{\"index\":5,\"text\":\"夜に駆ける\"},{\"index\":\"6\",\"text\":\"君は\"}]";

        let rewritten = try_rewrite_lyrics_study_quiz(prompt).unwrap();

        assert_eq!(rewritten.kind, IvLyricsPromptKind::LyricsStudyQuiz);
        assert_eq!(rewritten.line_count, 2);
        assert!(!rewritten.strip_number_tags_from_response);
        assert!(rewritten.prompt.contains("Return ONLY valid JSON"));
        assert!(rewritten.prompt.contains("Input lines JSON:"));
        assert!(rewritten.prompt.contains("\"index\":5"));
        assert_eq!(rewritten.original_prompt, prompt);
    }

    #[test]
    fn lyrics_study_quiz_rewrite_rejects_empty_line_context() {
        let prompt = "You are a language learning tutor inside a lyrics app.\n\
Category: quiz\n\
Build one category.\n\
Input lines:\n\
[{\"index\":1,\"text\":\"\"}]";

        assert!(try_rewrite_lyrics_study_quiz(prompt).is_none());
    }

    #[test]
    fn lyrics_study_quiz_rewrite_accepts_batched_input_requests_json() {
        let prompt = "You are a language learning tutor inside a lyrics app.\n\
Category: quiz\n\
Build one category.\n\
Batched input requests JSON:\n\
[{\"category\":\"quiz\",\"inputLines\":[{\"lineIndex\":7,\"sourceText\":\"夜に駆ける\"}]}]";

        let rewritten = try_rewrite_lyrics_study_quiz(prompt).unwrap();

        assert_eq!(rewritten.line_count, 1);
        assert!(rewritten.prompt.contains("\"index\":7"));
        assert!(rewritten.prompt.contains("\"text\":\"夜に駆ける\""));
    }

    #[test]
    fn detection_markers_are_case_insensitive() {
        let translation = "you are a lyrics translator\n\
this is a translation task\n\
INPUT:\n\
hello\n\
OUTPUT (1 lines in Korean)";
        let phonetic = "you are a pronunciation converter\n\
this is a pronunciation task\n\
INPUT:\n\
春\n\
OUTPUT (1 lines)";
        let study = "You are a LANGUAGE LEARNING TUTOR inside a lyrics app.\n\
Category: Quiz\n\
BUILD ONE CATEGORY.\n\
Input lines:\n\
[{\"index\":1,\"text\":\"hello\"}]";

        assert_eq!(
            try_detect_kind(translation),
            Some(IvLyricsPromptKind::Translation)
        );
        assert_eq!(
            try_detect_kind(phonetic),
            Some(IvLyricsPromptKind::Phonetic)
        );
        assert_eq!(
            try_detect_lyrics_study_category(study),
            Some("quiz".to_owned())
        );
    }

    #[test]
    fn translation_rewrite_pads_line_count_and_strips_number_tags() {
        let prompt = "Translate these 3 lines of song lyrics into Korean (한국어)\n\
INPUT:\n\
one\n\
two\n\
OUTPUT (3 lines in Korean)";
        let config = PromptConfig::default();
        let rewritten = try_rewrite_translation(prompt, &config).unwrap();

        assert_eq!(rewritten.line_count, 3);
        assert!(rewritten.strip_number_tags_from_response);
        assert!(rewritten.prompt.contains("[1] one"));
        assert!(rewritten.prompt.contains("[2] two"));
        assert!(rewritten.prompt.contains("[3]"));
    }

    #[test]
    fn scope_identity_prefers_song_artist_metadata() {
        let prompt = "You are a language learning tutor inside a lyrics app.\n\
Category: quiz\n\
Build one category.\n\
Song:  Test   Song \n\
Artist: Test   Artist\n\
Input lines:\n\
[{\"index\":1,\"text\":\"hello\"}]";
        let identity = build_scope_identity(IvLyricsPromptKind::LyricsStudyQuiz, prompt);

        assert_eq!(identity.key, "meta:test artist|test song");
        assert_eq!(identity.description, "Test Artist - Test Song");
    }
}
