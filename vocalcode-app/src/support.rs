//! About & help support tooling: a durable trace of the last panic, the
//! one-time "closed unexpectedly" notice, and the diagnostics text a user may
//! choose to paste into a bug report. Nothing here uploads anything; every
//! outward link is a fixed GitHub page opened in the user's own browser.
//!
//! Release builds use `panic = "abort"`, so without this a panic ended the
//! process with no line in `vocalcode.log` and nothing on screen: the only
//! evidence of a crash was the app no longer being there.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const SOURCE_URL: &str = "https://github.com/wudaming00/vocalcode-community";
pub const REPORT_URL: &str = "https://github.com/wudaming00/vocalcode-community/issues/new/choose";
pub const PRIVACY_URL: &str =
    "https://github.com/wudaming00/vocalcode-community#privacy-and-network-access";

pub const LOG_FILE_NAME: &str = "vocalcode.log";
pub const LOG_BACKUP_NAME: &str = "vocalcode.log.1";
/// Present only between a panic and the next GUI start that reads it.
const CRASH_MARKER_NAME: &str = "last-crash.json";
const CRASH_MARKER_MAX_BYTES: usize = 4096;
const PANIC_MESSAGE_MAX_CHARS: usize = 400;
pub const DIAGNOSTIC_LOG_LINES: usize = 200;
const DIAGNOSTIC_LINE_MAX_CHARS: usize = 1000;
/// Enough for 200 ordinary log lines without reading the whole 2 MiB file.
const DIAGNOSTIC_TAIL_BYTES: u64 = 256 * 1024;

struct CrashRecorder {
    dir: PathBuf,
    /// CLI diagnostics log panics but never leave a notice for the GUI.
    leave_notice: bool,
}

static RECORDER: OnceLock<CrashRecorder> = OnceLock::new();

/// Installed first thing in `main`. It records nothing until
/// [`enable_crash_records`] names the data directory, because early startup
/// modes (uninstall cleanup, the deferred purge helper) must not create it.
/// The previous hook still runs afterwards, so stderr keeps the standard
/// message and any requested backtrace.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(recorder) = RECORDER.get() {
            let thread = std::thread::current();
            let location = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
            record_panic(
                recorder,
                thread.name().unwrap_or("<unnamed>"),
                location.as_deref(),
                info.payload_as_str(),
            );
        }
        previous(info);
    }));
}

/// Arm the hook once the data directory holds the log. Only the first call
/// counts; the directory never changes during a process lifetime.
pub fn enable_crash_records(dir: &Path, gui: bool) {
    let _ = RECORDER.set(CrashRecorder {
        dir: dir.to_path_buf(),
        leave_notice: gui,
    });
}

/// Deliberately not `log::error!`: a panic raised while the logger holds its
/// own lock would deadlock the hook. The sink below takes the same bounded
/// (250 ms) process and file locks as ordinary log writes instead.
fn record_panic(
    recorder: &CrashRecorder,
    thread: &str,
    location: Option<&str>,
    payload: Option<&str>,
) {
    let location = location.unwrap_or("an unknown location");
    let message = payload
        .map(sanitize_panic_message)
        .unwrap_or_else(|| "non-text panic payload".to_string());
    let stamp = jiff::Timestamp::now()
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let line = panic_log_line(&stamp, thread, location, &message);
    crate::LogFileSink::new(&recorder.dir, crate::LOG_MAX_BYTES).append(line.as_bytes());
    if recorder.leave_notice && panic_ends_process(thread) {
        let notice = CrashNotice {
            at: crate::now_unix(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            thread: thread.to_string(),
            location: location.to_string(),
        };
        if let Ok(bytes) = serde_json::to_vec(&notice) {
            if let Err(error) =
                crate::storage::atomic_write(&recorder.dir.join(CRASH_MARKER_NAME), bytes)
            {
                eprintln!("could not record the crash marker: {error}");
            }
        }
    }
}

/// Release builds abort on any panic. With unwinding (developer builds) a
/// worker panic can be caught and joined while the app keeps running, which
/// is not "closed unexpectedly"; the main thread's panic still ends it.
fn panic_ends_process(thread: &str) -> bool {
    cfg!(panic = "abort") || thread == "main"
}

fn panic_log_line(stamp: &str, thread: &str, location: &str, message: &str) -> String {
    format!("[{stamp} ERROR panic] thread '{thread}' panicked at {location}: {message}\n")
}

/// The log is not a transcript log, and a panic must not turn it into one.
/// The standard library's string-slicing panics quote the string they were
/// slicing — which in this app can be dictated text — so that quotation is
/// dropped. Everything is kept to one bounded line.
fn sanitize_panic_message(message: &str) -> String {
    let mut kept = message;
    if message.starts_with("byte index ") || message.starts_with("begin <= end") {
        if let Some(cut) = ["; it is inside", " of `", " slicing `"]
            .iter()
            .filter_map(|marker| message.find(marker))
            .min()
        {
            kept = &message[..cut];
        }
    }
    let single_line = kept
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>();
    truncate_chars(&single_line, PANIC_MESSAGE_MAX_CHARS)
}

fn truncate_chars(value: &str, limit: usize) -> String {
    match value.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &value[..end]),
        None => value.to_string(),
    }
}

/// What the Settings window needs to say that the previous session crashed.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CrashNotice {
    pub at: u64,
    pub version: String,
    pub thread: String,
    pub location: String,
}

/// Consume the marker left by a panic. Removing it here makes the notice
/// one-time: it lives in memory for this session until the user dismisses it,
/// and a later start without a new crash says nothing. An unreadable or
/// malformed marker still proves a crash happened, so it is reported with
/// empty details rather than ignored.
pub fn take_crash_notice(dir: &Path) -> Option<CrashNotice> {
    let path = dir.join(CRASH_MARKER_NAME);
    let bytes = match read_marker(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            log::warn!("could not read the crash marker: {error}");
            Vec::new()
        }
    };
    if let Err(error) = std::fs::remove_file(&path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            log::warn!("could not remove the crash marker: {error}");
        }
    }
    let notice = serde_json::from_slice::<CrashNotice>(&bytes).unwrap_or_default();
    let known = |value: &str| if value.is_empty() { "?" } else { value }.to_string();
    log::warn!(
        "the previous session ended unexpectedly (VocalCode {}, panic in thread '{}' at {}); see the panic line earlier in this log",
        known(&notice.version),
        known(&notice.thread),
        known(&notice.location),
    );
    Some(notice)
}

fn read_marker(path: &Path) -> std::io::Result<Vec<u8>> {
    // Same no-follow, regular-file-only open as the log itself.
    let file = crate::open_private_regular_nofollow(path, |options| {
        options.read(true);
    })?;
    let mut bytes = Vec::new();
    file.take(CRASH_MARKER_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > CRASH_MARKER_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "crash marker exceeds its size limit",
        ));
    }
    Ok(bytes)
}

/// The newest `limit` log lines, oldest first, reaching into the rotated
/// backup when the active file is short (for example just after rotation).
pub fn log_tail(dir: &Path, limit: usize) -> Vec<String> {
    let mut lines = tail_lines(&dir.join(LOG_BACKUP_NAME), limit);
    lines.extend(tail_lines(&dir.join(LOG_FILE_NAME), limit));
    let skip = lines.len().saturating_sub(limit);
    lines.split_off(skip)
}

fn tail_lines(path: &Path, limit: usize) -> Vec<String> {
    let Ok(mut file) = crate::open_private_regular_nofollow(path, |options| {
        options.read(true);
    }) else {
        return Vec::new();
    };
    let length = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = length.saturating_sub(DIAGNOSTIC_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file
        .take(DIAGNOSTIC_TAIL_BYTES)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines().collect::<Vec<_>>();
    // A tail that starts mid-file starts mid-line; that fragment is noise.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let skip = lines.len().saturating_sub(limit);
    lines[skip..]
        .iter()
        .map(|line| truncate_chars(line, DIAGNOSTIC_LINE_MAX_CHARS))
        .collect()
}

/// Facts for a bug report. Deliberately no transcript, history, dictionary,
/// meeting or calendar content: only what identifies the build and setup.
pub struct DiagnosticFacts<'a> {
    pub version: &'a str,
    pub os: &'a str,
    pub arch: &'a str,
    pub hardware: &'a str,
    pub language: &'a str,
    pub model: &'a str,
    pub model_status: &'a str,
    pub microphone: &'a str,
}

pub fn diagnostics_report(
    facts: &DiagnosticFacts<'_>,
    log_lines: &[String],
    home: Option<&Path>,
) -> String {
    let mut report = format!(
        "VocalCode diagnostics\n\
         Version: {}\n\
         OS: {} {}\n\
         Hardware: {}\n\
         Spoken language: {}\n\
         Model: {} ({})\n\
         Microphone: {}\n\
         \n\
         Last {} lines of {} (no transcripts are written to this log):\n",
        facts.version,
        facts.os,
        facts.arch,
        facts.hardware,
        facts.language,
        facts.model,
        facts.model_status,
        facts.microphone,
        log_lines.len(),
        LOG_FILE_NAME,
    );
    for line in log_lines {
        report.push_str(line);
        report.push('\n');
    }
    redact_home(&report, home)
}

/// Log lines carry absolute paths such as the data directory, and the
/// profile folder in them is usually the person's name. Diagnostics are
/// meant to be pasted into a public issue, so that prefix becomes `~`.
fn redact_home(text: &str, home: Option<&Path>) -> String {
    let Some(home) = home.map(|path| path.to_string_lossy().into_owned()) else {
        return text.to_string();
    };
    let home = home.trim_end_matches(['/', '\\']);
    // Never rewrite a drive root or `/`: that would mangle every path.
    if home.chars().filter(|c| *c == '/' || *c == '\\').count() < 1 || home.len() < 4 {
        return text.to_string();
    }
    let mut redacted = text.replace(home, "~");
    let flipped = home.replace('\\', "/");
    if flipped != home {
        redacted = redacted.replace(&flipped, "~");
    }
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "vocalcode-support-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn panic_record_reaches_the_log_and_leaves_a_one_time_gui_notice() {
        let scratch = Scratch::new("record");
        let recorder = CrashRecorder {
            dir: scratch.0.clone(),
            leave_notice: true,
        };
        record_panic(
            &recorder,
            "main",
            Some("vocalcode-app/src/main.rs:12:5"),
            Some("config lock poisoned"),
        );
        let log = std::fs::read_to_string(scratch.0.join(LOG_FILE_NAME)).unwrap();
        assert!(log.contains(
            "ERROR panic] thread 'main' panicked at vocalcode-app/src/main.rs:12:5: config lock poisoned\n"
        ));

        let notice = take_crash_notice(&scratch.0).expect("the crash must be reported");
        assert_eq!(notice.thread, "main");
        assert_eq!(notice.location, "vocalcode-app/src/main.rs:12:5");
        assert_eq!(notice.version, env!("CARGO_PKG_VERSION"));
        assert!(notice.at > 0);
        assert!(!scratch.0.join(CRASH_MARKER_NAME).exists());
        assert_eq!(take_crash_notice(&scratch.0), None, "one-time only");
    }

    const HOOK_TEST_DIR: &str = "VOCALCODE_PANIC_HOOK_TEST_DIR";

    /// Runs only in the child process started by the test below, where
    /// replacing the process-wide panic hook cannot affect other tests.
    #[test]
    #[ignore = "subprocess helper for the installed panic hook contract"]
    fn panic_hook_child_helper() {
        let Some(dir) = std::env::var_os(HOOK_TEST_DIR) else {
            return;
        };
        install_panic_hook();
        enable_crash_records(Path::new(&dir), true);
        // Named like the real main thread, whose panic ends even an
        // unwinding (test) build. A slicing panic quotes its input.
        let _ = std::thread::Builder::new()
            .name("main".to_string())
            .spawn(|| {
                let dictated = String::from("secret dictation 你好");
                let end = dictated.len() - 1;
                std::hint::black_box(&dictated[..end]).len()
            })
            .unwrap()
            .join();
    }

    #[test]
    fn installed_hook_records_a_real_panic_and_keeps_the_standard_report() {
        let scratch = Scratch::new("hook");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "support::tests::panic_hook_child_helper",
                "--ignored",
                "--nocapture",
            ])
            .env(HOOK_TEST_DIR, &scratch.0)
            .output()
            .unwrap();
        let log = std::fs::read_to_string(scratch.0.join(LOG_FILE_NAME)).unwrap();
        assert!(
            log.contains("ERROR panic] thread 'main' panicked at "),
            "{log}"
        );
        assert!(log.contains("support.rs:"), "{log}");
        assert!(log.contains("is not a char boundary"), "{log}");
        assert!(!log.contains("secret"), "{log}");
        // The chained standard hook still reports on stderr.
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("is not a char boundary"), "{stderr}");
        let notice = take_crash_notice(&scratch.0).expect("marker for the next start");
        assert_eq!(notice.thread, "main");
        assert!(notice.location.contains("support.rs:"));
    }

    #[test]
    fn cli_panics_are_logged_without_a_gui_notice() {
        let scratch = Scratch::new("cli");
        let recorder = CrashRecorder {
            dir: scratch.0.clone(),
            leave_notice: false,
        };
        record_panic(&recorder, "main", None, None);
        let log = std::fs::read_to_string(scratch.0.join(LOG_FILE_NAME)).unwrap();
        assert!(log.contains("panicked at an unknown location: non-text panic payload"));
        assert_eq!(take_crash_notice(&scratch.0), None);
    }

    #[test]
    fn malformed_markers_still_report_a_crash_and_are_removed() {
        let scratch = Scratch::new("malformed");
        std::fs::write(scratch.0.join(CRASH_MARKER_NAME), b"{not json").unwrap();
        assert_eq!(take_crash_notice(&scratch.0), Some(CrashNotice::default()));
        assert!(!scratch.0.join(CRASH_MARKER_NAME).exists());
    }

    #[test]
    fn only_process_ending_panics_leave_a_notice() {
        assert!(panic_ends_process("main"));
        assert_eq!(
            panic_ends_process("vocalcode-engine"),
            cfg!(panic = "abort")
        );
    }

    #[test]
    fn slicing_panics_never_copy_dictated_text_into_the_log() {
        for (message, kept) in [
            (
                "byte index 4 is not a char boundary; it is inside '好' (bytes 3..6) of `你好 secret dictation`",
                "byte index 4 is not a char boundary",
            ),
            (
                "byte index 99 is out of bounds of `secret dictation`",
                "byte index 99 is out of bounds",
            ),
            (
                "begin <= end (4 <= 2) when slicing `secret dictation`",
                "begin <= end (4 <= 2) when",
            ),
        ] {
            let sanitized = sanitize_panic_message(message);
            assert_eq!(sanitized, kept);
            assert!(!sanitized.contains("secret"));
        }
        assert_eq!(
            sanitize_panic_message("called `Option::unwrap()` on a `None` value"),
            "called `Option::unwrap()` on a `None` value"
        );
        assert_eq!(sanitize_panic_message("two\nlines"), "two lines");
        let long = sanitize_panic_message(&"x".repeat(PANIC_MESSAGE_MAX_CHARS + 50));
        assert_eq!(long.chars().count(), PANIC_MESSAGE_MAX_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn log_tail_is_bounded_and_continues_into_the_rotated_backup() {
        let scratch = Scratch::new("tail");
        let backup = (0..5).map(|i| format!("old {i}\n")).collect::<String>();
        let active = (0..3).map(|i| format!("new {i}\n")).collect::<String>();
        std::fs::write(scratch.0.join(LOG_BACKUP_NAME), backup).unwrap();
        std::fs::write(scratch.0.join(LOG_FILE_NAME), active).unwrap();
        assert_eq!(
            log_tail(&scratch.0, 5),
            ["old 3", "old 4", "new 0", "new 1", "new 2"]
        );
        assert_eq!(log_tail(&scratch.0, 2), ["new 1", "new 2"]);

        let long_line = "y".repeat(DIAGNOSTIC_LINE_MAX_CHARS * 2);
        std::fs::write(scratch.0.join(LOG_FILE_NAME), format!("{long_line}\n")).unwrap();
        assert_eq!(
            log_tail(&scratch.0, 1)[0].chars().count(),
            DIAGNOSTIC_LINE_MAX_CHARS + 1
        );
        assert!(log_tail(&Scratch::new("empty").0, DIAGNOSTIC_LOG_LINES).is_empty());
    }

    #[test]
    fn diagnostics_identify_the_setup_and_hide_the_profile_folder() {
        let facts = DiagnosticFacts {
            version: "1.4.0",
            os: "windows",
            arch: "x86_64",
            hardware: "8 cores · 16 GB RAM · standard",
            language: "zh",
            model: "SenseVoice",
            model_status: "ready",
            microphone: "USB microphone",
        };
        let home = Path::new(r"C:\Users\Jane Doe");
        let lines = vec![
            r"[t INFO vocalcode_app] VocalCode 1.4.0 starting — log at C:\Users\Jane Doe\AppData\Local\VocalCode\vocalcode.log".to_string(),
            "[t INFO vocalcode_app] model at C:/Users/Jane Doe/models".to_string(),
        ];
        let report = diagnostics_report(&facts, &lines, Some(home));
        for expected in [
            "Version: 1.4.0",
            "OS: windows x86_64",
            "Hardware: 8 cores · 16 GB RAM · standard",
            "Spoken language: zh",
            "Model: SenseVoice (ready)",
            "Microphone: USB microphone",
            "Last 2 lines of vocalcode.log",
            r"log at ~\AppData\Local\VocalCode\vocalcode.log",
            "model at ~/models",
        ] {
            assert!(report.contains(expected), "{expected}\n{report}");
        }
        assert!(!report.contains("Jane Doe"));
        // A root "home" must never rewrite every path in the report.
        assert_eq!(redact_home(r"C:\x", Some(Path::new(r"C:\"))), r"C:\x");
        assert_eq!(redact_home("/x/y", Some(Path::new("/"))), "/x/y");
        assert_eq!(redact_home("/Users/jd/x", None), "/Users/jd/x");
    }

    #[test]
    fn help_links_are_fixed_public_github_pages() {
        for url in [SOURCE_URL, REPORT_URL, PRIVACY_URL] {
            assert!(url.starts_with("https://github.com/wudaming00/vocalcode-community"));
        }
        assert!(REPORT_URL.ends_with("/issues/new/choose"));
        assert!(PRIVACY_URL.ends_with("#privacy-and-network-access"));
        let readme = include_str!("../../README.md");
        assert!(readme.contains("\n## Privacy and network access\n"));
    }
}
