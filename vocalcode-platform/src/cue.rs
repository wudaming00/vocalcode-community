//! Short sounds marking the start and end of a recording.
//!
//! # Why this exists
//!
//! Push-to-talk gives no feedback the speaker can perceive without looking. The
//! on-screen indicator is the only signal, and it lives on one screen — so a
//! user with several displays, or one who simply looked away, cannot tell
//! whether the key registered until the text either lands or does not. The
//! failure that costs them is the quiet one: speaking a whole sentence into a
//! recorder that never started.
//!
//! # Why the sounds are synthesised rather than picked
//!
//! The first attempt used two of the system sounds. That is the wrong shape of
//! answer: macOS ships *alert* sounds, each designed to stand alone, so any two
//! of them are two unrelated noises rather than a pair. Asked which one meant
//! "started", a listener has to remember rather than hear.
//!
//! These are one gesture in two directions — the same two notes, the same
//! timbre, rising to open and falling to close. A rising perfect fifth reads as
//! a question opening; the same fifth descending resolves it. Nothing has to be
//! memorised, which is the entire point.
//!
//! Generating them keeps the definition in the code as numbers anyone can read
//! and change, instead of a binary blob in the repository that nobody can
//! review or adjust.
//!
//! # Constraints these have to respect
//!
//! **They must not delay the microphone.** The start cue fires on the same key
//! press that opens capture, immediately after a fight to remove 250 ms from
//! exactly that path. Rendering happens once, at startup; playing hands a
//! prepared sound to the OS and returns.
//!
//! **They belong to the keypress, not to the recording.** Played from the
//! engine's published state instead, the start cue arrives after the microphone
//! and the focus lookup, and the stop cue after the whole blocking decode —
//! seconds after the finger left the key. `Engine::will_start` and `will_finish`
//! answer both edges before any of that work runs.
//!
//! **They are heard by the microphone.** The start cue plays into capture that
//! is already live, so it is inside the audio the recogniser sees. Hence 140 ms
//! rather than the ~770 ms of the system sounds, a gentle envelope rather than
//! a transient, and nothing speech-like.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Which edge of a recording a cue marks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cue {
    /// Capture just opened — the user may speak.
    Start,
    /// Capture just closed — what follows is not being heard.
    Stop,
}

const SAMPLE_RATE: u32 = 44_100;
/// D5 and A5 — a perfect fifth. Consonant in both directions, and clear of the
/// band where speech carries most of its energy, so the cue does not sit on top
/// of the first word.
const LOW_HZ: f32 = 587.33;
const HIGH_HZ: f32 = 880.00;
/// Per note. Two of these overlapping give a ~140 ms gesture — short enough to
/// be inside a recording without being part of it.
const NOTE_S: f32 = 0.080;
/// How much the second note starts before the first has finished. The overlap
/// is what makes the pair read as one gesture rather than two beeps.
const OVERLAP_S: f32 = 0.020;
/// Peak amplitude. Deliberately low: this plays on every single press, and a
/// cue that makes people flinch gets switched off, which costs them the signal.
const PEAK: f32 = 0.28;
/// A little second harmonic. A pure sine reads as thin and electronic; this is
/// what makes it sound rounded.
const HARMONIC: f32 = 0.18;

/// One note with a soft attack and an exponential decay.
///
/// The 4 ms attack is not cosmetic: starting a waveform at full amplitude puts
/// a step in the signal, and a step is a click. The decay runs to silence
/// inside the note so consecutive notes never sum into a jump either.
fn note(buf: &mut [f32], start: usize, freq: f32) {
    let attack = (0.004 * SAMPLE_RATE as f32) as usize;
    let len = (NOTE_S * SAMPLE_RATE as f32) as usize;
    for i in 0..len {
        let idx = start + i;
        if idx >= buf.len() {
            break;
        }
        let t = i as f32 / SAMPLE_RATE as f32;
        let env = if i < attack {
            i as f32 / attack as f32
        } else {
            let past = (i - attack) as f32 / (len - attack) as f32;
            (1.0 - past).powf(2.2)
        };
        let phase = std::f32::consts::TAU * freq * t;
        buf[idx] += env * PEAK * (phase.sin() + HARMONIC * (2.0 * phase).sin());
    }
}

/// Render the two-note gesture as a 16-bit mono WAV.
fn render(rising: bool) -> Vec<u8> {
    let (first, second) = if rising {
        (LOW_HZ, HIGH_HZ)
    } else {
        (HIGH_HZ, LOW_HZ)
    };
    let step = ((NOTE_S - OVERLAP_S) * SAMPLE_RATE as f32) as usize;
    let total = step + (NOTE_S * SAMPLE_RATE as f32) as usize;
    let mut buf = vec![0.0f32; total];
    note(&mut buf, 0, first);
    note(&mut buf, step, second);

    let mut wav = Vec::with_capacity(44 + total * 2);
    let data_len = (total * 2) as u32;
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // PCM chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for s in buf {
        wav.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    wav
}

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Tell the cues where they may keep their rendered audio, and render it.
///
/// Called once at startup with the app's data directory. Both backends need a
/// file rather than a buffer, and doing this lazily on the first press would
/// put a file write on the path this module exists to keep fast.
pub fn init(dir: &Path) {
    let _ = DIR.set(dir.to_path_buf());
    for (cue, name) in [(Cue::Start, "cue-start.wav"), (Cue::Stop, "cue-stop.wav")] {
        let path = dir.join(name);
        // Rewritten every launch on purpose: it is a handful of kilobytes, and
        // it means changing the constants above actually changes what the user
        // hears instead of being masked by a stale file.
        if let Err(e) = std::fs::write(&path, render(cue == Cue::Start)) {
            log::warn!("cue: could not write {}: {e}", path.display());
        }
    }
    imp::prepare();
}

fn path_for(cue: Cue) -> Option<PathBuf> {
    let name = match cue {
        Cue::Start => "cue-start.wav",
        Cue::Stop => "cue-stop.wav",
    };
    DIR.get().map(|d| d.join(name))
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{path_for, Cue};
    use std::ffi::c_void;
    use std::sync::OnceLock;

    // Declared here rather than pulled in as a crate: the app already reaches
    // AppKit, IOKit and CoreServices this way, and two `extern "C"` blocks are
    // cheaper than a dependency tree for four calls.
    #[link(name = "AudioToolbox", kind = "framework")]
    extern "C" {
        fn AudioServicesCreateSystemSoundID(url: *const c_void, out: *mut u32) -> i32;
        /// Returns as soon as the sound is queued, which is what makes this
        /// safe to call on the key-press path.
        fn AudioServicesPlaySystemSound(id: u32);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFURLCreateFromFileSystemRepresentation(
            allocator: *const c_void,
            buffer: *const u8,
            len: isize,
            is_directory: bool,
        ) -> *const c_void;
        fn CFRelease(cf: *const c_void);
    }

    static IDS: OnceLock<(Option<u32>, Option<u32>)> = OnceLock::new();

    fn register(cue: Cue) -> Option<u32> {
        let path = path_for(cue)?;
        let bytes = path.to_str()?.as_bytes();
        unsafe {
            let url = CFURLCreateFromFileSystemRepresentation(
                std::ptr::null(),
                bytes.as_ptr(),
                bytes.len() as isize,
                false,
            );
            if url.is_null() {
                return None;
            }
            let mut id: u32 = 0;
            let status = AudioServicesCreateSystemSoundID(url, &mut id);
            CFRelease(url);
            (status == 0).then_some(id)
        }
    }

    /// Registering reads the file, so it happens at startup and never on the
    /// key-press path.
    pub fn prepare() {
        let ids = (register(Cue::Start), register(Cue::Stop));
        if ids.0.is_none() || ids.1.is_none() {
            // Losing the cue is a degradation, not a failure: say so once and
            // let dictation carry on in silence.
            log::warn!("cue: could not register the rendered sounds; recording will be silent");
        }
        let _ = IDS.set(ids);
    }

    pub fn play(cue: Cue) {
        let Some((start, stop)) = IDS.get() else {
            return;
        };
        let id = match cue {
            Cue::Start => *start,
            Cue::Stop => *stop,
        };
        if let Some(id) = id {
            unsafe { AudioServicesPlaySystemSound(id) };
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::{path_for, Cue};
    use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_NODEFAULT};

    /// Nothing to do ahead of time: `PlaySoundW` takes the path directly.
    pub fn prepare() {}

    /// NOT VERIFIED ON WINDOWS — written from the Mac. `SND_ASYNC` is what
    /// keeps this off the key-press path, and `SND_NODEFAULT` stops Windows
    /// substituting its own beep if the file is missing: a wrong sound is worse
    /// than none, because the user would learn to trust it.
    pub fn play(cue: Cue) {
        let Some(path) = path_for(cue) else {
            return;
        };
        let wide: Vec<u16> = path
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            PlaySoundW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                SND_FILENAME | SND_ASYNC | SND_NODEFAULT,
            );
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use super::Cue;
    pub fn prepare() {}
    pub fn play(_cue: Cue) {}
}

/// Play the cue for one edge of a recording. Never blocks; never fails loudly.
pub fn play(cue: Cue) {
    imp::play(cue);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header has to be right or every platform's player rejects the file
    /// silently, which would look exactly like "the sound does not work".
    #[test]
    fn rendered_cue_is_a_well_formed_mono_16_bit_wav() {
        let wav = render(true);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1, "mono");
        assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16, "16-bit");
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            SAMPLE_RATE
        );
        let data_len = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]) as usize;
        assert_eq!(wav.len(), 44 + data_len, "declared length matches the body");
    }

    /// Rising and falling must be different audio. Getting this wrong gives two
    /// identical cues, which is the failure the pair exists to avoid and which
    /// no amount of listening to one of them would reveal.
    #[test]
    fn start_and_stop_are_mirror_images_not_copies() {
        let up = render(true);
        let down = render(false);
        assert_eq!(up.len(), down.len(), "same gesture, same length");
        assert_ne!(up, down, "one rises and the other falls");
    }

    /// Both ends must sit at silence. A waveform that starts or stops mid-swing
    /// is a step, and a step is an audible click on top of the sound.
    #[test]
    fn the_gesture_begins_and_ends_at_silence() {
        let wav = render(true);
        let sample = |i: usize| {
            let at = 44 + i * 2;
            i16::from_le_bytes([wav[at], wav[at + 1]])
        };
        let total = (wav.len() - 44) / 2;
        assert_eq!(sample(0), 0, "starts from silence");
        assert!(
            sample(total - 1).abs() < 64,
            "ends at silence, got {}",
            sample(total - 1)
        );
    }
}
