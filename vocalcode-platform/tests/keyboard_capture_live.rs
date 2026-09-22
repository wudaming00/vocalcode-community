//! Does a real keyboard key, pressed while a capture is open, actually bind?
//!
//! Three rounds of diagnosis argued about that question from log archaeology,
//! and all three were wrong. It is answerable in three seconds: install the
//! real hook, open a capture, synthesise a key, read the result.
//!
//! `#[ignore]`d because it installs a *global* low-level hook and injects
//! input, so it fights anything else holding a grab (a running VocalCode, the
//! pre-rename Wilco build) and it types into whatever has focus if it fails.
//! Run it deliberately:
//!
//! ```text
//! cargo test -p vocalcode-platform --test keyboard_capture_live -- --ignored --nocapture
//! ```

#![cfg(windows)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use vocalcode_core::traits::HotkeyListener;
use vocalcode_core::trigger_event_channel;
use vocalcode_platform::hotkey::{CaptureShared, RdevHotkey, SharedTriggers};

/// Give the hook a moment to be installed before injecting anything: the grab
/// is set up on its own thread and a key sent before that is simply missed.
const HOOK_SETTLE: Duration = Duration::from_millis(600);
const ANSWER_TIMEOUT: Duration = Duration::from_secs(3);

fn wait_for_answer(capture: &CaptureShared) -> Option<(String, String)> {
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(answer) = capture.take_result() {
            return Some(answer);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

fn tap(key: rdev::Key) {
    rdev::simulate(&rdev::EventType::KeyPress(key)).expect("inject key press");
    std::thread::sleep(Duration::from_millis(40));
    rdev::simulate(&rdev::EventType::KeyRelease(key)).expect("inject key release");
    std::thread::sleep(Duration::from_millis(40));
}

#[test]
#[ignore = "installs a global hook and injects input; run deliberately"]
fn a_keyboard_key_pressed_during_a_capture_binds() {
    let triggers = SharedTriggers::default();
    let capture = Arc::new(CaptureShared::default());
    // Keep the receiver alive: the hook marks itself not-ready when a send
    // fails, and a dropped receiver would take the listener down mid-test.
    let (tx, _rx) = trigger_event_channel();

    let listener = Box::new(RdevHotkey::new(triggers, capture.clone()));
    std::thread::spawn(move || {
        let _ = listener.run(tx, Arc::new(std::sync::atomic::AtomicBool::new(false)));
    });
    std::thread::sleep(HOOK_SETTLE);

    // A bindable non-modifier key. `rdev::simulate` is SendInput, so it
    // arrives with LLKHF_INJECTED set — which a capture accepts for exactly
    // this class of key (remapping software and this test both press keys
    // that way) and for nothing else.
    capture.start("talk");
    tap(rdev::Key::F7);
    assert_eq!(
        wait_for_answer(&capture),
        Some(("talk".to_string(), "key:F7".to_string())),
        "F7 did not bind — the hook never delivered it, or the capture was already spent"
    );

    // An injected MODIFIER is a tool's chord (paste sends Ctrl this way), not
    // a hand offering a binding: it must be ignored outright, and the prompt
    // must still be waiting for the next real answer.
    capture.start("talk");
    tap(rdev::Key::ControlRight);
    assert_eq!(
        wait_for_answer(&capture),
        None,
        "an injected modifier must not become a candidate, bind, or hint"
    );
    tap(rdev::Key::F7);
    assert_eq!(
        wait_for_answer(&capture),
        Some(("talk".to_string(), "key:F7".to_string())),
        "the injected modifier spent the capture instead of being ignored"
    );

    // And the case this whole investigation was actually about: something the
    // prompt cannot bind must NOT end it. An injected letter is a tool typing
    // (dictation), so it is ignored in silence — physical letters hinting
    // `typing_key` is covered by the unit tests, where the injected flag can
    // be false.
    capture.start("talk");
    tap(rdev::Key::KeyA);
    assert_eq!(
        wait_for_answer(&capture),
        None,
        "an injected letter must be ignored without a hint"
    );
    // No `is_capturing` check here — it is crate-private, and the behaviour is
    // what matters anyway: if the letter ended the capture, F7 now answers
    // nothing at all, which is exactly the silence the owner reported.
    tap(rdev::Key::F7);
    assert_eq!(
        wait_for_answer(&capture),
        Some(("talk".to_string(), "key:F7".to_string())),
        "the letter ENDED the capture, so the real key that followed it went \
         nowhere — this is the defect the fix was for"
    );
}

/// Does a BOUND key actually get swallowed before Windows acts on it?
///
/// The capture path proves delivery; this proves *suppression* — the half no
/// unit test can see, because the consume decision's effect lives in the OS.
/// CapsLock is the perfect witness: if the hook's "consume" return does not
/// stop the event, the system caps state flips, and `GetKeyState` says so.
///
/// If this test FAILS it may leave the machine's caps state toggled.
#[test]
#[ignore = "installs a global hook and injects input; run deliberately"]
fn a_bound_capslock_is_swallowed_before_windows_toggles_caps() {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CAPITAL};
    let caps_on = || unsafe { (GetKeyState(VK_CAPITAL as i32) & 1) != 0 };

    let triggers: SharedTriggers = Arc::new(std::sync::Mutex::new((
        vec![vocalcode_core::config::Trigger::Key("CapsLock".into())],
        Vec::new(),
        Vec::new(),
    )));
    let capture = Arc::new(CaptureShared::default());
    let (tx, rx) = trigger_event_channel();
    let listener = Box::new(RdevHotkey::new(triggers, capture));
    std::thread::spawn(move || {
        let _ = listener.run(tx, Arc::new(std::sync::atomic::AtomicBool::new(false)));
    });
    std::thread::sleep(HOOK_SETTLE);

    let before = caps_on();
    tap(rdev::Key::CapsLock);
    std::thread::sleep(Duration::from_millis(300));
    let after = caps_on();

    // The engine must have heard the press…
    let heard = std::iter::from_fn(|| rx.try_recv().ok())
        .any(|ev| matches!(ev, vocalcode_core::traits::TriggerEvent::TalkPressed(_)));
    assert!(heard, "the bound CapsLock never fired TalkPressed");
    // …and Windows must not have: a consumed key cannot toggle caps.
    assert_eq!(
        before, after,
        "the bound CapsLock LEAKED: the hook consumed it for talk, yet the \
         system caps state still toggled — suppression is broken"
    );
}
