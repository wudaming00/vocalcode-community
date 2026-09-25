//! Opt-in, account-encrypted text diagnostics. No raw audio and no network.
//! Count/byte limits stop new entries; no age cutoff or silent deletion.
//!
//! The same encryption and writer thread also keep recent History across a
//! restart ("Keep history" on the History page). That store lives in its own
//! folder with the opposite contract — entries expire by age and count, and
//! turning it off deletes them — so it can never touch a diagnostic record.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
};
use vocalcode_core::{config::HistoryRetention, engine::DictationTrace};

const MAX_RECORD: usize = 512 * 1024;
static SERIAL: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CorrectionPair {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Record {
    schema: u32,
    pub recorded_unix_ms: u64,
    kind: String,
    model: String,
    language: String,
    app_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    app_version: String,
    // Old schema-1 learned_correction records must survive read/export intact.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub corrections: Vec<CorrectionPair>,
    #[serde(flatten)]
    pub trace: DictationTrace,
}
impl Record {
    pub fn dictation(
        trace: DictationTrace,
        model: String,
        language: String,
        app_id: String,
    ) -> Self {
        Self {
            schema: 1,
            recorded_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            kind: "dictation".into(),
            model,
            language,
            app_id,
            app_version: env!("CARGO_PKG_VERSION").into(),
            corrections: Vec::new(),
            trace,
        }
    }
    pub fn correction(kind: &str, pairs: &[(String, String)]) -> Self {
        let mut record = Self::dictation(
            DictationTrace::default(),
            String::new(),
            String::new(),
            String::new(),
        );
        record.kind = kind.into();
        record.trace.result = kind.into();
        record.corrections = pairs
            .iter()
            .map(|(from, to)| CorrectionPair {
                from: from.clone(),
                to: to.clone(),
            })
            .collect();
        record
    }
}

// UI and input threads enqueue only with current text-diagnostics consent.
// The engine drains a bounded queue and rechecks consent before persistence.
pub(crate) fn queue_event(
    status: &crate::webui::RuntimeStatus,
    kind: &str,
    pairs: &[(String, String)],
) {
    if !status
        .workflows
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .diagnostics
    {
        return;
    }
    let mut pending = status
        .diagnostic_events
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if pending.len() < 64 {
        pending.push_back(Record::correction(kind, pairs));
    } else {
        status
            .runtime_errors
            .push("Diagnostic event queue full; this event was not saved.".to_string());
    }
}

#[cfg(windows)]
pub(crate) fn protect(bytes: &[u8], encrypt: bool) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    };
    // Keep compatibility with earlier opt-in Windows diagnostic files.
    const ENTROPY: &[u8] = b"VocalCode local diagnostic history v1";
    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(bytes.len()).map_err(|e| e.to_string())?,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: ENTROPY.len() as u32,
        pbData: ENTROPY.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let ok = unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                std::ptr::null(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if ok == 0 {
        return Err("Windows could not access account-encrypted diagnostic storage.".into());
    }
    let result = unsafe {
        let data = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(output.pbData.cast());
        data
    };
    Ok(result)
}

#[cfg(target_os = "macos")]
pub(crate) fn protect(bytes: &[u8], encrypt: bool) -> Result<Vec<u8>, String> {
    use ring::{
        aead,
        rand::{SecureRandom, SystemRandom},
    };
    use security_framework::passwords::{get_generic_password, set_generic_password};
    static KEY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _key_guard = KEY_LOCK
        .lock()
        .map_err(|_| "Diagnostic key initialization failed")?;
    const SERVICE: &str = "app.vocalcode.diagnostics.v1";
    // A missing key during decryption is an error, never an invitation to
    // replace the key and strand existing records.
    let secret = match get_generic_password(SERVICE, "local-history") {
        Ok(secret) => secret,
        Err(e) if encrypt && e.code() == -25300 => {
            let mut secret = vec![0u8; 32];
            SystemRandom::new()
                .fill(&mut secret)
                .map_err(|_| "Secure randomness unavailable")?;
            set_generic_password(SERVICE, "local-history", &secret)
                .map_err(|_| "Cannot save diagnostic key in Keychain")?;
            secret
        }
        Err(_) => return Err("Cannot access the diagnostic encryption key in Keychain.".into()),
    };
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, &secret).map_err(|_| "Invalid diagnostic key")?,
    );
    if encrypt {
        let mut nonce = [0u8; 12];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| "Secure randomness unavailable")?;
        let mut output = bytes.to_vec();
        key.seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(SERVICE.as_bytes()),
            &mut output,
        )
        .map_err(|_| "Cannot encrypt diagnostics")?;
        let mut envelope = b"VCD1".to_vec();
        envelope.extend(nonce);
        envelope.extend(output);
        Ok(envelope)
    } else {
        if bytes.len() < 32 || &bytes[..4] != b"VCD1" {
            return Err("Unsupported diagnostic record".into());
        }
        let nonce: [u8; 12] = bytes[4..16].try_into().map_err(|_| "Invalid nonce")?;
        let mut body = bytes[16..].to_vec();
        let plain = key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(SERVICE.as_bytes()),
                &mut body,
            )
            .map_err(|_| "Cannot authenticate diagnostic record")?;
        Ok(plain.to_vec())
    }
}
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn protect(_: &[u8], _: bool) -> Result<Vec<u8>, String> {
    Err("Encrypted storage is unavailable on this platform.".into())
}
pub(crate) fn check_encryption() -> Result<(), String> {
    let encrypted = protect(b"storage check", true)?;
    if protect(&encrypted, false)? != b"storage check" {
        return Err("Encrypted storage verification failed".into());
    }
    Ok(())
}
fn dir(base: &Path) -> Result<PathBuf, String> {
    crate::paths::ensure_trusted_data_subdir(base, Path::new("diagnostic-history"))
        .map_err(|e| e.to_string())
}
fn files(base: &Path) -> Result<Vec<(u64, PathBuf, u64)>, String> {
    files_in(&dir(base)?)
}
/// Records in `directory`, newest first: (time in ms from the name, path, size).
fn files_in(directory: &Path) -> Result<Vec<(u64, PathBuf, u64)>, String> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("vcd") {
            continue;
        }
        let Some(time) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.split('-').next())
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        let metadata = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if !metadata.file_type().is_file() {
            continue;
        }
        files.push((time, path, metadata.len()));
    }
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    Ok(files)
}
fn read(path: &Path) -> Result<Record, String> {
    let bytes =
        crate::read_bounded_bytes(path, MAX_RECORD).map_err(|_| "Cannot read diagnostic record")?;
    let record: Record = serde_json::from_slice(&protect(&bytes, false)?)
        .map_err(|_| "Invalid diagnostic record")?;
    if record.schema != 1 {
        return Err("Unsupported diagnostic schema".into());
    }
    Ok(record)
}
pub(crate) fn page(base: &Path, offset: usize) -> Result<Value, String> {
    let files = files(base)?;
    let mut entries = Vec::new();
    let mut unreadable = 0;
    for (_, path, _) in files.iter().skip(offset).take(20) {
        match read(path) {
            Ok(record) => entries.push(record),
            Err(_) => unreadable += 1,
        }
    }
    Ok(
        json!({"entries":entries,"offset":offset,"total":files.len(),"bytes":files.iter().map(|f| f.2).sum::<u64>(),"unreadable":unreadable}),
    )
}
pub(crate) fn inventory(base: &Path) -> Result<Value, String> {
    let files = files(base)?;
    let latest = files.first().map(|(_, path, _)| read(path)).transpose()?;
    Ok(
        json!({"total":files.len(),"bytes":files.iter().map(|f| f.2).sum::<u64>(),
        "latest_record_unix_ms":latest.map(|r|r.recorded_unix_ms)}),
    )
}
pub(crate) fn recent(base: &Path) -> Result<Vec<crate::webui::HistoryEntry>, String> {
    Ok(files(base)?
        .iter()
        .take(50)
        .filter_map(|(_, path, _)| read(path).ok())
        .filter(|r| {
            (!r.trace.final_text.is_empty() || r.trace.filler_removed > 0) && r.kind == "dictation"
        })
        .map(|r| {
            crate::webui::HistoryEntry::new(r.recorded_unix_ms / 1000, r.trace.final_text.clone())
                .with_trace(&r.trace)
        })
        .collect())
}
pub(crate) fn export(base: &Path, destination: &Path) -> Result<usize, String> {
    use std::io::Write;
    let files = files(base)?;
    // Stream one bounded decrypted entry at a time. A corrupt record aborts
    // atomically; it cannot silently yield a supposedly complete export.
    crate::storage::atomic_write_stream(destination, |out| {
        for (_, path, _) in &files {
            let record = read(path).map_err(std::io::Error::other)?;
            serde_json::to_writer(&mut *out, &record)?;
            out.write_all(b"\n")?;
        }
        Ok(())
    })
    .map_err(|e| e.to_string())?;
    Ok(files.len())
}

// ---------------------------------------------------------------------------
// Kept History
// ---------------------------------------------------------------------------

/// Where "Keep history" puts recent dictations. Separate from
/// `diagnostic-history`: these expire, diagnostics never do.
const KEPT_DIR: &str = "dictation-history";
const KEPT_SCHEMA: u32 = 1;
/// Encrypted storage exists only where `protect` has a backend. Elsewhere the
/// setting behaves as Off rather than failing on every dictation, and the
/// page does not offer it.
pub(crate) const KEPT_SUPPORTED: bool = cfg!(any(windows, target_os = "macos"));
/// How far past the current clock a kept entry's name may be. One written
/// while the clock ran ahead would otherwise never reach its age limit and
/// would stay among the newest [`HistoryRetention::MAX_ENTRIES`] for good,
/// pushing real new dictations out, so beyond this it counts as expired.
const KEPT_CLOCK_SLACK_MS: u64 = 5 * 60 * 1000;

#[derive(Serialize, Deserialize)]
struct Kept {
    schema: u32,
    entry: crate::webui::HistoryEntry,
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn kept_dir(base: &Path) -> Result<PathBuf, String> {
    crate::paths::ensure_trusted_data_subdir(base, Path::new(KEPT_DIR)).map_err(|e| e.to_string())
}

/// The kept-history folder only if it is already there. Reading or pruning
/// with the setting off must never be what creates it.
fn existing_kept_dir(base: &Path) -> Result<Option<PathBuf>, String> {
    match std::fs::symlink_metadata(base.join(KEPT_DIR)) {
        Ok(_) => kept_dir(base).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// Whether an entry saved at `saved_ms` is still inside the window that
/// starts at `cutoff`: not past its age, and not stamped in the future.
fn kept_in_window(saved_ms: u64, cutoff: u64, now_ms: u64) -> bool {
    saved_ms >= cutoff && saved_ms <= now_ms.saturating_add(KEPT_CLOCK_SLACK_MS)
}

/// Delete what `retention` no longer allows: everything when it is Off,
/// otherwise entries past their age (or stamped in the future) and all but
/// the newest [`HistoryRetention::MAX_ENTRIES`]. Returns how many were removed.
pub(crate) fn prune_kept(
    base: &Path,
    retention: HistoryRetention,
    now_ms: u64,
) -> Result<usize, String> {
    let Some(directory) = existing_kept_dir(base)? else {
        return Ok(0);
    };
    let cutoff = retention
        .max_age_secs()
        .map(|secs| now_ms.saturating_sub(secs.saturating_mul(1000)));
    let mut removed = 0;
    let mut kept = 0;
    let mut failed = false;
    for (saved_ms, path, _) in files_in(&directory)? {
        let allowed = cutoff.is_some_and(|cutoff| {
            kept < HistoryRetention::MAX_ENTRIES && kept_in_window(saved_ms, cutoff, now_ms)
        });
        if allowed {
            kept += 1;
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                log::warn!("could not remove kept history {}: {error}", path.display());
                failed = true;
            }
        }
    }
    if cutoff.is_none() {
        // Only succeeds once nothing is left; anything unexpected stays put.
        let _ = std::fs::remove_dir(&directory);
    }
    if failed {
        return Err(
            "Some saved History entries could not be deleted yet; VocalCode will try again.".into(),
        );
    }
    Ok(removed)
}

fn read_kept(path: &Path) -> Result<crate::webui::HistoryEntry, String> {
    let bytes =
        crate::read_bounded_bytes(path, MAX_RECORD).map_err(|_| "Cannot read a History entry")?;
    let kept: Kept =
        serde_json::from_slice(&protect(&bytes, false)?).map_err(|_| "Invalid History entry")?;
    if kept.schema != KEPT_SCHEMA {
        return Err("Unsupported History entry".into());
    }
    Ok(kept.entry)
}

/// Entries `retention` still allows, newest first, for the list at startup.
/// An entry that cannot be read is skipped, never shown half-decrypted.
pub(crate) fn recent_kept(
    base: &Path,
    retention: HistoryRetention,
    now_ms: u64,
) -> Result<Vec<crate::webui::HistoryEntry>, String> {
    let Some(max_age) = retention.max_age_secs() else {
        return Ok(Vec::new());
    };
    let Some(directory) = existing_kept_dir(base)? else {
        return Ok(Vec::new());
    };
    let cutoff = now_ms.saturating_sub(max_age.saturating_mul(1000));
    Ok(files_in(&directory)?
        .into_iter()
        .filter(|(saved_ms, _, _)| kept_in_window(*saved_ms, cutoff, now_ms))
        .take(HistoryRetention::MAX_ENTRIES)
        .filter_map(|(_, path, _)| read_kept(&path).ok())
        .collect())
}

fn write_kept(base: &Path, entry: crate::webui::HistoryEntry, saved_ms: u64) -> Result<(), String> {
    let serialized = serde_json::to_vec(&Kept {
        schema: KEPT_SCHEMA,
        entry,
    })
    .map_err(|e| e.to_string())?;
    if serialized.len() > MAX_RECORD - 4096 {
        return Err(
            "This dictation is too long to keep on disk; it stays in History until VocalCode quits."
                .into(),
        );
    }
    let encrypted = protect(&serialized, true)?;
    let path = kept_dir(base)?.join(format!(
        "{saved_ms}-{}-{:020}.vcd",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    crate::storage::atomic_write_new(&path, &encrypted).map_err(|_| {
        "Could not keep this dictation on disk; check disk space and permissions. It stays in History until VocalCode quits."
            .to_string()
    })
}

enum Job {
    // Boxed: a diagnostic record dwarfs the other jobs in the queue.
    Diagnostic(Box<(Record, crate::workflows::Preferences)>),
    Keep(crate::webui::HistoryEntry, HistoryRetention),
    Retain(HistoryRetention),
}

/// One background thread owns every encrypted write, so a slow disk or a
/// Keychain prompt never holds up dictation.
pub(crate) struct Writer {
    sender: Option<mpsc::SyncSender<Job>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Writer {
    pub fn start(base: PathBuf, status: Arc<crate::webui::RuntimeStatus>) -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel::<Job>(64);
        let worker = std::thread::Builder::new().name("vocalcode-diagnostics".into()).spawn(move || {
            let mut inventory: Option<(u64,u64)> = None;
            // One report per run of failures, not one per dictation.
            let mut kept_failing = false;
            while let Ok(job) = receiver.recv() {
                let (record,prefs) = match job {
                    Job::Diagnostic(job) => *job,
                    Job::Keep(entry, retention) => {
                        let result = if retention.keeps() && KEPT_SUPPORTED {
                            write_kept(&base, entry, now_ms()).and_then(|()| prune_kept(&base, retention, now_ms()).map(drop))
                        } else {
                            Ok(())
                        };
                        report_kept(&status, result, &mut kept_failing);
                        continue;
                    }
                    Job::Retain(retention) => {
                        let result = prune_kept(&base, retention, now_ms()).map(drop);
                        report_kept(&status, result, &mut kept_failing);
                        continue;
                    }
                };
                let result = (|| -> Result<(),String> {
                    let (count,bytes) = match inventory {
                        Some(value) => value,
                        None => { let files=files(&base)?; (files.len() as u64, files.iter().map(|f| f.2).sum()) }
                    };
                    inventory=Some((count,bytes));
                    let serialized = serde_json::to_vec(&record).map_err(|e|e.to_string())?;
                    if serialized.len()>MAX_RECORD-4096 { return Err("Diagnostic entry is too large; it was not saved.".into()); }
                    let encrypted=protect(&serialized,true)?;
                    if (prefs.max_entries>0 && count>=prefs.max_entries) || (prefs.max_bytes>0 && bytes+encrypted.len() as u64>prefs.max_bytes) {
                        return Err("Diagnostic storage limit reached. Earlier records are preserved; raise the limit to save new records.".into());
                    }
                    let path=dir(&base)?.join(format!("{}-{}-{:020}.vcd",record.recorded_unix_ms,std::process::id(),SERIAL.fetch_add(1,Ordering::Relaxed)));
                    crate::storage::atomic_write_new(&path,&encrypted).map_err(|_|"Could not save diagnostics; check disk space and permissions.")?;
                    inventory=Some((count+1,bytes+encrypted.len() as u64)); Ok(())
                })();
                if let Err(error)=result { status.runtime_errors.push(error); }
            }
        }).map_err(|e|e.to_string())?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
        })
    }
    fn send(&self, job: Job, full: &str) -> Result<(), String> {
        self.sender
            .as_ref()
            .ok_or("Diagnostic writer stopped")?
            .try_send(job)
            .map_err(|_| full.to_string())
    }
    pub fn append(
        &self,
        record: Record,
        prefs: crate::workflows::Preferences,
    ) -> Result<(), String> {
        self.send(
            Job::Diagnostic(Box::new((record, prefs))),
            "Diagnostic queue full; this entry was not saved. Dictation continues.",
        )
    }
    /// Keep one History entry on disk under `retention`, then drop whatever
    /// that retention no longer allows.
    pub fn keep(
        &self,
        entry: crate::webui::HistoryEntry,
        retention: HistoryRetention,
    ) -> Result<(), String> {
        self.send(
            Job::Keep(entry, retention),
            "History is busy; this dictation stays in the list until VocalCode quits but was not kept on disk.",
        )
    }
    /// Apply `retention` to what is already kept: after the setting changes,
    /// and periodically so entries expire while the app stays open.
    pub fn retain(&self, retention: HistoryRetention) -> Result<(), String> {
        self.send(
            Job::Retain(retention),
            "History is busy; saved entries will be tidied up shortly.",
        )
    }
}
fn report_kept(
    status: &crate::webui::RuntimeStatus,
    result: Result<(), String>,
    failing: &mut bool,
) {
    match result {
        Ok(()) => *failing = false,
        Err(error) => {
            log::warn!("kept history: {error}");
            if !std::mem::replace(failing, true) {
                status.runtime_errors.push(error);
            }
        }
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_correction_pairs_survive_schema_one_roundtrip() {
        let old = r#"{"schema":1,"kind":"learned_correction","recorded_unix_ms":123,"result":"learned","corrections":[{"from":"Collie","to":"考虑"},{"from":"prome","to":"prompt"}]}"#;
        let record: Record = serde_json::from_str(old).unwrap();
        assert_eq!(record.corrections.len(), 2);
        let saved = serde_json::to_value(&record).unwrap();
        assert_eq!(
            saved["corrections"],
            serde_json::from_str::<Value>(old).unwrap()["corrections"]
        );
        assert_eq!(saved["kind"], "learned_correction");
    }
    #[test]
    fn diagnostic_events_require_consent_and_are_bounded() {
        let status = crate::webui::RuntimeStatus::default();
        queue_event(
            &status,
            "learned_correction",
            &[("heard".into(), "written".into())],
        );
        assert!(status.diagnostic_events.lock().unwrap().is_empty());
        status.workflows.lock().unwrap().diagnostics = true;
        for _ in 0..65 {
            queue_event(&status, "correction_proposed", &[]);
        }
        assert_eq!(status.diagnostic_events.lock().unwrap().len(), 64);
        assert_eq!(
            status.runtime_errors.drain(),
            ["Diagnostic event queue full; this event was not saved."],
            "one failure repeated is reported once"
        );
    }
    // Do not create Keychain entries from macOS unit tests.
    #[cfg(windows)]
    #[test]
    fn windows_records_are_encrypted_and_restore_with_unicode() {
        let base = crate::test_support::TempDir::new("diag-test");
        let status = Arc::new(crate::webui::RuntimeStatus::default());
        let writer = Writer::start(base.to_path_buf(), status.clone()).unwrap();
        let prefs = crate::workflows::Preferences {
            diagnostics: true,
            max_entries: 1,
            ..Default::default()
        };
        let trace = DictationTrace {
            raw_text: "原始 words".into(),
            final_text: "私密 words".into(),
            filler_removed: 1,
            ..Default::default()
        };
        writer
            .append(
                Record::dictation(trace.clone(), "fake".into(), "zh".into(), "test.exe".into()),
                prefs.clone(),
            )
            .unwrap();
        writer
            .append(
                Record::dictation(trace, "fake".into(), "zh".into(), "test.exe".into()),
                prefs,
            )
            .unwrap();
        drop(writer);
        // Dropping the writer joins its thread: no record file is left open.
        crate::test_support::assert_directory_released(&base);
        assert_eq!(recent(&base).unwrap()[0].text, "私密 words");
        assert_eq!(
            recent(&base).unwrap()[0].recognition.as_deref(),
            Some("原始 words")
        );
        assert_eq!(recent(&base).unwrap()[0].filler_removed, 1);
        assert_eq!(page(&base, 0).unwrap()["total"], 1);
        assert!(status
            .runtime_errors
            .drain()
            .last()
            .unwrap()
            .contains("limit reached"));
        let encrypted = std::fs::read(&files(&base).unwrap()[0].1).unwrap();
        assert!(!String::from_utf8_lossy(&encrypted).contains("words"));
        assert_eq!(export(&base, &base.join("test.jsonl")).unwrap(), 1);
    }

    fn scratch(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "vocalcode-kept-{name}-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    const DAY_MS: u64 = 24 * 60 * 60 * 1000;

    /// Stand-ins named exactly like kept records. Pruning decides from the
    /// name alone and never needs to decrypt, so this runs on every platform.
    fn fake_kept(base: &Path, saved_ms: &[u64]) {
        let folder = base.join(KEPT_DIR);
        std::fs::create_dir_all(&folder).unwrap();
        for (index, saved) in saved_ms.iter().enumerate() {
            std::fs::write(folder.join(format!("{saved}-1-{index:020}.vcd")), b"x").unwrap();
        }
    }

    #[test]
    fn off_deletes_kept_history_and_never_creates_the_folder() {
        let base = scratch("off");
        let now = 100 * DAY_MS;
        assert_eq!(prune_kept(&base, HistoryRetention::Off, now).unwrap(), 0);
        assert!(recent_kept(&base, HistoryRetention::Week, now)
            .unwrap()
            .is_empty());
        assert!(
            !base.join(KEPT_DIR).exists(),
            "an install that keeps nothing gets no folder"
        );

        fake_kept(&base, &[now, now - DAY_MS]);
        // Diagnostics are a different contract: Off never touches them.
        let diagnostic = dir(&base).unwrap().join(format!("{now}-1-0.vcd"));
        std::fs::write(&diagnostic, b"evidence").unwrap();
        assert_eq!(prune_kept(&base, HistoryRetention::Off, now).unwrap(), 2);
        assert!(!base.join(KEPT_DIR).exists());
        assert_eq!(std::fs::read(&diagnostic).unwrap(), b"evidence");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn kept_history_expires_by_age_and_count_only() {
        let base = scratch("expiry");
        let now = 100 * DAY_MS;
        fake_kept(
            &base,
            &[now - 2 * DAY_MS, now - DAY_MS / 2, now - 8 * DAY_MS],
        );
        let unrelated = base.join(KEPT_DIR).join("notes.txt");
        std::fs::write(&unrelated, b"not ours").unwrap();

        assert_eq!(prune_kept(&base, HistoryRetention::Week, now).unwrap(), 1);
        assert_eq!(files_in(&base.join(KEPT_DIR)).unwrap().len(), 2);
        assert_eq!(prune_kept(&base, HistoryRetention::Day, now).unwrap(), 1);
        let left = files_in(&base.join(KEPT_DIR)).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].0, now - DAY_MS / 2);
        assert!(unrelated.exists(), "only kept records are ever deleted");

        let stamps: Vec<u64> = (0..(HistoryRetention::MAX_ENTRIES as u64 + 7))
            .map(|index| now - index * 1000)
            .collect();
        fake_kept(&base, &stamps);
        prune_kept(&base, HistoryRetention::Week, now).unwrap();
        let left = files_in(&base.join(KEPT_DIR)).unwrap();
        assert_eq!(left.len(), HistoryRetention::MAX_ENTRIES);
        assert_eq!(left[0].0, now, "the newest are the ones kept");
        std::fs::remove_dir_all(base).unwrap();
    }

    /// Entries written while the clock ran ahead neither outlive their period
    /// nor take the places of real new dictations under the count cap.
    #[test]
    fn kept_history_stamped_in_the_future_counts_as_expired() {
        let base = scratch("future");
        let now = 100 * DAY_MS;
        let future: Vec<u64> = (0..HistoryRetention::MAX_ENTRIES as u64)
            .map(|index| now + DAY_MS + index)
            .collect();
        fake_kept(&base, &future);
        // A clock a little behind the one that wrote an entry is not "ahead".
        fake_kept(&base, &[now + 60_000, now - 1000, now - 2 * DAY_MS]);

        assert_eq!(
            prune_kept(&base, HistoryRetention::Week, now).unwrap(),
            HistoryRetention::MAX_ENTRIES
        );
        let left: Vec<u64> = files_in(&base.join(KEPT_DIR))
            .unwrap()
            .into_iter()
            .map(|(saved, _, _)| saved)
            .collect();
        assert_eq!(left, [now + 60_000, now - 1000, now - 2 * DAY_MS]);
        assert!(!kept_in_window(now + DAY_MS, 0, now));
        assert!(kept_in_window(now + KEPT_CLOCK_SLACK_MS, 0, now));
        std::fs::remove_dir_all(base).unwrap();
    }

    // Do not create Keychain entries from macOS unit tests.
    #[cfg(windows)]
    #[test]
    fn kept_history_is_encrypted_restored_and_removed_by_the_writer() {
        let base = scratch("writer");
        let status = Arc::new(crate::webui::RuntimeStatus::default());
        let writer = Writer::start(base.clone(), status.clone()).unwrap();
        let mut entry = crate::webui::HistoryEntry::new(1_700_000_000, "私密 words".into());
        entry.recognition = Some("um 私密 words".into());
        entry.filler_removed = 1;
        writer.keep(entry.clone(), HistoryRetention::Off).unwrap();
        writer.keep(entry.clone(), HistoryRetention::Week).unwrap();
        writer
            .keep(
                crate::webui::HistoryEntry::new(1_700_000_001, "newer".into()),
                HistoryRetention::Day,
            )
            .unwrap();
        drop(writer);
        assert!(status.runtime_errors.drain().is_empty());

        let kept = files_in(&base.join(KEPT_DIR)).unwrap();
        assert_eq!(kept.len(), 2, "Off writes nothing");
        for (_, path, _) in &kept {
            let bytes = std::fs::read(path).unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("words"));
        }
        let restored = recent_kept(&base, HistoryRetention::Week, now_ms()).unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].text, "newer");
        assert_eq!(restored[1], entry, "the reviewable original survives");
        assert!(recent_kept(&base, HistoryRetention::Off, now_ms())
            .unwrap()
            .is_empty());
        assert!(
            recent(&base).unwrap().is_empty(),
            "kept history is not diagnostics"
        );

        let writer = Writer::start(base.clone(), status.clone()).unwrap();
        writer.retain(HistoryRetention::Off).unwrap();
        drop(writer);
        assert!(!base.join(KEPT_DIR).exists());
        assert!(status.runtime_errors.drain().is_empty());
        std::fs::remove_dir_all(base).unwrap();
    }
}
