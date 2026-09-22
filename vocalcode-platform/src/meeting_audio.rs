//! Bounded streaming capture for local meeting transcription.
//!
//! CPAL treats an output device used as an input as loopback capture. On
//! Windows this is WASAPI loopback; on macOS 14.6+ it is a CoreAudio process
//! tap. This module keeps those platform details out of the meeting pipeline.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{SyncSender, TrySendError},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use vocalcode_core::error::{Result, VocalCodeError};

const START_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_CHANNELS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamedAudioSource {
    Microphone,
    System,
}

#[derive(Debug, Clone)]
pub struct StreamedAudioBlock {
    pub source: StreamedAudioSource,
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: usize,
    pub start_ms: u64,
}

/// Owns all live streams. Dropping it is the stop operation.
pub struct MeetingAudioCapture {
    streams: Vec<cpal::Stream>,
    error: Arc<Mutex<Option<String>>>,
}

impl MeetingAudioCapture {
    pub fn take_error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|mut error| error.take())
    }

    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }
}

/// Starts explicit microphone and/or system-output capture. The caller must
/// provide a bounded sender; callbacks use `try_send` and turn overflow into a
/// surfaced recording error instead of blocking the real-time audio thread.
pub fn start_meeting_audio(
    microphone_selector: Option<&str>,
    microphone: bool,
    system_audio: bool,
    sender: SyncSender<StreamedAudioBlock>,
) -> Result<MeetingAudioCapture> {
    if !microphone && !system_audio {
        return Err(VocalCodeError::Audio(
            "select microphone, system audio, or both".to_string(),
        ));
    }
    let host = cpal::default_host();
    let error = Arc::new(Mutex::new(None));
    let mut streams = Vec::with_capacity(2);
    let mut callbacks = Vec::with_capacity(2);

    if microphone {
        let device = resolve_microphone(&host, microphone_selector)?;
        let config = device
            .default_input_config()
            .map_err(|problem| VocalCodeError::Audio(format!("microphone format: {problem}")))?;
        let (stream, seen) = build_stream(
            device,
            config,
            StreamedAudioSource::Microphone,
            sender.clone(),
            error.clone(),
        )?;
        streams.push(stream);
        callbacks.push(("microphone", seen));
    }

    if system_audio {
        #[cfg(target_os = "macos")]
        if !macos_system_audio_supported() {
            return Err(VocalCodeError::Audio(
                "system audio capture requires macOS 14.6 or later; microphone recording and file import remain available"
                    .to_string(),
            ));
        }
        let device = host
            .default_output_device()
            .ok_or_else(|| VocalCodeError::Audio("no default system output device".to_string()))?;
        // CPAL's loopback contract uses the output format for devices that do
        // not have a physical input side.
        let config = if device.supports_input() {
            device.default_input_config()
        } else {
            device.default_output_config()
        }
        .map_err(|problem| VocalCodeError::Audio(format!("system output format: {problem}")))?;
        let (stream, seen) = build_stream(
            device,
            config,
            StreamedAudioSource::System,
            sender,
            error.clone(),
        )?;
        streams.push(stream);
        callbacks.push(("system audio", seen));
    }

    for stream in &streams {
        stream
            .play()
            .map_err(|problem| VocalCodeError::Audio(format!("start meeting audio: {problem}")))?;
    }

    let deadline = Instant::now() + START_TIMEOUT;
    while callbacks
        .iter()
        .any(|(_, seen)| !seen.load(Ordering::Acquire))
    {
        if let Some(problem) = error.lock().ok().and_then(|mut value| value.take()) {
            return Err(VocalCodeError::Audio(format!(
                "meeting audio failed while starting: {problem}"
            )));
        }
        if Instant::now() >= deadline {
            let pending = callbacks
                .iter()
                .filter(|(_, seen)| !seen.load(Ordering::Acquire))
                .map(|(label, _)| *label)
                .collect::<Vec<_>>()
                .join(" and ");
            return Err(VocalCodeError::Audio(format!(
                "{pending} produced no audio callback within {} ms",
                START_TIMEOUT.as_millis()
            )));
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    Ok(MeetingAudioCapture { streams, error })
}

fn resolve_microphone(host: &cpal::Host, selector: Option<&str>) -> Result<cpal::Device> {
    const PREFIX: &str = "vocalcode-cpal-device:";
    let Some(selector) = selector.filter(|value| !value.is_empty()) else {
        return host
            .default_input_device()
            .ok_or_else(|| VocalCodeError::Audio("no default microphone".to_string()));
    };
    let devices = host.input_devices().map_err(|problem| {
        VocalCodeError::Audio(format!("could not enumerate microphones: {problem}"))
    })?;
    if let Some(identifier) = selector.strip_prefix(PREFIX) {
        return devices
            .filter_map(|device| device.id().ok().map(|id| (device, id.to_string())))
            .find_map(|(device, id)| (id == identifier).then_some(device))
            .ok_or_else(|| {
                VocalCodeError::Audio("selected microphone is unavailable".to_string())
            });
    }
    let mut matches = devices.filter(|device| {
        device
            .description()
            .map(|description| description.name() == selector)
            .unwrap_or(false)
    });
    let device = matches
        .next()
        .ok_or_else(|| VocalCodeError::Audio("selected microphone is unavailable".to_string()))?;
    if matches.next().is_some() {
        return Err(VocalCodeError::Audio(
            "more than one microphone has that name; select the exact device again".to_string(),
        ));
    }
    Ok(device)
}

fn build_stream(
    device: cpal::Device,
    config: cpal::SupportedStreamConfig,
    source: StreamedAudioSource,
    sender: SyncSender<StreamedAudioBlock>,
    error: Arc<Mutex<Option<String>>>,
) -> Result<(cpal::Stream, Arc<AtomicBool>)> {
    let channels = config.channels() as usize;
    let sample_rate = config.sample_rate();
    if channels == 0 || channels > MAX_CHANNELS || sample_rate == 0 {
        return Err(VocalCodeError::Audio(format!(
            "unsupported meeting audio layout: {sample_rate} Hz, {channels} channels"
        )));
    }
    let stream_config = config.config();
    let seen = Arc::new(AtomicBool::new(false));
    let frames = Arc::new(AtomicU64::new(0));
    let error_callback = error.clone();
    let on_error = move |problem: cpal::Error| publish_error(&error_callback, problem.to_string());

    macro_rules! build {
        ($sample:ty, $convert:expr) => {{
            let sender = sender.clone();
            let error = error.clone();
            let seen = seen.clone();
            let frames = frames.clone();
            device.build_input_stream(
                stream_config.clone(),
                move |data: &[$sample], _: &cpal::InputCallbackInfo| {
                    seen.store(true, Ordering::Release);
                    if data.is_empty() || data.len() % channels != 0 {
                        if !data.is_empty() {
                            publish_error(&error, "audio callback returned incomplete frames".to_string());
                        }
                        return;
                    }
                    let frame_count = data.len() / channels;
                    let first_frame = frames.fetch_add(frame_count as u64, Ordering::Relaxed);
                    let samples = data.iter().copied().map($convert).collect();
                    let block = StreamedAudioBlock {
                        source,
                        samples,
                        sample_rate,
                        channels,
                        start_ms: first_frame.saturating_mul(1_000) / sample_rate as u64,
                    };
                    match sender.try_send(block) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => publish_error(
                            &error,
                            "the local meeting pipeline could not keep up; recording stopped before silently losing audio".to_string(),
                        ),
                        Err(TrySendError::Disconnected(_)) => {}
                    }
                },
                on_error,
                None,
            )
        }};
    }

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => build!(f32, |sample: f32| sample),
        cpal::SampleFormat::F64 => build!(f64, |sample: f64| sample as f32),
        cpal::SampleFormat::I8 => build!(i8, |sample: i8| sample as f32 / 128.0),
        cpal::SampleFormat::I16 => build!(i16, |sample: i16| sample as f32 / 32768.0),
        cpal::SampleFormat::I24 => build!(cpal::I24, |sample: cpal::I24| sample.inner() as f32
            / 8_388_608.0),
        cpal::SampleFormat::I32 => build!(i32, |sample: i32| sample as f32 / 2_147_483_648.0),
        cpal::SampleFormat::I64 => build!(i64, |sample: i64| (sample as f64
            / 9_223_372_036_854_775_808.0)
            as f32),
        cpal::SampleFormat::U8 => build!(u8, |sample: u8| (sample as f32 - 128.0) / 128.0),
        cpal::SampleFormat::U16 => build!(u16, |sample: u16| (sample as f32 - 32768.0) / 32768.0),
        cpal::SampleFormat::U24 => build!(cpal::U24, |sample: cpal::U24| (sample.inner() as f32
            - 8_388_608.0)
            / 8_388_608.0),
        cpal::SampleFormat::U32 => build!(u32, |sample: u32| (sample as f64 - 2_147_483_648.0)
            as f32
            / 2_147_483_648.0),
        cpal::SampleFormat::U64 => build!(u64, |sample: u64| ((sample as f64
            - 9_223_372_036_854_775_808.0)
            / 9_223_372_036_854_775_808.0)
            as f32),
        other => {
            return Err(VocalCodeError::Audio(format!(
                "unsupported meeting sample format {other:?}"
            )))
        }
    }
    .map_err(|problem| VocalCodeError::Audio(format!("build meeting stream: {problem}")))?;
    Ok((stream, seen))
}

fn publish_error(slot: &Mutex<Option<String>>, problem: String) {
    if let Ok(mut slot) = slot.lock() {
        if slot.is_none() {
            *slot = Some(problem);
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_system_audio_supported() -> bool {
    // CPAL's CoreAudio process-tap implementation requires macOS 14.6+.
    let output = std::process::Command::new("/usr/bin/sw_vers")
        .args(["-productVersion"])
        .output();
    let Ok(output) = output else { return false };
    let version = String::from_utf8_lossy(&output.stdout);
    let mut parts = version
        .trim()
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok());
    let major = parts.next().unwrap_or_default();
    let minor = parts.next().unwrap_or_default();
    major > 14 || (major == 14 && minor >= 6)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_slot_preserves_the_first_failure() {
        let slot = Mutex::new(None);
        publish_error(&slot, "first".to_string());
        publish_error(&slot, "second".to_string());
        assert_eq!(slot.into_inner().unwrap().as_deref(), Some("first"));
    }
}
