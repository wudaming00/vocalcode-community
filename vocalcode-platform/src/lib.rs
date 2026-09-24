//! vocalcode-platform — concrete implementations of the vocalcode-core traits.
//!
//! Built on cross-platform crates (`cpal`, `rdev`, `enigo`), so most of this
//! already works on macOS/Linux; genuinely OS-specific bits (mouse extra-button
//! codes, clipboard paste) are isolated and `cfg`-gated as they arrive.

pub mod asr;
pub mod audio;
pub mod cleaner;
pub mod cue;
pub mod device;
pub mod foreground;
pub mod hardware;
pub mod hotkey;
pub mod inject;
pub mod meeting_audio;
pub mod meeting_presence;
pub mod mute;
pub mod speech_gate;

// Descriptor decoding is platform-neutral so macOS HID edge behavior remains
// covered by tests on the Windows development machine.
#[cfg(any(target_os = "macos", test))]
mod hid_input;

/// Windows-only native input sources that rdev cannot see: XInput controllers
/// and Raw Input HID/Consumer controls.
#[cfg(windows)]
pub(crate) mod input_windows;

/// Our own WH_KEYBOARD_LL / WH_MOUSE_LL delivery layer — replaced `rdev::grab`
/// on Windows, whose in-hook key-name resolution froze the machine's keyboard.
#[cfg(windows)]
pub(crate) mod hook_windows;

/// macOS native HID/Consumer/gamepad discovery and hot-plug callbacks.
#[cfg(target_os = "macos")]
pub(crate) mod input_macos;

/// macOS needs its own event tap — `rdev` drops the mouse thumb buttons there.
#[cfg(target_os = "macos")]
pub mod hotkey_macos;

pub use asr::{
    SherpaOmnilingualAsr, SherpaParaformerAsr, SherpaParakeetAsr, SherpaQwen3Asr,
    SherpaSenseVoiceAsr, SherpaWhisperAsr, SherpaZipformerAsr,
};
pub use audio::{list_input_device_choices, AudioLevel, CpalAudioCapture, InputDeviceChoice};
pub use cleaner::{
    AcronymCollapser, JapaneseSpaceCollapser, Normalizer, SherpaPunctuator, T2sCleaner,
};
pub use cue::Cue;
pub use device::device_id;
pub use foreground::{foreground_app_id, foreground_application, ForegroundApplication};
pub use hardware::{
    asr_threads, detect_tier, has_gpu, inference_cores, logical_cores, memory_mib,
    performance_class, HardwareProfile, PerformanceClass, Tier,
};
pub use hotkey::{CaptureShared, RdevHotkey, SharedTriggers};
pub use inject::{
    copy_selection, write_clipboard_text, CorrectionEvent, CorrectionMonitor, EnigoInjector,
};
pub use meeting_audio::{
    start_meeting_audio, MeetingAudioCapture, StreamedAudioBlock, StreamedAudioSource,
};

#[cfg(target_os = "macos")]
pub use hotkey_macos::MacHotkey;

/// The hotkey listener for the platform being built.
///
/// Use this rather than naming a backend directly: macOS is served by
/// [`hotkey_macos::MacHotkey`] (a native `CGEventTap`, so the mouse thumb
/// buttons work), everything else by [`hotkey::RdevHotkey`]. Both take
/// `(SharedTriggers, Arc<CaptureShared>)` and implement `HotkeyListener`.
#[cfg(target_os = "macos")]
pub type PlatformHotkey = hotkey_macos::MacHotkey;

#[cfg(not(target_os = "macos"))]
pub type PlatformHotkey = hotkey::RdevHotkey;
