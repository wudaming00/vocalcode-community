//! Local meeting runtime. Capture, imported-file decoding, speech segmentation,
//! transcript persistence, notes, search, and export all stay in this process.

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};
use vocalcode_meeting::{
    build_local_summary, decode_audio_file, export_json, export_markdown, export_srt, export_text,
    search_meetings, AudioBlock, AudioRetention, AudioSource, ChunkedPcmWriter,
    LinearMonoResampler, Meeting, MeetingId, MeetingSource, MeetingStatus, MeetingStore,
    NewMeeting, OnlineSpeakerClusterer, RealtimeEchoCanceller, Speaker, SpeechSegment,
    SpeechSegmenter, SummaryOptions, TranscriptSegment, Voiceprint,
};
use vocalcode_platform::{start_meeting_audio, StreamedAudioBlock, StreamedAudioSource};

use crate::MeetingAsrRequest;
use std::time::Instant;
use vocalcode_meeting::auto_end::{Action as AutoEndAction, AutoEnd, Notice as AutoEndNotice};
use vocalcode_platform::speech_gate::{Decision, SpeechGate};

const AUDIO_QUEUE_CAPACITY: usize = 2_048;
const COMMAND_QUEUE_CAPACITY: usize = 32;
const MAX_PENDING_SEGMENTS: usize = 32;
const MAX_PENDING_BOOKMARKS: usize = 256;

#[derive(Debug)]
enum Command {
    StartLive {
        stop: Arc<AtomicBool>,
        title: String,
        microphone: bool,
        system_audio: bool,
        microphone_selector: Option<String>,
        keep_audio: bool,
        language: String,
        auto_end_minutes: u64,
    },
    Import {
        stop: Arc<AtomicBool>,
        path: PathBuf,
        title: String,
        language: String,
    },
    Select(MeetingId),
    Delete(MeetingId),
    Rename(MeetingId, String),
    RenameSpeaker(MeetingId, String, String),
    Bookmark(MeetingId, u64, String),
    Search(String),
    Export(MeetingId, ExportKind, PathBuf),
    /// Meetings were added to the store from outside this controller.
    Refresh(String),
    Shutdown,
}

#[derive(Debug, Clone, Copy)]
pub enum ExportKind {
    Markdown,
    Text,
    Json,
    Srt,
}

#[derive(Default)]
struct Shared {
    command: Mutex<Option<mpsc::SyncSender<Command>>>,
    state: Mutex<Value>,
    active: AtomicBool,
    transcribing: AtomicBool,
    shutting_down: AtomicBool,
    revision: std::sync::atomic::AtomicU64,
    stop: Mutex<Option<Arc<AtomicBool>>>,
    active_id: Mutex<Option<MeetingId>>,
    pending_bookmarks: Mutex<Vec<(u64, String)>>,
    auto_end_notice: Mutex<Option<AutoEndNotice>>,
    auto_end_actions: Mutex<Vec<(u64, AutoEndAction)>>,
}

#[derive(Clone, Default)]
pub struct Bridge(Arc<Shared>);

impl Bridge {
    pub fn snapshot(&self) -> Value {
        let mut value = self
            .0
            .state
            .lock()
            .map(|state| state.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone());
        if value.is_object() {
            value["auto_end"] = json!(self.auto_end_notice());
        }
        value
    }

    pub fn update_after(&self, revision: u64) -> Option<(u64, Value)> {
        let current = self.0.revision.load(Ordering::Acquire);
        (current != revision).then(|| (current, self.snapshot()))
    }

    pub fn is_active(&self) -> bool {
        self.0.active.load(Ordering::Acquire)
    }

    pub fn is_transcribing(&self) -> bool {
        self.0.transcribing.load(Ordering::Acquire)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start_live(
        &self,
        title: String,
        microphone: bool,
        system_audio: bool,
        microphone_selector: Option<String>,
        keep_audio: bool,
        language: String,
        auto_end_minutes: u64,
    ) -> Result<(), String> {
        let stop = Arc::new(AtomicBool::new(false));
        self.reserve_and_send(
            Command::StartLive {
                stop: stop.clone(),
                title,
                microphone,
                system_audio,
                microphone_selector,
                keep_audio,
                language,
                auto_end_minutes,
            },
            stop,
        )
    }

    pub fn stop(&self) -> Result<(), String> {
        let guard = self
            .0
            .stop
            .lock()
            .unwrap_or_else(|value| value.into_inner());
        let Some(stop) = guard.as_ref() else {
            return Err("No meeting is recording or importing.".to_string());
        };
        stop.store(true, Ordering::Release);
        Ok(())
    }

    pub fn auto_end_notice(&self) -> Option<AutoEndNotice> {
        self.0
            .auto_end_notice
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn auto_end_action(&self, id: u64, action: AutoEndAction) {
        // Separate bounded control path: buttons must not wait behind ASR.
        let notice = self
            .0
            .auto_end_notice
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if !self.is_active() || notice.as_ref().is_none_or(|n| n.id != id) {
            return;
        }
        let mut actions = self
            .0
            .auto_end_actions
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if actions.len() < 16 && !actions.contains(&(id, action)) {
            actions.push((id, action));
        }
    }

    fn set_auto_end_notice(&self, notice: Option<AutoEndNotice>) {
        let mut current = self
            .0
            .auto_end_notice
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if *current != notice {
            *current = notice;
            self.0.revision.fetch_add(1, Ordering::Release);
        }
    }

    pub fn import(&self, path: PathBuf, title: String, language: String) -> Result<(), String> {
        let stop = Arc::new(AtomicBool::new(false));
        self.reserve_and_send(
            Command::Import {
                stop: stop.clone(),
                path,
                title,
                language,
            },
            stop,
        )
    }

    pub fn select(&self, id: MeetingId) -> Result<(), String> {
        self.send(Command::Select(id))
    }

    pub fn delete(&self, id: MeetingId) -> Result<(), String> {
        self.send(Command::Delete(id))
    }

    pub fn rename(&self, id: MeetingId, title: String) -> Result<(), String> {
        self.send(Command::Rename(id, title))
    }

    pub fn rename_speaker(
        &self,
        id: MeetingId,
        speaker_id: String,
        label: String,
    ) -> Result<(), String> {
        self.send(Command::RenameSpeaker(id, speaker_id, label))
    }

    pub fn bookmark(&self, id: MeetingId, at_ms: u64, label: String) -> Result<(), String> {
        if self.is_active() {
            let active_id = self
                .0
                .active_id
                .lock()
                .unwrap_or_else(|value| value.into_inner());
            if active_id.as_ref() != Some(&id) {
                return Err("The active meeting is still starting or has changed.".to_string());
            }
            let label = truncate_utf8(label.trim(), 256);
            if label.is_empty() || label.contains('\0') {
                return Err("Enter a bookmark label.".to_string());
            }
            let mut pending = self
                .0
                .pending_bookmarks
                .lock()
                .unwrap_or_else(|value| value.into_inner());
            if pending.len() >= MAX_PENDING_BOOKMARKS {
                return Err("Too many meeting bookmarks are waiting to save.".to_string());
            }
            pending.push((at_ms, label.to_string()));
            drop(pending);
            drop(active_id);
            return Ok(());
        }
        self.send(Command::Bookmark(id, at_ms, label))
    }

    pub fn search(&self, query: String) -> Result<(), String> {
        self.send(Command::Search(query))
    }

    /// Re-read the store after complete meetings were published into it by
    /// someone else (the previous-edition import) and show `notice`.
    pub fn refresh(&self, notice: String) -> Result<(), String> {
        self.send(Command::Refresh(notice))
    }

    pub fn export(&self, id: MeetingId, kind: ExportKind, path: PathBuf) -> Result<(), String> {
        self.send(Command::Export(id, kind, path))
    }

    fn send(&self, command: Command) -> Result<(), String> {
        let sender = self
            .0
            .command
            .lock()
            .unwrap_or_else(|value| value.into_inner())
            .clone()
            .ok_or_else(|| "The local meeting service is not ready.".to_string())?;
        sender.try_send(command).map_err(|_| {
            "The local meeting service is busy; wait a moment and try again.".to_string()
        })
    }

    fn reserve_and_send(&self, command: Command, stop: Arc<AtomicBool>) -> Result<(), String> {
        self.0
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "Finish the current meeting first.".to_string())?;
        // Publish the cancellation token before queueing the command so an
        // immediate Stop click cannot race the controller thread.
        self.set_stop(Some(stop.clone()));
        if let Err(error) = self.send(command) {
            self.clear_stop_if(&stop);
            self.0.active.store(false, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }

    fn publish(&self, mut value: Value) {
        let active_id = self
            .0
            .active_id
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|state| state.into_inner());
        // Browsing a saved meeting must not change the live recording's title,
        // clock, or Stop state. Keep that identity separate from the detail pane.
        if let Some(id) = active_id.filter(|_| self.is_active()) {
            value["recording"] = if value["detail"]["id"].as_str() == Some(id.as_str()) {
                let detail = &value["detail"];
                json!({"id": id, "title": detail["title"], "started_at_ms": detail["started_at_ms"],
                    "duration_ms": detail["duration_ms"], "status": detail["status"],
                    "stopping": detail["status"] == "processing" && detail["ended_at_ms"].is_number()})
            } else {
                state["recording"].clone()
            };
        }
        *state = value;
        self.0.revision.fetch_add(1, Ordering::Release);
    }

    fn set_stop(&self, stop: Option<Arc<AtomicBool>>) {
        *self
            .0
            .stop
            .lock()
            .unwrap_or_else(|value| value.into_inner()) = stop;
    }

    fn clear_stop_if(&self, expected: &Arc<AtomicBool>) {
        let mut current = self
            .0
            .stop
            .lock()
            .unwrap_or_else(|value| value.into_inner());
        if current
            .as_ref()
            .is_some_and(|stop| Arc::ptr_eq(stop, expected))
        {
            *current = None;
        }
    }

    fn set_active_meeting(&self, id: Option<MeetingId>) {
        let should_clear = id.is_none();
        *self
            .0
            .active_id
            .lock()
            .unwrap_or_else(|value| value.into_inner()) = id;
        if should_clear {
            self.set_auto_end_notice(None);
            self.0
                .auto_end_actions
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clear();
            self.0
                .pending_bookmarks
                .lock()
                .unwrap_or_else(|value| value.into_inner())
                .clear();
        }
    }
}

pub struct Runtime {
    bridge: Bridge,
    worker: Option<thread::JoinHandle<()>>,
}

impl Runtime {
    pub fn start(base: &Path, asr: mpsc::SyncSender<MeetingAsrRequest>) -> Result<Self, String> {
        let bridge = Bridge::default();
        bridge.publish(json!({
            "ready": false,
            "active": false,
            "transcribing": false,
            "meetings": [],
            "detail": null,
            "search": [],
            "error": null,
            "notice": "Preparing local meetings…"
        }));
        let (sender, receiver) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
        *bridge
            .0
            .command
            .lock()
            .unwrap_or_else(|value| value.into_inner()) = Some(sender);
        let worker_bridge = bridge.clone();
        let root = base.join("meetings");
        let worker = thread::Builder::new()
            .name("vocalcode-meetings".to_string())
            .spawn(move || run_controller(root, asr, receiver, worker_bridge))
            .map_err(|error| format!("could not start local meeting service: {error}"))?;
        Ok(Self {
            bridge,
            worker: Some(worker),
        })
    }

    pub fn bridge(&self) -> Bridge {
        self.bridge.clone()
    }

    pub fn shutdown(mut self) {
        self.bridge.0.shutting_down.store(true, Ordering::Release);
        let _ = self.bridge.stop();
        if let Some(sender) = self
            .bridge
            .0
            .command
            .lock()
            .unwrap_or_else(|value| value.into_inner())
            .clone()
        {
            let _ = sender.send(Command::Shutdown);
        }
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                log::error!("local meeting service panicked during shutdown");
            }
        }
    }
}

fn run_controller(
    root: PathBuf,
    asr: mpsc::SyncSender<MeetingAsrRequest>,
    receiver: mpsc::Receiver<Command>,
    bridge: Bridge,
) {
    let store = match MeetingStore::open(&root) {
        Ok(store) => store,
        Err(error) => {
            bridge.publish(error_state(format!(
                "Could not open local meetings: {error}"
            )));
            return;
        }
    };
    if let Err(error) = store.recover_interrupted(now_ms()) {
        log::error!("meeting recovery: {error}");
    }
    let mut selected = None;
    let mut search = Vec::new();
    publish_store(&bridge, &store, selected.as_ref(), &search, None, None);
    let mut task: Option<thread::JoinHandle<()>> = None;

    while let Ok(command) = receiver.recv() {
        let carries_new_reservation =
            matches!(&command, Command::StartLive { .. } | Command::Import { .. });
        if carries_new_reservation || task.as_ref().is_some_and(|worker| worker.is_finished()) {
            if task.take().is_some_and(|worker| worker.join().is_err()) {
                publish_store(
                    &bridge,
                    &store,
                    selected.as_ref(),
                    &search,
                    Some("The local meeting worker stopped unexpectedly.".to_string()),
                    None,
                );
            }
            if !carries_new_reservation {
                bridge.set_stop(None);
                bridge.0.active.store(false, Ordering::Release);
            }
            bridge.0.transcribing.store(false, Ordering::Release);
            bridge.set_active_meeting(None);
        }
        match command {
            Command::StartLive {
                stop,
                title,
                microphone,
                system_audio,
                microphone_selector,
                keep_audio,
                language,
                auto_end_minutes,
            } => {
                if task.is_some() {
                    bridge.clear_stop_if(&stop);
                    bridge.0.active.store(false, Ordering::Release);
                    publish_store(
                        &bridge,
                        &store,
                        selected.as_ref(),
                        &search,
                        Some("Finish the current meeting first.".to_string()),
                        None,
                    );
                    continue;
                }
                let task_bridge = bridge.clone();
                let task_store = store.clone();
                let task_asr = asr.clone();
                let spawn_failure_stop = stop.clone();
                let spawned = thread::Builder::new()
                    .name("vocalcode-live-meeting".to_string())
                    .spawn(move || {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            run_live(
                                &task_store,
                                &task_bridge,
                                &task_asr,
                                stop.clone(),
                                title,
                                microphone,
                                system_audio,
                                microphone_selector,
                                keep_audio,
                                language,
                                auto_end_minutes,
                            )
                        }))
                        .unwrap_or_else(|_| {
                            Err("The local meeting worker stopped unexpectedly.".to_string())
                        });
                        task_bridge.set_active_meeting(None);
                        task_bridge.0.active.store(false, Ordering::Release);
                        task_bridge.0.transcribing.store(false, Ordering::Release);
                        task_bridge.clear_stop_if(&stop);
                        match result {
                            Ok(()) => publish_store(
                                &task_bridge,
                                &task_store,
                                None,
                                &[],
                                None,
                                Some("Meeting saved locally.".to_string()),
                            ),
                            Err(error) => {
                                log::error!("live meeting: {error}");
                                let _ = task_store.recover_interrupted(now_ms());
                                publish_store(
                                    &task_bridge,
                                    &task_store,
                                    None,
                                    &[],
                                    Some(error),
                                    None,
                                );
                            }
                        }
                    });
                match spawned {
                    Ok(worker) => task = Some(worker),
                    Err(error) => {
                        bridge.clear_stop_if(&spawn_failure_stop);
                        bridge.0.active.store(false, Ordering::Release);
                        publish_store(
                            &bridge,
                            &store,
                            selected.as_ref(),
                            &search,
                            Some(format!("Could not start the meeting worker: {error}")),
                            None,
                        );
                    }
                }
            }
            Command::Import {
                stop,
                path,
                title,
                language,
            } => {
                if task.is_some() {
                    bridge.clear_stop_if(&stop);
                    bridge.0.active.store(false, Ordering::Release);
                    publish_store(
                        &bridge,
                        &store,
                        selected.as_ref(),
                        &search,
                        Some("Finish the current meeting first.".to_string()),
                        None,
                    );
                    continue;
                }
                let task_bridge = bridge.clone();
                let task_store = store.clone();
                let task_asr = asr.clone();
                let spawn_failure_stop = stop.clone();
                let spawned = thread::Builder::new()
                    .name("vocalcode-meeting-import".to_string())
                    .spawn(move || {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            run_import(
                                &task_store,
                                &task_bridge,
                                &task_asr,
                                stop.clone(),
                                path,
                                title,
                                language,
                            )
                        }))
                        .unwrap_or_else(|_| {
                            Err("The local meeting import worker stopped unexpectedly.".to_string())
                        });
                        task_bridge.set_active_meeting(None);
                        task_bridge.0.active.store(false, Ordering::Release);
                        task_bridge.0.transcribing.store(false, Ordering::Release);
                        task_bridge.clear_stop_if(&stop);
                        match result {
                            Ok(()) => publish_store(
                                &task_bridge,
                                &task_store,
                                None,
                                &[],
                                None,
                                Some("Imported meeting saved locally.".to_string()),
                            ),
                            Err(error) => {
                                log::error!("meeting import: {error}");
                                let _ = task_store.recover_interrupted(now_ms());
                                publish_store(
                                    &task_bridge,
                                    &task_store,
                                    None,
                                    &[],
                                    Some(error),
                                    None,
                                );
                            }
                        }
                    });
                match spawned {
                    Ok(worker) => task = Some(worker),
                    Err(error) => {
                        bridge.clear_stop_if(&spawn_failure_stop);
                        bridge.0.active.store(false, Ordering::Release);
                        publish_store(
                            &bridge,
                            &store,
                            selected.as_ref(),
                            &search,
                            Some(format!("Could not start the import worker: {error}")),
                            None,
                        );
                    }
                }
            }
            Command::Select(id) => {
                selected = Some(id);
                publish_store(&bridge, &store, selected.as_ref(), &search, None, None);
            }
            Command::Refresh(notice) => {
                publish_store(
                    &bridge,
                    &store,
                    selected.as_ref(),
                    &search,
                    None,
                    Some(notice),
                );
            }
            Command::Delete(id) => {
                if bridge.is_active() {
                    publish_store(
                        &bridge,
                        &store,
                        selected.as_ref(),
                        &search,
                        Some("Stop recording before deleting a meeting.".to_string()),
                        None,
                    );
                } else {
                    match crate::calendar::delete_meeting_and_link(&store, &id) {
                        Ok(cleanup_warning) => {
                            if selected.as_ref() == Some(&id) {
                                selected = None;
                            }
                            publish_store(
                                &bridge,
                                &store,
                                selected.as_ref(),
                                &search,
                                None,
                                Some(cleanup_warning.unwrap_or_else(|| {
                                    "Meeting deleted from this device.".to_string()
                                })),
                            );
                        }
                        Err(error) => publish_store(
                            &bridge,
                            &store,
                            selected.as_ref(),
                            &search,
                            Some(format!("Could not delete meeting: {error}")),
                            None,
                        ),
                    }
                }
            }
            Command::Rename(_, _) if bridge.is_active() => publish_store(
                &bridge,
                &store,
                selected.as_ref(),
                &search,
                Some("Stop the current meeting before renaming it.".to_string()),
                None,
            ),
            Command::Rename(id, title) => match store.load(&id) {
                Ok(mut meeting) => {
                    meeting.title = title.trim().to_string();
                    meeting.updated_at_ms = now_ms();
                    match store.save(&meeting) {
                        Ok(()) => publish_store(
                            &bridge,
                            &store,
                            selected.as_ref(),
                            &search,
                            None,
                            Some("Meeting renamed.".to_string()),
                        ),
                        Err(error) => publish_store(
                            &bridge,
                            &store,
                            selected.as_ref(),
                            &search,
                            Some(format!("Could not rename meeting: {error}")),
                            None,
                        ),
                    }
                }
                Err(error) => publish_store(
                    &bridge,
                    &store,
                    selected.as_ref(),
                    &search,
                    Some(format!("Could not load meeting: {error}")),
                    None,
                ),
            },
            Command::RenameSpeaker(_, _, _) if bridge.is_active() => publish_store(
                &bridge,
                &store,
                selected.as_ref(),
                &search,
                Some("Stop the current meeting before renaming a speaker.".to_string()),
                None,
            ),
            Command::RenameSpeaker(id, speaker_id, label) => match store.load(&id) {
                Ok(mut meeting) => {
                    let result = meeting
                        .speakers
                        .iter()
                        .position(|speaker| speaker.id == speaker_id)
                        .ok_or_else(|| "That speaker is no longer in this meeting.".to_string())
                        .and_then(|index| {
                            let label = label.trim();
                            if label.is_empty() || label.len() > 128 {
                                return Err("Enter a speaker name up to 128 bytes.".to_string());
                            }
                            meeting.speakers[index].label = label.to_string();
                            meeting.updated_at_ms = now_ms();
                            store.save(&meeting).map_err(|error| error.to_string())
                        });
                    match result {
                        Ok(()) => publish_store(
                            &bridge,
                            &store,
                            selected.as_ref(),
                            &search,
                            None,
                            Some("Speaker renamed.".to_string()),
                        ),
                        Err(error) => publish_store(
                            &bridge,
                            &store,
                            selected.as_ref(),
                            &search,
                            Some(error),
                            None,
                        ),
                    }
                }
                Err(error) => publish_store(
                    &bridge,
                    &store,
                    selected.as_ref(),
                    &search,
                    Some(format!("Could not load meeting: {error}")),
                    None,
                ),
            },
            Command::Bookmark(_, _, _) if bridge.is_active() => publish_store(
                &bridge,
                &store,
                selected.as_ref(),
                &search,
                Some("Stop the current meeting before adding a bookmark.".to_string()),
                None,
            ),
            Command::Bookmark(id, at_ms, label) => match store.load(&id) {
                Ok(mut meeting) => {
                    meeting.bookmarks.push(vocalcode_meeting::Bookmark {
                        at_ms,
                        label: truncate_utf8(&label, 256).to_string(),
                    });
                    meeting.updated_at_ms = now_ms();
                    match store.save(&meeting) {
                        Ok(()) => publish_store(
                            &bridge,
                            &store,
                            selected.as_ref(),
                            &search,
                            None,
                            Some("Bookmark added.".to_string()),
                        ),
                        Err(error) => publish_store(
                            &bridge,
                            &store,
                            selected.as_ref(),
                            &search,
                            Some(format!("Could not add bookmark: {error}")),
                            None,
                        ),
                    }
                }
                Err(error) => publish_store(
                    &bridge,
                    &store,
                    selected.as_ref(),
                    &search,
                    Some(format!("Could not load meeting: {error}")),
                    None,
                ),
            },
            Command::Search(query) => {
                let meetings = load_all(&store);
                search = search_meetings(&query, &meetings)
                    .into_iter()
                    .map(|hit| {
                        json!({
                            "id": hit.meeting_id,
                            "title": hit.title,
                            "score": hit.score,
                            "segment_id": hit.matched_segment_id,
                            "at_ms": hit.matched_at_ms,
                            "excerpt": hit.excerpt,
                        })
                    })
                    .collect();
                publish_store(&bridge, &store, selected.as_ref(), &search, None, None);
            }
            Command::Export(id, kind, path) => {
                let result = export_to(&store, &id, kind, &path);
                let (error, notice) = match result {
                    Ok(()) => (None, Some(format!("Exported to {}", path.display()))),
                    Err(error) => (Some(error), None),
                };
                publish_store(&bridge, &store, selected.as_ref(), &search, error, notice);
            }
            Command::Shutdown => {
                if let Some(stop) = bridge
                    .0
                    .stop
                    .lock()
                    .unwrap_or_else(|value| value.into_inner())
                    .as_ref()
                {
                    stop.store(true, Ordering::Release);
                }
                if let Some(worker) = task.take() {
                    let _ = worker.join();
                }
                return;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_live(
    store: &MeetingStore,
    bridge: &Bridge,
    asr: &mpsc::SyncSender<MeetingAsrRequest>,
    stop: Arc<AtomicBool>,
    title: String,
    microphone: bool,
    system_audio: bool,
    microphone_selector: Option<String>,
    keep_audio: bool,
    language: String,
    auto_end_minutes: u64,
) -> Result<(), String> {
    let retention = if keep_audio {
        AudioRetention::KeepUntilDeleted
    } else {
        AudioRetention::DeleteAfterTranscription
    };
    let mut meeting = store
        .create(NewMeeting {
            title,
            now_ms: now_ms(),
            source: MeetingSource::Live {
                microphone,
                system_audio,
            },
            language,
            audio_retention: retention,
        })
        .map_err(|error| error.to_string())?;
    if microphone {
        meeting.speakers.push(Speaker {
            id: "you".to_string(),
            label: "You".to_string(),
            source: AudioSource::Microphone,
        });
    }
    store.save(&meeting).map_err(|error| error.to_string())?;
    bridge.set_active_meeting(Some(meeting.id.clone()));
    let audio_dir = store
        .audio_directory(&meeting.id)
        .map_err(|error| error.to_string())?;
    let (sender, receiver) = mpsc::sync_channel(AUDIO_QUEUE_CAPACITY);
    let capture = match start_meeting_audio(
        microphone_selector.as_deref(),
        microphone,
        system_audio,
        sender,
    ) {
        Ok(capture) => capture,
        Err(error) => {
            meeting.status = MeetingStatus::Failed;
            meeting.error = Some(error.to_string());
            meeting.updated_at_ms = now_ms();
            store
                .save(&meeting)
                .map_err(|problem| problem.to_string())?;
            return Err(error.to_string());
        }
    };
    let mut tracks =
        TrackSet::new(&audio_dir, microphone, system_audio).map_err(|error| error.to_string())?;
    tracks.detector =
        crate::noise_filter::meeting_detector(store.root().parent().unwrap_or(store.root()));
    if tracks.detector.is_none() {
        meeting.warnings.push("Local speech detection is unavailable. Recognition is preserved; silence auto-end is disabled for safety.".into());
    }
    // Disjoint token ranges across meetings prevent delayed popup events from
    // ever stopping the next recording, including after a quick stop/start.
    static AUTO_END_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let mut auto_end = AutoEnd::new(
        auto_end_minutes,
        AUTO_END_IDS.fetch_add(1_000_000, Ordering::Relaxed),
    );
    let capture_clock = Instant::now();
    publish_active(bridge, store, &meeting, "recording", None);
    let mut capture_failure = None;
    let mut transcription = LiveTranscription::default();
    let mut last_checkpoint = std::time::Instant::now();

    while !stop.load(Ordering::Acquire) {
        let elapsed = capture_clock.elapsed().as_millis() as u64;
        let mut should_end = auto_end.tick(
            elapsed,
            std::mem::take(&mut tracks.speech_seen),
            tracks.monitor_healthy(),
        );
        // A word may have started in the unfinished one-second VAD window.
        // Inspect it with detector-only padding before accepting the deadline.
        if should_end && tracks.pending_speech() {
            should_end = auto_end.tick(elapsed, true, tracks.monitor_healthy());
        }
        let actions = std::mem::take(
            &mut *bridge
                .0
                .auto_end_actions
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
        );
        let explicit_stop = actions.into_iter().fold(false, |end, (id, action)| {
            auto_end.action(id, action, elapsed) || end
        });
        bridge.set_auto_end_notice(auto_end.notice(elapsed));
        // Evaluate the deadline again after Continue/Disable; a boundary click
        // processed in this tick has precedence over an automatic stop.
        if explicit_stop || (should_end && auto_end.tick(elapsed, false, tracks.monitor_healthy()))
        {
            meeting.end_reason = Some(
                if explicit_stop {
                    "auto_end_confirmed"
                } else {
                    "silence_auto_end"
                }
                .into(),
            );
            stop.store(true, Ordering::Release);
            break;
        }
        if let Some(error) = capture.take_error() {
            capture_failure = Some(error);
            stop.store(true, Ordering::Release);
            break;
        }
        if let Err(error) = transcription.tick(store, bridge, asr, &mut meeting) {
            capture_failure = Some(error);
            break;
        }
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(block) => {
                meeting.duration_ms = now_ms().saturating_sub(meeting.started_at_ms);
                let result = tracks
                    .push(block)
                    .map_err(|error| error.to_string())
                    .and_then(|segments| transcription.enqueue(segments));
                if let Err(error) = result {
                    capture_failure = Some(error);
                    break;
                }
                tracks.collect_warnings(&mut meeting);
                apply_pending_bookmarks(bridge, store, &mut meeting)?;
                if last_checkpoint.elapsed() >= Duration::from_secs(5) {
                    // Silence/slow ASR must still persist recording progress.
                    meeting.updated_at_ms = now_ms();
                    store.save(&meeting).map_err(|error| error.to_string())?;
                    publish_active(bridge, store, &meeting, "recording", None);
                    last_checkpoint = std::time::Instant::now();
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // Stop the devices before waiting for any model. Freeze the recording clock
    // here: draining/transcribing the backlog is not additional meeting time.
    capture_failure = capture_failure.or_else(|| capture.take_error());
    bridge.set_auto_end_notice(None);
    drop(capture);
    meeting.duration_ms = now_ms().saturating_sub(meeting.started_at_ms);
    meeting.ended_at_ms = Some(meeting.started_at_ms.saturating_add(meeting.duration_ms));
    meeting.status = MeetingStatus::Processing;
    store.save(&meeting).map_err(|error| error.to_string())?;
    publish_active(bridge, store, &meeting, "processing", None);
    while let Ok(block) = receiver.try_recv() {
        match tracks.push(block) {
            Ok(segments) if capture_failure.is_none() => {
                if let Err(error) = transcription.enqueue(segments) {
                    capture_failure = Some(error);
                }
            }
            Ok(_) => {} // Persist queued audio even when ASR can no longer keep up.
            Err(error) => {
                capture_failure.get_or_insert_with(|| error.to_string());
            }
        }
    }
    match tracks.finish() {
        Ok(segments) if capture_failure.is_none() => {
            if let Err(error) = transcription.enqueue(segments) {
                capture_failure = Some(error);
            }
        }
        Ok(_) => {}
        Err(error) => {
            capture_failure.get_or_insert_with(|| error.to_string());
        }
    }
    tracks.collect_warnings(&mut meeting);
    apply_pending_bookmarks(bridge, store, &mut meeting)?;
    while capture_failure.is_none() && !transcription.is_empty() {
        if bridge.0.shutting_down.load(Ordering::Acquire) {
            capture_failure = Some(
                "VocalCode is closing before the remaining speech finished transcribing".into(),
            );
            break;
        }
        if let Err(error) = transcription.tick(store, bridge, asr, &mut meeting) {
            capture_failure = Some(error);
        }
        thread::sleep(Duration::from_millis(10));
    }
    if let Some(error) = capture_failure {
        let message = preserve_failed_capture(store, bridge, &mut meeting, &error)?;
        return Err(message);
    }
    complete_meeting(store, bridge, &mut meeting)?;
    if retention == AudioRetention::DeleteAfterTranscription {
        if let Err(error) = store.delete_audio(&meeting.id) {
            let message = format!(
                "The transcript and notes were saved, but VocalCode could not delete the temporary meeting audio: {error}"
            );
            meeting.status = MeetingStatus::Failed;
            meeting.updated_at_ms = now_ms();
            meeting.error = Some(message.clone());
            store
                .save(&meeting)
                .map_err(|problem| problem.to_string())?;
            return Err(message);
        }
    }
    Ok(())
}

fn run_import(
    store: &MeetingStore,
    bridge: &Bridge,
    asr: &mpsc::SyncSender<MeetingAsrRequest>,
    stop: Arc<AtomicBool>,
    path: PathBuf,
    title: String,
    language: String,
) -> Result<(), String> {
    let detector =
        crate::noise_filter::meeting_detector(store.root().parent().unwrap_or(store.root()));
    run_import_with_detector(store, bridge, asr, stop, path, title, language, detector)
}

#[allow(clippy::too_many_arguments)]
fn run_import_with_detector(
    store: &MeetingStore,
    bridge: &Bridge,
    asr: &mpsc::SyncSender<MeetingAsrRequest>,
    stop: Arc<AtomicBool>,
    path: PathBuf,
    title: String,
    language: String,
    mut detector: Option<SpeechGate>,
) -> Result<(), String> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Imported audio")
        .to_string();
    let mut meeting = store
        .create(NewMeeting {
            title,
            now_ms: now_ms(),
            source: MeetingSource::Imported { file_name },
            language,
            audio_retention: AudioRetention::DeleteAfterTranscription,
        })
        .map_err(|error| error.to_string())?;
    meeting.status = MeetingStatus::Processing;
    store.save(&meeting).map_err(|error| error.to_string())?;
    bridge.set_active_meeting(Some(meeting.id.clone()));
    publish_active(bridge, store, &meeting, "importing", None);
    let mut resampler = LinearMonoResampler::default();
    let mut segmenter = SpeechSegmenter::default();
    let mut clusterer = OnlineSpeakerClusterer::default();
    if detector.is_none() {
        meeting.warnings.push("Local speech detection is unavailable; imported audio was passed to recognition without acoustic filtering.".into());
    }
    let result = decode_audio_file(&path, |block| {
        if stop.load(Ordering::Acquire) {
            return Err(vocalcode_meeting::MeetingError::Invalid(
                "import cancelled".to_string(),
            ));
        }
        let block_frames = block.samples.len() as u64 / block.channels.max(1) as u64;
        meeting.duration_ms = meeting.duration_ms.max(
            block
                .start_ms
                .saturating_add(block_frames.saturating_mul(1_000) / u64::from(block.sample_rate)),
        );
        let normalized = resampler.push(&block)?;
        for segment in segmenter.push(&normalized) {
            if !permit_meeting_audio(&mut detector, &segment.samples) {
                meeting.filtered_noise_segments += 1;
                continue;
            }
            let draft = draft_segment(AudioSource::Imported, segment, &mut clusterer);
            process_one(store, bridge, asr, &mut meeting, draft, &stop)
                .map_err(vocalcode_meeting::MeetingError::AudioDecode)?;
        }
        apply_pending_bookmarks(bridge, store, &mut meeting)
            .map_err(vocalcode_meeting::MeetingError::AudioDecode)?;
        Ok(())
    });
    if let Err(error) = result {
        meeting.status = if stop.load(Ordering::Acquire) {
            MeetingStatus::Interrupted
        } else {
            MeetingStatus::Failed
        };
        meeting.error = Some(error.to_string());
        meeting.updated_at_ms = now_ms();
        store
            .save(&meeting)
            .map_err(|problem| problem.to_string())?;
        return Err(error.to_string());
    }
    let tail = resampler.finish();
    let mut remaining = segmenter.push(&tail);
    remaining.extend(segmenter.finish());
    for segment in remaining {
        if !permit_meeting_audio(&mut detector, &segment.samples) {
            meeting.filtered_noise_segments += 1;
            continue;
        }
        let draft = draft_segment(AudioSource::Imported, segment, &mut clusterer);
        process_one(store, bridge, asr, &mut meeting, draft, &stop)?;
    }
    apply_pending_bookmarks(bridge, store, &mut meeting)?;
    complete_meeting(store, bridge, &mut meeting)
}

fn apply_pending_bookmarks(
    bridge: &Bridge,
    store: &MeetingStore,
    meeting: &mut Meeting,
) -> Result<(), String> {
    let pending = {
        let mut pending = bridge
            .0
            .pending_bookmarks
            .lock()
            .unwrap_or_else(|value| value.into_inner());
        std::mem::take(&mut *pending)
    };
    if pending.is_empty() {
        return Ok(());
    }
    for (at_ms, label) in pending {
        meeting.bookmarks.push(vocalcode_meeting::Bookmark {
            at_ms: at_ms.min(meeting.duration_ms),
            label,
        });
    }
    meeting.updated_at_ms = now_ms();
    store.save(meeting).map_err(|error| error.to_string())?;
    publish_active(
        bridge,
        store,
        meeting,
        "recording",
        Some("Bookmark saved.".to_string()),
    );
    Ok(())
}

struct Track {
    source: AudioSource,
    resampler: LinearMonoResampler,
    segmenter: SpeechSegmenter,
    writer: ChunkedPcmWriter,
    clusterer: OnlineSpeakerClusterer,
    activity_samples: Vec<f32>,
    last_audio: Option<Instant>,
}

struct TrackSet {
    tracks: HashMap<AudioSource, Track>,
    echo: Option<RealtimeEchoCanceller>,
    detector: Option<SpeechGate>,
    speech_seen: bool,
    filtered: u64,
    warnings: Vec<String>,
}

fn permit_meeting_audio(detector: &mut Option<SpeechGate>, samples: &[f32]) -> bool {
    detector
        .as_mut()
        .is_none_or(|gate| gate.classify_meeting(samples, 16_000).permits_asr())
}

impl TrackSet {
    fn new(root: &Path, microphone: bool, system_audio: bool) -> vocalcode_meeting::Result<Self> {
        let mut tracks = HashMap::new();
        for (enabled, source) in [
            (microphone, AudioSource::Microphone),
            (system_audio, AudioSource::System),
        ] {
            if enabled {
                tracks.insert(
                    source,
                    Track {
                        source,
                        resampler: LinearMonoResampler::default(),
                        segmenter: SpeechSegmenter::default(),
                        // A short meeting must leave recoverable audio before
                        // Stop is pressed. Only the current <=10 s chunk is volatile.
                        writer: ChunkedPcmWriter::with_chunk_samples(root, source, 16_000 * 10)?,
                        clusterer: OnlineSpeakerClusterer::default(),
                        activity_samples: Vec::new(),
                        last_audio: None,
                    },
                );
            }
        }
        Ok(Self {
            tracks,
            echo: (microphone && system_audio).then(RealtimeEchoCanceller::new),
            detector: None,
            speech_seen: false,
            filtered: 0,
            warnings: Vec::new(),
        })
    }

    fn push(&mut self, block: StreamedAudioBlock) -> vocalcode_meeting::Result<Vec<Draft>> {
        let source = match block.source {
            StreamedAudioSource::Microphone => AudioSource::Microphone,
            StreamedAudioSource::System => AudioSource::System,
        };
        if !block.samples.is_empty() {
            if let Some(track) = self.tracks.get_mut(&source) {
                track.last_audio = Some(Instant::now());
            }
        }
        let block = AudioBlock {
            samples: block.samples,
            sample_rate: block.sample_rate,
            channels: block.channels,
            start_ms: block.start_ms,
        };

        let Some(echo) = self.echo.as_mut() else {
            return self.push_raw(source, &block);
        };
        let echo_output = echo.push(source, &block)?;
        if let Some(problem) = echo_output.degradation.as_deref() {
            log::warn!("meeting acoustic echo cancellation disabled; continuing raw: {problem}");
            self.warnings.push(format!("Echo cancellation fell back to raw microphone audio: {problem}. Use headphones to reduce duplicate voices."));
        }

        let mut drafts = Vec::new();
        if source == AudioSource::System {
            // The clean loopback track is the AEC reference, but must still be
            // persisted and transcribed unchanged as the remote participants.
            drafts.extend(self.push_raw(source, &block)?);
        }
        if !echo_output.microphone_samples.is_empty() {
            drafts.extend(
                self.push_normalized(AudioSource::Microphone, &echo_output.microphone_samples)?,
            );
        }
        drafts.sort_by_key(|draft| draft.segment.start_ms);
        Ok(drafts)
    }

    fn push_raw(
        &mut self,
        source: AudioSource,
        block: &AudioBlock,
    ) -> vocalcode_meeting::Result<Vec<Draft>> {
        let track = self.tracks.get_mut(&source).ok_or_else(|| {
            vocalcode_meeting::MeetingError::Invalid(
                "audio arrived for a disabled meeting track".to_string(),
            )
        })?;
        let samples = track.resampler.push(block)?;
        self.push_normalized(source, &samples)
    }

    fn push_normalized(
        &mut self,
        source: AudioSource,
        samples: &[f32],
    ) -> vocalcode_meeting::Result<Vec<Draft>> {
        let track = self.tracks.get_mut(&source).ok_or_else(|| {
            vocalcode_meeting::MeetingError::Invalid(
                "processed audio targeted a disabled meeting track".to_string(),
            )
        })?;
        let _ = track.writer.push(samples)?;
        // Check fresh 1-second windows, not just completed ASR segments (which
        // may be 45 seconds long). Resumed speech must cancel a stop promptly.
        if let Some(detector) = &mut self.detector {
            track.activity_samples.extend_from_slice(samples);
            let complete = track.activity_samples.len() / 16_000 * 16_000;
            for window in track.activity_samples[..complete].chunks_exact(16_000) {
                if detector.classify(window, 16_000) != Decision::NoSpeech {
                    self.speech_seen = true;
                }
            }
            track.activity_samples.drain(..complete);
        }
        Ok(filtered_drafts(
            source,
            track.segmenter.push(samples),
            &mut track.clusterer,
            &mut self.detector,
            &mut self.filtered,
        ))
    }

    fn monitor_healthy(&self) -> bool {
        self.detector.is_some()
            && self.tracks.values().all(|track| {
                track
                    .last_audio
                    .is_some_and(|time| time.elapsed() < Duration::from_secs(3))
            })
    }

    fn pending_speech(&mut self) -> bool {
        let Some(detector) = &mut self.detector else {
            return true;
        };
        self.tracks.values().any(|track| {
            if track.activity_samples.is_empty() {
                return false;
            }
            let mut padded = vec![0.0; 16_000];
            let count = track.activity_samples.len().min(padded.len());
            padded[..count].copy_from_slice(&track.activity_samples[..count]);
            detector.classify(&padded, 16_000).permits_asr()
        })
    }

    fn collect_warnings(&mut self, meeting: &mut Meeting) {
        meeting.filtered_noise_segments += std::mem::take(&mut self.filtered);
        for warning in self.warnings.drain(..) {
            if !meeting.warnings.contains(&warning) {
                meeting.warnings.push(warning);
            }
        }
    }

    fn finish(&mut self) -> vocalcode_meeting::Result<Vec<Draft>> {
        let mut output = Vec::new();
        if let Some(echo) = self.echo.as_mut() {
            let tail = echo.finish()?;
            let stats = echo.stats();
            if let Some(problem) = tail.degradation.as_deref() {
                self.warnings.push(format!("Echo cancellation fell back to raw microphone audio while finishing: {problem}."));
                log::warn!(
                    "meeting acoustic echo cancellation disabled while finishing; continuing raw: {problem}"
                );
            }
            log::info!(
                "meeting AEC completed: active={}, render_frames={}, capture_frames={}, estimated_delay_ms={:?}, protected_capture_frames={}",
                stats.active,
                stats.render_frames,
                stats.capture_frames,
                stats.estimated_delay_ms,
                stats.protected_capture_frames
            );
            if !tail.microphone_samples.is_empty() {
                output.extend(
                    self.push_normalized(AudioSource::Microphone, &tail.microphone_samples)?,
                );
            }
        }
        for track in self.tracks.values_mut() {
            let tail = track.resampler.finish();
            let _ = track.writer.push(&tail)?;
            let mut segments = track.segmenter.push(&tail);
            segments.extend(track.segmenter.finish());
            output.extend(filtered_drafts(
                track.source,
                segments,
                &mut track.clusterer,
                &mut self.detector,
                &mut self.filtered,
            ));
            let _ = track.writer.finish()?;
        }
        output.sort_by_key(|draft| draft.segment.start_ms);
        Ok(output)
    }
}

struct Draft {
    source: AudioSource,
    segment: SpeechSegment,
    speaker_id: String,
}

fn filtered_drafts(
    source: AudioSource,
    segments: Vec<SpeechSegment>,
    clusterer: &mut OnlineSpeakerClusterer,
    detector: &mut Option<SpeechGate>,
    filtered: &mut u64,
) -> Vec<Draft> {
    segments
        .into_iter()
        .filter_map(|segment| {
            if permit_meeting_audio(detector, &segment.samples) {
                Some(draft_segment(source, segment, clusterer))
            } else {
                // Noise must not become a speaker centroid and contaminate later
                // real participants. Filtering belongs before speaker assignment.
                *filtered += 1;
                None
            }
        })
        .collect()
}

/// Capture must never block on native inference. At most one request is in
/// flight; the bounded queue holds already-persisted audio drafts. Overflow
/// stops capture explicitly and preserves the audio for recovery.
#[derive(Default)]
struct LiveTranscription {
    queued: VecDeque<Draft>,
    pending: Option<(Draft, mpsc::Receiver<Result<String, String>>)>,
}

impl LiveTranscription {
    fn is_empty(&self) -> bool {
        self.queued.is_empty() && self.pending.is_none()
    }

    fn enqueue(&mut self, drafts: Vec<Draft>) -> Result<(), String> {
        if self.queued.len() + drafts.len() + usize::from(self.pending.is_some())
            > MAX_PENDING_SEGMENTS
        {
            return Err("The speech model could not keep up with meeting audio; recording stopped and audio was retained locally.".into());
        }
        self.queued.extend(drafts);
        Ok(())
    }

    fn tick(
        &mut self,
        store: &MeetingStore,
        bridge: &Bridge,
        asr: &mpsc::SyncSender<MeetingAsrRequest>,
        meeting: &mut Meeting,
    ) -> Result<(), String> {
        if let Some((_, receiver)) = self.pending.as_ref() {
            match receiver.try_recv() {
                Ok(result) => {
                    let (draft, _) = self.pending.take().expect("pending reply owner");
                    bridge.0.transcribing.store(false, Ordering::Release);
                    save_transcription(store, bridge, meeting, draft, result?)?;
                }
                Err(mpsc::TryRecvError::Empty) => return Ok(()),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("The speech engine dropped the meeting segment.".into())
                }
            }
        }
        if let Some(mut draft) = self.queued.pop_front() {
            let (reply, receiver) = mpsc::sync_channel(1);
            let request = MeetingAsrRequest {
                samples: meeting_asr_samples(std::mem::take(&mut draft.segment.samples)),
                sample_rate: 16_000,
                reply,
            };
            match asr.try_send(request) {
                Ok(()) => {
                    self.pending = Some((draft, receiver));
                    bridge.0.transcribing.store(true, Ordering::Release);
                    publish_active(bridge, store, meeting, "transcribing", None);
                }
                Err(mpsc::TrySendError::Full(request)) => {
                    draft.segment.samples = request.samples;
                    self.queued.push_front(draft);
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err("The speech engine stopped.".into())
                }
            }
        }
        Ok(())
    }
}

fn process_one(
    store: &MeetingStore,
    bridge: &Bridge,
    asr: &mpsc::SyncSender<MeetingAsrRequest>,
    meeting: &mut Meeting,
    mut draft: Draft,
    stop: &AtomicBool,
) -> Result<(), String> {
    bridge.0.transcribing.store(true, Ordering::Release);
    publish_active(bridge, store, meeting, "transcribing", None);
    let text = request_asr(asr, std::mem::take(&mut draft.segment.samples), stop);
    bridge.0.transcribing.store(false, Ordering::Release);
    save_transcription(store, bridge, meeting, draft, text?)
}

fn save_transcription(
    store: &MeetingStore,
    bridge: &Bridge,
    meeting: &mut Meeting,
    draft: Draft,
    text: String,
) -> Result<(), String> {
    let Draft {
        source,
        segment: speech,
        speaker_id,
    } = draft;
    if vocalcode_meeting::quality::punctuation_only(&text) {
        meeting.filtered_noise_segments += 1;
        return Ok(());
    }
    let new_speaker = !meeting
        .speakers
        .iter()
        .any(|speaker| speaker.id == speaker_id);
    if new_speaker {
        meeting.speakers.push(Speaker {
            id: speaker_id.clone(),
            label: if speaker_id == "you" {
                "You".to_string()
            } else {
                format!(
                    "Speaker {}",
                    meeting
                        .speakers
                        .iter()
                        .filter(|speaker| speaker.id.starts_with("speaker-"))
                        .count()
                        + 1
                )
            },
            source,
        });
        // Commit a new speaker before the transcript record that references it.
        // A power loss can then leave an unused speaker, never an unloadable
        // transcript with a dangling speaker identifier.
        meeting.updated_at_ms = now_ms();
        store.save(meeting).map_err(|error| error.to_string())?;
    }
    let segment = TranscriptSegment {
        id: meeting
            .segments
            .last()
            .map(|segment| segment.id + 1)
            .unwrap_or(1),
        start_ms: speech.start_ms,
        end_ms: speech.end_ms,
        speaker_id,
        source,
        text,
    };
    store
        .append_segment(&meeting.id, &segment)
        .map_err(|error| error.to_string())?;
    meeting.duration_ms = meeting.duration_ms.max(segment.end_ms);
    meeting.segments.push(segment);
    meeting.segment_count = meeting.segments.len();
    meeting.updated_at_ms = now_ms();
    store.save(meeting).map_err(|error| error.to_string())?;
    publish_active(bridge, store, meeting, "recording", None);
    Ok(())
}

fn draft_segment(
    source: AudioSource,
    segment: SpeechSegment,
    clusterer: &mut OnlineSpeakerClusterer,
) -> Draft {
    let speaker_id = if source == AudioSource::Microphone {
        "you".to_string()
    } else {
        Voiceprint::from_samples(&segment.samples)
            .ok()
            .map(|voiceprint| clusterer.assign(&voiceprint))
            .map(|index| format!("speaker-{}", index + 1))
            .unwrap_or_else(|| "speaker-1".to_string())
    };
    Draft {
        source,
        segment,
        speaker_id,
    }
}

// Recognition-only trailing context stabilizes very short offline utterances.
// Keep original timestamps, audio chunks, speaker features and VAD unchanged.
fn meeting_asr_samples(mut samples: Vec<f32>) -> Vec<f32> {
    if (3_200..16_000).contains(&samples.len()) {
        samples.resize(24_000, 0.0);
    }
    samples
}

fn request_asr(
    sender: &mpsc::SyncSender<MeetingAsrRequest>,
    samples: Vec<f32>,
    stop: &AtomicBool,
) -> Result<String, String> {
    let (reply, receiver) = mpsc::sync_channel(1);
    let mut request = MeetingAsrRequest {
        samples: meeting_asr_samples(samples),
        sample_rate: 16_000,
        reply,
    };
    loop {
        if stop.load(Ordering::Acquire) {
            return Err("Meeting import cancelled.".into());
        }
        match sender.try_send(request) {
            Ok(()) => break,
            Err(mpsc::TrySendError::Full(returned)) => {
                request = returned;
                thread::sleep(Duration::from_millis(10));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Err("The speech engine stopped.".into())
            }
        }
    }
    loop {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) if stop.load(Ordering::Acquire) => {
                // Drop only this reply receiver, not the native decoder. Its
                // single owner finishes safely and ignores the abandoned reply.
                return Err("Meeting import cancelled.".into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("The speech engine dropped the meeting segment.".to_string())
            }
        }
    }
}

fn complete_meeting(
    store: &MeetingStore,
    bridge: &Bridge,
    meeting: &mut Meeting,
) -> Result<(), String> {
    let cleaned = deduplicate_live_tracks(&meeting.segments);
    if cleaned.len() != meeting.segments.len() {
        store
            .replace_transcript(&meeting.id, &cleaned)
            .map_err(|error| error.to_string())?;
        meeting.segments = cleaned;
        meeting.segment_count = meeting.segments.len();
    }
    meeting.summary = Some(build_local_summary(
        &meeting.segments,
        SummaryOptions::default(),
    ));
    meeting.status = MeetingStatus::Completed;
    meeting.updated_at_ms = now_ms();
    meeting.ended_at_ms = Some(meeting.started_at_ms.saturating_add(meeting.duration_ms));
    meeting.error = None;
    store.save(meeting).map_err(|error| error.to_string())?;
    publish_active(
        bridge,
        store,
        meeting,
        "completed",
        Some("Local summary created from transcript excerpts.".to_string()),
    );
    Ok(())
}

/// Removes only likely acoustic echo captured by both live tracks. This is
/// deliberately completion-time cleanup: the append-only transcript remains
/// the crash-recovery source of truth while recording.
fn deduplicate_live_tracks(segments: &[TranscriptSegment]) -> Vec<TranscriptSegment> {
    const MAX_RECENT_COMPARISONS: usize = 32;
    let mut ordered = segments.to_vec();
    ordered.sort_by_key(|segment| (segment.start_ms, segment.id));
    let mut kept: Vec<TranscriptSegment> = Vec::with_capacity(ordered.len());

    for segment in ordered {
        let duplicate = kept
            .iter()
            .enumerate()
            .rev()
            .take(MAX_RECENT_COMPARISONS)
            .find(|(_, previous)| cross_track_duplicate(previous, &segment))
            .map(|(index, _)| index);
        match duplicate {
            Some(index)
                if segment.source == AudioSource::System
                    && kept[index].source == AudioSource::Microphone =>
            {
                // Loopback audio is normally cleaner than sound re-captured by
                // the microphone, so preserve its text and speaker assignment.
                kept[index] = segment;
            }
            Some(_) => {}
            None => kept.push(segment),
        }
    }
    kept.sort_by_key(|segment| (segment.start_ms, segment.id));
    kept
}

fn cross_track_duplicate(left: &TranscriptSegment, right: &TranscriptSegment) -> bool {
    let live_pair = matches!(
        (left.source, right.source),
        (AudioSource::Microphone, AudioSource::System)
            | (AudioSource::System, AudioSource::Microphone)
    );
    if !live_pair {
        return false;
    }
    let overlap = left
        .end_ms
        .min(right.end_ms)
        .saturating_sub(left.start_ms.max(right.start_ms));
    let shorter = left
        .end_ms
        .saturating_sub(left.start_ms)
        .min(right.end_ms.saturating_sub(right.start_ms));
    let temporally_close = shorter > 0 && overlap.saturating_mul(2) >= shorter;
    temporally_close && similar_transcript(&left.text, &right.text)
}

fn similar_transcript(left: &str, right: &str) -> bool {
    const MAX_COMPARE_CHARACTERS: usize = 512;
    let normalize = |value: &str| {
        let characters: Vec<_> = value.chars().collect();
        characters
            .iter()
            .enumerate()
            .filter(|(index, character)| {
                character.is_alphanumeric()
                    || matches!(character, '+' | '-' | '−' | '%' | '$' | '€' | '¥')
                    || (matches!(character, '.' | ',' | ':' | '/')
                        && characters
                            .get(index.saturating_sub(1))
                            .is_some_and(|c| c.is_ascii_digit())
                        && characters
                            .get(index + 1)
                            .is_some_and(|c| c.is_ascii_digit()))
            })
            .map(|(_, character)| *character)
            .flat_map(char::to_lowercase)
            .take(MAX_COMPARE_CHARACTERS + 1)
            .collect::<Vec<_>>()
    };
    let left = normalize(left);
    let right = normalize(right);
    if left.len() > MAX_COMPARE_CHARACTERS
        || right.len() > MAX_COMPARE_CHARACTERS
        || left.len() < 2
        || right.len() < 2
    {
        return false;
    }
    // Fuzzy overlap can silently erase a correction ("approve" vs "do not
    // approve", or 1200 vs 12000). AEC handles acoustic echo; text cleanup only
    // removes exact normalized duplicates. Short acknowledgments are ambiguous.
    left.len() >= 4 && left == right
}

fn preserve_failed_capture(
    store: &MeetingStore,
    bridge: &Bridge,
    meeting: &mut Meeting,
    source_error: &str,
) -> Result<String, String> {
    let message = format!(
        "Meeting did not finish successfully: {source_error}. The partial transcript and temporary audio were retained locally; delete this meeting to remove them."
    );
    meeting.summary = Some(build_local_summary(
        &meeting.segments,
        SummaryOptions::default(),
    ));
    meeting.status = MeetingStatus::Failed;
    meeting.audio_retention = AudioRetention::KeepUntilDeleted;
    meeting.updated_at_ms = now_ms();
    meeting.ended_at_ms = Some(meeting.started_at_ms.saturating_add(meeting.duration_ms));
    meeting.error = Some(message.clone());
    store.save(meeting).map_err(|error| error.to_string())?;
    publish_active(
        bridge,
        store,
        meeting,
        "failed",
        Some("Partial local notes were saved after the audio capture error.".to_string()),
    );
    Ok(message)
}

fn publish_active(
    bridge: &Bridge,
    store: &MeetingStore,
    meeting: &Meeting,
    phase: &str,
    notice: Option<String>,
) {
    let mut state = base_state(store, Some(meeting), &[], None, notice);
    state["active"] = json!(bridge.is_active());
    state["transcribing"] = json!(bridge.is_transcribing());
    state["phase"] = json!(if meeting.status == MeetingStatus::Processing {
        "processing"
    } else {
        phase
    });
    state["elapsed_ms"] = json!(meeting.duration_ms);
    bridge.publish(state);
}

fn publish_store(
    bridge: &Bridge,
    store: &MeetingStore,
    selected: Option<&MeetingId>,
    search: &[Value],
    error: Option<String>,
    notice: Option<String>,
) {
    let previous = bridge.snapshot()["detail"]["id"]
        .as_str()
        .and_then(|id| MeetingId::parse(id.to_string()).ok());
    let detail = selected
        .or(previous.as_ref())
        .and_then(|id| store.load(id).ok());
    let mut state = base_state(store, detail.as_ref(), search, error, notice);
    state["active"] = json!(bridge.is_active());
    state["transcribing"] = json!(bridge.is_transcribing());
    bridge.publish(state);
}

fn base_state(
    store: &MeetingStore,
    detail: Option<&Meeting>,
    search: &[Value],
    error: Option<String>,
    notice: Option<String>,
) -> Value {
    let meetings = store.list().unwrap_or_default();
    json!({
        "ready": true,
        "active": false,
        "transcribing": false,
        "phase": "idle",
        "elapsed_ms": detail.map(|meeting| meeting.duration_ms).unwrap_or_default(),
        "meetings": meetings,
        "detail": detail.map(meeting_value),
        "search": search,
        "error": error,
        "notice": notice,
        "privacy": "Audio, transcripts, speaker labels, notes, and search stay on this device.",
    })
}

fn meeting_value(meeting: &Meeting) -> Value {
    // Derived display fields only. Never change saved segments, summaries,
    // search indices, timestamps, or exports to produce a reading view.
    let segments: Vec<Value> = meeting
        .segments
        .iter()
        .map(|segment| {
            let cleaned = vocalcode_core::fillers::clean(&segment.text, &meeting.language);
            let mut value = json!(segment);
            value["reading_text"] = json!(cleaned.text);
            value["filler_removed"] = json!(cleaned.removed);
            value["noise_only"] =
                json!(vocalcode_meeting::quality::punctuation_only(&segment.text));
            value["review_recommended"] = json!(vocalcode_meeting::quality::sparse_long_segment(
                &segment.text,
                segment.end_ms.saturating_sub(segment.start_ms)
            ));
            value
        })
        .collect();
    json!({
        "schema_version": meeting.schema_version,
        "id": meeting.id,
        "title": meeting.title,
        "created_at_ms": meeting.created_at_ms,
        "updated_at_ms": meeting.updated_at_ms,
        "started_at_ms": meeting.started_at_ms,
        "ended_at_ms": meeting.ended_at_ms,
        "duration_ms": meeting.duration_ms,
        "status": meeting.status,
        "source": meeting.source,
        "language": meeting.language,
        "audio_retention": meeting.audio_retention,
        "speakers": meeting.speakers,
        "bookmarks": meeting.bookmarks,
        "summary": meeting.summary,
        "error": meeting.error,
        "warnings": meeting.warnings,
        "end_reason": meeting.end_reason,
        "filtered_noise_segments": meeting.filtered_noise_segments,
        "segments": segments,
    })
}

fn error_state(error: String) -> Value {
    json!({ "ready": false, "active": false, "transcribing": false, "meetings": [], "detail": null, "search": [], "error": error, "notice": null })
}

fn load_all(store: &MeetingStore) -> Vec<Meeting> {
    store
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| store.load(&entry.id).ok())
        .collect()
}

fn export_to(
    store: &MeetingStore,
    id: &MeetingId,
    kind: ExportKind,
    path: &Path,
) -> Result<(), String> {
    let meeting = store.load(id).map_err(|error| error.to_string())?;
    let contents = match kind {
        ExportKind::Markdown => export_markdown(&meeting),
        ExportKind::Text => export_text(&meeting),
        ExportKind::Json => export_json(&meeting).map_err(|error| error.to_string())?,
        ExportKind::Srt => export_srt(&meeting),
    };
    if path.exists() {
        return Err("The export target already exists; choose a new file name.".to_string());
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options
        .open(path)
        .map_err(|error| format!("Could not create export: {error}"))?;
    use std::io::Write as _;
    file.write_all(contents.as_bytes())
        .map_err(|error| format!("Could not write export: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("Could not finish export: {error}"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn truncate_utf8(value: &str, maximum_bytes: usize) -> &str {
    if value.len() <= maximum_bytes {
        return value;
    }
    let mut end = maximum_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "vocalcode-app-meeting-{label}-{}-{}",
            std::process::id(),
            now_ms()
        ))
    }

    #[test]
    fn export_refuses_to_overwrite_an_existing_file() {
        let root = test_root("export");
        let _ = std::fs::remove_dir_all(&root);
        let store = MeetingStore::open(&root).unwrap();
        let meeting = store
            .create(NewMeeting {
                title: "Test".to_string(),
                now_ms: 1_787_796_747_000,
                source: MeetingSource::Imported {
                    file_name: "test.wav".to_string(),
                },
                language: "en".to_string(),
                audio_retention: AudioRetention::DeleteAfterTranscription,
            })
            .unwrap();
        let target = root.join("existing.md");
        std::fs::write(&target, "keep").unwrap();
        assert!(export_to(&store, &meeting.id, ExportKind::Markdown, &target).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn utf8_truncation_never_splits_a_character() {
        assert_eq!(truncate_utf8("会议记录", 7), "会议");
        assert_eq!(truncate_utf8("hello", 99), "hello");
    }

    #[test]
    fn live_aec_is_enabled_only_when_both_audio_sources_are_selected() {
        let root = test_root("aec-source-gating");
        let _ = std::fs::remove_dir_all(&root);

        let microphone_only = TrackSet::new(&root.join("microphone"), true, false).unwrap();
        assert!(microphone_only.echo.is_none());

        let system_only = TrackSet::new(&root.join("system"), false, true).unwrap();
        assert!(system_only.echo.is_none());

        let both = TrackSet::new(&root.join("both"), true, true).unwrap();
        assert!(both.echo.is_some());

        drop((microphone_only, system_only, both));
        std::fs::remove_dir_all(root).unwrap();
    }

    pub(super) fn transcript(
        id: u64,
        start_ms: u64,
        end_ms: u64,
        source: AudioSource,
        text: &str,
    ) -> TranscriptSegment {
        TranscriptSegment {
            id,
            start_ms,
            end_ms,
            speaker_id: match source {
                AudioSource::Microphone => "you",
                _ => "speaker-1",
            }
            .to_string(),
            source,
            text: text.to_string(),
        }
    }

    #[test]
    fn live_echo_is_deduplicated_and_prefers_system_audio() {
        let input = vec![
            transcript(
                1,
                1_000,
                4_000,
                AudioSource::Microphone,
                "我们今天先测试这个会议记录功能。",
            ),
            transcript(
                2,
                1_180,
                3_900,
                AudioSource::System,
                "我们今天先测试这个会议记录功能",
            ),
        ];
        let cleaned = deduplicate_live_tracks(&input);
        assert_eq!(cleaned.len(), 1);
        assert_eq!(cleaned[0].source, AudioSource::System);
        assert_eq!(cleaned[0].id, 2);
    }

    #[test]
    fn simultaneous_different_speech_is_not_deduplicated() {
        let input = vec![
            transcript(
                1,
                1_000,
                3_000,
                AudioSource::Microphone,
                "我来负责测试 Windows 版本。",
            ),
            transcript(
                2,
                1_100,
                2_900,
                AudioSource::System,
                "Mac 版本明天再安排其他人验证。",
            ),
        ];
        assert_eq!(deduplicate_live_tracks(&input).len(), 2);
    }

    #[test]
    fn same_track_repetition_is_preserved() {
        let input = vec![
            transcript(1, 1_000, 2_000, AudioSource::Microphone, "请再说一遍。"),
            transcript(2, 2_100, 3_100, AudioSource::Microphone, "请再说一遍。"),
        ];
        assert_eq!(deduplicate_live_tracks(&input).len(), 2);
    }

    #[test]
    fn a_meeting_command_reserves_the_single_active_slot_before_queueing() {
        let bridge = Bridge::default();
        let (sender, receiver) = mpsc::sync_channel(2);
        *bridge
            .0
            .command
            .lock()
            .unwrap_or_else(|value| value.into_inner()) = Some(sender);
        bridge
            .start_live(
                "First".to_string(),
                true,
                false,
                None,
                false,
                "en".to_string(),
                5,
            )
            .unwrap();
        assert!(bridge.is_active());
        bridge.stop().unwrap();
        assert!(bridge
            .import(
                PathBuf::from("second.wav"),
                "Second".to_string(),
                "en".to_string()
            )
            .is_err());
        match receiver.try_recv().unwrap() {
            Command::StartLive { stop, .. } => assert!(stop.load(Ordering::Acquire)),
            command => panic!("unexpected command: {command:?}"),
        }
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn capture_failure_keeps_partial_notes_and_audio_explicitly_recoverable() {
        let root = test_root("capture-failure");
        let store = MeetingStore::open(root.join("meetings")).unwrap();
        let mut meeting = store
            .create(NewMeeting {
                title: "Capture failure".to_string(),
                now_ms: 1_787_796_747_000,
                source: MeetingSource::Live {
                    microphone: true,
                    system_audio: true,
                },
                language: "en".to_string(),
                audio_retention: AudioRetention::DeleteAfterTranscription,
            })
            .unwrap();
        meeting.duration_ms = 1_000;
        let bridge = Bridge::default();
        let message =
            preserve_failed_capture(&store, &bridge, &mut meeting, "audio queue overflow").unwrap();

        let loaded = store.load(&meeting.id).unwrap();
        assert_eq!(loaded.status, MeetingStatus::Failed);
        assert_eq!(loaded.audio_retention, AudioRetention::KeepUntilDeleted);
        assert!(loaded.summary.is_some());
        assert_eq!(loaded.error.as_deref(), Some(message.as_str()));
        assert!(message.contains("retained locally"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finishing_worker_cannot_clear_the_next_meetings_stop_token() {
        let bridge = Bridge::default();
        let old = Arc::new(AtomicBool::new(false));
        let next = Arc::new(AtomicBool::new(false));
        bridge.set_stop(Some(next.clone()));

        bridge.clear_stop_if(&old);
        bridge.stop().unwrap();
        assert!(next.load(Ordering::Acquire));

        bridge.clear_stop_if(&next);
        assert!(bridge.stop().is_err());
    }

    #[test]
    fn active_bookmarks_are_applied_by_the_recording_owner() {
        let root = test_root("bookmark");
        let store = MeetingStore::open(root.join("meetings")).unwrap();
        let mut meeting = store
            .create(NewMeeting {
                title: "Bookmark test".to_string(),
                now_ms: 1_787_796_747_000,
                source: MeetingSource::Live {
                    microphone: true,
                    system_audio: false,
                },
                language: "en".to_string(),
                audio_retention: AudioRetention::DeleteAfterTranscription,
            })
            .unwrap();
        meeting.duration_ms = 2_000;
        store.save(&meeting).unwrap();
        let bridge = Bridge::default();
        bridge.0.active.store(true, Ordering::Release);
        bridge.set_active_meeting(Some(meeting.id.clone()));
        bridge
            .bookmark(meeting.id.clone(), 1_250, "Decision".to_string())
            .unwrap();
        apply_pending_bookmarks(&bridge, &store, &mut meeting).unwrap();
        let loaded = store.load(&meeting.id).unwrap();
        assert_eq!(loaded.bookmarks.len(), 1);
        assert_eq!(loaded.bookmarks[0].at_ms, 1_250);
        assert_eq!(loaded.bookmarks[0].label, "Decision");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn imported_audio_stays_local_and_runs_the_full_notes_pipeline() {
        let root = test_root("import");
        let store = MeetingStore::open(root.join("meetings")).unwrap();
        let input_directory = root.join("private-input");
        let mut writer = ChunkedPcmWriter::new(&input_directory, AudioSource::Imported).unwrap();
        let mut samples = vec![0.0; 8_000];
        samples
            .extend((0..24_000).map(|index| {
                (std::f32::consts::TAU * 220.0 * index as f32 / 16_000.0).sin() * 0.2
            }));
        samples.extend(vec![0.0; 12_000]);
        writer.push(&samples).unwrap();
        let input = writer.finish().unwrap().remove(0).path;

        let (sender, receiver) = mpsc::sync_channel::<MeetingAsrRequest>(2);
        let responder = thread::spawn(move || {
            while let Ok(request) = receiver.recv() {
                assert_eq!(request.sample_rate, 16_000);
                assert!(!request.samples.is_empty());
                request
                    .reply
                    .send(Ok("We decided to keep it local. I will follow up tomorrow. Any open questions?".to_string()))
                    .unwrap();
            }
        });
        let bridge = Bridge::default();
        bridge.0.active.store(true, Ordering::Release);
        // This fixture is a tone, not speech. Inject the documented fail-open
        // path to test persistence/notes separately from acoustic recognition.
        run_import_with_detector(
            &store,
            &bridge,
            &sender,
            Arc::new(AtomicBool::new(false)),
            input.clone(),
            "Private review".to_string(),
            "en".to_string(),
            None,
        )
        .unwrap();
        drop(sender);
        responder.join().unwrap();

        let entry = store.list().unwrap().remove(0);
        let meeting = store.load(&entry.id).unwrap();
        assert_eq!(meeting.status, MeetingStatus::Completed);
        assert_eq!(meeting.segment_count, meeting.segments.len());
        assert_eq!(meeting.segment_count, 1);
        let summary = meeting.summary.unwrap();
        assert!(summary.generated_locally);
        assert_eq!(summary.decisions.len(), 1);
        assert_eq!(summary.action_items.len(), 1);
        assert_eq!(summary.open_questions.len(), 1);
        assert!(matches!(
            meeting.source,
            MeetingSource::Imported { ref file_name } if file_name == "imported-000000.wav"
        ));
        let metadata =
            std::fs::read_to_string(store.root().join(entry.id.as_str()).join("meeting.json"))
                .unwrap();
        assert!(!metadata.contains("private-input"));
        assert!(!metadata.contains(&input.to_string_lossy().to_string()));
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
#[path = "meeting_regression.rs"]
mod regression;

#[cfg(test)]
#[path = "meeting_simulation.rs"]
mod simulation;
