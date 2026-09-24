use crate::error::Result;

/// The OS input family that produced a trigger. `device` and `control` in
/// [`TriggerId`] refine this into one stable physical control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TriggerSource {
    Keyboard,
    Mouse,
    ConsumerControl,
    Gamepad,
    Hid,
    /// Events synthesized by a safety path rather than read from hardware.
    Synthetic,
}

/// Runtime identity of one physical trigger.
///
/// `device == 0` means an OS-wide source for which the backend cannot expose a
/// device identity (the legacy keyboard/mouse hook). Native HID backends use a
/// stable 64-bit fingerprint of the device selector. `control` is a normalized
/// key/button/usage code, so aliases resolve to the same identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TriggerId {
    pub source: TriggerSource,
    pub device: u64,
    pub control: u32,
}

impl TriggerId {
    pub const fn new(source: TriggerSource, device: u64, control: u32) -> Self {
        Self {
            source,
            device,
            control,
        }
    }

    pub const fn synthetic(control: u32) -> Self {
        Self::new(TriggerSource::Synthetic, 0, control)
    }
}

/// A finished recording, always normalized to **16 kHz mono f32** by the
/// platform capture layer so the ASR never has to care about device formats.
#[derive(Debug, Clone)]
pub struct Recording {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

impl Recording {
    pub fn duration_secs(&self) -> f32 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        self.samples.len() as f32 / self.sample_rate as f32
    }
}

/// Push-to-talk / send events, produced by a platform `HotkeyListener` and
/// consumed by the [`crate::engine::Engine`]. Deliberately tiny and
/// platform-neutral: the core state machine only speaks in these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerEvent {
    /// Talk key/button pressed down — start capturing.
    TalkPressed(TriggerId),
    /// Explicit mouse/UI start, independent of the saved hold/toggle setting.
    /// Idempotent while recording: repeated requests must never act as stop.
    HandsFreeStart(TriggerId),
    /// Wake the engine to inspect an application-owned control mailbox.
    /// Carries no recording, text delivery or clipboard authority by itself.
    Wake,
    /// Talk key/button released — stop, transcribe, insert.
    TalkReleased(TriggerId),
    /// The "send" trigger tapped (maps to Enter in target apps).
    SendTapped(TriggerId),
    /// The "teach" trigger tapped: take whatever is selected in the app the user
    /// is in and start a dictionary rule from it. Handled by the app rather than
    /// the engine — no audio is involved and the engine owns no clipboard.
    TeachTapped(TriggerId),
    /// A native device vanished. The engine removes every held control from
    /// that device and, if it started a latched utterance, stops safely even
    /// though the second press can no longer arrive.
    DeviceDisconnected(u64),
    /// Stop recording even when the backend cannot identify which release was
    /// lost (device removal, input service restart, permission loss).
    ForceStop,
    /// User-facing cancellation. Kept distinct from ForceStop so the app can
    /// avoid presenting a hardware failure for a deliberate cancel action.
    Cancel,
    /// User asked to quit.
    Quit,
}

/// Captures microphone audio while the talk trigger is held.
/// Implementations resample to 16 kHz mono f32 before returning.
///
/// Not `Send`: some backends (WASAPI on Windows) hold a `!Send` stream. The
/// engine owns capture on a single thread, so this is fine.
pub trait AudioCapture {
    fn start(&mut self) -> Result<()>;
    fn stop(&mut self) -> Result<Recording>;
    fn is_recording(&self) -> bool;
    /// Non-destructive copy of audio captured so far (for live partial
    /// transcription while the key is still held). Default: empty.
    fn snapshot(&self) -> Result<Recording> {
        Ok(Recording {
            samples: Vec::new(),
            sample_rate: 16_000,
        })
    }

    /// Non-destructive audio captured after `start` samples. Backends can
    /// override this to avoid cloning the whole growing utterance on every live
    /// caption tick; the default preserves compatibility.
    fn snapshot_since(&self, start: usize) -> Result<Recording> {
        let mut recording = self.snapshot()?;
        if start >= recording.samples.len() {
            recording.samples.clear();
        } else {
            recording.samples.drain(..start);
        }
        Ok(recording)
    }

    /// Take an asynchronous stream error reported by the realtime callback.
    /// Most test/fake captures have no asynchronous error channel.
    fn take_error(&self) -> Option<String> {
        None
    }
}

/// Turns audio into text. Backed by sherpa-onnx (Paraformer) in the platform
/// layer; a stub lives in the app for wiring/tests.
pub trait Asr: Send {
    fn transcribe(&mut self, samples: &[f32], sample_rate: u32) -> Result<String>;
    /// Submit work without blocking the capture/control thread. At most one
    /// request is outstanding per engine. Dropping the receiver discards a
    /// cancelled utterance's result; a decoder must never inject text itself.
    /// `None` preserves compatibility with synchronous adapters.
    fn transcribe_async(
        &mut self,
        _samples: &[f32],
        _sample_rate: u32,
    ) -> Result<Option<std::sync::mpsc::Receiver<Result<String>>>> {
        Ok(None)
    }
    /// Human-readable name of the active model/tier, for the tray/logs.
    fn model_label(&self) -> &str;
}

/// Inserts recognized text at the current cursor, and can fire the "send" key.
pub trait TextInjector {
    /// Remember the target that owns the caret before recording starts.
    /// Backends that cannot identify a foreground target may keep the default.
    fn begin_utterance(&self) -> Result<()> {
        Ok(())
    }

    /// Clear any target token after success, cancel or error.
    fn end_utterance(&self) {}

    fn inject_text(&self, text: &str) -> Result<()>;
    fn send_enter(&self) -> Result<()>;
    /// Delete the last `n` characters. Kept for platform compatibility and
    /// explicit edit operations; progressive typing itself is append-only.
    fn backspace(&self, n: usize) -> Result<()>;
}

/// A text transform applied once on release: add punctuation (lightweight
/// punctuation model — universal, CPU) or, optionally, LLM cleanup. Chained in
/// order. `&mut self` because some models carry interior state.
pub trait TextCleaner: Send {
    fn clean(&mut self, text: &str) -> Result<String>;
}

/// Listens for the configured trigger(s) globally and forwards
/// [`TriggerEvent`]s. Runs on its own thread; `run` blocks that thread until
/// `shutdown` is set or the platform listener fails.
pub trait HotkeyListener: Send {
    fn run(
        self: Box<Self>,
        tx: crate::trigger_bus::TriggerEventSender,
        shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()>;
}
