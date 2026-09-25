//! The VocalCode app window: a WebView2 surface (via `wry`) hosting `webui.html`,
//! driven by a `tao` event loop, with a tray icon and a small JSON IPC bridge.
//!
//! - Rust → JS: `window.vocalcodeInit(cfg)` once, then `window.vocalcodeStatus(state)`
//!   on a timer (listening / last text / model / license).
//! - JS → Rust: `window.ipc.postMessage(json)` for save / activate / buy.
//!
//! The heavy audio/ASR/injection work runs on the background threads started in
//! `main`; this module is purely the control surface.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde_json::Value;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::WindowBuilder;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use vocalcode_core::config::{
    validate_trigger_bounds, MAX_IGNORED_MEETING_APPS, MAX_MEETING_APP_KEY_UTF8_BYTES,
};
use vocalcode_core::limits::{
    MAX_CONFIG_TOKEN_UTF8_BYTES, MAX_DICTIONARY_DOCUMENT_BYTES, MAX_DICTIONARY_RULES,
    MAX_DICTIONARY_SIDE_UTF8_BYTES, MAX_INPUT_DEVICE_UTF8_BYTES, MAX_SERIALIZED_TRIGGER_UTF8_BYTES,
    MAX_SETTINGS_IPC_BYTES, MAX_TRIGGERS_PER_ACTION, MAX_UI_LANGUAGE_UTF8_BYTES,
};
use vocalcode_core::{Config, MouseExtra, Trigger};
use vocalcode_platform::CaptureShared;
use wry::WebViewBuilder;

/// Where to send buyers: the same first-party checkout the website uses.
///
/// This was a Stripe Payment Link from the initial commit until 2026-08-24,
/// and that link had been left behind in an account this deployment no longer
/// sells from. Fulfilment matches a completed session against the configured
/// links, so a purchase made through it would have been charged and then
/// refused a licence. Routing through the site keeps the buyer on one storefront
/// whose price, allowlist and health probe all move together.
const BUY_URL: &str = "https://vocalcode.app/buy/";
const DELIVER_URL: &str = "https://vocalcode-deliver.wudaming00.workers.dev";
const CHECKOUT_INTENT_RESPONSE_MAX_BYTES: usize = 8 * 1024;
const CHECKOUT_REGISTRATION_ERROR: &str =
    "Could not securely register this checkout. Please try again.";
const USER_DATA_PURGE_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(target_os = "macos")]
const MAC_METADATA_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(target_os = "macos")]
const MAC_UPDATE_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(target_os = "macos")]
const MAC_UPDATE_COPY_TIMEOUT: Duration = Duration::from_secs(120);
#[cfg(target_os = "macos")]
const MAC_UPDATE_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(windows)]
const WINDOWS_SIGNATURE_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const CONFIG_SAVE_QUEUE_CAPACITY: usize = 16;
const SERVICE_WORKER_LIMIT: usize = 16;
const TEACH_WORKER_LIMIT: usize = 8;
#[cfg(target_os = "macos")]
const MAC_AUTOSTART_PLIST_MAX_BYTES: usize = 1024 * 1024;
#[cfg(target_os = "macos")]
const MAC_UPDATE_TRANSACTION_MAX_BYTES: usize = 16 * 1024;

#[cfg(target_os = "macos")]
fn read_bounded_local_file(path: &Path, maximum: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;

    let probe_limit = maximum.checked_add(1).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid local file limit")
    })?;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(probe_limit as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file exceeds the {maximum}-byte safety limit"),
        ));
    }
    Ok(bytes)
}

fn purchase_poll_sleep(now: Instant, deadline: Instant) -> Option<Duration> {
    let remaining = deadline.checked_duration_since(now)?;
    (!remaining.is_zero()).then_some(remaining.min(Duration::from_secs(3)))
}

fn purchase_poll_request_timeout(now: Instant, deadline: Instant) -> Option<Duration> {
    let remaining = deadline.checked_duration_since(now)?;
    (!remaining.is_zero()).then_some(remaining.min(Duration::from_secs(10)))
}

fn checkout_intent_reply_accepted(status_success: bool, body: &[u8]) -> bool {
    if !status_success || body.len() > CHECKOUT_INTENT_RESPONSE_MAX_BYTES {
        return false;
    }
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|reply| reply.get("ok").and_then(Value::as_bool))
        == Some(true)
}

fn send_restore_request(email: String) -> anyhow::Result<bool> {
    let body = serde_json::json!({ "email": email }).to_string();
    ureq::post(&format!("{DELIVER_URL}/restore"))
        .config()
        .https_only(true)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .send(body)
        .map(|response| response.status().as_u16() == 202)
        .map_err(|error| anyhow::anyhow!("restore request failed: {error}"))
}

fn send_checkout_intent_request(body: String) -> anyhow::Result<bool> {
    let mut response = ureq::post(&format!("{DELIVER_URL}/intent"))
        .config()
        .https_only(true)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .send(body)
        .map_err(|error| anyhow::anyhow!("checkout registration request failed: {error}"))?;
    let status_success = response.status().is_success();
    // Read max+1 so the exact limit remains valid while an oversized or
    // decompression-bomb response is distinguishable and rejected.
    let reply = response
        .body_mut()
        .with_config()
        .limit((CHECKOUT_INTENT_RESPONSE_MAX_BYTES + 1) as u64)
        .read_to_vec()
        .map_err(|error| anyhow::anyhow!("read checkout registration response: {error}"))?;
    Ok(checkout_intent_reply_accepted(status_success, &reply))
}

enum PurchasePollReply {
    Retry,
    Rejected,
    Receipt(String),
}

fn send_purchase_poll_request(
    body: String,
    request_timeout: Duration,
) -> anyhow::Result<PurchasePollReply> {
    let mut response = ureq::post(&format!("{DELIVER_URL}/poll"))
        .config()
        .https_only(true)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_connect(Some(request_timeout.min(Duration::from_secs(5))))
        .timeout_global(Some(request_timeout))
        .build()
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .send(body)
        .map_err(|error| anyhow::anyhow!("checkout poll request failed: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        // Never parse or display a remote rejection body: it may reflect the
        // reference/device or contain forged log/UI text.
        return Ok(if status.is_client_error() && status.as_u16() != 429 {
            PurchasePollReply::Rejected
        } else {
            PurchasePollReply::Retry
        });
    }
    let reply = response
        .body_mut()
        .with_config()
        .limit(64 * 1024)
        .read_json::<serde_json::Value>()
        .map_err(|error| anyhow::anyhow!("read checkout poll response: {error}"))?;
    Ok(reply
        .get("token")
        .and_then(|token| token.as_str())
        .map(|token| PurchasePollReply::Receipt(token.to_string()))
        .unwrap_or(PurchasePollReply::Retry))
}

/// Live runtime state the background threads publish and the UI polls.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownAction {
    #[default]
    None,
    Quit,
    PurgeData,
    RestartForUpdate,
}

const DICTIONARY_INVALID_ENTRY: &str =
    "Dictionary entries need one non-empty, single-line phrase on each side.";
const DICTIONARY_REVISION_REQUIRED: &str =
    "Dictionary state is unavailable. Reopen this window before editing.";
const DICTIONARY_CONFLICT_RELOADED: &str =
    "Dictionary changed outside this window. Your list was reloaded; make your change again.";
const DICTIONARY_CONFLICT_RELOAD_FAILED: &str =
    "Dictionary changed outside this window, but the latest file could not be reloaded. Reopen this window before editing.";

#[derive(Debug)]
pub(crate) struct DictionaryResult {
    request_id: u64,
    ok: bool,
    conflict: bool,
    message: String,
    document: Option<crate::RulesDocument>,
}

#[derive(Debug)]
pub(crate) struct LearnedRuleChange {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) previous: Option<(String, String)>,
}

#[derive(Debug)]
pub(crate) struct CorrectionResult {
    pub(crate) ok: bool,
    pub(crate) review_only: bool,
    pub(crate) message: String,
    pub(crate) document: Option<crate::RulesDocument>,
    pub(crate) changes: Vec<LearnedRuleChange>,
}

#[derive(Debug)]
struct ConfigResult {
    request_id: u64,
    ok: bool,
    /// True only after this request was durably published into a config
    /// generation and the engine made its final decision. A pre-publication
    /// persistence failure must not make the page discard an older generation
    /// that can still complete afterward.
    generation_bound: bool,
    message: String,
    authoritative: Config,
}

#[derive(Debug)]
struct ConfigSaveRequest {
    request_id: u64,
    config: Value,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HistoryEntry {
    pub at: u64,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recognition: Option<String>,
    pub filler_removed: u32,
    /// Writing-rule applications (line breaks, "scratch that", lists, coding
    /// words, style). Like pause-word removal, the original stays reviewable.
    #[serde(skip_serializing_if = "is_zero")]
    pub writing_edits: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl HistoryEntry {
    pub fn new(at: u64, text: String) -> Self {
        Self {
            at,
            text,
            recognition: None,
            filler_removed: 0,
            writing_edits: 0,
        }
    }

    pub fn with_trace(mut self, trace: &vocalcode_core::engine::DictationTrace) -> Self {
        // Never attach a partial recognition and label it as a restorable whole.
        if (trace.filler_removed > 0 || trace.writing_edits > 0) && !trace.raw_text_truncated {
            self.recognition = Some(trace.raw_text.clone());
            self.filler_removed = trace.filler_removed;
            self.writing_edits = trace.writing_edits;
        }
        self
    }
}

#[derive(Default)]
pub struct RuntimeStatus {
    pub(crate) noise_filter: Arc<crate::noise_filter::Control>,
    /// Local meeting recorder/importer state and command bridge. It owns no
    /// network capability; all durable content lives under app-data/meetings.
    pub meetings: crate::meeting::Bridge,
    pub listening: AtomicBool,
    pub dictation_control: crate::dictation_control::Bridge,
    /// False until the ASR model is downloaded + loaded and the engine is live.
    /// Drives the "Setting up…" banner so first-launch isn't a blind wait.
    pub ready: Arc<AtomicBool>,
    /// Speech model availability independent of global-input readiness. Local
    /// meeting import does not require the hotkey hook, but it does require ASR.
    pub model_available: AtomicBool,
    /// Wakes pending model-route application or recovery. The engine thread
    /// notices between utterances, fetches and swaps only when the resolved
    /// route changes, and reuses one loaded Parakeet pipeline when only its
    /// stored language-preference label changes.
    pub reload_model: AtomicBool,
    pub last_text: Mutex<String>,
    pub model_label: Mutex<String>,
    /// The licence state as an English sentence, for logs and as a fallback.
    pub license: Mutex<String>,
    /// The same state as `(kind, days_left)`, where kind is one of `licensed`,
    /// `trial`, `basic`, or `basic_error`.
    ///
    /// The sentence above cannot be translated: it carries a number, so a
    /// dictionary keyed on English source text can never match it, and the
    /// window showed "Trial — 12 days left" in English to a French or Chinese
    /// user — on the one screen that asks them for money. The page composes and
    /// translates the sentence itself from this.
    pub license_state: Mutex<(String, u32)>,
    /// The latest automatic trial provisioning attempt failed. This is a
    /// presentation hint only: the signed receipt remains the sole authority.
    /// It prevents a server outage from looking like an endless connection
    /// attempt while the maintenance thread continues its bounded retries.
    pub trial_setup_error: AtomicBool,
    /// Set by the activation thread: (ok, message) to hand back to the UI once.
    pub activation: Mutex<Option<(bool, String)>>,
    /// Independent single-flight gates for IPC operations whose network owner
    /// is tracked in `service_workers`. Repeated clicks must not allocate an
    /// unbounded number of threads or outbound requests.
    activation_in_progress: AtomicBool,
    restore_in_progress: AtomicBool,
    update_check_in_progress: AtomicBool,
    /// The live replacement rules the engine applies. Shared rather than copied
    /// into the engine, because a dictionary you can only add to on the next
    /// launch is a dictionary that does not work.
    pub rules: Arc<Mutex<Vec<(String, String)>>>,
    pub snippets: Arc<Mutex<Vec<vocalcode_core::migration::Entry>>>,
    pub(crate) migration: Mutex<crate::migration::Session>,
    migration_in_progress: AtomicBool,
    migration_result: Mutex<Option<Value>>,
    pub(crate) workflows: Mutex<crate::workflows::Preferences>,
    workflow_in_progress: AtomicBool,
    workflow_result: Mutex<Option<Value>>,
    /// Latest "Try it" answer from the Writing page: the local rules applied
    /// to typed text. Pure string work; nothing is recorded or delivered.
    writing_preview: Mutex<Option<Value>>,
    calendar_in_progress: AtomicBool,
    calendar_result: Mutex<Option<Value>>,
    pub(crate) calendar_cancel: AtomicBool,
    pub(crate) calendar_snapshot: Mutex<crate::calendar::ReminderSnapshot>,
    /// A word arriving from the macOS Services menu, handed to the page once.
    ///
    /// The Service runs on whatever thread AppKit calls it on and the WebView can
    /// only be touched from the one that owns it, so it parks the text here and
    /// the status pump delivers it — the same shape as `activation`.
    pub teach: Mutex<Option<String>>,
    /// Only one Teach clipboard transaction may be in flight.  Trigger events
    /// arrive on the input thread and the copy itself runs asynchronously; a
    /// repeated key/button press must not create overlapping Ctrl+C workers.
    pub teach_in_progress: AtomicBool,
    /// Joinable selection-copy workers. Finished handles are reaped by the UI
    /// status cadence and admission has a hard cap; shutdown first stops the
    /// engine so no new Teach task can be created, then drains the remainder.
    pub teach_workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// The same, for the updater: (ok, message), handed over once.
    ///
    /// Separate from `activation` because that one lands in the licence note.
    /// Update failures were being written there, so "Update download failed"
    /// appeared as a line about your licence, on a page the user was not on —
    /// while the button that started it still said "Updating…" and had had its
    /// click handler removed. The one message that needed to reach them was the
    /// one delivered furthest from where they were looking.
    pub update_result: Mutex<Option<(bool, String)>>,
    /// Guards the complete download/verify/install transaction.  The update
    /// button can be clicked again before the first worker has reported state,
    /// and two installers must never race over the same application bundle.
    pub update_in_progress: AtomicBool,
    /// The installer worker is joinable so normal Quit cannot race a detached
    /// download/verification task and purge refuses to start while it is live.
    pub update_worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Joinable IPC workers that can outlive one WebView callback. The FIFO
    /// settings worker may write config, activation/purchase may write the
    /// receipt, and restore/update checks may publish state, so shutdown/purge
    /// must wait for all of them rather than leaving detached owners behind.
    /// Finished handles are reaped during normal status ticks, and admission
    /// remains hard-bounded even if a future caller forgets a single-flight gate.
    pub service_workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// Result of a simple settings write, such as language selection.
    pub settings_result: Mutex<Option<(bool, String)>>,
    /// Result of one revision-bound dictionary transaction. Kept separate so
    /// the page can rotate the exact revision on success and reload the latest
    /// document after a compare-and-swap conflict.
    pub dictionary_result: Mutex<Option<DictionaryResult>>,
    /// An automatic, same-input correction transaction completed. Kept apart
    /// from manual dictionary saves so it cannot acknowledge the wrong page
    /// request or consume that request's revision token.
    pub correction_result: Mutex<Option<CorrectionResult>>,
    pub diagnostic_events: Mutex<std::collections::VecDeque<crate::diagnostics::Record>>,
    /// Result of applying one page config snapshot: (request id, ok, detail).
    /// Kept separate from dictionary/language acknowledgements so an unrelated
    /// save cannot be mistaken for the config request currently in flight.
    config_results: Mutex<VecDeque<ConfigResult>>,
    /// A persisted config snapshot still needs the engine thread to apply its
    /// microphone/model/runtime pieces.  Coalescing is intentional: the page
    /// submits desired state, and one acknowledgement covers the newest state.
    pub settings_apply_pending: AtomicBool,
    /// `(request_id, ok, detail)` for History -> Copy.
    pub clipboard_result: Mutex<Option<(String, bool, String)>>,
    /// Operational errors from the audio/input/ASR loop and workers.  Logging
    /// is not enough for failures such as a wireless microphone disappearing:
    /// the person holding the key needs to know why no text arrived and that
    /// the app has returned to idle instead of remaining silently wedged.
    pub runtime_errors: RuntimeErrors,
    /// Short one-line notices for the passive indicator. See `notice.rs`.
    pub notices: crate::notice::Board,
    /// The selected microphone could not be opened or stopped responding.
    /// Published by the engine for the tray and the not-ready notice; the
    /// default (false) means "no failure seen", so startup never flashes an
    /// error before the engine has opened the device.
    pub microphone_failed: AtomicBool,
    /// Generation, pending desired snapshot, and active model-preparation
    /// cancellation token. The FIFO save worker holds this mutex across
    /// persistence and publication; the engine holds it across its final
    /// generation check and runtime commit, closing the old check-then-swap
    /// race.
    pub(crate) config_apply: Mutex<crate::ConfigApplyCoordinator>,
    /// Prevent two checkout buttons/tabs from starting overlapping six-minute
    /// polling loops and issuing the same entitlement twice.
    pub purchase_polling: AtomicBool,
    /// Set by the update-check thread when a newer version exists. The offer
    /// binds the URL to the exact version, digest, and byte length published by
    /// the release pipeline; the notes are shown in the update prompt.
    pub update: Mutex<Option<crate::UpdateOffer>>,
    /// Monotonic generation for overlapping startup/manual update checks. Only
    /// the newest request may publish or clear an offer.
    pub update_generation: AtomicU64,
    /// Serializes generation changes with offer/outcome publication. The atomic
    /// generation alone is insufficient: an older request can pass a load,
    /// pause, then overwrite a newer result unless comparison and writes share
    /// this mutex.
    pub update_state_guard: Mutex<()>,
    /// The outcome of the most recent check: "newer" | "current" | "failed".
    /// Taken once, so the window can answer a "check now" press either way.
    /// A kind rather than a sentence — prose built here cannot be translated.
    pub update_check: Mutex<Option<String>>,
    /// Recent transcriptions this session, newest first: (clock time, text).
    /// In memory only — a dictation log on disk is a privacy liability for an
    /// app whose whole pitch is that nothing leaves the machine.
    /// Unix seconds plus text. JavaScript formats the timestamp in the user's
    /// actual local timezone; the host must not label UTC as local HH:MM.
    pub history: Mutex<Vec<HistoryEntry>>,
    /// Lifetime dictation counts, loaded at startup and written after each
    /// utterance. Counts only — never the text; see `Totals` in main.rs.
    pub totals: Mutex<crate::Totals>,
    /// Text-free per-day counters behind Home -> Insights.
    pub activity: crate::activity::Store,
    /// The model download in flight: (label, percent, done MB, total MB).
    /// None when nothing is downloading, which is also how the UI knows to hide
    /// the bar rather than leaving it stuck at 100%.
    pub model_download: Mutex<Option<(String, f64, f64, f64)>>,
    /// Updater progress is independent from model setup. The status payload
    /// gives this precedence while an explicit update is running, but clearing
    /// it can never erase a concurrent model download.
    pub update_download: Mutex<Option<(String, f64, f64, f64)>>,
    /// macOS: whether Accessibility + Input Monitoring are both granted. Drives
    /// the permission banner. Always true on platforms without TCC, so the
    /// banner never shows there.
    pub permissions_ok: AtomicBool,
    /// Shared Basic dictation gate the engine reads live. It remains open
    /// without a receipt and closes only during shutdown.
    pub inject_gate: Arc<AtomicBool>,
    /// Paid/trial feature gate. Meetings and automatic correction learning use
    /// this without making ordinary local dictation depend on a licence.
    pub pro_gate: AtomicBool,
    /// True when the talk trigger latches (press to start, press again to stop).
    /// Shared with the engine and read per press, so the setting applies to the
    /// next utterance rather than needing a restart.
    pub talk_latched: Arc<AtomicBool>,
    /// True when a short sound marks the start and end of each recording.
    /// Shared with the press path for the same reason as `talk_latched`: a
    /// preference about feedback is worthless if it needs a restart.
    pub cue_sounds: Arc<AtomicBool>,
    /// True once the user has picked a language in the first-run picker. Drives
    /// the picker overlay, and gates the background thread's first model
    /// download so nothing is fetched until a language is chosen.
    pub onboarded: AtomicBool,
    /// Cooperative stop for maintenance/permission/download loops.
    pub shutdown: AtomicBool,
    /// What main should do only after engine/model/clipboard workers are gone.
    pub shutdown_action: Mutex<ShutdownAction>,
}

impl RuntimeStatus {
    pub(crate) fn with_meetings(meetings: crate::meeting::Bridge) -> Self {
        Self {
            meetings,
            ..Self::default()
        }
    }
}

/// Errors waiting for the settings page, oldest first.
///
/// This was a single slot, drained once per UI tick. Two failures inside one
/// tick — a microphone vanishing and the recording that then could not finish
/// — kept only the second, and the first was usually the cause. A short queue
/// keeps both; the page shows them in turn.
#[derive(Default)]
pub struct RuntimeErrors(Mutex<VecDeque<String>>);

impl RuntimeErrors {
    /// Room for a burst. A loop repeating one failure never fills it, because
    /// a message already waiting is not queued twice.
    pub const CAPACITY: usize = 8;

    pub fn push(&self, message: String) {
        let mut queue = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if queue.contains(&message) {
            return;
        }
        if queue.len() == Self::CAPACITY {
            // Keep the newest: they describe the state the user is in now.
            queue.pop_front();
        }
        queue.push_back(message);
    }

    pub fn drain(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
            .collect()
    }
}

/// Recomputes the licence state after an activation: the English sentence for
/// logs, plus `(kind, days_left)` for the window to translate.
type LicenseRefresh = Arc<dyn Fn() -> (String, (String, u32)) + Send + Sync>;

/// Hide the window, but only when there is a tray icon to get it back from.
///
/// With no Dock icon and no status item, a hidden window is an app the user
/// cannot reach at all — the process keeps running and clicking it in Finder
/// does nothing, because it is already launched. Minimising instead keeps it in
/// Mission Control and the window list, which is recoverable.
fn hide_to_tray(window: &tao::window::Window, has_tray: bool) {
    if has_tray {
        window.set_visible(false);
    } else {
        log::warn!(
            "no tray icon — minimising instead of hiding, or the window could not be reopened"
        );
        window.set_minimized(true);
    }
}

const TEACH_CAPSULE_WIDTH: f64 = 640.0;
const TEACH_CAPSULE_HEIGHT: f64 = 90.0;
const TEACH_CAPSULE_BOTTOM_MARGIN: f64 = 88.0;
const CORRECTION_REVIEW_REQUEST_ID_BASE: u64 = 8_000_000_000_000_000;

fn is_correction_review_request_id(request_id: u64) -> bool {
    request_id >= CORRECTION_REVIEW_REQUEST_ID_BASE
}

#[derive(Debug)]
struct TeachWindowRestore {
    inner_size: tao::dpi::PhysicalSize<u32>,
    outer_position: Option<tao::dpi::PhysicalPosition<i32>>,
    was_visible: bool,
    was_maximized: bool,
    #[cfg(target_os = "windows")]
    had_undecorated_shadow: bool,
}

fn show_teach_capsule(window: &tao::window::Window, focus: bool) -> TeachWindowRestore {
    #[cfg(target_os = "windows")]
    use tao::platform::windows::WindowExtWindows as _;

    let restore = TeachWindowRestore {
        inner_size: window.inner_size(),
        outer_position: window.outer_position().ok(),
        was_visible: window.is_visible(),
        was_maximized: window.is_maximized(),
        #[cfg(target_os = "windows")]
        had_undecorated_shadow: window.has_undecorated_shadow(),
    };
    window.set_maximized(false);
    // Windows adds a small rectangular DWM shadow around undecorated windows.
    // It is useful on the full settings surface but reads as a black box around
    // the rounded Teach pill, so suppress it only for capsule mode.
    #[cfg(target_os = "windows")]
    window.set_undecorated_shadow(false);
    window.set_min_inner_size(None::<tao::dpi::LogicalSize<f64>>);
    window.set_resizable(false);
    window.set_inner_size(tao::dpi::LogicalSize::new(
        TEACH_CAPSULE_WIDTH,
        TEACH_CAPSULE_HEIGHT,
    ));
    if let Some(monitor) = window
        .current_monitor()
        .or_else(|| window.primary_monitor())
    {
        let scale = monitor.scale_factor();
        let position = monitor.position().to_logical::<f64>(scale);
        let size = monitor.size().to_logical::<f64>(scale);
        window.set_outer_position(tao::dpi::LogicalPosition::new(
            position.x + (size.width - TEACH_CAPSULE_WIDTH) / 2.0,
            position.y + size.height - TEACH_CAPSULE_HEIGHT - TEACH_CAPSULE_BOTTOM_MARGIN,
        ));
    }
    window.set_always_on_top(true);
    window.set_visible(true);
    window.set_minimized(false);
    if focus {
        window.set_focus();
    } else {
        window.request_user_attention(Some(tao::window::UserAttentionType::Informational));
    }
    restore
}

fn focus_learned_correction_window(window: &tao::window::Window) {
    window.set_visible(true);
    window.set_minimized(false);

    // `Window::set_focus` is the portable request, but WebView2 windows shown
    // asynchronously after correction learning can otherwise remain behind the
    // editor on Windows. Make the foreground request explicit; Windows may still
    // reject it under its focus-stealing policy, in which case the attention
    // request remains a visible fallback.
    #[cfg(target_os = "windows")]
    {
        use tao::platform::windows::WindowExtWindows as _;
        use windows_sys::Win32::Foundation::HWND;
        use windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow;

        let hwnd = window.hwnd() as HWND;
        if unsafe { SetForegroundWindow(hwnd) } == 0 {
            window.request_user_attention(Some(tao::window::UserAttentionType::Informational));
        }
    }
    window.set_focus();
}

fn position_correction_review(window: &tao::window::Window) {
    if let Some(monitor) = window
        .current_monitor()
        .or_else(|| window.primary_monitor())
    {
        let scale = monitor.scale_factor();
        let position = monitor.position().to_logical::<f64>(scale);
        let size = monitor.size().to_logical::<f64>(scale);
        window.set_outer_position(tao::dpi::LogicalPosition::new(
            position.x + (size.width - TEACH_CAPSULE_WIDTH) / 2.0,
            position.y + size.height - TEACH_CAPSULE_HEIGHT - TEACH_CAPSULE_BOTTOM_MARGIN,
        ));
    }
}

#[cfg(windows)]
fn round_correction_review_window(window: &tao::window::Window) {
    use tao::platform::windows::WindowExtWindows as _;
    use windows_sys::Win32::Graphics::Gdi::{CreateRoundRectRgn, DeleteObject, SetWindowRgn};

    let size = window.inner_size();
    let diameter = (44.0 * window.scale_factor()).round() as i32;
    let region = unsafe {
        CreateRoundRectRgn(
            0,
            0,
            size.width.saturating_add(1) as i32,
            size.height.saturating_add(1) as i32,
            diameter,
            diameter,
        )
    };
    if region.is_null() {
        log::warn!("could not create the rounded correction-review window region");
        return;
    }
    // On success Windows owns the region. On failure the caller still owns it
    // and must release it, otherwise every learned correction leaks a GDI
    // object for the rest of the process lifetime.
    if unsafe { SetWindowRgn(window.hwnd() as _, region, 1) } == 0 {
        unsafe {
            DeleteObject(region as _);
        }
        log::warn!("could not apply the rounded correction-review window region");
    }
}

#[cfg(not(windows))]
fn round_correction_review_window(_window: &tao::window::Window) {}

fn present_learned_correction(window: &tao::window::Window, webview: &wry::WebView, payload: &str) {
    position_correction_review(window);
    round_correction_review_window(window);
    if let Err(error) =
        webview.evaluate_script(&format!("window.vocalcodeCorrectionReview({payload})"))
    {
        log::warn!("learned correction could not reach its review window: {error}");
        return;
    }
    focus_learned_correction_window(window);
}

fn hide_correction_review(window: &tao::window::Window, webview: &wry::WebView) {
    window.set_visible(false);
    let _ = webview.evaluate_script("window.vocalcodeCorrectionReviewClosed()");
}

fn restore_from_teach_capsule(
    window: &tao::window::Window,
    restore: TeachWindowRestore,
    has_tray: bool,
) {
    #[cfg(target_os = "windows")]
    use tao::platform::windows::WindowExtWindows as _;

    window.set_always_on_top(false);
    window.set_resizable(true);
    #[cfg(target_os = "windows")]
    window.set_undecorated_shadow(restore.had_undecorated_shadow);
    if restore.was_maximized {
        window.set_maximized(true);
    } else {
        window.set_inner_size(restore.inner_size);
        if let Some(position) = restore.outer_position {
            window.set_outer_position(position);
        }
    }
    window.set_min_inner_size(Some(tao::dpi::LogicalSize::new(720.0, 480.0)));
    if restore.was_visible {
        window.set_visible(true);
        window.set_minimized(false);
        window.set_focus();
    } else {
        hide_to_tray(window, has_tray);
    }
}

/// Show the window on the Space the user is looking at.
///
/// AppKit otherwise keeps the one long-lived window on whichever Space it was
/// last shown on. Because it is never destroyed, that binding outlives the
/// session: opening VocalCode from the Dock, Spotlight or Finder while some
/// full-screen app owns another Space drags the whole screen over to that
/// Space instead of showing the window where the user actually is.
#[cfg(target_os = "macos")]
fn follow_active_space(window: &tao::window::Window) {
    use objc2::runtime::AnyObject;
    use tao::platform::macos::WindowExtMacOS;

    // NSWindowCollectionBehaviorMoveToActiveSpace
    const MOVE_TO_ACTIVE_SPACE: u64 = 1 << 1;

    let ns_window = window.ns_window() as *mut AnyObject;
    if ns_window.is_null() {
        log::warn!("no NSWindow for the settings window; it may open on another Space");
        return;
    }
    unsafe {
        let _: () = objc2::msg_send![ns_window, setCollectionBehavior: MOVE_TO_ACTIVE_SPACE];
    }
}

/// Bring the one long-lived window back, whatever it was doing.
///
/// Every "show the window" request routes through here so they cannot drift
/// apart: leaving out `set_minimized(false)` makes the request a no-op on a
/// minimised window, because `set_focus` refuses to activate one and macOS then
/// never raises an app that owns no Dock icon.
fn surface_window(
    window: &tao::window::Window,
    webview: &wry::WebView,
    teach_window_restore: &mut Option<TeachWindowRestore>,
    has_tray: bool,
) {
    if let Some(restore) = teach_window_restore.take() {
        restore_from_teach_capsule(window, restore, has_tray);
        let _ = webview.evaluate_script("window.vocalcodeTeachClosed()");
    }
    window.set_visible(true);
    window.set_minimized(false);
    window.set_focus();
}

/// Window actions sent from the page's custom (frameless) titlebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IpcSurface {
    Settings,
    CorrectionReview,
}

#[derive(Debug, Clone, Copy)]
enum UserEvent {
    Drag,
    Minimize,
    HideToTray,
    TeachPopupOpen,
    TeachPopupClose,
    /// macOS double-click-to-zoom, which the webview would otherwise swallow.
    Zoom,
    /// A key-capture answer is waiting. The event carries nothing — its job is
    /// to *wake* the loop, whose end-of-iteration `push_status` does the
    /// delivery. Without it, an answer waited for the 180 ms `WaitUntil` tick,
    /// which is not guaranteed to fire while the window sits idle: the owner
    /// bound a key and saw nothing until he happened to click somewhere.
    CaptureReady,
    /// Wake and terminate the UI loop after recording the requested main-thread
    /// shutdown action. Cleanup itself happens after the loop returns.
    Shutdown,
    /// The page has parsed and defined its entry points, so the config it needs
    /// to render itself can be delivered. The host cannot infer this moment:
    /// `with_html` hands the document to the webview to load asynchronously,
    /// and the first loop iteration — the only other place this could fire —
    /// runs long before any of it has been parsed.
    SettingsPageReady,
    CorrectionReviewReady,
    CorrectionReviewClose,
    MeetingPrompt(crate::meeting_prompt::Event),
    MeetingPresenceReady,
    #[cfg(windows)]
    DesktopControl(crate::control_bar::Event),
}

fn request_shutdown(
    status: &RuntimeStatus,
    proxy: &EventLoopProxy<UserEvent>,
    action: ShutdownAction,
) {
    status.ready.store(false, Ordering::Release);
    status.shutdown.store(true, Ordering::Release);
    status
        .config_apply
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .cancel_prepare();
    if let Ok(mut requested) = status.shutdown_action.lock() {
        *requested = merge_shutdown_action(*requested, action);
    }
    let _ = proxy.send_event(UserEvent::Shutdown);
}

fn merge_shutdown_action(current: ShutdownAction, next: ShutdownAction) -> ShutdownAction {
    // Purge is the strongest user intent. A helper that has already been
    // launched must still get its restart shutdown, while an ordinary Quit may
    // never weaken either destructive/transactional action.
    use ShutdownAction::{None, PurgeData, Quit, RestartForUpdate};
    match (current, next) {
        (PurgeData, _) | (_, PurgeData) => PurgeData,
        (RestartForUpdate, _) | (_, RestartForUpdate) => RestartForUpdate,
        (Quit, _) | (_, Quit) => Quit,
        _ => None,
    }
}

fn take_finished_workers(
    workers: &mut Vec<std::thread::JoinHandle<()>>,
) -> Vec<std::thread::JoinHandle<()>> {
    let mut finished = Vec::new();
    let mut index = 0;
    while index < workers.len() {
        if workers[index].is_finished() {
            finished.push(workers.swap_remove(index));
        } else {
            index += 1;
        }
    }
    finished
}

fn join_finished_workers(workers: Vec<std::thread::JoinHandle<()>>, label: &str) {
    for worker in workers {
        if worker.join().is_err() {
            log::error!("{label} worker panicked");
        }
    }
}

fn reap_tracked_workers(workers: &Mutex<Vec<std::thread::JoinHandle<()>>>, label: &str) -> usize {
    let finished = {
        let mut workers = workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        take_finished_workers(&mut workers)
    };
    let count = finished.len();
    join_finished_workers(finished, label);
    count
}

fn spawn_tracked_worker(
    shutdown: &AtomicBool,
    workers: &Mutex<Vec<std::thread::JoinHandle<()>>>,
    limit: usize,
    name: &str,
    label: &str,
    work: impl FnOnce() + Send + 'static,
) -> Result<(), String> {
    // The list lock linearizes reap, capacity, spawn registration, and the
    // shutdown drain. Removed handles are already `is_finished`, so joining
    // them after releasing the lock cannot leave a live owner unregistered.
    let (finished, result) = {
        let mut workers = workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let finished = take_finished_workers(&mut workers);
        let result = if shutdown.load(Ordering::Acquire) {
            Err("VocalCode is shutting down".to_string())
        } else if workers.len() >= limit {
            Err(format!("{label} worker limit ({limit}) is busy"))
        } else {
            std::thread::Builder::new()
                .name(name.to_string())
                .spawn(work)
                .map(|handle| workers.push(handle))
                .map_err(|error| error.to_string())
        };
        (finished, result)
    };
    join_finished_workers(finished, label);
    result
}

fn spawn_service_worker(
    status: &Arc<RuntimeStatus>,
    name: &str,
    work: impl FnOnce() + Send + 'static,
) -> Result<(), String> {
    spawn_tracked_worker(
        &status.shutdown,
        &status.service_workers,
        SERVICE_WORKER_LIMIT,
        name,
        "IPC service",
        work,
    )
}

pub(crate) fn spawn_teach_worker(
    status: &Arc<RuntimeStatus>,
    work: impl FnOnce() + Send + 'static,
) -> Result<(), String> {
    spawn_tracked_worker(
        &status.shutdown,
        &status.teach_workers,
        TEACH_WORKER_LIMIT,
        "vocalcode-teach",
        "Teach",
        work,
    )
}

fn reap_finished_workers(status: &RuntimeStatus) {
    reap_tracked_workers(&status.service_workers, "IPC service");
    reap_tracked_workers(&status.teach_workers, "Teach");
    reap_finished_update_worker(status);
}

fn reap_finished_update_worker(status: &RuntimeStatus) -> bool {
    let finished = {
        let mut worker = status
            .update_worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if worker.as_ref().is_some_and(|handle| handle.is_finished()) {
            worker.take()
        } else {
            None
        }
    };
    let reaped = finished.is_some();
    if let Some(worker) = finished {
        if worker.join().is_err() {
            log::error!("update worker panicked");
        }
    }
    reaped
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateInstallStart {
    Started,
    Busy,
}

fn spawn_update_worker(
    status: &Arc<RuntimeStatus>,
    work: impl FnOnce() + Send + 'static,
) -> Result<UpdateInstallStart, String> {
    if status
        .update_in_progress
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(UpdateInstallStart::Busy);
    }

    struct UpdateReset(Arc<RuntimeStatus>);
    impl Drop for UpdateReset {
        fn drop(&mut self) {
            self.0.update_in_progress.store(false, Ordering::Release);
        }
    }

    let reset = UpdateReset(status.clone());
    let (finished, result) = {
        let mut worker = status
            .update_worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let finished = if worker.as_ref().is_some_and(|handle| handle.is_finished()) {
            worker.take()
        } else {
            None
        };
        let result = if worker.is_some() {
            Ok(UpdateInstallStart::Busy)
        } else if status.shutdown.load(Ordering::Acquire) {
            Err("VocalCode is shutting down".to_string())
        } else {
            std::thread::Builder::new()
                .name("vocalcode-update".to_string())
                .spawn(move || {
                    let _reset = reset;
                    work();
                })
                .map(|handle| {
                    *worker = Some(handle);
                    UpdateInstallStart::Started
                })
                .map_err(|error| error.to_string())
        };
        (finished, result)
    };
    if let Some(worker) = finished {
        if worker.join().is_err() {
            log::error!("update worker panicked");
        }
    }
    result
}

#[derive(Clone, Copy)]
enum IpcSingleFlight {
    Activation,
    Restore,
    UpdateCheck,
    Migration,
    Workflow,
    Calendar,
}

impl IpcSingleFlight {
    fn flag(self, status: &RuntimeStatus) -> &AtomicBool {
        match self {
            Self::Activation => &status.activation_in_progress,
            Self::Restore => &status.restore_in_progress,
            Self::UpdateCheck => &status.update_check_in_progress,
            Self::Migration => &status.migration_in_progress,
            Self::Workflow => &status.workflow_in_progress,
            Self::Calendar => &status.calendar_in_progress,
        }
    }
}

struct IpcSingleFlightReset {
    status: Arc<RuntimeStatus>,
    operation: IpcSingleFlight,
}

impl IpcSingleFlightReset {
    fn claim(status: &Arc<RuntimeStatus>, operation: IpcSingleFlight) -> Option<Self> {
        operation
            .flag(status)
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(Self {
            status: status.clone(),
            operation,
        })
    }
}

impl Drop for IpcSingleFlightReset {
    fn drop(&mut self) {
        self.operation
            .flag(&self.status)
            .store(false, Ordering::Release);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdateCheckStart {
    Started,
    Busy,
}

pub(crate) fn spawn_update_check(
    status: &Arc<RuntimeStatus>,
    name: &str,
    work: impl FnOnce() + Send + 'static,
) -> Result<UpdateCheckStart, String> {
    let Some(single_flight) = IpcSingleFlightReset::claim(status, IpcSingleFlight::UpdateCheck)
    else {
        return Ok(UpdateCheckStart::Busy);
    };
    spawn_service_worker(status, name, move || {
        let _single_flight = single_flight;
        work();
    })?;
    Ok(UpdateCheckStart::Started)
}

fn publish_activation_if_empty(status: &RuntimeStatus, message: &str) {
    let mut result = status
        .activation
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if result.is_none() {
        *result = Some((false, message.to_string()));
    }
}

fn publish_config_save_failure(
    status: &RuntimeStatus,
    request_id: u64,
    message: impl Into<String>,
    authoritative: &Config,
) {
    let message = message.into();
    if request_id == 0 {
        *status
            .settings_result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((false, message));
    } else {
        queue_config_result(status, request_id, false, false, message, authoritative);
    }
}

/// Preserve page arrival order while keeping every filesystem, launch-agent,
/// and registry operation off the WebView event thread. A thread per save can
/// acquire the coordinator out of order; one FIFO receiver cannot.
fn start_config_save_worker(
    status: &Arc<RuntimeStatus>,
    config: &Arc<Mutex<Config>>,
    base: &Path,
) -> Result<std::sync::mpsc::SyncSender<ConfigSaveRequest>, String> {
    let (sender, receiver) =
        std::sync::mpsc::sync_channel::<ConfigSaveRequest>(CONFIG_SAVE_QUEUE_CAPACITY);
    let worker_status = status.clone();
    let worker_config = config.clone();
    let worker_base = base.to_path_buf();
    spawn_service_worker(status, "vocalcode-config-save", move || {
        while let Ok(request) = receiver.recv() {
            let mut apply = worker_status
                .config_apply
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match apply_save(&worker_config, &worker_base, &request.config) {
                Ok(saved) => {
                    worker_status
                        .onboarded
                        .store(saved.onboarded, Ordering::Release);
                    apply.publish(request.request_id, saved);
                    drop(apply);
                    worker_status
                        .settings_apply_pending
                        .store(true, Ordering::Release);
                    worker_status.reload_model.store(true, Ordering::Release);
                }
                Err(error) => {
                    log::error!("save config: {error}");
                    let authoritative = worker_config
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    // Queue the non-generation failure before releasing the
                    // coordinator. Otherwise the older generation that this
                    // failed save did not supersede can commit in this gap and
                    // enqueue its newer runtime truth first; the stale save
                    // failure would then arrive last and overwrite the page.
                    publish_config_save_failure(
                        &worker_status,
                        request.request_id,
                        format!("Could not save settings: {error}"),
                        &authoritative,
                    );
                    drop(apply);
                }
            }
        }
    })?;
    Ok(sender)
}

pub(crate) fn join_service_workers(status: &RuntimeStatus) {
    let workers = status
        .service_workers
        .lock()
        .map(|mut workers| std::mem::take(&mut *workers))
        .unwrap_or_else(|poisoned| std::mem::take(&mut *poisoned.into_inner()));
    for worker in workers {
        if worker.join().is_err() {
            log::error!("IPC service worker panicked during shutdown");
        }
    }
}

/// Owns a polling worker that must not outlive the UI scope. The `Drop` path is
/// essential: `run_return` can unwind, and future fallible setup must not turn
/// a harmless UI startup error into a permanently detached thread.
struct StopJoinGuard {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    label: &'static str,
}

impl StopJoinGuard {
    fn new(
        stop: Arc<AtomicBool>,
        worker: std::thread::JoinHandle<()>,
        label: &'static str,
    ) -> Self {
        Self {
            stop,
            worker: Some(worker),
            label,
        }
    }

    fn stop_and_join(&mut self) -> bool {
        self.stop.store(true, Ordering::Release);
        self.worker
            .take()
            .is_none_or(|worker| worker.join().is_ok())
    }
}

impl Drop for StopJoinGuard {
    fn drop(&mut self) {
        if !self.stop_and_join() {
            log::error!("{} panicked during scope cleanup", self.label);
        }
    }
}

/// Run the window + tray. Blocks (the tao loop drives the process) until Quit.
// Grouped-argument refactors were considered and rejected: these are wiring
// functions called once each, the parameters are all distinct types, and
// bundling them into a struct would add a layer to trace through without making
// any call site clearer.
#[allow(clippy::too_many_arguments)]
pub fn run(
    config: Arc<Mutex<Config>>,
    status: Arc<RuntimeStatus>,
    base: PathBuf,
    refresh_license: LicenseRefresh,
    capture: Arc<CaptureShared>,
    overlay_state: crate::overlay::OverlayState,
    audio_level: vocalcode_platform::AudioLevel,
    show_existing: std::sync::mpsc::Receiver<()>,
) -> anyhow::Result<()> {
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let builder = WindowBuilder::new()
        .with_title("VocalCode")
        .with_window_icon(Some(make_window_icon()))
        // Settings paints an opaque page in normal mode, but the same native
        // window is temporarily reused for the rounded Teach capsule.  Making
        // the host transparent lets those four capsule corners show the
        // desktop instead of a rectangular black window background.
        .with_transparent(true)
        .with_inner_size(tao::dpi::LogicalSize::new(900.0, 600.0))
        .with_min_inner_size(tao::dpi::LogicalSize::new(720.0, 480.0));

    // `with_window_icon` sets ICON_SMALL on Windows. Windows 11's taskbar uses
    // ICON_BIG at common display scales; without this explicit second icon it
    // falls back to the generic application-window glyph even though the title
    // bar, tray and executable resources all carry the VocalCode mark.
    #[cfg(windows)]
    let builder = {
        use tao::platform::windows::WindowBuilderExtWindows;
        builder.with_taskbar_icon(Some(make_window_icon()))
    };

    // Windows/Linux: frameless, with the titlebar drawn in the page.
    #[cfg(not(target_os = "macos"))]
    let builder = builder.with_decorations(false);

    // macOS keeps its real window frame, because the frame *is* the traffic
    // lights — dropping decorations there leaves a Mac user with no close
    // button, and the page's own controls end up on the wrong side of the
    // window. Instead the titlebar is made transparent and the content is
    // allowed to run underneath it, so the page still paints the full surface
    // while dragging, double-click-to-zoom and the window menu stay native.
    #[cfg(target_os = "macos")]
    let builder = {
        use tao::platform::macos::WindowBuilderExtMacOS;
        builder
            .with_titlebar_transparent(true)
            .with_fullsize_content_view(true)
            .with_title_hidden(true)
            // Centre the traffic lights in the page's 64px header rather than
            // letting them sit at the default inset, which would leave them
            // floating above the header's own vertical rhythm.
            .with_traffic_light_inset(tao::dpi::LogicalPosition::new(19.0, 24.0))
    };

    let window = builder.build(&event_loop)?;
    #[cfg(target_os = "macos")]
    follow_active_space(&window);

    // Automatic correction review is its own window. Reusing Settings made a
    // learned rule either shrink the whole application into a capsule or cover
    // its content with an embedded bar. The review must be able to come forward
    // while leaving both Settings and the editor behind it completely intact.
    let correction_builder = WindowBuilder::new()
        .with_title("VocalCode Correction")
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top(true)
        .with_resizable(false)
        .with_focused(false)
        .with_visible(false)
        .with_inner_size(tao::dpi::LogicalSize::new(
            TEACH_CAPSULE_WIDTH,
            TEACH_CAPSULE_HEIGHT,
        ));
    #[cfg(windows)]
    let correction_builder = {
        use tao::platform::windows::WindowBuilderExtWindows;
        correction_builder
            .with_skip_taskbar(true)
            .with_undecorated_shadow(false)
    };
    let correction_window = correction_builder.build(&event_loop)?;
    position_correction_review(&correction_window);
    round_correction_review_window(&correction_window);
    #[cfg(target_os = "macos")]
    follow_active_space(&correction_window);
    // WebView2 otherwise defaults beside the executable. That is unwritable in
    // Program Files, leaves legacy data after uninstall, and lets two surfaces
    // with different environment options collide on one profile. Keep each
    // profile in its own verified app-data subdirectory.
    let settings_webview_dir =
        crate::paths::ensure_trusted_data_subdir(&base, std::path::Path::new("webview2/settings"))
            .map_err(|error| {
                anyhow::anyhow!("could not prepare the Settings browser profile: {error}")
            })?;
    let overlay_webview_dir =
        crate::paths::ensure_trusted_data_subdir(&base, std::path::Path::new("webview2/overlay"));

    let cfg_state = config;
    let devices = vocalcode_platform::list_input_device_choices();
    let dictionary = crate::load_rules_document(&base);
    if let Err(error) = &dictionary {
        log::error!("load dictionary for Settings: {error}");
    } else if let Ok(document) = &dictionary {
        *status.rules.lock().unwrap() = document.rules.clone();
    }
    let init_json = init_config_json(
        &cfg_state.lock().unwrap(),
        &devices,
        dictionary.as_ref().ok(),
        dictionary.is_err().then_some(DICTIONARY_REVISION_REQUIRED),
    );

    let config_saves = start_config_save_worker(&status, &cfg_state, &base)
        .map_err(|error| anyhow::anyhow!("could not start the settings save worker: {error}"))?;

    // IPC handler runs on the UI thread. Saves enter one FIFO worker; activate
    // spawns a network thread; buy opens the checkout.
    let ipc_status = status.clone();
    let ipc_cfg = cfg_state.clone();
    let tick_cfg = cfg_state.clone();
    let ipc_base = base.clone();
    let ipc_refresh = refresh_license.clone();
    let ipc_proxy = proxy.clone();
    let ipc_capture = capture.clone();
    let ipc_config_saves = config_saves.clone();
    let correction_ipc_status = status.clone();
    let correction_ipc_cfg = cfg_state.clone();
    let correction_ipc_base = base.clone();
    let correction_ipc_refresh = refresh_license.clone();
    let correction_ipc_proxy = proxy.clone();
    let correction_ipc_capture = capture.clone();
    let correction_ipc_config_saves = config_saves;
    let handler = move |req: wry::http::Request<String>| {
        handle_ipc(
            req.into_body(),
            &ipc_status,
            &ipc_cfg,
            &ipc_base,
            &ipc_refresh,
            &ipc_proxy,
            &ipc_capture,
            &ipc_config_saves,
            IpcSurface::Settings,
        );
    };
    let correction_handler = move |req: wry::http::Request<String>| {
        handle_ipc(
            req.into_body(),
            &correction_ipc_status,
            &correction_ipc_cfg,
            &correction_ipc_base,
            &correction_ipc_refresh,
            &correction_ipc_proxy,
            &correction_ipc_capture,
            &correction_ipc_config_saves,
            IpcSurface::CorrectionReview,
        );
    };

    // This surface owns privileged IPC (activation, settings, update install and
    // data removal). Never let a remote document inherit that bridge. WebKit
    // reports the HTML loaded by loadHTMLString as `about:blank`; WebView2 reports
    // NavigateToString as the exact base64 data URL below. Allow only those exact
    // initial documents, then deny all other navigations and popup requests.
    // External product/checkout links are opened by the native `open_url` path.
    let settings_document_url = settings_document_data_url();
    let mut settings_web_context = wry::WebContext::new(Some(settings_webview_dir));
    let webview = WebViewBuilder::new_with_web_context(&mut settings_web_context)
        // Window, WebView and HTML are three separate background layers.  All
        // three need alpha support for capsule mode; the ordinary settings
        // page remains opaque because its body paints the full surface.
        .with_transparent(true)
        .with_html(include_str!("webui.html"))
        .with_navigation_handler(move |url| local_webview_navigation(&url, &settings_document_url))
        .with_new_window_req_handler(|url, _| {
            log::warn!("blocked settings WebView popup request: {url}");
            wry::NewWindowResponse::Deny
        })
        .with_ipc_handler(handler)
        .build(&window)?;

    // The review document has only three capabilities: acknowledge/undo an
    // already-learned dictionary rule, close itself, and announce readiness.
    // It has a separate exact-document navigation allowlist even though it
    // shares WebView2's trusted local profile with Settings.
    let correction_document_url = correction_review_document_data_url();
    let correction_webview = WebViewBuilder::new_with_web_context(&mut settings_web_context)
        .with_html(include_str!("correction_review.html"))
        .with_navigation_handler(move |url| {
            local_correction_review_navigation(&url, &correction_document_url)
        })
        .with_new_window_req_handler(|url, _| {
            log::warn!("blocked correction-review WebView popup request: {url}");
            wry::NewWindowResponse::Deny
        })
        .with_ipc_handler(correction_handler);
    #[cfg(not(windows))]
    let correction_webview = correction_webview.with_transparent(true);
    #[cfg(windows)]
    let correction_webview = correction_webview.with_background_color((9, 10, 12, 255));
    let correction_webview = correction_webview.build(&correction_window)?;

    let reminder_proxy = proxy.clone();
    // Do not pay for another WebView2 profile/process when reminders are off.
    // Create the isolated surface only when the first eligible card is needed.
    let mut meeting_prompt: Option<crate::meeting_prompt::MeetingPrompt> = None;
    let mut meeting_prompt_attempted = false;

    // Tray icon: Show / Quit.
    let tray_menu = Menu::new();
    let show_item = MenuItem::new("Show VocalCode", true, None);
    let quit_item = MenuItem::new("Quit VocalCode", true, None);
    tray_menu.append(&show_item)?;
    tray_menu.append(&quit_item)?;
    let show_id = show_item.id().clone();
    let quit_id = quit_item.id().clone();
    let menu_rx = MenuEvent::receiver();

    // Built on Init rather than here, because tray-icon requires an event loop
    // to already be *running* on this thread. Constructing it before
    // event_loop.run_return() works on Windows but silently produces no status item on
    // macOS — and with no Dock icon (LSUIElement) and the close button hiding
    // the window, that leaves a running app with no way to reach it at all.
    let mut tray_menu = Some(tray_menu);
    let mut tray: Option<tray_icon::TrayIcon> = None;
    // Built alongside the tray, and for the same reason: it needs a running
    // event loop. A failure here is not fatal — the app still dictates, it just
    // does so without on-screen feedback.
    let mut overlay: Option<crate::overlay::Overlay> = None;
    #[cfg(windows)]
    let mut desktop_control: Option<crate::control_bar::ControlBar> = None;
    #[cfg(windows)]
    let mut desktop_control_attempted = false;
    let demo_overlay = std::env::var("VOCALCODE_OVERLAY_DEMO").as_deref() == Ok("1");
    // Notices and the tray tooltip follow the interface language. The last
    // value read is kept, so a contended config lock never blanks the copy.
    let mut ui_lang = String::new();
    let mut notice_lang = crate::notice::LangCache::default();
    let mut not_ready_presses =
        crate::notice::PressWatch::new(vocalcode_platform::hotkey::talk_presses_while_not_ready());
    let mut last_tooltip = String::new();
    let started = Instant::now();
    // Last status handed to the page; see `push_status`.
    let mut last_status = String::new();
    let mut last_meeting_revision = 0;
    let mut teach_window_restore: Option<TeachWindowRestore> = None;
    let settings_window_id = window.id();
    let correction_window_id = correction_window.id();
    let mut correction_review_ready = false;
    let mut pending_correction_review: Option<String> = None;
    let mut meeting_reminder_gate = crate::meeting_reminder::ReminderGate::default();
    let presence_proxy = proxy.clone();
    let meeting_probe = crate::meeting_reminder::Probe::new(move || {
        let _ = presence_proxy.send_event(UserEvent::MeetingPresenceReady);
    })
    .ok();
    let mut next_meeting_reminder_check = Instant::now();
    let mut next_meeting_probe = Instant::now();
    let mut next_calendar_refresh = Instant::now();

    // This is intentionally after the final fallible UI setup. Watch for
    // finished key-captures and wake the loop for each one, so the page hears
    // an answer within ~50 ms. The guard also stops and joins on unwind.
    let capture_waker_stop = Arc::new(AtomicBool::new(false));
    let capture_waker_thread = {
        let capture = capture.clone();
        let waker = proxy.clone();
        let waker_status = status.clone();
        let stop = capture_waker_stop.clone();
        std::thread::Builder::new()
            .name("vocalcode-capture-waker".to_string())
            .spawn(move || {
                let mut was_waiting = false;
                while !stop.load(Ordering::Acquire)
                    && !waker_status.shutdown.load(Ordering::Acquire)
                {
                    let waiting = capture.has_result();
                    if waiting && !was_waiting {
                        // One wake per queued batch, not one per 50 ms poll: the
                        // handler drains the whole queue, so a second event would
                        // find nothing — and a stalled UI thread should accumulate
                        // one wake, not twenty a second.
                        log::info!("capture: result queued, waking the UI loop");
                        if waker.send_event(UserEvent::CaptureReady).is_err() {
                            // The event loop is gone; the process is shutting down.
                            return;
                        }
                    }
                    was_waiting = waiting;
                    std::thread::sleep(Duration::from_millis(50));
                }
            })
            .map_err(|error| anyhow::anyhow!("could not start capture waker: {error}"))?
    };
    let mut capture_waker = StopJoinGuard::new(
        capture_waker_stop.clone(),
        capture_waker_thread,
        "capture waker",
    );
    let shutdown_status = status.clone();
    let exit_code = event_loop.run_return(move |event, event_target, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(180));

        // A later launch does not create a second hook/audio engine.  It asks
        // this process to surface the one window the user already owns.
        while show_existing.try_recv().is_ok() {
            surface_window(&window, &webview, &mut teach_window_restore, tray.is_some());
        }

        match event {
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } if meeting_prompt
                .as_ref()
                .is_some_and(|p| p.window_id() == window_id) =>
            {
                if let Some(prompt) = &mut meeting_prompt {
                    if let Some(id) = prompt.auto_end_id() {
                        status.meetings.auto_end_action(id, vocalcode_meeting::auto_end::Action::Continue);
                    } else {
                        prompt.dismiss(
                            &mut meeting_reminder_gate,
                            started.elapsed().as_millis() as u64,
                        );
                    }
                }
            }
            // macOS never starts a second process for an app that is already
            // running, so opening VocalCode from the Dock, Spotlight, Finder or
            // Launchpad never reaches the single-instance socket above — that
            // path only fires on platforms that really do launch a second
            // process. LaunchServices sends this instead. Without it those
            // launches are silently inert, which for a status-item app with no
            // Dock icon and a hidden window looks exactly like a broken app.
            Event::Reopen { .. } => {
                surface_window(&window, &webview, &mut teach_window_restore, tray.is_some());
            }
            Event::NewEvents(StartCause::Init) => {
                if let Some(menu) = tray_menu.take() {
                    match TrayIconBuilder::new()
                        .with_tooltip("VocalCode — push-to-talk voice input")
                        .with_menu(Box::new(menu))
                        .with_icon(make_icon())
                        // No-op off macOS; there `make_icon` returns the
                        // alpha-only glyph the system expects to recolour.
                        .with_icon_as_template(cfg!(target_os = "macos"))
                        .build()
                    {
                        // Held for the process lifetime: dropping the handle
                        // removes the status item.
                        Ok(t) => tray = Some(t),
                        // Not fatal, but the window must then stay reachable,
                        // so refuse to hide it on close further down.
                        Err(e) => log::error!("tray icon failed: {e}"),
                    }
                }
                match overlay_webview_dir.as_ref() {
                    Ok(directory) => {
                        match crate::overlay::Overlay::new(event_target, directory.to_path_buf()) {
                            Ok(o) => overlay = Some(o),
                            Err(e) => log::error!("recording indicator unavailable: {e}"),
                        }
                    }
                    Err(e) => log::error!("recording indicator profile unavailable: {e}"),
                }
                // The config is *not* pushed here. This runs on the first loop
                // iteration, before the webview has parsed a byte of the
                // document, so `window.vocalcodeInit` does not exist yet and
                // the call is discarded without a trace. The page asks for it
                // instead — see `UserEvent::SettingsPageReady`.
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } if window_id == correction_window_id => {
                hide_correction_review(&correction_window, &correction_webview);
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } if window_id == settings_window_id => {
                if let Some(restore) = teach_window_restore.take() {
                    restore_from_teach_capsule(&window, restore, tray.is_some());
                    let _ = webview.evaluate_script("window.vocalcodeTeachClosed()");
                } else {
                    let _ = webview.evaluate_script("window.vocalcodeTeachClosed()");
                    hide_to_tray(&window, tray.is_some());
                }
            }
            Event::UserEvent(ue) => match ue {
                #[cfg(windows)]
                UserEvent::DesktopControl(event) => {
                    let enabled = tick_cfg.try_lock().is_ok_and(|c| c.desktop_control && c.onboarded);
                    if let Some(bar) = &mut desktop_control {
                        if let Some(panel) = bar.event(event, overlay_state.snapshot(),
                            status.ready.load(Ordering::Acquire) && !status.shutdown.load(Ordering::Acquire),
                            status.listening.load(Ordering::Acquire), enabled, &status.dictation_control) {
                            surface_window(&window, &webview, &mut teach_window_restore, tray.is_some());
                            let panel = serde_json::to_string(panel).expect("fixed panel name");
                            let _ = webview.evaluate_script(&format!("window.vocalcodeControlOpen({panel})"));
                        }
                    }
                }
                UserEvent::Drag => {
                    let _ = window.drag_window();
                }
                UserEvent::Minimize => window.set_minimized(true),
                UserEvent::HideToTray => hide_to_tray(&window, tray.is_some()),
                UserEvent::TeachPopupOpen => {
                    teach_window_restore.get_or_insert_with(|| show_teach_capsule(&window, true));
                }
                UserEvent::TeachPopupClose => {
                    if let Some(restore) = teach_window_restore.take() {
                        restore_from_teach_capsule(&window, restore, tray.is_some());
                    }
                    let _ = webview.evaluate_script("window.vocalcodeTeachClosed()");
                }
                UserEvent::Zoom => window.set_maximized(!window.is_maximized()),
                // The wake itself is the work: `push_status` below delivers.
                UserEvent::CaptureReady => {
                    log::info!("capture: UI loop woken for a result");
                }
                UserEvent::Shutdown => {
                    *control_flow = ControlFlow::Exit;
                }
                // Answering the page's own signal is the only delivery that
                // cannot race the document load. A reload re-announces, and
                // re-sending the same snapshot is how the page recovers its
                // rendered state, so this deliberately is not a one-shot.
                UserEvent::SettingsPageReady => {
                    log::info!("settings page ready; delivering config");
                    last_meeting_revision = 0;
                    let _ = webview.evaluate_script(&format!("window.vocalcodeInit({init_json})"));
                }
                UserEvent::CorrectionReviewReady => {
                    correction_review_ready = true;
                    log::info!("correction-review page ready");
                }
                UserEvent::CorrectionReviewClose => {
                    hide_correction_review(&correction_window, &correction_webview);
                }
                UserEvent::MeetingPrompt(crate::meeting_prompt::Event::Ready) => {
                    if let Some(prompt) = &mut meeting_prompt {
                        prompt.ready();
                    }
                    next_meeting_reminder_check = Instant::now();
                }
                UserEvent::MeetingPresenceReady => {
                    next_meeting_reminder_check = Instant::now();
                }
                UserEvent::MeetingPrompt(crate::meeting_prompt::Event::AutoEnd(id, action)) => {
                    if let Some(prompt) = &mut meeting_prompt {
                        if prompt.accepts_auto_end(id) {
                            status.meetings.auto_end_action(id, action);
                            // Keep the acknowledged countdown visible. Other
                            // actions are hidden after the worker clears it.
                        }
                    }
                }
                UserEvent::MeetingPrompt(crate::meeting_prompt::Event::Action(id, action)) => {
                    if let Some(prompt) = &mut meeting_prompt {
                        if let crate::meeting_reminder::ActionResult::Review(candidate) = prompt.apply_action(
                            &mut meeting_reminder_gate,
                            id,
                            action,
                            started.elapsed().as_millis() as u64,
                        ) {
                            // Explicit review only. Starting capture remains the
                            // Meetings page's separate, validated action.
                            surface_window(
                                &window,
                                &webview,
                                &mut teach_window_restore,
                                tray.is_some(),
                            );
                            if crate::meeting_prompt::open_review(&window, &webview, candidate).is_err() {
                                log::warn!("meeting review could not reach the settings page");
                            }
                        }
                    }
                }
            },
            _ => {}
        }

        #[cfg(not(windows))]
        let desktop_control_visible = false;
        #[cfg(windows)]
        let mut desktop_control_visible = desktop_control.as_ref().is_some_and(|bar| bar.is_shown());
        #[cfg(windows)]
        if let Ok(c) = tick_cfg.try_lock() {
            let enabled = c.desktop_control && c.onboarded && !status.shutdown.load(Ordering::Acquire);
            if enabled && !desktop_control_attempted {
                desktop_control_attempted = true;
                let control_proxy = reminder_proxy.clone();
                desktop_control = crate::paths::ensure_trusted_data_subdir(&base, Path::new("webview2/desktop-control"))
                    .map_err(anyhow::Error::from)
                    .and_then(|directory| crate::control_bar::ControlBar::new(event_target, directory, move |event| {
                        let _ = control_proxy.send_event(UserEvent::DesktopControl(event));
                    }))
                    .map_err(|error| {
                        log::warn!("desktop controls unavailable: {error}");
                        status.runtime_errors.push("Desktop controls could not open. Your shortcut and recording indicator are still available. Restart VocalCode to retry.".to_string());
                    }).ok();
            }
            if let Some(bar) = &mut desktop_control {
                desktop_control_visible = bar.tick(crate::control_bar::Frame {
                    enabled, edge: &c.desktop_control_edge, language: &c.ui_lang,
                    snapshot: overlay_state.snapshot(),
                    ready: status.ready.load(Ordering::Acquire) && !status.shutdown.load(Ordering::Acquire),
                    level: audio_level.get(),
                }, &status.dictation_control);
                if let (Some(deadline), ControlFlow::WaitUntil(existing)) = (bar.next_wake(Instant::now()), &mut *control_flow) {
                    *existing = (*existing).min(deadline);
                }
            }
        }

        if let Ok(config) = tick_cfg.try_lock() {
            if config.ui_lang != ui_lang {
                ui_lang.clone_from(&config.ui_lang);
            }
        }
        let lang = notice_lang.get(&ui_lang);
        let readiness = notice_readiness(&status, overlay_state.snapshot().phase);
        let mut notice = status.notices.take();
        // A talk press the input hook passed through because the engine was
        // not ready. The key still reached the foreground app as before; this
        // only says why nothing started. It is the newest thing the person
        // did, so it replaces anything else waiting.
        if not_ready_presses.saw_press(vocalcode_platform::hotkey::talk_presses_while_not_ready()) {
            if let Some(reason) = crate::notice::not_ready_reason(&readiness) {
                notice = Some(crate::notice::Notice::NotReady(reason));
            }
        }
        if !readiness.shutdown {
            if let Some(tray) = &tray {
                let tooltip =
                    crate::notice::tray_tooltip(crate::notice::tray_state(&readiness), lang);
                if tooltip != last_tooltip {
                    if let Err(error) = tray.set_tooltip(Some(&tooltip)) {
                        log::warn!("tray tooltip update failed: {error}");
                    }
                    last_tooltip = tooltip;
                }
            }
        }

        if let Some(o) = overlay.as_mut() {
            // Read back rather than pushed from the save handler: the handler
            // runs on the webview thread and has no way to reach the overlay,
            // which belongs to this loop. One uncontended lock per tick is far
            // cheaper than plumbing a second channel through, and `set_style`
            // does nothing when the value has not changed.
            // Suppression must not depend on obtaining a second config lock:
            // even a single contended tick must never show both indicators.
            let configured_style = tick_cfg.try_lock().ok().map(|c| {
                o.set_lang(&c.ui_lang);
                crate::overlay::Style::parse(&c.overlay_style)
            });
            if let Some(style) = crate::overlay::Style::alongside_capsule(configured_style, desktop_control_visible) {
                o.set_style(style);
            }
            // In demo mode there is no microphone stream, so synthesise a level
            // — otherwise the bars sit at their floor and the level→height path
            // goes unexercised.
            let level = if demo_overlay {
                let t = Instant::now().duration_since(started).as_secs_f32();
                ((t * 3.1).sin() * 0.5 + 0.5) * 0.85 + 0.1
            } else {
                audio_level.get()
            };
            o.tick(&overlay_state, level, notice.map(|notice| notice.text(lang)));
        }

        let reminder_now = Instant::now();
        if reminder_now >= next_meeting_reminder_check {
            next_meeting_reminder_check = reminder_now + Duration::from_millis(500);
            if let Ok(config) = tick_cfg.try_lock() {
                let calendar_connected = status
                    .calendar_snapshot
                    .lock()
                    .ok()
                    .is_some_and(|s| s.connected);
                if calendar_connected
                    && reminder_now >= next_calendar_refresh
                    && !status.shutdown.load(Ordering::Acquire)
                {
                    if let Some(single_flight) =
                        IpcSingleFlightReset::claim(&status, IpcSingleFlight::Calendar)
                    {
                        next_calendar_refresh = reminder_now + Duration::from_secs(5 * 60);
                        status.calendar_cancel.store(false, Ordering::Release);
                        let worker_status = status.clone();
                        let calendar_base = base.clone();
                        // Refresh is metadata-only; the existing reminder gate
                        // decides whether to show anything. No recording action.
                        let _ = spawn_service_worker(
                            &status,
                            "vocalcode-calendar-refresh",
                            move || {
                                let _single_flight = single_flight;
                                if let Err(error) = crate::calendar::refresh_saved_calendar(
                                    &calendar_base,
                                    &worker_status,
                                ) {
                                    worker_status.runtime_errors.push(error);
                                }
                            },
                        );
                    }
                }
                let usable = config.smart_meeting_reminders
                    && config.onboarded
                    && !status.listening.load(Ordering::Acquire)
                    && teach_window_restore.is_none()
                    && status.model_available.load(Ordering::Acquire)
                    && status.pro_gate.load(Ordering::Acquire);
                let mut candidates = Vec::new();
                if let Some(probe) = &meeting_probe {
                    // Drain even while disabled: late observations never arm a
                    // prompt. One outstanding request, no native calls on UI.
                    if let Some((sampled_at, result)) = probe.result() {
                        if usable && sampled_at.elapsed() < Duration::from_secs(3) {
                            candidates = result;
                        }
                    }
                    if usable && reminder_now >= next_meeting_probe {
                        probe.request();
                        next_meeting_probe = reminder_now + Duration::from_millis(500);
                    }
                }
                let calendar_candidate = usable
                    .then(|| {
                        status.calendar_snapshot.lock().ok().and_then(|snapshot| {
                            snapshot.candidate(jiff::Timestamp::now().as_millisecond())
                        })
                    })
                    .flatten();
                candidates.extend(calendar_candidate);
                let elapsed_ms = reminder_now
                    .duration_since(started)
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64;
                // Suspending a card during a dictation is not a dismissal.
                let enabled = config.smart_meeting_reminders && config.onboarded;
                meeting_reminder_gate.update(
                    elapsed_ms,
                    &candidates,
                    enabled,
                    status.meetings.is_active(),
                    &config.ignored_meeting_apps,
                );
                if !usable {
                    meeting_reminder_gate.pause();
                }
                let auto_end_notice = status.meetings.auto_end_notice();
                if (usable || auto_end_notice.is_some()) && !meeting_prompt_attempted {
                    // Warm the hidden page while reminders are enabled, so a
                    // confirmed call does not wait for a new WebView2 startup.
                    // One attempt per process: provider/runtime failure must not
                    // create repeated windows or block the UI every half-second.
                    meeting_prompt_attempted = true;
                    let proxy = reminder_proxy.clone();
                    meeting_prompt = crate::paths::ensure_trusted_data_subdir(
                        &base,
                        Path::new("webview2/meeting-prompt"),
                    )
                    .map_err(anyhow::Error::from)
                    .and_then(|directory| {
                        crate::meeting_prompt::MeetingPrompt::new(
                            event_target,
                            directory,
                            move |event| {
                                let _ = proxy.send_event(UserEvent::MeetingPrompt(event));
                            },
                        )
                    })
                    .map_err(|_| {
                        log::warn!("meeting reminder surface unavailable; manual Meetings remains available")
                    })
                    .ok();
                }
                if let Some(prompt) = &mut meeting_prompt {
                    if let Some(notice) = auto_end_notice {
                        prompt.sync_auto_end(&notice, &config.ui_lang);
                    } else {
                        prompt.sync(meeting_reminder_gate.current(), &config.ui_lang);
                    }
                }
            }
        }

        // A word from the Services menu opens only the Teach capsule. Resize
        // before delivering the word so the full Settings page never flashes.
        let teach_pending = status
            .teach
            .lock()
            .ok()
            .map(|t| t.is_some())
            .unwrap_or(false);
        if teach_pending && teach_window_restore.is_none() {
            teach_window_restore = Some(show_teach_capsule(&window, true));
        }
        if let Some(result) = status
            .correction_result
            .lock()
            .ok()
            .and_then(|mut result| result.take())
        {
            // Settings still needs the authoritative dictionary snapshot, but
            // never the presentation instruction: only the dedicated review
            // window receives the list of learned changes.
            let settings_payload = correction_result_payload(&result, false);
            let _ = webview.evaluate_script(&format!(
                "window.vocalcodeCorrectionResult({settings_payload})"
            ));
            if result.ok && !result.changes.is_empty() && result.document.is_some() {
                pending_correction_review =
                    Some(correction_result_payload(&result, true).to_string());
            }
        }
        if correction_review_ready {
            if let Some(payload) = pending_correction_review.take() {
                present_learned_correction(&correction_window, &correction_webview, &payload);
            }
        }
        push_status(
            &webview,
            &correction_webview,
            &status,
            &capture,
            &mut last_status,
            &mut last_meeting_revision,
        );

        while let Ok(ev) = menu_rx.try_recv() {
            if ev.id == show_id {
                surface_window(&window, &webview, &mut teach_window_restore, tray.is_some());
            } else if ev.id == quit_id {
                request_shutdown(&status, &proxy, ShutdownAction::Quit);
                *control_flow = ControlFlow::Exit;
            }
        }
    });
    // `run_return` is the boundary that makes main's orderly engine/service
    // joins and purge/restart transaction reachable. Stop this module's only
    // auxiliary thread before returning ownership to main.
    shutdown_status.shutdown.store(true, Ordering::Release);
    if !capture_waker.stop_and_join() {
        anyhow::bail!("capture waker panicked during UI shutdown");
    }
    if exit_code != 0 {
        anyhow::bail!("UI event loop exited with status {exit_code}");
    }
    Ok(())
}

const SETTINGS_DATA_URL_PREFIX: &str = "data:text/html;charset=utf-8;base64,";

fn settings_document_data_url() -> String {
    format!(
        "{SETTINGS_DATA_URL_PREFIX}{}",
        BASE64_STANDARD.encode(include_bytes!("webui.html"))
    )
}

fn correction_review_document_data_url() -> String {
    format!(
        "{SETTINGS_DATA_URL_PREFIX}{}",
        BASE64_STANDARD.encode(include_bytes!("correction_review.html"))
    )
}

fn local_webview_navigation(url: &str, settings_document_url: &str) -> bool {
    let allowed = url == "about:blank" || url == settings_document_url;
    if !allowed {
        // A rejected data URL can contain the entire 300 KiB settings document.
        // Log only its bounded metadata so a blocked navigation cannot flood the
        // user's rotating log or reflect attacker-controlled document contents.
        log::warn!("blocked settings WebView navigation ({} bytes)", url.len());
    }
    allowed
}

fn local_correction_review_navigation(url: &str, document_url: &str) -> bool {
    let allowed = url == "about:blank" || url == document_url;
    if !allowed {
        log::warn!(
            "blocked correction-review WebView navigation ({} bytes)",
            url.len()
        );
    }
    allowed
}

/// Push live status (and any pending activation result) into the page — but
/// only when it has actually changed.
///
/// `last` is the previous payload, kept by the caller across ticks.
///
/// This runs on every loop tick — roughly five times a second, forever. It used
/// to serialise the whole status, transcript history included, and hand it to
/// `evaluate_script` unconditionally; the page then re-rendered the history
/// list and the statistics from scratch each time. Idle, with the window shut
/// away in the tray and nobody watching, that was costing about half a core.
///
/// While nothing is happening the payload is byte-identical tick after tick, so
/// comparing it is enough to reduce a permanent background burn to nothing.
/// Comparison is against the serialised form rather than the fields because the
/// history is a growing `Vec` that would have to be cloned to diff any other
/// way — the string has to be built regardless.
fn correction_result_payload(result: &CorrectionResult, include_changes: bool) -> Value {
    let document = result.document.as_ref();
    serde_json::json!({
        "ok": result.ok,
        "review_only": result.review_only,
        "msg": result.message,
        "revision": document.map(|document| document.revision.as_str()),
        "rules": document.map(|document| document.rules.iter()
            .map(|(from, to)| serde_json::json!([from, to]))
            .collect::<Vec<_>>()),
        "changes": if include_changes {
            result.changes.iter().map(|change| serde_json::json!({
                "from": change.from,
                "to": change.to,
                "previous": change.previous.as_ref().map(|(from, to)| [from, to]),
            })).collect::<Vec<_>>()
        } else {
            Vec::new()
        },
    })
}

fn config_snapshot_for_page(config: &Config) -> Value {
    serde_json::json!({
        "talk": config.talk.iter().map(trigger_to_code).collect::<Vec<_>>(),
        "send": config.send.iter().map(trigger_to_code).collect::<Vec<_>>(),
        "teach": config.teach.iter().map(trigger_to_code).collect::<Vec<_>>(),
        "language": config.language,
        "onboarded": config.onboarded,
        "model": config.model,
        "live_caption": config.live_caption,
        "noise_filter": config.noise_filter,
        "overlay_style": config.overlay_style,
        "desktop_control": config.desktop_control,
        "desktop_control_edge": config.desktop_control_edge,
        "desktop_control_available": cfg!(windows),
        "talk_mode": config.talk_mode,
        "paste_insert": config.paste_insert,
        "correction_window_ms": config.correction_window_ms,
        "cue_sounds": config.cue_sounds,
        "min_record_ms": config.min_record_ms,
        // The page models "system default" as an empty selector. Keep result
        // snapshots type-compatible with the initial payload instead of
        // turning that value into JSON null after the first unrelated save.
        "input_device": config.input_device.as_deref().unwrap_or(""),
        "ui_lang": config.ui_lang,
        "autostart": config.autostart,
        "smart_meeting_reminders": config.smart_meeting_reminders,
        "ignored_meeting_apps": config.ignored_meeting_apps,
        "double_tap_lock": config.double_tap_lock,
        "mute_while_dictating": config.mute_while_dictating,
        "mute_available": cfg!(windows),
        "writing": config.writing,
    })
}

pub(crate) fn queue_config_result(
    status: &RuntimeStatus,
    request_id: u64,
    ok: bool,
    generation_bound: bool,
    message: impl Into<String>,
    authoritative: &Config,
) {
    status
        .config_results
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push_back(ConfigResult {
            request_id,
            ok,
            generation_bound,
            message: message.into(),
            authoritative: authoritative.clone(),
        });
}

fn visible_download(status: &RuntimeStatus) -> Option<(String, f64, f64, f64)> {
    status
        .update_download
        .lock()
        .ok()
        .and_then(|download| download.clone())
        .or_else(|| {
            status
                .model_download
                .lock()
                .ok()
                .and_then(|download| download.clone())
        })
}

fn push_status(
    webview: &wry::WebView,
    correction_webview: &wry::WebView,
    status: &RuntimeStatus,
    capture: &CaptureShared,
    last: &mut String,
    last_meeting_revision: &mut u64,
) {
    // The window's regular status cadence is also the normal-operation reaper.
    // Shutdown owns the same list locks and drains whatever remains.
    reap_finished_workers(status);
    if let Some(result) = status
        .calendar_result
        .lock()
        .ok()
        .and_then(|mut r| r.take())
    {
        let _ = webview.evaluate_script(&format!("window.vocalcodeCalendarResult({result})"));
    }
    if let Some(result) = status
        .workflow_result
        .lock()
        .ok()
        .and_then(|mut r| r.take())
    {
        let _ = webview.evaluate_script(&format!("window.vocalcodeWorkflowResult({result})"));
    }
    if let Some(result) = status
        .writing_preview
        .lock()
        .ok()
        .and_then(|mut r| r.take())
    {
        let _ = webview.evaluate_script(&format!("window.vocalcodeWritingPreview({result})"));
    }
    if let Some(result) = status
        .migration_result
        .lock()
        .ok()
        .and_then(|mut result| result.take())
    {
        let _ = webview.evaluate_script(&format!("window.vocalcodeMigrationResult({result})"));
    }
    // Finished key-captures from the global hook → hand each to JS. Drain the
    // queue: one-per-tick delivery staggered a hint and its answer across
    // separate wakeups, which read as the prompt lagging behind the hand.
    while let Some((which, code)) = capture.take_result() {
        let p = serde_json::json!({ "which": which, "code": code });
        log::info!("capture: delivering {which} -> {code} to the page");
        if let Err(e) = webview.evaluate_script(&format!(
            "window.vocalcodeCaptured({}, {})",
            p["which"], p["code"]
        )) {
            log::warn!("capture: evaluate_script failed: {e}");
        }
    }
    let listening = status.listening.load(Ordering::Relaxed);
    let model = status
        .model_label
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default();
    let license = status.license.lock().map(|s| s.clone()).unwrap_or_default();
    let (lic_kind, lic_days) = status
        .license_state
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default();
    let update = status
        .update
        .lock()
        .ok()
        .and_then(|u| u.clone())
        .map(|offer| {
            serde_json::json!({
                "version": offer.version,
                "url": offer.url,
                "notes": offer.notes,
                "sha256": offer.sha256,
                "size": offer.size,
            })
        });
    let update_check = status.update_check.lock().ok().and_then(|mut c| c.take());
    let download = visible_download(status);
    // Note: recognized text is deliberately NOT sent to the window — no echo.
    let payload = serde_json::json!({
        "listening": listening, "model": model, "license": license, "update": update,
        "license_kind": lic_kind, "license_days": lic_days,
        "pro": status.pro_gate.load(Ordering::Relaxed),
        "trial_setup_error": status.trial_setup_error.load(Ordering::Relaxed),
        "update_check": update_check,
        "ready": status.ready.load(Ordering::Relaxed),
        "meeting_active": status.meetings.is_active(),
        "noise_filter": status.noise_filter.snapshot(),
        "meeting_transcribing": status.meetings.is_transcribing(),
        "onboarded": status.onboarded.load(Ordering::Relaxed),
        "permissions_ok": status.permissions_ok.load(Ordering::Relaxed),
        "totals": status.totals.lock().ok().map(|t| serde_json::json!({
            "dictations": t.dictations, "words": t.words, "chars": t.chars })),
        "insights": status.activity.snapshot(),
        "history": status.history.lock().ok().map(|h| h.clone()).unwrap_or_default(),
        "download": download
            .map(|(label, pct, done, total)| serde_json::json!({
                "label": label, "pct": pct, "done": done, "total": total })),
    });
    let payload = payload.to_string();
    if payload != *last {
        let _ = webview.evaluate_script(&format!("window.vocalcodeStatus({payload})"));
        *last = payload;
    }

    if let Some((revision, meetings)) = status.meetings.update_after(*last_meeting_revision) {
        let _ = webview.evaluate_script(&format!("window.vocalcodeMeetings({meetings})"));
        *last_meeting_revision = revision;
    }

    // Hand a finished activation result to the page exactly once.
    if let Some((ok, msg)) = status.activation.lock().ok().and_then(|mut a| a.take()) {
        let payload = serde_json::json!({ "ok": ok, "msg": msg });
        let _ = webview.evaluate_script(&format!(
            "window.vocalcodeActivated({}, {})",
            payload["ok"], payload["msg"]
        ));
    }
    // A word the user right-clicked somewhere else entirely.
    if let Some(word) = status
        .teach
        .lock()
        .map(|mut teach| teach.take())
        .unwrap_or_else(|poisoned| poisoned.into_inner().take())
    {
        let payload = serde_json::json!(word);
        let _ = webview.evaluate_script(&format!("window.vocalcodeTeach({payload}, true)"));
    }
    // Likewise the updater's, which needs to land on the update button rather
    // than in the licence note.
    if let Some((ok, msg)) = status.update_result.lock().ok().and_then(|mut a| a.take()) {
        let payload = serde_json::json!({ "ok": ok, "msg": msg });
        let _ = webview.evaluate_script(&format!(
            "window.vocalcodeUpdateResult({}, {})",
            payload["ok"], payload["msg"]
        ));
    }
    if let Some((ok, msg)) = status
        .settings_result
        .lock()
        .ok()
        .and_then(|mut a| a.take())
    {
        let payload = serde_json::json!({ "ok": ok, "msg": msg });
        let _ = webview.evaluate_script(&format!(
            "window.vocalcodeSettingsResult({}, {})",
            payload["ok"], payload["msg"]
        ));
    }
    if let Some(result) = status
        .dictionary_result
        .lock()
        .ok()
        .and_then(|mut result| result.take())
    {
        let document = result.document.as_ref();
        let payload = serde_json::json!({
            "request_id": result.request_id,
            "ok": result.ok,
            "conflict": result.conflict,
            "msg": result.message,
            "revision": document.map(|document| document.revision.as_str()),
            "rules": document.map(|document| document.rules.iter()
                .map(|(from, to)| serde_json::json!([from, to]))
                .collect::<Vec<_>>()),
        });
        if is_correction_review_request_id(result.request_id) {
            let _ = correction_webview
                .evaluate_script(&format!("window.vocalcodeCorrectionSaveResult({payload})"));
            // Reconcile Settings without reopening its old teach surface.
            let sync = serde_json::json!({
                "ok": result.ok,
                "msg": result.message,
                "revision": document.map(|document| document.revision.as_str()),
                "rules": document.map(|document| document.rules.iter()
                    .map(|(from, to)| serde_json::json!([from, to]))
                    .collect::<Vec<_>>()),
                "changes": [],
            });
            let _ = webview.evaluate_script(&format!("window.vocalcodeCorrectionResult({sync})"));
        } else {
            let _ =
                webview.evaluate_script(&format!("window.vocalcodeDictionaryResult({payload})"));
        }
    }
    let config_results = status
        .config_results
        .lock()
        .map(|mut results| results.drain(..).collect::<Vec<_>>())
        .unwrap_or_default();
    for result in config_results {
        let payload = serde_json::json!({
            "id": result.request_id,
            "ok": result.ok,
            "generation_bound": result.generation_bound,
            "msg": result.message,
            "config": config_snapshot_for_page(&result.authoritative),
        });
        let _ = webview.evaluate_script(&format!(
            "window.vocalcodeConfigResult({}, {}, {}, {}, {})",
            payload["id"],
            payload["ok"],
            payload["generation_bound"],
            payload["msg"],
            payload["config"]
        ));
    }
    if let Some((id, ok, msg)) = status
        .clipboard_result
        .lock()
        .ok()
        .and_then(|mut a| a.take())
    {
        let payload = serde_json::json!({ "id": id, "ok": ok, "msg": msg });
        let _ = webview.evaluate_script(&format!(
            "window.vocalcodeCopyResult({}, {}, {})",
            payload["id"], payload["ok"], payload["msg"]
        ));
    }
    for msg in status.runtime_errors.drain() {
        let payload = serde_json::json!(msg);
        let _ = webview.evaluate_script(&format!("window.vocalcodeRuntimeError({payload})"));
    }
}

/// What the UI thread can see of readiness, for the not-ready notice and the
/// tray tooltip. Read fresh each tick; none of it waits on the engine.
fn notice_readiness(
    status: &RuntimeStatus,
    phase: crate::overlay::Phase,
) -> crate::notice::Readiness {
    crate::notice::Readiness {
        shutdown: status.shutdown.load(Ordering::Acquire),
        onboarded: status.onboarded.load(Ordering::Acquire),
        permissions_ok: status.permissions_ok.load(Ordering::Acquire),
        download: status
            .model_download
            .lock()
            .ok()
            .and_then(|download| download.as_ref().map(|(_, percent, _, _)| *percent)),
        model_available: status.model_available.load(Ordering::Acquire),
        model_failed: status
            .model_label
            .lock()
            .is_ok_and(|label| label.starts_with(crate::MODEL_ERROR_LABEL)),
        microphone_failed: status.microphone_failed.load(Ordering::Acquire),
        ready: status.ready.load(Ordering::Acquire),
        phase,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InfoTarget {
    Privacy,
    Support,
}

impl InfoTarget {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "privacy" => Some(Self::Privacy),
            "support" => Some(Self::Support),
            _ => None,
        }
    }
}

fn save_dictionary_request(
    base: &Path,
    request_id: u64,
    revision: &str,
    lines: &[String],
) -> DictionaryResult {
    let expected = match crate::RulesRevision::parse(revision) {
        Ok(revision) => revision,
        Err(error) => {
            log::warn!("save dictionary: {error}");
            return DictionaryResult {
                request_id,
                ok: false,
                conflict: false,
                message: DICTIONARY_REVISION_REQUIRED.to_string(),
                document: None,
            };
        }
    };

    match crate::save_rules_if_current(base, &expected, lines) {
        Ok(document) => DictionaryResult {
            request_id,
            ok: true,
            conflict: false,
            message: "Saved".to_string(),
            document: Some(document),
        },
        Err(error) if error.is_conflict() => {
            log::warn!("save dictionary conflict: {error}");
            match crate::load_rules_document(base) {
                Ok(document) => DictionaryResult {
                    request_id,
                    ok: false,
                    conflict: true,
                    message: DICTIONARY_CONFLICT_RELOADED.to_string(),
                    document: Some(document),
                },
                Err(reload_error) => {
                    log::error!(
                        "save dictionary conflict could not reload the latest document: {reload_error}"
                    );
                    DictionaryResult {
                        request_id,
                        ok: false,
                        conflict: true,
                        message: DICTIONARY_CONFLICT_RELOAD_FAILED.to_string(),
                        document: None,
                    }
                }
            }
        }
        Err(error) => {
            log::error!("save dictionary: {error}");
            DictionaryResult {
                request_id,
                ok: false,
                conflict: false,
                message: format!("Could not save dictionary: {error}"),
                document: None,
            }
        }
    }
}

/// Handle one JSON message from the page.
#[allow(clippy::too_many_arguments)]
fn handle_ipc(
    body: String,
    status: &Arc<RuntimeStatus>,
    cfg: &Arc<Mutex<Config>>,
    base: &Path,
    refresh_license: &LicenseRefresh,
    proxy: &EventLoopProxy<UserEvent>,
    capture: &Arc<CaptureShared>,
    config_saves: &std::sync::mpsc::SyncSender<ConfigSaveRequest>,
    surface: IpcSurface,
) {
    if body.len() > MAX_SETTINGS_IPC_BYTES {
        log::warn!(
            "ipc: rejected oversized message ({} bytes; limit {MAX_SETTINGS_IPC_BYTES})",
            body.len()
        );
        status.runtime_errors.push(
            "A Settings request was too large and was refused. Reduce the entry and try again."
                .to_string(),
        );
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        // IPC bodies may contain a reusable licence key or dictated text. Log
        // only metadata; malformed input must never turn secrets into disk logs.
        log::warn!("ipc: rejected malformed json ({} bytes)", body.len());
        return;
    };
    let message_type = v.get("type").and_then(|t| t.as_str());
    // Enforce the edition boundary natively, not just by hiding buttons. No
    // community WebView can start a purchase, send a key/email, or install a
    // binary from the paid release channel.
    if let Some(reason) = crate::community::blocked_ipc_reason(message_type) {
        if matches!(message_type, Some("checkupdate" | "update")) {
            *status.update_result.lock().unwrap() = Some((false, reason.to_string()));
        } else {
            *status.activation.lock().unwrap() = Some((false, reason.to_string()));
        }
        return;
    }
    if surface == IpcSurface::CorrectionReview
        && !matches!(
            message_type,
            Some("ready" | "save_dict" | "correction_popup_close")
        )
    {
        log::warn!("correction-review ipc: rejected unavailable message type {message_type:?}");
        return;
    }
    match message_type {
        Some("calendar_cancel") => {
            status.calendar_cancel.store(true, Ordering::Release);
        }
        Some("calendar") => {
            let Some(single_flight) =
                IpcSingleFlightReset::claim(status, IpcSingleFlight::Calendar)
            else {
                return;
            };
            status.calendar_cancel.store(false, Ordering::Release);
            let input = if v["op"] == "configure" {
                rfd::FileDialog::new()
                    .set_title("Select Google desktop OAuth client JSON")
                    .add_filter("JSON", &["json"])
                    .pick_file()
            } else {
                None
            };
            if v["op"] == "configure" && input.is_none() {
                *status.calendar_result.lock().unwrap() =
                    Some(serde_json::json!({"id":v["id"],"ok":true,"cancelled":true}));
                return;
            }
            let base = base.to_path_buf();
            let worker_status = status.clone();
            let request_id = v["id"].clone();
            if let Err(error) = spawn_service_worker(status, "vocalcode-calendar", move || {
                let result = crate::calendar::handle(&base, &worker_status, &v, input);
                let value = match result {
                    Ok(data) => serde_json::json!({"id":v["id"],"ok":true,"data":data}),
                    Err(error) => serde_json::json!({"id":v["id"],"ok":false,"message":error}),
                };
                drop(single_flight);
                *worker_status.calendar_result.lock().unwrap() = Some(value);
            }) {
                *status.calendar_result.lock().unwrap() =
                    Some(serde_json::json!({"id":request_id,"ok":false,"message":error}));
            }
        }
        Some("writing_preview") => {
            // The Writing page's "Try it" box: run the same rules a dictation
            // would get, on text the person typed, against unsaved settings.
            // Bounded like any IPC string; never logged, stored or delivered.
            let text = v["text"].as_str().unwrap_or("");
            let language = v["language"].as_str().unwrap_or("en");
            let app = v["app"].as_str().unwrap_or("");
            let result = serde_json::from_value::<vocalcode_core::config::WritingConfig>(
                v["writing"].clone(),
            )
            .map_err(|_| "writing settings are malformed".to_string())
            .and_then(|writing| writing.validate().map(|()| writing))
            .map(|writing| {
                let written =
                    vocalcode_core::writing::apply(text, language, &writing.options_for(app));
                serde_json::json!({
                    "id": v["id"],
                    "ok": true,
                    "text": written.text,
                    "edits": written.edits,
                    "send": written.send,
                })
            })
            .unwrap_or_else(
                |message| serde_json::json!({"id": v["id"], "ok": false, "message": message}),
            );
            *status
                .writing_preview
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
            // Only a wake-up: the end-of-iteration push_status delivers it.
            let _ = proxy.send_event(UserEvent::CaptureReady);
        }
        Some("workflow") => {
            let Some(single_flight) =
                IpcSingleFlightReset::claim(status, IpcSingleFlight::Workflow)
            else {
                return;
            };
            let output = if v["op"] == "export_history" {
                rfd::FileDialog::new()
                    .set_title("Export readable diagnostics — keep this file private")
                    .set_file_name("vocalcode-diagnostics.jsonl")
                    .add_filter("JSON Lines", &["jsonl"])
                    .save_file()
            } else {
                None
            };
            if v["op"] == "export_history" && output.is_none() {
                *status.workflow_result.lock().unwrap() =
                    Some(serde_json::json!({"id":v["id"],"ok":true,"cancelled":true}));
                return;
            }
            let base = base.to_path_buf();
            let worker_status = status.clone();
            let request_id = v["id"].clone();
            if let Err(error) = spawn_service_worker(status, "vocalcode-workflow", move || {
                let result = crate::workflows::handle(&base, &worker_status, &v, output);
                let value = match result {
                    Ok(data) => serde_json::json!({"id":v["id"],"ok":true,"data":data}),
                    Err(error) => serde_json::json!({"id":v["id"],"ok":false,"message":error}),
                };
                drop(single_flight);
                *worker_status.workflow_result.lock().unwrap() = Some(value);
            }) {
                *status.workflow_result.lock().unwrap() =
                    Some(serde_json::json!({"id":request_id,"ok":false,"message":error}));
            }
        }
        Some("migration") => {
            let Some(single_flight) =
                IpcSingleFlightReset::claim(status, IpcSingleFlight::Migration)
            else {
                return;
            };
            let op = v["op"].as_str().unwrap_or("");
            let input = if op == "pick" {
                rfd::FileDialog::new()
                    .set_title("Import dictionary or snippets — preview first")
                    .add_filter("UTF-8 CSV / JSON / text", &["csv", "json", "txt"])
                    .pick_file()
            } else {
                None
            };
            let output = if op == "export" {
                rfd::FileDialog::new()
                    .set_title("Export VocalCode entries (plain text JSON)")
                    .set_file_name(if v["kind"] == "snippets" {
                        "vocalcode-snippets.json"
                    } else {
                        "vocalcode-dictionary.json"
                    })
                    .add_filter("JSON", &["json"])
                    .save_file()
            } else {
                None
            };
            if (op == "pick" && input.is_none()) || (op == "export" && output.is_none()) {
                *status.migration_result.lock().unwrap() =
                    Some(serde_json::json!({"id":v["id"],"ok":true,"cancelled":true}));
                return;
            }
            let base = base.to_path_buf();
            let worker_status = status.clone();
            let request_id = v["id"].clone();
            if let Err(error) = spawn_service_worker(status, "vocalcode-migration", move || {
                let _single_flight = single_flight;
                let result = crate::migration::handle(&base, &worker_status, &v, input, output);
                let value = match result {
                    Ok(value) => {
                        serde_json::json!({"id":v["id"],"op":v["op"],"ok":true,"data":value})
                    }
                    Err(error) => {
                        serde_json::json!({"id":v["id"],"op":v["op"],"ok":false,"message":error})
                    }
                };
                // Release admission before publishing the reply; the UI can
                // submit its next request as soon as it sees this value.
                drop(_single_flight);
                *worker_status.migration_result.lock().unwrap() = Some(value);
            }) {
                *status.migration_result.lock().unwrap() =
                    Some(serde_json::json!({"id":request_id,"ok":false,"message":error}));
            }
        }
        // Sent once, after the page has defined `window.vocalcodeInit`. Only
        // the page knows when that is, so it says so rather than the host
        // guessing at a moment that is always too early.
        Some("ready") => {
            let event = match surface {
                IpcSurface::Settings => UserEvent::SettingsPageReady,
                IpcSurface::CorrectionReview => UserEvent::CorrectionReviewReady,
            };
            let _ = proxy.send_event(event);
        }
        Some("save") => {
            let request_id = v.get("request_id").and_then(Value::as_u64).unwrap_or(0);
            if let Some(c) = v.get("config") {
                if config_saves
                    .try_send(ConfigSaveRequest {
                        request_id,
                        config: c.clone(),
                    })
                    .is_err()
                {
                    let authoritative = cfg
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    publish_config_save_failure(
                        status,
                        request_id,
                        "Could not save settings: the settings queue is busy or unavailable",
                        &authoritative,
                    );
                }
            }
        }
        Some("setlang") => {
            // First-run picker: set the chosen language, mark onboarded, persist,
            // and release the background thread that is blocked waiting to
            // download exactly the chosen model (nothing before the pick).
            if let Some(l) = v.get("lang").and_then(|x| x.as_str()) {
                let request = ConfigSaveRequest {
                    request_id: 0,
                    config: serde_json::json!({ "language": l, "onboarded": true }),
                };
                if config_saves.try_send(request).is_err() {
                    *status
                        .settings_result
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((
                        false,
                        "Could not save language: the settings queue is busy or unavailable"
                            .to_string(),
                    ));
                }
            }
        }
        Some("activate") => {
            if let Some(key) = v.get("key").and_then(|k| k.as_str()) {
                let Some(single_flight) =
                    IpcSingleFlightReset::claim(status, IpcSingleFlight::Activation)
                else {
                    publish_activation_if_empty(status, "Activation is already in progress.");
                    return;
                };
                let key = key.to_string();
                let base = base.to_path_buf();
                let worker_status = status.clone();
                let refresh = refresh_license.clone();
                let started = spawn_service_worker(status, "vocalcode-activate", move || {
                    let _single_flight = single_flight;
                    let (ok, msg) = match crate::activation::activate_cancellable(
                        &key,
                        &base,
                        &worker_status.shutdown,
                    ) {
                        Ok(Some(())) => {
                            if worker_status.shutdown.load(Ordering::Acquire) {
                                return;
                            }
                            let (text, state) = refresh();
                            let allowed = matches!(state.0.as_str(), "licensed" | "trial");
                            worker_status.inject_gate.store(true, Ordering::Release);
                            worker_status.pro_gate.store(allowed, Ordering::Release);
                            *worker_status.license.lock().unwrap() = text;
                            *worker_status.license_state.lock().unwrap() = state;
                            if allowed {
                                (
                                    true,
                                    "Activated — you're all set. No restart needed.".to_string(),
                                )
                            } else {
                                (
                                false,
                                "The receipt was saved, but it does not authorize this version."
                                    .to_string(),
                            )
                            }
                        }
                        Ok(None) => return,
                        Err(e) => (false, format!("Activation failed: {e}")),
                    };
                    if worker_status.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    *worker_status.activation.lock().unwrap() = Some((ok, msg));
                });
                if let Err(error) = started {
                    *status.activation.lock().unwrap() =
                        Some((false, format!("Activation could not start: {error}")));
                }
            }
        }
        Some("save_dict") => {
            let request_id = v.get("request_id").and_then(Value::as_u64).unwrap_or(0);
            if surface == IpcSurface::CorrectionReview
                && !is_correction_review_request_id(request_id)
            {
                log::warn!("correction-review ipc: rejected an unscoped dictionary request");
                return;
            }
            if let Some(rules) = v.get("rules").and_then(|x| x.as_array()) {
                let reject = |message: String| {
                    *status
                        .dictionary_result
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                        Some(DictionaryResult {
                            request_id,
                            ok: false,
                            conflict: false,
                            message,
                            document: None,
                        });
                };
                if rules.len() > MAX_DICTIONARY_RULES {
                    reject(format!(
                        "Dictionary has {} rules; at most {MAX_DICTIONARY_RULES} are allowed.",
                        rules.len()
                    ));
                    return;
                }
                let mut lines = Vec::with_capacity(rules.len());
                let mut normalized_bytes = 0usize;
                for rule in rules {
                    let Some(a) = rule.get(0).and_then(Value::as_str).map(str::trim) else {
                        continue;
                    };
                    let Some(b) = rule.get(1).and_then(Value::as_str).map(str::trim) else {
                        continue;
                    };
                    if a.is_empty() && b.is_empty() {
                        continue;
                    }
                    if a.is_empty()
                        || b.is_empty()
                        || a.len() > MAX_DICTIONARY_SIDE_UTF8_BYTES
                        || b.len() > MAX_DICTIONARY_SIDE_UTF8_BYTES
                        || a.starts_with('#')
                        || a.contains("=>")
                        || a.contains('\r')
                        || a.contains('\n')
                        || b.contains('\r')
                        || b.contains('\n')
                    {
                        reject(
                            if a.len() > MAX_DICTIONARY_SIDE_UTF8_BYTES
                                || b.len() > MAX_DICTIONARY_SIDE_UTF8_BYTES
                            {
                                format!(
                                "Dictionary phrases must be at most {MAX_DICTIONARY_SIDE_UTF8_BYTES} UTF-8 bytes."
                            )
                            } else {
                                DICTIONARY_INVALID_ENTRY.to_string()
                            },
                        );
                        return;
                    }
                    normalized_bytes = match normalized_bytes
                        .checked_add(a.len())
                        .and_then(|size| size.checked_add(b.len()))
                        .and_then(|size| size.checked_add(5))
                    {
                        Some(size) if size <= MAX_DICTIONARY_DOCUMENT_BYTES => size,
                        _ => {
                            reject(format!(
                                "Dictionary rules exceed the {MAX_DICTIONARY_DOCUMENT_BYTES}-byte document safety limit."
                            ));
                            return;
                        }
                    };
                    lines.push(format!("{a} => {b}"));
                }
                let revision = v.get("revision").and_then(Value::as_str).unwrap_or("");
                let result = save_dictionary_request(base, request_id, revision, &lines);
                if result.ok {
                    if let Some(document) = &result.document {
                        crate::diagnostics::queue_event(
                            status,
                            "dictionary_saved",
                            &document.rules,
                        );
                    }
                }
                if let Some(document) = result.document.as_ref() {
                    // Both a successful edit and a conflict reload are exact
                    // disk snapshots, so the next utterance should use the same
                    // rules the Dictionary panel now shows.
                    *status.rules.lock().unwrap() = crate::merge_rules(&document.rules);
                }
                *status.dictionary_result.lock().unwrap() = Some(result);
            } else {
                *status.dictionary_result.lock().unwrap() = Some(DictionaryResult {
                    request_id,
                    ok: false,
                    conflict: false,
                    message: DICTIONARY_INVALID_ENTRY.to_string(),
                    document: None,
                });
            }
        }
        Some("restore") => {
            if let Some(email) = v.get("email").and_then(|x| x.as_str()) {
                let email = email.trim().to_lowercase();
                if email.len() > 254 || !email.contains('@') {
                    *status.activation.lock().unwrap() =
                        Some((false, "Enter the email you bought with".to_string()));
                    return;
                }
                let Some(single_flight) =
                    IpcSingleFlightReset::claim(status, IpcSingleFlight::Restore)
                else {
                    publish_activation_if_empty(status, "Restore is already in progress.");
                    return;
                };
                let worker_status = status.clone();
                let started = spawn_service_worker(status, "vocalcode-restore", move || {
                    let _single_flight = single_flight;
                    let request = crate::activation::cancellable_network_request(
                        &worker_status.shutdown,
                        "vocalcode-restore-request",
                        move || send_restore_request(email),
                    );
                    let ok = match request {
                        Ok(crate::activation::CancellableRequest::Completed(ok)) => ok,
                        Ok(crate::activation::CancellableRequest::Cancelled) => return,
                        Err(_) => false,
                    };
                    if worker_status.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    let message = if ok {
                        // Deliberately identical whether the address exists. A
                        // 202 only accepts the background recovery task; it does
                        // not prove that the later email delivery succeeded.
                        "If a purchase exists, recovery instructions will arrive by email."
                    } else {
                        "Network error — try again."
                    };
                    *worker_status.activation.lock().unwrap() = Some((ok, message.to_string()));
                });
                if let Err(error) = started {
                    *status.activation.lock().unwrap() =
                        Some((false, format!("Restore could not start: {error}")));
                }
            }
        }
        Some("meeting_start") => {
            let requested_title = v
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            if requested_title.len() > 512 {
                status
                    .runtime_errors
                    .push("Meeting title is too long.".to_string());
                return;
            }
            let title = if requested_title.is_empty() {
                "Meeting"
            } else {
                requested_title
            }
            .to_string();
            let microphone = v.get("microphone").and_then(Value::as_bool).unwrap_or(true);
            let system_audio = v
                .get("system_audio")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let keep_audio = v
                .get("keep_audio")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !status.pro_gate.load(Ordering::Acquire) {
                status.runtime_errors.push("Meetings are included in Pro. Start the Pro trial or activate a licence to record one."
                        .to_string());
                return;
            }
            if !status.model_available.load(Ordering::Acquire) {
                status
                    .runtime_errors
                    .push("Wait for the local speech model to finish loading.".to_string());
                return;
            }
            if status.listening.load(Ordering::Acquire) {
                status
                    .runtime_errors
                    .push("Finish the current dictation before starting a meeting.".to_string());
                return;
            }
            let config = cfg.lock().unwrap_or_else(|value| value.into_inner());
            if let Err(error) = status.meetings.start_live(
                title,
                microphone,
                system_audio,
                config.input_device.clone(),
                keep_audio,
                config.language.clone(),
                v["auto_end_minutes"]
                    .as_u64()
                    .filter(|value| matches!(value, 0 | 5 | 10 | 15))
                    .unwrap_or(5),
            ) {
                status.runtime_errors.push(error);
            } else {
                status.ready.store(false, Ordering::Release);
            }
        }
        Some("meeting_stop") => {
            if let Err(error) = status.meetings.stop() {
                status.runtime_errors.push(error);
            }
        }
        Some("meeting_auto_end_continue") => {
            if let Some(id) = v["id"].as_u64() {
                status
                    .meetings
                    .auto_end_action(id, vocalcode_meeting::auto_end::Action::Continue);
            }
        }
        Some("meeting_import") => {
            if !status.pro_gate.load(Ordering::Acquire) {
                status.runtime_errors.push("Meeting import is included in Pro. Start the Pro trial or activate a licence to use it."
                        .to_string());
                return;
            }
            if !status.model_available.load(Ordering::Acquire) {
                status
                    .runtime_errors
                    .push("Wait for the local speech model to finish loading.".to_string());
                return;
            }
            if status.listening.load(Ordering::Acquire) {
                status
                    .runtime_errors
                    .push("Finish the current dictation before importing a meeting.".to_string());
                return;
            }
            let Some(path) = rfd::FileDialog::new()
                .set_title("Import meeting audio")
                .add_filter(
                    "Audio",
                    &[
                        "wav", "mp3", "m4a", "mp4", "aac", "flac", "ogg", "caf", "aiff", "mkv",
                    ],
                )
                .pick_file()
            else {
                return;
            };
            let source_title = path
                .file_stem()
                .and_then(|name| name.to_str())
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or("Imported meeting");
            let title = if source_title.len() <= 512 {
                source_title
            } else {
                let mut end = 512;
                while !source_title.is_char_boundary(end) {
                    end -= 1;
                }
                &source_title[..end]
            }
            .to_string();
            let language = cfg
                .lock()
                .unwrap_or_else(|value| value.into_inner())
                .language
                .clone();
            if let Err(error) = status.meetings.import(path, title, language) {
                status.runtime_errors.push(error);
            } else {
                status.ready.store(false, Ordering::Release);
            }
        }
        Some("meeting_select") => {
            let result = v
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "Missing meeting identifier.".to_string())
                .and_then(|id| {
                    vocalcode_meeting::MeetingId::parse(id).map_err(|error| error.to_string())
                })
                .and_then(|id| status.meetings.select(id));
            if let Err(error) = result {
                status.runtime_errors.push(error);
            }
        }
        Some("meeting_search") => {
            let query = v
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(256)
                .collect();
            if let Err(error) = status.meetings.search(query) {
                status.runtime_errors.push(error);
            }
        }
        Some("meeting_rename") => {
            let result = (|| -> Result<(), String> {
                let id = vocalcode_meeting::MeetingId::parse(
                    v.get("id").and_then(Value::as_str).unwrap_or(""),
                )
                .map_err(|error| error.to_string())?;
                let title = v
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|title| !title.is_empty() && title.len() <= 512)
                    .ok_or_else(|| "Enter a meeting title.".to_string())?
                    .to_string();
                status.meetings.rename(id, title)
            })();
            if let Err(error) = result {
                status.runtime_errors.push(error);
            }
        }
        Some("meeting_rename_speaker") => {
            let result = (|| -> Result<(), String> {
                let id = vocalcode_meeting::MeetingId::parse(
                    v.get("id").and_then(Value::as_str).unwrap_or(""),
                )
                .map_err(|error| error.to_string())?;
                let speaker_id = v
                    .get("speaker_id")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty() && value.len() <= 128)
                    .ok_or_else(|| "Missing speaker identifier.".to_string())?
                    .to_string();
                let label = v
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty() && value.len() <= 128)
                    .ok_or_else(|| "Enter a speaker name up to 128 bytes.".to_string())?
                    .to_string();
                status.meetings.rename_speaker(id, speaker_id, label)
            })();
            if let Err(error) = result {
                status.runtime_errors.push(error);
            }
        }
        Some("meeting_bookmark") => {
            let result = (|| -> Result<(), String> {
                let id = vocalcode_meeting::MeetingId::parse(
                    v.get("id").and_then(Value::as_str).unwrap_or(""),
                )
                .map_err(|error| error.to_string())?;
                let at_ms = v.get("at_ms").and_then(Value::as_u64).unwrap_or_default();
                let label = v
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("Bookmark")
                    .chars()
                    .take(256)
                    .collect();
                status.meetings.bookmark(id, at_ms, label)
            })();
            if let Err(error) = result {
                status.runtime_errors.push(error);
            }
        }
        Some("meeting_delete") => {
            let result = vocalcode_meeting::MeetingId::parse(
                v.get("id").and_then(Value::as_str).unwrap_or(""),
            )
            .map_err(|error| error.to_string())
            .and_then(|id| status.meetings.delete(id));
            if let Err(error) = result {
                status.runtime_errors.push(error);
            }
        }
        Some("meeting_export") => {
            let result = (|| -> Result<(), String> {
                let id = vocalcode_meeting::MeetingId::parse(
                    v.get("id").and_then(Value::as_str).unwrap_or(""),
                )
                .map_err(|error| error.to_string())?;
                let (kind, extension, label) = match v.get("format").and_then(Value::as_str) {
                    Some("txt") => (crate::meeting::ExportKind::Text, "txt", "Text"),
                    Some("json") => (crate::meeting::ExportKind::Json, "json", "JSON"),
                    Some("srt") => (crate::meeting::ExportKind::Srt, "srt", "SubRip subtitles"),
                    _ => (crate::meeting::ExportKind::Markdown, "md", "Markdown"),
                };
                let Some(path) = rfd::FileDialog::new()
                    .set_title("Export local meeting")
                    .set_file_name(format!("vocalcode-meeting.{extension}"))
                    .add_filter(label, &[extension])
                    .save_file()
                else {
                    return Ok(());
                };
                status.meetings.export(id, kind, path)
            })();
            if let Err(error) = result {
                status.runtime_errors.push(error);
            }
        }
        Some("copy") => {
            if let Some(text) = v.get("text").and_then(|x| x.as_str()) {
                let id = v
                    .get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let result = copy_to_clipboard(text);
                *status.clipboard_result.lock().unwrap() = Some(match result {
                    Ok(()) => (id, true, "Copied".to_string()),
                    Err(e) => (id, false, format!("Copy failed: {e}")),
                });
            }
        }
        Some("capture") => {
            if let Some(w) = v.get("which").and_then(|x| x.as_str()) {
                capture.start(w);
            }
        }
        // The page answering a capture from its own `keydown`.
        //
        // While this window has focus the low-level hook is not called at all
        // — a Chromium-backed foreground window suppresses it, which is exactly
        // the state a person is in when they click Add and press a key. The
        // keystroke is being delivered to this window anyway, so the page reads
        // it directly and hands it here. The hook still owns every unfocused
        // case and both paths end in the same `CaptureShared`, so whichever
        // sees the key first answers and the other finds nothing pending.
        Some("capture_key") => match v.get("code").and_then(|x| x.as_str()) {
            Some(code) if !code.is_empty() && code.len() <= MAX_SERIALIZED_TRIGGER_UTF8_BYTES => {
                capture.answer_from_page(code)
            }
            Some(code) if code.len() > MAX_SERIALIZED_TRIGGER_UTF8_BYTES => log::warn!(
                "capture: page sent an oversized key code ({} bytes)",
                code.len()
            ),
            _ => log::warn!("capture: page sent a key with no code"),
        },
        Some("captured_ack") => {
            // The page confirming it RAN vocalcodeCaptured — the last probe in
            // the delivery pipeline (queued → woken → delivered → executed).
            // A delivery log without this line means the script call was
            // swallowed inside the WebView.
            let which = v.get("which").and_then(|x| x.as_str()).unwrap_or("?");
            let code = v.get("code").and_then(|x| x.as_str()).unwrap_or("?");
            if which.len() <= MAX_CONFIG_TOKEN_UTF8_BYTES
                && code.len() <= MAX_SERIALIZED_TRIGGER_UTF8_BYTES
            {
                log::info!("capture: page acknowledged {which}:{code}");
            } else {
                log::warn!(
                    "capture: page acknowledged oversized fields (which {} bytes, code {} bytes)",
                    which.len(),
                    code.len(),
                );
            }
        }
        Some("buy") => {
            // Tag the checkout with the app's ref so this native host can poll
            // for a signed, device-bound receipt and auto-activate. Neither the
            // native client nor WebView receives the reusable key.
            let Some(reference) = v
                .get("ref")
                .and_then(|x| x.as_str())
                .filter(|r| {
                    r.len() >= 20
                        && r.len() <= 128
                        && r.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
                .map(str::to_string)
            else {
                *status.activation.lock().unwrap() =
                    Some((false, "Could not start checkout.".to_string()));
                return;
            };
            // Claim the one active purchase before opening a browser tab. If
            // this happened after `open_url`, a double-click created a second
            // checkout with a different reference while the app kept polling
            // only the first; paying in that second tab could never auto-
            // activate.
            if status
                .purchase_polling
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                *status.activation.lock().unwrap() = Some((
                    false,
                    "A checkout is already open; finish it in your browser.".to_string(),
                ));
                return;
            }
            let worker_status = status.clone();
            let base = base.to_path_buf();
            let refresh = refresh_license.clone();
            let started = spawn_service_worker(status, "vocalcode-checkout", move || {
                struct PollReset(Arc<RuntimeStatus>);
                impl Drop for PollReset {
                    fn drop(&mut self) {
                        self.0.purchase_polling.store(false, Ordering::Release);
                    }
                }
                let _reset = PollReset(worker_status.clone());
                let device = vocalcode_platform::device_id();
                if device.trim().is_empty() || device == "vocalcode-unknown-device" {
                    *worker_status.activation.lock().unwrap() = Some((
                        false,
                        "A stable device identity is required for activation.".to_string(),
                    ));
                    return;
                }
                // Bind the reference to this installation before putting it in
                // a browser URL. Browser and payment-provider logs may expose
                // the reference; it must not be a bearer credential that lets
                // another machine consume an activation slot.
                let intent_body =
                    serde_json::json!({ "ref": reference, "device": device }).to_string();
                let intent = crate::activation::cancellable_network_request(
                    &worker_status.shutdown,
                    "vocalcode-checkout-intent-request",
                    move || send_checkout_intent_request(intent_body),
                );
                let accepted = match intent {
                    Ok(crate::activation::CancellableRequest::Completed(accepted)) => accepted,
                    Ok(crate::activation::CancellableRequest::Cancelled) => return,
                    Err(_) => false,
                };
                if !accepted {
                    if worker_status.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    *worker_status.activation.lock().unwrap() =
                        Some((false, CHECKOUT_REGISTRATION_ERROR.to_string()));
                    return;
                }
                if worker_status.shutdown.load(Ordering::Acquire) {
                    return;
                }
                // The buy page forwards this to the checkout call, which sets it
                // as the session's client_reference_id -- the field fulfilment
                // reads to find the intent this installation just bound.
                let url = format!("{BUY_URL}?ref={reference}");
                if !open_url(&url) {
                    *worker_status.activation.lock().unwrap() = Some((
                        false,
                        "Checkout was registered, but the browser could not be opened.".to_string(),
                    ));
                    return;
                }
                let deadline = Instant::now() + Duration::from_secs(6 * 60);
                while let Some(sleep_for) = purchase_poll_sleep(Instant::now(), deadline) {
                    let sleep_deadline = Instant::now() + sleep_for;
                    while Instant::now() < sleep_deadline {
                        if worker_status.shutdown.load(Ordering::Acquire) {
                            return;
                        }
                        std::thread::sleep(
                            sleep_deadline
                                .saturating_duration_since(Instant::now())
                                .min(Duration::from_millis(100)),
                        );
                    }
                    if worker_status.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    let Some(request_timeout) =
                        purchase_poll_request_timeout(Instant::now(), deadline)
                    else {
                        break;
                    };
                    let body =
                        serde_json::json!({ "ref": reference, "device": device }).to_string();
                    let response = crate::activation::cancellable_network_request(
                        &worker_status.shutdown,
                        "vocalcode-checkout-poll-request",
                        move || send_purchase_poll_request(body, request_timeout),
                    );
                    let reply = match response {
                        Ok(crate::activation::CancellableRequest::Completed(reply)) => reply,
                        Ok(crate::activation::CancellableRequest::Cancelled) => return,
                        Err(_) => continue,
                    };
                    let receipt = match reply {
                        PurchasePollReply::Retry => continue,
                        PurchasePollReply::Rejected => {
                            if worker_status.shutdown.load(Ordering::Acquire) {
                                return;
                            }
                            *worker_status.activation.lock().unwrap() = Some((
                                false,
                                "Activation failed: purchase activation was rejected".to_string(),
                            ));
                            return;
                        }
                        PurchasePollReply::Receipt(receipt) => receipt,
                    };
                    if worker_status.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    let (ok, message) = match crate::activation::install_receipt_cancellable(
                        &receipt,
                        &base,
                        &worker_status.shutdown,
                    ) {
                        Ok(Some(())) => {
                            if worker_status.shutdown.load(Ordering::Acquire) {
                                return;
                            }
                            let (text, state) = refresh();
                            let allowed = matches!(state.0.as_str(), "licensed" | "trial");
                            worker_status.inject_gate.store(true, Ordering::Release);
                            worker_status.pro_gate.store(allowed, Ordering::Release);
                            *worker_status.license.lock().unwrap() = text;
                            *worker_status.license_state.lock().unwrap() = state;
                            if allowed {
                                (
                                    true,
                                    "Activated — you're all set. No restart needed.".to_string(),
                                )
                            } else {
                                (
                                false,
                                "The receipt was saved, but it does not authorize this version."
                                    .to_string(),
                            )
                            }
                        }
                        Ok(None) => return,
                        Err(e) => (false, format!("Activation failed: {e}")),
                    };
                    if worker_status.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    *worker_status.activation.lock().unwrap() = Some((ok, message));
                    return;
                }
                if worker_status.shutdown.load(Ordering::Acquire) {
                    return;
                }
                *worker_status.activation.lock().unwrap() = Some((
                    false,
                    "Checkout is still pending. Use Restore after payment completes.".to_string(),
                ));
            });
            if let Err(error) = started {
                status.purchase_polling.store(false, Ordering::Release);
                *status.activation.lock().unwrap() =
                    Some((false, format!("Checkout could not start: {error}")));
            }
        }
        // Re-check on demand in addition to startup and six-hour maintenance:
        // somebody just told a fix exists should not have to wait for the next
        // scheduled pass.
        Some("checkupdate") => {
            let worker_status = status.clone();
            match spawn_update_check(status, "vocalcode-update-check", move || {
                crate::check_for_update(&worker_status)
            }) {
                Ok(UpdateCheckStart::Started) => {}
                Ok(UpdateCheckStart::Busy) => {
                    log::info!("manual update check is already in progress")
                }
                Err(error) => log::warn!("update check could not start: {error}"),
            }
        }
        Some("open_info") => match v
            .get("target")
            .and_then(Value::as_str)
            .and_then(InfoTarget::parse)
        {
            Some(InfoTarget::Privacy) => {
                open_url("https://vocalcode.app/privacy/");
            }
            Some(InfoTarget::Support) => {
                open_support_email();
            }
            None => log::warn!("ipc: rejected missing or unknown info target"),
        },
        Some("reveal_data") => reveal_in_file_manager(&crate::app_dir()),
        Some("purge_data") => {
            // Deletion is deliberately deferred to main. The engine owns ONNX
            // models/audio, Teach may own a complete clipboard snapshot, and the
            // logger lives under this directory; all must be stopped/joined first.
            if status.update_in_progress.load(Ordering::Acquire) {
                *status.update_result.lock().unwrap() = Some((
                    false,
                    "Wait for the update to finish before removing app data.".to_string(),
                ));
            } else {
                log::info!("data removal requested; shutting down runtime first");
                request_shutdown(status, proxy, ShutdownAction::PurgeData);
            }
        }
        Some("update") => {
            // One-click update: download the installer and run it silently, then
            // it closes this app, upgrades in place, and relaunches — no wizard,
            // no manual reinstall. Windows only; macOS falls back to a download.
            // Both URL and hash come from the host's validated manifest state.
            // The WebView only asks to install; it never chooses executable
            // bytes, even if page script is compromised.
            let Some(offer) = status.update.lock().ok().and_then(|u| u.clone()) else {
                *status.update_result.lock().unwrap() =
                    Some((false, "No verified update is available.".to_string()));
                return;
            };
            let crate::UpdateOffer {
                version,
                url,
                sha256: want,
                size,
                ..
            } = offer;
            if !crate::update_entitled(&crate::license_status(), &version) {
                *status.update.lock().unwrap() = None;
                *status.update_result.lock().unwrap() = Some((
                    false,
                    "This release is outside your licence entitlement; no update was installed."
                        .to_string(),
                ));
                return;
            }
            let worker_status = status.clone();
            let update_proxy = proxy.clone();
            match spawn_update_worker(status, move || {
                self_update(&version, &url, &want, size, &worker_status, &update_proxy);
            }) {
                Ok(UpdateInstallStart::Started) => {}
                Ok(UpdateInstallStart::Busy) => {
                    log::debug!("update: install already in progress")
                }
                Err(error) => {
                    *status.update_result.lock().unwrap() =
                        Some((false, format!("Update could not start: {error}")));
                }
            }
        }
        // macOS permission banner: jump straight to the pane that needs a switch
        // flipped. A denied grant never prompts again, so this is the only route.
        Some("permission") => {
            #[cfg(target_os = "macos")]
            {
                use crate::macos::{open_settings, request_or_open_microphone, Pane};
                match v.get("pane").and_then(|x| x.as_str()) {
                    Some("accessibility") => open_settings(Pane::Accessibility),
                    Some("input_monitoring") => open_settings(Pane::InputMonitoring),
                    Some("microphone") => request_or_open_microphone(),
                    other => log::warn!("ipc: unknown permission pane {other:?}"),
                }
            }
        }
        Some("drag") => {
            let _ = proxy.send_event(UserEvent::Drag);
        }
        Some("minimize") => {
            let _ = proxy.send_event(UserEvent::Minimize);
        }
        Some("close") => {
            let _ = proxy.send_event(UserEvent::HideToTray);
        }
        Some("teach_popup_open") => {
            let _ = proxy.send_event(UserEvent::TeachPopupOpen);
        }
        Some("teach_popup_close") => {
            let _ = proxy.send_event(UserEvent::TeachPopupClose);
        }
        Some("correction_popup_close") => {
            let _ = proxy.send_event(UserEvent::CorrectionReviewClose);
        }
        Some("zoom") => {
            let _ = proxy.send_event(UserEvent::Zoom);
        }
        other => log::warn!("ipc: unknown message type {other:?}"),
    }
}

/// Merge the page's config object into the stored config and write vocalcode.toml.
fn apply_save(cfg: &Mutex<Config>, base: &Path, v: &Value) -> Result<Config, String> {
    apply_save_with(
        cfg,
        v,
        |old, next| crate::persist_config_if_current(&base.join("vocalcode.toml"), old, next),
        set_autostart,
        autostart_enabled,
    )
}

fn apply_save_with<P, A, O>(
    cfg: &Mutex<Config>,
    v: &Value,
    persist: P,
    mut apply_autostart: A,
    observe_autostart: O,
) -> Result<Config, String>
where
    P: FnOnce(&Config, &Config) -> Result<(), String>,
    A: FnMut(bool) -> Result<(), String>,
    O: Fn() -> bool,
{
    // The caller owns ConfigApplyCoordinator, so this snapshot cannot be
    // superseded while its durable/OS transaction is in progress. This work
    // runs on the dedicated FIFO worker, never the WebView event thread.
    let mut c = cfg.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let old = c.clone();
    let mut next = old.clone();
    if let Some(list) = trigger_list(v.get("talk"))? {
        // Refuse to leave talk unbound — with no binding at all the app is
        // silently inert, which is indistinguishable from it being broken.
        if !list.is_empty() {
            next.talk = list;
        }
    }
    if let Some(list) = trigger_list(v.get("send"))? {
        next.send = list;
    }
    if let Some(list) = trigger_list(v.get("teach"))? {
        next.teach = list;
    }
    let language_changed = if let Some(l) = v.get("language").and_then(|x| x.as_str()) {
        let changed = next.language != l;
        next.language = l.to_string();
        if changed {
            // A visible language selection must win over a stale manual or
            // legacy model override. Otherwise the page can display Français
            // while an explicit Paraformer id silently keeps the Chinese
            // pipeline active. The page also submits model=""; this branch
            // protects the partial setlang IPC used during onboarding.
            next.model.clear();
        }
        changed
    } else {
        false
    };
    if let Some(m) = v.get("model").and_then(|x| x.as_str()) {
        // A language change may carry the hardware recommendation selected by
        // the visible model picker. Accept only models deliberately exposed
        // for that language; this still prevents a stale Chinese override from
        // silently surviving a switch to French.
        if !language_changed
            || m.is_empty()
            || crate::models::selectable_models(&next.language).contains(&m)
        {
            next.model = m.to_string();
        }
    }
    if let Some(b) = v.get("live_caption").and_then(|x| x.as_bool()) {
        next.live_caption = b;
    }
    if let Some(b) = v.get("noise_filter").and_then(Value::as_bool) {
        next.noise_filter = b;
    }
    if let Some(b) = v.get("cue_sounds").and_then(|x| x.as_bool()) {
        next.cue_sounds = b;
    }
    if let Some(b) = v.get("paste_insert").and_then(|x| x.as_bool()) {
        next.paste_insert = b;
    }
    if let Some(n) = v.get("correction_window_ms").and_then(|x| x.as_u64()) {
        next.correction_window_ms = if n == 0 {
            0
        } else {
            n.clamp(2_000, 30_000) as u32
        };
    }
    if let Some(n) = v.get("min_record_ms").and_then(|x| x.as_u64()) {
        next.min_record_ms = n.clamp(50, 5000) as u32;
    }
    if let Some(d) = v.get("input_device").and_then(|x| x.as_str()) {
        next.input_device = if d.is_empty() {
            None
        } else {
            Some(d.to_string())
        };
    }
    if let Some(l) = v.get("ui_lang").and_then(|x| x.as_str()) {
        next.ui_lang = l.to_string();
    }
    if let Some(s) = v.get("overlay_style").and_then(|x| x.as_str()) {
        next.overlay_style = s.to_string();
    }
    if let Some(value) = v.get("desktop_control").and_then(Value::as_bool) {
        next.desktop_control = value;
    }
    if let Some(value) = v.get("desktop_control_edge").and_then(Value::as_str) {
        if !matches!(value, "bottom" | "left" | "right") {
            return Err("unknown desktop-control edge".into());
        }
        next.desktop_control_edge = value.to_string();
    }
    if let Some(m) = v.get("talk_mode").and_then(|x| x.as_str()) {
        next.talk_mode = m.to_string();
    }
    if let Some(b) = v.get("double_tap_lock").and_then(Value::as_bool) {
        next.double_tap_lock = b;
    }
    if let Some(b) = v.get("mute_while_dictating").and_then(Value::as_bool) {
        next.mute_while_dictating = b;
    }
    if let Some(writing) = v.get("writing") {
        next.writing = serde_json::from_value(writing.clone())
            .map_err(|_| "writing settings are malformed".to_string())?;
    }
    if let Some(onboarded) = v.get("onboarded").and_then(Value::as_bool) {
        next.onboarded = onboarded;
    }
    if let Some(b) = v.get("autostart").and_then(|x| x.as_bool()) {
        next.autostart = b;
    }
    if let Some(b) = v.get("smart_meeting_reminders").and_then(Value::as_bool) {
        next.smart_meeting_reminders = b;
    }
    if let Some(apps) = v.get("ignored_meeting_apps") {
        let apps = apps
            .as_array()
            .ok_or_else(|| "ignored meeting applications must be a list".to_string())?;
        if apps.len() > MAX_IGNORED_MEETING_APPS {
            return Err(format!(
                "at most {MAX_IGNORED_MEETING_APPS} meeting applications can be ignored"
            ));
        }
        let mut normalized = Vec::with_capacity(apps.len());
        for app in apps {
            let app = app
                .as_str()
                .ok_or_else(|| "ignored meeting application identity is invalid".to_string())?
                .trim()
                .to_ascii_lowercase();
            if app.len() > MAX_MEETING_APP_KEY_UTF8_BYTES {
                return Err("ignored meeting application identity is too long".to_string());
            }
            if !normalized.contains(&app) {
                normalized.push(app);
            }
        }
        next.ignored_meeting_apps = normalized;
    }
    // Bound list cardinality and every string-bearing selector before the
    // cross-role overlap checks below. Those checks are intentionally
    // pairwise, so an untrusted giant array must never reach them.
    next.validate_bounds()?;
    let overlaps = |a: &[Trigger], b: &[Trigger]| {
        a.iter().any(|left| {
            b.iter()
                .any(|right| vocalcode_core::config::triggers_overlap(left, right))
        })
    };
    if overlaps(&next.talk, &next.send)
        || overlaps(&next.talk, &next.teach)
        || overlaps(&next.send, &next.teach)
    {
        return Err("one physical control cannot be assigned to more than one action".to_string());
    }
    if !matches!(next.talk_mode.as_str(), "hold" | "toggle") {
        return Err("unknown talk-key behaviour".to_string());
    }
    if !matches!(next.overlay_style.as_str(), "classic" | "mini" | "off") {
        return Err("unknown recording-indicator style".to_string());
    }
    if crate::models::route_for(&next.model, &next.language).is_none() {
        return Err("that language/model combination is not supported".to_string());
    }

    // Do not probe the microphone here. CPAL device creation can block in a
    // driver, and the engine already stages exactly one capture before its
    // generation-checked atomic commit. A failed stage rolls this persisted
    // desired snapshot back and supplies the authoritative result to the page.
    if next.autostart != old.autostart {
        if let Err(error) = apply_autostart(next.autostart) {
            // A platform operation can fail after partially changing registry,
            // plist, or launchd state. The OS is authoritative for this field;
            // never return a failure with a control that claims the old value.
            c.autostart = observe_autostart();
            return Err(format!(
                "could not change launch at login: {error}; the control was reconciled to the operating-system state ({})",
                if c.autostart { "enabled" } else { "disabled" }
            ));
        }
    }
    if let Err(error) = persist(&old, &next) {
        if next.autostart != old.autostart {
            if let Err(rollback_error) = apply_autostart(old.autostart) {
                c.autostart = observe_autostart();
                return Err(format!(
                    "{error}; restoring the previous launch-at-login state also failed: {rollback_error}; the control was reconciled to the operating-system state ({})",
                    if c.autostart { "enabled" } else { "disabled" }
                ));
            }
        }
        return Err(error);
    }
    *c = next.clone();
    // Runtime-visible fields are committed together by the engine thread only
    // after every fallible resource (microphone/model) has been prepared. This
    // handler deliberately publishes just the durable desired snapshot; doing
    // trigger/toggle/model side effects here made a later model failure leave a
    // half-old, half-new process even though the UI rolled the save back.
    Ok(next)
}

/// Build the JSON the page's `vocalcodeInit` expects from the current config.
fn input_device_for_init(
    configured: Option<&str>,
    devices: &[vocalcode_platform::InputDeviceChoice],
) -> String {
    let Some(configured) = configured.filter(|value| !value.is_empty()) else {
        return String::new();
    };

    // Current configs already contain the stable selector. Old releases saved
    // only a display name; migrate that value only when it identifies exactly
    // one enumerated device. Duplicate names deliberately remain unavailable
    // until the user picks the exact microphone again.
    if devices.iter().any(|choice| choice.selector == configured) {
        return configured.to_string();
    }
    let mut legacy_matches = devices
        .iter()
        .filter(|choice| choice.legacy_name == configured);
    let Some(choice) = legacy_matches.next() else {
        return configured.to_string();
    };
    if legacy_matches.next().is_some() {
        configured.to_string()
    } else {
        choice.selector.clone()
    }
}

fn init_config_json(
    c: &Config,
    devices: &[vocalcode_platform::InputDeviceChoice],
    dictionary: Option<&crate::RulesDocument>,
    dictionary_error: Option<&str>,
) -> String {
    let hardware = vocalcode_platform::HardwareProfile::detect();
    let hardware_class = match vocalcode_platform::performance_class(hardware) {
        vocalcode_platform::PerformanceClass::Compact => "compact",
        vocalcode_platform::PerformanceClass::Standard => "standard",
        vocalcode_platform::PerformanceClass::Performance => "performance",
    };
    let input_device = input_device_for_init(c.input_device.as_deref(), devices);
    let device_options = devices
        .iter()
        .map(|choice| {
            serde_json::json!({
                "selector": choice.selector,
                "label": choice.label,
                "legacy_name": choice.legacy_name,
            })
        })
        .collect::<Vec<_>>();
    let mut initial = serde_json::json!({
        "talk": c.talk.iter().map(trigger_to_code).collect::<Vec<_>>(),
        "send": c.send.iter().map(trigger_to_code).collect::<Vec<_>>(),
        "teach": c.teach.iter().map(trigger_to_code).collect::<Vec<_>>(),
        "language": c.language,
        "onboarded": c.onboarded,
        "model": c.model,
        "live_caption": c.live_caption,
        "noise_filter": c.noise_filter,
        "overlay_style": c.overlay_style,
        "talk_mode": c.talk_mode,
        "paste_insert": c.paste_insert,
        "correction_window_ms": c.correction_window_ms,
        "cue_sounds": c.cue_sounds,
        "min_record_ms": c.min_record_ms,
        "input_device": input_device,
        "devices": device_options,
        "autostart": c.autostart,
        "smart_meeting_reminders": c.smart_meeting_reminders,
        "ignored_meeting_apps": c.ignored_meeting_apps,
        "system": system_summary(hardware),
        "hardware": {
            "class": hardware_class,
            "memory_gib": hardware.memory_mib.map(|mib| (mib as f64 / 1024.0 * 10.0).round() / 10.0),
            "inference_cores": hardware.inference_cores,
            "recommendations": {
                "zh": crate::models::recommended_model("zh", hardware),
                "hi": crate::models::recommended_model("hi", hardware),
                "en": crate::models::recommended_model("en", hardware),
            },
        },
        // The page had no way to tell which machine it was on, so it rendered
        // Mac keycaps and a Finder button to Windows users. Named `os` rather
        // than parsed out of `system`, which is a human-readable summary.
        "os": std::env::consts::OS,
        "version": env!("CARGO_PKG_VERSION"),
        // Measured once, at window open. Walking the models directory on every
        // status push would be a directory scan several times a second for a
        // number that changes twice in the app's life.
        "data_mb": dir_size_mb(&crate::app_dir()),
        "ui_lang": c.ui_lang,
        // VOCALCODE_PANEL=<name> opens that settings page at launch. The
        // WebView does not accept synthetic clicks, so without this a
        // screenshot of any page but the first cannot be taken unattended.
        "panel": std::env::var("VOCALCODE_PANEL").unwrap_or_default(),
        "dict": dictionary.map(|document| document.rules.iter()
            .map(|(from, to)| serde_json::json!([from, to]))
            .collect::<Vec<_>>()).unwrap_or_default(),
        "dict_revision": dictionary.map(|document| document.revision.as_str()),
        "dict_error": dictionary_error,
        "limits": {
            "dictionary_rules": MAX_DICTIONARY_RULES,
            "dictionary_side_utf8_bytes": MAX_DICTIONARY_SIDE_UTF8_BYTES,
            "dictionary_document_bytes": MAX_DICTIONARY_DOCUMENT_BYTES,
            "triggers_per_action": MAX_TRIGGERS_PER_ACTION,
            "input_device_utf8_bytes": MAX_INPUT_DEVICE_UTF8_BYTES,
            "config_token_utf8_bytes": MAX_CONFIG_TOKEN_UTF8_BYTES,
            "ui_language_utf8_bytes": MAX_UI_LANGUAGE_UTF8_BYTES,
            "ipc_utf8_bytes": MAX_SETTINGS_IPC_BYTES,
            "ignored_meeting_apps": MAX_IGNORED_MEETING_APPS,
            "meeting_app_key_utf8_bytes": MAX_MEETING_APP_KEY_UTF8_BYTES,
        },
    });
    initial["desktop_control"] = serde_json::json!(c.desktop_control);
    initial["desktop_control_edge"] = serde_json::json!(c.desktop_control_edge);
    initial["desktop_control_available"] = serde_json::json!(cfg!(windows));
    initial["double_tap_lock"] = serde_json::json!(c.double_tap_lock);
    initial["mute_while_dictating"] = serde_json::json!(c.mute_while_dictating);
    initial["mute_available"] = serde_json::json!(cfg!(windows));
    initial["writing"] = serde_json::json!(c.writing);
    initial.to_string()
}

/// Total size of a directory tree, in whole megabytes. Errors count as zero:
/// this is a figure shown next to an uninstall button, and refusing to render
/// the section because one file could not be stat'd would hide the button that
/// is the point of it.
fn dir_size_mb(dir: &std::path::Path) -> u64 {
    fn walk(dir: &std::path::Path) -> u64 {
        // `FileType::is_symlink` does not cover Windows junctions and all
        // reparse tags. Never recurse through one while calculating an
        // informational size: it could point outside app data, loop, or make
        // this otherwise harmless status render traverse an unrelated tree.
        #[cfg(windows)]
        if crate::paths::is_reparse_point(dir) {
            return 0;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .filter_map(Result::ok)
            .map(|e| match e.file_type() {
                // Symlinks are not followed: a link into the models directory
                // would otherwise be counted twice, and one pointing outside it
                // would report someone else's disk usage as ours.
                Ok(t) if t.is_dir() => walk(&e.path()),
                Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => 0,
            })
            .sum()
    }
    walk(dir) / 1_000_000
}

/// The signed trial receipt and monotonic trusted-time anchor are retained by
/// name in [`purge_app_data`]. Deleting the trial receipt does not reset the
/// server-owned epoch, but preserving both files keeps reinstall fully offline
/// and prevents a supported cleanup action from discarding clock-rollover
/// evidence.
const TRIAL_FILE: &str = "vocalcode-trial.dat";
const TRUSTED_TIME_FILE: &str = crate::activation::TRUSTED_TIME_FILE;

fn is_preserved_license_file_name(name: &std::ffi::OsStr) -> bool {
    #[cfg(windows)]
    {
        let name = name.to_string_lossy();
        name.eq_ignore_ascii_case(TRIAL_FILE) || name.eq_ignore_ascii_case(TRUSTED_TIME_FILE)
    }
    #[cfg(not(windows))]
    {
        name == TRIAL_FILE || name == TRUSTED_TIME_FILE
    }
}

/// Disable the login item before touching any data. A successful purge followed
/// by a failed autostart removal would let the next login launch VocalCode and
/// recreate state the user had just asked us to remove. Keeping the sequencing
/// here generic makes that destructive contract testable without changing the
/// developer machine's real login settings.
fn run_uninstall_cleanup<D, P>(disable_autostart: D, purge: P) -> Result<(), String>
where
    D: FnOnce() -> Result<(), String>,
    P: FnOnce() -> Result<(), String>,
{
    disable_autostart().map_err(|error| format!("Could not disable launch at login: {error}"))?;
    purge()
}

fn purge_path_contains_executable(data: &std::path::Path, executable: &std::path::Path) -> bool {
    data == executable || executable.starts_with(data)
}

/// Remove everything VocalCode has written except the signed trial receipt and
/// trusted-time high-water mark. The Worker owns the immutable first-seen
/// epoch, so deleting a cache cannot grant a fresh trial; retaining these small
/// files additionally keeps reinstall offline and ensures an expired receipt
/// cannot be revived through the supported cleanup path plus clock rollback.
///
/// Every entry is attempted so one busy file never prevents the large model
/// directory from being removed. Success, however, means every non-trial entry
/// is actually gone; a privacy-facing removal action must not quietly report a
/// partial delete as complete.
fn purge_app_data(dir: &std::path::Path) -> Result<(), String> {
    // Defence in depth: writable state must never be the installation directory
    // again. Refuse before read_dir so a path regression cannot turn this button
    // back into a partial uninstaller.
    #[cfg(windows)]
    if crate::paths::is_reparse_point(dir) {
        return Err(format!(
            "refusing to remove reparse-point app data {}",
            dir.display()
        ));
    }
    #[cfg(not(windows))]
    if std::fs::symlink_metadata(dir)
        .map_err(|error| format!("could not inspect app-data root: {error}"))?
        .file_type()
        .is_symlink()
    {
        return Err(format!(
            "refusing to remove symlinked app data {}",
            dir.display()
        ));
    }

    let executable_dir = crate::paths::exe_dir();
    let canonical_data = std::fs::canonicalize(dir)
        .map_err(|error| format!("could not resolve app-data path: {error}"))?;
    let canonical_executable = std::fs::canonicalize(&executable_dir)
        .map_err(|error| format!("could not resolve application directory: {error}"))?;
    if purge_path_contains_executable(&canonical_data, &canonical_executable) {
        return Err(format!(
            "refusing to remove the application directory {}",
            dir.display()
        ));
    }
    let entries = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    let mut failures = Vec::new();
    for result in entries {
        let entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                failures.push(format!("could not enumerate an app-data entry: {error}"));
                continue;
            }
        };
        if is_preserved_license_file_name(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        // Never recurse through a Windows junction/reparse point. Rust's
        // `FileType::is_symlink` does not classify every reparse tag, and a
        // junction inside models could otherwise turn an app-data purge into a
        // recursive delete of an unrelated directory.
        #[cfg(windows)]
        let removed = if crate::paths::is_reparse_point(&path) {
            let target_is_dir = std::fs::metadata(&path)
                .map(|metadata| metadata.is_dir())
                .unwrap_or(false);
            if target_is_dir {
                std::fs::remove_dir(&path)
            } else {
                std::fs::remove_file(&path).or_else(|_| std::fs::remove_dir(&path))
            }
        } else {
            match entry.file_type() {
                Ok(t) if t.is_dir() => std::fs::remove_dir_all(&path),
                _ => std::fs::remove_file(&path),
            }
        };
        #[cfg(not(windows))]
        let removed = match entry.file_type() {
            // `remove_file` unlinks a POSIX symlink without following it.
            Ok(t) if t.is_dir() => std::fs::remove_dir_all(&path),
            _ => std::fs::remove_file(&path),
        };
        if let Err(e) = removed {
            log::warn!("could not remove {}: {e}", path.display());
            failures.push(format!("{}: {e}", path.display()));
        }
    }
    let remaining = std::fs::read_dir(dir)
        .map_err(|error| format!("could not verify app-data removal: {error}"))?
        .filter_map(Result::ok)
        .filter(|entry| !is_preserved_license_file_name(&entry.file_name()))
        .map(|entry| entry.path().display().to_string())
        .collect::<Vec<_>>();
    if !remaining.is_empty() || !failures.is_empty() {
        return Err(format!(
            "some app data could not be removed: {}{}",
            remaining.join(", "),
            if failures.is_empty() {
                String::new()
            } else {
                format!(" ({})", failures.join("; "))
            }
        ));
    }
    Ok(())
}

/// Called by main only after engine/update/Teach/maintenance workers have been
/// stopped and joined. Keeping that ordering outside the IPC handler prevents a
/// future UI refactor from deleting live model or clipboard state again.
pub(crate) fn purge_user_data_after_shutdown_until(deadline: Instant) -> Result<(), String> {
    let _lifecycle = crate::paths::lock_data_lifecycle_exclusive_until(deadline)
        .map_err(|error| format!("could not obtain exclusive app-data lock: {error}"))?;
    let dir = crate::app_dir();
    log::info!("removing {} at the user's request", dir.display());
    run_uninstall_cleanup(|| set_autostart(false), || purge_app_data(&dir))
}

pub(crate) fn purge_user_data_after_shutdown() -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(USER_DATA_PURGE_LOCK_TIMEOUT)
        .unwrap_or_else(Instant::now);
    purge_user_data_after_shutdown_until(deadline)
}

/// Open a directory in the platform's file manager.
fn reveal_in_file_manager(dir: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let cmd = "/usr/bin/open";
    #[cfg(windows)]
    let cmd = match windows_executable("explorer.exe") {
        Ok(path) => path,
        Err(error) => {
            log::warn!("could not resolve Windows Explorer: {error}");
            return;
        }
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let cmd = "xdg-open";
    if let Err(e) = std::process::Command::new(cmd).arg(dir).spawn() {
        log::warn!("could not reveal {}: {e}", dir.display());
    }
}

/// Short human-readable hardware summary for the Language & model page, so the
/// user can see what was detected and why these (CPU-fast, local) models fit.
fn system_summary(profile: vocalcode_platform::HardwareProfile) -> String {
    let memory = profile
        .memory_mib
        .map(|mib| format!(" · {:.0} GB RAM", mib as f64 / 1024.0))
        .unwrap_or_default();
    let class = match vocalcode_platform::performance_class(profile) {
        vocalcode_platform::PerformanceClass::Compact => "compact",
        vocalcode_platform::PerformanceClass::Standard => "standard",
        vocalcode_platform::PerformanceClass::Performance => "performance",
    };
    if vocalcode_platform::has_gpu() {
        return format!(
            "NVIDIA GPU · {} cores{memory} · {class}",
            profile.logical_cores
        );
    }
    // On Apple Silicon the logical count is misleading here: an M1 Max reports
    // 10, but only the 8 performance cores are given to the ASR thread pool, so
    // that is the number explaining the model choice on this page.
    #[cfg(target_os = "macos")]
    {
        let perf = vocalcode_platform::inference_cores();
        let total = vocalcode_platform::logical_cores();
        if perf < total {
            return format!("Apple Silicon · {perf} performance cores{memory} · {class}");
        }
        format!("CPU · {total} cores{memory} · {class}")
    }
    #[cfg(not(target_os = "macos"))]
    format!("CPU · {} cores{memory} · {class}", profile.logical_cores)
}

/// Enable/disable "launch at login" via the per-user Run registry key.
#[cfg(windows)]
pub(crate) fn set_autostart(enabled: bool) -> Result<(), String> {
    if enabled {
        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|e| format!("current executable: {e}"))?;
        // A Run entry is a command line, not a path field.  Without quotes a
        // username containing a space is parsed as two different tokens.
        let command = format!("\"{exe}\"");
        write_windows_autostart_value(&command)?;
    } else {
        delete_windows_autostart_value()?;
    }

    // A community build must never remove the paid edition's startup shortcut.
    if crate::community::ENABLED {
        return Ok(());
    }
    // Migrate the shortcut written by installers through 0.4.19.  Otherwise
    // switching the setting off removes only the Run entry and VocalCode still
    // launches from Startup; switching it on creates two instances.
    match windows_startup_directory() {
        Ok(startup) => {
            let legacy = startup.join("VocalCode.lnk");
            if let Err(e) = std::fs::remove_file(&legacy) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    // When enabling, the desired state is already true and a
                    // duplicate legacy entry is noisy but not a false setting.
                    // When disabling, this shortcut still launches VocalCode;
                    // claiming success would make the control observably wrong.
                    if !enabled {
                        return Err(format!(
                            "remove legacy launch-at-login shortcut {}: {e}",
                            legacy.display()
                        ));
                    }
                    log::warn!("remove legacy startup shortcut {}: {e}", legacy.display());
                }
            }
        }
        Err(error) => {
            if !enabled {
                return Err(format!(
                    "could not verify removal of the legacy launch-at-login shortcut: {error}"
                ));
            }
            log::warn!("resolve legacy Startup directory: {error}");
        }
    }
    Ok(())
}

#[cfg(windows)]
fn windows_startup_directory() -> Result<PathBuf, String> {
    use std::ffi::{c_void, OsString};
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Startup, SHGetKnownFolderPath};

    let mut raw = std::ptr::null_mut();
    let result =
        unsafe { SHGetKnownFolderPath(&FOLDERID_Startup, 0, std::ptr::null_mut(), &mut raw) };
    if result < 0 || raw.is_null() {
        if !raw.is_null() {
            unsafe { CoTaskMemFree(raw.cast::<c_void>()) };
        }
        return Err(format!(
            "SHGetKnownFolderPath(Startup) failed with HRESULT {result:#010x}"
        ));
    }
    let mut len = 0_usize;
    unsafe {
        while *raw.add(len) != 0 {
            len += 1;
        }
    }
    let startup = unsafe { OsString::from_wide(std::slice::from_raw_parts(raw, len)) };
    unsafe { CoTaskMemFree(raw.cast::<c_void>()) };
    let startup = PathBuf::from(startup);
    if startup.is_absolute() {
        Ok(startup)
    } else {
        Err("Windows returned a non-absolute Startup directory".to_string())
    }
}

#[cfg(windows)]
fn write_windows_autostart_value(command: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    struct OwnedRegistryKey(HKEY);
    impl Drop for OwnedRegistryKey {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }

    let subkey = std::ffi::OsStr::new(r"Software\Microsoft\Windows\CurrentVersion\Run")
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let value_name = std::ffi::OsStr::new(crate::community::AUTOSTART_NAME)
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let value = std::ffi::OsStr::new(command)
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let byte_count = value
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or_else(|| "launch-at-login command is too large".to_string())?;
    let mut raw_key = std::ptr::null_mut();
    let mut disposition = 0_u32;
    let created = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut raw_key,
            &mut disposition,
        )
    };
    if created != 0 {
        return Err(format!(
            "open launch-at-login registry key: {}",
            std::io::Error::from_raw_os_error(created as i32)
        ));
    }
    let key = OwnedRegistryKey(raw_key);
    let written = unsafe {
        RegSetValueExW(
            key.0,
            value_name.as_ptr(),
            0,
            REG_SZ,
            value.as_ptr().cast::<u8>(),
            byte_count,
        )
    };
    if written == 0 {
        Ok(())
    } else {
        Err(format!(
            "write launch-at-login entry: {}",
            std::io::Error::from_raw_os_error(written as i32)
        ))
    }
}

#[cfg(windows)]
fn delete_windows_autostart_value() -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
    use windows_sys::Win32::System::Registry::{RegDeleteKeyValueW, HKEY_CURRENT_USER};

    let subkey = std::ffi::OsStr::new(r"Software\Microsoft\Windows\CurrentVersion\Run")
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let value = std::ffi::OsStr::new(crate::community::AUTOSTART_NAME)
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, subkey.as_ptr(), value.as_ptr()) };
    if result == 0 || result == ERROR_FILE_NOT_FOUND || result == ERROR_PATH_NOT_FOUND {
        Ok(())
    } else {
        Err(format!(
            "remove launch-at-login entry: {}",
            std::io::Error::from_raw_os_error(result as i32)
        ))
    }
}

#[cfg(windows)]
fn query_autostart_command() -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RRF_ZEROONFAILURE,
    };

    let subkey = std::ffi::OsStr::new(r"Software\Microsoft\Windows\CurrentVersion\Run")
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let value_name = std::ffi::OsStr::new(crate::community::AUTOSTART_NAME)
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // A Windows process command line is capped well below this. A larger or
    // malformed registry value is not a valid setting for this app.
    let mut value = vec![0_u16; 32 * 1024];
    let mut byte_count = u32::try_from(value.len() * std::mem::size_of::<u16>()).ok()?;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value_name.as_ptr(),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_ZEROONFAILURE,
            std::ptr::null_mut(),
            value.as_mut_ptr().cast(),
            &mut byte_count,
        )
    };
    if result == ERROR_FILE_NOT_FOUND || result == ERROR_PATH_NOT_FOUND {
        return None;
    }
    if result != 0 {
        log::warn!(
            "read launch-at-login entry: {}",
            std::io::Error::from_raw_os_error(result as i32)
        );
        return None;
    }
    if byte_count == 0 || byte_count % 2 != 0 {
        return None;
    }
    let units = usize::try_from(byte_count / 2).ok()?;
    if units > value.len() {
        return None;
    }
    value.truncate(units);
    while value.last() == Some(&0) {
        value.pop();
    }
    String::from_utf16(&value)
        .ok()
        .filter(|command| !command.trim().is_empty())
}

#[cfg(windows)]
pub(crate) fn autostart_enabled() -> bool {
    let run_matches = std::env::current_exe()
        .ok()
        .zip(query_autostart_command())
        .is_some_and(|(exe, command)| {
            let configured = command
                .trim()
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(command.trim());
            // A stale Run value pointing at a previous install directory is
            // not an enabled setting for this copy of VocalCode.
            configured.eq_ignore_ascii_case(&exe.to_string_lossy())
        });
    if crate::community::ENABLED {
        return run_matches;
    }
    let legacy_exists = match windows_startup_directory() {
        Ok(startup) => startup.join("VocalCode.lnk").is_file(),
        Err(error) => {
            // Do not claim disabled when the legacy mechanism could not be
            // inspected. The user can retry the setting and receive the
            // actionable error from set_autostart(false).
            log::warn!("inspect legacy Startup directory: {error}");
            true
        }
    };
    run_matches || legacy_exists
}

#[cfg(target_os = "macos")]
fn macos_home_directory() -> Result<PathBuf, String> {
    let home = PathBuf::from(objc2_foundation::NSHomeDirectory().to_string());
    if home.is_absolute() {
        Ok(home)
    } else {
        Err("macOS returned a non-absolute home directory".to_string())
    }
}

/// Enable/disable "launch at login" via a per-user LaunchAgent.
///
/// The macOS equivalent of the Run key: a plist in `~/Library/LaunchAgents`
/// that launchd runs at login. Written unloaded/loaded with `launchctl` so the
/// change takes effect without a logout.
#[cfg(target_os = "macos")]
pub(crate) fn set_autostart(enabled: bool) -> Result<(), String> {
    const LABEL: &str = crate::community::BUNDLE_ID;

    let home = macos_home_directory()?;
    let dir = home.join("Library").join("LaunchAgents");
    let plist = dir.join(format!("{LABEL}.plist"));
    let uid = unsafe { libc_getuid() };
    let domain = format!("gui/{uid}");
    let service = format!("{domain}/{LABEL}");
    let was_loaded = launch_agent_loaded(&service);
    let previous = match read_bounded_local_file(&plist, MAC_AUTOSTART_PLIST_MAX_BYTES) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("read existing LaunchAgent: {error}")),
    };
    // Resolve everything that can fail without changing launchd first. Once an
    // existing agent is booted out, every later error path must restore it.
    let exe = if enabled {
        Some(std::env::current_exe().map_err(|e| format!("current executable: {e}"))?)
    } else {
        None
    };
    if enabled {
        std::fs::create_dir_all(&dir).map_err(|e| format!("create LaunchAgents: {e}"))?;
    }

    if was_loaded {
        launchctl_checked(["bootout", service.as_str()], "unload LaunchAgent")?;
    }

    if !enabled {
        if let Err(e) = std::fs::remove_file(&plist) {
            if e.kind() != std::io::ErrorKind::NotFound {
                if was_loaded {
                    if let Err(rollback) = launchctl_bootstrap(&domain, &plist) {
                        return Err(format!(
                            "remove LaunchAgent: {e}; restoring launchd state also failed: {rollback}"
                        ));
                    }
                }
                return Err(format!("remove LaunchAgent: {e}"));
            }
        }
        return Ok(());
    }

    let exe = exe.expect("enabled LaunchAgent has a resolved executable");
    let contents = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#,
        xml_escape(&exe.to_string_lossy())
    );
    if let Err(e) = crate::storage::atomic_write(&plist, contents) {
        if was_loaded {
            if let Err(rollback) = launchctl_bootstrap(&domain, &plist) {
                return Err(format!(
                    "write LaunchAgent atomically: {e}; restoring launchd state also failed: {rollback}"
                ));
            }
        }
        return Err(format!("write LaunchAgent atomically: {e}"));
    }
    if let Err(error) = launchctl_bootstrap(&domain, &plist) {
        // File state and launchd state form one setting. Put both back if the
        // new agent cannot be loaded, rather than persisting an enabled plist
        // while reporting failure to the settings transaction.
        if let Err(rollback) =
            rollback_launch_agent(&domain, &plist, previous.as_deref(), was_loaded)
        {
            return Err(format!(
                "{error}; restoring the previous LaunchAgent also failed: {rollback}"
            ));
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn autostart_enabled() -> bool {
    let Ok(home) = macos_home_directory() else {
        return false;
    };
    let plist = home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{}.plist", crate::community::BUNDLE_ID));
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mut command = std::process::Command::new("/usr/bin/plutil");
    command
        .args(["-extract", "ProgramArguments.0", "raw", "-o", "-"])
        .arg(&plist);
    let Ok(output) = bounded_command_output(&mut command, MAC_METADATA_COMMAND_TIMEOUT, None)
    else {
        return false;
    };
    output.status.success()
        && String::from_utf8_lossy(&output.stdout).trim() == exe.to_string_lossy().as_ref()
}

#[cfg(target_os = "macos")]
fn launch_agent_loaded(service: &str) -> bool {
    let mut command = std::process::Command::new("/bin/launchctl");
    command.args(["print", service]);
    bounded_command_output(&mut command, MAC_METADATA_COMMAND_TIMEOUT, None)
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn launchctl_checked<const N: usize>(args: [&str; N], action: &str) -> Result<(), String> {
    let mut command = std::process::Command::new("/bin/launchctl");
    command.args(args);
    let output = bounded_command_output(&mut command, MAC_METADATA_COMMAND_TIMEOUT, None)
        .map_err(|e| format!("{action}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&output.stderr);
        Err(format!("{action}: {}", detail.trim()))
    }
}

#[cfg(target_os = "macos")]
fn launchctl_bootstrap(domain: &str, plist: &Path) -> Result<(), String> {
    let plist = plist.to_string_lossy();
    launchctl_checked(["bootstrap", domain, plist.as_ref()], "load LaunchAgent")
}

#[cfg(target_os = "macos")]
fn rollback_launch_agent(
    domain: &str,
    plist: &Path,
    previous: Option<&[u8]>,
    was_loaded: bool,
) -> Result<(), String> {
    match previous {
        Some(bytes) => crate::storage::atomic_write(plist, bytes)
            .map_err(|e| format!("restore LaunchAgent file: {e}"))?,
        None => match std::fs::remove_file(plist) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("remove replacement LaunchAgent: {e}")),
        },
    }
    if was_loaded {
        if previous.is_none() {
            return Err("the previously loaded LaunchAgent had no file to restore".to_string());
        }
        launchctl_bootstrap(domain, plist)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The current user id, for the `gui/<uid>` launchd domain.
#[cfg(target_os = "macos")]
unsafe fn libc_getuid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    getuid()
}

#[cfg(all(not(windows), not(target_os = "macos")))]
pub(crate) fn set_autostart(_enabled: bool) -> Result<(), String> {
    Ok(())
}

#[cfg(all(not(windows), not(target_os = "macos")))]
pub(crate) fn autostart_enabled() -> bool {
    false
}

fn trigger_to_code(t: &Trigger) -> String {
    match t {
        Trigger::MouseButton(MouseExtra::X2) => "mouse_x2".to_string(),
        Trigger::MouseButton(MouseExtra::X1) => "mouse_x1".to_string(),
        Trigger::Key(k) => format!(
            "key:{}",
            vocalcode_core::config::canonical_key_name(k).unwrap_or(k.as_str())
        ),
        // JSON keeps the full device selector and remains forward compatible
        // when a trigger grows another optional field.  The prefix prevents it
        // being confused with legacy key codes.
        other => format!(
            "trigger:{}",
            serde_json::to_string(other).expect("Trigger is serializable")
        ),
    }
}

/// Read a binding list sent by the page.
///
/// Accepts an array of codes, and also a bare string so an older page (or a
/// hand-crafted IPC message) still works. `"off"` and an empty array both mean
/// "no binding"; unrecognised codes are dropped rather than aborting the save.
fn trigger_list(v: Option<&Value>) -> Result<Option<Vec<Trigger>>, String> {
    let Some(v) = v else {
        return Ok(None);
    };
    if let Some(s) = v.as_str() {
        if s.len() > MAX_SERIALIZED_TRIGGER_UTF8_BYTES {
            return Err(format!(
                "trigger code exceeds the {MAX_SERIALIZED_TRIGGER_UTF8_BYTES}-byte safety limit"
            ));
        }
        return Ok(Some(match s {
            "off" => Vec::new(),
            other => code_to_trigger(other)?.into_iter().collect(),
        }));
    }
    let Some(values) = v.as_array() else {
        return Ok(None);
    };
    if values.len() > MAX_TRIGGERS_PER_ACTION {
        return Err(format!(
            "a trigger action has {} entries; at most {MAX_TRIGGERS_PER_ACTION} are allowed",
            values.len()
        ));
    }
    let mut triggers = Vec::with_capacity(values.len());
    for value in values {
        let Some(code) = value.as_str() else {
            continue;
        };
        if code == "off" {
            continue;
        }
        if code.len() > MAX_SERIALIZED_TRIGGER_UTF8_BYTES {
            return Err(format!(
                "trigger code exceeds the {MAX_SERIALIZED_TRIGGER_UTF8_BYTES}-byte safety limit"
            ));
        }
        if let Some(trigger) = code_to_trigger(code)? {
            triggers.push(trigger);
        }
    }
    Ok(Some(triggers))
}

fn code_to_trigger(code: &str) -> Result<Option<Trigger>, String> {
    if let Some(json) = code.strip_prefix("trigger:") {
        let Some(trigger) = serde_json::from_str::<Trigger>(json).ok() else {
            return Ok(None);
        };
        validate_trigger_bounds(&trigger)?;
        return Ok(Some(trigger));
    }
    let trigger = match code {
        "mouse_x2" => Some(Trigger::MouseButton(MouseExtra::X2)),
        "mouse_x1" => Some(Trigger::MouseButton(MouseExtra::X1)),
        s => s.strip_prefix("key:").map(|k| {
            Trigger::Key(
                vocalcode_core::config::canonical_key_name(k)
                    .unwrap_or(k)
                    .to_string(),
            )
        }),
    };
    if let Some(trigger) = trigger.as_ref() {
        validate_trigger_bounds(trigger)?;
    }
    Ok(trigger)
}

#[cfg(test)]
mod trigger_code_tests {
    use super::*;
    use vocalcode_core::config::{DeviceSelector, GamepadButton};

    #[test]
    fn external_trigger_codes_keep_the_device_selector() {
        let trigger = Trigger::GamepadButton {
            device: DeviceSelector {
                stable_id: Some("hid-hash".into()),
                vendor_id: Some(0x045e),
                product_id: Some(0x0b13),
                serial: Some("controller-1".into()),
            },
            button: GamepadButton::South,
        };
        let code = trigger_to_code(&trigger);
        assert!(code.starts_with("trigger:"));
        assert_eq!(code_to_trigger(&code).unwrap(), Some(trigger));
    }

    #[test]
    fn legacy_aliases_are_canonical_on_the_wire() {
        let trigger = Trigger::Key("rightctrl".into());
        assert_eq!(trigger_to_code(&trigger), "key:ControlRight");
        assert_eq!(
            code_to_trigger("key:rightctrl").unwrap(),
            Some(Trigger::Key("ControlRight".into()))
        );
    }

    #[test]
    fn oversized_trigger_lists_are_rejected_before_overlap_work() {
        let values = (0..=MAX_TRIGGERS_PER_ACTION)
            .map(|index| Value::String(format!("key:F{}", index + 1)))
            .collect::<Vec<_>>();
        let error = trigger_list(Some(&Value::Array(values))).unwrap_err();
        assert!(error.contains("at most"));
    }

    #[test]
    fn oversized_structured_selector_is_rejected() {
        let trigger = Trigger::GamepadButton {
            device: DeviceSelector {
                stable_id: Some(
                    "x".repeat(vocalcode_core::limits::MAX_TRIGGER_SELECTOR_UTF8_BYTES + 1),
                ),
                ..DeviceSelector::default()
            },
            button: GamepadButton::South,
        };
        let code = format!("trigger:{}", serde_json::to_string(&trigger).unwrap());
        assert!(code_to_trigger(&code).unwrap_err().contains("stable_id"));
    }
}

/// Put text on the clipboard, so a transcript that landed in the wrong window
/// can be recovered without dictating it again.
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    vocalcode_platform::write_clipboard_text(text).map_err(|e| e.to_string())
}

/// Temporarily borrow the shared model-status line for updater progress. On
/// every return-to-UI path the previous engine label is restored, but only if
/// nobody else has published a newer label in the meantime. `set` also notices
/// an intervening engine update before changing phases, so a long download does
/// not resurrect the label that happened to exist when it began.
#[cfg(any(windows, target_os = "macos", test))]
struct TransientModelLabel<'a> {
    target: &'a Mutex<String>,
    restore: String,
    last_set: Option<String>,
}

#[cfg(any(windows, target_os = "macos", test))]
impl<'a> TransientModelLabel<'a> {
    fn new(target: &'a Mutex<String>) -> Self {
        Self {
            target,
            restore: target.lock().unwrap().clone(),
            last_set: None,
        }
    }

    fn set(&mut self, value: &str) {
        let mut current = self.target.lock().unwrap();
        let still_owns_label = self
            .last_set
            .as_ref()
            .is_some_and(|owned| current.as_str() == owned);
        if !still_owns_label {
            self.restore = current.clone();
        }
        *current = value.to_string();
        self.last_set = Some(value.to_string());
    }
}

#[cfg(any(windows, target_os = "macos", test))]
impl Drop for TransientModelLabel<'_> {
    fn drop(&mut self) {
        let Some(last_set) = self.last_set.as_ref() else {
            return;
        };
        let mut current = self.target.lock().unwrap();
        if current.as_str() == last_set {
            *current = self.restore.clone();
        }
    }
}

#[cfg(any(windows, target_os = "macos", test))]
const COMMAND_OUTPUT_LIMIT: usize = 256 * 1024;
#[cfg(any(windows, target_os = "macos", test))]
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(20);
#[cfg(any(windows, target_os = "macos", test))]
const COMMAND_DRAIN_GRACE: Duration = Duration::from_secs(2);

#[cfg(windows)]
const WINDOWS_CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const WINDOWS_UPDATE_HELPER_READY_TIMEOUT: Duration = Duration::from_secs(20);
#[cfg(all(test, windows))]
const WINDOWS_POWERSHELL_TEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Hide console-subsystem utilities without detaching them from the console
/// runtime itself. `DETACHED_PROCESS` can make Windows PowerShell exit
/// successfully before running `-Command`, so it must never be combined with
/// this flag for updater/signature-check processes.
#[cfg(windows)]
fn hide_windows_console(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(WINDOWS_CREATE_NO_WINDOW);
}

#[cfg(any(windows, target_os = "macos", test))]
#[derive(Debug)]
pub(crate) enum BoundedCommandError {
    Cancelled,
    TimedOut(Duration),
    OutputTooLarge,
    Io(std::io::Error),
}

#[cfg(any(windows, target_os = "macos", test))]
impl std::fmt::Display for BoundedCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => {
                formatter.write_str("command cancelled because VocalCode is quitting")
            }
            Self::TimedOut(timeout) => write!(formatter, "command timed out after {timeout:?}"),
            Self::OutputTooLarge => write!(
                formatter,
                "command output exceeded the {} KiB safety limit",
                COMMAND_OUTPUT_LIMIT / 1024
            ),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

#[cfg(any(windows, target_os = "macos", test))]
struct CapturedCommandOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

#[cfg(any(windows, target_os = "macos", test))]
fn drain_command_output(mut reader: impl std::io::Read) -> std::io::Result<CapturedCommandOutput> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let available = COMMAND_OUTPUT_LIMIT.saturating_sub(bytes.len());
        let retained = count.min(available);
        bytes.extend_from_slice(&buffer[..retained]);
        truncated |= retained != count;
        // Continue draining after the cap. Stopping here would let a child fill
        // the pipe and deadlock before it can be killed or reaped.
    }
    Ok(CapturedCommandOutput { bytes, truncated })
}

#[cfg(any(windows, target_os = "macos", test))]
#[cfg(windows)]
struct CommandProcessTree {
    job: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl CommandProcessTree {
    fn prepare(command: &mut std::process::Command) -> std::io::Result<Self> {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        // Signature verification and every other joined console utility should
        // stay invisible when VocalCode is launched from Explorer.
        hide_windows_console(command);

        // SAFETY: null security/name pointers request an unnamed job with
        // default security. The returned handle is owned by this guard.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` has the exact information-class layout and remains
        // live for the duration of the call.
        let configured = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        };
        if configured == 0 {
            let error = std::io::Error::last_os_error();
            // SAFETY: `job` is a valid owned handle.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
            return Err(error);
        }
        Ok(Self { job })
    }

    fn attach(&mut self, child: &std::process::Child) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        // SAFETY: both handles remain valid for the duration of the call.
        let assigned = unsafe { AssignProcessToJobObject(self.job, child.as_raw_handle()) };
        if assigned == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn terminate(&mut self) -> std::io::Result<()> {
        // SAFETY: the job handle is owned and valid until Drop.
        let terminated =
            unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1) };
        if terminated == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(windows)]
impl Drop for CommandProcessTree {
    fn drop(&mut self) {
        // KILL_ON_JOB_CLOSE is a final backstop for every return path.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.job) };
    }
}

#[cfg(all(unix, not(windows)))]
struct CommandProcessTree {
    process_group: Option<i32>,
}

#[cfg(all(unix, not(windows)))]
impl CommandProcessTree {
    fn prepare(command: &mut std::process::Command) -> std::io::Result<Self> {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        Ok(Self {
            process_group: None,
        })
    }

    fn attach(&mut self, child: &std::process::Child) -> std::io::Result<()> {
        let process_group = i32::try_from(child.id())
            .map_err(|_| std::io::Error::other("child process id does not fit pid_t"))?;
        self.process_group = Some(process_group);
        Ok(())
    }

    fn terminate(&mut self) -> std::io::Result<()> {
        let Some(process_group) = self.process_group.take() else {
            return Ok(());
        };
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        const SIGKILL: i32 = 9;
        // Negative pid targets the process group created before spawn.
        let result = unsafe { kill(-process_group, SIGKILL) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(3) {
            // ESRCH: the child and all of its descendants already exited.
            Ok(())
        } else {
            Err(error)
        }
    }
}

#[cfg(all(unix, not(windows)))]
impl Drop for CommandProcessTree {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

#[cfg(any(windows, target_os = "macos", test))]
fn wait_for_command_exit(
    child: &mut std::process::Child,
    timeout: Duration,
) -> std::io::Result<bool> {
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Ok(false);
        }
        std::thread::sleep(COMMAND_POLL_INTERVAL.min(remaining));
    }
}

#[cfg(any(windows, target_os = "macos", test))]
fn stop_and_reap_command(child: &mut std::process::Child, process_tree: &mut CommandProcessTree) {
    if let Err(error) = process_tree.terminate() {
        log::warn!("could not terminate command process tree: {error}");
    }
    if let Err(error) = child.kill() {
        if child.try_wait().ok().flatten().is_none() {
            log::warn!("could not terminate timed-out command: {error}");
        }
    }
    // Killing can fail (including a failed job/group cleanup). Do not turn an
    // explicit request deadline into an unbounded wait for that same process.
    // The job guard remains a final Windows cleanup attempt when dropped.
    match wait_for_command_exit(child, COMMAND_DRAIN_GRACE) {
        Ok(true) => {}
        Ok(false) => log::warn!("command did not exit within cleanup grace; stopped waiting"),
        Err(error) => log::warn!("could not reap terminated command: {error}"),
    }
}

/// Run a trusted platform utility without allowing it to wedge the joined
/// updater or the settings transaction forever. Output is drained concurrently
/// (and capped while still being discarded) so a verbose child cannot block on
/// a full pipe. Cancellation is checked at a short polling cadence; timeout and
/// cancellation attempt tree termination and use bounded cleanup waits as well.
#[cfg(any(windows, target_os = "macos", test))]
pub(crate) fn bounded_command_output(
    command: &mut std::process::Command,
    timeout: Duration,
    shutdown: Option<&AtomicBool>,
) -> Result<std::process::Output, BoundedCommandError> {
    bounded_command_inner(command, timeout, shutdown, None)
}

/// Source text uses a private pipe, never shell quoting, argv or a prompt file.
/// The same deadline and process-tree cleanup also bound a blocked stdin write.
#[cfg(any(windows, target_os = "macos", test))]
pub(crate) fn bounded_command_input(
    command: &mut std::process::Command,
    timeout: Duration,
    shutdown: Option<&AtomicBool>,
    input: Vec<u8>,
) -> Result<std::process::Output, BoundedCommandError> {
    if input.len() > 16 * 1024 {
        return Err(BoundedCommandError::Io(std::io::Error::other(
            "command input exceeds 16 KiB",
        )));
    }
    bounded_command_inner(command, timeout, shutdown, Some(input))
}

#[cfg(any(windows, target_os = "macos", test))]
fn bounded_command_inner(
    command: &mut std::process::Command,
    timeout: Duration,
    shutdown: Option<&AtomicBool>,
    input: Option<Vec<u8>>,
) -> Result<std::process::Output, BoundedCommandError> {
    use std::process::Stdio;
    use std::sync::mpsc;

    if timeout.is_zero() {
        return Err(BoundedCommandError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "command timeout must be non-zero",
        )));
    }
    if shutdown.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        return Err(BoundedCommandError::Cancelled);
    }
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        BoundedCommandError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "command deadline overflow",
        ))
    })?;
    let mut process_tree = CommandProcessTree::prepare(command).map_err(BoundedCommandError::Io)?;
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(BoundedCommandError::Io)?;
    if let Err(error) = process_tree.attach(&child) {
        stop_and_reap_command(&mut child, &mut process_tree);
        return Err(BoundedCommandError::Io(error));
    }
    let Some(stdout) = child.stdout.take() else {
        stop_and_reap_command(&mut child, &mut process_tree);
        return Err(BoundedCommandError::Io(std::io::Error::other(
            "command stdout pipe is unavailable",
        )));
    };
    let Some(stderr) = child.stderr.take() else {
        drop(stdout);
        stop_and_reap_command(&mut child, &mut process_tree);
        return Err(BoundedCommandError::Io(std::io::Error::other(
            "command stderr pipe is unavailable",
        )));
    };
    let (output_tx, output_rx) = mpsc::channel();
    let stdout_tx = output_tx.clone();
    let stdout_thread = match std::thread::Builder::new()
        .name("vocalcode-command-stdout".to_string())
        .spawn(move || {
            let _ = stdout_tx.send((true, drain_command_output(stdout)));
        }) {
        Ok(thread) => thread,
        Err(error) => {
            stop_and_reap_command(&mut child, &mut process_tree);
            return Err(BoundedCommandError::Io(error));
        }
    };
    let stderr_thread = match std::thread::Builder::new()
        .name("vocalcode-command-stderr".to_string())
        .spawn(move || {
            let _ = output_tx.send((false, drain_command_output(stderr)));
        }) {
        Ok(thread) => thread,
        Err(error) => {
            stop_and_reap_command(&mut child, &mut process_tree);
            // Cleanup can fail at the OS boundary. Joining an unfinished pipe
            // reader here would undo the bounded reap above.
            if stdout_thread.is_finished() {
                let _ = stdout_thread.join();
            }
            return Err(BoundedCommandError::Io(error));
        }
    };

    let (input_tx, input_rx) = mpsc::channel();
    let input_thread = if let Some(bytes) = input {
        let Some(mut stdin) = child.stdin.take() else {
            stop_and_reap_command(&mut child, &mut process_tree);
            return Err(BoundedCommandError::Io(std::io::Error::other(
                "command stdin pipe is unavailable",
            )));
        };
        match std::thread::Builder::new()
            .name("vocalcode-command-stdin".into())
            .spawn(move || {
                use std::io::Write;
                let result = stdin.write_all(&bytes);
                drop(stdin);
                let _ = input_tx.send(result);
            }) {
            Ok(thread) => Some(thread),
            Err(error) => {
                stop_and_reap_command(&mut child, &mut process_tree);
                return Err(BoundedCommandError::Io(error));
            }
        }
    } else {
        None
    };

    let status = loop {
        if shutdown.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            stop_and_reap_command(&mut child, &mut process_tree);
            break Err(BoundedCommandError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(error) => {
                stop_and_reap_command(&mut child, &mut process_tree);
                break Err(BoundedCommandError::Io(error));
            }
        }
        let now = Instant::now();
        if now >= deadline {
            stop_and_reap_command(&mut child, &mut process_tree);
            break Err(BoundedCommandError::TimedOut(timeout));
        }
        std::thread::sleep(COMMAND_POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
    };

    // A trusted utility may let its direct process exit while a helper keeps
    // inherited pipe handles alive. End the whole tree before waiting for EOF.
    let tree_cleanup = process_tree.terminate().map_err(BoundedCommandError::Io);
    let input_result = input_thread.map(|thread| {
        // If a failed OS tree cleanup leaves an inherited pipe alive, do not
        // replace our command deadline with an unbounded writer-thread join.
        match input_rx.recv_timeout(COMMAND_DRAIN_GRACE) {
            Ok(result) => {
                let _ = thread.join();
                result
            }
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out completing command input",
            )),
        }
    });

    let mut stdout = None;
    let mut stderr = None;
    let mut drain_error = None;
    let mut output_too_large = false;
    for _ in 0..2 {
        let (is_stdout, captured) = output_rx.recv_timeout(COMMAND_DRAIN_GRACE).map_err(|_| {
            BoundedCommandError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out draining command output",
            ))
        })?;
        match captured {
            Ok(captured) => {
                output_too_large |= captured.truncated;
                if is_stdout {
                    stdout = Some(captured.bytes);
                } else {
                    stderr = Some(captured.bytes);
                }
            }
            Err(error) => {
                if drain_error.is_none() {
                    drain_error = Some(error);
                }
            }
        }
    }
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    let status = status?;
    tree_cleanup?;
    if status.success() {
        if let Some(Err(error)) = input_result {
            return Err(BoundedCommandError::Io(error));
        }
    }
    if let Some(error) = drain_error {
        return Err(BoundedCommandError::Io(error));
    }
    if output_too_large {
        return Err(BoundedCommandError::OutputTooLarge);
    }
    Ok(std::process::Output {
        status,
        stdout: stdout.unwrap_or_default(),
        stderr: stderr.unwrap_or_default(),
    })
}

#[cfg(target_os = "macos")]
struct MacMountGuard {
    mount: PathBuf,
    armed: bool,
}

#[cfg(target_os = "macos")]
impl MacMountGuard {
    fn new(mount: &Path) -> Self {
        Self {
            mount: mount.to_path_buf(),
            armed: false,
        }
    }

    fn arm(&mut self) {
        self.armed = true;
    }

    fn detach(&mut self) -> Result<(), String> {
        if !self.armed {
            return Ok(());
        }
        let mut detach = std::process::Command::new("/usr/bin/hdiutil");
        detach.args(["detach", "-force"]).arg(&self.mount);
        let output = bounded_command_output(&mut detach, MAC_UPDATE_COMMAND_TIMEOUT, None)
            .map_err(|error| format!("could not detach update image: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "could not detach update image: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        self.armed = false;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Drop for MacMountGuard {
    fn drop(&mut self) {
        if let Err(error) = self.detach() {
            log::warn!("{error}");
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
static UPDATE_WORK_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Fixed PowerShell program for the detached Windows update transaction.
/// Paths are environment values rather than interpolated script, and every
/// process-creation exception after the old process exits reaches a best-effort
/// restart of the executable that was running before we exited. Timeouts that
/// cannot prove competing processes are gone preserve their private workspace
/// and a diagnostic rather than deleting live inputs or claiming a restart.
#[cfg(windows)]
const WINDOWS_UPDATE_HELPER: &str = concat!(
    "$ErrorActionPreference = 'Stop'; ",
    "$cleanupWorkspace = $true; ",
    "function Restart-Normal { ",
    "try { Start-Process -FilePath $env:VC_UPDATE_EXE -ErrorAction Stop | Out-Null } ",
    "catch { [System.Diagnostics.Process]::Start($env:VC_UPDATE_EXE) | Out-Null } ",
    "}; ",
    "function Restart-Failed([string]$reason) { ",
    "try { Start-Process -FilePath $env:VC_UPDATE_EXE ",
    "-ArgumentList @('--update-failed', $reason) -ErrorAction Stop | Out-Null } ",
    "catch { [System.Diagnostics.Process]::Start($env:VC_UPDATE_EXE, ",
    "('--update-failed ' + $reason)) | Out-Null } ",
    "}; ",
    "function Preserve-Diagnostic([string]$reason) { ",
    "$script:cleanupWorkspace = $false; ",
    "try { [System.IO.File]::WriteAllText((Join-Path $env:VC_UPDATE_DIR 'failure.txt'), $reason) } ",
    "catch {} ",
    "}; ",
    "$waitId = [int]$env:VC_UPDATE_PID; ",
    // Keep the Process object, and therefore its kernel process handle, rather
    // than polling the numeric PID. A PID reused after VocalCode exits cannot
    // extend this wait to the lifetime of an unrelated process. Five minutes
    // is deliberately longer than every joined network/model cancellation
    // deadline in the main process, while still giving this detached helper a
    // hard terminal state if shutdown itself has wedged.
    "$parent = Get-Process -Id $waitId -ErrorAction SilentlyContinue; ",
    "try { ",
    // The old process may exit only after this acknowledgement exists. Merely
    // creating powershell.exe is not proof that it parsed or ran `-Command`.
    "[System.IO.File]::WriteAllText($env:VC_UPDATE_READY, 'ready'); ",
    "if ($null -ne $parent -and -not $parent.WaitForExit(300000)) { ",
    // Starting the same executable here would only notify the still-live
    // single-instance owner; its command line (and therefore the failure
    // reason) is not forwarded. Leave a durable diagnostic and do not claim
    // that a visible recovery instance was launched.
    "Preserve-Diagnostic 'parent-exit-timeout'; return ",
    "}; ",
    "$p = Start-Process -FilePath $env:VC_UPDATE_INSTALLER ",
    "-ArgumentList @('/VERYSILENT','/SUPPRESSMSGBOXES','/NOCANCEL','/NORESTART') ",
    "-PassThru -ErrorAction Stop; ",
    // A broken installer must not keep the application closed forever. If it
    // exceeds fifteen minutes, ask the trusted system taskkill to terminate its
    // complete process tree. Preserve the workspace on every timeout: even a
    // successful tree kill is a recovery event worth diagnosing, and a failed
    // kill must never delete files under a possibly live descendant. Restart
    // only after taskkill itself and the tracked installer are both reaped.
    "if (-not $p.WaitForExit(900000)) { ",
    "Preserve-Diagnostic 'installer-timeout'; ",
    "$treeStopped = $false; ",
    "try { ",
    "$taskkill = Join-Path ([System.Environment]::SystemDirectory) 'taskkill.exe'; ",
    "$killer = Start-Process -FilePath $taskkill ",
    "-ArgumentList @('/PID', ([string]$p.Id), '/T', '/F') -PassThru -ErrorAction Stop; ",
    "$killerDone = $killer.WaitForExit(10000); ",
    "if (-not $killerDone) { try { $killer.Kill(); $null = $killer.WaitForExit(5000) } catch {} } ",
    "$treeStopped = $killerDone -and $killer.ExitCode -eq 0 -and $p.WaitForExit(10000); ",
    "} catch {}; ",
    "if ($treeStopped) { Restart-Failed 'installer-timeout' } ",
    "else { Preserve-Diagnostic 'installer-timeout-process-tree-alive' }; ",
    "return ",
    "}; ",
    "if ($p.ExitCode -eq 0) { Restart-Normal } ",
    "else { Restart-Failed ([string]$p.ExitCode) } ",
    "} catch { ",
    "Restart-Failed 'helper-launch-error' ",
    "} finally { ",
    "if ($cleanupWorkspace) { ",
    "Remove-Item -LiteralPath $env:VC_UPDATE_INSTALLER -Force -ErrorAction SilentlyContinue; ",
    "Remove-Item -LiteralPath $env:VC_UPDATE_DIR -Recurse -Force -ErrorAction SilentlyContinue ",
    "} ",
    "}",
);

#[cfg(windows)]
fn stop_and_reap_update_helper(child: &mut std::process::Child) {
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Do not close the running app until the background helper proves that
/// PowerShell actually parsed and began executing the fixed update program.
#[cfg(windows)]
fn wait_for_update_helper_ready(
    child: &mut std::process::Child,
    ready_path: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "update helper readiness deadline overflow".to_string())?;
    loop {
        if ready_path.is_file() {
            return match child.try_wait() {
                Ok(None) => Ok(()),
                Ok(Some(status)) => Err(format!(
                    "update helper exited immediately after initialization ({status})"
                )),
                Err(error) => Err(format!("could not inspect update helper: {error}")),
            };
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "update helper exited before initialization ({status})"
                ));
            }
            Ok(None) => {}
            Err(error) => return Err(format!("could not inspect update helper: {error}")),
        }
        if Instant::now() >= deadline {
            stop_and_reap_update_helper(child);
            return Err(format!(
                "update helper did not initialize within {} seconds",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(COMMAND_POLL_INTERVAL);
    }
}

#[cfg(windows)]
fn windows_directory(system: bool) -> Result<std::path::PathBuf, String> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::{
        GetSystemDirectoryW, GetWindowsDirectoryW,
    };

    let mut buffer = vec![0_u16; 32_768];
    let written = unsafe {
        if system {
            GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32)
        } else {
            GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32)
        }
    };
    if written == 0 || written as usize >= buffer.len() {
        return Err("could not resolve a trusted Windows directory".to_string());
    }
    let directory = std::ffi::OsString::from_wide(&buffer[..written as usize]);
    let path = std::path::PathBuf::from(directory);
    if !path.is_dir() || !path.is_absolute() {
        return Err("a trusted Windows directory is missing".to_string());
    }
    Ok(path)
}

#[cfg(windows)]
fn windows_executable(name: &str) -> Result<std::path::PathBuf, String> {
    let path = windows_directory(false)?.join(name);
    if !path.is_file() || !path.is_absolute() {
        return Err(format!("the system executable {name} is missing"));
    }
    Ok(path)
}

#[cfg(windows)]
fn system_powershell() -> Result<std::path::PathBuf, String> {
    let path = windows_directory(true)?
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    if !path.is_file() || !path.is_absolute() {
        return Err("the system PowerShell executable is missing".to_string());
    }
    Ok(path)
}

/// A collision-resistant, process-private workspace for one updater run.
/// Fixed names in the shared TEMP directory let a stale download, a second app
/// instance, or another local process replace bytes between verification and
/// execution. `create_dir` is the ownership boundary: it never reuses an
/// existing path, and Drop removes every failed/finished attempt.
#[cfg(any(windows, target_os = "macos"))]
struct UpdateWorkDir(PathBuf);

#[cfg(any(windows, target_os = "macos"))]
impl UpdateWorkDir {
    fn create() -> Result<Self, String> {
        let tick = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..64 {
            let nonce = UPDATE_WORK_NONCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "VocalCode-update-{}-{tick:032x}-{nonce:016x}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    let work = Self(path);
                    #[cfg(target_os = "macos")]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(
                            work.path(),
                            std::fs::Permissions::from_mode(0o700),
                        )
                        .map_err(|e| format!("secure update workspace: {e}"))?;
                    }
                    return Ok(work);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("create update workspace: {e}")),
            }
        }
        Err("could not allocate a unique update workspace".to_string())
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn hand_off_to_helper(self) {
        // Verified update state now belongs to the detached helper. Dropping it
        // in this process would race that helper after a graceful (rather than
        // process::exit) shutdown and could delete its installer/diagnostics.
        std::mem::forget(self);
    }
}

#[cfg(any(windows, target_os = "macos"))]
impl Drop for UpdateWorkDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!(
                    "could not remove update workspace {}: {e}",
                    self.0.display()
                );
            }
        }
    }
}

#[cfg(any(windows, target_os = "macos", test))]
const UPDATE_DISK_RESERVE_BYTES: u64 = 64 * 1024 * 1024;
#[cfg(any(windows, target_os = "macos", test))]
const UPDATE_IO_SLICE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(any(windows, target_os = "macos", test))]
const UPDATE_TRANSFER_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[cfg(any(windows, target_os = "macos", test))]
fn update_required_disk_space(expected_size: u64) -> Option<u64> {
    expected_size.checked_add(UPDATE_DISK_RESERVE_BYTES)
}

#[cfg(any(windows, target_os = "macos", test))]
#[derive(Debug, PartialEq, Eq)]
enum UpdateDownloadError {
    Cancelled,
    Failed(String),
}

#[cfg(any(windows, target_os = "macos", test))]
impl std::fmt::Display for UpdateDownloadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => {
                formatter.write_str("update cancelled because VocalCode is quitting")
            }
            Self::Failed(message) => formatter.write_str(message),
        }
    }
}

#[cfg(any(windows, target_os = "macos", test))]
#[derive(Clone, Copy)]
struct UpdateDownloadPolicy {
    https_only: bool,
    io_slice_timeout: Duration,
    transfer_timeout: Duration,
    retry_delay: Duration,
    max_no_progress_attempts: u32,
}

#[cfg(any(windows, target_os = "macos", test))]
impl UpdateDownloadPolicy {
    fn production() -> Self {
        Self {
            https_only: true,
            io_slice_timeout: UPDATE_IO_SLICE_TIMEOUT,
            transfer_timeout: UPDATE_TRANSFER_TIMEOUT,
            retry_delay: Duration::from_millis(100),
            max_no_progress_attempts: 6,
        }
    }
}

#[cfg(any(windows, target_os = "macos", test))]
fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(any(windows, target_os = "macos", test))]
fn parse_update_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    let total = total.parse::<u64>().ok()?;
    (start <= end && end < total).then_some((start, end, total))
}

#[cfg(any(windows, target_os = "macos", test))]
fn retryable_update_request_error(error: &ureq::Error) -> bool {
    matches!(
        error,
        ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
            | ureq::Error::HostNotFound
            | ureq::Error::ConnectionFailed
    )
}

/// Download one signed update payload with bounded cancellation latency.
///
/// `Read::read` cannot be interrupted by an atomic flag. Each HTTP request is
/// therefore capped to a short time slice and subsequent requests resume from
/// the exact number of bytes already hashed and written. A server that cannot
/// prove the requested range is rejected instead of silently appending a second
/// copy of the file. The caller still verifies the platform signature after
/// this transport/hash check.
#[cfg(any(windows, target_os = "macos", test))]
fn download_update_with_policy(
    url: &str,
    path: &Path,
    want_sha: &str,
    expected_size: u64,
    policy: UpdateDownloadPolicy,
    is_cancelled: impl Fn() -> bool,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<String, UpdateDownloadError> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};

    if !valid_sha256(want_sha) {
        return Err(UpdateDownloadError::Failed(
            "the update manifest does not contain a valid sha256".to_string(),
        ));
    }
    if !(1..=crate::MAX_UPDATE_BYTES).contains(&expected_size) {
        return Err(UpdateDownloadError::Failed(format!(
            "the update manifest size must be between 1 and {} bytes",
            crate::MAX_UPDATE_BYTES
        )));
    }
    if policy.io_slice_timeout.is_zero() || policy.transfer_timeout.is_zero() {
        return Err(UpdateDownloadError::Failed(
            "update download timeout must be non-zero".to_string(),
        ));
    }
    if is_cancelled() {
        return Err(UpdateDownloadError::Cancelled);
    }

    let required_space = update_required_disk_space(expected_size).ok_or_else(|| {
        UpdateDownloadError::Failed("update disk-space budget overflow".to_string())
    })?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let available = fs2::available_space(parent).map_err(|error| {
        UpdateDownloadError::Failed(format!(
            "could not verify free space for the update: {error}"
        ))
    })?;
    if available < required_space {
        return Err(UpdateDownloadError::Failed(format!(
            "not enough free space for the update (need {required_space} bytes, have {available})"
        )));
    }

    let request_timeout = policy.io_slice_timeout.min(policy.transfer_timeout);
    let resolved_url = if crate::community::ENABLED && url.starts_with("https://github.com/") {
        Some(
            crate::community::resolve_download_url(url, &is_cancelled)
                .map_err(UpdateDownloadError::Failed)?,
        )
    } else {
        None
    };
    let url = resolved_url.as_deref().unwrap_or(url);
    let agent = ureq::Agent::config_builder()
        .https_only(policy.https_only)
        .max_redirects(0)
        .timeout_connect(Some(Duration::from_secs(15).min(request_timeout)))
        .timeout_recv_response(Some(Duration::from_secs(30).min(request_timeout)))
        .timeout_recv_body(Some(request_timeout))
        .timeout_global(Some(request_timeout))
        .build()
        .new_agent();
    let deadline = Instant::now()
        .checked_add(policy.transfer_timeout)
        .unwrap_or_else(Instant::now);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| UpdateDownloadError::Failed(error.to_string()))?;

    let result = (|| -> Result<String, UpdateDownloadError> {
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 128 * 1024];
        let mut done = 0_u64;
        let mut no_progress_attempts = 0_u32;
        on_progress(0, Some(expected_size));

        'request: loop {
            if is_cancelled() {
                return Err(UpdateDownloadError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(UpdateDownloadError::Failed(format!(
                    "update transfer timed out after {:?}",
                    policy.transfer_timeout
                )));
            }
            if done == expected_size {
                break;
            }

            let request_start = done;
            let range = format!("bytes={request_start}-");
            let response = match agent
                .get(url)
                .header("Accept-Encoding", "identity")
                .header("Range", &range)
                .call()
            {
                Ok(response) => response,
                Err(error) if retryable_update_request_error(&error) => {
                    if is_cancelled() {
                        return Err(UpdateDownloadError::Cancelled);
                    }
                    no_progress_attempts = no_progress_attempts.saturating_add(1);
                    if no_progress_attempts > policy.max_no_progress_attempts {
                        return Err(UpdateDownloadError::Failed(format!(
                            "update request made no progress after {} attempts: {error}",
                            policy.max_no_progress_attempts
                        )));
                    }
                    if !policy.retry_delay.is_zero() {
                        std::thread::sleep(policy.retry_delay);
                    }
                    continue;
                }
                Err(error) => return Err(UpdateDownloadError::Failed(error.to_string())),
            };
            if is_cancelled() {
                return Err(UpdateDownloadError::Cancelled);
            }

            let status = response.status().as_u16();
            let advertised = response
                .headers()
                .get("content-length")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            let (response_end, resumable) = if status == 206 {
                let (start, end, total) = response
                    .headers()
                    .get("content-range")
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_update_content_range)
                    .ok_or_else(|| {
                        UpdateDownloadError::Failed(
                            "range response omitted a valid Content-Range".to_string(),
                        )
                    })?;
                if start != request_start {
                    return Err(UpdateDownloadError::Failed(format!(
                        "server resumed the update at byte {start}, expected {request_start}"
                    )));
                }
                if total != expected_size {
                    return Err(UpdateDownloadError::Failed(format!(
                        "range response total mismatch: expected {expected_size}, server {total}"
                    )));
                }
                let response_size = end - start + 1;
                if advertised.is_some_and(|size| size != response_size) {
                    return Err(UpdateDownloadError::Failed(format!(
                        "range response size mismatch: expected {response_size}, server {}",
                        advertised.unwrap_or_default()
                    )));
                }
                (Some(end), true)
            } else if status == 200 && request_start == 0 {
                if let Some(total) = advertised {
                    if total != expected_size {
                        return Err(UpdateDownloadError::Failed(format!(
                            "update Content-Length mismatch: expected {expected_size}, server {total}"
                        )));
                    }
                }
                (Some(expected_size - 1), false)
            } else {
                return Err(UpdateDownloadError::Failed(format!(
                    "server did not honor safe update resume at byte {request_start} (HTTP {status})"
                )));
            };

            let mut reader = response.into_body().into_reader();
            let mut made_progress = false;
            loop {
                if is_cancelled() {
                    return Err(UpdateDownloadError::Cancelled);
                }
                let count = match reader.read(&mut buffer) {
                    Ok(0) => {
                        if !made_progress {
                            no_progress_attempts = no_progress_attempts.saturating_add(1);
                            if no_progress_attempts > policy.max_no_progress_attempts {
                                return Err(UpdateDownloadError::Failed(format!(
                                    "update body ended without progress after {} attempts",
                                    policy.max_no_progress_attempts
                                )));
                            }
                        }
                        continue 'request;
                    }
                    Ok(count) => count,
                    Err(error) => {
                        if is_cancelled() {
                            return Err(UpdateDownloadError::Cancelled);
                        }
                        if !resumable && done == request_start && request_start != 0 {
                            return Err(UpdateDownloadError::Failed(format!(
                                "server does not support safe update resume: {error}"
                            )));
                        }
                        if !made_progress {
                            no_progress_attempts = no_progress_attempts.saturating_add(1);
                            if no_progress_attempts > policy.max_no_progress_attempts {
                                return Err(UpdateDownloadError::Failed(format!(
                                    "update body made no progress after {} attempts: {error}",
                                    policy.max_no_progress_attempts
                                )));
                            }
                        }
                        continue 'request;
                    }
                };
                let next_done = done.checked_add(count as u64).ok_or_else(|| {
                    UpdateDownloadError::Failed("update byte count overflow".to_string())
                })?;
                if next_done > expected_size {
                    return Err(UpdateDownloadError::Failed(
                        "server sent bytes beyond the exact manifest size".to_string(),
                    ));
                }
                if response_end.is_some_and(|end| next_done > end.saturating_add(1)) {
                    return Err(UpdateDownloadError::Failed(
                        "server sent bytes beyond its declared update range".to_string(),
                    ));
                }
                file.write_all(&buffer[..count])
                    .map_err(|error| UpdateDownloadError::Failed(error.to_string()))?;
                hasher.update(&buffer[..count]);
                done = next_done;
                made_progress = true;
                no_progress_attempts = 0;
                on_progress(done, Some(expected_size));
                if done == expected_size {
                    break 'request;
                }
                if response_end.is_some_and(|end| done == end.saturating_add(1)) {
                    continue 'request;
                }
            }
        }

        if done != expected_size {
            return Err(UpdateDownloadError::Failed(format!(
                "incomplete download: {done} of {expected_size} bytes"
            )));
        }
        file.sync_all()
            .map_err(|error| UpdateDownloadError::Failed(error.to_string()))?;
        let got = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if !got.eq_ignore_ascii_case(want_sha) {
            return Err(UpdateDownloadError::Failed(format!(
                "sha256 mismatch: expected {want_sha}, got {got}"
            )));
        }
        Ok(got)
    })();

    drop(file);
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// Hand a URL to the browser — `https://` only.
///
/// Every caller wants a web page: checkout, the share link, and the download for
/// an available update. That last one is the reason for the check, because its
/// URL is not ours at the point of use — it is read out of `latest.json`, which
/// arrives over the network, and `VOCALCODE_UPDATE_URL` can point that anywhere.
/// Passing an arbitrary string to `open` would let a `file://` URL open a local
/// file, or worse launch an application.
///
/// The Windows path avoids `cmd /C start` for a second reason: `cmd` re-parses
/// its own command line, so an `&` in a query string is read as a command
/// separator regardless of how Rust escaped the argument. `rundll32` involves no
/// shell at all.
/// One-click self-update. Downloads the installer and runs it silently; Inno's
/// Restart Manager then closes this app, upgrades in place (keeping the user's
/// config + licence — the installer first migrates a legacy config with
/// no-clobber semantics, then writes its default only if still absent), and
/// relaunches it. The user does nothing but click once.
///
/// This function is the Windows half. The macOS twin below downloads into a
/// private workspace, verifies SHA-256 plus the signed Team ID, mounts the dmg,
/// atomically replaces the bundle, and rolls back on failure.
///
/// The proving is the whole job. This runs an executable fetched over the
/// network, silently, with /NOCANCEL — so anything short of certainty here is a
/// code-execution channel with a friendly button on it. Two checks, because they
/// fail in different directions: the sha256 the deploy job publishes in
/// latest.json binds these bytes to the release we built and catches a corrupted
/// or swapped CDN object, but the hash and the installer come from the same
/// server, so a forged manifest carries a matching hash. The Authenticode
/// signature is what says "we built this" — see [`verify_authenticode`].
///
/// `want_sha` comes from the host's own manifest copy, never from the page.
///
/// Note the single `#[cfg(windows)]` below: an attribute separated from its item
/// by anything — including another doc comment — is easy to leave stranded, and
/// two stacked `cfg`s AND together rather than replacing one another.
#[cfg(windows)]
fn self_update(
    expected_version: &str,
    url: &str,
    want_sha: &str,
    expected_size: u64,
    status: &RuntimeStatus,
    proxy: &EventLoopProxy<UserEvent>,
) {
    if !crate::approved_update_url("windows", expected_version, url) {
        log::warn!("refusing to download a non-https update: {url}");
        *status.update_result.lock().unwrap() =
            Some((false, "The update link was rejected as unsafe.".to_string()));
        return;
    }
    if !valid_sha256(want_sha) {
        // Refusing rather than proceeding unverified. An older manifest with no
        // hash means we cannot tell a good download from a bad one, and the
        // fallback — opening the download page — costs the user two clicks
        // instead of running something unchecked.
        log::warn!("no usable sha256 in the manifest; opening the download instead");
        open_url(url);
        *status.update_result.lock().unwrap() = Some((
            false,
            "Automatic verification was unavailable; the download page was opened instead."
                .to_string(),
        ));
        return;
    }
    let work = match UpdateWorkDir::create() {
        Ok(work) => work,
        Err(e) => {
            log::error!("update workspace: {e}");
            *status.update_result.lock().unwrap() =
                Some((false, format!("Update could not start: {e}")));
            return;
        }
    };
    let mut model_label = TransientModelLabel::new(&status.model_label);
    model_label.set("Downloading update…");
    let tmp = work.path().join("VocalCodeSetup.exe");
    let dl = download_update_with_policy(
        url,
        &tmp,
        want_sha,
        expected_size,
        UpdateDownloadPolicy::production(),
        || status.shutdown.load(Ordering::Acquire),
        |done, total| {
            if let Some(total) = total {
                *status.update_download.lock().unwrap() = Some((
                    "update".to_string(),
                    (done.min(total) as f64 / total.max(1) as f64) * 100.0,
                    done as f64 / 1_000_000.0,
                    total as f64 / 1_000_000.0,
                ));
            }
        },
    )
    .and_then(|got| {
        log::info!("update sha256 ok: {got}");
        // The hash and the installer come from the same server, so a forged
        // manifest carries a matching hash — it only proves the CDN object was
        // not corrupted or swapped. The Authenticode signature is checked
        // against a chain rooted in Microsoft's trust store, which is not
        // something we serve, so it is the part that actually says "we built
        // this". Both, because they fail in different directions.
        verify_authenticode(&tmp, expected_version, Some(&status.shutdown)).map_err(|error| {
            if status.shutdown.load(Ordering::Acquire) {
                UpdateDownloadError::Cancelled
            } else {
                UpdateDownloadError::Failed(error)
            }
        })
    });
    *status.update_download.lock().unwrap() = None;
    match dl {
        Ok(()) => {
            model_label.set("Installing update — VocalCode will restart…");
            // A running exe locks its own files, so we can't install over
            // ourselves, and Restart Manager (/CLOSEAPPLICATIONS) hangs on this
            // GUI app. Instead spawn an independent hidden helper that waits for
            // us to exit, installs silently, and relaunches the app — then exit.
            let exe = match std::env::current_exe() {
                Ok(exe) => exe,
                Err(e) => {
                    log::error!("resolve executable for update restart: {e}");
                    *status.update_result.lock().unwrap() =
                        Some((false, format!("Update failed to start: {e}")));
                    return;
                }
            };
            // Dynamic paths travel only through environment variables. A batch
            // file expands `%NAME%` even inside quotes, so a legitimate path
            // containing percent signs could be rewritten or interpreted. The
            // PowerShell program below is fixed text and reads each path as an
            // opaque value. It also removes the unique private workspace after
            // the synchronous installer exits.
            let powershell = match system_powershell() {
                Ok(path) => path,
                Err(error) => {
                    log::error!("resolve system PowerShell: {error}");
                    *status.update_result.lock().unwrap() = Some((false, error));
                    return;
                }
            };
            let ready = work.path().join("helper.ready");
            let mut command = std::process::Command::new(powershell);
            command
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    WINDOWS_UPDATE_HELPER,
                ])
                .env("VC_UPDATE_INSTALLER", &tmp)
                .env("VC_UPDATE_EXE", &exe)
                .env("VC_UPDATE_DIR", work.path())
                .env("VC_UPDATE_PID", std::process::id().to_string())
                .env("VC_UPDATE_READY", &ready)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            hide_windows_console(&mut command);
            match command.spawn() {
                Ok(mut helper) => match wait_for_update_helper_ready(
                    &mut helper,
                    &ready,
                    WINDOWS_UPDATE_HELPER_READY_TIMEOUT,
                ) {
                    Ok(()) => {
                        // Dropping `Child` does not terminate it. The helper is
                        // now executing and waiting on our process handle.
                        drop(helper);
                        work.hand_off_to_helper();
                        request_shutdown(status, proxy, ShutdownAction::RestartForUpdate);
                    }
                    Err(error) => {
                        stop_and_reap_update_helper(&mut helper);
                        log::error!("launch updater helper: {error}");
                        *status.update_result.lock().unwrap() =
                            Some((false, format!("Update failed to start: {error}")));
                    }
                },
                Err(e) => {
                    log::error!("launch updater helper: {e}");
                    *status.update_result.lock().unwrap() =
                        Some((false, format!("Update failed to start: {e}")));
                }
            }
        }
        Err(UpdateDownloadError::Cancelled) => {
            log::info!("update download cancelled during shutdown");
        }
        Err(e) => {
            log::error!("update download failed: {e}");
            *status.update_result.lock().unwrap() =
                Some((false, format!("Update download failed: {e}")));
        }
    }
}

/// The certificate subject our installers are signed with (Azure Trusted
/// Signing). Checked as well as the signature's validity: a chain-valid
/// signature only means *somebody* legitimate signed the file, and anyone can
/// buy a certificate. This is the Windows counterpart of matching the Team ID
/// on a notarised macOS bundle.
#[cfg(windows)]
const SIGNER_SUBJECT: &str = "CN=Daming Wu, O=Daming Wu, L=Newberry, S=FL, C=US";

/// Refuse to run an installer that is not validly signed by us.
///
/// Fails closed: any doubt — an invalid chain, a different signer, PowerShell
/// missing, output we cannot parse — means we do not execute it. The caller
/// falls back to opening the download page, which costs the user two clicks
/// instead of running something unproven.
#[cfg(windows)]
fn verify_authenticode(
    path: &std::path::Path,
    expected_version: &str,
    shutdown: Option<&AtomicBool>,
) -> Result<(), String> {
    // The path travels in an environment variable rather than inside the
    // command text, so nothing in it can be read as script. The detached update
    // helper follows the same rule.
    let script = concat!(
        "$s = Get-AuthenticodeSignature -LiteralPath $env:VC_UPDATE_PATH; ",
        "Write-Output $s.Status; ",
        "Write-Output $s.SignerCertificate.Subject; ",
        "$v = (Get-Item -LiteralPath $env:VC_UPDATE_PATH).VersionInfo.ProductVersion; ",
        "$i = (Get-Item -LiteralPath $env:VC_UPDATE_PATH).VersionInfo; ",
        "Write-Output $v; ",
        "Write-Output $i.ProductName; ",
        "Write-Output $i.OriginalFilename",
    );
    let mut command = std::process::Command::new(system_powershell()?);
    command
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("VC_UPDATE_PATH", path);
    let out = bounded_command_output(&mut command, WINDOWS_SIGNATURE_COMMAND_TIMEOUT, shutdown)
        .map_err(|e| format!("could not check the signature: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "signature check failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let verdict = String::from_utf8_lossy(&out.stdout);
    let subject = read_verdict(&verdict)?;
    let version = read_signed_artifact_version(&verdict, expected_version)?;
    read_signed_artifact_identity(&verdict)?;
    log::info!("update signature and version ok: {subject}, {version}");
    Ok(())
}

#[cfg(windows)]
fn read_signed_artifact_identity(text: &str) -> Result<(), String> {
    let values = text
        .lines()
        .map(|line| line.trim().trim_start_matches('\u{feff}').trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if values.get(3).copied() != Some(crate::community::DATA_DIR_NAME) {
        return Err("installer ProductName is not VocalCode".to_string());
    }
    let filename = if crate::community::ENABLED {
        "VocalCodeCommunitySetup.exe"
    } else {
        "VocalCodeSetup.exe"
    };
    if values.get(4).copied() != Some(filename) {
        return Err("installer OriginalFilename is not VocalCodeSetup.exe".to_string());
    }
    Ok(())
}

/// Compare a strict three-component release with the only extra shape Windows
/// version resources legitimately add: one trailing `.0`. Arbitrarily stripping
/// zeroes made malformed values such as `0.5.2.0.0` or `0.5.2.` look authentic.
fn numeric_release_version(version: &str) -> Result<[u64; 3], String> {
    let version = version.trim();
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(format!("invalid release version {version:?}"));
    }
    let mut parsed = [0_u64; 3];
    for (slot, part) in parsed.iter_mut().zip(parts) {
        *slot = part
            .parse()
            .map_err(|_| format!("invalid release version {version:?}"))?;
    }
    Ok(parsed)
}

fn versions_match(actual: &str, expected: &str) -> Result<bool, String> {
    let expected = numeric_release_version(expected)?;
    let actual = actual.trim();
    let component_count = actual.split('.').count();
    let actual = if component_count == 4 {
        actual
            .strip_suffix(".0")
            .ok_or_else(|| format!("invalid Windows resource version {actual:?}"))?
    } else {
        actual
    };
    // Only one optional resource-version component is accepted. A fifth
    // component or a non-zero fourth component reaches the fail-closed paths.
    Ok(numeric_release_version(actual)? == expected)
}

#[cfg(windows)]
fn read_signed_artifact_version(text: &str, expected_version: &str) -> Result<String, String> {
    let version = text
        .lines()
        .map(|line| line.trim().trim_start_matches('\u{feff}').trim())
        .filter(|line| !line.is_empty())
        .nth(2)
        .ok_or("installer carries no product version")?;
    if !versions_match(version, expected_version)? {
        return Err(format!(
            "installer version {version} does not match offered update {expected_version}"
        ));
    }
    Ok(version.to_string())
}

/// The verdict half of [`verify_authenticode`], split out so it can be tested
/// without a signed file to hand — every way this rejects a *good* installer
/// lives here, and each one looks like the feature was never wired up rather
/// than like a failure.
///
/// A byte-order mark is not whitespace, so `trim` leaves it in place and the
/// status would never equal "Valid" — verification failing *closed* on a
/// perfectly good file, which degrades to the download page. Since this path
/// cannot be compiled or run on the machine it is written on, it is worth being
/// forgiving about shape and strict only about meaning.
#[cfg(windows)]
fn read_verdict(text: &str) -> Result<String, String> {
    let mut lines = text
        .lines()
        .map(|l| l.trim().trim_start_matches('\u{feff}').trim())
        .filter(|l| !l.is_empty());
    let status = lines.next().unwrap_or("");
    let subject = lines.next().unwrap_or("");
    if status != "Valid" {
        return Err(format!("installer signature is {status}, not Valid"));
    }
    let normalized = subject
        .split(',')
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(", ");
    if normalized != SIGNER_SUBJECT {
        return Err(format!("installer signed by someone else: {subject}"));
    }
    Ok(subject.to_string())
}

/// The Team ID our builds are notarised under. The macOS counterpart of
/// [`SIGNER_CN`], and checked for the same reason: "the signature is valid"
/// only means somebody legitimate signed it, and a Developer ID is something
/// anyone can buy.
#[cfg(target_os = "macos")]
const TEAM_ID: &str = "58Y98W3QQK";
#[cfg(target_os = "macos")]
const APP_BUNDLE_ID: &str = crate::community::BUNDLE_ID;

/// macOS: download the dmg, verify it, replace this bundle in place, relaunch.
///
/// This used to hand the URL to the browser and stop, which was defensible when
/// written and stopped being so once we looked at what it cost. Every update
/// put a macOS user back through the whole manual install — find the dmg, mount
/// it, drag it across, confirm the replacement — and that install is the part
/// people were already getting stuck on. Meanwhile the page told them the app
/// was downloading and would restart itself, which on this platform was simply
/// untrue.
///
/// **The bundle is moved by this process and no other.** macOS 14 onward guards
/// app bundles with the App Management permission: a helper script doing the
/// swap is a different process with a different signature and gets `Operation
/// not permitted`, which is exactly what happens if you try (I hit it by hand on
/// this machine before writing this). An app replacing *itself* with a build
/// carrying the same Team ID is the sanctioned path, so every file operation
/// happens here, and only the relaunch — which touches no bundle — is delegated.
///
/// The `cfg` sits directly on the fn, below the docs, for the reason given on the
/// Windows twin: an attribute placed above a doc block is one edit away from
/// attaching to whatever ends up next instead.
#[cfg(target_os = "macos")]
fn self_update(
    expected_version: &str,
    url: &str,
    want_sha: &str,
    expected_size: u64,
    status: &RuntimeStatus,
    proxy: &EventLoopProxy<UserEvent>,
) {
    if !crate::approved_update_url("macos", expected_version, url) {
        log::warn!("refusing to download a non-https update: {url}");
        *status.update_result.lock().unwrap() =
            Some((false, "The update link was rejected as unsafe.".to_string()));
        return;
    }
    if !valid_sha256(want_sha) {
        log::warn!("no usable sha256 in the manifest; opening the download instead");
        open_url(url);
        *status.update_result.lock().unwrap() = Some((
            false,
            "Automatic verification was unavailable; the download page was opened instead."
                .to_string(),
        ));
        return;
    }
    // Where we cannot install over ourselves, say so and fall back rather than
    // failing halfway. Running from the dmg or from Downloads is common enough
    // to be worth naming precisely: replacing a translocated copy updates a
    // throwaway the system made, and the app the user launches tomorrow is
    // still the old one.
    let bundle = match own_bundle() {
        Ok(b) => b,
        Err(e) => {
            log::warn!("cannot update in place ({e}); opening the download instead");
            *status.update_result.lock().unwrap() = Some((false, e));
            open_url(url);
            return;
        }
    };

    let work = match UpdateWorkDir::create() {
        Ok(work) => work,
        Err(e) => {
            log::error!("update workspace: {e}");
            *status.update_result.lock().unwrap() =
                Some((false, format!("Update could not start: {e}")));
            return;
        }
    };
    let tmp = work.path().join("VocalCode-update.dmg");
    let mount = work.path().join("mount");
    let dl = download_update_with_policy(
        url,
        &tmp,
        want_sha,
        expected_size,
        UpdateDownloadPolicy::production(),
        || status.shutdown.load(Ordering::Acquire),
        |done, total| {
            if let Some(total) = total {
                *status.update_download.lock().unwrap() = Some((
                    "update".to_string(),
                    (done.min(total) as f64 / total.max(1) as f64) * 100.0,
                    done as f64 / 1_000_000.0,
                    total as f64 / 1_000_000.0,
                ));
            }
        },
    );
    *status.update_download.lock().unwrap() = None;
    match dl {
        Ok(got) => log::info!("update sha256 ok: {got}"),
        Err(UpdateDownloadError::Cancelled) => {
            log::info!("update download cancelled during shutdown");
            return;
        }
        Err(e) => {
            log::error!("update download failed: {e}");
            *status.update_result.lock().unwrap() =
                Some((false, format!("Update download failed: {e}")));
            return;
        }
    }

    let mut model_label = TransientModelLabel::new(&status.model_label);
    model_label.set("Installing update — VocalCode will restart…");
    let install = (|| -> Result<(), String> {
        // The hash and the dmg come from the same server, so a forged manifest
        // carries a matching hash — it only proves the CDN object was intact.
        // The signature is checked against Apple's trust store, which we do not
        // serve, so it is the half that says "we built this". Both, because they
        // fail in different directions. Gatekeeper used to do this for us when
        // the user double-clicked; installing without a double-click means
        // inheriting the obligation, not dropping it.
        //
        // Assess the signed, notarised disk image itself *before* handing its
        // bytes to hdiutil. Verifying only the app after mounting would stop an
        // untrusted app from being installed, but would still expose the disk
        // image parser to a manifest+DMG substitution.
        verify_dmg_before_mount(&tmp, Some(&status.shutdown))?;
        let mut detach_stale = std::process::Command::new("/usr/bin/hdiutil");
        detach_stale.args(["detach", "-force"]).arg(&mount);
        let _ = bounded_command_output(
            &mut detach_stale,
            MAC_METADATA_COMMAND_TIMEOUT,
            Some(&status.shutdown),
        );
        if status.shutdown.load(Ordering::Acquire) {
            return Err("update cancelled because VocalCode is quitting".to_string());
        }
        std::fs::create_dir_all(&mount).map_err(|e| e.to_string())?;
        // Arm before invoking hdiutil: attach can mount successfully and then
        // time out, be cancelled, or report a later error. Every early return
        // from this point must still issue a bounded, non-cancellable detach.
        let mut mount_guard = MacMountGuard::new(&mount);
        mount_guard.arm();
        let mut attach = std::process::Command::new("/usr/bin/hdiutil");
        attach
            .args([
                "attach",
                "-nobrowse",
                "-readonly",
                "-noverify",
                "-mountpoint",
            ])
            .arg(&mount)
            .arg(&tmp);
        let out = bounded_command_output(
            &mut attach,
            MAC_UPDATE_COMMAND_TIMEOUT,
            Some(&status.shutdown),
        )
        .map_err(|e| format!("could not mount the update: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "could not mount the update: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let result = (|| -> Result<(), String> {
            let new_app = mount.join(crate::community::BUNDLE_NAME);
            if !new_app.is_dir() {
                return Err(format!(
                    "the update does not contain {}",
                    crate::community::BUNDLE_NAME
                ));
            }
            verify_bundle(&new_app, expected_version, Some(&status.shutdown))?;
            swap_bundle(&new_app, &bundle, expected_version, Some(&status.shutdown))
        })();
        // Once mounted, detach is cleanup rather than cancellable work. A
        // failed explicit detach remains armed and Drop retries it once.
        if let Err(error) = mount_guard.detach() {
            log::warn!("{error}; retrying during mount-guard cleanup");
        }
        result
    })();
    // Nothing unverified is left lying around, and nothing verified either: the
    // Windows path shipped for weeks parking a 7 MB installer in TEMP after
    // every successful update because only the failure branch cleaned up.
    let _ = std::fs::remove_file(&tmp);

    match install {
        Ok(()) => {
            // Relaunching is the one step this process cannot do, because it has
            // to happen after we are gone. The helper waits for this PID, checks
            // `open`'s exit status, retries, then directly execs the verified
            // bundle binary if LaunchServices refuses every attempt. The old
            // implementation treated spawning `/bin/sh` as proof that a later
            // `open` succeeded and could leave the app silently closed.
            let executable = bundle.join("Contents").join("MacOS").join(
                std::env::current_exe()
                    .ok()
                    .and_then(|path| path.file_name().map(std::ffi::OsStr::to_os_string))
                    .unwrap_or_else(|| std::ffi::OsString::from("VocalCode")),
            );
            let script = mac_relaunch_script(&bundle, &executable, work.path(), std::process::id());
            match std::process::Command::new("/bin/sh")
                .args(["-c", &script])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(_) => {
                    work.hand_off_to_helper();
                    log::info!("update installed; restarting");
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    request_shutdown(status, proxy, ShutdownAction::RestartForUpdate);
                }
                Err(e) => {
                    // Installed but not restarted: say precisely that, because
                    // "update failed" would send the user to reinstall a version
                    // they already have.
                    log::error!("relaunch after update: {e}");
                    *status.update_result.lock().unwrap() = Some((
                        true,
                        "Update installed — quit and reopen VocalCode to use it.".to_string(),
                    ));
                }
            }
        }
        Err(e) if status.shutdown.load(Ordering::Acquire) => {
            log::info!("update installation cancelled during shutdown: {e}");
        }
        Err(e) => {
            log::error!("update install failed: {e}");
            *status.update_result.lock().unwrap() = Some((false, format!("Update failed: {e}")));
        }
    }
}

/// Verify ownership and notarisation before the disk image parser sees the
/// downloaded file. Hash validation alone is not an authenticity boundary
/// because the update manifest and DMG share an origin.
#[cfg(target_os = "macos")]
fn verify_dmg_before_mount(
    dmg: &std::path::Path,
    shutdown: Option<&AtomicBool>,
) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(dmg)
        .map_err(|error| format!("could not inspect the update disk image: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("the update disk image is not a regular file".to_string());
    }

    let mut verify = std::process::Command::new("/usr/bin/codesign");
    verify
        .args(["--verify", "--strict", "--verbose=2"])
        .arg(dmg);
    let verified = bounded_command_output(&mut verify, MAC_UPDATE_COMMAND_TIMEOUT, shutdown)
        .map_err(|error| format!("could not verify the disk image signature: {error}"))?;
    if !verified.status.success() {
        return Err(format!(
            "the update disk image signature is invalid: {}",
            String::from_utf8_lossy(&verified.stderr).trim()
        ));
    }
    let mut signer = std::process::Command::new("/usr/bin/codesign");
    signer.args(["-dv", "--verbose=2"]).arg(dmg);
    let details = bounded_command_output(&mut signer, MAC_METADATA_COMMAND_TIMEOUT, shutdown)
        .map_err(|error| format!("could not read the disk image signer: {error}"))?;
    if !details.status.success() {
        return Err("could not read the disk image signing identity".to_string());
    }
    let details = format!(
        "{}{}",
        String::from_utf8_lossy(&details.stdout),
        String::from_utf8_lossy(&details.stderr)
    );
    let team = read_team_id(&details)?;

    let mut assess = std::process::Command::new("/usr/sbin/spctl");
    assess
        .args([
            "--assess",
            "--type",
            "open",
            "--context",
            "context:primary-signature",
            "--verbose=2",
        ])
        .arg(dmg);
    let gatekeeper = bounded_command_output(&mut assess, MAC_UPDATE_COMMAND_TIMEOUT, shutdown)
        .map_err(|error| format!("could not ask Gatekeeper to assess the disk image: {error}"))?;
    let verdict = format!(
        "{}{}",
        String::from_utf8_lossy(&gatekeeper.stdout),
        String::from_utf8_lossy(&gatekeeper.stderr)
    );
    if !gatekeeper.status.success() {
        return Err(read_gatekeeper_verdict(&verdict)
            .err()
            .unwrap_or_else(|| "Gatekeeper rejected the update disk image".to_string()));
    }
    read_gatekeeper_verdict(&verdict)?;
    log::info!("update disk image accepted before mount: notarised, team {team}");
    Ok(())
}

/// The `.app` this process is running from, if we are allowed to replace it.
#[cfg(any(test, target_os = "macos"))]
fn is_canonical_bundle_path(path: &std::path::Path) -> bool {
    path.file_name() == Some(std::ffi::OsStr::new(crate::community::BUNDLE_NAME))
}

#[cfg(target_os = "macos")]
fn own_bundle() -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // …/VocalCode.app/Contents/MacOS/VocalCode
    let bundle = exe
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .ok_or("VocalCode is not running from an app bundle")?
        .to_path_buf();
    if bundle.extension().and_then(|s| s.to_str()) != Some("app") {
        return Err("VocalCode is not running from an app bundle".to_string());
    }
    // Recovery copies deliberately use canonical `.VocalCode-update-*.app`
    // names. Updating a user-renamed Foo.app would strand those copies beside
    // Foo.app, while startup cleanup (correctly) refuses to touch them because
    // it cannot prove they belong to VocalCode. Fall back to manual install.
    if !is_canonical_bundle_path(&bundle) {
        return Err(format!(
            "Rename the app to {}, or download the update manually.",
            crate::community::BUNDLE_NAME
        ));
    }
    // App Translocation: launching a quarantined app from the dmg or from
    // Downloads runs it out of a read-only copy under a random path. Replacing
    // that copy would report success and change nothing the user ever opens
    // again — the worst possible outcome, so it is refused by name.
    if bundle
        .components()
        .any(|c| c.as_os_str() == "AppTranslocation")
    {
        return Err("Move VocalCode into your Applications folder first, then update.".to_string());
    }
    let parent = bundle
        .parent()
        .ok_or("VocalCode is at the root of a volume")?;
    // Checked by writing rather than by reading permissions: a read-only volume,
    // an app owned by another user and an unwritable directory all present
    // differently, and all of them mean the same thing here.
    let mut probe = None;
    for _ in 0..64 {
        let nonce = UPDATE_WORK_NONCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".vocalcode-update-probe-{}-{nonce:016x}",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                drop(file);
                probe = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                return Err(format!(
                    "VocalCode cannot update itself in {}",
                    parent.display()
                ));
            }
        }
    }
    let probe = probe.ok_or_else(|| {
        format!(
            "VocalCode cannot reserve an update probe in {}",
            parent.display()
        )
    })?;
    std::fs::remove_file(&probe).map_err(|error| {
        format!(
            "VocalCode could not remove its update probe {}: {error}",
            probe.display()
        )
    })?;
    Ok(bundle)
}

/// Refuse a bundle that is not validly signed by us and notarised by Apple.
///
/// Fails closed, like its Windows twin: a bad chain, a different team, a missing
/// tool or output we cannot parse all mean we do not install it.
#[cfg(target_os = "macos")]
fn verify_bundle(
    app: &std::path::Path,
    expected_version: &str,
    shutdown: Option<&AtomicBool>,
) -> Result<(), String> {
    let team = verify_bundle_identity(app, shutdown)?;
    let mut assess = std::process::Command::new("/usr/sbin/spctl");
    assess.args(["-a", "-t", "exec", "-vv"]).arg(app);
    let out = bounded_command_output(&mut assess, MAC_UPDATE_COMMAND_TIMEOUT, shutdown)
        .map_err(|e| format!("could not check the signature: {e}"))?;
    // spctl reports its verdict on stderr.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    read_gatekeeper_verdict(&text)?;

    let plist = app.join("Contents").join("Info.plist");
    let mut read_version = std::process::Command::new("/usr/libexec/PlistBuddy");
    read_version
        .args(["-c", "Print :CFBundleShortVersionString"])
        .arg(&plist);
    let out = bounded_command_output(&mut read_version, MAC_METADATA_COMMAND_TIMEOUT, shutdown)
        .map_err(|error| format!("could not read the update version: {error}"))?;
    if !out.status.success() {
        return Err(format!(
            "could not read the update version: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !versions_match(&version, expected_version)? {
        return Err(format!(
            "bundle version {version} does not match offered update {expected_version}"
        ));
    }
    log::info!("update signature and version ok: notarised, team {team}, {version}");
    Ok(())
}

/// Verify product ownership independently of its version. This is also the
/// deletion guard for fixed-name updater leftovers: an unrelated directory,
/// symlink, ad-hoc bundle, or bundle from another team is preserved.
#[cfg(target_os = "macos")]
fn verify_bundle_identity(
    app: &std::path::Path,
    shutdown: Option<&AtomicBool>,
) -> Result<String, String> {
    let metadata = std::fs::symlink_metadata(app)
        .map_err(|error| format!("could not inspect the app bundle: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("the app bundle path is not an owned directory".to_string());
    }
    let mut verify = std::process::Command::new("/usr/bin/codesign");
    verify
        .args(["--verify", "--deep", "--strict", "--verbose=2"])
        .arg(app);
    let verified = bounded_command_output(&mut verify, MAC_UPDATE_COMMAND_TIMEOUT, shutdown)
        .map_err(|error| format!("could not verify the app signature: {error}"))?;
    if !verified.status.success() {
        return Err(format!(
            "the app signature is invalid: {}",
            String::from_utf8_lossy(&verified.stderr).trim()
        ));
    }
    let mut signer = std::process::Command::new("/usr/bin/codesign");
    signer.args(["-dv", "--verbose=2"]).arg(app);
    let details = bounded_command_output(&mut signer, MAC_METADATA_COMMAND_TIMEOUT, shutdown)
        .map_err(|error| format!("could not read the app signature: {error}"))?;
    if !details.status.success() {
        return Err("could not read the app signature identity".to_string());
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&details.stdout),
        String::from_utf8_lossy(&details.stderr)
    );
    let team = read_team_id(&text)?;
    read_bundle_identifier(&text)?;

    let mut read_requirement = std::process::Command::new("/usr/bin/codesign");
    read_requirement.args(["-dr", "-"]).arg(app);
    let requirement = bounded_command_output(
        &mut read_requirement,
        MAC_METADATA_COMMAND_TIMEOUT,
        shutdown,
    )
    .map_err(|error| format!("could not read the designated requirement: {error}"))?;
    let requirement_text = format!(
        "{}{}",
        String::from_utf8_lossy(&requirement.stdout),
        String::from_utf8_lossy(&requirement.stderr)
    );
    if !requirement.status.success() || !designated_requirement_matches(&requirement_text) {
        return Err("the app designated requirement does not identify VocalCode".to_string());
    }

    let plist = app.join("Contents").join("Info.plist");
    let mut read_bundle_id = std::process::Command::new("/usr/libexec/PlistBuddy");
    read_bundle_id
        .args(["-c", "Print :CFBundleIdentifier"])
        .arg(&plist);
    let bundle_id =
        bounded_command_output(&mut read_bundle_id, MAC_METADATA_COMMAND_TIMEOUT, shutdown)
            .map_err(|error| format!("could not read the bundle identifier: {error}"))?;
    if !bundle_id.status.success()
        || String::from_utf8_lossy(&bundle_id.stdout).trim() != APP_BUNDLE_ID
    {
        return Err("the app Info.plist has the wrong bundle identifier".to_string());
    }
    Ok(team)
}

/// The verdict half of [`verify_bundle`], split out so the ways it can wrongly
/// reject *our own* build are testable without a signed bundle to hand.
#[cfg(target_os = "macos")]
fn read_gatekeeper_verdict(text: &str) -> Result<(), String> {
    if !text.lines().any(|l| l.trim().ends_with(": accepted")) {
        // "rejected" and "no usable signature" read very differently to a user
        // and mean the same thing to us, so the message carries what was said.
        let why = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("no verdict");
        return Err(format!("the update is not accepted by Gatekeeper: {why}"));
    }
    if !text.contains("source=Notarized Developer ID") {
        return Err("the update is not notarised".to_string());
    }
    Ok(())
}

/// Pull `TeamIdentifier` out of `codesign -dv` and insist it is ours.
#[cfg(target_os = "macos")]
fn read_team_id(text: &str) -> Result<String, String> {
    let team = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("TeamIdentifier="))
        .ok_or("the update carries no Team ID")?
        .trim()
        .to_string();
    if team != TEAM_ID {
        return Err(format!("the update is signed by another team ({team})"));
    }
    Ok(team)
}

#[cfg(target_os = "macos")]
fn read_bundle_identifier(text: &str) -> Result<(), String> {
    let identifier = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Identifier="))
        .ok_or("the update carries no signing identifier")?;
    if identifier == APP_BUNDLE_ID {
        Ok(())
    } else {
        Err(format!(
            "the update has the wrong signing identifier ({identifier})"
        ))
    }
}

#[cfg(target_os = "macos")]
fn designated_requirement_matches(text: &str) -> bool {
    text.contains(&format!("identifier \"{APP_BUNDLE_ID}\""))
        && text.contains(&format!("certificate leaf[subject.OU] = \"{TEAM_ID}\""))
}

#[cfg(any(test, target_os = "macos"))]
const MAC_UPDATE_TRANSACTION: &str = if crate::community::ENABLED {
    ".VocalCodeCommunity-update-transaction.json"
} else {
    ".VocalCode-update-transaction.json"
};
#[cfg(target_os = "macos")]
const MAC_UPDATE_LOCK: &str = if crate::community::ENABLED {
    ".VocalCodeCommunity-update.lock"
} else {
    ".VocalCode-update.lock"
};

#[cfg(any(test, target_os = "macos"))]
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum MacUpdatePhase {
    Preparing,
    Ready,
}

#[cfg(any(test, target_os = "macos"))]
#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct MacUpdateTransaction {
    version: u32,
    expected_version: String,
    phase: MacUpdatePhase,
}

#[cfg(any(test, target_os = "macos"))]
fn parse_mac_update_transaction(bytes: &[u8]) -> Result<MacUpdateTransaction, String> {
    let transaction: MacUpdateTransaction = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid update transaction: {error}"))?;
    if transaction.version != 1 || numeric_release_version(&transaction.expected_version).is_err() {
        return Err("invalid update transaction version or release".to_string());
    }
    Ok(transaction)
}

#[cfg(target_os = "macos")]
struct MacUpdateLock(std::fs::File);

#[cfg(target_os = "macos")]
impl Drop for MacUpdateLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

#[cfg(target_os = "macos")]
fn lock_mac_update(
    parent: &std::path::Path,
    shutdown: Option<&AtomicBool>,
) -> Result<MacUpdateLock, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(parent.join(MAC_UPDATE_LOCK))
        .map_err(|error| format!("could not open update lock: {error}"))?;
    let deadline = Instant::now()
        .checked_add(MAC_UPDATE_LOCK_TIMEOUT)
        .ok_or_else(|| "macOS update lock deadline overflow".to_string())?;
    acquire_update_file_lock(&file, deadline, shutdown)
        .map_err(|error| format!("could not lock update transaction: {error}"))?;
    Ok(MacUpdateLock(file))
}

#[cfg(any(test, target_os = "macos"))]
fn update_file_lock_is_contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock || {
        #[cfg(windows)]
        {
            error.raw_os_error() == Some(33)
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}

#[cfg(any(test, target_os = "macos"))]
fn acquire_update_file_lock(
    file: &std::fs::File,
    deadline: Instant,
    shutdown: Option<&AtomicBool>,
) -> std::io::Result<()> {
    loop {
        if shutdown.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "update lock cancelled because VocalCode is quitting",
            ));
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for the macOS update transaction lock",
            ));
        }
        match fs2::FileExt::try_lock_exclusive(file) {
            Ok(()) => {
                if shutdown.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                    let _ = fs2::FileExt::unlock(file);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "update lock cancelled because VocalCode is quitting",
                    ));
                }
                return Ok(());
            }
            Err(error) if update_file_lock_is_contended(&error) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "timed out waiting for the macOS update transaction lock",
                    ));
                }
                std::thread::sleep(
                    COMMAND_POLL_INTERVAL.min(deadline.saturating_duration_since(now)),
                );
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(target_os = "macos")]
fn write_mac_update_transaction(
    parent: &std::path::Path,
    expected: &str,
    phase: MacUpdatePhase,
) -> Result<(), String> {
    let transaction = MacUpdateTransaction {
        version: 1,
        expected_version: expected.to_string(),
        phase,
    };
    let bytes = serde_json::to_vec(&transaction)
        .map_err(|error| format!("could not encode update transaction: {error}"))?;
    crate::storage::atomic_write(&parent.join(MAC_UPDATE_TRANSACTION), bytes)
        .map_err(|error| format!("could not persist update transaction: {error}"))
}

#[cfg(target_os = "macos")]
fn remove_mac_update_transaction(parent: &std::path::Path) -> Result<(), String> {
    match std::fs::remove_file(parent.join(MAC_UPDATE_TRANSACTION)) {
        Ok(()) => {
            if let Ok(directory) = std::fs::File::open(parent) {
                let _ = directory.sync_all();
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not remove update transaction: {error}")),
    }
}

#[cfg(target_os = "macos")]
fn discard_mac_staging_transaction(
    parent: &std::path::Path,
    staged: &std::path::Path,
) -> Result<(), String> {
    match std::fs::remove_dir_all(staged) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            // Keep the marker: startup can retry, whereas deleting it now
            // would make this fixed-name directory look unrelated forever.
            return Err(format!(
                "could not discard update stage {}: {error}",
                staged.display()
            ));
        }
    }
    remove_mac_update_transaction(parent)
}

/// Atomically exchange two same-volume directory names. At every crash point
/// the canonical `VocalCode.app` name resolves to either the complete old
/// bundle or the complete verified new bundle; there is no rename gap.
#[cfg(target_os = "macos")]
fn atomic_exchange_paths(left: &std::path::Path, right: &std::path::Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    const AT_FDCWD: i32 = -2;
    const RENAME_SWAP: u32 = 0x0000_0002;
    extern "C" {
        fn renameatx_np(
            from_fd: i32,
            from: *const std::ffi::c_char,
            to_fd: i32,
            to: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }
    let left = CString::new(left.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
    let right = CString::new(right.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
    let result = unsafe {
        renameatx_np(
            AT_FDCWD,
            left.as_ptr(),
            AT_FDCWD,
            right.as_ptr(),
            RENAME_SWAP,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Put `new_app` where `installed` is with macOS's atomic name exchange. The
/// canonical bundle therefore never disappears, even if power is lost at the
/// exact publication point.
#[cfg(target_os = "macos")]
fn swap_bundle(
    new_app: &std::path::Path,
    installed: &std::path::Path,
    expected_version: &str,
    shutdown: Option<&AtomicBool>,
) -> Result<(), String> {
    let parent = installed.parent().ok_or("no parent directory")?;
    let staged = parent.join(format!("{}-staged.app", crate::community::UPDATE_STEM));
    let old = parent.join(format!("{}-old.app", crate::community::UPDATE_STEM));
    let _update_lock = lock_mac_update(parent, shutdown)?;
    ensure_recovery_paths_absent(&staged, &old)?;
    if parent.join(MAC_UPDATE_TRANSACTION).exists() {
        return Err(format!(
            "a previous update transaction remains at {}; reopen VocalCode to recover it before retrying",
            parent.join(MAC_UPDATE_TRANSACTION).display()
        ));
    }
    if shutdown.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        return Err("update cancelled because VocalCode is quitting".to_string());
    }

    // Own the fixed staging name durably before ditto can create it. If this
    // process dies anywhere in preparation, startup can distinguish its partial
    // directory from an unrelated path and finish cleanup.
    write_mac_update_transaction(parent, expected_version, MacUpdatePhase::Preparing)?;

    // ditto, not fs::copy: it is the tool that preserves the symlinks, resource
    // forks and extended attributes a signed bundle is made of. A hand-rolled
    // recursive copy produces a bundle whose signature no longer validates,
    // which Gatekeeper would then refuse on the next launch.
    let mut copy = std::process::Command::new("/usr/bin/ditto");
    copy.arg(new_app).arg(&staged);
    let out = match bounded_command_output(&mut copy, MAC_UPDATE_COPY_TIMEOUT, shutdown) {
        Ok(output) => output,
        Err(error) => {
            let _ = discard_mac_staging_transaction(parent, &staged);
            return Err(format!("could not stage the update: {error}"));
        }
    };
    if !out.status.success() {
        let _ = discard_mac_staging_transaction(parent, &staged);
        return Err(format!(
            "could not stage the update: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // Verified again where it will actually run. ditto is faithful, but the
    // thing we checked was on a read-only image and this is the copy that gets
    // launched; the check is cheap and the failure it catches is silent.
    if let Err(error) = verify_bundle(&staged, expected_version, shutdown) {
        let _ = discard_mac_staging_transaction(parent, &staged);
        return Err(error);
    }
    if shutdown.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        let _ = discard_mac_staging_transaction(parent, &staged);
        return Err("update cancelled because VocalCode is quitting".to_string());
    }

    if let Err(error) =
        write_mac_update_transaction(parent, expected_version, MacUpdatePhase::Ready)
    {
        let _ = discard_mac_staging_transaction(parent, &staged);
        return Err(error);
    }
    if let Err(error) = atomic_exchange_paths(installed, &staged) {
        // Payload first, marker last: a crash between the operations remains a
        // recoverable transaction instead of a permanent unowned stage.
        let _ = discard_mac_staging_transaction(parent, &staged);
        return Err(format!("could not atomically publish the update: {error}"));
    }
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }

    // After the exchange, `staged` is the complete old bundle this process is
    // executing from. Giving it the support-friendly name is best effort only:
    // a crash or rename failure still leaves the canonical new app intact and
    // the persisted transaction tells its first launch exactly what to clean.
    if let Err(error) = std::fs::rename(&staged, &old) {
        log::warn!(
            "update committed; old bundle remains at {}: {error}",
            staged.display()
        );
    }
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

/// Fixed recovery names make the failure locations discoverable to support,
/// but they must never be reused or deleted at the start of another attempt.
/// After an install rename and its rollback both fail, these may be the only
/// two recoverable bundles left. A retry therefore fails closed and preserves
/// them for explicit recovery.
#[cfg(any(test, target_os = "macos"))]
fn ensure_recovery_paths_absent(
    staged: &std::path::Path,
    old: &std::path::Path,
) -> Result<(), String> {
    let present = [old, staged]
        .into_iter()
        .filter(|path| std::fs::symlink_metadata(path).is_ok())
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    if present.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "a previous update left recovery copies at {}; refusing to delete or overwrite them",
            present.join(" and ")
        ))
    }
}

/// Delete the bundle the previous version was running from.
///
/// [`swap_bundle`] cannot remove it: the process doing the swap is executing out
/// of it. So the copy that starts afterwards clears it, which is also the only
/// process that can prove the update worked — if we are running, the new bundle
/// is in place. Called at startup and silent when there is nothing to do.
#[cfg(target_os = "macos")]
pub fn clear_update_leftovers() {
    let Ok(bundle) = own_bundle() else { return };
    if !is_canonical_install_bundle(&bundle) {
        log::warn!(
            "not clearing update recovery copies while running from {}",
            bundle.display()
        );
        return;
    }
    let Some(parent) = bundle.parent() else {
        return;
    };
    let Ok(_update_lock) = lock_mac_update(parent, None) else {
        log::warn!("could not lock macOS update recovery");
        return;
    };
    let staged = parent.join(format!("{}-staged.app", crate::community::UPDATE_STEM));
    let old = parent.join(format!("{}-old.app", crate::community::UPDATE_STEM));
    let marker_path = parent.join(MAC_UPDATE_TRANSACTION);
    let source = match read_bounded_local_file(&marker_path, MAC_UPDATE_TRANSACTION_MAX_BYTES) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Compatibility recovery for leftovers made by builds from before
            // transaction markers existed (0.5.x and earlier). Those updaters
            // moved the running bundle aside and had no marker to hand the new
            // version, so nothing ever cleared it — and `ensure_recovery_paths_
            // absent` then refused every later update, permanently and almost
            // silently: the failure surfaces only when somebody presses Update
            // by hand. `old` used to be excluded here, which is exactly the
            // case that stranded anyone upgrading from 0.5.x.
            //
            // Ownership of the running bundle and of each leftover is the whole
            // proof available without a marker, and it is the same proof the
            // orphan stage has always been cleared on. The update lock is held
            // throughout, so no swap can be relying on these copies.
            if verify_bundle_identity(&bundle, None).is_err() {
                log::warn!("preserving update leftovers: this bundle is not a verified install");
                return;
            }
            for path in [&old, &staged] {
                if !path.exists() {
                    continue;
                }
                if let Err(error) = verify_bundle_identity(path, None) {
                    log::warn!(
                        "preserving unowned update leftover {}: {error}",
                        path.display()
                    );
                    continue;
                }
                match std::fs::remove_dir_all(path) {
                    Ok(()) => {
                        log::info!("removed verified orphan update leftover {}", path.display())
                    }
                    Err(error) => log::warn!(
                        "could not remove orphan update leftover {}: {error}",
                        path.display()
                    ),
                }
            }
            return;
        }
        Err(error) => {
            log::warn!("could not read update transaction: {error}");
            return;
        }
    };
    let transaction = match parse_mac_update_transaction(&source) {
        Ok(transaction) => transaction,
        Err(error) => {
            log::warn!("preserving malformed update transaction: {error}");
            return;
        }
    };
    if verify_bundle(&bundle, &transaction.expected_version, None).is_ok() {
        let mut complete = true;
        for path in [&old, &staged] {
            if !path.exists() {
                continue;
            }
            if let Err(error) = verify_bundle_identity(path, None) {
                log::warn!(
                    "preserving unowned update leftover {}: {error}",
                    path.display()
                );
                complete = false;
                continue;
            }
            match std::fs::remove_dir_all(path) {
                Ok(()) => log::info!("removed committed update leftover {}", path.display()),
                Err(error) => {
                    complete = false;
                    log::warn!("could not remove {}: {error}", path.display());
                }
            }
        }
        if complete {
            if let Err(error) = remove_mac_update_transaction(parent) {
                log::warn!("{error}");
            }
        }
        return;
    }

    // Canonical does not contain the offered update. Only an owned, still-
    // running canonical bundle with no separately named old recovery permits
    // pre-commit cleanup.
    if verify_bundle_identity(&bundle, None).is_err() || old.exists() {
        log::warn!(
            "update transaction does not match the canonical bundle; preserving recovery evidence"
        );
        return;
    }

    let cleanup_stage = match transaction.phase {
        // Preparing was durable before ditto. A real directory at this exact
        // name is our potentially partial output; symlinks/non-directories are
        // never followed or removed.
        MacUpdatePhase::Preparing => match std::fs::symlink_metadata(&staged) {
            Ok(metadata) => metadata.is_dir() && !metadata.file_type().is_symlink(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                log::warn!("could not inspect update stage: {error}");
                return;
            }
        },
        // Ready was durable only after exact verification. If exchange already
        // happened, staged is the old app; its version will differ and it must
        // remain as recovery evidence.
        MacUpdatePhase::Ready => {
            staged.exists() && verify_bundle(&staged, &transaction.expected_version, None).is_ok()
        }
    };
    if cleanup_stage {
        if let Err(error) = std::fs::remove_dir_all(&staged) {
            log::warn!(
                "could not remove uncommitted update stage {}: {error}",
                staged.display()
            );
            return;
        }
    } else if staged.exists() {
        log::warn!("preserving an update stage that cannot be proven uncommitted");
        return;
    }
    if let Err(error) = remove_mac_update_transaction(parent) {
        log::warn!("{error}");
    }
}

#[cfg(not(target_os = "macos"))]
pub fn clear_update_leftovers() {}

#[cfg(any(test, target_os = "macos"))]
fn is_canonical_install_bundle(bundle: &std::path::Path) -> bool {
    bundle.file_name() == Some(std::ffi::OsStr::new(crate::community::BUNDLE_NAME))
}

/// Single-quote a path for `/bin/sh`. Applications folders are not usually
/// exotic, but "Macintosh HD" ships with a space in it and a user directory can
/// contain anything at all.
#[cfg(any(test, target_os = "macos"))]
fn shell_quote(p: &std::path::Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"))
}

/// Build the detached macOS relaunch protocol. The helper waits for the exact
/// parent birth stamp under a hard deadline, treats `open` success as a checked
/// condition rather than assuming it, retries transient LaunchServices
/// failures, and finally executes the verified binary directly.
#[cfg(any(test, target_os = "macos"))]
fn mac_relaunch_script(
    bundle: &std::path::Path,
    executable: &std::path::Path,
    workspace: &std::path::Path,
    parent_pid: u32,
) -> String {
    format!(
        // `kill -0 PID` alone can attach to an unrelated process after PID
        // reuse. Capture ps' process birth stamp while the parent is guaranteed
        // to still be alive (the caller waits only after spawning this helper),
        // and stop waiting if that identity changes. The attempt counter is an
        // absolute five-minute terminal bound even if identity inspection is
        // unavailable or the old process genuinely wedges during shutdown.
        "parent_pid={parent_pid}; \
         parent_birth=$(/bin/ps -p \"$parent_pid\" -o lstart= 2>/dev/null || true); \
         parent_waits=0; \
         while /bin/kill -0 \"$parent_pid\" 2>/dev/null; do \
           current_birth=$(/bin/ps -p \"$parent_pid\" -o lstart= 2>/dev/null || true); \
           if [ -n \"$parent_birth\" ] && [ \"$current_birth\" != \"$parent_birth\" ]; then break; fi; \
           parent_waits=$((parent_waits + 1)); \
           if [ \"$parent_waits\" -ge 1500 ]; then \
             /usr/bin/printf '%s\\n' 'parent-exit-timeout' > {} 2>/dev/null || true; \
             exit 75; \
           fi; \
           /bin/sleep 0.2; \
         done; \
         /bin/rm -rf -- {}; \
         for delay in 0 1 2; do \
           if [ \"$delay\" -ne 0 ]; then /bin/sleep \"$delay\"; fi; \
           if /usr/bin/open {}; then exit 0; fi; \
         done; \
         exec {}",
        shell_quote(&workspace.join("relaunch-failure.txt")),
        shell_quote(workspace),
        shell_quote(bundle),
        shell_quote(executable)
    )
}

/// Everything that is neither Windows nor macOS: hand the download to the
/// browser. There is no packaged Linux build to replace.
#[cfg(not(any(windows, target_os = "macos")))]
fn self_update(
    _expected_version: &str,
    url: &str,
    _want_sha: &str,
    _expected_size: u64,
    status: &RuntimeStatus,
    _proxy: &EventLoopProxy<UserEvent>,
) {
    open_url(url);
    *status.update_result.lock().unwrap() = Some((
        true,
        "The update download was opened in your browser.".to_string(),
    ));
}

pub(crate) fn open_url(url: &str) -> bool {
    if !url.starts_with("https://") {
        log::warn!("refusing to open a non-https URL: {url}");
        return false;
    }
    #[cfg(windows)]
    {
        let Ok(rundll32) = windows_directory(true).map(|dir| dir.join("rundll32.exe")) else {
            log::warn!("could not resolve the Windows system directory for URL handling");
            return false;
        };
        if !rundll32.is_file() {
            log::warn!("the system URL handler is missing");
            return false;
        }
        std::process::Command::new(rundll32)
            .args(["url.dll,FileProtocolHandler", url])
            .spawn()
            .is_ok()
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("/usr/bin/open")
            .arg(url)
            .spawn()
            .is_ok()
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .is_ok()
    }
}

/// Open the one support address exposed by the settings UI. The IPC caller
/// sends only the `support` enum value; no page-controlled URI reaches an OS
/// handler, avoiding `file:`, custom-protocol, or command-line surprises.
fn open_support_email() -> bool {
    const SUPPORT: &str = "mailto:support@vocalcode.app";
    #[cfg(windows)]
    {
        let Ok(rundll32) = windows_directory(true).map(|dir| dir.join("rundll32.exe")) else {
            log::warn!("could not resolve the Windows system directory for email handling");
            return false;
        };
        if !rundll32.is_file() {
            log::warn!("the system email handler is missing");
            return false;
        }
        std::process::Command::new(rundll32)
            .args(["url.dll,FileProtocolHandler", SUPPORT])
            .spawn()
            .is_ok()
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("/usr/bin/open")
            .arg(SUPPORT)
            .spawn()
            .is_ok()
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(SUPPORT)
            .spawn()
            .is_ok()
    }
}

/// The VocalCode mark: five bars of differing height, the same waveform the
/// recording indicator draws.
///
/// `(x centre, height)` as fractions of the glyph box. Kept identical to `BARS`
/// in `packaging/macos/make_icon.py`, which renders the .icns from the same
/// geometry — change one and the Dock icon stops matching the tray.
const BARS: [(f32, f32); 5] = [
    (0.22, 0.34),
    (0.36, 0.60),
    (0.50, 0.82),
    (0.64, 0.52),
    (0.78, 0.28),
];
const BAR_W: f32 = 0.085;

/// Coverage of the bar capsules at a point, in a `size`-px box. 0 outside,
/// 1 inside, fractional on the edge so callers get antialiasing for free.
fn bars_coverage(fx: f32, fy: f32, size: f32) -> f32 {
    let c = size / 2.0;
    let half_w = BAR_W * size / 2.0;
    let mut best = f32::MAX;
    for (cx, h) in BARS {
        let bx = cx * size;
        let bar_h = h * size;
        let (top, bottom) = (c - bar_h / 2.0, c + bar_h / 2.0);
        // Signed distance to a capsule: horizontal distance along the shaft,
        // radial distance past either cap.
        let dy = (fy - bottom).max(top - fy).max(0.0);
        let d = ((fx - bx).powi(2) + dy.powi(2)).sqrt() - half_w;
        best = best.min(d);
    }
    // One pixel of feathering across the boundary.
    (0.5 - best).clamp(0.0, 1.0)
}

/// The app icon — the waveform on a dark rounded tile. Used for the window and
/// the taskbar, and for the tray on platforms that want a full-colour icon.
fn icon_rgba() -> (Vec<u8>, u32, u32) {
    let (w, h) = (64u32, 64u32);
    let size = w as f32;
    let c = size / 2.0;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Rounded-square tile, 3px inset and 14px corner radius at 64px.
            let (inset, rad) = (3.0, 14.0);
            let dx = (fx - c).abs() - (c - inset - rad);
            let dy = (fy - c).abs() - (c - inset - rad);
            let corner = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() - rad;
            let inside = dx.max(dy).min(0.0) + corner.min(0.0) < 0.0;
            if !inside {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let t = bars_coverage(fx, fy, size);
            // Tile gradient, lighter at the top like every icon beside it.
            let shade = (fx / size * 0.25 + fy / size * 0.9).min(1.0);
            let mix = |a: f32, b: f32| (a + (b - a) * shade) as u8;
            let (r, g, b) = if t > 0.5 {
                (238, 138, 62) // amber bars
            } else {
                (mix(44.0, 18.0), mix(48.0, 20.0), mix(55.0, 24.0))
            };
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    (rgba, w, h)
}

/// The menu-bar glyph: the bare waveform in solid black, no tile.
///
/// macOS status items take a *template* image — the system throws the colour
/// away and re-renders it from the alpha channel, so it tracks the light/dark
/// menu bar, the highlight state and Reduce Transparency on its own. Handing it
/// the full-colour app icon instead would leave a small dark sticker pasted on
/// the menu bar, which is the tell that an app was not written for the Mac.
///
/// 36px = 18pt at 2x, the size Apple's own status items use.
#[cfg(target_os = "macos")]
fn tray_template_rgba() -> (Vec<u8>, u32, u32) {
    // A hair narrower than tall: the menu bar constrains height, and the
    // waveform is wider than it is tall, so it needs the room.
    let (w, h) = (36u32, 36u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let a = bars_coverage(x as f32 + 0.5, y as f32 + 0.5, w as f32);
            rgba.extend_from_slice(&[0, 0, 0, (a * 255.0) as u8]);
        }
    }
    (rgba, w, h)
}

fn make_icon() -> Icon {
    #[cfg(target_os = "macos")]
    let (rgba, w, h) = tray_template_rgba();
    #[cfg(not(target_os = "macos"))]
    let (rgba, w, h) = icon_rgba();
    Icon::from_rgba(rgba, w, h).expect("valid icon")
}

fn make_window_icon() -> tao::window::Icon {
    let (rgba, w, h) = icon_rgba();
    tao::window::Icon::from_rgba(rgba, w, h).expect("valid window icon")
}

// ---------------------------------------------------------------------------
// update verification
// ---------------------------------------------------------------------------

#[cfg(test)]
mod artifact_version_tests {
    use super::versions_match;

    #[test]
    fn artifact_versions_are_numeric_and_semantically_exact() {
        assert!(versions_match("0.5.2", "0.5.2").unwrap());
        assert!(versions_match("0.5.2.0", "0.5.2").unwrap());
        assert!(versions_match("0.5.0", "0.5.0").unwrap());
        assert!(versions_match("0.5.0.0", "0.5.0").unwrap());
        assert!(!versions_match("0.5.1", "0.5.2").unwrap());
        assert!(versions_match("latest", "0.5.2").is_err());
        assert!(versions_match("0.5.2.1", "0.5.2").is_err());
        assert!(versions_match("0.5.2.0.0", "0.5.2").is_err());
        assert!(versions_match("0.5.2.", "0.5.2").is_err());
        assert!(versions_match("0.5", "0.5.0").is_err());
    }
}

/// The one-click update runs a downloaded executable, so these guard the only
/// thing standing between the CDN and code execution. Two halves, tested
/// differently: [`read_verdict`] is pure and covers every way we could reject an
/// installer that is in fact ours (the failure that hides, because it degrades
/// to the download page instead of raising), while the [`verify_authenticode`]
/// tests need real bytes on disk and so are gated on `VC_TEST_INSTALLER`
/// pointing at a signed installer. Run them as:
///
///     $env:VC_TEST_INSTALLER = "<path to VocalCodeSetup.exe>"
///     cargo test -p vocalcode-app signature
#[cfg(all(test, windows))]
mod signature_tests {
    use super::{
        read_signed_artifact_identity, read_signed_artifact_version, read_verdict,
        verify_authenticode, SIGNER_SUBJECT,
    };

    const OURS: &str = "CN=Daming Wu, O=Daming Wu, L=Newberry, S=FL, C=US";

    #[test]
    fn accepts_our_signature() {
        assert_eq!(read_verdict(&format!("Valid\r\n{OURS}\r\n")).unwrap(), OURS);
    }

    /// PowerShell writing UTF-16 or a BOM must not read as "not Valid". A BOM is
    /// not whitespace, so `trim` alone leaves it attached to `Valid`.
    #[test]
    fn tolerates_bom_and_blank_lines() {
        assert!(read_verdict(&format!("\u{feff}Valid\r\n\r\n  {OURS}  \r\n")).is_ok());
        assert!(read_verdict(&format!("\n\nValid\n{OURS}")).is_ok());
    }

    #[test]
    fn rejects_a_bad_status() {
        let e = read_verdict(&format!("HashMismatch\r\n{OURS}")).unwrap_err();
        assert!(e.contains("HashMismatch"), "{e}");
        assert!(read_verdict("NotSigned\r\n\r\n").is_err());
    }

    /// A chain-valid signature only means *somebody* legitimate signed it.
    #[test]
    fn rejects_another_signer() {
        let e = read_verdict("Valid\r\nCN=Microsoft Windows, O=Microsoft Corporation").unwrap_err();
        assert!(e.contains("signed by someone else"), "{e}");
        assert!(!SIGNER_SUBJECT.is_empty());
    }

    #[test]
    fn rejects_a_subject_that_only_contains_our_name() {
        assert!(read_verdict(
            "Valid\r\nCN=Evil Daming Wu Test, O=Someone Else, L=Newberry, S=FL, C=US"
        )
        .is_err());
    }

    /// Empty output — PowerShell present but silent — must not pass.
    #[test]
    fn rejects_nothing_at_all() {
        assert!(read_verdict("").is_err());
        assert!(read_verdict("Valid\r\n").is_err());
    }

    #[test]
    fn signed_artifact_version_must_match_the_manifest() {
        let output = format!("Valid\r\n{OURS}\r\n0.5.2.0\r\n");
        assert_eq!(
            read_signed_artifact_version(&output, "0.5.2").unwrap(),
            "0.5.2.0"
        );
        let error = read_signed_artifact_version(&output, "0.5.3").unwrap_err();
        assert!(error.contains("does not match"), "{error}");
        assert!(read_signed_artifact_version(&format!("Valid\r\n{OURS}\r\n"), "0.5.2").is_err());
    }

    #[test]
    fn signed_installer_identity_is_exact() {
        let product = crate::community::DATA_DIR_NAME;
        let filename = if crate::community::ENABLED {
            "VocalCodeCommunitySetup.exe"
        } else {
            "VocalCodeSetup.exe"
        };
        let output = format!("Valid\r\n{OURS}\r\n0.5.2.0\r\n{product}\r\n{filename}\r\n");
        assert!(read_signed_artifact_identity(&output).is_ok());
        assert!(read_signed_artifact_identity(&output.replace(filename, "Other.exe")).is_err());
        assert!(read_signed_artifact_identity(&output.replace(product, "Other")).is_err());
    }

    fn installer() -> Option<std::path::PathBuf> {
        let required = std::env::var_os("VC_REQUIRE_TEST_INSTALLER").is_some();
        let Some(path) = std::env::var_os("VC_TEST_INSTALLER") else {
            assert!(!required, "release verification requires VC_TEST_INSTALLER");
            return None;
        };
        let path = std::path::PathBuf::from(path);
        assert!(
            !required || path.is_file(),
            "VC_TEST_INSTALLER does not name a file: {}",
            path.display()
        );
        path.is_file().then_some(path)
    }

    /// The end-to-end verdict on a real released installer: this is the check
    /// that had compiled but never run.
    #[test]
    fn real_installer_verifies() {
        let Some(p) = installer() else {
            eprintln!("skipped: set VC_TEST_INSTALLER to a signed installer");
            return;
        };
        verify_authenticode(&p, env!("CARGO_PKG_VERSION"), None)
            .expect("the released installer should verify");
    }

    /// Flip one byte and the same call must refuse it. Proves the check is load
    /// bearing rather than always returning Ok.
    #[test]
    fn tampered_installer_is_refused() {
        let Some(p) = installer() else {
            eprintln!("skipped: set VC_TEST_INSTALLER to a signed installer");
            return;
        };
        let mut bytes = std::fs::read(&p).unwrap();
        // Well past the headers, so the file still parses as a PE and it is the
        // signature that objects rather than the loader.
        let at = bytes.len() / 2;
        bytes[at] ^= 0xff;
        let tampered = std::env::temp_dir().join("vc-tampered-installer.exe");
        std::fs::write(&tampered, &bytes).unwrap();
        let got = verify_authenticode(&tampered, env!("CARGO_PKG_VERSION"), None);
        let _ = std::fs::remove_file(&tampered);
        assert!(got.is_err(), "a modified installer must not verify");
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_update_tests {
    use super::*;

    /// Real `spctl -a -t exec -vv` output for a build of ours. Every way this
    /// rejects a good bundle degrades to "the update failed" on a machine where
    /// nothing is wrong, so the shapes it must accept are pinned here.
    const ACCEPTED: &str = "/Applications/VocalCode.app: accepted\n\
         source=Notarized Developer ID\n\
         origin=Developer ID Application: Daming Wu (58Y98W3QQK)\n";

    #[test]
    fn a_notarised_bundle_is_accepted() {
        assert!(read_gatekeeper_verdict(ACCEPTED).is_ok());
    }

    #[test]
    fn an_unsigned_or_rejected_bundle_is_refused() {
        let rejected = "/tmp/X.app: rejected\nsource=no usable signature\n";
        assert!(read_gatekeeper_verdict(rejected).is_err());
        assert!(
            read_gatekeeper_verdict("").is_err(),
            "no verdict is not consent"
        );
    }

    /// Signed and valid, but by somebody else — the case a hash check cannot
    /// see, because whoever served the dmg also served the hash.
    #[test]
    fn a_valid_signature_from_another_developer_is_refused() {
        let other = "/tmp/X.app: accepted\n\
             source=Developer ID\n\
             origin=Developer ID Application: Someone Else (ABCDE12345)\n";
        assert!(
            read_gatekeeper_verdict(other).is_err(),
            "accepted but not notarised"
        );
    }

    #[test]
    fn team_id_is_pinned() {
        let ours = "Identifier=app.vocalcode.VocalCode\nTeamIdentifier=58Y98W3QQK\n";
        assert_eq!(read_team_id(ours).unwrap(), TEAM_ID);
        let theirs = "Identifier=app.vocalcode.VocalCode\nTeamIdentifier=ABCDE12345\n";
        assert!(read_team_id(theirs).is_err());
        // adhoc signatures report this instead of a team
        assert!(read_team_id("TeamIdentifier=not set\n").is_err());
        assert!(
            read_team_id("Identifier=x\n").is_err(),
            "no Team ID is not our Team ID"
        );
    }

    /// A space in the path is ordinary ("Macintosh HD", any user folder), and an
    /// unquoted one would relaunch nothing after the app had already replaced
    /// itself and exited — an update that looks like a crash.
    #[test]
    fn paths_survive_the_shell() {
        assert_eq!(
            shell_quote(std::path::Path::new("/Applications/VocalCode.app")),
            "'/Applications/VocalCode.app'"
        );
        assert_eq!(
            shell_quote(std::path::Path::new("/Users/a b/My Apps/VocalCode.app")),
            "'/Users/a b/My Apps/VocalCode.app'"
        );
        let tricky = shell_quote(std::path::Path::new("/tmp/it's here/VocalCode.app"));
        assert_eq!(tricky, r"'/tmp/it'\''s here/VocalCode.app'");
    }

    /// The path that must never silently "succeed": replacing the throwaway copy
    /// macOS makes when an app is run from the dmg or from Downloads changes
    /// nothing the user will ever open again.
    #[test]
    fn a_translocated_copy_is_refused_by_name() {
        let p =
            std::path::Path::new("/private/var/folders/xq/T/AppTranslocation/35FD/d/VocalCode.app");
        assert!(p.components().any(|c| c.as_os_str() == "AppTranslocation"));
    }

    #[test]
    fn launch_agent_paths_are_xml_escaped() {
        assert_eq!(
            xml_escape("/Users/A & B/<VocalCode>.app/'\""),
            "/Users/A &amp; B/&lt;VocalCode&gt;.app/&apos;&quot;"
        );
    }
}

#[cfg(test)]
mod microphone_picker_contract_tests {
    use super::*;

    fn device(
        selector: &str,
        label: &str,
        legacy_name: &str,
    ) -> vocalcode_platform::InputDeviceChoice {
        vocalcode_platform::InputDeviceChoice {
            selector: selector.to_string(),
            label: label.to_string(),
            legacy_name: legacy_name.to_string(),
        }
    }

    #[test]
    fn init_payload_separates_stable_device_values_from_human_labels() {
        let devices = vec![
            device(
                "vocalcode-cpal-device:opaque-a",
                "Conference microphone \u{b7} 1/2",
                "Conference microphone",
            ),
            device(
                "vocalcode-cpal-device:opaque-b",
                "Conference microphone \u{b7} 2/2",
                "Conference microphone",
            ),
        ];
        let payload: Value =
            serde_json::from_str(&init_config_json(&Config::default(), &devices, None, None))
                .unwrap();

        assert_eq!(payload["devices"][0]["selector"], devices[0].selector);
        assert_eq!(payload["devices"][0]["label"], devices[0].label);
        assert_eq!(payload["devices"][0]["legacy_name"], devices[0].legacy_name);
        assert_eq!(payload["input_device"], "");
    }

    #[test]
    fn a_legacy_device_name_migrates_only_when_it_is_unique() {
        let unique = vec![device(
            "vocalcode-cpal-device:unique",
            "Desk microphone",
            "Old microphone name",
        )];
        assert_eq!(
            input_device_for_init(Some("Old microphone name"), &unique),
            "vocalcode-cpal-device:unique"
        );

        let duplicate = vec![
            device(
                "vocalcode-cpal-device:first",
                "Headset \u{b7} 1/2",
                "Headset",
            ),
            device(
                "vocalcode-cpal-device:second",
                "Headset \u{b7} 2/2",
                "Headset",
            ),
        ];
        assert_eq!(
            input_device_for_init(Some("Headset"), &duplicate),
            "Headset"
        );
    }

    #[test]
    fn current_or_temporarily_missing_stable_selector_is_preserved_exactly() {
        let present = device(
            "vocalcode-cpal-device:wireless",
            "Wireless microphone",
            "Wireless microphone",
        );
        assert_eq!(
            input_device_for_init(Some(&present.selector), std::slice::from_ref(&present)),
            present.selector
        );
        assert_eq!(
            input_device_for_init(Some("vocalcode-cpal-device:offline"), &[]),
            "vocalcode-cpal-device:offline"
        );
    }

    #[test]
    fn html_never_renders_an_unavailable_opaque_selector_as_its_label() {
        let html = include_str!("webui.html");
        assert!(html.contains("o.value=d.selector"));
        assert!(html.contains("o.textContent=typeof d.label"));
        assert!(!html.contains("o.textContent=d.selector"));
        assert!(html.contains(
            "unavailable.textContent=t(\"Previously selected microphone (unavailable)\")"
        ));
        assert!(html.contains("option[data-unavailable='true']"));
        assert!(html.contains("unavailable.dataset.unavailable=\"true\""));
        assert_eq!(
            html.matches("\"Previously selected microphone (unavailable)\":")
                .count(),
            4
        );
    }
}

#[cfg(test)]
mod config_apply_contract_tests {
    use super::*;

    fn configured() -> Config {
        Config {
            language: "en".to_string(),
            onboarded: true,
            ..Config::default()
        }
    }

    fn wait_for_worker_reap(status: &RuntimeStatus, teach: bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let workers = if teach {
                &status.teach_workers
            } else {
                &status.service_workers
            };
            reap_tracked_workers(workers, if teach { "Teach test" } else { "IPC test" });
            if workers.lock().unwrap().is_empty() {
                return;
            }
            assert!(Instant::now() < deadline, "finished worker was not reaped");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_update_reap(status: &RuntimeStatus) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            reap_finished_update_worker(status);
            let empty = status
                .update_worker
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_none();
            if empty {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "finished update worker was not reaped"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn service_worker_capacity_is_hard_bounded_and_recovers_after_reap() {
        let status = Arc::new(RuntimeStatus::default());
        let barrier = Arc::new(std::sync::Barrier::new(SERVICE_WORKER_LIMIT + 1));
        for _ in 0..SERVICE_WORKER_LIMIT {
            let barrier = barrier.clone();
            spawn_service_worker(&status, "vocalcode-service-cap-test", move || {
                barrier.wait();
            })
            .unwrap();
        }
        assert_eq!(
            status.service_workers.lock().unwrap().len(),
            SERVICE_WORKER_LIMIT
        );
        let error =
            spawn_service_worker(&status, "vocalcode-service-overflow-test", || {}).unwrap_err();
        assert!(error.contains("worker limit"), "{error}");
        assert_eq!(
            status.service_workers.lock().unwrap().len(),
            SERVICE_WORKER_LIMIT
        );

        barrier.wait();
        wait_for_worker_reap(&status, false);
        spawn_service_worker(&status, "vocalcode-service-after-reap-test", || {}).unwrap();
        wait_for_worker_reap(&status, false);
    }

    #[test]
    fn completed_service_and_teach_handles_do_not_accumulate_over_time() {
        let status = Arc::new(RuntimeStatus::default());
        for _ in 0..100 {
            let (service_tx, service_rx) = std::sync::mpsc::channel();
            spawn_service_worker(&status, "vocalcode-service-repeat-test", move || {
                service_tx.send(()).unwrap();
            })
            .unwrap();
            service_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            wait_for_worker_reap(&status, false);

            let (teach_tx, teach_rx) = std::sync::mpsc::channel();
            spawn_teach_worker(&status, move || {
                teach_tx.send(()).unwrap();
            })
            .unwrap();
            teach_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            wait_for_worker_reap(&status, true);

            assert!(status.service_workers.lock().unwrap().len() <= SERVICE_WORKER_LIMIT);
            assert!(status.teach_workers.lock().unwrap().len() <= TEACH_WORKER_LIMIT);
        }
    }

    #[test]
    fn teach_worker_capacity_is_hard_bounded() {
        let status = Arc::new(RuntimeStatus::default());
        let barrier = Arc::new(std::sync::Barrier::new(TEACH_WORKER_LIMIT + 1));
        for _ in 0..TEACH_WORKER_LIMIT {
            let barrier = barrier.clone();
            spawn_teach_worker(&status, move || {
                barrier.wait();
            })
            .unwrap();
        }
        let error = spawn_teach_worker(&status, || {}).unwrap_err();
        assert!(error.contains("worker limit"), "{error}");
        barrier.wait();
        wait_for_worker_reap(&status, true);
    }

    #[test]
    fn each_repeatable_network_ipc_has_an_independent_single_flight_gate() {
        let status = Arc::new(RuntimeStatus::default());
        for operation in [
            IpcSingleFlight::Activation,
            IpcSingleFlight::Restore,
            IpcSingleFlight::UpdateCheck,
        ] {
            let first = IpcSingleFlightReset::claim(&status, operation).unwrap();
            assert!(IpcSingleFlightReset::claim(&status, operation).is_none());
            drop(first);
            let next = IpcSingleFlightReset::claim(&status, operation).unwrap();
            drop(next);
        }

        let activation = IpcSingleFlightReset::claim(&status, IpcSingleFlight::Activation).unwrap();
        let restore = IpcSingleFlightReset::claim(&status, IpcSingleFlight::Restore).unwrap();
        let update = IpcSingleFlightReset::claim(&status, IpcSingleFlight::UpdateCheck).unwrap();
        assert!(IpcSingleFlightReset::claim(&status, IpcSingleFlight::Activation).is_none());
        assert!(IpcSingleFlightReset::claim(&status, IpcSingleFlight::Restore).is_none());
        assert!(IpcSingleFlightReset::claim(&status, IpcSingleFlight::UpdateCheck).is_none());
        drop((activation, restore, update));

        *status.activation.lock().unwrap() = Some((true, "authoritative result".to_string()));
        publish_activation_if_empty(&status, "duplicate click");
        assert_eq!(
            status.activation.lock().unwrap().as_ref().unwrap().1,
            "authoritative result"
        );
    }

    #[test]
    fn single_flight_resets_after_spawn_failure_panic_and_early_return() {
        let status = Arc::new(RuntimeStatus::default());

        let activation = IpcSingleFlightReset::claim(&status, IpcSingleFlight::Activation).unwrap();
        status.shutdown.store(true, Ordering::Release);
        assert!(
            spawn_service_worker(&status, "vocalcode-reset-spawn-failure", move || {
                let _activation = activation;
            })
            .is_err()
        );
        assert!(!status.activation_in_progress.load(Ordering::Acquire));
        status.shutdown.store(false, Ordering::Release);

        let restore = IpcSingleFlightReset::claim(&status, IpcSingleFlight::Restore).unwrap();
        spawn_service_worker(&status, "vocalcode-reset-panic", move || {
            let _restore = restore;
            panic!("intentional single-flight reset test panic");
        })
        .unwrap();
        wait_for_worker_reap(&status, false);
        assert!(!status.restore_in_progress.load(Ordering::Acquire));

        let update = IpcSingleFlightReset::claim(&status, IpcSingleFlight::UpdateCheck).unwrap();
        spawn_service_worker(&status, "vocalcode-reset-return", move || {
            let _update = update;
        })
        .unwrap();
        wait_for_worker_reap(&status, false);
        assert!(!status.update_check_in_progress.load(Ordering::Acquire));
    }

    #[test]
    fn startup_and_manual_update_checks_share_one_tracked_single_flight() {
        let status = Arc::new(RuntimeStatus::default());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let worker_barrier = barrier.clone();
        assert_eq!(
            spawn_update_check(&status, "vocalcode-startup-check-test", move || {
                worker_barrier.wait();
            })
            .unwrap(),
            UpdateCheckStart::Started
        );
        assert_eq!(
            spawn_update_check(&status, "vocalcode-manual-check-test", || {}).unwrap(),
            UpdateCheckStart::Busy
        );
        assert_eq!(status.service_workers.lock().unwrap().len(), 1);

        barrier.wait();
        wait_for_worker_reap(&status, false);
        assert!(!status.update_check_in_progress.load(Ordering::Acquire));
        assert_eq!(
            spawn_update_check(&status, "vocalcode-later-check-test", || {}).unwrap(),
            UpdateCheckStart::Started
        );
        wait_for_worker_reap(&status, false);

        status.shutdown.store(true, Ordering::Release);
        assert!(spawn_update_check(&status, "vocalcode-check-shutdown-test", || {}).is_err());
        assert!(!status.update_check_in_progress.load(Ordering::Acquire));
    }

    #[test]
    fn shutdown_and_spawn_race_never_leaves_a_registered_owner_unjoined() {
        for _ in 0..50 {
            let status = Arc::new(RuntimeStatus::default());
            let gate = Arc::new(std::sync::Barrier::new(2));
            let completed = Arc::new(AtomicBool::new(false));
            let spawn_status = status.clone();
            let spawn_gate = gate.clone();
            let worker_completed = completed.clone();
            let spawner = std::thread::spawn(move || {
                spawn_gate.wait();
                spawn_service_worker(&spawn_status, "vocalcode-shutdown-race-test", move || {
                    worker_completed.store(true, Ordering::Release);
                })
            });

            gate.wait();
            status.shutdown.store(true, Ordering::Release);
            join_service_workers(&status);
            let spawn_result = spawner.join().unwrap();
            assert!(status.service_workers.lock().unwrap().is_empty());
            if spawn_result.is_ok() {
                assert!(completed.load(Ordering::Acquire));
            } else {
                assert!(!completed.load(Ordering::Acquire));
            }
        }
    }

    #[test]
    fn update_worker_never_overwrites_a_live_registered_owner() {
        let status = Arc::new(RuntimeStatus::default());
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let live = std::thread::spawn(move || release_rx.recv().unwrap());
        *status.update_worker.lock().unwrap() = Some(live);
        // Exercise the narrow interval after a worker clears its atomic but
        // before its registered JoinHandle has become finished.
        status.update_in_progress.store(false, Ordering::Release);
        let invoked = Arc::new(AtomicBool::new(false));
        let worker_invoked = invoked.clone();
        assert_eq!(
            spawn_update_worker(&status, move || {
                worker_invoked.store(true, Ordering::Release);
            })
            .unwrap(),
            UpdateInstallStart::Busy
        );
        assert!(!invoked.load(Ordering::Acquire));
        assert!(status.update_worker.lock().unwrap().is_some());
        assert!(!status.update_in_progress.load(Ordering::Acquire));

        release_tx.send(()).unwrap();
        wait_for_update_reap(&status);
    }

    #[test]
    fn completed_update_workers_are_reaped_before_replacement_and_do_not_accumulate() {
        let status = Arc::new(RuntimeStatus::default());
        for _ in 0..100 {
            let (finished_tx, finished_rx) = std::sync::mpsc::channel();
            assert_eq!(
                spawn_update_worker(&status, move || {
                    finished_tx.send(()).unwrap();
                })
                .unwrap(),
                UpdateInstallStart::Started
            );
            finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            wait_for_update_reap(&status);
            assert!(!status.update_in_progress.load(Ordering::Acquire));
        }
    }

    #[test]
    fn poisoned_update_worker_mutex_is_recovered_and_remains_tracked() {
        let status = Arc::new(RuntimeStatus::default());
        let poison_status = status.clone();
        assert!(std::thread::spawn(move || {
            let _guard = poison_status.update_worker.lock().unwrap();
            panic!("intentional update-worker mutex poison test");
        })
        .join()
        .is_err());

        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        assert_eq!(
            spawn_update_worker(&status, move || {
                finished_tx.send(()).unwrap();
            })
            .unwrap(),
            UpdateInstallStart::Started
        );
        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        wait_for_update_reap(&status);
        assert!(!status.update_in_progress.load(Ordering::Acquire));
    }

    #[test]
    fn update_worker_reset_survives_panic_and_shutdown_rejection() {
        let status = Arc::new(RuntimeStatus::default());
        assert_eq!(
            spawn_update_worker(&status, || panic!("intentional update worker panic")).unwrap(),
            UpdateInstallStart::Started
        );
        wait_for_update_reap(&status);
        assert!(!status.update_in_progress.load(Ordering::Acquire));

        status.shutdown.store(true, Ordering::Release);
        assert!(spawn_update_worker(&status, || {}).is_err());
        assert!(!status.update_in_progress.load(Ordering::Acquire));
        assert!(status.update_worker.lock().unwrap().is_none());
    }

    #[test]
    fn update_reaper_never_joins_an_unfinished_worker() {
        let status = Arc::new(RuntimeStatus::default());
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *status.update_worker.lock().unwrap() =
            Some(std::thread::spawn(move || release_rx.recv().unwrap()));
        assert!(!reap_finished_update_worker(&status));
        assert!(status.update_worker.lock().unwrap().is_some());
        release_tx.send(()).unwrap();
        wait_for_update_reap(&status);
    }

    #[test]
    fn ui_event_loop_returns_through_the_orderly_shutdown_boundary() {
        let source = include_str!("webui.rs");
        assert!(source.contains(concat!("event_loop.", "run_return(")));
        assert!(!source.contains(concat!("event_loop.", "run(")));
        let boundary = source
            .find(concat!("let exit_code = event_loop.", "run_return("))
            .unwrap();
        let after_boundary = &source[boundary..];
        let shutdown = after_boundary
            .find(concat!("shutdown_status.shutdown.", "store(true"))
            .unwrap();
        let stop_and_join = after_boundary
            .find(concat!("capture_waker.", "stop_and_join()"))
            .unwrap();
        assert!(shutdown < stop_and_join);
    }

    #[test]
    fn stop_join_guard_joins_on_early_return_and_unwind() {
        fn simulate_early_return(completed: Arc<AtomicBool>) -> Result<(), ()> {
            let stop = Arc::new(AtomicBool::new(false));
            let worker_stop = stop.clone();
            let worker = std::thread::spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                completed.store(true, Ordering::Release);
            });
            let _guard = StopJoinGuard::new(stop, worker, "early-return test worker");
            Err(())
        }

        let early_completed = Arc::new(AtomicBool::new(false));
        assert!(simulate_early_return(early_completed.clone()).is_err());
        assert!(early_completed.load(Ordering::Acquire));

        let unwind_completed = Arc::new(AtomicBool::new(false));
        let worker_completed = unwind_completed.clone();
        let unwind = std::panic::catch_unwind(move || {
            let stop = Arc::new(AtomicBool::new(false));
            let worker_stop = stop.clone();
            let worker = std::thread::spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                worker_completed.store(true, Ordering::Release);
            });
            let _guard = StopJoinGuard::new(stop, worker, "unwind test worker");
            panic!("intentional guard unwind test");
        });
        assert!(unwind.is_err());
        assert!(unwind_completed.load(Ordering::Acquire));
    }

    #[test]
    fn config_results_queue_without_overwriting_and_keep_authoritative_snapshots() {
        let status = RuntimeStatus::default();
        let first = Config {
            language: "en".to_string(),
            ..Config::default()
        };
        let mut second = first.clone();
        second.language = "zh".to_string();

        queue_config_result(&status, 1, false, false, "first", &first);
        queue_config_result(&status, 2, true, true, "second", &second);

        let mut results = status.config_results.lock().unwrap();
        assert_eq!(results.len(), 2);
        let first_result = results.pop_front().unwrap();
        let second_result = results.pop_front().unwrap();
        assert_eq!(first_result.request_id, 1);
        assert!(!first_result.generation_bound);
        assert_eq!(first_result.authoritative.language, "en");
        assert_eq!(second_result.request_id, 2);
        assert!(second_result.generation_bound);
        assert_eq!(second_result.authoritative.language, "zh");
        assert_eq!(
            config_snapshot_for_page(&Config::default())["input_device"],
            ""
        );
    }

    #[test]
    fn updater_progress_has_priority_without_erasing_model_progress() {
        let status = RuntimeStatus::default();
        let model = ("model".to_string(), 25.0, 10.0, 40.0);
        let update = ("update".to_string(), 50.0, 20.0, 40.0);
        *status.model_download.lock().unwrap() = Some(model.clone());
        *status.update_download.lock().unwrap() = Some(update.clone());

        assert_eq!(visible_download(&status), Some(update));
        *status.update_download.lock().unwrap() = None;
        assert_eq!(visible_download(&status), Some(model));
    }

    #[test]
    fn page_sends_new_desired_snapshots_without_waiting_for_runtime_ack() {
        let html = include_str!("webui.html");
        assert!(!html.contains("configInFlight"));
        assert!(!html.contains("configSaveQueued"));
        assert!(html.contains("configLatest={id:id,base:base,snapshot:snapshot}"));
        assert!(html.contains(
            "window.vocalcodeConfigResult = function(id, ok, generationBound, msg, authoritative)"
        ));
        assert!(html.contains("var completed=configRequests[id]"));
        assert!(html.contains("if(Number(pendingId)<id) delete configRequests[pendingId]"));
        assert!(html.contains("rollbackRejected(host,completed.snapshot)"));
        assert!(html.contains("reconcileAccepted(host,completed.snapshot)"));
        assert!(html.contains("if(reconciled) renderConfigState()"));
    }

    #[test]
    fn config_bounds_are_rejected_before_persistence_or_os_changes() {
        let cfg = Mutex::new(configured());
        let persisted = std::cell::Cell::new(false);
        let os_changed = std::cell::Cell::new(false);
        let error = apply_save_with(
            &cfg,
            &serde_json::json!({
                "input_device": "d".repeat(MAX_INPUT_DEVICE_UTF8_BYTES + 1)
            }),
            |_, _| {
                persisted.set(true);
                Ok(())
            },
            |_| {
                os_changed.set(true);
                Ok(())
            },
            || false,
        )
        .unwrap_err();

        assert!(error.contains("input_device"), "{error}");
        assert!(!persisted.get());
        assert!(!os_changed.get());
        assert_eq!(cfg.lock().unwrap().input_device, None);
    }

    #[test]
    fn meeting_reminder_preferences_round_trip_through_the_config_transaction() {
        let cfg = Mutex::new(configured());
        let saved = apply_save_with(
            &cfg,
            &serde_json::json!({
                "smart_meeting_reminders": true,
                "noise_filter": true,
                "ignored_meeting_apps": [
                    "Browser:Chrome.exe:Google-Meet",
                    "browser:chrome.exe:google-meet"
                ]
            }),
            |_, _| Ok(()),
            |_| Ok(()),
            || false,
        )
        .unwrap();

        assert!(saved.smart_meeting_reminders);
        assert!(saved.noise_filter);
        assert_eq!(
            saved.ignored_meeting_apps,
            ["browser:chrome.exe:google-meet"]
        );
        let snapshot = config_snapshot_for_page(&saved);
        assert_eq!(snapshot["smart_meeting_reminders"], true);
        assert_eq!(snapshot["noise_filter"], true);
        assert_eq!(
            snapshot["ignored_meeting_apps"][0],
            "browser:chrome.exe:google-meet"
        );
    }

    #[test]
    fn save_ipc_only_enqueues_and_never_runs_devices_or_disk_on_the_ui_thread() {
        let source = include_str!("webui.rs");
        let start = source.find("Some(\"save\") => {").unwrap();
        let end = source[start..].find("Some(\"setlang\") => {").unwrap() + start;
        let branch = &source[start..end];
        assert!(branch.contains("config_saves"));
        assert!(branch.contains(".try_send(ConfigSaveRequest"));
        assert!(!branch.contains("apply_save("));
        assert!(!branch.contains("CpalAudioCapture"));
    }

    #[test]
    fn config_worker_persists_requests_in_page_arrival_order() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "vocalcode-config-fifo-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let initial = configured();
        std::fs::write(
            base.join("vocalcode.toml"),
            toml::to_string_pretty(&initial).unwrap(),
        )
        .unwrap();
        let cfg = Arc::new(Mutex::new(initial.clone()));
        let status = Arc::new(RuntimeStatus::default());
        let sender = start_config_save_worker(&status, &cfg, &base).unwrap();

        sender
            .send(ConfigSaveRequest {
                request_id: 1,
                config: serde_json::json!({ "live_caption": !initial.live_caption }),
            })
            .unwrap();
        sender
            .send(ConfigSaveRequest {
                request_id: 2,
                config: serde_json::json!({ "live_caption": initial.live_caption }),
            })
            .unwrap();
        drop(sender);
        join_service_workers(&status);

        assert_eq!(cfg.lock().unwrap().live_caption, initial.live_caption);
        let durable: Config =
            toml::from_str(&std::fs::read_to_string(base.join("vocalcode.toml")).unwrap()).unwrap();
        assert_eq!(durable.live_caption, initial.live_caption);
        let apply = status.config_apply.lock().unwrap();
        assert_eq!(apply.generation, 2);
        assert_eq!(apply.pending.as_ref().unwrap().request_id, 2);
        drop(apply);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn shutdown_joins_a_stalled_network_owner_without_late_status_publication() {
        let status = Arc::new(RuntimeStatus::default());
        let worker_status = status.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        spawn_service_worker(&status, "vocalcode-cancel-owner-test", move || {
            let result = crate::activation::cancellable_network_request(
                &worker_status.shutdown,
                "vocalcode-cancel-request-test",
                move || {
                    started_tx.send(()).unwrap();
                    let _ = release_rx.recv_timeout(Duration::from_secs(2));
                    Ok((true, "must not publish".to_string()))
                },
            );
            if let Ok(crate::activation::CancellableRequest::Completed(value)) = result {
                if !worker_status.shutdown.load(Ordering::Acquire) {
                    *worker_status.activation.lock().unwrap() = Some(value);
                }
            }
        })
        .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        status.shutdown.store(true, Ordering::Release);
        let started = Instant::now();
        join_service_workers(&status);

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(status.activation.lock().unwrap().is_none());
        let _ = release_tx.send(());
    }

    #[test]
    fn persistence_failure_is_queued_before_the_older_generation_can_finish() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "vocalcode-config-result-order-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let initial = configured();
        let mut external = initial.clone();
        external.cue_sounds = !initial.cue_sounds;
        std::fs::write(
            base.join("vocalcode.toml"),
            toml::to_string_pretty(&external).unwrap(),
        )
        .unwrap();
        let cfg = Arc::new(Mutex::new(initial.clone()));
        let status = Arc::new(RuntimeStatus::default());
        status
            .config_apply
            .lock()
            .unwrap()
            .publish(1, initial.clone());
        let sender = start_config_save_worker(&status, &cfg, &base).unwrap();

        // Stop the worker exactly while it is trying to enqueue B's disk
        // failure. It must still own config_apply at this point, preventing A's
        // engine result from overtaking it in the result queue.
        let result_guard = status.config_results.lock().unwrap();
        sender
            .send(ConfigSaveRequest {
                request_id: 2,
                config: serde_json::json!({ "live_caption": !initial.live_caption }),
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match status.config_apply.try_lock() {
                Err(std::sync::TryLockError::WouldBlock) => break,
                Err(std::sync::TryLockError::Poisoned(_)) => panic!("config lock poisoned"),
                Ok(guard) => {
                    drop(guard);
                    assert!(Instant::now() < deadline, "save worker did not start");
                    std::thread::yield_now();
                }
            }
        }

        let engine_status = status.clone();
        let engine_authoritative = initial.clone();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let engine = std::thread::spawn(move || {
            let apply = engine_status.config_apply.lock().unwrap();
            acquired_tx.send(()).unwrap();
            queue_config_result(
                &engine_status,
                1,
                false,
                true,
                "older model failed",
                &engine_authoritative,
            );
            drop(apply);
        });
        assert!(
            acquired_rx.recv_timeout(Duration::from_millis(75)).is_err(),
            "older generation overtook the queued persistence failure"
        );

        drop(result_guard);
        acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        engine.join().unwrap();
        drop(sender);
        join_service_workers(&status);

        let results = status.config_results.lock().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].request_id, 2);
        assert!(!results[0].generation_bound);
        assert_eq!(results[1].request_id, 1);
        assert!(results[1].generation_bound);
        drop(results);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn microphone_choice_is_persisted_for_one_engine_stage_without_a_ui_probe() {
        let cfg = Mutex::new(configured());
        let desired = "vocalcode-cpal-device:temporarily-offline";
        let persisted = std::cell::RefCell::new(None);
        let autostart_calls = std::cell::Cell::new(0_u32);
        let saved = apply_save_with(
            &cfg,
            &serde_json::json!({ "input_device": desired }),
            |_, next| {
                *persisted.borrow_mut() = next.input_device.clone();
                Ok(())
            },
            |_| {
                autostart_calls.set(autostart_calls.get() + 1);
                Ok(())
            },
            || false,
        )
        .unwrap();

        assert_eq!(saved.input_device.as_deref(), Some(desired));
        assert_eq!(persisted.borrow().as_deref(), Some(desired));
        assert_eq!(autostart_calls.get(), 0);
    }

    #[test]
    fn changing_language_clears_a_conflicting_explicit_model_override() {
        let mut current = configured();
        current.model = "paraformer-zh".to_string();
        let cfg = Mutex::new(current);
        let persisted = std::cell::RefCell::new(None);
        let saved = apply_save_with(
            &cfg,
            &serde_json::json!({
                "language": "fr",
                // A full-page save may still carry the stale value. The
                // language control is authoritative when it changes.
                "model": "paraformer-zh"
            }),
            |_, next| {
                *persisted.borrow_mut() = Some(next.clone());
                Ok(())
            },
            |_| Ok(()),
            || false,
        )
        .unwrap();

        assert_eq!(saved.language, "fr");
        assert!(saved.model.is_empty());
        assert_eq!(
            crate::models::route_for(&saved.model, &saved.language),
            Some(crate::models::Route::Single("parakeet-tdt-v3"))
        );
        assert!(persisted.borrow().as_ref().unwrap().model.is_empty());
    }

    #[test]
    fn changing_language_accepts_only_a_model_exposed_for_that_language() {
        let cfg = Mutex::new(configured());
        let saved = apply_save_with(
            &cfg,
            &serde_json::json!({
                "language": "hi",
                "model": "qwen3-asr-0.6b"
            }),
            |_, _| Ok(()),
            |_| Ok(()),
            || false,
        )
        .unwrap();

        assert_eq!(saved.language, "hi");
        assert_eq!(saved.model, "qwen3-asr-0.6b");
        assert_eq!(
            crate::models::route_for(&saved.model, &saved.language),
            Some(crate::models::Route::Single("qwen3-asr-0.6b"))
        );

        let rejected = apply_save_with(
            &cfg,
            &serde_json::json!({
                "language": "zh",
                "model": "parakeet-tdt-v3"
            }),
            |_, _| Ok(()),
            |_| Ok(()),
            || false,
        )
        .unwrap();
        assert_eq!(rejected.language, "zh");
        assert!(rejected.model.is_empty());
    }

    #[test]
    fn reselecting_the_visible_language_can_clear_an_explicit_model_override() {
        let mut current = configured();
        current.model = "paraformer-zh".to_string();
        let cfg = Mutex::new(current);
        let saved = apply_save_with(
            &cfg,
            &serde_json::json!({ "language": "en", "model": "" }),
            |_, _| Ok(()),
            |_| Ok(()),
            || false,
        )
        .unwrap();

        assert_eq!(saved.language, "en");
        assert!(saved.model.is_empty());
        assert_eq!(
            crate::models::route_for(&saved.model, &saved.language),
            Some(crate::models::Route::Single("parakeet-tdt-v3"))
        );
    }

    #[test]
    fn failed_autostart_rollback_is_reported_and_reconciles_authoritative_state() {
        let cfg = Mutex::new(configured());
        let calls = std::cell::RefCell::new(Vec::new());
        let error = apply_save_with(
            &cfg,
            &serde_json::json!({ "autostart": true }),
            |_, _| Err("settings disk is read-only".to_string()),
            |enabled| {
                calls.borrow_mut().push(enabled);
                if enabled {
                    Ok(())
                } else {
                    Err("registry rollback was denied".to_string())
                }
            },
            || true,
        )
        .unwrap_err();

        assert_eq!(*calls.borrow(), [true, false]);
        assert!(error.contains("settings disk is read-only"), "{error}");
        assert!(error.contains("registry rollback was denied"), "{error}");
        assert!(
            error.contains("operating-system state (enabled)"),
            "{error}"
        );
        assert!(cfg.lock().unwrap().autostart);
    }

    #[test]
    fn partially_failed_autostart_apply_also_reconciles_the_control() {
        let cfg = Mutex::new(configured());
        let error = apply_save_with(
            &cfg,
            &serde_json::json!({ "autostart": true }),
            |_, _| panic!("persistence must not run after an OS apply failure"),
            |_| Err("launchd changed state and then timed out".to_string()),
            || true,
        )
        .unwrap_err();

        assert!(error.contains("launchd changed state and then timed out"));
        assert!(cfg.lock().unwrap().autostart);
    }
}

#[cfg(test)]
mod dictionary_revision_contract_tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "vocalcode-dictionary-ui-{name}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn init_payload_binds_dictionary_rows_to_the_exact_revision() {
        let document = crate::RulesDocument {
            rules: vec![("heard".to_string(), "written".to_string())],
            revision: crate::RulesRevision::parse(&"a".repeat(64)).unwrap(),
        };
        let payload: Value = serde_json::from_str(&init_config_json(
            &Config::default(),
            &[],
            Some(&document),
            None,
        ))
        .unwrap();

        assert_eq!(payload["dict"], serde_json::json!([["heard", "written"]]));
        assert_eq!(payload["dict_revision"], document.revision.as_str());
        assert!(payload["dict_error"].is_null());
        assert_eq!(payload["limits"]["dictionary_rules"], MAX_DICTIONARY_RULES);
        assert_eq!(
            payload["limits"]["dictionary_side_utf8_bytes"],
            MAX_DICTIONARY_SIDE_UTF8_BYTES
        );
        assert_eq!(
            payload["limits"]["dictionary_document_bytes"],
            MAX_DICTIONARY_DOCUMENT_BYTES
        );
        assert_eq!(payload["limits"]["ipc_utf8_bytes"], MAX_SETTINGS_IPC_BYTES);
    }

    #[test]
    fn another_ui_stale_save_is_rejected_and_reloads_the_winner() {
        let base = scratch("two-windows");
        let path = base.join("replacements.txt");
        std::fs::write(&path, "old => value\n").unwrap();
        let initial = crate::load_rules_document(&base).unwrap();

        let winner = save_dictionary_request(
            &base,
            1,
            initial.revision.as_str(),
            &["first => winner".to_string()],
        );
        assert!(winner.ok);
        let winner_document = winner.document.as_ref().unwrap();
        assert_ne!(winner_document.revision, initial.revision);

        let stale = save_dictionary_request(
            &base,
            2,
            initial.revision.as_str(),
            &["second => stale".to_string()],
        );
        assert!(!stale.ok);
        assert!(stale.conflict);
        assert_eq!(stale.message, DICTIONARY_CONFLICT_RELOADED);
        let reloaded = stale.document.unwrap();
        assert_eq!(reloaded.revision, winner_document.revision);
        assert_eq!(reloaded.rules, winner_document.rules);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first => winner\n");

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn external_edit_is_not_overwritten_and_its_exact_snapshot_is_reloaded() {
        let base = scratch("external-edit");
        let path = base.join("replacements.txt");
        std::fs::write(&path, "old => value\n").unwrap();
        let initial = crate::load_rules_document(&base).unwrap();
        let external = b"# hand edit\r\nexternal => value\r\ninvalid evidence";
        std::fs::write(&path, external).unwrap();
        let latest = crate::load_rules_document(&base).unwrap();

        let stale = save_dictionary_request(
            &base,
            7,
            initial.revision.as_str(),
            &["page => stale".to_string()],
        );
        assert!(stale.conflict);
        let reloaded = stale.document.unwrap();
        assert_eq!(reloaded, latest);
        assert_eq!(std::fs::read(&path).unwrap(), external);

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn html_requires_revision_on_every_save_and_rotates_or_reloads_it() {
        let html = include_str!("webui.html");
        let rust = include_str!("webui.rs");
        let main = include_str!("main.rs");

        assert!(html.contains("window.vocalcodeDict(c.dict||[], c.dict_revision)"));
        assert!(html.contains(
            "send({type:\"save_dict\",request_id:id,revision:dictRevision,rules:snapshot})"
        ));
        assert!(html.contains("dictRevision=result.revision"));
        assert!(html.contains("if(result.conflict"));
        assert!(html.contains("dict=cloneDict(result.rules)"));
        assert!(rust.contains("window.vocalcodeDictionaryResult({payload})"));
        assert!(!main.contains(concat!("RULES_SESSION_", "REVISIONS")));
        assert!(!main.contains("pub fn save_rules(base"));
        assert_eq!(html.matches(DICTIONARY_CONFLICT_RELOADED).count(), 4);
    }

    #[test]
    fn html_preflights_host_limits_before_mutating_or_sending() {
        let html = include_str!("webui.html");

        assert!(html.contains("applyHostLimits(c&&c.limits)"));
        assert!(html.contains("utf8Bytes(payload)>LIMITS.ipc_utf8_bytes"));
        assert!(html.contains("if(!dictionaryWithinLimits(next,true)) return"));
        assert!(html.contains("if(!dictionaryWithinLimits(snapshot,true)) return"));
        assert!(html.contains("list.length>=LIMITS.triggers_per_action"));
        assert!(html.contains("maxlength=\"16384\""));
    }
}

#[cfg(test)]
mod webui_copy_contract_tests {
    use super::*;

    #[test]
    fn writing_page_is_wired_to_the_host_rules_and_translated() {
        let html = include_str!("webui.html");
        assert!(html.contains(r#"class="nav" data-panel="writing""#));
        assert!(html.contains(r#"class="panel" data-panel="writing""#));
        for id in [
            "wCommands",
            "wBacktrack",
            "wLists",
            "wCode",
            "wPressEnter",
            "wStyleSeg",
            "wAppAdd",
            "wChatPreset",
            "wTry",
            "wTryOut",
            "doubleTap",
            "muteDictating",
            "insights",
            "insHeat",
        ] {
            assert!(html.contains(&format!(r#"id="{id}""#)), "{id}");
        }
        // The Try-it box runs the real Rust rules, never a JavaScript copy.
        assert!(html.contains(r#"send({type:"writing_preview""#));
        assert!(html.contains("window.vocalcodeWritingPreview=function"));
        assert!(!html.contains("function applyWritingRules"));
        // Spoken phrases stay visible in compact layout.
        assert!(html.contains(".row .k small.w-say{display:block}"));
        for (english, chinese) in [
            ("Writing", "写作"),
            ("Scratch that", "撤回上一句"),
            ("Double-tap to lock", "双击锁定免提"),
            ("Mute other audio while dictating", "听写时静音其他声音"),
            ("Words per minute", "每分钟字数"),
        ] {
            assert!(
                html.contains(&format!(r#""{english}":"{chinese}""#)),
                "{english}"
            );
        }
    }

    #[test]
    fn smart_meeting_reminder_only_opens_review_and_never_starts_capture() {
        let html = include_str!("webui.html");
        let start = html
            .find(r#"document.getElementById("meetingReminderOpen").onclick=function()"#)
            .expect("the meeting reminder review action must exist");
        let end = html[start..]
            .find(r#"document.getElementById("meetingReminderLater").onclick"#)
            .map(|offset| start + offset)
            .expect("the reminder review action must be bounded by the next action");
        let review_action = &html[start..end];

        assert!(review_action.contains(r#"showPanel("meetings",true)"#));
        assert!(review_action.contains(r#"getElementById("meetingStart").focus()"#));
        assert!(!review_action.contains("meeting_start"));
        assert!(!review_action.contains("send("));
    }

    #[test]
    fn meeting_start_controls_use_bounded_wrapping_layout() {
        let html = include_str!("webui.html");

        assert!(html.contains("class=\"meeting-start-main\""));
        assert!(html.contains("class=\"meeting-start-controls\""));
        assert!(html.contains(".meeting-start{display:flex;flex-direction:column"));
        assert!(html.contains(".meeting-start-controls{display:flex"));
        assert!(html.contains("flex-wrap:wrap"));
        assert!(html.contains(".meeting-retention>span{flex:1 1 330px;min-width:0}"));
        assert!(html.contains(".meeting-workspace{grid-template-columns:165px minmax(0,1fr)}"));
        assert!(html.contains(".body{grid-template-columns:144px minmax(0,1fr)}"));
        assert!(html.contains("Nothing is uploaded. Record only with everyone’s permission."));
        assert!(!html.contains("Audio is chunked for crash recovery while recording."));
        assert!(html
            .contains(r#"getElementById("meetingStartBox").style.display=active?"none":"flex""#));
        assert!(!html
            .contains(r#"getElementById("meetingStartBox").style.display=active?"none":"grid""#));
        assert!(html.contains("class=\"meeting-keep-short\">Keep audio</span>"));
        assert!(html.contains(".meeting-retention{align-items:center;flex-wrap:nowrap}"));
        assert!(html.contains(".usage,.footline{display:none}"));
        assert_eq!(html.matches("class=\"nav-label\"").count(), 8);
        assert!(html.contains(
            ".nav-label{min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}"
        ));
        assert!(!html.contains(".meeting-tools{flex-wrap:nowrap}"));
        assert!(html.contains(".meeting-tools{flex-wrap:wrap}"));
        assert!(html.contains("grid-template-rows:130px minmax(260px,1fr)"));
        assert!(html.contains(".meeting-export{width:64px;min-width:64px}"));
        assert!(html.contains(".meeting-bookmark{width:32px;min-width:32px"));
        assert!(html.contains(".meeting-delete{width:43px;min-width:43px"));
    }

    #[test]
    fn narrow_meeting_detail_groups_exports_into_one_bounded_control() {
        let html = include_str!("webui.html");

        assert!(html.contains("meeting-btn meeting-export"));
        assert!(html.contains("exportPicker.onchange=function()"));
        assert!(html.contains(r#"send({type:"meeting_export""#));
        assert!(html.contains("meeting-bookmark\"));"));
        assert!(html.contains("danger meeting-delete\"));"));
        assert!(!html.contains(
            r#"tools.appendChild(meetingButton(pair[0],function(){send({type:"meeting_export""#
        ));
    }

    #[test]
    fn windows_titlebar_keeps_standard_controls_on_the_right_without_overflow() {
        let html = include_str!("webui.html");

        assert!(html.contains("html.win .hleft{grid-column:3;grid-row:1"));
        assert!(html.contains("class=\"wctl\" id=\"zoom\""));
        assert!(html.contains(r#"getElementById("zoom").onclick=()=>send({type:"zoom"})"#));
        assert!(html.contains("header{grid-template-columns:96px minmax(0,1fr) 96px"));
        assert!(html.contains(".hcentre .lang{max-width:none"));
        assert!(html.contains(
            "html.win header{height:65px;min-height:65px;flex:0 0 65px;padding-top:21px}"
        ));
    }

    // The config reaches the page only if the page asks for it, and it can only
    // ask once its entry points exist. Pushing on the first event-loop
    // iteration instead lost the call silently — the webview had not parsed the
    // document — and left every config-driven control rendering page defaults.
    #[test]
    fn the_page_announces_readiness_after_defining_its_entry_point() {
        let page = include_str!("webui.html");
        let announce = page
            .find(r#"send({type:"ready"})"#)
            .expect("the page must announce readiness to the host");
        let define = page
            .find("window.vocalcodeInit = function")
            .expect("the page must define vocalcodeInit");
        assert!(
            announce > define,
            "readiness is announced before vocalcodeInit exists, so the host's reply is discarded"
        );

        // The host must answer that announcement, and must not go back to
        // pushing the config at a moment the page cannot yet receive it.
        // Both needles are assembled from fragments so that this test's own
        // source, which `include_str!` also reads, cannot satisfy them.
        let host = include_str!("webui.rs");
        let handler = concat!(r#"Some("ready")"#, " => {");
        assert!(
            host.contains(handler),
            "the host must handle the page's readiness message"
        );
        let push = concat!("window.vocalcodeInit(", "{init_json})");
        assert_eq!(
            host.matches(push).count(),
            1,
            "the config must be delivered from exactly one place: the readiness reply"
        );
    }

    #[test]
    fn privileged_webview_only_accepts_its_local_document() {
        let settings_document_url = settings_document_data_url();
        assert!(local_webview_navigation(
            "about:blank",
            &settings_document_url
        ));
        assert!(local_webview_navigation(
            &settings_document_url,
            &settings_document_url
        ));
        let mut altered_document = settings_document_url.clone();
        altered_document.push('A');
        assert!(!local_webview_navigation(
            &altered_document,
            &settings_document_url
        ));
        for url in [
            "https://example.com/",
            "data:text/html,<script>window.ipc.postMessage('{}')</script>",
            "file:///tmp/untrusted.html",
            "javascript:window.ipc.postMessage('{}')",
        ] {
            assert!(
                !local_webview_navigation(url, &settings_document_url),
                "{url}"
            );
        }
        let html = include_str!("webui.html");
        assert!(html.contains("Content-Security-Policy"));
        assert!(html.contains("default-src 'none'"));
        assert!(html.contains("connect-src 'none'"));
        assert!(!html.contains("日本語 · 한국어 · ไทย"));
        assert!(!html.contains("each with its own model"));
        assert!(html.contains("id=\"frLanguageGrid\""));
        assert!(html.contains("id=\"languageGrid\""));
        assert!(html.contains("renderLanguageButtons(\"frLanguageGrid\",true)"));
        assert!(html.contains("renderLanguageButtons(\"languageGrid\",false)"));
        assert!(!html.contains("id=\"frMore\""));
        assert!(!html.contains("id=\"langMore\""));
    }

    #[test]
    fn accepted_recovery_copy_does_not_claim_background_delivery_completed() {
        const ACCEPTED: &str = "If a purchase exists, recovery instructions will arrive by email.";
        let rust = include_str!("webui.rs");
        let production_rust = rust.split("#[cfg(test)]").next().unwrap();
        let html = include_str!("webui.html");

        assert!(production_rust.contains(ACCEPTED));
        assert_eq!(html.matches(&format!("\"{ACCEPTED}\":")).count(), 4);
        for completed_claim in [
            "the key has been sent to that email",
            "the email has been sent",
            "recovery is complete",
        ] {
            assert!(
                !production_rust.contains(completed_claim),
                "{completed_claim}"
            );
            assert!(!html.contains(completed_claim), "{completed_claim}");
        }
    }

    #[test]
    fn reduced_motion_disables_every_infinite_indicator_and_progress_transition() {
        let html = include_str!("webui.html");
        assert!(html.contains(
            ".wave i,.tx::after,.sspin,.keycap.capturing,.binds.capturing{animation:none}"
        ));
        assert!(html.contains(".dlbar{transition:none}"));
    }

    #[test]
    fn settings_panels_scroll_when_needed_and_teach_uses_the_compact_capsule() {
        let html = include_str!("webui.html");
        let rust = include_str!("webui.rs");

        assert!(html.contains(".content{padding:18px 22px;overflow:auto;min-height:0"));
        assert!(html.contains("class=\"settings-tabs\""));
        assert_eq!(html.matches("data-settings-pane=").count(), 3);
        assert!(html.contains("pane.dataset.settingsPane===wanted"));
        assert!(html.contains("class=\"home-focus\" id=\"homeQuickStart\""));
        assert!(html.contains("class=\"ui-button act\" data-go=\"triggers\">Shortcuts"));
        assert!(html.contains("class=\"home-summary\" id=\"stats\""));
        assert!(!html.contains("class=\"home-flow\""));
        assert!(!html.contains("id=\"stSessions\""));
        assert!(!html.contains("id=\"stSaved\""));
        assert!(html.contains("else if(saved >= 1)"));
        assert!(html.contains("of typing at 200 characters per minute"));
        assert_eq!(html.matches("class=\"navsplit\"").count(), 1);
        assert_eq!(html.matches("\"Your talk shortcut\":").count(), 4);
        assert_eq!(html.matches("\"Your talk shortcuts\":").count(), 4);
        assert!(!html.contains(".panel[data-panel=\"home\"] .hero{display:none}"));
        assert!(html.contains("class=\"card shortcut-primary\""));
        assert!(html.contains("class=\"card shortcut-optional\""));
        assert!(html.contains(
            ".shortcut-mode{display:grid;grid-template-columns:minmax(0,1fr) minmax(280px,330px)"
        ));
        assert!(html.contains(".shortcut-optional .row{display:grid;grid-template-columns:minmax(0,1fr) minmax(300px,340px)"));
        assert!(html.contains("flex-wrap:nowrap"));
        assert!(html.contains("id=\"teachCapsule\""));
        assert!(html.contains("send({type:\"teach_popup_open\"})"));
        assert!(html.contains("send({type:\"teach_popup_close\"})"));
        assert!(html.contains("if(e.key!==\"Escape\" || pageCaptureWhich) return"));

        let teach = html
            .split("function teachWord(heard, exact)")
            .nth(1)
            .and_then(|tail| tail.split("window.vocalcodeTeach=teachWord").next())
            .expect("Teach capsule JavaScript");
        assert!(teach.contains("teach-popup-open"));
        assert!(!teach.contains("showPanel(\"dictionary\")"));
        assert!(rust.contains("const TEACH_CAPSULE_HEIGHT: f64 = 90.0"));
        #[cfg(target_os = "windows")]
        assert!(rust.contains("window.set_undecorated_shadow(false)"));
        assert!(rust.contains("UserEvent::TeachPopupClose"));
        assert!(rust.contains("window.vocalcodeTeachClosed()"));
    }

    #[test]
    fn language_onboarding_paste_and_updates_follow_one_clear_path() {
        let html = include_str!("webui.html");

        // Every supported language is a first-layer radio button in Settings
        // and first run. Picking one determines the model; there is no second
        // selector whose meaning can disagree with the model route.
        assert!(html.contains("function renderLanguageButtons(id, onboarding)"));
        assert!(html.contains("updateModelRoute()"));
        assert!(!html.contains("id=\"langMoreRow\""));
        assert!(!html.contains("id=\"frMoreRow\""));

        // Choosing the required model moves into a real guided tour over the
        // actual UI instead of duplicating Home inside a second full dialog.
        assert!(html.contains("id=\"frLanguageStep\""));
        assert!(!html.contains("id=\"frGuideStep\""));
        assert!(html.contains("id=\"tourLayer\""));
        assert!(html.contains("var TOUR_STEPS=["));
        assert!(html.contains("save(true); renderLangs(); showOnboardingStep(0); startTour()"));
        assert!(html.contains("tourReplay\").onclick=startTour"));

        // Clipboard insertion is a labelled compatibility escape hatch, not a
        // peer default, and updates surface beside the persistent version.
        assert!(
            html.contains("Compatibility paste <span class=\"state-badge recommended\">Keep off")
        );
        assert!(html.contains("Keep it off unless a specific app rejects direct insertion."));
        let footer = html.find("class=\"footline\"").unwrap();
        let version = html[footer..].find("id=\"foot\"").unwrap();
        let update = html[footer..].find("id=\"updbar\"").unwrap();
        assert!(
            version < update,
            "Update must sit beside the footer version"
        );
        assert!(html.contains("Checked at startup and every 6 hours"));
    }

    #[test]
    fn correction_learning_is_configurable_and_has_a_separate_dictionary_result_path() {
        let html = include_str!("webui.html");
        let review = include_str!("correction_review.html");
        let rust = include_str!("webui.rs");

        assert!(html.contains("id=\"correctionWindow\""));
        for milliseconds in ["0", "5000", "8000", "10000", "15000"] {
            assert!(html.contains(&format!("<option value=\"{milliseconds}\"")));
        }
        assert!(html.contains("cfg.correction_window_ms=parseInt"));
        assert!(html.contains("window.vocalcodeCorrectionResult=function(result)"));
        assert!(rust.contains("window.vocalcodeCorrectionResult({payload})"));
        assert!(review.contains("window.vocalcodeCorrectionReview=function(result)"));
        assert!(review.contains("timer=setTimeout(close,4000)"));
        assert!(review.contains("id=\"keep\""));
        assert!(review.contains("id=\"undo\""));
        assert!(review.contains("type:\"correction_popup_close\""));
        assert!(review.contains("REVIEW_REQUEST_BASE=8000000000000000"));
        assert!(review.contains("background:linear-gradient(180deg,#111214,#090a0c)"));
        assert!(!review.contains("rgba(238,138,62"));
        assert!(rust.contains("result.ok && !result.changes.is_empty()"));
        assert!(rust.contains("IpcSurface::CorrectionReview"));
        assert!(rust.contains("with_skip_taskbar(true)"));
        assert!(rust.contains("round_correction_review_window(&correction_window)"));
        assert_eq!(
            html.matches("\"Correction added to Dictionary.\":").count(),
            4
        );
    }

    #[test]
    fn learned_correction_uses_an_independent_window_and_scoped_requests() {
        let rust = include_str!("webui.rs");
        let implementation = rust.split("#[cfg(test)]").next().unwrap();
        assert!(implementation.contains("with_title(\"VocalCode Correction\")"));
        assert!(implementation
            .contains("let correction_window = correction_builder.build(&event_loop)"));
        assert!(implementation
            .contains("present_learned_correction(&correction_window, &correction_webview"));
        assert!(!implementation.contains("learned_correction_host("));
        assert!(is_correction_review_request_id(
            CORRECTION_REVIEW_REQUEST_ID_BASE
        ));
        assert!(is_correction_review_request_id(u64::MAX));
        assert!(!is_correction_review_request_id(
            CORRECTION_REVIEW_REQUEST_ID_BASE - 1
        ));
    }

    #[test]
    fn duplicate_trigger_capture_explains_that_the_binding_is_already_active() {
        let html = include_str!("webui.html");
        const COPY: &str = "Already bound here; no need to add it again.";

        // The host reports an existing binding without ending capture, while
        // the page also checks its current list before saving. Both paths must
        // describe a working, already-active binding rather than sounding like
        // the mouse button or key is unsupported.
        assert_eq!(html.matches(&format!("\"{COPY}\":")).count(), 4);
        assert!(html.contains(
            "labelFor(bcode)+\" — \"+t(\"Already bound here; no need to add it again.\")"
        ));
        assert!(html.contains(
            "labelFor(code) + \" — \" + t(\"Already bound here; no need to add it again.\")"
        ));
        assert!(!html.contains("t(\"Already bound here\")"));
    }

    #[test]
    fn unavailable_pro_trial_never_blocks_basic_setup_or_ready_state() {
        let html = include_str!("webui.html");
        let rust = include_str!("webui.rs");
        assert!(rust.contains("\"trial_setup_error\":"));
        assert!(!html.contains("s.trial_setup_error===true"));
        assert!(html.contains("setup.classList.remove(\"error\")"));
        assert!(html.contains("if(s.ready!==false) announceSetup(\"\")"));
        assert!(!html.contains("setup.classList.add(\"on\")"));
        assert!(!html.contains("pill.classList.toggle(\"error\", trialSetupFailed)"));
    }

    #[test]
    fn progressive_typing_is_visible_configurable_and_describes_append_only_safety() {
        let html = include_str!("webui.html");
        assert!(html.contains("id=\"liveCaption\""));
        assert!(html.contains("bindToggle(\"liveCaption\",\"live_caption\""));
        assert!(html.contains("[\"liveCaption\",\"live_caption\"]"));
        assert!(html.contains("Already-inserted text is never rewritten"));
        assert!(html.contains("keep this off in terminals"));
    }

    #[test]
    fn basic_and_pro_controls_are_explicit_and_native_meeting_gates_remain() {
        let html = include_str!("webui.html");
        let rust = include_str!("webui.rs");
        assert!(html.contains("Basic dictation is free forever."));
        assert!(html.contains("Upgrade to Pro — $4.99 once"));
        assert!(html.contains("correction.disabled=!hasPro"));
        assert!(html.contains("if(!hasPro){showPanel(\"license\",true)"));
        assert!(rust.contains("if !status.pro_gate.load(Ordering::Acquire)"));
        assert!(rust.contains("Meetings are included in Pro."));
    }

    #[test]
    fn checkout_reference_is_cryptographically_random_and_fails_closed() {
        let html = include_str!("webui.html");
        let buy = html
            .split("function securePurchaseReference()")
            .nth(1)
            .and_then(|tail| tail.split("function flashNotes").next())
            .expect("checkout JavaScript");
        assert!(buy.contains("source.randomUUID()"));
        assert!(buy.contains("source.getRandomValues(bytes)"));
        assert!(buy.contains("if(!ref){"));
        assert!(!buy.contains("Math.random"));
        assert!(!buy.contains("Date.now"));
    }

    #[test]
    fn checkout_requires_a_success_status_and_explicit_true_acknowledgement() {
        assert!(checkout_intent_reply_accepted(true, br#"{"ok":true}"#));
        assert!(!checkout_intent_reply_accepted(false, br#"{"ok":true}"#));
        assert!(!checkout_intent_reply_accepted(true, br#"{"ok":false}"#));
        assert!(!checkout_intent_reply_accepted(true, br#"{"ok":"true"}"#));
        assert!(!checkout_intent_reply_accepted(true, br#"{"status":"ok"}"#));
        assert!(!checkout_intent_reply_accepted(true, b""));
        assert!(!checkout_intent_reply_accepted(true, b"not json"));
    }

    #[test]
    fn checkout_poll_never_reflects_remote_rejection_text() {
        let source = include_str!("webui.rs");
        let start = source.find("fn send_purchase_poll_request(").unwrap();
        let end = source[start..].find("/// Live runtime state").unwrap() + start;
        let request = &source[start..end];

        assert!(!request.contains("get(\"error\")"));
        assert!(!request.contains("read_to_string"));
        assert!(request.contains("PurchasePollReply::Rejected"));
        assert!(source.contains("purchase activation was rejected"));
    }

    #[test]
    fn checkout_intent_parser_accepts_the_exact_limit_and_rejects_one_byte_more() {
        let mut boundary = br#"{"ok":true}"#.to_vec();
        boundary.resize(CHECKOUT_INTENT_RESPONSE_MAX_BYTES, b' ');
        assert!(checkout_intent_reply_accepted(true, &boundary));

        boundary.push(b' ');
        assert!(!checkout_intent_reply_accepted(true, &boundary));
    }

    #[test]
    fn revealed_license_key_is_remasked_on_context_changes() {
        let html = include_str!("webui.html");
        assert!(html.contains("if(p!==\"license\") maskLicenseKey()"));
        assert!(html.contains(
            "document.addEventListener(\"visibilitychange\",function(){ if(document.hidden) maskLicenseKey(); })"
        ));
        assert!(html.contains("window.addEventListener(\"blur\",maskLicenseKey)"));
        assert!(html.contains("window.addEventListener(\"pagehide\",maskLicenseKey)"));
        assert!(html.contains("field.type=\"password\""));
        assert!(html.contains("button.setAttribute(\"aria-pressed\",\"false\")"));
    }

    #[test]
    fn share_link_uses_the_native_clipboard_bridge() {
        let html = include_str!("webui.html");
        let rust = include_str!("webui.rs");
        assert!(html.contains("send({type:\"copy\", id:id, text:\"https://vocalcode.app\"})"));
        assert!(!html.contains("navigator.clipboard.writeText"));
        let enabled_webview_clipboard = [".with_clipboard(", "true)"].concat();
        assert!(!rust.contains(&enabled_webview_clipboard));
    }

    #[test]
    fn windows_autostart_does_not_search_path_for_registry_tools() {
        let rust = include_str!("webui.rs");
        let searched_reg = ["Command::new(\"", "reg", "\")"].concat();
        let appdata_lookup = ["var_os(\"", "APPDATA", "\")"].concat();
        let home_lookup = ["var_os(\"", "HOME", "\")"].concat();
        assert!(rust.contains("RegSetValueExW"));
        assert!(rust.contains("RegGetValueW"));
        assert!(rust.contains("SHGetKnownFolderPath"));
        assert!(rust.contains("NSHomeDirectory"));
        assert!(rust.contains("startup.join(\"VocalCode.lnk\").is_file()"));
        assert!(rust.contains("could not verify removal of the legacy launch-at-login shortcut"));
        assert!(!rust.contains(&searched_reg));
        assert!(!rust.contains(&appdata_lookup));
        assert!(!rust.contains(&home_lookup));
    }

    #[test]
    fn privileged_webviews_use_distinct_trusted_app_data_profiles() {
        // Build the forbidden spelling at runtime. Keeping it as a source
        // literal made this include_str! check match its own assertion even
        // when every real WebView used an explicit trusted profile.
        let settings = include_str!("webui.rs");
        let overlay = include_str!("overlay.rs");
        let implicit_profile = ["WebViewBuilder::", "new()"].concat();
        assert!(settings.contains("Path::new(\"webview2/settings\")"));
        assert!(settings.contains("Path::new(\"webview2/overlay\")"));
        assert!(settings.contains("WebViewBuilder::new_with_web_context"));
        assert!(overlay.contains("WebViewBuilder::new_with_web_context"));
        assert!(overlay.contains("_web_context: wry::WebContext"));
        assert!(!settings.contains(&implicit_profile));
        assert!(!overlay.contains(&implicit_profile));
    }
}

#[cfg(all(test, any(windows, target_os = "macos")))]
mod update_workspace_tests {
    use super::UpdateWorkDir;

    #[test]
    fn update_workspaces_are_unique_and_removed_on_drop() {
        let first = UpdateWorkDir::create().unwrap();
        let first_path = first.path().to_path_buf();
        let second = UpdateWorkDir::create().unwrap();
        let second_path = second.path().to_path_buf();
        assert_ne!(first_path, second_path);
        assert!(first_path.is_dir());
        assert!(second_path.is_dir());
        drop(first);
        drop(second);
        assert!(!first_path.exists());
        assert!(!second_path.exists());
    }
}

#[cfg(test)]
mod updater_contract_tests {
    use super::*;

    #[cfg(windows)]
    fn lock_windows_powershell_test() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    #[test]
    fn mac_bundle_requests_only_permissions_used_by_native_code() {
        let info = include_str!("../../packaging/macos/Info.plist");
        let entitlements = include_str!("../../packaging/macos/VocalCode.entitlements");
        let mac = include_str!("macos.rs");
        let html = include_str!("webui.html");
        let manifest = include_str!("../Cargo.toml");
        assert!(info.contains("NSMicrophoneUsageDescription"));
        assert!(info.contains("NSAudioCaptureUsageDescription"));
        assert!(
            include_str!("../../vocalcode-platform/src/meeting_audio.rs")
                .contains("start_meeting_audio")
        );
        assert!(entitlements.contains("com.apple.security.device.audio-input"));
        assert!(mac.contains("pub microphone: bool"));
        assert!(mac.contains("authorizationStatusForMediaType"));
        assert!(mac.contains("requestAccessForMediaType_completionHandler"));
        assert!(mac.contains("&& self.microphone"));
        assert!(html.contains("id=\"permMic\""));
        assert!(html.contains("pane:\"microphone\""));
        assert!(manifest.contains("objc2-av-foundation"));
        assert!(!info.contains("NSAppleEventsUsageDescription"));
        assert!(!entitlements.contains("com.apple.security.automation.apple-events"));
        for obsolete in [
            "AEDeterminePermissionToAutomateTarget",
            "warm_automation_permission",
            "Privacy_Automation",
            "com.apple.systemevents",
        ] {
            assert!(
                !mac.contains(obsolete),
                "obsolete mac permission path: {obsolete}"
            );
        }
    }

    #[test]
    fn shutdown_actions_cannot_be_weakened_by_a_racing_request() {
        assert_eq!(
            merge_shutdown_action(ShutdownAction::None, ShutdownAction::Quit),
            ShutdownAction::Quit
        );
        assert_eq!(
            merge_shutdown_action(ShutdownAction::RestartForUpdate, ShutdownAction::Quit),
            ShutdownAction::RestartForUpdate
        );
        assert_eq!(
            merge_shutdown_action(ShutdownAction::PurgeData, ShutdownAction::RestartForUpdate),
            ShutdownAction::PurgeData
        );
        assert_eq!(
            merge_shutdown_action(ShutdownAction::Quit, ShutdownAction::PurgeData),
            ShutdownAction::PurgeData
        );
    }

    #[test]
    fn privileged_runtime_never_bypasses_shutdown_with_process_exit() {
        let forbidden = ["std::process", "::exit("].concat();
        assert!(!include_str!("webui.rs").contains(&forbidden));
    }

    #[test]
    fn info_ipc_accepts_only_the_two_constant_targets() {
        assert_eq!(InfoTarget::parse("privacy"), Some(InfoTarget::Privacy));
        assert_eq!(InfoTarget::parse("support"), Some(InfoTarget::Support));
        for rejected in [
            "",
            "Privacy",
            "terms",
            "https://evil.example/",
            "mailto:attacker@example.com",
            "file:///etc/passwd",
        ] {
            assert_eq!(InfoTarget::parse(rejected), None, "{rejected}");
        }
    }

    #[test]
    fn mac_self_update_requires_the_canonical_bundle_name() {
        assert!(is_canonical_bundle_path(
            &std::path::Path::new("/Applications").join(crate::community::BUNDLE_NAME)
        ));
        if crate::community::ENABLED {
            assert!(!is_canonical_bundle_path(std::path::Path::new(
                "/Applications/VocalCode.app"
            )));
        }
        for path in [
            "/Applications/Foo.app",
            "/Applications/vocalcode.app",
            "/Applications/VocalCode.app.backup",
        ] {
            assert!(
                !is_canonical_bundle_path(std::path::Path::new(path)),
                "{path}"
            );
        }
    }

    /// Two failures inside one UI tick used to share one slot, and the first —
    /// usually the cause — was overwritten before the page ever saw it.
    #[test]
    fn runtime_errors_queue_in_order_instead_of_overwriting() {
        let errors = RuntimeErrors::default();
        errors.push("Microphone stopped responding: device removed".into());
        errors.push("Could not finish the recording".into());
        assert_eq!(
            errors.drain(),
            [
                "Microphone stopped responding: device removed",
                "Could not finish the recording"
            ]
        );
        assert!(errors.drain().is_empty(), "delivery is once");
    }

    #[test]
    fn runtime_errors_are_bounded_newest_kept_and_never_duplicated() {
        let errors = RuntimeErrors::default();
        for _ in 0..50 {
            errors.push("Diagnostic event queue full".into());
        }
        assert_eq!(errors.drain(), ["Diagnostic event queue full"]);
        for i in 0..20 {
            errors.push(format!("error {i}"));
        }
        let pending = errors.drain();
        assert_eq!(pending.len(), RuntimeErrors::CAPACITY);
        assert_eq!(pending.first().map(String::as_str), Some("error 12"));
        assert_eq!(pending.last().map(String::as_str), Some("error 19"));
    }

    #[test]
    fn notice_readiness_reads_the_model_error_label_and_download() {
        use crate::overlay::Phase;
        let status = RuntimeStatus::default();
        status.onboarded.store(true, Ordering::Release);
        status.permissions_ok.store(true, Ordering::Release);
        *status.model_label.lock().unwrap() = format!("{}offline", crate::MODEL_ERROR_LABEL);
        let readiness = notice_readiness(&status, Phase::Idle);
        assert!(readiness.model_failed && !readiness.model_available);
        assert_eq!(
            crate::notice::tray_state(&readiness),
            crate::notice::TrayState::Error(crate::notice::TrayError::Model)
        );
        *status.model_label.lock().unwrap() = "Preparing…".into();
        *status.model_download.lock().unwrap() = Some(("Parakeet".into(), 37.9, 190.0, 500.0));
        let readiness = notice_readiness(&status, Phase::Idle);
        assert!(!readiness.model_failed);
        assert_eq!(
            crate::notice::not_ready_reason(&readiness),
            Some(crate::notice::NotReady::Downloading(37))
        );
        status.microphone_failed.store(true, Ordering::Release);
        *status.model_download.lock().unwrap() = None;
        status.model_available.store(true, Ordering::Release);
        assert_eq!(
            crate::notice::not_ready_reason(&notice_readiness(&status, Phase::Idle)),
            Some(crate::notice::NotReady::Microphone)
        );
    }

    #[test]
    fn transient_update_label_restores_without_clobbering_engine_updates() {
        let label = Mutex::new("Ready · original".to_string());
        {
            let mut temporary = TransientModelLabel::new(&label);
            temporary.set("Downloading update…");
            assert_eq!(*label.lock().unwrap(), "Downloading update…");
        }
        assert_eq!(*label.lock().unwrap(), "Ready · original");

        {
            let mut temporary = TransientModelLabel::new(&label);
            temporary.set("Downloading update…");
            *label.lock().unwrap() = "Ready · engine changed".to_string();
            temporary.set("Installing update — VocalCode will restart…");
        }
        assert_eq!(
            *label.lock().unwrap(),
            "Ready · engine changed",
            "the phase transition must remember a newer engine label"
        );

        {
            let mut temporary = TransientModelLabel::new(&label);
            temporary.set("Installing update — VocalCode will restart…");
            *label.lock().unwrap() = "Microphone error: unplugged".to_string();
        }
        assert_eq!(
            *label.lock().unwrap(),
            "Microphone error: unplugged",
            "Drop must not overwrite a label published after the updater"
        );
    }

    #[test]
    fn purchase_polling_uses_one_monotonic_six_minute_deadline() {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(6 * 60);
        assert_eq!(
            purchase_poll_sleep(start, deadline),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            purchase_poll_request_timeout(start, deadline),
            Some(Duration::from_secs(10))
        );
        let near = deadline - Duration::from_millis(250);
        assert_eq!(
            purchase_poll_sleep(near, deadline),
            Some(Duration::from_millis(250))
        );
        assert_eq!(purchase_poll_request_timeout(deadline, deadline), None);
        assert_eq!(
            purchase_poll_request_timeout(deadline + Duration::from_secs(1), deadline),
            None
        );
    }

    #[test]
    fn mac_relaunch_script_waits_checks_open_and_has_a_direct_fallback() {
        let bundle = std::path::Path::new("/Users/A B/It's Here/VocalCode.app");
        let executable =
            std::path::Path::new("/Users/A B/It's Here/VocalCode.app/Contents/MacOS/VocalCode");
        let workspace = std::path::Path::new("/private/tmp/VocalCode update");
        let script = mac_relaunch_script(bundle, executable, workspace, 4242);
        assert!(script.contains("parent_pid=4242"));
        assert!(script.contains("parent_birth=$(/bin/ps"));
        assert!(script.contains("current_birth=$(/bin/ps"));
        assert!(script.contains("while /bin/kill -0 \"$parent_pid\""));
        assert!(script.contains("parent_waits=$((parent_waits + 1))"));
        assert!(script.contains("[ \"$parent_waits\" -ge 1500 ]"));
        assert!(script.contains("exit 75"));
        assert!(script.contains("relaunch-failure.txt' 2>/dev/null"));
        assert!(script.contains("/bin/rm -rf -- '/private/tmp/VocalCode update'"));
        assert!(script.contains("if /usr/bin/open"));
        assert!(script.contains("then exit 0"));
        assert!(script.contains("for delay in 0 1 2"));
        assert!(script
            .contains("exec '/Users/A B/It'\\''s Here/VocalCode.app/Contents/MacOS/VocalCode'"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_relaunch_helper_is_valid_posix_shell_syntax() {
        let script = mac_relaunch_script(
            std::path::Path::new("/Applications/VocalCode.app"),
            std::path::Path::new("/Applications/VocalCode.app/Contents/MacOS/VocalCode"),
            std::path::Path::new("/private/tmp/VocalCode-update-test"),
            4242,
        );
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-n", "-c", &script]);
        let output = bounded_command_output(&mut command, Duration::from_secs(10), None).unwrap();
        assert!(
            output.status.success(),
            "POSIX shell parser rejected relaunch helper: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn mac_update_transaction_is_strict_and_versioned() {
        assert_eq!(
            MAC_UPDATE_TRANSACTION,
            format!("{}-transaction.json", crate::community::UPDATE_STEM)
        );
        let parsed = parse_mac_update_transaction(
            br#"{"version":1,"expected_version":"0.5.2","phase":"preparing"}"#,
        )
        .expect("a current transaction should parse");
        assert_eq!(parsed.expected_version, "0.5.2");
        assert!(parse_mac_update_transaction(
            br#"{"version":1,"expected_version":"0.5.2","phase":"ready","surprise":true}"#
        )
        .is_err());
        assert!(parse_mac_update_transaction(
            br#"{"version":2,"expected_version":"0.5.2","phase":"ready"}"#
        )
        .is_err());
        assert!(parse_mac_update_transaction(
            br#"{"version":1,"expected_version":"0.5.2","phase":"unknown"}"#
        )
        .is_err());
    }

    #[test]
    fn mac_staging_marker_precedes_copy_and_cleanup_removes_payload_first() {
        let source = include_str!("webui.rs");
        let marker = source
            .find("MacUpdatePhase::Preparing)?;")
            .expect("a durable preparing phase is required");
        let ditto = source
            .find("Command::new(\"/usr/bin/ditto\")")
            .expect("the updater must stage with ditto");
        assert!(marker < ditto, "the marker must own the name before ditto");

        let helper = source
            .find("fn discard_mac_staging_transaction")
            .expect("staging cleanup must be transactional");
        let tail = &source[helper..];
        let remove_stage = tail.find("remove_dir_all(staged)").unwrap();
        let remove_marker = tail.find("remove_mac_update_transaction(parent)").unwrap();
        assert!(
            remove_stage < remove_marker,
            "stage deletion must finish before marker deletion"
        );
    }

    #[test]
    fn mac_dmg_gatekeeper_check_precedes_every_mount() {
        let source = include_str!("webui.rs");
        let install = source
            .find("verify_dmg_before_mount(&tmp, Some(&status.shutdown))?;")
            .expect("the install path must verify the DMG");
        let mount = source[install..]
            .find("Command::new(\"/usr/bin/hdiutil\")")
            .map(|offset| install + offset)
            .expect("the install path must mount the DMG");
        assert!(install < mount, "Gatekeeper must run before hdiutil");
        assert!(source.contains("Command::new(\"/usr/sbin/spctl\")"));
        assert!(source.contains("\"context:primary-signature\""));
    }

    #[test]
    fn only_the_canonical_bundle_may_clear_recovery_copies() {
        assert!(is_canonical_install_bundle(
            &std::path::Path::new("/Applications").join(crate::community::BUNDLE_NAME)
        ));
        assert!(!is_canonical_install_bundle(std::path::Path::new(
            "/Applications/.VocalCode-update-old.app"
        )));
        assert!(!is_canonical_install_bundle(std::path::Path::new(
            "/Applications/VocalCode Beta.app"
        )));
    }

    #[test]
    fn a_retry_never_deletes_preserved_mac_recovery_copies() {
        let root = std::env::temp_dir().join(format!(
            "vocalcode-mac-recovery-contract-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let staged = root.join(".VocalCode-update-staged.app");
        let old = root.join(".VocalCode-update-old.app");
        assert!(ensure_recovery_paths_absent(&staged, &old).is_ok());
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("only-recovery-copy"), b"keep").unwrap();
        let error = ensure_recovery_paths_absent(&staged, &old).unwrap_err();
        assert!(error.contains("refusing to delete or overwrite"));
        assert!(old.join("only-recovery-copy").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn windows_helper_bounds_parent_and_installer_waits_and_preserves_live_work() {
        assert!(WINDOWS_UPDATE_HELPER.contains("} catch {"));
        assert!(WINDOWS_UPDATE_HELPER.contains("Restart-Failed 'helper-launch-error'"));
        assert!(WINDOWS_UPDATE_HELPER.contains("Preserve-Diagnostic 'parent-exit-timeout'"));
        assert!(WINDOWS_UPDATE_HELPER.contains("Restart-Failed 'installer-timeout'"));
        assert!(WINDOWS_UPDATE_HELPER.contains("--update-failed"));
        assert!(WINDOWS_UPDATE_HELPER.contains("$env:VC_UPDATE_INSTALLER"));
        assert!(WINDOWS_UPDATE_HELPER.contains("$env:VC_UPDATE_EXE"));
        assert!(WINDOWS_UPDATE_HELPER.contains("$env:VC_UPDATE_PID"));
        assert!(WINDOWS_UPDATE_HELPER.contains("$env:VC_UPDATE_READY"));
        assert!(WINDOWS_UPDATE_HELPER.contains("WriteAllText"));
        assert!(WINDOWS_UPDATE_HELPER.contains("$parent.WaitForExit(300000)"));
        assert!(WINDOWS_UPDATE_HELPER.contains("$p.WaitForExit(900000)"));
        assert!(WINDOWS_UPDATE_HELPER.contains("$p.WaitForExit(10000)"));
        assert!(WINDOWS_UPDATE_HELPER.contains("$killer.WaitForExit(10000)"));
        assert!(WINDOWS_UPDATE_HELPER.contains("'taskkill.exe'"));
        assert!(WINDOWS_UPDATE_HELPER.contains("'/T', '/F'"));
        assert!(WINDOWS_UPDATE_HELPER.contains("$script:cleanupWorkspace = $false"));
        assert!(WINDOWS_UPDATE_HELPER.contains("'failure.txt'"));
        assert!(!WINDOWS_UPDATE_HELPER.contains("Wait-Process"));
        assert!(!WINDOWS_UPDATE_HELPER.contains("-Wait -PassThru"));
        assert!(WINDOWS_UPDATE_HELPER.contains("'/NORESTART'"));
        assert!(WINDOWS_UPDATE_HELPER.contains("finally"));
        let forbidden_detached_flag = ["const DETACHED", "_PROCESS"].concat();
        assert!(!include_str!("webui.rs").contains(&forbidden_detached_flag));
    }

    #[cfg(windows)]
    #[test]
    fn hidden_windows_powershell_still_executes_its_command() {
        let _powershell_test = lock_windows_powershell_test();
        let mut command = std::process::Command::new(system_powershell().unwrap());
        command
            .args(["-NoProfile", "-NonInteractive", "-Command", "exit 37"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        hide_windows_console(&mut command);
        let status = command.status().unwrap();
        assert_eq!(status.code(), Some(37));
    }

    #[test]
    fn windows_taskbar_gets_an_explicit_big_product_icon() {
        let source = include_str!("webui.rs");
        assert!(source.contains("WindowBuilderExtWindows"));
        assert!(source.contains("with_taskbar_icon(Some(make_window_icon()))"));
    }

    #[cfg(windows)]
    #[test]
    fn updater_does_not_accept_process_creation_as_helper_readiness() {
        let work = UpdateWorkDir::create().unwrap();
        let ready = work.path().join("never-created.ready");
        let mut command =
            std::process::Command::new(windows_directory(true).unwrap().join("cmd.exe"));
        command
            .args(["/D", "/S", "/C", "exit 0"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        hide_windows_console(&mut command);
        let mut child = command.spawn().unwrap();
        let error =
            wait_for_update_helper_ready(&mut child, &ready, WINDOWS_UPDATE_HELPER_READY_TIMEOUT)
                .unwrap_err();
        assert!(error.contains("exited before initialization"));
        assert!(!ready.exists());
    }

    #[cfg(windows)]
    #[test]
    fn updater_accepts_a_live_hidden_helper_acknowledgement() {
        let _powershell_test = lock_windows_powershell_test();
        let work = UpdateWorkDir::create().unwrap();
        let ready = work.path().join("helper.ready");
        let mut command = std::process::Command::new(system_powershell().unwrap());
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[System.IO.File]::WriteAllText($env:VC_TEST_READY, 'ready'); Start-Sleep -Seconds 5",
            ])
            .env("VC_TEST_READY", &ready)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        hide_windows_console(&mut command);
        let mut child = command.spawn().unwrap();
        let result =
            wait_for_update_helper_ready(&mut child, &ready, WINDOWS_UPDATE_HELPER_READY_TIMEOUT);
        stop_and_reap_update_helper(&mut child);
        assert!(result.is_ok(), "helper acknowledgement failed: {result:?}");
        assert_eq!(std::fs::read_to_string(&ready).unwrap(), "ready");
    }

    #[cfg(windows)]
    #[test]
    fn windows_update_helper_is_valid_powershell_syntax() {
        let _powershell_test = lock_windows_powershell_test();
        let mut command = std::process::Command::new(system_powershell().unwrap());
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$null = [ScriptBlock]::Create($env:VC_UPDATE_HELPER_SOURCE)",
            ])
            .env("VC_UPDATE_HELPER_SOURCE", WINDOWS_UPDATE_HELPER);
        let output =
            bounded_command_output(&mut command, WINDOWS_POWERSHELL_TEST_TIMEOUT, None).unwrap();
        assert!(
            output.status.success(),
            "PowerShell parser rejected update helper: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn joined_platform_commands_never_use_unbounded_output_waits() {
        let source = include_str!("webui.rs");
        let blocking_output = [".", "output()"].concat();
        assert!(!source.contains(&blocking_output));
        assert!(source.contains("bounded_command_output("));
        assert!(source.contains("WINDOWS_SIGNATURE_COMMAND_TIMEOUT"));
        assert!(source.contains("MAC_UPDATE_COPY_TIMEOUT"));
    }
}

#[cfg(test)]
mod bounded_command_tests {
    use super::*;

    #[test]
    fn cleanup_wait_is_bounded_even_before_a_child_is_terminated() {
        let mut command = slow_command();
        let mut tree = CommandProcessTree::prepare(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        tree.attach(&child).unwrap();
        let started = Instant::now();
        let running = wait_for_command_exit(&mut child, Duration::from_millis(60));
        // Always clean the owned helper before assertions, including on error.
        stop_and_reap_command(&mut child, &mut tree);
        assert!(!running.unwrap());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(wait_for_command_exit(&mut child, Duration::ZERO).unwrap());
    }

    #[test]
    #[ignore = "Subprocess helper for UTF-8 stdin; only invoked by bounded input test"]
    fn stdin_echo_helper() {
        use std::io::{Read, Write};
        let mut bytes = Vec::new();
        std::io::stdin().read_to_end(&mut bytes).unwrap();
        std::io::stdout().write_all(&bytes).unwrap();
    }

    #[test]
    fn source_text_uses_stdin_without_shell_interpolation() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "webui::bounded_command_tests::stdin_echo_helper",
            "--ignored",
            "--nocapture",
        ]);
        let source =
            "中文 日本語 한국어 Hindi नमस्ते $(no_execution) `text` & | \"quoted\"\nnext line";
        let output = bounded_command_input(
            &mut command,
            Duration::from_secs(5),
            None,
            source.as_bytes().to_vec(),
        )
        .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout).unwrap().contains(source));
        assert!(!command
            .get_args()
            .any(|arg| arg.to_string_lossy().contains("no_execution")));
    }

    #[test]
    fn an_unread_stdin_pipe_still_obeys_the_deadline() {
        let mut command = slow_command();
        let started = Instant::now();
        let error = bounded_command_input(
            &mut command,
            Duration::from_millis(150),
            None,
            vec![b'x'; 16 * 1024],
        )
        .unwrap_err();
        assert!(matches!(error, BoundedCommandError::TimedOut(_)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    fn shell_command(_windows_script: &str, _unix_script: &str) -> std::process::Command {
        #[cfg(windows)]
        {
            let mut command = std::process::Command::new("cmd.exe");
            command.args(["/D", "/S", "/C", _windows_script]);
            command
        }
        #[cfg(not(windows))]
        {
            let mut command = std::process::Command::new("/bin/sh");
            command.args(["-c", _unix_script]);
            command
        }
    }

    fn slow_command() -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "webui::bounded_command_tests::slow_command_helper",
            "--ignored",
            "--nocapture",
        ]);
        command
    }

    fn oversized_output_command() -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "webui::bounded_command_tests::oversized_output_helper",
            "--ignored",
            "--nocapture",
        ]);
        command
    }

    #[test]
    #[ignore = "subprocess helper invoked by the bounded-command timeout contract"]
    fn slow_command_helper() {
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    #[ignore = "subprocess helper invoked by the bounded-command output contract"]
    fn oversized_output_helper() {
        use std::io::Write as _;

        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&vec![b'x'; COMMAND_OUTPUT_LIMIT + 1024])
            .unwrap();
        stdout.flush().unwrap();
    }

    #[test]
    fn bounded_command_captures_both_streams_and_nonzero_status() {
        let mut command = shell_command(
            "echo hello & echo problem 1>&2 & exit /b 7",
            "printf hello; printf problem >&2; exit 7",
        );
        let output = bounded_command_output(&mut command, Duration::from_secs(3), None).unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert!(String::from_utf8_lossy(&output.stdout).contains("hello"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("problem"));
    }

    #[test]
    fn bounded_command_kills_and_reaps_a_timed_out_child() {
        let mut command = slow_command();
        let started = Instant::now();
        let error =
            bounded_command_output(&mut command, Duration::from_millis(150), None).unwrap_err();
        assert!(matches!(error, BoundedCommandError::TimedOut(_)));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timed-out child was not reaped promptly: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn bounded_command_observes_shutdown_before_its_deadline() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let cancel = shutdown.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(75));
            cancel.store(true, Ordering::Release);
        });
        let mut command = slow_command();
        let started = Instant::now();
        let error = bounded_command_output(&mut command, Duration::from_secs(5), Some(&shutdown))
            .unwrap_err();
        canceller.join().unwrap();
        assert!(matches!(error, BoundedCommandError::Cancelled));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancelled child was not reaped promptly: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn bounded_command_drains_but_rejects_oversized_output() {
        let mut command = oversized_output_command();
        let started = Instant::now();
        let error = bounded_command_output(&mut command, Duration::from_secs(5), None).unwrap_err();
        assert!(matches!(error, BoundedCommandError::OutputTooLarge));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "oversized output blocked its child pipe: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn bounded_command_closes_pipes_inherited_by_a_descendant() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "webui::bounded_command_tests::command_tree_parent_helper",
                "--nocapture",
            ])
            .env("VOCALCODE_COMMAND_TREE_TEST", "parent");
        let started = Instant::now();
        let output = bounded_command_output(&mut command, Duration::from_secs(5), None).unwrap();
        assert!(output.status.success());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a descendant kept command pipes alive: {:?}",
            started.elapsed()
        );
    }

    #[test]
    #[allow(clippy::zombie_processes)]
    fn command_tree_parent_helper() {
        if std::env::var("VOCALCODE_COMMAND_TREE_TEST").as_deref() != Ok("parent") {
            return;
        }
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "webui::bounded_command_tests::command_tree_descendant_helper",
                "--nocapture",
            ])
            .env("VOCALCODE_COMMAND_TREE_TEST", "descendant")
            .spawn()
            .unwrap();
    }

    #[test]
    fn command_tree_descendant_helper() {
        if std::env::var("VOCALCODE_COMMAND_TREE_TEST").as_deref() == Ok("descendant") {
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    #[test]
    fn mac_update_arms_raii_cleanup_before_attach() {
        let source = include_str!("webui.rs");
        let update_start = source
            .find("/// macOS: download the dmg")
            .expect("macOS update function must exist");
        let update = &source[update_start..];
        let guard = update
            .find("let mut mount_guard = MacMountGuard::new(&mount)")
            .expect("mount cleanup guard must be created");
        let arm = update[guard..]
            .find("mount_guard.arm()")
            .map(|offset| guard + offset)
            .expect("mount cleanup guard must be armed");
        let attach = update[arm..]
            .find("let mut attach = std::process::Command::new(\"/usr/bin/hdiutil\")")
            .map(|offset| arm + offset)
            .expect("hdiutil attach must exist");
        assert!(guard < arm && arm < attach);
        assert!(source.contains("impl Drop for MacMountGuard"));
        assert!(source.contains("MAC_UPDATE_COMMAND_TIMEOUT, None"));
    }

    #[test]
    fn update_file_lock_times_out_and_recovers_after_its_owner_exits() {
        let root = std::env::temp_dir().join(format!(
            "vocalcode-update-lock-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("update.lock");
        let open = || {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .unwrap()
        };
        let owner = open();
        fs2::FileExt::lock_exclusive(&owner).unwrap();
        let contender = open();
        let started = Instant::now();
        let error =
            acquire_update_file_lock(&contender, started + Duration::from_millis(125), None)
                .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));

        fs2::FileExt::unlock(&owner).unwrap();
        acquire_update_file_lock(&contender, Instant::now() + Duration::from_secs(1), None)
            .unwrap();
        fs2::FileExt::unlock(&contender).unwrap();
        drop(contender);
        drop(owner);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod update_download_tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::AtomicBool;

    fn policy(io_slice_timeout: Duration) -> UpdateDownloadPolicy {
        UpdateDownloadPolicy {
            https_only: false,
            io_slice_timeout,
            transfer_timeout: Duration::from_secs(3),
            retry_delay: Duration::from_millis(5),
            max_no_progress_attempts: 3,
        }
    }

    fn sha256(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn request_headers(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 512];
        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            assert_ne!(count, 0, "client closed before sending HTTP headers");
            bytes.extend_from_slice(&buffer[..count]);
            assert!(bytes.len() < 32 * 1024, "unexpectedly large test request");
        }
        String::from_utf8(bytes).unwrap()
    }

    fn update_url(listener: &TcpListener) -> String {
        format!("http://{}/update.bin", listener.local_addr().unwrap())
    }

    #[test]
    fn update_download_resumes_only_at_the_verified_range_boundary() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = update_url(&listener);
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let headers = request_headers(&mut first).to_ascii_lowercase();
            assert!(headers.contains("range: bytes=0-"), "{headers}");
            first
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 0-3/8\r\nConnection: close\r\n\r\nabcd",
                )
                .unwrap();

            let (mut second, _) = listener.accept().unwrap();
            let headers = request_headers(&mut second).to_ascii_lowercase();
            assert!(headers.contains("range: bytes=4-"), "{headers}");
            second
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 4-7/8\r\nConnection: close\r\n\r\nefgh",
                )
                .unwrap();
        });
        let work = UpdateWorkDir::create().unwrap();
        let target = work.path().join("update.bin");
        let expected = sha256(b"abcdefgh");
        let mut progress = Vec::new();

        let got = download_update_with_policy(
            &url,
            &target,
            &expected,
            8,
            policy(Duration::from_millis(500)),
            || false,
            |done, total| progress.push((done, total)),
        )
        .unwrap();

        assert_eq!(got, expected);
        assert_eq!(std::fs::read(&target).unwrap(), b"abcdefgh");
        assert_eq!(progress.last(), Some(&(8, Some(8))));
        server.join().unwrap();
    }

    #[test]
    fn update_download_rejects_a_mismatched_range_and_removes_partial_bytes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = update_url(&listener);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = request_headers(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 1-3/4\r\nConnection: close\r\n\r\nbcd",
                )
                .unwrap();
        });
        let work = UpdateWorkDir::create().unwrap();
        let target = work.path().join("update.bin");

        let error = download_update_with_policy(
            &url,
            &target,
            &sha256(b"abcd"),
            4,
            policy(Duration::from_millis(500)),
            || false,
            |_, _| {},
        )
        .unwrap_err();

        assert!(error.to_string().contains("expected 0"), "{error}");
        assert!(!target.exists(), "failed update bytes must be removed");
        server.join().unwrap();
    }

    #[test]
    fn update_download_bounds_repeated_empty_range_bodies() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = update_url(&listener);
        let server = std::thread::spawn(move || {
            for _ in 0..=3 {
                let (mut stream, _) = listener.accept().unwrap();
                let headers = request_headers(&mut stream).to_ascii_lowercase();
                assert!(headers.contains("range: bytes=0-"), "{headers}");
                stream
                    .write_all(
                        b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-7/8\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
            }
        });
        let work = UpdateWorkDir::create().unwrap();
        let target = work.path().join("update.bin");
        let started = Instant::now();

        let error = download_update_with_policy(
            &url,
            &target,
            &sha256(b"abcdefgh"),
            8,
            policy(Duration::from_millis(500)),
            || false,
            |_, _| {},
        )
        .unwrap_err();

        assert!(
            matches!(error, UpdateDownloadError::Failed(message) if message.contains("without progress after 3 attempts"))
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!target.exists(), "empty response bytes must be removed");
        server.join().unwrap();
    }

    #[test]
    fn update_download_rejects_invalid_manifest_sizes_before_creating_a_file() {
        let work = UpdateWorkDir::create().unwrap();
        for size in [0, crate::MAX_UPDATE_BYTES + 1] {
            let target = work.path().join(format!("invalid-{size}.bin"));
            let error = download_update_with_policy(
                "http://127.0.0.1:1/update.bin",
                &target,
                &sha256(b"x"),
                size,
                policy(Duration::from_millis(50)),
                || false,
                |_, _| {},
            )
            .unwrap_err();
            assert!(error.to_string().contains("manifest size"), "{error}");
            assert!(!target.exists());
        }
    }

    #[test]
    fn update_disk_reserve_is_checked_with_overflow_protection() {
        assert_eq!(
            update_required_disk_space(1),
            Some(UPDATE_DISK_RESERVE_BYTES + 1)
        );
        assert_eq!(update_required_disk_space(u64::MAX), None);
    }

    #[test]
    fn update_download_rejects_content_length_that_disagrees_with_manifest() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = update_url(&listener);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = request_headers(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\nabcdefghi",
                )
                .unwrap();
        });
        let work = UpdateWorkDir::create().unwrap();
        let target = work.path().join("update.bin");

        let error = download_update_with_policy(
            &url,
            &target,
            &sha256(b"abcdefgh"),
            8,
            policy(Duration::from_millis(500)),
            || false,
            |_, _| {},
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("Content-Length mismatch"),
            "{error}"
        );
        assert!(!target.exists());
        server.join().unwrap();
    }

    #[test]
    fn update_download_rejects_content_range_total_that_disagrees_with_manifest() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = update_url(&listener);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = request_headers(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Length: 8\r\nContent-Range: bytes 0-7/9\r\nConnection: close\r\n\r\nabcdefgh",
                )
                .unwrap();
        });
        let work = UpdateWorkDir::create().unwrap();
        let target = work.path().join("update.bin");

        let error = download_update_with_policy(
            &url,
            &target,
            &sha256(b"abcdefgh"),
            8,
            policy(Duration::from_millis(500)),
            || false,
            |_, _| {},
        )
        .unwrap_err();

        assert!(error.to_string().contains("total mismatch"), "{error}");
        assert!(!target.exists());
        server.join().unwrap();
    }

    #[test]
    fn chunked_update_cannot_write_past_the_exact_manifest_size() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = update_url(&listener);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = request_headers(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n9\r\nabcdefghi\r\n0\r\n\r\n",
                )
                .unwrap();
        });
        let work = UpdateWorkDir::create().unwrap();
        let target = work.path().join("update.bin");

        let error = download_update_with_policy(
            &url,
            &target,
            &sha256(b"abcdefgh"),
            8,
            policy(Duration::from_millis(500)),
            || false,
            |_, _| {},
        )
        .unwrap_err();

        assert!(error.to_string().contains("exact manifest size"), "{error}");
        assert!(!target.exists());
        server.join().unwrap();
    }

    #[test]
    fn stalled_update_body_observes_shutdown_within_one_io_slice() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = update_url(&listener);
        let (body_started_tx, body_started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = request_headers(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Length: 8\r\nContent-Range: bytes 0-7/8\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            stream.flush().unwrap();
            body_started_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(2));
        });
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_from_thread = cancelled.clone();
        let canceller = std::thread::spawn(move || {
            body_started_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            // Give the local client time to enter its body read. The server
            // deliberately sends no body bytes, so this exercises the only
            // place a plain atomic check cannot interrupt by itself.
            std::thread::sleep(Duration::from_millis(75));
            cancel_from_thread.store(true, Ordering::Release);
        });
        let work = UpdateWorkDir::create().unwrap();
        let target = work.path().join("update.bin");
        let started = Instant::now();

        let error = download_update_with_policy(
            &url,
            &target,
            &sha256(b"abcdefgh"),
            8,
            policy(Duration::from_millis(150)),
            || cancelled.load(Ordering::Acquire),
            |_, _| {},
        )
        .unwrap_err();
        let elapsed = started.elapsed();

        assert_eq!(error, UpdateDownloadError::Cancelled);
        assert!(
            elapsed >= Duration::from_millis(75),
            "the test did not reach the stalled body read: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "cancellation took {elapsed:?}"
        );
        assert!(!target.exists(), "cancelled update bytes must be removed");
        let _ = release_tx.send(());
        canceller.join().unwrap();
        server.join().unwrap();
    }
}

#[cfg(test)]
mod purge_tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("vocalcode-purge-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("models/paraformer-zh")).unwrap();
        std::fs::write(d.join("models/paraformer-zh/model.onnx"), b"x").unwrap();
        std::fs::write(d.join(TRIAL_FILE), b"signed-trial-cache").unwrap();
        std::fs::write(d.join(TRUSTED_TIME_FILE), b"trusted-time-cache").unwrap();
        std::fs::write(d.join("vocalcode.toml"), b"language = \"zh\"").unwrap();
        std::fs::write(d.join("replacements.txt"), b"a => b").unwrap();
        std::fs::create_dir_all(d.join("webview2/settings/Default")).unwrap();
        std::fs::write(
            d.join("webview2/settings/Default/Web Data"),
            b"browser-profile",
        )
        .unwrap();
        d
    }

    /// The models are the reason the button exists; the signed trial cache is
    /// the one thing pressing it must not hand back.
    #[test]
    fn purge_clears_the_models_and_keeps_the_trial() {
        let d = scratch("keeps");
        purge_app_data(&d).unwrap();
        assert!(!d.join("models").exists(), "the 537 MB is the point");
        assert!(!d.join("vocalcode.toml").exists());
        assert!(!d.join("replacements.txt").exists());
        assert!(!d.join("webview2").exists(), "browser state is user data");
        assert!(
            d.join(TRIAL_FILE).exists(),
            "uninstalling must not restart the trial"
        );
        assert_eq!(
            std::fs::read_to_string(d.join(TRIAL_FILE)).unwrap(),
            "signed-trial-cache"
        );
        assert_eq!(
            std::fs::read_to_string(d.join(TRUSTED_TIME_FILE)).unwrap(),
            "trusted-time-cache"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Twice in a row, because someone will: the second press must not throw and
    /// must not be a way to get the signed cache deleted after all.
    #[test]
    fn purging_twice_is_harmless() {
        let d = scratch("twice");
        purge_app_data(&d).unwrap();
        purge_app_data(&d).unwrap();
        assert!(d.join(TRIAL_FILE).exists());
        assert!(d.join(TRUSTED_TIME_FILE).exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn purge_refuses_the_executable_directory() {
        let executable_dir = crate::paths::exe_dir();
        let error = purge_app_data(&executable_dir).unwrap_err();
        assert!(
            error.contains("refusing to remove the application directory"),
            "{error}"
        );
    }

    #[test]
    fn purge_guard_refuses_any_ancestor_of_the_executable() {
        let root = std::env::temp_dir().join("vocalcode-purge-boundary");
        let executable = root.join("Program Files/VocalCode");
        assert!(purge_path_contains_executable(&root, &executable));
        assert!(purge_path_contains_executable(&executable, &executable));
        assert!(!purge_path_contains_executable(
            &root.join("AppData/VocalCode"),
            &executable
        ));
    }

    #[test]
    fn uninstall_disables_autostart_before_purge_and_stops_if_that_fails() {
        let calls = std::cell::RefCell::new(Vec::new());
        run_uninstall_cleanup(
            || {
                calls.borrow_mut().push("autostart");
                Ok(())
            },
            || {
                calls.borrow_mut().push("purge");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*calls.borrow(), ["autostart", "purge"]);

        calls.borrow_mut().clear();
        let error = run_uninstall_cleanup(
            || {
                calls.borrow_mut().push("autostart");
                Err("registry denied".to_string())
            },
            || {
                calls.borrow_mut().push("purge");
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.contains("registry denied"));
        assert_eq!(
            *calls.borrow(),
            ["autostart"],
            "data removal must not start while login launch remains enabled"
        );
    }

    /// A symlink is unlinked, not followed. Deleting whatever a user pointed at
    /// their models directory is not this button's business.
    #[cfg(unix)]
    #[test]
    fn a_symlink_is_unlinked_rather_than_followed() {
        let d = scratch("symlink");
        let outside = std::env::temp_dir().join("vocalcode-purge-outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keepme"), b"not ours").unwrap();
        std::os::unix::fs::symlink(&outside, d.join("linked")).unwrap();
        purge_app_data(&d).unwrap();
        assert!(!d.join("linked").exists(), "the link goes");
        assert!(outside.join("keepme").exists(), "what it pointed at stays");
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_app_data_root_is_refused() {
        let scratch = std::env::temp_dir().join("vocalcode-purge-root-link");
        let outside = std::env::temp_dir().join("vocalcode-purge-root-target");
        let _ = std::fs::remove_file(&scratch);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keepme"), b"not ours").unwrap();
        std::os::unix::fs::symlink(&outside, &scratch).unwrap();
        let error = purge_app_data(&scratch).unwrap_err();
        assert!(error.contains("symlinked app data"), "{error}");
        assert!(outside.join("keepme").exists());
        let _ = std::fs::remove_file(&scratch);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(windows)]
    #[test]
    fn trial_stamp_is_preserved_case_insensitively_on_windows() {
        let d = scratch("trial-case");
        let lower = d.join(TRIAL_FILE);
        let intermediate = d.join("trial-temp.dat");
        let upper = d.join("VOCALCODE-TRIAL.DAT");
        std::fs::rename(&lower, &intermediate).unwrap();
        std::fs::rename(&intermediate, &upper).unwrap();
        purge_app_data(&d).unwrap();
        assert!(upper.exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
