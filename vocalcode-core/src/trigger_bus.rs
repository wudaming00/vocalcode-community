//! Bounded, callback-safe delivery for global input events.
//!
//! OS input callbacks must never wait for the engine thread: on macOS that can
//! disable the event tap, and on Windows it can stall input system-wide.  At the
//! same time, losing a release, disconnect or shutdown event can leave capture
//! running.  This queue therefore gives ordinary actions a fixed budget and
//! reserves additional space for controls.  If even that reserve is unavailable
//! (or the queue mutex is momentarily held), controls collapse into one
//! allocation-free atomic pending mask.  A lost release/disconnect becomes the
//! stronger `ForceStop`, while `Cancel` and `Quit` retain their exact meaning.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use crate::traits::TriggerEvent;

/// Maximum ordinary actions waiting for the engine.
pub const TRIGGER_ACTION_QUEUE_CAPACITY: usize = 128;

/// Space kept away from action floods for releases and lifecycle controls.
const TRIGGER_CONTROL_RESERVE: usize = 64;
const TRIGGER_QUEUE_CAPACITY: usize = TRIGGER_ACTION_QUEUE_CAPACITY + TRIGGER_CONTROL_RESERVE;
/// Once half the action budget is waiting, preserving every stale tap is less
/// important than delivering the release/lifecycle control promptly.
const CONTROL_PREEMPT_ACTION_THRESHOLD: usize = TRIGGER_ACTION_QUEUE_CAPACITY / 2;

#[derive(Default)]
struct QueueState {
    events: VecDeque<TriggerEvent>,
    actions: usize,
}

const FORCE_STOP_PENDING: u8 = 1 << 0;
const CANCEL_PENDING: u8 = 1 << 1;
const QUIT_PENDING: u8 = 1 << 2;
const EMERGENCY_PENDING: u8 = FORCE_STOP_PENDING | CANCEL_PENDING | QUIT_PENDING;

/// Serializes the receiver's action-pop commit with emergency publication.
///
/// This is deliberately a bit in the same atomic word as the pending events.
/// A publisher can therefore never slip between a separate "no emergency"
/// check and an action pop: either its `fetch_or` precedes the receiver's
/// commit CAS and the action is retracted, or it follows that CAS and the
/// action was already committed first in the atomic modification order.
const ACTION_POP_CLAIM: u8 = 1 << 3;

#[derive(Default)]
struct EmergencyControls {
    pending: AtomicU8,
}

impl EmergencyControls {
    fn publish(&self, event: TriggerEvent) {
        let pending = match event {
            TriggerEvent::Quit => QUIT_PENDING,
            TriggerEvent::Cancel => CANCEL_PENDING,
            TriggerEvent::TalkReleased(_)
            | TriggerEvent::DeviceDisconnected(_)
            | TriggerEvent::ForceStop => FORCE_STOP_PENDING,
            TriggerEvent::TalkPressed(_)
            | TriggerEvent::SendTapped(_)
            | TriggerEvent::TeachTapped(_) => {
                debug_assert!(false, "ordinary action published as an emergency control");
                return;
            }
        };
        self.pending.fetch_or(pending, Ordering::Release);
    }

    fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire) & EMERGENCY_PENDING != 0
    }

    fn take(&self) -> Option<TriggerEvent> {
        let mut observed = self.pending.load(Ordering::Acquire);
        loop {
            // Privacy and process termination dominate transcription. A
            // remaining lower-priority bit is returned on the next poll.
            let (bit, event) = if observed & QUIT_PENDING != 0 {
                (QUIT_PENDING, TriggerEvent::Quit)
            } else if observed & CANCEL_PENDING != 0 {
                (CANCEL_PENDING, TriggerEvent::Cancel)
            } else if observed & FORCE_STOP_PENDING != 0 {
                (FORCE_STOP_PENDING, TriggerEvent::ForceStop)
            } else {
                return None;
            };
            match self.pending.compare_exchange_weak(
                observed,
                observed & !bit,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(event),
                Err(current) => observed = current,
            }
        }
    }

    fn try_claim_action_pop(&self) -> bool {
        self.pending
            .compare_exchange(0, ACTION_POP_CLAIM, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn try_commit_action_pop(&self) -> bool {
        self.pending
            .compare_exchange(ACTION_POP_CLAIM, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn abandon_action_pop(&self) {
        let previous = self.pending.fetch_and(!ACTION_POP_CLAIM, Ordering::AcqRel);
        debug_assert_ne!(previous & ACTION_POP_CLAIM, 0);
    }
}

struct Shared {
    queue: Mutex<QueueState>,
    wake: Condvar,
    emergency: EmergencyControls,
    receiver_alive: AtomicBool,
    senders: AtomicUsize,
}

/// Cloneable producer used by every global-input backend and lifecycle caller.
pub struct TriggerEventSender {
    shared: Arc<Shared>,
}

impl Clone for TriggerEventSender {
    fn clone(&self) -> Self {
        self.shared.senders.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl Drop for TriggerEventSender {
    fn drop(&mut self) {
        if self.shared.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            // Synchronize with the receiver's check-then-Condvar-wait sequence.
            // Without taking this mutex, the final notify can land after the
            // check but before the wait begins and sleep until the full timeout.
            let _queue = lock_recover(&self.shared.queue);
            self.shared.wake.notify_all();
        }
    }
}

impl std::fmt::Debug for TriggerEventSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TriggerEventSender")
            .finish_non_exhaustive()
    }
}

impl TriggerEventSender {
    /// Attempt delivery without ever waiting for the engine or another callback.
    ///
    /// Ordinary actions return `Full` when saturated/contended. Critical events
    /// instead use the bounded emergency representation and return success.
    pub fn try_send(&self, event: TriggerEvent) -> Result<(), TrySendError<TriggerEvent>> {
        if !self.shared.receiver_alive.load(Ordering::Acquire) {
            return Err(TrySendError::Disconnected(event));
        }
        if is_action(event) {
            self.try_send_action(event)
        } else {
            self.try_send_control(event)
        }
    }

    fn try_send_action(&self, event: TriggerEvent) -> Result<(), TrySendError<TriggerEvent>> {
        if self.shared.emergency.is_pending() {
            return Err(TrySendError::Full(event));
        }
        let mut queue = match self.shared.queue.try_lock() {
            Ok(queue) => queue,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return Err(TrySendError::Full(event)),
        };
        if !self.shared.receiver_alive.load(Ordering::Acquire) {
            return Err(TrySendError::Disconnected(event));
        }
        if queue.actions >= TRIGGER_ACTION_QUEUE_CAPACITY
            || queue.events.len() >= TRIGGER_QUEUE_CAPACITY
            || self.shared.emergency.is_pending()
        {
            return Err(TrySendError::Full(event));
        }
        queue.events.push_back(event);
        queue.actions += 1;
        // If an emergency raced this enqueue, retract the just-added action.
        // No other producer or the receiver can touch the tail while we hold the
        // mutex, so `pop_back` is exactly our event.
        if self.shared.emergency.is_pending() {
            let removed = queue.events.pop_back();
            debug_assert_eq!(removed, Some(event));
            queue.actions -= 1;
            return Err(TrySendError::Full(event));
        }
        drop(queue);
        self.shared.wake.notify_one();
        Ok(())
    }

    fn try_send_control(&self, event: TriggerEvent) -> Result<(), TrySendError<TriggerEvent>> {
        let queue = self.shared.queue.try_lock();
        match queue {
            Ok(mut queue) => {
                if !self.shared.receiver_alive.load(Ordering::Acquire) {
                    return Err(TrySendError::Disconnected(event));
                }
                if should_preempt_actions(event, queue.actions) {
                    discard_actions(&mut queue);
                }
                if queue.events.len() < TRIGGER_QUEUE_CAPACITY {
                    queue.events.push_back(event);
                    drop(queue);
                    self.shared.wake.notify_one();
                    return Ok(());
                }
            }
            Err(TryLockError::Poisoned(mut poisoned)) => {
                let queue = poisoned.get_mut();
                if !self.shared.receiver_alive.load(Ordering::Acquire) {
                    return Err(TrySendError::Disconnected(event));
                }
                if should_preempt_actions(event, queue.actions) {
                    discard_actions(queue);
                }
                if queue.events.len() < TRIGGER_QUEUE_CAPACITY {
                    queue.events.push_back(event);
                    self.shared.wake.notify_one();
                    return Ok(());
                }
            }
            Err(TryLockError::WouldBlock) => {}
        }
        self.shared.emergency.publish(event);
        self.shared.wake.notify_one();
        Ok(())
    }
}

/// Sole consumer for a [`trigger_event_channel`].
pub struct TriggerEventReceiver {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for TriggerEventReceiver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TriggerEventReceiver")
            .finish_non_exhaustive()
    }
}

impl Drop for TriggerEventReceiver {
    fn drop(&mut self) {
        self.shared.receiver_alive.store(false, Ordering::Release);
        self.shared.wake.notify_all();
    }
}

impl TriggerEventReceiver {
    pub fn try_recv(&self) -> Result<TriggerEvent, TryRecvError> {
        let mut queue = lock_recover(&self.shared.queue);
        if let Some(event) = self.pop_ready(&mut queue) {
            return Ok(event);
        }
        if self.shared.senders.load(Ordering::Acquire) == 0 {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<TriggerEvent, RecvTimeoutError> {
        let deadline = Instant::now().checked_add(timeout);
        let mut queue = lock_recover(&self.shared.queue);
        loop {
            if let Some(event) = self.pop_ready(&mut queue) {
                return Ok(event);
            }
            if self.shared.senders.load(Ordering::Acquire) == 0 {
                return Err(RecvTimeoutError::Disconnected);
            }
            let remaining = deadline
                .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
                .ok_or(RecvTimeoutError::Timeout)?;
            let waited = self.shared.wake.wait_timeout(queue, remaining);
            let (next, result) = match waited {
                Ok(value) => value,
                Err(poisoned) => poisoned.into_inner(),
            };
            queue = next;
            if result.timed_out() {
                if let Some(event) = self.pop_ready(&mut queue) {
                    return Ok(event);
                }
                return Err(RecvTimeoutError::Timeout);
            }
        }
    }

    fn pop_ready(&self, queue: &mut QueueState) -> Option<TriggerEvent> {
        self.pop_ready_with_action_hook(queue, || {})
    }

    fn pop_ready_with_action_hook<F>(
        &self,
        queue: &mut QueueState,
        mut before_action_commit: F,
    ) -> Option<TriggerEvent>
    where
        F: FnMut(),
    {
        loop {
            if let Some(event) = self.shared.emergency.take() {
                // An emergency represents a control that could not join the
                // FIFO. Old or racing actions must never replay after it; doing
                // so could restart a latched recording after the fail-safe stop.
                discard_actions(queue);
                return Some(event);
            }

            let event = *queue.events.front()?;
            if !is_action(event) {
                return queue.events.pop_front();
            }

            if !self.shared.emergency.try_claim_action_pop() {
                // The only other writer of ACTION_POP_CLAIM is this sole
                // receiver, so failure means an emergency became pending.
                continue;
            }

            let popped = queue.events.pop_front();
            debug_assert_eq!(popped, Some(event));
            queue.actions -= 1;
            before_action_commit();

            if self.shared.emergency.try_commit_action_pop() {
                return Some(event);
            }

            // A publisher won the atomic race. Restore the tentative pop, drop
            // our claim without disturbing its bits, and let the next loop
            // iteration deliver the highest-priority emergency first.
            queue.events.push_front(event);
            queue.actions += 1;
            self.shared.emergency.abandon_action_pop();
        }
    }
}

/// Construct the process-wide trigger queue.
pub fn trigger_event_channel() -> (TriggerEventSender, TriggerEventReceiver) {
    let shared = Arc::new(Shared {
        queue: Mutex::new(QueueState {
            events: VecDeque::with_capacity(TRIGGER_QUEUE_CAPACITY),
            actions: 0,
        }),
        wake: Condvar::new(),
        emergency: EmergencyControls::default(),
        receiver_alive: AtomicBool::new(true),
        senders: AtomicUsize::new(1),
    });
    (
        TriggerEventSender {
            shared: shared.clone(),
        },
        TriggerEventReceiver { shared },
    )
}

fn is_action(event: TriggerEvent) -> bool {
    matches!(
        event,
        TriggerEvent::TalkPressed(_) | TriggerEvent::SendTapped(_) | TriggerEvent::TeachTapped(_)
    )
}

fn should_preempt_actions(event: TriggerEvent, actions: usize) -> bool {
    matches!(
        event,
        TriggerEvent::ForceStop | TriggerEvent::Cancel | TriggerEvent::Quit
    ) || actions >= CONTROL_PREEMPT_ACTION_THRESHOLD
}

fn discard_actions(queue: &mut QueueState) {
    queue.events.retain(|event| !is_action(*event));
    queue.actions = 0;
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::TriggerId;

    #[test]
    fn action_flood_is_bounded_and_non_blocking() {
        let (sender, receiver) = trigger_event_channel();
        let id = TriggerId::synthetic(7);
        for _ in 0..TRIGGER_ACTION_QUEUE_CAPACITY {
            sender.try_send(TriggerEvent::SendTapped(id)).unwrap();
        }
        assert!(matches!(
            sender.try_send(TriggerEvent::TeachTapped(id)),
            Err(TrySendError::Full(TriggerEvent::TeachTapped(_)))
        ));
        for _ in 0..TRIGGER_ACTION_QUEUE_CAPACITY {
            assert_eq!(receiver.try_recv().unwrap(), TriggerEvent::SendTapped(id));
        }
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn fifo_order_is_preserved_without_saturation() {
        let (sender, receiver) = trigger_event_channel();
        let first = TriggerId::synthetic(1);
        let second = TriggerId::synthetic(2);
        let expected = [
            TriggerEvent::TalkPressed(first),
            TriggerEvent::TalkReleased(first),
            TriggerEvent::SendTapped(second),
            TriggerEvent::DeviceDisconnected(second.device),
        ];
        for event in expected {
            sender.try_send(event).unwrap();
        }
        for event in expected {
            assert_eq!(receiver.try_recv().unwrap(), event);
        }
    }

    #[test]
    fn saturated_actions_are_preempted_by_release_and_quit() {
        let (sender, receiver) = trigger_event_channel();
        let id = TriggerId::synthetic(9);
        for _ in 0..TRIGGER_ACTION_QUEUE_CAPACITY {
            sender.try_send(TriggerEvent::SendTapped(id)).unwrap();
        }
        sender.try_send(TriggerEvent::TalkReleased(id)).unwrap();
        sender.try_send(TriggerEvent::Quit).unwrap();

        assert_eq!(receiver.try_recv().unwrap(), TriggerEvent::TalkReleased(id));
        assert_eq!(receiver.try_recv().unwrap(), TriggerEvent::Quit);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn emergency_control_discards_actions_and_coalesces() {
        let (sender, receiver) = trigger_event_channel();
        let id = TriggerId::synthetic(11);
        // Fill the complete bounded queue with precise controls, forcing later
        // controls into the allocation-free emergency representation.
        for device in 0..TRIGGER_QUEUE_CAPACITY as u64 {
            sender
                .try_send(TriggerEvent::DeviceDisconnected(device))
                .unwrap();
        }
        sender.try_send(TriggerEvent::TalkReleased(id)).unwrap();
        sender.try_send(TriggerEvent::Cancel).unwrap();
        sender.try_send(TriggerEvent::Quit).unwrap();

        assert_eq!(receiver.try_recv().unwrap(), TriggerEvent::Quit);
        assert_eq!(receiver.try_recv().unwrap(), TriggerEvent::Cancel);
        assert_eq!(receiver.try_recv().unwrap(), TriggerEvent::ForceStop);
        // The bounded precise controls that already occupied the queue remain.
        for device in 0..TRIGGER_QUEUE_CAPACITY as u64 {
            assert_eq!(
                receiver.try_recv().unwrap(),
                TriggerEvent::DeviceDisconnected(device)
            );
        }
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn emergency_publication_retracts_an_uncommitted_action_pop() {
        let (sender, receiver) = trigger_event_channel();
        let id = TriggerId::synthetic(13);
        sender
            .try_send(TriggerEvent::SendTapped(id))
            .expect("action should enter an empty queue");

        let shared = receiver.shared.clone();
        let mut queue = lock_recover(&shared.queue);
        let mut hook_ran = false;
        let event = receiver.pop_ready_with_action_hook(&mut queue, || {
            assert!(!hook_ran, "the tentative action pop should occur once");
            hook_ran = true;
            // Deterministically place publication after the action was removed
            // from the deque but before its atomic commit.
            shared.emergency.publish(TriggerEvent::Cancel);
        });

        assert!(hook_ran);
        assert_eq!(event, Some(TriggerEvent::Cancel));
        assert!(
            queue.events.is_empty(),
            "the stale action must be discarded"
        );
        assert_eq!(queue.actions, 0);
        assert!(!shared.emergency.is_pending());
        assert_eq!(shared.emergency.pending.load(Ordering::Acquire), 0);
    }

    #[test]
    fn pending_mask_linearizes_every_emergency_against_action_pop() {
        let emergencies = [
            TriggerEvent::ForceStop,
            TriggerEvent::Cancel,
            TriggerEvent::Quit,
        ];

        for emergency in emergencies {
            // Publication before the claim prevents a tentative pop.
            let controls = EmergencyControls::default();
            controls.publish(emergency);
            assert!(!controls.try_claim_action_pop());
            assert_eq!(controls.take(), Some(emergency));

            // Publication after the claim but before the commit makes the CAS
            // fail, so the receiver must retract the tentative pop.
            let controls = EmergencyControls::default();
            assert!(controls.try_claim_action_pop());
            controls.publish(emergency);
            assert!(!controls.try_commit_action_pop());
            controls.abandon_action_pop();
            assert_eq!(controls.take(), Some(emergency));
            assert_eq!(controls.pending.load(Ordering::Acquire), 0);

            // A successful commit is ordered before a later publication.
            let controls = EmergencyControls::default();
            assert!(controls.try_claim_action_pop());
            assert!(controls.try_commit_action_pop());
            controls.publish(emergency);
            assert_eq!(controls.take(), Some(emergency));
            assert_eq!(controls.pending.load(Ordering::Acquire), 0);
        }
    }

    #[test]
    fn pending_mask_preserves_emergency_priority() {
        let controls = EmergencyControls::default();
        controls.publish(TriggerEvent::ForceStop);
        controls.publish(TriggerEvent::Quit);
        controls.publish(TriggerEvent::Cancel);

        assert_eq!(controls.take(), Some(TriggerEvent::Quit));
        assert_eq!(controls.take(), Some(TriggerEvent::Cancel));
        assert_eq!(controls.take(), Some(TriggerEvent::ForceStop));
        assert_eq!(controls.take(), None);
    }

    #[test]
    fn contended_callback_control_uses_non_blocking_emergency_path() {
        let (sender, receiver) = trigger_event_channel();
        let queue_guard = sender.shared.queue.lock().unwrap();
        // This call would deadlock the test if a callback ever waited on the
        // queue mutex. A release degrades to the stronger bounded ForceStop.
        sender
            .try_send(TriggerEvent::TalkReleased(TriggerId::synthetic(12)))
            .unwrap();
        sender.try_send(TriggerEvent::Quit).unwrap();
        drop(queue_guard);

        assert_eq!(receiver.try_recv().unwrap(), TriggerEvent::Quit);
        assert_eq!(receiver.try_recv().unwrap(), TriggerEvent::ForceStop);
    }

    #[test]
    fn concurrent_action_flood_cannot_starve_shutdown_controls() {
        let (sender, receiver) = trigger_event_channel();
        let mut producers = Vec::new();
        for producer in 0..8 {
            let sender = sender.clone();
            producers.push(std::thread::spawn(move || {
                let id = TriggerId::synthetic(100 + producer);
                for _ in 0..2_000 {
                    let _ = sender.try_send(TriggerEvent::SendTapped(id));
                }
            }));
        }
        for producer in producers {
            producer.join().unwrap();
        }

        let talk = TriggerId::synthetic(999);
        sender.try_send(TriggerEvent::TalkReleased(talk)).unwrap();
        sender.try_send(TriggerEvent::Cancel).unwrap();
        sender.try_send(TriggerEvent::Quit).unwrap();

        assert_eq!(
            receiver.try_recv().unwrap(),
            TriggerEvent::TalkReleased(talk)
        );
        assert_eq!(receiver.try_recv().unwrap(), TriggerEvent::Cancel);
        assert_eq!(receiver.try_recv().unwrap(), TriggerEvent::Quit);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn disconnect_and_timeout_match_standard_channel_semantics() {
        let (sender, receiver) = trigger_event_channel();
        assert_eq!(
            receiver.recv_timeout(Duration::from_millis(1)),
            Err(RecvTimeoutError::Timeout)
        );
        drop(sender);
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(1)),
            Err(RecvTimeoutError::Disconnected)
        );
    }

    #[test]
    fn final_sender_wakes_a_waiting_receiver_without_lost_notification() {
        let (sender, receiver) = trigger_event_channel();
        let started = std::sync::Arc::new(std::sync::Barrier::new(2));
        let waiter_started = started.clone();
        let waiter = std::thread::spawn(move || {
            waiter_started.wait();
            let began = Instant::now();
            let result = receiver.recv_timeout(Duration::from_secs(5));
            (result, began.elapsed())
        });
        started.wait();
        drop(sender);
        let (result, elapsed) = waiter.join().unwrap();
        assert_eq!(result, Err(RecvTimeoutError::Disconnected));
        assert!(
            elapsed < Duration::from_secs(1),
            "last-sender wake was lost"
        );
    }
}
