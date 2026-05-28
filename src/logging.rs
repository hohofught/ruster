use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Local;
use parking_lot::Mutex;

#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<VecDeque<String>>>,
    stdout_enabled: Arc<AtomicBool>,
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::new())),
            stdout_enabled: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl LogBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_stdout_enabled(&self, enabled: bool) {
        self.stdout_enabled.store(enabled, Ordering::SeqCst);
    }

    pub fn push(&self, message: impl Into<String>) {
        let line = format!("{} {}", Local::now().format("%H:%M:%S"), message.into());
        if self.stdout_enabled.load(Ordering::SeqCst) {
            println!("{line}");
        }

        let mut inner = self.inner.lock();
        inner.push_back(line);
        while inner.len() > 800 {
            inner.pop_front();
        }
    }

    pub fn recent(&self, max_lines: usize) -> Vec<String> {
        if max_lines == 0 {
            return Vec::new();
        }

        let inner = self.inner.lock();
        let skip = inner.len().saturating_sub(max_lines);
        inner.iter().skip(skip).cloned().collect()
    }
}

pub fn summarize_text(text: impl AsRef<str>, max_chars: usize) -> String {
    let text = text.as_ref().replace('\r', "\\r").replace('\n', "\\n");
    if text.chars().count() <= max_chars {
        return text;
    }

    let mut shortened: String = text.chars().take(max_chars).collect();
    shortened.push_str("...");
    shortened
}
