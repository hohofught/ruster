use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::app_paths::AppPaths;
use crate::logging::LogBuffer;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PromptConfig {
    pub translation: TranslationPromptConfig,
    pub phonetic: PhoneticPromptConfig,
    #[serde(rename = "ivLyrics")]
    pub iv_lyrics: IvLyricsPromptConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TranslationPromptConfig {
    pub prefix: String,
    pub system_prompt: String,
    pub rules: Vec<String>,
    pub english_prefix: String,
    pub english_system_prompt: String,
    pub english_rules: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PhoneticPromptConfig {
    pub prefix: String,
    pub system_prompt: String,
    pub rules: Vec<String>,
    pub english_note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct IvLyricsPromptConfig {
    pub phonetic_template: String,
    pub translation_template: String,
}

impl Default for TranslationPromptConfig {
    fn default() -> Self {
        Self {
            prefix: "Keep every [N] tag and output only translated lyric lines.".to_owned(),
            system_prompt: "당신은 원곡의 맛을 살리는 전문 작사가입니다.".to_owned(),
            rules: Vec::new(),
            english_prefix: "Keep every [N] tag and output only translated lyric lines.".to_owned(),
            english_system_prompt: "You are a professional lyric adapter.".to_owned(),
            english_rules: Vec::new(),
        }
    }
}

impl Default for PhoneticPromptConfig {
    fn default() -> Self {
        Self {
            prefix: "[N] 태그가 붙은 가사 발음 변환 작업이다.".to_owned(),
            system_prompt: "너는 발음 변환기다.".to_owned(),
            rules: Vec::new(),
            english_note: String::new(),
        }
    }
}

impl Default for IvLyricsPromptConfig {
    fn default() -> Self {
        Self {
            phonetic_template: "STRICT PRONUNCIATION TASK.\n{lyrics}".to_owned(),
            translation_template:
                "Translate these {lineCount} lines to {targetLanguage}.\n{lyrics}".to_owned(),
        }
    }
}

impl PromptConfig {
    pub fn load(paths: &AppPaths, logs: &LogBuffer) -> Self {
        if paths.prompt_override_path().exists()
            && let Ok(text) = std::fs::read_to_string(paths.prompt_override_path())
            && let Ok(config) = serde_json::from_str::<PromptConfig>(&text)
        {
            logs.push("[PromptConfig] 사용자 prompts.json 로드 완료");
            return config;
        }

        let bundled = include_str!("../assets/prompts.json");
        match serde_json::from_str::<PromptConfig>(bundled) {
            Ok(config) => config,
            Err(error) => {
                logs.push(format!(
                    "[PromptConfig] 내장 prompts.json 로드 실패: {error}"
                ));
                PromptConfig::default()
            }
        }
    }

    pub fn default_config() -> Self {
        serde_json::from_str::<PromptConfig>(include_str!("../assets/prompts.json"))
            .unwrap_or_else(|_| PromptConfig::default())
    }

    pub fn editable_document(&self) -> String {
        let mut out = String::new();
        out.push_str("# ruster prompt document\n");
        out.push_str(
            "# @@ key 줄은 섹션 이름입니다. 섹션 이름은 바꾸지 말고, 그 아래 내용만 편집하세요.\n",
        );
        out.push_str("# @@ key lines are section names. Do not rename them; edit only the text below each section.\n");
        out.push_str("# 저장 시 이 문서는 prompts.json으로 변환됩니다.\n");
        out.push_str("# When saved, this document is converted back to prompts.json.\n\n");

        append_document_block(&mut out, "translation.prefix", &self.translation.prefix);
        append_document_block(
            &mut out,
            "translation.systemPrompt",
            &self.translation.system_prompt,
        );
        append_document_blocks(&mut out, "translation.rules", &self.translation.rules);
        append_document_block(
            &mut out,
            "translation.englishPrefix",
            &self.translation.english_prefix,
        );
        append_document_block(
            &mut out,
            "translation.englishSystemPrompt",
            &self.translation.english_system_prompt,
        );
        append_document_blocks(
            &mut out,
            "translation.englishRules",
            &self.translation.english_rules,
        );

        append_document_block(&mut out, "phonetic.prefix", &self.phonetic.prefix);
        append_document_block(
            &mut out,
            "phonetic.systemPrompt",
            &self.phonetic.system_prompt,
        );
        append_document_blocks(&mut out, "phonetic.rules", &self.phonetic.rules);
        append_document_block(
            &mut out,
            "phonetic.englishNote",
            &self.phonetic.english_note,
        );

        append_document_block(
            &mut out,
            "ivLyrics.phoneticTemplate",
            &self.iv_lyrics.phonetic_template,
        );
        append_document_block(
            &mut out,
            "ivLyrics.translationTemplate",
            &self.iv_lyrics.translation_template,
        );

        out.trim_end().to_owned() + "\n"
    }

    pub fn default_editable_document() -> String {
        Self::default_config().editable_document()
    }

    pub fn save_user_override_document(
        paths: &AppPaths,
        document: &str,
    ) -> Result<PromptConfig, String> {
        if document.trim().is_empty() {
            return Err("프롬프트 문서가 비어 있습니다.".to_owned());
        }

        let config = if document.trim_start().starts_with('{') {
            serde_json::from_str::<PromptConfig>(document)
                .map_err(|error| format!("프롬프트 JSON 형식이 올바르지 않습니다: {error}"))?
        } else {
            parse_editable_document(document)?
        };

        paths.ensure_data_dir();
        let json = serde_json::to_string_pretty(&config)
            .map_err(|error| format!("프롬프트 JSON 직렬화 실패: {error}"))?;
        std::fs::write(paths.prompt_override_path(), json)
            .map_err(|error| format!("프롬프트 저장 실패: {error}"))?;
        Ok(config)
    }

    pub fn build_translation_prompt(&self, action: &str, lines_to_send: &str) -> String {
        let rules = self.translation.rules.join("\n");
        format!(
            "{}\nTarget language: {}.\n{} {}\n\nINPUT_LINES_START\n{}\nINPUT_LINES_END\nOUTPUT_LINES_START",
            self.translation.prefix,
            action,
            self.translation.system_prompt,
            rules,
            lines_to_send.trim_end()
        )
    }

    pub fn build_translation_prompt_english_instructions(
        &self,
        target_language: &str,
        lines_to_send: &str,
    ) -> String {
        let rules = self.translation.english_rules.join("\n");
        format!(
            "{}\nTarget language: {}.\n{} {}\n\nINPUT_LINES_START\n{}\nINPUT_LINES_END\nOUTPUT_LINES_START",
            self.translation.english_prefix,
            target_language,
            self.translation.english_system_prompt,
            rules,
            lines_to_send.trim_end()
        )
    }

    #[allow(dead_code)]
    pub fn build_phonetic_prompt(&self, is_english: bool, lines_to_send: &str) -> String {
        let mut header = self.phonetic.prefix.clone();
        if !self.phonetic.system_prompt.trim().is_empty() {
            header.push('\n');
            header.push_str(&self.phonetic.system_prompt);
        }
        if !self.phonetic.rules.is_empty() {
            header.push('\n');
            header.push_str(&self.phonetic.rules.join("\n"));
        }
        if is_english && !self.phonetic.english_note.trim().is_empty() {
            header.push_str("\nEnglish lyric rule: ");
            header.push_str(&self.phonetic.english_note);
        }

        format!(
            "{header}\n\nINPUT_LINES_START\n{}\nINPUT_LINES_END\nOUTPUT_LINES_START",
            lines_to_send.trim_end()
        )
    }

    #[allow(dead_code)]
    pub fn build_ivlyrics_phonetic_prompt(
        &self,
        line_count: usize,
        lyrics: &str,
        target_language: &str,
    ) -> String {
        let target_language = if target_language.trim().is_empty() {
            "Korean"
        } else {
            target_language.trim()
        };
        if !is_korean_pronunciation_target(target_language) {
            return build_ivlyrics_phonetic_prompt_english_instructions(
                line_count,
                lyrics,
                target_language,
            );
        }

        let script_instruction = if target_language.contains("English")
            || target_language == "en"
            || target_language.contains("영어")
        {
            "영어 가사도 로마자가 아니라 한국어 화자용 한글 발음으로만 출력한다."
        } else {
            "한국어 화자용 한글 발음으로만 출력한다."
        };

        self.iv_lyrics
            .phonetic_template
            .replace("{lineCount}", &line_count.to_string())
            .replace("{scriptInstruction}", script_instruction)
            .replace("{lyrics}", lyrics)
    }

    #[allow(dead_code)]
    pub fn build_ivlyrics_translation_prompt(
        &self,
        line_count: usize,
        lyrics: &str,
        target_language: &str,
    ) -> String {
        self.iv_lyrics
            .translation_template
            .replace("{lineCount}", &line_count.to_string())
            .replace("{targetLanguage}", target_language)
            .replace("{lyrics}", lyrics)
    }
}

fn append_document_blocks(out: &mut String, key: &str, values: &[String]) {
    for value in values {
        append_document_block(out, key, value);
    }
}

fn append_document_block(out: &mut String, key: &str, value: &str) {
    out.push_str("@@ ");
    out.push_str(key);
    out.push('\n');
    out.push_str(value.replace("\r\n", "\n").replace('\r', "\n").trim_end());
    out.push_str("\n\n");
}

fn parse_editable_document(document: &str) -> Result<PromptConfig, String> {
    let mut sections: HashMap<String, Vec<String>> = HashMap::new();
    let mut current_key: Option<String> = None;
    let mut current_value = String::new();
    let normalized = document.replace("\r\n", "\n").replace('\r', "\n");

    for line in normalized.split('\n') {
        if let Some(key) = line.strip_prefix("@@ ") {
            flush_document_block(&mut sections, current_key.take(), &mut current_value);
            let key = key.trim();
            if !is_known_document_key(key) {
                return Err(format!("알 수 없는 섹션입니다: {key}"));
            }
            current_key = Some(key.to_owned());
            continue;
        }

        if current_key.is_none() {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            return Err("@@ 섹션 헤더 앞에는 주석과 빈 줄만 둘 수 있습니다.".to_owned());
        }

        current_value.push_str(line);
        current_value.push('\n');
    }

    flush_document_block(&mut sections, current_key, &mut current_value);

    Ok(PromptConfig {
        translation: TranslationPromptConfig {
            prefix: get_required_document_value(&sections, "translation.prefix")?,
            system_prompt: get_required_document_value(&sections, "translation.systemPrompt")?,
            rules: get_document_values(&sections, "translation.rules"),
            english_prefix: get_required_document_value(&sections, "translation.englishPrefix")?,
            english_system_prompt: get_required_document_value(
                &sections,
                "translation.englishSystemPrompt",
            )?,
            english_rules: get_document_values(&sections, "translation.englishRules"),
        },
        phonetic: PhoneticPromptConfig {
            prefix: get_required_document_value(&sections, "phonetic.prefix")?,
            system_prompt: get_required_document_value(&sections, "phonetic.systemPrompt")?,
            rules: get_document_values(&sections, "phonetic.rules"),
            english_note: get_required_document_value(&sections, "phonetic.englishNote")?,
        },
        iv_lyrics: IvLyricsPromptConfig {
            phonetic_template: get_required_document_value(&sections, "ivLyrics.phoneticTemplate")?,
            translation_template: get_required_document_value(
                &sections,
                "ivLyrics.translationTemplate",
            )?,
        },
    })
}

fn flush_document_block(
    sections: &mut HashMap<String, Vec<String>>,
    key: Option<String>,
    value: &mut String,
) {
    let Some(key) = key else {
        return;
    };
    sections
        .entry(key)
        .or_default()
        .push(value.trim_end_matches('\n').to_owned());
    value.clear();
}

fn get_required_document_value(
    sections: &HashMap<String, Vec<String>>,
    key: &str,
) -> Result<String, String> {
    let Some(values) = sections.get(key) else {
        return Err(format!("필수 섹션이 없습니다: {key}"));
    };
    if values.len() != 1 {
        return Err(format!("하나만 있어야 하는 섹션이 중복되었습니다: {key}"));
    }
    Ok(values[0].clone())
}

fn get_document_values(sections: &HashMap<String, Vec<String>>, key: &str) -> Vec<String> {
    sections.get(key).cloned().unwrap_or_default()
}

fn is_known_document_key(key: &str) -> bool {
    matches!(
        key,
        "translation.prefix"
            | "translation.systemPrompt"
            | "translation.rules"
            | "translation.englishPrefix"
            | "translation.englishSystemPrompt"
            | "translation.englishRules"
            | "phonetic.prefix"
            | "phonetic.systemPrompt"
            | "phonetic.rules"
            | "phonetic.englishNote"
            | "ivLyrics.phoneticTemplate"
            | "ivLyrics.translationTemplate"
    )
}

fn build_ivlyrics_phonetic_prompt_english_instructions(
    line_count: usize,
    lyrics: &str,
    target_language: &str,
) -> String {
    let script_instruction = if is_english_pronunciation_target(target_language) {
        "Use Latin alphabet romanization.".to_owned()
    } else {
        format!("Write pronunciation in the script normally used by {target_language} speakers.")
    };

    format!(
        "STRICT PRONUNCIATION TASK.\nTarget pronunciation audience/script: {target_language}.\n{script_instruction}\n\nOutput contract:\n- Output EXACTLY {line_count} lines, one output line for each input line, in the same order.\n- Keep empty input lines as empty output lines.\n- Do NOT add line numbers, prefixes, bullets, quotes, explanations, titles, summaries, JSON, Markdown, or code blocks.\n- Do NOT merge, split, drop, reorder, complete, or create lyrics.\n- Keep music symbols and section markers like [Chorus], [Verse], (Yeah), and (Oh) as-is.\n- For non-{target_language} sung lyric text, output pronunciation only, not meaning.\n- Do not output the original lyrics unchanged unless a line is already valid for the requested pronunciation script.\n\nINPUT:\n{}\n\nOUTPUT EXACTLY {line_count} LINES:",
        lyrics.trim_end()
    )
}

fn is_korean_pronunciation_target(target_language: &str) -> bool {
    let normalized = normalize_language_target(target_language);
    normalized == "ko"
        || normalized == "ko-kr"
        || normalized.contains("korean")
        || normalized.contains("hangul")
        || normalized.contains("hangeul")
        || normalized.contains("한국")
        || normalized.contains("한글")
        || normalized.contains("조선")
}

fn is_english_pronunciation_target(target_language: &str) -> bool {
    let normalized = normalize_language_target(target_language);
    normalized == "en"
        || normalized == "en-us"
        || normalized == "en-gb"
        || normalized.contains("english")
        || normalized.contains("latin alphabet")
        || normalized.contains("romanization")
        || normalized.contains("romanisation")
        || normalized.contains("영어")
}

fn normalize_language_target(target_language: &str) -> String {
    target_language.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_prompts_json_keeps_ivlyrics_sections() {
        let config: PromptConfig = serde_json::from_str(include_str!("../assets/prompts.json"))
            .expect("bundled prompts.json should deserialize");

        assert!(
            config
                .iv_lyrics
                .phonetic_template
                .contains("STRICT PRONUNCIATION TASK")
        );
        assert!(
            config
                .iv_lyrics
                .phonetic_template
                .contains("{scriptInstruction}")
        );
        assert!(
            config
                .iv_lyrics
                .translation_template
                .contains("{targetLanguage}")
        );
        assert!(config.translation.prefix.contains("[N]"));
    }

    #[test]
    fn ivlyrics_prompt_builders_replace_all_placeholders() {
        let config = PromptConfig {
            iv_lyrics: IvLyricsPromptConfig {
                phonetic_template: "{lineCount}|{scriptInstruction}|{lyrics}|{lineCount}"
                    .to_owned(),
                translation_template: "{lineCount}|{targetLanguage}|{lyrics}".to_owned(),
            },
            ..Default::default()
        };

        let phonetic = config.build_ivlyrics_phonetic_prompt(2, "hello", "Korean");
        assert!(phonetic.contains("2|"));
        assert!(phonetic.contains("한국어 화자용 한글 발음으로만"));
        assert!(phonetic.ends_with("|hello|2"));

        assert_eq!(
            config.build_ivlyrics_translation_prompt(3, "가사", "Korean (한국어)"),
            "3|Korean (한국어)|가사"
        );
    }

    #[test]
    fn editable_prompt_document_round_trips_repeated_rule_sections() {
        let config = PromptConfig::default_config();
        let document = config.editable_document();
        let parsed = parse_editable_document(&document).unwrap();

        assert_eq!(parsed.translation.prefix, config.translation.prefix);
        assert_eq!(parsed.translation.rules, config.translation.rules);
        assert_eq!(parsed.phonetic.rules, config.phonetic.rules);
        assert_eq!(
            parsed.iv_lyrics.phonetic_template,
            config.iv_lyrics.phonetic_template
        );
    }

    #[test]
    fn editable_prompt_document_rejects_unknown_sections() {
        let document = PromptConfig::default_config()
            .editable_document()
            .replace("@@ phonetic.prefix", "@@ phonetic.unknown");

        let error = parse_editable_document(&document).unwrap_err();
        assert!(error.contains("알 수 없는 섹션"));
    }

    #[test]
    fn ivlyrics_non_korean_pronunciation_target_uses_global_prompt() {
        let config = PromptConfig::default_config();
        let prompt = config.build_ivlyrics_phonetic_prompt(2, "かな\nsong", "English");

        assert!(prompt.contains("Target pronunciation audience/script: English"));
        assert!(prompt.contains("Use Latin alphabet romanization."));
        assert!(!prompt.contains("한국어 화자용 한글 발음으로만"));
    }
}
