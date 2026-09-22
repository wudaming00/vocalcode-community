//! Local, language-independent speech presence, not a speaker identifier or
//! denoiser. A positive decision preserves the complete original ASR waveform.
use sha2::{Digest, Sha256};
use sherpa_onnx::{SileroVadModelConfig, VadModelConfig, VoiceActivityDetector};
use std::path::Path;

pub const MODEL_FILE: &str = "silero-v5.onnx";
pub const MODEL_SIZE: u64 = 2_313_101;
pub const MODEL_SHA256: &str = "6b99cbfd39246b6706f98ec13c7c50c6b299181f2474fa05cbc8046acc274396";
pub const MODEL_URL: &str =
    "https://raw.githubusercontent.com/snakers4/silero-vad/v5.0/files/silero_vad.onnx";
const RATE: u32 = 16_000;
const FRAME: usize = 512;
const MAX_SECONDS: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Speech,
    NoSpeech,
    /// Conservative escape hatch, not a positive speech claim.
    Short,
    Unsupported,
}
impl Decision {
    pub fn permits_asr(self) -> bool {
        self != Self::NoSpeech
    }
}

fn bypass(samples: &[f32], rate: u32) -> Option<Decision> {
    if rate != RATE
        || samples.len() > RATE as usize * MAX_SECONDS
        || samples.iter().any(|v| !v.is_finite() || v.abs() > 1.0)
    {
        return Some(Decision::Unsupported);
    }
    // A local beta should not sacrifice one-syllable answers for a more
    // flattering noise score. Brief noise may pass; this is intentional.
    (samples.len() < RATE as usize * 3 / 4).then_some(Decision::Short)
}

pub struct SpeechGate {
    detector: VoiceActivityDetector,
}
impl SpeechGate {
    /// Meeting segmentation emits brief candidates, including real one-syllable
    /// answers. Unlike explicit push-to-talk, those must not bypass acoustic
    /// classification merely because they are short. Pad detector context only;
    /// the original recording and recognizer input are not changed here.
    pub fn classify_meeting(&mut self, samples: &[f32], rate: u32) -> Decision {
        if bypass(samples, rate) == Some(Decision::Short) {
            let mut padded = vec![0.0; RATE as usize];
            padded[..samples.len()].copy_from_slice(samples);
            self.classify(&padded, rate)
        } else {
            self.classify(samples, rate)
        }
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        anyhow::ensure!(
            metadata.is_file() && metadata.len() == MODEL_SIZE,
            "Unexpected speech-gate model file"
        );
        let bytes = std::fs::read(path)?;
        anyhow::ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == MODEL_SHA256,
            "Speech-gate model integrity check failed"
        );
        let model = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Unsupported speech-gate model path"))?;
        let config = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model: Some(model.into()),
                // Bias the local beta toward retaining speech; rejecting a
                // user's words is worse than letting an uncertain sound pass.
                threshold: 0.35,
                min_silence_duration: 0.1,
                min_speech_duration: 0.064,
                window_size: FRAME as i32,
                max_speech_duration: 30.0,
            },
            sample_rate: RATE as i32,
            num_threads: 1,
            provider: Some("cpu".into()),
            ..Default::default()
        };
        let detector = VoiceActivityDetector::create(&config, 32.0)
            .ok_or_else(|| anyhow::anyhow!("Could not load local speech detector"))?;
        Ok(Self { detector })
    }

    pub fn classify(&mut self, samples: &[f32], rate: u32) -> Decision {
        if let Some(decision) = bypass(samples, rate) {
            return decision;
        }
        self.detector.reset();
        let mut decision = self.scan(samples, 1.0);
        // Some microphones (and real multilingual corpus recordings) have very
        // low gain. Retry a rejected quiet waveform with bounded detector-only
        // gain; never normalize the audio passed to the recognizer.
        let peak = samples.iter().fold(0.0_f32, |a, b| a.max(b.abs()));
        if decision == Decision::NoSpeech && peak > 0.0 && peak < 0.1 {
            self.detector.reset();
            decision = self.scan(samples, (0.25 / peak).min(32.0));
        }
        // No recurrent state, device history or queued speech crosses utterances.
        self.detector.reset();
        decision
    }

    fn scan(&self, samples: &[f32], gain: f32) -> Decision {
        for frame in samples.chunks(FRAME) {
            let mut padded = [0.0; FRAME];
            padded[..frame.len()].copy_from_slice(frame);
            if gain != 1.0 {
                padded.iter_mut().for_each(|sample| *sample *= gain);
            }
            self.detector.accept_waveform(&padded);
            if self.detector.detected() || !self.detector.is_empty() {
                return Decision::Speech;
            }
        }
        // Give final speech enough detector context without padding/cropping
        // the waveform that will be passed to ASR.
        for _ in 0..10 {
            self.detector.accept_waveform(&[0.0; FRAME]);
            if self.detector.detected() || !self.detector.is_empty() {
                return Decision::Speech;
            }
        }
        self.detector.flush();
        if self.detector.is_empty() {
            Decision::NoSpeech
        } else {
            Decision::Speech
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn short_and_unsupported_audio_fail_open() {
        assert_eq!(bypass(&[0.0; 11_999], RATE), Some(Decision::Short));
        assert_eq!(bypass(&[0.0; 12_000], RATE), None);
        for samples in [
            vec![f32::NAN; 12_000],
            vec![f32::INFINITY; 12_000],
            vec![1.1; 12_000],
            vec![0.0; RATE as usize * MAX_SECONDS + 1],
        ] {
            assert_eq!(bypass(&samples, RATE), Some(Decision::Unsupported));
        }
        assert_eq!(bypass(&[0.0; 12_000], 48_000), Some(Decision::Unsupported));
        assert_eq!(bypass(&[], 0), Some(Decision::Unsupported));
        assert!(Decision::Short.permits_asr());
        assert!(Decision::Unsupported.permits_asr());
        assert!(Decision::Speech.permits_asr());
        assert!(!Decision::NoSpeech.permits_asr());
    }
    #[test]
    fn missing_model_is_an_error_not_a_silence_decision() {
        assert!(SpeechGate::load(Path::new("nonexistent-vocalcode-vad-model.onnx")).is_err());
    }
}
