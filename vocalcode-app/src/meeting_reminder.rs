//! Conservative, local-only meeting reminder classification and debounce.
//!
//! A reminder is a hint, never recording authority. The page still takes the
//! person to the Meetings controls where Start is an explicit second action.

use sha2::{Digest, Sha256};
use vocalcode_platform::meeting_presence::{self, CallState, MeetingWindow};
use vocalcode_platform::ForegroundApplication;

pub const DETECTION_DWELL_MS: u64 = 1_000;
const MAX_RECENT_REMINDERS: usize = 32;
pub const CONFIRMED_DWELL_MS: u64 = 0;
const SIGNAL_FRESH_MS: u64 = 8_000;
const ENCOUNTER_GONE_MS: u64 = 90_000;
const SNOOZE_MS: u64 = 2 * 60 * 1_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Strength {
    #[default]
    Title,
    Calendar,
    Audio,
    Confirmed,
}
impl Strength {
    fn dwell(self) -> u64 {
        match self {
            Self::Confirmed => CONFIRMED_DWELL_MS,
            Self::Audio | Self::Calendar => 0,
            Self::Title => DETECTION_DWELL_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeetingCandidate {
    pub app_key: String,
    /// In-memory encounter identity, separate from the user's app-wide ignore
    /// preference. Never persist or display the foreground title itself.
    pub instance_key: String,
    pub app_name: String,
    pub suggested_title: String,
    pub strength: Strength,
    pub foreground: bool,
    pub conference_key: Option<String>,
}

impl MeetingCandidate {
    fn key(&self) -> &str {
        self.conference_key.as_deref().unwrap_or(&self.instance_key)
    }
}
#[derive(Debug, Clone)]
pub struct Prompt {
    pub id: u64,
    pub candidate: MeetingCandidate,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Review,
    Dismiss,
    Snooze,
}
/// An explicit click only navigates to controls; it never authorizes capture.
#[derive(Debug, PartialEq, Eq)]
pub enum ActionResult {
    Ignored,
    Closed,
    Review(Option<MeetingCandidate>),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    Pending,
    Shown,
    Done,
    Snoozed(u64),
}
#[derive(Debug)]
struct Encounter {
    candidate: MeetingCandidate,
    first_seen_ms: u64,
    last_seen_ms: u64,
    confirmed_since_ms: Option<u64>,
    disposition: Disposition,
}

#[derive(Debug, Default)]
pub struct ReminderGate {
    encounters: Vec<Encounter>,
    current: Option<Prompt>,
    next_id: u64,
}

impl ReminderGate {
    pub fn current(&self) -> Option<&Prompt> {
        self.current.as_ref()
    }

    /// Bind IPC to the card actually displayed, not a background settings lock
    /// or a transient audio state. Late events must not close a newer card.
    pub fn visible_action(
        &mut self,
        visible_id: Option<u64>,
        id: u64,
        action: Action,
        now_ms: u64,
    ) -> ActionResult {
        if visible_id != Some(id) || self.current.as_ref().is_some_and(|p| p.id != id) {
            return ActionResult::Ignored;
        }
        let candidate = self.action(id, action, now_ms);
        if action == Action::Review {
            // If presence expired while clicking, still open manual controls,
            // but do not carry a stale meeting title into a new recording.
            ActionResult::Review(candidate)
        } else {
            ActionResult::Closed
        }
    }

    /// Pause rendering during dictation/correction. Do not record a dismissal.
    pub fn pause(&mut self) {
        if let Some(prompt) = self.current.take() {
            if let Some(entry) = self
                .encounters
                .iter_mut()
                .find(|e| e.candidate.key() == prompt.candidate.key())
            {
                if entry.disposition == Disposition::Shown {
                    entry.disposition = Disposition::Pending;
                }
            }
        }
    }

    pub fn action(&mut self, id: u64, action: Action, now_ms: u64) -> Option<MeetingCandidate> {
        let prompt = self.current.as_ref().filter(|p| p.id == id)?;
        let entry = self
            .encounters
            .iter_mut()
            .find(|e| e.candidate.key() == prompt.candidate.key())?;
        let fresh = now_ms.saturating_sub(entry.last_seen_ms) <= SIGNAL_FRESH_MS;
        entry.disposition = match action {
            Action::Snooze => Disposition::Snoozed(now_ms + SNOOZE_MS),
            _ => Disposition::Done,
        };
        let candidate = self.current.take()?.candidate;
        (action == Action::Review && fresh).then_some(candidate)
    }

    /// All candidates from one completed native observation. Current verified
    /// calls beat calendar guesses. Same conference across sources is one row.
    pub fn update(
        &mut self,
        now_ms: u64,
        candidates: &[MeetingCandidate],
        enabled: bool,
        meeting_active: bool,
        ignored_apps: &[String],
    ) {
        if !enabled {
            self.encounters.clear();
            self.current = None;
            return;
        }
        // Sleep/resume, wall-clock discontinuities, and a disappeared conference
        // must not leave an old visible card with recording authority.
        self.encounters
            .retain(|e| now_ms >= e.last_seen_ms && now_ms - e.last_seen_ms < ENCOUNTER_GONE_MS);
        let mut observed: Vec<_> = candidates
            .iter()
            .filter(|c| !ignored_apps.contains(&c.app_key))
            .cloned()
            .collect();
        observed.sort_by_key(|c| std::cmp::Reverse((c.strength, c.foreground)));
        let mut keys = std::collections::HashSet::new();
        for mut candidate in observed {
            // A provider can temporarily omit the document URL (or gain it
            // after the title hint). Merge only an unambiguous same-window
            // identity, never two distinct known conferences in one browser.
            let aliases: Vec<_> = self
                .encounters
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    e.candidate.instance_key == candidate.instance_key
                        && (e.candidate.conference_key.is_none()
                            || candidate.conference_key.is_none())
                })
                .map(|(i, _)| i)
                .collect();
            if aliases.len() == 1 {
                let entry = &mut self.encounters[aliases[0]];
                let previous_key = entry.candidate.key().to_string();
                if candidate.conference_key.is_none() {
                    candidate.conference_key = entry.candidate.conference_key.clone();
                }
                entry.candidate.conference_key = candidate.conference_key.clone();
                if let Some(prompt) = &mut self.current {
                    if prompt.candidate.key() == previous_key {
                        prompt.candidate.conference_key = candidate.conference_key.clone();
                    }
                }
            }
            if !keys.insert(candidate.key().to_string()) {
                continue;
            }
            if let Some(entry) = self
                .encounters
                .iter_mut()
                .find(|e| e.candidate.key() == candidate.key())
            {
                if now_ms.saturating_sub(entry.last_seen_ms) > SIGNAL_FRESH_MS {
                    // A missed observation is not continuous evidence, nor a
                    // dismissal. Require a new dwell after the provider recovers.
                    entry.first_seen_ms = now_ms;
                }
                if candidate.strength == Strength::Confirmed {
                    if entry.confirmed_since_ms.is_none()
                        || now_ms.saturating_sub(entry.last_seen_ms) > SIGNAL_FRESH_MS
                    {
                        entry.confirmed_since_ms = Some(now_ms);
                    }
                } else {
                    entry.confirmed_since_ms = None;
                }
                entry.candidate = candidate;
                entry.last_seen_ms = now_ms;
            } else {
                if self.encounters.len() >= MAX_RECENT_REMINDERS {
                    // Prefer dropping the least recently observed encounter;
                    // collection and IPC are bounded for the process lifetime.
                    if let Some((index, _)) = self
                        .encounters
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, e)| e.last_seen_ms)
                    {
                        self.encounters.remove(index);
                    }
                }
                let confirmed_since_ms =
                    (candidate.strength == Strength::Confirmed).then_some(now_ms);
                self.encounters.push(Encounter {
                    candidate,
                    first_seen_ms: now_ms,
                    last_seen_ms: now_ms,
                    confirmed_since_ms,
                    disposition: Disposition::Pending,
                });
            }
        }
        if meeting_active {
            // No stale suggestion reappears after a manually recorded meeting.
            for entry in &mut self.encounters {
                if keys.contains(entry.candidate.key()) {
                    entry.disposition = Disposition::Done;
                }
            }
            self.current = None;
            return;
        }
        if let Some(prompt) = &self.current {
            let valid = self.encounters.iter().any(|e| {
                e.candidate.key() == prompt.candidate.key()
                    && now_ms.saturating_sub(e.last_seen_ms) <= SIGNAL_FRESH_MS
                    && !ignored_apps.contains(&e.candidate.app_key)
            });
            let stronger = prompt.candidate.strength < Strength::Confirmed
                && self.encounters.iter().any(|e| {
                    keys.contains(e.candidate.key())
                        && e.candidate.strength == Strength::Confirmed
                        && e.confirmed_since_ms.is_some()
                        && e.disposition == Disposition::Pending
                });
            if valid && !stronger {
                return;
            }
            if let Some(entry) = self
                .encounters
                .iter_mut()
                .find(|e| e.candidate.key() == prompt.candidate.key())
            {
                entry.disposition = if stronger {
                    Disposition::Done
                } else {
                    Disposition::Pending
                };
            }
            self.current = None;
        }
        let eligible = self
            .encounters
            .iter_mut()
            .filter(|e| {
                if !keys.contains(e.candidate.key()) {
                    return false;
                }
                let since = if e.candidate.strength == Strength::Confirmed {
                    e.confirmed_since_ms.unwrap_or(now_ms)
                } else {
                    e.first_seen_ms
                };
                now_ms.saturating_sub(since) >= e.candidate.strength.dwell()
                    && match e.disposition {
                        Disposition::Pending => true,
                        Disposition::Snoozed(until) => now_ms >= until,
                        _ => false,
                    }
            })
            .max_by_key(|e| (e.candidate.strength, e.candidate.foreground));
        if let Some(entry) = eligible {
            self.next_id = self.next_id.saturating_add(1);
            entry.disposition = Disposition::Shown;
            self.current = Some(Prompt {
                id: self.next_id,
                candidate: entry.candidate.clone(),
            });
            // Categories only: never log titles, URLs, conference keys or apps.
            log::debug!(
                "meeting reminder: shown, signal={:?}",
                entry.candidate.strength
            );
        }
    }
}

pub fn classify_foreground(observed: &ForegroundApplication) -> Option<MeetingCandidate> {
    let app_id = observed.app_id.trim().to_ascii_lowercase();
    let title = observed.window_title.trim();
    if app_id.is_empty() || title.is_empty() {
        return None;
    }
    let folded_title = title.to_lowercase().replace(['\u{2013}', '\u{2014}'], "-");
    let display = safe_label(&observed.display_name, "Browser");

    if is_browser(&app_id) {
        let (service_key, service_name) = if contains_any(
            &folded_title,
            &["meet.google.com", "google meet", "meet - "],
        ) {
            ("google-meet", "Google Meet")
        } else if contains_any(&folded_title, &["zoom meeting", "zoom webinar", ".zoom.us"]) {
            ("zoom", "Zoom")
        } else if (folded_title.contains("microsoft teams")
            || folded_title.contains("teams meeting"))
            && contains_any(&folded_title, &["meeting", "call", "会议", "通话"])
        {
            ("teams", "Microsoft Teams")
        } else if folded_title.contains("webex")
            && contains_any(
                &folded_title,
                &["meeting", "webinar", "call", "会议", "通话"],
            )
        {
            ("webex", "Webex")
        } else {
            return None;
        };
        return Some(MeetingCandidate {
            app_key: format!("browser:{app_id}:{service_key}"),
            instance_key: instance_key(observed, service_key),
            app_name: format!("{service_name} ({display})"),
            suggested_title: format!("{service_name} meeting"),
            strength: Strength::Title,
            foreground: true,
            conference_key: None,
        });
    }

    let (service_key, service_name) = if is_zoom(&app_id)
        && contains_any(&folded_title, &["meeting", "webinar", "会议", "研讨会"])
    {
        ("zoom", "Zoom")
    } else if is_teams(&app_id) && contains_any(&folded_title, &["meeting", "call", "会议", "通话"])
    {
        ("teams", "Microsoft Teams")
    } else if is_slack(&app_id) && contains_any(&folded_title, &["huddle", "会议", "通话"]) {
        ("slack", "Slack Huddle")
    } else if is_webex(&app_id)
        && contains_any(
            &folded_title,
            &["meeting", "webinar", "call", "会议", "通话"],
        )
    {
        ("webex", "Webex")
    } else {
        return None;
    };
    Some(MeetingCandidate {
        app_key: format!("app:{app_id}:{service_key}"),
        instance_key: instance_key(observed, service_key),
        app_name: service_name.to_string(),
        suggested_title: format!("{service_name} meeting"),
        strength: Strength::Title,
        foreground: true,
        conference_key: None,
    })
}

fn instance_key(observed: &ForegroundApplication, service: &str) -> String {
    let title: String = observed.window_title.trim().chars().take(512).collect();
    let mut hash = Sha256::new();
    for part in [observed.app_id.as_str(), service, title.as_str()] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    hash.update(observed.process_id.to_le_bytes());
    format!("{:x}", hash.finalize())
}

pub fn conference_key(conference: &meeting_presence::Conference) -> Option<String> {
    let code = conference.code.as_ref()?;
    Some(format!(
        "conference:{:x}",
        Sha256::digest(format!("{}:{code}", conference.service.key()).as_bytes())
    ))
}

pub fn classify_window(window: &MeetingWindow) -> Option<MeetingCandidate> {
    if matches!(window.call_state, CallState::Lobby | CallState::Ended) {
        return None;
    }
    let conference = window.conference.as_ref();
    let mut candidate = classify_foreground(&window.application).or_else(|| {
        let conference = conference?;
        // A native home screen / unconfirmed background browser is not proof.
        if window.call_state != CallState::InCall && !window.microphone_active {
            return None;
        }
        Some(MeetingCandidate {
            app_key: format!(
                "{}:{}:{}",
                if meeting_presence::is_browser(&window.application.app_id) {
                    "browser"
                } else {
                    "app"
                },
                window.application.app_id,
                conference.service.key()
            ),
            instance_key: String::new(),
            app_name: conference.service.name().into(),
            suggested_title: format!("{} meeting", conference.service.name()),
            strength: Strength::Title,
            foreground: window.foreground,
            conference_key: None,
        })
    })?;
    if let Some(conference) = conference {
        candidate.app_key = format!(
            "{}:{}:{}",
            if meeting_presence::is_browser(&window.application.app_id) {
                "browser"
            } else {
                "app"
            },
            window.application.app_id,
            conference.service.key()
        );
        candidate.app_name = conference.service.name().into();
        candidate.suggested_title = format!("{} meeting", conference.service.name());
    }
    candidate.conference_key = conference.and_then(conference_key);
    if window.window_id != 0 {
        candidate.instance_key = format!(
            "window:{}:{}:{}",
            window.application.process_id, window.window_id, candidate.app_key
        );
    }
    candidate.foreground = window.foreground;
    candidate.strength = if window.call_state == CallState::InCall {
        Strength::Confirmed
    } else if window.microphone_active && conference.is_some() {
        Strength::Audio
    } else {
        Strength::Title
    };
    // Title-only background pages may be a forgotten tab, not an active call.
    if !window.foreground && candidate.strength == Strength::Title {
        return None;
    }
    Some(candidate)
}

/// A single disposable read-only worker; a wedged provider can never create a
/// growing thread pool or block the UI, input hooks, audio or application exit.
pub struct Probe {
    request: std::sync::mpsc::SyncSender<()>,
    response: std::sync::mpsc::Receiver<(std::time::Instant, Vec<MeetingCandidate>)>,
    busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl Probe {
    pub fn new(on_result: impl Fn() + Send + 'static) -> std::io::Result<Self> {
        let (request, requests) = std::sync::mpsc::sync_channel(1);
        let (responses, response) = std::sync::mpsc::sync_channel(1);
        let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_busy = busy.clone();
        std::thread::Builder::new()
            .name("vocalcode-meeting-presence".into())
            .spawn(move || {
                while requests.recv().is_ok() {
                    let started = std::time::Instant::now();
                    let windows = meeting_presence::observe();
                    let candidates = if started.elapsed() <= std::time::Duration::from_secs(3) {
                        windows.iter().filter_map(classify_window).collect()
                    } else {
                        Vec::new()
                    };
                    if responses.send((started, candidates)).is_err() {
                        return;
                    }
                    worker_busy.store(false, std::sync::atomic::Ordering::Release);
                    on_result();
                }
            })?;
        Ok(Self {
            request,
            response,
            busy,
        })
    }
    pub fn request(&self) {
        use std::sync::atomic::Ordering;
        if !self.busy.swap(true, Ordering::AcqRel) && self.request.try_send(()).is_err() {
            self.busy.store(false, Ordering::Release);
        }
    }
    pub fn result(&self) -> Option<(std::time::Instant, Vec<MeetingCandidate>)> {
        self.response.try_recv().ok()
    }
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

fn is_browser(app_id: &str) -> bool {
    [
        "chrome.exe",
        "msedge.exe",
        "firefox.exe",
        "brave.exe",
        "vivaldi.exe",
        "arc.exe",
        "com.google.chrome",
        "com.microsoft.edgemac",
        "org.mozilla.firefox",
        "com.brave.browser",
        "company.thebrowser.browser",
    ]
    .iter()
    .any(|browser| app_id == *browser || app_id.ends_with(browser))
}

fn is_zoom(app_id: &str) -> bool {
    app_id == "zoom.exe" || app_id.contains("zoom.us") || app_id.contains("zoom.xos")
}

fn is_teams(app_id: &str) -> bool {
    matches!(app_id, "teams.exe" | "ms-teams.exe") || app_id.contains("microsoft.teams")
}

fn is_slack(app_id: &str) -> bool {
    app_id == "slack.exe" || app_id.contains("tinyspeck.slackmacgap")
}

fn is_webex(app_id: &str) -> bool {
    app_id.contains("webex") || app_id == "ciscocollabhost.exe"
}

fn safe_label(value: &str, fallback: &str) -> String {
    let label = value
        .chars()
        .filter(|character| !character.is_control())
        .take(48)
        .collect::<String>();
    if label.trim().is_empty() {
        fallback.to_string()
    } else {
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observed(app_id: &str, name: &str, title: &str) -> ForegroundApplication {
        ForegroundApplication {
            process_id: 42,
            app_id: app_id.to_string(),
            display_name: name.to_string(),
            window_title: title.to_string(),
        }
    }

    fn confirmed(code: &str) -> MeetingCandidate {
        classify_window(&MeetingWindow {
            application: observed("chrome.exe", "Chrome", &format!("Meet - {code} - Chrome")),
            window_id: 10,
            foreground: true,
            microphone_active: true,
            conference: meeting_presence::conference_url(&format!(
                "https://meet.google.com/{code}"
            )),
            call_state: CallState::InCall,
        })
        .unwrap()
    }
    #[test]
    fn verified_call_prompts_on_first_observation_and_survives_foreground_switch() {
        let mut gate = ReminderGate::default();
        let mut candidate = confirmed("abc-defg-hij");
        gate.update(0, std::slice::from_ref(&candidate), true, false, &[]);
        let first_id = gate.current().unwrap().id;
        gate.update(1000, std::slice::from_ref(&candidate), true, false, &[]);
        assert_eq!(gate.current().unwrap().id, first_id);
        candidate.foreground = false;
        candidate.app_name = "changed display title".into();
        gate.update(2000, std::slice::from_ref(&candidate), true, false, &[]);
        let id = gate.current().unwrap().id;
        gate.update(3000, &[candidate], true, false, &[]);
        assert_eq!(gate.current().unwrap().id, id);
    }
    #[test]
    fn conference_and_mic_prompt_immediately_but_are_not_call_confirmation() {
        let mut window = MeetingWindow {
            application: observed("chrome.exe", "Chrome", "Meet - abc-defg-hij - Chrome"),
            window_id: 1,
            foreground: true,
            microphone_active: true,
            conference: meeting_presence::conference_url("https://meet.google.com/abc-defg-hij"),
            call_state: CallState::Lobby,
        };
        assert!(classify_window(&window).is_none());
        window.call_state = CallState::Ended;
        assert!(classify_window(&window).is_none());
        window.call_state = CallState::Unknown;
        let candidate = classify_window(&window).unwrap();
        assert_eq!(candidate.strength, Strength::Audio);
        let mut gate = ReminderGate::default();
        gate.update(0, std::slice::from_ref(&candidate), true, false, &[]);
        assert_eq!(gate.current().unwrap().candidate.strength, Strength::Audio);
        gate.update(2000, &[candidate], true, false, &[]);
        assert!(gate.current().is_some());
        window.microphone_active = false;
        window.foreground = false;
        assert!(classify_window(&window).is_none());
        window.application = observed("chrome.exe", "Chrome", "Voice recorder");
        window.conference = None;
        window.microphone_active = true;
        assert!(classify_window(&window).is_none());
    }
    #[test]
    fn snooze_requires_fresh_presence_and_old_actions_cannot_affect_new_card() {
        let mut gate = ReminderGate::default();
        let a = confirmed("abc-defg-hij");
        gate.update(0, std::slice::from_ref(&a), true, false, &[]);
        gate.update(2000, std::slice::from_ref(&a), true, false, &[]);
        let old = gate.current().unwrap().id;
        gate.action(old, Action::Snooze, 2000);
        for now in (3000..122000).step_by(1000) {
            gate.update(now, std::slice::from_ref(&a), true, false, &[]);
            assert!(gate.current().is_none());
        }
        gate.update(122000, std::slice::from_ref(&a), true, false, &[]);
        let new = gate.current().unwrap().id;
        assert_ne!(old, new);
        assert!(gate.action(old, Action::Review, 122000).is_none());
        assert_eq!(gate.current().unwrap().id, new);
        gate.action(new, Action::Dismiss, 122000);
        gate.update(123000, &[a], true, false, &[]);
        assert!(gate.current().is_none());
    }
    #[test]
    fn vanished_snoozed_meeting_never_prompts_and_stale_review_is_refused() {
        let mut gate = ReminderGate::default();
        let a = confirmed("abc-defg-hij");
        gate.update(0, std::slice::from_ref(&a), true, false, &[]);
        gate.update(2000, std::slice::from_ref(&a), true, false, &[]);
        let id = gate.current().unwrap().id;
        assert!(gate.action(id, Action::Review, 11000).is_none());
        gate.update(120000, &[], true, false, &[]);
        assert!(gate.current().is_none());
        let mut gate = ReminderGate::default();
        gate.update(0, std::slice::from_ref(&a), true, false, &[]);
        gate.update(2000, &[a], true, false, &[]);
        gate.action(gate.current().unwrap().id, Action::Snooze, 2000);
        gate.update(122000, &[], true, false, &[]);
        assert!(gate.current().is_none());
    }
    #[test]
    fn calendar_and_window_share_identity_but_different_codes_do_not() {
        let mut gate = ReminderGate::default();
        let a = confirmed("abc-defg-hij");
        let mut calendar = a.clone();
        calendar.app_key = "calendar:google".into();
        calendar.instance_key = "calendar:event-one".into();
        calendar.strength = Strength::Calendar;
        gate.update(0, &[calendar.clone(), a.clone()], true, false, &[]);
        gate.update(2000, &[calendar.clone(), a.clone()], true, false, &[]);
        assert_eq!(gate.encounters.len(), 1);
        assert_eq!(
            gate.current().unwrap().candidate.strength,
            Strength::Confirmed
        );
        gate.action(gate.current().unwrap().id, Action::Dismiss, 2000);
        let b = confirmed("klm-nopq-rst");
        gate.update(
            3000,
            &[calendar.clone(), a.clone(), b.clone()],
            true,
            false,
            &[],
        );
        gate.update(5000, &[calendar, a, b.clone()], true, false, &[]);
        assert_eq!(gate.current().unwrap().candidate.key(), b.key());
    }
    #[test]
    fn real_call_replaces_an_unrelated_calendar_guess() {
        let mut gate = ReminderGate::default();
        let a = confirmed("abc-defg-hij");
        let mut calendar = confirmed("klm-nopq-rst");
        calendar.strength = Strength::Calendar;
        gate.update(0, std::slice::from_ref(&calendar), true, false, &[]);
        gate.update(1000, std::slice::from_ref(&calendar), true, false, &[]);
        assert_eq!(
            gate.current().unwrap().candidate.strength,
            Strength::Calendar
        );
        gate.update(2000, &[calendar.clone(), a.clone()], true, false, &[]);
        gate.update(4000, &[calendar, a], true, false, &[]);
        assert_eq!(
            gate.current().unwrap().candidate.strength,
            Strength::Confirmed
        );
    }
    #[test]
    fn no_duplicate_when_uia_temporarily_loses_a_conference_code() {
        let mut gate = ReminderGate::default();
        let a = confirmed("abc-defg-hij");
        gate.update(0, std::slice::from_ref(&a), true, false, &[]);
        gate.update(2000, std::slice::from_ref(&a), true, false, &[]);
        gate.action(gate.current().unwrap().id, Action::Dismiss, 2000);
        let mut degraded = a.clone();
        degraded.conference_key = None;
        degraded.strength = Strength::Audio;
        gate.update(3000, std::slice::from_ref(&degraded), true, false, &[]);
        gate.update(11000, &[degraded], true, false, &[]);
        assert!(gate.current().is_none());
        assert_eq!(gate.encounters.len(), 1);
    }
    #[test]
    fn paused_card_resumes_but_recording_or_disabled_never_prompts() {
        let mut gate = ReminderGate::default();
        let a = confirmed("abc-defg-hij");
        gate.update(0, std::slice::from_ref(&a), true, false, &[]);
        gate.update(2000, std::slice::from_ref(&a), true, false, &[]);
        gate.pause();
        assert!(gate.current().is_none());
        gate.update(3000, std::slice::from_ref(&a), true, false, &[]);
        assert!(gate.current().is_some());
        gate.update(4000, std::slice::from_ref(&a), true, true, &[]);
        assert!(gate.current().is_none());
        gate.update(5000, std::slice::from_ref(&a), true, false, &[]);
        assert!(gate.current().is_none());
        gate.update(6000, std::slice::from_ref(&a), false, false, &[]);
        assert!(gate.encounters.is_empty());
        gate.update(
            7000,
            std::slice::from_ref(&a),
            true,
            false,
            std::slice::from_ref(&a.app_key),
        );
        assert!(gate.current().is_none());
    }
    #[test]
    fn fresh_confirmed_observation_recovers_immediately_after_a_gap() {
        let mut gate = ReminderGate::default();
        let a = confirmed("abc-defg-hij");
        gate.update(0, std::slice::from_ref(&a), true, false, &[]);
        gate.update(9000, &[], true, false, &[]);
        assert!(gate.current().is_none());
        gate.update(20000, std::slice::from_ref(&a), true, false, &[]);
        assert!(gate.current().is_some());
        gate.update(22000, &[a], true, false, &[]);
        assert!(gate.current().is_some());
    }

    #[test]
    fn classifies_supported_desktop_and_browser_meetings_conservatively() {
        let meet = classify_foreground(&observed(
            "chrome.exe",
            "Chrome",
            "Meet - abc-defg-hij - Google Chrome",
        ))
        .unwrap();
        assert_eq!(meet.app_key, "browser:chrome.exe:google-meet");
        assert_eq!(meet.app_name, "Google Meet (Chrome)");

        let teams = classify_foreground(&observed(
            "ms-teams.exe",
            "Microsoft Teams",
            "Weekly planning | Meeting | Microsoft Teams",
        ))
        .unwrap();
        assert_eq!(teams.app_key, "app:ms-teams.exe:teams");

        assert!(classify_foreground(&observed(
            "ms-teams.exe",
            "Microsoft Teams",
            "Chat | Microsoft Teams",
        ))
        .is_none());
        assert!(classify_foreground(&observed(
            "chrome.exe",
            "Chrome",
            "Meet the team - Google Chrome",
        ))
        .is_none());
    }

    #[test]
    fn title_hint_requires_continuous_dwell_and_respects_dismissal() {
        let mut candidate = confirmed("abc-defg-hij");
        candidate.strength = Strength::Title;
        let mut gate = ReminderGate::default();
        for now in [0, 500, 999] {
            gate.update(now, std::slice::from_ref(&candidate), true, false, &[]);
            assert!(gate.current().is_none());
        }
        gate.update(1000, std::slice::from_ref(&candidate), true, false, &[]);
        gate.action(gate.current().unwrap().id, Action::Dismiss, 1000);
        for now in (2000..60_000).step_by(4000) {
            gate.update(now, std::slice::from_ref(&candidate), true, false, &[]);
            assert!(gate.current().is_none());
        }
    }

    #[test]
    fn vanished_prompt_can_recover_but_old_button_stays_invalid() {
        let a = confirmed("abc-defg-hij");
        let mut gate = ReminderGate::default();
        gate.update(0, std::slice::from_ref(&a), true, false, &[]);
        gate.update(2000, std::slice::from_ref(&a), true, false, &[]);
        let old_id = gate.current().unwrap().id;
        gate.update(11_000, &[], true, false, &[]);
        assert!(gate.current().is_none());
        gate.update(12_000, std::slice::from_ref(&a), true, false, &[]);
        assert!(gate.current().is_some());
        gate.update(14_000, std::slice::from_ref(&a), true, false, &[]);
        assert_ne!(gate.current().unwrap().id, old_id);
        assert!(gate.action(old_id, Action::Review, 14_000).is_none());
        assert!(gate.current().is_some());

        let second_id = gate.current().unwrap().id;
        assert!(gate.action(second_id, Action::Review, 23_000).is_none());
        gate.update(24_000, std::slice::from_ref(&a), true, false, &[]);
        assert!(gate.current().is_none());
        gate.update(26_000, std::slice::from_ref(&a), true, false, &[]);
        // Explicit Review consumes the reminder even when its title is stale.
        assert!(gate.current().is_none());
    }

    #[test]
    fn interrupted_title_hint_cannot_accumulate_stale_dwell() {
        let mut a = confirmed("abc-defg-hij");
        a.strength = Strength::Title;
        let mut gate = ReminderGate::default();
        gate.update(0, std::slice::from_ref(&a), true, false, &[]);
        gate.update(20_000, std::slice::from_ref(&a), true, false, &[]);
        assert!(gate.current().is_none());
        for now in [20_500, 20_999] {
            gate.update(now, std::slice::from_ref(&a), true, false, &[]);
            assert!(gate.current().is_none());
        }
        gate.update(21_000, &[a], true, false, &[]);
        assert!(gate.current().is_some());
    }

    #[test]
    fn visible_review_opens_controls_even_if_presence_expires_but_drops_stale_title() {
        for now in [0, 9000] {
            let mut gate = ReminderGate::default();
            let a = confirmed("abc-defg-hij");
            gate.update(0, std::slice::from_ref(&a), true, false, &[]);
            let id = gate.current().unwrap().id;
            let result = gate.visible_action(Some(id), id, Action::Review, now);
            assert_eq!(
                result,
                ActionResult::Review((now == 0).then_some(a.clone()))
            );
            assert!(gate.current().is_none());
            gate.update(now + 1, &[a], true, false, &[]);
            assert!(gate.current().is_none());
        }
    }

    #[test]
    fn stale_signal_cannot_swallow_dismiss_or_snooze() {
        for action in [Action::Dismiss, Action::Snooze] {
            let mut gate = ReminderGate::default();
            let a = confirmed("abc-defg-hij");
            gate.update(0, std::slice::from_ref(&a), true, false, &[]);
            let id = gate.current().unwrap().id;
            assert_eq!(
                gate.visible_action(Some(id), id, action, 9000),
                ActionResult::Closed
            );
            for now in (10_000..129_000).step_by(1000) {
                gate.update(now, std::slice::from_ref(&a), true, false, &[]);
                assert!(gate.current().is_none());
            }
            gate.update(129_000, &[a], true, false, &[]);
            assert_eq!(gate.current().is_some(), action == Action::Snooze);
        }
    }

    #[test]
    fn late_or_hidden_clicks_never_close_a_newer_card() {
        let mut gate = ReminderGate::default();
        gate.update(0, &[confirmed("abc-defg-hij")], true, false, &[]);
        let old = gate.current().unwrap().id;
        gate.action(old, Action::Dismiss, 0);
        gate.update(1, &[confirmed("klm-nopq-rst")], true, false, &[]);
        let new = gate.current().unwrap().id;
        for visible in [None, Some(old), Some(new)] {
            for action in [Action::Review, Action::Snooze, Action::Dismiss] {
                assert_eq!(
                    gate.visible_action(visible, old, action, 1),
                    ActionResult::Ignored
                );
                assert_eq!(gate.current().unwrap().id, new);
            }
        }
    }

    #[test]
    fn expired_visible_card_can_still_open_manual_controls() {
        let mut gate = ReminderGate::default();
        gate.update(0, &[confirmed("abc-defg-hij")], true, false, &[]);
        let id = gate.current().unwrap().id;
        gate.update(9000, &[], true, false, &[]);
        assert_eq!(
            gate.visible_action(Some(id), id, Action::Review, 9000),
            ActionResult::Review(None)
        );
    }

    #[test]
    fn separate_meetings_in_one_browser_do_not_share_dismissal() {
        let first = confirmed("abc-defg-hij");
        let second = confirmed("klm-nopq-rst");
        assert_eq!(first.app_key, second.app_key);
        assert_ne!(first.key(), second.key());
        let mut gate = ReminderGate::default();
        gate.update(0, std::slice::from_ref(&first), true, false, &[]);
        gate.update(2000, std::slice::from_ref(&first), true, false, &[]);
        gate.action(gate.current().unwrap().id, Action::Dismiss, 2000);
        gate.update(3000, std::slice::from_ref(&second), true, false, &[]);
        gate.update(5000, std::slice::from_ref(&second), true, false, &[]);
        assert_eq!(gate.current().unwrap().candidate.key(), second.key());
        gate.action(gate.current().unwrap().id, Action::Dismiss, 5000);
        gate.update(6000, std::slice::from_ref(&first), true, false, &[]);
        gate.update(8000, std::slice::from_ref(&first), true, false, &[]);
        assert!(gate.current().is_none());
    }
}
