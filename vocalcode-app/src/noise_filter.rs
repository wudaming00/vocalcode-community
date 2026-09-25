//! Local acoustic detector provisioning and optional dictation gate. No audio/text persistence, capture,
//! injection or network. Model failure always retains the original ASR path.
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use vocalcode_platform::speech_gate::{Decision, SpeechGate, MODEL_FILE};

// Small acoustic detector only, not an ASR model. Keeping the pinned bytes in
// the signed executable makes first use offline on a clean installation too.
const BUNDLED_MODEL: &[u8] = include_bytes!("../assets/silero-v5.onnx");

/// Meetings use a separate detector, independent of the dictation toggle and
/// progressive mode. No model download or shared recurrent state is involved.
pub(crate) fn meeting_detector(base: &std::path::Path) -> Option<SpeechGate> {
    prepare_model(base)
        .ok()
        .and_then(|path| SpeechGate::load(&path).ok())
}

fn prepare_model(base: &std::path::Path) -> std::io::Result<PathBuf> {
    let directory =
        crate::paths::ensure_trusted_data_subdir(base, std::path::Path::new("models/speech-gate"))?;
    let path = directory.join(MODEL_FILE);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(std::io::Error::other(
                "Speech filter model is not a regular file",
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match crate::storage::atomic_write_new(&path, BUNDLED_MODEL) {
                Ok(()) => {}
                // A concurrent instance may have published it first. The
                // loader still verifies its type, size and pinned hash.
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    }
    // Do not silently replace an existing damaged or user-supplied file.
    Ok(path)
}

#[derive(Default)]
pub struct Control {
    enabled: AtomicBool,
    progressive: AtomicBool,
    generation: AtomicU64,
    snapshot: Mutex<Snapshot>,
}
#[derive(Clone, serde::Serialize)]
pub struct Snapshot {
    pub state: &'static str,
    pub rejected: u64,
    pub checked: u64,
}
impl Default for Snapshot {
    fn default() -> Self {
        Self {
            state: "off",
            rejected: 0,
            checked: 0,
        }
    }
}
impl Control {
    pub fn set_enabled(&self, enabled: bool) {
        if self.enabled.swap(enabled, Ordering::AcqRel) != enabled {
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.state(if enabled { "pending" } else { "off" });
        }
    }
    pub fn set_progressive(&self, progressive: bool) {
        self.progressive.store(progressive, Ordering::Release);
    }
    fn state(&self, state: &'static str) {
        self.snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .state = state;
    }
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

pub struct Filter {
    base: PathBuf,
    control: Arc<Control>,
    detector: Option<SpeechGate>,
    attempted: bool,
    generation: u64,
}
pub trait Gate: Send {
    fn permit(&mut self, samples: &[f32], rate: u32) -> bool;
}
impl Filter {
    pub fn new(base: PathBuf, control: Arc<Control>) -> Self {
        Self {
            base,
            control,
            detector: None,
            attempted: false,
            generation: 0,
        }
    }
}
impl Gate for Filter {
    fn permit(&mut self, samples: &[f32], rate: u32) -> bool {
        if !self.control.enabled.load(Ordering::Acquire) {
            self.control.state("off");
            return true;
        }
        if self.control.progressive.load(Ordering::Acquire) {
            // Progressive phrases can be incomplete; beta gating is on-release
            // only, including exact-app progressive overrides.
            self.control.state("progressive");
            return true;
        }
        let generation = self.control.generation.load(Ordering::Acquire);
        if generation != self.generation {
            self.generation = generation;
            self.attempted = false;
        }
        if self.detector.is_none() && !self.attempted {
            self.attempted = true;
            self.control.state("loading");
            self.detector = prepare_model(&self.base)
                .ok()
                .and_then(|path| SpeechGate::load(&path).ok());
            if self.detector.is_none() {
                log::warn!("Local speech filter unavailable; preserving recognition. Check model integrity and data-directory permissions, then toggle off/on to retry.");
            }
        }
        let Some(detector) = &mut self.detector else {
            self.control.state("unavailable");
            return true;
        };
        let decision = detector.classify(samples, rate);
        let mut snapshot = self
            .control
            .snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        snapshot.checked = snapshot.checked.saturating_add(1);
        snapshot.state = match decision {
            Decision::NoSpeech => {
                snapshot.rejected = snapshot.rejected.saturating_add(1);
                "rejected"
            }
            Decision::Speech => "speech",
            Decision::Short => "short",
            Decision::Unsupported => "bypassed",
        };
        log::debug!("Local speech filter decision={decision:?}");
        decision.permits_asr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scratch() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "vocalcode-vad-bundled-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&base).unwrap();
        base
    }

    #[test]
    fn bundled_model_matches_reviewed_upstream_pin_and_installs_offline() {
        use sha2::{Digest, Sha256};
        use vocalcode_platform::speech_gate::{MODEL_SHA256, MODEL_SIZE};
        assert_eq!(BUNDLED_MODEL.len() as u64, MODEL_SIZE);
        assert_eq!(format!("{:x}", Sha256::digest(BUNDLED_MODEL)), MODEL_SHA256);
        let base = scratch();
        let path = prepare_model(&base).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), BUNDLED_MODEL);
        assert_eq!(prepare_model(&base).unwrap(), path);
        assert!(SpeechGate::load(&path).is_ok());
    }

    #[test]
    fn existing_invalid_model_is_preserved_and_recognition_fails_open() {
        let base = scratch();
        let path = prepare_model(&base).unwrap();
        std::fs::write(&path, b"invalid existing model").unwrap();
        let control = Arc::new(Control::default());
        control.set_enabled(true);
        let mut filter = Filter::new(base, control.clone());
        assert!(filter.permit(&[0.; 16000], 16000));
        assert_eq!(control.snapshot().state, "unavailable");
        assert_eq!(std::fs::read(path).unwrap(), b"invalid existing model");
    }

    #[test]
    fn disabled_and_progressive_filters_leave_fresh_install_untouched() {
        let base = scratch();
        let control = Arc::new(Control::default());
        let mut filter = Filter::new(base.clone(), control.clone());
        assert!(filter.permit(&[0.; 16000], 16000));
        control.set_enabled(true);
        control.set_progressive(true);
        assert!(filter.permit(&[0.; 16000], 16000));
        assert_eq!(std::fs::read_dir(base).unwrap().count(), 0);
    }

    #[test]
    fn real_model_worker_skips_noise_and_meeting_jobs_bypass_only_the_dictation_gate() {
        use std::sync::atomic::AtomicUsize;
        use vocalcode_core::Asr;
        struct AsrSpy(Arc<AtomicUsize>);
        impl Asr for AsrSpy {
            fn transcribe(&mut self, _: &[f32], _: u32) -> vocalcode_core::Result<String> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok("unchanged recognition".into())
            }
            fn model_label(&self) -> &str {
                "spy"
            }
        }
        // No developer-installed model: exercise the actual first-run path.
        let base = scratch();
        let control = Arc::new(Control::default());
        control.set_enabled(true);
        let calls = Arc::new(AtomicUsize::new(0));
        let (worker, mut asr, _) = crate::inference::Worker::start(
            Box::new(AsrSpy(calls.clone())),
            vec![],
            Some(Box::new(Filter::new(base.clone(), control.clone()))),
        )
        .unwrap();
        assert_eq!(asr.transcribe(&[0.; 48_000], 16000).unwrap(), "");
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(control.snapshot().state, "rejected");
        assert_eq!(
            asr.transcribe(&[0.01; 6400], 16000).unwrap(),
            "unchanged recognition"
        );
        assert_eq!(control.snapshot().state, "short");
        control.set_progressive(true);
        assert_eq!(
            asr.transcribe(&[0.; 48_000], 16000).unwrap(),
            "unchanged recognition"
        );
        assert_eq!(control.snapshot().state, "progressive");
        control.set_progressive(false);
        control.set_enabled(false);
        assert_eq!(
            asr.transcribe(&[0.; 48_000], 16000).unwrap(),
            "unchanged recognition"
        );
        control.set_enabled(true);
        assert_eq!(asr.transcribe(&[0.; 48_000], 16000).unwrap(), "");
        // The meeting runtime owns its independent gate upstream of this job.
        assert_eq!(
            worker
                .meeting(vec![0.; 48_000], 16000)
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
                .unwrap(),
            "unchanged recognition"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 4);
        drop(asr);
        drop(worker);
        println!("Isolated model test directory retained: {}", base.display());
    }
    #[test]
    fn disabled_and_progressive_paths_do_not_load_a_model() {
        let control = Arc::new(Control::default());
        let mut filter = Filter::new(
            PathBuf::from("not-used-for-disabled-speech-filter"),
            control.clone(),
        );
        assert!(filter.permit(&[0.; 16000], 16000));
        assert!(!filter.attempted);
        control.set_enabled(true);
        control.set_progressive(true);
        assert!(filter.permit(&[0.; 16000], 16000));
        assert!(!filter.attempted);
        assert_eq!(control.snapshot().state, "progressive");
    }
    #[test]
    fn unavailable_detector_fails_open_without_retrying_every_utterance() {
        let control = Arc::new(Control::default());
        control.set_enabled(true);
        let mut filter = Filter::new(PathBuf::new(), control.clone());
        // Simulate one failed load, without touching an actual data directory.
        filter.generation = control.generation.load(Ordering::Relaxed);
        filter.attempted = true;
        assert!(filter.permit(&[0.; 16000], 16000));
        assert!(filter.permit(&[0.; 16000], 16000));
        assert_eq!(control.snapshot().state, "unavailable");
        assert_eq!(control.snapshot().checked, 0);
        control.set_enabled(false);
        assert_eq!(control.snapshot().state, "off");
        let old = filter.generation;
        control.set_enabled(true);
        assert_ne!(control.generation.load(Ordering::Relaxed), old);
    }
}
