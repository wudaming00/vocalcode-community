use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::{AudioSource, MeetingError, Result};

pub const TARGET_SAMPLE_RATE: u32 = 16_000;
const MAX_IMPORT_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_IMPORT_DURATION_MS: u64 = 12 * 60 * 60 * 1_000;
const MAX_CHANNELS: usize = 32;
const FRAME_SAMPLES: usize = 320; // 20 ms at 16 kHz
const PRE_ROLL_FRAMES: usize = 8;
const SPEECH_START_FRAMES: usize = 2;
const SPEECH_END_SILENCE_FRAMES: usize = 25;
const TRAILING_SILENCE_FRAMES: usize = 6;
// This is a candidate generator, not a language-dependent short-word filter.
// A 600ms minimum discarded valid isolated answers such as 不 / 是 / No before
// the neural speech gate could examine them. The downstream gate distinguishes
// brief noise from speech; 200ms also meets the recognizer's minimum input size.
const MIN_SPEECH_SAMPLES: usize = TARGET_SAMPLE_RATE as usize * 200 / 1_000;
const MAX_SPEECH_SAMPLES: usize = TARGET_SAMPLE_RATE as usize * 45;
const SPLIT_OVERLAP_FRAMES: usize = 10;
const DEFAULT_CHUNK_SAMPLES: usize = TARGET_SAMPLE_RATE as usize * 60;

#[derive(Debug, Clone)]
pub struct AudioBlock {
    /// Interleaved normalized floating-point samples.
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: usize,
    pub start_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDecoderInfo {
    pub sample_rate: u32,
    pub channels: usize,
    pub duration_ms: u64,
}

/// Decode a supported local audio file without retaining the whole recording.
///
/// The callback receives bounded decoder packets.  The absolute source path is
/// never copied into meeting metadata by this layer.
pub fn decode_audio_file(
    path: &Path,
    mut on_block: impl FnMut(AudioBlock) -> Result<()>,
) -> Result<AudioDecoderInfo> {
    let metadata = fs::metadata(path).map_err(|error| MeetingError::io(path, error))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_IMPORT_BYTES {
        return Err(MeetingError::TooLarge(
            "audio file is empty, not a regular file, or exceeds 64 GiB".to_string(),
        ));
    }

    let file = File::open(path).map_err(|error| MeetingError::io(path, error))?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
        hint.with_extension(extension);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|error| MeetingError::UnsupportedAudio(error.to_string()))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|track| track.codec_params.sample_rate.is_some())
        .ok_or_else(|| {
            MeetingError::UnsupportedAudio("file contains no audio track".to_string())
        })?;
    let track_id = track.id;
    let codec_params = track.codec_params.clone();
    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|error| MeetingError::UnsupportedAudio(error.to_string()))?;

    let mut total_frames = 0u64;
    let mut observed_rate = 0u32;
    let mut observed_channels = 0usize;
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(SymphoniaError::ResetRequired) => {
                return Err(MeetingError::AudioDecode(
                    "audio stream changed format mid-file".to_string(),
                ))
            }
            Err(error) => return Err(MeetingError::AudioDecode(error.to_string())),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(error) => return Err(MeetingError::AudioDecode(error.to_string())),
        };
        let specification = *decoded.spec();
        let rate = specification.rate;
        let channels = specification.channels.count();
        if !(8_000..=384_000).contains(&rate) || channels == 0 || channels > MAX_CHANNELS {
            return Err(MeetingError::UnsupportedAudio(format!(
                "unsupported audio layout: {rate} Hz, {channels} channels"
            )));
        }
        if observed_rate != 0 && (observed_rate != rate || observed_channels != channels) {
            return Err(MeetingError::AudioDecode(
                "audio sample rate or channel count changed mid-file".to_string(),
            ));
        }
        observed_rate = rate;
        observed_channels = channels;
        let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, specification);
        buffer.copy_interleaved_ref(decoded);
        let frame_count = buffer.samples().len() / channels;
        let start_ms = total_frames.saturating_mul(1_000) / rate as u64;
        total_frames = total_frames.saturating_add(frame_count as u64);
        let duration_ms = total_frames.saturating_mul(1_000) / rate as u64;
        if duration_ms > MAX_IMPORT_DURATION_MS {
            return Err(MeetingError::TooLarge(
                "audio duration exceeds the 12-hour safety limit".to_string(),
            ));
        }
        on_block(AudioBlock {
            samples: buffer.samples().to_vec(),
            sample_rate: rate,
            channels,
            start_ms,
        })?;
    }
    if observed_rate == 0 || total_frames == 0 {
        return Err(MeetingError::AudioDecode(
            "audio decoder produced no samples".to_string(),
        ));
    }
    Ok(AudioDecoderInfo {
        sample_rate: observed_rate,
        channels: observed_channels,
        duration_ms: total_frames.saturating_mul(1_000) / observed_rate as u64,
    })
}

/// Incremental downmix + anti-aliased resampler (legacy type name retained).
/// State is carried across decoder or
/// device callbacks so block boundaries do not duplicate or drop a sample.
#[derive(Debug, Default)]
pub struct LinearMonoResampler {
    input_rate: Option<u32>,
    input_channels: Option<usize>,
    resampler: Option<vocalcode_core::resample::MonoResampler>,
    finished: bool,
}

impl LinearMonoResampler {
    pub fn push(&mut self, block: &AudioBlock) -> Result<Vec<f32>> {
        if self.finished {
            return Err(MeetingError::Invalid(
                "audio stream already finished".into(),
            ));
        }
        if block.sample_rate == 0
            || block.channels == 0
            || block.channels > MAX_CHANNELS
            || !block.samples.len().is_multiple_of(block.channels)
        {
            return Err(MeetingError::Invalid("malformed audio block".to_string()));
        }
        if self
            .input_rate
            .is_some_and(|rate| rate != block.sample_rate)
            || self
                .input_channels
                .is_some_and(|channels| channels != block.channels)
        {
            return Err(MeetingError::Invalid(
                "audio format changed inside one stream".to_string(),
            ));
        }
        self.input_rate = Some(block.sample_rate);
        self.input_channels = Some(block.channels);
        if self.resampler.is_none() {
            self.resampler =
                vocalcode_core::resample::MonoResampler::new(block.sample_rate, TARGET_SAMPLE_RATE);
        }
        if self.resampler.is_none() {
            return Err(MeetingError::Invalid(
                "unsupported audio sample rate".into(),
            ));
        }
        let mut output = Vec::with_capacity(
            block
                .samples
                .len()
                .saturating_mul(TARGET_SAMPLE_RATE as usize)
                / block.sample_rate as usize
                / block.channels
                + 2,
        );
        for frame in block.samples.chunks_exact(block.channels) {
            if frame.iter().any(|sample| !sample.is_finite()) {
                return Err(MeetingError::Invalid(
                    "audio block contains a non-finite sample".to_string(),
                ));
            }
            let mono = frame.iter().copied().sum::<f32>() / block.channels as f32;
            self.push_mono(mono.clamp(-1.0, 1.0), block.sample_rate, &mut output);
        }
        Ok(output)
    }

    fn push_mono(&mut self, sample: f32, input_rate: u32, output: &mut Vec<f32>) {
        let _ = input_rate;
        self.resampler
            .as_mut()
            .expect("validated rate")
            .push(sample, |sample| output.push(sample));
    }

    pub fn finish(&mut self) -> Vec<f32> {
        self.finished = true;
        let mut output = Vec::new();
        if let Some(resampler) = self.resampler.as_mut() {
            resampler.finish(|sample| output.push(sample));
        }
        output
    }
}

#[derive(Debug, Clone)]
pub struct SpeechSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub samples: Vec<f32>,
}

/// Bounded streaming energy VAD.  It is intentionally model-free so meeting
/// capture starts immediately; an optional neural diarization pass can refine
/// speakers after recording without making capture depend on another download.
#[derive(Debug)]
pub struct SpeechSegmenter {
    pending: VecDeque<f32>,
    pre_roll: VecDeque<Vec<f32>>,
    active: Vec<f32>,
    active_start_sample: u64,
    sample_cursor: u64,
    emitted_end_sample: u64,
    speech_run: usize,
    silence_run: usize,
    noise_floor: f32,
}

impl Default for SpeechSegmenter {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            pre_roll: VecDeque::with_capacity(PRE_ROLL_FRAMES),
            active: Vec::new(),
            active_start_sample: 0,
            sample_cursor: 0,
            emitted_end_sample: 0,
            speech_run: 0,
            silence_run: 0,
            // Loopback and imported voices can be intelligible below -60 dBFS.
            // The old 0.006 RMS floor discarded whole otherwise-decodable clips.
            noise_floor: 0.00003,
        }
    }
}

impl SpeechSegmenter {
    pub fn push(&mut self, samples: &[f32]) -> Vec<SpeechSegment> {
        self.pending.extend(samples.iter().copied());
        let mut segments = Vec::new();
        while self.pending.len() >= FRAME_SAMPLES {
            let frame: Vec<f32> = self.pending.drain(..FRAME_SAMPLES).collect();
            self.process_frame(frame, &mut segments);
        }
        segments
    }

    fn process_frame(&mut self, frame: Vec<f32>, output: &mut Vec<SpeechSegment>) {
        let mean = frame.iter().sum::<f32>() / frame.len() as f32;
        let rms = (frame
            .iter()
            .map(|sample| (sample - mean).powi(2))
            .sum::<f32>()
            / frame.len() as f32)
            .sqrt();
        let threshold = (self.noise_floor * 3.2).clamp(0.0001, 0.02);
        let speech = rms >= threshold;
        if !speech && self.active.is_empty() {
            self.noise_floor = self.noise_floor * 0.98 + rms.min(0.08) * 0.02;
        }
        self.sample_cursor = self.sample_cursor.saturating_add(frame.len() as u64);

        if self.active.is_empty() {
            self.pre_roll.push_back(frame);
            while self.pre_roll.len() > PRE_ROLL_FRAMES {
                self.pre_roll.pop_front();
            }
            self.speech_run = if speech { self.speech_run + 1 } else { 0 };
            if self.speech_run >= SPEECH_START_FRAMES {
                let pre_roll_samples: usize = self.pre_roll.iter().map(Vec::len).sum();
                self.active_start_sample =
                    self.sample_cursor.saturating_sub(pre_roll_samples as u64);
                for buffered in self.pre_roll.drain(..) {
                    self.active.extend(buffered);
                }
                self.silence_run = 0;
            }
            return;
        }

        self.active.extend(frame);
        self.silence_run = if speech { 0 } else { self.silence_run + 1 };
        if self.silence_run >= SPEECH_END_SILENCE_FRAMES {
            let remove_frames = SPEECH_END_SILENCE_FRAMES - TRAILING_SILENCE_FRAMES;
            let remove_samples = remove_frames * FRAME_SAMPLES;
            self.active
                .truncate(self.active.len().saturating_sub(remove_samples));
            let end_sample = self.sample_cursor.saturating_sub(remove_samples as u64);
            if let Some(segment) = self.take_active(end_sample) {
                output.push(segment);
            }
            self.pre_roll.clear();
            self.speech_run = 0;
            self.silence_run = 0;
        } else if self.active.len() >= MAX_SPEECH_SAMPLES {
            let end_sample = self.sample_cursor;
            let overlap_samples = SPLIT_OVERLAP_FRAMES * FRAME_SAMPLES;
            let overlap = self.active[self.active.len().saturating_sub(overlap_samples)..].to_vec();
            if let Some(segment) = self.take_active(end_sample) {
                output.push(segment);
            }
            self.active_start_sample = end_sample.saturating_sub(overlap.len() as u64);
            self.active = overlap;
            self.silence_run = 0;
        }
    }

    fn take_active(&mut self, end_sample: u64) -> Option<SpeechSegment> {
        let samples = std::mem::take(&mut self.active);
        if samples.len() < MIN_SPEECH_SAMPLES || end_sample <= self.emitted_end_sample {
            return None;
        }
        self.emitted_end_sample = end_sample;
        Some(SpeechSegment {
            start_ms: self.active_start_sample.saturating_mul(1_000) / TARGET_SAMPLE_RATE as u64,
            end_ms: end_sample.saturating_mul(1_000) / TARGET_SAMPLE_RATE as u64,
            samples,
        })
    }

    pub fn finish(&mut self) -> Vec<SpeechSegment> {
        let mut output = Vec::new();
        if !self.pending.is_empty() {
            let mut frame: Vec<f32> = self.pending.drain(..).collect();
            frame.resize(FRAME_SAMPLES, 0.0);
            self.process_frame(frame, &mut output);
        }
        let end_sample = self.sample_cursor;
        output.extend(self.take_active(end_sample));
        output
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioChunk {
    pub source: AudioSource,
    pub index: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub path: PathBuf,
}

/// Writes normalized meeting audio as one-minute PCM chunks.  Memory stays
/// bounded and every completed chunk is independently recoverable after a crash.
pub struct ChunkedPcmWriter {
    root: PathBuf,
    source: AudioSource,
    chunk_samples: usize,
    buffered: Vec<f32>,
    written_samples: u64,
    next_index: u64,
}

impl ChunkedPcmWriter {
    pub fn new(root: &Path, source: AudioSource) -> Result<Self> {
        Self::with_chunk_samples(root, source, DEFAULT_CHUNK_SAMPLES)
    }

    pub fn with_chunk_samples(
        root: &Path,
        source: AudioSource,
        chunk_samples: usize,
    ) -> Result<Self> {
        if chunk_samples == 0 || chunk_samples > TARGET_SAMPLE_RATE as usize * 10 * 60 {
            return Err(MeetingError::Invalid("unsafe audio chunk size".to_string()));
        }
        fs::create_dir_all(root).map_err(|error| MeetingError::io(root, error))?;
        let metadata = fs::symlink_metadata(root).map_err(|error| MeetingError::io(root, error))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(MeetingError::Invalid(
                "meeting audio directory is not a trusted directory".to_string(),
            ));
        }
        Ok(Self {
            root: root.to_path_buf(),
            source,
            chunk_samples,
            buffered: Vec::with_capacity(chunk_samples),
            written_samples: 0,
            next_index: 0,
        })
    }

    pub fn push(&mut self, samples: &[f32]) -> Result<Vec<AudioChunk>> {
        let mut chunks = Vec::new();
        let mut remaining = samples;
        while !remaining.is_empty() {
            let available = self.chunk_samples - self.buffered.len();
            let take = available.min(remaining.len());
            self.buffered.extend_from_slice(&remaining[..take]);
            remaining = &remaining[take..];
            if self.buffered.len() == self.chunk_samples {
                chunks.push(self.flush_one()?);
            }
        }
        Ok(chunks)
    }

    pub fn finish(&mut self) -> Result<Vec<AudioChunk>> {
        if self.buffered.is_empty() {
            Ok(Vec::new())
        } else {
            self.flush_one().map(|chunk| vec![chunk])
        }
    }

    fn flush_one(&mut self) -> Result<AudioChunk> {
        let samples = std::mem::take(&mut self.buffered);
        self.buffered = Vec::with_capacity(self.chunk_samples);
        let label = match self.source {
            AudioSource::Microphone => "microphone",
            AudioSource::System => "system",
            AudioSource::Imported => "imported",
        };
        let name = format!("{label}-{:06}.wav", self.next_index);
        let final_path = self.root.join(&name);
        let temporary = self.root.join(format!(
            ".{name}.part-{}-{}",
            std::process::id(),
            self.next_index
        ));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| MeetingError::io(&temporary, error))?;
        let specification = hound::WavSpec {
            channels: 1,
            sample_rate: TARGET_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::new(BufWriter::new(file), specification)?;
        for sample in &samples {
            writer.write_sample((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        }
        writer.finalize()?;
        OpenOptions::new()
            .write(true)
            .open(&temporary)
            .and_then(|file| file.sync_all())
            .map_err(|error| MeetingError::io(&temporary, error))?;
        fs::rename(&temporary, &final_path).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            MeetingError::io(&final_path, error)
        })?;
        let start_ms = self.written_samples.saturating_mul(1_000) / TARGET_SAMPLE_RATE as u64;
        self.written_samples = self.written_samples.saturating_add(samples.len() as u64);
        let end_ms = self.written_samples.saturating_mul(1_000) / TARGET_SAMPLE_RATE as u64;
        let chunk = AudioChunk {
            source: self.source,
            index: self.next_index,
            start_ms,
            end_ms,
            path: final_path,
        };
        self.next_index = self.next_index.saturating_add(1);
        Ok(chunk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, frequency: f32, seconds: f32) -> Vec<f32> {
        (0..(rate as f32 * seconds) as usize)
            .map(|index| {
                (std::f32::consts::TAU * frequency * index as f32 / rate as f32).sin() * 0.2
            })
            .collect()
    }

    #[test]
    fn streaming_resampler_does_not_change_length_at_sixteen_khz() {
        let samples = sine(16_000, 440.0, 1.0);
        let mut resampler = LinearMonoResampler::default();
        let mut output = Vec::new();
        for (index, block) in samples.chunks(777).enumerate() {
            output.extend(
                resampler
                    .push(&AudioBlock {
                        samples: block.to_vec(),
                        sample_rate: 16_000,
                        channels: 1,
                        start_ms: index as u64,
                    })
                    .unwrap(),
            );
        }
        output.extend(resampler.finish());
        assert_eq!(output.len(), samples.len());
        assert!((output[5_000] - samples[5_000]).abs() < 1e-6);
    }

    #[test]
    fn resampler_rejects_non_finite_audio() {
        let mut resampler = LinearMonoResampler::default();
        assert!(resampler
            .push(&AudioBlock {
                samples: vec![0.0, f32::NAN],
                sample_rate: 16_000,
                channels: 1,
                start_ms: 0,
            })
            .is_err());
    }

    #[test]
    fn resampler_downmixes_and_resamples_across_block_boundaries() {
        let mono = sine(48_000, 220.0, 2.0);
        let stereo: Vec<f32> = mono
            .iter()
            .flat_map(|sample| [*sample, *sample * 0.5])
            .collect();
        let mut resampler = LinearMonoResampler::default();
        let mut output = Vec::new();
        for block in stereo.chunks(2 * 1_003) {
            output.extend(
                resampler
                    .push(&AudioBlock {
                        samples: block.to_vec(),
                        sample_rate: 48_000,
                        channels: 2,
                        start_ms: 0,
                    })
                    .unwrap(),
            );
        }
        output.extend(resampler.finish());
        assert!(
            (output.len() as isize - 32_000).abs() <= 1,
            "{}",
            output.len()
        );
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn segmenter_ignores_silence_and_emits_bounded_speech() {
        let mut input = vec![0.0; TARGET_SAMPLE_RATE as usize];
        input.extend(sine(TARGET_SAMPLE_RATE, 220.0, 1.5));
        input.extend(vec![0.0; TARGET_SAMPLE_RATE as usize]);
        let mut segmenter = SpeechSegmenter::default();
        let mut segments = Vec::new();
        for block in input.chunks(913) {
            segments.extend(segmenter.push(block));
        }
        segments.extend(segmenter.finish());
        assert_eq!(segments.len(), 1);
        assert!(segments[0].start_ms <= 1_000);
        assert!(segments[0].end_ms >= 2_500);
        assert!(segments[0].samples.len() < TARGET_SAMPLE_RATE as usize * 3);
    }

    #[test]
    fn segmenter_preserves_brief_candidates_for_the_neural_gate() {
        for duration in [0.12, 0.20, 0.28] {
            let mut segmenter = SpeechSegmenter::default();
            let mut input = vec![0.0; 16_000];
            input.extend(sine(16_000, 220.0, duration));
            input.extend(vec![0.0; 16_000]);
            let mut segments = Vec::new();
            for block in input.chunks(731) {
                segments.extend(segmenter.push(block));
            }
            segments.extend(segmenter.finish());
            assert_eq!(segments.len(), 1, "{duration}s candidate was lost");
            assert!(segments[0].start_ms <= 1_000);
            assert!(segments[0].end_ms >= 1_000 + (duration * 1_000.0) as u64);
            assert!(segments[0].samples.len() >= MIN_SPEECH_SAMPLES);
        }
    }

    #[test]
    fn one_frame_click_does_not_start_a_speech_candidate() {
        let mut input = vec![0.0; 16_000];
        input.extend(sine(16_000, 220.0, 0.02));
        input.extend(vec![0.0; 16_000]);
        let mut segmenter = SpeechSegmenter::default();
        assert!(segmenter.push(&input).is_empty());
        assert!(segmenter.finish().is_empty());
    }

    #[test]
    fn finish_keeps_segment_emitted_by_the_last_partial_silence_frame() {
        let mut segmenter = SpeechSegmenter::default();
        assert!(segmenter
            .push(&sine(TARGET_SAMPLE_RATE, 220.0, 1.0))
            .is_empty());
        // The partial final frame completes the 500ms end-of-speech threshold.
        assert!(segmenter
            .push(&vec![
                0.0;
                FRAME_SAMPLES * (SPEECH_END_SILENCE_FRAMES - 1) + 7
            ])
            .is_empty());
        let segments = segmenter.finish();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].end_ms, 1120);
        assert!(segmenter.finish().is_empty());
    }

    #[test]
    fn finish_keeps_segment_emitted_at_the_max_duration_boundary() {
        let mut segmenter = SpeechSegmenter::default();
        let input: Vec<f32> = (0..MAX_SPEECH_SAMPLES - 1)
            .map(|i| {
                (std::f32::consts::TAU * 220.0 * i as f32 / TARGET_SAMPLE_RATE as f32).sin() * 0.2
            })
            .collect();
        assert!(segmenter.push(&input).is_empty());
        let segments = segmenter.finish();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].end_ms, 45_000);
    }

    #[test]
    fn segmenter_preserves_quiet_voice_and_ignores_dc_offset() {
        let mut input = vec![0.0; 16_000];
        input.extend(
            sine(16_000, 220.0, 2.0)
                .into_iter()
                .map(|sample| sample * 0.005),
        );
        input.extend(vec![0.0; 16_000]);
        let mut segmenter = SpeechSegmenter::default();
        let mut segments = segmenter.push(&input);
        segments.extend(segmenter.finish());
        assert_eq!(segments.len(), 1);
        assert!(segments[0].start_ms <= 1000 && segments[0].end_ms >= 3000);
        let mut dc = SpeechSegmenter::default();
        assert!(dc.push(&vec![0.2; 16_000 * 3]).is_empty());
        assert!(dc.finish().is_empty());
    }

    #[test]
    fn chunk_writer_bounds_memory_and_writes_recoverable_wav_files() {
        let root = crate::test_support::TempDir::new("meeting-audio");
        let mut writer =
            ChunkedPcmWriter::with_chunk_samples(&root, AudioSource::Imported, 1_000).unwrap();
        let chunks = writer.push(&vec![0.25; 2_500]).unwrap();
        assert_eq!(chunks.len(), 2);
        let final_chunk = writer.finish().unwrap();
        assert_eq!(final_chunk.len(), 1);
        for chunk in chunks.into_iter().chain(final_chunk) {
            let reader = hound::WavReader::open(&chunk.path).unwrap();
            assert_eq!(reader.spec().sample_rate, TARGET_SAMPLE_RATE);
        }
    }
}
