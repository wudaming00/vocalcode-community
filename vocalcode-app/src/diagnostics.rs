//! Opt-in, account-encrypted text diagnostics. No raw audio and no network.
//! Count/byte limits stop new entries; no age cutoff or silent deletion.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
};
use vocalcode_core::engine::DictationTrace;

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
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir(base)?).map_err(|e| e.to_string())? {
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

pub(crate) struct Writer {
    sender: Option<mpsc::SyncSender<(Record, crate::workflows::Preferences)>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Writer {
    pub fn start(base: PathBuf, status: Arc<crate::webui::RuntimeStatus>) -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel::<(Record, crate::workflows::Preferences)>(64);
        let worker = std::thread::Builder::new().name("vocalcode-diagnostics".into()).spawn(move || {
            let mut inventory: Option<(u64,u64)> = None;
            while let Ok((record,prefs)) = receiver.recv() {
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
    pub fn append(
        &self,
        record: Record,
        prefs: crate::workflows::Preferences,
    ) -> Result<(), String> {
        self.sender
            .as_ref()
            .ok_or("Diagnostic writer stopped")?
            .try_send((record, prefs))
            .map_err(|_| {
                "Diagnostic queue full; this entry was not saved. Dictation continues.".into()
            })
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
}
