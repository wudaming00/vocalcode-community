//! VocalCode's own Windows low-level hooks — the delivery layer that replaced
//! `rdev::grab` here.
//!
//! Why not rdev's: its Windows grab resolved the pressed key's *text name*
//! inside `WH_KEYBOARD_LL` — `AttachThreadInput` to whatever thread owns the
//! foreground window, then `GetKeyboardState` and `ToUnicodeEx`, on every key
//! press. When that thread does not cooperate, the callback stalls and the
//! machine's entire keyboard pipeline serialises behind it: about half of
//! 0.5.0 launches began with seconds-to-a-minute of system-wide keyboard
//! deadness (measured; see HANDOFF.md 2026-08-05). Nothing here ever read the
//! name. This layer does the one thing a low-level hook may do: decode the
//! struct it was handed, consult in-process state, return.
//!
//! Contract: [`run`] installs both hooks on the calling thread and blocks in a
//! message loop until shutdown or a supervised restart. The
//! handler receives every key press/release and mouse button edge as an
//! [`EventType`] and answers `true` to consume the event (the OS never
//! delivers it) or `false` to pass it through. Mouse motion and wheel never
//! reach the handler — a 1000 Hz mouse is filtered before any lock is taken.
//!
//! The handler still speaks rdev's `Key`/`Button` vocabulary: persisted
//! bindings are stored as those names, so the VK->Key table is imported from
//! the (vendored) crate rather than re-derived and allowed to drift.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rdev::{windows_key_from_code, Button, EventType};
use windows_sys::Win32::Foundation::{GetLastError, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, KillTimer, PostThreadMessageW, SetTimer,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, MSG,
    MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

type Handler = Box<dyn FnMut(&EventType, bool) -> bool + Send>;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
}

/// `KBDLLHOOKSTRUCT.flags` bit: the event came from `SendInput`, not hardware.
const LLKHF_INJECTED: u32 = 0x10;
/// `MSLLHOOKSTRUCT.flags` bit: same, for mouse events.
const LLMHF_INJECTED: u32 = 0x01;

/// At most one grab at a time. Unlike a `OnceLock<Handler>`, the slot is cleared
/// after unhooking so the supervisor can install a fresh listener without
/// leaving callbacks pointed at a closure whose thread has exited.
static HANDLER: Mutex<Option<Handler>> = Mutex::new(None);

/// A single callback overrunning `LowLevelHooksTimeout` (300 ms default) is
/// how the old backend froze the machine's keyboard. Self-report long before
/// that line so a regression is visible in the log instead of in the user's
/// stalled keystrokes. The warning itself is I/O inside the hook, so it is
/// rate-limited to one per second — a healthy hook never pays it at all.
const SLOW_CALLBACK_WARN_MS: u128 = 20;
const CALLBACK_RENEW_MS: u128 = 200;
const HEARTBEAT_INTERVAL_MS: u32 = 1_000;
const HEARTBEAT_STALL_TICKS: u32 = 4;
static RENEW_REQUESTED: AtomicBool = AtomicBool::new(false);
static HOOK_HEARTBEAT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct HeartbeatWatchdog {
    last: u64,
    stagnant_ticks: u32,
}

impl HeartbeatWatchdog {
    fn new(initial: u64) -> Self {
        Self {
            last: initial,
            stagnant_ticks: 0,
        }
    }

    fn observe(&mut self, current: u64) -> bool {
        if current != self.last {
            self.last = current;
            self.stagnant_ticks = 0;
            false
        } else {
            self.stagnant_ticks = self.stagnant_ticks.saturating_add(1);
            self.stagnant_ticks >= HEARTBEAT_STALL_TICKS
        }
    }
}

fn warn_slow(spent_ms: u128) {
    static STARTED: OnceLock<Instant> = OnceLock::new();
    static LAST_WARN_MS: AtomicU64 = AtomicU64::new(0);
    let now = STARTED.get_or_init(Instant::now).elapsed().as_millis() as u64;
    let last = LAST_WARN_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= 1_000
        && LAST_WARN_MS
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        crate::hotkey::hook_diagnostic(crate::hotkey::HookDiagnostic::WindowsSlowCallback(
            spent_ms,
        ));
    }
}

fn dispatch(event: &EventType, injected: bool) -> bool {
    // Callback activity is also proof of liveness. WM_TIMER is deliberately
    // low priority and may be delayed by a sustained input stream; counting
    // both avoids treating a busy, healthy hook as stalled.
    HOOK_HEARTBEAT.fetch_add(1, Ordering::Release);
    let Ok(mut handler) = HANDLER.lock() else {
        return false;
    };
    let Some(handler) = handler.as_mut() else {
        return false;
    };
    let t0 = Instant::now();
    let consumed = handler(event, injected);
    let spent = t0.elapsed().as_millis();
    if spent >= SLOW_CALLBACK_WARN_MS {
        warn_slow(spent);
    }
    if spent >= CALLBACK_RENEW_MS {
        // Windows may silently remove a low-level hook when a callback nears
        // LowLevelHooksTimeout. Ask the message loop to retire this pair while
        // it is still observable; the outer supervisor installs a fresh pair.
        RENEW_REQUESTED.store(true, Ordering::Release);
    }
    consumed
}

/// Diagnostic gate for the first thing [`keyboard_proc`] does. "No key in the
/// log" has two very different causes — Windows never calling this procedure,
/// or this procedure running and something below it dropping the event — and
/// every other log sits below the decode and the lock, so none of them can tell
/// those apart. This one can, and it is how the Chromium focus behaviour was
/// pinned down. `VOCALCODE_TRACE_HOOKPROC=1`.
#[cfg(any(test, debug_assertions))]
static TRACE_PROC: OnceLock<bool> = OnceLock::new();

#[cfg(any(test, debug_assertions))]
fn trace_proc() -> bool {
    *TRACE_PROC.get_or_init(|| std::env::var_os("VOCALCODE_TRACE_HOOKPROC").is_some())
}

#[cfg(not(any(test, debug_assertions)))]
#[inline(always)]
fn trace_proc() -> bool {
    false
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == 0 {
        let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
        if trace_proc() {
            crate::hotkey::hook_diagnostic(crate::hotkey::HookDiagnostic::WindowsHookProc {
                wparam,
                vk: kb.vkCode,
            });
        }
        let key = windows_key_from_code(kb.vkCode as u16);
        // A synthetic keystroke (SendInput) is a tool talking — our own text
        // injection, a paste chord, another dictation program. The decision
        // layer must be able to tell it from a hand on the keyboard.
        let injected = kb.flags & LLKHF_INJECTED != 0;
        let event = match wparam as u32 {
            WM_KEYDOWN | WM_SYSKEYDOWN => Some(EventType::KeyPress(key)),
            WM_KEYUP | WM_SYSKEYUP => Some(EventType::KeyRelease(key)),
            _ => None,
        };
        if let Some(event) = event {
            if dispatch(&event, injected) {
                return 1;
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == 0 {
        // Motion and wheel fall through without touching the handler or its
        // lock; only button edges are worth a dispatch.
        let mouse = &*(lparam as *const MSLLHOOKSTRUCT);
        let injected = mouse.flags & LLMHF_INJECTED != 0;
        let event = match wparam as u32 {
            WM_LBUTTONDOWN => Some(EventType::ButtonPress(Button::Left)),
            WM_LBUTTONUP => Some(EventType::ButtonRelease(Button::Left)),
            WM_RBUTTONDOWN => Some(EventType::ButtonPress(Button::Right)),
            WM_RBUTTONUP => Some(EventType::ButtonRelease(Button::Right)),
            WM_MBUTTONDOWN => Some(EventType::ButtonPress(Button::Middle)),
            WM_MBUTTONUP => Some(EventType::ButtonRelease(Button::Middle)),
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                // X1/X2 arrive in the high word; the same numbering rdev used,
                // so persisted mouse_x1/mouse_x2 bindings keep their meaning.
                let button = Button::Unknown((mouse.mouseData >> 16) as u8);
                if wparam as u32 == WM_XBUTTONDOWN {
                    Some(EventType::ButtonPress(button))
                } else {
                    Some(EventType::ButtonRelease(button))
                }
            }
            _ => None,
        };
        if let Some(event) = event {
            if dispatch(&event, injected) {
                return 1;
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

/// Install both hooks on the calling thread and pump messages until shutdown
/// or a health failure asks the supervisor to reinstall them.
pub(crate) fn run(
    handler: impl FnMut(&EventType, bool) -> bool + Send + 'static,
    installed: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), String> {
    // Resolve the debug-only environment opt-in before Windows can enter the
    // callback. Callback code may read the OnceLock, never initialize it.
    let _ = trace_proc();
    {
        let mut slot = HANDLER
            .lock()
            .map_err(|_| "input hook handler lock was poisoned".to_string())?;
        if slot.is_some() {
            return Err("input hooks are already installed in this process".into());
        }
        *slot = Some(Box::new(handler));
    }
    struct HandlerGuard;
    impl Drop for HandlerGuard {
        fn drop(&mut self) {
            if let Ok(mut slot) = HANDLER.lock() {
                *slot = None;
            }
        }
    }
    let _handler_guard = HandlerGuard;
    struct HookGuard {
        keyboard: HHOOK,
        mouse: HHOOK,
        heartbeat_timer: usize,
        installed: Arc<AtomicBool>,
        ready: Arc<AtomicBool>,
        watchdog_done: Arc<AtomicBool>,
        watchdog: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for HookGuard {
        fn drop(&mut self) {
            self.watchdog_done.store(true, Ordering::Release);
            self.installed.store(false, Ordering::Release);
            self.ready.store(false, Ordering::Release);
            unsafe {
                if self.heartbeat_timer != 0 {
                    let _ = KillTimer(std::ptr::null_mut(), self.heartbeat_timer);
                }
                if !self.mouse.is_null() {
                    let _ = UnhookWindowsHookEx(self.mouse);
                }
                if !self.keyboard.is_null() {
                    let _ = UnhookWindowsHookEx(self.keyboard);
                }
            }
            if let Some(watchdog) = self.watchdog.take() {
                let _ = watchdog.join();
            }
        }
    }

    unsafe {
        let keyboard =
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), std::ptr::null_mut(), 0);
        if keyboard.is_null() {
            return Err("SetWindowsHookExW(WH_KEYBOARD_LL) failed".into());
        }
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), std::ptr::null_mut(), 0);
        if mouse.is_null() {
            let _ = UnhookWindowsHookEx(keyboard);
            return Err("SetWindowsHookExW(WH_MOUSE_LL) failed".into());
        }
        // A thread timer makes the message pump observable even on an idle
        // desktop. It does not claim keyboard activity; it proves that the
        // thread responsible for delivering hook callbacks is still pumping.
        let heartbeat_timer = SetTimer(std::ptr::null_mut(), 0, HEARTBEAT_INTERVAL_MS, None);
        if heartbeat_timer == 0 {
            let _ = UnhookWindowsHookEx(mouse);
            let _ = UnhookWindowsHookEx(keyboard);
            return Err("could not create the input hook heartbeat timer".into());
        }
        HOOK_HEARTBEAT.fetch_add(1, Ordering::Release);
        let watchdog_done = Arc::new(AtomicBool::new(false));
        let mut guard = HookGuard {
            keyboard,
            mouse,
            heartbeat_timer,
            installed: installed.clone(),
            ready: ready.clone(),
            watchdog_done: watchdog_done.clone(),
            watchdog: None,
        };
        let hook_thread_id = GetCurrentThreadId();
        let watch_installed = installed.clone();
        let watch_ready = ready.clone();
        let watch_shutdown = shutdown.clone();
        guard.watchdog = Some(
            std::thread::Builder::new()
                .name("vocalcode-hook-watchdog".to_string())
                .spawn(move || {
                    let mut health = HeartbeatWatchdog::new(HOOK_HEARTBEAT.load(Ordering::Acquire));
                    let mut restart_requested = false;
                    loop {
                        std::thread::sleep(Duration::from_millis(u64::from(HEARTBEAT_INTERVAL_MS)));
                        if watchdog_done.load(Ordering::Acquire) {
                            break;
                        }
                        if watch_shutdown.load(Ordering::Acquire) {
                            if PostThreadMessageW(hook_thread_id, WM_QUIT, 0, 0) != 0 {
                                break;
                            }
                            log::error!(
                                "could not wake the input hook for shutdown: {}; retrying",
                                GetLastError()
                            );
                            continue;
                        }
                        if !restart_requested
                            && health.observe(HOOK_HEARTBEAT.load(Ordering::Acquire))
                        {
                            watch_installed.store(false, Ordering::Release);
                            watch_ready.store(false, Ordering::Release);
                            log::error!("input hook message-loop heartbeat stalled");
                            restart_requested = true;
                        }
                        if restart_requested {
                            if PostThreadMessageW(hook_thread_id, WM_QUIT, 0, 0) != 0 {
                                break;
                            }
                            log::error!(
                                "could not wake the stalled input hook thread: {}; retrying",
                                GetLastError()
                            );
                        }
                    }
                })
                .map_err(|error| format!("could not start input hook watchdog: {error}"))?,
        );
        let _guard = guard;
        RENEW_REQUESTED.store(false, Ordering::Release);
        installed.store(true, Ordering::Release);
        log::info!("input hooks installed (own WH_KEYBOARD_LL / WH_MOUSE_LL, no name resolution)");
        // Low-level hook callbacks are delivered while this thread waits in
        // GetMessageW. The watchdog posts WM_QUIT when shutdown is requested,
        // allowing HookGuard to unhook and the owning supervisor to join.
        let mut message: MSG = std::mem::zeroed();
        loop {
            let result = GetMessageW(&mut message, std::ptr::null_mut(), 0, 0);
            if result == -1 {
                return Err(format!("input hook GetMessageW failed: {}", GetLastError()));
            }
            if result == 0 {
                if shutdown.load(Ordering::Acquire) {
                    return Ok(());
                }
                return Err("input hook message loop exited unexpectedly".into());
            }
            if heartbeat_timer != 0
                && message.message == WM_TIMER
                && message.wParam == heartbeat_timer
            {
                HOOK_HEARTBEAT.fetch_add(1, Ordering::Release);
                log::debug!("input hook message-loop heartbeat");
                if RENEW_REQUESTED.swap(false, Ordering::AcqRel) {
                    return Err("input hook renewal requested after a slow callback".into());
                }
                continue;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_heartbeat_reaches_the_restart_threshold() {
        let mut watchdog = HeartbeatWatchdog::new(7);
        for _ in 1..HEARTBEAT_STALL_TICKS {
            assert!(!watchdog.observe(7));
        }
        assert!(watchdog.observe(7));
    }

    #[test]
    fn advancing_heartbeat_never_triggers_a_restart() {
        let mut watchdog = HeartbeatWatchdog::new(1);
        for heartbeat in 2..100 {
            assert!(!watchdog.observe(heartbeat));
        }
    }
}
