use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::{Result, VocalCodeError};
use crate::limits::{
    MAX_DICTIONARY_OUTPUT_BYTES, MAX_DICTIONARY_RULES, MAX_DICTIONARY_SIDE_UTF8_BYTES,
    MAX_DICTIONARY_WORK_BYTES,
};
use crate::traits::{Asr, AudioCapture, TextCleaner, TextInjector, TriggerEvent, TriggerId};

/// The portable heart of VocalCode. Owns the three platform capabilities and runs
/// the push-to-talk state machine. Zero OS-specific code — every platform
/// reuses it unchanged.
///
/// Two output modes:
/// - `live = false` (default): prepare long paused phrases while recording,
///   then clean the combined transcript and insert once on release.
/// - `live = true`: **segmented append** — at each pause, decode only the new
///   segment since the last one and *append* it (never backspaces), so text
///   lands phrase-by-phrase as you speak.
pub struct Engine {
    /// Swapped live when the user changes language — see `swap_asr`.
    audio: Box<dyn AudioCapture>,
    asr: Box<dyn Asr>,
    injector: Box<dyn TextInjector>,
    /// Minimum utterance duration. Kept as milliseconds rather than a sample
    /// count so a future capture backend returning a rate other than 16 kHz does
    /// not silently apply the wrong threshold.
    min_record_ms: u32,
    live: bool,
    /// Optional host/QA policy for pause-based chunking. It is never a hard
    /// audio cut, and changing it is refused during an utterance.
    segment_minimum_ms: Option<u32>,
    /// Shared insert gate: normally open for permanent Basic dictation and
    /// closed only while the runtime is shutting down or delivery is otherwise
    /// unavailable. It is read again before every insert.
    inject_gate: Arc<AtomicBool>,
    /// True when the talk trigger latches: press to start, press again to stop.
    ///
    /// Shared and read fresh on every press, like `inject_gate`, so switching it
    /// in Settings takes effect on the next utterance with no restart.
    latch: Arc<AtomicBool>,
    recording: bool,
    /// The mode belongs to an utterance, not to an individual edge. Settings
    /// may change `latch` while a key is down; the matching release/second press
    /// must still be interpreted in the mode that started this recording.
    active_latched: bool,
    /// Physical control that opened the utterance. In toggle mode its release
    /// is intentionally forgotten from `pressed`, but a device-removal event
    /// still needs to know whether the missing device owns the recording.
    started_by: Option<TriggerId>,
    /// Physical talk controls currently down. Identity matters when several
    /// alternatives are bound: releasing a wireless button must not stop a
    /// recording still held by the keyboard, and auto-repeat must not look like
    /// the second press of toggle mode.
    pressed: HashSet<TriggerId>,
    recording_since: Option<Instant>,
    max_recording: Duration,
    /// Sample index where the not-yet-decoded segment begins. Successful
    /// background preparation and progressive delivery share this cursor.
    seg_start: usize,
    pending_segment: Option<PendingSegment>,
    prepared_text: String,
    predecode_disabled: bool,
    /// Scan only new audio plus one second of endpoint context. Without this,
    /// uninterrupted speech would be copied and rescanned from zero every tick.
    scanned_samples: usize,
    scan_rate: u32,
    /// Text appended so far this utterance (for the return value + spacing).
    utterance: String,
    /// A transcript that ASR completed but the injector could not deliver.
    ///
    /// Injection is deliberately the last step, and focus/clipboard failures
    /// are recoverable operational errors rather than a reason to throw away
    /// speech the model already decoded.  The app takes this value on the error
    /// path and publishes it to History / last-text so the user can copy it.
    recoverable_text: Option<String>,
    /// User replacement rules (heard → desired), applied last (power-user
    /// override). Fixes specific mis-heard terms if someone wants them.
    /// Shared and re-read on every utterance, like `inject_gate` — teaching the
    /// dictionary a word has to change the next thing you say, not the next time
    /// you launch the app. It was an owned `Vec` copied in at construction, so
    /// every rule saved after startup was written to disk and never applied:
    /// the feature looked like it worked and silently did nothing.
    rules: Arc<Mutex<Vec<(String, String)>>>,
    snippets: Arc<Mutex<Vec<crate::migration::Entry>>>,
    active_snippets: Vec<crate::migration::Entry>,
    /// Text transforms applied on release, in order (e.g. punctuation model,
    /// then optionally an LLM cleanup). Empty = raw ASR text.
    cleaners: Vec<Box<dyn TextCleaner>>,
    cleanup_enabled: bool,
    filler_language: Option<String>,
    trace_enabled: bool,
    trace: Option<DictationTrace>,
    completed_trace: Option<DictationTrace>,
}

struct PendingSegment {
    result: mpsc::Receiver<Result<String>>,
    samples: usize,
    rate: u32,
    started: Instant,
}

/// Text-only diagnostic data. Retention and encryption belong to the host;
/// cancelled/gated utterances never expose a trace. No audio is retained here.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DictationTrace {
    pub sample_rate: u32,
    pub sample_count: usize,
    pub audio_ms: u64,
    /// Includes queueing and result-poll delay behind the native model owner.
    pub asr_ms: u64,
    pub asr_chunks: u32,
    /// Audio decoded before release, not including the final in-flight chunk.
    pub predecoded_audio_ms: u64,
    /// Stop/capture flush through final delivery. Separate from total ASR work.
    pub finish_ms: u64,
    /// Includes exact-focus validation and native insertion/clipboard work.
    pub injection_ms: u64,
    pub cleaner_ms: u64,
    pub raw_text: String,
    pub raw_text_truncated: bool,
    pub filler_removed: u32,
    pub cleaned_text: String,
    pub final_text: String,
    pub live_caption: bool,
    pub result: String,
}

fn append_trace_text(target: &mut String, text: &str) {
    const LIMIT: usize = 64 * 1024;
    if !target.is_empty() && target.len() < LIMIT {
        target.push('\n');
    }
    let mut end = text.len().min(LIMIT.saturating_sub(target.len()));
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&text[..end]);
}

/// A hard safety net for a lost key-up (for example a wireless button being
/// switched off while held). The app may choose a different limit with
/// [`Engine::set_max_record_ms`], but recording must never be unbounded.
const DEFAULT_MAX_RECORD_MS: u64 = 10 * 60 * 1_000;

/// Outcome of handling one event, so the caller (tray/UI) can reflect state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Listening,
    Transcribed(String),
    /// The licence/trial gate is closed. No audio was decoded and no transcript
    /// is exposed to History as an alternate unlicensed copy-out path.
    LicenseRequired,
    Sent,
    Idle,
    Quit,
}

impl Engine {
    // Grouped-argument refactors were considered and rejected: these are wiring
    // functions called once each, the parameters are all distinct types, and
    // bundling them into a struct would add a layer to trace through without making
    // any call site clearer.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        audio: Box<dyn AudioCapture>,
        asr: Box<dyn Asr>,
        injector: Box<dyn TextInjector>,
        min_record_ms: u32,
        assumed_rate: u32,
        live: bool,
        inject_gate: Arc<AtomicBool>,
        latch: Arc<AtomicBool>,
        rules: Arc<Mutex<Vec<(String, String)>>>,
        cleaners: Vec<Box<dyn TextCleaner>>,
    ) -> Self {
        // `assumed_rate` stays in the constructor for API compatibility with
        // existing platform wiring. The real recording rate is used on finish.
        let _ = assumed_rate;
        Self {
            audio,
            asr,
            injector,
            min_record_ms,
            live,
            segment_minimum_ms: None,
            inject_gate,
            latch,
            recording: false,
            active_latched: false,
            started_by: None,
            pressed: HashSet::new(),
            recording_since: None,
            max_recording: Duration::from_millis(DEFAULT_MAX_RECORD_MS),
            seg_start: 0,
            pending_segment: None,
            prepared_text: String::new(),
            predecode_disabled: false,
            scanned_samples: 0,
            scan_rate: 0,
            utterance: String::new(),
            recoverable_text: None,
            rules,
            snippets: Arc::new(Mutex::new(Vec::new())),
            active_snippets: Vec::new(),
            cleaners,
            cleanup_enabled: true,
            filler_language: None,
            trace_enabled: false,
            trace: None,
            completed_trace: None,
        }
    }

    /// Run recognized text through the cleaner chain (punctuation, optional LLM).
    /// Replace the speech model without restarting the app.
    ///
    /// The language is a setting, and a setting that needs the app killed and
    /// reopened to take effect reads as broken — people change it, speak, get
    /// the old language back, and conclude the choice did not save. The cleaners
    /// come with it because they are model-shaped: Paraformer's output needs
    /// punctuation added, Parakeet's already has it.
    ///
    /// Refuses mid-recording. Swapping the decoder out from under a held key
    /// would drop whatever the user is in the middle of saying.
    pub fn swap_asr(&mut self, asr: Box<dyn Asr>, cleaners: Vec<Box<dyn TextCleaner>>) -> bool {
        if self.recording {
            return false;
        }
        self.asr = asr;
        self.cleaners = cleaners;
        true
    }

    /// Change the minimum duration for future utterances. Reconfiguration is
    /// deliberately refused while recording so one utterance cannot be judged
    /// by two settings.
    pub fn set_min_record_ms(&mut self, min_record_ms: u32) -> bool {
        if self.recording {
            return false;
        }
        self.min_record_ms = min_record_ms;
        true
    }

    /// Select a more conservative minimum context window for comparative
    /// replay or a host's explicitly chosen policy. None restores the default
    /// (300 ms progressive, 8 s on-release predecode). Pauses are still required,
    /// short final utterances stay intact, and no already-inserted text changes.
    pub fn set_segment_minimum_ms(&mut self, minimum_ms: Option<u32>) -> bool {
        if self.recording || minimum_ms.is_some_and(|ms| !(300..=30_000).contains(&ms)) {
            return false;
        }
        self.segment_minimum_ms = minimum_ms;
        true
    }

    /// Snippets are snapshotted at the start of an utterance. They are never
    /// expanded in meeting transcription or segmented/progressive dictation.
    pub fn set_snippets(&mut self, snippets: Arc<Mutex<Vec<crate::migration::Entry>>>) {
        self.snippets = snippets;
    }

    /// Original mode bypasses the optional cleaner chain, not normalization
    /// already built into the speech model. Dictionary/snippets still apply.
    pub fn set_cleanup_enabled(&mut self, enabled: bool) -> bool {
        if self.recording {
            return false;
        }
        self.cleanup_enabled = enabled;
        true
    }

    pub fn take_trace(&mut self) -> Option<DictationTrace> {
        self.completed_trace.take()
    }

    /// Independent of punctuation/casing. Original ASR remains in a bounded
    /// in-memory trace for review; persistence still requires the host opt-in.
    /// Progressive insertion is intentionally excluded: a chunk cannot know
    /// whether an earlier chunk opened a quotation or code passage.
    pub fn set_filler_removal(&mut self, enabled: bool, language: &str) -> bool {
        if self.recording {
            return false;
        }
        self.filler_language = enabled.then(|| language.to_string());
        true
    }

    fn clean_dictation(&mut self, text: &str) -> String {
        if self.live || self.filler_language.is_none() {
            return self.apply_cleaners(text);
        }
        let started = Instant::now();
        // A manually taught pause-word spelling/replacement takes precedence.
        // On poisoned/unknown dictionary state, leave text intact for recovery.
        let chinese =
            crate::fillers::is_chinese(self.filler_language.as_deref().unwrap_or_default());
        let dictionary_protects_pause = self
            .rules
            .lock()
            .map(|rules| {
                rules.iter().any(|(from, _)| {
                    if chinese {
                        from.chars().any(|c| matches!(c, '呃' | '嗯'))
                    } else {
                        from.split_whitespace().any(|word| {
                            matches!(
                                word.to_ascii_lowercase().as_str(),
                                "um" | "uh" | "erm" | "uhm"
                            )
                        })
                    }
                })
            })
            .unwrap_or(true);
        let cleaned = if !self.live && !dictionary_protects_pause {
            self.filler_language
                .as_deref()
                .map(|language| crate::fillers::clean(text, language))
        } else {
            None
        };
        if let Some(trace) = &mut self.trace {
            trace.cleaner_ms += started.elapsed().as_millis() as u64;
        }
        if let Some(cleaned) = cleaned {
            if let Some(trace) = &mut self.trace {
                trace.filler_removed += cleaned.removed;
            }
            self.apply_cleaners(&cleaned.text)
        } else {
            self.apply_cleaners(text)
        }
    }

    pub fn set_trace_enabled(&mut self, enabled: bool) -> bool {
        if self.recording {
            return false;
        }
        self.trace_enabled = enabled;
        true
    }

    fn decode_dictation(&mut self, samples: &[f32], rate: u32) -> Result<String> {
        let started = Instant::now();
        let result = self.asr.transcribe(samples, rate);
        self.record_decode(samples.len(), rate, started, &result);
        result
    }

    fn inject_dictation(&mut self, text: &str) -> Result<()> {
        let started = Instant::now();
        let result = self.injector.inject_text(text);
        if let Some(trace) = &mut self.trace {
            trace.injection_ms += started.elapsed().as_millis() as u64;
        }
        result
    }

    fn record_decode(
        &mut self,
        count: usize,
        rate: u32,
        started: Instant,
        result: &Result<String>,
    ) {
        if let Some(trace) = &mut self.trace {
            trace.sample_rate = rate;
            trace.sample_count += count;
            trace.audio_ms += (count as u64 * 1000).checked_div(rate as u64).unwrap_or(0);
            trace.asr_ms += started.elapsed().as_millis() as u64;
            trace.asr_chunks += 1;
            if let Ok(text) = result {
                trace.raw_text_truncated |= trace
                    .raw_text
                    .len()
                    .saturating_add(text.len())
                    .saturating_add(1)
                    > 64 * 1024;
                append_trace_text(&mut trace.raw_text, text);
            }
        }
    }

    /// Poll without waiting during capture; join the single in-flight request
    /// only after capture has stopped. Native results never touch the injector.
    fn complete_segment(&mut self, wait: bool, deliver: bool) -> Result<bool> {
        let Some(pending) = self.pending_segment.as_ref() else {
            return Ok(true);
        };
        let disconnected = || VocalCodeError::Asr("Background dictation worker stopped".into());
        let result = if wait {
            pending
                .result
                .recv()
                .unwrap_or_else(|_| Err(disconnected()))
        } else {
            match pending.result.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return Ok(false),
                Err(mpsc::TryRecvError::Disconnected) => Err(disconnected()),
            }
        };
        let pending = self.pending_segment.take().expect("pending segment");
        // A closed gate/cancellation must never leak even a completed result.
        if !self.inject_allowed() {
            return Err(VocalCodeError::License(
                "text delivery is temporarily unavailable".into(),
            ));
        }
        self.record_decode(pending.samples, pending.rate, pending.started, &result);
        let text = result?;
        if self.live && deliver {
            self.append_segment(text.trim())?;
        } else {
            push_phrase(&mut self.prepared_text, text.trim());
        }
        self.seg_start += pending.samples;
        self.scanned_samples = self.seg_start;
        if self.recording {
            if let Some(trace) = &mut self.trace {
                trace.predecoded_audio_ms += pending.samples as u64 * 1000 / pending.rate as u64;
            }
        }
        Ok(true)
    }

    /// Enable/disable segmented live insertion for future utterances.
    pub fn set_live_caption(&mut self, live: bool) -> bool {
        if self.recording {
            return false;
        }
        self.live = live;
        self.seg_start = 0;
        self.pending_segment = None;
        self.prepared_text.clear();
        self.predecode_disabled = false;
        self.scanned_samples = 0;
        self.scan_rate = 0;
        self.utterance.clear();
        true
    }

    /// Replace the injector without losing the new value when a recording is in
    /// flight. The caller can keep the returned object and retry after finish.
    pub fn replace_injector(
        &mut self,
        injector: Box<dyn TextInjector>,
    ) -> std::result::Result<(), Box<dyn TextInjector>> {
        if self.recording {
            return Err(injector);
        }
        self.injector = injector;
        Ok(())
    }

    /// Replace the capture backend only between utterances. Dropping a live
    /// backend would cut off audio and can leave an OS stream in an ambiguous
    /// state, so refusal returns ownership to the caller.
    pub fn replace_audio(
        &mut self,
        audio: Box<dyn AudioCapture>,
    ) -> std::result::Result<(), Box<dyn AudioCapture>> {
        if self.recording {
            return Err(audio);
        }
        self.audio = audio;
        Ok(())
    }

    /// Change the safety limit. A zero duration is useful to force the next
    /// safety poll immediately (and makes the behavior deterministic in tests).
    pub fn set_max_record_ms(&mut self, max_record_ms: u64) {
        // The platform capture buffer has a hard upper bound just beyond this
        // watchdog. Do not let callers move the normal stop past that boundary,
        // where the buffer would fail first and discard the completed audio.
        self.max_recording = Duration::from_millis(max_record_ms.min(DEFAULT_MAX_RECORD_MS));
    }

    /// Poll from the event loop's regular timeout. When the safety limit ends a
    /// recording, the transcript goes to the History panel, never the target —
    /// see [`Self::force_stop_to_history`].
    pub fn poll_recording_limit(&mut self) -> Result<Option<Outcome>> {
        self.poll_audio_health()?;
        if self.recording_limit_expired() {
            log::warn!("maximum recording duration reached; forcing stop");
            self.force_stop_to_history().map(Some)
        } else {
            Ok(None)
        }
    }

    /// Surface an asynchronous device/stream failure to the event loop and
    /// leave the state machine idle. Capture callbacks cannot return through
    /// `start`, so the platform backend publishes the error through
    /// `AudioCapture::take_error` and this regular poll closes the loop.
    ///
    /// Audio that was successfully captured before the failure is salvaged
    /// into the History panel rather than discarded: a stream error at minute
    /// nine of a dictation must not cost the whole utterance. It is never
    /// injected — focus may have moved during whatever broke the device.
    pub fn poll_audio_health(&mut self) -> Result<()> {
        let Some(error) = self.audio.take_error() else {
            return Ok(());
        };
        if self.recording {
            self.recording = false;
            self.recording_since = None;
            if let Ok(rec) = self.audio.stop() {
                self.salvage_to_history(&rec);
            }
            self.reset_after_recording();
        }
        Err(VocalCodeError::Audio(error))
    }

    /// Stop and transcribe regardless of trigger mode. Injects into the
    /// snapshotted target like a normal release. For the lost-key-up watchdog
    /// use [`Self::force_stop_to_history`] instead.
    pub fn force_stop(&mut self) -> Result<Outcome> {
        self.finish()
    }

    /// Watchdog stop: transcribe what was captured but never inject it. A
    /// lost key-up (secure desktop, lock screen, a wireless button dying,
    /// sleep) means the recording may hold minutes of ambient room audio and
    /// the focus snapshot is stale — typing that into whatever now has focus
    /// is the worst possible outcome. The transcript lands in the History
    /// panel; in live mode only the not-yet-injected tail is salvaged.
    pub fn force_stop_to_history(&mut self) -> Result<Outcome> {
        if !self.recording {
            return Ok(Outcome::Idle);
        }
        let stopped = self.audio.stop();
        self.recording = false;
        self.recording_since = None;
        if let Ok(rec) = &stopped {
            self.salvage_to_history(rec);
        }
        self.reset_after_recording();
        stopped.map(|_| Outcome::Idle)
    }

    /// True when a refused or salvaged transcript is waiting in
    /// [`Self::take_recoverable_text`].
    pub fn has_recoverable_text(&self) -> bool {
        self.recoverable_text.is_some()
    }

    /// Best-effort transcription of captured audio into `recoverable_text`.
    /// Errors are swallowed on purpose: salvage runs on paths that already
    /// carry a primary error, and a decode failure must not mask it.
    fn salvage_to_history(&mut self, rec: &crate::traits::Recording) {
        if !self.inject_allowed() {
            return;
        }
        // Keep already prepared raw phrases; never insert from this recovery
        // path, including a live result that finished after the watchdog fired.
        let _ = self.complete_segment(true, false);
        if !self.inject_allowed() {
            return;
        }
        let start = self.seg_start.min(rec.samples.len());
        let samples = &rec.samples[start..];
        let mut raw = std::mem::take(&mut self.prepared_text);
        if !samples.is_empty() {
            if let Ok(tail) = self.decode_dictation(samples, rec.sample_rate) {
                push_phrase(&mut raw, tail.trim());
            }
        }
        let cleaned = self.clean_dictation(raw.trim());
        let text = self.apply_rules(&cleaned).unwrap_or(cleaned);
        let text = text.trim();
        if !text.is_empty() && self.inject_allowed() {
            self.recoverable_text = Some(text.to_string());
        }
    }

    /// Stop capture and discard the utterance. State is reset even when the
    /// platform stop call fails, because an error must never leave the portable
    /// state machine permanently recording.
    pub fn force_cancel(&mut self) -> Result<Outcome> {
        if !self.recording {
            return Ok(Outcome::Idle);
        }
        let stopped = self.audio.stop();
        self.trace = None;
        self.reset_after_recording();
        stopped.map(|_| Outcome::Idle)
    }

    /// True when injected live segments so far end mid-sentence, so the next
    /// segment is a continuation rather than a fresh sentence.
    fn mid_sentence_continuation(&self) -> bool {
        match self.utterance.chars().rev().find(|c| !c.is_whitespace()) {
            Some(c) => !matches!(c, '.' | '!' | '?' | '。' | '！' | '？'),
            None => false,
        }
    }

    fn apply_cleaners(&mut self, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let mut t = text.to_string();
        let started = Instant::now();
        for c in self
            .cleaners
            .iter_mut()
            .take(if self.cleanup_enabled { usize::MAX } else { 0 })
        {
            match c.clean(&t) {
                Ok(x) => t = x,
                Err(e) => log::warn!("cleaner failed: {e}"),
            }
        }
        if let Some(trace) = &mut self.trace {
            trace.cleaner_ms += started.elapsed().as_millis() as u64;
            append_trace_text(&mut trace.cleaned_text, &t);
        }
        t
    }

    /// Apply the user's replacement rules (case-insensitive) to recognized text.
    fn apply_rules(&self, text: &str) -> Result<String> {
        if text.len() > MAX_DICTIONARY_OUTPUT_BYTES {
            return Err(dictionary_output_limit_error());
        }
        let mut out = text.to_string();
        let Ok(rules) = self.rules.lock() else {
            return Ok(out);
        };
        if rules.len() > MAX_DICTIONARY_RULES {
            return Err(VocalCodeError::Config(format!(
                "dictionary contains more than {MAX_DICTIONARY_RULES} rules"
            )));
        }
        let mut work_remaining = MAX_DICTIONARY_WORK_BYTES;
        for (from, to) in rules.iter() {
            if from.len() > MAX_DICTIONARY_SIDE_UTF8_BYTES
                || to.len() > MAX_DICTIONARY_SIDE_UTF8_BYTES
            {
                return Err(VocalCodeError::Config(format!(
                    "dictionary phrase exceeds the {MAX_DICTIONARY_SIDE_UTF8_BYTES}-byte safety limit"
                )));
            }
            charge_dictionary_work(&mut work_remaining, out.len())?;
            out = ci_replace(&out, from, to, MAX_DICTIONARY_OUTPUT_BYTES)?;
        }
        Ok(out)
    }

    pub fn model_label(&self) -> &str {
        self.asr.model_label()
    }

    /// Transcribe one already-bounded meeting segment without capture, focus,
    /// clipboard, or text injection. The desktop host calls this on the same
    /// engine thread as push-to-talk, so the large ASR model remains single-
    /// owner and is never loaded a second time for meetings.
    pub fn transcribe_meeting_segment(
        &mut self,
        samples: &[f32],
        sample_rate: u32,
    ) -> Result<String> {
        if self.recording {
            return Err(VocalCodeError::Audio(
                "push-to-talk is currently recording".to_string(),
            ));
        }
        if !self.inject_allowed() {
            return Err(VocalCodeError::License(
                "text delivery is temporarily unavailable".to_string(),
            ));
        }
        if sample_rate == 0 || samples.is_empty() {
            return Err(VocalCodeError::Audio(
                "meeting segment contains no usable audio".to_string(),
            ));
        }
        let raw = self.asr.transcribe(samples, sample_rate)?;
        let cleaned = self.apply_cleaners(raw.trim());
        self.finish_meeting_text(&cleaned)
    }

    /// Finish a worker-decoded meeting segment. No microphone, focus or text
    /// injector access; safe while an independent dictation is recording.
    pub fn finish_meeting_text(&self, cleaned: &str) -> Result<String> {
        let text = self.apply_rules(cleaned)?;
        if !self.inject_allowed() {
            return Err(VocalCodeError::License(
                "text delivery is temporarily unavailable".to_string(),
            ));
        }
        Ok(text.trim().to_string())
    }

    fn inject_allowed(&self) -> bool {
        self.inject_gate.load(Ordering::Relaxed)
    }

    pub fn is_recording(&self) -> bool {
        self.recording
    }

    /// Take text that was recognised successfully but could not be injected.
    /// A new utterance clears any unclaimed older value so it can never be
    /// mistaken for the result of a later recording.
    pub fn take_recoverable_text(&mut self) -> Option<String> {
        self.recoverable_text.take()
    }

    /// Whether the lost-release safety limit has elapsed.
    ///
    /// The app asks this before calling [`Self::force_stop`] so it can close the
    /// global-input readiness gate *before* the synchronous decode begins.  The
    /// former `poll_recording_limit` API could only announce expiry after it had
    /// already performed the blocking decode.
    pub fn recording_limit_expired(&self) -> bool {
        self.recording
            && self
                .recording_since
                .is_some_and(|started| started.elapsed() >= self.max_recording)
    }

    pub fn handle(&mut self, ev: TriggerEvent) -> Result<Outcome> {
        match ev {
            TriggerEvent::TalkPressed(id) | TriggerEvent::HandsFreeStart(id) => {
                let hands_free = matches!(ev, TriggerEvent::HandsFreeStart(_));
                if hands_free && self.recording {
                    return Ok(Outcome::Idle);
                }
                // Latched: the second press is the stop. Checked before the
                // "already recording" guard, which in hold mode only exists to
                // swallow key-repeat.
                if self.recording {
                    // An already-down id is keyboard auto-repeat, not another
                    // physical press. This distinction is essential in toggle
                    // mode, where a repeat used to stop the recording instantly.
                    if !self.pressed.insert(id) {
                        return Ok(Outcome::Idle);
                    }
                    return if self.active_latched {
                        self.finish()
                    } else {
                        // Hold mode ends only when the last held alternative is
                        // released; this newly pressed id joins that set.
                        Ok(Outcome::Idle)
                    };
                }
                if !self.inject_allowed() {
                    self.pressed.clear();
                    return Ok(Outcome::LicenseRequired);
                }
                self.seg_start = 0;
                self.pending_segment = None;
                self.prepared_text.clear();
                self.predecode_disabled = false;
                self.scanned_samples = 0;
                self.scan_rate = 0;
                self.utterance.clear();
                self.recoverable_text = None;
                let active_latched = hands_free || self.latched();
                self.active_snippets = self
                    .snippets
                    .lock()
                    .map(|entries| entries.clone())
                    .unwrap_or_default();
                self.pressed.clear();
                self.pressed.insert(id);
                // Snapshot the focused control before opening the
                // microphone. Focus resolution is native and bounded on both
                // shipping platforms; letting a script/OS lookup run after
                // capture starts could hang before `recording_since` and its
                // watchdog exist, as well as snapshot the wrong later target.
                // The snapshot pins automatic delivery, but is not a recording
                // precondition: a missing or changed target uses the injector's
                // clipboard recovery once the transcript exists.
                if let Err(error) = self.injector.begin_utterance() {
                    self.pressed.clear();
                    return Err(error);
                }
                if let Err(error) = self.audio.start() {
                    self.injector.end_utterance();
                    self.pressed.clear();
                    return Err(error);
                }
                self.recording = true;
                self.trace = (self.trace_enabled || (self.filler_language.is_some() && !self.live))
                    .then(|| DictationTrace {
                        live_caption: self.live,
                        ..Default::default()
                    });
                self.active_latched = active_latched;
                self.started_by = Some(id);
                self.recording_since = Some(Instant::now());
                Ok(Outcome::Listening)
            }
            TriggerEvent::TalkReleased(id) => {
                if !self.pressed.remove(&id) {
                    return Ok(Outcome::Idle);
                }
                // In latched mode the release that started the recording must
                // not also end it — the next *press* does.
                if self.recording && self.active_latched {
                    return Ok(Outcome::Idle);
                }
                if self.recording && !self.pressed.is_empty() {
                    return Ok(Outcome::Idle);
                }
                self.finish()
            }
            TriggerEvent::SendTapped(_) => {
                self.injector.send_enter()?;
                Ok(Outcome::Sent)
            }
            // Not the engine's business: no audio, no model, and the clipboard
            // round trip it needs belongs to the app. Reaching here at all would
            // mean the caller forgot to intercept it, so say so rather than
            // swallowing it into Idle.
            TriggerEvent::TeachTapped(_) => Ok(Outcome::Idle),
            TriggerEvent::Wake => Ok(Outcome::Idle),
            TriggerEvent::DeviceDisconnected(device) => {
                let before = self.pressed.len();
                self.pressed.retain(|id| id.device != device);
                if !self.recording {
                    return Ok(Outcome::Idle);
                }
                let started_here = self.started_by.is_some_and(|id| id.device == device);
                if (self.active_latched && started_here)
                    || (!self.active_latched
                        && before != self.pressed.len()
                        && self.pressed.is_empty())
                {
                    self.finish()
                } else {
                    Ok(Outcome::Idle)
                }
            }
            TriggerEvent::ForceStop => self.force_stop(),
            TriggerEvent::Cancel => self.force_cancel(),
            TriggerEvent::Quit => {
                self.trace = None;
                if self.recording {
                    if let Err(error) = self.audio.stop() {
                        // A broken microphone must not veto an explicit process
                        // shutdown. The capture object is dropped immediately
                        // after the event loop observes `Quit` as a final guard.
                        log::warn!("audio stop while quitting failed: {error}");
                    }
                    self.reset_after_recording();
                }
                Ok(Outcome::Quit)
            }
        }
    }

    fn latched(&self) -> bool {
        self.latch.load(Ordering::Relaxed)
    }

    /// Will this event begin a recording?
    ///
    /// The counterpart to [`Self::will_finish`], and needed for the same
    /// reason: feedback has to be emitted *before* `handle` runs. A cue played
    /// afterwards is a cue played after `audio.start()` and the focus lookup —
    /// 100-200 ms late on a signal whose whole job is to say "your key
    /// registered". The user hears it as lag, because it is.
    ///
    /// When nothing is being recorded, every talk press starts one: `handle`
    /// has no readiness gate in front of that branch. Auto-repeat cannot reach
    /// here, since a repeat only arrives while already recording.
    pub fn will_start(&self, ev: TriggerEvent) -> bool {
        !self.recording
            && self.inject_allowed()
            && matches!(
                ev,
                TriggerEvent::TalkPressed(_) | TriggerEvent::HandsFreeStart(_)
            )
    }

    /// Will this event end the recording and start a decode?
    ///
    /// The UI needs to know *before* `handle` runs, because the decode blocks
    /// and the gap with no feedback is the longest one there is. It used to
    /// guess — "a talk release means transcribing" — which is true only in hold
    /// mode. In latched mode the release that happens right after the starting
    /// press ends nothing, so the indicator said "Transcribing…" for the whole
    /// recording and then for good, and people waited on a machine that was
    /// waiting on them. Which event finishes is the state machine's to answer.
    pub fn will_finish(&self, ev: TriggerEvent) -> bool {
        self.recording
            && match ev {
                TriggerEvent::TalkReleased(id) => {
                    !self.active_latched && self.pressed.contains(&id) && self.pressed.len() == 1
                }
                TriggerEvent::TalkPressed(id) => self.active_latched && !self.pressed.contains(&id),
                TriggerEvent::DeviceDisconnected(device) => {
                    (self.active_latched && self.started_by.is_some_and(|id| id.device == device))
                        || (!self.active_latched
                            && self.pressed.iter().any(|id| id.device == device)
                            && self.pressed.iter().all(|id| id.device == device))
                }
                TriggerEvent::ForceStop => true,
                _ => false,
            }
    }

    /// Stop capturing, transcribe, clean, insert. Reached by releasing the key
    /// in hold mode, and by the second press in latched mode.
    fn finish(&mut self) -> Result<Outcome> {
        if !self.recording {
            return Ok(Outcome::Idle);
        }

        // Stop first, then unconditionally leave the recording state. ASR,
        // cleanup and injection all happen afterwards and may fail; none of
        // those failures means the microphone is still running.
        let finish_started = Instant::now();
        let stopped = self.audio.stop();
        self.recording = false;
        self.recording_since = None;

        let result = (|| {
            let rec = stopped?;
            // Gate before ASR. Stopping still happens first so an expiry that
            // lands mid-utterance cannot leave capture running.
            if !self.inject_allowed() {
                return Ok(Outcome::LicenseRequired);
            }
            if let Err(error) = self.complete_segment(true, true) {
                if self.live || !self.inject_allowed() {
                    return Err(error);
                }
                // Failed speculative work never discards the captured audio:
                // the cursor only advances on success, so release can retry it.
                log::warn!("background preparation failed; retrying remaining audio: {error}");
            }
            let min_samples = self.min_samples(rec.sample_rate);
            let out = if self.live {
                // Append the final trailing segment (since the last pause).
                let start = self.seg_start.min(rec.samples.len());
                let tail = &rec.samples[start..];
                // A short tail after one or more completed live segments is
                // still part of a valid utterance. A whole utterance shorter
                // than the configured minimum, however, remains a tap.
                let utterance_long_enough = self.seg_start > 0 || rec.samples.len() >= min_samples;
                if !tail.is_empty() && utterance_long_enough {
                    let text = self.decode_dictation(tail, rec.sample_rate)?;
                    self.append_segment(text.trim())?;
                }
                std::mem::take(&mut self.utterance)
            } else {
                // Reuse completed phrases and decode only the unprocessed
                // tail. Whole-utterance cleanup/dictionary/snippets stay here.
                let mut raw = self.prepared_text.clone();
                let tail = &rec.samples[self.seg_start.min(rec.samples.len())..];
                if !tail.is_empty() && rec.samples.len() >= min_samples {
                    let text = self.decode_dictation(tail, rec.sample_rate)?;
                    push_phrase(&mut raw, text.trim());
                }
                let cleaned = self.clean_dictation(&raw);
                let expansion = crate::migration::expand_snippet(&raw, &self.active_snippets)
                    .or_else(|| crate::migration::expand_snippet(&cleaned, &self.active_snippets));
                if expansion.is_some() {
                    if let Some(trace) = &mut self.trace {
                        trace.filler_removed = 0;
                    }
                }
                // A chosen snippet is literal content, not input to another
                // chain of dictionary transformations.
                let processed = match expansion {
                    Some(text) => Ok(text),
                    None => self.apply_rules(&cleaned),
                };
                let text = match processed {
                    Ok(text) => text,
                    Err(error) => {
                        // ASR succeeded. Preserve the bounded, pre-dictionary
                        // transcript so a pathological rule cannot turn a safe
                        // refusal into lost speech.
                        self.recoverable_text = Some(cleaned);
                        return Err(error);
                    }
                };
                if !text.is_empty() {
                    // Recheck after synchronous ASR: a receipt can expire while
                    // decoding, and that race must discard the result instead
                    // of publishing it to the copyable history panel.
                    if !self.inject_allowed() {
                        return Ok(Outcome::LicenseRequired);
                    }
                    if let Err(error) = self.inject_dictation(&text) {
                        self.recoverable_text = Some(text);
                        return Err(error);
                    }
                }
                text
            };
            if self.inject_allowed() {
                Ok(Outcome::Transcribed(out))
            } else {
                Ok(Outcome::LicenseRequired)
            }
        })();

        match &result {
            Ok(Outcome::LicenseRequired) => self.trace = None,
            Ok(Outcome::Transcribed(text)) => {
                if let Some(trace) = &mut self.trace {
                    append_trace_text(&mut trace.final_text, text);
                    trace.result = "delivery_completed".into();
                }
            }
            Err(_) => {
                if self.inject_allowed()
                    && self.recoverable_text.is_none()
                    && !self.prepared_text.is_empty()
                {
                    self.recoverable_text = Some(self.prepared_text.clone());
                }
                if let Some(trace) = &mut self.trace {
                    trace.result = "failed".into();
                }
            }
            _ => {}
        }
        if let Some(trace) = &mut self.trace {
            trace.finish_ms = finish_started.elapsed().as_millis() as u64;
        }
        self.reset_after_recording();
        result
    }

    fn min_samples(&self, sample_rate: u32) -> usize {
        (sample_rate as u64 * self.min_record_ms as u64 / 1_000) as usize
    }

    fn reset_after_recording(&mut self) {
        if let Some(mut trace) = self.trace.take().filter(|_| self.inject_allowed()) {
            if trace.final_text.is_empty() {
                append_trace_text(
                    &mut trace.final_text,
                    self.recoverable_text.as_deref().unwrap_or(&self.utterance),
                );
            }
            if trace.result.is_empty() {
                trace.result = if self.recoverable_text.is_some() {
                    "history_recovery"
                } else {
                    "interrupted"
                }
                .into();
            }
            self.completed_trace = Some(trace);
        }
        self.active_snippets.clear();
        self.recording = false;
        self.active_latched = false;
        self.started_by = None;
        self.pressed.clear();
        self.recording_since = None;
        self.seg_start = 0;
        // Dropping the receiver isolates cancelled/failed utterances. A late
        // native result cannot become the next utterance's text.
        self.pending_segment = None;
        self.prepared_text.clear();
        self.predecode_disabled = false;
        self.scanned_samples = 0;
        self.scan_rate = 0;
        self.utterance.clear();
        self.injector.end_utterance();
    }

    /// Poll background phrase recognition in both output modes. Progressive
    /// mode appends completed phrases; normal mode keeps them until release.
    pub fn tick_partial(&mut self) -> Result<()> {
        if !self.recording {
            return Ok(());
        }
        let result = self.tick_partial_inner();
        if result.is_err() && self.recording {
            // Snapshot, ASR and injection failures used to return to the app
            // while capture kept running. The UI then correctly reflected the
            // engine as "Listening", but there was no useful path left to a
            // successful live utterance. End the failed transaction instead.
            if let Err(stop_error) = self.audio.stop() {
                log::warn!("audio stop after live-caption failure also failed: {stop_error}");
            }
            self.reset_after_recording();
        }
        result
    }

    fn tick_partial_inner(&mut self) -> Result<()> {
        self.poll_audio_health()?;
        if !self.inject_allowed() {
            return Err(VocalCodeError::License(
                "text delivery is temporarily unavailable".to_string(),
            ));
        }
        match self.complete_segment(false, true) {
            Ok(false) => return Ok(()),
            Err(error) if !self.live => {
                // No busy-loop retry and no stopped microphone from optional
                // preparation. The complete captured tail is retried on release.
                self.predecode_disabled = true;
                log::warn!("background preparation disabled for this utterance: {error}");
                return Ok(());
            }
            Err(error) => return Err(error),
            Ok(true) => {}
        }
        if self.predecode_disabled {
            return Ok(());
        }
        let scan_start = self
            .seg_start
            .max(self.scanned_samples.saturating_sub(self.scan_rate as usize));
        let rec = self.audio.snapshot_since(scan_start)?;
        let len = rec.samples.len();
        if rec.sample_rate == 0 || (self.scan_rate != 0 && self.scan_rate != rec.sample_rate) {
            return Err(VocalCodeError::Audio(
                "capture sample rate changed during dictation".into(),
            ));
        }
        self.scan_rate = rec.sample_rate;
        self.scanned_samples = scan_start + len;
        // The minimum-recording preference applies to live mode too. Without
        // this guard a 0.3-second quiet tap was injected during the tick, before
        // `finish` had a chance to reject it as shorter than (say) 1 second.
        if self.seg_start == 0 && self.scanned_samples < self.min_samples(rec.sample_rate) {
            return Ok(());
        }
        let minimum_ms = self
            .segment_minimum_ms
            .unwrap_or(if self.live { 300 } else { 8_000 });
        let minimum_end = self.seg_start + rec.sample_rate as usize * minimum_ms as usize / 1000;
        let remaining_ms = minimum_end.saturating_sub(scan_start) * 1000 / rec.sample_rate as usize;
        let Some(end) =
            crate::segmentation::pause_boundary(&rec.samples, rec.sample_rate, remaining_ms as u32)
        else {
            return Ok(());
        };
        let end = scan_start + end - self.seg_start;
        // Snapshot the full pending phrase only once a boundary exists. Samples
        // after that boundary remain captured while the native worker runs.
        let segment = if scan_start == self.seg_start {
            rec
        } else {
            self.audio.snapshot_since(self.seg_start)?
        };
        if segment.samples.len() < end || segment.sample_rate != self.scan_rate {
            return Err(VocalCodeError::Audio(
                "capture changed while preparing a phrase".into(),
            ));
        }
        let started = Instant::now();
        match self
            .asr
            .transcribe_async(&segment.samples[..end], segment.sample_rate)
        {
            Ok(Some(result)) => {
                self.pending_segment = Some(PendingSegment {
                    result,
                    samples: end,
                    rate: segment.sample_rate,
                    started,
                });
            }
            Ok(None) if self.live => {
                // Direct synchronous adapters remain usable in core tests and
                // embedded hosts. The desktop always supplies an async proxy.
                let text = self.decode_dictation(&segment.samples[..end], segment.sample_rate)?;
                self.append_segment(text.trim())?;
                self.seg_start += end;
                self.scanned_samples = self.seg_start;
            }
            Ok(None) => self.predecode_disabled = true,
            Err(error) if !self.live => {
                self.predecode_disabled = true;
                log::warn!("could not queue background preparation: {error}");
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    /// Append one decoded segment, preserving word spacing for spaced scripts
    /// while keeping Chinese/Japanese boundaries adjacent.
    fn append_segment(&mut self, text: &str) -> Result<()> {
        if !self.inject_allowed() {
            return Err(VocalCodeError::License(
                "text delivery is temporarily unavailable".to_string(),
            ));
        }
        // Cleaners run here too, not only on the one-shot path.
        //
        // They were missing, so turning on live caption quietly turned off
        // punctuation, capitalisation and acronym fixing — while the settings
        // window went on describing all three as "Cleanup · always on". The
        // cost of running them per segment rather than per utterance is that a
        // sentence split across two pauses is punctuated as two sentences;
        // that is a fair price for the window telling the truth.
        //
        // One repair on top: cleaners see each segment as a fresh text and
        // capitalize its first word, but a segment that continues an unfinished
        // sentence ("this is a long … ⏸ … sentence about") must not become
        // "… Sentence about". When the already-injected utterance does not end
        // at a sentence boundary, a leading capitalization that the cleaners
        // introduced is undone; capitalization the recognizer itself produced
        // (proper nouns) is left alone because the raw text is the reference.
        let raw = text;
        let text = self.apply_cleaners(text);
        let text = if self.mid_sentence_continuation() {
            preserve_leading_case(raw, text)
        } else {
            text
        };
        let text = match self.apply_rules(&text) {
            Ok(text) => text,
            Err(error) => {
                // Earlier live segments are already in the target. Keep this
                // rejected segment copyable without duplicating the text that
                // was successfully inserted before it.
                self.recoverable_text = Some(text);
                return Err(error);
            }
        };
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        let sep = crate::segmentation::join_separator(&self.utterance, text);
        let chunk = format!("{sep}{text}");
        if !self.inject_allowed() {
            return Err(VocalCodeError::License(
                "text delivery is temporarily unavailable".to_string(),
            ));
        }
        if let Err(error) = self.inject_dictation(&chunk) {
            self.recoverable_text = Some(format!("{}{chunk}", self.utterance));
            return Err(error);
        }
        self.utterance.push_str(&chunk);
        Ok(())
    }
}

/// Undo exactly one thing: a cleaner capitalizing the first word of a segment.
/// The raw ASR text is the reference — the leading capitalization is reverted
/// only when the cleaned first word is precisely the raw first word with its
/// first letter uppercased. Anything else ("m c p" collapsed to "MCP", a
/// proper noun the recognizer already capitalized, a reworded start) is left
/// untouched.
fn preserve_leading_case(raw: &str, cleaned: String) -> String {
    let raw_word: String = raw
        .trim_start()
        .chars()
        .take_while(|c| !c.is_whitespace())
        .collect();
    let mut raw_chars = raw_word.chars();
    let Some(raw_first) = raw_chars.next() else {
        return cleaned;
    };
    if !raw_first.is_lowercase() {
        return cleaned;
    }
    let trimmed = cleaned.trim_start();
    let clean_word: String = trimmed.chars().take_while(|c| !c.is_whitespace()).collect();
    let mut clean_chars = clean_word.chars();
    let Some(clean_first) = clean_chars.next() else {
        return cleaned;
    };
    if raw_first.to_uppercase().collect::<String>() != clean_first.to_string() {
        return cleaned;
    }
    if raw_chars.as_str() != clean_chars.as_str() {
        return cleaned;
    }
    let prefix_len = cleaned.len() - trimmed.len();
    let mut out = String::with_capacity(cleaned.len());
    out.push_str(&cleaned[..prefix_len]);
    out.push(raw_first);
    out.push_str(&trimmed[clean_first.len_utf8()..]);
    out
}

/// Case-insensitive replace of all `from` occurrences with `to`.
///
/// Lowercased byte offsets cannot be used directly against the original UTF-8
/// string: Unicode lowercase may expand a character (`İ` becomes `i` plus a
/// combining dot). The old implementation did exactly that and could slice the
/// original in the middle of a code point, which is a panic -- an application
/// abort in the release profile. Keep an explicit mapping from folded ranges to
/// original character ranges instead.
fn dictionary_output_limit_error() -> VocalCodeError {
    VocalCodeError::Config(format!(
        "dictionary replacements would exceed the {MAX_DICTIONARY_OUTPUT_BYTES}-byte transcript safety limit; shorten or remove the expanding rules"
    ))
}

fn dictionary_work_limit_error() -> VocalCodeError {
    VocalCodeError::Config(format!(
        "dictionary replacement workload exceeds the {MAX_DICTIONARY_WORK_BYTES}-byte safety budget; shorten the transcript or dictionary"
    ))
}

fn charge_dictionary_work(remaining: &mut usize, input_bytes: usize) -> Result<()> {
    // One rule creates a folded UTF-8 buffer, an output buffer, and up to one
    // four-usize FoldRange per input byte. Forty bytes is deliberately a
    // conservative, architecture-independent upper estimate of that traffic.
    const ESTIMATED_BYTES_PER_INPUT_BYTE: usize = 40;
    let charge = input_bytes
        .checked_mul(ESTIMATED_BYTES_PER_INPUT_BYTE)
        .ok_or_else(dictionary_work_limit_error)?;
    *remaining = remaining
        .checked_sub(charge)
        .ok_or_else(dictionary_work_limit_error)?;
    Ok(())
}

fn push_bounded(output: &mut String, value: &str, maximum: usize) -> Result<()> {
    let Some(next) = output.len().checked_add(value.len()) else {
        return Err(dictionary_output_limit_error());
    };
    if next > maximum {
        return Err(dictionary_output_limit_error());
    }
    output.push_str(value);
    Ok(())
}

/// Scripts written without spaces between words (CJK ideographs, Japanese
/// kana, Korean hangul). For these, adjacent characters are NOT a word
/// boundary, so a dictionary phrase must still match when glued to its
/// neighbours — otherwise Chinese/Japanese/Korean corrections could never fire
/// (there is no space to sit next to).
fn is_spaceless_script(c: char) -> bool {
    matches!(u32::from(c),
        0x3040..=0x30FF          // Hiragana + Katakana
        | 0x3400..=0x4DBF        // CJK Extension A
        | 0x4E00..=0x9FFF        // CJK Unified Ideographs
        | 0xF900..=0xFAFF        // CJK Compatibility Ideographs
        | 0x1100..=0x11FF        // Hangul Jamo
        | 0x3130..=0x318F        // Hangul Compatibility Jamo
        | 0xAC00..=0xD7A3        // Hangul Syllables
        | 0x2_0000..=0x2_FA1F) // CJK Extension B..F + Compatibility Supplement
}

/// A "word character" for whole-word dictionary matching: alphanumerics from
/// space-delimited scripts (Latin incl. accents, Cyrillic, Greek, digits).
/// A boundary is any place a word char meets a non-word char. Spaceless
/// scripts (CJK/kana/hangul) are deliberately excluded so their glued phrases
/// still match; spaces, punctuation and symbols are boundaries everywhere.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() && !is_spaceless_script(c)
}

fn ci_replace(hay: &str, from: &str, to: &str, maximum: usize) -> Result<String> {
    if hay.len() > maximum {
        return Err(dictionary_output_limit_error());
    }
    let needle: String = from.chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return Ok(hay.to_string());
    }
    let mut source_characters = from.chars();
    let single_spaceless_character = source_characters.next().is_some_and(is_spaceless_script)
        && source_characters.next().is_none();

    #[derive(Clone, Copy)]
    struct FoldRange {
        folded_start: usize,
        folded_end: usize,
        source_start: usize,
        source_end: usize,
    }

    let mut folded = String::with_capacity(hay.len());
    let mut ranges = Vec::with_capacity(hay.chars().count());
    for (source_start, c) in hay.char_indices() {
        let folded_start = folded.len();
        folded.extend(c.to_lowercase());
        ranges.push(FoldRange {
            folded_start,
            folded_end: folded.len(),
            source_start,
            source_end: source_start + c.len_utf8(),
        });
    }

    let mut out = String::with_capacity(hay.len().min(maximum));
    let (mut source_tail, mut folded_search) = (0usize, 0usize);
    let (mut first_cursor, mut last_cursor) = (0usize, 0usize);
    while let Some(relative) = folded[folded_search..].find(&needle) {
        let folded_start = folded_search + relative;
        let folded_end = folded_start + needle.len();

        // A match can begin/end inside a lowercase expansion. Replacing the
        // corresponding complete source character is both safe and the closest
        // Unicode-equivalent behavior (so a rule for `i` can match `İ`).
        // Match positions only move forward. Advancing two cursors makes a
        // match-heavy transcript linear instead of rescanning `ranges` from
        // the beginning for every occurrence (the former O(n^2) path).
        while first_cursor < ranges.len() && ranges[first_cursor].folded_end <= folded_start {
            first_cursor += 1;
        }
        if first_cursor >= ranges.len() {
            break;
        }
        last_cursor = last_cursor.max(first_cursor);
        while last_cursor < ranges.len() && ranges[last_cursor].folded_end < folded_end {
            last_cursor += 1;
        }
        if last_cursor >= ranges.len() {
            break;
        }
        let first = ranges[first_cursor];
        let last = ranges[last_cursor];
        if first.folded_start > folded_start || last.folded_start >= folded_end {
            break;
        }

        // Whole-word matching: reject a candidate whose edge sits between two
        // word characters, so `versal` never fires inside `universal` while a
        // phrase like `cloud code` still matches when bounded by spaces,
        // punctuation, or the ends of the transcript. A word boundary is any
        // place a word char meets a non-word char (or a string edge).
        let starts_clean = if single_spaceless_character {
            hay[..first.source_start]
                .chars()
                .next_back()
                .is_none_or(|before| !before.is_alphanumeric())
        } else {
            match (
                hay[..first.source_start].chars().next_back(),
                hay[first.source_start..].chars().next(),
            ) {
                (Some(before), Some(inside)) => !(is_word_char(before) && is_word_char(inside)),
                _ => true,
            }
        };
        let ends_clean = if single_spaceless_character {
            hay[last.source_end..]
                .chars()
                .next()
                .is_none_or(|after| !after.is_alphanumeric())
        } else {
            match (
                hay[..last.source_end].chars().next_back(),
                hay[last.source_end..].chars().next(),
            ) {
                (Some(inside), Some(after)) => !(is_word_char(inside) && is_word_char(after)),
                _ => true,
            }
        };
        if !(starts_clean && ends_clean) {
            folded_search = last.folded_end;
            continue;
        }

        if first.source_start < source_tail {
            folded_search = last.folded_end;
            continue;
        }
        push_bounded(&mut out, &hay[source_tail..first.source_start], maximum)?;
        push_bounded(&mut out, to, maximum)?;
        source_tail = last.source_end;
        // Skip the entire expansion of the final matched source character so
        // another match cannot replace the other half of that same character.
        folded_search = last.folded_end;
    }
    push_bounded(&mut out, &hay[source_tail..], maximum)?;
    Ok(out)
}

/// Join stable raw phrases without changing their contents.
fn push_phrase(target: &mut String, text: &str) {
    target.push_str(crate::segmentation::join_separator(target, text));
    target.push_str(text);
}

#[cfg(test)]
mod leading_case_tests {
    use super::preserve_leading_case;

    /// A continuation segment whose cleaner-capitalized first word matches the
    /// raw text apart from case gets the raw case back; everything else stays.
    #[test]
    fn only_pure_capitalization_is_undone() {
        // The regression case: cleaner capitalized a mid-sentence continuation.
        assert_eq!(
            preserve_leading_case("sentence about", "Sentence about".to_string()),
            "sentence about"
        );
        // Acronym collapse rewrote the word — not a capitalization; keep it.
        assert_eq!(
            preserve_leading_case("m c p server", "MCP server".to_string()),
            "MCP server"
        );
        // The recognizer itself produced the capital — raw is uppercase; keep.
        assert_eq!(
            preserve_leading_case("Claude Code", "Claude Code".to_string()),
            "Claude Code"
        );
        // Unicode: Spanish accented first letter reverts too.
        assert_eq!(
            preserve_leading_case("él llegó", "Él llegó".to_string()),
            "él llegó"
        );
        // CJK first char has no case; untouched.
        assert_eq!(
            preserve_leading_case("中文 test", "中文 test".to_string()),
            "中文 test"
        );
        // Empty inputs survive.
        assert_eq!(preserve_leading_case("", String::new()), "");
    }
}

#[cfg(test)]
mod tests {
    use super::{charge_dictionary_work, ci_replace};
    use crate::limits::{MAX_DICTIONARY_OUTPUT_BYTES, MAX_DICTIONARY_WORK_BYTES};

    #[test]
    fn replaces_case_insensitively() {
        assert_eq!(
            ci_replace("please Call It now", "call it", "Collie", 1024).unwrap(),
            "please Collie now"
        );
        assert_eq!(
            ci_replace("cally cally", "cally", "Collie", 1024).unwrap(),
            "Collie Collie"
        );
        assert_eq!(
            ci_replace("no match here", "xyz", "Q", 1024).unwrap(),
            "no match here"
        );
        assert_eq!(
            ci_replace("用 call it 打开", "call it", "Collie", 1024).unwrap(),
            "用 Collie 打开"
        );
    }

    /// Unicode lowercase can expand, so indices in the folded string do not
    /// necessarily exist in the source. This used to panic (and abort release
    /// builds) on the final source slice.
    #[test]
    fn unicode_case_insensitive_replace_never_slices_mid_character() {
        // İ (U+0130) lowercases to the two-char "i̇"; matching the whole-word
        // rule `i` against it must still slice on the source character boundary.
        assert_eq!(ci_replace("İ", "i", "X", 1024).unwrap(), "X");
        assert_eq!(ci_replace("İ İ", "i", "x", 1024).unwrap(), "x x");
        assert_eq!(
            ci_replace("Straße 😀", "straße", "road", 1024).unwrap(),
            "road 😀"
        );
        // Rust's lowercase mapping is deliberately not full Unicode case-fold
        // (final sigma remains distinct); the important invariant here is safe
        // source boundaries for every Unicode input.
        assert_eq!(ci_replace("ΟΣ ος", "ος", "x", 1024).unwrap(), "ΟΣ x");
    }

    #[test]
    fn matches_whole_words_only() {
        // The reason this exists: a short rule must never fire inside a longer
        // word. `versal => Vercel` must leave `universal` alone.
        assert_eq!(
            ci_replace("a universal fix", "versal", "Vercel", 1024).unwrap(),
            "a universal fix"
        );
        assert_eq!(
            ci_replace("push it to versal", "versal", "Vercel", 1024).unwrap(),
            "push it to Vercel"
        );
        // Multi-word phrases still match when the whole phrase is bounded by
        // spaces or punctuation.
        assert_eq!(
            ci_replace("cloud code, commit this", "cloud code", "Claude Code", 1024).unwrap(),
            "Claude Code, commit this"
        );
        // But a phrase glued inside a larger token does not match.
        assert_eq!(
            ci_replace("cloudcode", "cloud", "X", 1024).unwrap(),
            "cloudcode"
        );
        // Digits are word characters too: `api` must not fire inside `apis`.
        assert_eq!(ci_replace("apis", "api", "API", 1024).unwrap(), "apis");
        assert_eq!(
            ci_replace("the api call", "api", "API", 1024).unwrap(),
            "the API call"
        );
        // A legacy automatically learned one-character CJK rule must not
        // rewrite that character inside every later CJK word. It still works
        // when the character is an isolated utterance/token.
        assert_eq!(ci_replace("派对 派", "派", "pi", 1024).unwrap(), "派对 pi");
        assert_eq!(
            ci_replace("安排任务", "排", "pai", 1024).unwrap(),
            "安排任务"
        );
    }

    #[test]
    fn replacement_refuses_growth_before_crossing_the_output_limit() {
        let error = ci_replace("a a a a", "a", "bbbb", 15).unwrap_err();
        assert!(error.to_string().contains("safety limit"));
    }

    #[test]
    fn tiny_chained_rules_cannot_expand_a_transcript_without_bound() {
        let mut text = "a".to_string();
        let mut refused = false;
        for (from, to) in [
            ("a", "b b"),
            ("b", "c c"),
            ("c", "d d"),
            ("d", "e e"),
            ("e", "f f"),
            ("f", "g g"),
            ("g", "h h"),
            ("h", "i i"),
        ] {
            match ci_replace(&text, from, to, 128) {
                Ok(next) => text = next,
                Err(_) => {
                    refused = true;
                    break;
                }
            }
        }
        assert!(refused);
        assert!(text.len() <= 128);
    }

    #[test]
    fn match_heavy_replacement_preserves_every_occurrence() {
        let source = "a ".repeat(16_384);
        let replaced = ci_replace(&source, "a", "b", source.len()).unwrap();
        assert_eq!(replaced, "b ".repeat(16_384));
    }

    #[test]
    fn dictionary_work_budget_bounds_repeated_maximum_transcript_scans() {
        let mut remaining = MAX_DICTIONARY_WORK_BYTES;
        let mut accepted = 0;
        while charge_dictionary_work(&mut remaining, MAX_DICTIONARY_OUTPUT_BYTES).is_ok() {
            accepted += 1;
        }
        assert!(accepted > 0);
        assert!(
            accepted < 10,
            "maximum transcripts must not be rebuilt hundreds of times"
        );
    }

    #[test]
    fn dictionary_work_budget_allows_a_full_dictionary_on_normal_voice_text() {
        let mut remaining = MAX_DICTIONARY_WORK_BYTES;
        for _ in 0..512 {
            charge_dictionary_work(&mut remaining, 8 * 1024).unwrap();
        }
    }
}

#[cfg(test)]
mod state_machine_tests {
    use super::*;
    use crate::traits::Recording;

    const TALK: TriggerId = TriggerId::synthetic(1);
    const OTHER_TALK: TriggerId = TriggerId::synthetic(2);

    pub(super) fn down() -> TriggerEvent {
        TriggerEvent::TalkPressed(TALK)
    }

    pub(super) fn up() -> TriggerEvent {
        TriggerEvent::TalkReleased(TALK)
    }

    /// Fakes rather than mocks: each records just enough to assert on. The state
    /// machine had no tests at all before latched mode gave it a second path
    /// through, which is exactly when the first one stops being obviously right.
    #[derive(Default)]
    struct FakeAudio {
        running: bool,
        starts: usize,
        stops: usize,
    }
    impl AudioCapture for FakeAudio {
        fn start(&mut self) -> Result<()> {
            self.running = true;
            self.starts += 1;
            Ok(())
        }
        fn stop(&mut self) -> Result<Recording> {
            self.running = false;
            self.stops += 1;
            // Comfortably longer than any min_samples the tests use.
            Ok(Recording {
                samples: vec![0.0; 32_000],
                sample_rate: 16_000,
            })
        }
        fn is_recording(&self) -> bool {
            self.running
        }
    }

    struct FakeAsr;
    impl Asr for FakeAsr {
        fn transcribe(&mut self, _s: &[f32], _r: u32) -> Result<String> {
            Ok("hello".to_string())
        }
        fn model_label(&self) -> &str {
            "fake"
        }
    }

    #[derive(Default)]
    struct FakeInjector {
        inserted: std::sync::Mutex<Vec<String>>,
    }
    impl TextInjector for FakeInjector {
        fn inject_text(&self, text: &str) -> Result<()> {
            self.inserted.lock().unwrap().push(text.to_string());
            Ok(())
        }
        fn send_enter(&self) -> Result<()> {
            Ok(())
        }
        fn backspace(&self, _n: usize) -> Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    pub(super) struct RecordingInjector {
        pub(super) calls: Arc<Mutex<Vec<String>>>,
        pub(super) fail_inject: bool,
    }
    impl TextInjector for RecordingInjector {
        fn begin_utterance(&self) -> Result<()> {
            self.calls.lock().unwrap().push("begin".to_string());
            Ok(())
        }
        fn end_utterance(&self) {
            self.calls.lock().unwrap().push("end".to_string());
        }
        fn inject_text(&self, text: &str) -> Result<()> {
            self.calls.lock().unwrap().push(format!("inject:{text}"));
            if self.fail_inject {
                Err(crate::VocalCodeError::Inject(
                    "focused target changed".into(),
                ))
            } else {
                Ok(())
            }
        }
        fn send_enter(&self) -> Result<()> {
            Ok(())
        }
        fn backspace(&self, n: usize) -> Result<()> {
            self.calls.lock().unwrap().push(format!("backspace:{n}"));
            Ok(())
        }
    }

    struct CountingAsr(Arc<std::sync::atomic::AtomicUsize>);
    impl Asr for CountingAsr {
        fn transcribe(&mut self, _s: &[f32], _r: u32) -> Result<String> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok("must stay private".to_string())
        }
        fn model_label(&self) -> &str {
            "counting"
        }
    }

    pub(super) struct InjectFails;
    impl TextInjector for InjectFails {
        fn inject_text(&self, _text: &str) -> Result<()> {
            Err(crate::VocalCodeError::Inject("target refused text".into()))
        }

        fn send_enter(&self) -> Result<()> {
            Ok(())
        }

        fn backspace(&self, _n: usize) -> Result<()> {
            Ok(())
        }
    }

    fn engine(latched: bool) -> (Engine, Arc<AtomicBool>) {
        let latch = Arc::new(AtomicBool::new(latched));
        let e = Engine::new(
            Box::new(FakeAudio::default()),
            Box::new(FakeAsr),
            Box::new(FakeInjector::default()),
            100,
            16_000,
            false,
            Arc::new(AtomicBool::new(true)),
            latch.clone(),
            Arc::new(Mutex::new(Vec::new())),
            Vec::new(),
        );
        (e, latch)
    }

    #[test]
    fn hold_mode_records_between_press_and_release() {
        let (mut e, _) = engine(false);
        assert_eq!(e.handle(down()).unwrap(), Outcome::Listening);
        assert!(e.is_recording());
        assert_eq!(
            e.handle(up()).unwrap(),
            Outcome::Transcribed("hello".to_string())
        );
        assert!(!e.is_recording());
    }

    #[test]
    fn diagnostics_are_opt_in_cancel_safe_and_preserve_failed_delivery() {
        let (mut e, _) = engine(false);
        e.handle(down()).unwrap();
        e.handle(up()).unwrap();
        assert!(e.take_trace().is_none());
        e.set_trace_enabled(true);
        e.handle(down()).unwrap();
        e.force_cancel().unwrap();
        assert!(e.take_trace().is_none());
        assert!(e.replace_injector(Box::new(InjectFails)).is_ok());
        e.handle(down()).unwrap();
        assert!(e.handle(up()).is_err());
        let trace = e.take_trace().unwrap();
        assert_eq!(trace.raw_text, "hello");
        assert_eq!(trace.cleaned_text, "hello");
        assert_eq!(trace.final_text, "hello");
        assert_eq!(trace.audio_ms, 2000);
        assert_eq!(trace.result, "failed");
        assert!(e.take_trace().is_none());
    }

    #[test]
    fn cleanup_choice_is_frozen_during_recording_and_never_changes_focus_guards() {
        struct Cleaner;
        impl TextCleaner for Cleaner {
            fn clean(&mut self, text: &str) -> Result<String> {
                Ok(text.to_uppercase())
            }
        }
        let (mut e, _) = engine(false);
        e.cleaners = vec![Box::new(Cleaner)];
        e.set_cleanup_enabled(false);
        e.handle(down()).unwrap();
        assert!(!e.set_cleanup_enabled(true));
        assert!(!e.set_trace_enabled(true));
        assert_eq!(
            e.handle(up()).unwrap(),
            Outcome::Transcribed("hello".into())
        );
        assert!(e.set_cleanup_enabled(true));
        e.handle(down()).unwrap();
        assert_eq!(
            e.handle(up()).unwrap(),
            Outcome::Transcribed("HELLO".into())
        );
    }

    struct PauseAsr(&'static str);
    impl Asr for PauseAsr {
        fn transcribe(&mut self, _: &[f32], _: u32) -> Result<String> {
            Ok(self.0.into())
        }
        fn model_label(&self) -> &str {
            "pause-test"
        }
    }

    #[test]
    fn chinese_filter_is_frozen_reviewable_and_respects_dictionary_overrides() {
        let (mut e, _) = engine(false);
        e.swap_asr(Box::new(PauseAsr("这件事情，嗯，需要再讨论。")), vec![]);
        e.set_filler_removal(true, "zh");
        e.rules.lock().unwrap().push(("um".into(), "Um".into()));
        e.handle(down()).unwrap();
        assert!(!e.set_filler_removal(false, "en"));
        assert_eq!(
            e.handle(up()).unwrap(),
            Outcome::Transcribed("这件事情，需要再讨论。".into())
        );
        let trace = e.take_trace().unwrap();
        assert_eq!(trace.raw_text, "这件事情，嗯，需要再讨论。");
        assert_eq!(trace.filler_removed, 1);
        e.rules.lock().unwrap().push(("嗯".into(), "嗯".into()));
        e.handle(down()).unwrap();
        assert_eq!(
            e.handle(up()).unwrap(),
            Outcome::Transcribed(trace.raw_text)
        );
        assert_eq!(e.take_trace().unwrap().filler_removed, 0);
        e.rules.lock().unwrap().clear();
        e.set_filler_removal(false, "zh");
        e.handle(down()).unwrap();
        assert_eq!(
            e.handle(up()).unwrap(),
            Outcome::Transcribed("这件事情，嗯，需要再讨论。".into())
        );
        assert!(e.take_trace().is_none());
    }

    #[test]
    fn filler_option_is_explicit_frozen_and_reviewable_without_persistence() {
        let (mut e, _) = engine(false);
        e.swap_asr(Box::new(PauseAsr("Um, we should, uh, retry.")), vec![]);
        e.handle(down()).unwrap();
        assert_eq!(
            e.handle(up()).unwrap(),
            Outcome::Transcribed("Um, we should, uh, retry.".into())
        );
        assert!(e.take_trace().is_none());
        assert!(e.set_filler_removal(true, "en"));
        e.rules.lock().unwrap().push(("嗯".into(), "嗯".into()));
        e.set_cleanup_enabled(false); // punctuation/casing are independent
        e.handle(down()).unwrap();
        assert!(!e.set_filler_removal(false, "de"));
        assert_eq!(
            e.handle(up()).unwrap(),
            Outcome::Transcribed("We should retry.".into())
        );
        let trace = e.take_trace().unwrap();
        assert_eq!(trace.raw_text, "Um, we should, uh, retry.");
        assert_eq!(trace.cleaned_text, "We should retry.");
        assert_eq!(trace.filler_removed, 2);
        assert!(!trace.raw_text_truncated);
        e.set_filler_removal(false, "en");
        e.handle(down()).unwrap();
        assert_eq!(
            e.handle(up()).unwrap(),
            Outcome::Transcribed(trace.raw_text)
        );
        assert!(e.take_trace().is_none());
    }

    #[test]
    fn filler_removal_never_relaxes_target_failure_or_cancel_gate() {
        let (mut e, _) = engine(false);
        e.swap_asr(Box::new(PauseAsr("uh retry")), vec![]);
        e.set_filler_removal(true, "en");
        let calls = Arc::new(Mutex::new(vec![]));
        assert!(e
            .replace_injector(Box::new(RecordingInjector {
                calls: calls.clone(),
                fail_inject: true
            }))
            .is_ok());
        e.handle(down()).unwrap();
        assert!(e.handle(up()).is_err());
        assert_eq!(*calls.lock().unwrap(), vec!["begin", "inject:Retry", "end"]);
        assert_eq!(e.take_recoverable_text().as_deref(), Some("Retry"));
        assert_eq!(e.take_trace().unwrap().raw_text, "uh retry");
        e.handle(down()).unwrap();
        e.force_cancel().unwrap();
        assert!(e.take_trace().is_none());
        e.handle(down()).unwrap();
        e.inject_gate.store(false, Ordering::Release);
        assert_eq!(e.handle(up()).unwrap(), Outcome::LicenseRequired);
        assert!(e.take_trace().is_none());
    }

    #[test]
    fn progressive_and_meeting_text_do_not_inherit_dictation_filler_removal() {
        let (mut e, _) = engine(false);
        e.swap_asr(Box::new(PauseAsr("uh retry")), vec![]);
        e.set_filler_removal(true, "en");
        assert_eq!(
            e.transcribe_meeting_segment(&[0.1; 3200], 16000).unwrap(),
            "uh retry"
        );
        assert!(e.take_trace().is_none());
        e.set_live_caption(true);
        e.handle(down()).unwrap();
        assert_eq!(
            e.handle(up()).unwrap(),
            Outcome::Transcribed("uh retry".into())
        );
        assert!(e.take_trace().is_none());
    }

    #[test]
    fn pause_only_utterance_has_review_but_no_injection() {
        let (mut e, _) = engine(false);
        e.swap_asr(Box::new(PauseAsr("um uh")), vec![]);
        e.set_filler_removal(true, "en");
        let calls = Arc::new(Mutex::new(vec![]));
        assert!(e
            .replace_injector(Box::new(RecordingInjector {
                calls: calls.clone(),
                fail_inject: false
            }))
            .is_ok());
        e.handle(down()).unwrap();
        assert_eq!(e.handle(up()).unwrap(), Outcome::Transcribed("".into()));
        assert_eq!(*calls.lock().unwrap(), vec!["begin", "end"]);
        assert_eq!(e.take_trace().unwrap().filler_removed, 2);
    }

    #[test]
    fn explicit_dictionary_override_and_snippet_contents_are_preserved() {
        let (mut e, _) = engine(false);
        e.swap_asr(Box::new(PauseAsr("uh retry")), vec![]);
        e.set_filler_removal(true, "en");
        e.rules.lock().unwrap().push(("uh".into(), "UH".into()));
        e.handle(down()).unwrap();
        assert_eq!(
            e.handle(up()).unwrap(),
            Outcome::Transcribed("UH retry".into())
        );
        assert_eq!(e.take_trace().unwrap().filler_removed, 0);
        e.rules.lock().unwrap().clear();
        e.swap_asr(Box::new(PauseAsr("snippet greeting")), vec![]);
        e.set_snippets(Arc::new(Mutex::new(vec![crate::migration::Entry {
            name: "greeting".into(),
            text: "um uh literal text".into(),
        }])));
        e.handle(down()).unwrap();
        assert_eq!(
            e.handle(up()).unwrap(),
            Outcome::Transcribed("um uh literal text".into())
        );
        assert_eq!(e.take_trace().unwrap().filler_removed, 0);
    }

    #[test]
    fn completed_transcript_survives_an_injection_error() {
        let mut e = Engine::new(
            Box::new(FakeAudio::default()),
            Box::new(FakeAsr),
            Box::new(InjectFails),
            100,
            16_000,
            false,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(Vec::new())),
            Vec::new(),
        );

        e.handle(down()).unwrap();
        assert!(e.handle(up()).is_err());
        assert_eq!(e.take_recoverable_text().as_deref(), Some("hello"));
        assert!(e.take_recoverable_text().is_none(), "recovery is one-shot");
        assert!(!e.is_recording());
    }

    #[test]
    fn transcript_survives_a_dictionary_growth_refusal() {
        let side_limit = crate::limits::MAX_DICTIONARY_SIDE_UTF8_BYTES;
        let mut e = Engine::new(
            Box::new(FakeAudio::default()),
            Box::new(FakeAsr),
            Box::new(FakeInjector::default()),
            100,
            16_000,
            false,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(vec![
                // Space-separated so each `a` is its own whole word the second
                // rule can expand — whole-word matching would otherwise skip the
                // a's glued inside `aaaa…` and never grow past the limit.
                ("hello".to_string(), "a ".repeat(side_limit / 2)),
                ("a".to_string(), "b".repeat(side_limit)),
            ])),
            Vec::new(),
        );

        e.handle(down()).unwrap();
        let error = e.handle(up()).unwrap_err();
        assert!(error.to_string().contains("safety limit"));
        assert_eq!(e.take_recoverable_text().as_deref(), Some("hello"));
        assert!(!e.is_recording());
    }

    #[test]
    fn a_closed_or_expired_gate_never_runs_asr_or_returns_copyable_text() {
        let gate = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut e = Engine::new(
            Box::new(FakeAudio::default()),
            Box::new(CountingAsr(calls.clone())),
            Box::new(FakeInjector::default()),
            100,
            16_000,
            false,
            gate.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(Vec::new())),
            Vec::new(),
        );

        assert_eq!(e.handle(down()).unwrap(), Outcome::LicenseRequired);
        assert!(!e.is_recording());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(e.take_recoverable_text().is_none());

        gate.store(true, Ordering::Release);
        assert_eq!(e.handle(down()).unwrap(), Outcome::Listening);
        gate.store(false, Ordering::Release);
        assert_eq!(e.handle(up()).unwrap(), Outcome::LicenseRequired);
        assert!(!e.is_recording());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(e.take_recoverable_text().is_none());
    }

    #[test]
    fn meeting_segment_reuses_asr_without_injecting() {
        let (mut engine, _) = engine(false);
        assert_eq!(
            engine
                .transcribe_meeting_segment(&vec![0.1; 8_000], 16_000)
                .unwrap(),
            "hello"
        );
        assert!(!engine.is_recording());
    }

    #[test]
    fn meeting_segment_obeys_the_license_and_recording_gates() {
        let gate = Arc::new(AtomicBool::new(false));
        let mut engine = Engine::new(
            Box::new(FakeAudio::default()),
            Box::new(FakeAsr),
            Box::new(FakeInjector::default()),
            100,
            16_000,
            false,
            gate.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(Vec::new())),
            Vec::new(),
        );
        assert!(matches!(
            engine.transcribe_meeting_segment(&[0.1; 8_000], 16_000),
            Err(VocalCodeError::License(_))
        ));
        gate.store(true, Ordering::Release);
        engine.handle(down()).unwrap();
        assert!(engine
            .transcribe_meeting_segment(&[0.1; 8_000], 16_000)
            .is_err());
        engine.force_cancel().unwrap();
    }

    /// The point of the mode: the release must not end the utterance.
    #[test]
    fn latched_mode_ignores_the_release() {
        let (mut e, _) = engine(true);
        e.handle(down()).unwrap();
        assert_eq!(e.handle(up()).unwrap(), Outcome::Idle);
        assert!(
            e.is_recording(),
            "letting go must not stop a latched recording"
        );
    }

    #[test]
    fn latched_mode_stops_on_the_second_press() {
        let (mut e, _) = engine(true);
        e.handle(down()).unwrap();
        e.handle(up()).unwrap();
        assert_eq!(
            e.handle(down()).unwrap(),
            Outcome::Transcribed("hello".to_string())
        );
        assert!(!e.is_recording());
    }

    /// Two full utterances back to back, to catch state left behind by the first.
    #[test]
    fn latched_mode_can_run_twice() {
        let (mut e, _) = engine(true);
        for _ in 0..2 {
            e.handle(down()).unwrap();
            e.handle(up()).unwrap();
            assert!(e.is_recording());
            assert_eq!(
                e.handle(down()).unwrap(),
                Outcome::Transcribed("hello".to_string())
            );
            assert!(!e.is_recording());
        }
    }

    /// Key repeat while held must not be read as a stop, or holding the key in
    /// hold mode would cut itself off after the first repeat.
    #[test]
    fn hold_mode_swallows_key_repeat() {
        let (mut e, _) = engine(false);
        e.handle(down()).unwrap();
        assert_eq!(e.handle(down()).unwrap(), Outcome::Idle);
        assert!(e.is_recording());
    }

    /// The setting is shared and read per press, so flipping it mid-session must
    /// take effect without rebuilding the engine.
    #[test]
    fn the_mode_can_change_between_utterances() {
        let (mut e, latch) = engine(false);
        e.handle(down()).unwrap();
        e.handle(up()).unwrap();
        assert!(!e.is_recording());

        latch.store(true, Ordering::Relaxed);
        e.handle(down()).unwrap();
        e.handle(up()).unwrap();
        assert!(e.is_recording(), "now latched: the release is ignored");
    }

    /// A settings change while the physical key is still down applies to the
    /// next utterance, never to the unmatched half of the current one.
    #[test]
    fn the_mode_is_snapshotted_for_the_whole_utterance() {
        // Hold -> toggle while down: the release still belongs to hold mode.
        let (mut e, latch) = engine(false);
        e.handle(down()).unwrap();
        latch.store(true, Ordering::Relaxed);
        assert_eq!(
            e.handle(up()).unwrap(),
            Outcome::Transcribed("hello".to_string())
        );
        assert!(!e.is_recording());

        // Toggle -> hold after the opening press: its release remains ignored,
        // and the second press remains the stop edge.
        let (mut e, latch) = engine(true);
        e.handle(down()).unwrap();
        latch.store(false, Ordering::Relaxed);
        assert_eq!(e.handle(up()).unwrap(), Outcome::Idle);
        assert!(e.is_recording());
        assert_eq!(
            e.handle(down()).unwrap(),
            Outcome::Transcribed("hello".to_string())
        );
    }

    struct StopFails;
    impl AudioCapture for StopFails {
        fn start(&mut self) -> Result<()> {
            Ok(())
        }
        fn stop(&mut self) -> Result<Recording> {
            Err(crate::VocalCodeError::Audio("device disappeared".into()))
        }
        fn is_recording(&self) -> bool {
            true
        }
    }

    struct DecodeFails;
    impl Asr for DecodeFails {
        fn transcribe(&mut self, _s: &[f32], _r: u32) -> Result<String> {
            Err(crate::VocalCodeError::Asr("decoder failed".into()))
        }
        fn model_label(&self) -> &str {
            "failing"
        }
    }

    struct AsyncErrorAudio {
        running: bool,
        error: Mutex<Option<String>>,
    }
    impl AudioCapture for AsyncErrorAudio {
        fn start(&mut self) -> Result<()> {
            self.running = true;
            Ok(())
        }
        fn stop(&mut self) -> Result<Recording> {
            self.running = false;
            Ok(Recording {
                samples: vec![0.0; 32_000],
                sample_rate: 16_000,
            })
        }
        fn is_recording(&self) -> bool {
            self.running
        }
        fn take_error(&self) -> Option<String> {
            self.error.lock().unwrap().take()
        }
    }

    pub(super) struct SnapshotAudio {
        running: bool,
        samples: Vec<f32>,
    }
    impl SnapshotAudio {
        pub(super) fn silent(samples: usize) -> Self {
            Self {
                running: false,
                samples: vec![0.0; samples],
            }
        }
        fn paused() -> Self {
            let mut audio = Self::silent(8_000);
            for (i, sample) in audio.samples[..3_200].iter_mut().enumerate() {
                *sample = if i % 2 == 0 { 0.1 } else { -0.1 };
            }
            audio
        }
    }
    impl AudioCapture for SnapshotAudio {
        fn start(&mut self) -> Result<()> {
            self.running = true;
            Ok(())
        }
        fn stop(&mut self) -> Result<Recording> {
            self.running = false;
            Ok(Recording {
                samples: self.samples.clone(),
                sample_rate: 16_000,
            })
        }
        fn is_recording(&self) -> bool {
            self.running
        }
        fn snapshot_since(&self, start: usize) -> Result<Recording> {
            Ok(Recording {
                samples: self.samples[start.min(self.samples.len())..].to_vec(),
                sample_rate: 16_000,
            })
        }
    }

    pub(super) fn custom_engine(audio: Box<dyn AudioCapture>, asr: Box<dyn Asr>) -> Engine {
        Engine::new(
            audio,
            asr,
            Box::new(FakeInjector::default()),
            100,
            16_000,
            false,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(Vec::new())),
            Vec::new(),
        )
    }

    #[test]
    fn every_finish_error_leaves_the_engine_idle() {
        let mut e = custom_engine(Box::new(StopFails), Box::new(FakeAsr));
        e.handle(down()).unwrap();
        assert!(e.handle(up()).is_err());
        assert!(
            !e.is_recording(),
            "audio stop error left portable state recording"
        );

        let mut e = custom_engine(Box::new(FakeAudio::default()), Box::new(DecodeFails));
        e.handle(down()).unwrap();
        assert!(e.handle(up()).is_err());
        assert!(
            !e.is_recording(),
            "decode error left portable state recording"
        );
    }

    /// The focused control must be captured before the microphone opens. Focus
    /// lookup is now a native bounded API on macOS; capturing first prevents a
    /// hung lookup from leaving an un-watched live microphone and prevents a
    /// later focus change from becoming the recorded target.
    #[test]
    fn focus_is_resolved_before_capture_starts() {
        #[derive(Default, Clone)]
        struct Order(Arc<Mutex<Vec<&'static str>>>);

        struct OrderedAudio(Order, bool);
        impl AudioCapture for OrderedAudio {
            fn start(&mut self) -> Result<()> {
                self.0 .0.lock().unwrap().push("audio.start");
                self.1 = true;
                Ok(())
            }
            fn stop(&mut self) -> Result<Recording> {
                self.0 .0.lock().unwrap().push("audio.stop");
                self.1 = false;
                Ok(Recording {
                    samples: vec![0.0; 32_000],
                    sample_rate: 16_000,
                })
            }
            fn is_recording(&self) -> bool {
                self.1
            }
        }

        struct OrderedInjector(Order, bool);
        impl TextInjector for OrderedInjector {
            fn begin_utterance(&self) -> Result<()> {
                self.0 .0.lock().unwrap().push("begin_utterance");
                if self.1 {
                    Err(crate::VocalCodeError::Inject("no focused target".into()))
                } else {
                    Ok(())
                }
            }
            fn end_utterance(&self) {
                self.0 .0.lock().unwrap().push("end_utterance");
            }
            fn inject_text(&self, _t: &str) -> Result<()> {
                Ok(())
            }
            fn send_enter(&self) -> Result<()> {
                Ok(())
            }
            fn backspace(&self, _n: usize) -> Result<()> {
                Ok(())
            }
        }

        let build = |focus_fails: bool| {
            let order = Order::default();
            let engine = Engine::new(
                Box::new(OrderedAudio(order.clone(), false)),
                Box::new(FakeAsr),
                Box::new(OrderedInjector(order.clone(), focus_fails)),
                100,
                16_000,
                false,
                Arc::new(AtomicBool::new(true)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(Mutex::new(Vec::new())),
                Vec::new(),
            );
            (engine, order)
        };

        let (mut e, order) = build(false);
        assert_eq!(e.handle(down()).unwrap(), Outcome::Listening);
        assert_eq!(
            order.0.lock().unwrap().as_slice(),
            ["begin_utterance", "audio.start"],
            "microphone opened before the focus snapshot"
        );

        // A focus failure must not touch the microphone at all.
        let (mut e, order) = build(true);
        assert!(e.handle(down()).is_err());
        assert!(!e.is_recording());
        let calls = order.0.lock().unwrap().clone();
        assert_eq!(calls, ["begin_utterance"]);
    }

    #[test]
    fn cancel_and_watchdog_always_end_recording() {
        let (mut e, _) = engine(false);
        e.handle(down()).unwrap();
        assert_eq!(e.force_cancel().unwrap(), Outcome::Idle);
        assert!(!e.is_recording());

        // The watchdog fires when a key-up was lost, which means the recording
        // may hold ambient room audio and the focus snapshot is stale. Its
        // transcript must land in History (recoverable text), never be
        // injected into whatever now has focus.
        let (mut e, _) = engine(false);
        e.set_max_record_ms(0);
        e.handle(down()).unwrap();
        assert_eq!(e.poll_recording_limit().unwrap(), Some(Outcome::Idle));
        assert!(!e.is_recording());
        assert_eq!(e.take_recoverable_text().as_deref(), Some("hello"));
    }

    #[test]
    fn asynchronous_capture_error_is_reported_and_resets_recording() {
        let audio = AsyncErrorAudio {
            running: false,
            error: Mutex::new(Some("wireless microphone disconnected".into())),
        };
        let mut e = custom_engine(Box::new(audio), Box::new(FakeAsr));
        e.handle(down()).unwrap();

        let error = e.poll_recording_limit().unwrap_err().to_string();
        assert!(error.contains("wireless microphone disconnected"));
        assert!(!e.is_recording());
    }

    #[test]
    fn live_mode_does_not_inject_before_the_minimum_duration() {
        let mut e = Engine::new(
            Box::new(SnapshotAudio::silent(4_800)),
            Box::new(FakeAsr),
            Box::new(FakeInjector::default()),
            1_000,
            16_000,
            true,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(Vec::new())),
            Vec::new(),
        );
        e.handle(down()).unwrap();
        e.tick_partial().unwrap();
        assert_eq!(e.handle(up()).unwrap(), Outcome::Transcribed(String::new()));
    }

    #[test]
    fn live_mode_appends_a_stable_segment_once_without_rewriting_it() {
        let injector = RecordingInjector::default();
        let calls = injector.calls.clone();
        let mut e = Engine::new(
            Box::new(SnapshotAudio::paused()),
            Box::new(FakeAsr),
            Box::new(injector),
            100,
            16_000,
            true,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(Vec::new())),
            Vec::new(),
        );

        e.handle(down()).unwrap();
        e.tick_partial().unwrap();
        assert_eq!(
            e.handle(up()).unwrap(),
            Outcome::Transcribed("hello".to_string())
        );
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["begin", "inject:hello", "end"],
            "release must not duplicate or rewrite the already-inserted segment"
        );
    }

    #[test]
    fn live_target_failure_stops_recording_and_preserves_recoverable_text() {
        let injector = RecordingInjector {
            fail_inject: true,
            ..Default::default()
        };
        let calls = injector.calls.clone();
        let mut e = Engine::new(
            Box::new(SnapshotAudio::paused()),
            Box::new(FakeAsr),
            Box::new(injector),
            100,
            16_000,
            true,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(Vec::new())),
            Vec::new(),
        );

        e.handle(down()).unwrap();
        assert!(e.tick_partial().is_err());
        assert!(!e.is_recording());
        assert_eq!(e.take_recoverable_text().as_deref(), Some("hello"));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["begin", "inject:hello", "end"],
            "a failed exact-target check must end the transaction without edits"
        );
    }

    #[test]
    fn live_decode_error_stops_capture_and_resets_recording() {
        let mut e = Engine::new(
            Box::new(SnapshotAudio::paused()),
            Box::new(DecodeFails),
            Box::new(FakeInjector::default()),
            100,
            16_000,
            true,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(Vec::new())),
            Vec::new(),
        );
        e.handle(down()).unwrap();
        assert!(e.tick_partial().is_err());
        assert!(!e.is_recording());
    }

    #[test]
    fn quit_stops_an_active_capture() {
        let (mut e, _) = engine(false);
        e.handle(down()).unwrap();
        assert_eq!(e.handle(TriggerEvent::Quit).unwrap(), Outcome::Quit);
        assert!(!e.is_recording());

        let mut e = custom_engine(Box::new(StopFails), Box::new(FakeAsr));
        e.handle(down()).unwrap();
        assert_eq!(e.handle(TriggerEvent::Quit).unwrap(), Outcome::Quit);
        assert!(!e.is_recording(), "a device error must not veto shutdown");
    }

    #[test]
    fn live_reconfiguration_is_rejected_mid_utterance() {
        let (mut e, _) = engine(false);
        e.handle(down()).unwrap();
        assert!(!e.set_min_record_ms(999));
        assert!(!e.set_live_caption(true));
        assert!(e.replace_audio(Box::new(FakeAudio::default())).is_err());
        assert!(e
            .replace_injector(Box::new(FakeInjector::default()))
            .is_err());
        e.force_cancel().unwrap();

        assert!(e.set_min_record_ms(999));
        assert!(e.set_live_caption(true));
        assert!(e.replace_audio(Box::new(FakeAudio::default())).is_ok());
        assert!(e
            .replace_injector(Box::new(FakeInjector::default()))
            .is_ok());
    }

    /// A release with nothing running is meaningless in either mode.
    #[test]
    fn a_stray_release_does_nothing() {
        for latched in [false, true] {
            let (mut e, _) = engine(latched);
            assert_eq!(e.handle(up()).unwrap(), Outcome::Idle);
            assert!(!e.is_recording());
        }
    }

    /// The indicator asks this before every event, and shipping the wrong answer
    /// for latched mode is what put "Transcribing…" on screen for the length of
    /// a recording and then permanently. Both modes, both keys, both directions.
    #[test]
    fn will_finish_marks_only_the_event_that_ends_a_recording() {
        // Hold: the release finishes; a repeat of the press does not.
        let (mut e, _) = engine(false);
        assert!(!e.will_finish(up()), "idle: nothing to finish");
        e.handle(down()).unwrap();
        assert!(e.will_finish(up()));
        assert!(!e.will_finish(down()), "key repeat is not a finish");

        // Latched: the press finishes; the release right after the opening
        // press is the one that used to be mistaken for the end.
        let (mut e, _) = engine(true);
        e.handle(down()).unwrap();
        assert!(!e.will_finish(up()), "the opening release ends nothing");
        e.handle(up()).unwrap(); // still recording
        assert!(e.will_finish(down()), "the second press ends it");
        assert!(e.is_recording());
        assert!(!e.will_finish(up()));
    }

    /// The start cue is played from this answer, before `handle` runs, so a
    /// wrong `true` announces a recording that never begins and a wrong `false`
    /// leaves the press silent. It must also never agree with `will_finish`:
    /// one event cannot be both edges, and in latched mode the closing press is
    /// exactly where that confusion would live.
    #[test]
    fn will_start_marks_only_the_event_that_begins_a_recording() {
        for latched in [false, true] {
            let (mut e, _) = engine(latched);
            assert!(e.will_start(down()), "idle: a talk press starts one");
            assert!(!e.will_start(up()), "a release never starts anything");

            e.handle(down()).unwrap();
            assert!(e.is_recording());
            assert!(
                !e.will_start(down()),
                "already recording: neither auto-repeat nor the closing press starts a second"
            );
            assert!(
                !(e.will_start(down()) && e.will_finish(down())),
                "one event cannot be both edges"
            );
        }

        // And after a full cycle it can start again, or the cue would fall
        // silent for every utterance after the first.
        let (mut e, _) = engine(false);
        e.handle(down()).unwrap();
        e.handle(up()).unwrap();
        assert!(!e.is_recording());
        assert!(e.will_start(down()));
    }

    #[test]
    fn hands_free_start_ignores_saved_mode_and_duplicate_starts() {
        for latched in [false, true] {
            let (mut e, _) = engine(latched);
            let start = TriggerEvent::HandsFreeStart(OTHER_TALK);
            assert!(e.will_start(start));
            assert_eq!(e.handle(start).unwrap(), Outcome::Listening);
            assert!(!e.will_start(start));
            assert!(!e.will_finish(start));
            assert_eq!(e.handle(start).unwrap(), Outcome::Idle);
            assert!(e.is_recording());
            e.handle(TriggerEvent::TalkReleased(OTHER_TALK)).unwrap();
            assert!(
                e.is_recording(),
                "a mouse/UI release must not stop hands-free"
            );
            assert!(e.will_finish(TriggerEvent::ForceStop));
            e.handle(TriggerEvent::ForceStop).unwrap();
            assert!(!e.is_recording());
            assert!(e.will_start(down()));
        }
    }

    #[test]
    fn hands_free_does_not_change_next_hold_session_or_restart_an_existing_one() {
        let (mut e, _) = engine(false);
        e.handle(down()).unwrap();
        e.handle(TriggerEvent::HandsFreeStart(OTHER_TALK)).unwrap();
        assert!(
            e.will_finish(up()),
            "existing physical hold retains its semantics"
        );
        e.handle(up()).unwrap();
        e.handle(TriggerEvent::HandsFreeStart(OTHER_TALK)).unwrap();
        assert!(
            e.will_finish(down()),
            "a physical shortcut can finish hands-free"
        );
        e.handle(down()).unwrap();
        e.handle(up()).unwrap();
        e.handle(down()).unwrap();
        assert!(e.will_finish(up()), "saved hold preference is unchanged");
        e.handle(up()).unwrap();
        assert!(!e.is_recording());
    }

    #[test]
    fn hands_free_respects_delivery_gate_and_wake_has_no_authority() {
        let (mut e, _) = engine(false);
        assert!(!e.will_start(TriggerEvent::Wake));
        e.handle(TriggerEvent::Wake).unwrap();
        assert!(!e.is_recording());
        e.inject_gate.store(false, Ordering::Relaxed);
        let start = TriggerEvent::HandsFreeStart(OTHER_TALK);
        assert!(!e.will_start(start));
        assert_eq!(e.handle(start).unwrap(), Outcome::LicenseRequired);
        assert!(!e.is_recording());
    }

    #[test]
    fn hold_mode_stops_only_after_the_last_physical_trigger_is_released() {
        let (mut e, _) = engine(false);
        e.handle(down()).unwrap();
        e.handle(TriggerEvent::TalkPressed(OTHER_TALK)).unwrap();

        assert!(!e.will_finish(TriggerEvent::TalkReleased(OTHER_TALK)));
        assert_eq!(
            e.handle(TriggerEvent::TalkReleased(OTHER_TALK)).unwrap(),
            Outcome::Idle
        );
        assert!(e.is_recording(), "keyboard is still held");

        assert!(e.will_finish(up()));
        assert_eq!(
            e.handle(up()).unwrap(),
            Outcome::Transcribed("hello".to_string())
        );
    }

    #[test]
    fn toggle_mode_distinguishes_auto_repeat_from_a_second_press() {
        let (mut e, _) = engine(true);
        e.handle(down()).unwrap();
        assert_eq!(e.handle(down()).unwrap(), Outcome::Idle);
        assert!(e.is_recording(), "repeat must not toggle off");
        e.handle(up()).unwrap();
        assert_eq!(
            e.handle(down()).unwrap(),
            Outcome::Transcribed("hello".to_string())
        );
    }

    /// An event with no partner must leave the engine idle rather than half
    /// started: the tap goes down under load and takes one half of a press with
    /// it, and the UI reads `is_recording` to recover.
    #[test]
    fn an_unpaired_release_finishes_nothing() {
        for latched in [false, true] {
            let (mut e, _) = engine(latched);
            assert!(!e.will_finish(up()));
            assert_eq!(e.handle(up()).unwrap(), Outcome::Idle);
            assert!(!e.is_recording(), "latched={latched}");
        }
    }

    /// Never once, and never permanently: the indicator's whole life over two
    /// latched recordings, expressed as the questions the loop actually asks.
    #[test]
    fn latched_round_trip_leaves_nothing_stuck() {
        let (mut e, _) = engine(true);
        for _ in 0..2 {
            assert!(!e.will_finish(down()));
            assert_eq!(e.handle(down()).unwrap(), Outcome::Listening);
            // Release of the opening press: no decode, still recording, so the
            // indicator stays on "recording" rather than jumping to the spinner.
            assert!(!e.will_finish(up()));
            assert_eq!(e.handle(up()).unwrap(), Outcome::Idle);
            assert!(e.is_recording());
            // Second press: this is the decode.
            assert!(e.will_finish(down()));
            assert_eq!(
                e.handle(down()).unwrap(),
                Outcome::Transcribed("hello".to_string())
            );
            // And its release must not re-arm the spinner.
            assert!(!e.will_finish(up()));
            assert_eq!(e.handle(up()).unwrap(), Outcome::Idle);
            assert!(!e.is_recording());
        }
    }
}

#[cfg(test)]
mod live_rules_tests {
    use super::state_machine_tests::{
        custom_engine, down, up, InjectFails, RecordingInjector, SnapshotAudio,
    };
    use super::*;
    use crate::traits::Recording;

    struct Silent;
    impl AudioCapture for Silent {
        fn start(&mut self) -> Result<()> {
            Ok(())
        }
        fn stop(&mut self) -> Result<Recording> {
            Ok(Recording {
                samples: vec![0.0; 32_000],
                sample_rate: 16_000,
            })
        }
        fn is_recording(&self) -> bool {
            false
        }
    }
    struct Fixed(&'static str);
    impl Asr for Fixed {
        fn transcribe(&mut self, _s: &[f32], _r: u32) -> Result<String> {
            Ok(self.0.to_string())
        }
        fn model_label(&self) -> &str {
            "fixed"
        }
    }

    #[test]
    fn snippet_snapshot_is_verbatim_and_keeps_target_lifecycle() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut e = custom_engine(
            Box::new(SnapshotAudio::silent(3200)),
            Box::new(Fixed("Snippet greeting.")),
        );
        e.injector = Box::new(RecordingInjector {
            calls: calls.clone(),
            fail_inject: false,
        });
        e.rules
            .lock()
            .unwrap()
            .push(("Collie".into(), "Wrong".into()));
        let snippets = Arc::new(Mutex::new(vec![crate::migration::Entry {
            name: "greeting".into(),
            text: "Hi, Collie".into(),
        }]));
        e.set_snippets(snippets.clone());
        e.handle(down()).unwrap();
        snippets.lock().unwrap()[0].text = "Updated after recording started".into();
        assert_eq!(
            e.handle(up()).unwrap(),
            Outcome::Transcribed("Hi, Collie".into())
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["begin", "inject:Hi, Collie", "end"]
        );
        e.handle(down()).unwrap();
        assert_eq!(
            e.handle(up()).unwrap(),
            Outcome::Transcribed("Updated after recording started".into())
        );
    }

    #[test]
    fn snippets_never_expand_meetings_or_progressive_segments() {
        let mut e = custom_engine(
            Box::new(SnapshotAudio::silent(3200)),
            Box::new(Fixed("snippet greeting")),
        );
        e.set_snippets(Arc::new(Mutex::new(vec![crate::migration::Entry {
            name: "greeting".into(),
            text: "Expanded".into(),
        }])));
        assert_eq!(
            e.transcribe_meeting_segment(&[0.1; 3200], 16000).unwrap(),
            "snippet greeting"
        );
        e.set_live_caption(true);
        e.handle(down()).unwrap();
        assert_eq!(
            e.handle(up()).unwrap(),
            Outcome::Transcribed("snippet greeting".into())
        );
    }

    #[test]
    fn rejected_snippet_delivery_remains_recoverable() {
        let mut e = custom_engine(
            Box::new(SnapshotAudio::silent(3200)),
            Box::new(Fixed("snippet greeting")),
        );
        e.set_snippets(Arc::new(Mutex::new(vec![crate::migration::Entry {
            name: "greeting".into(),
            text: "Keep this text".into(),
        }])));
        e.injector = Box::new(InjectFails);
        e.handle(down()).unwrap();
        assert!(e.handle(up()).is_err());
        assert_eq!(e.take_recoverable_text().as_deref(), Some("Keep this text"));
        assert!(!e.is_recording());
    }
    #[derive(Default)]
    struct Captured(std::sync::Mutex<Vec<String>>);
    impl TextInjector for Captured {
        fn inject_text(&self, text: &str) -> Result<()> {
            self.0.lock().unwrap().push(text.to_string());
            Ok(())
        }
        fn send_enter(&self) -> Result<()> {
            Ok(())
        }
        fn backspace(&self, _n: usize) -> Result<()> {
            Ok(())
        }
    }

    /// A word taught to the dictionary has to change the *next* thing you say.
    /// The rules used to be copied into the engine at construction, so every rule
    /// added after launch was written to disk and never applied — the feature
    /// looked like it worked and did nothing until a restart.
    #[test]
    fn a_rule_added_after_startup_applies_to_the_next_utterance() {
        let rules = Arc::new(Mutex::new(Vec::new()));
        let mut e = Engine::new(
            Box::new(Silent),
            Box::new(Fixed("握口扣是一个工具")),
            Box::new(Captured::default()),
            100,
            16_000,
            false,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            rules.clone(),
            Vec::new(),
        );
        e.handle(TriggerEvent::TalkPressed(TriggerId::synthetic(1)))
            .unwrap();
        let before = e
            .handle(TriggerEvent::TalkReleased(TriggerId::synthetic(1)))
            .unwrap();
        assert_eq!(before, Outcome::Transcribed("握口扣是一个工具".to_string()));

        // The user teaches it a word, mid-session, without restarting anything.
        rules
            .lock()
            .unwrap()
            .push(("握口扣".to_string(), "VocalCode".to_string()));

        e.handle(TriggerEvent::TalkPressed(TriggerId::synthetic(1)))
            .unwrap();
        let after = e
            .handle(TriggerEvent::TalkReleased(TriggerId::synthetic(1)))
            .unwrap();
        assert_eq!(
            after,
            Outcome::Transcribed("VocalCode是一个工具".to_string())
        );
    }
}
