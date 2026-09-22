//! Real-time acoustic echo cancellation for live two-track meetings.
//!
//! The system-output track is the WebRTC render (far-end) reference.  Only the
//! microphone/capture track is modified, before it reaches storage, VAD, or
//! ASR.  Inputs are normalized to mono 16 kHz here so device callback sizes,
//! channel layouts, and sample rates never leak into the fixed 10 ms AEC API.

use std::collections::VecDeque;

use sonora::config::{EchoCanceller, TransparentModeType};
use sonora::{AudioProcessing, Config, StreamConfig};

use crate::{
    AudioBlock, AudioSource, LinearMonoResampler, MeetingError, Result, TARGET_SAMPLE_RATE,
};

const FRAME_SAMPLES: usize = TARGET_SAMPLE_RATE as usize / 100;
const MAX_REFERENCE_STARTUP_SAMPLES: usize = TARGET_SAMPLE_RATE as usize * 3;
const ADAPTATION_WARMUP_FRAMES: u64 = 200;

/// Samples made ready for the microphone track by one AEC input operation.
#[derive(Debug, Default)]
pub struct EchoCancellationOutput {
    pub microphone_samples: Vec<f32>,
    /// Set once when AEC has to fail open.  Recording remains usable.
    pub degradation: Option<String>,
}

/// Bounded diagnostic facts that contain no recorded audio or transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoCancellationStats {
    pub active: bool,
    pub render_frames: u64,
    pub capture_frames: u64,
    pub estimated_delay_ms: Option<i32>,
    pub degradation: Option<String>,
    /// Frames where double-talk protection limited aggressive suppression.
    pub protected_capture_frames: u64,
}

/// One meeting-scoped WebRTC AEC3 instance.
///
/// Calls are made by the meeting worker, never by a real-time CPAL callback.
/// The object is therefore intentionally single-owner and requires no locks.
pub struct RealtimeEchoCanceller {
    processor: Option<AudioProcessing>,
    render_resampler: LinearMonoResampler,
    capture_resampler: LinearMonoResampler,
    render_pending: VecDeque<f32>,
    capture_pending: VecDeque<f32>,
    render_frames: u64,
    capture_frames: u64,
    degradation: Option<String>,
    degradation_reported: bool,
    double_talk: crate::echo_guard::DoubleTalkGuard,
}

impl std::fmt::Debug for RealtimeEchoCanceller {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RealtimeEchoCanceller")
            .field("active", &self.processor.is_some())
            .field("render_pending", &self.render_pending.len())
            .field("capture_pending", &self.capture_pending.len())
            .field("render_frames", &self.render_frames)
            .field("capture_frames", &self.capture_frames)
            .field("degradation", &self.degradation)
            .finish()
    }
}

impl Default for RealtimeEchoCanceller {
    fn default() -> Self {
        Self::new()
    }
}

impl RealtimeEchoCanceller {
    pub fn new() -> Self {
        let stream = StreamConfig::new(TARGET_SAMPLE_RATE, 1);
        let config = Config {
            echo_canceller: Some(EchoCanceller {
                transparent_mode: TransparentModeType::Hmm,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut processor = AudioProcessing::builder()
            .config(config)
            .capture_config(stream)
            .render_config(stream)
            .echo_detector(true)
            .build();
        // AEC3 still estimates the acoustic-path delay internally.  This is
        // the known HAL scheduling delay at the point where both streams have
        // already reached the same meeting worker.
        let _ = processor.set_stream_delay_ms(0);
        Self {
            processor: Some(processor),
            render_resampler: LinearMonoResampler::default(),
            capture_resampler: LinearMonoResampler::default(),
            render_pending: VecDeque::new(),
            capture_pending: VecDeque::new(),
            render_frames: 0,
            capture_frames: 0,
            degradation: None,
            degradation_reported: false,
            double_talk: crate::echo_guard::DoubleTalkGuard::default(),
        }
    }

    /// Push one raw device block.  System blocks may release microphone audio
    /// buffered briefly while waiting for the first render reference.
    pub fn push(
        &mut self,
        source: AudioSource,
        block: &AudioBlock,
    ) -> Result<EchoCancellationOutput> {
        if !matches!(source, AudioSource::Microphone | AudioSource::System) {
            return Err(MeetingError::Invalid(
                "echo cancellation accepts only live microphone or system audio".to_string(),
            ));
        }
        if self.processor.is_none() {
            return self.push_after_degradation(source, block);
        }

        let mut microphone_samples = Vec::new();
        match source {
            AudioSource::System => {
                let samples = self.render_resampler.push(block)?;
                self.render_pending.extend(samples);
                self.process_render_frames(&mut microphone_samples);
            }
            AudioSource::Microphone => {
                let samples = self.capture_resampler.push(block)?;
                self.capture_pending.extend(samples);
            }
            AudioSource::Imported => unreachable!("source was checked above"),
        }

        if self.render_frames > 0 {
            self.process_capture_frames(&mut microphone_samples);
        }
        if self.capture_pending.len() > MAX_REFERENCE_STARTUP_SAMPLES {
            let reason = if self.render_frames == 0 {
                "system audio produced no usable AEC reference within three seconds"
            } else {
                "system audio reference fell more than three seconds behind the microphone"
            };
            self.degrade(reason, &mut microphone_samples);
        }
        self.append_pending_after_degradation(&mut microphone_samples);
        Ok(self.output(microphone_samples))
    }

    /// Flush resampler and partial-frame tails without dropping microphone
    /// samples.  A missing reference deliberately returns the original signal.
    pub fn finish(&mut self) -> Result<EchoCancellationOutput> {
        if self.processor.is_none() {
            let mut microphone_samples: Vec<_> = self.capture_pending.drain(..).collect();
            microphone_samples.extend(self.capture_resampler.finish());
            return Ok(self.output(microphone_samples));
        }

        let mut microphone_samples = Vec::new();
        self.render_pending.extend(self.render_resampler.finish());
        self.process_render_frames(&mut microphone_samples);
        if self.processor.is_some() && !self.render_pending.is_empty() {
            let actual = self.render_pending.len();
            self.render_pending.resize(FRAME_SAMPLES, 0.0);
            self.process_render_frames(&mut microphone_samples);
            debug_assert!(actual < FRAME_SAMPLES);
        }

        self.capture_pending.extend(self.capture_resampler.finish());
        if self.render_frames == 0 {
            self.degrade(
                "system audio ended without producing an AEC reference",
                &mut microphone_samples,
            );
        } else if !self.capture_pending.is_empty() && self.processor.is_some() {
            let actual = self.capture_pending.len();
            let before = microphone_samples.len();
            let padded = actual.next_multiple_of(FRAME_SAMPLES);
            self.capture_pending.resize(padded, 0.0);

            // The streams are stopped together, but one final device callback
            // can be ahead of the other. Feed silence for only that bounded
            // tail so every real microphone sample is returned.
            let needed_capture_frames = padded / FRAME_SAMPLES;
            while self.processor.is_some()
                && self.render_frames
                    < self
                        .capture_frames
                        .saturating_add(needed_capture_frames as u64)
            {
                self.render_pending
                    .extend(std::iter::repeat_n(0.0, FRAME_SAMPLES));
                self.process_render_frames(&mut microphone_samples);
            }
            self.process_capture_frames(&mut microphone_samples);
            microphone_samples.truncate(before + actual);
        }
        self.append_pending_after_degradation(&mut microphone_samples);
        Ok(self.output(microphone_samples))
    }

    pub fn stats(&self) -> EchoCancellationStats {
        let estimated_delay_ms = self
            .processor
            .as_ref()
            .and_then(|processor| processor.statistics().delay_ms);
        EchoCancellationStats {
            active: self.processor.is_some(),
            render_frames: self.render_frames,
            capture_frames: self.capture_frames,
            estimated_delay_ms,
            degradation: self.degradation.clone(),
            protected_capture_frames: self.double_talk.protected_frames(),
        }
    }

    fn process_render_frames(&mut self, microphone_samples: &mut Vec<f32>) {
        while self.processor.is_some() && self.render_pending.len() >= FRAME_SAMPLES {
            let frame: [f32; FRAME_SAMPLES] = std::array::from_fn(|_| {
                self.render_pending
                    .pop_front()
                    .expect("a complete render frame was checked")
            });
            let mut ignored = [0.0; FRAME_SAMPLES];
            let result = self
                .processor
                .as_mut()
                .expect("processor was checked")
                .process_render_f32(&[frame.as_slice()], &mut [&mut ignored]);
            if let Err(problem) = result {
                self.degrade(
                    &format!("WebRTC AEC render processing failed: {problem}"),
                    microphone_samples,
                );
                break;
            }
            self.render_frames = self.render_frames.saturating_add(1);
            self.double_talk.render(&frame);
        }
    }

    fn process_capture_frames(&mut self, output: &mut Vec<f32>) {
        while self.processor.is_some()
            && self.capture_pending.len() >= FRAME_SAMPLES
            && self.capture_frames < self.render_frames
        {
            let frame: [f32; FRAME_SAMPLES] = std::array::from_fn(|_| {
                self.capture_pending
                    .pop_front()
                    .expect("a complete capture frame was checked")
            });
            let mut cleaned = [0.0; FRAME_SAMPLES];
            let result = self
                .processor
                .as_mut()
                .expect("processor was checked")
                .process_capture_f32(&[frame.as_slice()], &mut [&mut cleaned]);
            if let Err(problem) = result {
                // Put the current frame back in front before failing open.
                for sample in frame.into_iter().rev() {
                    self.capture_pending.push_front(sample);
                }
                self.degrade(
                    &format!("WebRTC AEC capture processing failed: {problem}"),
                    output,
                );
                break;
            }
            if cleaned.iter().any(|sample| !sample.is_finite()) {
                for sample in frame.into_iter().rev() {
                    self.capture_pending.push_front(sample);
                }
                self.degrade("WebRTC AEC returned non-finite microphone audio", output);
                break;
            }
            self.double_talk.capture(&frame, &mut cleaned);
            // AEC3 needs a short acoustic-path adaptation period.  Feeding it
            // both tracks immediately is still important, but publishing its
            // early output can erase genuine near-end words when both people
            // start speaking together.  Keep the raw microphone mixture for
            // the first two seconds; completion-time cross-track transcript
            // deduplication removes any far-end words that leak through.
            if self.capture_frames < ADAPTATION_WARMUP_FRAMES {
                output.extend(frame.into_iter().map(|sample| sample.clamp(-1.0, 1.0)));
            } else {
                output.extend(cleaned.into_iter().map(|sample| sample.clamp(-1.0, 1.0)));
            }
            self.capture_frames = self.capture_frames.saturating_add(1);
        }
    }

    fn push_after_degradation(
        &mut self,
        source: AudioSource,
        block: &AudioBlock,
    ) -> Result<EchoCancellationOutput> {
        let microphone_samples = if source == AudioSource::Microphone {
            self.capture_resampler.push(block)?
        } else {
            Vec::new()
        };
        Ok(self.output(microphone_samples))
    }

    fn degrade(&mut self, reason: &str, microphone_samples: &mut Vec<f32>) {
        if self.degradation.is_none() {
            self.degradation = Some(reason.to_string());
        }
        self.processor = None;
        microphone_samples.extend(self.capture_pending.drain(..));
        self.render_pending.clear();
    }

    fn append_pending_after_degradation(&mut self, output: &mut Vec<f32>) {
        if self.processor.is_none() {
            output.extend(self.capture_pending.drain(..));
        }
    }

    fn output(&mut self, microphone_samples: Vec<f32>) -> EchoCancellationOutput {
        let degradation = if self.degradation_reported {
            None
        } else {
            let value = self.degradation.clone();
            self.degradation_reported = value.is_some();
            value
        };
        EchoCancellationOutput {
            microphone_samples,
            degradation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(samples: Vec<f32>, start_ms: u64) -> AudioBlock {
        AudioBlock {
            samples,
            sample_rate: TARGET_SAMPLE_RATE,
            channels: 1,
            start_ms,
        }
    }

    fn deterministic_render(samples: usize) -> Vec<f32> {
        let mut state = 0x1234_5678u32;
        (0..samples)
            .map(|index| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let noise = (state as f32 / u32::MAX as f32) * 2.0 - 1.0;
                let voiced = (std::f32::consts::TAU * 173.0 * index as f32
                    / TARGET_SAMPLE_RATE as f32)
                    .sin();
                (noise * 0.12 + voiced * 0.18).clamp(-1.0, 1.0)
            })
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt()
    }

    fn mse(left: &[f32], right: &[f32]) -> f32 {
        assert_eq!(left.len(), right.len());
        left.iter()
            .zip(right)
            .map(|(left, right)| (left - right).powi(2))
            .sum::<f32>()
            / left.len() as f32
    }

    #[test]
    fn capture_waits_for_the_first_reference_then_streams_in_real_time() {
        let mut aec = RealtimeEchoCanceller::new();
        let microphone = vec![0.2; FRAME_SAMPLES * 2];
        let waiting = aec
            .push(AudioSource::Microphone, &block(microphone.clone(), 0))
            .unwrap();
        assert!(waiting.microphone_samples.is_empty());

        let released = aec
            .push(AudioSource::System, &block(vec![0.0; FRAME_SAMPLES], 0))
            .unwrap();
        assert_eq!(released.microphone_samples.len(), FRAME_SAMPLES);
        let released_next = aec
            .push(AudioSource::System, &block(vec![0.0; FRAME_SAMPLES], 10))
            .unwrap();
        assert_eq!(released_next.microphone_samples.len(), FRAME_SAMPLES);
        assert!(released.degradation.is_none());
        assert_eq!(aec.stats().capture_frames, 2);
    }

    #[test]
    fn missing_reference_fails_open_without_losing_microphone_samples() {
        let mut aec = RealtimeEchoCanceller::new();
        let microphone = deterministic_render(FRAME_SAMPLES * 7 + 53);
        let output = aec
            .push(AudioSource::Microphone, &block(microphone.clone(), 0))
            .unwrap();
        assert!(output.microphone_samples.is_empty());
        let finished = aec.finish().unwrap();
        assert_eq!(finished.microphone_samples.len(), microphone.len());
        assert!(finished
            .microphone_samples
            .iter()
            .zip(&microphone)
            .all(|(actual, expected)| (actual - expected).abs() < 1e-6));
        assert!(finished.degradation.is_some());
        assert!(!aec.stats().active);
    }

    #[test]
    fn reference_startup_wait_is_bounded_and_fails_open() {
        let mut aec = RealtimeEchoCanceller::new();
        let microphone = vec![0.125; MAX_REFERENCE_STARTUP_SAMPLES + FRAME_SAMPLES];
        let output = aec
            .push(AudioSource::Microphone, &block(microphone, 0))
            .unwrap();
        assert_eq!(
            output.microphone_samples.len(),
            MAX_REFERENCE_STARTUP_SAMPLES + FRAME_SAMPLES
        );
        assert!(output.degradation.is_some());
        assert!(!aec.stats().active);
    }

    #[test]
    fn reference_lag_after_startup_is_bounded_and_fails_open() {
        let mut aec = RealtimeEchoCanceller::new();
        aec.push(AudioSource::System, &block(vec![0.0; FRAME_SAMPLES], 0))
            .unwrap();
        let microphone = vec![0.125; MAX_REFERENCE_STARTUP_SAMPLES + FRAME_SAMPLES * 2];
        let output = aec
            .push(AudioSource::Microphone, &block(microphone, 0))
            .unwrap();
        assert_eq!(
            output.microphone_samples.len(),
            MAX_REFERENCE_STARTUP_SAMPLES + FRAME_SAMPLES * 2
        );
        assert!(output
            .degradation
            .as_deref()
            .is_some_and(|reason| reason.contains("fell more than three seconds behind")));
        assert!(!aec.stats().active);
    }

    #[test]
    fn finish_preserves_an_active_partial_microphone_tail() {
        let mut aec = RealtimeEchoCanceller::new();
        aec.push(AudioSource::System, &block(vec![0.0; FRAME_SAMPLES], 0))
            .unwrap();
        let live = aec
            .push(
                AudioSource::Microphone,
                &block(vec![0.2; FRAME_SAMPLES + 53], 0),
            )
            .unwrap();
        assert_eq!(live.microphone_samples.len(), FRAME_SAMPLES);

        let tail = aec.finish().unwrap();
        assert_eq!(tail.microphone_samples.len(), 53);
        assert!(tail
            .microphone_samples
            .iter()
            .all(|sample| sample.is_finite()));
        assert!(tail.degradation.is_none());
    }

    #[test]
    fn aec_reduces_a_delayed_far_end_echo() {
        let frames = 600usize;
        let delay = FRAME_SAMPLES * 6;
        let render = deterministic_render(frames * FRAME_SAMPLES);
        let mut capture = vec![0.0; render.len()];
        for index in delay..capture.len() {
            capture[index] = render[index - delay] * 0.55;
        }

        let mut aec = RealtimeEchoCanceller::new();
        let mut cleaned = Vec::with_capacity(capture.len());
        for frame in 0..frames {
            let start = frame * FRAME_SAMPLES;
            let end = start + FRAME_SAMPLES;
            aec.push(
                AudioSource::System,
                &block(render[start..end].to_vec(), frame as u64 * 10),
            )
            .unwrap();
            cleaned.extend(
                aec.push(
                    AudioSource::Microphone,
                    &block(capture[start..end].to_vec(), frame as u64 * 10),
                )
                .unwrap()
                .microphone_samples,
            );
        }
        cleaned.extend(aec.finish().unwrap().microphone_samples);
        assert_eq!(cleaned.len(), capture.len());
        let settle = TARGET_SAMPLE_RATE as usize * 2;
        let before = rms(&capture[settle..]);
        let after = rms(&cleaned[settle..]);
        assert!(
            after < before * 0.45,
            "expected substantial echo reduction, before={before:.6}, after={after:.6}"
        );
    }

    #[test]
    fn adaptation_warmup_preserves_the_initial_microphone_mixture() {
        let frames = ADAPTATION_WARMUP_FRAMES as usize;
        let render = deterministic_render(frames * FRAME_SAMPLES);
        let microphone = (0..render.len())
            .map(|index| render[index] * 0.35 + (index as f32 * 0.017).sin() * 0.08)
            .collect::<Vec<_>>();
        let mut aec = RealtimeEchoCanceller::new();
        let mut expected_resampler = LinearMonoResampler::default();
        let mut expected = Vec::with_capacity(microphone.len());
        let mut output = Vec::with_capacity(microphone.len());
        for frame in 0..frames {
            let start = frame * FRAME_SAMPLES;
            let end = start + FRAME_SAMPLES;
            aec.push(
                AudioSource::System,
                &block(render[start..end].to_vec(), frame as u64 * 10),
            )
            .unwrap();
            let microphone_block = block(microphone[start..end].to_vec(), frame as u64 * 10);
            expected.extend(expected_resampler.push(&microphone_block).unwrap());
            output.extend(
                aec.push(AudioSource::Microphone, &microphone_block)
                    .unwrap()
                    .microphone_samples,
            );
        }
        assert_eq!(output, expected);
    }

    #[test]
    fn aec_preserves_near_end_during_double_talk() {
        let frames = 600usize;
        let delay = FRAME_SAMPLES * 5;
        let render = deterministic_render(frames * FRAME_SAMPLES);
        let near = (0..render.len())
            .map(|index| {
                let time = index as f32 / TARGET_SAMPLE_RATE as f32;
                let syllable = if (index / (TARGET_SAMPLE_RATE as usize / 5)) % 4 == 3 {
                    0.08
                } else {
                    1.0
                };
                let fundamental = (std::f32::consts::TAU * (137.0 + time * 2.1) * time).sin();
                let harmonics = (std::f32::consts::TAU * 274.0 * time).sin() * 0.42
                    + (std::f32::consts::TAU * 411.0 * time).sin() * 0.24;
                (fundamental + harmonics) * syllable * 0.11
            })
            .collect::<Vec<_>>();
        let mut capture = near.clone();
        for index in delay..capture.len() {
            capture[index] += render[index - delay] * 0.45;
        }

        let mut aec = RealtimeEchoCanceller::new();
        let mut cleaned = Vec::with_capacity(capture.len());
        for frame in 0..frames {
            let start = frame * FRAME_SAMPLES;
            let end = start + FRAME_SAMPLES;
            aec.push(
                AudioSource::System,
                &block(render[start..end].to_vec(), frame as u64 * 10),
            )
            .unwrap();
            cleaned.extend(
                aec.push(
                    AudioSource::Microphone,
                    &block(capture[start..end].to_vec(), frame as u64 * 10),
                )
                .unwrap()
                .microphone_samples,
            );
        }
        cleaned.extend(aec.finish().unwrap().microphone_samples);
        let settle = TARGET_SAMPLE_RATE as usize * 2;
        let cleaned_error = mse(&cleaned[settle..], &near[settle..]);
        let raw_error = mse(&capture[settle..], &near[settle..]);
        let cleaned_rms = rms(&cleaned[settle..]);
        let near_rms = rms(&near[settle..]);
        assert!(
            cleaned_error < raw_error * 1.05,
            "double-talk processing made the mixture worse: raw_error={raw_error:.6}, cleaned_error={cleaned_error:.6}, near_rms={near_rms:.6}, cleaned_rms={cleaned_rms:.6}"
        );
        assert!(cleaned_rms > near_rms * 0.45);
    }

    #[test]
    fn unrelated_headphone_render_does_not_erase_microphone_speech() {
        let frames = 500usize;
        let render = deterministic_render(frames * FRAME_SAMPLES);
        let microphone = (0..render.len())
            .map(|index| {
                let time = index as f32 / TARGET_SAMPLE_RATE as f32;
                ((std::f32::consts::TAU * 223.0 * time).sin()
                    + (std::f32::consts::TAU * 449.0 * time).sin() * 0.35)
                    * 0.12
            })
            .collect::<Vec<_>>();
        let mut aec = RealtimeEchoCanceller::new();
        let mut cleaned = Vec::new();
        for frame in 0..frames {
            let start = frame * FRAME_SAMPLES;
            let end = start + FRAME_SAMPLES;
            aec.push(
                AudioSource::System,
                &block(render[start..end].to_vec(), frame as u64 * 10),
            )
            .unwrap();
            cleaned.extend(
                aec.push(
                    AudioSource::Microphone,
                    &block(microphone[start..end].to_vec(), frame as u64 * 10),
                )
                .unwrap()
                .microphone_samples,
            );
        }
        cleaned.extend(aec.finish().unwrap().microphone_samples);
        let settle = TARGET_SAMPLE_RATE as usize * 2;
        assert!(rms(&cleaned[settle..]) > rms(&microphone[settle..]) * 0.55);
    }

    #[test]
    fn microphone_and_system_resamplers_accept_different_device_formats() {
        let mut aec = RealtimeEchoCanceller::new();
        let stereo_render = vec![0.1; 480 * 2];
        let mono_capture = vec![0.2; 441];
        let system = AudioBlock {
            samples: stereo_render,
            sample_rate: 48_000,
            channels: 2,
            start_ms: 0,
        };
        let microphone = AudioBlock {
            samples: mono_capture,
            sample_rate: 44_100,
            channels: 1,
            start_ms: 0,
        };
        aec.push(AudioSource::System, &system).unwrap();
        let mut output = aec.push(AudioSource::Microphone, &microphone).unwrap();
        // Downsampling has a small centered low-pass lookahead, flushed at EOF.
        output
            .microphone_samples
            .extend(aec.finish().unwrap().microphone_samples);
        assert_eq!(output.microphone_samples.len(), FRAME_SAMPLES);
        assert!(output
            .microphone_samples
            .iter()
            .all(|sample| sample.is_finite()));
    }
}
