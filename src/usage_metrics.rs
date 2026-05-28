use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::app_paths::AppPaths;
use crate::diagnostics;
use crate::logging::LogBuffer;

#[derive(Clone)]
pub struct UsageMetrics {
    path: PathBuf,
    inner: Arc<Mutex<UsageStore>>,
    logs: LogBuffer,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub total_requests: u64,
    pub succeeded_requests: u64,
    pub failed_requests: u64,
    pub cancelled_requests: u64,
    pub gemini_requests: u64,
    pub open_ai_requests: u64,
    pub mort_requests: u64,
    pub other_requests: u64,
    pub input_tokens: u64,
    pub successful_output_tokens: u64,
    pub input_characters: u64,
    pub successful_output_characters: u64,
    pub started_at_local: String,
    pub last_updated_at_local: String,
    pub last_failure: String,
}

impl UsageSnapshot {
    pub fn success_rate(&self) -> f64 {
        if self.total_requests == 0 {
            0.0
        } else {
            self.succeeded_requests as f64 * 100.0 / self.total_requests as f64
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageStatsPeriod {
    Daily,
    Weekly,
    Monthly,
}

#[derive(Clone, Debug)]
pub struct UsageBucket {
    pub label: String,
    #[allow(dead_code)]
    pub period_start_local: NaiveDate,
    pub total_requests: u64,
    pub succeeded_requests: u64,
    pub failed_requests: u64,
    pub cancelled_requests: u64,
    pub gemini_requests: u64,
    pub open_ai_requests: u64,
    pub mort_requests: u64,
    pub other_requests: u64,
    pub input_tokens: u64,
    pub successful_output_tokens: u64,
}

impl UsageBucket {
    pub fn success_rate(&self) -> f64 {
        if self.total_requests == 0 {
            0.0
        } else {
            self.succeeded_requests as f64 * 100.0 / self.total_requests as f64
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UsageStore {
    started_utc: DateTime<Utc>,
    events: Vec<UsageEvent>,
    last_failure: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UsageEvent {
    timestamp_utc: DateTime<Utc>,
    provider: String,
    route: String,
    success: bool,
    cancelled: bool,
    input_tokens: u64,
    output_tokens: u64,
    input_characters: u64,
    output_characters: u64,
}

impl Default for UsageStore {
    fn default() -> Self {
        Self {
            started_utc: Utc::now(),
            events: Vec::new(),
            last_failure: String::new(),
        }
    }
}

impl UsageMetrics {
    pub fn new(paths: &AppPaths, logs: LogBuffer) -> Self {
        let path = paths.usage_metrics_path();
        let store = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<UsageStore>(&text).ok())
            .unwrap_or_default();

        Self {
            path,
            inner: Arc::new(Mutex::new(store)),
            logs,
        }
    }

    pub fn record_success(
        &self,
        provider: &str,
        route: &str,
        input_text: Option<&str>,
        output_text: Option<&str>,
    ) {
        let event = UsageEvent {
            timestamp_utc: Utc::now(),
            provider: provider.to_owned(),
            route: route.to_owned(),
            success: true,
            cancelled: false,
            input_tokens: diagnostics::estimate_tokens(input_text),
            output_tokens: diagnostics::estimate_tokens(output_text),
            input_characters: input_text.map(str::len).unwrap_or(0) as u64,
            output_characters: output_text.map(str::len).unwrap_or(0) as u64,
        };
        self.push_event(event);
    }

    pub fn record_failure(
        &self,
        provider: &str,
        route: &str,
        input_text: Option<&str>,
        status_code: u16,
        message: &str,
    ) {
        let cancelled = status_code == 499;
        let event = UsageEvent {
            timestamp_utc: Utc::now(),
            provider: provider.to_owned(),
            route: route.to_owned(),
            success: false,
            cancelled,
            input_tokens: diagnostics::estimate_tokens(input_text),
            output_tokens: 0,
            input_characters: input_text.map(str::len).unwrap_or(0) as u64,
            output_characters: 0,
        };

        {
            let mut inner = self.inner.lock();
            inner.last_failure = format!(
                "{} {provider}/{route} {status_code} {message}",
                Local::now().format("%H:%M:%S")
            );
        }
        self.push_event(event);
    }

    pub fn snapshot(&self) -> UsageSnapshot {
        let inner = self.inner.lock();
        let mut snapshot = UsageSnapshot {
            started_at_local: inner
                .started_utc
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
            last_failure: inner.last_failure.clone(),
            ..UsageSnapshot::default()
        };

        for event in &inner.events {
            snapshot.total_requests += 1;
            snapshot.input_tokens += event.input_tokens;
            snapshot.input_characters += event.input_characters;
            if event.success {
                snapshot.succeeded_requests += 1;
                snapshot.successful_output_tokens += event.output_tokens;
                snapshot.successful_output_characters += event.output_characters;
            } else if event.cancelled {
                snapshot.cancelled_requests += 1;
            } else {
                snapshot.failed_requests += 1;
            }

            match event.provider.to_ascii_lowercase().as_str() {
                "gemini" | "geminiproxy" => snapshot.gemini_requests += 1,
                "openai" | "openaiproxy" => snapshot.open_ai_requests += 1,
                "mort" | "customapi" | "compatapi" | "compatibleapi" => {
                    snapshot.mort_requests += 1;
                }
                _ => snapshot.other_requests += 1,
            }
        }

        snapshot.last_updated_at_local = inner
            .events
            .last()
            .map(|e| {
                e.timestamp_utc
                    .with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_default();
        snapshot
    }

    pub fn buckets(&self, period: UsageStatsPeriod) -> Vec<UsageBucket> {
        let inner = self.inner.lock();
        build_buckets(&inner.events, period)
    }

    pub fn reset(&self) {
        {
            let mut inner = self.inner.lock();
            *inner = UsageStore::default();
        }
        let _ = std::fs::remove_file(&self.path);
    }

    fn push_event(&self, event: UsageEvent) {
        {
            let mut inner = self.inner.lock();
            inner.events.push(event);
            if inner.events.len() > 20_000 {
                let remove_count = inner.events.len() - 20_000;
                inner.events.drain(0..remove_count);
            }
        }
        self.save();
    }

    fn save(&self) {
        let Some(parent) = self.path.parent() else {
            return;
        };
        if let Err(error) = std::fs::create_dir_all(parent) {
            self.logs
                .push(format!("[UsageMetrics] 디렉터리 생성 실패: {error}"));
            return;
        }

        let json = {
            let inner = self.inner.lock();
            match serde_json::to_string_pretty(&*inner) {
                Ok(json) => json,
                Err(error) => {
                    self.logs
                        .push(format!("[UsageMetrics] 직렬화 실패: {error}"));
                    return;
                }
            }
        };

        if let Err(error) = std::fs::write(&self.path, json) {
            self.logs.push(format!("[UsageMetrics] 저장 실패: {error}"));
        }
    }
}

#[derive(Clone)]
struct BucketAccumulator {
    start: NaiveDate,
    label: String,
    total_requests: u64,
    succeeded_requests: u64,
    failed_requests: u64,
    cancelled_requests: u64,
    gemini_requests: u64,
    open_ai_requests: u64,
    mort_requests: u64,
    other_requests: u64,
    input_tokens: u64,
    successful_output_tokens: u64,
}

impl BucketAccumulator {
    fn new(start: NaiveDate, period: UsageStatsPeriod) -> Self {
        Self {
            start,
            label: format_bucket_label(period, start),
            total_requests: 0,
            succeeded_requests: 0,
            failed_requests: 0,
            cancelled_requests: 0,
            gemini_requests: 0,
            open_ai_requests: 0,
            mort_requests: 0,
            other_requests: 0,
            input_tokens: 0,
            successful_output_tokens: 0,
        }
    }

    fn to_bucket(&self) -> UsageBucket {
        UsageBucket {
            label: self.label.clone(),
            period_start_local: self.start,
            total_requests: self.total_requests,
            succeeded_requests: self.succeeded_requests,
            failed_requests: self.failed_requests,
            cancelled_requests: self.cancelled_requests,
            gemini_requests: self.gemini_requests,
            open_ai_requests: self.open_ai_requests,
            mort_requests: self.mort_requests,
            other_requests: self.other_requests,
            input_tokens: self.input_tokens,
            successful_output_tokens: self.successful_output_tokens,
        }
    }
}

fn build_buckets(events: &[UsageEvent], period: UsageStatsPeriod) -> Vec<UsageBucket> {
    let starts = bucket_starts(period);
    if starts.is_empty() {
        return Vec::new();
    }

    let first = starts[0];
    let last_exclusive = match period {
        UsageStatsPeriod::Weekly => add_days(*starts.last().unwrap_or(&first), 7),
        UsageStatsPeriod::Monthly => add_months(*starts.last().unwrap_or(&first), 1),
        UsageStatsPeriod::Daily => add_days(*starts.last().unwrap_or(&first), 1),
    };

    let mut buckets: HashMap<NaiveDate, BucketAccumulator> = starts
        .iter()
        .copied()
        .map(|start| (start, BucketAccumulator::new(start, period)))
        .collect();

    for event in events {
        let local_date = event.timestamp_utc.with_timezone(&Local).date_naive();
        if local_date < first || local_date >= last_exclusive {
            continue;
        }

        let key = match period {
            UsageStatsPeriod::Weekly => start_of_week(local_date),
            UsageStatsPeriod::Monthly => month_start(local_date),
            UsageStatsPeriod::Daily => local_date,
        };

        let Some(bucket) = buckets.get_mut(&key) else {
            continue;
        };
        bucket.total_requests += 1;
        bucket.input_tokens += event.input_tokens;
        bucket.successful_output_tokens += event.output_tokens;
        match event.provider.to_ascii_lowercase().as_str() {
            "gemini" | "geminiproxy" => bucket.gemini_requests += 1,
            "openai" | "openaiproxy" => bucket.open_ai_requests += 1,
            "mort" | "customapi" | "compatapi" | "compatibleapi" => bucket.mort_requests += 1,
            _ => bucket.other_requests += 1,
        }
        if event.success {
            bucket.succeeded_requests += 1;
        } else if event.cancelled {
            bucket.cancelled_requests += 1;
        } else {
            bucket.failed_requests += 1;
        }
    }

    starts
        .iter()
        .filter_map(|start| buckets.get(start))
        .map(BucketAccumulator::to_bucket)
        .collect()
}

fn bucket_starts(period: UsageStatsPeriod) -> Vec<NaiveDate> {
    let today = Local::now().date_naive();
    match period {
        UsageStatsPeriod::Weekly => {
            let current = start_of_week(today);
            (0..12).map(|i| add_days(current, (i - 11) * 7)).collect()
        }
        UsageStatsPeriod::Monthly => {
            let current = month_start(today);
            (0..12).map(|i| add_months(current, i - 11)).collect()
        }
        UsageStatsPeriod::Daily => (0..14).map(|i| add_days(today, i - 13)).collect(),
    }
}

fn start_of_week(date: NaiveDate) -> NaiveDate {
    add_days(date, -(date.weekday().num_days_from_monday() as i64))
}

fn month_start(date: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(date.year(), date.month(), 1).unwrap_or(date)
}

fn add_days(date: NaiveDate, days: i64) -> NaiveDate {
    date.checked_add_signed(Duration::days(days))
        .unwrap_or(date)
}

fn add_months(date: NaiveDate, months: i64) -> NaiveDate {
    let month_index = date.year() as i64 * 12 + date.month0() as i64 + months;
    let year = month_index.div_euclid(12) as i32;
    let month = month_index.rem_euclid(12) as u32 + 1;
    NaiveDate::from_ymd_opt(year, month, 1).unwrap_or(date)
}

fn format_bucket_label(period: UsageStatsPeriod, start: NaiveDate) -> String {
    match period {
        UsageStatsPeriod::Monthly => start.format("%Y-%m").to_string(),
        UsageStatsPeriod::Weekly | UsageStatsPeriod::Daily => start.format("%m/%d").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn bucket_accumulates_provider_breakdown() {
        let now = Utc::now();
        let events = vec![
            UsageEvent {
                timestamp_utc: now,
                provider: "Gemini".to_owned(),
                route: "generateContent".to_owned(),
                success: true,
                cancelled: false,
                input_tokens: 10,
                output_tokens: 20,
                input_characters: 40,
                output_characters: 80,
            },
            UsageEvent {
                timestamp_utc: now,
                provider: "OpenAI".to_owned(),
                route: "chat".to_owned(),
                success: false,
                cancelled: true,
                input_tokens: 5,
                output_tokens: 0,
                input_characters: 20,
                output_characters: 0,
            },
            UsageEvent {
                timestamp_utc: now,
                provider: "CustomApi".to_owned(),
                route: "/".to_owned(),
                success: false,
                cancelled: false,
                input_tokens: 3,
                output_tokens: 0,
                input_characters: 12,
                output_characters: 0,
            },
        ];

        let buckets = build_buckets(&events, UsageStatsPeriod::Daily);
        let today = Local::now().date_naive();
        let bucket = buckets
            .iter()
            .find(|bucket| bucket.period_start_local == today)
            .unwrap();

        assert_eq!(bucket.total_requests, 3);
        assert_eq!(bucket.succeeded_requests, 1);
        assert_eq!(bucket.cancelled_requests, 1);
        assert_eq!(bucket.failed_requests, 1);
        assert_eq!(bucket.gemini_requests, 1);
        assert_eq!(bucket.open_ai_requests, 1);
        assert_eq!(bucket.mort_requests, 1);
        assert_eq!(bucket.other_requests, 0);
    }

    #[test]
    fn old_events_outside_visible_range_are_ignored() {
        let old = Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap();
        let events = vec![UsageEvent {
            timestamp_utc: old,
            provider: "Gemini".to_owned(),
            route: "generateContent".to_owned(),
            success: true,
            cancelled: false,
            input_tokens: 1,
            output_tokens: 1,
            input_characters: 1,
            output_characters: 1,
        }];

        assert!(
            build_buckets(&events, UsageStatsPeriod::Daily)
                .iter()
                .all(|bucket| bucket.total_requests == 0)
        );
    }
}
