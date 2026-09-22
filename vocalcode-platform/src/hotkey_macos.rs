//! Global push-to-talk listener for macOS, built on a native `CGEventTap`.
//!
//! # Why not `rdev` here
//!
//! The rest of the platform layer uses `rdev`, but its macOS backend converts
//! only `LeftMouse*` and `RightMouse*` into `Button` values — `OtherMouseDown` /
//! `OtherMouseUp` are dropped on the floor. Those are exactly the events the
//! mouse thumb buttons produce, and holding the forward thumb button is
//! VocalCode's default (and signature) way to talk. Going straight to the
//! CoreGraphics event tap keeps that interaction working on macOS.
//!
//! # Two things the tap has to get right
//!
//! *Modifier keys never emit key-down/key-up.* Shift, Control, Option, Command
//! and Caps Lock arrive as `FlagsChanged`, so press-vs-release is decided by
//! testing the device-specific flag bit for that key code, not by event type.
//!
//! *The system can switch the tap off.* If our callback is too slow macOS
//! delivers `TapDisabledByTimeout` and stops sending events until the tap is
//! re-enabled, which would silently wedge push-to-talk. We re-enable on sight.
//!
//! Requires **Input Monitoring** (to observe events) and, because this is an
//! active tap that consumes the bound keys, **Accessibility**.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::TrySendError;
use std::sync::{Arc, Mutex};

use vocalcode_core::config::{MouseExtra, Trigger};
use vocalcode_core::error::{Result, VocalCodeError};
use vocalcode_core::traits::{HotkeyListener, TriggerEvent, TriggerId, TriggerSource};
use vocalcode_core::TriggerEventSender;

use crate::hotkey::{CaptureShared, SharedTriggers};

// ---------------------------------------------------------------------------
// CoreFoundation / CoreGraphics FFI
// ---------------------------------------------------------------------------

type CFMachPortRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFAllocatorRef = *const c_void;
type CFStringRef = *const c_void;
type CGEventRef = *mut c_void;
type CGEventTapProxy = *mut c_void;

type CGEventTapCallBack = extern "C" fn(
    proxy: CGEventTapProxy,
    etype: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventGetFlags(event: CGEventRef) -> u64;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFMachPortCreateRunLoopSource(
        allocator: CFAllocatorRef,
        port: CFMachPortRef,
        order: i64,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRemoveSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, return_after_source: u8) -> i32;
    static kCFRunLoopDefaultMode: CFStringRef;
    fn CFRelease(cf: *const c_void);
    static kCFRunLoopCommonModes: CFStringRef;
}

// `CGEventType` values we care about.
const EVENT_KEY_DOWN: u32 = 10;
const EVENT_KEY_UP: u32 = 11;
const EVENT_FLAGS_CHANGED: u32 = 12;
const EVENT_OTHER_MOUSE_DOWN: u32 = 25;
const EVENT_OTHER_MOUSE_UP: u32 = 26;
const EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

/// `kCGKeyboardEventKeycode`.
const FIELD_KEYCODE: u32 = 9;
/// `kCGMouseEventButtonNumber`.
const FIELD_BUTTON_NUMBER: u32 = 3;

/// `kCGHIDEventTap` — the earliest point in the stream, so we see input before
/// any application does and can consume it.
const TAP_HID: u32 = 0;
/// `kCGHeadInsertEventTap`.
const TAP_PLACE_HEAD: u32 = 0;
/// `kCGEventTapOptionDefault` — an *active* tap, i.e. one allowed to modify or
/// swallow events. A listen-only tap could not stop the thumb button from also
/// triggering Back in whatever app has focus.
const TAP_OPTION_ACTIVE: u32 = 0;

/// macOS mouse button numbers for the thumb buttons. Unlike Windows' X1/X2
/// these are plain button indices on `OtherMouse` events: 2 is the wheel click,
/// 3 the back/rear thumb button, 4 the forward/front one.
const BUTTON_BACK: i64 = 3;
const BUTTON_FORWARD: i64 = 4;

// ---------------------------------------------------------------------------
// Key codes
// ---------------------------------------------------------------------------

/// `kVK_Escape` — cancels an in-progress key capture rather than binding.
const VK_ESCAPE: i64 = 0x35;

// Modifier key codes, needed both in the table below and by `modifier_mask`.
const VK_COMMAND_RIGHT: i64 = 0x36;
const VK_COMMAND_LEFT: i64 = 0x37;
const VK_SHIFT_LEFT: i64 = 0x38;
const VK_CAPS_LOCK: i64 = 0x39;
const VK_OPTION_LEFT: i64 = 0x3A;
const VK_CONTROL_LEFT: i64 = 0x3B;
const VK_SHIFT_RIGHT: i64 = 0x3C;
const VK_OPTION_RIGHT: i64 = 0x3D;
const VK_CONTROL_RIGHT: i64 = 0x3E;
const VK_FUNCTION: i64 = 0x3F;

/// Every key that may execute a saved push-to-talk trigger: `(key code, name)`.
///
/// One table drives both directions of the mapping. Keeping two hand-written
/// match arms in sync was a standing invitation for them to drift, and a name
/// that survives a round trip in only one direction produces a binding that
/// saves but never fires.
///
/// Names are the `KeyboardEvent.code` spelling wherever one exists, so a config
/// file reads the same on every platform.
///
/// Key codes are the Carbon `Events.h` `kVK_*` constants. Note how little they
/// follow keyboard order: F1 is 0x7A, F2 is 0x78, F3 is 0x63.
static BINDABLE_KEYS: &[(i64, &str)] = &[
    // Modifiers — the natural hold-to-talk keys.
    // Legacy-only Caps Lock behaves as press-to-start/press-again-to-stop
    // because macOS exposes latch changes rather than a held edge. Continue to
    // execute old configs, but capture below does not offer it anew.
    (VK_CAPS_LOCK, "CapsLock"),
    (VK_SHIFT_LEFT, "ShiftLeft"),
    (VK_SHIFT_RIGHT, "ShiftRight"),
    (VK_CONTROL_LEFT, "ControlLeft"),
    (VK_CONTROL_RIGHT, "ControlRight"),
    (VK_OPTION_LEFT, "AltLeft"),
    (VK_OPTION_RIGHT, "AltRight"),
    (VK_COMMAND_LEFT, "MetaLeft"),
    (VK_COMMAND_RIGHT, "MetaRight"),
    // Function row. F13–F20 exist on full-size Apple keyboards and are the
    // cleanest triggers of all — nothing else claims them.
    (0x7A, "F1"),
    (0x78, "F2"),
    (0x63, "F3"),
    (0x76, "F4"),
    (0x60, "F5"),
    (0x61, "F6"),
    (0x62, "F7"),
    (0x64, "F8"),
    (0x65, "F9"),
    (0x6D, "F10"),
    (0x67, "F11"),
    (0x6F, "F12"),
    (0x69, "F13"),
    (0x6B, "F14"),
    (0x71, "F15"),
    (0x6A, "F16"),
    (0x40, "F17"),
    (0x4F, "F18"),
    (0x50, "F19"),
    (0x5A, "F20"),
    // Navigation and editing keys that are not part of ordinary typing.
    (0x7B, "ArrowLeft"),
    (0x7C, "ArrowRight"),
    (0x7D, "ArrowDown"),
    (0x7E, "ArrowUp"),
    (0x73, "Home"),
    (0x77, "End"),
    (0x74, "PageUp"),
    (0x79, "PageDown"),
    (0x75, "Delete"), // forward delete (fn-delete), not Backspace
    (0x72, "Help"),
    // Legacy-only: capture rejects typing controls before consulting this
    // table, but configs written by older releases must continue to work.
    (0x31, "Space"),
    (0x30, "Tab"),
    (0x32, "Backquote"),
    // Numeric keypad — plentiful and rarely used while dictating.
    (0x52, "Numpad0"),
    (0x53, "Numpad1"),
    (0x54, "Numpad2"),
    (0x55, "Numpad3"),
    (0x56, "Numpad4"),
    (0x57, "Numpad5"),
    (0x58, "Numpad6"),
    (0x59, "Numpad7"),
    (0x5B, "Numpad8"),
    (0x5C, "Numpad9"),
    (0x41, "NumpadDecimal"),
    (0x43, "NumpadMultiply"),
    (0x45, "NumpadAdd"),
    (0x4B, "NumpadDivide"),
    (0x4E, "NumpadSubtract"),
    (0x51, "NumpadEqual"),
    (0x47, "NumpadClear"),
    (0x4C, "NumpadEnter"),
];

/// Older/friendlier spellings accepted when reading a config, so hand-edited
/// files and configs written by the Windows build keep working.
static NAME_ALIASES: &[(&str, &str)] = &[
    ("scrolllock", "F14"), // a PC Scroll Lock arrives as F14 on macOS
    ("ScrollLock", "F14"),
    ("scroll", "F14"),
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

/// Keys that would fire constantly during normal typing. These are recognised
/// on purpose: the capture UI reports them as unusable instead of ignoring the
/// keypress, which is what made key recording look broken.
fn is_typing_key(keycode: i64) -> bool {
    matches!(
        keycode,
        // The main typing block: letters, digits, punctuation, Return, and —
        // at 0x33, the top of the range — Backspace. All ordinary typing
        // controls, so none of them can be stolen as a global binding.
        0x00..=0x33
    )
}

/// Device-dependent modifier flag bits (`NX_DEVICE*KEYMASK`), used to tell a
/// modifier press from a release on a `FlagsChanged` event. The generic masks
/// cannot do this — they do not distinguish left from right.
fn modifier_mask(keycode: i64) -> Option<u64> {
    Some(match keycode {
        VK_CONTROL_LEFT => 0x0000_0001,
        VK_SHIFT_LEFT => 0x0000_0002,
        VK_SHIFT_RIGHT => 0x0000_0004,
        VK_COMMAND_LEFT => 0x0000_0008,
        VK_COMMAND_RIGHT => 0x0000_0010,
        VK_OPTION_LEFT => 0x0000_0020,
        VK_OPTION_RIGHT => 0x0000_0040,
        VK_CONTROL_RIGHT => 0x0000_2000,
        // NX_ALPHASHIFTMASK — Caps Lock reports its latched state, not a hold.
        VK_CAPS_LOCK => 0x0001_0000,
        // NX_SECONDARYFNMASK
        VK_FUNCTION => 0x0080_0000,
        _ => return None,
    })
}

/// Key name → macOS key code, accepting the aliases above.
fn key_from_name(name: &str) -> Option<i64> {
    let shared = vocalcode_core::config::canonical_key_name(name).unwrap_or(name);
    let canonical = NAME_ALIASES
        .iter()
        .find(|(alias, _)| *alias == shared)
        .map(|(_, c)| *c)
        .unwrap_or(shared);
    BINDABLE_KEYS
        .iter()
        .find(|(_, n)| *n == canonical)
        .map(|(code, _)| *code)
}

/// macOS key code → the name we persist (the inverse of [`key_from_name`]).
fn key_to_name(keycode: i64) -> Option<&'static str> {
    BINDABLE_KEYS
        .iter()
        .find(|(code, _)| *code == keycode)
        .map(|(_, name)| *name)
}

fn mouse_matches(trigger: &Trigger, button: i64) -> bool {
    let Trigger::MouseButton(extra) = trigger else {
        return false;
    };
    match extra {
        MouseExtra::X1 => button == BUTTON_BACK,
        MouseExtra::X2 => button == BUTTON_FORWARD,
    }
}

fn key_matches(trigger: &Trigger, keycode: i64) -> bool {
    let Trigger::Key(name) = trigger else {
        return false;
    };
    key_from_name(name) == Some(keycode)
}

/// Log every key the tap actually receives, for 25 seconds.
///
/// Exists because whether a given key reaches a `CGEventTap` at all is not
/// something you can reason about — Fn in particular is handled low enough in
/// macOS that it may never surface. This answers it by observation.
pub fn probe_keys() {
    extern "C" fn cb(
        _p: CGEventTapProxy,
        etype: u32,
        event: CGEventRef,
        _u: *mut c_void,
    ) -> CGEventRef {
        if etype == EVENT_TAP_DISABLED_BY_TIMEOUT || etype == EVENT_TAP_DISABLED_BY_USER_INPUT {
            return event;
        }
        let is_mouse = etype == EVENT_OTHER_MOUSE_DOWN || etype == EVENT_OTHER_MOUSE_UP;
        let field = if is_mouse {
            FIELD_BUTTON_NUMBER
        } else {
            FIELD_KEYCODE
        };
        let code = unsafe { CGEventGetIntegerValueField(event, field) };
        let kind = match etype {
            EVENT_KEY_DOWN => "KeyDown",
            EVENT_KEY_UP => "KeyUp",
            EVENT_FLAGS_CHANGED => "FlagsChanged",
            EVENT_OTHER_MOUSE_DOWN => "MouseDown",
            EVENT_OTHER_MOUSE_UP => "MouseUp",
            _ => return event,
        };
        let name = if is_mouse {
            match code {
                BUTTON_BACK => "mouse_x1 (back)".to_string(),
                BUTTON_FORWARD => "mouse_x2 (forward)".to_string(),
                n => format!("mouse button {n}"),
            }
        } else {
            match key_to_name(code) {
                Some(n) => n.to_string(),
                None if is_typing_key(code) => "(typing key)".to_string(),
                None => "(unmapped)".to_string(),
            }
        };
        let state = if etype == EVENT_FLAGS_CHANGED {
            match modifier_mask(code) {
                Some(m) => {
                    if (unsafe { CGEventGetFlags(event) } & m) != 0 {
                        " pressed"
                    } else {
                        " released"
                    }
                }
                None => " (no mask — cannot tell press from release)",
            }
        } else {
            ""
        };
        println!("  {name:22} {kind:13} code={code:#04x}{state}");
        event
    }

    let mask = (1u64 << EVENT_KEY_DOWN)
        | (1u64 << EVENT_KEY_UP)
        | (1u64 << EVENT_FLAGS_CHANGED)
        | (1u64 << EVENT_OTHER_MOUSE_DOWN)
        | (1u64 << EVENT_OTHER_MOUSE_UP);
    unsafe {
        // Listen-only: a probe must not swallow the user's input.
        let tap = CGEventTapCreate(TAP_HID, TAP_PLACE_HEAD, 1, mask, cb, std::ptr::null_mut());
        if tap.is_null() {
            println!("TAP FAILED — Input Monitoring not granted to this binary");
            return;
        }
        let src = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
        CFRunLoopAddSource(CFRunLoopGetCurrent(), src, kCFRunLoopCommonModes);
        CGEventTapEnable(tap, true);
        println!("Probing for 25s — press the keys you want to test.");
        CFRunLoopRunInMode(kCFRunLoopDefaultMode, 25.0, false as u8);
        println!("done");
    }
}

// ---------------------------------------------------------------------------
// The listener
// ---------------------------------------------------------------------------

/// State handed to the C callback via the tap's `user_info` pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Talk,
    Send,
    Teach,
}

#[derive(Default)]
struct EdgeState {
    /// Every key/button physically held, including unbound and unready presses.
    /// `active` contains only actions accepted by the engine.
    physical_down: HashSet<TriggerId>,
    active: HashMap<TriggerId, Action>,
    captured_down: HashSet<TriggerId>,
}

impl EdgeState {
    /// Record a press. `None` means a new physical edge; `Some(consume)` means
    /// an auto-repeat whose pass/swallow disposition must match its first down.
    fn record_press(&mut self, id: TriggerId) -> Option<bool> {
        if self.physical_down.insert(id) {
            None
        } else {
            Some(self.active.contains_key(&id) || self.captured_down.contains(&id))
        }
    }
}

struct TapState {
    triggers: SharedTriggers,
    capture: Arc<CaptureShared>,
    ready: Arc<AtomicBool>,
    tx: TriggerEventSender,
    edges: Mutex<EdgeState>,
    /// The tap itself, so the callback can re-enable it after a timeout.
    tap: Mutex<CFMachPortRef>,
}

// The tap port is only ever touched from the callback on the tap's own thread.
unsafe impl Send for TapState {}
unsafe impl Sync for TapState {}

pub struct MacHotkey {
    triggers: SharedTriggers,
    capture: Arc<CaptureShared>,
    ready: Arc<AtomicBool>,
    installed: Arc<AtomicBool>,
}

impl MacHotkey {
    pub fn new(triggers: SharedTriggers, capture: Arc<CaptureShared>) -> Self {
        Self::new_with_readiness(triggers, capture, Arc::new(AtomicBool::new(true)))
    }

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

impl HotkeyListener for MacHotkey {
    fn run(self: Box<Self>, tx: TriggerEventSender, shutdown: Arc<AtomicBool>) -> Result<()> {
        struct InstalledReset(Arc<AtomicBool>);
        impl Drop for InstalledReset {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }

        crate::hotkey::start_hook_diagnostics().map_err(VocalCodeError::Hotkey)?;

        // IOHIDManager shares this listener thread's CFRunLoop with the event
        // tap. Failure leaves keyboard/mouse controls usable and is surfaced in
        // the log instead of taking down the primary hotkey backend.
        let hid = crate::input_macos::install(
            self.triggers.clone(),
            self.capture.clone(),
            self.ready.clone(),
            tx.clone(),
        )
        .map_err(|error| {
            log::warn!("external HID controls unavailable: {error}");
            error
        })
        .ok();

        let mask = (1u64 << EVENT_KEY_DOWN)
            | (1u64 << EVENT_KEY_UP)
            | (1u64 << EVENT_FLAGS_CHANGED)
            | (1u64 << EVENT_OTHER_MOUSE_DOWN)
            | (1u64 << EVENT_OTHER_MOUSE_UP);

        let state = Box::into_raw(Box::new(TapState {
            triggers: self.triggers,
            capture: self.capture,
            ready: self.ready,
            tx,
            edges: Mutex::new(EdgeState::default()),
            tap: Mutex::new(std::ptr::null_mut()),
        }));

        unsafe {
            let tap = CGEventTapCreate(
                TAP_HID,
                TAP_PLACE_HEAD,
                TAP_OPTION_ACTIVE,
                mask,
                tap_callback,
                state as *mut c_void,
            );
            if tap.is_null() {
                // Reclaim the state we would otherwise leak on this error path.
                drop(Box::from_raw(state));
                return Err(VocalCodeError::Hotkey(
                    "could not create event tap — grant Input Monitoring and \
                     Accessibility to VocalCode in System Settings › Privacy & Security"
                        .to_string(),
                ));
            }
            *(*state).tap.lock().unwrap() = tap;

            let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
            if source.is_null() {
                CFRelease(tap as *const c_void);
                drop(Box::from_raw(state));
                return Err(VocalCodeError::Hotkey(
                    "could not create run loop source for the event tap".to_string(),
                ));
            }
            // Attaches to *this* thread's run loop, so the caller is free to run
            // us on a background thread while the UI owns the main thread.
            let run_loop = CFRunLoopGetCurrent();
            CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
            CGEventTapEnable(tap, true);

            self.installed.store(true, Ordering::Release);
            let _installed_reset = InstalledReset(self.installed.clone());

            log::info!(
                "macOS event tap installed (keys + thumb mouse buttons; external HID={})",
                if hid.is_some() { "ready" } else { "disabled" }
            );
            while !shutdown.load(Ordering::Acquire) {
                // A finite slice makes graceful shutdown independent of new
                // keyboard/mouse input and keeps the callback thread joinable.
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.1, false as u8);
            }
            (*state).ready.store(false, Ordering::Release);
            CFRunLoopRemoveSource(run_loop, source, kCFRunLoopCommonModes);
            CGEventTapEnable(tap, false);
            CFRelease(source as *const c_void);
            CFRelease(tap as *const c_void);
            drop(Box::from_raw(state));
        }
        Ok(())
    }
}

extern "C" fn tap_callback(
    _proxy: CGEventTapProxy,
    etype: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    let state = unsafe { &*(user_info as *const TapState) };

    // macOS disables a tap that takes too long, or on certain user input. It
    // stays dead until re-enabled, which would silently break push-to-talk.
    if etype == EVENT_TAP_DISABLED_BY_TIMEOUT || etype == EVENT_TAP_DISABLED_BY_USER_INPUT {
        crate::hotkey::hook_diagnostic(crate::hotkey::HookDiagnostic::MacTapDisabled(etype));
        release_active_talk(state);
        if let Ok(tap) = state.tap.lock() {
            if !tap.is_null() {
                unsafe { CGEventTapEnable(*tap, true) };
            }
        }
        return event;
    }

    let consume = handle_event(state, etype, event);
    if consume {
        std::ptr::null_mut()
    } else {
        event
    }
}

/// Returns true when the event should be swallowed rather than passed on.
fn handle_event(state: &TapState, etype: u32, event: CGEventRef) -> bool {
    let is_mouse = etype == EVENT_OTHER_MOUSE_DOWN || etype == EVENT_OTHER_MOUSE_UP;
    let field = if is_mouse {
        FIELD_BUTTON_NUMBER
    } else {
        FIELD_KEYCODE
    };
    let code = unsafe { CGEventGetIntegerValueField(event, field) };

    // Work out whether this is a press or a release. Modifiers do not send
    // key-up/key-down at all, so their FlagsChanged event is resolved against
    // the device-specific flag bit for that key.
    let pressed = match etype {
        EVENT_KEY_DOWN | EVENT_OTHER_MOUSE_DOWN => true,
        EVENT_KEY_UP | EVENT_OTHER_MOUSE_UP => false,
        EVENT_FLAGS_CHANGED => match modifier_mask(code) {
            // Parenthesised: a match arm starting with a block would otherwise
            // be parsed as the whole arm expression.
            Some(mask) => (unsafe { CGEventGetFlags(event) } & mask) != 0,
            None => return false, // a modifier we do not bind
        },
        _ => return false,
    };

    let id = if is_mouse {
        TriggerId::new(TriggerSource::Mouse, 0, code as u32)
    } else {
        TriggerId::new(TriggerSource::Keyboard, 0, code as u32)
    };
    let Ok(mut edges) = state.edges.lock() else {
        return false;
    };

    if pressed {
        // Record every first down before capture, readiness and binding checks.
        // A held unbound/unready key therefore cannot become newly active when
        // an auto-repeat arrives after the shared state changes.
        if let Some(consume) = edges.record_press(id) {
            return consume;
        }
    } else {
        edges.physical_down.remove(&id);
        // A captured down event is swallowed, so its matching up event must be
        // too; otherwise the foreground app receives half a transition.
        if edges.captured_down.remove(&id) {
            return true;
        }
    }

    // Caps Lock has no release edge in a CGEventTap: FlagsChanged reports its
    // latched state. It is deliberately not bindable, but both latch directions
    // should still immediately resolve a capture as unsupported.
    let capture_press = pressed || (!is_mouse && code == VK_CAPS_LOCK);
    if capture_press && state.capture.is_capturing() {
        let (captured, supported) = if is_mouse {
            match code {
                BUTTON_BACK => ("mouse_x1".to_string(), true),
                BUTTON_FORWARD => ("mouse_x2".to_string(), true),
                _ => ("unsupported".to_string(), false),
            }
        } else if code == VK_ESCAPE {
            (String::new(), true)
        } else if code == VK_CAPS_LOCK {
            ("unsupported".to_string(), false)
        } else if is_typing_key(code) {
            ("typing_key".to_string(), false)
        } else {
            key_to_name(code)
                .map(|name| (format!("key:{name}"), true))
                .unwrap_or_else(|| ("unsupported".to_string(), false))
        };
        if answer_capture(&state.capture, captured, supported) {
            edges.captured_down.insert(id);
            return true;
        }
        if !supported {
            return false;
        }
    }

    if pressed {
        if !state.ready.load(Ordering::Acquire) {
            return false;
        }
        let action = {
            let Ok(bindings) = state.triggers.lock() else {
                return false;
            };
            let matches = |list: &[Trigger]| {
                list.iter().any(|trigger| {
                    if is_mouse {
                        mouse_matches(trigger, code)
                    } else {
                        key_matches(trigger, code)
                    }
                })
            };
            if matches(&bindings.0) {
                Some(Action::Talk)
            } else if matches(&bindings.1) {
                Some(Action::Send)
            } else if matches(&bindings.2) {
                Some(Action::Teach)
            } else {
                None
            }
        };
        let Some(action) = action else {
            return false;
        };
        let trigger_event = match action {
            Action::Talk => TriggerEvent::TalkPressed(id),
            Action::Send => TriggerEvent::SendTapped(id),
            Action::Teach => TriggerEvent::TeachTapped(id),
        };
        match state.tx.try_send(trigger_event) {
            Ok(()) => {
                edges.active.insert(id, action);
                true
            }
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => {
                state.ready.store(false, Ordering::Release);
                false
            }
        }
    } else {
        let Some(action) = edges.active.remove(&id) else {
            return false;
        };
        if action != Action::Talk || state.tx.try_send(TriggerEvent::TalkReleased(id)).is_ok() {
            true
        } else {
            state.ready.store(false, Ordering::Release);
            false
        }
    }
}

fn answer_capture(capture: &CaptureShared, captured: String, supported: bool) -> bool {
    if supported {
        capture.finish(captured)
    } else {
        // Unsupported input is a hint, not a terminal answer. The page
        // intentionally keeps rendering the capture prompt after receiving
        // this code; ending the host state here left that prompt waiting on a
        // capture that no longer existed.
        capture.hint(&captured);
        false
    }
}

/// A disabled event tap can lose the physical release. Close every active talk
/// edge before re-enabling so recording cannot remain wedged indefinitely.
fn release_active_talk(state: &TapState) {
    let Ok(mut edges) = state.edges.lock() else {
        let _ = state.tx.try_send(TriggerEvent::ForceStop);
        return;
    };
    let active = std::mem::take(&mut edges.active);
    edges.physical_down.clear();
    edges.captured_down.clear();
    for (id, action) in active {
        if action == Action::Talk && state.tx.try_send(TriggerEvent::TalkReleased(id)).is_err() {
            state.ready.store(false, Ordering::Release);
            break;
        }
    }
    // A release is deliberately ignored by toggle mode. Once the event tap is
    // disabled the next press may never arrive, so close the transaction after
    // clearing every known edge. In hold mode the synthesized last release has
    // already stopped it and this is a harmless Idle.
    if state.tx.try_send(TriggerEvent::ForceStop).is_err() {
        state.ready.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Every bindable key must survive code → name → code. A name that resolves
    /// in only one direction yields a binding the settings UI happily saves and
    /// the tap then never matches.
    #[test]
    fn bindable_keys_round_trip() {
        for (code, name) in BINDABLE_KEYS {
            assert_eq!(key_to_name(*code), Some(*name), "code {code:#04x} -> name");
            assert_eq!(key_from_name(name), Some(*code), "name {name} -> code");
        }
    }

    /// Duplicates would make the lookups order-dependent: two names for one code
    /// means one of them can never be produced by `key_to_name`.
    #[test]
    fn bindable_keys_are_unique() {
        let mut codes = HashSet::new();
        let mut names = HashSet::new();
        for (code, name) in BINDABLE_KEYS {
            assert!(codes.insert(*code), "duplicate key code {code:#04x}");
            assert!(names.insert(*name), "duplicate key name {name}");
        }
    }

    /// An alias pointing at a name that no longer exists silently disables the
    /// binding it was meant to preserve.
    #[test]
    fn aliases_resolve_to_real_keys() {
        for (alias, canonical) in NAME_ALIASES {
            assert!(
                key_from_name(canonical).is_some(),
                "alias {alias} -> unknown key {canonical}"
            );
            assert_eq!(key_from_name(alias), key_from_name(canonical));
        }
    }

    #[test]
    fn unsupported_hint_does_not_end_the_host_capture() {
        let capture = CaptureShared::default();
        capture.start("talk");

        assert!(!answer_capture(&capture, "typing_key".to_string(), false));
        assert_eq!(
            capture.take_result(),
            Some(("talk".to_string(), "typing_key".to_string()))
        );
        assert!(capture.is_capturing());
        assert!(capture.finish("key:F7".to_string()));
        assert_eq!(
            capture.take_result(),
            Some(("talk".to_string(), "key:F7".to_string()))
        );
        assert!(!capture.is_capturing());
    }

    /// Only compatibility entries may both execute and type. Capture checks the
    /// typing-key guard first and never creates a new one.
    #[test]
    fn bindable_keys_are_not_typing_keys() {
        for (code, name) in BINDABLE_KEYS {
            if is_typing_key(*code) {
                assert!(matches!(*name, "Space" | "Tab" | "Backquote"));
            }
        }
    }

    /// Bindable modifiers are delivered as FlagsChanged, and without a mask
    /// entry the press/release edge cannot be resolved, so the key would do
    /// nothing. Caps Lock is excluded because it exposes latch state rather
    /// than a hold edge; Fn is excluded because it may never reach the tap.
    #[test]
    fn every_modifier_has_a_flag_mask() {
        for code in [
            VK_SHIFT_LEFT,
            VK_SHIFT_RIGHT,
            VK_CONTROL_LEFT,
            VK_CONTROL_RIGHT,
            VK_OPTION_LEFT,
            VK_OPTION_RIGHT,
            VK_COMMAND_LEFT,
            VK_COMMAND_RIGHT,
        ] {
            assert!(modifier_mask(code).is_some(), "no mask for {code:#04x}");
            assert!(
                key_to_name(code).is_some(),
                "modifier {code:#04x} not bindable"
            );
        }
    }

    /// Escape cancels a capture, so it must not also be a bindable key.
    #[test]
    fn escape_is_not_bindable() {
        assert_eq!(key_to_name(VK_ESCAPE), None);
    }

    /// The default trigger shipped on macOS has to be resolvable here, or the
    /// app starts with a binding that cannot fire.
    #[test]
    fn macos_default_trigger_resolves() {
        assert!(key_from_name("AltRight").is_some());
    }

    #[test]
    fn held_unready_press_cannot_activate_from_repeat_after_ready() {
        let id = TriggerId::new(TriggerSource::Keyboard, 0, 0x60);
        let ready = AtomicBool::new(false);
        let mut edges = EdgeState::default();

        assert_eq!(edges.record_press(id), None);
        assert!(!ready.load(Ordering::Acquire));
        ready.store(true, Ordering::Release);
        assert_eq!(edges.record_press(id), Some(false));

        // Its release resets the physical edge, so the next press is fresh.
        edges.physical_down.remove(&id);
        assert_eq!(edges.record_press(id), None);
    }

    #[test]
    fn repeat_keeps_the_first_downs_consumed_disposition() {
        let id = TriggerId::new(TriggerSource::Keyboard, 0, 0x60);
        let mut edges = EdgeState::default();
        assert_eq!(edges.record_press(id), None);
        edges.active.insert(id, Action::Talk);
        assert_eq!(edges.record_press(id), Some(true));
    }
}
