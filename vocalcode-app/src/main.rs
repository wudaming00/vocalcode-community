//! vocalcode-app — the desktop app. A background pair of threads runs the
//! push-to-talk engine (global hotkey → capture → ASR → inject); the main
//! thread hosts the tray icon and egui settings window (see `gui`).
//!
//! `vocalcode-app transcribe <file.wav>` runs an offline ASR self-test / benchmark.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod activation;
mod activity;
mod calendar;
mod community;
#[cfg(windows)]
mod control_bar;
mod diagnostics;
mod dictation_control;
mod inference;
mod learning;
mod meeting;
mod meeting_prompt;
mod meeting_reminder;
mod migration;
mod models;
mod noise_filter;
mod overlay;
mod paths;
mod rewrite;
mod rewrite_cli;
mod storage;
#[cfg(test)]
mod voice_corpus;
mod webui;
mod workflows;

#[cfg(target_os = "macos")]
mod macos;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use vocalcode_core::error::VocalCodeError;
use vocalcode_core::license::{trial_days_left, TRIAL_DAYS};
use vocalcode_core::limits::{
    MAX_CONFIG_DOCUMENT_BYTES, MAX_DICTIONARY_DOCUMENT_BYTES, MAX_DICTIONARY_RULES,
    MAX_DICTIONARY_SIDE_UTF8_BYTES, MAX_TOTALS_DOCUMENT_BYTES,
};

use vocalcode_core::traits::{Asr, AudioCapture, Recording, TriggerEvent};
use vocalcode_core::{
    trigger_event_channel, Config, Engine, HotkeyListener, LicenseStatus, Outcome,
    TriggerEventReceiver, TriggerEventSender,
};
use vocalcode_platform::{
    device_id, AudioLevel, CaptureShared, CorrectionEvent, CorrectionMonitor, CpalAudioCapture,
    EnigoInjector, HardwareProfile, PlatformHotkey, SharedTriggers,
};
use webui::RuntimeStatus;

pub(crate) struct MeetingAsrRequest {
    pub(crate) samples: Vec<f32>,
    pub(crate) sample_rate: u32,
    pub(crate) reply: mpsc::SyncSender<Result<String, String>>,
}

const MEETING_ASR_INPUT_GRACE: Duration = Duration::from_millis(30);

fn meeting_asr_may_run(recording: bool, input_pending: bool, queued_for: Duration) -> bool {
    !recording && !input_pending && queued_for >= MEETING_ASR_INPUT_GRACE
}

const DOCUMENT_WRITE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LOG_WRITE_LOCK_TIMEOUT: Duration = Duration::from_millis(250);
const FILE_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);
static LOG_WRITE_PROCESS_LOCKS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Weak<ProcessLogLock>>>,
> = std::sync::OnceLock::new();

#[derive(Debug)]
struct ProcessLogLock {
    queue: std::sync::Mutex<ProcessLogQueue>,
    changed: std::sync::Condvar,
}

#[derive(Debug, Default)]
struct ProcessLogQueue {
    next: u64,
    serving: u64,
    cancelled: std::collections::BTreeSet<u64>,
}

struct ProcessLogGuard<'a> {
    lock: &'a ProcessLogLock,
}

impl Drop for ProcessLogGuard<'_> {
    fn drop(&mut self) {
        let mut queue = self
            .lock
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.serving = queue.serving.wrapping_add(1);
        while {
            let serving = queue.serving;
            queue.cancelled.remove(&serving)
        } {
            queue.serving = queue.serving.wrapping_add(1);
        }
        self.lock.changed.notify_all();
    }
}

/// Read a small control document without trusting metadata or allowing a
/// concurrently growing file to allocate past its declared limit.
fn read_bounded_bytes(path: &Path, maximum: usize) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let advertised = file.metadata()?.len();
    if advertised > maximum as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file exceeds the {maximum}-byte safety limit"),
        ));
    }
    let mut bytes = Vec::with_capacity(advertised as usize);
    file.take(maximum as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file exceeds the {maximum}-byte safety limit"),
        ));
    }
    Ok(bytes)
}

fn read_bounded_string(path: &Path, maximum: usize) -> std::io::Result<String> {
    String::from_utf8(read_bounded_bytes(path, maximum)?).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file is not valid UTF-8: {error}"),
        )
    })
}

fn file_lock_is_contended(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    match error.raw_os_error() {
        // LockFileEx reports ERROR_LOCK_VIOLATION for an already-held range.
        #[cfg(windows)]
        Some(33) => true,
        #[cfg(target_os = "linux")]
        Some(11) => true,
        #[cfg(target_os = "macos")]
        Some(35) => true,
        _ => false,
    }
}

fn file_lock_deadline(timeout: Duration, description: &str) -> std::io::Result<Instant> {
    Instant::now().checked_add(timeout).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{description} timeout is too large"),
        )
    })
}

fn file_lock_timeout(description: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("timed out waiting for {description}"),
    )
}

/// Acquire one cross-process lock without ever extending its caller's absolute
/// deadline. A successful OS lock that arrives after the deadline is released
/// before returning, so a slow scheduling wake cannot turn into a late write.
fn try_lock_exclusive_until(
    file: &std::fs::File,
    deadline: Instant,
    description: &str,
) -> std::io::Result<()> {
    loop {
        if Instant::now() >= deadline {
            return Err(file_lock_timeout(description));
        }
        match fs2::FileExt::try_lock_exclusive(file) {
            Ok(()) => {
                if Instant::now() >= deadline {
                    let _ = fs2::FileExt::unlock(file);
                    return Err(file_lock_timeout(description));
                }
                return Ok(());
            }
            Err(error) if file_lock_is_contended(&error) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(file_lock_timeout(description));
                }
                thread::sleep(FILE_LOCK_POLL_INTERVAL.min(remaining));
            }
            Err(error) => return Err(error),
        }
    }
}

/// One fully persisted desired settings snapshot. Generations, rather than UI
/// request ids, are the runtime ordering authority: request ids come from an
/// untrusted page and the first-run picker deliberately uses zero.
#[derive(Clone, Debug)]
pub(crate) struct PendingConfig {
    pub(crate) generation: u64,
    pub(crate) request_id: u64,
    pub(crate) config: Config,
}

/// Serializes desired-config publication with the engine's final commit.
///
/// Model preparation itself runs without this mutex. A newer persisted config
/// advances `generation` and cancels the registered token; the engine then
/// takes this mutex for its final generation check and resource swap. Thus a
/// save is ordered wholly before or wholly after a runtime commit, never in the
/// check/commit gap.
#[derive(Debug)]
struct ActivePrepare {
    generation: u64,
    cancellation: models::CancellationToken,
    /// The model route the prepare is building, so `publish` can tell a save
    /// that needs a different model from one that merely changed a toggle.
    model: String,
    language: String,
}

#[derive(Default, Debug)]
pub(crate) struct ConfigApplyCoordinator {
    generation: u64,
    pending: Option<PendingConfig>,
    active_prepare: Option<ActivePrepare>,
}

impl ConfigApplyCoordinator {
    pub(crate) fn publish(&mut self, request_id: u64, config: Config) -> u64 {
        // Cancel the running prepare only when the new config wants a
        // different model route. Any settings save used to cancel
        // unconditionally, so toggling the recording indicator during the
        // first multi-hundred-MB model download threw away every byte and
        // restarted from zero (a native tester hit exactly this). A same-route
        // prepare is left to finish: the apply loop discards its stale engine
        // swap, but the downloaded artifacts persist on disk, so the re-run
        // for the new generation verifies them and completes without another
        // download.
        let route_changed = self.active_prepare.as_ref().is_some_and(|active| {
            !models::same_model_route(
                &config.model,
                &config.language,
                &active.model,
                &active.language,
            )
        });
        if route_changed {
            self.cancel_prepare();
        }
        self.generation = self
            .generation
            .checked_add(1)
            .expect("config generation exhausted");
        self.pending = Some(PendingConfig {
            generation: self.generation,
            request_id,
            config,
        });
        self.generation
    }

    pub(crate) fn cancel_prepare(&mut self) {
        if let Some(active) = self.active_prepare.take() {
            active.cancellation.cancel();
        }
    }

    fn generation(&self) -> u64 {
        self.generation
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation == generation
    }

    fn take_pending(&mut self) -> Option<PendingConfig> {
        self.pending.take()
    }

    fn requeue_if_no_newer(&mut self, deferred: PendingConfig) -> bool {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.generation >= deferred.generation)
        {
            false
        } else {
            self.pending = Some(deferred);
            true
        }
    }

    fn begin_prepare(
        &mut self,
        generation: u64,
        config: &Config,
    ) -> Option<models::CancellationToken> {
        if !self.is_current(generation) {
            return None;
        }
        self.cancel_prepare();
        let cancellation = models::CancellationToken::new();
        self.active_prepare = Some(ActivePrepare {
            generation,
            cancellation: cancellation.clone(),
            model: config.model.clone(),
            language: config.language.clone(),
        });
        Some(cancellation)
    }

    fn finish_prepare(&mut self, generation: u64) {
        if self
            .active_prepare
            .as_ref()
            .is_some_and(|active| active.generation == generation)
        {
            self.active_prepare = None;
        }
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

const RULES_TEMPLATE: &str = "# VocalCode — your own recognition fixes.\n\
     # One rule per line:  heard text => what to write\n\
     # Lines starting with # are ignored. Matching is whole-word and\n\
     # case-insensitive.\n\
     #\n\
     # VocalCode already ships a built-in dictionary (Claude Code, GitHub,\n\
     # Vercel and many more) — you do NOT need to repeat those here. This file\n\
     # is just for YOUR words, kept separate from the built-ins.\n\
     #\n\
     # To override a built-in, add the same heard phrase with your wording.\n\
     # To turn a built-in off, map its heard phrase to itself, e.g.\n\
     #   cloud code => cloud code\n\
     #\n\
     # Example (edit or delete):\n\
     # my startup name => Acme\n";

/// The built-in recognition-correction dictionary, compiled into the binary and
/// never written into the user's editable file. Updated per release.
const DEFAULT_RULES_DOC: &str = include_str!("default_replacements.txt");

/// Parse the built-in default rules. Cheap (~70 rules); parsed per load.
fn default_rules() -> Vec<(String, String)> {
    parse_rules(DEFAULT_RULES_DOC).unwrap_or_default()
}

/// Merge the user's own rules over the built-in defaults for the engine. User
/// rules win by heard-phrase key (so a user can override a built-in, or disable
/// it by mapping its phrase to itself); non-overridden defaults follow. Capped
/// at MAX_DICTIONARY_RULES so a maxed-out user file can never push the merged
/// set past what the engine accepts — the defaults are the ones dropped.
fn merge_rules(user: &[(String, String)]) -> Vec<(String, String)> {
    let mut merged: Vec<(String, String)> = Vec::with_capacity(user.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (from, to) in user {
        if seen.insert(from.to_lowercase()) {
            merged.push((from.clone(), to.clone()));
        }
    }
    for (from, to) in default_rules() {
        if merged.len() >= MAX_DICTIONARY_RULES {
            break;
        }
        if seen.insert(from.to_lowercase()) {
            merged.push((from, to));
        }
    }
    merged
}

fn parse_rule_line(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (from, to) = line.split_once("=>")?;
    let (from, to) = (from.trim(), to.trim());
    (!from.is_empty() && !to.is_empty()).then_some((from, to))
}

fn validate_rule_sides(from: &str, to: &str) -> Result<(), String> {
    for (name, value) in [("heard phrase", from), ("replacement phrase", to)] {
        if value.len() > MAX_DICTIONARY_SIDE_UTF8_BYTES {
            return Err(format!(
                "dictionary {name} is {} UTF-8 bytes; at most {MAX_DICTIONARY_SIDE_UTF8_BYTES} are allowed",
                value.len()
            ));
        }
    }
    Ok(())
}

fn parse_rules(content: &str) -> Result<Vec<(String, String)>, String> {
    let mut rules = Vec::new();
    for (index, line) in content.lines().enumerate() {
        let line = if index == 0 {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        if let Some((from, to)) = parse_rule_line(line) {
            if rules.len() >= MAX_DICTIONARY_RULES {
                return Err(format!(
                    "dictionary contains more than {MAX_DICTIONARY_RULES} rules"
                ));
            }
            validate_rule_sides(from, to)?;
            // An empty right side is not "teach this word", it is "replace this
            // word with nothing" — and that is what it did: a rule saved through
            // the dictionary's "Add word" path deleted that word from every
            // transcript afterwards. Measured: with `握口扣 => ` in the file,
            // dictating the phrase produced "的是一个语音输入工具" with the word
            // gone. We have no way to bias the recogniser toward a word it does
            // not know, so until we do, a rule that rewrites nothing does
            // nothing rather than destroying text.
            rules.push((from.to_string(), to.to_string()));
        }
    }
    if !rules.is_empty() {
        log::info!("loaded {} replacement rule(s)", rules.len());
    }
    Ok(rules)
}

/// Opaque compare-and-swap token for the exact UTF-8 bytes read from
/// `replacements.txt`. The token crosses the WebView boundary; the save path
/// recomputes it while holding the cross-process writer lock before touching
/// the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RulesRevision(String);

impl RulesRevision {
    fn from_bytes(source: &[u8]) -> Self {
        Self(format!("{:x}", Sha256::digest(source)))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("dictionary revision is invalid; reload Dictionary and try again".into());
        }
        Ok(Self(value.to_string()))
    }
}

/// One coherent dictionary snapshot. `rules` is what the engine applies;
/// `revision` is derived from the complete source document, including comments,
/// blank lines and malformed hand edits that the engine deliberately ignores.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RulesDocument {
    pub(crate) rules: Vec<(String, String)>,
    pub(crate) revision: RulesRevision,
}

#[derive(Debug)]
pub(crate) enum RulesSaveError {
    Conflict(String),
    Other(String),
}

impl RulesSaveError {
    pub(crate) fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict(_))
    }

    fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }
}

impl std::fmt::Display for RulesSaveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(message) | Self::Other(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for RulesSaveError {}

fn rules_document_from_bytes(path: &Path, source: Vec<u8>) -> Result<RulesDocument, String> {
    if source.len() > MAX_DICTIONARY_DOCUMENT_BYTES {
        return Err(format!(
            "replacement rules at {} exceed the {}-byte safety limit; the existing file was left untouched",
            path.display(),
            MAX_DICTIONARY_DOCUMENT_BYTES
        ));
    }
    let revision = RulesRevision::from_bytes(&source);
    let content = String::from_utf8(source).map_err(|error| {
        format!(
            "replacement rules at {} are not valid UTF-8 ({error}); the existing file was left untouched",
            path.display()
        )
    })?;
    Ok(RulesDocument {
        rules: parse_rules(&content).map_err(|error| {
            format!(
                "replacement rules at {} are outside supported bounds ({error}); the existing file was left untouched",
                path.display()
            )
        })?,
        revision,
    })
}

fn load_rules_document_from_with<P>(path: &Path, publish_new: P) -> Result<RulesDocument, String>
where
    P: FnOnce(&Path, &[u8]) -> std::io::Result<()>,
{
    let source = match read_bounded_bytes(path, MAX_DICTIONARY_DOCUMENT_BYTES) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match publish_new(path, RULES_TEMPLATE.as_bytes()) {
                Ok(()) => RULES_TEMPLATE.as_bytes().to_vec(),
                Err(write_error) if write_error.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Another initializer won the no-clobber publish. Its file
                    // is authoritative; read and use it instead of replacing it
                    // with our template.
                    read_bounded_bytes(path, MAX_DICTIONARY_DOCUMENT_BYTES).map_err(|read_error| {
                        format!(
                            "replacement rules appeared concurrently at {}, but could not be read: {read_error}; they were left untouched",
                            path.display()
                        )
                    })?
                }
                Err(write_error) => {
                    return Err(format!(
                        "could not create the default replacement rules at {} without overwriting another writer: {write_error}",
                        path.display()
                    ));
                }
            }
        }
        Err(error) => {
            return Err(format!(
                "could not read replacement rules from {}; the existing path was left untouched: {error}",
                path.display()
            ));
        }
    };
    rules_document_from_bytes(path, source)
}

fn load_rules_document_from(path: &Path) -> Result<RulesDocument, String> {
    load_rules_document_from_with(path, |publish_path, bytes| {
        storage::atomic_write_new(publish_path, bytes)
    })
}

#[cfg(test)]
fn load_rules_from_with<P>(path: &Path, publish_new: P) -> Result<Vec<(String, String)>, String>
where
    P: FnOnce(&Path, &[u8]) -> std::io::Result<()>,
{
    load_rules_document_from_with(path, publish_new).map(|document| document.rules)
}

#[cfg(test)]
fn load_rules_from(path: &Path) -> Result<Vec<(String, String)>, String> {
    load_rules_document_from(path).map(|document| document.rules)
}

struct RulesWriteGuard(std::fs::File);

impl Drop for RulesWriteGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

fn lock_rules_writes(path: &Path) -> Result<RulesWriteGuard, String> {
    let deadline = file_lock_deadline(DOCUMENT_WRITE_LOCK_TIMEOUT, "dictionary write lock")
        .map_err(|error| format!("could not start the dictionary write lock wait: {error}"))?;
    lock_rules_writes_until(path, deadline)
}

fn lock_rules_writes_until(path: &Path, deadline: Instant) -> Result<RulesWriteGuard, String> {
    if Instant::now() >= deadline {
        return Err(format!(
            "dictionary write is busy: {}",
            file_lock_timeout("dictionary write lock")
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("dictionary path {} has no parent", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| {
        format!(
            "could not create dictionary directory {}: {error}",
            parent.display()
        )
    })?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options
        .open(parent.join(".vocalcode-rules-write.lock"))
        .map_err(|error| format!("could not open the dictionary write lock: {error}"))?;
    try_lock_exclusive_until(&lock, deadline, "dictionary write lock").map_err(|error| {
        if error.kind() == std::io::ErrorKind::TimedOut {
            format!("dictionary write is busy: {error}; try again")
        } else {
            format!("could not lock the dictionary for writing: {error}")
        }
    })?;
    Ok(RulesWriteGuard(lock))
}

fn normalize_rule_lines(lines: &[String]) -> Result<Vec<String>, String> {
    if lines.len() > MAX_DICTIONARY_RULES {
        return Err(format!(
            "dictionary has {} rules; at most {MAX_DICTIONARY_RULES} are allowed",
            lines.len()
        ));
    }
    let mut normalized = Vec::with_capacity(lines.len());
    let mut total = 0usize;
    for line in lines {
        if line.contains('\r') || line.contains('\n') {
            return Err("dictionary entries must be single-line phrases".to_string());
        }
        let Some((from, to)) = parse_rule_line(line) else {
            return Err(
                "dictionary entries need one non-empty phrase on each side of =>".to_string(),
            );
        };
        validate_rule_sides(from, to)?;
        let line = format!("{from} => {to}");
        total = total
            .checked_add(line.len() + 1)
            .ok_or_else(|| "dictionary size overflow".to_string())?;
        if total > MAX_DICTIONARY_DOCUMENT_BYTES {
            return Err(format!(
                "dictionary rules exceed the {MAX_DICTIONARY_DOCUMENT_BYTES}-byte document safety limit"
            ));
        }
        normalized.push(line);
    }
    Ok(normalized)
}

fn split_rule_line_ending(line: &str) -> (&str, &str) {
    if let Some(content) = line.strip_suffix("\r\n") {
        (content, "\r\n")
    } else if let Some(content) = line.strip_suffix('\n') {
        (content, "\n")
    } else {
        (line, "")
    }
}

/// Rewrite only syntactically valid rule lines. Every comment, blank line and
/// malformed hand-written line is copied byte-for-byte, so using Settings can
/// never erase evidence that the parser did not understand. Extra new rules are
/// appended using the document's existing newline convention.
fn rewrite_rules_source_normalized(source: &str, normalized: &[String]) -> Result<String, String> {
    let mut replacements = normalized.iter();
    let mut output = String::with_capacity(source.len() + normalized.len() * 32);
    let mut newline = None;

    for (index, line) in source.split_inclusive('\n').enumerate() {
        let (content, ending) = split_rule_line_ending(line);
        let (prefix, rule_content) = if index == 0 {
            content
                .strip_prefix('\u{feff}')
                .map_or(("", content), |content| ("\u{feff}", content))
        } else {
            ("", content)
        };
        if newline.is_none() && !ending.is_empty() {
            newline = Some(ending);
        }
        if parse_rule_line(rule_content).is_some() {
            output.push_str(prefix);
            if let Some(replacement) = replacements.next() {
                output.push_str(replacement);
            }
            output.push_str(ending);
        } else {
            output.push_str(line);
        }
    }

    let newline = newline.unwrap_or("\n");
    if !replacements.as_slice().is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push_str(newline);
        }
        for replacement in replacements {
            output.push_str(replacement);
            output.push_str(newline);
        }
    }
    if output.len() > MAX_DICTIONARY_DOCUMENT_BYTES {
        return Err(format!(
            "dictionary would exceed the {MAX_DICTIONARY_DOCUMENT_BYTES}-byte document safety limit"
        ));
    }
    Ok(output)
}

fn save_rules_document_if_current(
    path: &Path,
    expected: &RulesRevision,
    lines: &[String],
) -> Result<RulesDocument, RulesSaveError> {
    // Validate caller-controlled lines before taking a cross-process lock.
    let normalized = normalize_rule_lines(lines).map_err(RulesSaveError::other)?;
    let _write_guard = lock_rules_writes(path).map_err(RulesSaveError::other)?;
    let source = read_bounded_bytes(path, MAX_DICTIONARY_DOCUMENT_BYTES).map_err(|error| {
        RulesSaveError::other(format!(
            "could not re-read dictionary at {} before saving: {error}; nothing was changed",
            path.display()
        ))
    })?;
    let current = rules_document_from_bytes(path, source.clone()).map_err(RulesSaveError::other)?;
    if &current.revision != expected {
        return Err(RulesSaveError::Conflict(format!(
            "dictionary at {} changed outside this window; reload Dictionary and try again. The newer disk file was left byte-for-byte unchanged",
            path.display()
        )));
    }
    let source = String::from_utf8(source).expect("validated UTF-8 dictionary");
    let replacement =
        rewrite_rules_source_normalized(&source, &normalized).map_err(RulesSaveError::other)?;
    storage::atomic_write(path, replacement.as_bytes()).map_err(|error| {
        RulesSaveError::other(format!(
            "could not save dictionary at {}: {error}; the previous file was left intact",
            path.display()
        ))
    })?;
    rules_document_from_bytes(path, replacement.into_bytes()).map_err(RulesSaveError::other)
}

/// Load the complete document snapshot that Settings must bind a future save to.
pub(crate) fn load_rules_document(base: &Path) -> Result<RulesDocument, String> {
    load_rules_document_from(&base.join("replacements.txt"))
}

/// Save a Settings edit only if the exact source document represented by
/// `expected` is still on disk. Returns the freshly rotated revision.
pub(crate) fn save_rules_if_current(
    base: &Path,
    expected: &RulesRevision,
    lines: &[String],
) -> Result<RulesDocument, RulesSaveError> {
    save_rules_document_if_current(&base.join("replacements.txt"), expected, lines)
}

/// Merge one committed batch of automatically observed corrections without
/// overwriting a manual
/// Settings edit that raced it. The compare-and-swap loop is deliberately
/// bounded; a constantly changing external file is left authoritative.
#[derive(Debug)]
struct LearnCorrectionResult {
    document: RulesDocument,
    changes: Vec<webui::LearnedRuleChange>,
    review_message: Option<&'static str>,
}

fn learn_correction_pairs(
    base: &Path,
    pairs: &[(String, String)],
) -> Result<LearnCorrectionResult, String> {
    if pairs.is_empty() {
        return Err("no corrections were available to learn".to_string());
    }
    for (from, to) in pairs {
        validate_rule_sides(from, to)?;
        if from.trim().is_empty()
            || to.trim().is_empty()
            || from.starts_with('#')
            || from.contains("=>")
            || from.contains(['\r', '\n'])
            || to.contains(['\r', '\n'])
        {
            return Err("Invalid automatic correction; nothing was learned.".into());
        }
    }
    for (index, (from, to)) in pairs.iter().enumerate() {
        if pairs[..index].iter().any(|(earlier_from, earlier_to)| {
            earlier_from.eq_ignore_ascii_case(from) && earlier_to != to
        }) {
            return Err(format!(
                "the same heard phrase maps to conflicting corrections: {from}"
            ));
        }
    }
    for _ in 0..4 {
        let document = load_rules_document(base)?;
        let new_pairs: Vec<_> = pairs
            .iter()
            .filter(|(from, to)| {
                !document
                    .rules
                    .iter()
                    .any(|(a, b)| a.eq_ignore_ascii_case(from) && b == to)
            })
            .cloned()
            .collect();
        if let Some(proposal) = learning::review(&new_pairs, &merge_rules(&document.rules)) {
            let changes = proposal
                .pairs
                .into_iter()
                .map(|(from, to)| {
                    let previous = document
                        .rules
                        .iter()
                        .find(|(a, _)| a.eq_ignore_ascii_case(&from))
                        .cloned();
                    webui::LearnedRuleChange { from, to, previous }
                })
                .collect();
            return Ok(LearnCorrectionResult {
                document,
                changes,
                review_message: Some(proposal.message),
            });
        }
        let mut rules = document.rules.clone();
        let mut changes = Vec::new();
        for (from, to) in pairs {
            if let Some(existing) = rules
                .iter_mut()
                .find(|(heard, _)| heard.eq_ignore_ascii_case(from))
            {
                if existing.0 != *from || existing.1 != *to {
                    let previous = existing.clone();
                    *existing = (from.clone(), to.clone());
                    changes.push(webui::LearnedRuleChange {
                        from: from.clone(),
                        to: to.clone(),
                        previous: Some(previous),
                    });
                }
            } else {
                if rules.len() >= MAX_DICTIONARY_RULES {
                    return Err(format!(
                        "the dictionary already has the maximum of {MAX_DICTIONARY_RULES} rules"
                    ));
                }
                rules.push((from.clone(), to.clone()));
                changes.push(webui::LearnedRuleChange {
                    from: from.clone(),
                    to: to.clone(),
                    previous: None,
                });
            }
        }
        if changes.is_empty() {
            return Ok(LearnCorrectionResult {
                document,
                changes,
                review_message: None,
            });
        }
        let lines = rules
            .iter()
            .map(|(heard, corrected)| format!("{heard} => {corrected}"))
            .collect::<Vec<_>>();
        match save_rules_if_current(base, &document.revision, &lines) {
            Ok(document) => {
                return Ok(LearnCorrectionResult {
                    document,
                    changes,
                    review_message: None,
                });
            }
            Err(error) if error.is_conflict() => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("the dictionary kept changing while VocalCode tried to add the correction".to_string())
}

fn load_rules_checked() -> Result<Vec<(String, String)>, String> {
    load_rules_document(&app_dir()).map(|document| document.rules)
}

/// Directory holding all writable app files (config, model, trial/license).
///
/// Resolved relative to a fixed per-platform location, not the current working
/// directory — otherwise launching from a shortcut or the Dock can't find
/// anything. On Windows this is the exe directory (portable install); on macOS
/// it is `~/Library/Application Support/VocalCode`, because writing inside the
/// signed `.app` bundle would invalidate its signature. See [`paths`].
fn app_dir() -> PathBuf {
    paths::data_dir()
}

static NEXT_INVALID_RECOVERY: AtomicU64 = AtomicU64::new(0);

/// Atomically detach an invalid source into a private, uniquely named recovery
/// directory. Once this succeeds, a concurrent writer can only create a new
/// source path; it cannot be overwritten by the no-clobber publish below.
fn move_invalid_file_to_recovery(path: &Path, _contents: &str) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("settings path {} has no parent directory", path.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("settings path {} has no file name", path.display()))?
        .to_string_lossy();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    for _ in 0..128 {
        let nonce = NEXT_INVALID_RECOVERY.fetch_add(1, Ordering::Relaxed);
        let recovery_dir = parent.join(format!(
            "{file_name}.invalid-{timestamp}-{}-{nonce}",
            std::process::id()
        ));
        match std::fs::create_dir(&recovery_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "could not reserve a recovery path beside {}: {error}",
                    path.display()
                ));
            }
        }
        let recovery = recovery_dir.join("contents");

        if let Err(error) = storage::atomic_move_into_reserved(path, &recovery) {
            let _ = std::fs::remove_dir(&recovery_dir);
            return Err(format!(
                "could not atomically move {} to its reserved recovery path {}: {error}; the source was left untouched",
                path.display(),
                recovery.display(),
            ));
        }

        return Ok(recovery);
    }

    Err(format!(
        "could not allocate a unique recovery file beside {}",
        path.display()
    ))
}

/// Complete invalid-file recovery without a compare→replace race. The old name
/// is first removed atomically by `move_source`; defaults are then linked into
/// that vacant name only if no concurrent writer has recreated it.
fn recover_invalid_source_with<M, P>(
    path: &Path,
    original: &str,
    replacement: &[u8],
    move_source: M,
    publish: P,
) -> Result<PathBuf, String>
where
    M: FnOnce(&Path, &str) -> Result<PathBuf, String>,
    P: FnOnce(&Path, &[u8]) -> std::io::Result<()>,
{
    let recovery = move_source(path, original)?;
    let probe_limit = original.len().checked_add(1).ok_or_else(|| {
        format!(
            "could not verify the source moved from {} to {}: source length overflow; defaults were not installed",
            path.display(),
            recovery.display()
        )
    })?;
    let mut moved = Vec::with_capacity(probe_limit);
    std::fs::File::open(&recovery)
        .and_then(|file| file.take(probe_limit as u64).read_to_end(&mut moved))
        .map_err(|error| {
            format!(
            "could not verify the source moved from {} to {}: {error}; defaults were not installed",
            path.display(),
            recovery.display()
        )
        })?;

    if moved.len() > original.len() {
        // We deliberately did not accumulate the complete raced file. Restore
        // its exact bytes without reading or copying them by adding a second
        // no-clobber name for the recovery inode; the reserved recovery name
        // remains available as evidence.
        let disposition = match std::fs::hard_link(&recovery, path) {
            Ok(()) => "the complete moved file was also restored to the source path".to_string(),
            Err(error) if path.exists() => {
                format!("a concurrent source already exists and was preserved ({error})")
            }
            Err(error) => format!(
                "the complete moved file remains only in recovery because restoring its name failed: {error}"
            ),
        };
        return Err(format!(
            "{} changed before it could be moved to {}; the moved bytes did not match the version initially read (verification stopped after {} bytes), so defaults were not installed and {disposition}",
            path.display(),
            recovery.display(),
            probe_limit,
        ));
    }

    if moved != original.as_bytes() {
        let restore = storage::atomic_write_new(path, &moved);
        let disposition = match restore {
            Ok(()) => "the moved bytes were also restored to the source path".to_string(),
            Err(error) if path.exists() => {
                format!("a concurrent source already exists and was preserved ({error})")
            }
            Err(error) => format!(
                "the moved bytes remain in recovery because restoring the source failed: {error}"
            ),
        };
        return Err(format!(
            "{} changed before it could be moved to {}; the moved bytes did not match the version initially read, so defaults were not installed and {disposition}",
            path.display(),
            recovery.display()
        ));
    }

    if let Err(error) = publish(path, replacement) {
        let disposition = if path.exists() {
            "the concurrently created source was preserved".to_string()
        } else {
            match storage::atomic_write_new(path, &moved) {
                Ok(()) => "the original invalid source was restored".to_string(),
                Err(restore_error) => format!(
                    "the original remains available only in recovery because restoring it failed: {restore_error}"
                ),
            }
        };
        return Err(format!(
            "the invalid source was preserved at {}, but defaults could not be published without overwriting {}: {error}; {disposition}",
            recovery.display(),
            path.display()
        ));
    }

    Ok(recovery)
}

fn recover_invalid_source(
    path: &Path,
    original: &str,
    replacement: &[u8],
) -> Result<PathBuf, String> {
    recover_invalid_source_with(
        path,
        original,
        replacement,
        move_invalid_file_to_recovery,
        |publish_path, bytes| storage::atomic_write_new(publish_path, bytes),
    )
}

fn migrate_loaded_config_in_memory(path: &Path, mut config: Config) -> Config {
    if config.migrate() {
        // Startup is read-only with respect to an existing valid settings file:
        // another process or a hand editor may change it immediately after our
        // read. The migrated representation is persisted only by an explicit
        // Settings save, which already owns the user's chosen snapshot.
        log::info!(
            "migrated config from {} to v{} in memory; it will be persisted on the next Settings save",
            path.display(),
            vocalcode_core::config::CONFIG_VERSION
        );
    }
    config
}

fn accept_supported_config(path: &Path, config: Config) -> Result<Config, String> {
    let supported = vocalcode_core::config::CONFIG_VERSION;
    if config.config_version > supported {
        return Err(format!(
            "settings at {} were written by a newer VocalCode config version {} (this build supports through {}); the file was left byte-for-byte unchanged. Install a newer VocalCode build or restore a compatible settings file",
            path.display(),
            config.config_version,
            supported,
        ));
    }
    config.validate_bounds().map_err(|error| {
        format!(
            "settings at {} are outside supported safety bounds ({error}); the file was left byte-for-byte unchanged",
            path.display()
        )
    })?;
    Ok(migrate_loaded_config_in_memory(path, config))
}

/// Inspect the generic TOML tree before deserializing the current schema. A
/// future release is allowed to change field types, so typed deserialization
/// may fail even though the file is valid and its version says exactly why.
/// Such a file is authoritative evidence, not corrupt input to recover over.
fn reject_future_config_version(path: &Path, source: &str) -> Result<(), String> {
    let Ok(value) = toml::from_str::<toml::Value>(source) else {
        // Preserve the existing invalid-TOML recovery path and its exact backup.
        return Ok(());
    };
    let Some(version) = value
        .get("config_version")
        .and_then(toml::Value::as_integer)
    else {
        return Ok(());
    };
    let supported = i64::from(vocalcode_core::config::CONFIG_VERSION);
    if version > supported {
        return Err(format!(
            "settings at {} were written by a newer VocalCode config version {} (this build supports through {}); the file was left byte-for-byte unchanged. Install a newer VocalCode build or restore a compatible settings file",
            path.display(),
            version,
            supported,
        ));
    }
    Ok(())
}

struct ConfigWriteGuard(std::fs::File);

impl Drop for ConfigWriteGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

fn lock_config_writes(path: &Path) -> Result<ConfigWriteGuard, String> {
    let deadline = file_lock_deadline(DOCUMENT_WRITE_LOCK_TIMEOUT, "settings write lock")
        .map_err(|error| format!("could not start the settings write lock wait: {error}"))?;
    lock_config_writes_until(path, deadline)
}

fn lock_config_writes_until(path: &Path, deadline: Instant) -> Result<ConfigWriteGuard, String> {
    if Instant::now() >= deadline {
        return Err(format!(
            "settings write is busy: {}",
            file_lock_timeout("settings write lock")
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("settings path {} has no parent", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| {
        format!(
            "could not create settings directory {}: {error}",
            parent.display()
        )
    })?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(parent.join(".vocalcode-config-write.lock"))
        .map_err(|error| format!("could not open the settings write lock: {error}"))?;
    try_lock_exclusive_until(&lock, deadline, "settings write lock").map_err(|error| {
        if error.kind() == std::io::ErrorKind::TimedOut {
            format!("settings write is busy: {error}; try again")
        } else {
            format!("could not lock settings for writing: {error}")
        }
    })?;
    Ok(ConfigWriteGuard(lock))
}

/// Publish a settings edit only if disk still represents the snapshot the UI
/// edited. This serializes VocalCode writers and refuses a hand edit, another
/// process's save, malformed data, or a future-version file rather than
/// replacing it with stale in-memory state.
pub(crate) fn persist_config_if_current(
    path: &Path,
    expected: &Config,
    replacement: &Config,
) -> Result<(), String> {
    replacement.validate_bounds().map_err(|error| {
        format!("refusing to save settings outside supported safety bounds: {error}")
    })?;
    let _write_guard = lock_config_writes(path)?;
    let source = read_bounded_string(path, MAX_CONFIG_DOCUMENT_BYTES).map_err(|error| {
        format!(
            "could not re-read settings at {} before saving: {error}; nothing was changed",
            path.display()
        )
    })?;
    reject_future_config_version(path, &source)?;
    let mut disk = toml::from_str::<Config>(&source).map_err(|error| {
        format!(
            "settings at {} changed to an incompatible or invalid document ({error}); nothing was overwritten",
            path.display()
        )
    })?;
    if disk.config_version > vocalcode_core::config::CONFIG_VERSION {
        return accept_supported_config(path, disk).map(|_| ());
    }
    disk.validate_bounds().map_err(|error| {
        format!(
            "settings at {} changed to a document outside supported safety bounds ({error}); nothing was overwritten",
            path.display()
        )
    })?;
    disk.migrate();
    // Launch-at-login is owned by the per-user registry/launchd state. Startup
    // deliberately replaces the TOML value with that OS truth; comparing the
    // stale cache here would make every unrelated settings save fail after an
    // installer migration or an external OS settings change. The successful
    // replacement below writes the observed value back to TOML.
    disk.autostart = expected.autostart;
    if !config_snapshots_match(&disk, expected)? {
        return Err(format!(
            "settings at {} changed outside this window; reload Settings and try again. The newer disk file was left untouched",
            path.display()
        ));
    }
    let serialized = toml::to_string_pretty(replacement).map_err(|error| error.to_string())?;
    if serialized.len() > MAX_CONFIG_DOCUMENT_BYTES {
        return Err(format!(
            "refusing to save settings larger than {MAX_CONFIG_DOCUMENT_BYTES} bytes"
        ));
    }
    storage::atomic_write(path, serialized).map_err(|error| error.to_string())
}

fn load_config_from_with_publish<R, P>(
    path: &Path,
    recover: R,
    publish_new: P,
) -> Result<Config, String>
where
    R: FnOnce(&Path, &str, &[u8]) -> Result<PathBuf, String>,
    P: FnOnce(&Path, &[u8]) -> std::io::Result<()>,
{
    match read_bounded_string(path, MAX_CONFIG_DOCUMENT_BYTES) {
        Ok(s) => {
            reject_future_config_version(path, &s)?;
            match toml::from_str::<Config>(&s) {
                Ok(c) => {
                    log::info!("loaded config from {}", path.display());
                    accept_supported_config(path, c)
                }
                Err(e) => {
                    // A default config is allowed to replace invalid TOML only after
                    // the exact source text has reached a unique, flushed recovery
                    // file. If that cannot be guaranteed, propagate the error so no
                    // later save can silently erase the only copy.
                    let config = Config::default();
                    let serialized = toml::to_string_pretty(&config).map_err(|error| {
                    format!(
                        "invalid settings in {} ({e}), and defaults could not be serialized: {error}; the original was left untouched",
                        path.display()
                    )
                })?;
                    let recovery = recover(path, &s, serialized.as_bytes()).map_err(|error| {
                        format!("invalid settings in {} ({e}); {error}", path.display(),)
                    })?;
                    log::warn!(
                        "invalid {} ({e}); exact source preserved as {} and defaults installed",
                        path.display(),
                        recovery.display()
                    );
                    Ok(config)
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let config = Config::default();
            let serialized = toml::to_string_pretty(&config)
                .map_err(|error| format!("could not serialize default settings: {error}"))?;
            match publish_new(path, serialized.as_bytes()) {
                Ok(()) => Ok(config),
                Err(write_error)
                    if write_error.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    let concurrent = read_bounded_string(path, MAX_CONFIG_DOCUMENT_BYTES).map_err(|read_error| {
                        format!(
                            "settings appeared concurrently at {}, but could not be read: {read_error}; they were left untouched",
                            path.display()
                        )
                    })?;
                    reject_future_config_version(path, &concurrent)?;
                    let parsed = toml::from_str::<Config>(&concurrent).map_err(|parse_error| {
                        format!(
                            "settings appeared concurrently at {}, but are invalid ({parse_error}); they were left untouched",
                            path.display()
                        )
                    })?;
                    log::info!("adopted settings concurrently created at {}", path.display());
                    accept_supported_config(path, parsed)
                }
                Err(write_error) => Err(format!(
                    "settings do not exist and defaults could not be created at {} without overwriting another writer: {write_error}",
                    path.display()
                )),
            }
        }
        Err(error) => Err(format!(
            "could not read settings from {}; the existing path was left untouched: {error}",
            path.display()
        )),
    }
}

fn load_config_from_with(
    path: &Path,
    recover: impl FnOnce(&Path, &str, &[u8]) -> Result<PathBuf, String>,
) -> Result<Config, String> {
    load_config_from_with_publish(path, recover, |publish_path, bytes| {
        storage::atomic_write_new(publish_path, bytes)
    })
}

fn load_config_from(path: &Path) -> Result<Config, String> {
    load_config_from_with(path, recover_invalid_source)
}

fn load_config() -> Result<Config, String> {
    load_config_from(&app_dir().join("vocalcode.toml"))
}

// ---------------------------------------------------------------------------
// ASR model + cleaners (see `models`)
// ---------------------------------------------------------------------------

/// Locate the punctuation model (data dir, cwd, or a local CapsWriter copy).
#[cfg(feature = "diagnostic-cli")]
fn find_punct() -> Option<PathBuf> {
    // `mut` is only needed on Windows, where the CapsWriter path is appended.
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut candidates = vec![
        app_dir().join("models").join("punct").join("model.onnx"),
        PathBuf::from("models").join("punct").join("model.onnx"),
    ];
    // Reuse an existing CapsWriter-Offline install if one is present. Windows
    // only — that layout does not exist on macOS.
    #[cfg(windows)]
    candidates.push(PathBuf::from(
        r"C:\Apps\CapsWriter-Offline\models\Punct-CT-Transformer\sherpa-onnx-punct-ct-transformer-zh-en-vocab272727-2024-04-12\model.onnx",
    ));
    candidates.into_iter().find(|m| m.exists())
}

// ---------------------------------------------------------------------------
// License / trial
// ---------------------------------------------------------------------------

/// The active file is capped on every write, not just at startup. Keeping only
/// one previous generation bounds diagnostics to roughly four MiB even when a
/// GUI process runs for weeks.
const LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Open a private app-owned path without ever following a final-component link.
/// Validation is performed through the returned handle, so there is no
/// check-then-open window in which a link can be swapped in.
fn open_private_regular_nofollow(
    path: &Path,
    configure: impl FnOnce(&mut std::fs::OpenOptions),
) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    configure(&mut options);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(target_os = "macos")]
        const O_NOFOLLOW: i32 = 0x0000_0100;
        #[cfg(not(target_os = "macos"))]
        const O_NOFOLLOW: i32 = 0x0002_0000;
        options.custom_flags(O_NOFOLLOW).mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: open the link itself so the handle can
        // be rejected below instead of silently opening its victim.
        options.custom_flags(0x0020_0000);
    }

    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(std::io::Error::other(format!(
                "{} is a reparse point",
                path.display()
            )));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(file)
}

struct LogWriteGuard(std::fs::File);

impl Drop for LogWriteGuard {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

fn process_log_write_lock(lock_path: &Path) -> Arc<ProcessLogLock> {
    let registry = LOG_WRITE_PROCESS_LOCKS
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut locks = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(lock) = locks.get(lock_path).and_then(std::sync::Weak::upgrade) {
        return lock;
    }
    locks.retain(|_, lock| lock.strong_count() != 0);
    let lock = Arc::new(ProcessLogLock {
        queue: std::sync::Mutex::new(ProcessLogQueue::default()),
        changed: std::sync::Condvar::new(),
    });
    locks.insert(lock_path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

fn lock_log_process_until(
    lock: &ProcessLogLock,
    deadline: Instant,
) -> std::io::Result<ProcessLogGuard<'_>> {
    let mut queue = lock
        .queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ticket = queue.next;
    queue.next = queue
        .next
        .checked_add(1)
        .ok_or_else(|| std::io::Error::other("log write lock ticket space exhausted"))?;
    loop {
        if queue.serving == ticket {
            return Ok(ProcessLogGuard { lock });
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            queue.cancelled.insert(ticket);
            return Err(file_lock_timeout("log write lock"));
        }
        let (next_queue, timeout) = lock
            .changed
            .wait_timeout(queue, remaining)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue = next_queue;
        if timeout.timed_out() && queue.serving != ticket {
            queue.cancelled.insert(ticket);
            return Err(file_lock_timeout("log write lock"));
        }
    }
}

fn lock_log_file_until(lock_path: &Path, deadline: Instant) -> std::io::Result<std::fs::File> {
    if Instant::now() >= deadline {
        return Err(file_lock_timeout("log write lock"));
    }
    let lock = open_private_regular_nofollow(lock_path, |options| {
        options.read(true).write(true).create(true).truncate(false);
    })?;
    try_lock_exclusive_until(&lock, deadline, "log write lock")?;
    Ok(lock)
}

fn remove_log_backup_if_present(path: &Path) -> std::io::Result<()> {
    let file = match open_private_regular_nofollow(path, |options| {
        options.read(true).write(true);
    }) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    // Windows rename/delete sharing is easiest to reason about when no process
    // retains a handle. Every log handle, including this validation handle, is
    // deliberately scoped to one transaction.
    drop(file);
    std::fs::remove_file(path)
}

fn rotate_log_locked(active: &Path, backup: &Path) -> std::io::Result<()> {
    remove_log_backup_if_present(backup)?;
    std::fs::rename(active, backup)
}

#[derive(Debug)]
struct LogFileSink {
    active: PathBuf,
    backup: PathBuf,
    lock: PathBuf,
    process_lock: Arc<ProcessLogLock>,
    max_bytes: u64,
    enabled: bool,
}

impl LogFileSink {
    fn new(dir: &Path, max_bytes: u64) -> Self {
        let lock = dir.join(".vocalcode-log-write.lock");
        Self {
            active: dir.join("vocalcode.log"),
            backup: dir.join("vocalcode.log.1"),
            process_lock: process_log_write_lock(&lock),
            lock,
            max_bytes,
            enabled: true,
        }
    }

    fn append_locked(&self, bytes: &[u8]) -> std::io::Result<()> {
        let deadline = file_lock_deadline(LOG_WRITE_LOCK_TIMEOUT, "log write lock")?;
        let _process_guard = lock_log_process_until(&self.process_lock, deadline)?;
        let file_guard = lock_log_file_until(&self.lock, deadline)?;
        let _file_guard = LogWriteGuard(file_guard);
        let bytes = &bytes[..bytes.len().min(self.max_bytes as usize)];
        let mut active = open_private_regular_nofollow(&self.active, |options| {
            options.create(true).append(true);
        })?;
        let current = active.metadata()?.len();
        if current > self.max_bytes || current.saturating_add(bytes.len() as u64) > self.max_bytes {
            drop(active);
            rotate_log_locked(&self.active, &self.backup)?;
            active = open_private_regular_nofollow(&self.active, |options| {
                options.create(true).append(true);
            })?;
        }
        std::io::Write::write_all(&mut active, bytes)?;
        // Dropping here is intentional. A process-lifetime handle prevents a
        // different VocalCode process from rotating the file on Windows.
        drop(active);
        Ok(())
    }

    fn append(&mut self, bytes: &[u8]) {
        if !self.enabled {
            return;
        }
        if let Err(error) = self.append_locked(bytes) {
            self.enabled = false;
            eprintln!(
                "file logging disabled after a safe write/rotation failure at {}: {error}",
                self.active.display()
            );
        }
    }
}

/// stderr keeps working when the app is run from a shell; `LogFileSink` is the
/// bounded copy that survives a normal GUI launch. File errors never make the
/// logger fail, recurse through `log`, truncate evidence, or stop stderr.
struct LogTee(LogFileSink);

impl std::io::Write for LogTee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::Write::write_all(&mut std::io::stderr(), buf);
        self.0.append(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::Write::flush(&mut std::io::stderr());
        Ok(())
    }
}

/// Log to a bounded file as well as stderr. Deliberately not a transcript log:
/// recognised text must never be written here.
fn init_logging() {
    let dir = app_dir();
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("could not create log directory {}: {error}", dir.display());
    }
    let path = dir.join("vocalcode.log");
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(Box::new(LogTee(
            LogFileSink::new(&dir, LOG_MAX_BYTES),
        ))))
        .init();
    log::info!(
        "VocalCode {} starting — log at {}",
        env!("CARGO_PKG_VERSION"),
        path.display()
    );
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Current license/trial state. Both paid and trial authority comes from a
/// cached, signed, device-matched receipt; no editable local timestamp grants
/// access.
fn license_status() -> LicenseStatus {
    // No device fingerprint, receipt read, migration, or trusted-clock write
    // is needed for community access. Do not manufacture a paid receipt.
    if community::ENABLED {
        return LicenseStatus::TrialSetupRequired;
    }
    let required_version = env!("CARGO_PKG_VERSION")
        .split('.')
        .next()
        .and_then(|major| major.parse::<u32>().ok())
        // Pre-1.0 builds belong to the first paid-upgrade generation.
        .map(|major| major.max(1))
        // An unparsable build version must never accidentally weaken the gate.
        .unwrap_or(u32::MAX);
    let base = app_dir();
    let device = device_id();
    if let Some(max_version) = activation::license_entitlement(&base, &device) {
        return if max_version >= required_version {
            LicenseStatus::Licensed { max_version }
        } else {
            LicenseStatus::Invalid(format!(
                "This licence covers VocalCode through generation {max_version}, but this build requires generation {required_version}."
            ))
        };
    }
    let now = now_unix();
    match activation::trial_status(&base, &device, now) {
        activation::TrialReceiptStatus::Active(claims) => LicenseStatus::Trial {
            days_left: trial_days_left(claims.iat, now, TRIAL_DAYS),
        },
        activation::TrialReceiptStatus::Expired => LicenseStatus::Expired,
        activation::TrialReceiptStatus::SetupRequired => LicenseStatus::TrialSetupRequired,
        activation::TrialReceiptStatus::Invalid(error) => LicenseStatus::Invalid(error),
    }
}

/// Product access derived from the signed paid/trial receipt.
///
/// Basic dictation is deliberately not represented here: a missing, expired,
/// or invalid receipt cannot disable it. A valid paid receipt or the existing
/// 30-day trial unlocks Pro.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProductTier {
    Basic,
    Pro,
    Community,
}

fn product_tier(status: &LicenseStatus) -> ProductTier {
    if community::ENABLED {
        return ProductTier::Community;
    }
    match status {
        LicenseStatus::Licensed { .. } | LicenseStatus::Trial { .. } => ProductTier::Pro,
        LicenseStatus::TrialSetupRequired | LicenseStatus::Expired | LicenseStatus::Invalid(_) => {
            ProductTier::Basic
        }
    }
}

/// The licence state in a form the window can translate.
///
/// `license_string` cannot be translated — it embeds a day count, so no
/// dictionary keyed on English source text will ever match it. Sending the
/// parts instead lets the page build the sentence in its own language.
fn license_state(status: &LicenseStatus) -> (String, u32) {
    if community::ENABLED {
        return ("community".to_string(), 0);
    }
    match status {
        LicenseStatus::Licensed { .. } => ("licensed".to_string(), 0),
        LicenseStatus::Trial { days_left } => ("trial".to_string(), *days_left),
        LicenseStatus::TrialSetupRequired | LicenseStatus::Expired => ("basic".to_string(), 0),
        LicenseStatus::Invalid(_) => ("basic_error".to_string(), 0),
    }
}

/// Short display string for the license/trial state.
fn license_string(status: &LicenseStatus) -> String {
    if community::ENABLED {
        return community::LABEL.to_string();
    }
    match status {
        LicenseStatus::Licensed { max_version } => format!("Activated (v{max_version})"),
        LicenseStatus::Trial { days_left } => format!("Pro trial — {days_left} days left"),
        LicenseStatus::TrialSetupRequired | LicenseStatus::Expired => {
            "Basic — free forever".to_string()
        }
        LicenseStatus::Invalid(e) => format!("Basic — Pro licence needs attention ({e})"),
    }
}

/// Core local dictation is the permanent Basic product and never depends on a
/// receipt. Keeping this as a function makes the policy explicit at every
/// former licence-gate call site and gives it a regression test.
fn injection_allowed(_status: &LicenseStatus) -> bool {
    true
}

fn pro_allowed(status: &LicenseStatus) -> bool {
    matches!(
        product_tier(status),
        ProductTier::Pro | ProductTier::Community
    )
}

// ---------------------------------------------------------------------------
// Background engine + hotkey threads
// ---------------------------------------------------------------------------

/// Publish a download's progress for the UI: a numeric form for the bar and a
/// human line for the status text.
fn publish_progress(status: &Arc<RuntimeStatus>, p: models::Progress) {
    let mb = |b: u64| b as f64 / 1_000_000.0;
    *status.model_label.lock().unwrap() = format!(
        "Downloading {} … {:.0}%  ({:.0} / {:.0} MB)",
        p.label,
        p.percent(),
        mb(p.done),
        mb(p.total)
    );
    *status.model_download.lock().unwrap() =
        Some((p.label.clone(), p.percent(), mb(p.done), mb(p.total)));
}

fn desired_config_snapshot(
    status: &RuntimeStatus,
    shared_config: &std::sync::Mutex<Config>,
) -> (u64, Config) {
    let apply = status
        .config_apply
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let config = shared_config
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    (apply.generation(), config)
}

fn begin_model_prepare(
    status: &RuntimeStatus,
    generation: u64,
    config: &Config,
) -> Option<models::CancellationToken> {
    if status.shutdown.load(Ordering::Acquire) {
        return None;
    }
    let cancellation = status
        .config_apply
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .begin_prepare(generation, config)?;
    // The new attempt owns the setup banner now: its progress replaces the
    // previous failure until it succeeds or records a failure of its own.
    *status
        .model_failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    Some(cancellation)
}

/// Schedule the automatic retry of a failed model preparation, publish the
/// failure for the red setup banner, and return when the retry is due.
/// Downloads resume from what is on disk, so a failed attempt that still
/// moved the download forward restarts the delays instead of lengthening them.
fn schedule_model_retry(
    status: &RuntimeStatus,
    backoff: &mut ModelRetryBackoff,
    config: &Config,
    base: &Path,
    message: &str,
) -> Instant {
    let downloaded = models::downloaded_bytes(&config.model, &config.language, base);
    let now = Instant::now();
    let retry_at = backoff.next_deadline_after(now, downloaded.map(|(done, _)| done));
    // The page counts down on its own clock, so it gets wall-clock time.
    let retry_at_ms = (SystemTime::now() + retry_at.saturating_duration_since(now))
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|since| since.as_millis() as u64);
    *status
        .model_failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(webui::ModelFailure {
        message: message.to_string(),
        retry_at_ms,
        downloaded,
        smaller_model: models::smaller_alternative(&config.model, &config.language),
    });
    retry_at
}

fn finish_model_prepare(status: &RuntimeStatus, generation: u64) {
    status
        .config_apply
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .finish_prepare(generation);
}

fn publish_model_progress(
    status: &Arc<RuntimeStatus>,
    generation: u64,
    progress: models::Progress,
) {
    let apply = status
        .config_apply
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if apply.is_current(generation) && !status.shutdown.load(Ordering::Acquire) {
        publish_progress(status, progress);
    }
}

type PreparedModel = (
    Box<dyn Asr>,
    Vec<Box<dyn vocalcode_core::traits::TextCleaner>>,
    String,
);

fn prepare_model_pipeline(
    config: &Config,
    base: &Path,
    threads: i32,
    cancellation: &models::CancellationToken,
    status: &Arc<RuntimeStatus>,
    generation: u64,
) -> models::ModelPrepareResult<PreparedModel> {
    let progress_status = status.clone();
    let (asr, label) = models::prepare_asr_cancellable(
        &config.model,
        &config.language,
        base,
        threads,
        cancellation,
        move |progress| publish_model_progress(&progress_status, generation, progress),
    )?;
    let progress_status = status.clone();
    let cleaners = models::prepare_cleaners_cancellable(
        &config.model,
        &config.language,
        base,
        cancellation,
        move |progress| publish_model_progress(&progress_status, generation, progress),
    )?;
    Ok((asr, cleaners, label))
}

/// Record a transcription in the session history.
/// Lifetime totals, kept across restarts.
///
/// Deliberately not the transcripts. Those stay in memory only, because a
/// dictation log on disk is a privacy liability for an app whose whole pitch is
/// that nothing leaves the machine — but that argument is about the *text*, not
/// about how much of it there was. Three integers give away nothing, and without
/// them the window said "you have saved 12 min of typing with VocalCode" and
/// then reset to zero the next time you opened it, which reads as a lifetime
/// achievement and behaved like a scratch counter. Updating the app made it
/// worse: the reward for updating was losing the number.
#[derive(Debug, Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Totals {
    pub dictations: u64,
    pub words: u64,
    pub chars: u64,
}

fn totals_path() -> PathBuf {
    app_dir().join("totals.json")
}

static TOTALS_WRITABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn load_totals_from_with(
    path: &Path,
    recover: impl FnOnce(&Path, &str, &[u8]) -> Result<PathBuf, String>,
) -> Result<Totals, String> {
    let source = match read_bounded_string(path, MAX_TOTALS_DOCUMENT_BYTES) {
        Ok(source) => source,
        // There is no data to lose. Defer creating the file until the first
        // dictation, when there are actual counters worth persisting.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Totals::default());
        }
        Err(error) => {
            return Err(format!(
                "could not read lifetime totals from {}; writes are disabled for this session and the existing path was left untouched: {error}",
                path.display()
            ));
        }
    };

    match serde_json::from_str::<Totals>(&source) {
        Ok(totals) => Ok(totals),
        Err(parse_error) => {
            let totals = Totals::default();
            let serialized = serde_json::to_string(&totals).map_err(|error| {
                format!(
                    "invalid lifetime totals in {} ({parse_error}), and defaults could not be serialized: {error}; writes are disabled for this session and the original was left untouched",
                    path.display()
                )
            })?;
            let recovery = recover(path, &source, serialized.as_bytes()).map_err(|error| {
                format!(
                    "invalid lifetime totals in {} ({parse_error}); {error}; writes are disabled for this session",
                    path.display(),
                )
            })?;
            log::warn!(
                "invalid lifetime totals in {} ({parse_error}); exact source preserved as {} and fresh counters installed",
                path.display(),
                recovery.display()
            );
            Ok(totals)
        }
    }
}

fn load_totals_from(path: &Path) -> Result<Totals, String> {
    load_totals_from_with(path, recover_invalid_source)
}

fn load_totals() -> Totals {
    match load_totals_from(&totals_path()) {
        Ok(totals) => {
            TOTALS_WRITABLE.store(true, Ordering::Release);
            totals
        }
        Err(error) => {
            TOTALS_WRITABLE.store(false, Ordering::Release);
            log::error!("{error}");
            Totals::default()
        }
    }
}

/// Add one dictation to the running totals and write them back.
///
/// Written every time rather than on exit: the app is normally quit by closing
/// the window or being replaced by an update, neither of which is a clean
/// shutdown we could hook. A few dozen bytes per utterance is nothing next to
/// losing the count.
fn bump_totals(status: &Arc<RuntimeStatus>, text: &str) {
    let words = text.split_whitespace().filter(|w| !w.is_empty()).count() as u64;
    let chars = text.chars().count() as u64;
    let mut t = status.totals.lock().unwrap();
    t.dictations = t.dictations.saturating_add(1);
    t.words = t.words.saturating_add(words);
    t.chars = t.chars.saturating_add(chars);
    let snapshot = *t;
    drop(t);
    // A load failure means zero is not known to be the true previous count.
    // Keep useful in-memory session counters, but never turn that guess into a
    // destructive overwrite of the unreadable existing file.
    if !TOTALS_WRITABLE.load(Ordering::Acquire) {
        return;
    }
    match serde_json::to_string(&snapshot) {
        Ok(j) => {
            if let Err(e) = storage::atomic_write(&totals_path(), j) {
                log::warn!("save totals: {e}");
            }
        }
        Err(e) => log::warn!("serialise totals: {e}"),
    }
}

fn push_history(status: &Arc<RuntimeStatus>, text: &str) {
    bump_totals(status, text);
    let secs = now_unix();
    if let Ok(mut h) = status.history.lock() {
        h.insert(0, webui::HistoryEntry::new(secs, text.to_string()));
        // Bounded: this lives in memory and is pushed to the UI on every tick.
        h.truncate(50);
    }
}

fn publish_filler_review(status: &RuntimeStatus, trace: &vocalcode_core::engine::DictationTrace) {
    if (trace.filler_removed == 0 && trace.writing_edits == 0) || trace.raw_text_truncated {
        return;
    }
    if let Ok(mut history) = status.history.lock() {
        if trace.final_text.is_empty() && trace.result == "delivery_completed" {
            // A pause-only utterance inserts nothing, but its original remains
            // available to copy without counting an empty dictation in totals.
            history.insert(
                0,
                webui::HistoryEntry::new(now_unix(), String::new()).with_trace(trace),
            );
            history.truncate(50);
        } else if let Some(entry) = history.first_mut() {
            // Called before handling the next input event. Do not associate a
            // failed/partial transcript with an unrelated previous history row.
            if !trace.final_text.is_empty() && entry.text == trace.final_text {
                *entry = entry.clone().with_trace(trace);
            }
        }
    }
}

/// Renew finite signed receipts without holding up the first window. The first
/// pass starts immediately; later passes keep a long-running tray process from
/// crossing an expiry boundary without attempting renewal or updating its gate.
const LOCAL_LICENSE_GATE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
const NETWORK_LICENSE_REFRESH_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(12 * 60 * 60);
const LICENSE_REFRESH_RETRY_DELAYS: [std::time::Duration; 4] = [
    std::time::Duration::from_secs(60),
    std::time::Duration::from_secs(5 * 60),
    std::time::Duration::from_secs(30 * 60),
    std::time::Duration::from_secs(60 * 60),
];
const MODEL_RETRY_DELAYS: [std::time::Duration; 5] = [
    std::time::Duration::from_secs(30),
    std::time::Duration::from_secs(60),
    std::time::Duration::from_secs(2 * 60),
    std::time::Duration::from_secs(5 * 60),
    std::time::Duration::from_secs(10 * 60),
];

#[derive(Default)]
struct ModelRetryBackoff {
    failures: usize,
    /// Bytes on disk when the previous failure was scheduled.
    downloaded: Option<u64>,
}

impl ModelRetryBackoff {
    fn next_deadline(&mut self, now: std::time::Instant) -> std::time::Instant {
        let delay = MODEL_RETRY_DELAYS[self.failures.min(MODEL_RETRY_DELAYS.len() - 1)];
        self.failures = self.failures.saturating_add(1);
        now + delay
    }

    /// `next_deadline`, except that a failure after more bytes reached the
    /// disk than at the previous one starts the delays over. A connection
    /// that keeps dropping but keeps delivering is making progress, and
    /// stretching its retries to ten minutes would only waste that.
    fn next_deadline_after(
        &mut self,
        now: std::time::Instant,
        downloaded: Option<u64>,
    ) -> std::time::Instant {
        if let (Some(before), Some(after)) = (self.downloaded, downloaded) {
            if after > before {
                self.failures = 0;
            }
        }
        self.downloaded = downloaded;
        self.next_deadline(now)
    }

    fn reset(&mut self) {
        self.failures = 0;
        self.downloaded = None;
    }
}

#[derive(Default)]
struct LicenseRefreshBackoff {
    failures: usize,
}

impl LicenseRefreshBackoff {
    fn next_deadline(
        &mut self,
        now: std::time::Instant,
        attempt_succeeded: bool,
    ) -> std::time::Instant {
        if attempt_succeeded {
            self.failures = 0;
            return now + NETWORK_LICENSE_REFRESH_INTERVAL;
        }
        let delay =
            LICENSE_REFRESH_RETRY_DELAYS[self.failures.min(LICENSE_REFRESH_RETRY_DELAYS.len() - 1)];
        self.failures = self.failures.saturating_add(1);
        now + delay
    }
}

fn run_license_refresh_sequence<L, C, T, S, LR, CR, TR>(
    legacy: L,
    cached: C,
    trial: T,
    should_stop: S,
) -> Option<(LR, CR, TR)>
where
    L: FnOnce() -> LR,
    C: FnOnce() -> CR,
    T: FnOnce() -> TR,
    S: Fn() -> bool,
{
    let legacy = legacy();
    if should_stop() {
        return None;
    }
    let cached = cached();
    if should_stop() {
        return None;
    }
    let trial = trial();
    if should_stop() {
        return None;
    }
    Some((legacy, cached, trial))
}

fn start_license_maintenance(status: Arc<RuntimeStatus>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        if community::ENABLED {
            return;
        }
        let mut next_network = std::time::Instant::now();
        let mut refresh_backoff = LicenseRefreshBackoff::default();
        loop {
            if status.shutdown.load(Ordering::Acquire) {
                return;
            }
            let now = std::time::Instant::now();
            if now >= next_network {
                let base = app_dir();
                let device = device_id();
                let Some((legacy, cached, trial)) = run_license_refresh_sequence(
                    || activation::refresh_legacy_receipt_cancellable(&base, &status.shutdown),
                    || {
                        activation::refresh_cached_receipt_cancellable(
                            &base,
                            &device,
                            &status.shutdown,
                        )
                    },
                    || {
                        activation::refresh_trial_receipt_cancellable(
                            &base,
                            &device,
                            &status.shutdown,
                        )
                    },
                    || status.shutdown.load(Ordering::Acquire),
                ) else {
                    return;
                };
                let legacy = match legacy {
                    Ok(Some(value)) => Ok(value),
                    Ok(None) => return,
                    Err(error) => Err(error),
                };
                let cached = match cached {
                    Ok(Some(value)) => Ok(value),
                    Ok(None) => return,
                    Err(error) => Err(error),
                };
                let trial = match trial {
                    Ok(Some(value)) => Ok(value),
                    Ok(None) => return,
                    Err(error) => Err(error),
                };
                if let Err(e) = &legacy {
                    // The old key remains in its explicitly untrusted backup so
                    // an offline launch can retry without granting access.
                    log::warn!("legacy licence migration deferred: {e}");
                }
                if let Err(e) = &cached {
                    log::warn!("licence receipt refresh deferred: {e}");
                }
                if let Err(e) = &trial {
                    log::warn!("trial receipt provisioning deferred: {e}");
                }
                status
                    .trial_setup_error
                    .store(trial.is_err(), Ordering::Release);
                // A temporary network/server/signature failure must not lock a
                // paid user out until the next twelve-hour pass. Successful
                // refreshes and explicit no-op results take the long cadence;
                // errors retry quickly with a capped backoff and reset after
                // the first healthy attempt.
                next_network = refresh_backoff.next_deadline(
                    std::time::Instant::now(),
                    legacy.is_ok() && cached.is_ok() && trial.is_ok(),
                );
            }

            // Local signature/expiry/trial validation is cheap and deliberately
            // separate from the network cadence. A receipt expiring just after
            // a refresh can therefore leave injection enabled for at most one
            // minute, never the former twelve hours.
            let license = license_status();
            status
                .inject_gate
                .store(injection_allowed(&license), Ordering::Release);
            status
                .pro_gate
                .store(pro_allowed(&license), Ordering::Release);
            *status.license.lock().unwrap() = license_string(&license);
            *status.license_state.lock().unwrap() = license_state(&license);

            let until_network = next_network.saturating_duration_since(std::time::Instant::now());
            let deadline =
                std::time::Instant::now() + LOCAL_LICENSE_GATE_INTERVAL.min(until_network);
            while std::time::Instant::now() < deadline {
                if status.shutdown.load(Ordering::Acquire) {
                    return;
                }
                thread::sleep(
                    deadline
                        .saturating_duration_since(std::time::Instant::now())
                        .min(std::time::Duration::from_millis(100)),
                );
            }
        }
    })
}

/// Keeps the engine/control loop alive when a device or model cannot be opened.
/// Settings can then replace the failed component without requiring a process
/// restart; the hotkey readiness gate remains false until that succeeds.
struct UnavailableAudio(String);

impl AudioCapture for UnavailableAudio {
    fn start(&mut self) -> vocalcode_core::error::Result<()> {
        Err(VocalCodeError::Audio(self.0.clone()))
    }
    fn stop(&mut self) -> vocalcode_core::error::Result<Recording> {
        Err(VocalCodeError::Audio(self.0.clone()))
    }
    fn is_recording(&self) -> bool {
        false
    }
}

struct UnavailableAsr(String);

impl Asr for UnavailableAsr {
    fn transcribe(
        &mut self,
        _samples: &[f32],
        _sample_rate: u32,
    ) -> vocalcode_core::error::Result<String> {
        Err(VocalCodeError::Asr(self.0.clone()))
    }
    fn model_label(&self) -> &str {
        &self.0
    }
}

fn publish_engine_outcome(
    status: &Arc<RuntimeStatus>,
    overlay: &overlay::OverlayState,
    corrections: &CorrectionMonitor,
    correction_window_ms: u32,
    outcome: Outcome,
    recording: bool,
) -> bool {
    match &outcome {
        Outcome::Transcribed(text) if !text.is_empty() => {
            // Core only returns a transcript after the live delivery gate was
            // checked immediately before insertion. Injection failures take
            // the separate recoverable-text path below.
            push_history(status, text);
            *status.last_text.lock().unwrap() = text.clone();
            if status.pro_gate.load(Ordering::Acquire) {
                corrections.arm(text, Duration::from_millis(correction_window_ms as u64));
            } else {
                corrections.cancel();
            }
        }
        Outcome::LicenseRequired => {
            *status.last_text.lock().unwrap() =
                "Text delivery is temporarily unavailable.".to_string();
        }
        _ => {}
    }
    status.listening.store(recording, Ordering::Release);
    overlay.set(if recording {
        overlay::Phase::Recording
    } else {
        overlay::Phase::Idle
    });
    matches!(outcome, Outcome::Quit)
}

fn publish_correction_events(
    status: &Arc<RuntimeStatus>,
    overlay: &overlay::OverlayState,
    corrections: &CorrectionMonitor,
    recording: bool,
    diagnostic_consent: bool,
) {
    let current = corrections.generation();
    while let Some(event) = corrections.try_recv() {
        let session = match &event {
            CorrectionEvent::Started { session }
            | CorrectionEvent::Learned { session, .. }
            | CorrectionEvent::Stopped { session } => *session,
        };
        if session != current {
            continue;
        }
        match event {
            CorrectionEvent::Started { .. } => {
                if !recording {
                    overlay.set(overlay::Phase::Learning);
                }
            }
            CorrectionEvent::Learned { pairs, .. } => {
                let multiple = pairs.len() > 1;
                match learn_correction_pairs(&app_dir(), &pairs) {
                    Ok(learned) => {
                        if diagnostic_consent {
                            diagnostics::queue_event(
                                status,
                                if learned.review_message.is_some() {
                                    "correction_proposed"
                                } else {
                                    "learned_correction"
                                },
                                &pairs,
                            );
                        }
                        *status.rules.lock().unwrap() = merge_rules(&learned.document.rules);
                        *status.correction_result.lock().unwrap() = Some(webui::CorrectionResult {
                            ok: true,
                            review_only: learned.review_message.is_some(),
                            message: if let Some(message) = learned.review_message {
                                message
                            } else if multiple {
                                "Corrections added to Dictionary."
                            } else {
                                "Correction added to Dictionary."
                            }
                            .to_string(),
                            document: Some(learned.document),
                            changes: learned.changes,
                        });
                    }
                    Err(error) => {
                        log::error!("automatic correction: {error}");
                        *status.correction_result.lock().unwrap() = Some(webui::CorrectionResult {
                            ok: false,
                            review_only: false,
                            message: format!("Could not add that correction: {error}"),
                            document: None,
                            changes: Vec::new(),
                        });
                    }
                }
                if !recording {
                    overlay.set(overlay::Phase::Idle);
                }
            }
            CorrectionEvent::Stopped { .. } => {
                if !recording {
                    overlay.set(overlay::Phase::Idle);
                }
            }
        }
    }
}

/// Preserve speech that ASR completed but the injector could not deliver.
/// The accompanying runtime error still explains why insertion failed; this
/// copy is the user's recovery path and must be published exactly once.
fn publish_recoverable_text(status: &Arc<RuntimeStatus>, engine: &mut Engine) {
    let Some(text) = engine.take_recoverable_text() else {
        return;
    };
    if text.is_empty() {
        return;
    }
    push_history(status, &text);
    *status.last_text.lock().unwrap() = text;
    status
        .dictation_control
        .notify_recovery(std::time::Instant::now());
}

/// Audio errors invalidate the concrete CPAL device object.  Returning an
/// error without clearing this bit prevents the reopen loop from ever running.
fn invalidate_audio_on_error(error: &VocalCodeError, audio_ready: &mut bool) -> bool {
    if matches!(error, VocalCodeError::Audio(_)) {
        *audio_ready = false;
        true
    } else {
        false
    }
}

fn accept_model_config_after_prepare_failure(had_working_model: bool) -> bool {
    // With a usable decoder, failure means a hot-switch request was rejected
    // and the whole snapshot rolls back. First run has no usable decoder to
    // return to, so the explicit language choice remains accepted and recovery
    // retries that chosen model.
    !had_working_model
}

fn publish_config_result(
    status: &RuntimeStatus,
    request_id: u64,
    ok: bool,
    message: impl Into<String>,
    authoritative: &Config,
) {
    let message = message.into();
    if request_id == 0 {
        *status.settings_result.lock().unwrap() = Some((ok, message));
    } else {
        webui::queue_config_result(status, request_id, ok, true, message, authoritative);
    }
}

fn config_snapshots_match(left: &Config, right: &Config) -> Result<bool, String> {
    let left = toml::to_string(left).map_err(|error| error.to_string())?;
    let right = toml::to_string(right).map_err(|error| error.to_string())?;
    Ok(left == right)
}

/// Restore the durable/shared settings to what the engine actually accepted.
///
/// Keep the config-apply coordinator for the complete compare/write
/// transaction. The UI's FIFO save worker holds the same guard from before
/// persistence through generation publication, so no newer save can hide in
/// the old gap between a generation check and rollback.
#[derive(Debug, PartialEq, Eq)]
enum ConfigRollback {
    Skipped,
    Applied { disk_error: Option<String> },
}

fn rollback_rejected_config_at_with<F>(
    path: &Path,
    status: &RuntimeStatus,
    shared_config: &std::sync::Mutex<Config>,
    generation: u64,
    rejected: &Config,
    active: &Config,
    on_applied: F,
) -> Result<ConfigRollback, String>
where
    F: FnOnce(&Option<String>),
{
    let apply = status
        .config_apply
        .lock()
        .map_err(|_| "config apply lock was poisoned".to_string())?;
    if !apply.is_current(generation) {
        return Ok(ConfigRollback::Skipped);
    }

    let mut shared = shared_config
        .lock()
        .map_err(|_| "shared settings lock was poisoned".to_string())?;
    if !config_snapshots_match(&shared, rejected)? {
        return Ok(ConfigRollback::Skipped);
    }

    let disk_error = persist_config_if_current(path, rejected, active).err();
    // Even if the disk has become read-only, future UI edits must start from
    // the runtime truth rather than silently reintroducing the rejected value.
    *shared = active.clone();
    drop(shared);
    // Keep `apply` alive while the caller restores every runtime/OS side
    // effect. A newer save must be ordered wholly after this callback; letting
    // it publish in between disk rollback and (for example) autostart restore
    // would leave the new file paired with the old registry state.
    on_applied(&disk_error);
    drop(apply);
    Ok(ConfigRollback::Applied { disk_error })
}

#[cfg(test)]
fn rollback_rejected_config_at(
    path: &Path,
    status: &RuntimeStatus,
    shared_config: &std::sync::Mutex<Config>,
    generation: u64,
    rejected: &Config,
    active: &Config,
) -> Result<ConfigRollback, String> {
    rollback_rejected_config_at_with(
        path,
        status,
        shared_config,
        generation,
        rejected,
        active,
        |_| {},
    )
}

struct ConfigRejection<'a> {
    generation: u64,
    request_id: u64,
    rejected: &'a Config,
    active: &'a Config,
    message: String,
}

fn reject_config(
    status: &RuntimeStatus,
    shared_config: &std::sync::Mutex<Config>,
    triggers: &SharedTriggers,
    rejection: ConfigRejection<'_>,
) -> bool {
    let ConfigRejection {
        generation,
        request_id,
        rejected,
        active,
        message,
    } = rejection;
    let mut restored_message = None;
    let mut authoritative = active.clone();
    let outcome = rollback_rejected_config_at_with(
        &app_dir().join("vocalcode.toml"),
        status,
        shared_config,
        generation,
        rejected,
        active,
        |disk_error| {
            *triggers.lock().unwrap() = (
                active.talk.clone(),
                active.send.clone(),
                active.teach.clone(),
            );
            status
                .talk_latched
                .store(active.talk_mode == "toggle", Ordering::Release);
            status
                .cue_sounds
                .store(active.cue_sounds, Ordering::Release);
            status.onboarded.store(active.onboarded, Ordering::Release);
            let mut completed_message = message.clone();
            if let Some(error) = disk_error {
                completed_message.push_str(&format!(
                    " VocalCode could not restore the accepted settings on disk: {error}"
                ));
            }
            if active.autostart != rejected.autostart {
                if let Err(error) = webui::set_autostart(active.autostart) {
                    // A registry/launchd operation may fail after changing
                    // state. Keep the shared snapshot and result aligned with
                    // the OS source of truth so the page never claims a state
                    // that is observably false. The next successful save
                    // refreshes the non-authoritative TOML cache as well.
                    let actual = webui::autostart_enabled();
                    authoritative.autostart = actual;
                    shared_config
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .autostart = actual;
                    completed_message.push_str(&format!(
                        " VocalCode also could not restore the accepted startup setting: {error}. The control was reconciled to the operating-system state ({}).",
                        if actual { "enabled" } else { "disabled" }
                    ));
                }
            }
            restored_message = Some(completed_message);
        },
    );
    let message = match outcome {
        Ok(ConfigRollback::Applied { .. }) => restored_message
            .expect("applied rollback must run its protected side effects"),
        Ok(ConfigRollback::Skipped) => return false,
        Err(rollback_error) => format!(
            "{message} VocalCode also could not restore the accepted settings on disk: {rollback_error}"
        ),
    };
    publish_config_result(status, request_id, false, message, &authoritative);
    true
}

const RUNTIME_MAINTENANCE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(450);
// Cheap nonblocking result polls should not inherit the slower UI/maintenance
// cadence. This also catches phrase boundaries after speech has resumed.
const DICTATION_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(80);

struct MaintenanceClock {
    next: std::time::Instant,
}

impl MaintenanceClock {
    fn new(now: std::time::Instant) -> Self {
        Self {
            next: now + RUNTIME_MAINTENANCE_INTERVAL,
        }
    }

    fn wait(&self, now: std::time::Instant) -> std::time::Duration {
        self.next.saturating_duration_since(now)
    }

    fn take_due(&mut self, now: std::time::Instant) -> bool {
        if now < self.next {
            return false;
        }
        // Schedule from `now`, not the stale deadline: after a long decode we
        // need one health pass, not hundreds of catch-up iterations.
        self.next = now + RUNTIME_MAINTENANCE_INTERVAL;
        true
    }
}

/// Remove input that arrived while a synchronous finish/decode was in flight.
///
/// The global hook is gated before decoding starts, so actions observed after
/// that point normally pass through to the foreground application. There can
/// still be actions already queued by another input thread, though. Replaying a
/// queued toggle tap after ASR returns would start a recording after the user
/// had already released the button, while send/teach would execute seconds
/// late. Drop those action edges, but retain terminal and cleanup events: a
/// release removes any pre-gate held edge, device loss and cancellation are
/// harmless idempotent cleanup, and Quit must never be swallowed.
fn drain_busy_events(
    receiver: &TriggerEventReceiver,
    mut cleanup: impl FnMut(TriggerEvent),
) -> bool {
    let mut quit_requested = false;
    while let Ok(event) = receiver.try_recv() {
        match event {
            TriggerEvent::TalkPressed(_)
            | TriggerEvent::HandsFreeStart(_)
            | TriggerEvent::Wake
            | TriggerEvent::SendTapped(_)
            | TriggerEvent::TeachTapped(_) => {
                log::debug!("discarding action queued while runtime was busy: {event:?}");
            }
            TriggerEvent::Quit => quit_requested = true,
            event => cleanup(event),
        }
    }
    quit_requested
}

fn report_runtime_error(status: &RuntimeStatus, message: impl Into<String>) {
    *status.runtime_error.lock().unwrap() = Some(message.into());
}

// ---------------------------------------------------------------------------
// Single instance + "show the existing window" hand-off
// ---------------------------------------------------------------------------

const INSTANCE_SHOW: &[u8] = b"VOCALCODE_SHOW_V1";
#[cfg(windows)]
const INSTANCE_DELIVERED: &[u8] = b"VOCALCODE_DELIVERED_V1";
const INSTANCE_SCOPE: &str = "vocalcode-gui-v2";

/// The endpoint name is deterministic for one authenticated OS identity, but
/// never uses mutable environment variables. 128 bits is ample collision
/// resistance while keeping macOS Unix-domain socket paths below its small
/// platform path limit for ordinary Application Support locations.
fn instance_token(scope: &str, identity: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"VocalCode instance\0");
    digest.update(scope.as_bytes());
    digest.update([0]);
    digest.update(identity);
    let mut token = format!("{:x}", digest.finalize());
    token.truncate(32);
    token
}

struct InstancePrimary {
    show: mpsc::Receiver<()>,
    guard: instance_platform::Guard,
}

/// Keep at most one pending request to surface the existing window. A launch
/// storm should not allocate one queue node per process, and a duplicate is
/// already satisfied by the pending notification.
fn enqueue_show(show: &mpsc::SyncSender<()>) -> bool {
    !matches!(show.try_send(()), Err(mpsc::TrySendError::Disconnected(())))
}

enum InstanceState {
    Primary(InstancePrimary),
    Existing,
}

fn acquire_instance(app_data: &Path, scope: &str) -> std::io::Result<InstanceState> {
    instance_platform::acquire(app_data, scope)
}

#[cfg(windows)]
const WINDOWS_INSTALLER_OBSERVATION_MUTEX: &str = community::INSTALLER_MUTEX;

#[cfg(windows)]
mod instance_platform {
    use super::{
        instance_token, InstancePrimary, InstanceState, INSTANCE_DELIVERED, INSTANCE_SHOW,
    };
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, SetLastError, ERROR_ALREADY_EXISTS,
        ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_NO_DATA, ERROR_PIPE_BUSY,
        ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING, GENERIC_READ, GENERIC_WRITE, HANDLE,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetLengthSid, GetTokenInformation, IsValidSid, TokenUser, PSECURITY_DESCRIPTOR,
        SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING,
        PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, SetNamedPipeHandleState,
        WaitNamedPipeW, PIPE_NOWAIT, PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_MESSAGE,
    };
    use windows_sys::Win32::System::Threading::{
        CreateMutexW, GetCurrentProcess, OpenProcessToken, ReleaseMutex,
    };

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    struct OwnedMutex {
        handle: OwnedHandle,
        owned: bool,
    }

    impl Drop for OwnedMutex {
        fn drop(&mut self) {
            if self.owned {
                unsafe {
                    ReleaseMutex(self.handle.0);
                }
            }
        }
    }

    struct PrivateSecurityDescriptor(PSECURITY_DESCRIPTOR);

    impl PrivateSecurityDescriptor {
        fn for_sid(sid: &str) -> io::Result<Self> {
            let sddl = wide(&private_sddl(sid));
            let mut descriptor = null_mut();
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(descriptor))
        }

        fn attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.0,
                bInheritHandle: 0,
            }
        }
    }

    impl Drop for PrivateSecurityDescriptor {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }

    fn private_sddl(sid: &str) -> String {
        format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})")
    }

    fn sid_string(bytes: &[u8]) -> io::Result<String> {
        if bytes.len() < 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SID is truncated",
            ));
        }
        let sub_authorities = usize::from(bytes[1]);
        let required =
            8usize
                .checked_add(sub_authorities.checked_mul(4).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "SID length overflow")
                })?)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "SID length overflow"))?;
        if bytes.len() != required {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SID has an invalid length",
            ));
        }
        let authority = bytes[2..8]
            .iter()
            .fold(0u64, |value, byte| (value << 8) | u64::from(*byte));
        let mut result = format!("S-{}-{authority}", bytes[0]);
        for chunk in bytes[8..].chunks_exact(4) {
            let value = u32::from_le_bytes(chunk.try_into().expect("four-byte SID component"));
            result.push_str(&format!("-{value}"));
        }
        Ok(result)
    }

    pub(super) fn current_identity() -> io::Result<Vec<u8>> {
        let mut token = null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle(token);
        let mut required = 0u32;
        unsafe {
            GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut required);
        }
        if required == 0 {
            return Err(io::Error::last_os_error());
        }

        // TOKEN_USER contains pointers and therefore requires pointer-aligned
        // backing storage; Vec<u8> would not provide that language guarantee.
        let words = (required as usize).div_ceil(std::mem::size_of::<usize>());
        let mut storage = vec![0usize; words];
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                storage.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
        if user.User.Sid.is_null() || unsafe { IsValidSid(user.User.Sid) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows returned an invalid token user SID",
            ));
        }
        let length = unsafe { GetLengthSid(user.User.Sid) } as usize;
        let storage_start = storage.as_ptr() as usize;
        let storage_end = storage_start
            .checked_add(required as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "token buffer overflow"))?;
        let sid_start = user.User.Sid as usize;
        let sid_end = sid_start
            .checked_add(length)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "SID pointer overflow"))?;
        if length < 8 || sid_start < storage_start || sid_end > storage_end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows returned an invalid token user SID length",
            ));
        }
        Ok(unsafe { std::slice::from_raw_parts(user.User.Sid.cast::<u8>(), length) }.to_vec())
    }

    #[derive(Clone, Debug)]
    struct InstanceNames {
        mutex: String,
        pipe: String,
    }

    fn names(scope: &str, identity: &[u8]) -> InstanceNames {
        let token = instance_token(scope, identity);
        InstanceNames {
            mutex: format!(r"Local\VocalCode.Gui.{token}"),
            pipe: format!(r"\\.\pipe\VocalCode.Gui.{token}"),
        }
    }

    fn create_mutex(name: &str, sid: &str, initial_owner: bool) -> io::Result<(OwnedHandle, bool)> {
        let security = PrivateSecurityDescriptor::for_sid(sid)?;
        let attributes = security.attributes();
        let name = wide(name);
        unsafe {
            SetLastError(0);
        }
        let handle = unsafe { CreateMutexW(&attributes, initial_owner.into(), name.as_ptr()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let already_existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        Ok((OwnedHandle(handle), already_existed))
    }

    fn notify_pipe(pipe_name: &str) -> io::Result<()> {
        let pipe_name = wide(pipe_name);
        let mut last_error = io::Error::new(io::ErrorKind::NotFound, "instance pipe not ready");
        for _ in 0..10 {
            let handle = unsafe {
                CreateFileW(
                    pipe_name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    null(),
                    OPEN_EXISTING,
                    0,
                    null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                let handle = OwnedHandle(handle);
                let mut written = 0u32;
                if unsafe {
                    WriteFile(
                        handle.0,
                        INSTANCE_SHOW.as_ptr(),
                        INSTANCE_SHOW.len() as u32,
                        &mut written,
                        null_mut(),
                    )
                } == 0
                {
                    return Err(io::Error::last_os_error());
                }
                if written as usize != INSTANCE_SHOW.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "instance notification was only partially written",
                    ));
                }
                // This acknowledgement confirms only that the protected show
                // channel consumed the request. The SID-scoped kernel mutex,
                // never an IPC response, remains the ownership authority.
                let mode = PIPE_READMODE_MESSAGE | PIPE_NOWAIT;
                let deadline = Instant::now() + Duration::from_millis(750);
                loop {
                    if unsafe { SetNamedPipeHandleState(handle.0, &mode, null(), null()) } != 0 {
                        break;
                    }
                    let error = io::Error::last_os_error();
                    if error.raw_os_error().map(|value| value as u32) == Some(ERROR_PIPE_BUSY)
                        && Instant::now() < deadline
                    {
                        // CreateFile may return just before the server's
                        // ConnectNamedPipe poll observes us. Retain the client
                        // handle so the request cannot disappear in that gap.
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    return Err(error);
                }
                let mut acknowledgement = [0u8; 32];
                while Instant::now() < deadline {
                    let mut read = 0u32;
                    if unsafe {
                        ReadFile(
                            handle.0,
                            acknowledgement.as_mut_ptr(),
                            acknowledgement.len() as u32,
                            &mut read,
                            null_mut(),
                        )
                    } != 0
                    {
                        return if &acknowledgement[..read as usize] == INSTANCE_DELIVERED {
                            Ok(())
                        } else {
                            Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "invalid instance delivery acknowledgement",
                            ))
                        };
                    }
                    match unsafe { GetLastError() } {
                        ERROR_NO_DATA => thread::sleep(Duration::from_millis(10)),
                        _ => return Err(io::Error::last_os_error()),
                    }
                }
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "instance notification delivery timed out",
                ));
            }
            last_error = io::Error::last_os_error();
            let code = last_error.raw_os_error().map(|value| value as u32);
            if !matches!(code, Some(ERROR_PIPE_BUSY) | Some(ERROR_FILE_NOT_FOUND)) {
                return Err(last_error);
            }
            unsafe {
                WaitNamedPipeW(pipe_name.as_ptr(), 25);
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err(last_error)
    }

    fn pipe_listener(
        pipe_name: String,
        sid: String,
        show: mpsc::SyncSender<()>,
        stop: Arc<AtomicBool>,
        ready: mpsc::SyncSender<Result<(), String>>,
    ) {
        let security = match PrivateSecurityDescriptor::for_sid(&sid) {
            Ok(security) => security,
            Err(error) => {
                let _ = ready.send(Err(error.to_string()));
                return;
            }
        };
        let attributes = security.attributes();
        let pipe_name = wide(&pipe_name);
        let mut ready = Some(ready);
        'listen: while !stop.load(Ordering::Acquire) {
            let pipe = unsafe {
                CreateNamedPipeW(
                    pipe_name.as_ptr(),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                    PIPE_TYPE_MESSAGE
                        | PIPE_READMODE_MESSAGE
                        | PIPE_NOWAIT
                        | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    32,
                    64,
                    100,
                    &attributes,
                )
            };
            if pipe == INVALID_HANDLE_VALUE {
                let error = io::Error::last_os_error();
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Err(error.to_string()));
                }
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            let pipe = OwnedHandle(pipe);
            if let Some(ready) = ready.take() {
                let _ = ready.send(Ok(()));
            }

            loop {
                if stop.load(Ordering::Acquire) {
                    break 'listen;
                }
                if unsafe { ConnectNamedPipe(pipe.0, null_mut()) } != 0 {
                    break;
                }
                match unsafe { GetLastError() } {
                    ERROR_PIPE_CONNECTED => break,
                    ERROR_PIPE_LISTENING => thread::sleep(Duration::from_millis(15)),
                    ERROR_NO_DATA => continue 'listen,
                    _ => continue 'listen,
                }
            }

            let mut message = [0u8; 64];
            let idle_deadline = Instant::now() + Duration::from_millis(500);
            loop {
                if stop.load(Ordering::Acquire) {
                    break 'listen;
                }
                let mut read = 0u32;
                if unsafe {
                    ReadFile(
                        pipe.0,
                        message.as_mut_ptr(),
                        message.len() as u32,
                        &mut read,
                        null_mut(),
                    )
                } != 0
                {
                    if &message[..read as usize] == INSTANCE_SHOW && crate::enqueue_show(&show) {
                        let mut written = 0u32;
                        if unsafe {
                            WriteFile(
                                pipe.0,
                                INSTANCE_DELIVERED.as_ptr(),
                                INSTANCE_DELIVERED.len() as u32,
                                &mut written,
                                null_mut(),
                            )
                        } != 0
                            && written as usize == INSTANCE_DELIVERED.len()
                        {
                            // Keep the instance connected long enough for the
                            // client to switch its just-opened handle to
                            // nonblocking message-read mode and consume the
                            // acknowledgement.
                            thread::sleep(Duration::from_millis(50));
                        }
                    }
                    break;
                }
                let read_error = unsafe { GetLastError() };
                match read_error {
                    // PIPE_NOWAIT keeps shutdown and a same-user client that
                    // connects without writing from wedging this thread. Give
                    // a legitimate client ample time to write after connect.
                    ERROR_NO_DATA if Instant::now() < idle_deadline => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    ERROR_NO_DATA | ERROR_BROKEN_PIPE => break,
                    _ => break,
                }
            }
            unsafe {
                DisconnectNamedPipe(pipe.0);
            }
        }
    }

    pub(super) struct Guard {
        stop: Arc<AtomicBool>,
        listener: Option<thread::JoinHandle<()>>,
        _mutex: OwnedMutex,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(listener) = self.listener.take() {
                let _ = listener.join();
            }
        }
    }

    pub(super) struct InstallerObservationGuard(#[allow(dead_code)] OwnedHandle);

    pub(super) fn acquire_installer_observation(
        name: &str,
    ) -> io::Result<InstallerObservationGuard> {
        let identity = current_identity()?;
        let sid = sid_string(&identity)?;
        let (handle, _) = create_mutex(name, &sid, false)?;
        Ok(InstallerObservationGuard(handle))
    }

    pub(super) fn acquire(_app_data: &Path, scope: &str) -> io::Result<InstanceState> {
        let identity = current_identity()?;
        let sid = sid_string(&identity)?;
        let names = names(scope, &identity);
        let (mutex, already_existed) = create_mutex(&names.mutex, &sid, true)?;
        if already_existed {
            drop(mutex);
            // The kernel mutex, not a response from IPC, is the authority. A
            // missing/busy/spoofed show channel must never create two primaries.
            if let Err(error) = notify_pipe(&names.pipe) {
                eprintln!("could not notify the existing VocalCode window: {error}");
            }
            return Ok(InstanceState::Existing);
        }

        let mutex = OwnedMutex {
            handle: mutex,
            owned: true,
        };
        let (show_tx, show_rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let listener_stop = stop.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let pipe_name = names.pipe;
        let listener =
            thread::spawn(move || pipe_listener(pipe_name, sid, show_tx, listener_stop, ready_tx));
        // Avoid losing a launch that arrives immediately after this one. An
        // occupied endpoint is non-authoritative, though, so a failed listener
        // never suppresses the legitimate mutex owner.
        if let Ok(Err(error)) = ready_rx.recv_timeout(Duration::from_secs(1)) {
            eprintln!("protected single-instance notification channel unavailable: {error}");
        }
        Ok(InstanceState::Primary(InstancePrimary {
            show: show_rx,
            guard: Guard {
                stop,
                listener: Some(listener),
                _mutex: mutex,
            },
        }))
    }

    #[cfg(test)]
    pub(super) fn private_sddl_for_test(sid: &str) -> String {
        private_sddl(sid)
    }

    #[cfg(test)]
    pub(super) struct TestPipeOccupant(#[allow(dead_code)] OwnedHandle);

    #[cfg(test)]
    pub(super) fn occupy_pipe_for_test(scope: &str) -> io::Result<TestPipeOccupant> {
        let identity = current_identity()?;
        let pipe_name = wide(&names(scope, &identity).pipe);
        let pipe = unsafe {
            CreateNamedPipeW(
                pipe_name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_NOWAIT,
                1,
                0,
                64,
                100,
                null(),
            )
        };
        if pipe == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(TestPipeOccupant(OwnedHandle(pipe)))
    }
}

#[cfg(unix)]
mod instance_platform {
    use super::INSTANCE_SHOW;
    use super::{instance_token, open_private_regular_nofollow, InstancePrimary, InstanceState};
    use std::io;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::Duration;

    extern "C" {
        fn getuid() -> u32;
    }

    pub(super) fn current_identity() -> io::Result<Vec<u8>> {
        Ok(unsafe { getuid() }.to_le_bytes().to_vec())
    }

    fn endpoint_paths(app_data: &Path, scope: &str, identity: &[u8]) -> (PathBuf, PathBuf) {
        let token = instance_token(scope, identity);
        // sockaddr_un is only 104 bytes on macOS. The lock retains the full
        // 128-bit name; the socket uses a 64-bit suffix inside the already
        // private, application-specific directory so ordinary longer account
        // names still fit without weakening ownership authority.
        let socket_token = &token[..16];
        (
            app_data.join(format!(".instance-{token}.lock")),
            app_data.join(format!(".i-{socket_token}.s")),
        )
    }

    fn remove_stale_socket(path: &Path) -> io::Result<()> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(path),
            Ok(_) => Err(io::Error::other(format!(
                "refusing to replace non-socket instance endpoint {}",
                path.display()
            ))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn notify_socket(path: &Path) -> io::Result<()> {
        let mut last_error = io::Error::new(io::ErrorKind::NotFound, "instance socket not ready");
        for _ in 0..10 {
            match UnixStream::connect(path) {
                Ok(mut stream) => {
                    std::io::Write::write_all(&mut stream, INSTANCE_SHOW)?;
                    return Ok(());
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound
                            | io::ErrorKind::ConnectionRefused
                            | io::ErrorKind::WouldBlock
                    ) =>
                {
                    last_error = error;
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error)
    }

    pub(super) struct Guard {
        stop: Arc<AtomicBool>,
        listener: Option<thread::JoinHandle<()>>,
        socket: PathBuf,
        lock: std::fs::File,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(listener) = self.listener.take() {
                let _ = listener.join();
            }
            // Only the process holding the exclusive lock may unlink a stale
            // or live endpoint. Keep the lock held through this removal.
            if std::fs::symlink_metadata(&self.socket)
                .is_ok_and(|metadata| metadata.file_type().is_socket())
            {
                let _ = std::fs::remove_file(&self.socket);
            }
            let _ = fs2::FileExt::unlock(&self.lock);
        }
    }

    pub(super) fn acquire(app_data: &Path, scope: &str) -> io::Result<InstanceState> {
        std::fs::create_dir_all(app_data)?;
        let metadata = std::fs::symlink_metadata(app_data)?;
        let uid = unsafe { getuid() };
        if metadata.file_type().is_symlink() || !metadata.is_dir() || metadata.uid() != uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "untrusted VocalCode app-data directory {}",
                    app_data.display()
                ),
            ));
        }
        if metadata.permissions().mode() & 0o022 != 0 {
            std::fs::set_permissions(app_data, std::fs::Permissions::from_mode(0o700))?;
        }

        let identity = current_identity()?;
        let (lock_path, socket_path) = endpoint_paths(app_data, scope, &identity);
        let lock = open_private_regular_nofollow(&lock_path, |options| {
            options.read(true).write(true).create(true).truncate(false);
        })?;
        match fs2::FileExt::try_lock_exclusive(&lock) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let _ = notify_socket(&socket_path);
                return Ok(InstanceState::Existing);
            }
            Err(error) => return Err(error),
        }

        // A crashed owner may leave a socket inode behind. Ownership of the
        // lock is the sole authority to inspect and remove it.
        remove_stale_socket(&socket_path)?;
        let listener = UnixListener::bind(&socket_path)?;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;

        let (show_tx, show_rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let listener_stop = stop.clone();
        let listener_thread = thread::spawn(move || {
            while !listener_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
                        let mut message = [0u8; 64];
                        if let Ok(read) = std::io::Read::read(&mut stream, &mut message) {
                            if &message[..read] == INSTANCE_SHOW {
                                let _ = crate::enqueue_show(&show_tx);
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(15));
                    }
                    Err(error) => {
                        eprintln!("single-instance Unix socket listener stopped: {error}");
                        break;
                    }
                }
            }
        });
        Ok(InstanceState::Primary(InstancePrimary {
            show: show_rx,
            guard: Guard {
                stop,
                listener: Some(listener_thread),
                socket: socket_path,
                lock,
            },
        }))
    }

    #[cfg(test)]
    pub(super) fn socket_path_for_test(app_data: &Path, scope: &str) -> io::Result<PathBuf> {
        Ok(endpoint_paths(app_data, scope, &current_identity()?).1)
    }
}

fn show_startup_error(message: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
        let text = std::ffi::OsStr::new(message)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let title = std::ffi::OsStr::new("VocalCode could not start")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
    }
    #[cfg(not(windows))]
    eprintln!("VocalCode could not start: {message}");
}

struct BackgroundRuntime {
    events: TriggerEventSender,
    input_shutdown: Arc<std::sync::atomic::AtomicBool>,
    hotkey: thread::JoinHandle<()>,
    engine: thread::JoinHandle<()>,
    engine_done: std::sync::mpsc::Receiver<()>,
}

fn engine_ready(
    audio_ready: bool,
    model_ready: bool,
    input_ready: &std::sync::atomic::AtomicBool,
    status: &RuntimeStatus,
) -> bool {
    audio_ready
        && model_ready
        && input_ready.load(Ordering::Acquire)
        && !status.shutdown.load(Ordering::Acquire)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "the engine shutdown state determines whether in-process cleanup is safe"]
enum EngineShutdown {
    Stopped,
    Detached,
}

impl BackgroundRuntime {
    fn shutdown(self, status: &RuntimeStatus) -> EngineShutdown {
        self.shutdown_with_timeout(status, Duration::from_secs(10))
    }

    fn shutdown_with_timeout(
        self,
        status: &RuntimeStatus,
        engine_timeout: Duration,
    ) -> EngineShutdown {
        status.ready.store(false, Ordering::Release);
        status.shutdown.store(true, Ordering::Release);
        status.inject_gate.store(false, Ordering::Release);
        status.pro_gate.store(false, Ordering::Release);
        self.input_shutdown.store(true, Ordering::Release);
        status
            .config_apply
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .cancel_prepare();
        let _ = self.events.try_send(TriggerEvent::Quit);
        if self.hotkey.join().is_err() {
            log::error!("hotkey supervisor panicked during shutdown");
        }
        let engine_shutdown = match self.engine_done.recv_timeout(engine_timeout) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if self.engine.join().is_err() {
                    log::error!("engine thread panicked during shutdown");
                }
                EngineShutdown::Stopped
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // A native ASR/driver call cannot be safely cancelled in-process.
                // Detach after a bounded grace period so returning from main lets
                // the OS terminate the process instead of hanging Quit forever.
                log::error!(
                    "engine did not stop within {engine_timeout:?}; detaching the stuck native worker"
                );
                drop(self.engine);
                EngineShutdown::Detached
            }
        };
        // Shutdown is latched, so the engine cannot accept new Teach work even
        // if a stuck native call had to be detached above.
        let workers = status
            .teach_workers
            .lock()
            .map(|mut workers| std::mem::take(&mut *workers))
            .unwrap_or_else(|poisoned| {
                let mut workers = poisoned.into_inner();
                std::mem::take(&mut *workers)
            });
        for worker in workers {
            if worker.join().is_err() {
                log::error!("Teach worker panicked during shutdown");
            }
        }
        engine_shutdown
    }
}

fn start_background(
    shared_config: Arc<std::sync::Mutex<Config>>,
    status: Arc<RuntimeStatus>,
    capture: Arc<CaptureShared>,
    triggers: SharedTriggers,
    overlay: overlay::OverlayState,
    level_out: std::sync::mpsc::SyncSender<vocalcode_platform::AudioLevel>,
    meeting_asr: std::sync::mpsc::Receiver<MeetingAsrRequest>,
) -> BackgroundRuntime {
    struct ReadyReset(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for ReadyReset {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    struct TeachReset(Arc<RuntimeStatus>);
    impl Drop for TeachReset {
        fn drop(&mut self) {
            self.0.teach_in_progress.store(false, Ordering::Release);
        }
    }

    let config = shared_config.lock().unwrap().clone();
    let (tx, rx) = trigger_event_channel();
    status.dictation_control.connect(tx.clone());
    let input_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let input_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let hotkey_status = status.clone();
    let hotkey_ready = input_ready.clone();
    let hotkey_triggers = triggers.clone();
    let hotkey_capture = capture.clone();
    let hotkey_tx = tx.clone();
    let hotkey_shutdown = input_shutdown.clone();
    let hotkey_thread = thread::spawn(move || {
        let mut retry_delay = std::time::Duration::from_millis(250);
        loop {
            hotkey_ready.store(false, Ordering::Release);
            hotkey_status.ready.store(false, Ordering::Release);
            let listener = Box::new(PlatformHotkey::new_with_readiness_and_installation(
                hotkey_triggers.clone(),
                hotkey_capture.clone(),
                hotkey_status.ready.clone(),
                hotkey_ready.clone(),
            ));
            let started = std::time::Instant::now();
            let failure = match listener.run(hotkey_tx.clone(), hotkey_shutdown.clone()) {
                Ok(()) => "input listener exited unexpectedly".to_string(),
                Err(error) => error.to_string(),
            };
            hotkey_ready.store(false, Ordering::Release);
            hotkey_status.ready.store(false, Ordering::Release);
            if hotkey_shutdown.load(Ordering::Acquire) {
                break;
            }
            log::error!("hotkey listener stopped: {failure}");
            report_runtime_error(
                &hotkey_status,
                format!(
                    "Input controls stopped working; VocalCode will retry automatically: {failure}"
                ),
            );
            // Release any recording whose physical key-up can no longer reach
            // us. The engine owns the audio backend, so signal it rather than
            // trying to stop capture from the listener thread.
            if hotkey_tx.try_send(TriggerEvent::ForceStop).is_err() {
                break;
            }
            // A listener that stayed healthy for a while gets a fresh short
            // retry; repeated installation failures back off to avoid a tight
            // loop while still healing without a process restart.
            if started.elapsed() >= std::time::Duration::from_secs(30) {
                retry_delay = std::time::Duration::from_millis(250);
            }
            let retry_deadline = std::time::Instant::now() + retry_delay;
            while !hotkey_shutdown.load(Ordering::Acquire) {
                let remaining = retry_deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                thread::sleep(remaining.min(std::time::Duration::from_millis(100)));
            }
            if hotkey_shutdown.load(Ordering::Acquire) {
                break;
            }
            retry_delay = (retry_delay * 2).min(std::time::Duration::from_secs(30));
        }
    });

    let (engine_done_tx, engine_done_rx) = std::sync::mpsc::channel();
    let engine_thread = thread::spawn(move || {
        struct EngineDone(Option<std::sync::mpsc::Sender<()>>);
        impl Drop for EngineDone {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }
        let _engine_done = EngineDone(Some(engine_done_tx));
        let _ready_reset = ReadyReset(status.ready.clone());
        let corrections = CorrectionMonitor::new();
        let shared_level = AudioLevel::default();
        let (mut audio, mut audio_ready): (Box<dyn AudioCapture>, bool) =
            match CpalAudioCapture::new_for_with_level(
                config.input_device.as_deref(),
                shared_level.clone(),
            ) {
                Ok(a) => (Box::new(a), true),
                Err(e) => {
                    log::error!("audio init failed: {e}");
                    *status.model_label.lock().unwrap() = format!("Microphone error: {e}");
                    report_runtime_error(&status, format!("Could not open the microphone: {e}"));
                    (Box::new(UnavailableAudio(e.to_string())), false)
                }
            };
        // Always release the UI startup wait. A shared silent meter is useful
        // even when the selected device failed; a later hot-swap writes into
        // the same object and the overlay begins moving without reconstruction.
        let _ = level_out.send(shared_level.clone());

        // Download nothing until a language has actually been chosen. This
        // catches two cases: a fresh install (`onboarded == false`), and an
        // install upgraded from when per-utterance routing existed, whose config
        // still says `language = "auto"` — that used to mean "load both models
        // and switch between them per phrase", and there is nothing to migrate
        // it to that would not be a guess about what the person speaks.
        if !config.onboarded || models::needs_language_pick(&config.model, &config.language) {
            *status.model_label.lock().unwrap() = "Choose a language to begin…".to_string();
            while !status.onboarded.load(Ordering::Relaxed) {
                if status.shutdown.load(Ordering::Acquire) {
                    return;
                }
                thread::sleep(std::time::Duration::from_millis(120));
            }
        }

        // Prepare the newest persisted model generation. The WebView is live
        // during this work; a rapid A -> B change cancels A and this loop takes
        // a fresh atomic desired snapshot instead of ever publishing A as the
        // startup model.
        // Detect once for this engine generation. The selected route turns the
        // profile into a measured model-specific cap on every load/hot switch.
        let hardware = HardwareProfile::detect();
        let (asr, cleaners, label, mut model_ready, mut active_config, startup_model_error) = loop {
            if status.shutdown.load(Ordering::Acquire) {
                return;
            }
            let (generation, want) = desired_config_snapshot(&status, &shared_config);
            if !want.onboarded || models::needs_language_pick(&want.model, &want.language) {
                *status.model_label.lock().unwrap() = "Choose a language to begin…".to_string();
                thread::sleep(std::time::Duration::from_millis(120));
                continue;
            }
            let Some(cancellation) = begin_model_prepare(&status, generation, &want) else {
                continue;
            };
            let threads = models::recommended_threads(&want.model, &want.language, hardware) as i32;
            *status.model_label.lock().unwrap() = "Preparing…".to_string();
            let prepared = prepare_model_pipeline(
                &want,
                &app_dir(),
                threads,
                &cancellation,
                &status,
                generation,
            );

            let mut apply = status
                .config_apply
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            apply.finish_prepare(generation);
            if status.shutdown.load(Ordering::Acquire) {
                return;
            }
            if !apply.is_current(generation) || cancellation.is_cancelled() {
                drop(apply);
                *status.model_download.lock().unwrap() = None;
                continue;
            }

            *status.model_download.lock().unwrap() = None;
            match prepared {
                Ok((asr, cleaners, label)) => {
                    *status.model_label.lock().unwrap() = label.clone();
                    break (asr, cleaners, label, true, want, None);
                }
                Err(models::ModelPrepareError::Cancelled) => {
                    // Cancellation is control flow, never evidence that a
                    // model is damaged or should enter the retry loop.
                    drop(apply);
                    continue;
                }
                Err(error) => {
                    log::error!("model prepare failed: {error}");
                    let label = format!("Model error: {error}");
                    *status.model_label.lock().unwrap() = label.clone();
                    report_runtime_error(
                        &status,
                        format!("The speech model is not ready yet: {error}"),
                    );
                    break (
                        Box::new(UnavailableAsr(error.to_string())),
                        Vec::new(),
                        label,
                        false,
                        want,
                        Some(error.to_string()),
                    );
                }
            }
        };

        // Audio was opened early so the UI could receive its level meter. If a
        // settings generation changed the selected device during startup,
        // align that resource with the model/config generation before building
        // the engine; otherwise the pending snapshot would look already active
        // and its device change would be skipped.
        if active_config.input_device != config.input_device {
            match CpalAudioCapture::new_for_with_level(
                active_config.input_device.as_deref(),
                shared_level.clone(),
            ) {
                Ok(capture) => {
                    audio = Box::new(capture);
                    audio_ready = true;
                }
                Err(error) => {
                    log::error!("startup microphone setting unavailable: {error}");
                    audio = Box::new(UnavailableAudio(error.to_string()));
                    audio_ready = false;
                    report_runtime_error(
                        &status,
                        format!("Could not open the selected microphone: {error}"),
                    );
                }
            }
        }

        let injector = Box::new(EnigoInjector::new(active_config.paste_insert));
        match load_rules_checked() {
            Ok(rules) => *status.rules.lock().unwrap() = merge_rules(&rules),
            Err(error) => {
                log::error!("{error}");
                report_runtime_error(&status, error);
            }
        }
        let rules = status.rules.clone();
        match migration::snapshot(
            &paths::data_dir(),
            vocalcode_core::migration::Kind::Snippets,
        ) {
            Ok((_, snippets)) => *status.snippets.lock().unwrap() = snippets,
            Err(error) => log::warn!("Could not load snippets: {error}"),
        }
        if status.shutdown.load(Ordering::Acquire) {
            return;
        }
        if model_ready {
            *status.model_label.lock().unwrap() = label.clone();
        }
        let live = active_config.live_caption;
        status.noise_filter.set_enabled(active_config.noise_filter);
        status.noise_filter.set_progressive(live);
        let (mut inference, asr, cleaners) = match inference::Worker::start(
            asr,
            cleaners,
            Some(Box::new(noise_filter::Filter::new(
                paths::data_dir(),
                status.noise_filter.clone(),
            ))),
        ) {
            Ok(worker) => worker,
            Err(error) => {
                report_runtime_error(&status, format!("Could not start inference: {error}"));
                return;
            }
        };
        let mut engine = Engine::new(
            audio,
            asr,
            injector,
            active_config.min_record_ms,
            16_000,
            live,
            status.inject_gate.clone(),
            status.talk_latched.clone(),
            rules,
            cleaners,
        );
        engine.set_snippets(status.snippets.clone());
        let workflow_base = paths::data_dir();
        match workflows::load(&workflow_base) {
            Ok((_, prefs)) => {
                if prefs.diagnostics {
                    match diagnostics::recent(&workflow_base) {
                        Ok(history) => *status.history.lock().unwrap() = history,
                        Err(error) => report_runtime_error(&status, error),
                    }
                }
                *status.workflows.lock().unwrap() = prefs;
            }
            Err(error) => report_runtime_error(&status, error),
        }
        let diagnostic_writer = match diagnostics::Writer::start(workflow_base, status.clone()) {
            Ok(writer) => Some(writer),
            Err(error) => {
                report_runtime_error(&status, error);
                None
            }
        };
        let mut utterance_prefs = workflows::Preferences::default();
        let mut utterance_app = String::new();
        let mut effective_paste = active_config.paste_insert;
        status.model_available.store(model_ready, Ordering::Release);
        status.ready.store(
            engine_ready(audio_ready, model_ready, &input_ready, &status),
            Ordering::Release,
        );
        // `setlang` wakes first-run preparation by setting this flag before an
        // engine exists. The model we just built already incorporates that
        // choice; a newer choice is still present in the config coordinator and will
        // set the flag again below.
        status.reload_model.store(false, Ordering::Release);
        let mut last_audio_retry = std::time::Instant::now() - std::time::Duration::from_secs(5);
        let mut model_retry_backoff = ModelRetryBackoff::default();
        let mut model_retry_at = (!model_ready).then(|| {
            schedule_model_retry(
                &status,
                &mut model_retry_backoff,
                &active_config,
                &app_dir(),
                startup_model_error
                    .as_deref()
                    .unwrap_or("The speech model is not ready yet."),
            )
        });
        // recv with a deadline so maintenance still runs under a continuous
        // stream of input events rather than only after a quiet timeout.
        let mut maintenance = MaintenanceClock::new(std::time::Instant::now());
        let mut last_dictation_poll = std::time::Instant::now();
        let mut pending_input = None;
        let mut pending_meeting = None;
        let mut meeting_in_flight: Option<inference::PendingMeeting> = None;
        loop {
            let events: Vec<_> = status
                .diagnostic_events
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .drain(..)
                .collect();
            let event_prefs = status
                .workflows
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            if event_prefs.diagnostics {
                if let Some(writer) = &diagnostic_writer {
                    for record in events {
                        if let Err(error) = writer.append(record, event_prefs.clone()) {
                            report_runtime_error(&status, error);
                        }
                    }
                }
            }
            if let Some(trace) = engine.take_trace() {
                publish_filler_review(&status, &trace);
                // A mid-utterance opt-out suppresses persistence immediately.
                if utterance_prefs.diagnostics && status.workflows.lock().unwrap().diagnostics {
                    if let Some(writer) = &diagnostic_writer {
                        let record = diagnostics::Record::dictation(
                            trace,
                            engine.model_label().into(),
                            active_config.language.clone(),
                            utterance_app.clone(),
                        );
                        if let Err(error) = writer.append(record, utterance_prefs.clone()) {
                            report_runtime_error(&status, error);
                        }
                    }
                }
            }
            if let Some((result, _)) = meeting_in_flight.as_ref() {
                let completed = match result.try_recv() {
                    Ok(text) => Some(text),
                    Err(mpsc::TryRecvError::Disconnected) => Some(Err(VocalCodeError::Asr(
                        "Meeting inference worker stopped".into(),
                    ))),
                    Err(mpsc::TryRecvError::Empty) => None,
                };
                if let Some(text) = completed {
                    let (_, reply) = meeting_in_flight.take().expect("pending inference");
                    let text = text
                        .and_then(|text| engine.finish_meeting_text(&text))
                        .map_err(|e| e.to_string());
                    let _ = reply.send(text);
                }
            }
            // Dispatch one bounded meeting segment to the native-model owner.
            // The input loop remains free to start capture while it decodes.
            if pending_input.is_none() {
                pending_input = rx.try_recv().ok();
            }
            if pending_meeting.is_none() && meeting_in_flight.is_none() && !engine.is_recording() {
                if let Ok(request) = meeting_asr.try_recv() {
                    pending_meeting = Some((std::time::Instant::now(), request));
                }
            }
            if let Some((queued_at, _)) = pending_meeting.as_ref() {
                if pending_input.is_none() {
                    pending_input = rx.try_recv().ok();
                }
                if meeting_asr_may_run(
                    engine.is_recording(),
                    pending_input.is_some(),
                    queued_at.elapsed(),
                ) {
                    let (_, request) = pending_meeting.take().expect("meeting request exists");
                    let result = if model_ready {
                        inference
                            .meeting(request.samples, request.sample_rate)
                            .map_err(|error| error.to_string())
                    } else {
                        Err("The local speech model is not ready.".to_string())
                    };
                    match result {
                        Ok(result) => meeting_in_flight = Some((result, request.reply)),
                        Err(error) => {
                            let _ = request.reply.send(Err(error));
                        }
                    }
                    continue;
                }
            }
            let maintenance_now = std::time::Instant::now();
            if maintenance.take_due(maintenance_now) {
                publish_correction_events(
                    &status,
                    &overlay,
                    &corrections,
                    engine.is_recording(),
                    utterance_prefs.diagnostics,
                );
                if let Err(error) = engine.poll_audio_health() {
                    invalidate_audio_on_error(&error, &mut audio_ready);
                    publish_recoverable_text(&status, &mut engine);
                    status.ready.store(false, Ordering::Release);
                    status
                        .listening
                        .store(engine.is_recording(), Ordering::Release);
                    overlay.set(if engine.is_recording() {
                        overlay::Phase::Recording
                    } else {
                        overlay::Phase::Idle
                    });
                    report_runtime_error(
                        &status,
                        format!("Microphone stopped responding: {error}"),
                    );
                    log::error!("audio health: {error}");
                }

                if engine.recording_limit_expired() {
                    log::warn!("maximum recording duration reached; forcing stop");
                    status.ready.store(false, Ordering::Release);
                    overlay.set(overlay::Phase::Transcribing);
                    if status.cue_sounds.load(Ordering::Relaxed) {
                        vocalcode_platform::cue::play(vocalcode_platform::Cue::Stop);
                    }
                    match engine.force_stop_to_history() {
                        Ok(outcome) => {
                            // The key-up was lost, so the recording may hold
                            // minutes of ambient audio and the focus snapshot
                            // is stale. The transcript goes to History only;
                            // tell the user where it went instead of typing it
                            // into whatever now has focus.
                            let salvaged = engine.has_recoverable_text();
                            publish_recoverable_text(&status, &mut engine);
                            if salvaged {
                                report_runtime_error(
                                    &status,
                                    "Recording hit the safety limit (the release was never seen). \
                                     The transcript was saved to History and was not typed anywhere."
                                        .to_string(),
                                );
                            }
                            let recording = engine.is_recording();
                            if publish_engine_outcome(
                                &status,
                                &overlay,
                                &corrections,
                                active_config.correction_window_ms,
                                outcome,
                                recording,
                            ) {
                                break;
                            }
                        }
                        Err(error) => {
                            invalidate_audio_on_error(&error, &mut audio_ready);
                            publish_recoverable_text(&status, &mut engine);
                            status
                                .listening
                                .store(engine.is_recording(), Ordering::Release);
                            overlay.set(if engine.is_recording() {
                                overlay::Phase::Recording
                            } else {
                                overlay::Phase::Idle
                            });
                            report_runtime_error(
                                &status,
                                format!("Could not finish the safety-limited recording: {error}"),
                            );
                            log::error!("recording watchdog: {error}");
                        }
                    }
                    if drain_busy_events(&rx, |cleanup| {
                        if let Err(error) = engine.handle(cleanup) {
                            invalidate_audio_on_error(&error, &mut audio_ready);
                            publish_recoverable_text(&status, &mut engine);
                            log::warn!("watchdog input cleanup failed: {error}");
                        }
                    }) {
                        break;
                    }
                }

                // Device changes and wireless disconnects are transient in
                // practice. Re-open only the accepted active input in place;
                // rejected settings are rolled back and never enter this loop.
                if !audio_ready
                    && !engine.is_recording()
                    && last_audio_retry.elapsed() >= std::time::Duration::from_secs(3)
                {
                    last_audio_retry = std::time::Instant::now();
                    match CpalAudioCapture::new_for_with_level(
                        active_config.input_device.as_deref(),
                        shared_level.clone(),
                    ) {
                        Ok(capture) => {
                            if engine.replace_audio(Box::new(capture)).is_ok() {
                                audio_ready = true;
                                *status.model_label.lock().unwrap() =
                                    engine.model_label().to_string();
                                log::info!("microphone recovered");
                            }
                        }
                        Err(error) => log::debug!("microphone retry: {error}"),
                    }
                }

                status.ready.store(
                    engine_ready(audio_ready, model_ready, &input_ready, &status),
                    Ordering::Release,
                );
            }

            if engine.is_recording() && last_dictation_poll.elapsed() >= DICTATION_POLL_INTERVAL {
                last_dictation_poll = std::time::Instant::now();
                if let Err(error) = engine.tick_partial() {
                    invalidate_audio_on_error(&error, &mut audio_ready);
                    publish_recoverable_text(&status, &mut engine);
                    status
                        .listening
                        .store(engine.is_recording(), Ordering::Release);
                    overlay.set(if engine.is_recording() {
                        overlay::Phase::Recording
                    } else {
                        overlay::Phase::Idle
                    });
                    report_runtime_error(&status, format!("Background dictation stopped: {error}"));
                    log::error!("partial: {error}");
                }
            }

            if model_retry_at.is_some_and(|at| std::time::Instant::now() >= at) {
                model_retry_at = None;
                status.reload_model.store(true, Ordering::Release);
            }
            // Apply the latest persisted settings only between utterances. A
            // single pending snapshot coalesces slider/toggle bursts and avoids
            // replaying obsolete intermediate configurations.
            // Drop the guard before entering the body: the busy path locks the
            // same coordinator again to put the desired snapshot back.
            let pending = status
                .config_apply
                .lock()
                .map(|mut apply| apply.take_pending())
                .unwrap_or_else(|poisoned| poisoned.into_inner().take_pending());
            if let Some(PendingConfig {
                generation,
                request_id,
                config: want,
            }) = pending
            {
                status
                    .settings_apply_pending
                    .store(false, Ordering::Release);
                // This desired generation supersedes any generic reload wake.
                // Its staged result below either installs a model, schedules a
                // real failure retry, or explicitly wakes recovery for the
                // still-unavailable active model.
                status.reload_model.store(false, Ordering::Release);
                if engine.is_recording()
                    || status.meetings.is_active()
                    || meeting_in_flight.is_some()
                {
                    status
                        .config_apply
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .requeue_if_no_newer(PendingConfig {
                            generation,
                            request_id,
                            config: want,
                        });
                } else {
                    // Prepare every fallible resource before changing the
                    // running engine. A request that changes a model and a
                    // toggle/device is one transaction, not a partial commit.
                    status.ready.store(false, Ordering::Release);
                    let needs_model = !models::same_model_route(
                        &want.model,
                        &want.language,
                        &active_config.model,
                        &active_config.language,
                    );
                    let mut accept_failed_model_for_retry = None;
                    let staged_model = if needs_model {
                        if models::needs_language_pick(&want.model, &want.language) {
                            reject_config(
                                &status,
                                &shared_config,
                                &triggers,
                                ConfigRejection {
                                    generation,
                                    request_id,
                                    rejected: &want,
                                    active: &active_config,
                                    message: "Choose a supported language before saving."
                                        .to_string(),
                                },
                            );
                            status.ready.store(
                                engine_ready(audio_ready, model_ready, &input_ready, &status),
                                Ordering::Release,
                            );
                            continue;
                        }
                        if drain_busy_events(&rx, |event| {
                            if let Err(error) = engine.handle(event) {
                                invalidate_audio_on_error(&error, &mut audio_ready);
                                publish_recoverable_text(&status, &mut engine);
                                log::warn!("config-stage input cleanup failed: {error}");
                            }
                        }) {
                            break;
                        }
                        let Some(cancellation) = begin_model_prepare(&status, generation, &want)
                        else {
                            continue;
                        };
                        let threads =
                            models::recommended_threads(&want.model, &want.language, hardware)
                                as i32;
                        let prepared_pipeline = prepare_model_pipeline(
                            &want,
                            &app_dir(),
                            threads,
                            &cancellation,
                            &status,
                            generation,
                        );
                        finish_model_prepare(&status, generation);
                        match prepared_pipeline {
                            Ok(pipeline) => Some(pipeline),
                            Err(models::ModelPrepareError::Cancelled) => {
                                // A newer persisted snapshot or shutdown owns
                                // the next action. Do not roll back, report a
                                // broken model, or schedule the fixed retry.
                                *status.model_download.lock().unwrap() = None;
                                *status.model_label.lock().unwrap() =
                                    engine.model_label().to_string();
                                status.ready.store(
                                    engine_ready(audio_ready, model_ready, &input_ready, &status),
                                    Ordering::Release,
                                );
                                if status.shutdown.load(Ordering::Acquire) {
                                    break;
                                }
                                continue;
                            }
                            Err(error)
                                if !accept_model_config_after_prepare_failure(model_ready) =>
                            {
                                let rejected = reject_config(
                                    &status,
                                    &shared_config,
                                    &triggers,
                                    ConfigRejection {
                                        generation,
                                        request_id,
                                        rejected: &want,
                                        active: &active_config,
                                        message: format!("Could not load that model: {error}"),
                                    },
                                );
                                if rejected {
                                    log::error!("prepare model setting: {error}");
                                    *status.model_download.lock().unwrap() = None;
                                    *status.model_label.lock().unwrap() =
                                        engine.model_label().to_string();
                                }
                                status.ready.store(
                                    engine_ready(audio_ready, model_ready, &input_ready, &status),
                                    Ordering::Release,
                                );
                                continue;
                            }
                            Err(error) => {
                                // First-run/offline has no usable model to roll
                                // back to. Keep the explicit language choice,
                                // acknowledge the persisted config, and retry
                                // the accepted model in the recovery loop.
                                accept_failed_model_for_retry = Some(error.to_string());
                                None
                            }
                        }
                    } else {
                        None
                    };

                    let staged_model = match staged_model {
                        Some((asr, cleaners, label)) => {
                            match inference::Worker::start(
                                asr,
                                cleaners,
                                Some(Box::new(noise_filter::Filter::new(
                                    paths::data_dir(),
                                    status.noise_filter.clone(),
                                ))),
                            ) {
                                Ok((worker, asr, cleaners)) => Some((worker, asr, cleaners, label)),
                                Err(error) => {
                                    reject_config(
                                        &status,
                                        &shared_config,
                                        &triggers,
                                        ConfigRejection {
                                            generation,
                                            request_id,
                                            rejected: &want,
                                            active: &active_config,
                                            message: format!(
                                                "Could not prepare inference worker: {error}"
                                            ),
                                        },
                                    );
                                    status.ready.store(
                                        engine_ready(
                                            audio_ready,
                                            model_ready,
                                            &input_ready,
                                            &status,
                                        ),
                                        Ordering::Release,
                                    );
                                    continue;
                                }
                            }
                        }
                        None => None,
                    };
                    // Open a second device only after any potentially long
                    // model download/load has completed. The old capture stays
                    // usable until the atomic commit and the staged stream is
                    // held for only the short commit window.
                    let staged_audio = if want.input_device != active_config.input_device {
                        match CpalAudioCapture::new_for_with_level(
                            want.input_device.as_deref(),
                            shared_level.clone(),
                        ) {
                            Ok(capture) => Some(capture),
                            Err(error) => {
                                let rejected = reject_config(
                                    &status,
                                    &shared_config,
                                    &triggers,
                                    ConfigRejection {
                                        generation,
                                        request_id,
                                        rejected: &want,
                                        active: &active_config,
                                        message: format!("Could not open that microphone: {error}"),
                                    },
                                );
                                if rejected {
                                    log::error!("prepare microphone setting: {error}");
                                    *status.model_download.lock().unwrap() = None;
                                    *status.model_label.lock().unwrap() =
                                        engine.model_label().to_string();
                                }
                                status.ready.store(
                                    engine_ready(audio_ready, model_ready, &input_ready, &status),
                                    Ordering::Release,
                                );
                                continue;
                            }
                        }
                    } else {
                        None
                    };

                    if drain_busy_events(&rx, |event| {
                        if let Err(error) = engine.handle(event) {
                            invalidate_audio_on_error(&error, &mut audio_ready);
                            publish_recoverable_text(&status, &mut engine);
                            log::warn!("config-commit input cleanup failed: {error}");
                        }
                    }) {
                        break;
                    }
                    if engine.is_recording() {
                        status
                            .config_apply
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .requeue_if_no_newer(PendingConfig {
                                generation,
                                request_id,
                                config: want,
                            });
                        continue;
                    }

                    // This guard is the linearization point shared with UI
                    // persistence/publication. Once held, either this exact
                    // generation commits in full or every staged resource is
                    // discarded without touching the live engine.
                    let apply = status
                        .config_apply
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if status.shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    if !apply.is_current(generation) {
                        drop(apply);
                        *status.model_download.lock().unwrap() = None;
                        *status.model_label.lock().unwrap() = engine.model_label().to_string();
                        continue;
                    }

                    if let Some(capture) = staged_audio {
                        let swapped = engine.replace_audio(Box::new(capture)).is_ok();
                        debug_assert!(swapped, "staged audio commit must be idle");
                        audio_ready = swapped;
                    }
                    if let Some((worker, asr, cleaners, label)) = staged_model {
                        let swapped = engine.swap_asr(asr, cleaners);
                        debug_assert!(swapped, "staged model commit must be idle");
                        if swapped {
                            inference = worker;
                            model_ready = true;
                            status.model_available.store(true, Ordering::Release);
                            model_retry_backoff.reset();
                            model_retry_at = None;
                            *status.model_label.lock().unwrap() = label;
                        }
                    }
                    if want.paste_insert != active_config.paste_insert {
                        let replaced = engine
                            .replace_injector(Box::new(EnigoInjector::new(want.paste_insert)))
                            .is_ok();
                        debug_assert!(replaced, "staged injector commit must be idle");
                        if replaced {
                            effective_paste = want.paste_insert;
                        }
                    }
                    let min_applied = engine.set_min_record_ms(want.min_record_ms);
                    let live_applied = engine.set_live_caption(want.live_caption);
                    debug_assert!(min_applied && live_applied, "config commit must be idle");
                    if want.correction_window_ms != active_config.correction_window_ms {
                        corrections.cancel();
                        overlay.set(overlay::Phase::Idle);
                    }
                    *triggers.lock().unwrap() =
                        (want.talk.clone(), want.send.clone(), want.teach.clone());
                    status
                        .talk_latched
                        .store(want.talk_mode == "toggle", Ordering::Release);
                    status.cue_sounds.store(want.cue_sounds, Ordering::Release);
                    status.onboarded.store(want.onboarded, Ordering::Release);
                    active_config = want;
                    status.noise_filter.set_enabled(active_config.noise_filter);
                    status
                        .noise_filter
                        .set_progressive(active_config.live_caption);

                    if let Some(error) = accept_failed_model_for_retry {
                        log::error!("initial model setting unavailable: {error}");
                        model_ready = false;
                        status.model_available.store(false, Ordering::Release);
                        model_retry_backoff.reset();
                        *status.model_download.lock().unwrap() = None;
                        model_retry_at = Some(schedule_model_retry(
                            &status,
                            &mut model_retry_backoff,
                            &active_config,
                            &app_dir(),
                            &error,
                        ));
                        *status.model_label.lock().unwrap() = format!("Model error: {error}");
                        report_runtime_error(
                            &status,
                            format!("The setting was saved, but the speech model is not ready yet: {error}"),
                        );
                    } else if !model_ready && model_retry_at.is_none() {
                        status.reload_model.store(true, Ordering::Release);
                    }
                    status.ready.store(
                        engine_ready(audio_ready, model_ready, &input_ready, &status),
                        Ordering::Release,
                    );
                    publish_config_result(&status, request_id, true, "Saved", &active_config);
                }
            }

            // Automatic recovery only retries the already accepted active
            // model. Page-requested model changes are staged atomically above;
            // rejected snapshots never reach this loop.
            if status.reload_model.swap(false, Ordering::Relaxed) {
                if engine.is_recording()
                    || status.meetings.is_active()
                    || meeting_in_flight.is_some()
                {
                    status.reload_model.store(true, Ordering::Release);
                } else if !model_ready {
                    status.ready.store(false, Ordering::Release);
                    if drain_busy_events(&rx, |event| {
                        if let Err(error) = engine.handle(event) {
                            invalidate_audio_on_error(&error, &mut audio_ready);
                            publish_recoverable_text(&status, &mut engine);
                            log::warn!("model-load input cleanup failed: {error}");
                        }
                    }) {
                        break;
                    }
                    let generation = status
                        .config_apply
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .generation();
                    let Some(cancellation) =
                        begin_model_prepare(&status, generation, &active_config)
                    else {
                        continue;
                    };
                    let threads = models::recommended_threads(
                        &active_config.model,
                        &active_config.language,
                        hardware,
                    ) as i32;
                    let prepared = prepare_model_pipeline(
                        &active_config,
                        &app_dir(),
                        threads,
                        &cancellation,
                        &status,
                        generation,
                    );
                    finish_model_prepare(&status, generation);
                    if matches!(&prepared, Err(models::ModelPrepareError::Cancelled)) {
                        *status.model_download.lock().unwrap() = None;
                        *status.model_label.lock().unwrap() = engine.model_label().to_string();
                        status.ready.store(
                            engine_ready(audio_ready, model_ready, &input_ready, &status),
                            Ordering::Release,
                        );
                        if status.shutdown.load(Ordering::Acquire) {
                            break;
                        }
                        continue;
                    }
                    if drain_busy_events(&rx, |event| {
                        if let Err(error) = engine.handle(event) {
                            invalidate_audio_on_error(&error, &mut audio_ready);
                            publish_recoverable_text(&status, &mut engine);
                            log::warn!("model-load input cleanup failed: {error}");
                        }
                    }) {
                        break;
                    }
                    if engine.is_recording() {
                        status.reload_model.store(true, Ordering::Release);
                        continue;
                    }
                    let apply = status
                        .config_apply
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if status.shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    if !apply.is_current(generation) || cancellation.is_cancelled() {
                        drop(apply);
                        *status.model_download.lock().unwrap() = None;
                        *status.model_label.lock().unwrap() = engine.model_label().to_string();
                        continue;
                    }
                    match prepared {
                        Ok((new_asr, cleaners, new_label)) => {
                            let (worker, new_asr, cleaners) = match inference::Worker::start(
                                new_asr,
                                cleaners,
                                Some(Box::new(noise_filter::Filter::new(
                                    paths::data_dir(),
                                    status.noise_filter.clone(),
                                ))),
                            ) {
                                Ok(worker) => worker,
                                Err(error) => {
                                    let message = format!("Could not recover inference: {error}");
                                    model_retry_at = Some(schedule_model_retry(
                                        &status,
                                        &mut model_retry_backoff,
                                        &active_config,
                                        &app_dir(),
                                        &message,
                                    ));
                                    report_runtime_error(&status, message);
                                    continue;
                                }
                            };
                            let swapped = engine.swap_asr(new_asr, cleaners);
                            debug_assert!(swapped, "model swap must be idle here");
                            if !swapped {
                                status.reload_model.store(true, Ordering::Release);
                                status.ready.store(
                                    engine_ready(audio_ready, model_ready, &input_ready, &status),
                                    Ordering::Release,
                                );
                                continue;
                            }
                            model_ready = true;
                            inference = worker;
                            status.model_available.store(true, Ordering::Release);
                            model_retry_backoff.reset();
                            model_retry_at = None;
                            *status.model_download.lock().unwrap() = None;
                            *status.model_label.lock().unwrap() = new_label.clone();
                            log::info!("speech model recovered -> {new_label}");
                        }
                        Err(models::ModelPrepareError::TimedOut(message))
                        | Err(models::ModelPrepareError::Failed(message)) => {
                            log::error!("model reload failed: {message}");
                            *status.model_download.lock().unwrap() = None;
                            *status.model_label.lock().unwrap() = format!("Model error: {message}");
                            model_retry_at = Some(schedule_model_retry(
                                &status,
                                &mut model_retry_backoff,
                                &active_config,
                                &app_dir(),
                                &message,
                            ));
                            report_runtime_error(
                                &status,
                                format!("Could not load the speech model: {message}"),
                            );
                        }
                        Err(models::ModelPrepareError::Cancelled) => {
                            unreachable!("cancelled model preparation was handled before commit")
                        }
                    }
                    status.ready.store(
                        engine_ready(audio_ready, model_ready, &input_ready, &status),
                        Ordering::Release,
                    );
                }
            }
            let mut input_wait = maintenance.wait(std::time::Instant::now());
            if engine.is_recording() {
                input_wait = input_wait
                    .min(DICTATION_POLL_INTERVAL.saturating_sub(last_dictation_poll.elapsed()));
            }
            if !engine.is_recording() && meeting_in_flight.is_none() {
                if let Some((queued_at, _)) = pending_meeting.as_ref() {
                    input_wait =
                        input_wait.min(MEETING_ASR_INPUT_GRACE.saturating_sub(queued_at.elapsed()));
                }
            }
            let input = pending_input
                .take()
                .map(Ok)
                .unwrap_or_else(|| rx.recv_timeout(input_wait));
            // Never poll the UI mailbox ahead of the global input queue:
            // Cancel/ForceStop/Quit discard ordinary queued starts there. Its
            // Wake must first pass the same ordered/emergency-aware receiver.
            let control_dispatch = if matches!(input, Ok(TriggerEvent::Wake)) {
                status.dictation_control.take(
                    overlay.snapshot(),
                    status.ready.load(Ordering::Acquire)
                        && !status.shutdown.load(Ordering::Acquire),
                    engine.is_recording(),
                    active_config.desktop_control,
                    Instant::now(),
                )
            } else {
                if let Ok(event) = input {
                    status.dictation_control.observe_control(event);
                }
                None
            };
            let input = control_dispatch
                .as_ref()
                .map(|dispatch| Ok(dispatch.event))
                .unwrap_or(input);
            match input {
                Ok(ev) => {
                    // Show "transcribing" the moment the recording actually
                    // ends, before the decode starts — that gap is otherwise
                    // the longest stretch with no feedback at all. Ask the
                    // engine which event ends it; a talk release only does in
                    // hold mode.
                    // Grabbing a selection is not the engine's job and must not
                    // block it: the clipboard round trip sleeps ~180 ms waiting
                    // for the other app to answer, and doing that on this thread
                    // would stall the next keypress.
                    if matches!(ev, TriggerEvent::TeachTapped(_)) {
                        if status
                            .teach_in_progress
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                            .is_err()
                        {
                            log::debug!("teach: selection copy already in progress");
                            continue;
                        }
                        let s = status.clone();
                        let worker_status = s.clone();
                        let spawned = webui::spawn_teach_worker(&s, move || {
                            let _reset = TeachReset(worker_status.clone());
                            match vocalcode_platform::copy_selection() {
                                Ok(Some(text)) => {
                                    log::info!(
                                        "teach: {} characters from the selection",
                                        text.chars().count()
                                    );
                                    *worker_status
                                        .teach
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                                        Some(text);
                                }
                                // Nothing selected. Deliberately silent — a shortcut
                                // that pops a window open when you hit it by mistake
                                // is worse than one that does nothing.
                                Ok(None) => log::info!("teach: nothing selected"),
                                Err(error) => {
                                    log::error!("teach: selection copy failed: {error}");
                                    report_runtime_error(
                                        &worker_status,
                                        format!("Could not copy the selected text: {error}"),
                                    );
                                }
                            }
                        });
                        match spawned {
                            Ok(()) => {}
                            Err(e) => {
                                s.teach_in_progress.store(false, Ordering::Release);
                                report_runtime_error(
                                    &s,
                                    format!("Could not start selection capture: {e}"),
                                );
                                log::error!("teach: start worker: {e}");
                            }
                        }
                        continue;
                    }
                    // Both cues are decided here, before `handle`, because
                    // both are answers to "your key registered" and `handle` is
                    // where the expensive work lives. Hung off the published
                    // engine state instead, the start cue landed after
                    // audio.start() and the focus lookup, and the stop cue
                    // after the whole blocking decode — seconds late, long
                    // after the finger left the key.
                    let starting = engine.will_start(ev);
                    let finishing = engine.will_finish(ev);
                    if starting {
                        utterance_prefs = status.workflows.lock().unwrap().clone();
                        utterance_app = if utterance_prefs.diagnostics
                            || !utterance_prefs.profiles.is_empty()
                            || active_config.writing.needs_app_identity()
                        {
                            vocalcode_platform::foreground_app_id().unwrap_or_default()
                        } else {
                            String::new()
                        };
                        let (cleanup, progressive, paste) = utterance_prefs.resolve(
                            &utterance_app,
                            active_config.live_caption,
                            active_config.paste_insert,
                        );
                        engine.set_trace_enabled(utterance_prefs.diagnostics);
                        engine.set_filler_removal(
                            utterance_prefs.fillers_for(&utterance_app, &active_config.language),
                            &active_config.language,
                        );
                        engine.set_cleanup_enabled(cleanup == workflows::Cleanup::Light);
                        engine.set_writing(
                            active_config.writing.options_for(&utterance_app),
                            &active_config.language,
                        );
                        engine.set_double_tap_lock(
                            active_config.talk_mode == "hold" && active_config.double_tap_lock,
                        );
                        engine.set_live_caption(progressive);
                        status.noise_filter.set_progressive(progressive);
                        if paste != effective_paste
                            && engine
                                .replace_injector(Box::new(EnigoInjector::new(paste)))
                                .is_ok()
                        {
                            effective_paste = paste;
                        }
                        corrections.cancel();
                    }
                    let cue_on = status.cue_sounds.load(Ordering::Relaxed);
                    if cue_on && (starting || finishing) {
                        vocalcode_platform::cue::play(if starting {
                            vocalcode_platform::Cue::Start
                        } else {
                            vocalcode_platform::Cue::Stop
                        });
                    }
                    if finishing {
                        // `Engine::finish` performs ASR/cleanup/injection on this
                        // thread and can take seconds. Do not keep consuming
                        // global actions into the channel while it is busy: in
                        // toggle mode a tap replayed afterwards would otherwise
                        // begin an unexpected, indefinitely latched recording.
                        status.ready.store(false, Ordering::Release);
                        overlay.set(overlay::Phase::Transcribing);
                    }
                    let mut should_quit = false;
                    match engine.handle(ev) {
                        Ok(outcome) => {
                            let recording = engine.is_recording();
                            if let Outcome::Transcribed(text) = &outcome {
                                if !text.is_empty() {
                                    status.activity.record(text, engine.last_audio_ms());
                                }
                            }
                            if publish_engine_outcome(
                                &status,
                                &overlay,
                                &corrections,
                                active_config.correction_window_ms,
                                outcome,
                                recording,
                            ) {
                                should_quit = true;
                            }
                        }
                        Err(e) => {
                            if invalidate_audio_on_error(&e, &mut audio_ready) {
                                status.ready.store(false, Ordering::Release);
                            }
                            publish_recoverable_text(&status, &mut engine);
                            // The start cue has already been played, so say the
                            // recording ended rather than leaving the user
                            // listening to a microphone that never opened.
                            if cue_on && starting {
                                vocalcode_platform::cue::play(vocalcode_platform::Cue::Stop);
                            }
                            let recording = engine.is_recording();
                            status.listening.store(recording, Ordering::Release);
                            overlay.set(if recording {
                                overlay::Phase::Recording
                            } else {
                                overlay::Phase::Idle
                            });
                            // A diverted transcript is not a failed action:
                            // it already carries the whole sentence the user
                            // needs, including where to find their words.
                            if let VocalCodeError::Diverted(notice) = &e {
                                report_runtime_error(&status, notice.clone());
                                log::info!("engine: {e}");
                            } else {
                                report_runtime_error(
                                    &status,
                                    format!("VocalCode could not complete that action: {e}"),
                                );
                                log::error!("engine: {e}");
                            }
                        }
                    }
                    // Other playback is muted only while this dictation is
                    // recording, and never during a meeting: its system-audio
                    // loopback is exactly that playback.
                    vocalcode_platform::mute::set_others_muted(
                        active_config.mute_while_dictating
                            && engine.is_recording()
                            && !status.meetings.is_active(),
                    );
                    if finishing {
                        // A release/disconnect may be emitted by a control that
                        // was already active when readiness closed. Feed those
                        // cleanup edges through the now-idle engine, but never
                        // replay a newly queued action after the decode.
                        status.dictation_control.discard_pending();
                        should_quit |= drain_busy_events(&rx, |cleanup| {
                            if let Err(error) = engine.handle(cleanup) {
                                invalidate_audio_on_error(&error, &mut audio_ready);
                                publish_recoverable_text(&status, &mut engine);
                                log::warn!("busy-window input cleanup failed: {error}");
                            }
                        });
                        status.ready.store(
                            engine_ready(audio_ready, model_ready, &input_ready, &status)
                                && !should_quit,
                            Ordering::Release,
                        );
                    }
                    if should_quit {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    vocalcode_platform::mute::set_others_muted(
                        active_config.mute_while_dictating
                            && engine.is_recording()
                            && !status.meetings.is_active(),
                    );
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        // Never leave someone's music muted behind an exiting engine.
        vocalcode_platform::mute::restore_blocking(Duration::from_millis(500));
    });
    BackgroundRuntime {
        events: tx,
        input_shutdown,
        hotkey: hotkey_thread,
        engine: engine_thread,
        engine_done: engine_done_rx,
    }
}

// ---------------------------------------------------------------------------
// Offline self-test / benchmark
// ---------------------------------------------------------------------------

/// Background check for a newer release. Fetches a small JSON manifest
/// `{version, url}`; if it advertises a newer version, stashes it for the UI to
/// surface as a download banner. Silent on any failure (e.g. offline / no site).
/// Kick off one check in the background. The maintenance owner calls this at
/// startup and every six hours; the settings UI can also call it on demand.
fn start_update_check(status: Arc<RuntimeStatus>) {
    let worker_status = status.clone();
    match webui::spawn_update_check(&status, "vocalcode-update-check", move || {
        check_for_update(&worker_status);
    }) {
        Ok(webui::UpdateCheckStart::Started) => {}
        Ok(webui::UpdateCheckStart::Busy) => {
            log::info!("update check already in progress");
        }
        Err(error) => {
            log::warn!("could not start update check: {error}");
        }
    }
}

const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

fn shutdown_aware_pause(status: &RuntimeStatus, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        if status.shutdown.load(Ordering::Acquire) {
            return false;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return true;
        };
        thread::sleep(remaining.min(Duration::from_millis(50)));
    }
}

fn start_update_maintenance(status: Arc<RuntimeStatus>) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("vocalcode-update-maintenance".to_string())
        .spawn(move || {
            // Check immediately, then keep a tray process current without
            // making the user reopen Settings or restart the app.
            start_update_check(status.clone());
            while shutdown_aware_pause(&status, UPDATE_CHECK_INTERVAL) {
                start_update_check(status.clone());
            }
        })
}

const UPDATE_MANIFEST_URL: &str = if community::ENABLED {
    community::UPDATE_MANIFEST_URL
} else {
    "https://vocalcode.app/latest.json"
};

fn update_manifest_url() -> String {
    // Local/debug contract tests may point at a fixture. A production binary
    // must never inherit a service/launcher environment override and silently
    // turn a different origin into its update authority.
    #[cfg(any(test, debug_assertions))]
    {
        std::env::var("VOCALCODE_UPDATE_URL").unwrap_or_else(|_| UPDATE_MANIFEST_URL.to_string())
    }
    #[cfg(not(any(test, debug_assertions)))]
    {
        UPDATE_MANIFEST_URL.to_string()
    }
}

pub(crate) fn approved_update_url(platform: &str, version: &str, url: &str) -> bool {
    if release_version(version).is_none() {
        return false;
    }
    if community::ENABLED {
        return community::artifact_url(platform, version).as_deref() == Some(url);
    }
    match platform {
        "windows" => url == "https://vocalcode.app/VocalCodeSetup.exe",
        "macos" => url == format!("https://vocalcode.app/VocalCode-{version}.dmg"),
        _ => false,
    }
}

pub(crate) fn update_entitled(status: &LicenseStatus, version: &str) -> bool {
    if community::ENABLED {
        return release_version(version).is_some();
    }
    let Some([major, _, _]) = release_version(version) else {
        return false;
    };
    let generation = major.max(1);
    match status {
        LicenseStatus::Licensed { max_version } => generation <= u64::from(*max_version),
        // An active trial evaluates the currently published build, including a
        // new major; expiry is rechecked again when Install is pressed.
        LicenseStatus::Trial { .. } => true,
        LicenseStatus::TrialSetupRequired | LicenseStatus::Expired | LicenseStatus::Invalid(_) => {
            false
        }
    }
}

/// A release offer whose executable bytes are bound to both a digest and an
/// exact published size.  The size is security-sensitive: without it, a
/// compromised or misconfigured origin can consume almost the entire disk
/// before the final digest mismatch is discovered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UpdateOffer {
    version: String,
    url: String,
    notes: String,
    sha256: String,
    size: u64,
}

/// Both current installers are far smaller than this (the optional bundled
/// model build is roughly 250 MiB). Leave generous release headroom without
/// accepting the former 2 GiB resource-exhaustion window.
pub(crate) const MAX_UPDATE_BYTES: u64 = 512 * 1024 * 1024;

fn valid_update_size(size: u64) -> bool {
    (1..=MAX_UPDATE_BYTES).contains(&size)
}

fn valid_update_sha256(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn update_offer_from_manifest(v: &serde_json::Value, platform: &str) -> Option<UpdateOffer> {
    let platform_node = v.get(platform);
    let node = match platform_node {
        Some(node) if node.is_object() => node,
        Some(_) => return None,
        None => v,
    };
    // A modern platform object is one indivisible signed-artifact description:
    // never splice its identity/hash/size together with legacy root fields.
    // Only release notes are intentionally shared across platforms.
    let pick_required = |key: &str| {
        node.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let size = node
        .get("size")
        .and_then(serde_json::Value::as_u64)
        .filter(|size| valid_update_size(*size))?;
    let sha256 = pick_required("sha256")?.to_ascii_lowercase();
    // Do not normalize uppercase input into an accepted digest. The release
    // contract is canonical lowercase hex, which makes comparisons unambiguous.
    if !valid_update_sha256(&sha256)
        || node.get("sha256").and_then(serde_json::Value::as_str) != Some(sha256.as_str())
    {
        return None;
    }
    Some(UpdateOffer {
        version: pick_required("version")?,
        url: pick_required("url")?,
        notes: node
            .get("notes")
            .or_else(|| platform_node.and_then(|_| v.get("notes")))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        sha256,
        size,
    })
}

fn begin_update_check(status: &RuntimeStatus) -> u64 {
    let _state_guard = status.update_state_guard.lock().unwrap();
    let generation = status.update_generation.fetch_add(1, Ordering::AcqRel) + 1;
    *status.update.lock().unwrap() = None;
    generation
}

fn finish_update_check(
    status: &RuntimeStatus,
    generation: u64,
    kind: &str,
    offer: Option<UpdateOffer>,
) {
    if status.shutdown.load(Ordering::Acquire) {
        return;
    }
    let _state_guard = status.update_state_guard.lock().unwrap();
    if status.update_generation.load(Ordering::Acquire) != generation {
        return;
    }
    *status.update.lock().unwrap() = offer;
    *status.update_check.lock().unwrap() = Some(kind.to_string());
}

/// Look for a newer build and always record an outcome.
///
/// It used to return silently on every failure path, which was fine while this
/// only ran at startup — nobody was waiting on an answer. With a button behind
/// it, silence is indistinguishable from a hung check, so each exit now leaves
/// something for the window to say.
///
/// The outcome is a bare kind, not a sentence: prose assembled in Rust cannot be
/// translated by a dictionary keyed on English, which is the mistake the licence
/// line made.
fn check_for_update(status: &RuntimeStatus) {
    {
        if status.shutdown.load(Ordering::Acquire) {
            return;
        }
        let generation = begin_update_check(status);
        let current = env!("CARGO_PKG_VERSION");
        let manifest = update_manifest_url();
        let request = activation::cancellable_network_request(
            &status.shutdown,
            "vocalcode-update-manifest-request",
            move || {
                let manifest = if community::ENABLED {
                    community::resolve_download_url(&manifest, || false)
                        .map_err(anyhow::Error::msg)?
                } else {
                    manifest
                };
                let mut response = ureq::get(&manifest)
                    .config()
                    .https_only(true)
                    .max_redirects(0)
                    .timeout_connect(Some(std::time::Duration::from_secs(10)))
                    .timeout_recv_response(Some(std::time::Duration::from_secs(20)))
                    .timeout_global(Some(std::time::Duration::from_secs(45)))
                    .build()
                    .call()
                    .map_err(|error| anyhow::anyhow!("update manifest request failed: {error}"))?;
                response
                    .body_mut()
                    .with_config()
                    .limit(64 * 1024)
                    .read_json::<serde_json::Value>()
                    .map_err(|error| anyhow::anyhow!("read update manifest: {error}"))
            },
        );
        let v = match request {
            Ok(activation::CancellableRequest::Completed(value)) => value,
            Ok(activation::CancellableRequest::Cancelled) => return,
            Err(_) => {
                finish_update_check(status, generation, "failed", None);
                return;
            }
        };
        if community::ENABLED
            && (v["schema"] != "vocalcode-community-update-v1"
                || v["channel"] != "community-stable")
        {
            finish_update_check(status, generation, "failed", None);
            return;
        }
        // The manifest may carry a per-platform block, so a Mac is not offered
        // the Windows installer. Falls back to the top-level {version, url} for
        // compatibility with manifests published before the macOS build.
        let platform = if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(windows) {
            "windows"
        } else {
            "linux"
        };
        let Some(offer) = update_offer_from_manifest(&v, platform) else {
            log::warn!("update manifest omitted a valid bounded platform offer");
            finish_update_check(status, generation, "failed", None);
            return;
        };
        let (ver, url) = (offer.version.as_str(), offer.url.as_str());
        if is_newer(ver, current) {
            if !approved_update_url(platform, ver, url) {
                log::warn!("update manifest contained an unapproved artifact URL");
                finish_update_check(status, generation, "failed", None);
                return;
            }
            if !update_entitled(&license_status(), ver) {
                log::info!("newer release {ver} is outside this licence entitlement");
                finish_update_check(status, generation, "upgrade", None);
                return;
            }
            log::info!("update available: {ver}");
            finish_update_check(status, generation, "newer", Some(offer));
        } else {
            log::info!("up to date at {current}");
            finish_update_check(status, generation, "current", None);
        }
    }
}

/// Strict stable release compare: exactly three decimal numeric components.
fn release_version(value: &str) -> Option<[u64; 3]> {
    if value.is_empty() || value.len() > 64 {
        return None;
    }
    let mut parts = value.split('.');
    let mut parsed = [0_u64; 3];
    for slot in &mut parsed {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 20 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    parts.next().is_none().then_some(parsed)
}

fn is_newer(remote: &str, local: &str) -> bool {
    matches!((release_version(remote), release_version(local)), (Some(remote), Some(local)) if remote > local)
}

#[cfg(feature = "diagnostic-cli")]
fn run_transcribe(paths: &[String]) -> anyhow::Result<()> {
    // Use the running configuration, not a hardcoded default. This subcommand is
    // the self-test, and one that always exercised the routed pair could tell you
    // nothing about the single-model setup you actually have.
    let cfg = load_config().map_err(anyhow::Error::msg)?;
    let threads =
        models::recommended_threads(&cfg.model, &cfg.language, HardwareProfile::detect()) as i32;
    let (mut asr, _label) = models::prepare_asr(
        &cfg.model,
        &cfg.language,
        &app_dir(),
        threads,
        |p: models::Progress| eprintln!("{} {:.0}%", p.label, p.percent()),
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    eprintln!(
        "config:     model={:?} language={:?}",
        cfg.model, cfg.language
    );

    // Gated exactly as the running app gates it. A self-test that assembles a
    // different cleaner chain from the one users get reports on a pipeline
    // nobody runs — it is how the punctuator went on mangling Russian here
    // after being switched off there.
    let punct = if models::wants_punct(&cfg.model, &cfg.language) {
        find_punct()
    } else {
        None
    };
    let mut cleaners = models::build_cleaners(
        punct,
        models::wants_cjk_space_collapse(&cfg.model, &cfg.language),
    )
    .map_err(anyhow::Error::msg)?;
    let many = paths.len() > 1;

    for path in paths {
        let mut reader = hound::WavReader::open(path)?;
        let spec = reader.spec();
        let samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => {
                reader.samples::<f32>().map(|s| s.unwrap_or(0.0)).collect()
            }
            hound::SampleFormat::Int => {
                let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
                reader
                    .samples::<i32>()
                    .map(|s| s.unwrap_or(0) as f32 / scale)
                    .collect()
            }
        };
        let ch = spec.channels as usize;
        let mono: Vec<f32> = if ch > 1 {
            samples
                .chunks(ch)
                .map(|f| f.iter().sum::<f32>() / ch as f32)
                .collect()
        } else {
            samples
        };
        let secs = mono.len() as f32 / spec.sample_rate as f32;
        let t = std::time::Instant::now();
        let raw = asr.transcribe(&mono, spec.sample_rate)?.trim().to_string();
        let elapsed = t.elapsed();
        let mut cleaned = raw.clone();
        for c in &mut cleaners {
            if let Ok(x) = c.clean(&cleaned) {
                cleaned = x;
            }
        }
        if many {
            // One tab-separated line per file, so a corpus run can be piped
            // straight into whatever is scoring it.
            println!(
                "{path}\t{:.0}\t{raw}\t{cleaned}",
                elapsed.as_secs_f32() * 1000.0
            );
        } else {
            println!("model:      {}", asr.model_label());
            println!("audio:      {secs:.2}s @ {} Hz, {ch} ch", spec.sample_rate);
            println!(
                "latency:    {:.0} ms  (RTF {:.2})",
                elapsed.as_secs_f32() * 1000.0,
                elapsed.as_secs_f32() / secs.max(0.001)
            );
            println!("raw:        {raw}");
            println!("cleaned:    {cleaned}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn update_failure_message(args: &[String]) -> Option<String> {
    (args.get(1).map(String::as_str) == Some("--update-failed")).then(|| {
        let code = args.get(2).map(String::as_str).unwrap_or("unknown");
        match code {
            "helper-launch-error" =>
                "The update helper could not start a required process. VocalCode was reopened so you can retry or install the update manually."
                    .to_string(),
            "installer-timeout" =>
                "The update installer did not finish within 15 minutes and was stopped. VocalCode was reopened without completing the update."
                    .to_string(),
            "parent-exit-timeout" =>
                "VocalCode did not finish shutting down within 5 minutes, so the update was not started. Close VocalCode and retry the update."
                    .to_string(),
            _ => format!(
                "The update installer exited with code {code}. VocalCode was reopened without completing the update."
            ),
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivationInput {
    Prompt,
    Stdin,
}

fn diagnostic_command(args: &[String]) -> Option<&str> {
    match args.get(1).map(String::as_str) {
        Some(command @ ("punct" | "keyprobe" | "transcribe")) => Some(command),
        _ => None,
    }
}

#[cfg(windows)]
const WINDOWS_ACTIVATION_PROMPT_ERROR: &str =
    "this Windows GUI build has no safe interactive console; activate in the License panel or pipe the key to `vocalcode-app activate --stdin`";

fn activation_input(args: &[String]) -> Result<Option<ActivationInput>, String> {
    if args.get(1).map(String::as_str) != Some("activate") {
        return Ok(None);
    }
    match args.get(2).map(String::as_str) {
        None => Ok(Some(ActivationInput::Prompt)),
        Some("--stdin") if args.len() == 3 => Ok(Some(ActivationInput::Stdin)),
        _ => Err(
            "secure usage: vocalcode-app activate [--stdin] (license keys are never accepted in command-line arguments)"
                .to_string(),
        ),
    }
}

const UNINSTALL_CLEANUP_ARG: &str = "--uninstall-cleanup";
const DETACHED_PURGE_HELPER_ARG: &str = "--purge-after-parent-exit";
const UNINSTALL_CLEANUP_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn run_early_uninstall_cleanup_with<C>(args: &[String], cleanup: C) -> Result<bool, String>
where
    C: FnOnce() -> Result<(), String>,
{
    let requested = args.iter().skip(1).any(|arg| arg == UNINSTALL_CLEANUP_ARG);
    if !requested {
        return Ok(false);
    }
    if args.len() != 2 || args[1] != UNINSTALL_CLEANUP_ARG {
        return Err(format!(
            "internal usage: {} {UNINSTALL_CLEANUP_ARG}",
            args.first().map(String::as_str).unwrap_or("vocalcode-app")
        ));
    }
    cleanup()?;
    Ok(true)
}

fn run_uninstall_cleanup_transaction_with<G, M, P>(
    deadline: std::time::Instant,
    migrate: M,
    purge: P,
) -> Result<(), String>
where
    M: FnOnce(std::time::Instant) -> Result<G, String>,
    P: FnOnce(std::time::Instant) -> Result<(), String>,
{
    let migration = migrate(deadline)?;
    drop(migration);
    purge(deadline)
}

fn run_uninstall_cleanup_transaction() -> Result<(), String> {
    // An uninstall may be the first process launched after an upgrade. Let the
    // existing no-follow/no-clobber migration finish recovering allow-listed
    // legacy data, then release its shared guard before purge asks for the
    // exclusive lifecycle lock. Holding both would deadlock this process.
    let deadline = std::time::Instant::now()
        .checked_add(UNINSTALL_CLEANUP_LOCK_TIMEOUT)
        .unwrap_or_else(std::time::Instant::now);
    run_uninstall_cleanup_transaction_with(
        deadline,
        |deadline| {
            paths::enter_data_lifecycle_until(deadline).map_err(|error| {
                format!("could not migrate legacy app data before cleanup: {error}")
            })
        },
        webui::purge_user_data_after_shutdown_until,
    )
}

/// The parent owns the write end of this helper's stdin pipe and deliberately
/// keeps it open until process termination. EOF is therefore a kernel-backed
/// proof that every thread in the old process, including a detached native
/// engine call, has ceased to exist. Any data on the control pipe is invalid
/// and fails closed instead of allowing an early purge.
fn wait_for_parent_process_exit<R: Read>(parent_lifetime: &mut R) -> Result<(), String> {
    let mut unexpected = [0_u8; 1];
    loop {
        match parent_lifetime.read(&mut unexpected) {
            Ok(0) => return Ok(()),
            Ok(_) => {
                return Err(
                    "detached purge helper received unexpected parent control data".to_string(),
                )
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(format!(
                    "detached purge helper could not observe parent exit: {error}"
                ))
            }
        }
    }
}

fn run_early_detached_purge_helper_with<R, C>(
    args: &[String],
    parent_lifetime: &mut R,
    cleanup: C,
) -> Result<bool, String>
where
    R: Read,
    C: FnOnce() -> Result<(), String>,
{
    let requested = args
        .iter()
        .skip(1)
        .any(|arg| arg == DETACHED_PURGE_HELPER_ARG);
    if !requested {
        return Ok(false);
    }
    if args.len() != 2 || args[1] != DETACHED_PURGE_HELPER_ARG {
        return Err(format!(
            "internal usage: {} {DETACHED_PURGE_HELPER_ARG}",
            args.first().map(String::as_str).unwrap_or("vocalcode-app")
        ));
    }
    wait_for_parent_process_exit(parent_lifetime)?;
    cleanup()?;
    Ok(true)
}

fn spawn_detached_purge_helper() -> Result<(), String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not locate the purge helper executable: {error}"))?;
    let mut child = std::process::Command::new(executable)
        .arg(DETACHED_PURGE_HELPER_ARG)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start the deferred purge helper: {error}"))?;
    let parent_lifetime = child
        .stdin
        .take()
        .ok_or_else(|| "deferred purge helper has no parent-lifetime pipe".to_string())?;

    // Dropping Child does not terminate it. Keep only the pipe writer alive;
    // Rust creates the parent side non-inheritable/CLOEXEC, so the helper owns
    // only the read side and sees EOF exactly when this process terminates.
    drop(child);
    std::mem::forget(parent_lifetime);
    Ok(())
}

fn complete_shutdown_action_with<G, P, S>(
    action: webui::ShutdownAction,
    engine_shutdown: EngineShutdown,
    data_lifecycle: G,
    purge_in_process: P,
    spawn_deferred_purge: S,
) -> Result<(), String>
where
    P: FnOnce() -> Result<(), String>,
    S: FnOnce() -> Result<(), String>,
{
    match engine_shutdown {
        EngineShutdown::Stopped => {
            drop(data_lifecycle);
            if action == webui::ShutdownAction::PurgeData {
                purge_in_process()
            } else {
                Ok(())
            }
        }
        EngineShutdown::Detached => {
            // Launch while the old process still owns its shared lifecycle
            // lock. Forgetting the guard is intentional: the OS releases the
            // underlying file handle only while terminating this process, so
            // no external purge can slip into the detach-to-exit window.
            let result = if action == webui::ShutdownAction::PurgeData {
                spawn_deferred_purge()
            } else {
                Ok(())
            };
            std::mem::forget(data_lifecycle);
            result
        }
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // Packaging can validate the real executable and native linkage without
    // opening devices, downloading models, or creating a user-data directory.
    if args.len() == 2 && args[1] == "--build-info" {
        println!(
            "{}",
            serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "edition": if community::ENABLED { "community" } else { "legacy" },
                "data_directory": community::DATA_DIR_NAME,
                "update_manifest": UPDATE_MANIFEST_URL,
            })
        );
        return Ok(());
    }
    if community::ENABLED && args.get(1).map(String::as_str) == Some("activate") {
        anyhow::bail!(community::ACTIVATION_NOTICE);
    }
    // A deferred purge helper must perform no ordinary startup side effects.
    // It blocks on the private stdin sentinel until its parent process is gone,
    // then uses the same lifecycle-lock transaction as the uninstaller.
    {
        let stdin = std::io::stdin();
        let mut parent_lifetime = stdin.lock();
        match run_early_detached_purge_helper_with(
            &args,
            &mut parent_lifetime,
            run_uninstall_cleanup_transaction,
        ) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => {
                show_startup_error(&format!("Could not remove VocalCode data: {error}"));
                return Err(anyhow::Error::msg(error));
            }
        }
    }
    // The uninstaller must wait for every GUI/CLI shared lifecycle guard and
    // then acquire the cleanup routine's exclusive guard itself. Route it
    // before this process starts logging, single-instance IPC, GUI state, or a
    // shared lifecycle guard; otherwise it can deadlock on its own lock.
    if run_early_uninstall_cleanup_with(&args, run_uninstall_cleanup_transaction)
        .map_err(anyhow::Error::msg)?
    {
        return Ok(());
    }
    let update_failure = update_failure_message(&args);
    let diagnostic = diagnostic_command(&args);
    #[cfg(not(feature = "diagnostic-cli"))]
    if let Some(command) = diagnostic {
        anyhow::bail!("the '{command}' test harness is not included in production builds");
    }
    let is_cli = args.get(1).map(String::as_str) == Some("activate")
        || cfg!(feature = "diagnostic-cli") && diagnostic.is_some();
    // Inno Setup's AppMutex observes this exact fixed name. It is deliberately
    // separate from per-user single-instance ownership: it exists for the full
    // normal GUI lifetime, but is never consulted to decide whether this user
    // already has a primary window. Cleanup/CLI modes must not hold it.
    #[cfg(windows)]
    let _installer_observation = if is_cli {
        None
    } else {
        match instance_platform::acquire_installer_observation(WINDOWS_INSTALLER_OBSERVATION_MUTEX)
        {
            Ok(guard) => Some(guard),
            Err(error) => {
                let message = format!("could not create installer observation mutex: {error}");
                show_startup_error(&message);
                return Err(anyhow::anyhow!(message));
            }
        }
    };
    // Protect all writable state (including migration and model installation)
    // from an uninstall/purge in another process. This guard is intentionally
    // acquired before logging and retained through every join below.
    let data_lifecycle = paths::enter_data_lifecycle()
        .map_err(|error| anyhow::anyhow!("could not lock VocalCode app data: {error}"))?;
    // Acquire before opening/rotating the shared log or touching update files.
    // CLI diagnostics do not own hooks and remain safe to run alongside the UI.
    let (show_existing, _instance_guard) = if is_cli {
        (None, None)
    } else {
        match acquire_instance(&app_dir(), INSTANCE_SCOPE) {
            Ok(InstanceState::Primary(primary)) => (Some(primary.show), Some(primary.guard)),
            Ok(InstanceState::Existing) => return Ok(()),
            Err(error) => {
                show_startup_error(&error.to_string());
                return Err(error.into());
            }
        }
    };
    init_logging();
    // If we are here after a self-update, the bundle the old version ran from is
    // still on disk — it could not delete itself. This is the first moment it
    // can go, and reaching this line is the proof the update took.
    // A diagnostic CLI never proves that a newly installed GUI bundle started
    // successfully and must not clean an updater transaction in its name.
    if !is_cli && !community::ENABLED {
        webui::clear_update_leftovers();
    }

    #[cfg(feature = "diagnostic-cli")]
    if args.get(1).map(String::as_str) == Some("punct") {
        // Test harness: `vocalcode-app punct <text...>` runs the cleaner chain.
        let text = args[2..].join(" ");
        let mut cleaners =
            models::build_cleaners(find_punct(), false).map_err(anyhow::Error::msg)?;
        let mut out = text.clone();
        for c in &mut cleaners {
            if let Ok(x) = c.clean(&out) {
                out = x;
            }
        }
        println!("in:  {text}");
        println!("out: {out}");
        return Ok(());
    }
    #[cfg(all(feature = "diagnostic-cli", target_os = "macos"))]
    if args.get(1).map(String::as_str) == Some("keyprobe") {
        vocalcode_platform::hotkey_macos::probe_keys();
        return Ok(());
    }
    #[cfg(all(feature = "diagnostic-cli", not(target_os = "macos")))]
    if args.get(1).map(String::as_str) == Some("keyprobe") {
        anyhow::bail!("keyprobe is supported only by the macOS diagnostic build");
    }
    #[cfg(feature = "diagnostic-cli")]
    if args.get(1).map(String::as_str) == Some("transcribe") {
        // Several files in one run, because the model load dominates: measuring a
        // language across a corpus was ~5 s of loading per 0.2 s of decoding, and
        // that ratio is what stops anyone from measuring.
        let paths = &args[2..];
        if paths.is_empty() {
            anyhow::bail!("usage: vocalcode-app transcribe <file.wav> [more.wav …]");
        }
        return run_transcribe(paths);
    }
    if let Some(input) = activation_input(&args).map_err(anyhow::Error::msg)? {
        return match input {
            ActivationInput::Prompt => {
                #[cfg(windows)]
                anyhow::bail!(WINDOWS_ACTIVATION_PROMPT_ERROR);
                #[cfg(not(windows))]
                {
                    let key = rpassword::prompt_password("License key: ")?;
                    let mut reader =
                        std::io::BufReader::new(std::io::Cursor::new(key.into_bytes()));
                    activation::activate_cli_from_reader(&mut reader, &app_dir())
                }
            }
            ActivationInput::Stdin => {
                let stdin = std::io::stdin();
                let mut reader = std::io::BufReader::new(stdin.lock());
                activation::activate_cli_from_reader(&mut reader, &app_dir())
            }
        };
    }

    let show_existing = show_existing.expect("GUI launch owns the instance receiver");

    let mut config = load_config().map_err(|error| {
        log::error!("startup settings: {error}");
        anyhow::anyhow!(error)
    })?;
    // The OS is the source of truth.  Older installers created a Startup
    // shortcut while Settings managed a Run value, so trusting TOML could show
    // "off" while Windows still launched the app (or vice versa).
    config.autostart = webui::autostart_enabled();
    log::info!(
        "talk = {:?}, send = {:?}, teach = {:?}",
        config.talk,
        config.send,
        config.teach
    );

    // macOS gates the global key tap and synthetic keystrokes behind TCC. Ask
    // before the engine starts, otherwise both fail silently and the app just
    // looks broken. Not fatal: the window still opens and shows what's missing.
    #[cfg(target_os = "macos")]
    let mac_permissions = macos::ensure_permissions();

    let (meeting_asr_tx, meeting_asr_rx) = mpsc::sync_channel(2);
    let meeting_runtime = match meeting::Runtime::start(&app_dir(), meeting_asr_tx) {
        Ok(runtime) => Some(runtime),
        Err(error) => {
            log::error!("{error}");
            None
        }
    };
    let status = Arc::new(RuntimeStatus::with_meetings(
        meeting_runtime
            .as_ref()
            .map(meeting::Runtime::bridge)
            .unwrap_or_default(),
    ));
    if let Some(message) = update_failure {
        *status.update_result.lock().unwrap() = Some((false, message));
    }

    // Right-click → Services → "Add to VocalCode dictionary", from inside any
    // app. The selection is parked on `status.teach`; the UI loop raises the
    // window and hands it to the page. Installed here rather than in the UI
    // thread because it only needs to happen once and failing is not fatal —
    // the in-app Teach button is the same behaviour through a door that cannot
    // be missing from a stale services cache.
    // ⌘V has nowhere to go without a menu bar — see `install_edit_menu`. Done
    // here rather than in the UI thread only because everything else macOS-shaped
    // is; it must run before the window takes a key event, which it does.
    #[cfg(target_os = "macos")]
    macos::install_edit_menu();

    #[cfg(target_os = "macos")]
    {
        let s = status.clone();
        macos::install_dictionary_service(move |text| {
            *s.teach
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(text);
        });
    }
    *status.totals.lock().unwrap() = load_totals();
    status.activity.load(&app_dir());
    // Read once here and updated live from Settings; the engine reads it per press.
    status
        .talk_latched
        .store(config.talk_mode == "toggle", Ordering::Relaxed);
    status
        .cue_sounds
        .store(config.cue_sounds, Ordering::Relaxed);
    // Render and register the cues now. Both backends need a file on disk, and
    // doing that on the first press would put a write and a decode on exactly
    // the path these are supposed to keep fast.
    vocalcode_platform::cue::init(&app_dir());
    #[cfg(target_os = "macos")]
    status
        .permissions_ok
        .store(mac_permissions.all_granted(), Ordering::Relaxed);
    #[cfg(not(target_os = "macos"))]
    status.permissions_ok.store(true, Ordering::Relaxed);
    // Show the first-run language picker until the user has explicitly chosen;
    // the background thread also blocks on this before downloading any model.
    // An upgraded install carrying the retired `language = "auto"` counts as not
    // chosen: that value used to mean per-phrase routing between both models,
    // and there is no honest value to migrate it to.
    status.onboarded.store(
        config.onboarded && !models::needs_language_pick(&config.model, &config.language),
        Ordering::Relaxed,
    );
    let lic = license_status();
    let gate = injection_allowed(&lic);
    status.inject_gate.store(gate, Ordering::Relaxed);
    status.pro_gate.store(pro_allowed(&lic), Ordering::Relaxed);
    *status.license.lock().unwrap() = license_string(&lic);
    *status.license_state.lock().unwrap() = license_state(&lic);
    let license_maintenance = start_license_maintenance(status.clone());

    let capture = Arc::new(CaptureShared::default());
    let triggers: SharedTriggers = Arc::new(std::sync::Mutex::new((
        config.talk.clone(),
        config.send.clone(),
        config.teach.clone(),
    )));
    let shared_config = Arc::new(std::sync::Mutex::new(config));
    let overlay_state = overlay::OverlayState::default();
    // VOCALCODE_OVERLAY_DEMO=1 cycles the indicator through its phases so it can
    // be looked at without holding the talk key — the only way to check the
    // visual without a person and a microphone.
    let overlay_demo = if std::env::var("VOCALCODE_OVERLAY_DEMO").as_deref() == Ok("1") {
        let demo = overlay_state.clone();
        let demo_status = status.clone();
        Some(thread::spawn(move || loop {
            demo.set(overlay::Phase::Recording);
            if !shutdown_aware_pause(&demo_status, Duration::from_secs(4)) {
                return;
            }
            demo.set(overlay::Phase::Transcribing);
            if !shutdown_aware_pause(&demo_status, Duration::from_millis(1500)) {
                return;
            }
            demo.set(overlay::Phase::Idle);
            if !shutdown_aware_pause(&demo_status, Duration::from_millis(800)) {
                return;
            }
        }))
    } else {
        None
    };
    let (level_tx, level_rx) = mpsc::sync_channel(1);
    let background = start_background(
        shared_config.clone(),
        status.clone(),
        capture.clone(),
        triggers.clone(),
        overlay_state.clone(),
        level_tx,
        meeting_asr_rx,
    );
    let update_maintenance = start_update_maintenance(status.clone())
        .map_err(|error| anyhow::anyhow!("could not start update maintenance: {error}"))?;

    // Permissions can be granted while the app is running, so keep checking and
    // let the banner clear itself rather than making the user guess. Cheap
    // calls, but no need to run them at the UI's frame rate.
    #[cfg(target_os = "macos")]
    let permission_maintenance = {
        let status = status.clone();
        Some(thread::spawn(move || loop {
            for _ in 0..20 {
                if status.shutdown.load(Ordering::Acquire) {
                    return;
                }
                thread::sleep(std::time::Duration::from_millis(100));
            }
            status
                .permissions_ok
                .store(macos::check().all_granted(), Ordering::Relaxed);
        }))
    };
    #[cfg(not(target_os = "macos"))]
    let permission_maintenance: Option<thread::JoinHandle<()>> = None;

    // Licence state the UI refreshes to after a successful in-app activation:
    // the English sentence for logs, and the parts the window translates.
    let refresh: Arc<dyn Fn() -> (String, (String, u32)) + Send + Sync> = Arc::new(|| {
        let l = license_status();
        (license_string(&l), license_state(&l))
    });
    // The capture device is opened on the engine thread, so wait briefly for it
    // to hand back the level meter. A default meter (always silent) is a fine
    // fallback: the indicator still shows, just without moving bars.
    let audio_level = level_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap_or_default();
    let ui_result = webui::run(
        shared_config,
        status.clone(),
        app_dir(),
        refresh,
        capture,
        overlay_state,
        audio_level,
        show_existing,
    );

    // Meeting workers may still be waiting for the engine to finish one native
    // ASR call, so stop/join them while that single model owner is still alive.
    if let Some(runtime) = meeting_runtime {
        runtime.shutdown();
    }
    // The UI no longer owns any native window/clipboard interaction. Stop the
    // engine (which closes audio and drops ONNX), then every joinable task that
    // can still touch app data, and only then honor purge/restart/quit.
    status.shutdown.store(true, Ordering::Release);
    let engine_shutdown = background.shutdown(&status);
    let update_worker = status
        .update_worker
        .lock()
        .map(|mut worker| worker.take())
        .unwrap_or_else(|poisoned| poisoned.into_inner().take());
    if let Some(worker) = update_worker {
        if worker.join().is_err() {
            log::error!("update worker panicked during shutdown");
        }
    }
    // Stop the periodic producer before draining the tracked check workers; no
    // new check can appear between the drain and process exit.
    if update_maintenance.join().is_err() {
        log::error!("update maintenance panicked during shutdown");
    }
    webui::join_service_workers(&status);
    if license_maintenance.join().is_err() {
        log::error!("license maintenance panicked during shutdown");
    }
    if let Some(worker) = permission_maintenance {
        if worker.join().is_err() {
            log::error!("permission maintenance panicked during shutdown");
        }
    }
    if let Some(worker) = overlay_demo {
        if worker.join().is_err() {
            log::error!("overlay demo panicked during shutdown");
        }
    }

    let requested_action = status
        .shutdown_action
        .lock()
        .map(|action| *action)
        .unwrap_or(webui::ShutdownAction::Quit);
    // A UI failure retains the old no-purge behaviour. We still pass through
    // the lifecycle finalizer so a detached engine keeps the shared lock until
    // OS process termination rather than exposing a last-millisecond race.
    let action = if ui_result.is_ok() {
        requested_action
    } else {
        webui::ShutdownAction::Quit
    };
    let cleanup_result = complete_shutdown_action_with(
        action,
        engine_shutdown,
        data_lifecycle,
        webui::purge_user_data_after_shutdown,
        spawn_detached_purge_helper,
    );
    ui_result?;
    if let Err(error) = cleanup_result {
        show_startup_error(&format!("Could not remove VocalCode data: {error}"));
        return Err(anyhow::anyhow!("could not remove VocalCode data: {error}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vocalcode_core::traits::TriggerId;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn filler_review_is_session_only_bounded_and_never_attaches_to_other_text() {
        let status = RuntimeStatus::default();
        let mut trace = vocalcode_core::engine::DictationTrace {
            raw_text: "uh retry".into(),
            final_text: "Retry".into(),
            filler_removed: 1,
            result: "delivery_completed".into(),
            ..Default::default()
        };
        status
            .history
            .lock()
            .unwrap()
            .push(webui::HistoryEntry::new(1, "unrelated".into()));
        publish_filler_review(&status, &trace);
        assert!(status.history.lock().unwrap()[0].recognition.is_none());
        status.history.lock().unwrap()[0].text = "Retry".into();
        publish_filler_review(&status, &trace);
        assert_eq!(
            status.history.lock().unwrap()[0].recognition.as_deref(),
            Some("uh retry")
        );
        assert!(!status.workflows.lock().unwrap().diagnostics);
        trace.final_text.clear();
        for _ in 0..55 {
            publish_filler_review(&status, &trace);
        }
        assert_eq!(status.history.lock().unwrap().len(), 50);
        assert!(status.history.lock().unwrap()[0].text.is_empty());
        trace.raw_text_truncated = true;
        assert!(webui::HistoryEntry::new(1, "Retry".into())
            .with_trace(&trace)
            .recognition
            .is_none());
    }

    #[test]
    fn meeting_decode_never_takes_priority_over_dictation() {
        assert!(!meeting_asr_may_run(true, false, MEETING_ASR_INPUT_GRACE));
        assert!(!meeting_asr_may_run(false, true, MEETING_ASR_INPUT_GRACE));
        assert!(!meeting_asr_may_run(
            false,
            false,
            MEETING_ASR_INPUT_GRACE - Duration::from_millis(1)
        ));
        assert!(meeting_asr_may_run(false, false, MEETING_ASR_INPUT_GRACE));
    }

    struct ScratchDirectory(PathBuf);

    impl ScratchDirectory {
        fn new(label: &str) -> Self {
            let nonce = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "vocalcode-main-{label}-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn new_for_instance(label: &str) -> Self {
            #[cfg(unix)]
            {
                let nonce = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
                let path = Path::new("/tmp").join(format!(
                    "vc-instance-{label}-{}-{nonce}",
                    std::process::id()
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
            #[cfg(not(unix))]
            {
                Self::new(label)
            }
        }
    }

    impl Drop for ScratchDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn hold_test_file_lock(path: &Path) -> std::fs::File {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&file).unwrap();
        file
    }

    fn assert_lock_wait_was_bounded(started: Instant, minimum: Duration) {
        let elapsed = started.elapsed();
        assert!(
            elapsed >= minimum,
            "contended lock returned too early after {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "bounded lock wait took {elapsed:?}"
        );
    }

    #[test]
    fn dictionary_write_lock_has_a_hard_deadline_and_recovers() {
        let scratch = ScratchDirectory::new("rules-lock-deadline");
        let rules_path = scratch.path().join("replacements.txt");
        let holder = hold_test_file_lock(&scratch.path().join(".vocalcode-rules-write.lock"));

        let wait = Duration::from_millis(120);
        let started = Instant::now();
        let error = lock_rules_writes_until(&rules_path, started + wait)
            .err()
            .expect("a held dictionary lock must time out");
        assert_lock_wait_was_bounded(started, Duration::from_millis(80));
        assert!(error.contains("dictionary write is busy"), "{error}");
        assert!(error.contains("timed out"), "{error}");

        fs2::FileExt::unlock(&holder).unwrap();
        drop(holder);
        let recovered =
            lock_rules_writes_until(&rules_path, Instant::now() + Duration::from_secs(1)).unwrap();
        drop(recovered);
    }

    #[test]
    fn settings_write_lock_has_a_hard_deadline_and_recovers() {
        let scratch = ScratchDirectory::new("config-lock-deadline");
        let config_path = scratch.path().join("vocalcode.toml");
        let holder = hold_test_file_lock(&scratch.path().join(".vocalcode-config-write.lock"));

        let wait = Duration::from_millis(120);
        let started = Instant::now();
        let error = lock_config_writes_until(&config_path, started + wait)
            .err()
            .expect("a held settings lock must time out");
        assert_lock_wait_was_bounded(started, Duration::from_millis(80));
        assert!(error.contains("settings write is busy"), "{error}");
        assert!(error.contains("timed out"), "{error}");

        fs2::FileExt::unlock(&holder).unwrap();
        drop(holder);
        let recovered =
            lock_config_writes_until(&config_path, Instant::now() + Duration::from_secs(1))
                .unwrap();
        drop(recovered);
    }

    #[test]
    fn contended_log_lock_disables_only_file_logging_and_recovers() {
        let scratch = ScratchDirectory::new("log-lock-deadline");
        let sink = LogFileSink::new(scratch.path(), 128);
        let holder = hold_test_file_lock(&sink.lock);

        let wait = Duration::from_millis(120);
        let started = Instant::now();
        let error = lock_log_file_until(&sink.lock, started + wait)
            .expect_err("a held log lock must time out");
        assert_lock_wait_was_bounded(started, Duration::from_millis(80));
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("log write lock"));

        // The production logger gets its own short bounded wait, disables only
        // its file copy, and still reports successful stderr writes.
        let mut tee = LogTee(sink);
        let message = b"log lock contention still reaches stderr\n";
        let written = std::io::Write::write(&mut tee, message).unwrap();
        assert_eq!(written, message.len());
        assert!(!tee.0.enabled);
        assert!(!tee.0.active.exists());
        assert_eq!(
            std::io::Write::write(&mut tee, b"stderr remains available\n").unwrap(),
            b"stderr remains available\n".len()
        );

        fs2::FileExt::unlock(&holder).unwrap();
        drop(holder);
        let recovered =
            lock_log_file_until(&tee.0.lock, Instant::now() + Duration::from_secs(1)).unwrap();
        drop(recovered);
        assert!(!tee.0.enabled, "a failed file sink stays safely disabled");
    }

    #[test]
    fn an_expired_deadline_never_acquires_or_leaks_a_file_lock() {
        let scratch = ScratchDirectory::new("expired-file-lock");
        let path = scratch.path().join("lock");
        let first = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        let error = try_lock_exclusive_until(&first, Instant::now(), "test lock").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);

        let second = hold_test_file_lock(&path);
        fs2::FileExt::unlock(&second).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_lock_violation_is_classified_as_contention() {
        assert!(file_lock_is_contended(&std::io::Error::from_raw_os_error(
            33
        )));
    }

    fn recovery_files(directory: &Path, source_name: &str) -> Vec<PathBuf> {
        let prefix = format!("{source_name}.invalid-");
        let mut files: Vec<_> = std::fs::read_dir(directory)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
            })
            .map(|path| {
                if path.is_dir() {
                    path.join("contents")
                } else {
                    path
                }
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn missing_config_and_rules_create_defaults() {
        let scratch = ScratchDirectory::new("missing-user-files");
        let config_path = scratch.path().join("vocalcode.toml");
        let rules_path = scratch.path().join("replacements.txt");

        let config = load_config_from(&config_path).unwrap();
        let persisted_config: Config =
            toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(persisted_config.model, config.model);

        // The user file now ships empty of rules; the built-in "vocal code"
        // correction lives in the separate default layer instead.
        let rules = load_rules_from(&rules_path).unwrap();
        assert!(rules.is_empty(), "user rules start empty: {rules:?}");
        assert!(merge_rules(&rules)
            .iter()
            .any(|(from, to)| from == "vocal code" && to == "VocalCode"));
        assert_eq!(
            std::fs::read_to_string(&rules_path).unwrap(),
            RULES_TEMPLATE
        );
    }

    #[test]
    fn built_in_defaults_are_separate_and_overridable() {
        let defaults = default_rules();
        assert!(
            defaults.len() >= 40,
            "default pack looks empty: {}",
            defaults.len()
        );
        assert!(defaults.len() <= vocalcode_core::limits::MAX_DICTIONARY_RULES);
        assert!(defaults
            .iter()
            .any(|(f, t)| f == "cloud code" && t == "Claude Code"));

        // A user rule overrides a built-in by heard-phrase key (no duplicate).
        let merged = merge_rules(&[("cloud code".to_string(), "cloud code the CLI".to_string())]);
        let cloud: Vec<_> = merged
            .iter()
            .filter(|(f, _)| f.eq_ignore_ascii_case("cloud code"))
            .collect();
        assert_eq!(cloud.len(), 1, "override must not duplicate the key");
        assert_eq!(cloud[0].1, "cloud code the CLI");

        // Mapping a built-in phrase to itself disables it.
        let off = merge_rules(&[("versal".to_string(), "versal".to_string())]);
        assert!(off.iter().any(|(f, t)| f == "versal" && t == "versal"));
        assert!(!off.iter().any(|(f, t)| f == "versal" && t == "Vercel"));

        // Non-overridden built-ins remain present alongside the user's rules.
        assert!(merged.iter().any(|(f, t)| f == "git lab" && t == "GitLab"));
    }

    #[test]
    fn merge_never_exceeds_the_engine_rule_cap() {
        let cap = vocalcode_core::limits::MAX_DICTIONARY_RULES;
        let user: Vec<(String, String)> = (0..cap)
            .map(|i| (format!("phrase {i}"), format!("Repl{i}")))
            .collect();
        let merged = merge_rules(&user);
        assert!(merged.len() <= cap);
        assert!(merged.iter().any(|(f, _)| f == "phrase 0"));
    }

    #[test]
    fn bounded_reader_accepts_the_exact_limit_and_rejects_one_byte_more() {
        let scratch = ScratchDirectory::new("bounded-control-read");
        let path = scratch.path().join("control.dat");
        std::fs::write(&path, vec![b'x'; 64]).unwrap();
        assert_eq!(read_bounded_bytes(&path, 64).unwrap().len(), 64);

        std::fs::write(&path, vec![b'x'; 65]).unwrap();
        let error = read_bounded_bytes(&path, 64).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn oversized_control_documents_fail_closed_without_recovery_or_rewrite() {
        let scratch = ScratchDirectory::new("oversized-control-files");
        let cases = [
            ("vocalcode.toml", MAX_CONFIG_DOCUMENT_BYTES),
            ("replacements.txt", MAX_DICTIONARY_DOCUMENT_BYTES),
            ("totals.json", MAX_TOTALS_DOCUMENT_BYTES),
        ];

        for (name, maximum) in cases {
            let path = scratch.path().join(name);
            let source = vec![b'x'; maximum + 1];
            std::fs::write(&path, &source).unwrap();
            let error = match name {
                "vocalcode.toml" => {
                    load_config_from_with(&path, |_path, _original, _replacement| {
                        panic!("oversized config must not recover")
                    })
                    .unwrap_err()
                }
                "replacements.txt" => load_rules_from(&path).unwrap_err(),
                "totals.json" => load_totals_from_with(&path, |_path, _original, _replacement| {
                    panic!("oversized totals must not recover")
                })
                .unwrap_err(),
                _ => unreachable!(),
            };
            assert!(error.contains("safety limit"), "{name}: {error}");
            assert_eq!(std::fs::read(&path).unwrap(), source, "{name} changed");
            assert!(recovery_files(scratch.path(), name).is_empty());
        }
    }

    #[test]
    fn dictionary_load_and_save_reject_count_and_phrase_bounds() {
        let too_many = (0..=MAX_DICTIONARY_RULES)
            .map(|index| format!("heard-{index} => written-{index}\n"))
            .collect::<String>();
        let error =
            rules_document_from_bytes(Path::new("replacements.txt"), too_many.as_bytes().to_vec())
                .unwrap_err();
        assert!(error.contains("more than"), "{error}");

        let oversized_side = format!(
            "{} => written\n",
            "x".repeat(MAX_DICTIONARY_SIDE_UTF8_BYTES + 1)
        );
        let error = rules_document_from_bytes(
            Path::new("replacements.txt"),
            oversized_side.as_bytes().to_vec(),
        )
        .unwrap_err();
        assert!(error.contains("heard phrase"), "{error}");

        let lines = (0..=MAX_DICTIONARY_RULES)
            .map(|index| format!("heard-{index} => written-{index}"))
            .collect::<Vec<_>>();
        assert!(normalize_rule_lines(&lines)
            .unwrap_err()
            .contains("at most"));
    }

    #[test]
    fn startup_migrates_valid_old_config_only_in_memory() {
        let scratch = ScratchDirectory::new("in-memory-config-migration");
        let path = scratch.path().join("vocalcode.toml");
        let old = Config {
            config_version: 0,
            paste_insert: true,
            onboarded: false,
            ..Config::default()
        };
        let source = toml::to_string_pretty(&old).unwrap();
        std::fs::write(&path, &source).unwrap();

        let loaded = load_config_from(&path).unwrap();

        assert_eq!(
            loaded.config_version,
            vocalcode_core::config::CONFIG_VERSION
        );
        assert!(!loaded.paste_insert);
        assert!(loaded.onboarded);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            source,
            "startup must not write a migrated snapshot over a concurrent edit"
        );
    }

    #[test]
    fn future_config_version_with_unknown_fields_fails_closed_without_touching_bytes() {
        let scratch = ScratchDirectory::new("future-config-version");
        let path = scratch.path().join("vocalcode.toml");
        let future = vocalcode_core::config::CONFIG_VERSION + 1;
        let source = format!(
            "# written by a future release\nconfig_version = {future}\ntalk = \"new-schema\"\nfuture_voice_router = {{ mode = \"semantic\" }}\n"
        );
        std::fs::write(&path, &source).unwrap();

        let error = load_config_from_with(&path, |_path, _source, _replacement| {
            panic!("a valid future config must not be moved into invalid recovery")
        })
        .unwrap_err();

        assert!(error.contains("newer VocalCode config version"), "{error}");
        assert!(error.contains(&future.to_string()), "{error}");
        assert!(error.contains("left byte-for-byte unchanged"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        assert!(recovery_files(scratch.path(), "vocalcode.toml").is_empty());
    }

    #[test]
    fn settings_save_refuses_a_future_file_that_appeared_after_load() {
        let scratch = ScratchDirectory::new("future-config-save-race");
        let path = scratch.path().join("vocalcode.toml");
        let expected = Config::default();
        std::fs::write(&path, toml::to_string_pretty(&expected).unwrap()).unwrap();

        let future = vocalcode_core::config::CONFIG_VERSION + 1;
        let source = format!("config_version = {future}\ntalk = \"new-schema\"\n");
        std::fs::write(&path, &source).unwrap();
        let mut replacement = expected.clone();
        replacement.live_caption = !expected.live_caption;

        let error = persist_config_if_current(&path, &expected, &replacement).unwrap_err();
        assert!(error.contains("newer VocalCode config version"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    }

    #[test]
    fn settings_save_refuses_a_compatible_external_edit() {
        let scratch = ScratchDirectory::new("compatible-config-save-race");
        let path = scratch.path().join("vocalcode.toml");
        let expected = Config::default();
        std::fs::write(&path, toml::to_string_pretty(&expected).unwrap()).unwrap();
        let mut external = expected.clone();
        external.cue_sounds = !expected.cue_sounds;
        external.autostart = !expected.autostart;
        let external_source = toml::to_string_pretty(&external).unwrap();
        std::fs::write(&path, &external_source).unwrap();
        let mut replacement = expected.clone();
        replacement.live_caption = !expected.live_caption;

        let error = persist_config_if_current(&path, &expected, &replacement).unwrap_err();
        assert!(error.contains("changed outside this window"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), external_source);
    }

    #[test]
    fn settings_save_accepts_os_authoritative_autostart_over_stale_toml() {
        let scratch = ScratchDirectory::new("autostart-config-reconcile");
        let path = scratch.path().join("vocalcode.toml");
        let disk = Config::default();
        std::fs::write(&path, toml::to_string_pretty(&disk).unwrap()).unwrap();

        // Startup performs this exact normalization after reading the file.
        let mut expected = disk.clone();
        expected.autostart = !disk.autostart;
        let mut replacement = expected.clone();
        replacement.live_caption = !expected.live_caption;

        persist_config_if_current(&path, &expected, &replacement).unwrap();
        let durable: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(durable.autostart, expected.autostart);
        assert_eq!(durable.live_caption, replacement.live_caption);
    }

    #[test]
    fn production_classifies_every_developer_harness_before_gui_startup() {
        for command in ["punct", "keyprobe", "transcribe"] {
            let args = vec!["VocalCode".to_string(), command.to_string()];
            assert_eq!(diagnostic_command(&args), Some(command));
        }
        let activate = vec!["VocalCode".to_string(), "activate".to_string()];
        assert_eq!(diagnostic_command(&activate), None);
    }

    #[test]
    fn uninstall_cleanup_accepts_only_the_exact_internal_argument() {
        let calls = std::cell::Cell::new(0);
        let exact = vec!["VocalCode".to_string(), UNINSTALL_CLEANUP_ARG.to_string()];
        assert!(run_early_uninstall_cleanup_with(&exact, || {
            calls.set(calls.get() + 1);
            Ok(())
        })
        .unwrap());
        assert_eq!(calls.get(), 1);

        for rejected in [
            vec![
                "VocalCode".to_string(),
                UNINSTALL_CLEANUP_ARG.to_string(),
                "extra".to_string(),
            ],
            vec![
                "VocalCode".to_string(),
                "activate".to_string(),
                UNINSTALL_CLEANUP_ARG.to_string(),
            ],
        ] {
            assert!(run_early_uninstall_cleanup_with(&rejected, || {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .is_err());
        }
        assert_eq!(calls.get(), 1, "rejected shapes must never clean data");

        let ordinary = vec!["VocalCode".to_string(), "activate".to_string()];
        assert!(!run_early_uninstall_cleanup_with(&ordinary, || {
            calls.set(calls.get() + 1);
            Ok(())
        })
        .unwrap());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn uninstall_cleanup_failure_propagates_for_a_nonzero_process_exit() {
        let args = vec!["VocalCode".to_string(), UNINSTALL_CLEANUP_ARG.to_string()];
        let error =
            run_early_uninstall_cleanup_with(&args, || Err("exclusive cleanup failed".to_string()))
                .unwrap_err();
        assert_eq!(error, "exclusive cleanup failed");
    }

    #[test]
    fn detached_purge_helper_waits_for_parent_eof_before_deleting() {
        struct ParentLifetimeReader {
            entered: Option<mpsc::Sender<()>>,
            exited: mpsc::Receiver<()>,
        }

        impl Read for ParentLifetimeReader {
            fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
                if let Some(entered) = self.entered.take() {
                    entered.send(()).unwrap();
                }
                self.exited.recv().map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "test parent lifetime channel disconnected",
                    )
                })?;
                Ok(0)
            }
        }

        let scratch = ScratchDirectory::new("deferred-purge-parent-exit");
        let marker = scratch.path().join("must-survive-parent.txt");
        std::fs::write(&marker, b"still owned by parent").unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (exited_tx, exited_rx) = mpsc::channel();
        let helper_marker = marker.clone();
        let helper = thread::spawn(move || {
            let args = vec![
                "VocalCode".to_string(),
                DETACHED_PURGE_HELPER_ARG.to_string(),
            ];
            let mut parent_lifetime = ParentLifetimeReader {
                entered: Some(entered_tx),
                exited: exited_rx,
            };
            run_early_detached_purge_helper_with(&args, &mut parent_lifetime, || {
                std::fs::remove_file(&helper_marker).map_err(|error| error.to_string())
            })
        });

        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("helper did not begin waiting for its parent");
        assert!(
            marker.exists(),
            "helper deleted data while its parent was still alive"
        );
        exited_tx.send(()).unwrap();
        assert!(helper.join().unwrap().unwrap());
        assert!(
            !marker.exists(),
            "helper did not delete after observing parent EOF"
        );
    }

    #[test]
    fn detached_purge_helper_fails_closed_on_control_data_or_extra_arguments() {
        let cleanup_calls = std::cell::Cell::new(0);
        let exact = vec![
            "VocalCode".to_string(),
            DETACHED_PURGE_HELPER_ARG.to_string(),
        ];
        let error =
            run_early_detached_purge_helper_with(&exact, &mut std::io::Cursor::new([1_u8]), || {
                cleanup_calls.set(cleanup_calls.get() + 1);
                Ok(())
            })
            .unwrap_err();
        assert!(error.contains("unexpected parent control data"));

        let extra = vec![
            "VocalCode".to_string(),
            DETACHED_PURGE_HELPER_ARG.to_string(),
            "extra".to_string(),
        ];
        assert!(run_early_detached_purge_helper_with(
            &extra,
            &mut std::io::Cursor::new(Vec::<u8>::new()),
            || {
                cleanup_calls.set(cleanup_calls.get() + 1);
                Ok(())
            },
        )
        .is_err());
        assert_eq!(cleanup_calls.get(), 0);
    }

    #[test]
    fn detached_engine_never_purges_inside_the_old_process() {
        struct LifecycleProbe<'a>(&'a std::cell::Cell<u32>);
        impl Drop for LifecycleProbe<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let in_process = std::cell::Cell::new(0);
        let deferred = std::cell::Cell::new(0);
        let detached_lifecycle_drops = std::cell::Cell::new(0);
        complete_shutdown_action_with(
            webui::ShutdownAction::PurgeData,
            EngineShutdown::Detached,
            LifecycleProbe(&detached_lifecycle_drops),
            || {
                in_process.set(in_process.get() + 1);
                Ok(())
            },
            || {
                assert_eq!(
                    detached_lifecycle_drops.get(),
                    0,
                    "old process released its lifecycle lock before helper launch"
                );
                deferred.set(deferred.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(in_process.get(), 0);
        assert_eq!(deferred.get(), 1);
        assert_eq!(
            detached_lifecycle_drops.get(),
            0,
            "detached lifecycle guard must survive until OS process teardown"
        );

        let stopped_in_process = std::cell::Cell::new(0);
        let stopped_deferred = std::cell::Cell::new(0);
        let stopped_lifecycle_drops = std::cell::Cell::new(0);
        complete_shutdown_action_with(
            webui::ShutdownAction::PurgeData,
            EngineShutdown::Stopped,
            LifecycleProbe(&stopped_lifecycle_drops),
            || {
                assert_eq!(
                    stopped_lifecycle_drops.get(),
                    1,
                    "stopped engine must release its lifecycle lock before inline purge"
                );
                stopped_in_process.set(stopped_in_process.get() + 1);
                Ok(())
            },
            || {
                stopped_deferred.set(stopped_deferred.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(stopped_in_process.get(), 1);
        assert_eq!(stopped_deferred.get(), 0);
        assert_eq!(stopped_lifecycle_drops.get(), 1);

        let failed_spawn_lifecycle_drops = std::cell::Cell::new(0);
        let failed_spawn_inline_purges = std::cell::Cell::new(0);
        let error = complete_shutdown_action_with(
            webui::ShutdownAction::PurgeData,
            EngineShutdown::Detached,
            LifecycleProbe(&failed_spawn_lifecycle_drops),
            || {
                failed_spawn_inline_purges.set(failed_spawn_inline_purges.get() + 1);
                Ok(())
            },
            || Err("helper spawn failed".to_string()),
        )
        .unwrap_err();
        assert_eq!(error, "helper spawn failed");
        assert_eq!(failed_spawn_inline_purges.get(), 0);
        assert_eq!(
            failed_spawn_lifecycle_drops.get(),
            0,
            "spawn failure must retain both data and the old process lock until exit"
        );
    }

    #[test]
    fn detached_purge_pipe_tracks_real_parent_process_exit() {
        const ROLE: &str = "VOCALCODE_PURGE_PIPE_TEST_ROLE";
        const MARKER: &str = "VOCALCODE_PURGE_PIPE_TEST_MARKER";
        const READY: &str = "VOCALCODE_PURGE_PIPE_TEST_READY";
        const TEST_NAME: &str = "tests::detached_purge_pipe_tracks_real_parent_process_exit";

        match std::env::var(ROLE).as_deref() {
            Ok("helper") => {
                let marker = PathBuf::from(std::env::var_os(MARKER).unwrap());
                let (finished_tx, finished_rx) = mpsc::channel();
                thread::spawn(move || {
                    let stdin = std::io::stdin();
                    let result = wait_for_parent_process_exit(&mut stdin.lock());
                    let _ = finished_tx.send(result);
                });
                finished_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("helper's stdin never reached EOF after its real parent exited")
                    .unwrap();
                std::fs::remove_file(marker).unwrap();
                return;
            }
            Ok("parent") => {
                let executable = std::env::current_exe().unwrap();
                let marker = PathBuf::from(std::env::var_os(MARKER).unwrap());
                let ready = PathBuf::from(std::env::var_os(READY).unwrap());
                let mut helper = std::process::Command::new(executable)
                    .args(["--exact", TEST_NAME, "--nocapture"])
                    .env(ROLE, "helper")
                    .env(MARKER, &marker)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap();
                let parent_lifetime = helper.stdin.take().unwrap();
                drop(helper);
                std::mem::forget(parent_lifetime);
                std::fs::write(&ready, b"helper spawned").unwrap();
                thread::sleep(Duration::from_millis(300));
                assert!(
                    marker.exists(),
                    "helper inherited the pipe writer or purged before parent exit"
                );
                // The forgotten writer remains live through this test and the
                // test harness teardown. The OS closes it when this subprocess
                // actually exits; only then may the helper remove the marker.
                return;
            }
            Ok(other) => panic!("unknown detached purge subprocess role {other}"),
            Err(_) => {}
        }

        let scratch = ScratchDirectory::new("real-parent-exit-pipe");
        let marker = scratch.path().join("protected-data");
        let ready = scratch.path().join("parent-ready");
        std::fs::write(&marker, b"must survive while parent is alive").unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut parent = std::process::Command::new(executable)
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(ROLE, "parent")
            .env(MARKER, &marker)
            .env(READY, &ready)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        let ready_deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < ready_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            ready.exists(),
            "parent subprocess did not launch its helper"
        );
        assert!(
            marker.exists(),
            "helper removed protected data while its real parent was alive"
        );
        assert!(parent.wait().unwrap().success());

        let purge_deadline = Instant::now() + Duration::from_secs(5);
        while marker.exists() && Instant::now() < purge_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !marker.exists(),
            "helper did not observe EOF and purge after its real parent exited"
        );
    }

    #[test]
    fn background_shutdown_reports_stopped_and_detached_explicitly() {
        let status = RuntimeStatus::default();
        let (events, _events_rx) = trigger_event_channel();
        let (done_tx, done_rx) = mpsc::channel();
        let stopped = BackgroundRuntime {
            events,
            input_shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            hotkey: thread::spawn(|| {}),
            engine: thread::spawn(move || drop(done_tx)),
            engine_done: done_rx,
        }
        .shutdown_with_timeout(&status, Duration::from_secs(1));
        assert_eq!(stopped, EngineShutdown::Stopped);

        let status = RuntimeStatus::default();
        let (events, _events_rx) = trigger_event_channel();
        let (done_tx, done_rx) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (exited_tx, exited_rx) = mpsc::channel();
        let engine = thread::spawn(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            drop(done_tx);
            exited_tx.send(()).unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let detached = BackgroundRuntime {
            events,
            input_shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            hotkey: thread::spawn(|| {}),
            engine,
            engine_done: done_rx,
        }
        .shutdown_with_timeout(&status, Duration::from_millis(5));
        assert_eq!(detached, EngineShutdown::Detached);
        release_tx.send(()).unwrap();
        exited_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("detached test engine did not exit after release");
    }

    #[test]
    fn uninstall_cleanup_releases_migration_guard_before_exclusive_purge() {
        struct MigrationGuard<'a>(&'a std::cell::RefCell<Vec<&'static str>>);
        impl Drop for MigrationGuard<'_> {
            fn drop(&mut self) {
                self.0.borrow_mut().push("release migration");
            }
        }

        let calls = std::cell::RefCell::new(Vec::new());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        run_uninstall_cleanup_transaction_with(
            deadline,
            |observed_deadline| {
                assert_eq!(observed_deadline, deadline);
                calls.borrow_mut().push("migrate");
                Ok(MigrationGuard(&calls))
            },
            |observed_deadline| {
                assert_eq!(observed_deadline, deadline);
                calls.borrow_mut().push("exclusive purge");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            *calls.borrow(),
            ["migrate", "release migration", "exclusive purge"]
        );
    }

    #[test]
    fn uninstall_cleanup_dispatch_precedes_every_normal_startup_side_effect() {
        let source = include_str!("main.rs");
        let main = &source[source.find("fn main() -> anyhow::Result<()>").unwrap()..];
        let detached_dispatch = main.find("run_early_detached_purge_helper_with").unwrap();
        let dispatch = main.find("run_early_uninstall_cleanup_with").unwrap();
        assert!(detached_dispatch < dispatch);
        assert!(main[dispatch..].contains("run_uninstall_cleanup_transaction"));
        for later in [
            "acquire_installer_observation",
            "paths::enter_data_lifecycle",
            "acquire_instance(&app_dir(), INSTANCE_SCOPE)",
            "init_logging()",
            "webui::run(",
        ] {
            assert!(
                detached_dispatch < main.find(later).unwrap()
                    && dispatch < main.find(later).unwrap(),
                "{later} ran too early"
            );
        }

        let helper_start = source
            .find("fn run_uninstall_cleanup_transaction_with")
            .unwrap();
        let transaction_start = source[helper_start..]
            .find("fn run_uninstall_cleanup_transaction()")
            .unwrap()
            + helper_start;
        let helper = &source[helper_start..transaction_start];
        let migrate = helper.find("let migration = migrate(deadline)").unwrap();
        let release = helper.find("drop(migration)").unwrap();
        let purge = helper.find("purge(deadline)").unwrap();
        assert!(migrate < release && release < purge);

        let transaction = &source[transaction_start..source.find("fn main()").unwrap()];
        let migrate = transaction.find("paths::enter_data_lifecycle").unwrap();
        let purge = transaction
            .find("webui::purge_user_data_after_shutdown")
            .unwrap();
        assert!(migrate < purge);
    }

    #[test]
    fn missing_config_adopts_a_concurrently_created_file() {
        let scratch = ScratchDirectory::new("config-create-race");
        let path = scratch.path().join("vocalcode.toml");
        let concurrent = Config {
            model: "small.en".to_string(),
            language: "en".to_string(),
            ..Config::default()
        };
        let source = toml::to_string_pretty(&concurrent).unwrap();

        let loaded = load_config_from_with_publish(
            &path,
            |_path, _original, _replacement| panic!("missing config is not recovered"),
            |publish_path, _defaults| {
                std::fs::write(publish_path, &source)?;
                Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "simulated concurrent settings publish",
                ))
            },
        )
        .unwrap();

        assert_eq!(loaded.model, concurrent.model);
        assert_eq!(loaded.language, concurrent.language);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    }

    #[test]
    fn missing_rules_adopt_a_concurrently_created_dictionary() {
        let scratch = ScratchDirectory::new("rules-create-race");
        let path = scratch.path().join("replacements.txt");
        let concurrent = "wire less => wireless\n";

        let loaded = load_rules_from_with(&path, |publish_path, _template| {
            std::fs::write(publish_path, concurrent)?;
            Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "simulated concurrent dictionary publish",
            ))
        })
        .unwrap();

        assert_eq!(
            loaded,
            vec![("wire less".to_string(), "wireless".to_string())]
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), concurrent);
    }

    #[test]
    fn dictionary_revision_hashes_the_exact_bom_eol_and_trailing_bytes() {
        let plain = RulesRevision::from_bytes(b"old => value\n");
        let crlf = RulesRevision::from_bytes(b"old => value\r\n");
        let no_final_eol = RulesRevision::from_bytes(b"old => value");
        let bom = RulesRevision::from_bytes(b"\xef\xbb\xbfold => value\n");

        assert_ne!(plain, crlf);
        assert_ne!(plain, no_final_eol);
        assert_ne!(plain, bom);
        assert_eq!(
            RulesRevision::parse(plain.as_str()).unwrap(),
            plain,
            "the WebView revision token must round-trip without normalization"
        );
        assert!(RulesRevision::parse(&"A".repeat(64)).is_err());
    }

    #[test]
    fn dictionary_save_preserves_every_non_rule_byte_and_line_ending() {
        let scratch = ScratchDirectory::new("rules-preserve-document");
        let path = scratch.path().join("replacements.txt");
        let source = "\u{feff}old => value\r\n# hand-written header\r\n\r\nnot a rule\r\nempty => \r\n# tail has no newline";
        std::fs::write(&path, source.as_bytes()).unwrap();
        let loaded = load_rules_document_from(&path).unwrap();
        assert_eq!(loaded.rules, vec![("old".to_string(), "value".to_string())]);

        let saved = save_rules_document_if_current(
            &path,
            &loaded.revision,
            &["new => replacement".to_string()],
        )
        .unwrap();
        let expected = "\u{feff}new => replacement\r\n# hand-written header\r\n\r\nnot a rule\r\nempty => \r\n# tail has no newline";

        assert_eq!(std::fs::read(&path).unwrap(), expected.as_bytes());
        assert_eq!(
            saved.rules,
            vec![("new".to_string(), "replacement".to_string())]
        );
        assert_ne!(saved.revision, loaded.revision);
    }

    #[test]
    fn automatic_corrections_merge_atomically_without_losing_document_metadata() {
        let scratch = ScratchDirectory::new("rules-auto-correction");
        let path = scratch.path().join("replacements.txt");
        std::fs::write(&path, "# user note\r\nvoput code => old\r\n").unwrap();

        let replaced = learn_correction_pairs(
            scratch.path(),
            &[
                ("VOPUT CODE".to_string(), "VocalCode".to_string()),
                ("seaquel".to_string(), "SQL".to_string()),
            ],
        )
        .unwrap();
        assert_eq!(
            replaced.document.rules,
            vec![
                ("VOPUT CODE".to_string(), "VocalCode".to_string()),
                ("seaquel".to_string(), "SQL".to_string()),
            ]
        );
        assert_eq!(replaced.changes.len(), 2);
        assert_eq!(
            replaced.changes[0].previous,
            Some(("voput code".to_string(), "old".to_string()))
        );
        assert_eq!(replaced.changes[1].previous, None);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# user note\r\nVOPUT CODE => VocalCode\r\nseaquel => SQL\r\n"
        );

        let unchanged = learn_correction_pairs(scratch.path(), &replaced.document.rules).unwrap();
        assert_eq!(unchanged.document.revision, replaced.document.revision);
        assert!(unchanged.changes.is_empty());

        let before_invalid = std::fs::read(&path).unwrap();
        assert!(learn_correction_pairs(
            scratch.path(),
            &[
                ("another".to_string(), "valid".to_string()),
                ("".to_string(), "invalid".to_string()),
            ],
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before_invalid);
    }

    #[test]
    fn risky_learning_is_only_a_proposal_and_inverse_rule_is_not_written() {
        let scratch = ScratchDirectory::new("learning-risk-review");
        let path = scratch.path().join("replacements.txt");
        let original = "# existing notes\n考虑 => Collie\n";
        std::fs::write(&path, original).unwrap();
        let proposal =
            learn_correction_pairs(scratch.path(), &[("Collie".into(), "考虑".into())]).unwrap();
        assert!(proposal.review_message.is_some());
        assert_eq!(proposal.changes[0].from, "考虑");
        assert_eq!(proposal.changes[0].to, "考虑");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let risky = learn_correction_pairs(
            scratch.path(),
            &[
                ("in".into(), "linkedin".into()),
                ("seaquel".into(), "SQL".into()),
            ],
        )
        .unwrap();
        assert!(risky.review_message.is_some());
        assert_eq!(risky.changes.len(), 2);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        // Explicit confirmation uses the original revision; concurrent manual
        // changes must reject it, not apply a stale disable or reverse rule.
        std::fs::write(&path, "manual => edit\n").unwrap();
        assert!(save_rules_document_if_current(
            &path,
            &proposal.document.revision,
            &["考虑 => 考虑".into()]
        )
        .unwrap_err()
        .is_conflict());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "manual => edit\n");
    }

    #[test]
    fn legacy_diagnostics_marker_survives_config_roundtrip_before_migration() {
        let config: Config = toml::from_str("local_diagnostic_history = true").unwrap();
        assert_eq!(config.local_diagnostic_history, Some(true));
        let saved = toml::to_string(&config).unwrap();
        assert!(saved.contains("local_diagnostic_history = true"));
        assert!(!toml::to_string(&Config::default())
            .unwrap()
            .contains("local_diagnostic_history"));
    }

    #[test]
    fn dictionary_cas_refuses_valid_comment_and_invalid_external_edits() {
        let scratch = ScratchDirectory::new("rules-external-edits");
        let path = scratch.path().join("replacements.txt");
        for external in [
            "external => valid\n",
            "old => value\n# comment added by hand\n",
            "old => value\nthis line is intentionally invalid\n",
        ] {
            std::fs::write(&path, "old => value\n").unwrap();
            let loaded = load_rules_document_from(&path).unwrap();
            std::fs::write(&path, external).unwrap();

            let error = save_rules_document_if_current(
                &path,
                &loaded.revision,
                &["ui => edit".to_string()],
            )
            .unwrap_err();
            assert!(error.is_conflict(), "{error}");
            assert!(
                error.to_string().contains("changed outside this window"),
                "{error}"
            );
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                external,
                "a CAS conflict must leave every external byte untouched"
            );
        }
    }

    #[test]
    fn dictionary_cross_process_lock_allows_only_one_stale_writer() {
        let scratch = ScratchDirectory::new("rules-double-writer");
        let path = scratch.path().join("replacements.txt");
        std::fs::write(&path, "old => value\n").unwrap();
        let revision = load_rules_document_from(&path).unwrap().revision;
        let path = Arc::new(path);
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();

        for replacement in ["first => winner", "second => winner"] {
            let path = path.clone();
            let revision = revision.clone();
            let barrier = barrier.clone();
            workers.push(thread::spawn(move || {
                barrier.wait();
                save_rules_document_if_current(&path, &revision, &[replacement.to_string()])
            }));
        }
        barrier.wait();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let conflict = results
            .iter()
            .find_map(|result| result.as_ref().err())
            .expect("one stale writer must lose");
        assert!(conflict.is_conflict(), "{conflict}");
        assert!(matches!(
            std::fs::read_to_string(path.as_ref()).unwrap().as_str(),
            "first => winner\n" | "second => winner\n"
        ));
    }

    #[test]
    fn dictionary_revision_rotates_after_each_successful_edit() {
        let scratch = ScratchDirectory::new("rules-revision-rotation");
        let path = scratch.path().join("replacements.txt");
        std::fs::write(&path, "# keep\nold => value\ninvalid evidence\n").unwrap();
        let first = load_rules_document_from(&path).unwrap();
        let second =
            save_rules_document_if_current(&path, &first.revision, &["new => value".to_string()])
                .unwrap();
        assert_ne!(first.revision, second.revision);

        let stale = save_rules_document_if_current(
            &path,
            &first.revision,
            &["stale => writer".to_string()],
        )
        .unwrap_err();
        assert!(stale.is_conflict(), "{stale}");

        let third = save_rules_document_if_current(
            &path,
            &second.revision,
            &["newest => value".to_string()],
        )
        .unwrap();
        assert_ne!(second.revision, third.revision);
        assert_eq!(
            third.revision,
            load_rules_document_from(&path).unwrap().revision
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# keep\nnewest => value\ninvalid evidence\n"
        );
    }

    #[test]
    fn invalid_config_gets_a_unique_exact_recovery_without_replacing_old_backup() {
        let scratch = ScratchDirectory::new("invalid-config-recovery");
        let path = scratch.path().join("vocalcode.toml");
        let old_backup = path.with_extension("toml.invalid");
        let invalid = "# user's hand edit\ntalk = [\n";
        std::fs::write(&path, invalid).unwrap();
        std::fs::write(&old_backup, "older recovery must survive").unwrap();

        let defaults = load_config_from(&path).unwrap();

        assert_eq!(
            std::fs::read_to_string(&old_backup).unwrap(),
            "older recovery must survive"
        );
        let recoveries = recovery_files(scratch.path(), "vocalcode.toml");
        assert_eq!(recoveries.len(), 1);
        assert_eq!(std::fs::read_to_string(&recoveries[0]).unwrap(), invalid);
        let installed: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(installed.model, defaults.model);
        assert_eq!(installed.language, defaults.language);
    }

    #[test]
    fn config_is_not_replaced_when_recovery_cannot_be_created() {
        let scratch = ScratchDirectory::new("config-recovery-failure");
        let path = scratch.path().join("vocalcode.toml");
        let invalid = "model = [ definitely invalid\n";
        std::fs::write(&path, invalid).unwrap();

        let error = load_config_from_with(&path, |_path, _source, _replacement| {
            Err("simulated recovery disk failure; the original was left untouched".to_string())
        })
        .unwrap_err();

        assert!(error.contains("simulated recovery disk failure"));
        assert!(error.contains("original was left untouched"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
        assert!(recovery_files(scratch.path(), "vocalcode.toml").is_empty());
    }

    #[test]
    fn config_recovery_restores_moved_bytes_that_changed_after_initial_read() {
        let scratch = ScratchDirectory::new("config-recovery-race");
        let path = scratch.path().join("vocalcode.toml");
        let invalid = "model = [ definitely invalid\n";
        let concurrent_edit = "# changed by another process\nmodel = \"base.en\"\n";
        std::fs::write(&path, invalid).unwrap();

        let error = load_config_from_with(&path, |source_path, original, replacement| {
            recover_invalid_source_with(
                source_path,
                original,
                replacement,
                |move_path, move_original| {
                    std::fs::write(move_path, concurrent_edit)
                        .map_err(|error| error.to_string())?;
                    move_invalid_file_to_recovery(move_path, move_original)
                },
                |publish_path, bytes| storage::atomic_write_new(publish_path, bytes),
            )
        })
        .expect_err("moved bytes that differ from the initial read must stop recovery");

        assert!(error.contains("moved bytes did not match"));
        assert!(error.contains("defaults were not installed"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), concurrent_edit);
        let recoveries = recovery_files(scratch.path(), "vocalcode.toml");
        assert_eq!(recoveries.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&recoveries[0]).unwrap(),
            concurrent_edit
        );
    }

    #[test]
    fn non_not_found_read_errors_never_replace_user_files() {
        let scratch = ScratchDirectory::new("read-errors");
        for name in ["vocalcode.toml", "replacements.txt", "totals.json"] {
            let path = scratch.path().join(name);
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("keep-me"), "original data").unwrap();

            let error = match name {
                "vocalcode.toml" => load_config_from(&path).unwrap_err(),
                "replacements.txt" => load_rules_from(&path).unwrap_err(),
                "totals.json" => load_totals_from(&path).unwrap_err(),
                _ => unreachable!(),
            };
            assert!(error.contains("left untouched"), "{name}: {error}");
            assert!(path.is_dir(), "{name} was replaced");
            assert_eq!(
                std::fs::read_to_string(path.join("keep-me")).unwrap(),
                "original data"
            );
        }
    }

    #[test]
    fn dictionary_read_failure_is_isolated_to_its_exact_path() {
        let scratch = ScratchDirectory::new("rules-read-failure-isolation");
        let good_base = scratch.path().join("good");
        let bad_base = scratch.path().join("bad");
        std::fs::create_dir_all(&good_base).unwrap();
        std::fs::create_dir_all(&bad_base).unwrap();
        let good_path = good_base.join("replacements.txt");
        let bad_path = bad_base.join("replacements.txt");
        std::fs::write(&good_path, "old => value\n").unwrap();
        std::fs::create_dir(&bad_path).unwrap();
        std::fs::write(bad_path.join("keep-me"), "unreadable original").unwrap();

        // A successful read elsewhere supplies authority only for that exact
        // document. Passing its revision to the unreadable path cannot turn a
        // prior failure into write permission.
        assert!(load_rules_document_from(&bad_path).is_err());
        let good = load_rules_document(&good_base).unwrap();
        assert_eq!(good.rules, vec![("old".to_string(), "value".to_string())]);
        let bad_save = save_rules_if_current(
            &bad_base,
            &good.revision,
            &["unsafe => overwrite".to_string()],
        )
        .unwrap_err();
        assert!(!bad_save.is_conflict(), "{bad_save}");
        assert!(bad_path.is_dir());

        // Nor may a later failure in another directory revoke the good path's
        // exact revision. Its CAS remains usable and updates only that file.
        assert!(load_rules_document_from(&bad_path).is_err());
        save_rules_if_current(
            &good_base,
            &good.revision,
            &["new => replacement".to_string()],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&good_path).unwrap(),
            "new => replacement\n"
        );
        assert_eq!(
            std::fs::read_to_string(bad_path.join("keep-me")).unwrap(),
            "unreadable original"
        );
    }

    #[test]
    fn invalid_totals_are_recovered_before_fresh_counters_are_installed() {
        let scratch = ScratchDirectory::new("invalid-totals-recovery");
        let path = scratch.path().join("totals.json");
        let old_backup = scratch.path().join("totals.json.invalid");
        let invalid = "{\"dictations\": 91, this is truncated";
        std::fs::write(&path, invalid).unwrap();
        std::fs::write(&old_backup, "old totals recovery").unwrap();

        let totals = load_totals_from(&path).unwrap();

        assert_eq!(totals.dictations, 0);
        assert_eq!(
            std::fs::read_to_string(&old_backup).unwrap(),
            "old totals recovery"
        );
        let recoveries = recovery_files(scratch.path(), "totals.json");
        assert_eq!(recoveries.len(), 1);
        assert_eq!(std::fs::read_to_string(&recoveries[0]).unwrap(), invalid);
        let installed: Totals =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(installed.dictations, 0);
        assert_eq!(installed.words, 0);
        assert_eq!(installed.chars, 0);
    }

    #[test]
    fn totals_recovery_no_clobber_publish_preserves_a_concurrent_edit() {
        let scratch = ScratchDirectory::new("totals-recovery-race");
        let path = scratch.path().join("totals.json");
        let invalid = "{\"dictations\": 91, this is truncated";
        let concurrent_edit = r#"{"dictations":4,"words":40,"chars":240}"#;
        std::fs::write(&path, invalid).unwrap();

        let error = load_totals_from_with(&path, |source_path, original, replacement| {
            recover_invalid_source_with(
                source_path,
                original,
                replacement,
                move_invalid_file_to_recovery,
                |publish_path, defaults| {
                    // This hook runs only after the moved recovery was read and
                    // compared with `original`, at the final publish boundary.
                    std::fs::write(publish_path, concurrent_edit)?;
                    storage::atomic_write_new(publish_path, defaults)
                },
            )
        })
        .expect_err("no-clobber publish must reject a concurrently recreated source");

        assert!(error.contains("concurrently created source was preserved"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), concurrent_edit);
        let recoveries = recovery_files(scratch.path(), "totals.json");
        assert_eq!(recoveries.len(), 1);
        assert_eq!(std::fs::read_to_string(&recoveries[0]).unwrap(), invalid);
    }

    #[test]
    fn recovery_growth_is_detected_with_one_extra_byte_and_kept_complete() {
        let scratch = ScratchDirectory::new("recovery-growth-bound");
        let path = scratch.path().join("vocalcode.toml");
        let recovery = scratch.path().join("vocalcode.toml.invalid");
        let original = "bad";
        let grown = b"badX";
        std::fs::write(&path, original).unwrap();
        let published = std::cell::Cell::new(false);

        let error = recover_invalid_source_with(
            &path,
            original,
            b"defaults",
            |source, _| {
                std::fs::remove_file(source).unwrap();
                std::fs::write(&recovery, grown).unwrap();
                Ok(recovery.clone())
            },
            |_, _| {
                published.set(true);
                Ok(())
            },
        )
        .unwrap_err();

        assert!(
            error.contains("verification stopped after 4 bytes"),
            "{error}"
        );
        assert!(!published.get());
        assert_eq!(std::fs::read(&path).unwrap(), grown);
        assert_eq!(std::fs::read(&recovery).unwrap(), grown);
    }

    #[test]
    fn busy_decode_drops_late_actions_but_keeps_cleanup_and_quit() {
        let (tx, rx) = trigger_event_channel();
        let talk = TriggerId::synthetic(41);
        let other = TriggerId::synthetic(42);

        // This is the dangerous toggle gesture: both halves happened while ASR
        // was blocking. The press must never be replayed as a fresh recording;
        // its release remains useful as idempotent held-edge cleanup.
        tx.try_send(TriggerEvent::TalkPressed(talk)).unwrap();
        tx.try_send(TriggerEvent::TalkReleased(talk)).unwrap();
        tx.try_send(TriggerEvent::SendTapped(other)).unwrap();
        tx.try_send(TriggerEvent::TeachTapped(other)).unwrap();
        tx.try_send(TriggerEvent::DeviceDisconnected(other.device))
            .unwrap();
        tx.try_send(TriggerEvent::ForceStop).unwrap();
        tx.try_send(TriggerEvent::Cancel).unwrap();
        tx.try_send(TriggerEvent::Quit).unwrap();

        let mut cleanup = Vec::new();
        assert!(drain_busy_events(&rx, |event| cleanup.push(event)));
        assert_eq!(
            cleanup,
            vec![
                TriggerEvent::TalkReleased(talk),
                TriggerEvent::DeviceDisconnected(other.device),
                TriggerEvent::ForceStop,
                TriggerEvent::Cancel,
            ]
        );
        assert!(rx.try_recv().is_err(), "busy queue was not fully drained");
    }

    #[test]
    fn busy_decode_without_quit_resumes_normally() {
        let (tx, rx) = trigger_event_channel();
        tx.try_send(TriggerEvent::TalkPressed(TriggerId::synthetic(7)))
            .unwrap();

        let mut cleanup = Vec::new();
        assert!(!drain_busy_events(&rx, |event| cleanup.push(event)));
        assert!(cleanup.is_empty());
    }

    #[test]
    fn duplicate_single_instance_show_requests_share_one_pending_slot() {
        let (sender, receiver) = mpsc::sync_channel(1);
        assert!(enqueue_show(&sender));
        assert!(enqueue_show(&sender), "a duplicate is already represented");
        assert_eq!(receiver.try_recv(), Ok(()));
        assert!(receiver.try_recv().is_err());
        drop(receiver);
        assert!(!enqueue_show(&sender));
    }

    #[test]
    fn shutdown_aware_pause_observes_shutdown_without_waiting_for_the_phase() {
        let status = RuntimeStatus::default();
        status.shutdown.store(true, Ordering::Release);
        let started = Instant::now();
        assert!(!shutdown_aware_pause(&status, Duration::from_secs(4)));
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn periodic_update_checks_are_regular_but_not_noisy() {
        assert!(UPDATE_CHECK_INTERVAL >= Duration::from_secs(60 * 60));
        assert!(UPDATE_CHECK_INTERVAL <= Duration::from_secs(24 * 60 * 60));
    }

    #[test]
    fn background_runtime_owns_and_joins_input_supervisor() {
        let source = include_str!("main.rs");
        assert!(source.contains("input_shutdown: Arc<std::sync::atomic::AtomicBool>"));
        assert!(source.contains("self.input_shutdown.store(true, Ordering::Release)"));
        assert!(source.contains("self.hotkey.join()"));
        assert!(source.contains("remaining.min(std::time::Duration::from_millis(100))"));
    }

    #[test]
    fn long_running_log_sink_stays_bounded_and_keeps_one_generation() {
        let scratch = ScratchDirectory::new("log-long-running");
        let sink = LogFileSink::new(scratch.path(), 128);
        for index in 0..500 {
            sink.append_locked(format!("line-{index:04}\n").as_bytes())
                .unwrap();
        }

        assert!(std::fs::metadata(&sink.active).unwrap().len() <= 128);
        assert!(std::fs::metadata(&sink.backup).unwrap().len() <= 128);
        assert!(!scratch.path().join("vocalcode.log.2").exists());

        // No process-lifetime handle may remain: Windows must be able to rename
        // the active file immediately after any write transaction.
        let moved = scratch.path().join("renamed.log");
        std::fs::rename(&sink.active, &moved).unwrap();
        std::fs::rename(moved, &sink.active).unwrap();
    }

    #[test]
    fn two_log_writers_share_one_lock_without_torn_records() {
        let scratch = ScratchDirectory::new("log-two-writers");
        let directory = Arc::new(scratch.path().to_path_buf());
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut writers = Vec::new();
        for label in ['A', 'B'] {
            let directory = directory.clone();
            let barrier = barrier.clone();
            writers.push(thread::spawn(move || {
                let sink = LogFileSink::new(&directory, 512);
                barrier.wait();
                for index in 0..250 {
                    sink.append_locked(format!("{label}-{index:04}\n").as_bytes())
                        .unwrap();
                }
            }));
        }
        barrier.wait();
        for writer in writers {
            writer.join().unwrap();
        }

        let sink = LogFileSink::new(&directory, 512);
        for path in [&sink.backup, &sink.active] {
            let bytes = std::fs::read(path).unwrap();
            assert!(bytes.len() <= 512);
            let source = std::str::from_utf8(&bytes).unwrap();
            assert!(source.ends_with('\n'));
            for line in source.lines() {
                assert_eq!(line.len(), 6, "torn/interleaved log record: {line:?}");
                assert!(line.starts_with("A-") || line.starts_with("B-"), "{line}");
                assert!(
                    line[2..].bytes().all(|byte| byte.is_ascii_digit()),
                    "{line}"
                );
            }
        }
    }

    #[test]
    fn failed_rotation_disables_file_sink_without_truncating_active_log() {
        let scratch = ScratchDirectory::new("log-rotate-failure");
        let mut sink = LogFileSink::new(scratch.path(), 8);
        let evidence = b"12345678";
        std::fs::write(&sink.active, evidence).unwrap();
        std::fs::create_dir(&sink.backup).unwrap();

        sink.append(b"x");
        assert!(!sink.enabled);
        assert_eq!(std::fs::read(&sink.active).unwrap(), evidence);

        // Even after the obstacle disappears, a failed sink stays disabled for
        // the session rather than repeatedly risking the same evidence.
        std::fs::remove_dir(&sink.backup).unwrap();
        sink.append(b"y");
        assert_eq!(std::fs::read(&sink.active).unwrap(), evidence);
    }

    #[cfg(unix)]
    #[test]
    fn log_symlink_is_rejected_without_touching_its_victim() {
        let scratch = ScratchDirectory::new("log-symlink");
        let sink = LogFileSink::new(scratch.path(), 128);
        let victim = scratch.path().join("victim.txt");
        std::fs::write(&victim, b"keep this evidence").unwrap();
        std::os::unix::fs::symlink(&victim, &sink.active).unwrap();

        assert!(sink.append_locked(b"must not follow\n").is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep this evidence");
        assert!(std::fs::symlink_metadata(&sink.active)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(windows)]
    #[test]
    fn log_reparse_point_is_rejected_without_touching_its_victim() {
        let scratch = ScratchDirectory::new("log-reparse");
        let sink = LogFileSink::new(scratch.path(), 128);
        let victim = scratch.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        let evidence = victim.join("keep.txt");
        std::fs::write(&evidence, b"keep this evidence").unwrap();
        // Directory junctions are reparse points but do not require Developer
        // Mode's symbolic-link privilege, so this protection is exercised on
        // ordinary release-test machines as well.
        let created = std::process::Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&sink.active)
            .arg(&victim)
            .output()
            .unwrap();
        assert!(
            created.status.success(),
            "could not create junction: {}",
            String::from_utf8_lossy(&created.stderr)
        );

        assert!(sink.append_locked(b"must not follow\n").is_err());
        assert_eq!(std::fs::read(&evidence).unwrap(), b"keep this evidence");
        assert!(crate::paths::is_reparse_point(&sink.active));
    }

    fn unique_instance_scope(label: &str) -> String {
        let nonce = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        format!("vocalcode-test-{label}-{}-{nonce}", std::process::id())
    }

    #[test]
    fn second_instance_notifies_the_authenticated_primary() {
        let scratch = ScratchDirectory::new_for_instance("notify");
        let scope = unique_instance_scope("notify");
        let InstanceState::Primary(primary) =
            acquire_instance(scratch.path(), &scope).expect("first acquire must succeed")
        else {
            panic!("first acquire must be primary");
        };
        assert!(matches!(
            acquire_instance(scratch.path(), &scope).unwrap(),
            InstanceState::Existing
        ));
        primary
            .show
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the protected endpoint must deliver the show request");
        drop(primary.guard);
    }

    #[test]
    fn mutable_login_environment_cannot_spoof_instance_identity() {
        let before = instance_platform::current_identity();
        let keys = ["USERDOMAIN", "USERNAME", "USER"];
        let saved: Vec<_> = keys
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        for key in keys {
            std::env::set_var(key, "attacker-controlled-instance-identity");
        }
        let after = instance_platform::current_identity();
        for (key, value) in saved {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
        assert_eq!(before.unwrap(), after.unwrap());
    }

    #[test]
    fn authenticated_identity_isolates_instance_names() {
        let first = instance_token("scope", b"identity-one");
        assert_eq!(first, instance_token("scope", b"identity-one"));
        assert_ne!(first, instance_token("scope", b"identity-two"));
        assert_ne!(first, instance_token("other-scope", b"identity-one"));
        assert_eq!(first.len(), 32);
    }

    #[test]
    fn simultaneous_launch_race_has_exactly_one_primary() {
        const CONTENDERS: usize = 6;
        let scratch = ScratchDirectory::new_for_instance("race");
        let scope = unique_instance_scope("race");
        let release = Arc::new(std::sync::Barrier::new(CONTENDERS + 1));
        let (send, receive) = mpsc::channel();
        let mut contenders = Vec::new();
        for _ in 0..CONTENDERS {
            let directory = scratch.path().to_path_buf();
            let scope = scope.clone();
            let release = release.clone();
            let send = send.clone();
            contenders.push(thread::spawn(move || {
                let state = acquire_instance(&directory, &scope);
                let result = state
                    .as_ref()
                    .map(|state| matches!(state, InstanceState::Primary(_)))
                    .map_err(ToString::to_string);
                let _ = send.send(result);
                release.wait();
                drop(state);
            }));
        }
        drop(send);
        let results: Vec<_> = receive.iter().take(CONTENDERS).collect();
        release.wait();
        for contender in contenders {
            contender.join().unwrap();
        }
        let primary_count = results
            .into_iter()
            .map(|result| result.expect("instance acquisition must succeed"))
            .filter(|primary| *primary)
            .count();
        assert_eq!(primary_count, 1);
    }

    #[test]
    fn ownership_recovers_after_the_primary_handle_closes() {
        let scratch = ScratchDirectory::new_for_instance("recover");
        let scope = unique_instance_scope("recover");
        let first = acquire_instance(scratch.path(), &scope).unwrap();
        assert!(matches!(first, InstanceState::Primary(_)));
        drop(first);
        let recovered = acquire_instance(scratch.path(), &scope).unwrap();
        assert!(matches!(recovered, InstanceState::Primary(_)));
    }

    #[cfg(unix)]
    #[test]
    fn crashed_unix_primary_stale_socket_is_recovered_only_by_lock_owner() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;

        let scratch = ScratchDirectory::new_for_instance("stale");
        let scope = unique_instance_scope("stale-socket");
        let socket = instance_platform::socket_path_for_test(scratch.path(), &scope).unwrap();
        drop(UnixListener::bind(&socket).unwrap());
        assert!(socket.exists(), "the simulated crash must leave an inode");

        let primary = acquire_instance(scratch.path(), &scope).unwrap();
        assert!(matches!(primary, InstanceState::Primary(_)));
        let mode = std::fs::symlink_metadata(&socket)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(windows)]
    #[test]
    fn untrusted_pipe_occupant_cannot_suppress_a_new_primary() {
        let scratch = ScratchDirectory::new("instance-pipe-spoof");
        let scope = unique_instance_scope("pipe-spoof");
        let _occupant = instance_platform::occupy_pipe_for_test(&scope).unwrap();
        let state = acquire_instance(scratch.path(), &scope).unwrap();
        assert!(matches!(state, InstanceState::Primary(_)));
    }

    #[cfg(windows)]
    #[test]
    fn windows_instance_acl_contains_only_system_and_current_user() {
        assert_eq!(
            instance_platform::private_sddl_for_test("S-1-5-21-123"),
            "D:P(A;;GA;;;SY)(A;;GA;;;S-1-5-21-123)"
        );
        assert_eq!(
            WINDOWS_INSTALLER_OBSERVATION_MUTEX,
            if community::ENABLED {
                r"Local\VocalCode.Community.Desktop"
            } else {
                r"Local\VocalCode.Desktop"
            }
        );
    }

    #[test]
    fn single_instance_path_has_no_udp_or_environment_identity_authority() {
        let source = include_str!("main.rs");
        let start = source
            .find("// Single instance + \"show the existing window\" hand-off")
            .unwrap();
        let end = source[start..].find("fn show_startup_error").unwrap() + start;
        let implementation = &source[start..end];
        assert!(!implementation.contains("UdpSocket"));
        assert!(!implementation.contains("INSTANCE_ACK"));
        assert!(!implementation.contains("USERDOMAIN"));
        assert!(!implementation.contains("USERNAME"));
        assert!(!implementation.contains("std::env::var(\"USER\")"));
    }

    #[test]
    fn update_installer_failure_argument_becomes_a_visible_message() {
        let args = vec![
            "vocalcode-app".to_string(),
            "--update-failed".to_string(),
            "17".to_string(),
        ];
        let message = update_failure_message(&args).expect("failure must be surfaced");
        assert!(message.contains("17"));
        let helper = vec![
            "vocalcode-app".to_string(),
            "--update-failed".to_string(),
            "helper-launch-error".to_string(),
        ];
        let message = update_failure_message(&helper).expect("helper failure must be surfaced");
        assert!(message.contains("could not start"));
        let timeout = vec![
            "vocalcode-app".to_string(),
            "--update-failed".to_string(),
            "installer-timeout".to_string(),
        ];
        let message = update_failure_message(&timeout).expect("timeout must be surfaced");
        assert!(message.contains("did not finish within 15 minutes"));
        assert!(!message.contains("exited with code"));
        assert!(update_failure_message(&["vocalcode-app".to_string()]).is_none());
    }

    #[test]
    fn audio_errors_close_readiness_but_other_errors_do_not() {
        let mut ready = true;
        assert!(invalidate_audio_on_error(
            &VocalCodeError::Audio("device gone".to_string()),
            &mut ready,
        ));
        assert!(!ready);

        ready = true;
        assert!(!invalidate_audio_on_error(
            &VocalCodeError::Inject("focus changed".to_string()),
            &mut ready,
        ));
        assert!(ready);
    }

    #[test]
    fn maintenance_deadline_fires_even_when_events_keep_arriving() {
        let start = std::time::Instant::now();
        let mut clock = MaintenanceClock::new(start);
        assert!(!clock.take_due(start + RUNTIME_MAINTENANCE_INTERVAL / 2));
        assert!(clock.take_due(start + RUNTIME_MAINTENANCE_INTERVAL));
        assert!(!clock.take_due(start + RUNTIME_MAINTENANCE_INTERVAL));
    }

    #[test]
    fn newer_config_generation_cancels_and_cannot_be_overwritten_by_deferred_work() {
        let mut apply = ConfigApplyCoordinator::default();
        let config_a = Config {
            language: "es".to_string(),
            ..Default::default()
        };
        let generation_a = apply.publish(8, config_a.clone());
        let deferred_a = apply.take_pending().unwrap();
        let cancellation_a = apply.begin_prepare(generation_a, &config_a).unwrap();

        // A different model route must cancel the running prepare.
        let config_b = Config {
            language: "zh".to_string(),
            ..Default::default()
        };
        let generation_b = apply.publish(9, config_b);
        assert!(generation_b > generation_a);
        assert!(cancellation_a.is_cancelled());
        assert!(!apply.is_current(generation_a));
        assert!(!apply.requeue_if_no_newer(deferred_a));
        assert_eq!(
            apply.pending.as_ref().map(|pending| pending.generation),
            Some(generation_b)
        );

        let current = apply.take_pending().unwrap();
        assert!(apply.requeue_if_no_newer(current));
        assert_eq!(
            apply.pending.as_ref().map(|pending| pending.generation),
            Some(generation_b)
        );
    }

    /// Toggling an unrelated setting must NOT throw away an in-flight model
    /// download: a same-route save leaves the running prepare alive (the apply
    /// loop discards its stale swap, but the downloaded artifacts persist). A
    /// native tester lost a multi-hundred-MB first download to exactly this.
    #[test]
    fn same_route_save_does_not_cancel_a_running_prepare() {
        let mut apply = ConfigApplyCoordinator::default();
        let config = Config {
            language: "es".to_string(),
            ..Default::default()
        };
        let generation_a = apply.publish(8, config.clone());
        let cancellation_a = apply.begin_prepare(generation_a, &config).unwrap();

        let generation_b = apply.publish(9, config.clone());
        assert!(generation_b > generation_a);
        assert!(!cancellation_a.is_cancelled());
        assert!(!apply.is_current(generation_a));

        // The next route change still cancels it.
        let mut other = config.clone();
        other.language = "zh".to_string();
        apply.publish(10, other);
        assert!(cancellation_a.is_cancelled());
    }

    #[test]
    fn final_generation_check_and_new_save_are_one_atomic_ordering_point() {
        let status = Arc::new(RuntimeStatus::default());
        let generation_a = status
            .config_apply
            .lock()
            .unwrap()
            .publish(1, Config::default());
        let commit_guard = status.config_apply.lock().unwrap();
        assert!(commit_guard.is_current(generation_a));

        let (entered_tx, entered_rx) = mpsc::channel();
        let (published_tx, published_rx) = mpsc::channel();
        let writer_status = status.clone();
        let writer = thread::spawn(move || {
            entered_tx.send(()).unwrap();
            let generation_b = writer_status
                .config_apply
                .lock()
                .unwrap()
                .publish(2, Config::default());
            published_tx.send(generation_b).unwrap();
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(
            published_rx
                .recv_timeout(std::time::Duration::from_millis(75))
                .is_err(),
            "a new save must not publish inside the engine's checked commit"
        );

        drop(commit_guard);
        let generation_b = published_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(generation_b > generation_a);
        writer.join().unwrap();
    }

    #[test]
    fn failed_multi_field_model_request_rolls_back_the_whole_snapshot() {
        let scratch = ScratchDirectory::new("atomic-config-rollback");
        let path = scratch.path().join("vocalcode.toml");
        let active = Config {
            model: "base.en".to_string(),
            language: "en".to_string(),
            live_caption: false,
            paste_insert: false,
            input_device: None,
            ..Config::default()
        };
        let rejected = Config {
            model: "small.en".to_string(),
            live_caption: true,
            paste_insert: true,
            input_device: Some("missing wireless microphone".to_string()),
            ..active.clone()
        };
        storage::atomic_write(&path, toml::to_string_pretty(&rejected).unwrap()).unwrap();
        let status = RuntimeStatus::default();
        let shared = std::sync::Mutex::new(rejected.clone());
        let generation = {
            let mut apply = status.config_apply.lock().unwrap();
            let generation = apply.publish(4, rejected.clone());
            let _ = apply.take_pending();
            generation
        };

        assert_eq!(
            rollback_rejected_config_at(&path, &status, &shared, generation, &rejected, &active,)
                .unwrap(),
            ConfigRollback::Applied { disk_error: None }
        );
        assert!(config_snapshots_match(&shared.lock().unwrap(), &active).unwrap());
        let durable: Config = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(config_snapshots_match(&durable, &active).unwrap());
    }

    #[test]
    fn failed_disk_rollback_still_restores_the_shared_runtime_snapshot() {
        let scratch = ScratchDirectory::new("config-rollback-disk-error");
        let path = scratch.path().join("vocalcode.toml");
        std::fs::create_dir(&path).unwrap();
        let active = Config::default();
        let rejected = Config {
            live_caption: !active.live_caption,
            ..active.clone()
        };
        let status = RuntimeStatus::default();
        let shared = std::sync::Mutex::new(rejected.clone());
        let generation = {
            let mut apply = status.config_apply.lock().unwrap();
            let generation = apply.publish(7, rejected.clone());
            let _ = apply.take_pending();
            generation
        };

        let outcome =
            rollback_rejected_config_at(&path, &status, &shared, generation, &rejected, &active)
                .unwrap();
        assert!(matches!(
            outcome,
            ConfigRollback::Applied {
                disk_error: Some(_)
            }
        ));
        assert!(config_snapshots_match(&shared.lock().unwrap(), &active).unwrap());
    }

    #[test]
    fn rollback_never_overwrites_a_future_config_that_appeared_after_rejection() {
        let scratch = ScratchDirectory::new("future-config-rollback-race");
        let path = scratch.path().join("vocalcode.toml");
        let active = Config::default();
        let rejected = Config {
            live_caption: !active.live_caption,
            ..active.clone()
        };
        let future = vocalcode_core::config::CONFIG_VERSION + 1;
        let source = format!("config_version = {future}\ntalk = \"new-schema\"\n");
        std::fs::write(&path, &source).unwrap();
        let status = RuntimeStatus::default();
        let shared = std::sync::Mutex::new(rejected.clone());
        let generation = {
            let mut apply = status.config_apply.lock().unwrap();
            let generation = apply.publish(8, rejected.clone());
            let _ = apply.take_pending();
            generation
        };

        let outcome =
            rollback_rejected_config_at(&path, &status, &shared, generation, &rejected, &active)
                .unwrap();
        assert!(matches!(
            outcome,
            ConfigRollback::Applied {
                disk_error: Some(_)
            }
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        assert!(config_snapshots_match(&shared.lock().unwrap(), &active).unwrap());
    }

    #[test]
    fn superseded_model_failure_never_rolls_back_the_newer_persisted_config() {
        let scratch = ScratchDirectory::new("superseded-config-failure");
        let path = scratch.path().join("vocalcode.toml");
        let active = Config::default();
        let rejected_a = Config {
            model: "small.en".to_string(),
            ..active.clone()
        };
        let desired_b = Config {
            language: "zh".to_string(),
            model: "paraformer-zh".to_string(),
            ..active.clone()
        };
        storage::atomic_write(&path, toml::to_string_pretty(&desired_b).unwrap()).unwrap();
        let status = RuntimeStatus::default();
        let shared = std::sync::Mutex::new(desired_b.clone());
        let generation_a;
        {
            let mut apply = status.config_apply.lock().unwrap();
            generation_a = apply.publish(41, rejected_a.clone());
            let _ = apply.take_pending();
            let generation_b = apply.publish(42, desired_b.clone());
            assert!(generation_b > generation_a);
        }

        assert_eq!(
            rollback_rejected_config_at(
                &path,
                &status,
                &shared,
                generation_a,
                &rejected_a,
                &active,
            )
            .unwrap(),
            ConfigRollback::Skipped
        );
        assert!(config_snapshots_match(&shared.lock().unwrap(), &desired_b).unwrap());
        let durable: Config = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(config_snapshots_match(&durable, &desired_b).unwrap());
    }

    #[test]
    fn rollback_side_effects_finish_before_a_new_save_can_publish() {
        let scratch = ScratchDirectory::new("rollback-side-effect-order");
        let path = scratch.path().join("vocalcode.toml");
        let active = Config::default();
        let rejected_a = Config {
            model: "small.en".to_string(),
            ..active.clone()
        };
        let desired_b = Config {
            language: "zh".to_string(),
            model: "paraformer-zh".to_string(),
            ..active.clone()
        };
        storage::atomic_write(&path, toml::to_string_pretty(&rejected_a).unwrap()).unwrap();
        let status = Arc::new(RuntimeStatus::default());
        let shared = Arc::new(std::sync::Mutex::new(rejected_a.clone()));
        let generation_a = {
            let mut apply = status.config_apply.lock().unwrap();
            let generation = apply.publish(71, rejected_a.clone());
            let _ = apply.take_pending();
            generation
        };
        let external_state = Arc::new(AtomicU64::new(0));
        let writer_state = std::cell::RefCell::new(None);

        let outcome = rollback_rejected_config_at_with(
            &path,
            &status,
            &shared,
            generation_a,
            &rejected_a,
            &active,
            |_| {
                let writer_status = status.clone();
                let writer_shared = shared.clone();
                let writer_path = path.clone();
                let writer_config = desired_b.clone();
                let writer_external = external_state.clone();
                let (contended_tx, contended_rx) = mpsc::channel();
                let (published_tx, published_rx) = mpsc::channel();
                let writer = thread::spawn(move || {
                    let mut reported_contention = false;
                    let mut apply = loop {
                        match writer_status.config_apply.try_lock() {
                            Ok(apply) => break apply,
                            Err(std::sync::TryLockError::WouldBlock) => {
                                if !reported_contention {
                                    contended_tx.send(()).unwrap();
                                    reported_contention = true;
                                }
                                thread::yield_now();
                            }
                            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                                break poisoned.into_inner();
                            }
                        }
                    };
                    storage::atomic_write(
                        &writer_path,
                        toml::to_string_pretty(&writer_config).unwrap(),
                    )
                    .unwrap();
                    *writer_shared.lock().unwrap() = writer_config.clone();
                    let generation_b = apply.publish(72, writer_config);
                    writer_external.store(generation_b, Ordering::Release);
                    published_tx.send(generation_b).unwrap();
                });
                contended_rx
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .expect("B must observe A's coordinator guard during side effects");
                assert!(
                    published_rx
                        .recv_timeout(std::time::Duration::from_millis(75))
                        .is_err(),
                    "B published before A's protected rollback side effects completed"
                );
                external_state.store(generation_a, Ordering::Release);
                *writer_state.borrow_mut() = Some((writer, published_rx));
            },
        )
        .unwrap();
        assert_eq!(outcome, ConfigRollback::Applied { disk_error: None });

        let (writer, published_rx) = writer_state.borrow_mut().take().unwrap();
        let generation_b = published_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        writer.join().unwrap();
        assert!(generation_b > generation_a);
        assert_eq!(external_state.load(Ordering::Acquire), generation_b);
        assert!(config_snapshots_match(&shared.lock().unwrap(), &desired_b).unwrap());
        let durable: Config = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(config_snapshots_match(&durable, &desired_b).unwrap());
    }

    #[test]
    fn model_failure_policy_distinguishes_hot_switch_from_first_run() {
        assert!(!accept_model_config_after_prepare_failure(true));
        assert!(accept_model_config_after_prepare_failure(false));
    }

    #[test]
    fn activation_cli_never_accepts_a_key_in_argv() {
        let interactive = vec!["vocalcode-app".to_string(), "activate".to_string()];
        assert_eq!(
            activation_input(&interactive).unwrap(),
            Some(ActivationInput::Prompt)
        );
        let stdin = vec![
            "vocalcode-app".to_string(),
            "activate".to_string(),
            "--stdin".to_string(),
        ];
        assert_eq!(
            activation_input(&stdin).unwrap(),
            Some(ActivationInput::Stdin)
        );
        let secret = "vc-secret-that-must-not-echo";
        let argv_key = vec![
            "vocalcode-app".to_string(),
            "activate".to_string(),
            secret.to_string(),
        ];
        let error = activation_input(&argv_key).unwrap_err();
        assert!(!error.contains(secret));
        assert!(error.contains("never accepted"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_gui_activation_points_to_safe_input_paths() {
        assert!(WINDOWS_ACTIVATION_PROMPT_ERROR.contains("License panel"));
        assert!(WINDOWS_ACTIVATION_PROMPT_ERROR.contains("activate --stdin"));
    }

    /// This is the whole of the auto-update decision, and it had no test. A
    /// wrong answer here is not cosmetic: too eager and every launch nags about
    /// an update that is already installed, too lax and nobody ever hears about
    /// a fix.
    #[test]
    fn newer_versions_are_offered() {
        assert!(is_newer("0.4.1", "0.4.0"));
        assert!(is_newer("0.5.0", "0.4.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
    }

    #[test]
    fn same_or_older_is_not_offered() {
        assert!(!is_newer("0.4.0", "0.4.0"));
        assert!(!is_newer("0.3.9", "0.4.0"));
        assert!(!is_newer("0.9.9", "1.0.0"));
    }

    /// The trap a plain string compare falls into: "0.10.0" sorts before
    /// "0.9.0" lexically, so a tenth minor release would never be offered.
    #[test]
    fn double_digit_components_compare_numerically() {
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.9.0", "0.10.0"));
        assert!(is_newer("0.4.10", "0.4.9"));
    }

    /// Production manifests carry stable three-component releases only. Invalid
    /// strings fail closed rather than partially parsing attacker-controlled
    /// suffixes or substituting zero for garbage.
    #[test]
    fn odd_shapes_do_not_panic_or_mislead() {
        assert!(!is_newer("0.4", "0.4.0"));
        assert!(!is_newer("0.4.1", "0.4"));
        assert!(!is_newer("", "0.4.0"));
        assert!(!is_newer("0.5.0-beta", "0.4.0"));
        assert!(!is_newer("0.garbage.999", "0.4.0"));
        assert!(!is_newer("0..999", "0.4.0"));
        assert!(!is_newer("+1.0.0", "0.4.0"));
        assert!(!is_newer("1.-1.0", "0.4.0"));
        assert!(!is_newer("1.0.0.1", "0.4.0"));
        assert!(!is_newer(&format!("{}.0.0", "9".repeat(65)), "0.4.0"));
    }

    #[test]
    fn update_artifacts_are_pinned_to_the_official_origin_and_shape() {
        assert!(approved_update_url(
            "windows",
            "1.2.3",
            if community::ENABLED {
                "https://github.com/wudaming00/vocalcode-community/releases/download/v1.2.3/VocalCodeCommunitySetup.exe"
            } else {
                "https://vocalcode.app/VocalCodeSetup.exe"
            }
        ));
        assert!(approved_update_url(
            "macos",
            "1.2.3",
            if community::ENABLED {
                "https://github.com/wudaming00/vocalcode-community/releases/download/v1.2.3/VocalCodeCommunity-1.2.3.dmg"
            } else {
                "https://vocalcode.app/VocalCode-1.2.3.dmg"
            }
        ));
        for url in [
            "https://evil.example/VocalCodeSetup.exe",
            "https://vocalcode.app/VocalCodeSetup.exe?other=1",
            "http://vocalcode.app/VocalCodeSetup.exe",
            "https://vocalcode.app/VocalCodeSetup.exe\" --silent",
            "https://vocalcode.app/VocalCodeSetup.exe' && open /tmp",
            "https://vocalcode.app/%56ocalCodeSetup.exe",
            "https://vocalcode.app//VocalCodeSetup.exe",
        ] {
            assert!(!approved_update_url("windows", "1.2.3", url), "{url}");
        }
        assert!(!approved_update_url(
            "macos",
            "1.2.3",
            "https://vocalcode.app/VocalCode-1.2.4.dmg"
        ));
        assert!(!approved_update_url(
            "macos",
            "1.2.3",
            "https://vocalcode.app/VocalCode-1.2.3.dmg' --args"
        ));
    }

    #[test]
    #[cfg(not(feature = "community"))]
    fn paid_major_upgrades_are_not_offered_outside_entitlement() {
        let v1 = LicenseStatus::Licensed { max_version: 1 };
        assert!(update_entitled(&v1, "1.9.9"));
        assert!(!update_entitled(&v1, "2.0.0"));
        let v2 = LicenseStatus::Licensed { max_version: 2 };
        assert!(update_entitled(&v2, "2.0.0"));
        assert!(update_entitled(
            &LicenseStatus::Trial { days_left: 1 },
            "2.0.0"
        ));
        assert!(!update_entitled(&LicenseStatus::Expired, "1.0.1"));
        assert!(!update_entitled(
            &LicenseStatus::Invalid("bad receipt".to_string()),
            "1.0.1"
        ));
    }

    #[test]
    #[cfg(not(feature = "community"))]
    fn basic_dictation_survives_trial_expiry_and_invalid_pro_receipts() {
        assert!(LOCAL_LICENSE_GATE_INTERVAL <= std::time::Duration::from_secs(60));
        assert!(NETWORK_LICENSE_REFRESH_INTERVAL >= std::time::Duration::from_secs(60 * 60));
        assert!(injection_allowed(&LicenseStatus::TrialSetupRequired));
        assert!(injection_allowed(&LicenseStatus::Expired));
        assert!(injection_allowed(&LicenseStatus::Invalid(
            "expired".to_string()
        )));
        assert_eq!(product_tier(&LicenseStatus::Expired), ProductTier::Basic);
        assert!(!pro_allowed(&LicenseStatus::Expired));
        assert!(!pro_allowed(&LicenseStatus::Invalid("bad".to_string())));
    }

    #[test]
    #[cfg(not(feature = "community"))]
    fn paid_and_trial_receipts_unlock_pro_without_changing_basic_dictation() {
        for status in [
            LicenseStatus::Licensed { max_version: 1 },
            LicenseStatus::Trial { days_left: 1 },
        ] {
            assert!(injection_allowed(&status));
            assert_eq!(product_tier(&status), ProductTier::Pro);
            assert!(pro_allowed(&status));
        }
        assert_eq!(license_state(&LicenseStatus::Expired).0, "basic");
        assert_eq!(
            license_string(&LicenseStatus::Expired),
            "Basic — free forever"
        );
    }

    #[test]
    #[cfg(feature = "community")]
    fn community_access_never_depends_on_receipts_or_paid_updates() {
        assert_eq!(license_status(), LicenseStatus::TrialSetupRequired);
        for status in [
            LicenseStatus::TrialSetupRequired,
            LicenseStatus::Expired,
            LicenseStatus::Invalid("bad receipt".into()),
            LicenseStatus::Trial { days_left: 0 },
            LicenseStatus::Licensed { max_version: 1 },
        ] {
            assert!(injection_allowed(&status));
            assert!(pro_allowed(&status));
            assert_eq!(product_tier(&status), ProductTier::Community);
            assert_eq!(license_state(&status), ("community".to_string(), 0));
            assert_eq!(license_string(&status), community::LABEL);
            assert!(update_entitled(&status, "1.3.1"));
            assert!(update_entitled(&status, "2.0.0"));
            assert!(!approved_update_url(
                "windows",
                "1.3.1",
                "https://vocalcode.app/VocalCodeSetup.exe"
            ));
        }
    }

    #[test]
    #[cfg(feature = "community")]
    fn community_background_services_exit_without_network_or_user_data() {
        let status = Arc::new(RuntimeStatus::default());
        start_license_maintenance(status.clone()).join().unwrap();
        status.shutdown.store(true, Ordering::Release);
        start_update_maintenance(status.clone())
            .unwrap()
            .join()
            .unwrap();
        check_for_update(&status);
        assert!(status.update.lock().unwrap().is_none());
        assert!(status.update_check.lock().unwrap().is_none());
        assert!(!status.trial_setup_error.load(Ordering::Relaxed));
    }

    #[test]
    fn licence_refresh_sequence_stops_between_blocking_network_requests() {
        let calls = std::cell::RefCell::new(Vec::new());
        let checks = std::cell::Cell::new(0);
        let result = run_license_refresh_sequence(
            || calls.borrow_mut().push("legacy"),
            || calls.borrow_mut().push("cached"),
            || calls.borrow_mut().push("trial"),
            || {
                let next = checks.get() + 1;
                checks.set(next);
                next == 2
            },
        );

        assert!(result.is_none());
        assert_eq!(*calls.borrow(), ["legacy", "cached"]);
        assert_eq!(checks.get(), 2);
    }

    #[test]
    fn licence_refresh_errors_back_off_and_a_healthy_attempt_resets_the_clock() {
        let start = std::time::Instant::now();
        let mut backoff = LicenseRefreshBackoff::default();
        let mut now = start;
        for expected in LICENSE_REFRESH_RETRY_DELAYS {
            let next = backoff.next_deadline(now, false);
            assert_eq!(next.duration_since(now), expected);
            now = next;
        }
        let capped = backoff.next_deadline(now, false);
        assert_eq!(
            capped.duration_since(now),
            std::time::Duration::from_secs(60 * 60)
        );

        let healthy = backoff.next_deadline(capped, true);
        assert_eq!(
            healthy.duration_since(capped),
            NETWORK_LICENSE_REFRESH_INTERVAL
        );
        let first_retry_again = backoff.next_deadline(healthy, false);
        assert_eq!(
            first_retry_again.duration_since(healthy),
            LICENSE_REFRESH_RETRY_DELAYS[0]
        );
    }

    #[test]
    fn unavailable_model_retries_back_off_cap_and_reset_after_recovery() {
        let start = std::time::Instant::now();
        let mut backoff = ModelRetryBackoff::default();
        let mut now = start;
        for expected in MODEL_RETRY_DELAYS {
            let next = backoff.next_deadline(now);
            assert_eq!(next.duration_since(now), expected);
            now = next;
        }
        let capped = backoff.next_deadline(now);
        assert_eq!(
            capped.duration_since(now),
            std::time::Duration::from_secs(10 * 60)
        );

        backoff.reset();
        let first_retry_again = backoff.next_deadline(capped);
        assert_eq!(
            first_retry_again.duration_since(capped),
            MODEL_RETRY_DELAYS[0]
        );
    }

    #[test]
    fn a_failed_model_attempt_that_moved_the_download_forward_retries_soon() {
        let mut now = std::time::Instant::now();
        let mut backoff = ModelRetryBackoff::default();
        let mut delay_after = |downloaded: u64, now: &mut std::time::Instant| {
            let next = backoff.next_deadline_after(*now, Some(downloaded));
            let delay = next.duration_since(*now);
            *now = next;
            delay
        };
        assert_eq!(delay_after(100, &mut now), MODEL_RETRY_DELAYS[0]);
        assert_eq!(
            delay_after(100, &mut now),
            MODEL_RETRY_DELAYS[1],
            "no new bytes on disk: back off further"
        );
        assert_eq!(delay_after(100, &mut now), MODEL_RETRY_DELAYS[2]);
        assert_eq!(
            delay_after(250, &mut now),
            MODEL_RETRY_DELAYS[0],
            "a resumed attempt that fetched more is not an outage"
        );
        assert_eq!(delay_after(250, &mut now), MODEL_RETRY_DELAYS[1]);
    }

    #[test]
    fn a_new_model_attempt_takes_the_failure_banner_down() {
        let status = RuntimeStatus::default();
        let config = Config {
            language: "en".to_string(),
            onboarded: true,
            ..Config::default()
        };
        let base = ScratchDirectory::new("model-failure");
        let encoder = base
            .path()
            .join("models/parakeet-tdt-v3/encoder.onnx.partial");
        std::fs::create_dir_all(encoder.parent().unwrap()).unwrap();
        std::fs::write(&encoder, vec![0u8; 2_000_000]).unwrap();
        let mut backoff = ModelRetryBackoff::default();
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        schedule_model_retry(
            &status,
            &mut backoff,
            &config,
            base.path(),
            "download encoder.onnx: offline",
        );
        let failure = status.model_failure.lock().unwrap().clone().unwrap();
        assert_eq!(failure.message, "download encoder.onnx: offline");
        assert_eq!(
            failure.downloaded.map(|(done, _)| done),
            Some(2_000_000),
            "the kept partial counts toward what is already downloaded"
        );
        let retry_in = failure.retry_at_ms.unwrap() - before;
        assert!(
            retry_in >= MODEL_RETRY_DELAYS[0].as_millis() as u64 - 1_000
                && retry_in <= MODEL_RETRY_DELAYS[0].as_millis() as u64 + 1_000,
            "{retry_in} ms"
        );
        assert_eq!(
            failure.smaller_model.map(|(id, _)| id),
            Some("sensevoice"),
            "English on Parakeet can switch to the smaller SenseVoice"
        );
        assert!(failure
            .downloaded
            .is_some_and(|(_, total)| total > 600_000_000));

        let generation = status.config_apply.lock().unwrap().generation();
        assert!(begin_model_prepare(&status, generation, &config).is_some());
        assert_eq!(*status.model_failure.lock().unwrap(), None);
        finish_model_prepare(&status, generation);
    }

    #[test]
    fn stale_update_checks_cannot_clear_or_overwrite_a_newer_offer() {
        let status = RuntimeStatus::default();
        let first = begin_update_check(&status);
        let second = begin_update_check(&status);
        let newest = UpdateOffer {
            version: "1.2.3".to_string(),
            url: "https://vocalcode.app/VocalCodeSetup.exe".to_string(),
            notes: "notes".to_string(),
            sha256: "a".repeat(64),
            size: 7_476_736,
        };
        finish_update_check(&status, second, "newer", Some(newest.clone()));
        finish_update_check(&status, first, "failed", None);
        assert_eq!(*status.update.lock().unwrap(), Some(newest));
        assert_eq!(
            status.update_check.lock().unwrap().as_deref(),
            Some("newer")
        );
    }

    #[test]
    fn completed_update_request_cannot_publish_after_shutdown() {
        let status = RuntimeStatus::default();
        let generation = begin_update_check(&status);
        *status.update_check.lock().unwrap() = Some("previous".to_string());
        status.shutdown.store(true, Ordering::Release);

        finish_update_check(&status, generation, "failed", None);

        assert_eq!(
            status.update_check.lock().unwrap().as_deref(),
            Some("previous")
        );
    }

    #[test]
    fn update_artifact_size_is_strictly_positive_and_bounded() {
        assert!(!valid_update_size(0));
        assert!(valid_update_size(1));
        assert!(valid_update_size(MAX_UPDATE_BYTES));
        assert!(!valid_update_size(MAX_UPDATE_BYTES + 1));
    }

    #[test]
    fn update_manifest_requires_an_exact_integer_platform_size() {
        let valid = |size: serde_json::Value| {
            serde_json::json!({
                "version": "9.8.7",
                "notes": "top-level notes",
                "windows": {
                    "version": "9.8.7",
                    "url": "https://vocalcode.app/VocalCodeSetup.exe",
                    "sha256": "a".repeat(64),
                    "size": size,
                }
            })
        };

        let offer = update_offer_from_manifest(&valid(serde_json::json!(7_476_736)), "windows")
            .expect("release pipeline shape must parse");
        assert_eq!(offer.size, 7_476_736);
        assert_eq!(offer.notes, "top-level notes");

        for value in [
            serde_json::Value::Null,
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!(true),
            serde_json::json!("7476736"),
            serde_json::json!(crate::MAX_UPDATE_BYTES + 1),
        ] {
            assert!(
                update_offer_from_manifest(&valid(value), "windows").is_none(),
                "invalid artifact size was accepted"
            );
        }
        let mut missing = valid(serde_json::json!(1));
        missing["windows"].as_object_mut().unwrap().remove("size");
        assert!(update_offer_from_manifest(&missing, "windows").is_none());
        let malformed_platform = serde_json::json!({
            "version": "9.8.7",
            "url": "https://vocalcode.app/VocalCodeSetup.exe",
            "size": 1,
            "windows": "not an object",
        });
        assert!(update_offer_from_manifest(&malformed_platform, "windows").is_none());
    }

    #[test]
    fn platform_update_identity_never_falls_back_to_legacy_root_fields() {
        let complete = serde_json::json!({
            "version": "1.2.3",
            "url": "https://legacy.invalid/root.exe",
            "sha256": "b".repeat(64),
            "size": 99,
            "notes": "shared notes",
            "windows": {
                "version": "9.8.7",
                "url": "https://vocalcode.app/VocalCodeSetup.exe",
                "sha256": "a".repeat(64),
                "size": 7_476_736,
            }
        });
        for required in ["version", "url", "sha256", "size"] {
            let mut missing = complete.clone();
            missing["windows"].as_object_mut().unwrap().remove(required);
            assert!(
                update_offer_from_manifest(&missing, "windows").is_none(),
                "platform offer borrowed root {required}"
            );
        }

        let offer = update_offer_from_manifest(&complete, "windows").unwrap();
        assert_eq!(offer.notes, "shared notes");
    }

    #[test]
    fn update_manifest_requires_canonical_lowercase_sha256() {
        let manifest = |sha256: String| {
            serde_json::json!({
                "version": "9.8.7",
                "url": "https://vocalcode.app/VocalCodeSetup.exe",
                "sha256": sha256,
                "size": 1,
            })
        };
        assert!(update_offer_from_manifest(&manifest("a".repeat(64)), "windows").is_some());
        for invalid in [
            "".to_string(),
            "a".repeat(63),
            "a".repeat(65),
            "A".repeat(64),
            format!("{}g", "a".repeat(63)),
        ] {
            assert!(update_offer_from_manifest(&manifest(invalid), "windows").is_none());
        }
    }
}
