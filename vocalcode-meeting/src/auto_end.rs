//! Silence-based auto-end policy. The caller supplies a monotonic clock and
//! fresh acoustic observations, never a window title or an ASR text guess.
use serde::Serialize;

pub const COUNTDOWN_MS: u64 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Visible,
    Continue,
    Disable,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notice {
    pub id: u64,
    pub idle_minutes: u64,
    pub remaining_seconds: u64,
}

pub struct AutoEnd {
    idle_ms: u64,
    last_activity: u64,
    last_tick: u64,
    pending: Option<(u64, Option<u64>)>,
    next_id: u64,
}

impl AutoEnd {
    pub fn new(minutes: u64, first_id: u64) -> Self {
        Self {
            idle_ms: match minutes {
                5 | 10 | 15 => minutes * 60_000,
                _ => 0,
            },
            last_activity: 0,
            last_tick: 0,
            pending: None,
            next_id: first_id,
        }
    }

    pub fn tick(&mut self, now: u64, speech: bool, healthy: bool) -> bool {
        let gap = now.saturating_sub(self.last_tick);
        self.last_tick = now;
        // Suspend/resume, missing capture, or a failed detector is not silence.
        if self.idle_ms == 0 || speech || !healthy || gap > 5_000 {
            self.last_activity = now;
            self.pending = None;
            return false;
        }
        if let Some((_, Some(shown))) = self.pending {
            return now.saturating_sub(shown) >= COUNTDOWN_MS;
        }
        if self.pending.is_none() && now.saturating_sub(self.last_activity) >= self.idle_ms {
            self.pending = Some((self.next_id, None));
            self.next_id += 1;
        }
        false
    }

    pub fn notice(&self, now: u64) -> Option<Notice> {
        self.pending.map(|(id, shown)| Notice {
            id,
            idle_minutes: self.idle_ms / 60_000,
            remaining_seconds: COUNTDOWN_MS
                .saturating_sub(shown.map_or(0, |t| now.saturating_sub(t)))
                .div_ceil(1_000),
        })
    }

    /// Token-bound: a late button from an older countdown cannot affect a new
    /// one. A missing popup acknowledgement never starts the stop timer.
    pub fn action(&mut self, id: u64, action: Action, now: u64) -> bool {
        let Some((expected, shown)) = self.pending.as_mut() else {
            return false;
        };
        if *expected != id {
            return false;
        }
        match action {
            Action::Visible => {
                shown.get_or_insert(now);
                false
            }
            Action::Stop => true,
            Action::Continue | Action::Disable => {
                if action == Action::Disable {
                    self.idle_ms = 0;
                }
                self.last_activity = now;
                self.pending = None;
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn advance(policy: &mut AutoEnd, from: u64, to: u64) -> bool {
        (from..=to)
            .step_by(1_000)
            .any(|now| policy.tick(now, false, true))
    }
    #[test]
    fn stops_only_after_five_minutes_and_acknowledged_countdown() {
        let mut p = AutoEnd::new(5, 11);
        assert!(!advance(&mut p, 0, 300_000));
        let n = p.notice(300_000).unwrap();
        assert_eq!(n.remaining_seconds, 30);
        assert!(!advance(&mut p, 301_000, 600_000)); // failed/hidden UI cannot end capture
        assert!(!p.action(n.id, Action::Visible, 600_000));
        assert!(!advance(&mut p, 601_000, 629_000));
        assert!(p.tick(630_000, false, true));
    }
    #[test]
    fn resumed_speech_and_unhealthy_capture_cancel_countdown() {
        for (speech, healthy) in [(true, true), (false, false)] {
            let mut p = AutoEnd::new(5, 12);
            advance(&mut p, 0, 300_000);
            p.action(12, Action::Visible, 300_000);
            assert!(!p.tick(301_000, speech, healthy));
            assert!(p.notice(301_000).is_none());
            assert!(!advance(&mut p, 302_000, 330_000));
        }
    }
    #[test]
    fn continue_disable_and_stale_actions_are_safe() {
        let mut p = AutoEnd::new(5, 20);
        advance(&mut p, 0, 300_000);
        assert!(!p.action(19, Action::Stop, 300_000));
        assert!(!p.action(20, Action::Continue, 300_000));
        advance(&mut p, 301_000, 600_000);
        assert_eq!(p.notice(600_000).unwrap().id, 21);
        assert!(!p.action(20, Action::Stop, 600_000));
        p.action(21, Action::Disable, 600_000);
        assert!(!advance(&mut p, 601_000, 1_500_000));
        assert!(p.notice(1_500_000).is_none());
    }
    #[test]
    fn short_pauses_disabled_policy_and_sleep_never_stop() {
        let mut p = AutoEnd::new(5, 1);
        assert!(!advance(&mut p, 0, 30_000));
        assert!(p.notice(30_000).is_none());
        assert!(!p.tick(1_000_000, false, true));
        assert!(p.notice(1_000_000).is_none());
        let mut disabled = AutoEnd::new(0, 1);
        assert!(!advance(&mut disabled, 0, 900_000));
        assert!(disabled.notice(900_000).is_none());
    }
}
