//! Microphone capture via `cpal` (cross-platform). Records interleaved device
//! audio while the talk trigger is held, downmixes to mono, and resamples to
//! 16 kHz f32 — the format every ASR backend here expects.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use vocalcode_core::error::{Result, VocalCodeError};
use vocalcode_core::traits::{AudioCapture, Recording};

const TARGET_RATE: u32 = 16_000;
/// The engine normally stops at ten minutes. Keep a small reserve beyond the
/// watchdog so scheduling jitter cannot make this lower-level memory boundary
/// win the race and discard an otherwise complete ten-minute dictation.
const MAX_CAPTURE_SECS: usize = 10 * 60 + 5;
const MAX_CAPTURE_SAMPLES: usize = TARGET_RATE as usize * MAX_CAPTURE_SECS;
/// `StreamTrait::play` only asks the backend to start; some backends report a
/// disconnected or denied device asynchronously.  Do not tell the engine that
/// recording started until at least one real input callback has arrived.
const STREAM_START_TIMEOUT: Duration = Duration::from_secs(2);
const DEVICE_ID_PREFIX: &str = "vocalcode-cpal-device:";

fn i24_to_f32(sample: cpal::I24) -> f32 {
    sample.inner() as f32 / 8_388_608.0
}

fn u24_to_f32(sample: cpal::U24) -> f32 {
    (sample.inner() as f32 - 8_388_608.0) / 8_388_608.0
}

/// Live microphone level, 0.0–1.0, shared with whatever wants to draw it.
///
/// Published as a `u16` of `level * 10_000` inside an atomic rather than a
/// float behind the buffer mutex: the capture callback runs on a realtime audio
/// thread, where taking a lock risks a priority inversion and a dropped buffer,
/// and `f32` has no stable atomic. A relaxed store is all this needs — it feeds
/// an animation, so a reader seeing a slightly stale value is invisible.
#[derive(Clone, Default)]
pub struct AudioLevel(Arc<AtomicU16>);

impl AudioLevel {
    const SCALE: f32 = 10_000.0;

    pub fn get(&self) -> f32 {
        self.0.load(Ordering::Relaxed) as f32 / Self::SCALE
    }

    fn set(&self, level: f32) {
        self.0.store(
            (level.clamp(0.0, 1.0) * Self::SCALE) as u16,
            Ordering::Relaxed,
        );
    }

    /// Publish the level for one callback's worth of samples.
    ///
    /// Speech sits far below full scale, so raw RMS would leave the meter
    /// barely twitching. The curve below maps a normal speaking voice across
    /// most of the range; it is for looking at, not for measuring.
    fn publish(&self, sum_sq: f32, n: usize) {
        let rms = (sum_sq / n as f32).sqrt();
        self.set((rms * 6.0).powf(0.7).min(1.0));
    }
}

pub struct CpalAudioCapture {
    device: cpal::Device,
    config: cpal::SupportedStreamConfig,
    requested_device: Option<String>,
    buffer: Arc<Mutex<CaptureBuffer>>,
    stream: Option<cpal::Stream>,
    level: AudioLevel,
    stream_error: Arc<Mutex<Option<String>>>,
}

/// Capture is normalized incrementally rather than storing device-rate audio
/// and re-cloning/re-sampling the complete history every live-caption tick.
/// Besides turning O(duration^2) work into O(new audio), this keeps the realtime
/// callback's mutex hold short and gives the buffer a clear hard bound.
#[derive(Default)]
struct CaptureBuffer {
    samples: Vec<f32>,
    resampler: Option<vocalcode_core::resample::MonoResampler>,
    overflowed: bool,
}

impl CaptureBuffer {
    fn push_mono(&mut self, sample: f32, input_rate: u32, limit: usize) {
        if self.resampler.is_none() {
            self.resampler = vocalcode_core::resample::MonoResampler::new(input_rate, TARGET_RATE);
        }
        let Some(resampler) = self.resampler.as_mut() else {
            self.overflowed = true;
            return;
        };
        let samples = &mut self.samples;
        let overflowed = &mut self.overflowed;
        if !resampler.push(sample, |sample| {
            if samples.len() < limit {
                samples.push(sample);
            } else {
                *overflowed = true;
            }
        }) {
            *overflowed = true;
        }
    }

    fn finish(&mut self, input_rate: u32, limit: usize) {
        if input_rate == 0 {
            self.overflowed = true;
            return;
        }
        if let Some(resampler) = self.resampler.as_mut() {
            let samples = &mut self.samples;
            let overflowed = &mut self.overflowed;
            resampler.finish(|sample| {
                if samples.len() < limit {
                    samples.push(sample);
                } else {
                    *overflowed = true;
                }
            });
        }
    }
}

/// Stable value and human label for one microphone picker option. CPAL's
/// device ID is designed to survive reconnects/reboots where the host can
/// provide that guarantee; a display name alone is neither unique nor stable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDeviceChoice {
    pub selector: String,
    pub label: String,
    /// Kept only to migrate old name-based settings when the name is unique.
    pub legacy_name: String,
}

#[derive(Debug, Clone)]
struct DeviceChoiceRecord {
    id: String,
    name: String,
    detail: String,
}

fn clean_device_label(value: &str) -> String {
    let cleaned = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        "Unnamed microphone".to_string()
    } else {
        cleaned.chars().take(256).collect()
    }
}

fn choices_from_records(mut records: Vec<DeviceChoiceRecord>) -> Vec<InputDeviceChoice> {
    records.retain(|record| {
        !record.id.is_empty()
            && record.id.len() <= 2_048
            && !record.id.chars().any(char::is_control)
            && !record.name.is_empty()
    });
    records.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    // A backend should enumerate each stable ID once, but do not rely on it:
    // duplicate IDs are not necessarily adjacent after sorting by display
    // name. Publishing the same selector twice would make the picker
    // ambiguous and could select a different physical device after restart.
    let mut seen_ids = BTreeSet::new();
    records.retain(|record| seen_ids.insert(record.id.clone()));

    let mut choices = Vec::with_capacity(records.len());
    let mut start = 0;
    while start < records.len() {
        let name = records[start].name.clone();
        let mut end = start + 1;
        while end < records.len() && records[end].name == name {
            end += 1;
        }
        let total = end - start;
        for (offset, record) in records[start..end].iter().enumerate() {
            let base = if total > 1 && record.detail != record.name {
                clean_device_label(&record.detail)
            } else {
                clean_device_label(&record.name)
            };
            // Descriptions supplied by drivers are often identical too. The
            // stable-ID sort keeps numbering deterministic across launches.
            let same_detail = records[start..end]
                .iter()
                .filter(|candidate| candidate.detail == record.detail)
                .count();
            let label = if total > 1 && (record.detail == record.name || same_detail > 1) {
                format!("{base} · {}/{}", offset + 1, total)
            } else {
                base
            };
            choices.push(InputDeviceChoice {
                selector: format!("{DEVICE_ID_PREFIX}{}", record.id),
                label,
                legacy_name: record.name.clone(),
            });
        }
        start = end;
    }
    choices
}

/// Available input devices with stable selectors and disambiguated labels.
pub fn list_input_device_choices() -> Vec<InputDeviceChoice> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|it| {
            choices_from_records(
                it.filter_map(|device| {
                    let id = device.id().ok()?.to_string();
                    let description = device.description().ok()?;
                    Some(DeviceChoiceRecord {
                        id,
                        name: description.name().to_string(),
                        detail: description.to_string(),
                    })
                })
                .collect(),
            )
        })
        .unwrap_or_default()
}

fn resolve_input_device(device_selector: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    let Some(selector) = device_selector.filter(|value| !value.is_empty()) else {
        return host
            .default_input_device()
            .ok_or_else(|| VocalCodeError::Audio("no default input device".into()));
    };
    let devices = host.input_devices().map_err(|error| {
        VocalCodeError::Audio(format!("could not enumerate input devices: {error}"))
    })?;

    if let Some(wanted_id) = selector.strip_prefix(DEVICE_ID_PREFIX) {
        return devices
            .filter_map(|device| device.id().ok().map(|id| (device, id.to_string())))
            .find_map(|(device, id)| (id == wanted_id).then_some(device))
            .ok_or_else(|| {
                VocalCodeError::Audio("selected input device is unavailable".to_string())
            });
    }

    // Backward-compatible migration for settings written before stable IDs.
    // A duplicate name is deliberately not guessed: choosing the wrong laptop
    // or headset microphone is both surprising and a privacy failure.
    let mut matches = devices.filter(|device| {
        device
            .description()
            .map(|description| description.name() == selector)
            .unwrap_or(false)
    });
    let Some(device) = matches.next() else {
        return Err(VocalCodeError::Audio(
            "selected input device is unavailable".to_string(),
        ));
    };
    if matches.next().is_some() {
        let label = clean_device_label(selector);
        return Err(VocalCodeError::Audio(format!(
            "more than one input device is named '{label}'; select the exact microphone again"
        )));
    }
    Ok(device)
}

impl CpalAudioCapture {
    /// Open the default input device.
    pub fn new() -> Result<Self> {
        Self::new_for(None)
    }

    /// Open a specific input device by stable selector (or one unique legacy
    /// name during migration). An explicit selection is exact:
    /// silently recording from the laptop microphone when a wireless headset
    /// disappears is both surprising and a privacy problem. Callers that want
    /// the system default pass `None` explicitly.
    pub fn new_for(device_selector: Option<&str>) -> Result<Self> {
        Self::new_for_with_level(device_selector, AudioLevel::default())
    }

    /// Open a device while continuing to publish into an existing meter. The UI
    /// keeps an `AudioLevel` clone for its lifetime, so hot-swapping microphones
    /// must reuse the same atomic source rather than handing it a now-orphaned
    /// meter from the old capture object.
    pub fn new_for_with_level(device_selector: Option<&str>, level: AudioLevel) -> Result<Self> {
        let requested_device = device_selector
            .filter(|selector| !selector.is_empty())
            .map(ToOwned::to_owned);
        let device = resolve_input_device(requested_device.as_deref())?;
        let config = device
            .default_input_config()
            .map_err(|e| VocalCodeError::Audio(format!("default input config: {e}")))?;
        log::info!(
            "input device @ {} Hz, {} ch, {:?}",
            config.sample_rate(),
            config.channels(),
            config.sample_format()
        );
        Ok(Self {
            device,
            config,
            requested_device,
            buffer: Arc::new(Mutex::new(CaptureBuffer::default())),
            stream: None,
            level,
            stream_error: Arc::new(Mutex::new(None)),
        })
    }
}

impl CpalAudioCapture {
    /// Handle to the live input level, for a recording indicator.
    pub fn level(&self) -> AudioLevel {
        self.level.clone()
    }
}

impl AudioCapture for CpalAudioCapture {
    fn start(&mut self) -> Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        // Re-resolve between utterances. This follows a changed system default
        // and recovers the same explicitly selected stable device after a
        // disconnect/reconnect without silently falling back to another mic.
        let device = resolve_input_device(self.requested_device.as_deref())?;
        let config = device
            .default_input_config()
            .map_err(|error| VocalCodeError::Audio(format!("default input config: {error}")))?;
        self.device = device;
        self.config = config;
        *self
            .buffer
            .lock()
            .map_err(|_| VocalCodeError::Audio("capture buffer lock poisoned".into()))? =
            CaptureBuffer::default();
        if let Ok(mut error) = self.stream_error.lock() {
            *error = None;
        }

        let channels = self.config.channels() as usize;
        if channels == 0 {
            return Err(VocalCodeError::Audio(
                "input device reported zero channels".into(),
            ));
        }
        let input_rate = self.config.sample_rate();
        let stream_config: cpal::StreamConfig = self.config.config();
        let buf = Arc::clone(&self.buffer);
        let level = self.level.clone();
        let async_error = Arc::clone(&self.stream_error);
        let stream_error = Arc::clone(&async_error);
        let callback_seen = Arc::new(AtomicBool::new(false));
        let err_fn = move |e: cpal::Error| {
            // CPAL invokes this on an audio-driver callback thread. Never run
            // the synchronous stderr/file logger here: the cross-process log
            // lock alone may wait hundreds of milliseconds, which can starve
            // the driver while it is already reporting a failure. Publish the
            // first error into memory; `take_error` moves it to the ordinary
            // engine/event-loop path where it is surfaced and logged safely.
            if let Ok(mut slot) = stream_error.lock() {
                if slot.is_none() {
                    *slot = Some(e.to_string());
                }
            }
        };

        // Push one mono sample per frame by averaging channels.
        macro_rules! build {
            ($t:ty, $to_f32:expr) => {{
                let buf = Arc::clone(&buf);
                let callback_error = Arc::clone(&async_error);
                let callback_seen = Arc::clone(&callback_seen);
                self.device.build_input_stream(
                    stream_config.clone(),
                    move |data: &[$t], _: &cpal::InputCallbackInfo| {
                        let Ok(mut b) = buf.lock() else {
                            if let Ok(mut slot) = callback_error.lock() {
                                if slot.is_none() {
                                    *slot = Some("capture buffer lock poisoned".to_string());
                                }
                            }
                            return;
                        };
                        // The callback is only ready once its shared capture
                        // state is usable. Otherwise `start` could win a race
                        // against the poison report and return a false success.
                        callback_seen.store(true, Ordering::Release);
                        let mut sum_sq = 0.0f32;
                        let mut n = 0usize;
                        for frame in data.chunks(channels) {
                            let sum: f32 = frame.iter().map(|&s| $to_f32(s)).sum();
                            let mono = sum / channels as f32;
                            b.push_mono(mono, input_rate, MAX_CAPTURE_SAMPLES);
                            sum_sq += mono * mono;
                            n += 1;
                        }
                        if n > 0 {
                            level.publish(sum_sq, n);
                        }
                    },
                    err_fn,
                    None,
                )
            }};
        }

        let stream = match self.config.sample_format() {
            cpal::SampleFormat::F32 => build!(f32, |s: f32| s),
            cpal::SampleFormat::F64 => build!(f64, |s: f64| s as f32),
            cpal::SampleFormat::I8 => build!(i8, |s: i8| s as f32 / 128.0),
            cpal::SampleFormat::I16 => build!(i16, |s: i16| s as f32 / 32768.0),
            cpal::SampleFormat::I24 => build!(cpal::I24, i24_to_f32),
            cpal::SampleFormat::I32 => build!(i32, |s: i32| s as f32 / 2_147_483_648.0),
            cpal::SampleFormat::I64 => build!(i64, |s: i64| {
                (s as f64 / 9_223_372_036_854_775_808.0) as f32
            }),
            cpal::SampleFormat::U8 => build!(u8, |s: u8| (s as f32 - 128.0) / 128.0),
            cpal::SampleFormat::U16 => build!(u16, |s: u16| (s as f32 - 32768.0) / 32768.0),
            cpal::SampleFormat::U24 => build!(cpal::U24, u24_to_f32),
            cpal::SampleFormat::U32 => build!(u32, |s: u32| {
                (s as f64 - 2_147_483_648.0) as f32 / 2_147_483_648.0
            }),
            cpal::SampleFormat::U64 => build!(u64, |s: u64| {
                ((s as f64 - 9_223_372_036_854_775_808.0) / 9_223_372_036_854_775_808.0) as f32
            }),
            other => {
                return Err(VocalCodeError::Audio(format!(
                    "unsupported sample format {other:?}"
                )))
            }
        }
        .map_err(|e| VocalCodeError::Audio(format!("build input stream: {e}")))?;

        stream
            .play()
            .map_err(|e| VocalCodeError::Audio(format!("play stream: {e}")))?;

        let deadline = Instant::now() + STREAM_START_TIMEOUT;
        while !callback_seen.load(Ordering::Acquire) {
            if let Some(error) = self
                .stream_error
                .lock()
                .ok()
                .and_then(|mut error| error.take())
            {
                return Err(VocalCodeError::Audio(format!(
                    "input stream failed while starting: {error}"
                )));
            }
            if Instant::now() >= deadline {
                return Err(VocalCodeError::Audio(format!(
                    "input stream produced no audio callback within {} ms",
                    STREAM_START_TIMEOUT.as_millis()
                )));
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        // Close the narrow race where an error callback arrives immediately
        // after the first data callback but before ownership is committed.
        if let Some(error) = self
            .stream_error
            .lock()
            .ok()
            .and_then(|mut error| error.take())
        {
            return Err(VocalCodeError::Audio(format!(
                "input stream failed while starting: {error}"
            )));
        }
        self.stream = Some(stream);
        Ok(())
    }

    fn stop(&mut self) -> Result<Recording> {
        // Dropping the stream stops capture.
        self.stream = None;
        // Zero the meter, or the indicator freezes at the last frame's height.
        self.level.set(0.0);
        let (samples, overflowed) = {
            let mut captured = self
                .buffer
                .lock()
                .map_err(|_| VocalCodeError::Audio("capture buffer lock poisoned".into()))?;
            captured.finish(self.config.sample_rate(), MAX_CAPTURE_SAMPLES);
            let overflowed = captured.overflowed;
            (std::mem::take(&mut captured.samples), overflowed)
        };
        let stream_error = self
            .stream_error
            .lock()
            .ok()
            .and_then(|mut error| error.take());
        if let Some(error) = stream_error {
            return Err(VocalCodeError::Audio(format!(
                "input stream failed: {error}"
            )));
        }
        if overflowed {
            return Err(VocalCodeError::Audio(format!(
                "recording exceeded the {MAX_CAPTURE_SECS}-second safety buffer"
            )));
        }
        Ok(Recording {
            samples,
            sample_rate: TARGET_RATE,
        })
    }

    fn is_recording(&self) -> bool {
        self.stream.is_some()
    }

    fn snapshot(&self) -> Result<Recording> {
        self.snapshot_since(0)
    }

    fn snapshot_since(&self, start: usize) -> Result<Recording> {
        let captured = self
            .buffer
            .lock()
            .map_err(|_| VocalCodeError::Audio("capture buffer lock poisoned".into()))?;
        let start = start.min(captured.samples.len());
        Ok(Recording {
            samples: captured.samples[start..].to_vec(),
            sample_rate: TARGET_RATE,
        })
    }

    fn take_error(&self) -> Option<String> {
        if let Some(error) = self
            .stream_error
            .lock()
            .ok()
            .and_then(|mut error| error.take())
        {
            return Some(error);
        }
        self.buffer.lock().ok().and_then(|mut captured| {
            if captured.overflowed {
                captured.overflowed = false;
                Some(format!(
                    "recording exceeded the {MAX_CAPTURE_SECS}-second safety buffer"
                ))
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Silence must read as zero, or the indicator shows bars with no sound.
    #[test]
    fn silence_is_zero() {
        let l = AudioLevel::default();
        l.publish(0.0, 512);
        assert_eq!(l.get(), 0.0);
    }

    /// Louder input must never read lower — the meter has to be monotonic or it
    /// looks like it is reacting to the wrong thing.
    #[test]
    fn louder_reads_higher() {
        let l = AudioLevel::default();
        let mut last = -1.0;
        for amp in [0.001f32, 0.01, 0.05, 0.1, 0.3] {
            l.publish(amp * amp * 512.0, 512);
            let v = l.get();
            assert!(v > last, "level fell from {last} to {v} at amplitude {amp}");
            last = v;
        }
    }

    /// Full-scale input must not overflow the fixed-point encoding.
    #[test]
    fn stays_within_range() {
        let l = AudioLevel::default();
        l.publish(1.0 * 512.0, 512); // RMS 1.0, well past the boost curve
        assert!(l.get() <= 1.0, "level {} exceeded 1.0", l.get());
        assert!(l.get() > 0.9, "full scale should read near the top");
    }

    /// Ordinary speech should land in a visible part of the range — a meter
    /// that only twitches for shouting is the reason the raw RMS is boosted.
    #[test]
    fn speech_is_clearly_visible() {
        let l = AudioLevel::default();
        l.publish(0.03f32.powi(2) * 512.0, 512); // ~ -30 dBFS, a normal voice
        let v = l.get();
        assert!(v > 0.2, "quiet speech reads {v}, too low to see");
    }

    #[test]
    fn capture_is_normalized_incrementally() {
        let mut b = CaptureBuffer::default();
        for i in 0..48_000 {
            b.push_mono(i as f32 / 48_000.0, 48_000, usize::MAX);
        }
        b.finish(48_000, usize::MAX);
        assert_eq!(b.samples.len(), 16_000);
        assert!((b.samples[8_000] - 0.5).abs() < 0.001);

        let mut b = CaptureBuffer::default();
        for i in 0..8_000 {
            b.push_mono(i as f32 / 8_000.0, 8_000, usize::MAX);
        }
        b.finish(8_000, usize::MAX);
        assert_eq!(b.samples.len(), 16_000);
    }

    #[test]
    fn capture_buffer_stops_growing_at_its_limit() {
        let mut b = CaptureBuffer::default();
        for i in 0..1_000 {
            b.push_mono(i as f32, 16_000, 32);
        }
        assert_eq!(b.samples.len(), 32);
        assert!(b.overflowed);
    }

    #[test]
    fn signed_24_bit_samples_cover_the_normalized_range() {
        assert_eq!(i24_to_f32(cpal::I24::new(0).unwrap()), 0.0);
        assert_eq!(i24_to_f32(cpal::I24::new(-8_388_608).unwrap()), -1.0);
        let max = i24_to_f32(cpal::I24::new(8_388_607).unwrap());
        assert!(max > 0.999_999 && max < 1.0);
    }

    #[test]
    fn unsigned_24_bit_samples_are_centered_at_zero() {
        assert_eq!(u24_to_f32(cpal::U24::new(0).unwrap()), -1.0);
        assert_eq!(u24_to_f32(cpal::U24::new(8_388_608).unwrap()), 0.0);
        let max = u24_to_f32(cpal::U24::new(16_777_215).unwrap());
        assert!(max > 0.999_999 && max < 1.0);
    }

    #[test]
    fn duplicate_microphone_names_get_stable_distinct_options() {
        let choices = choices_from_records(vec![
            DeviceChoiceRecord {
                id: "usb-b".into(),
                name: "Conference Mic".into(),
                detail: "USB Conference Mic".into(),
            },
            DeviceChoiceRecord {
                id: "usb-a".into(),
                name: "Conference Mic".into(),
                detail: "USB Conference Mic".into(),
            },
        ]);

        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].selector, "vocalcode-cpal-device:usb-a");
        assert_eq!(choices[1].selector, "vocalcode-cpal-device:usb-b");
        assert_eq!(choices[0].label, "USB Conference Mic \u{b7} 1/2");
        assert_eq!(choices[1].label, "USB Conference Mic \u{b7} 2/2");
        assert!(choices
            .iter()
            .all(|choice| choice.legacy_name == "Conference Mic"));
    }

    #[test]
    fn microphone_labels_are_sanitized_and_bounded() {
        let long_name = format!("Studio\nMic {}", "x".repeat(400));
        let choices = choices_from_records(vec![DeviceChoiceRecord {
            id: "safe-id".into(),
            name: long_name.clone(),
            detail: long_name,
        }]);

        assert_eq!(choices.len(), 1);
        assert!(choices[0].label.starts_with("Studio Mic "));
        assert_eq!(choices[0].label.chars().count(), 256);
        assert!(!choices[0].label.chars().any(char::is_control));
    }

    #[test]
    fn repeated_or_unsafe_device_ids_never_create_ambiguous_selectors() {
        let choices = choices_from_records(vec![
            DeviceChoiceRecord {
                id: "same-id".into(),
                name: "Zulu".into(),
                detail: "Zulu".into(),
            },
            DeviceChoiceRecord {
                id: "other-id".into(),
                name: "Middle".into(),
                detail: "Middle".into(),
            },
            DeviceChoiceRecord {
                id: "same-id".into(),
                name: "Alpha".into(),
                detail: "Alpha".into(),
            },
            DeviceChoiceRecord {
                id: "bad\nid".into(),
                name: "Unsafe".into(),
                detail: "Unsafe".into(),
            },
        ]);

        assert_eq!(choices.len(), 2);
        assert_eq!(
            choices
                .iter()
                .filter(|choice| choice.selector.ends_with("same-id"))
                .count(),
            1
        );
        assert!(choices
            .iter()
            .all(|choice| !choice.selector.chars().any(char::is_control)));
    }
}
