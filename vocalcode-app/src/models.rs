//! Model registry + smart selection + download-on-demand.
//!
//! Design: the installer ships thin (no models). On first run the user chooses
//! a spoken-language preference, the registry resolves it to one approved model
//! route, and the app downloads just that route. The release registry is
//! intentionally limited to models whose redistribution and product quality
//! have both been approved: Parakeet for its documented European languages
//! (English included), Paraformer/SenseVoice/Qwen3-ASR choices for Mandarin
//! Chinese, SenseVoice for Japanese and Korean and for English on compact
//! hardware, and Qwen3-ASR for Hindi.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use vocalcode_core::traits::{Asr, TextCleaner};
use vocalcode_platform::{
    asr_threads, performance_class, AcronymCollapser, HardwareProfile, JapaneseSpaceCollapser,
    Normalizer, PerformanceClass, SherpaParaformerAsr, SherpaParakeetAsr, SherpaPunctuator,
    SherpaQwen3Asr, SherpaSenseVoiceAsr, Tier,
};

/// A download in flight, reported to the UI so it can draw a real bar rather
/// than a spinner that says nothing about how long is left.
#[derive(Clone)]
pub struct Progress {
    pub label: String,
    pub done: u64,
    /// Best known total. Never 0, so the caller can always divide.
    pub total: u64,
}

impl Progress {
    pub fn percent(&self) -> f64 {
        (self.done.min(self.total) as f64 / self.total.max(1) as f64) * 100.0
    }
}

/// A cooperative stop signal for one model preparation transaction.
///
/// Clones observe the same state. `cancel` also wakes model-lock waiters; HTTP
/// operations use short, resumable request slices so a blocked socket observes
/// the signal within a bounded interval instead of waiting for the full model
/// transfer timeout.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    state: Arc<CancellationState>,
}

#[derive(Debug)]
struct CancellationState {
    cancelled: AtomicBool,
    wait_mutex: Mutex<()>,
    wake: Condvar,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            state: Arc::new(CancellationState {
                cancelled: AtomicBool::new(false),
                wait_mutex: Mutex::new(()),
                wake: Condvar::new(),
            }),
        }
    }

    #[allow(dead_code)] // Public integration API; the runtime owns the token.
    pub fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::Release);
        self.state.wake.notify_all();
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    fn check(&self) -> Result<(), ModelPrepareError> {
        if self.is_cancelled() {
            Err(ModelPrepareError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Wait for a retry interval, returning true when cancellation won the
    /// race. A condvar keeps lock contention responsive without busy polling.
    fn wait_cancelled(&self, duration: Duration) -> bool {
        if self.is_cancelled() {
            return true;
        }
        let Ok(guard) = self.state.wait_mutex.lock() else {
            // Poisoning only means another waiter panicked; cancellation is the
            // safe result for an installation transaction whose coordination
            // primitive is no longer trustworthy.
            return true;
        };
        if self.is_cancelled() {
            return true;
        }
        let _ = self
            .state
            .wake
            .wait_timeout_while(guard, duration, |_| !self.is_cancelled());
        self.is_cancelled()
    }
}

/// Cancellation is deliberately distinct from integrity/network failures.
/// Callers must discard a superseded preparation without rolling settings back
/// or scheduling the "broken model" retry loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelPrepareError {
    Cancelled,
    TimedOut(String),
    Failed(String),
}

impl ModelPrepareError {
    #[allow(dead_code)] // Public integration convenience; main is wired separately.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

impl std::fmt::Display for ModelPrepareError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("model preparation cancelled"),
            Self::TimedOut(message) | Self::Failed(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ModelPrepareError {}

impl From<String> for ModelPrepareError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

pub type ModelPrepareResult<T> = Result<T, ModelPrepareError>;

/// Which model(s) a config resolves to. Split out from `prepare_asr` so the
/// decision is testable without downloading a gigabyte of weights — this is the
/// rule that once silently dropped Chinese, and a test is the only thing that
/// makes that visible.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// One model, by registry id.
    Single(&'static str),
}

/// Which model a config asks for, or `None` when nobody has said yet.
///
/// There is deliberately no branch that decides for the user. Two things were
/// tried and both were wrong: inferring the language from the OS locale (people
/// run English-locale machines whatever they speak, so a Chinese speaker lost
/// Chinese silently), and routing per utterance between both models (the engine
/// changed under the same speaker mid-session). `None` means the first-run
/// picker runs — including for old installs still carrying `language = "auto"`,
/// which is why that value maps here rather than to a guess.
/// Spoken-language choices exposed by the app and their release-approved route.
///
/// Parakeet v3 is one multilingual model. Giving each of its 25 documented
/// languages an explicit choice does not download or load 25 models; it makes
/// the supported set discoverable without reviving the old unsafe behaviour of
/// guessing from the OS locale. The model still performs its own recognition,
/// while the stored code records the choice the user deliberately made.
pub const SUPPORTED_LANGUAGES: &[(&str, &str, &str)] = &[
    ("zh", "Mandarin Chinese + English", "sensevoice"),
    ("hi", "हिन्दी (Hindi)", "qwen3-asr-0.6b"),
    ("en", "English", "parakeet-tdt-v3"),
    ("ko", "한국어", "sensevoice"),
    ("ja", "日本語", "sensevoice"),
    ("bg", "Български", "parakeet-tdt-v3"),
    ("hr", "Hrvatski", "parakeet-tdt-v3"),
    ("cs", "Čeština", "parakeet-tdt-v3"),
    ("da", "Dansk", "parakeet-tdt-v3"),
    ("nl", "Nederlands", "parakeet-tdt-v3"),
    ("et", "Eesti", "parakeet-tdt-v3"),
    ("fi", "Suomi", "parakeet-tdt-v3"),
    ("fr", "Français", "parakeet-tdt-v3"),
    ("de", "Deutsch", "parakeet-tdt-v3"),
    ("el", "Ελληνικά", "parakeet-tdt-v3"),
    ("hu", "Magyar", "parakeet-tdt-v3"),
    ("it", "Italiano", "parakeet-tdt-v3"),
    ("lv", "Latviešu", "parakeet-tdt-v3"),
    ("lt", "Lietuvių", "parakeet-tdt-v3"),
    ("mt", "Malti", "parakeet-tdt-v3"),
    ("pl", "Polski", "parakeet-tdt-v3"),
    ("pt", "Português (Portugal)", "parakeet-tdt-v3"),
    ("ro", "Română", "parakeet-tdt-v3"),
    ("sk", "Slovenčina", "parakeet-tdt-v3"),
    ("sl", "Slovenščina", "parakeet-tdt-v3"),
    ("es", "Español", "parakeet-tdt-v3"),
    ("sv", "Svenska", "parakeet-tdt-v3"),
    ("ru", "Русский", "parakeet-tdt-v3"),
    ("uk", "Українська", "parakeet-tdt-v3"),
];

pub fn route_for(model_id: &str, lang: &str) -> Option<Route> {
    if !model_id.is_empty() {
        return spec_of(model_id).map(|spec| Route::Single(spec.id));
    }
    language_route(lang).map(Route::Single)
}

/// The model a language resolves to when no model id is stored: the single
/// source for both an empty-model config and the picker's recommendation.
pub fn language_route(lang: &str) -> Option<&'static str> {
    SUPPORTED_LANGUAGES
        .iter()
        .find(|(code, _, _)| *code == lang)
        .map(|(_, _, model)| *model)
}

/// One-time recommendation for a language picker. The persisted explicit
/// model remains authoritative afterwards; hardware changes and future model
/// releases never silently replace what the user chose.
///
/// This is `language_route` with one hardware exception. English is Parakeet
/// (voice corpus run 5: 98% of features and 97% of coding words on clean
/// voices, against SenseVoice's 91% and 69%), except on a Compact machine,
/// where Parakeet's latency (2-3.5x SenseVoice's on the same two cores),
/// 640 MB download and ~800 MiB resident set are not affordable. Qwen3-ASR is
/// never an automatic English default: it is slow and its 512-token context
/// breaks long audio.
///
/// An empty English model deliberately keeps resolving to Parakeet even on a
/// Compact machine. `route_for` runs on every load, hot-switch comparison and
/// save validation and has no hardware profile — nor should it, or a changed
/// RAM or core probe would swap a stored config's model. The page stores this
/// recommendation explicitly whenever English is picked, so a fresh Compact
/// install persists "sensevoice"; an empty English model only survives from
/// configs written before that, and those already run Parakeet.
pub fn recommended_model(lang: &str, profile: HardwareProfile) -> Option<&'static str> {
    match (lang, performance_class(profile)) {
        ("en", PerformanceClass::Compact) => Some("sensevoice"),
        _ => language_route(lang),
    }
}

pub fn selectable_models(lang: &str) -> &'static [&'static str] {
    match lang {
        "zh" => &["paraformer-zh", "sensevoice", "qwen3-asr-0.6b"],
        "hi" => &["qwen3-asr-0.6b"],
        "en" => &["sensevoice", "parakeet-tdt-v3", "qwen3-asr-0.6b"],
        _ => &[],
    }
}

/// Runtime thread count for the selected route. Unlike the one-time model
/// recommendation, this is recalculated on every model load so a manual
/// override and a hot switch receive the right measured cap.
pub fn recommended_threads(model_id: &str, lang: &str, profile: HardwareProfile) -> usize {
    let heavyweight = matches!(
        route_for(model_id, lang),
        Some(Route::Single("qwen3-asr-0.6b"))
    );
    asr_threads(profile, heavyweight)
}

/// Whether two persisted selections resolve to the same loaded ASR pipeline.
///
/// The 25 Parakeet language entries are preference labels over one
/// auto-detecting model. Comparing the raw language strings would needlessly
/// hash and construct a second ~670 MB model when, for example, a user changes
/// Français to Deutsch. SenseVoice also has language-dependent decoder hints
/// and cleaners, which must participate in pipeline identity.
pub fn same_model_route(
    left_model: &str,
    left_lang: &str,
    right_model: &str,
    right_lang: &str,
) -> bool {
    let left = route_for(left_model, left_lang);
    let right = route_for(right_model, right_lang);
    left.is_some()
        && left == right
        && wants_cjk_space_collapse(left_model, left_lang)
            == wants_cjk_space_collapse(right_model, right_lang)
        && (!matches!(left, Some(Route::Single("sensevoice")))
            || sensevoice_language_hint(left_lang) == sensevoice_language_hint(right_lang))
}

fn sensevoice_language_hint(language: &str) -> &'static str {
    if language == "zh" {
        "zh"
    } else {
        "auto"
    }
}

/// Has this config got a usable choice, or does the picker need to run?
pub fn needs_language_pick(model_id: &str, lang: &str) -> bool {
    model_id.is_empty() && route_for(model_id, lang).is_none()
}

fn spec_of(id: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().find(|m| m.id == id)
}

/// Does this model's output want the Chinese/English punctuation model?
///
/// Only where it was trained to help. Applying it to everything was doing real
/// damage, measured by feeding each supported language through `transcribe`:
///
/// - **Russian, Arabic, Hebrew**: every space removed. "сегодня хорошая погода"
///   came back "сегодняхорошаяпогода" — one unreadable word per utterance.
/// - **Vietnamese**: spaces eaten at word boundaries — "THỜI TIẾTĐẸP".
/// - **Japanese**: sentence breaks inserted mid-phrase, in Chinese marks —
///   "音声認識。のテスト", "資料，を準備して".
/// - **Thai**: a 。 appended to a script that ends sentences with a space.
///
/// Parakeet emits its own punctuation and is left alone. Paraformer emits none,
/// and for its Chinese/English output this model adds correct 。and ，.
///
/// Skipping it is also a 288 MB download this user no longer makes on first run.
pub fn wants_punct(model_id: &str, lang: &str) -> bool {
    matches!(
        route_for(model_id, lang),
        Some(Route::Single("paraformer-zh"))
    )
}

/// Does this route want SenseVoice's artifact spaces collapsed before the
/// dictionary runs?
///
/// SenseVoice space-splits Japanese morphemes ("ギット ハブ" for GitHub, "ハブ は
/// 便利" at particle joins). Japanese has no native inter-word spaces, so those
/// are recognition artifacts, and collapsing them lets the katakana correction
/// entries match. Korean shares the SenseVoice route but writes real word
/// spaces, so it is deliberately excluded — collapsing hangul spaces would fuse
/// its words. Hence this is keyed on the Japanese language, not the route alone.
pub fn wants_cjk_space_collapse(model_id: &str, lang: &str) -> bool {
    lang == "ja" && matches!(route_for(model_id, lang), Some(Route::Single("sensevoice")))
}

/// Prepare the ASR for the running config. Returns (asr, label).
/// `language="zh"` = SenseVoice (Mandarin Chinese + English in one model); any of Parakeet's
/// documented 25 European language codes = Parakeet; an explicit `model` id =
/// that model. Exactly one model is ever loaded, and it does not change until
/// the user changes the selected language.
#[allow(dead_code)] // Compatibility API for diagnostic/non-GUI callers.
pub fn prepare_asr(
    model_id: &str,
    lang: &str,
    base: &std::path::Path,
    threads: i32,
    on_status: impl FnMut(Progress),
) -> Result<(Box<dyn Asr>, String), String> {
    prepare_asr_cancellable(
        model_id,
        lang,
        base,
        threads,
        &CancellationToken::new(),
        on_status,
    )
    .map_err(|error| error.to_string())
}

/// Cancellable counterpart used by the GUI runtime. The compatibility wrapper
/// above remains for diagnostics and callers that deliberately run to
/// completion.
pub fn prepare_asr_cancellable(
    model_id: &str,
    lang: &str,
    base: &std::path::Path,
    threads: i32,
    cancellation: &CancellationToken,
    mut on_status: impl FnMut(Progress),
) -> ModelPrepareResult<(Box<dyn Asr>, String)> {
    cancellation.check()?;
    if !model_id.is_empty() && spec_of(model_id).is_none() {
        return Err(ModelPrepareError::Failed(format!(
            "unknown model id {model_id:?}"
        )));
    }
    // One decision function, so what the tests check is what actually runs.
    match route_for(model_id, lang) {
        Some(Route::Single(id)) => {
            let spec = spec_of(id)
                .ok_or_else(|| ModelPrepareError::Failed(format!("unknown model id {id:?}")))?;
            let dir = ensure_cancellable(spec, base, cancellation, &mut on_status)?;
            cancellation.check()?;
            let asr = build_asr(spec, &dir, lang, threads).map_err(ModelPrepareError::Failed)?;
            cancellation.check()?;
            Ok((asr, spec.label.to_string()))
        }
        // Unreachable from the app, which blocks on the picker first; a caller
        // that skips that gets told rather than handed a guess.
        None => Err(ModelPrepareError::Failed(
            "no language chosen yet".to_string(),
        )),
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Paraformer,
    /// NeMo Parakeet TDT (offline transducer: encoder + decoder + joiner).
    Parakeet,
    /// SenseVoice (Alibaba FunASR) — one model covering zh/en/ja/ko/yue with
    /// built-in inverse text normalization. Used for the Korean/Japanese routes.
    SenseVoice,
    /// Qwen3-ASR 0.6B INT8 — manually selectable high-context multilingual model.
    Qwen3,
}

pub struct ModelSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub kind: Kind,
    /// Registry metadata for a future model-picker UI (tier / language / size).
    #[allow(dead_code)]
    pub tier: Tier,
    #[allow(dead_code)]
    pub multilingual: bool,
    #[allow(dead_code)]
    pub size_mb: u32,
    /// (local filename, download url)
    pub files: &'static [(&'static str, &'static str)],
}

#[derive(Clone, Debug, Deserialize)]
struct Artifact {
    sha256: String,
    size: u64,
}

type ArtifactManifest = HashMap<String, HashMap<String, Artifact>>;
static ARTIFACT_MANIFEST: OnceLock<Result<ArtifactManifest, String>> = OnceLock::new();
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const MODEL_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const MODEL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const MODEL_TRANSFER_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MODEL_IO_SLICE_TIMEOUT: Duration = Duration::from_secs(5);
const MODEL_LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MODEL_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MODEL_RETRY_DELAY: Duration = Duration::from_millis(100);
const MODEL_MAX_NO_PROGRESS_ATTEMPTS: u32 = 6;

fn artifact_manifest() -> Result<&'static ArtifactManifest, String> {
    match ARTIFACT_MANIFEST.get_or_init(|| {
        let text = include_str!("../../packaging/models.json")
            .strip_prefix('\u{feff}')
            .unwrap_or(include_str!("../../packaging/models.json"));
        serde_json::from_str(text).map_err(|e| format!("invalid embedded model manifest: {e}"))
    }) {
        Ok(manifest) => Ok(manifest),
        Err(error) => Err(error.clone()),
    }
}

fn artifact(model: &str, name: &str) -> Result<&'static Artifact, String> {
    artifact_manifest()?
        .get(model)
        .and_then(|files| files.get(name))
        .ok_or_else(|| format!("model manifest has no entry for {model}/{name}"))
}

#[cfg(unix)]
const OPEN_NO_FOLLOW: i32 = if cfg!(any(target_os = "linux", target_os = "android")) {
    0x20_000
} else {
    // Darwin and the BSD family all define O_NOFOLLOW as 0x100. VocalCode's
    // supported Unix target is macOS; keeping the BSD values here makes the
    // helper safe if the crate is checked on one of those targets too.
    0x100
};

#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(windows)]
const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;

#[cfg(windows)]
fn windows_attributes_are_reparse(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn metadata_is_reparse(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        windows_attributes_are_reparse(metadata.file_attributes())
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

fn plain_file_metadata(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_file()
        && !metadata.file_type().is_symlink()
        && !metadata_is_reparse(metadata)
}

/// Open a file for validation without ever following its final path component.
/// A path check followed by `File::open` would leave a symlink-swap window, so
/// the no-follow flag belongs to the open itself and the returned handle is
/// what gets inspected and hashed.
fn open_plain_file_for_read(path: &Path) -> Result<Option<File>, String> {
    let result = {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(OPEN_NO_FOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        options.open(path)
    };

    let file = match result {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            // O_NOFOLLOW normally reports ELOOP for a symlink. Inspecting with
            // symlink_metadata is safe (it never follows) and lets us return a
            // useful fail-closed error instead of treating it as a cache miss.
            if let Ok(metadata) = std::fs::symlink_metadata(path) {
                if metadata.file_type().is_symlink() || metadata_is_reparse(&metadata) {
                    return Err(format!(
                        "refusing symlink or reparse-point model file {}",
                        path.display()
                    ));
                }
            }
            return Err(format!("open model file {}: {error}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect model file {}: {error}", path.display()))?;
    if !plain_file_metadata(&metadata) {
        return Err(format!(
            "refusing non-regular or reparse-point model file {}",
            path.display()
        ));
    }
    Ok(Some(file))
}

fn sha256_reader_with_control(
    mut file: &File,
    cancellation: &CancellationToken,
) -> ModelPrepareResult<String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        cancellation.check()?;
        let count = file
            .read(&mut buffer)
            .map_err(|error| ModelPrepareError::Failed(error.to_string()))?;
        cancellation.check()?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
fn sha256_reader(file: &File) -> Result<String, String> {
    sha256_reader_with_control(file, &CancellationToken::new()).map_err(|error| error.to_string())
}

#[cfg(test)]
fn sha256_file(path: &Path) -> Result<String, String> {
    let file = open_plain_file_for_read(path)?
        .ok_or_else(|| format!("model file does not exist: {}", path.display()))?;
    sha256_reader(&file)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArtifactState {
    Missing,
    Invalid,
    Valid,
}

fn artifact_state_with_control(
    path: &Path,
    expected: &Artifact,
    cancellation: &CancellationToken,
) -> ModelPrepareResult<ArtifactState> {
    cancellation.check()?;
    let Some(file) = open_plain_file_for_read(path).map_err(ModelPrepareError::Failed)? else {
        return Ok(ArtifactState::Missing);
    };
    let metadata = file.metadata().map_err(|error| {
        ModelPrepareError::Failed(format!("inspect model file {}: {error}", path.display()))
    })?;
    if metadata.len() != expected.size {
        return Ok(ArtifactState::Invalid);
    }
    let actual = sha256_reader_with_control(&file, cancellation)?;
    cancellation.check()?;
    if actual.eq_ignore_ascii_case(&expected.sha256) {
        Ok(ArtifactState::Valid)
    } else {
        Ok(ArtifactState::Invalid)
    }
}

#[cfg(test)]
fn artifact_state(path: &Path, expected: &Artifact) -> Result<ArtifactState, String> {
    artifact_state_with_control(path, expected, &CancellationToken::new())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
fn artifact_is_valid(path: &Path, expected: &Artifact) -> bool {
    matches!(artifact_state(path, expected), Ok(ArtifactState::Valid))
}

// Every URL points at our own mirror in R2 (models.vocalcode.app), not at the
// upstream repository the weights came from. The app downloads its model on
// first run, so an upstream that moves could otherwise break every new install.
//
// Provenance for each is beside its entry. The mirrored bytes are
// identical to what those URLs served; sha256 of every file was recorded when
// the mirror was made.
pub static MODELS: &[ModelSpec] = &[
    // Exact ONNX conversion:
    // https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/tree/2bda32ec70b097a55adaa07d9a7173915b43cc78
    // Derived from NVIDIA nvidia/parakeet-tdt-0.6b-v3 under CC-BY-4.0:
    // https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3
    // https://creativecommons.org/licenses/by/4.0/
    ModelSpec {
        id: "parakeet-tdt-v3",
        label: "Parakeet TDT v3 · auto-detects 25 languages",
        kind: Kind::Parakeet,
        tier: Tier::Cpu,
        multilingual: true,
        size_mb: 640,
        files: &[
            (
                "encoder.onnx",
                "https://models.vocalcode.app/parakeet-tdt-v3/encoder.onnx",
            ),
            (
                "decoder.onnx",
                "https://models.vocalcode.app/parakeet-tdt-v3/decoder.onnx",
            ),
            (
                "joiner.onnx",
                "https://models.vocalcode.app/parakeet-tdt-v3/joiner.onnx",
            ),
            (
                "tokens.txt",
                "https://models.vocalcode.app/parakeet-tdt-v3/tokens.txt",
            ),
        ],
    },
    // Exact bilingual ONNX conversion (Apache-2.0):
    // https://huggingface.co/csukuangfj/sherpa-onnx-paraformer-zh-2023-09-14/tree/def027084691107096b5ebba69785756d63de6c5
    // https://www.apache.org/licenses/LICENSE-2.0
    ModelSpec {
        id: "paraformer-zh",
        label: "Paraformer · Mandarin Chinese + English",
        kind: Kind::Paraformer,
        tier: Tier::Cpu,
        multilingual: false,
        size_mb: 232,
        files: &[
            (
                "model.onnx",
                "https://models.vocalcode.app/paraformer-zh/model.onnx",
            ),
            (
                "tokens.txt",
                "https://models.vocalcode.app/paraformer-zh/tokens.txt",
            ),
        ],
    },
    // Exact int8 ONNX conversion:
    // https://huggingface.co/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17
    // Derived from Alibaba FunAudioLLM/SenseVoice under the FunASR Model License
    // v1.1 (commercial use permitted with attribution to Alibaba/FunASR):
    // https://github.com/FunAudioLLM/SenseVoice
    ModelSpec {
        id: "sensevoice",
        label: "SenseVoice · Korean, Japanese, Chinese & English",
        kind: Kind::SenseVoice,
        tier: Tier::Cpu,
        multilingual: true,
        size_mb: 229,
        files: &[
            (
                "model.int8.onnx",
                "https://models.vocalcode.app/sensevoice/model.int8.onnx",
            ),
            (
                "tokens.txt",
                "https://models.vocalcode.app/sensevoice/tokens.txt",
            ),
        ],
    },
    // Official Qwen3-ASR 0.6B model (Apache-2.0) converted to INT8 ONNX for
    // sherpa-onnx. The conversion is pinned and benchmarked in
    // target/asr-bench/report-2026-08-29.md before entering this registry.
    // https://huggingface.co/csukuangfj2/sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25/tree/2cc50d1abfe4d4f2df8d71f536d108bb40f943d2
    ModelSpec {
        id: "qwen3-asr-0.6b",
        label: "Qwen3-ASR 0.6B · Hindi + high-context multilingual",
        kind: Kind::Qwen3,
        tier: Tier::Cpu,
        multilingual: true,
        size_mb: 941,
        files: &[
            (
                "conv_frontend.onnx",
                "https://models.vocalcode.app/qwen3-asr-0.6b/conv_frontend.onnx",
            ),
            (
                "encoder.int8.onnx",
                "https://models.vocalcode.app/qwen3-asr-0.6b/encoder.int8.onnx",
            ),
            (
                "decoder.int8.onnx",
                "https://models.vocalcode.app/qwen3-asr-0.6b/decoder.int8.onnx",
            ),
            (
                "merges.txt",
                "https://models.vocalcode.app/qwen3-asr-0.6b/merges.txt",
            ),
            (
                "tokenizer_config.json",
                "https://models.vocalcode.app/qwen3-asr-0.6b/tokenizer_config.json",
            ),
            (
                "vocab.json",
                "https://models.vocalcode.app/qwen3-asr-0.6b/vocab.json",
            ),
        ],
    },
];

fn ensure_plain_directory(path: &Path, description: &str) -> Result<(), String> {
    match std::fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("create {description} {}: {error}", path.display())),
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {description} {}: {error}", path.display()))?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata_is_reparse(&metadata)
    {
        return Err(format!(
            "refusing non-directory symlink or reparse point for {description}: {}",
            path.display()
        ));
    }
    Ok(())
}

fn ensure_model_directory(base: &Path, model_id: &str) -> Result<PathBuf, String> {
    if Path::new(model_id).file_name() != Some(std::ffi::OsStr::new(model_id)) {
        return Err(format!("invalid model id path component {model_id:?}"));
    }
    std::fs::create_dir_all(base)
        .map_err(|error| format!("create model base {}: {error}", base.display()))?;
    let base_metadata = std::fs::symlink_metadata(base)
        .map_err(|error| format!("inspect model base {}: {error}", base.display()))?;
    if !base_metadata.file_type().is_dir()
        || base_metadata.file_type().is_symlink()
        || metadata_is_reparse(&base_metadata)
    {
        return Err(format!(
            "refusing symlink or reparse-point model base {}",
            base.display()
        ));
    }

    let models = base.join("models");
    ensure_plain_directory(&models, "models directory")?;
    let directory = models.join(model_id);
    ensure_plain_directory(&directory, "model directory")?;
    Ok(directory)
}

#[derive(Debug)]
struct ModelInstallLock {
    file: File,
}

fn model_lock_is_contended(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    if error.raw_os_error() == Some(33) {
        // ERROR_LOCK_VIOLATION is reported as ErrorKind::Other by std.
        return true;
    }
    #[cfg(target_os = "linux")]
    if error.raw_os_error() == Some(11) {
        return true;
    }
    #[cfg(target_os = "macos")]
    if error.raw_os_error() == Some(35) {
        return true;
    }
    false
}

impl ModelInstallLock {
    #[cfg(test)]
    fn acquire(model_directory: &Path) -> Result<Self, String> {
        Self::acquire_cancellable(
            model_directory,
            &CancellationToken::new(),
            MODEL_LOCK_WAIT_TIMEOUT,
        )
        .map_err(|error| error.to_string())
    }

    fn acquire_cancellable(
        model_directory: &Path,
        cancellation: &CancellationToken,
        wait_timeout: Duration,
    ) -> ModelPrepareResult<Self> {
        cancellation.check()?;
        let path = model_directory.join(".install.lock");
        let result = {
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600).custom_flags(OPEN_NO_FOLLOW);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
            }
            options.open(&path)
        };
        let file = match result {
            Ok(file) => file,
            Err(error) => {
                if let Ok(metadata) = std::fs::symlink_metadata(&path) {
                    if metadata.file_type().is_symlink() || metadata_is_reparse(&metadata) {
                        return Err(ModelPrepareError::Failed(format!(
                            "refusing symlink or reparse-point model lock {}",
                            path.display()
                        )));
                    }
                }
                return Err(ModelPrepareError::Failed(format!(
                    "open model lock {}: {error}",
                    path.display()
                )));
            }
        };
        let metadata = file.metadata().map_err(|error| {
            ModelPrepareError::Failed(format!("inspect model lock {}: {error}", path.display()))
        })?;
        if !plain_file_metadata(&metadata) {
            return Err(ModelPrepareError::Failed(format!(
                "refusing non-regular or reparse-point model lock {}",
                path.display()
            )));
        }
        let deadline = Instant::now()
            .checked_add(wait_timeout)
            .unwrap_or_else(Instant::now);
        loop {
            cancellation.check()?;
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => {
                    cancellation.check()?;
                    return Ok(Self { file });
                }
                Err(error) if model_lock_is_contended(&error) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(ModelPrepareError::TimedOut(format!(
                            "timed out waiting for model directory lock {}",
                            model_directory.display()
                        )));
                    }
                    let wait =
                        MODEL_LOCK_POLL_INTERVAL.min(deadline.saturating_duration_since(now));
                    if cancellation.wait_cancelled(wait) {
                        return Err(ModelPrepareError::Cancelled);
                    }
                }
                Err(error) => {
                    return Err(ModelPrepareError::Failed(format!(
                        "lock model directory {}: {error}",
                        model_directory.display()
                    )))
                }
            }
        }
    }
}

impl Drop for ModelInstallLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
fn with_model_install_lock<T>(
    model_directory: &Path,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let _lock = ModelInstallLock::acquire(model_directory)?;
    action()
}

fn with_model_install_lock_cancellable<T>(
    model_directory: &Path,
    cancellation: &CancellationToken,
    action: impl FnOnce() -> ModelPrepareResult<T>,
) -> ModelPrepareResult<T> {
    let _lock = ModelInstallLock::acquire_cancellable(
        model_directory,
        cancellation,
        MODEL_LOCK_WAIT_TIMEOUT,
    )?;
    cancellation.check()?;
    let result = action();
    cancellation.check()?;
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResidueKind {
    Partial,
    Replaced,
}

fn process_id_component(value: &str) -> bool {
    value
        .parse::<u32>()
        .ok()
        .filter(|id| *id != 0)
        .is_some_and(|id| id.to_string() == value)
}

fn sequence_component(value: &str) -> bool {
    value
        .parse::<u64>()
        .ok()
        .is_some_and(|sequence| sequence.to_string() == value)
}

fn owned_residue_kind(canonical: &Path, candidate_name: &str) -> Option<ResidueKind> {
    for (marker, kind) in [
        ("part-", ResidueKind::Partial),
        ("replaced-", ResidueKind::Replaced),
    ] {
        let prefix_path = canonical.with_extension(marker);
        let prefix = prefix_path.file_name()?.to_str()?;
        let Some(suffix) = candidate_name.strip_prefix(prefix) else {
            continue;
        };
        let components: Vec<_> = suffix.split('-').collect();
        let strict = match kind {
            ResidueKind::Partial => {
                components.len() == 2
                    && process_id_component(components[0])
                    && sequence_component(components[1])
            }
            ResidueKind::Replaced => {
                // Current backups carry PID + sequence. Releases immediately
                // before this protocol used just the PID; accepting precisely
                // that numeric legacy form lets a crash there be recovered.
                matches!(components.len(), 1 | 2)
                    && process_id_component(components[0])
                    && components
                        .get(1)
                        .is_none_or(|sequence| sequence_component(sequence))
            }
        };
        if strict {
            return Some(kind);
        }
    }
    None
}

fn owned_residues(canonical: &Path) -> Result<Vec<(PathBuf, ResidueKind)>, String> {
    let parent = canonical
        .parent()
        .ok_or_else(|| format!("model file has no parent: {}", canonical.display()))?;
    let mut residues = Vec::new();
    for entry in std::fs::read_dir(parent)
        .map_err(|error| format!("scan model directory {}: {error}", parent.display()))?
    {
        let entry = entry.map_err(|error| format!("scan model directory entry: {error}"))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Some(kind) = owned_residue_kind(canonical, &name) {
            residues.push((entry.path(), kind));
        }
    }
    residues.sort_by(|left, right| left.0.file_name().cmp(&right.0.file_name()));
    Ok(residues)
}

fn require_plain_owned_residue(path: &Path) -> Result<bool, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "inspect model-install residue {}: {error}",
                path.display()
            ))
        }
    };
    if !plain_file_metadata(&metadata) {
        return Err(format!(
            "refusing symlink, reparse point, or non-file model-install residue {}",
            path.display()
        ));
    }
    Ok(true)
}

fn remove_owned_residue(path: &Path) -> Result<(), String> {
    if !require_plain_owned_residue(path)? {
        return Ok(());
    }
    std::fs::remove_file(path)
        .map_err(|error| format!("remove model-install residue {}: {error}", path.display()))
}

fn destination_is_plain_or_missing(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("inspect destination {}: {error}", path.display())),
        Ok(metadata) if plain_file_metadata(&metadata) => Ok(true),
        Ok(_) => Err(format!(
            "refusing symlink, reparse point, or non-file model destination {}",
            path.display()
        )),
    }
}

#[cfg(windows)]
fn wide_path(path: &Path) -> Result<Vec<u16>, String> {
    use std::os::windows::ffi::OsStrExt;
    let mut value: Vec<u16> = path.as_os_str().encode_wide().collect();
    if value.contains(&0) {
        return Err(format!("path contains an embedded NUL: {}", path.display()));
    }
    value.push(0);
    Ok(value)
}

#[cfg(windows)]
fn sync_plain_file_windows(path: &Path) -> Result<(), String> {
    use std::os::windows::fs::OpenOptionsExt;
    let mut retry = 0;
    let file = loop {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH)
            .open(path)
        {
            Ok(file) => break file,
            Err(error) if matches!(error.raw_os_error(), Some(5 | 32 | 33)) && retry < 500 => {
                retry += 1;
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => {
                return Err(format!(
                    "open published model {} for write-through sync: {error}",
                    path.display()
                ))
            }
        }
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect published model {}: {error}", path.display()))?;
    if !plain_file_metadata(&metadata) {
        return Err(format!(
            "refusing to sync reparse-point published model {}",
            path.display()
        ));
    }
    file.sync_all()
        .map_err(|error| format!("write-through sync model file {}: {error}", path.display()))
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn MoveFileExW(existing_file_name: *const u16, new_file_name: *const u16, flags: u32) -> i32;
}

/// Publish a verified sibling file without ever removing the canonical name.
/// On Windows this uses the write-through rename primitive directly. Old
/// `.replaced-PID-*` backups from the pre-atomic installer are still recovered
/// by `recover_artifact_locked`, but new publishes no longer need to create one.
fn atomic_publish_with_control(
    source: &Path,
    canonical: &Path,
    cancellation: &CancellationToken,
) -> ModelPrepareResult<Option<PathBuf>> {
    cancellation.check()?;
    let destination_exists =
        destination_is_plain_or_missing(canonical).map_err(ModelPrepareError::Failed)?;
    cancellation.check()?;

    #[cfg(windows)]
    {
        const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
        const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;
        let source_wide = wide_path(source).map_err(ModelPrepareError::Failed)?;
        let canonical_wide = wide_path(canonical).map_err(ModelPrepareError::Failed)?;
        // ReplaceFileW has documented partial-failure states and its nominal
        // WRITE_THROUGH flag is explicitly unsupported. A same-directory
        // MoveFileExW rename with REPLACE_EXISTING keeps one directory entry
        // continuously bound to either the old or new file. Without that flag,
        // the missing-destination case refuses a name introduced by a race.
        let flags = MOVEFILE_WRITE_THROUGH
            | if destination_exists {
                MOVEFILE_REPLACE_EXISTING
            } else {
                0
            };
        let mut retry = 0;
        loop {
            cancellation.check()?;
            let moved =
                unsafe { MoveFileExW(source_wide.as_ptr(), canonical_wide.as_ptr(), flags) };
            if moved != 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            let transient_reader = matches!(error.raw_os_error(), Some(5 | 32 | 33));
            if transient_reader && retry < 500 {
                // Virus scanners and another process validating the canonical
                // can briefly hold a handle without delete sharing. The failed
                // rename leaves the old canonical untouched, so retrying is both
                // safe and materially more reliable on Windows.
                retry += 1;
                if cancellation.wait_cancelled(Duration::from_millis(2)) {
                    return Err(ModelPrepareError::Cancelled);
                }
                continue;
            }
            return Err(ModelPrepareError::Failed(format!(
                "atomically publish model file {}: {error}",
                canonical.display()
            )));
        }
        // Once the directory entry has moved, finish the durability operation
        // even if cancellation raced the syscall. Returning early here could
        // leave a caller believing no commit occurred.
        sync_plain_file_windows(canonical).map_err(ModelPrepareError::Failed)?;
        Ok(None)
    }

    #[cfg(not(windows))]
    {
        let _ = destination_exists;
        std::fs::rename(source, canonical).map_err(|error| {
            ModelPrepareError::Failed(format!(
                "atomically publish model file {}: {error}",
                canonical.display()
            ))
        })?;
        #[cfg(unix)]
        {
            let parent = canonical
                .parent()
                .ok_or_else(|| format!("model file has no parent: {}", canonical.display()))?;
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| {
                    ModelPrepareError::Failed(format!(
                        "sync model directory {}: {error}",
                        parent.display()
                    ))
                })?;
        }
        Ok(None)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstallFault {
    None,
    #[cfg(test)]
    BeforePublish,
    #[cfg(test)]
    AfterPublish,
}

#[cfg(test)]
fn install_verified_with_fault(
    source: &Path,
    canonical: &Path,
    expected: &Artifact,
    _fault: InstallFault,
) -> Result<(), String> {
    install_verified_with_fault_and_control(
        source,
        canonical,
        expected,
        _fault,
        &CancellationToken::new(),
    )
    .map_err(|error| error.to_string())
}

fn install_verified_with_fault_and_control(
    source: &Path,
    canonical: &Path,
    expected: &Artifact,
    _fault: InstallFault,
    cancellation: &CancellationToken,
) -> ModelPrepareResult<()> {
    cancellation.check()?;
    if artifact_state_with_control(source, expected, cancellation)? != ArtifactState::Valid {
        return Err(ModelPrepareError::Failed(format!(
            "refusing to publish unverified model bytes from {}",
            source.display()
        )));
    }
    cancellation.check()?;
    #[cfg(test)]
    if _fault == InstallFault::BeforePublish {
        return Err(ModelPrepareError::Failed(
            "injected crash before model replacement".to_string(),
        ));
    }

    let backup = atomic_publish_with_control(source, canonical, cancellation)?;
    #[cfg(test)]
    if _fault == InstallFault::AfterPublish {
        return Err(ModelPrepareError::Failed(
            "injected crash after model replacement".to_string(),
        ));
    }

    // Publication is a critical section: after the rename, verify and clean up
    // using an uncancelled token, then report a racing cancellation. This keeps
    // the canonical path trustworthy without making pre-publish work ignore a
    // stop request.
    let cancelled_after_publish = cancellation.is_cancelled();
    let finish = CancellationToken::new();
    if artifact_state_with_control(canonical, expected, &finish)? != ArtifactState::Valid {
        return Err(ModelPrepareError::Failed(format!(
            "published model failed verification: {}",
            canonical.display()
        )));
    }
    if let Some(backup) = backup {
        remove_owned_residue(&backup).map_err(ModelPrepareError::Failed)?;
    }
    if cancelled_after_publish || cancellation.is_cancelled() {
        return Err(ModelPrepareError::Cancelled);
    }
    Ok(())
}

#[cfg(test)]
fn install_verified(source: &Path, canonical: &Path, expected: &Artifact) -> Result<(), String> {
    install_verified_with_fault(source, canonical, expected, InstallFault::None)
}

fn install_verified_cancellable(
    source: &Path,
    canonical: &Path,
    expected: &Artifact,
    cancellation: &CancellationToken,
) -> ModelPrepareResult<()> {
    install_verified_with_fault_and_control(
        source,
        canonical,
        expected,
        InstallFault::None,
        cancellation,
    )
}

/// Validate a canonical artifact and repair/clean crash residue while the
/// model's exclusive install lock is held. A verified backup can replace a
/// missing or corrupt canonical file atomically; corrupt owned backups and
/// stale owned partials are removed. Lookalike names are ignored, while an
/// owned-looking symlink/reparse point fails closed and is never touched.
#[cfg(test)]
fn recover_artifact_locked(canonical: &Path, expected: &Artifact) -> Result<bool, String> {
    recover_artifact_locked_cancellable(canonical, expected, &CancellationToken::new())
        .map_err(|error| error.to_string())
}

fn recover_artifact_locked_cancellable(
    canonical: &Path,
    expected: &Artifact,
    cancellation: &CancellationToken,
) -> ModelPrepareResult<bool> {
    cancellation.check()?;
    let canonical_state = artifact_state_with_control(canonical, expected, cancellation)?;
    cancellation.check()?;
    let residues = owned_residues(canonical).map_err(ModelPrepareError::Failed)?;

    // Inspect every exact owned name before mutating anything. This keeps a
    // malicious reparse-point residue from being silently skipped or removed.
    for (path, _) in &residues {
        cancellation.check()?;
        require_plain_owned_residue(path).map_err(ModelPrepareError::Failed)?;
    }

    for (path, kind) in &residues {
        cancellation.check()?;
        if *kind == ResidueKind::Partial {
            remove_owned_residue(path).map_err(ModelPrepareError::Failed)?;
        }
    }

    if canonical_state == ArtifactState::Valid {
        for (path, kind) in &residues {
            cancellation.check()?;
            if *kind == ResidueKind::Replaced {
                remove_owned_residue(path).map_err(ModelPrepareError::Failed)?;
            }
        }
        cancellation.check()?;
        return Ok(true);
    }

    for (backup, kind) in &residues {
        cancellation.check()?;
        if *kind != ResidueKind::Replaced {
            continue;
        }
        match artifact_state_with_control(backup, expected, cancellation)? {
            ArtifactState::Valid => {
                install_verified_cancellable(backup, canonical, expected, cancellation)?;
                for (other, other_kind) in &residues {
                    cancellation.check()?;
                    if *other_kind == ResidueKind::Replaced && other != backup {
                        remove_owned_residue(other).map_err(ModelPrepareError::Failed)?;
                    }
                }
                cancellation.check()?;
                return Ok(true);
            }
            ArtifactState::Invalid => {
                remove_owned_residue(backup).map_err(ModelPrepareError::Failed)?
            }
            ArtifactState::Missing => {}
        }
    }
    cancellation.check()?;
    Ok(false)
}

/// Ensure the model's files exist under `base/models/<id>/`, downloading any
/// that are missing. `on_status` gets progress lines for the UI.
#[allow(dead_code)] // Compatibility API for diagnostic/non-GUI callers.
pub fn ensure(
    spec: &ModelSpec,
    base: &std::path::Path,
    on_status: impl FnMut(Progress),
) -> Result<PathBuf, String> {
    ensure_cancellable(spec, base, &CancellationToken::new(), on_status)
        .map_err(|error| error.to_string())
}

pub fn ensure_cancellable(
    spec: &ModelSpec,
    base: &std::path::Path,
    cancellation: &CancellationToken,
    on_status: impl FnMut(Progress),
) -> ModelPrepareResult<PathBuf> {
    let mut downloader = HttpArtifactDownloader::production();
    ensure_cancellable_with_downloader(spec, base, cancellation, on_status, &mut downloader)
}

struct MissingArtifact {
    name: &'static str,
    url: &'static str,
    expected: &'static Artifact,
}

trait ArtifactDownloader {
    fn fetch(
        &mut self,
        url: &str,
        path: &Path,
        expected: &Artifact,
        cancellation: &CancellationToken,
        on_progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> ModelPrepareResult<()>;
}

fn ensure_cancellable_with_downloader(
    spec: &ModelSpec,
    base: &std::path::Path,
    cancellation: &CancellationToken,
    mut on_status: impl FnMut(Progress),
    downloader: &mut dyn ArtifactDownloader,
) -> ModelPrepareResult<PathBuf> {
    cancellation.check()?;
    let dir = ensure_model_directory(base, spec.id).map_err(ModelPrepareError::Failed)?;
    cancellation.check()?;
    with_model_install_lock_cancellable(&dir, cancellation, || {
        // Progress is reported across the whole model, not per file: a user
        // waiting on "Parakeet" does not care that it happens to be four files,
        // and a bar that restarts at 0% reads as if it were stuck in a loop.
        let mut missing = Vec::new();
        let mut expected_total = 0u64;
        let mut completed = 0u64;
        for &(name, url) in spec.files {
            cancellation.check()?;
            let expected = artifact(spec.id, name).map_err(ModelPrepareError::Failed)?;
            expected_total = expected_total.saturating_add(expected.size);
            let path = dir.join(name);
            if !recover_artifact_locked_cancellable(&path, expected, cancellation)? {
                if std::fs::symlink_metadata(&path).is_ok() {
                    log::warn!(
                        "cached model file failed integrity check: {}",
                        path.display()
                    );
                }
                missing.push(MissingArtifact {
                    name,
                    url,
                    expected,
                });
            } else {
                completed = completed.saturating_add(expected.size);
            }
        }
        for item in missing {
            cancellation.check()?;
            let path = dir.join(item.name);
            log::info!("downloading {} from {}", item.name, item.url);
            let mut progress = |done, _total| {
                // Deliberately ignores this file's Content-Length: a model is
                // several files, and a per-file total would make the bar restart
                // partway through. The registry size covers the whole model.
                let overall = completed + done;
                on_status(Progress {
                    label: short_label(spec.label),
                    done: overall,
                    total: expected_total.max(overall),
                });
            };
            downloader
                .fetch(item.url, &path, item.expected, cancellation, &mut progress)
                .map_err(|error| match error {
                    ModelPrepareError::Cancelled => ModelPrepareError::Cancelled,
                    ModelPrepareError::TimedOut(message) => {
                        ModelPrepareError::TimedOut(format!("download {}: {message}", item.name))
                    }
                    ModelPrepareError::Failed(message) => {
                        ModelPrepareError::Failed(format!("download {}: {message}", item.name))
                    }
                })?;
            // This explicit boundary is what prevents a downloader that notices
            // cancellation on its final callback from advancing to the next
            // artifact.
            cancellation.check()?;
            if !recover_artifact_locked_cancellable(&path, item.expected, cancellation)? {
                return Err(ModelPrepareError::Failed(format!(
                    "downloaded model file did not survive verification: {}",
                    path.display()
                )));
            }
            completed += item.expected.size;
        }
        cancellation.check()?;
        Ok(())
    })?;
    cancellation.check()?;
    Ok(dir)
}

/// The punctuation model, on our mirror like everything else.
///
/// Exact CT-Transformer conversion (Apache-2.0):
/// https://huggingface.co/csukuangfj/sherpa-onnx-punct-ct-transformer-zh-en-vocab272727-2024-04-12/tree/432aeba669265e7aeb06b9359753419683b38597
/// https://www.apache.org/licenses/LICENSE-2.0
const PUNCT_URL: &str = "https://models.vocalcode.app/punct/model.onnx";

/// Ensure the (universal) punctuation model is available, downloading it if
/// needed. Punctuation is part of the advertised Paraformer pipeline, so a
/// failure is explicit: callers must keep the engine unready and retry instead
/// of silently shipping acronym cleanup while the UI says punctuation is on.
#[allow(dead_code)] // Compatibility API for diagnostic/non-GUI callers.
pub fn ensure_punct(
    base: &std::path::Path,
    on_status: impl FnMut(Progress),
) -> Result<PathBuf, String> {
    ensure_punct_cancellable(base, &CancellationToken::new(), on_status)
        .map_err(|error| error.to_string())
}

pub fn ensure_punct_cancellable(
    base: &std::path::Path,
    cancellation: &CancellationToken,
    mut on_status: impl FnMut(Progress),
) -> ModelPrepareResult<PathBuf> {
    cancellation.check()?;
    let directory = ensure_model_directory(base, "punct").map_err(ModelPrepareError::Failed)?;
    let path = directory.join("model.onnx");
    let expected = artifact("punct", "model.onnx").map_err(ModelPrepareError::Failed)?;
    let mut downloader = HttpArtifactDownloader::production();
    with_model_install_lock_cancellable(&directory, cancellation, || {
        if recover_artifact_locked_cancellable(&path, expected, cancellation)? {
            return Ok(path.clone());
        }
        // Reuse an existing CapsWriter-Offline install if present (Windows
        // layout). This check stays inside our model lock so a second VocalCode
        // process cannot concurrently begin a local punctuation download.
        #[cfg(windows)]
        {
            let caps = PathBuf::from(
                r"C:\Apps\CapsWriter-Offline\models\Punct-CT-Transformer\sherpa-onnx-punct-ct-transformer-zh-en-vocab272727-2024-04-12\model.onnx",
            );
            match artifact_state_with_control(&caps, expected, cancellation) {
                Ok(ArtifactState::Valid) => return Ok(caps),
                Ok(ArtifactState::Missing | ArtifactState::Invalid) => {}
                Err(ModelPrepareError::Cancelled) => return Err(ModelPrepareError::Cancelled),
                // CapsWriter is an optional import source. An unreadable copy
                // must not prevent the verified mirror fallback.
                Err(ModelPrepareError::TimedOut(_) | ModelPrepareError::Failed(_)) => {}
            }
        }
        cancellation.check()?;
        log::info!("downloading punctuation model");
        let mut progress = |done: u64, total: Option<u64>| {
            on_status(Progress {
                label: "Punctuation model".to_string(),
                done,
                total: total.unwrap_or(expected.size).max(done),
            })
        };
        downloader
            .fetch(PUNCT_URL, &path, expected, cancellation, &mut progress)
            .map_err(|error| match error {
                ModelPrepareError::Cancelled => ModelPrepareError::Cancelled,
                ModelPrepareError::TimedOut(message) => {
                    ModelPrepareError::TimedOut(format!("punctuation download failed: {message}"))
                }
                ModelPrepareError::Failed(message) => {
                    ModelPrepareError::Failed(format!("punctuation download failed: {message}"))
                }
            })?;
        cancellation.check()?;
        if !recover_artifact_locked_cancellable(&path, expected, cancellation)? {
            return Err(ModelPrepareError::Failed(format!(
                "punctuation model did not survive verification: {}",
                path.display()
            )));
        }
        cancellation.check()?;
        Ok(path.clone())
    })
}

/// Download `url` to `path`, reporting progress as `(bytes_so_far, total)`.
///
/// Chunked rather than `io::copy` so there is something to report at all: these
/// files run to hundreds of megabytes, and a single blocking copy leaves the
/// user watching a motionless "Downloading…" for minutes with no way to tell
/// progress from a hang.
///
/// `total` is None when the server sends no Content-Length, in which case the
/// caller can still show bytes transferred.
/// Registry labels carry a parenthesised description that is far too long for a
/// progress line, so only the leading name is shown.
fn short_label(label: &str) -> String {
    label.split(" · ").next().unwrap_or(label).to_string()
}

/// Create a new partial file without ever following a pre-created symlink or
/// truncating somebody else's file. The name is only an implementation detail;
/// `create_new` is the security boundary and the loop handles stale partials.
fn create_download_temp(path: &Path) -> Result<(PathBuf, File), String> {
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("part-{}-{sequence}", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err(format!(
        "could not reserve a unique partial file beside {}",
        path.display()
    ))
}

#[derive(Clone, Copy)]
struct DownloadPolicy {
    https_only: bool,
    io_slice_timeout: Duration,
    transfer_timeout: Duration,
    retry_delay: Duration,
    max_no_progress_attempts: u32,
}

impl DownloadPolicy {
    fn production() -> Self {
        Self {
            https_only: true,
            io_slice_timeout: MODEL_IO_SLICE_TIMEOUT,
            transfer_timeout: MODEL_TRANSFER_TIMEOUT,
            retry_delay: MODEL_RETRY_DELAY,
            max_no_progress_attempts: MODEL_MAX_NO_PROGRESS_ATTEMPTS,
        }
    }
}

struct HttpArtifactDownloader {
    policy: DownloadPolicy,
}

impl HttpArtifactDownloader {
    fn production() -> Self {
        Self {
            policy: DownloadPolicy::production(),
        }
    }
}

impl ArtifactDownloader for HttpArtifactDownloader {
    fn fetch(
        &mut self,
        url: &str,
        path: &Path,
        expected: &Artifact,
        cancellation: &CancellationToken,
        on_progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> ModelPrepareResult<()> {
        download_with_policy(url, path, expected, cancellation, self.policy, on_progress)
    }
}

fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    let total = total.parse::<u64>().ok()?;
    (start <= end && end < total).then_some((start, end, total))
}

fn retryable_request_error(error: &ureq::Error) -> bool {
    matches!(
        error,
        ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
            | ureq::Error::HostNotFound
            | ureq::Error::ConnectionFailed
    )
}

fn download_with_policy(
    url: &str,
    path: &Path,
    expected: &Artifact,
    cancellation: &CancellationToken,
    policy: DownloadPolicy,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> ModelPrepareResult<()> {
    cancellation.check()?;
    if expected.size == 0 {
        return Err(ModelPrepareError::Failed(
            "model manifest declared a zero-byte artifact".to_string(),
        ));
    }
    if policy.io_slice_timeout.is_zero() || policy.transfer_timeout.is_zero() {
        return Err(ModelPrepareError::Failed(
            "model download timeout must be non-zero".to_string(),
        ));
    }

    // Every request is a resumable range and has a short absolute deadline.
    // Dropping the timed-out response closes that connection; the next request
    // resumes at exactly the number of bytes already hashed and written. This
    // is what bounds cancellation while `Read::read` itself is blocked.
    let request_timeout = policy.io_slice_timeout.min(policy.transfer_timeout);
    let agent = ureq::Agent::config_builder()
        .https_only(policy.https_only)
        .max_redirects(0)
        .timeout_connect(Some(MODEL_CONNECT_TIMEOUT.min(request_timeout)))
        .timeout_recv_response(Some(MODEL_RESPONSE_TIMEOUT.min(request_timeout)))
        .timeout_recv_body(Some(request_timeout))
        .timeout_global(Some(request_timeout))
        .build()
        .new_agent();
    let transfer_deadline = Instant::now()
        .checked_add(policy.transfer_timeout)
        .unwrap_or_else(Instant::now);
    let (tmp, mut file) = create_download_temp(path).map_err(ModelPrepareError::Failed)?;
    cancellation.check()?;

    let result = (|| -> ModelPrepareResult<()> {
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 128 * 1024];
        let mut done: u64 = 0;
        let mut no_progress_attempts = 0u32;
        let mut last_progress = Instant::now();
        on_progress(0, Some(expected.size));
        cancellation.check()?;

        'request: while done < expected.size {
            cancellation.check()?;
            if Instant::now() >= transfer_deadline {
                return Err(ModelPrepareError::TimedOut(format!(
                    "model transfer timed out after {:?}",
                    policy.transfer_timeout
                )));
            }

            let request_start = done;
            let range = format!("bytes={request_start}-{}", expected.size - 1);
            let response = match agent
                .get(url)
                .header("Accept-Encoding", "identity")
                .header("Range", &range)
                .call()
            {
                Ok(response) => response,
                Err(error) if retryable_request_error(&error) => {
                    cancellation.check()?;
                    no_progress_attempts = no_progress_attempts.saturating_add(1);
                    if no_progress_attempts > policy.max_no_progress_attempts {
                        return Err(ModelPrepareError::TimedOut(format!(
                            "model request made no progress after {} attempts: {error}",
                            policy.max_no_progress_attempts
                        )));
                    }
                    if cancellation.wait_cancelled(policy.retry_delay) {
                        return Err(ModelPrepareError::Cancelled);
                    }
                    continue;
                }
                Err(error) => return Err(ModelPrepareError::Failed(error.to_string())),
            };
            cancellation.check()?;

            let status = response.status().as_u16();
            let advertised = response
                .headers()
                .get("content-length")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            let resumable = if status == 206 {
                let content_range = response
                    .headers()
                    .get("content-range")
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_content_range)
                    .ok_or_else(|| {
                        ModelPrepareError::Failed(
                            "range response omitted a valid Content-Range".to_string(),
                        )
                    })?;
                if content_range != (request_start, expected.size - 1, expected.size) {
                    return Err(ModelPrepareError::Failed(format!(
                        "server returned unexpected Content-Range for {}: expected bytes {}-{}/{}, got bytes {}-{}/{}",
                        path.display(),
                        request_start,
                        expected.size - 1,
                        expected.size,
                        content_range.0,
                        content_range.1,
                        content_range.2
                    )));
                }
                if advertised.is_some_and(|size| size != expected.size - request_start) {
                    return Err(ModelPrepareError::Failed(format!(
                        "server range size mismatch for {}: expected {}, server {}",
                        path.display(),
                        expected.size - request_start,
                        advertised.unwrap_or_default()
                    )));
                }
                true
            } else if status == 200 && request_start == 0 {
                if advertised.is_some_and(|size| size != expected.size) {
                    return Err(ModelPrepareError::Failed(format!(
                        "server size mismatch for {}: manifest {}, server {}",
                        path.display(),
                        expected.size,
                        advertised.unwrap_or_default()
                    )));
                }
                false
            } else {
                return Err(ModelPrepareError::Failed(format!(
                    "server did not honor resume range at byte {request_start} (HTTP {status})"
                )));
            };

            let mut reader = response.into_body().into_reader();
            loop {
                cancellation.check()?;
                let count = match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) => {
                        cancellation.check()?;
                        if !resumable && done != 0 {
                            return Err(ModelPrepareError::Failed(format!(
                                "server does not support safe model download resume: {error}"
                            )));
                        }
                        if done == request_start {
                            no_progress_attempts = no_progress_attempts.saturating_add(1);
                        } else {
                            no_progress_attempts = 0;
                        }
                        if no_progress_attempts > policy.max_no_progress_attempts {
                            return Err(ModelPrepareError::TimedOut(format!(
                                "model body made no progress after {} attempts: {error}",
                                policy.max_no_progress_attempts
                            )));
                        }
                        if Instant::now() >= transfer_deadline {
                            return Err(ModelPrepareError::TimedOut(format!(
                                "model transfer timed out after {:?}",
                                policy.transfer_timeout
                            )));
                        }
                        if cancellation.wait_cancelled(policy.retry_delay) {
                            return Err(ModelPrepareError::Cancelled);
                        }
                        continue 'request;
                    }
                };
                cancellation.check()?;
                let next_done = done.saturating_add(count as u64);
                if next_done > expected.size {
                    return Err(ModelPrepareError::Failed(format!(
                        "model exceeded manifest size: received at least {next_done}, expected {}",
                        expected.size
                    )));
                }
                file.write_all(&buffer[..count])
                    .map_err(|error| ModelPrepareError::Failed(error.to_string()))?;
                hasher.update(&buffer[..count]);
                done = next_done;
                no_progress_attempts = 0;
                if last_progress.elapsed() >= Duration::from_millis(120) {
                    on_progress(done, Some(expected.size));
                    cancellation.check()?;
                    last_progress = Instant::now();
                }
            }

            cancellation.check()?;
            if done == expected.size {
                break;
            }
            if !resumable {
                return Err(ModelPrepareError::Failed(format!(
                    "model size mismatch without resume support: got {done}, expected {}",
                    expected.size
                )));
            }
            if done == request_start {
                no_progress_attempts = no_progress_attempts.saturating_add(1);
                if no_progress_attempts > policy.max_no_progress_attempts {
                    return Err(ModelPrepareError::TimedOut(format!(
                        "model body ended without progress after {} attempts",
                        policy.max_no_progress_attempts
                    )));
                }
                if cancellation.wait_cancelled(policy.retry_delay) {
                    return Err(ModelPrepareError::Cancelled);
                }
            } else {
                no_progress_attempts = 0;
            }
        }

        cancellation.check()?;
        on_progress(done, Some(expected.size));
        cancellation.check()?;
        if done != expected.size {
            return Err(ModelPrepareError::Failed(format!(
                "model size mismatch: got {done}, expected {}",
                expected.size
            )));
        }
        let actual = format!("{:x}", hasher.finalize());
        if !actual.eq_ignore_ascii_case(&expected.sha256) {
            return Err(ModelPrepareError::Failed(format!(
                "model sha256 mismatch: got {actual}, expected {}",
                expected.sha256
            )));
        }
        cancellation.check()?;
        file.sync_all()
            .map_err(|error| ModelPrepareError::Failed(error.to_string()))?;
        drop(file);
        cancellation.check()?;
        install_verified_cancellable(&tmp, path, expected, cancellation)?;
        cancellation.check()?;
        Ok(())
    })();

    if result.is_err() {
        // The exact name was reserved by create_new above. If an attacker has
        // swapped it for a link, fail closed and leave that link untouched.
        let _ = remove_owned_residue(&tmp);
    }
    result
}

pub fn build_asr(
    spec: &ModelSpec,
    dir: &std::path::Path,
    language: &str,
    threads: i32,
) -> Result<Box<dyn Asr>, String> {
    let p = |n: &str| dir.join(n).to_string_lossy().into_owned();
    let threads = threads
        .max(1)
        .min(if spec.kind == Kind::Qwen3 { 8 } else { 4 });
    match spec.kind {
        Kind::Paraformer => SherpaParaformerAsr::new(&p("model.onnx"), &p("tokens.txt"), threads)
            .map(|a| Box::new(a) as Box<dyn Asr>)
            .map_err(|e| e.to_string()),
        Kind::Parakeet => SherpaParakeetAsr::new(
            &p("encoder.onnx"),
            &p("decoder.onnx"),
            &p("joiner.onnx"),
            &p("tokens.txt"),
            threads,
        )
        .map(|a| Box::new(a) as Box<dyn Asr>)
        .map_err(|e| e.to_string()),
        // Chinese uses a zh hint; other supported preferences use auto. Hint
        // or cleaner changes require a rebuild even when the weights match.
        // Its output is already punctuated (use_itn),
        // so `wants_punct` correctly leaves it out of the Chinese punctuator.
        Kind::SenseVoice => SherpaSenseVoiceAsr::new(
            &p("model.int8.onnx"),
            &p("tokens.txt"),
            sensevoice_language_hint(language),
            threads,
            spec.label,
        )
        .map(|a| Box::new(a) as Box<dyn Asr>)
        .map_err(|e| e.to_string()),
        Kind::Qwen3 => SherpaQwen3Asr::new(
            &p("conv_frontend.onnx"),
            &p("encoder.int8.onnx"),
            &p("decoder.int8.onnx"),
            &dir.to_string_lossy(),
            threads,
            spec.label,
        )
        .map(|a| Box::new(a) as Box<dyn Asr>)
        .map_err(|e| e.to_string()),
    }
}

/// Cleaner chain for a model: acronym collapse always, plus the Chinese/English
/// punctuator only when its verified model was requested and is present, and
/// the normalizer always at the tail. Parakeet punctuates its own output but
/// used to skip normalization entirely — the first native Spanish tester got
/// glued sentence boundaries and lowercase sentence starts. For Paraformer the
/// punctuator already normalizes internally; the tail pass is idempotent.
pub fn build_cleaners(
    punct_model: Option<PathBuf>,
    collapse_cjk_spaces: bool,
) -> Result<Vec<Box<dyn TextCleaner>>, String> {
    let mut cleaners: Vec<Box<dyn TextCleaner>> = vec![Box::new(AcronymCollapser)];
    if let Some(m) = punct_model {
        let p = SherpaPunctuator::new(&m.to_string_lossy())
            .map_err(|error| format!("load punctuation model: {error}"))?;
        cleaners.push(Box::new(p));
    }
    cleaners.push(Box::new(Normalizer));
    // Runs after Normalizer (so ASCII space runs are already single) and, being
    // a cleaner, before the recognition-correction dictionary. Japanese route
    // only — see `wants_cjk_space_collapse`.
    if collapse_cjk_spaces {
        cleaners.push(Box::new(JapaneseSpaceCollapser));
    }
    Ok(cleaners)
}

#[cfg(test)]
fn required_punct_path_with(
    model_id: &str,
    language: &str,
    ensure: impl FnOnce() -> Result<PathBuf, String>,
) -> Result<Option<PathBuf>, String> {
    if wants_punct(model_id, language) {
        ensure().map(Some)
    } else {
        Ok(None)
    }
}

/// Prepare the exact cleaner chain advertised for a model. A required
/// punctuator is atomic with ASR readiness: download or load failure returns an
/// error so the control loop can remain closed and retry the whole pipeline.
#[allow(dead_code)] // Compatibility API for diagnostic/non-GUI callers.
pub fn prepare_cleaners(
    model_id: &str,
    language: &str,
    base: &Path,
    on_status: impl FnMut(Progress),
) -> Result<Vec<Box<dyn TextCleaner>>, String> {
    prepare_cleaners_cancellable(
        model_id,
        language,
        base,
        &CancellationToken::new(),
        on_status,
    )
    .map_err(|error| error.to_string())
}

pub fn prepare_cleaners_cancellable(
    model_id: &str,
    language: &str,
    base: &Path,
    cancellation: &CancellationToken,
    on_status: impl FnMut(Progress),
) -> ModelPrepareResult<Vec<Box<dyn TextCleaner>>> {
    cancellation.check()?;
    let punct = if wants_punct(model_id, language) {
        Some(ensure_punct_cancellable(base, cancellation, on_status)?)
    } else {
        None
    };
    cancellation.check()?;
    let cleaners = build_cleaners(punct, wants_cjk_space_collapse(model_id, language))
        .map_err(ModelPrepareError::Failed)?;
    cancellation.check()?;
    Ok(cleaners)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_punctuation_failure_closes_pipeline_and_a_retry_can_recover() {
        let first = required_punct_path_with("paraformer-zh", "zh", || Err("offline".to_string()));
        assert_eq!(first.unwrap_err(), "offline");

        let expected = PathBuf::from("models/punct/model.onnx");
        let recovered =
            required_punct_path_with("paraformer-zh", "zh", || Ok(expected.clone())).unwrap();
        assert_eq!(recovered, Some(expected));

        let not_required = required_punct_path_with("parakeet-tdt-v3", "en", || {
            panic!("a self-punctuating model must not download the punctuator")
        })
        .unwrap();
        assert_eq!(not_required, None);
    }

    /// Nothing may choose a model route on the user's behalf. "auto" and
    /// "multi" are the values old installs carry from when per-utterance routing
    /// existed, and an empty string is a fresh install; all three must send the
    /// person to the picker rather than resolve to some model. Once Parakeet is
    /// chosen, that model still detects among its documented languages itself.
    ///
    /// Two guesses have been tried and both broke someone: inferring from the OS
    /// locale (0.2.0 — a Chinese speaker on an en_US machine lost Chinese and
    /// got their speech decoded as English), and routing per utterance (the
    /// engine changed under the same speaker mid-session).
    #[test]
    fn nothing_is_chosen_for_the_user() {
        for lang in [
            "auto", "", "multi", "th", "garbage", "FR", "fr-CA", "pt-PT", "pt-BR",
        ] {
            assert!(
                route_for("", lang).is_none(),
                "{lang:?} must go to the picker, not to a model"
            );
            assert!(needs_language_pick("", lang));
        }
    }

    /// An explicit choice loads exactly one model, and keeps it.
    #[test]
    fn explicit_language_loads_one_model() {
        assert_eq!(route_for("", "zh"), Some(Route::Single("sensevoice")));
        assert!(!needs_language_pick("", "zh"));
        for (code, _, model) in SUPPORTED_LANGUAGES {
            assert_eq!(
                route_for("", code),
                Some(Route::Single(model)),
                "{code} must resolve to its documented model"
            );
            assert!(!needs_language_pick("", code));
        }
    }

    /// Keep the public picker aligned with the exact language set documented by
    /// NVIDIA for Parakeet TDT 0.6B v3. Broadening this to an arbitrary locale
    /// would turn a product promise into an untested fallback.
    #[test]
    fn parakeet_routes_exactly_its_documented_25_languages() {
        let expected = [
            "bg", "hr", "cs", "da", "nl", "en", "et", "fi", "fr", "de", "el", "hu", "it", "lv",
            "lt", "mt", "pl", "pt", "ro", "sk", "sl", "es", "sv", "ru", "uk",
        ];
        let actual: Vec<_> = SUPPORTED_LANGUAGES
            .iter()
            .filter_map(|(code, _, model)| (*model == "parakeet-tdt-v3").then_some(*code))
            .collect();
        assert_eq!(actual.len(), expected.len());
        for code in expected {
            assert!(actual.contains(&code), "missing documented language {code}");
        }

        let mut unique = actual.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), actual.len(), "duplicate language route");
    }

    #[test]
    fn parakeet_preferences_share_one_loaded_pipeline() {
        assert!(same_model_route("", "en", "", "fr"));
        assert!(same_model_route("", "fr", "", "de"));
        assert!(same_model_route("", "pt", "", "ru"));
        assert!(!same_model_route("", "zh", "", "fr"));
        assert!(!same_model_route("paraformer-zh", "fr", "", "fr"));
    }

    // Regressions discovered by the 2026-09-06 assessment.
    #[test]
    fn assessment_sensevoice_hint_change_requires_reload() {
        assert!(
            !same_model_route("sensevoice", "zh", "sensevoice", "en"),
            "Switching zh -> en must rebuild the zh-pinned SenseVoice decoder"
        );
    }

    #[test]
    fn assessment_sensevoice_cleaner_change_requires_reload() {
        assert!(wants_cjk_space_collapse("sensevoice", "ja"));
        assert!(!wants_cjk_space_collapse("sensevoice", "ko"));
        assert!(
            !same_model_route("sensevoice", "ja", "sensevoice", "ko"),
            "The current commit path only rebuilds cleaners when route identity changes"
        );
    }

    #[test]
    fn sensevoice_auto_routes_reuse_only_equivalent_cleaners() {
        assert!(same_model_route("sensevoice", "en", "sensevoice", "ko"));
        assert!(same_model_route("sensevoice", "ja", "sensevoice", "ja"));
        assert!(!same_model_route("invalid", "xx", "invalid", "xx"));
    }

    /// Every language offered in the picker must resolve to a model that is
    /// actually in the registry. A typo here is not a compile error, so keep the
    /// routing table and registry tied together explicitly.
    #[test]
    fn every_offered_language_has_its_model() {
        for (code, label, id) in SUPPORTED_LANGUAGES {
            assert_eq!(route_for("", code), Some(Route::Single(id)), "{label}");
            assert_eq!(spec_of(id).unwrap().id, *id, "{id} is not in MODELS");
        }
    }

    /// Nothing is downloaded from somebody else's server. The Japanese model's
    /// upstream went 401 mid-development; first-run downloads must come from
    /// infrastructure we control.
    #[test]
    fn every_runtime_url_is_the_exact_validated_mirror_object() {
        // Keep the URLs the app actually fetches structurally tied to the
        // prefix/filename pairs verified by mirror-models.ps1. A host-only
        // check would let the release gate verify one good object while a typo
        // in the runtime path sends every fresh install to a different one.
        const MIRROR: &str = "https://models.vocalcode.app/";
        for m in MODELS {
            for (name, url) in m.files {
                let expected = format!("{MIRROR}{}/{name}", m.id);
                assert_eq!(
                    *url,
                    expected.as_str(),
                    "{}/{name} does not match the validated mirror object",
                    m.id,
                );
            }
        }
        // Not part of MODELS, and that is exactly why it was missed: the
        // registry was mirrored, the guard walked the registry, and the one
        // download declared beside it went on hitting HuggingFace.
        assert_eq!(PUNCT_URL, format!("{MIRROR}punct/model.onnx"));
    }

    /// The punctuator is part of the Paraformer route only. An unsupported or
    /// retired explicit model id cannot use the language to smuggle it back in.
    #[test]
    fn only_paraformer_gets_the_chinese_punctuator() {
        assert!(
            !wants_punct("", "zh"),
            "the default SenseVoice route emits its own punctuation"
        );
        assert!(wants_punct("paraformer-zh", "zh"));
        assert!(wants_punct("paraformer-zh", "en"));
        for (lang, _, _) in SUPPORTED_LANGUAGES {
            assert!(
                !wants_punct("", lang),
                "no automatic route should require the Paraformer punctuator"
            );
        }
        for retired in [
            "zipformer-ko",
            "zipformer-th",
            "zipformer-vi",
            "zipformer-ru",
            "zipformer-yue",
            "zipformer-ja",
            "whisper-base",
        ] {
            assert!(!wants_punct(retired, "zh"), "retired model {retired}");
        }
    }

    /// Only the Japanese SenseVoice route collapses artifact spaces. Korean
    /// shares the model but writes real word spaces, so it must never collapse;
    /// Paraformer/Parakeet routes have nothing to collapse.
    #[test]
    fn only_japanese_collapses_cjk_spaces() {
        assert!(wants_cjk_space_collapse("", "ja"));
        assert!(!wants_cjk_space_collapse("", "ko"));
        assert!(!wants_cjk_space_collapse("", "zh"));
        assert!(!wants_cjk_space_collapse("", "en"));
        // Same conclusion whether the choice is stored as a language or as an
        // explicit model id.
        assert!(wants_cjk_space_collapse("sensevoice", "ja"));
        assert!(!wants_cjk_space_collapse("paraformer-zh", "zh"));
    }

    /// The picker's list lives in the page and the models live here, and when
    /// they drift the failure is silent and total: a language the page offers
    /// but the tables do not resolves to "no route", `needs_language_pick` stays
    /// true, and the user who picks it is handed the picker again for ever. That
    /// shipped in 0.4.15 — six languages were taken out of these tables and left
    /// in the page.
    #[test]
    fn every_language_the_page_offers_has_a_model() {
        let page = include_str!("webui.html");
        let start = page
            .find("var LANGS = [")
            .expect("the picker's language list");
        let end = start + page[start..].find("];").expect("end of LANGS");
        let block = &page[start..end];
        let mut page_codes = Vec::new();
        for line in block.lines() {
            let Some(rest) = line.trim().strip_prefix("[\"") else {
                continue;
            };
            let Some(code) = rest.split('"').next() else {
                continue;
            };
            page_codes.push(code);
            assert!(
                route_for("", code).is_some(),
                "the picker offers {code:?} and no model answers to it — \
                 picking it would loop the first-run screen for ever"
            );
        }
        assert!(
            page_codes.len() >= 2,
            "parsed {} languages out of the page; the parser is broken, not the list",
            page_codes.len()
        );
        assert_eq!(
            page_codes.len(),
            SUPPORTED_LANGUAGES.len(),
            "the page and Rust routing table expose different language counts"
        );
        for (code, label, _) in SUPPORTED_LANGUAGES {
            assert!(page_codes.contains(code), "the picker is missing {code:?}");
            assert!(
                block.contains(&format!("[\"{code}\",\"{label}\",")),
                "the picker and Rust metadata disagree for {code:?}"
            );
        }
    }

    /// An explicit model id wins over the language.
    #[test]
    fn explicit_model_id_wins() {
        assert_eq!(
            route_for("parakeet-tdt-v3", "zh"),
            Some(Route::Single("parakeet-tdt-v3"))
        );
        assert!(!needs_language_pick("parakeet-tdt-v3", "auto"));
    }

    #[test]
    fn routing_ids_resolve() {
        for id in [
            "parakeet-tdt-v3",
            "paraformer-zh",
            "qwen3-asr-0.6b",
            "sensevoice",
        ] {
            assert_eq!(
                spec_of(id).unwrap().id,
                id,
                "{id} missing from the registry"
            );
        }
    }

    #[test]
    fn registry_and_manifest_contain_only_release_approved_models() {
        let mut registry_ids: Vec<_> = MODELS.iter().map(|model| model.id).collect();
        registry_ids.sort_unstable();
        assert_eq!(
            registry_ids,
            [
                "paraformer-zh",
                "parakeet-tdt-v3",
                "qwen3-asr-0.6b",
                "sensevoice",
            ]
        );

        let mut manifest_ids: Vec<_> = artifact_manifest()
            .expect("embedded model manifest")
            .keys()
            .map(String::as_str)
            .collect();
        manifest_ids.sort_unstable();
        assert_eq!(
            manifest_ids,
            [
                "paraformer-zh",
                "parakeet-tdt-v3",
                "punct",
                "qwen3-asr-0.6b",
                "sensevoice",
            ]
        );

        let paraformer = artifact("paraformer-zh", "model.onnx").unwrap();
        assert_eq!(paraformer.size, 243_371_218);
        assert_eq!(
            paraformer.sha256,
            "f36a0433bcf096bd6d6f11b80a3ac8bed110bdca632fe0d731df8d1a84475945"
        );
        let punct = artifact("punct", "model.onnx").unwrap();
        assert_eq!(punct.size, 294_372_519);
        assert_eq!(
            punct.sha256,
            "e93593a6dbd69a07f8734ef269dbe861a379755f8d1c8354719432116f2c44bd"
        );
        let sensevoice = artifact("sensevoice", "model.int8.onnx").unwrap();
        assert_eq!(sensevoice.size, 239_233_841);
        assert_eq!(
            sensevoice.sha256,
            "c71f0ce00bec95b07744e116345e33d8cbbe08cef896382cf907bf4b51a2cd51"
        );
        let qwen_decoder = artifact("qwen3-asr-0.6b", "decoder.int8.onnx").unwrap();
        assert_eq!(qwen_decoder.size, 755_914_231);
        assert_eq!(
            qwen_decoder.sha256,
            "4f6885be5959ae26af3089d38ee7972c5fafbeeb1cf8d5e76eab6d8b61ca5771"
        );
    }

    #[test]
    fn hardware_recommendations_are_one_time_and_language_scoped() {
        let profile = |cores: usize, memory_gib: u64, fast_vector| HardwareProfile {
            logical_cores: cores,
            inference_cores: cores,
            memory_mib: Some(memory_gib * 1024),
            fast_vector,
        };

        assert_eq!(
            recommended_model("zh", profile(4, 16, true)),
            Some("sensevoice")
        );
        assert_eq!(
            recommended_model("zh", profile(8, 8, true)),
            Some("sensevoice")
        );
        assert_eq!(
            recommended_model("zh", profile(12, 32, true)),
            Some("sensevoice")
        );
        assert_eq!(
            recommended_model("hi", profile(4, 8, false)),
            Some("qwen3-asr-0.6b")
        );
        assert_eq!(selectable_models("hi"), &["qwen3-asr-0.6b"]);
        assert_eq!(
            selectable_models("zh"),
            &["paraformer-zh", "sensevoice", "qwen3-asr-0.6b"]
        );
        assert_eq!(
            selectable_models("en"),
            &["sensevoice", "parakeet-tdt-v3", "qwen3-asr-0.6b"]
        );
        assert_eq!(
            recommended_model("en", profile(4, 8, false)),
            Some("sensevoice")
        );
        assert_eq!(
            recommended_model("en", profile(2, 16, true)),
            Some("sensevoice")
        );
        assert_eq!(
            recommended_model("en", profile(4, 16, true)),
            Some("parakeet-tdt-v3")
        );
        assert_eq!(
            recommended_model("en", profile(12, 32, true)),
            Some("parakeet-tdt-v3")
        );
        assert_eq!(
            recommended_threads("qwen3-asr-0.6b", "hi", profile(12, 32, true)),
            6
        );
        assert_eq!(
            recommended_threads("sensevoice", "zh", profile(12, 32, true)),
            4
        );
        assert_eq!(
            recommended_threads("qwen3-asr-0.6b", "hi", profile(4, 8, true)),
            4
        );
    }

    fn profile_of_class(class: PerformanceClass) -> HardwareProfile {
        let (cores, memory_gib) = match class {
            PerformanceClass::Compact => (2, 8),
            PerformanceClass::Standard => (4, 16),
            PerformanceClass::Performance => (16, 64),
        };
        let profile = HardwareProfile {
            logical_cores: cores * 2,
            inference_cores: cores,
            memory_mib: Some(memory_gib * 1024),
            fast_vector: true,
        };
        assert_eq!(performance_class(profile), class);
        profile
    }

    const CLASSES: [PerformanceClass; 3] = [
        PerformanceClass::Compact,
        PerformanceClass::Standard,
        PerformanceClass::Performance,
    ];

    /// Qwen3-ASR is a manual choice for English on every machine: its 512-token
    /// context breaks long audio and it is the slowest route.
    #[test]
    fn english_defaults_to_parakeet_wherever_the_hardware_affords_it() {
        for class in CLASSES {
            let expected = match class {
                PerformanceClass::Compact => "sensevoice",
                PerformanceClass::Standard | PerformanceClass::Performance => "parakeet-tdt-v3",
            };
            let recommended = recommended_model("en", profile_of_class(class));
            assert_eq!(recommended, Some(expected), "{class:?}");
            assert!(selectable_models("en").contains(&expected));
        }
        // The laptop Windows used to count as eight cores.
        let laptop = HardwareProfile {
            logical_cores: 8,
            inference_cores: 4,
            memory_mib: Some(16 * 1024),
            fast_vector: true,
        };
        assert_eq!(recommended_model("en", laptop), Some("parakeet-tdt-v3"));
        assert_eq!(recommended_threads("qwen3-asr-0.6b", "en", laptop), 4);
        assert_eq!(recommended_threads("parakeet-tdt-v3", "en", laptop), 4);
    }

    /// An empty model and the picker's recommendation come from one table.
    /// The single exception is English on a Compact machine, and it is
    /// deliberate (see `recommended_model`): the page stores "sensevoice"
    /// explicitly there, while an empty English model predates that and has
    /// always meant Parakeet, so re-resolving it by hardware would switch an
    /// existing user's model without asking.
    #[test]
    fn empty_model_routing_agrees_with_the_recommendation() {
        for class in CLASSES {
            let profile = profile_of_class(class);
            for (code, _, _) in SUPPORTED_LANGUAGES {
                let recommended = recommended_model(code, profile).map(Route::Single);
                if *code == "en" && class == PerformanceClass::Compact {
                    assert_eq!(route_for("", code), Some(Route::Single("parakeet-tdt-v3")));
                    assert_eq!(recommended, Some(Route::Single("sensevoice")));
                    continue;
                }
                assert_eq!(route_for("", code), recommended, "{code} on {class:?}");
            }
        }
    }

    /// Changing the recommendation must not move anyone who already stored a
    /// model: an explicit id resolves to itself on every class of machine.
    #[test]
    fn a_stored_english_model_survives_the_new_default() {
        for class in CLASSES {
            let profile = profile_of_class(class);
            for &stored in selectable_models("en") {
                assert_eq!(route_for(stored, "en"), Some(Route::Single(stored)));
                assert!(!needs_language_pick(stored, "en"));
                assert!(same_model_route(stored, "en", stored, "en"));
            }
            assert_ne!(recommended_model("en", profile), Some("qwen3-asr-0.6b"));
        }
    }

    #[test]
    fn an_explicit_unknown_model_is_an_error_not_a_fallback() {
        for unknown in [
            "typo-model",
            "zipformer-ko",
            "zipformer-th",
            "zipformer-vi",
            "zipformer-ru",
            "zipformer-yue",
            "zipformer-ja",
            "whisper-base",
        ] {
            assert_eq!(route_for(unknown, "en"), None, "{unknown}");
            // An explicit value is not an unanswered language question. Let
            // prepare report the exact bad id instead of reopening a picker.
            assert!(!needs_language_pick(unknown, "en"), "{unknown}");
            let error = prepare_asr(unknown, "en", Path::new("."), 1, |_| {})
                .err()
                .expect("unknown model must fail before any download");
            assert!(error.contains(unknown), "{error}");
        }
    }

    #[test]
    fn every_download_has_a_size_and_sha256_in_the_manifest() {
        for model in MODELS {
            for (name, _) in model.files {
                let item = artifact(model.id, name).unwrap_or_else(|e| panic!("{e}"));
                assert!(item.size > 0, "{}/{name} has zero size", model.id);
                assert_eq!(item.sha256.len(), 64, "{}/{name}", model.id);
                assert!(
                    item.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                    "{}/{name} has a non-hex sha256",
                    model.id
                );
            }
        }
        let punct = artifact("punct", "model.onnx").expect("punctuation manifest entry");
        assert!(punct.size > 0);
        assert_eq!(punct.sha256.len(), 64);
    }

    /// Every model must declare a non-zero size: it is the denominator for the
    /// download bar when a server omits Content-Length, and a zero would make
    /// the bar divide by nothing.
    #[test]
    fn every_model_declares_a_size() {
        for m in MODELS {
            assert!(m.size_mb > 0, "{} has no size_mb", m.id);
            assert!(!m.files.is_empty(), "{} has no files", m.id);
        }
    }

    #[test]
    fn progress_percent_is_bounded() {
        let p = Progress {
            label: "x".into(),
            done: 500,
            total: 1000,
        };
        assert_eq!(p.percent().round() as i64, 50);
        // Overshoot happens when the registry size understates the real file.
        let over = Progress {
            label: "x".into(),
            done: 2000,
            total: 1000,
        };
        assert_eq!(over.percent().round() as i64, 100);
    }

    /// Registry labels are too long for a progress line; only the name is kept.
    #[test]
    fn short_label_trims_the_description() {
        assert_eq!(
            short_label("Parakeet TDT v3 · auto-detects 25 languages"),
            "Parakeet TDT v3"
        );
        assert_eq!(short_label("Punctuation model"), "Punctuation model");
    }

    #[test]
    fn model_status_labels_use_the_ui_source_language() {
        let label = |id| {
            MODELS
                .iter()
                .find(|candidate| candidate.id == id)
                .expect("registered model")
                .label
        };
        assert_eq!(
            label("parakeet-tdt-v3"),
            "Parakeet TDT v3 · auto-detects 25 languages"
        );
        assert_eq!(
            label("paraformer-zh"),
            "Paraformer · Mandarin Chinese + English"
        );
    }
}

#[cfg(test)]
mod download_tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::net::TcpListener;
    use std::process::{Command, Stdio};
    use std::sync::atomic::AtomicBool;
    use std::sync::{mpsc, Arc, Barrier};
    use std::thread;
    use std::time::Instant;

    fn expected_bytes(bytes: &[u8]) -> Artifact {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Artifact {
            size: bytes.len() as u64,
            sha256: format!("{:x}", hasher.finalize()),
        }
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            let count = stream.read(&mut byte).unwrap();
            assert_ne!(count, 0, "client closed before sending headers");
            request.extend_from_slice(&byte[..count]);
        }
        String::from_utf8(request).unwrap()
    }

    #[test]
    fn model_lock_wait_is_cancelled_without_waiting_for_the_holder() {
        let directory = TempDir::new("model-install-cancel-lock-wait");
        let held = ModelInstallLock::acquire(directory.path()).unwrap();
        let cancellation = CancellationToken::new();
        let waiter_token = cancellation.clone();
        let waiter_path = directory.path().to_path_buf();
        let (started_tx, started_rx) = mpsc::channel();
        let waiter = thread::spawn(move || {
            started_tx.send(()).unwrap();
            ModelInstallLock::acquire_cancellable(
                &waiter_path,
                &waiter_token,
                Duration::from_secs(30),
            )
        });

        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        thread::sleep(Duration::from_millis(75));
        let cancelled_at = Instant::now();
        cancellation.cancel();
        let error = waiter.join().unwrap().unwrap_err();
        assert_eq!(error, ModelPrepareError::Cancelled);
        // 100x the 50 ms lock poll; ignoring cancellation would take 30 s.
        assert!(
            cancelled_at.elapsed() < Duration::from_secs(5),
            "lock waiter ignored cancellation for {:?}",
            cancelled_at.elapsed()
        );
        drop(held);
    }

    #[test]
    fn model_lock_wait_has_a_hard_deadline() {
        let directory = TempDir::new("model-install-lock-deadline");
        let held = ModelInstallLock::acquire(directory.path()).unwrap();
        let started = Instant::now();
        let error = ModelInstallLock::acquire_cancellable(
            directory.path(),
            &CancellationToken::new(),
            Duration::from_millis(120),
        )
        .unwrap_err();
        assert!(matches!(error, ModelPrepareError::TimedOut(_)), "{error}");
        assert!(
            started.elapsed() >= Duration::from_millis(100)
                && started.elapsed() < Duration::from_secs(2),
            "unexpected lock deadline: {:?}",
            started.elapsed()
        );
        drop(held);
    }

    struct CancelAfterFirstFetch {
        calls: usize,
    }

    impl ArtifactDownloader for CancelAfterFirstFetch {
        fn fetch(
            &mut self,
            _url: &str,
            _path: &Path,
            _expected: &Artifact,
            cancellation: &CancellationToken,
            _on_progress: &mut dyn FnMut(u64, Option<u64>),
        ) -> ModelPrepareResult<()> {
            self.calls += 1;
            cancellation.cancel();
            Ok(())
        }
    }

    #[test]
    fn cancellation_after_one_fetch_never_requests_the_next_artifact() {
        let base = TempDir::new("model-install-cancel-before-next-artifact");
        let cancellation = CancellationToken::new();
        let mut downloader = CancelAfterFirstFetch { calls: 0 };
        let error = ensure_cancellable_with_downloader(
            &MODELS[0],
            base.path(),
            &cancellation,
            |_| {},
            &mut downloader,
        )
        .unwrap_err();

        assert_eq!(error, ModelPrepareError::Cancelled);
        assert_eq!(downloader.calls, 1, "a second artifact was requested");
    }

    #[test]
    fn stalled_http_body_is_cancelled_within_one_short_io_slice() {
        const BYTES: &[u8] = b"four";
        let directory = TempDir::new("model-install-stalled-body-cancel");
        let canonical = directory.path().join("model.onnx");
        let expected = expected_bytes(BYTES);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (body_started_tx, body_started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            assert!(
                request.to_ascii_lowercase().contains("range: bytes=0-3"),
                "request was not resumable: {request}"
            );
            stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 0-3/4\r\nConnection: close\r\n\r\nf",
                )
                .unwrap();
            stream.flush().unwrap();
            body_started_tx.send(()).unwrap();
            // Keep the peer and body open much longer than the client's I/O
            // slice. Cancellation must return while this wait is still active.
            let _ = release_rx.recv_timeout(Duration::from_secs(30));
        });

        let cancellation = CancellationToken::new();
        let download_token = cancellation.clone();
        let download_path = canonical.clone();
        let downloader = thread::spawn(move || {
            let policy = DownloadPolicy {
                https_only: false,
                io_slice_timeout: Duration::from_millis(250),
                transfer_timeout: Duration::from_secs(30),
                retry_delay: Duration::from_millis(10),
                max_no_progress_attempts: 2,
            };
            download_with_policy(
                &format!("http://{address}/model.onnx"),
                &download_path,
                &expected,
                &download_token,
                policy,
                &mut |_, _| {},
            )
        });

        body_started_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        // Let the client consume the one byte and enter its next blocking read.
        thread::sleep(Duration::from_millis(50));
        let cancelled_at = Instant::now();
        cancellation.cancel();
        let error = downloader.join().unwrap().unwrap_err();
        assert_eq!(error, ModelPrepareError::Cancelled);
        assert!(
            cancelled_at.elapsed() < Duration::from_millis(2_500),
            "stalled body held cancellation for {:?}",
            cancelled_at.elapsed()
        );
        assert!(!canonical.exists(), "cancelled bytes were published");
        assert!(
            exact_residues(&canonical).is_empty(),
            "cancelled partial was not removed"
        );
        let _ = release_tx.send(());
        server.join().unwrap();
    }

    #[test]
    fn timed_out_range_resumes_from_the_exact_hashed_offset() {
        const BYTES: &[u8] = b"abcdefgh";
        let directory = TempDir::new("model-install-range-resume");
        let canonical = directory.path().join("model.onnx");
        let expected = expected_bytes(BYTES);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let first_request = read_http_request(&mut first);
            assert!(
                first_request
                    .to_ascii_lowercase()
                    .contains("range: bytes=0-7"),
                "first request had the wrong range: {first_request}"
            );
            first
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Length: 8\r\nContent-Range: bytes 0-7/8\r\nConnection: close\r\n\r\nabcd",
                )
                .unwrap();
            first.flush().unwrap();
            let first_handler = thread::spawn(move || {
                // Keep the incomplete response open past the client's slice.
                let _ = release_rx.recv_timeout(Duration::from_secs(30));
                drop(first);
            });

            let (mut second, _) = listener.accept().unwrap();
            let second_request = read_http_request(&mut second);
            assert!(
                second_request
                    .to_ascii_lowercase()
                    .contains("range: bytes=4-7"),
                "resume did not start at the hashed offset: {second_request}"
            );
            second
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 4-7/8\r\nConnection: close\r\n\r\nefgh",
                )
                .unwrap();
            second.flush().unwrap();
            first_handler.join().unwrap();
        });

        let policy = DownloadPolicy {
            https_only: false,
            io_slice_timeout: Duration::from_secs(2),
            transfer_timeout: Duration::from_secs(30),
            retry_delay: Duration::from_millis(10),
            max_no_progress_attempts: 2,
        };
        download_with_policy(
            &format!("http://{address}/model.onnx"),
            &canonical,
            &expected,
            &CancellationToken::new(),
            policy,
            &mut |_, _| {},
        )
        .unwrap();

        assert_eq!(std::fs::read(&canonical).unwrap(), BYTES);
        assert!(exact_residues(&canonical).is_empty());
        let _ = release_tx.send(());
        server.join().unwrap();
    }

    #[test]
    fn model_transfer_deadline_is_absolute_across_resumable_requests() {
        const BYTES: &[u8] = b"deadline";
        let directory = TempDir::new("model-install-transfer-deadline");
        let canonical = directory.path().join("model.onnx");
        let expected = expected_bytes(BYTES);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server = thread::spawn(move || {
            let mut held_connections = Vec::new();
            while !server_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let request = read_http_request(&mut stream);
                        assert!(
                            request.to_ascii_lowercase().contains("range: bytes=0-7"),
                            "deadline retry had the wrong range: {request}"
                        );
                        stream
                            .write_all(
                                b"HTTP/1.1 206 Partial Content\r\nContent-Length: 8\r\nContent-Range: bytes 0-7/8\r\nConnection: close\r\n\r\n",
                            )
                            .unwrap();
                        stream.flush().unwrap();
                        // Keep every response body open and motionless. Each
                        // individual request times out, but only the one
                        // transfer deadline is allowed to govern the retries.
                        held_connections.push(stream);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept deadline test connection: {error}"),
                }
            }
        });

        let policy = DownloadPolicy {
            https_only: false,
            io_slice_timeout: Duration::from_millis(100),
            transfer_timeout: Duration::from_millis(350),
            retry_delay: Duration::from_millis(5),
            max_no_progress_attempts: 100,
        };
        let started = Instant::now();
        let error = download_with_policy(
            &format!("http://{address}/model.onnx"),
            &canonical,
            &expected,
            &CancellationToken::new(),
            policy,
            &mut |_, _| {},
        )
        .unwrap_err();
        let elapsed = started.elapsed();
        stop.store(true, Ordering::Release);
        server.join().unwrap();

        assert!(matches!(error, ModelPrepareError::TimedOut(_)), "{error}");
        assert!(
            elapsed >= Duration::from_millis(300) && elapsed < Duration::from_millis(3_500),
            "transfer deadline was not absolute: {elapsed:?}"
        );
        assert!(!canonical.exists(), "timed-out bytes were published");
        assert!(
            exact_residues(&canonical).is_empty(),
            "timed-out partial was not removed"
        );
    }

    fn verified_partial(canonical: &Path, bytes: &[u8]) -> PathBuf {
        let (path, mut file) = create_download_temp(canonical).unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
        drop(file);
        path
    }

    fn exact_residues(canonical: &Path) -> Vec<(PathBuf, ResidueKind)> {
        owned_residues(canonical).unwrap()
    }

    fn install_test_artifact(
        model_directory: &Path,
        canonical: &Path,
        expected: &Artifact,
        bytes: &[u8],
        publisher_marker: Option<&Path>,
    ) -> Result<bool, String> {
        with_model_install_lock(model_directory, || {
            if recover_artifact_locked(canonical, expected)? {
                return Ok(false);
            }
            if let Some(marker) = publisher_marker {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(marker)
                    .map_err(|error| format!("create publisher marker: {error}"))?;
            }
            let partial = verified_partial(canonical, bytes);
            install_verified(&partial, canonical, expected)?;
            if !recover_artifact_locked(canonical, expected)? {
                return Err("published test artifact was not reusable".to_string());
            }
            Ok(true)
        })
    }

    #[test]
    fn cache_validation_checks_both_size_and_hash() {
        let dir = TempDir::new("model-install-cache");
        let path = dir.path().join("model.bin");
        std::fs::write(&path, b"known good bytes").unwrap();
        let expected = Artifact {
            size: 16,
            sha256: sha256_file(&path).unwrap(),
        };
        assert!(artifact_is_valid(&path, &expected));

        // Same length, different bytes: a size-only cache check would accept it.
        std::fs::write(&path, b"known evil bytes").unwrap();
        assert!(!artifact_is_valid(&path, &expected));

        std::fs::write(&path, b"short").unwrap();
        assert!(!artifact_is_valid(&path, &expected));
    }

    #[test]
    fn verified_install_replaces_a_bad_cache_and_cleans_backup() {
        const VERIFIED: &[u8] = b"verified model bytes";
        let dir = TempDir::new("model-install-replace");
        let canonical = dir.path().join("encoder.onnx");
        let expected = expected_bytes(VERIFIED);
        std::fs::write(&canonical, b"corrupt model bytes!").unwrap();
        let partial = verified_partial(&canonical, VERIFIED);

        with_model_install_lock(dir.path(), || {
            install_verified(&partial, &canonical, &expected)
        })
        .unwrap();

        assert_eq!(std::fs::read(&canonical).unwrap(), VERIFIED);
        assert!(!partial.exists());
        assert!(exact_residues(&canonical).is_empty());
    }

    #[test]
    fn partial_downloads_are_created_exclusively() {
        let dir = TempDir::new("model-install-exclusive-partial");
        let final_path = dir.path().join("encoder.onnx");
        let (first_path, mut first) = create_download_temp(&final_path).unwrap();
        first.write_all(b"do not truncate").unwrap();
        first.sync_all().unwrap();
        drop(first);

        let (second_path, second) = create_download_temp(&final_path).unwrap();
        drop(second);
        assert_ne!(first_path, second_path);
        assert_eq!(std::fs::read(&first_path).unwrap(), b"do not truncate");
    }

    #[test]
    fn crash_before_replace_keeps_canonical_and_recovery_cleans_partial() {
        const VERIFIED: &[u8] = b"new canonical bytes";
        const OLD: &[u8] = b"old canonical bytes";
        assert_eq!(VERIFIED.len(), OLD.len());
        let dir = TempDir::new("model-install-before-replace-crash");
        let canonical = dir.path().join("model.onnx");
        let expected = expected_bytes(VERIFIED);
        std::fs::write(&canonical, OLD).unwrap();
        let partial = verified_partial(&canonical, VERIFIED);

        let error = with_model_install_lock(dir.path(), || {
            install_verified_with_fault(
                &partial,
                &canonical,
                &expected,
                InstallFault::BeforePublish,
            )
        })
        .unwrap_err();
        assert!(error.contains("before model replacement"), "{error}");
        assert_eq!(std::fs::read(&canonical).unwrap(), OLD);
        assert!(partial.exists());

        let valid = with_model_install_lock(dir.path(), || {
            recover_artifact_locked(&canonical, &expected)
        })
        .unwrap();
        assert!(!valid, "the old canonical has the wrong SHA-256");
        assert_eq!(std::fs::read(&canonical).unwrap(), OLD);
        assert!(!partial.exists(), "a crashed partial must be cleaned");
    }

    #[test]
    fn crash_after_replace_leaves_valid_canonical_and_recovery_cleans_backup() {
        const VERIFIED: &[u8] = b"new canonical bytes";
        const OLD: &[u8] = b"old canonical bytes";
        let dir = TempDir::new("model-install-after-replace-crash");
        let canonical = dir.path().join("model.onnx");
        let expected = expected_bytes(VERIFIED);
        std::fs::write(&canonical, OLD).unwrap();
        let partial = verified_partial(&canonical, VERIFIED);

        let error = with_model_install_lock(dir.path(), || {
            install_verified_with_fault(&partial, &canonical, &expected, InstallFault::AfterPublish)
        })
        .unwrap_err();
        assert!(error.contains("after model replacement"), "{error}");
        assert_eq!(std::fs::read(&canonical).unwrap(), VERIFIED);
        assert!(!partial.exists());

        let valid = with_model_install_lock(dir.path(), || {
            recover_artifact_locked(&canonical, &expected)
        })
        .unwrap();
        assert!(valid);
        assert_eq!(std::fs::read(&canonical).unwrap(), VERIFIED);
        assert!(exact_residues(&canonical).is_empty());
    }

    #[test]
    fn repeated_atomic_replace_never_makes_canonical_name_missing() {
        let dir = TempDir::new("model-install-no-canonical-gap");
        let canonical = dir.path().join("model.onnx");
        std::fs::write(&canonical, b"version-0000").unwrap();
        let observed_missing = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let start = Arc::new(Barrier::new(2));
        let observer_path = canonical.clone();
        let observer_missing = Arc::clone(&observed_missing);
        let observer_stop = Arc::clone(&stop);
        let observer_start = Arc::clone(&start);
        let observer = std::thread::spawn(move || {
            observer_start.wait();
            while !observer_stop.load(Ordering::SeqCst) {
                if std::fs::symlink_metadata(&observer_path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                {
                    observer_missing.store(true, Ordering::SeqCst);
                }
                std::thread::yield_now();
            }
        });

        start.wait();
        with_model_install_lock(dir.path(), || {
            for version in 1..128 {
                let bytes = format!("version-{version:04}");
                let expected = expected_bytes(bytes.as_bytes());
                let partial = verified_partial(&canonical, bytes.as_bytes());
                install_verified(&partial, &canonical, &expected)?;
            }
            Ok(())
        })
        .unwrap();
        stop.store(true, Ordering::SeqCst);
        observer.join().unwrap();
        assert!(
            !observed_missing.load(Ordering::SeqCst),
            "replacement exposed a missing canonical pathname"
        );
        assert_eq!(std::fs::read(&canonical).unwrap(), b"version-0127");
    }

    #[test]
    fn missing_canonical_is_recovered_from_verified_owned_backup() {
        const VERIFIED: &[u8] = b"recoverable model";
        let dir = TempDir::new("model-install-recover-backup");
        let canonical = dir.path().join("model.onnx");
        let backup = canonical.with_extension("replaced-4242-7");
        let expected = expected_bytes(VERIFIED);
        std::fs::write(&backup, VERIFIED).unwrap();

        let valid = with_model_install_lock(dir.path(), || {
            recover_artifact_locked(&canonical, &expected)
        })
        .unwrap();
        assert!(valid);
        assert_eq!(std::fs::read(&canonical).unwrap(), VERIFIED);
        assert!(!backup.exists());
    }

    #[test]
    fn corrupt_owned_backup_is_removed_but_never_published() {
        const VERIFIED: &[u8] = b"recoverable model";
        const CORRUPT: &[u8] = b"corrupt_____model";
        assert_eq!(VERIFIED.len(), CORRUPT.len());
        let dir = TempDir::new("model-install-corrupt-backup");
        let canonical = dir.path().join("model.onnx");
        let backup = canonical.with_extension("replaced-4242-7");
        let expected = expected_bytes(VERIFIED);
        std::fs::write(&backup, CORRUPT).unwrap();

        let valid = with_model_install_lock(dir.path(), || {
            recover_artifact_locked(&canonical, &expected)
        })
        .unwrap();
        assert!(!valid);
        assert!(!canonical.exists());
        assert!(!backup.exists());
    }

    #[test]
    fn cleanup_removes_only_strict_owned_crash_residue() {
        const VERIFIED: &[u8] = b"verified";
        let dir = TempDir::new("model-install-strict-cleanup");
        let canonical = dir.path().join("encoder.onnx");
        let expected = expected_bytes(VERIFIED);
        std::fs::write(&canonical, VERIFIED).unwrap();
        let stale_part = canonical.with_extension("part-4242-9");
        let stale_backup = canonical.with_extension("replaced-4242-9");
        std::fs::write(&stale_part, b"partial").unwrap();
        std::fs::write(&stale_backup, b"old").unwrap();
        let lookalikes = [
            canonical.with_extension("part-4242"),
            canonical.with_extension("part-nope-9"),
            canonical.with_extension("part-04242-9"),
            canonical.with_extension("part-4242-09"),
            canonical.with_extension("part-4294967296-9"),
            canonical.with_extension("part-4242-9-extra"),
            canonical.with_extension("replaced-nope-9"),
            canonical.with_extension("replaced-0-9"),
            canonical.with_extension("replaced-4242-18446744073709551616"),
            canonical.with_extension("download-4242-9"),
        ];
        for path in &lookalikes {
            std::fs::write(path, b"must stay").unwrap();
        }

        let valid = with_model_install_lock(dir.path(), || {
            recover_artifact_locked(&canonical, &expected)
        })
        .unwrap();
        assert!(valid);
        assert!(!stale_part.exists());
        assert!(!stale_backup.exists());
        for path in &lookalikes {
            assert!(path.exists(), "lookalike was removed: {}", path.display());
        }
    }

    #[cfg(any(unix, windows))]
    fn symlink_file_for_test(target: &Path, link: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link)
        }
    }

    #[test]
    #[cfg(windows)]
    fn windows_reparse_attribute_is_statically_fail_closed() {
        assert!(!windows_attributes_are_reparse(0));
        assert!(windows_attributes_are_reparse(FILE_ATTRIBUTE_REPARSE_POINT));
        assert!(windows_attributes_are_reparse(
            FILE_ATTRIBUTE_REPARSE_POINT | 0x20
        ));
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn no_follow_guards_lock_canonical_and_owned_residue_victims() {
        const VICTIM: &[u8] = b"victim bytes must never change";

        let lock_dir = TempDir::new("model-install-lock-symlink");
        let lock_victim = lock_dir.path().join("lock-victim.bin");
        let lock_link = lock_dir.path().join(".install.lock");
        std::fs::write(&lock_victim, VICTIM).unwrap();
        if let Err(error) = symlink_file_for_test(&lock_victim, &lock_link) {
            #[cfg(windows)]
            {
                eprintln!("skipping Windows reparse test without symlink privilege: {error}");
                return;
            }
            #[cfg(unix)]
            panic!("create lock symlink: {error}");
        }
        let error =
            ModelInstallLock::acquire(lock_dir.path()).expect_err("a symlink lock must be refused");
        assert!(error.contains("symlink") || error.contains("reparse"));
        assert_eq!(std::fs::read(&lock_victim).unwrap(), VICTIM);

        let canonical_dir = TempDir::new("model-install-canonical-symlink");
        let canonical_victim = canonical_dir.path().join("canonical-victim.bin");
        let canonical = canonical_dir.path().join("model.onnx");
        std::fs::write(&canonical_victim, VICTIM).unwrap();
        symlink_file_for_test(&canonical_victim, &canonical).unwrap();
        let expected = expected_bytes(VICTIM);
        let error = with_model_install_lock(canonical_dir.path(), || {
            recover_artifact_locked(&canonical, &expected)
        })
        .unwrap_err();
        assert!(error.contains("symlink") || error.contains("reparse"));
        assert_eq!(std::fs::read(&canonical_victim).unwrap(), VICTIM);

        let residue_dir = TempDir::new("model-install-residue-symlink");
        let residue_victim = residue_dir.path().join("residue-victim.bin");
        let residue_canonical = residue_dir.path().join("model.onnx");
        let residue_link = residue_canonical.with_extension("part-4242-9");
        std::fs::write(&residue_victim, VICTIM).unwrap();
        std::fs::write(&residue_canonical, VICTIM).unwrap();
        symlink_file_for_test(&residue_victim, &residue_link).unwrap();
        let error = with_model_install_lock(residue_dir.path(), || {
            recover_artifact_locked(&residue_canonical, &expected)
        })
        .unwrap_err();
        assert!(error.contains("symlink") || error.contains("reparse"));
        assert_eq!(std::fs::read(&residue_victim).unwrap(), VICTIM);
        assert!(std::fs::symlink_metadata(&residue_link)
            .unwrap()
            .file_type()
            .is_symlink());

        let backup_dir = TempDir::new("model-install-backup-symlink");
        let backup_victim = backup_dir.path().join("backup-victim.bin");
        let backup_canonical = backup_dir.path().join("model.onnx");
        let backup_link = backup_canonical.with_extension("replaced-4242-9");
        std::fs::write(&backup_victim, VICTIM).unwrap();
        symlink_file_for_test(&backup_victim, &backup_link).unwrap();
        let error = with_model_install_lock(backup_dir.path(), || {
            recover_artifact_locked(&backup_canonical, &expected)
        })
        .unwrap_err();
        assert!(error.contains("symlink") || error.contains("reparse"));
        assert_eq!(std::fs::read(&backup_victim).unwrap(), VICTIM);
        assert!(std::fs::symlink_metadata(&backup_link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    const CONCURRENT_CHILD_DIR: &str = "VOCALCODE_MODEL_INSTALL_TEST_CHILD_DIR";

    fn count_named(directory: &Path, prefix: &str) -> usize {
        std::fs::read_dir(directory)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
            .count()
    }

    #[test]
    fn concurrent_process_writers_publish_once_and_second_reuses() {
        const VERIFIED: &[u8] = b"one publisher, two callers";
        if let Some(directory) = std::env::var_os(CONCURRENT_CHILD_DIR) {
            let directory = PathBuf::from(directory);
            let ready = directory.join(format!("ready-{}", std::process::id()));
            std::fs::write(&ready, b"ready").unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while !directory.join("go").exists() {
                assert!(
                    Instant::now() < deadline,
                    "parent never released child writer"
                );
                std::thread::sleep(Duration::from_millis(10));
            }

            let canonical = directory.join("model.onnx");
            let expected = expected_bytes(VERIFIED);
            let marker = directory.join(format!("published-{}", std::process::id()));
            install_test_artifact(&directory, &canonical, &expected, VERIFIED, Some(&marker))
                .unwrap();
            return;
        }

        let directory = TempDir::new("model-install-concurrent-processes");
        let executable = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for _ in 0..2 {
            children.push(
                Command::new(&executable)
                    .arg("concurrent_process_writers_publish_once_and_second_reuses")
                    .arg("--nocapture")
                    .arg("--test-threads=1")
                    .env(CONCURRENT_CHILD_DIR, directory.path())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
        }

        let deadline = Instant::now() + Duration::from_secs(30);
        while count_named(directory.path(), "ready-") < 2 {
            if Instant::now() >= deadline {
                for child in &mut children {
                    let _ = child.kill();
                }
                panic!("both child writers did not become ready");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::write(directory.path().join("go"), b"go").unwrap();

        for child in children {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "child writer failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_eq!(
            count_named(directory.path(), "published-"),
            1,
            "only the lock winner may publish"
        );
        let canonical = directory.path().join("model.onnx");
        assert!(artifact_is_valid(&canonical, &expected_bytes(VERIFIED)));
        assert!(exact_residues(&canonical).is_empty());
    }
}
