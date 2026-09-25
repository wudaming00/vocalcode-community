//! Small, generation-bound UI-to-engine mailbox. A floating control is not a
//! second audio engine and cannot replay a stale click after transcription.
use crate::overlay::{Phase, Snapshot};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use vocalcode_core::traits::{TriggerEvent, TriggerId};
use vocalcode_core::TriggerEventSender;

pub const REQUEST_TTL: Duration = Duration::from_millis(750);
const CONTROL_ID: TriggerId = TriggerId::synthetic(0x444f_434b);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// The engine-side consumer is shared, but the production click producer is
// Windows-only until a native macOS control surface has its own verification.
#[cfg_attr(not(windows), allow(dead_code))]
pub enum Action {
    Start,
    Stop,
    Cancel,
}

#[derive(Debug, Clone, Copy)]
struct Request {
    action: Action,
    snapshot: Snapshot,
    at: Instant,
}

fn permitted(action: Action, phase: Phase, ready: bool, recording: bool) -> bool {
    match action {
        Action::Start => ready && !recording && matches!(phase, Phase::Idle | Phase::Learning),
        Action::Stop | Action::Cancel => recording && phase == Phase::Recording,
    }
}

#[derive(Default)]
pub struct Bridge {
    sender: Mutex<Option<TriggerEventSender>>,
    pending: Mutex<Option<Request>>,
    busy: AtomicBool,
    error: Mutex<Option<&'static str>>,
    recovery: Mutex<Option<Instant>>,
}

impl Bridge {
    pub fn connect(&self, sender: TriggerEventSender) {
        *self.sender.lock().unwrap_or_else(|e| e.into_inner()) = Some(sender);
    }

    #[cfg(any(windows, test))]
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    #[cfg(any(windows, test))]
    pub fn submit(
        &self,
        action: Action,
        snapshot: Snapshot,
        ready: bool,
        recording: bool,
        now: Instant,
    ) -> Result<(), &'static str> {
        if !permitted(action, snapshot.phase, ready, recording) {
            return Err("That control is no longer available. Please try again.");
        }
        let sender = self
            .sender
            .try_lock()
            .map_err(|_| "Controls are busy. Please try again.")?;
        let sender = sender.as_ref().ok_or("Dictation is not running yet.")?;
        let mut pending = self
            .pending
            .try_lock()
            .map_err(|_| "Controls are busy. Please try again.")?;
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("Please wait for the previous action.");
        }
        *pending = Some(Request {
            action,
            snapshot,
            at: now,
        });
        match sender.try_send(TriggerEvent::Wake) {
            Ok(()) => Ok(()),
            Err(error) => {
                *pending = None;
                self.busy.store(false, Ordering::Release);
                Err(match error {
                    std::sync::mpsc::TrySendError::Full(_) => {
                        "Controls are busy. Please try again."
                    }
                    std::sync::mpsc::TrySendError::Disconnected(_) => {
                        "Dictation stopped. Open VocalCode to check its status."
                    }
                })
            }
        }
    }

    /// A mailbox request has authority only after its Wake passes the shared
    /// input queue. Lifecycle controls can discard that Wake, so they must
    /// discard the mailbox too, even when cancelling an already-idle engine.
    pub fn observe_control(&self, event: TriggerEvent) {
        if matches!(
            event,
            TriggerEvent::Cancel
                | TriggerEvent::Quit
                | TriggerEvent::ForceStop
                | TriggerEvent::DeviceDisconnected(_)
        ) {
            self.discard_pending();
        }
    }

    pub fn discard_pending(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if pending.take().is_some() {
            self.busy.store(false, Ordering::Release);
            *self.error.lock().unwrap_or_else(|e| e.into_inner()) =
                Some("The recording state changed. Please try again.");
        }
    }

    pub fn take(
        &self,
        current: Snapshot,
        ready: bool,
        recording: bool,
        enabled: bool,
        now: Instant,
    ) -> Option<Dispatch<'_>> {
        // Wake has already been consumed from the ordered input queue. Its
        // producer can still own this mutex for the few instructions between
        // enqueueing Wake and returning from submit. A try_lock here loses the
        // only wake-up and leaves busy latched forever. Wait for this strictly
        // bounded critical section; it performs no I/O or callbacks and never
        // waits for the engine. No queue lock is held by this consumer.
        let wait_started = Instant::now();
        let request = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()?;
        let fresh = now
            .checked_add(wait_started.elapsed())
            .and_then(|checked_at| checked_at.checked_duration_since(request.at))
            .is_some_and(|age| age <= REQUEST_TTL);
        // Stop/cancel remain possible if the preference was disabled while a
        // recording was already active. Disabling never authorizes a start.
        if !fresh
            || request.snapshot != current
            || (!enabled && request.action == Action::Start)
            || !permitted(request.action, current.phase, ready, recording)
        {
            self.busy.store(false, Ordering::Release);
            *self.error.lock().unwrap_or_else(|e| e.into_inner()) =
                Some("The recording state changed. Please try again.");
            return None;
        }
        let event = match request.action {
            Action::Start => TriggerEvent::HandsFreeStart(CONTROL_ID),
            Action::Stop => TriggerEvent::ForceStop,
            Action::Cancel => TriggerEvent::Cancel,
        };
        Some(Dispatch {
            event,
            bridge: self,
        })
    }

    #[cfg(any(windows, test))]
    pub fn take_error(&self) -> Option<&'static str> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// UI notification only: the transcript stays in the existing history,
    /// never in the control surface's IPC or state. This does not retry input.
    pub fn notify_recovery(&self, now: Instant) {
        *self.recovery.lock().unwrap_or_else(|e| e.into_inner()) = Some(now);
    }

    #[cfg(any(windows, test))]
    pub fn take_recent_recovery(&self, now: Instant) -> bool {
        self.recovery
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .and_then(|at| now.checked_duration_since(at))
            .is_some_and(|age| age <= Duration::from_secs(12))
    }
}

pub struct Dispatch<'a> {
    pub event: TriggerEvent,
    bridge: &'a Bridge,
}
impl Drop for Dispatch<'_> {
    fn drop(&mut self) {
        self.bridge.busy.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_notice_is_recent_single_use_and_has_no_recording_authority() {
        let b = Bridge::default();
        let now = Instant::now();
        b.notify_recovery(now);
        assert!(b.take_recent_recovery(now));
        assert!(!b.take_recent_recovery(now));
        assert!(!b.is_busy());
        b.notify_recovery(now);
        assert!(!b.take_recent_recovery(now + Duration::from_secs(13)));
        b.notify_recovery(now);
        assert!(!b.take_recent_recovery(now - Duration::from_millis(1)));
    }
    #[test]
    fn lifecycle_controls_discard_the_wake_and_cannot_restart_an_idle_engine() {
        for event in [
            TriggerEvent::Cancel,
            TriggerEvent::Quit,
            TriggerEvent::ForceStop,
        ] {
            let (b, rx) = connected();
            let now = Instant::now();
            let s = snapshot(0, Phase::Idle);
            b.submit(Action::Start, s, true, false, now).unwrap();
            b.sender
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .try_send(event)
                .unwrap();
            assert_eq!(rx.try_recv().unwrap(), event);
            b.observe_control(event);
            assert!(!b.is_busy());
            assert!(b.take(s, true, false, true, now).is_none());
            assert!(rx.try_recv().is_err());
        }
    }
    #[test]
    fn a_full_input_queue_never_accepts_an_unordered_start() {
        let (b, rx) = connected();
        let now = Instant::now();
        let s = snapshot(0, Phase::Idle);
        let sender = b.sender.lock().unwrap().as_ref().unwrap().clone();
        for _ in 0..vocalcode_core::trigger_bus::TRIGGER_ACTION_QUEUE_CAPACITY {
            sender.try_send(TriggerEvent::Wake).unwrap();
        }
        assert!(b.submit(Action::Start, s, true, false, now).is_err());
        assert!(!b.is_busy());
        assert!(b.take(s, true, false, true, now).is_none());
        while rx.try_recv().is_ok() {}
        b.submit(Action::Start, s, true, false, now).unwrap();
        assert_eq!(rx.try_recv().unwrap(), TriggerEvent::Wake);
        assert!(b.take(s, true, false, true, now).is_some());
    }
    #[test]
    fn busy_window_drain_discards_only_waiting_requests_not_an_active_dispatch_guard() {
        let (b, _rx) = connected();
        let now = Instant::now();
        let s = snapshot(0, Phase::Idle);
        b.submit(Action::Start, s, true, false, now).unwrap();
        b.discard_pending();
        assert!(!b.is_busy());
        assert!(b.take(s, true, false, true, now).is_none());
        b.submit(Action::Start, s, true, false, now).unwrap();
        let dispatch = b.take(s, true, false, true, now).unwrap();
        b.discard_pending();
        assert!(b.is_busy());
        drop(dispatch);
        assert!(!b.is_busy());
    }
    fn connected() -> (Bridge, vocalcode_core::TriggerEventReceiver) {
        let (tx, rx) = vocalcode_core::trigger_event_channel();
        let bridge = Bridge::default();
        bridge.connect(tx);
        (bridge, rx)
    }
    fn snapshot(revision: u64, phase: Phase) -> Snapshot {
        Snapshot { revision, phase }
    }

    #[test]
    fn explicit_start_wakes_but_only_dispatch_starts_capture() {
        let (b, rx) = connected();
        let now = Instant::now();
        let s = snapshot(0, Phase::Idle);
        b.submit(Action::Start, s, true, false, now).unwrap();
        assert_eq!(rx.try_recv().unwrap(), TriggerEvent::Wake);
        let d = b.take(s, true, false, true, now).unwrap();
        assert_eq!(d.event, TriggerEvent::HandsFreeStart(CONTROL_ID));
        assert!(b.is_busy());
        drop(d);
        assert!(!b.is_busy());
    }

    #[test]
    fn waking_before_the_producer_unlocks_cannot_lose_the_only_request() {
        let (b, queue) = connected();
        let now = Instant::now();
        let s = snapshot(0, Phase::Idle);
        b.submit(Action::Start, s, true, false, now).unwrap();
        assert_eq!(queue.try_recv().unwrap(), TriggerEvent::Wake);
        let held = b.pending.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let event = b.take(s, true, false, true, now).map(|d| d.event);
                tx.send(event).unwrap();
            });
            // The old try_lock implementation immediately returned None and
            // never received another Wake. The consumer must wait, not drop it.
            assert!(matches!(
                rx.recv_timeout(Duration::from_millis(30)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            drop(held);
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(10)).unwrap(),
                Some(TriggerEvent::HandsFreeStart(CONTROL_ID))
            );
        });
        assert!(!b.is_busy());
    }
    #[test]
    fn duplicate_clicks_have_one_slot_and_do_not_toggle() {
        let (b, _rx) = connected();
        let now = Instant::now();
        let s = snapshot(0, Phase::Idle);
        b.submit(Action::Start, s, true, false, now).unwrap();
        assert!(b.submit(Action::Start, s, true, false, now).is_err());
        let d = b.take(s, true, false, true, now).unwrap();
        assert!(b.submit(Action::Start, s, true, false, now).is_err());
        drop(d);
        assert!(b.take(s, true, false, true, now).is_none());
    }
    #[test]
    fn stale_idle_start_does_not_begin_a_later_session() {
        let (b, _rx) = connected();
        let now = Instant::now();
        let old = snapshot(2, Phase::Idle);
        b.submit(Action::Start, old, true, false, now).unwrap();
        assert!(b
            .take(snapshot(5, Phase::Idle), true, false, true, now)
            .is_none());
        assert!(!b.is_busy());
        assert!(b.take_error().is_some());
    }
    #[test]
    fn stale_stop_and_cancel_cannot_control_next_recording() {
        for action in [Action::Stop, Action::Cancel] {
            let (b, _rx) = connected();
            let now = Instant::now();
            b.submit(action, snapshot(3, Phase::Recording), true, true, now)
                .unwrap();
            assert!(b
                .take(snapshot(6, Phase::Recording), true, true, true, now)
                .is_none());
        }
    }
    #[test]
    fn expired_requests_are_dropped_not_replayed_after_a_busy_worker() {
        let (b, _rx) = connected();
        let now = Instant::now();
        let s = snapshot(0, Phase::Idle);
        b.submit(Action::Start, s, true, false, now).unwrap();
        assert!(b
            .take(
                s,
                true,
                false,
                true,
                now + REQUEST_TTL + Duration::from_nanos(1)
            )
            .is_none());
        assert!(!b.is_busy());
    }
    #[test]
    fn not_ready_disabled_shutdown_and_inconsistent_states_do_not_start() {
        let s = snapshot(0, Phase::Idle);
        let now = Instant::now();
        for (ready, recording, enabled) in [
            (false, false, true),
            (true, true, true),
            (true, false, false),
        ] {
            let (b, _rx) = connected();
            b.submit(Action::Start, s, true, false, now).unwrap();
            assert!(b.take(s, ready, recording, enabled, now).is_none());
        }
        let (b, rx) = connected();
        drop(rx);
        assert!(b.submit(Action::Start, s, true, false, now).is_err());
        assert!(!b.is_busy());
        let (b, _rx) = connected();
        assert!(b.submit(Action::Start, s, false, false, now).is_err());
        assert!(b
            .submit(
                Action::Start,
                snapshot(1, Phase::Transcribing),
                true,
                false,
                now
            )
            .is_err());
    }
    #[test]
    fn stop_and_cancel_still_work_if_readiness_or_preference_changed() {
        for (action, event) in [
            (Action::Stop, TriggerEvent::ForceStop),
            (Action::Cancel, TriggerEvent::Cancel),
        ] {
            let (b, _rx) = connected();
            let now = Instant::now();
            let s = snapshot(4, Phase::Recording);
            b.submit(action, s, false, true, now).unwrap();
            assert_eq!(b.take(s, false, true, false, now).unwrap().event, event);
        }
    }
}
