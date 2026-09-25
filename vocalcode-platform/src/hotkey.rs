//! Global push-to-talk listener via `rdev` (cross-platform low-level input).
//!
//! `rdev::grab` blocks and streams every input event, so this runs on its own
//! thread and forwards only the events we care about as [`TriggerEvent`]s. It
//! also powers **key capture** for the settings UI: because the grab consumes
//! the bound mouse buttons globally, the WebView never sees them — so binding a
//! new trigger is done here, at the hook, where every key and button is visible.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rdev::{Button, EventType, Key as RdevKey};
use vocalcode_core::config::{DeviceSelector, GamepadButton, MouseExtra, Trigger};
use vocalcode_core::error::{Result, VocalCodeError};
use vocalcode_core::traits::{HotkeyListener, TriggerEvent, TriggerId, TriggerSource};
use vocalcode_core::TriggerEventSender;

/// How long a capture waits with nothing happening at all. v0.4.19 had no
/// deadline: the prompt stayed armed until it was answered, so it could not run
/// out from under somebody who was reading it. Fifteen seconds was short enough
/// to expire while a person looked away from the mouse and hunted for a key —
/// after which every key they pressed arrived with nothing waiting for it, and
/// produced no page event at all. Any input during the window pushes this out
/// again (see [`Inner::touch`]), so it only has to outlast hesitation.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(60);

/// Absolute ceiling on one armed prompt, however much the user keeps touching
/// it. A live capture swallows the control it captures, so a prompt left armed
/// forever would eventually eat a keystroke somebody meant for their editor.
const CAPTURE_MAX: Duration = Duration::from_secs(180);

/// Opt-in raw input trace. Read once; the hook runs on every keystroke.
static TRACE_INPUT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Monotonic observation of a physical, unmodified Enter press. The
/// correction watcher snapshots this generation and never consumes the event;
/// the foreground editor still receives Enter normally. Keeping this in the
/// existing global input hook avoids a second hook and lets a web composer
/// replace its DOM node immediately after submit without losing the commit.
static CORRECTION_SUBMIT_GENERATION: AtomicU64 = AtomicU64::new(0);

pub(crate) fn correction_submit_generation() -> u64 {
    CORRECTION_SUBMIT_GENERATION.load(Ordering::Acquire)
}

/// Monotonic count of talk presses passed through because the engine was not
/// ready (model downloading, microphone broken, a decode in progress). The key
/// still reaches the foreground app exactly as before; this only lets the app
/// say why nothing started, instead of looking dead. Callbacks never wait on
/// it — the app compares generations on its own UI cadence.
static TALK_WHILE_NOT_READY: AtomicU64 = AtomicU64::new(0);

/// See [`TALK_WHILE_NOT_READY`].
pub fn talk_presses_while_not_ready() -> u64 {
    TALK_WHILE_NOT_READY.load(Ordering::Acquire)
}

pub(crate) fn note_talk_while_not_ready() {
    TALK_WHILE_NOT_READY.fetch_add(1, Ordering::AcqRel);
}

/// Diagnostics emitted by OS input callbacks. Every variant is fixed-size and
/// allocation-free: callbacks may only `try_send` one into the bounded queue;
/// formatting and `log` I/O belong to the dedicated consumer thread.
#[derive(Debug, Clone, Copy)]
pub(crate) enum HookDiagnostic {
    CaptureTimedOut,
    CaptureHint,
    CaptureFinished,
    Input {
        input: RdevInput,
        pressed: bool,
        capturing: bool,
        held: bool,
    },
    AutoRepeat {
        input: RdevInput,
        active: bool,
        captured: bool,
    },
    CaptureInjected(RdevInput),
    CaptureInjectedCompanionSuppressed(RdevInput),
    CaptureOffered {
        input: RdevInput,
        supported: bool,
    },
    NativeCaptureOffered {
        control: NativeControl,
        vendor_id: u16,
        product_id: u16,
    },
    NativeVendorRejected {
        control: NativeControl,
        vendor_id: u16,
        product_id: u16,
    },
    #[cfg(target_os = "macos")]
    MacHidLifecycle {
        connected: bool,
        fingerprint: u64,
        vendor_id: u16,
        product_id: u16,
    },
    #[cfg(windows)]
    WindowsRawLifecycle {
        connected: bool,
        fingerprint: u64,
        vendor_id: u16,
        product_id: u16,
    },
    #[cfg(windows)]
    WindowsSlowCallback(u128),
    #[cfg(windows)]
    WindowsHookProc {
        wparam: usize,
        vk: u32,
    },
    #[cfg(target_os = "macos")]
    MacTapDisabled(u32),
}

const HOOK_DIAGNOSTIC_CAPACITY: usize = 128;
static HOOK_DIAGNOSTICS: OnceLock<SyncSender<HookDiagnostic>> = OnceLock::new();
static HOOK_DIAGNOSTICS_DROPPED: AtomicU64 = AtomicU64::new(0);

fn write_hook_diagnostic(event: HookDiagnostic) {
    match event {
        HookDiagnostic::CaptureTimedOut => {
            log::info!("capture: timed out with nothing pressed")
        }
        HookDiagnostic::CaptureHint => log::info!("capture: saw an unbindable control"),
        HookDiagnostic::CaptureFinished => log::info!("capture: answered by the input hook"),
        HookDiagnostic::Input {
            input,
            pressed,
            capturing,
            held,
        } => log::info!(
            "input: {input:?} pressed={pressed} capturing={capturing} held={held}"
        ),
        HookDiagnostic::AutoRepeat {
            input,
            active,
            captured,
        } => log::warn!(
            "capture: ignoring {input:?} as auto-repeat; it is already held (active={active}, captured={captured})"
        ),
        HookDiagnostic::CaptureInjected(input) => {
            log::info!("capture: {input:?} accepted from injected input")
        }
        HookDiagnostic::CaptureInjectedCompanionSuppressed(input) => {
            log::info!("capture: suppressed injected companion {input:?}")
        }
        HookDiagnostic::CaptureOffered { input, supported } => {
            log::info!("capture: {input:?} offered (usable={supported})")
        }
        HookDiagnostic::NativeCaptureOffered {
            control,
            vendor_id,
            product_id,
        } => log::info!(
            "capture: offering {control:?} from native device ({vendor_id:04x}:{product_id:04x})"
        ),
        HookDiagnostic::NativeVendorRejected {
            control,
            vendor_id,
            product_id,
        } => log::debug!(
            "capture: not offering vendor-page {control:?} from native device ({vendor_id:04x}:{product_id:04x})"
        ),
        #[cfg(target_os = "macos")]
        HookDiagnostic::MacHidLifecycle {
            connected,
            fingerprint,
            vendor_id,
            product_id,
        } => log::info!(
            "macOS HID device {}: fingerprint={fingerprint:016x} ({vendor_id:04x}:{product_id:04x})",
            if connected { "active" } else { "disconnected" }
        ),
        #[cfg(windows)]
        HookDiagnostic::WindowsRawLifecycle {
            connected,
            fingerprint,
            vendor_id,
            product_id,
        } => log::info!(
            "Raw Input device {}: fingerprint={fingerprint:016x} ({vendor_id:04x}:{product_id:04x})",
            if connected { "connected" } else { "disconnected" }
        ),
        #[cfg(windows)]
        HookDiagnostic::WindowsSlowCallback(spent_ms) => log::warn!(
            "hook: a callback took {spent_ms} ms; the budget before Windows starts skipping the hook is ~300 ms"
        ),
        #[cfg(windows)]
        HookDiagnostic::WindowsHookProc { wparam, vk } => {
            log::info!("hookproc: entered wparam={wparam:#x} vk={vk:#x}")
        }
        #[cfg(target_os = "macos")]
        HookDiagnostic::MacTapDisabled(etype) => {
            log::warn!("event tap disabled by the system (type {etype:#x}); re-enabling")
        }
    }
}

fn enqueue_hook_diagnostic(
    sender: &SyncSender<HookDiagnostic>,
    dropped: &AtomicU64,
    event: HookDiagnostic,
) {
    if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) = sender.try_send(event) {
        dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Start the bounded diagnostic drain before installing any OS callback.
/// Failure is fatal to listener installation: silently falling back to direct
/// logging would reintroduce synchronous file/stderr I/O in the callback.
pub(crate) fn start_hook_diagnostics() -> std::result::Result<(), String> {
    // Resolve the environment opt-in on this ordinary thread. The first hook
    // callback must not pay environment/OnceLock initialization either.
    let _ = TRACE_INPUT.get_or_init(|| std::env::var_os("VOCALCODE_TRACE_INPUT").is_some());
    if HOOK_DIAGNOSTICS.get().is_some() {
        return Ok(());
    }
    let (sender, receiver) = std::sync::mpsc::sync_channel(HOOK_DIAGNOSTIC_CAPACITY);
    std::thread::Builder::new()
        .name("vocalcode-hook-diagnostics".to_string())
        .spawn(move || {
            while let Ok(event) = receiver.recv() {
                let dropped = HOOK_DIAGNOSTICS_DROPPED.swap(0, Ordering::Relaxed);
                if dropped != 0 {
                    log::warn!(
                        "hook diagnostics dropped {dropped} events while the queue was full"
                    );
                }
                write_hook_diagnostic(event);
            }
        })
        .map_err(|error| format!("could not start hook diagnostic drain: {error}"))?;
    // A racing backend may have installed the process-global sender first. In
    // that case this sender is dropped and its just-created drain exits; both
    // callers still use the already installed bounded queue.
    let _ = HOOK_DIAGNOSTICS.set(sender);
    Ok(())
}

#[inline]
pub(crate) fn hook_diagnostic(event: HookDiagnostic) {
    if let Some(sender) = HOOK_DIAGNOSTICS.get() {
        enqueue_hook_diagnostic(sender, &HOOK_DIAGNOSTICS_DROPPED, event);
    }
}

/// Shared "record a new binding" state between the settings UI and the hook.
/// The UI calls [`CaptureShared::start`]; the next key/button the hook sees
/// becomes the result (and is swallowed, not acted on). Poll [`take_result`].
#[derive(Default)]
pub struct CaptureShared {
    inner: Mutex<Inner>,
}

struct Inner {
    /// Which trigger we're binding ("talk"/"send"), or None when idle.
    which: Option<String>,
    /// Completed captures. This is a queue rather than one slot because clicking
    /// Add on a second row must cancel the first row instead of leaving its UI
    /// stuck forever while overwriting the result.
    results: VecDeque<(String, String)>,
    deadline: Option<Instant>,
    timeout: Duration,
    /// When the current prompt was armed, so [`touch`](Inner::touch) can extend
    /// the deadline without extending it forever.
    started: Option<Instant>,
    /// The last thing we explained, so the same note is not repeated on every
    /// press of the same unbindable control — but a *different* one can still
    /// speak. A single `bool` meant the first note silenced every later one, so
    /// pressing a letter and then a mouse button explained only the letter.
    last_hint: Option<String>,
    /// Bumped by every [`CaptureShared::start_for`]. A modifier candidacy is
    /// stamped with the generation it was born under, so its release can only
    /// answer that prompt — never a newer one armed while the key was held.
    generation: u64,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            which: None,
            results: VecDeque::new(),
            deadline: None,
            timeout: CAPTURE_TIMEOUT,
            started: None,
            last_hint: None,
            generation: 0,
        }
    }
}

impl Inner {
    fn expire(&mut self) {
        self.expire_at(Instant::now());
    }

    fn expire_at(&mut self, now: Instant) {
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            if let Some(which) = self.which.take() {
                // Not the empty string, which the page reads as "the user
                // pressed Escape" and answers by silently restoring the old
                // binding. Running out of time is not a decision anybody made:
                // it looks exactly like the prompt giving up without a word,
                // which is how "nothing happened when I pressed my key" ends.
                hook_diagnostic(HookDiagnostic::CaptureTimedOut);
                self.results.push_back((which, "timeout".to_string()));
            }
            self.deadline = None;
            self.started = None;
        }
    }

    /// The user just produced input while the prompt was open, so they are
    /// demonstrably still at the machine: push the deadline out.
    ///
    /// This is what makes "click Add, read the row, hunt for the key, press it"
    /// safe. Clamped to `started + CAPTURE_MAX` so somebody who walks away
    /// mid-capture cannot leave a prompt armed indefinitely.
    fn touch(&mut self) {
        self.touch_at(Instant::now());
    }

    fn touch_at(&mut self, now: Instant) {
        let (Some(started), true) = (self.started, self.which.is_some()) else {
            return;
        };
        let extended = now + self.timeout;
        self.deadline = Some(extended.min(started + CAPTURE_MAX));
    }

    fn keep_alive_at(&mut self, now: Instant) {
        self.expire_at(now);
        self.touch_at(now);
    }
}

impl CaptureShared {
    /// OS callbacks must not unwind across their C/system ABI if another thread
    /// poisoned capture state. The protected data remains structurally valid,
    /// so recover ownership and let the existing state machine fail safely.
    fn lock_inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Begin capturing the next input as the binding for `which`.
    pub fn start(&self, which: &str) {
        self.start_for(which, CAPTURE_TIMEOUT);
    }

    /// Begin a capture with an explicit deadline (public for deterministic
    /// tests and for callers that want a shorter guided capture).
    pub fn start_for(&self, which: &str, timeout: Duration) {
        let mut g = self.lock_inner();
        g.expire();
        log::info!("capture: opened for {which} ({}s)", timeout.as_secs());
        let mut rearmed = false;
        if let Some(previous) = g.which.replace(which.to_string()) {
            if previous == which {
                // Clicking Add again on the row that is *already* waiting means
                // "I'm still here, start the clock over". It is not a
                // cancellation, and answering it with the cancellation spelling
                // is what made this feature look broken: the page cannot tell
                // that from Escape, so it put the old binding back and the
                // prompt disappeared about 180 ms after the click — while this
                // hook went on waiting for a key for another fifteen seconds.
                // Clicking again did it again. The owner's log has nine of these
                // in twelve seconds, which is simply what a person does when a
                // button looks like it did nothing.
                log::info!("capture: re-arming the pending {which} capture");
                rearmed = true;
            } else {
                log::info!("capture: cancelling the pending {previous} capture first");
                g.results.push_back((previous, String::new()));
            }
        }
        let now = Instant::now();
        g.generation = g.generation.wrapping_add(1);
        g.timeout = timeout;
        g.deadline = Some(now + timeout);
        // Clicking Add is an explicit act, so it always buys a fresh window —
        // the ceiling exists to bound *implicit* extension by [`Inner::touch`],
        // not to punish somebody who deliberately starts over.
        g.started = Some(now);
        // Re-arming keeps the "already explained" note: the hint exists to say
        // something once, and a person clicking a button that looks inert must
        // not be told the same thing on every click.
        if !rearmed {
            g.last_hint = None;
        }
    }

    /// Note that the user is still there, so the prompt does not expire under
    /// them. Called from the hook on every input seen while a capture is open;
    /// one uncontended lock and no I/O, which is all a low-level hook can pay.
    pub(crate) fn keep_alive(&self) {
        let mut g = self.lock_inner();
        g.keep_alive_at(Instant::now());
    }

    /// Tell the user what the hook is seeing, without ending the capture.
    ///
    /// A key that arrives as a mouse click is not something the prompt can
    /// bind, but staying silent about it is what cost the owner an afternoon:
    /// he pressed a key, the row said nothing, and the only place the truth
    /// existed was a log file. The capture stays open, so the real key still
    /// works if it ever arrives.
    pub(crate) fn hint(&self, code: &str) -> bool {
        let mut g = self.lock_inner();
        g.expire();
        let Some(which) = g.which.clone() else {
            return false;
        };
        // Pressing something unbindable is still proof the user is present.
        g.touch();
        if g.last_hint.as_deref() == Some(code) {
            return false;
        }
        g.last_hint = Some(code.to_string());
        hook_diagnostic(HookDiagnostic::CaptureHint);
        g.results.push_back((which, code.to_string()));
        true
    }
    /// Take a finished capture result, if any: (which, code).
    pub fn take_result(&self) -> Option<(String, String)> {
        let mut g = self.lock_inner();
        g.expire();
        g.results.pop_front()
    }
    /// Whether a result is waiting to be collected — without collecting it.
    /// Lets a notifier thread wake the UI event loop the moment there is
    /// something to deliver, while delivery itself stays in one place. Runs
    /// `expire` too, so a lapsed deadline becomes a deliverable "timeout"
    /// even when nothing else is polling.
    pub fn has_result(&self) -> bool {
        let mut g = self.lock_inner();
        g.expire();
        !g.results.is_empty()
    }
    pub(crate) fn is_capturing(&self) -> bool {
        let mut g = self.lock_inner();
        g.expire();
        g.which.is_some()
    }
    /// Which arming of the prompt is current. See [`Inner::generation`].
    pub(crate) fn generation(&self) -> u64 {
        self.lock_inner().generation
    }
    /// Cancel a pending capture explicitly. Returns whether one was active.
    pub fn cancel(&self) -> bool {
        let mut g = self.lock_inner();
        g.expire();
        if let Some(which) = g.which.take() {
            g.results.push_back((which, String::new()));
            g.deadline = None;
            g.started = None;
            true
        } else {
            false
        }
    }
    /// Answer a pending capture from a `KeyboardEvent.code` the settings page
    /// read, rather than from the hook.
    ///
    /// The page is the only observer that works while our own window has focus:
    /// a Chromium-backed foreground window stops `WH_KEYBOARD_LL` being called
    /// at all, and that is the state a person is in when they click Add. Same
    /// queue and same state machine as the hook's path — if the hook did see
    /// the key first, nothing is pending here and this answers nothing, which
    /// is what a duplicate should do.
    pub fn answer_from_page(&self, web_code: &str) {
        let (answer, usable) = capture_code_from_web(web_code);
        log::info!("capture: page key {web_code} -> {answer} (usable={usable})");
        if usable {
            self.finish(answer);
        } else {
            self.hint(&answer);
        }
    }

    /// Record a captured code for the pending trigger; returns true if consumed.
    pub(crate) fn finish(&self, code: String) -> bool {
        let mut g = self.lock_inner();
        g.expire();
        match g.which.take() {
            Some(which) => {
                hook_diagnostic(HookDiagnostic::CaptureFinished);
                g.results.push_back((which, code));
                g.deadline = None;
                g.started = None;
                true
            }
            None => false,
        }
    }
}

/// Live-updatable (talk, send) triggers — the settings UI writes here and the
/// hook reads it on the next event, so rebinding takes effect with no restart.
///
/// Each is a set of alternatives:
/// any one of them fires the action, so a mouse binding and a keyboard binding
/// can coexist and whichever device is actually attached wins. An empty list
/// disables that action.
/// `(talk, send, teach)`, read fresh by the tap on every event so rebinding
/// needs no restart.
pub type SharedTriggers = Arc<Mutex<(Vec<Trigger>, Vec<Trigger>, Vec<Trigger>)>>;

pub struct RdevHotkey {
    triggers: SharedTriggers,
    capture: Arc<CaptureShared>,
    ready: Arc<AtomicBool>,
    installed: Arc<AtomicBool>,
}

impl RdevHotkey {
    pub fn new(triggers: SharedTriggers, capture: Arc<CaptureShared>) -> Self {
        Self::new_with_readiness(triggers, capture, Arc::new(AtomicBool::new(true)))
    }

    /// Construct a listener gated by the engine's live readiness flag. Capture
    /// remains available while a model downloads, but a normal bound key is
    /// consumed only after its event has reached a ready receiver.
    pub fn new_with_readiness(
        triggers: SharedTriggers,
        capture: Arc<CaptureShared>,
        ready: Arc<AtomicBool>,
    ) -> Self {
        Self::new_with_readiness_and_installation(
            triggers,
            capture,
            ready,
            Arc::new(AtomicBool::new(false)),
        )
    }

    /// Construct a listener with a separate installation handshake. `ready`
    /// gates runtime actions; `installed` says the OS hook/message loop itself
    /// is live. Keeping them separate prevents model/audio readiness from
    /// claiming input is available before SetWindowsHookEx has succeeded.
    pub fn new_with_readiness_and_installation(
        triggers: SharedTriggers,
        capture: Arc<CaptureShared>,
        ready: Arc<AtomicBool>,
        installed: Arc<AtomicBool>,
    ) -> Self {
        Self {
            triggers,
            capture,
            ready,
            installed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Talk,
    Send,
    Teach,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum RdevInput {
    Key(RdevKey),
    Mouse(Button),
    Consumer(u16),
}

/// Concrete source metadata supplied by a native HID/gamepad backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeDevice {
    pub stable_id: String,
    pub fingerprint: u64,
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    pub serial: Option<String>,
}

impl NativeDevice {
    pub(crate) fn new(
        stable_id: String,
        vendor_id: Option<u16>,
        product_id: Option<u16>,
        serial: Option<String>,
    ) -> Self {
        let fingerprint = fnv1a64(stable_id.as_bytes());
        Self {
            stable_id,
            fingerprint,
            vendor_id,
            product_id,
            serial,
        }
    }

    fn selector(&self) -> DeviceSelector {
        // A Raw Input interface path can change after moving a USB receiver to
        // another port or re-pairing Bluetooth. Prefer the portable hardware
        // identity whenever the device exposes one; fall back to the opaque
        // path only for devices that publish no VID/PID/serial at all.
        let portable =
            self.serial.is_some() || self.vendor_id.is_some() || self.product_id.is_some();
        DeviceSelector {
            stable_id: (!portable).then(|| self.stable_id.clone()),
            vendor_id: self.vendor_id,
            product_id: self.product_id,
            serial: self.serial.clone(),
        }
    }

    fn matches(&self, selector: &DeviceSelector) -> bool {
        selector.matches(
            Some(&self.stable_id),
            self.vendor_id,
            self.product_id,
            self.serial.as_deref(),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum NativeControl {
    Gamepad(GamepadButton),
    Consumer(u16),
    Hid {
        usage_page: u16,
        usage: u16,
        control: u32,
    },
}

impl NativeControl {
    fn id(self, device: &NativeDevice) -> TriggerId {
        match self {
            NativeControl::Gamepad(button) => TriggerId::new(
                TriggerSource::Gamepad,
                device.fingerprint,
                gamepad_control(button),
            ),
            NativeControl::Consumer(usage) => TriggerId::new(
                TriggerSource::ConsumerControl,
                device.fingerprint,
                u32::from(usage),
            ),
            NativeControl::Hid {
                usage_page,
                usage,
                control,
            } => TriggerId::new(
                TriggerSource::Hid,
                device.fingerprint,
                (u32::from(usage_page) << 16) ^ u32::from(usage) ^ control.rotate_left(7),
            ),
        }
    }

    fn trigger(self, device: &NativeDevice) -> Trigger {
        match self {
            // XInput user slots are transient. Use an any-gamepad selector for
            // them; Raw Input devices carry a stable `raw:*` selector.
            NativeControl::Gamepad(button) if device.stable_id.starts_with("xinput:") => {
                Trigger::GamepadButton {
                    device: DeviceSelector::any(),
                    button,
                }
            }
            NativeControl::Gamepad(button) => Trigger::GamepadButton {
                device: device.selector(),
                button,
            },
            NativeControl::Consumer(usage) => Trigger::ConsumerControl {
                device: device.selector(),
                usage,
            },
            NativeControl::Hid {
                usage_page,
                usage,
                control,
            } => Trigger::HidButton {
                device: device.selector(),
                usage_page,
                usage,
                control,
            },
        }
    }
}

/// Edge/state handling shared by Raw Input and XInput. Native sources cannot be
/// suppressed by rdev, but they still need repeat filtering, capture, live
/// rebinding and disconnect releases.
pub(crate) struct NativeDispatcher {
    triggers: SharedTriggers,
    capture: Arc<CaptureShared>,
    ready: Arc<AtomicBool>,
    tx: TriggerEventSender,
    /// Every physical control currently held, including unbound controls and
    /// presses observed while the engine is not ready. `active` is deliberately
    /// narrower: it contains only actions successfully sent to the engine.
    physical_down: HashSet<TriggerId>,
    active: HashMap<TriggerId, Action>,
    captured_down: HashSet<TriggerId>,
}
#[derive(Default)]
struct HookState {
    physical_down: HashSet<TriggerId>,
    /// The pressed flag records whether the press that established the hold
    /// was synthetic: an injected release may only tear down a hold that an
    /// injected press created. Without it, the app's own paste chord's
    /// Ctrl-up cut off a recording the person was physically holding open.
    active: HashMap<TriggerId, (Action, bool)>,
    captured_down: HashSet<TriggerId>,
    /// A modifier pressed while a capture is open is only a *candidate*: it
    /// binds when it is released with nothing else pressed in between, and it
    /// stops being one the moment a second control goes down. A held modifier
    /// is how every chord in the OS begins — Alt+Tab and Ctrl+C pressed at an
    /// open prompt used to bind the modifier and cost the user the shortcut,
    /// which is the single most-repeated accident in this file's history.
    /// The third field is the capture generation the candidacy was born in:
    /// a release may only answer the prompt that saw the press, never a
    /// newer one armed while the modifier was still held.
    pending_modifier: Option<(TriggerId, String, u64)>,
}

impl NativeDispatcher {
    pub(crate) fn new(
        triggers: SharedTriggers,
        capture: Arc<CaptureShared>,
        ready: Arc<AtomicBool>,
        tx: TriggerEventSender,
    ) -> Self {
        Self {
            triggers,
            capture,
            ready,
            tx,
            physical_down: HashSet::new(),
            active: HashMap::new(),
            captured_down: HashSet::new(),
        }
    }

    /// Whether this control may complete the pending capture.
    ///
    /// Vendor-defined pages cannot. Three heuristics were tried against real
    /// hardware and all three lost, each to the next thing the owner's machine
    /// did. Excluding by control id: HID++ moves a different report bit every
    /// message, so identity never repeated. Excluding devices that spoke as the
    /// capture opened: he has two Logitech receivers and the quiet one walked in
    /// afterwards. Requiring silence before the edge: 046d:c548 speaks *rarely*,
    /// so it is silent by that measure every single time — it took the binding
    /// again with the same control id, 285212721.
    ///
    /// The pattern is that a report stream cannot tell us whether a person
    /// caused it. A Unifying receiver is a transport, not a control surface; its
    /// keyboard and mouse already arrive as ordinary standard-page devices,
    /// which is what anyone actually wants to bind. Vendor pages were registered
    /// for macro pads and pedals that declare no buttons, a case nobody has ever
    /// tested with real hardware, and it has cost three failed fixes and a
    /// shipped release in which pressing Add bound a dongle's housekeeping.
    ///
    /// So capture no longer offers them. Saved vendor bindings still execute —
    /// anyone who deliberately bound a pedal keeps it — but nothing on a vendor
    /// page can win the prompt. Restoring this needs a device in hand and a
    /// deliberate path, not another guess about timing.
    fn capture_admits(&self, control: NativeControl) -> bool {
        !matches!(control, NativeControl::Hid { usage_page, .. } if usage_page >= 0xff00)
    }

    pub(crate) fn edge(&mut self, device: &NativeDevice, control: NativeControl, pressed: bool) {
        let id = control.id(device);
        if pressed {
            // Record every first physical down before readiness, capture and
            // binding checks. Otherwise an auto-repeat can become a fresh press
            // after readiness flips or a binding is edited while the control is
            // still held.
            if !self.physical_down.insert(id) {
                return;
            }
        } else {
            self.physical_down.remove(&id);
            if self.captured_down.remove(&id) {
                return;
            }
        }
        if pressed && self.capture.is_capturing() {
            let admitted = self.capture_admits(control);
            if admitted {
                hook_diagnostic(HookDiagnostic::NativeCaptureOffered {
                    control,
                    vendor_id: device.vendor_id.unwrap_or(0),
                    product_id: device.product_id.unwrap_or(0),
                });
                if self
                    .capture
                    .finish(serialized_capture_code(&control.trigger(device)))
                {
                    self.captured_down.insert(id);
                    return;
                }
            } else {
                // Debug level deliberately: one receiver produced 2,894 log
                // lines in the minutes it took to find this, and a log that
                // buries its own evidence is no better than no log.
                hook_diagnostic(HookDiagnostic::NativeVendorRejected {
                    control,
                    vendor_id: device.vendor_id.unwrap_or(0),
                    product_id: device.product_id.unwrap_or(0),
                });
                return;
            }
        }
        if pressed {
            let action = {
                let Ok(g) = self.triggers.lock() else {
                    return;
                };
                native_matching_action(device, control, &g.0, &g.1, &g.2)
            };
            if !self.ready.load(Ordering::Acquire) {
                if action == Some(Action::Talk) {
                    note_talk_while_not_ready();
                }
                return;
            }
            let Some(action) = action else {
                return;
            };
            let event = match action {
                Action::Talk => TriggerEvent::TalkPressed(id),
                Action::Send => TriggerEvent::SendTapped(id),
                Action::Teach => TriggerEvent::TeachTapped(id),
            };
            match self.tx.try_send(event) {
                Ok(()) => {
                    self.active.insert(id, action);
                }
                // Saturation deliberately drops the action. The listener is
                // still healthy, and this press must not acquire active state.
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => {
                    self.ready.store(false, Ordering::Release);
                }
            }
        } else if let Some(action) = self.active.remove(&id) {
            if action == Action::Talk && self.tx.try_send(TriggerEvent::TalkReleased(id)).is_err() {
                self.ready.store(false, Ordering::Release);
            }
        }
    }

    /// Synthesize releases for every held control owned by a removed device.
    pub(crate) fn disconnect(&mut self, device_fingerprint: u64) {
        self.physical_down
            .retain(|id| id.device != device_fingerprint);
        self.captured_down
            .retain(|id| id.device != device_fingerprint);
        let released: Vec<_> = self
            .active
            .iter()
            .filter(|(id, _)| id.device == device_fingerprint)
            .map(|(id, action)| (*id, *action))
            .collect();
        for (id, action) in released {
            self.active.remove(&id);
            if action == Action::Talk && self.tx.try_send(TriggerEvent::TalkReleased(id)).is_err() {
                self.ready.store(false, Ordering::Release);
            }
        }
        if self
            .tx
            .try_send(TriggerEvent::DeviceDisconnected(device_fingerprint))
            .is_err()
        {
            self.ready.store(false, Ordering::Release);
        }
    }
}

impl HotkeyListener for RdevHotkey {
    fn run(self: Box<Self>, tx: TriggerEventSender, shutdown: Arc<AtomicBool>) -> Result<()> {
        let triggers = self.triggers;
        let capture = self.capture;
        let ready = self.ready;
        let installed = self.installed;

        run_backend(triggers, capture, ready, installed, tx, shutdown)
    }
}

/// The Windows delivery layer is our own pair of low-level hooks
/// (hook_windows) rather than rdev's grab: rdev resolved the pressed key's
/// text name inside WH_KEYBOARD_LL — AttachThreadInput to the foreground
/// thread plus ToUnicodeEx — and a stalled foreground thread froze the whole
/// machine's keyboard behind it. All decisions still happen in
/// [`dispatch_grabbed`], shared with the rdev path.
#[cfg(windows)]
fn run_backend(
    triggers: SharedTriggers,
    capture: Arc<CaptureShared>,
    ready: Arc<AtomicBool>,
    installed: Arc<AtomicBool>,
    tx: TriggerEventSender,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    start_hook_diagnostics().map_err(VocalCodeError::Hotkey)?;
    // These native HID/XInput workers share this listener attempt's lifetime.
    // Join them before a supervised reinstall so retries cannot multiply the
    // independent device polling threads, and app shutdown leaves none behind.
    let native_stop = Arc::new(AtomicBool::new(false));
    let native_workers = crate::input_windows::spawn(
        triggers.clone(),
        capture.clone(),
        ready.clone(),
        tx.clone(),
        native_stop.clone(),
    );
    let hook_state = Mutex::new(HookState::default());
    let hook_ready = ready.clone();
    let hook_result = crate::hook_windows::run(
        move |event_type, injected| {
            dispatch_grabbed(
                event_type,
                &hook_state,
                &capture,
                &triggers,
                &hook_ready,
                &tx,
                injected,
            )
        },
        installed,
        ready,
        shutdown,
    );
    native_stop.store(true, Ordering::Release);
    for worker in native_workers {
        if worker.join().is_err() {
            log::error!("native input worker panicked during shutdown");
        }
    }
    hook_result.map_err(VocalCodeError::Hotkey)
}

#[cfg(not(windows))]
fn run_backend(
    triggers: SharedTriggers,
    capture: Arc<CaptureShared>,
    ready: Arc<AtomicBool>,
    installed: Arc<AtomicBool>,
    tx: TriggerEventSender,
    _shutdown: Arc<AtomicBool>,
) -> Result<()> {
    start_hook_diagnostics().map_err(VocalCodeError::Hotkey)?;
    let hook_state = Mutex::new(HookState::default());
    // rdev exposes no post-install callback on this fallback backend. The call
    // below is nevertheless the installation boundary; clear the handshake on
    // every return so an exited listener can never remain advertised as live.
    installed.store(true, Ordering::Release);
    let result = rdev::grab(move |event| {
        if dispatch_grabbed(
            &event.event_type,
            &hook_state,
            &capture,
            &triggers,
            &ready,
            &tx,
            // rdev's grab does not expose the injected flag; treat everything
            // as physical, which is exactly what this path always did.
            false,
        ) {
            None
        } else {
            Some(event)
        }
    });
    installed.store(false, Ordering::Release);
    result.map_err(|e| VocalCodeError::Hotkey(format!("{e:?}")))
}

/// Everything the low-level hook decides, for every key and mouse-button edge,
/// on any backend. Returns `true` to consume the event (the OS never delivers
/// it to the foreground app) and `false` to pass it through.
///
/// The mutex around [`HookState`] is uncontended: each backend invokes this
/// serially from one OS input thread.
fn dispatch_grabbed(
    event_type: &EventType,
    hook_state: &Mutex<HookState>,
    capture: &CaptureShared,
    triggers: &SharedTriggers,
    ready: &std::sync::atomic::AtomicBool,
    tx: &TriggerEventSender,
    // The event was synthesised by software (`LLKHF_INJECTED`/`LLMHF_INJECTED`)
    // rather than produced by hardware. Runtime trigger matching ignores this —
    // remapping software that re-emits a bound control must keep working — but
    // a *capture* is a question asked of the person's hands, and synthetic
    // events must not answer for them.
    injected: bool,
) -> bool {
    {
        {
            // Ignore mouse motion and wheel events before touching the shared
            // trigger vectors. A 1000 Hz mouse used to lock and clone three Vecs
            // for every movement inside a low-level OS hook. (The Windows
            // backend filters them before this call; the rdev path does not.)
            let Some((pressed, input, id)) = physical_event(event_type) else {
                return false;
            };

            let Ok(mut hook) = hook_state.lock() else {
                return false;
            };

            // Every key and button the hook receives, capture or not. Set
            // VOCALCODE_TRACE_INPUT=1 to turn it on: it is the only way to tell
            // "the hook never got this key" apart from "something dropped it
            // later", and those two have already cost several wrong diagnoses.
            // Mouse motion and wheel never reach here, so the volume is bounded
            // by how fast a person can type.
            if TRACE_INPUT.get_or_init(|| std::env::var_os("VOCALCODE_TRACE_INPUT").is_some())
                == &true
                || capture.is_capturing()
            {
                hook_diagnostic(HookDiagnostic::Input {
                    input,
                    pressed,
                    capturing: capture.is_capturing(),
                    held: hook.physical_down.contains(&id),
                });
            }

            if pressed {
                // Track *all* physical downs, not just successfully activated
                // bindings. The first event's disposition remains stable for
                // its entire hold: consumed actions/captures keep swallowing
                // repeats, while an unbound or unready press keeps passing them.
                // Injected presses stay out of this set: they share the id of
                // the physical key, and a synthetic chord's bookkeeping used to
                // erase the record of a key the person was really holding.
                if !injected && !hook.physical_down.insert(id) {
                    // A press treated as auto-repeat never reaches the capture
                    // branch below. If that is happening while the settings UI
                    // is waiting for a key, the state is stale and the binding
                    // is unreachable — say so rather than looking inert.
                    if capture.is_capturing() {
                        hook_diagnostic(HookDiagnostic::AutoRepeat {
                            input,
                            active: hook.active.contains_key(&id),
                            captured: hook.captured_down.contains(&id),
                        });
                    }
                    return hook.active.contains_key(&id) || hook.captured_down.contains(&id);
                }
                if is_correction_submit_press(pressed, input, injected, has_held_modifier(&hook)) {
                    CORRECTION_SUBMIT_GENERATION.fetch_add(1, Ordering::AcqRel);
                }
            } else {
                if !injected {
                    hook.physical_down.remove(&id);
                }
                // A captured press is intentionally swallowed, and its matching
                // release must be swallowed too. Passing only the up edge gives
                // the foreground application a half-event.
                if hook.captured_down.remove(&id) {
                    return true;
                }
                // A candidate modifier released with nothing else pressed in
                // between is a deliberate, solitary press — which is what
                // binding a modifier looks like, and what a chord never does.
                // A synthetic release (a paste chord's Ctrl-up) is not the
                // person's hand and must never complete the binding for them.
                // And the release may only answer the prompt that saw the
                // press: a candidacy born under an older capture must not
                // bind to a row armed afterwards.
                if !injected
                    && hook
                        .pending_modifier
                        .as_ref()
                        .is_some_and(|(pending, _, _)| *pending == id)
                {
                    let (_, code, born) = hook.pending_modifier.take().expect("checked above");
                    if capture.is_capturing() && born == capture.generation() {
                        capture.keep_alive();
                        capture.finish(code);
                    }
                    // The press already reached the OS, so the release must
                    // too — holding back only the up edge would wedge the
                    // modifier down in the foreground application.
                    return false;
                }
            }

            // Capture only presses. A press observed by the prompt owns its
            // matching release through `captured_down`, so a binding cannot
            // execute underneath the binding UI.
            if pressed && capture.is_capturing() && !is_text_injection(input) {
                // A control that is already bound never spends the prompt, but
                // it must not execute either. In particular, pressing the last
                // X2 talk binding while following "bind another key first"
                // started a recording behind Settings. Logitech then emitted a
                // companion Win-key event, which won the still-open prompt; the
                // page displayed a Win-key warning and the recording later
                // failed to inject into a target that never existed.
                //
                // Report the duplicate, keep waiting, and swallow both edges.
                // This check remains before the chord test so a duplicate does
                // not silently cancel a modifier candidate.
                let bound = {
                    let Ok(g) = triggers.lock() else {
                        return false;
                    };
                    matching_action(input, &g.0, &g.1, &g.2)
                };
                if let Some(action) = bound {
                    // A synthetic re-emission is not a person at the prompt, so
                    // it does not hint or extend the deadline. It is still
                    // swallowed: capture mode must never fire a live action.
                    if !injected {
                        capture.keep_alive();
                        let (code, _) = capture_code(input);
                        capture.hint(&format!("bound:{}:{code}", action_name(action)));
                    }
                    hook.captured_down.insert(id);
                    return true;
                } else if injected {
                    // A synthetic keystroke is a tool typing — our own Enter
                    // after tap-to-send, a paste chord's Ctrl, another
                    // dictation program using real VKs. It must not hint, must
                    // not extend the deadline, must not touch a candidacy, and
                    // a synthetic modifier must never become one. A bindable
                    // non-modifier may still answer: remapping software emits
                    // exactly that on purpose, and it is how the live
                    // integration tests press keys.
                    // Logitech/G HUB commonly follows a mouse thumb-button
                    // edge with an injected keyboard event. If the first edge
                    // was an existing binding, accepting that companion is how
                    // X2 became Left Win in the UI. Keep the complete synthetic
                    // pair out of both the prompt and the foreground app.
                    if !hook.captured_down.is_empty() {
                        hook_diagnostic(HookDiagnostic::CaptureInjectedCompanionSuppressed(input));
                        hook.captured_down.insert(id);
                        return true;
                    }
                    let (code, supported) = capture_code(input);
                    if supported && !code.is_empty() && !is_modifier_input(input) {
                        hook_diagnostic(HookDiagnostic::CaptureInjected(input));
                        if capture.finish(code) {
                            // The prompt is spent; a candidacy held over from
                            // it must not survive to answer a later one.
                            hook.pending_modifier = None;
                            hook.captured_down.insert(id);
                            return true;
                        }
                    }
                    return false;
                } else {
                    // A second control going down while a candidate modifier
                    // is held is a chord in flight — Alt+Tab, Ctrl+C — not a
                    // bind. Drop the candidacy and let the chord do its job,
                    // and say so: the row just promised "release to bind", and
                    // a silent drop turns that promise into a lie the user
                    // discovers by releasing into nothing.
                    if hook.pending_modifier.take().is_some() {
                        capture.hint("chord");
                        return false;
                    }
                    let (code, supported) = capture_code(input);
                    hook_diagnostic(HookDiagnostic::CaptureOffered { input, supported });
                    if !supported {
                        // An answer the prompt cannot bind must never *end* the
                        // prompt. It used to: `finish` took the pending capture
                        // whatever the answer was, so the click that opened the
                        // row, or one stray letter, spent the capture — and
                        // every key pressed afterwards arrived with nothing
                        // waiting for it and produced no page event at all.
                        // That is the whole of "I press a key and nothing
                        // happens".
                        //
                        // Say what was seen, keep waiting, and pass the event
                        // on: a letter typed at an open prompt must still type.
                        capture.hint(&code);
                        return false;
                    }
                    // Any input at all is proof the user is still here, so the
                    // deadline moves even when the answer is the one we take.
                    capture.keep_alive();
                    if is_modifier_input(input) {
                        // A modifier becomes a candidate, not a binding: it
                        // binds on a clean release and is passed through in the
                        // meantime, so that if this press turns out to be the
                        // start of Alt+Tab, Alt+Tab still happens. Say so —
                        // a held modifier that visibly keeps working reads as
                        // "the listener is broken" unless the row explains
                        // what release will do.
                        hook.pending_modifier = Some((id, code.clone(), capture.generation()));
                        capture.hint(&format!("pending:{code}"));
                        return false;
                    }
                    if capture.finish(code) {
                        hook.pending_modifier = None;
                        hook.captured_down.insert(id);
                        return true;
                    }
                }
            }

            if pressed {
                // A press of a control that is already active is a repeat —
                // an injected re-emission, or bookkeeping that slipped. It
                // must neither double-fire the engine nor leak through.
                if hook.active.contains_key(&id) {
                    return true;
                }
                let action = {
                    let Ok(g) = triggers.lock() else {
                        return false;
                    };
                    matching_action(input, &g.0, &g.1, &g.2)
                };
                if !ready.load(Ordering::Acquire) {
                    // Passed through exactly as before. Only the count is new,
                    // and only a person's press counts: the app's own paste
                    // chord sends an injected Ctrl while a dictation is being
                    // delivered (unready), and with talk bound to Ctrl every
                    // pasted dictation would end in "still working".
                    if action == Some(Action::Talk) && !injected {
                        note_talk_while_not_ready();
                    }
                    return false;
                }
                let Some(action) = action else {
                    return false;
                };
                let trigger_event = match action {
                    Action::Talk => TriggerEvent::TalkPressed(id),
                    Action::Send => TriggerEvent::SendTapped(id),
                    Action::Teach => TriggerEvent::TeachTapped(id),
                };
                match tx.try_send(trigger_event) {
                    Ok(()) => {
                        hook.active.insert(id, (action, injected));
                        true
                    }
                    // The queue is deliberately lossy only for actions. Since
                    // this press did not reach the engine, do not swallow it or
                    // establish an active edge whose release would be misleading.
                    Err(TrySendError::Full(_)) => false,
                    Err(TrySendError::Disconnected(_)) => {
                        // The engine receiver is gone. Never keep stealing a
                        // global key for an action that can no longer run.
                        ready.store(false, Ordering::Release);
                        false
                    }
                }
            } else {
                let Some(&(action, was_injected)) = hook.active.get(&id) else {
                    return false;
                };
                // An injected release may only end a hold an injected press
                // began. The app's own paste chord sends a Ctrl-up while the
                // person is physically holding Ctrl to talk — cutting their
                // recording on a synthetic edge is the worse failure.
                if injected && !was_injected {
                    return false;
                }
                hook.active.remove(&id);
                if action != Action::Talk || tx.try_send(TriggerEvent::TalkReleased(id)).is_ok() {
                    // Send/teach have no release event, but both halves are
                    // swallowed because their press was swallowed.
                    true
                } else {
                    ready.store(false, Ordering::Release);
                    false
                }
            }
        }
    }
}

fn physical_event(event: &EventType) -> Option<(bool, RdevInput, TriggerId)> {
    let (pressed, input) = match *event {
        EventType::KeyPress(key) => (true, input_from_key(key)),
        EventType::KeyRelease(key) => (false, input_from_key(key)),
        EventType::ButtonPress(button) => (true, RdevInput::Mouse(button)),
        EventType::ButtonRelease(button) => (false, RdevInput::Mouse(button)),
        _ => return None,
    };
    let id = input_id(input);
    Some((pressed, input, id))
}

fn input_from_key(key: RdevKey) -> RdevInput {
    consumer_usage_from_key(key).map_or(RdevInput::Key(key), RdevInput::Consumer)
}

/// The keys the operating system composes chords with. These bind on a clean
/// release rather than on press — see [`HookState::pending_modifier`].
/// CapsLock and the F-keys are deliberately not here: nothing chords with
/// them, so they bind the instant they are pressed.
fn is_modifier_input(input: RdevInput) -> bool {
    matches!(
        input,
        RdevInput::Key(
            RdevKey::ShiftLeft
                | RdevKey::ShiftRight
                | RdevKey::ControlLeft
                | RdevKey::ControlRight
                | RdevKey::Alt
                | RdevKey::AltGr
                | RdevKey::MetaLeft
        )
    ) || matches!(input, RdevInput::Key(k) if k == META_RIGHT_KEY)
}

fn has_held_modifier(hook: &HookState) -> bool {
    [
        RdevKey::ShiftLeft,
        RdevKey::ShiftRight,
        RdevKey::ControlLeft,
        RdevKey::ControlRight,
        RdevKey::Alt,
        RdevKey::AltGr,
        RdevKey::MetaLeft,
        META_RIGHT_KEY,
    ]
    .into_iter()
    .any(|key| hook.physical_down.contains(&input_id(RdevInput::Key(key))))
}

fn is_correction_submit_press(
    pressed: bool,
    input: RdevInput,
    injected: bool,
    held_modifier: bool,
) -> bool {
    pressed && !injected && !held_modifier && matches!(input, RdevInput::Key(RdevKey::Return))
}

/// The key code text injection arrives as (`VK_PACKET`). It is a tool typing,
/// not a hand on a keyboard: the capture must neither take it, hint about it,
/// nor let it cancel a candidate modifier. The owner dictates *while* the
/// prompt is open — his own voice tool used to kill every modifier candidacy
/// within a second of speech, which is why Ctrl "never bound" for him while
/// CapsLock bound instantly.
fn is_text_injection(input: RdevInput) -> bool {
    matches!(input, RdevInput::Key(RdevKey::Unknown(231)))
}

/// Wire name for the row an action belongs to, for the page's
/// "already bound to …" note.
fn action_name(action: Action) -> &'static str {
    match action {
        Action::Talk => "talk",
        Action::Send => "send",
        Action::Teach => "teach",
    }
}

fn input_id(input: RdevInput) -> TriggerId {
    match input {
        RdevInput::Key(key) => TriggerId::new(TriggerSource::Keyboard, 0, key_control(key)),
        RdevInput::Mouse(button) => TriggerId::new(TriggerSource::Mouse, 0, mouse_control(button)),
        RdevInput::Consumer(usage) => {
            TriggerId::new(TriggerSource::ConsumerControl, 0, u32::from(usage))
        }
    }
}

fn key_control(key: RdevKey) -> u32 {
    if let Some(index) = BINDABLE_KEYS
        .iter()
        .position(|(candidate, _)| *candidate == key)
    {
        return index as u32 + 1;
    }
    match key {
        RdevKey::Escape => 0x0000_ff01,
        RdevKey::Unknown(code) => 0x8000_0000 | code,
        _ => fnv1a32(format!("{key:?}").as_bytes()),
    }
}

fn mouse_control(button: Button) -> u32 {
    match button {
        Button::Left => 0x100,
        Button::Right => 0x101,
        Button::Middle => 0x102,
        Button::Unknown(code) => u32::from(code),
    }
}

fn fnv1a32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5u32, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}

pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn gamepad_control(button: GamepadButton) -> u32 {
    match button {
        GamepadButton::South => 1,
        GamepadButton::East => 2,
        GamepadButton::West => 3,
        GamepadButton::North => 4,
        GamepadButton::LeftShoulder => 5,
        GamepadButton::RightShoulder => 6,
        GamepadButton::LeftThumb => 7,
        GamepadButton::RightThumb => 8,
        GamepadButton::Start => 9,
        GamepadButton::Back => 10,
        GamepadButton::Guide => 11,
        GamepadButton::DpadUp => 12,
        GamepadButton::DpadDown => 13,
        GamepadButton::DpadLeft => 14,
        GamepadButton::DpadRight => 15,
    }
}

/// The same verdict as [`capture_code`], reached from a `KeyboardEvent.code`
/// instead of a hook event. See [`CaptureShared::answer_from_page`] for why
/// this path exists at all.
///
/// Deliberately not a second opinion about which keys are legal: it consults
/// the same `BINDABLE_KEYS` and the same [`is_typing_key`]. That is possible
/// because the table already stores exactly what the web platform calls a key
/// (`ControlRight`, `F8`, `ArrowLeft`, `Numpad0`, `Backquote`), so this is a
/// lookup and not a translation — the two paths cannot drift into disagreeing
/// about a key without the tests below noticing.
fn capture_code_from_web(code: &str) -> (String, bool) {
    if code == "Escape" {
        return (String::new(), true);
    }
    match BINDABLE_KEYS.iter().find(|(_, name)| *name == code) {
        // Space/Tab/Backquote live in the table for compatibility with configs
        // that already name them, and `capture_code` refuses them; refuse them
        // here for the same reason rather than letting this path mint one.
        Some((key, _)) if is_typing_key(*key) => ("typing_key".to_string(), false),
        Some((_, name)) => (format!("key:{name}"), true),
        // Letters, digits and punctuation never reach the table. Naming them
        // as typing keys is what lets the page say why instead of going quiet.
        None if code.starts_with("Key")
            || code.starts_with("Digit")
            || matches!(
                code,
                "Enter"
                    | "NumpadEnter"
                    | "Backspace"
                    | "Comma"
                    | "Period"
                    | "Slash"
                    | "Semicolon"
                    | "Quote"
                    | "BracketLeft"
                    | "BracketRight"
                    | "Backslash"
                    | "Minus"
                    | "Equal"
            ) =>
        {
            ("typing_key".to_string(), false)
        }
        None => ("unsupported".to_string(), false),
    }
}

fn capture_code(input: RdevInput) -> (String, bool) {
    match input {
        RdevInput::Key(RdevKey::Escape) => (String::new(), true),
        RdevInput::Key(key) if is_typing_key(key) => ("typing_key".to_string(), false),
        RdevInput::Key(key) => key_to_name(key)
            .map(|name| (format!("key:{name}"), true))
            .unwrap_or_else(|| ("unsupported".to_string(), false)),
        RdevInput::Mouse(Button::Unknown(1)) => ("mouse_x1".to_string(), true),
        RdevInput::Mouse(Button::Unknown(2)) => ("mouse_x2".to_string(), true),
        // The ordinary buttons are how the prompt was opened in the first
        // place, so they get their own note rather than the generic one. They
        // are unbindable either way, and unbindable no longer ends a capture.
        RdevInput::Mouse(Button::Left | Button::Right | Button::Middle) => {
            ("mouse_click".to_string(), false)
        }
        RdevInput::Mouse(_) => ("unsupported".to_string(), false),
        RdevInput::Consumer(usage) => {
            let trigger = Trigger::ConsumerControl {
                device: DeviceSelector::any(),
                usage,
            };
            (serialized_capture_code(&trigger), true)
        }
    }
}

/// Stable capture wire format for structured triggers. Legacy key/mouse codes
/// stay short; new variants use serde JSON so no device-selector field is lost
/// while crossing the WebView IPC boundary.
pub fn serialized_capture_code(trigger: &Trigger) -> String {
    format!(
        "trigger:{}",
        serde_json::to_string(trigger).expect("Trigger serialization cannot fail")
    )
}

fn matching_action(
    input: RdevInput,
    talk: &[Trigger],
    send: &[Trigger],
    teach: &[Trigger],
) -> Option<Action> {
    let any = |list: &[Trigger]| list.iter().any(|trigger| input_matches(trigger, input));
    if any(talk) {
        Some(Action::Talk)
    } else if any(send) {
        Some(Action::Send)
    } else if any(teach) {
        Some(Action::Teach)
    } else {
        None
    }
}

fn native_matching_action(
    device: &NativeDevice,
    control: NativeControl,
    talk: &[Trigger],
    send: &[Trigger],
    teach: &[Trigger],
) -> Option<Action> {
    let any = |list: &[Trigger]| {
        list.iter()
            .any(|trigger| native_matches(trigger, device, control))
    };
    if any(talk) {
        Some(Action::Talk)
    } else if any(send) {
        Some(Action::Send)
    } else if any(teach) {
        Some(Action::Teach)
    } else {
        None
    }
}

fn native_matches(trigger: &Trigger, device: &NativeDevice, control: NativeControl) -> bool {
    match (trigger, control) {
        (
            Trigger::GamepadButton {
                device: selector,
                button: wanted,
            },
            NativeControl::Gamepad(actual),
        ) => *wanted == actual && device.matches(selector),
        (
            Trigger::ConsumerControl {
                device: selector,
                usage: wanted,
            },
            NativeControl::Consumer(actual),
        ) => *wanted == actual && device.matches(selector),
        (
            Trigger::HidButton {
                device: selector,
                usage_page: wanted_page,
                usage: wanted_usage,
                control: wanted_control,
            },
            NativeControl::Hid {
                usage_page,
                usage,
                control,
            },
        ) => {
            *wanted_page == usage_page
                && *wanted_usage == usage
                && (*wanted_control == 0 || *wanted_control == control)
                && device.matches(selector)
        }
        _ => false,
    }
}

fn input_matches(trigger: &Trigger, input: RdevInput) -> bool {
    match input {
        RdevInput::Key(key) => key_matches(trigger, key),
        RdevInput::Mouse(button) => mouse_matches(trigger, button),
        RdevInput::Consumer(usage) => consumer_matches(trigger, usage),
    }
}

fn mouse_matches(trigger: &Trigger, button: Button) -> bool {
    let Trigger::MouseButton(extra) = trigger else {
        return false;
    };
    match button {
        Button::Unknown(n) => match extra {
            MouseExtra::X1 => n == 1,
            MouseExtra::X2 => n == 2,
        },
        _ => false,
    }
}

fn key_matches(trigger: &Trigger, key: RdevKey) -> bool {
    let Trigger::Key(name) = trigger else {
        return false;
    };
    key_from_name(name).is_some_and(|k| k == key)
}

fn consumer_matches(trigger: &Trigger, usage: u16) -> bool {
    matches!(
        trigger,
        Trigger::ConsumerControl { device, usage: wanted }
            if *wanted == usage && device.matches(None, None, None, None)
    )
}

/// Windows multimedia keys reach rdev as raw virtual-key values. Keep their
/// semantic HID Consumer usages so a binding captured from a keyboard and one
/// captured from Raw Input compare as the same control.
#[cfg(windows)]
fn consumer_usage_from_key(key: RdevKey) -> Option<u16> {
    let RdevKey::Unknown(vk) = key else {
        return None;
    };
    Some(match vk {
        0xAD => 0x00E2, // Mute
        0xAE => 0x00EA, // Volume decrement
        0xAF => 0x00E9, // Volume increment
        0xB0 => 0x00B5, // Scan next track
        0xB1 => 0x00B6, // Scan previous track
        0xB2 => 0x00B7, // Stop
        0xB3 => 0x00CD, // Play/pause
        0xA6 => 0x0224, // Browser back
        0xA7 => 0x0225, // Browser forward
        0xA8 => 0x0227, // Browser refresh
        0xA9 => 0x0226, // Browser stop
        0xAA => 0x0221, // Browser search
        0xAB => 0x022A, // Browser favourites
        0xAC => 0x0223, // Browser home
        _ => return None,
    })
}

#[cfg(not(windows))]
fn consumer_usage_from_key(_key: RdevKey) -> Option<u16> {
    None
}

/// Every key that may execute a saved trigger: `(rdev key, name)`.
///
/// One table drives both directions, so the two cannot drift — a name that
/// resolves in only one direction saves a binding that never fires.
///
/// New capture excludes typing keys. Space/Tab/Backquote remain in this runtime
/// table only for configs written by earlier releases; silently making an
/// existing talk binding inert is worse than continuing an explicit choice.
#[cfg(windows)]
const META_RIGHT_KEY: RdevKey = RdevKey::Unknown(92); // VK_RWIN; missing in rdev 0.5.3
#[cfg(not(windows))]
const META_RIGHT_KEY: RdevKey = RdevKey::MetaRight;

static BINDABLE_KEYS: &[(RdevKey, &str)] = &[
    // Modifiers — the natural hold-to-talk keys.
    (RdevKey::CapsLock, "CapsLock"),
    (RdevKey::ShiftLeft, "ShiftLeft"),
    (RdevKey::ShiftRight, "ShiftRight"),
    (RdevKey::ControlLeft, "ControlLeft"),
    (RdevKey::ControlRight, "ControlRight"),
    (RdevKey::Alt, "AltLeft"),
    (RdevKey::AltGr, "AltRight"),
    (RdevKey::MetaLeft, "MetaLeft"),
    (META_RIGHT_KEY, "MetaRight"),
    // Function row.
    (RdevKey::F1, "F1"),
    (RdevKey::F2, "F2"),
    (RdevKey::F3, "F3"),
    (RdevKey::F4, "F4"),
    (RdevKey::F5, "F5"),
    (RdevKey::F6, "F6"),
    (RdevKey::F7, "F7"),
    (RdevKey::F8, "F8"),
    (RdevKey::F9, "F9"),
    (RdevKey::F10, "F10"),
    (RdevKey::F11, "F11"),
    (RdevKey::F12, "F12"),
    // F13-F15 have no rdev variant and arrive as raw scan codes.
    (RdevKey::Unknown(124), "F13"),
    (RdevKey::Unknown(125), "F14"),
    (RdevKey::Unknown(126), "F15"),
    (RdevKey::Unknown(127), "F16"),
    (RdevKey::Unknown(128), "F17"),
    (RdevKey::Unknown(129), "F18"),
    (RdevKey::Unknown(130), "F19"),
    (RdevKey::Unknown(131), "F20"),
    (RdevKey::Unknown(132), "F21"),
    (RdevKey::Unknown(133), "F22"),
    (RdevKey::Unknown(134), "F23"),
    (RdevKey::Unknown(135), "F24"),
    // Navigation and editing keys, none of which are part of typing.
    (RdevKey::LeftArrow, "ArrowLeft"),
    (RdevKey::RightArrow, "ArrowRight"),
    (RdevKey::UpArrow, "ArrowUp"),
    (RdevKey::DownArrow, "ArrowDown"),
    (RdevKey::Home, "Home"),
    (RdevKey::End, "End"),
    (RdevKey::PageUp, "PageUp"),
    (RdevKey::PageDown, "PageDown"),
    (RdevKey::Insert, "Insert"),
    (RdevKey::Delete, "Delete"),
    (RdevKey::PrintScreen, "PrintScreen"),
    (RdevKey::ScrollLock, "ScrollLock"),
    (RdevKey::Pause, "Pause"),
    (RdevKey::NumLock, "NumLock"),
    // Legacy-only: `capture_code` rejects these before consulting this table.
    (RdevKey::Space, "Space"),
    (RdevKey::Tab, "Tab"),
    (RdevKey::BackQuote, "Backquote"),
    // Numeric keypad — plentiful and rarely used while dictating.
    (RdevKey::Kp0, "Numpad0"),
    (RdevKey::Kp1, "Numpad1"),
    (RdevKey::Kp2, "Numpad2"),
    (RdevKey::Kp3, "Numpad3"),
    (RdevKey::Kp4, "Numpad4"),
    (RdevKey::Kp5, "Numpad5"),
    (RdevKey::Kp6, "Numpad6"),
    (RdevKey::Kp7, "Numpad7"),
    (RdevKey::Kp8, "Numpad8"),
    (RdevKey::Kp9, "Numpad9"),
    (RdevKey::KpMinus, "NumpadSubtract"),
    (RdevKey::KpPlus, "NumpadAdd"),
    (RdevKey::KpMultiply, "NumpadMultiply"),
    (RdevKey::KpDivide, "NumpadDivide"),
    (RdevKey::KpDelete, "NumpadDecimal"),
    // Deliberately no NumpadEnter: rdev's Windows backend maps both Enter keys
    // to Return and explicitly has no KpReturn, so advertising it created a
    // binding that could be saved but never fire.
];

/// Friendlier spellings accepted when reading a config, so hand-edited files
/// and configs written by another platform keep working.
static NAME_ALIASES: &[(&str, &str)] = &[
    ("capslock", "CapsLock"),
    ("caps", "CapsLock"),
    ("scrolllock", "ScrollLock"),
    ("scroll", "ScrollLock"),
    ("rightctrl", "ControlRight"),
    ("leftctrl", "ControlLeft"),
    ("rightalt", "AltRight"),
    ("altgr", "AltRight"),
    ("leftalt", "AltLeft"),
    ("rightshift", "ShiftRight"),
    ("leftshift", "ShiftLeft"),
    ("rightmeta", "MetaRight"),
    ("rightwin", "MetaRight"),
    ("leftmeta", "MetaLeft"),
    ("leftwin", "MetaLeft"),
    ("f13", "F13"),
];

/// Keys that would fire while typing. Recognised so the capture UI can say why
/// they cannot be bound rather than appearing to do nothing.
fn is_typing_key(k: RdevKey) -> bool {
    use RdevKey::*;
    matches!(
        k,
        KeyA | KeyB
            | KeyC
            | KeyD
            | KeyE
            | KeyF
            | KeyG
            | KeyH
            | KeyI
            | KeyJ
            | KeyK
            | KeyL
            | KeyM
            | KeyN
            | KeyO
            | KeyP
            | KeyQ
            | KeyR
            | KeyS
            | KeyT
            | KeyU
            | KeyV
            | KeyW
            | KeyX
            | KeyY
            | KeyZ
            | Num0
            | Num1
            | Num2
            | Num3
            | Num4
            | Num5
            | Num6
            | Num7
            | Num8
            | Num9
            | Minus
            | Equal
            | LeftBracket
            | RightBracket
            | SemiColon
            | Quote
            | BackSlash
            | IntlBackslash
            | Comma
            | Dot
            | Slash
            | Return
            | Backspace
            | Space
            | Tab
            | BackQuote
    )
}

fn key_from_name(name: &str) -> Option<RdevKey> {
    let shared = vocalcode_core::config::canonical_key_name(name).unwrap_or(name);
    let canonical = NAME_ALIASES
        .iter()
        .find(|(alias, _)| *alias == shared)
        .map(|(_, c)| *c)
        .unwrap_or(shared);
    BINDABLE_KEYS
        .iter()
        .find(|(_, n)| *n == canonical)
        .map(|(k, _)| *k)
}

fn key_to_name(k: RdevKey) -> Option<&'static str> {
    BINDABLE_KEYS
        .iter()
        .find(|(key, _)| *key == k)
        .map(|(_, name)| *name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use vocalcode_core::{
        trigger_event_channel, TriggerEventReceiver, TRIGGER_ACTION_QUEUE_CAPACITY,
    };

    fn source_between<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
        let tail = source
            .split_once(start)
            .unwrap_or_else(|| panic!("missing source marker {start:?}"))
            .1;
        tail.split_once(end)
            .unwrap_or_else(|| panic!("missing source marker {end:?}"))
            .0
    }

    #[test]
    fn hook_diagnostic_enqueue_is_bounded_and_never_uses_blocking_send() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let dropped = AtomicU64::new(0);
        enqueue_hook_diagnostic(&sender, &dropped, HookDiagnostic::CaptureHint);
        enqueue_hook_diagnostic(&sender, &dropped, HookDiagnostic::CaptureFinished);
        assert!(matches!(
            receiver.try_recv(),
            Ok(HookDiagnostic::CaptureHint)
        ));
        assert_eq!(dropped.load(Ordering::Relaxed), 1);

        let source = include_str!("hotkey.rs");
        let enqueue = source_between(
            source,
            "fn enqueue_hook_diagnostic(",
            "/// Start the bounded diagnostic drain",
        );
        assert!(enqueue.contains("sender.try_send(event)"));
        assert!(!enqueue.contains("sender.send("));
    }

    #[test]
    fn os_hook_callbacks_and_shared_dispatch_contain_no_log_io() {
        let hotkey = include_str!("hotkey.rs");
        let dispatch = source_between(hotkey, "fn dispatch_grabbed(", "\nfn physical_event(");
        assert!(!dispatch.contains("log::"));
        assert!(dispatch.contains("hook_diagnostic("));
        for (start, end) in [
            (
                "    fn expire(&mut self)",
                "    /// The user just produced input",
            ),
            (
                "    pub(crate) fn hint(&self",
                "    /// Take a finished capture result",
            ),
            (
                "    pub(crate) fn finish(&self",
                "\n}\n\n/// Live-updatable",
            ),
        ] {
            let callback_path = source_between(hotkey, start, end);
            assert!(
                !callback_path.contains("log::"),
                "{start} logs synchronously"
            );
        }

        let windows = include_str!("hook_windows.rs");
        let windows_callbacks = source_between(
            windows,
            "unsafe extern \"system\" fn keyboard_proc(",
            "/// Install both hooks",
        );
        assert!(!windows_callbacks.contains("log::"));
        assert!(windows_callbacks.contains("hook_diagnostic("));

        let mac = include_str!("hotkey_macos.rs");
        let mac_callback = source_between(
            mac,
            "extern \"C\" fn tap_callback(",
            "/// Returns true when the event should be swallowed",
        );
        assert!(!mac_callback.contains("log::"));
        assert!(mac_callback.contains("hook_diagnostic("));

        let mac_hid = include_str!("input_macos.rs");
        let mac_hid_callbacks = source_between(
            mac_hid,
            "unsafe extern \"C\" fn device_matched(",
            "unsafe fn load_device(",
        );
        assert!(!mac_hid_callbacks.contains("log::"));
        assert!(!mac_hid_callbacks.contains(".stable_id"));
        assert!(!mac_hid_callbacks.contains("record.product"));
        assert!(!mac_hid_callbacks.contains("native.serial"));
        assert!(mac_hid_callbacks.contains("hook_diagnostic("));

        let windows_raw = include_str!("input_windows.rs");
        let windows_raw_callbacks = source_between(
            windows_raw,
            "unsafe extern \"system\" fn raw_window_proc(",
            "fn decode_vendor_bits(",
        );
        assert!(!windows_raw_callbacks.contains("log::"));
        assert!(!windows_raw_callbacks.contains(".stable_id"));
        assert!(windows_raw_callbacks.contains("hook_diagnostic("));
    }

    #[test]
    fn diagnostic_drain_starts_before_native_or_os_hooks() {
        let hotkey = include_str!("hotkey.rs");
        for backend in hotkey.split("fn run_backend(").skip(1) {
            let body = backend
                .split_once("\n}")
                .expect("run_backend must have a body")
                .0;
            assert!(body.contains("start_hook_diagnostics()"));
        }

        let mac = include_str!("hotkey_macos.rs");
        let run = source_between(
            mac,
            "impl HotkeyListener for MacHotkey",
            "extern \"C\" fn tap_callback",
        );
        let diagnostics = run.find("start_hook_diagnostics()").unwrap();
        let hid = run.find("input_macos::install").unwrap();
        let tap = run.find("CGEventTapCreate").unwrap();
        assert!(diagnostics < hid && diagnostics < tap);
    }

    #[test]
    fn poisoned_capture_mutex_never_panics_on_callback_operations() {
        let capture = CaptureShared::default();
        capture.start("talk");
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = capture.inner.lock().unwrap();
            panic!("deliberately poison callback state");
        }));
        assert!(poisoned.is_err());

        assert!(capture.is_capturing());
        capture.keep_alive();
        let _ = capture.generation();
        assert!(capture.hint("typing-key:A"));
        assert!(capture.finish("key:F13".to_string()));
        assert!(capture.has_result());
        assert!(capture.take_result().is_some());
    }

    /// Mirrors the macOS suite: a name that resolves in only one direction
    /// saves a binding the settings UI accepts and the hook never matches.
    #[test]
    fn bindable_keys_round_trip() {
        for (key, name) in BINDABLE_KEYS {
            assert_eq!(key_to_name(*key), Some(*name), "{key:?} -> name");
            assert_eq!(key_from_name(name), Some(*key), "{name} -> key");
        }
    }

    #[test]
    fn bindable_keys_are_unique() {
        let mut names = HashSet::new();
        let mut keys = HashSet::new();
        for (key, name) in BINDABLE_KEYS {
            assert!(names.insert(*name), "duplicate name {name}");
            assert!(keys.insert(format!("{key:?}")), "duplicate key {key:?}");
        }
    }

    #[test]
    fn aliases_resolve_to_real_keys() {
        for (alias, canonical) in NAME_ALIASES {
            assert!(
                key_from_name(canonical).is_some(),
                "alias {alias} -> unknown {canonical}"
            );
            assert_eq!(key_from_name(alias), key_from_name(canonical));
        }
    }

    /// Only compatibility entries may both execute and type. Capture checks
    /// `is_typing_key` first and never creates a new one.
    #[test]
    fn bindable_keys_are_not_typing_keys() {
        for (key, name) in BINDABLE_KEYS {
            if is_typing_key(*key) {
                assert!(matches!(*name, "Space" | "Tab" | "Backquote"));
            }
        }
    }

    /// Escape cancels a capture, so it must never also be bindable.
    #[test]
    fn escape_is_not_bindable() {
        assert_eq!(key_to_name(RdevKey::Escape), None);
    }

    /// The page path and the hook path must never disagree about a key. They
    /// are two front doors to one binding, and a key that binds from one and is
    /// refused by the other is the kind of thing a user reports as "sometimes
    /// it works".
    #[test]
    fn page_capture_agrees_with_the_hook() {
        for (key, name) in BINDABLE_KEYS {
            let from_hook = capture_code(RdevInput::Key(*key));
            let from_page = capture_code_from_web(name);
            assert_eq!(
                from_hook, from_page,
                "{name} is decided differently by the hook and the page"
            );
        }
    }

    /// Escape cancels from the page exactly as it does from the hook: the empty
    /// string, which the page reads as "put the old binding back".
    #[test]
    fn page_escape_cancels() {
        assert_eq!(capture_code_from_web("Escape"), (String::new(), true));
    }

    /// Letters, digits and punctuation never appear in `BINDABLE_KEYS`, so the
    /// page path has to recognise them by shape. Answering `unsupported` for a
    /// letter would tell the user the wrong thing about why it was refused.
    #[test]
    fn page_capture_names_typing_keys_as_typing_keys() {
        for code in ["KeyA", "KeyZ", "Digit1", "Enter", "Backspace", "Comma"] {
            assert_eq!(
                capture_code_from_web(code),
                ("typing_key".to_string(), false),
                "{code} should be refused as a typing key"
            );
        }
    }

    /// Anything the table does not know and that is not typing-shaped is
    /// `unsupported`, not silently dropped — silence is what made key recording
    /// look broken in the first place.
    #[test]
    fn page_capture_refuses_unknown_codes() {
        for code in ["MediaPlayPause", "BrowserBack", ""] {
            assert_eq!(
                capture_code_from_web(code),
                ("unsupported".to_string(), false),
                "{code} should be refused as unsupported"
            );
        }
    }

    /// Letters and digits must be *recognised* as unusable rather than ignored
    /// — reporting nothing is what made key recording look broken.
    #[test]
    fn typing_keys_are_recognised() {
        for k in [
            RdevKey::KeyA,
            RdevKey::Num1,
            RdevKey::Return,
            RdevKey::Backspace,
            RdevKey::Comma,
        ] {
            assert!(is_typing_key(k), "{k:?} should be flagged as a typing key");
            assert_eq!(key_to_name(k), None);
        }
    }

    #[test]
    fn correction_submit_is_only_a_physical_unmodified_enter_press() {
        let enter = RdevInput::Key(RdevKey::Return);
        assert!(is_correction_submit_press(true, enter, false, false));
        assert!(!is_correction_submit_press(false, enter, false, false));
        assert!(!is_correction_submit_press(true, enter, true, false));
        assert!(!is_correction_submit_press(true, enter, false, true));
        assert!(!is_correction_submit_press(
            true,
            RdevInput::Key(RdevKey::Space),
            false,
            false
        ));
    }

    #[test]
    fn a_second_capture_cancels_the_first_instead_of_overwriting_it() {
        let capture = CaptureShared::default();
        capture.start("talk");
        capture.start("send");
        assert_eq!(capture.take_result(), Some(("talk".into(), String::new())));
        assert!(capture.finish("key:F13".into()));
        assert_eq!(
            capture.take_result(),
            Some(("send".into(), "key:F13".into()))
        );
    }

    /// A deadline that runs out has to be distinguishable from Escape. Both
    /// used to answer with the empty string, so the page restored the old
    /// binding and said nothing — the prompt simply vanished, which is
    /// indistinguishable from the app ignoring the key that was just pressed.
    #[test]
    fn capture_deadline_reports_a_timeout_not_a_cancellation() {
        let capture = CaptureShared::default();
        capture.start_for("teach", Duration::ZERO);
        assert_eq!(
            capture.take_result(),
            Some(("teach".into(), "timeout".into()))
        );
        assert!(!capture.is_capturing());

        // Escape keeps the empty spelling, because that one really is a choice.
        capture.start("teach");
        assert!(capture.finish(String::new()));
        assert_eq!(capture.take_result(), Some(("teach".into(), String::new())));
    }

    /// Clicking Add again on a row that is already waiting must not answer the
    /// pending capture at all. It used to answer it as cancelled, and since the
    /// page reads a cancellation as "put the old binding back", the prompt
    /// disappeared while the hook was still waiting for a key.
    #[test]
    fn re_arming_the_same_row_does_not_answer_the_pending_capture() {
        let capture = CaptureShared::default();
        capture.start("talk");
        capture.start("talk");
        capture.start("talk");
        assert_eq!(
            capture.take_result(),
            None,
            "a re-click cancelled the prompt"
        );
        assert!(capture.is_capturing());

        // And the key still lands on the row that is waiting for it.
        assert!(capture.finish("key:F13".into()));
        assert_eq!(
            capture.take_result(),
            Some(("talk".into(), "key:F13".into()))
        );
    }

    /// Re-arming must not re-open the one-shot hint either: the clicks that
    /// cause it are the same clicks that re-arm, so resetting the flag would
    /// repeat the message on every one of them.
    #[test]
    fn re_arming_does_not_repeat_the_hint() {
        let capture = CaptureShared::default();
        capture.start("talk");
        assert!(capture.hint("mouse_click"));
        capture.start("talk");
        assert!(!capture.hint("mouse_click"));

        // A genuinely new capture may explain itself once again.
        capture.start("send");
        assert!(capture.hint("mouse_click"));

        // But a *different* thing seen in the same capture still gets a word.
        // A single "already hinted" flag meant the first note silenced every
        // later one, so pressing a letter and then a mouse button explained
        // only the letter and left the second in the log alone.
        assert!(capture.hint("typing_key"));
        assert!(!capture.hint("typing_key"));
        assert!(capture.hint("mouse_click"));
    }

    /// The prompt must not run out from under somebody who is demonstrably
    /// still at the machine. v0.4.19 had no deadline at all and could not fail
    /// this way; the 15 s window that replaced it expired while a person looked
    /// away from the mouse and hunted for a key, and every key pressed after
    /// that arrived with nothing waiting for it and produced no page event.
    #[test]
    fn a_prompt_does_not_expire_under_a_user_who_is_still_pressing_things() {
        let started = Instant::now();
        let timeout = Duration::from_millis(200);
        let mut capture = Inner {
            which: Some("talk".into()),
            started: Some(started),
            deadline: Some(started + timeout),
            timeout,
            ..Inner::default()
        };
        // Drive the same transition as the input hook without depending on
        // the host scheduler waking this test within a 200 ms idle window.
        for step in 1..=5 {
            let now = started + Duration::from_millis(80 * step);
            capture.keep_alive_at(now);
            assert!(
                capture.which.is_some(),
                "the capture expired under a user who was still pressing things"
            );
            assert_eq!(capture.deadline, Some(now + timeout));
            assert!(capture.results.is_empty());
        }
    }

    /// Left alone, it still expires — otherwise a walked-away prompt would sit
    /// armed and eventually swallow a keystroke meant for somebody's editor.
    #[test]
    fn a_prompt_nobody_touches_still_expires() {
        let started = Instant::now();
        let timeout = Duration::from_millis(60);
        let mut capture = Inner {
            which: Some("talk".into()),
            started: Some(started),
            deadline: Some(started + timeout),
            timeout,
            ..Inner::default()
        };
        capture.expire_at(started + timeout - Duration::from_nanos(1));
        assert!(capture.which.is_some());
        capture.expire_at(started + timeout);
        assert!(capture.which.is_none());
        assert_eq!(
            capture.results.pop_front(),
            Some(("talk".into(), "timeout".into()))
        );
        // Input after expiry must not re-arm an abandoned prompt.
        capture.keep_alive_at(started + timeout + Duration::from_millis(1));
        assert!(capture.which.is_none());
        assert!(capture.deadline.is_none());
        assert!(capture.results.is_empty());
    }

    #[test]
    fn activity_cannot_extend_a_capture_beyond_its_absolute_ceiling() {
        let started = Instant::now();
        let mut capture = Inner {
            which: Some("talk".into()),
            started: Some(started),
            deadline: Some(started + CAPTURE_TIMEOUT),
            ..Inner::default()
        };
        let mut elapsed = Duration::from_secs(30);
        while elapsed < CAPTURE_MAX {
            let now = started + elapsed;
            capture.keep_alive_at(now);
            assert!(capture.which.is_some());
            assert_eq!(
                capture.deadline,
                Some((now + CAPTURE_TIMEOUT).min(started + CAPTURE_MAX))
            );
            elapsed += Duration::from_secs(30);
        }
        capture.keep_alive_at(started + CAPTURE_MAX);
        assert!(capture.which.is_none());
        assert!(capture.deadline.is_none());
        assert_eq!(
            capture.results.pop_front(),
            Some(("talk".into(), "timeout".into()))
        );
        capture.keep_alive_at(started + CAPTURE_MAX + Duration::from_secs(1));
        assert!(capture.results.is_empty());
    }

    /// An answer the prompt cannot bind must never *end* the prompt. This is
    /// the defect that survived every fix up to and including v0.5.0: `finish`
    /// took the pending capture whatever the answer was, so the click that
    /// opened the row — or one stray letter — spent it, and every key pressed
    /// afterwards was silently ignored.
    #[test]
    fn an_unbindable_answer_never_ends_the_capture() {
        for unbindable in [
            RdevInput::Key(RdevKey::KeyK),
            RdevInput::Mouse(Button::Left),
            RdevInput::Mouse(Button::Right),
            RdevInput::Mouse(Button::Middle),
        ] {
            let (code, supported) = capture_code(unbindable);
            assert!(
                !supported,
                "{unbindable:?} is expected to be unbindable, got {code}"
            );

            let capture = CaptureShared::default();
            capture.start("talk");
            capture.hint(&code);
            assert!(
                capture.is_capturing(),
                "{code} ended the capture instead of explaining itself"
            );
            // The note reaches the page, and the real key still lands after it.
            assert_eq!(capture.take_result(), Some(("talk".into(), code)));
            assert!(capture.finish("key:F13".into()));
            assert_eq!(
                capture.take_result(),
                Some(("talk".into(), "key:F13".into()))
            );
        }
    }

    /// The empty string means "the user pressed Escape", and the page answers
    /// it by silently restoring the old binding. Exactly one path may spell it.
    /// Both v0.5.0 regressions were this spelling escaping into a path nobody
    /// had chosen — a second click on Add, and a deadline running out.
    #[test]
    fn only_a_real_cancellation_answers_with_the_cancel_spelling() {
        let mut spellings = Vec::new();

        // A deadline running out.
        let capture = CaptureShared::default();
        capture.start_for("talk", Duration::ZERO);
        spellings.extend(capture.take_result());

        // Clicking Add again on the row that is already waiting.
        capture.start("talk");
        capture.start("talk");
        spellings.extend(capture.take_result());

        // An unbindable answer.
        capture.hint("typing_key");
        spellings.extend(capture.take_result());
        capture.hint("mouse_click");
        spellings.extend(capture.take_result());

        for (which, code) in &spellings {
            assert!(
                !code.is_empty(),
                "{which} was answered with the cancel spelling by a path that is not a cancellation"
            );
        }

        // Escape and an explicit cancel are the two that may.
        assert!(capture.finish(String::new()));
        assert_eq!(capture.take_result(), Some(("talk".into(), String::new())));
        capture.start("send");
        assert!(capture.cancel());
        assert_eq!(capture.take_result(), Some(("send".into(), String::new())));
    }

    #[allow(clippy::type_complexity)]
    fn grabbed_harness(
        talk: Vec<Trigger>,
    ) -> (
        Mutex<HookState>,
        CaptureShared,
        SharedTriggers,
        Arc<AtomicBool>,
        TriggerEventSender,
        TriggerEventReceiver,
    ) {
        let (tx, rx) = trigger_event_channel();
        (
            Mutex::new(HookState::default()),
            CaptureShared::default(),
            Arc::new(Mutex::new((talk, Vec::new(), Vec::new()))),
            Arc::new(AtomicBool::new(true)),
            tx,
            rx,
        )
    }

    /// A modifier binds on a clean release, not on press: a held modifier is
    /// how every chord begins, and binding it on press is how Alt+Tab at an
    /// open prompt bound Alt and cost the owner the shortcut.
    #[test]
    fn a_modifier_binds_on_a_clean_release_not_on_press() {
        let (hook, capture, triggers, ready, tx, _rx) = grabbed_harness(Vec::new());
        capture.start("talk");

        // The press is a candidate and passes through untouched — and the row
        // says what release will do, because a modifier that visibly keeps
        // working reads as a broken listener otherwise.
        assert!(!dispatch_grabbed(
            &EventType::KeyPress(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert!(capture.is_capturing());
        assert_eq!(
            capture.take_result(),
            Some(("talk".into(), "pending:key:ControlLeft".into()))
        );

        // The clean release completes the binding, and passes too — the OS
        // saw the press, so it must see the release.
        assert!(!dispatch_grabbed(
            &EventType::KeyRelease(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert_eq!(
            capture.take_result(),
            Some(("talk".into(), "key:ControlLeft".into()))
        );
    }

    /// Alt+Tab, Ctrl+C and every other chord pressed at an open prompt must
    /// work normally and bind nothing — and retract the "release to bind"
    /// promise the row just made, or releasing into nothing reads as a
    /// broken listener.
    #[test]
    fn a_chord_never_binds_its_modifier() {
        let (hook, capture, triggers, ready, tx, _rx) = grabbed_harness(Vec::new());
        capture.start("talk");

        assert!(!dispatch_grabbed(
            &EventType::KeyPress(RdevKey::Alt),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        let _pending = capture.take_result();
        // Tab while Alt is held: a chord member, passed through — and the
        // pending promise is withdrawn out loud.
        assert!(!dispatch_grabbed(
            &EventType::KeyPress(RdevKey::Tab),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert_eq!(
            capture.take_result(),
            Some(("talk".into(), "chord".into())),
            "a chord must retract the pending promise"
        );
        assert!(!dispatch_grabbed(
            &EventType::KeyRelease(RdevKey::Tab),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        // The candidacy died with the chord: releasing Alt binds nothing.
        assert!(!dispatch_grabbed(
            &EventType::KeyRelease(RdevKey::Alt),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert_eq!(capture.take_result(), None);
        assert!(capture.is_capturing(), "the prompt survives the chord");

        // And a real answer still lands afterwards.
        assert!(dispatch_grabbed(
            &EventType::KeyPress(RdevKey::F12),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert_eq!(
            capture.take_result(),
            Some(("talk".into(), "key:F12".into()))
        );
    }

    /// Dictated text arrives as a storm of `VK_PACKET` key events. It is a
    /// tool typing, not a hand on the keyboard: it must not hint, and above
    /// all it must not kill a candidate modifier — the owner dictates while
    /// the prompt is open, so his own speech used to cancel every Ctrl
    /// candidacy within a second.
    #[test]
    fn text_injection_never_disturbs_a_capture() {
        let (hook, capture, triggers, ready, tx, _rx) = grabbed_harness(Vec::new());
        capture.start("talk");

        assert!(!dispatch_grabbed(
            &EventType::KeyPress(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        let _pending = capture.take_result();

        // A burst of injected text while the modifier is held.
        for _ in 0..3 {
            assert!(!dispatch_grabbed(
                &EventType::KeyPress(RdevKey::Unknown(231)),
                &hook,
                &capture,
                &triggers,
                &ready,
                &tx,
                false
            ));
            assert!(!dispatch_grabbed(
                &EventType::KeyRelease(RdevKey::Unknown(231)),
                &hook,
                &capture,
                &triggers,
                &ready,
                &tx,
                false
            ));
        }
        assert_eq!(capture.take_result(), None, "injected text must not hint");

        // The candidacy survived the speech: the release still binds.
        assert!(!dispatch_grabbed(
            &EventType::KeyRelease(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert_eq!(
            capture.take_result(),
            Some(("talk".into(), "key:ControlLeft".into()))
        );
    }

    /// A binding prompt is modal for triggers: an already-bound control is
    /// explained, but must not also execute behind Settings.
    #[test]
    fn a_bound_control_is_suppressed_while_the_prompt_keeps_waiting() {
        let (hook, capture, triggers, ready, tx, rx) =
            grabbed_harness(vec![Trigger::MouseButton(MouseExtra::X2)]);
        capture.start("talk");

        // X2 is consumed by capture and the engine does not hear about it.
        assert!(dispatch_grabbed(
            &EventType::ButtonPress(Button::Unknown(2)),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert!(rx.try_recv().is_err(), "capture leaked a talk press");
        assert!(
            capture.is_capturing(),
            "the prompt must survive the talk key"
        );
        assert_eq!(
            capture.take_result(),
            Some(("talk".into(), "bound:talk:mouse_x2".into()))
        );
        assert!(dispatch_grabbed(
            &EventType::ButtonRelease(Button::Unknown(2)),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert!(rx.try_recv().is_err(), "capture leaked a talk release");

        // And the prompt still takes a real answer.
        assert!(dispatch_grabbed(
            &EventType::KeyPress(RdevKey::F12),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert_eq!(
            capture.take_result(),
            Some(("talk".into(), "key:F12".into()))
        );
    }

    /// An already-bound control does not become a chord with a modifier
    /// candidate, and neither edge leaks a live action.
    #[test]
    fn suppressing_the_bound_talk_button_keeps_a_modifier_candidacy() {
        let (hook, capture, triggers, ready, tx, rx) =
            grabbed_harness(vec![Trigger::MouseButton(MouseExtra::X2)]);
        capture.start("talk");

        assert!(!dispatch_grabbed(
            &EventType::KeyPress(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert_eq!(
            capture.take_result(),
            Some(("talk".into(), "pending:key:ControlLeft".into()))
        );

        // The existing talk button goes down and up while Ctrl is held.
        assert!(dispatch_grabbed(
            &EventType::ButtonPress(Button::Unknown(2)),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert!(rx.try_recv().is_err(), "capture leaked a talk press");
        assert!(dispatch_grabbed(
            &EventType::ButtonRelease(Button::Unknown(2)),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert!(rx.try_recv().is_err(), "capture leaked a talk release");
        let _bound_note = capture.take_result();

        // The candidacy survived the narration: releasing Ctrl binds it.
        assert!(!dispatch_grabbed(
            &EventType::KeyRelease(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert_eq!(
            capture.take_result(),
            Some(("talk".into(), "key:ControlLeft".into())),
            "suppressing the bound talk button killed the modifier candidacy"
        );
    }

    /// Synthetic keystrokes with real VKs — our own Enter after tap-to-send,
    /// a paste chord, another dictation tool typing — are not a hand on the
    /// keyboard. During a capture they must not hint, and they must not kill
    /// a candidate modifier the person is actually holding.
    #[test]
    fn an_injected_key_never_kills_a_candidacy_or_hints() {
        let (hook, capture, triggers, ready, tx, _rx) = grabbed_harness(Vec::new());
        capture.start("talk");

        assert!(!dispatch_grabbed(
            &EventType::KeyPress(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        let _pending = capture.take_result();

        // The app's own tap-to-send just typed an Enter.
        assert!(!dispatch_grabbed(
            &EventType::KeyPress(RdevKey::Return),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            true
        ));
        assert!(!dispatch_grabbed(
            &EventType::KeyRelease(RdevKey::Return),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            true
        ));
        assert_eq!(
            capture.take_result(),
            None,
            "an injected Return must neither hint nor spend anything"
        );

        // The candidacy survived the tool: the release still binds.
        assert!(!dispatch_grabbed(
            &EventType::KeyRelease(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert_eq!(
            capture.take_result(),
            Some(("talk".into(), "key:ControlLeft".into())),
            "an injected key killed the modifier candidacy"
        );
    }

    /// A synthetic paste chord (Ctrl down, 'v' as VK_PACKET, Ctrl up) is the
    /// app — or any other tool — typing. Its Ctrl must never become a
    /// candidate, and its release must never bind: this is how paste-insert
    /// could silently bind ControlLeft to an armed row.
    #[test]
    fn an_injected_modifier_never_becomes_a_candidate_or_binds() {
        let (hook, capture, triggers, ready, tx, _rx) = grabbed_harness(Vec::new());
        capture.start("talk");

        assert!(!dispatch_grabbed(
            &EventType::KeyPress(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            true
        ));
        assert!(!dispatch_grabbed(
            &EventType::KeyPress(RdevKey::Unknown(231)),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            true
        ));
        assert!(!dispatch_grabbed(
            &EventType::KeyRelease(RdevKey::Unknown(231)),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            true
        ));
        assert!(!dispatch_grabbed(
            &EventType::KeyRelease(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            true
        ));
        assert_eq!(
            capture.take_result(),
            None,
            "a synthetic paste chord produced a capture event — its Ctrl \
             became a candidate or bound on release"
        );
        assert!(capture.is_capturing(), "the prompt must still be waiting");

        // And a real hand still answers afterwards.
        assert!(dispatch_grabbed(
            &EventType::KeyPress(RdevKey::F12),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert_eq!(
            capture.take_result(),
            Some(("talk".into(), "key:F12".into()))
        );
    }

    /// The app's own paste chord releases Ctrl while the person is physically
    /// holding Ctrl to talk. A synthetic up-edge must not cut their recording,
    /// and a synthetic re-press of the held key must not double-fire it.
    #[test]
    fn an_injected_release_never_cuts_a_physical_hold() {
        let (hook, capture, triggers, ready, tx, rx) =
            grabbed_harness(vec![Trigger::Key("ControlLeft".into())]);

        assert!(dispatch_grabbed(
            &EventType::KeyPress(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert!(matches!(rx.try_recv(), Ok(TriggerEvent::TalkPressed(_))));

        // The paste chord's synthetic Ctrl edges arrive mid-hold.
        assert!(dispatch_grabbed(
            &EventType::KeyPress(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            true
        ));
        assert!(
            rx.try_recv().is_err(),
            "a synthetic re-press double-fired the engine"
        );
        assert!(!dispatch_grabbed(
            &EventType::KeyRelease(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            true
        ));
        assert!(
            rx.try_recv().is_err(),
            "a synthetic release cut off the physical hold"
        );

        // The real release still ends the recording exactly once.
        assert!(dispatch_grabbed(
            &EventType::KeyRelease(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert!(matches!(rx.try_recv(), Ok(TriggerEvent::TalkReleased(_))));
    }

    /// A synthetic chord sharing the held modifier's key must not erase the
    /// bookkeeping of the physical hold: the candidacy survives the tool's
    /// edges AND the auto-repeat that follows them, and the real release
    /// still binds.
    #[test]
    fn a_synthetic_chord_cannot_desync_a_held_candidacy() {
        let (hook, capture, triggers, ready, tx, _rx) = grabbed_harness(Vec::new());
        capture.start("talk");

        assert!(!dispatch_grabbed(
            &EventType::KeyPress(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        let _pending = capture.take_result();

        // The tool's Ctrl edges, then the keyboard's own auto-repeat of the
        // still-held physical Ctrl.
        assert!(!dispatch_grabbed(
            &EventType::KeyPress(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            true
        ));
        assert!(!dispatch_grabbed(
            &EventType::KeyRelease(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            true
        ));
        assert!(!dispatch_grabbed(
            &EventType::KeyPress(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert_eq!(
            capture.take_result(),
            None,
            "the auto-repeat after the synthetic chord killed the candidacy"
        );

        assert!(!dispatch_grabbed(
            &EventType::KeyRelease(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert_eq!(
            capture.take_result(),
            Some(("talk".into(), "key:ControlLeft".into())),
            "the candidacy did not survive the app's own paste chord"
        );
    }

    /// A modifier pressed at one prompt and released at a newer one must not
    /// answer the newer prompt: nobody pressed anything for that row.
    #[test]
    fn a_stale_candidacy_never_answers_a_newer_prompt() {
        let (hook, capture, triggers, ready, tx, _rx) = grabbed_harness(Vec::new());
        capture.start("talk");

        assert!(!dispatch_grabbed(
            &EventType::KeyPress(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        let _pending = capture.take_result();

        // The user clicks Add on another row while still holding Ctrl.
        capture.start("send");
        assert_eq!(
            capture.take_result(),
            Some(("talk".into(), String::new())),
            "arming another row cancels the first"
        );

        assert!(!dispatch_grabbed(
            &EventType::KeyRelease(RdevKey::ControlLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert_eq!(
            capture.take_result(),
            None,
            "a candidacy from the old prompt bound to the new one"
        );

        // The new prompt is still armed and takes a real answer.
        assert!(dispatch_grabbed(
            &EventType::KeyPress(RdevKey::F12),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert_eq!(
            capture.take_result(),
            Some(("send".into(), "key:F12".into()))
        );
    }

    /// A synthetic re-emission of a bound control is also capture-only. It is
    /// not a person at the prompt, so there is no hint or deadline extension.
    #[test]
    fn an_injected_bound_control_neither_fires_hints_nor_extends() {
        let (hook, capture, triggers, ready, tx, rx) =
            grabbed_harness(vec![Trigger::MouseButton(MouseExtra::X2)]);
        capture.start("talk");

        assert!(dispatch_grabbed(
            &EventType::ButtonPress(Button::Unknown(2)),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            true
        ));
        assert!(
            rx.try_recv().is_err(),
            "capture leaked an injected talk press"
        );
        assert_eq!(
            capture.take_result(),
            None,
            "a synthetic press of the bound control must not hint"
        );
        assert!(capture.is_capturing());
    }

    /// The owner's mouse reports X2 and then Logitech software injects Left
    /// Win. X2 was already the last talk binding: the prompt must explain the
    /// duplicate, suppress the live talk action, and reject the injected
    /// companion instead of binding it as the replacement.
    #[test]
    fn injected_companion_after_a_bound_x2_cannot_win_capture() {
        let (hook, capture, triggers, ready, tx, rx) =
            grabbed_harness(vec![Trigger::MouseButton(MouseExtra::X2)]);
        capture.start("talk");

        assert!(dispatch_grabbed(
            &EventType::ButtonPress(Button::Unknown(2)),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert_eq!(
            capture.take_result(),
            Some(("talk".into(), "bound:talk:mouse_x2".into()))
        );
        assert!(
            rx.try_recv().is_err(),
            "X2 started recording during capture"
        );

        assert!(dispatch_grabbed(
            &EventType::KeyPress(RdevKey::MetaLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            true
        ));
        assert_eq!(
            capture.take_result(),
            None,
            "the injected Win-key companion replaced X2"
        );
        assert!(capture.is_capturing());

        assert!(dispatch_grabbed(
            &EventType::KeyRelease(RdevKey::MetaLeft),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            true
        ));
        assert!(dispatch_grabbed(
            &EventType::ButtonRelease(Button::Unknown(2)),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert!(rx.try_recv().is_err(), "capture leaked a release action");

        assert!(dispatch_grabbed(
            &EventType::KeyPress(RdevKey::F12),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false
        ));
        assert_eq!(
            capture.take_result(),
            Some(("talk".into(), "key:F12".into()))
        );
    }

    fn gamepad_dispatcher(
        ready: Arc<AtomicBool>,
    ) -> (NativeDispatcher, NativeDevice, TriggerEventReceiver) {
        let triggers = Arc::new(Mutex::new((
            vec![Trigger::GamepadButton {
                device: DeviceSelector::any(),
                button: GamepadButton::South,
            }],
            Vec::new(),
            Vec::new(),
        )));
        let (tx, rx) = trigger_event_channel();
        (
            NativeDispatcher::new(triggers, Arc::new(CaptureShared::default()), ready, tx),
            NativeDevice::new("xinput:0".into(), None, None, None),
            rx,
        )
    }

    #[test]
    fn native_dispatch_deduplicates_repeat_and_releases_on_disconnect() {
        let ready = Arc::new(AtomicBool::new(true));
        let (mut dispatch, device, rx) = gamepad_dispatcher(ready);
        let control = NativeControl::Gamepad(GamepadButton::South);
        dispatch.edge(&device, control, true);
        dispatch.edge(&device, control, true);
        let TriggerEvent::TalkPressed(id) = rx.recv_timeout(Duration::from_secs(10)).unwrap()
        else {
            panic!("expected talk press");
        };
        assert!(
            rx.try_recv().is_err(),
            "auto-repeat dispatched a second edge"
        );
        dispatch.disconnect(device.fingerprint);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            TriggerEvent::TalkReleased(id)
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            TriggerEvent::DeviceDisconnected(device.fingerprint)
        );
    }

    #[test]
    fn native_dispatch_saturation_drops_press_without_stale_active_edge() {
        let ready = Arc::new(AtomicBool::new(true));
        let triggers = Arc::new(Mutex::new((
            vec![Trigger::GamepadButton {
                device: DeviceSelector::any(),
                button: GamepadButton::South,
            }],
            Vec::new(),
            Vec::new(),
        )));
        let (tx, rx) = trigger_event_channel();
        let filler = TriggerId::synthetic(88);
        for _ in 0..TRIGGER_ACTION_QUEUE_CAPACITY {
            tx.try_send(TriggerEvent::SendTapped(filler)).unwrap();
        }
        let mut dispatch = NativeDispatcher::new(
            triggers,
            Arc::new(CaptureShared::default()),
            ready.clone(),
            tx,
        );
        let device = NativeDevice::new("xinput:0".into(), None, None, None);
        let control = NativeControl::Gamepad(GamepadButton::South);

        dispatch.edge(&device, control, true);
        dispatch.edge(&device, control, false);
        assert!(
            ready.load(Ordering::Acquire),
            "a full queue is not a dead listener"
        );
        for _ in 0..TRIGGER_ACTION_QUEUE_CAPACITY {
            assert_eq!(rx.try_recv().unwrap(), TriggerEvent::SendTapped(filler));
        }
        assert!(
            rx.try_recv().is_err(),
            "dropped press acquired an active release"
        );
    }

    #[test]
    fn global_dispatch_saturation_passes_through_without_stale_release() {
        let (hook, capture, triggers, ready, tx, rx) =
            grabbed_harness(vec![Trigger::MouseButton(MouseExtra::X2)]);
        let filler = TriggerId::synthetic(89);
        for _ in 0..TRIGGER_ACTION_QUEUE_CAPACITY {
            tx.try_send(TriggerEvent::TeachTapped(filler)).unwrap();
        }

        assert!(!dispatch_grabbed(
            &EventType::ButtonPress(Button::Unknown(2)),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false,
        ));
        assert!(!dispatch_grabbed(
            &EventType::ButtonRelease(Button::Unknown(2)),
            &hook,
            &capture,
            &triggers,
            &ready,
            &tx,
            false,
        ));
        assert!(ready.load(Ordering::Acquire));
        for _ in 0..TRIGGER_ACTION_QUEUE_CAPACITY {
            assert_eq!(rx.try_recv().unwrap(), TriggerEvent::TeachTapped(filler));
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn macos_callback_source_uses_the_same_non_blocking_sender_contract() {
        let source = include_str!("hotkey_macos.rs");
        assert!(source.contains("TriggerEventSender"));
        assert!(source.contains("try_send(trigger_event)"));
        assert!(!source.contains(".tx.send("));
    }

    #[test]
    fn platform_listener_sources_have_explicit_bounded_shutdown_contracts() {
        let mac = include_str!("hotkey_macos.rs");
        assert!(mac.contains("while !shutdown.load(Ordering::Acquire)"));
        assert!(mac.contains("CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.1"));
        assert!(mac.contains("CFRunLoopRemoveSource(run_loop, source"));
        assert!(mac.contains("CFRelease(source as *const c_void)"));
        assert!(!mac.contains("CFRunLoopRun();"));

        let native = include_str!("input_windows.rs");
        assert!(native.contains(") -> Vec<thread::JoinHandle<()>>"));
        assert!(native.contains("while !shutdown.load(Ordering::Acquire)"));
        assert!(native.contains("message.message == WM_TIMER"));
        assert!(native.contains("DestroyWindow(window)"));

        let hook = include_str!("hook_windows.rs");
        assert!(hook.contains("watch_shutdown.load(Ordering::Acquire)"));
        assert!(hook.contains("PostThreadMessageW(hook_thread_id, WM_QUIT"));

        let supervisor = include_str!("hotkey.rs");
        assert!(supervisor.contains("native_stop.store(true, Ordering::Release)"));
        assert!(supervisor.contains("worker.join()"));
    }

    /// The not-ready count is process-wide, as it is for the real hook. Tests
    /// that press a bound talk control while unready hold this, so a count
    /// asserted by one cannot move under it because of another.
    static NOT_READY_COUNT_LOCK: Mutex<()> = Mutex::new(());

    fn not_ready_count_guard() -> std::sync::MutexGuard<'static, ()> {
        NOT_READY_COUNT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn unready_talk_press_passes_through_and_is_counted_once_per_hold() {
        let _count = not_ready_count_guard();
        let (hook, capture, triggers, ready, tx, rx) =
            grabbed_harness(vec![Trigger::Key("F9".into())]);
        ready.store(false, Ordering::Release);
        let press = |key| {
            dispatch_grabbed(
                &EventType::KeyPress(key),
                &hook,
                &capture,
                &triggers,
                &ready,
                &tx,
                false,
            )
        };
        let release = |key| {
            dispatch_grabbed(
                &EventType::KeyRelease(key),
                &hook,
                &capture,
                &triggers,
                &ready,
                &tx,
                false,
            )
        };

        let before = talk_presses_while_not_ready();
        assert!(!press(RdevKey::F9), "an unready talk key must still pass");
        assert_eq!(talk_presses_while_not_ready(), before + 1);
        assert!(!press(RdevKey::F9), "auto-repeat passes too");
        assert_eq!(
            talk_presses_while_not_ready(),
            before + 1,
            "auto-repeat is not another press"
        );
        assert!(!release(RdevKey::F9));
        assert!(!press(RdevKey::KeyA), "an unbound key is not a talk press");
        assert!(!release(RdevKey::KeyA));
        assert_eq!(talk_presses_while_not_ready(), before + 1);
        assert!(rx.try_recv().is_err(), "nothing may reach the engine");

        ready.store(true, Ordering::Release);
        assert!(press(RdevKey::F9), "a ready talk key is consumed as before");
        assert!(matches!(rx.try_recv(), Ok(TriggerEvent::TalkPressed(_))));
        assert_eq!(talk_presses_while_not_ready(), before + 1);
    }

    /// Delivering a dictation leaves the engine unready, and the paste chord
    /// that delivers it sends an injected Ctrl. With talk bound to Ctrl, that
    /// synthetic press is not the person trying again and must not end every
    /// pasted dictation with "still working".
    #[test]
    fn unready_injected_talk_press_passes_through_uncounted() {
        let _count = not_ready_count_guard();
        let (hook, capture, triggers, ready, tx, rx) =
            grabbed_harness(vec![Trigger::Key("ControlLeft".into())]);
        ready.store(false, Ordering::Release);
        let dispatch = |event, injected| {
            dispatch_grabbed(&event, &hook, &capture, &triggers, &ready, &tx, injected)
        };

        let before = talk_presses_while_not_ready();
        assert!(!dispatch(EventType::KeyPress(RdevKey::ControlLeft), true));
        assert!(!dispatch(EventType::KeyRelease(RdevKey::ControlLeft), true));
        assert_eq!(
            talk_presses_while_not_ready(),
            before,
            "the app's own paste chord is not a talk press"
        );

        // The person pressing the same key is still told why nothing started.
        assert!(!dispatch(EventType::KeyPress(RdevKey::ControlLeft), false));
        assert_eq!(talk_presses_while_not_ready(), before + 1);
        // A paste chord while they hold it adds nothing either.
        assert!(!dispatch(EventType::KeyPress(RdevKey::ControlLeft), true));
        assert!(!dispatch(EventType::KeyRelease(RdevKey::ControlLeft), true));
        assert!(!dispatch(
            EventType::KeyRelease(RdevKey::ControlLeft),
            false
        ));
        assert_eq!(talk_presses_while_not_ready(), before + 1);
        assert!(rx.try_recv().is_err(), "nothing may reach the engine");
    }

    #[test]
    fn native_unready_talk_press_is_counted_but_not_queued() {
        let _count = not_ready_count_guard();
        let ready = Arc::new(AtomicBool::new(false));
        let (mut dispatch, device, rx) = gamepad_dispatcher(ready);
        let control = NativeControl::Gamepad(GamepadButton::South);
        let before = talk_presses_while_not_ready();
        dispatch.edge(&device, control, true);
        dispatch.edge(&device, control, true);
        dispatch.edge(&device, control, false);
        assert_eq!(talk_presses_while_not_ready(), before + 1);
        dispatch.edge(&device, NativeControl::Gamepad(GamepadButton::North), true);
        assert_eq!(talk_presses_while_not_ready(), before + 1);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn native_dispatch_does_not_queue_input_while_engine_is_unready() {
        let _count = not_ready_count_guard();
        let ready = Arc::new(AtomicBool::new(false));
        let (mut dispatch, device, rx) = gamepad_dispatcher(ready.clone());
        let control = NativeControl::Gamepad(GamepadButton::South);
        dispatch.edge(&device, control, true);
        assert!(rx.try_recv().is_err());

        ready.store(true, Ordering::Release);
        // This is still the same physical hold (an auto-repeat), so becoming
        // ready must not turn it into a new press.
        dispatch.edge(&device, control, true);
        assert!(rx.try_recv().is_err());

        // Release clears the physical state; only the next real down activates.
        dispatch.edge(&device, control, false);
        dispatch.edge(&device, control, true);
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            TriggerEvent::TalkPressed(_)
        ));
    }

    #[test]
    fn native_dispatch_does_not_activate_held_control_after_live_rebind() {
        let ready = Arc::new(AtomicBool::new(true));
        let (mut dispatch, device, rx) = gamepad_dispatcher(ready);
        let control = NativeControl::Gamepad(GamepadButton::South);
        dispatch.triggers.lock().unwrap().0.clear();

        dispatch.edge(&device, control, true);
        assert!(rx.try_recv().is_err());
        dispatch
            .triggers
            .lock()
            .unwrap()
            .0
            .push(Trigger::GamepadButton {
                device: DeviceSelector::any(),
                button: GamepadButton::South,
            });

        // An OS repeat after the edit is not a new physical edge.
        dispatch.edge(&device, control, true);
        assert!(rx.try_recv().is_err());
        dispatch.edge(&device, control, false);
        dispatch.edge(&device, control, true);
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            TriggerEvent::TalkPressed(_)
        ));
    }

    /// Three heuristics were tried here against the owner's real hardware and
    /// all three lost: a control-id blacklist (HID++ never repeats an id), an
    /// opening-of-capture window (his second receiver was quiet for it and
    /// walked in after), and a silence-before-the-edge test (046d:c548 speaks
    /// rarely, so it is silent by that measure every time and took the binding
    /// again with control 285212721).
    ///
    /// A report stream cannot say whether a person caused it, so capture stops
    /// guessing. Vendor pages are never offered; a receiver's keyboard and mouse
    /// already arrive as standard-page devices, which is what anyone wants to
    /// bind anyway.
    /// The prompt is opened by clicking a button with the mouse, and an
    /// unsupported answer ends a capture, so the ordinary mouse buttons could
    /// close the very prompt they had just opened. The owner's log was ten
    /// straight `Mouse(Left) -> unsupported` lines and not one keystroke: every
    /// Ctrl he pressed arrived after a click had already spent the capture.
    /// He concluded his Ctrl key was broken. It was not, and neither was the
    /// other one.
    /// The whole value of the hint is that it explains without giving up. If it
    /// ended the capture it would be one more thing that says something and
    /// then stops listening — which is the failure it exists to prevent.
    #[test]
    fn a_hint_explains_without_ending_the_capture() {
        let capture = CaptureShared::default();
        capture.start("talk");

        assert!(
            capture.hint("mouse_click"),
            "first click should be explained"
        );
        assert!(capture.is_capturing(), "the hint closed the capture");
        assert_eq!(
            capture.take_result(),
            Some(("talk".into(), "mouse_click".into()))
        );

        // Once per capture. A remapped key arrives as a click on every press,
        // and repeating the message on each one is its own kind of noise.
        assert!(!capture.hint("mouse_click"));
        assert!(capture.is_capturing());

        // The real key still binds afterwards, which is the point.
        assert!(capture.finish("key:F9".into()));
        assert_eq!(
            capture.take_result(),
            Some(("talk".into(), "key:F9".into()))
        );
        assert!(!capture.is_capturing());

        // And a hint with nothing pending is not a result out of nowhere.
        assert!(!capture.hint("mouse_click"));
        assert_eq!(capture.take_result(), None);
    }

    #[test]
    fn ordinary_mouse_buttons_cannot_close_the_prompt_they_opened() {
        for button in [Button::Left, Button::Right, Button::Middle] {
            let (code, supported) = capture_code(RdevInput::Mouse(button));
            assert!(
                !supported,
                "{button:?} is not bindable, so it can only ever answer as unsupported"
            );
            // Its own note rather than the generic one: the click that opened
            // the prompt is the commonest thing to land in it, and "that was a
            // click" is more use to a reader than "unsupported". The special
            // case used to live in the hook closure; classification belongs
            // here, where every caller sees the same answer.
            assert_eq!(code, "mouse_click");
        }

        // The thumb buttons are the ones people actually bind, and they still
        // answer. Nothing about ignoring clicks may cost them that.
        assert_eq!(
            capture_code(RdevInput::Mouse(Button::Unknown(1))),
            ("mouse_x1".to_string(), true)
        );
        assert_eq!(
            capture_code(RdevInput::Mouse(Button::Unknown(2))),
            ("mouse_x2".to_string(), true)
        );

        // And the keys he was trying to bind resolve perfectly well — the
        // capture never reached them.
        for key in [RdevKey::ControlRight, RdevKey::ControlLeft] {
            let (code, supported) = capture_code(RdevInput::Key(key));
            assert!(supported, "{key:?} should be bindable");
            assert!(code.starts_with("key:Control"), "{code}");
        }
    }

    #[test]
    fn vendor_pages_are_never_offered_to_a_capture() {
        let capture = Arc::new(CaptureShared::default());
        capture.start("talk");
        let (tx, _rx) = trigger_event_channel();
        let dispatch = NativeDispatcher::new(
            Arc::new(Mutex::new((Vec::new(), Vec::new(), Vec::new()))),
            capture,
            Arc::new(AtomicBool::new(true)),
            tx,
        );
        let vendor = |usage, control| NativeControl::Hid {
            usage_page: 0xff00,
            usage,
            control,
        };

        // Both of the receivers that actually broke this, and the exact controls
        // they bound.
        assert!(!dispatch.capture_admits(vendor(2, 285212706)));
        assert!(!dispatch.capture_admits(vendor(2, 285212712)));
        assert!(!dispatch.capture_admits(vendor(2, 285212721)));
        // The top of the vendor range too, not just the page they happened to use.
        assert!(!dispatch.capture_admits(NativeControl::Hid {
            usage_page: 0xffff,
            usage: 1,
            control: 1,
        }));

        // Everything a person can only produce deliberately still answers.
        assert!(dispatch.capture_admits(NativeControl::Gamepad(GamepadButton::South)));
        assert!(dispatch.capture_admits(NativeControl::Consumer(0x00cd)));
        // Standard pages, including the Button page a pedal normally declares.
        assert!(dispatch.capture_admits(NativeControl::Hid {
            usage_page: 0x09,
            usage: 1,
            control: 1,
        }));
    }

    #[test]
    fn xinput_capture_uses_port_independent_structured_binding() {
        let capture = Arc::new(CaptureShared::default());
        capture.start("talk");
        let triggers = Arc::new(Mutex::new((Vec::new(), Vec::new(), Vec::new())));
        let (tx, rx) = trigger_event_channel();
        let mut dispatch = NativeDispatcher::new(
            triggers,
            capture.clone(),
            Arc::new(AtomicBool::new(true)),
            tx,
        );
        let device = NativeDevice::new("xinput:3".into(), None, None, None);
        let control = NativeControl::Gamepad(GamepadButton::South);
        dispatch.edge(&device, control, true);
        let (_, code) = capture.take_result().unwrap();
        let trigger: Trigger =
            serde_json::from_str(code.strip_prefix("trigger:").unwrap()).unwrap();
        assert_eq!(
            trigger,
            Trigger::GamepadButton {
                device: DeviceSelector::any(),
                button: GamepadButton::South,
            }
        );
        dispatch.edge(&device, control, false);
        assert!(rx.try_recv().is_err(), "capture leaked an action event");
    }

    /// The Windows default trigger has to resolve here.
    #[test]
    fn windows_default_trigger_resolves() {
        assert!(key_from_name("F13").is_some());
        assert!(key_from_name("CapsLock").is_some());
    }
}
