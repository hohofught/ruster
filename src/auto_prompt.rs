use std::time::Duration;

use regex::Regex;

use crate::app_paths::AppPaths;
use crate::host::TranslatorHost;
use crate::ivlyrics::IvLyricsTranslationRewriteInput;
use crate::logging::{LogBuffer, summarize_text};
use crate::prompt_config::PromptConfig;
use crate::settings::AppSettings;

const PRESET_A_ID: &str = "embedded:japanese-expressive-translation-optimized";
const PRESET_C_ID: &str = "embedded:long-lyric-translation";
const PRESET_D_ID: &str = "embedded:integrated-concise-lyric-translation";
const PRESET_E_ID: &str = "default";
const MAX_CLASSIFIER_INPUT_CHARS: usize = 12_000;

pub async fn build_translation_prompt(
    paths: &AppPaths,
    host: &TranslatorHost,
    settings: &AppSettings,
    default_config: &PromptConfig,
    input: &IvLyricsTranslationRewriteInput,
    logs: &LogBuffer,
) -> String {
    if !settings.iv_lyrics_auto_prompt_selection_enabled {
        return build_prompt(default_config, input, "");
    }

    let decision = classify(host, input, logs).await;
    let config =
        PromptConfig::create_preset_config(paths, &decision.preset_id).unwrap_or_else(|error| {
            logs.push(format!(
                "[AutoPrompt] 프리셋 로드 실패 - 기본 프롬프트 사용 ({error})"
            ));
            default_config.clone()
        });
    let prompt = build_prompt(&config, input, &decision.append_instruction);

    logs.push(format!(
        "[AutoPrompt] selected={} source={} append={}",
        decision.code,
        decision.source,
        summarize_text(&decision.append_instruction, 80)
    ));
    prompt
}

async fn classify(
    host: &TranslatorHost,
    input: &IvLyricsTranslationRewriteInput,
    logs: &LogBuffer,
) -> AutoPromptDecision {
    let classifier_prompt = build_classifier_prompt(&input.target_language, &input.numbered_lyrics);
    let response = match host
        .send_raw_prompt(&classifier_prompt, Duration::from_secs(90))
        .await
    {
        Ok(response) => response,
        Err(error) => {
            logs.push(format!(
                "[AutoPrompt] GPT 판정 요청 실패 - E 폴백 ({error})"
            ));
            return AutoPromptDecision::fallback("classifier-request-failed");
        }
    };

    if let Some(decision) = parse_decision(&response) {
        return decision;
    }

    logs.push(format!(
        "[AutoPrompt] 판정 응답 파싱 실패 - E 폴백 ({})",
        summarize_text(&response, 120)
    ));
    AutoPromptDecision::fallback("classifier-parse-failed")
}

fn build_classifier_prompt(target_language: &str, numbered_lyrics: &str) -> String {
    let sample = trim_classifier_input(numbered_lyrics);
    format!(
        "너는 ChatGPT 기반 ivLyrics 번역 프롬프트 라우터다.\n\
아래 가사의 언어, 분위기, 장르, 수위, 말맛을 보고 Gemini/ChatGPT 번역기에 보낼 A/C/D/E 프롬프트 중 하나를 고른다.\n\n\
반드시 딱 두 줄만 출력한다.\n\
prompt: a\n\
append: 없음\n\n\
선택지:\n\
a = 일본어 감성 번역 최적화. 의미 왜곡이 적고 서사/원문 정보/수위 절제가 중요할 때. 일본어/J-pop/순정/우울/일반 감성곡에 우선.\n\
c = 긴거 프롬프트. 복잡한 한국어 구어체 현지화, 혼재 언어, 강한 장르감, 슬랭/성적 은어/말장난/자동자막 의심 등 폭넓은 보정이 필요할 때.\n\
d = 통합 실험 프롬프트. A의 의미 보존을 유지하면서 C의 자연스러운 한국어 말투, 감정선, 가창성만 적당히 적용하면 충분한 일반/중간 난도 곡.\n\
e = 기본/lite 프롬프트. 최신 lite 기준의 안정형 가사 로컬라이즈. 일반곡에서 의미 보존, 관계성 보존, 화자 캐릭터, 번역투 제거가 중요하지만 C/D급 실험이나 강한 현지화는 필요 없을 때.\n\n\
판정 원칙:\n\
- 대다수 곡은 a, d, e 중 하나다.\n\
- 일본어 감성, 서사 보존, 원문 정보 보존이 핵심이고 과한 한국어 재가공이 필요 없으면 a.\n\
- c는 d로는 부족할 만큼 한국어 현지화, 단어 보정, 혼재 언어, 강한 수위/슬랭/펀치라인/영미권 랩 분석이 많이 필요할 때만 고른다.\n\
- d는 A의 의미 보존과 C의 자연어 후처리를 함께 강하게 적용해야 하는 실험형 중간 난도 곡에 고른다.\n\
- e는 가장 안정적인 기본값이다. 보통의 일본어/영어/혼재 가사에서 관계성 보존, 화자 말투, 번역투 제거가 핵심이면 e.\n\
- 애매하면 e.\n\n\
append 규칙:\n\
- append는 이 곡에만 붙일 짧은 추가 지시다. 곡 분위기, 화자 말투, 중요한 단어, 수위, 라임감 중 필요한 것만 쓴다.\n\
- 0~1문장, 최대 120자.\n\
- 전역 규칙을 반복하지 말고, 이 곡에서 특히 조심할 톤/수위/말투만 쓴다.\n\
- 추가 지시가 필요 없으면 정확히 `없음`이라고 쓴다.\n\
- 원문에 없는 의미, 미래형, 추측형, 과한 욕설, 과한 인터넷어를 권하지 않는다.\n\n\
Target language: {target_language}\n\n\
LYRICS_START\n\
{sample}\n\
LYRICS_END\n\n\
출력은 딱 두 줄:\n\
prompt: a|c|d|e\n\
append: ..."
    )
}

fn parse_decision(response: &str) -> Option<AutoPromptDecision> {
    if response.trim().is_empty() {
        return None;
    }
    let fence = Regex::new(r"(?im)^\s*```[a-z0-9_-]*\s*$").ok()?;
    let normalized = response.replace("\r\n", "\n").replace('\r', "\n");
    let normalized = fence.replace_all(&normalized, "");
    let prompt_match = Regex::new(r"(?im)^\s*prompt\s*:\s*(?P<code>[abcde])\s*$")
        .ok()?
        .captures(&normalized)?;
    let append_match = Regex::new(r"(?im)^\s*append\s*:\s*(?P<append>.*?)\s*$")
        .ok()?
        .captures(&normalized);

    let raw_code = prompt_match.name("code")?.as_str().to_ascii_lowercase();
    let code = if raw_code == "b" { "c" } else { &raw_code }.to_owned();
    let append = append_match
        .and_then(|caps| caps.name("append"))
        .map(|m| sanitize_append_instruction(m.as_str()))
        .unwrap_or_default();

    Some(AutoPromptDecision {
        preset_id: map_code_to_preset_id(&code).to_owned(),
        code,
        append_instruction: append,
        source: "gpt-classifier".to_owned(),
    })
}

fn sanitize_append_instruction(value: &str) -> String {
    let normalized = Regex::new(r"\s+")
        .unwrap()
        .replace_all(value, " ")
        .trim()
        .to_owned();
    if normalized.is_empty()
        || normalized.eq_ignore_ascii_case("없음")
        || normalized.eq_ignore_ascii_case("none")
        || normalized.eq_ignore_ascii_case("null")
        || normalized == "-"
    {
        return String::new();
    }
    normalized
        .chars()
        .take(160)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

fn build_prompt(
    config: &PromptConfig,
    input: &IvLyricsTranslationRewriteInput,
    append_instruction: &str,
) -> String {
    let mut prompt = if input.use_korean_instructions {
        config.build_translation_prompt(&input.target_language, &input.numbered_lyrics)
    } else {
        config.build_translation_prompt_english_instructions(
            &input.target_language,
            &input.numbered_lyrics,
        )
    };

    let append_instruction = append_instruction.trim();
    if append_instruction.is_empty() {
        return prompt;
    }

    let instruction = format!(
        "\n\n[AUTO SONG-SPECIFIC INSTRUCTION]\n{append_instruction}\n[/AUTO SONG-SPECIFIC INSTRUCTION]"
    );
    if let Some(index) = prompt.find("\n\nINPUT_LINES_START") {
        prompt.insert_str(index, &instruction);
        prompt
    } else {
        prompt.push_str(&instruction);
        prompt
    }
}

fn map_code_to_preset_id(code: &str) -> &'static str {
    match code {
        "a" => PRESET_A_ID,
        "b" | "c" => PRESET_C_ID,
        "d" => PRESET_D_ID,
        "e" => PRESET_E_ID,
        _ => PRESET_E_ID,
    }
}

fn trim_classifier_input(numbered_lyrics: &str) -> String {
    let normalized = numbered_lyrics.trim_end();
    if normalized.chars().count() <= MAX_CLASSIFIER_INPUT_CHARS {
        return normalized.to_owned();
    }

    let chars = normalized.chars().collect::<Vec<_>>();
    let head_len = MAX_CLASSIFIER_INPUT_CHARS * 2 / 3;
    let tail_len = MAX_CLASSIFIER_INPUT_CHARS - head_len;
    let head = chars[..head_len].iter().collect::<String>();
    let tail = chars[chars.len() - tail_len..].iter().collect::<String>();
    format!("{}\n...\n{}", head.trim_end(), tail.trim_start())
}

struct AutoPromptDecision {
    code: String,
    preset_id: String,
    append_instruction: String,
    source: String,
}

impl AutoPromptDecision {
    fn fallback(source: &str) -> Self {
        Self {
            code: "e".to_owned(),
            preset_id: PRESET_E_ID.to_owned(),
            append_instruction: String::new(),
            source: source.to_owned(),
        }
    }
}
