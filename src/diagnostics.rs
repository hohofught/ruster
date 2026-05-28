use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn next_request_id() -> u64 {
    REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1
}

pub fn fingerprint(text: impl AsRef<str>) -> String {
    let text = text.as_ref();
    if text.is_empty() {
        return "0000000000000000".to_owned();
    }

    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let hash = hasher.finalize();
    hash[..8].iter().map(|b| format!("{b:02x}")).collect()
}

#[allow(dead_code)]
pub fn build_forward_key(provider: &str, mode: &str, prompt: &str) -> String {
    format!("{provider}:{mode}:{}:{}", prompt.len(), fingerprint(prompt))
}

pub fn estimate_tokens(text: Option<&str>) -> u64 {
    let Some(text) = text else {
        return 0;
    };
    if text.trim().is_empty() {
        return 0;
    }

    let mut tokens = 0u64;
    let mut ascii_run = 0u64;
    for ch in text.chars() {
        if ch.is_whitespace() || ch.is_control() {
            flush_ascii(&mut tokens, &mut ascii_run);
        } else if ch.is_ascii() {
            ascii_run += 1;
        } else {
            flush_ascii(&mut tokens, &mut ascii_run);
            tokens += 1;
        }
    }
    flush_ascii(&mut tokens, &mut ascii_run);
    tokens
}

fn flush_ascii(tokens: &mut u64, ascii_run: &mut u64) {
    if *ascii_run > 0 {
        *tokens += (*ascii_run).div_ceil(4).max(1);
        *ascii_run = 0;
    }
}
