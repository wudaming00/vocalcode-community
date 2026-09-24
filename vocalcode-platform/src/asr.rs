//! Offline ASR adapters backed by sherpa-onnx's official Rust API.
//!
//! The product keeps one recognizer loaded at a time. All released models run
//! locally on the CPU; model selection and hardware recommendations live in
//! `vocalcode-app::models`.

use sherpa_onnx::{
    OfflineOmnilingualAsrCtcModelConfig, OfflineParaformerModelConfig, OfflineQwen3ASRModelConfig,
    OfflineRecognizer, OfflineRecognizerConfig, OfflineSenseVoiceModelConfig,
    OfflineTransducerModelConfig, OfflineWhisperModelConfig,
};
use vocalcode_core::error::{Result, VocalCodeError};
use vocalcode_core::traits::Asr;

/// Audio too short for the feature extractor to make a usable frame. Passing
/// this to the native graph can abort the process instead of returning an error.
fn too_short_to_decode(samples: &[f32], sample_rate: u32) -> bool {
    const MIN_MS: usize = 200;
    let need = (sample_rate as usize * MIN_MS / 1000).max(1);
    samples.len() < need
}

fn create_recognizer(config: &OfflineRecognizerConfig, model: &str) -> Result<OfflineRecognizer> {
    OfflineRecognizer::create(config)
        .ok_or_else(|| VocalCodeError::Asr(format!("load {model} model")))
}

fn decode(recognizer: &OfflineRecognizer, samples: &[f32], sample_rate: u32) -> Result<String> {
    if too_short_to_decode(samples, sample_rate) || has_no_signal(samples, sample_rate) {
        return Ok(String::new());
    }
    let stream = recognizer.create_stream();
    stream.accept_waveform(sample_rate as i32, samples);
    recognizer.decode(&stream);
    stream
        .get_result()
        .map(|result| result.text)
        .ok_or_else(|| VocalCodeError::Asr("sherpa-onnx returned no recognition result".into()))
}

/// Conservative no-signal guard, not a speech/music classifier. DC offset is
/// not speech. A low AC floor preserves quiet speech and avoids using the much
/// higher UI level/endpoint threshold to reject entire utterances.
fn has_no_signal(samples: &[f32], sample_rate: u32) -> bool {
    if sample_rate == 0 || samples.iter().any(|sample| !sample.is_finite()) {
        return true;
    }
    let frame_len = (sample_rate as usize / 100).max(1);
    !samples.chunks(frame_len).any(|frame| {
        let mean = frame.iter().map(|&v| v as f64).sum::<f64>() / frame.len() as f64;
        let energy = frame
            .iter()
            .map(|&v| (v as f64 - mean).powi(2))
            .sum::<f64>()
            / frame.len() as f64;
        energy > 1e-10
    })
}

fn base_config(tokens: Option<&str>, num_threads: i32) -> OfflineRecognizerConfig {
    let mut config = OfflineRecognizerConfig::default();
    config.model_config.tokens = tokens.map(str::to_owned);
    config.model_config.num_threads = num_threads.max(1);
    config.model_config.provider = Some("cpu".to_string());
    config.decoding_method = Some("greedy_search".to_string());
    config
}

/// Whisper remains available for diagnostics, but is not in the release model
/// registry because the tested sherpa conversion performed poorly on Hindi.
pub struct SherpaWhisperAsr {
    recognizer: OfflineRecognizer,
    label: String,
}

impl SherpaWhisperAsr {
    pub fn new(encoder: &str, decoder: &str, tokens: &str, language: &str) -> Result<Self> {
        let mut config = base_config(Some(tokens), 4);
        config.model_config.whisper = OfflineWhisperModelConfig {
            encoder: Some(encoder.to_string()),
            decoder: Some(decoder.to_string()),
            language: (!language.is_empty()).then(|| language.to_string()),
            task: Some("transcribe".to_string()),
            ..Default::default()
        };
        Ok(Self {
            recognizer: create_recognizer(&config, "Whisper")?,
            label: "Whisper · multilingual".to_string(),
        })
    }
}

impl Asr for SherpaWhisperAsr {
    fn transcribe(&mut self, samples: &[f32], sample_rate: u32) -> Result<String> {
        decode(&self.recognizer, samples, sample_rate)
    }

    fn model_label(&self) -> &str {
        &self.label
    }
}

/// NVIDIA Parakeet TDT v3, which detects among its documented European
/// languages and produces punctuation and casing itself.
pub struct SherpaParakeetAsr {
    recognizer: OfflineRecognizer,
    label: String,
}

impl SherpaParakeetAsr {
    pub fn new(
        encoder: &str,
        decoder: &str,
        joiner: &str,
        tokens: &str,
        num_threads: i32,
    ) -> Result<Self> {
        Ok(Self {
            recognizer: build_transducer(
                encoder,
                decoder,
                joiner,
                tokens,
                num_threads,
                "nemo_transducer",
            )?,
            label: "Parakeet TDT v3 · EN + European".to_string(),
        })
    }
}

impl Asr for SherpaParakeetAsr {
    fn transcribe(&mut self, samples: &[f32], sample_rate: u32) -> Result<String> {
        decode(&self.recognizer, samples, sample_rate)
    }

    fn model_label(&self) -> &str {
        &self.label
    }
}

/// k2/icefall Zipformer transducers retained for diagnostic compatibility.
pub struct SherpaZipformerAsr {
    recognizer: OfflineRecognizer,
    label: String,
}

impl SherpaZipformerAsr {
    pub fn new(
        encoder: &str,
        decoder: &str,
        joiner: &str,
        tokens: &str,
        num_threads: i32,
        label: &str,
    ) -> Result<Self> {
        Ok(Self {
            recognizer: build_transducer(
                encoder,
                decoder,
                joiner,
                tokens,
                num_threads,
                "transducer",
            )?,
            label: label.to_string(),
        })
    }
}

impl Asr for SherpaZipformerAsr {
    fn transcribe(&mut self, samples: &[f32], sample_rate: u32) -> Result<String> {
        decode(&self.recognizer, samples, sample_rate)
    }

    fn model_label(&self) -> &str {
        &self.label
    }
}

fn build_transducer(
    encoder: &str,
    decoder: &str,
    joiner: &str,
    tokens: &str,
    num_threads: i32,
    model_type: &str,
) -> Result<OfflineRecognizer> {
    let mut config = base_config(Some(tokens), num_threads);
    config.model_config.transducer = OfflineTransducerModelConfig {
        encoder: Some(encoder.to_string()),
        decoder: Some(decoder.to_string()),
        joiner: Some(joiner.to_string()),
    };
    config.model_config.model_type = Some(model_type.to_string());
    create_recognizer(&config, "Parakeet/Zipformer")
}

pub struct SherpaParaformerAsr {
    recognizer: OfflineRecognizer,
    label: String,
}

impl SherpaParaformerAsr {
    pub fn new(model: &str, tokens: &str, num_threads: i32) -> Result<Self> {
        let mut config = base_config(Some(tokens), num_threads);
        config.model_config.paraformer = OfflineParaformerModelConfig {
            model: Some(model.to_string()),
        };
        Ok(Self {
            recognizer: create_recognizer(&config, "Paraformer")?,
            label: "Paraformer · CPU".to_string(),
        })
    }
}

impl Asr for SherpaParaformerAsr {
    fn transcribe(&mut self, samples: &[f32], sample_rate: u32) -> Result<String> {
        decode(&self.recognizer, samples, sample_rate)
    }

    fn model_label(&self) -> &str {
        &self.label
    }
}

/// SenseVoice covers Chinese, Cantonese, English, Japanese and Korean. The
/// language hint is explicit for Chinese and `auto` for Korean/Japanese.
pub struct SherpaSenseVoiceAsr {
    recognizer: OfflineRecognizer,
    label: String,
}

impl SherpaSenseVoiceAsr {
    pub fn new(
        model: &str,
        tokens: &str,
        language: &str,
        num_threads: i32,
        label: &str,
    ) -> Result<Self> {
        let mut config = base_config(Some(tokens), num_threads);
        config.model_config.sense_voice = OfflineSenseVoiceModelConfig {
            model: Some(model.to_string()),
            language: Some(language.to_string()),
            use_itn: true,
        };
        Ok(Self {
            recognizer: create_recognizer(&config, "SenseVoice")?,
            label: label.to_string(),
        })
    }
}

impl Asr for SherpaSenseVoiceAsr {
    fn transcribe(&mut self, samples: &[f32], sample_rate: u32) -> Result<String> {
        decode(&self.recognizer, samples, sample_rate)
    }

    fn model_label(&self) -> &str {
        &self.label
    }
}

/// Meta Omnilingual ASR 300M v2 INT8. The CTC graph is substantially smaller
/// and faster than Qwen3-ASR on CPU while retaining essentially the same Hindi
/// accuracy in VocalCode's fixed 50-utterance FLEURS comparison.
pub struct SherpaOmnilingualAsr {
    recognizer: OfflineRecognizer,
    label: String,
}

impl SherpaOmnilingualAsr {
    pub fn new(model: &str, tokens: &str, num_threads: i32, label: &str) -> Result<Self> {
        let mut config = base_config(Some(tokens), num_threads);
        config.model_config.omnilingual = OfflineOmnilingualAsrCtcModelConfig {
            model: Some(model.to_string()),
        };
        Ok(Self {
            recognizer: create_recognizer(&config, "Omnilingual ASR")?,
            label: label.to_string(),
        })
    }
}

impl Asr for SherpaOmnilingualAsr {
    fn transcribe(&mut self, samples: &[f32], sample_rate: u32) -> Result<String> {
        decode(&self.recognizer, samples, sample_rate)
    }

    fn model_label(&self) -> &str {
        &self.label
    }
}

/// Qwen3-ASR 0.6B INT8: a manually selectable high-context multilingual route.
/// Hindi needs more than sherpa's 128-token default for normal
/// 10–15 second utterances, so 256 is pinned here.
pub struct SherpaQwen3Asr {
    recognizer: OfflineRecognizer,
    label: String,
}

impl SherpaQwen3Asr {
    pub fn new(
        conv_frontend: &str,
        encoder: &str,
        decoder: &str,
        tokenizer: &str,
        num_threads: i32,
        label: &str,
    ) -> Result<Self> {
        let mut config = base_config(None, num_threads);
        config.model_config.qwen3_asr = OfflineQwen3ASRModelConfig {
            conv_frontend: Some(conv_frontend.to_string()),
            encoder: Some(encoder.to_string()),
            decoder: Some(decoder.to_string()),
            tokenizer: Some(tokenizer.to_string()),
            max_total_len: 512,
            max_new_tokens: 256,
            ..Default::default()
        };
        Ok(Self {
            recognizer: create_recognizer(&config, "Qwen3-ASR")?,
            label: label.to_string(),
        })
    }
}

/// Qwen3-ASR answers "language English<asr_text>…". sherpa-onnx removes that
/// header only when it opens the output; when the model first emits a stray
/// token, the header reached the user ("提纲\nlanguage English<asr_text>Is the
/// cache warm?", voice corpus 2026-09-24). Keep what follows the last marker.
fn strip_qwen3_header(text: String) -> String {
    const MARKER: &str = "<asr_text>";
    match text.rfind(MARKER) {
        Some(at) => text[at + MARKER.len()..].trim_start().to_string(),
        None => text,
    }
}

impl Asr for SherpaQwen3Asr {
    fn transcribe(&mut self, samples: &[f32], sample_rate: u32) -> Result<String> {
        decode(&self.recognizer, samples, sample_rate).map(strip_qwen3_header)
    }

    fn model_label(&self) -> &str {
        &self.label
    }
}

#[cfg(test)]
mod short_input_tests {
    use super::*;

    #[test]
    fn qwen3_header_after_a_stray_token_is_removed() {
        assert_eq!(
            strip_qwen3_header(
                "提纲\nlanguage English<asr_text>Is the cache warm? Question mark".into()
            ),
            "Is the cache warm? Question mark"
        );
        assert_eq!(strip_qwen3_header("Plain text".into()), "Plain text");
    }

    #[test]
    fn no_signal_guard_rejects_silence_dc_and_invalid_samples() {
        assert!(has_no_signal(&[0.0; 48_000], 16_000));
        assert!(has_no_signal(&[0.2; 48_000], 16_000));
        assert!(has_no_signal(&[f32::NAN; 4_000], 16_000));
        assert!(has_no_signal(&[f32::INFINITY; 4_000], 16_000));
    }

    #[test]
    fn no_signal_guard_preserves_quiet_signal_and_short_voiced_tail() {
        let mut audio = vec![0.0; 48_000];
        for (i, sample) in audio[47_200..].iter_mut().enumerate() {
            *sample = 0.0001 * (i as f32 * 0.13).sin();
        }
        assert!(!has_no_signal(&audio, 16_000));
    }

    #[test]
    fn too_short_is_measured_in_time_not_samples() {
        assert!(too_short_to_decode(&vec![0.0; 1_000], 16_000), "62 ms");
        assert!(!too_short_to_decode(&vec![0.0; 4_000], 16_000), "250 ms");
        assert!(too_short_to_decode(&vec![0.0; 4_000], 48_000), "83 ms");
        assert!(!too_short_to_decode(&vec![0.0; 12_000], 48_000), "250 ms");
    }

    #[test]
    fn nothing_at_all_is_short() {
        assert!(too_short_to_decode(&[], 16_000));
        assert!(too_short_to_decode(&[0.0], 16_000));
        assert!(!too_short_to_decode(&[0.0; 10], 0), "no rate, no claim");
    }
}
