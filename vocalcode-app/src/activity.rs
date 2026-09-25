//! Local daily activity for Home → Insights: per local calendar day, how many
//! dictations, how many words, and how many seconds of speech.
//!
//! Counters only. No text, application names or times of day are stored, and
//! nothing leaves the device. The file is bounded to the most recent
//! [`MAX_DAYS`] days; an unreadable file is left untouched and the session
//! simply starts counting from zero in memory, never overwriting it.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use jiff::civil::Date;

const MAX_DAYS: usize = 400;
pub(crate) const MAX_FILE_BYTES: usize = 256 * 1024;
/// Twelve full weeks, Monday-aligned, ending with the current week.
const HEAT_WEEKS: i64 = 12;
/// Speaking-rate window, and the least speech it will report a rate for.
const RATE_DAYS: i64 = 30;
const RATE_MIN_SPEECH_MS: u64 = 30_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Day {
    pub dictations: u32,
    pub words: u64,
    pub speech_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Activity {
    pub version: u32,
    /// `YYYY-MM-DD` in the local time zone → counters.
    pub days: BTreeMap<String, Day>,
}

/// What Home shows. `heat` holds one value per day of the 12-week grid,
/// oldest first; `-1` marks days after today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Insights {
    pub streak: u32,
    pub best_streak: u32,
    pub active_days: u32,
    pub words_today: u64,
    pub words_week: u64,
    pub dictations_week: u32,
    pub words_per_minute: Option<u32>,
    pub heat: Vec<i64>,
}

/// Words for counting purposes: every CJK character is one word (the usual
/// convention for Chinese/Japanese), every other whitespace-separated token
/// containing a letter or digit is one word.
pub fn count_words(text: &str) -> u64 {
    let mut words = 0u64;
    for token in text.split_whitespace() {
        let mut latin = false;
        for c in token.chars() {
            if is_cjk(c) {
                words += 1;
                if latin {
                    words += 1;
                    latin = false;
                }
            } else if c.is_alphanumeric() {
                latin = true;
            }
        }
        if latin {
            words += 1;
        }
    }
    words
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x3040..=0x30FF | 0xAC00..=0xD7AF)
}

fn key(date: Date) -> String {
    date.to_string()
}

impl Activity {
    pub fn record(&mut self, today: Date, words: u64, speech_ms: u64) {
        self.version = 1;
        let day = self.days.entry(key(today)).or_default();
        day.dictations = day.dictations.saturating_add(1);
        day.words = day.words.saturating_add(words);
        // One dictation is at most ten minutes; clamp so a clock or driver
        // glitch cannot poison the speaking rate for a month.
        day.speech_ms = day.speech_ms.saturating_add(speech_ms.min(15 * 60 * 1000));
        self.trim();
    }

    fn trim(&mut self) {
        while self.days.len() > MAX_DAYS {
            let Some(oldest) = self.days.keys().next().cloned() else {
                break;
            };
            self.days.remove(&oldest);
        }
    }

    /// Add another installation's days to these, day by day. Both sets are
    /// counts of different dictations, so a shared day is the sum. Keys that
    /// are not calendar dates are ignored rather than stored. Returns the
    /// number of days that carried counts.
    pub fn merge(&mut self, other: &Activity) -> usize {
        let mut merged = 0;
        for (text, counts) in &other.days {
            if text.parse::<Date>().is_err() || counts.dictations == 0 {
                continue;
            }
            let day = self.days.entry(text.clone()).or_default();
            day.dictations = day.dictations.saturating_add(counts.dictations);
            day.words = day.words.saturating_add(counts.words);
            day.speech_ms = day.speech_ms.saturating_add(counts.speech_ms);
            merged += 1;
        }
        if merged > 0 {
            self.version = 1;
            self.trim();
        }
        merged
    }

    fn day(&self, date: Date) -> Option<&Day> {
        self.days.get(&key(date)).filter(|d| d.dictations > 0)
    }

    pub fn insights(&self, today: Date) -> Insights {
        let active = |date: Date| self.day(date).is_some();
        // A streak still counts while today has no dictation yet.
        let mut cursor = if active(today) {
            today
        } else {
            today.yesterday().unwrap_or(today)
        };
        let mut streak = 0;
        while active(cursor) {
            streak += 1;
            match cursor.yesterday() {
                Ok(previous) => cursor = previous,
                Err(_) => break,
            }
        }
        let mut best_streak = 0;
        let mut run = 0;
        let mut previous: Option<Date> = None;
        for (text, day) in &self.days {
            let Ok(date) = text.parse::<Date>() else {
                continue;
            };
            if day.dictations == 0 {
                continue;
            }
            run = match previous {
                Some(p) if p.tomorrow().ok() == Some(date) => run + 1,
                _ => 1,
            };
            best_streak = best_streak.max(run);
            previous = Some(date);
        }
        let monday = today
            .checked_sub(jiff::Span::new().days(i64::from(today.weekday().to_monday_zero_offset())))
            .unwrap_or(today);
        let start = monday
            .checked_sub(jiff::Span::new().days((HEAT_WEEKS - 1) * 7))
            .unwrap_or(monday);
        let mut heat = Vec::with_capacity((HEAT_WEEKS * 7) as usize);
        let mut words_week = 0;
        let mut dictations_week = 0;
        let mut date = start;
        for _ in 0..HEAT_WEEKS * 7 {
            if date > today {
                heat.push(-1);
            } else {
                let day = self.day(date).copied().unwrap_or_default();
                heat.push(day.words.min(i64::MAX as u64) as i64);
                if date >= monday {
                    words_week += day.words;
                    dictations_week += day.dictations;
                }
            }
            match date.tomorrow() {
                Ok(next) => date = next,
                Err(_) => break,
            }
        }
        let rate_from = today
            .checked_sub(jiff::Span::new().days(RATE_DAYS - 1))
            .unwrap_or(today);
        let (rate_words, rate_ms) = self
            .days
            .iter()
            .filter_map(|(text, day)| Some((text.parse::<Date>().ok()?, day)))
            .filter(|(date, _)| *date >= rate_from && *date <= today)
            .fold((0u64, 0u64), |(w, ms), (_, day)| {
                (w + day.words, ms + day.speech_ms)
            });
        let words_per_minute = (rate_ms >= RATE_MIN_SPEECH_MS)
            .then(|| ((rate_words as f64) * 60_000.0 / rate_ms as f64).round() as u32);
        Insights {
            streak,
            best_streak: best_streak.max(streak),
            active_days: self.days.values().filter(|d| d.dictations > 0).count() as u32,
            words_today: self.day(today).map_or(0, |d| d.words),
            words_week,
            dictations_week,
            words_per_minute,
            heat,
        }
    }
}

fn today() -> Date {
    jiff::Zoned::now().date()
}

/// Loaded once at startup, updated after each delivered dictation.
#[derive(Default)]
pub struct Store {
    inner: Mutex<StoreInner>,
}

#[derive(Default)]
struct StoreInner {
    path: Option<PathBuf>,
    writable: bool,
    activity: Activity,
    cached: Option<(Instant, serde_json::Value)>,
}

impl Store {
    pub fn load(&self, dir: &Path) {
        let path = dir.join("activity.json");
        let (activity, writable) = match crate::read_bounded_bytes(&path, MAX_FILE_BYTES) {
            Ok(bytes) => match serde_json::from_slice::<Activity>(&bytes) {
                Ok(activity) => (activity, true),
                Err(error) => {
                    log::warn!(
                        "activity: unreadable {}, left untouched: {error}",
                        path.display()
                    );
                    (Activity::default(), false)
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (Activity::default(), true)
            }
            Err(error) => {
                log::warn!("activity: could not read {}: {error}", path.display());
                (Activity::default(), false)
            }
        };
        if let Ok(mut inner) = self.inner.lock() {
            *inner = StoreInner {
                path: Some(path),
                writable,
                activity,
                cached: None,
            };
        }
    }

    pub fn record(&self, text: &str, speech_ms: u64) {
        let words = count_words(text);
        if words == 0 {
            return;
        }
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.activity.record(today(), words, speech_ms);
        inner.cached = None;
        if !inner.writable {
            return;
        }
        let Some(path) = inner.path.clone() else {
            return;
        };
        match serde_json::to_vec(&inner.activity) {
            Ok(bytes) => {
                if let Err(error) = crate::storage::atomic_write(&path, bytes) {
                    log::warn!("activity: save failed: {error}");
                }
            }
            Err(error) => log::warn!("activity: serialise failed: {error}"),
        }
    }

    /// Whether any day has been counted on this installation.
    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .map(|inner| inner.activity.days.is_empty())
            .unwrap_or(false)
    }

    /// Merge the previous VocalCode's activity and write the result. Refused
    /// when this session could not read its own file: saving a merge over a
    /// guessed empty history would destroy the real one.
    pub fn merge_imported(&self, other: &Activity) -> Result<usize, String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "Activity is unavailable.".to_string())?;
        let path = match (&inner.path, inner.writable) {
            (Some(path), true) => path.clone(),
            _ => return Err("This installation's activity file could not be read, so nothing was merged into it.".into()),
        };
        let mut next = inner.activity.clone();
        let days = next.merge(other);
        if days == 0 {
            return Ok(0);
        }
        let bytes = serde_json::to_vec(&next).map_err(|error| error.to_string())?;
        crate::storage::atomic_write(&path, bytes).map_err(|error| error.to_string())?;
        inner.activity = next;
        inner.cached = None;
        Ok(days)
    }

    /// Insights for the status payload. Recomputed after a new dictation and
    /// at most once a minute otherwise, so a streak rolls over at midnight.
    pub fn snapshot(&self) -> serde_json::Value {
        let Ok(mut inner) = self.inner.lock() else {
            return serde_json::Value::Null;
        };
        if let Some((at, value)) = &inner.cached {
            if at.elapsed() < Duration::from_secs(60) {
                return value.clone();
            }
        }
        let value = serde_json::to_value(inner.activity.insights(today()))
            .unwrap_or(serde_json::Value::Null);
        inner.cached = Some((Instant::now(), value.clone()));
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(text: &str) -> Date {
        text.parse().unwrap()
    }

    #[test]
    fn words_count_cjk_characters_and_latin_tokens() {
        assert_eq!(count_words("Hello, world!"), 2);
        assert_eq!(count_words("你好世界"), 4);
        assert_eq!(count_words("用 GitHub 发布v2"), 1 + 1 + 2 + 1);
        assert_eq!(count_words(" — … "), 0);
    }

    #[test]
    fn streaks_tolerate_an_empty_today_and_find_the_best_run() {
        let mut a = Activity::default();
        for day in [
            "2026-09-01",
            "2026-09-02",
            "2026-09-03",
            "2026-09-10",
            "2026-09-21",
            "2026-09-22",
        ] {
            a.record(d(day), 10, 6_000);
        }
        let today = d("2026-09-23");
        let i = a.insights(today);
        assert_eq!(i.streak, 2, "yesterday and the day before");
        assert_eq!(i.best_streak, 3);
        assert_eq!(i.active_days, 6);
        a.record(today, 5, 3_000);
        let i = a.insights(today);
        assert_eq!((i.streak, i.words_today), (3, 5));
        assert_eq!(i.best_streak, 3);
        assert_eq!(a.insights(d("2026-09-25")).streak, 0);
    }

    #[test]
    fn heat_grid_is_twelve_monday_aligned_weeks_ending_this_week() {
        let mut a = Activity::default();
        let today = d("2026-09-23"); // a Wednesday
        a.record(today, 42, 30_000);
        a.record(d("2026-09-21"), 8, 5_000); // Monday this week
        a.record(d("2026-09-20"), 100, 60_000); // Sunday last week
        let i = a.insights(today);
        assert_eq!(i.heat.len(), 84);
        let last_week = &i.heat[77..];
        assert_eq!(last_week, &[8, 0, 42, -1, -1, -1, -1]);
        assert_eq!(i.heat[76], 100);
        assert_eq!((i.words_week, i.dictations_week), (50, 2));
    }

    #[test]
    fn speaking_rate_needs_enough_recent_speech() {
        let mut a = Activity::default();
        let today = d("2026-09-23");
        a.record(today, 20, 10_000);
        assert_eq!(a.insights(today).words_per_minute, None);
        a.record(today, 280, 110_000);
        // 300 words over two minutes of speech.
        assert_eq!(a.insights(today).words_per_minute, Some(150));
        // Speech older than the window does not count.
        assert_eq!(a.insights(d("2026-11-30")).words_per_minute, None);
    }

    #[test]
    fn another_installations_days_add_up_and_stay_bounded() {
        let mut here = Activity::default();
        here.record(d("2026-09-02"), 7, 3_000);
        let mut previous = Activity::default();
        previous.record(d("2026-09-01"), 30, 9_000);
        previous.record(d("2026-09-02"), 5, 2_000);
        previous.days.insert("not a date".into(), Day::default());
        previous.days.insert(
            "2026-09-03".into(),
            Day {
                dictations: 0,
                words: 99,
                speech_ms: 0,
            },
        );
        assert_eq!(here.merge(&previous), 2);
        assert_eq!(here.days.len(), 2);
        assert_eq!(
            here.days["2026-09-02"],
            Day {
                dictations: 2,
                words: 12,
                speech_ms: 5_000
            }
        );
        let mut long = Activity::default();
        let mut date = d("2024-01-01");
        for _ in 0..MAX_DAYS {
            long.record(date, 1, 1_000);
            date = date.tomorrow().unwrap();
        }
        here.merge(&long);
        assert_eq!(here.days.len(), MAX_DAYS);
        assert!(here.days.contains_key("2026-09-02"), "newest days are kept");
    }

    #[test]
    fn imported_activity_is_saved_but_never_over_an_unreadable_file() {
        let dir = std::env::temp_dir().join(format!(
            "vocalcode-activity-import-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut previous = Activity::default();
        previous.record(d("2026-09-01"), 30, 9_000);

        let store = Store::default();
        assert!(store.merge_imported(&previous).is_err(), "never loaded");
        store.load(&dir);
        assert!(store.is_empty());
        assert_eq!(store.merge_imported(&previous), Ok(1));
        assert!(!store.is_empty());
        let saved: Activity =
            serde_json::from_slice(&std::fs::read(dir.join("activity.json")).unwrap()).unwrap();
        assert_eq!(saved.days["2026-09-01"].words, 30);

        std::fs::write(dir.join("activity.json"), b"damaged").unwrap();
        let damaged = Store::default();
        damaged.load(&dir);
        assert!(damaged.merge_imported(&previous).is_err());
        assert_eq!(
            std::fs::read(dir.join("activity.json")).unwrap(),
            b"damaged"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn history_is_bounded_and_round_trips() {
        let mut a = Activity::default();
        let mut date = d("2025-01-01");
        for _ in 0..(MAX_DAYS + 30) {
            a.record(date, 1, 1_000);
            date = date.tomorrow().unwrap();
        }
        assert_eq!(a.days.len(), MAX_DAYS);
        assert!(!a.days.contains_key("2025-01-01"));
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<Activity>(&json).unwrap(), a);
        assert!(json.len() < MAX_FILE_BYTES);
    }
}
