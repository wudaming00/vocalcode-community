//! Offline ASR adapters backed by sherpa-onnx's official Rust API.
//!
//! The product keeps one recognizer loaded at a time. All released models run
//! locally on the CPU; model selection and hardware recommendations live in
//! `vocalcode-app::models`.

use std::ops::Range;

use sherpa_onnx::{
    OfflineOmnilingualAsrCtcModelConfig, OfflineParaformerModelConfig, OfflineQwen3ASRModelConfig,
    OfflineRecognizer, OfflineRecognizerConfig, OfflineRecognizerResult,
    OfflineSenseVoiceModelConfig, OfflineTransducerModelConfig, OfflineWhisperModelConfig,
};
use vocalcode_core::error::{Result, VocalCodeError};
use vocalcode_core::segmentation::join_separator;
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
    Ok(recognize(recognizer, samples, sample_rate)?.map_or_else(String::new, |result| result.text))
}

/// `None` for input the native graph must not be given (see `decode`).
fn recognize(
    recognizer: &OfflineRecognizer,
    samples: &[f32],
    sample_rate: u32,
) -> Result<Option<OfflineRecognizerResult>> {
    if too_short_to_decode(samples, sample_rate) || has_no_signal(samples, sample_rate) {
        return Ok(None);
    }
    let stream = recognizer.create_stream();
    stream.accept_waveform(sample_rate as i32, samples);
    recognizer.decode(&stream);
    stream
        .get_result()
        .map(Some)
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
///
/// The exported decoder holds 512 tokens in all: a 15-token prompt, 13 audio
/// tokens a second, and the answer. One long decode first loses the end of the
/// answer (English from ~20 s), then the audio itself: 60 s kept 497 of 780
/// audio tokens and answered only "language". Dictation never cuts continuous
/// speech for time and a meeting segment reaches 45 s, so long input is decoded
/// in pieces the answer fits after (`plan_chunks`), and every piece is checked
/// for this model's known garbage (`suspect`) before it is accepted.
pub struct SherpaQwen3Asr {
    recognizer: OfflineRecognizer,
    label: String,
    /// Longest piece decoded at once.
    chunk_ms: u32,
    /// English route: Chinese, Japanese or Korean script is a wrong decode.
    english: bool,
}

impl SherpaQwen3Asr {
    /// `language` is the route's language code ("en", "zh", "hi", …).
    pub fn new(
        conv_frontend: &str,
        encoder: &str,
        decoder: &str,
        tokenizer: &str,
        language: &str,
        num_threads: i32,
        label: &str,
    ) -> Result<Self> {
        let mut config = base_config(None, num_threads);
        config.model_config.qwen3_asr = OfflineQwen3ASRModelConfig {
            conv_frontend: Some(conv_frontend.to_string()),
            encoder: Some(encoder.to_string()),
            decoder: Some(decoder.to_string()),
            tokenizer: Some(tokenizer.to_string()),
            max_total_len: QWEN3_CONTEXT_TOKENS as i32,
            max_new_tokens: QWEN3_ANSWER_TOKENS as i32,
            ..Default::default()
        };
        Ok(Self {
            recognizer: create_recognizer(&config, "Qwen3-ASR")?,
            label: label.to_string(),
            chunk_ms: qwen3_chunk_ms(language),
            english: language == "en",
        })
    }
}

/// One decode of one piece: the answer without its header, and how many
/// answer tokens it took. `None` when the piece was not decoded at all.
type Qwen3Decode<'a> = dyn FnMut(&[f32]) -> Result<Option<(String, usize)>> + 'a;

/// Decode input of any length in pieces of at most `chunk_ms`, each checked.
fn transcribe_in_pieces(
    decode: &mut Qwen3Decode,
    samples: &[f32],
    rate: u32,
    chunk_ms: u32,
    english: bool,
) -> Result<String> {
    let mut text = String::new();
    for chunk in plan_chunks(samples, rate, chunk_ms) {
        let piece = decode_checked(decode, &samples[chunk], rate, english, QWEN3_RETRIES)?;
        append_piece(&mut text, &piece);
    }
    Ok(text)
}

/// Decode one piece. A suspect answer is decoded again as two halves split at
/// a pause (then quarters); the halves replace it unless they recover fewer
/// words than a merely short or cut-off answer already had.
///
/// sherpa's per-stream language hint also cures "提纲", but it turns noise
/// into "The system." (all 24 noise and hum probes, 2026-09-24), while the
/// halves decode that noise to nothing — so no hint is used.
fn decode_checked(
    decode: &mut Qwen3Decode,
    samples: &[f32],
    rate: u32,
    english: bool,
    retries: u32,
) -> Result<String> {
    let Some((text, tokens)) = decode(samples)? else {
        return Ok(String::new());
    };
    let Some(problem) = suspect(&text, tokens, samples, rate, english) else {
        return Ok(text);
    };
    let ms = samples.len() as u64 * 1000 / rate.max(1) as u64;
    let kept = problem.salvage(text);
    let cut = if retries > 0 {
        split_near_middle(samples, rate)
    } else {
        None
    };
    let Some(cut) = cut else {
        log::warn!("Qwen3-ASR: {problem:?} answer for {ms} ms of audio could not be retried");
        return Ok(kept);
    };
    log::info!("Qwen3-ASR: {problem:?} answer for {ms} ms of audio; decoding it in halves");
    let mut halves = decode_checked(decode, &samples[..cut], rate, english, retries - 1)?;
    let second = decode_checked(decode, &samples[cut..], rate, english, retries - 1)?;
    append_piece(&mut halves, &second);
    Ok(if text_units(&halves) >= text_units(&kept) {
        halves
    } else {
        kept
    })
}

/// The exported decoder's fixed context: prompt, audio and answer together.
const QWEN3_CONTEXT_TOKENS: usize = 512;
const QWEN3_ANSWER_TOKENS: usize = 256;
/// sherpa's prompt scaffold around the audio placeholders; its truncation log
/// for a 60 s input reads "keep_audio=497 (before=9 after=6)".
const QWEN3_PROMPT_TOKENS: usize = 15;
/// "language English<asr_text>": generated, then dropped from `tokens`.
const QWEN3_HEADER_TOKENS: usize = 3;
/// A 20 s piece takes 260 audio tokens and leaves 238 for the answer, header
/// included. English and Chinese answers cost ~3 tokens a second of speech,
/// Hindi ~12 (4.8 per word), which nearly fills that: FLEURS Hindi joined to
/// 41-96 s scored 26.1% WER in 15 s pieces against 28.5% in 20 s ones.
const QWEN3_CHUNK_MS: u32 = 20_000;
const QWEN3_DENSE_CHUNK_MS: u32 = 15_000;
/// How far either side of an even split to look for a pause.
const QWEN3_CUT_SEARCH_MS: u32 = 3_000;
/// Halves, then quarters; no piece of a retry is shorter than this.
const QWEN3_RETRIES: u32 = 2;
const QWEN3_MIN_PIECE_MS: u32 = 750;
/// An empty answer is suspect once the audio holds this much speech.
const QWEN3_EMPTY_SPEECH_S: f64 = 1.0;
/// Below half a word a second of speech (over at least 4 s of it) is suspect.
/// The voice corpus's slowest clips carry 1.6 (English) and 3.9 (Chinese).
const QWEN3_SPARSE_SPEECH_S: f64 = 4.0;
const QWEN3_MIN_UNITS_PER_SPEECH_S: f64 = 0.5;

fn qwen3_chunk_ms(language: &str) -> u32 {
    if matches!(language, "en" | "zh") {
        QWEN3_CHUNK_MS
    } else {
        QWEN3_DENSE_CHUNK_MS
    }
}

/// Why an answer is not accepted as it stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Suspect {
    /// Only the "language English" header, or its marker, came back.
    Header,
    /// The answer used all the room the decoder had left, so it was cut off.
    Truncated,
    /// Chinese script on the English route ("提纲" for "Sounds good.").
    WrongScript,
    /// Nothing, for audio that holds speech.
    Empty,
    /// Far fewer words than the speech in the audio could carry.
    Sparse,
}

impl Suspect {
    /// What is worth keeping when no retry does better.
    fn salvage(self, text: String) -> String {
        match self {
            Self::Header | Self::WrongScript | Self::Empty => String::new(),
            Self::Truncated | Self::Sparse => text,
        }
    }
}

fn suspect(
    text: &str,
    tokens: usize,
    samples: &[f32],
    rate: u32,
    english: bool,
) -> Option<Suspect> {
    if is_bare_header(text) {
        return Some(Suspect::Header);
    }
    if tokens + QWEN3_HEADER_TOKENS + 1 >= answer_room(samples.len(), rate) {
        return Some(Suspect::Truncated);
    }
    if english && text.chars().any(is_cjk) {
        return Some(Suspect::WrongScript);
    }
    let units = text_units(text);
    let seconds = samples.len() as f64 / rate.max(1) as f64;
    if units > 0 && units as f64 >= seconds * QWEN3_MIN_UNITS_PER_SPEECH_S {
        return None; // enough words even if every frame were speech
    }
    let speech = speech_seconds(samples, rate);
    if units == 0 {
        (speech >= QWEN3_EMPTY_SPEECH_S).then_some(Suspect::Empty)
    } else {
        (speech >= QWEN3_SPARSE_SPEECH_S && (units as f64) < speech * QWEN3_MIN_UNITS_PER_SPEECH_S)
            .then_some(Suspect::Sparse)
    }
}

/// "language" or "language English": the answer's header and no answer. The
/// marker itself cannot survive `strip_qwen3_header`, but is garbage if it does.
fn is_bare_header(text: &str) -> bool {
    let text = text.trim();
    if text.contains("<asr_text>") {
        return true;
    }
    match text.strip_prefix("language") {
        Some("") => true,
        Some(rest) => {
            let name = rest.trim_start();
            rest.starts_with(char::is_whitespace)
                && !name.is_empty()
                && name.chars().all(char::is_alphabetic)
        }
        None => false,
    }
}

/// Answer tokens the decoder has room for after the prompt and this audio.
fn answer_room(samples: usize, rate: u32) -> usize {
    let rate = rate.max(1) as usize;
    let frames = (samples * 100 + rate / 2) / rate;
    // The front end turns each 100 mel frames into 13 audio tokens, and the
    // remainder into one token per 8 frames.
    let audio = 13 * (frames / 100) + (frames % 100).div_ceil(8);
    // sherpa samples the first answer token before it checks the length.
    (QWEN3_CONTEXT_TOKENS + 1)
        .saturating_sub(QWEN3_PROMPT_TOKENS + audio)
        .min(QWEN3_ANSWER_TOKENS)
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11ff | 0x3040..=0x30ff | 0x3130..=0x318f | 0x3400..=0x9fff
        | 0xac00..=0xd7af | 0xf900..=0xfaff | 0x20000..=0x2fa1f)
}

/// Words, counting each Chinese, Japanese or Korean character as one.
fn text_units(text: &str) -> usize {
    text.split_whitespace()
        .map(|word| {
            word.chars().filter(|&c| is_cjk(c)).count()
                + usize::from(word.chars().any(|c| c.is_alphanumeric() && !is_cjk(c)))
        })
        .sum()
}

/// RMS of each whole 10 ms frame, around the frame's own mean.
fn frame_rms(samples: &[f32], frame: usize) -> Vec<f32> {
    samples
        .chunks_exact(frame.max(1))
        .map(|values| {
            let mean = values.iter().sum::<f32>() / values.len() as f32;
            (values.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / values.len() as f32).sqrt()
        })
        .collect()
}

/// Seconds of 10 ms frames well above this audio's own quiet floor. Speech
/// rises and falls with its syllables; steady noise and hum stay level.
fn speech_seconds(samples: &[f32], rate: u32) -> f64 {
    if rate < 100 {
        return 0.0;
    }
    let rms = frame_rms(samples, rate as usize / 100);
    if rms.is_empty() {
        return 0.0;
    }
    let mut sorted = rms.clone();
    sorted.sort_by(f32::total_cmp);
    let loud = (sorted[sorted.len() / 10] * 3.0).max(0.002);
    rms.iter().filter(|&&rms| rms > loud).count() as f64 / 100.0
}

/// Contiguous pieces of at most `max_ms`. Each cut goes in the quietest part
/// within a few seconds of an even split, so no piece is left as a sliver.
fn plan_chunks(samples: &[f32], rate: u32, max_ms: u32) -> Vec<Range<usize>> {
    let max = (rate as usize * max_ms as usize / 1000).max(1);
    if rate < 100 || samples.len() <= max {
        return std::iter::once(0..samples.len()).collect();
    }
    let frame = rate as usize / 100;
    let rms = frame_rms(samples, frame);
    let search = rate as usize * QWEN3_CUT_SEARCH_MS as usize / 1000;
    let mut chunks = Vec::new();
    let mut start = 0;
    while samples.len() - start > max {
        let remaining = samples.len() - start;
        let pieces = remaining.div_ceil(max);
        let even = start + remaining / pieces;
        // Not so late that this piece is too long, nor so early that the rest
        // needs one more piece than planned.
        let lo = even
            .saturating_sub(search)
            .max(samples.len() - (pieces - 1) * max);
        let hi = (even + search).min(start + max);
        let cut = quiet_cut(&rms, frame, lo..hi, even);
        chunks.push(start..cut);
        start = cut;
    }
    chunks.push(start..samples.len());
    chunks
}

/// Where to split a suspect piece in two: at a pause near the middle.
fn split_near_middle(samples: &[f32], rate: u32) -> Option<usize> {
    let min = rate as usize * QWEN3_MIN_PIECE_MS as usize / 1000;
    if rate < 100 || samples.len() < 2 * min {
        return None;
    }
    let middle = samples.len() / 2;
    let search = (rate as usize * QWEN3_CUT_SEARCH_MS as usize / 1000).min(samples.len() / 4);
    let lo = middle.saturating_sub(search).max(min);
    let hi = (middle + search).min(samples.len() - min);
    let frame = rate as usize / 100;
    Some(quiet_cut(&frame_rms(samples, frame), frame, lo..hi, middle))
}

/// A cut within `range` (samples): the middle of the longest run of quiet
/// 10 ms frames — a real pause when there is one, otherwise the quietest
/// frame. Quiet is within twice the range's own floor; ties go nearest `even`.
/// A frame is as loud as its louder neighbour, so one dropped-out frame inside
/// a word does not outrank a pause with a little room noise in it.
fn quiet_cut(rms: &[f32], frame: usize, range: Range<usize>, even: usize) -> usize {
    let first = range.start.div_ceil(frame);
    let end = (range.end / frame).min(rms.len());
    if first >= end {
        return even.clamp(range.start, range.end);
    }
    let window: Vec<f32> = (first..end)
        .map(|i| {
            rms[i.saturating_sub(1)..(i + 2).min(rms.len())]
                .iter()
                .copied()
                .fold(0.0, f32::max)
        })
        .collect();
    let floor = window.iter().copied().fold(f32::INFINITY, f32::min);
    let quiet = floor * 2.0 + 1e-4;
    let mut best: Option<(usize, usize)> = None; // (frames, middle sample)
    let mut run_start = None;
    for i in 0..=window.len() {
        let is_quiet = window.get(i).is_some_and(|&rms| rms <= quiet);
        match (is_quiet, run_start) {
            (true, None) => run_start = Some(i),
            (false, Some(start)) => {
                let middle = (first + start + first + i) * frame / 2;
                let better = best.is_none_or(|(length, at)| {
                    i - start > length
                        || (i - start == length && middle.abs_diff(even) < at.abs_diff(even))
                });
                if better {
                    best = Some((i - start, middle));
                }
                run_start = None;
            }
            _ => {}
        }
    }
    best.map_or(even, |(_, at)| at)
        .clamp(range.start, range.end)
}

/// Join decoded pieces the way dictation joins its phrases: a space between
/// words of spaced scripts, nothing between Chinese or Japanese characters.
fn append_piece(text: &mut String, piece: &str) {
    let piece = piece.trim();
    if !piece.is_empty() {
        text.push_str(join_separator(text, piece));
        text.push_str(piece);
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
        let recognizer = &self.recognizer;
        let mut decode = |piece: &[f32]| {
            recognize(recognizer, piece, sample_rate).map(|result| {
                result.map(|result| (strip_qwen3_header(result.text), result.tokens.len()))
            })
        };
        transcribe_in_pieces(
            &mut decode,
            samples,
            sample_rate,
            self.chunk_ms,
            self.english,
        )
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

#[cfg(test)]
mod qwen3_long_audio_tests {
    use super::*;

    const RATE: u32 = 16_000;

    fn at(rate: u32, ms: usize) -> usize {
        rate as usize * ms / 1000
    }

    /// Five syllables a second: 120 ms loud, 80 ms softer, never silent.
    fn syllables(rate: u32, ms: usize) -> Vec<f32> {
        (0..at(rate, ms))
            .map(|i| {
                let loud = (i * 1000 / rate as usize) % 200 < 120;
                let tone = (i as f32 * 2.0 * std::f32::consts::PI * 220.0 / rate as f32).sin();
                if loud {
                    0.2 * tone
                } else {
                    0.03 * tone
                }
            })
            .collect()
    }

    /// Replace `ms` from `from_ms` with a steady tone of `amplitude` (0 = silence).
    fn quiet(audio: &mut [f32], rate: u32, from_ms: usize, ms: usize, amplitude: f32) {
        for (i, sample) in audio[at(rate, from_ms)..at(rate, from_ms + ms)]
            .iter_mut()
            .enumerate()
        {
            *sample = amplitude * (i as f32 * 0.37).sin();
        }
    }

    fn hum(rate: u32, ms: usize) -> Vec<f32> {
        (0..at(rate, ms))
            .map(|i| 0.05 * (i as f32 * 2.0 * std::f32::consts::PI * 60.0 / rate as f32).sin())
            .collect()
    }

    /// Loud syllable onsets, the fake recogniser's one "word" each.
    fn onsets(piece: &[f32]) -> usize {
        let frames = frame_rms(piece, RATE as usize / 100);
        frames
            .windows(2)
            .filter(|pair| pair[0] < 0.08 && pair[1] >= 0.08)
            .count()
            + usize::from(frames.first().is_some_and(|&rms| rms >= 0.08))
    }

    fn assert_covers(chunks: &[Range<usize>], len: usize, max: usize) {
        assert_eq!(chunks.first().unwrap().start, 0);
        assert_eq!(chunks.last().unwrap().end, len);
        for pair in chunks.windows(2) {
            assert_eq!(pair[0].end, pair[1].start, "gap or overlap: {chunks:?}");
        }
        for chunk in chunks {
            assert!(!chunk.is_empty() && chunk.len() <= max, "{chunk:?} > {max}");
        }
    }

    #[test]
    fn long_input_is_cut_into_pieces_at_its_pauses_at_any_rate() {
        for rate in [16_000, 48_000] {
            let mut audio = syllables(rate, 45_000);
            let pauses = [7_000, 14_500, 22_200, 29_000, 37_000];
            for from in pauses {
                quiet(&mut audio, rate, from, 300, 0.0);
            }
            let chunks = plan_chunks(&audio, rate, 20_000);
            assert_eq!(chunks.len(), 3, "{chunks:?}");
            assert_covers(&chunks, audio.len(), at(rate, 20_000));
            for chunk in &chunks[1..] {
                assert!(
                    pauses
                        .iter()
                        .any(|&from| (at(rate, from)..at(rate, from + 300)).contains(&chunk.start)),
                    "cut at {} ms is not in a pause",
                    chunk.start * 1000 / rate as usize
                );
            }
        }
    }

    #[test]
    fn input_up_to_the_limit_is_one_piece() {
        let audio = syllables(RATE, 20_000);
        for (samples, rate) in [(&audio[..], RATE), (&[][..], RATE), (&audio[..], 0)] {
            let chunks = plan_chunks(samples, rate, 20_000);
            assert_eq!(chunks.len(), 1);
            assert_eq!(chunks[0], 0..samples.len());
        }
    }

    #[test]
    fn pieces_are_even_rather_than_leaving_a_sliver() {
        // 21 s of speech with no pause at all: two ~10.5 s pieces, not 20 + 1.
        let audio = syllables(RATE, 21_000);
        let chunks = plan_chunks(&audio, RATE, 20_000);
        assert_eq!(chunks.len(), 2);
        assert_covers(&chunks, audio.len(), at(RATE, 20_000));
        assert!(chunks[1].len() >= at(RATE, 7_000), "{chunks:?}");
    }

    #[test]
    fn continuous_speech_is_cut_at_a_breath_not_a_one_frame_dropout() {
        let mut audio = syllables(RATE, 30_000);
        quiet(&mut audio, RATE, 14_000, 200, 0.005); // a breath: quiet, not silent
        quiet(&mut audio, RATE, 15_500, 10, 0.0); // one dropped frame mid-word
        let chunks = plan_chunks(&audio, RATE, 20_000);
        assert_eq!(chunks.len(), 2);
        let cut = chunks[1].start;
        assert!(
            (at(RATE, 14_000)..at(RATE, 14_200)).contains(&cut),
            "cut at {} ms",
            cut * 1000 / RATE as usize
        );
    }

    #[test]
    fn retry_split_is_a_pause_near_the_middle_and_never_a_sliver() {
        let mut audio = syllables(RATE, 2_000);
        quiet(&mut audio, RATE, 850, 100, 0.0);
        let cut = split_near_middle(&audio, RATE).unwrap();
        assert!((at(RATE, 850)..at(RATE, 950)).contains(&cut), "{cut}");
        assert_eq!(split_near_middle(&syllables(RATE, 1_400), RATE), None);
        let cut = split_near_middle(&syllables(RATE, 1_500), RATE).unwrap();
        assert_eq!(cut, at(RATE, 750), "both halves at least 750 ms");
        assert_eq!(split_near_middle(&audio, 0), None);
    }

    #[test]
    fn answer_room_follows_the_decoders_context() {
        assert_eq!(
            answer_room(at(RATE, 2_260), RATE),
            256,
            "short: max_new_tokens"
        );
        // 20 s = 2000 mel frames = 260 audio tokens; 513 - 15 - 260.
        assert_eq!(answer_room(at(RATE, 20_000), RATE), 238);
        assert_eq!(answer_room(at(48_000, 20_000), 48_000), 238);
        // 33.3 s: 3330 frames = 13 * 33 + ceil(30 / 8) = 433 tokens (sherpa log).
        assert_eq!(answer_room(at(RATE, 33_300), RATE), 65);
        assert_eq!(
            answer_room(at(RATE, 60_000), RATE),
            0,
            "only \"language\" fits"
        );
    }

    #[test]
    fn header_without_an_answer_is_garbage() {
        for text in [
            "language",
            " language English ",
            "language Chinese",
            "x<asr_text>",
        ] {
            assert!(is_bare_header(text), "{text:?}");
            assert_eq!(
                suspect(text, 1, &syllables(RATE, 2_000), RATE, false),
                Some(Suspect::Header)
            );
        }
        for text in [
            "languages",
            "language is hard",
            "Language.",
            "The language English",
        ] {
            assert!(!is_bare_header(text), "{text:?}");
        }
    }

    #[test]
    fn an_answer_that_filled_the_room_left_is_cut_off() {
        let audio = syllables(RATE, 20_000);
        let words = "word ".repeat(60);
        assert_eq!(
            suspect(&words, 234, &audio, RATE, true),
            Some(Suspect::Truncated)
        );
        assert_eq!(suspect(&words, 100, &audio, RATE, true), None);
        assert_eq!(
            suspect(&words, 252, &syllables(RATE, 5_000), RATE, true),
            Some(Suspect::Truncated),
            "max_new_tokens"
        );
    }

    #[test]
    fn chinese_script_is_wrong_only_on_the_english_route() {
        let audio = syllables(RATE, 2_000);
        assert_eq!(
            suspect("提纲", 2, &audio, RATE, true),
            Some(Suspect::WrongScript)
        );
        assert_eq!(
            suspect("提到了system,", 4, &audio, RATE, true),
            Some(Suspect::WrongScript)
        );
        assert_eq!(suspect("提纲", 2, &audio, RATE, false), None);
        assert_eq!(suspect("Sounds good.", 3, &audio, RATE, true), None);
    }

    #[test]
    fn empty_is_suspect_only_for_audio_that_holds_speech() {
        assert_eq!(
            suspect("", 0, &syllables(RATE, 2_000), RATE, true),
            Some(Suspect::Empty)
        );
        assert_eq!(suspect("", 0, &hum(RATE, 5_000), RATE, true), None);
        assert_eq!(
            suspect("", 0, &vec![0.0; at(RATE, 5_000)], RATE, true),
            None
        );
        assert_eq!(
            suspect(" . ", 1, &syllables(RATE, 2_000), RATE, true),
            Some(Suspect::Empty)
        );
    }

    #[test]
    fn far_too_few_words_for_the_speech_is_suspect() {
        let audio = syllables(RATE, 10_000);
        assert!((5.0..=7.0).contains(&speech_seconds(&audio, RATE)));
        assert_eq!(speech_seconds(&hum(RATE, 10_000), RATE), 0.0);
        assert_eq!(
            suspect("Okay.", 2, &audio, RATE, true),
            Some(Suspect::Sparse)
        );
        assert_eq!(
            suspect("好的。", 3, &audio, RATE, false),
            Some(Suspect::Sparse)
        );
        assert_eq!(
            suspect("Okay then, ship it today.", 6, &audio, RATE, true),
            None
        );
        assert_eq!(
            suspect("好的，我们今天就发布。", 10, &audio, RATE, false),
            None
        );
        // Under 4 s of speech a short answer is ordinary.
        assert_eq!(
            suspect("Okay.", 2, &syllables(RATE, 5_000), RATE, true),
            None
        );
    }

    #[test]
    fn only_a_short_or_cut_off_answer_is_worth_keeping() {
        for garbage in [Suspect::Header, Suspect::WrongScript, Suspect::Empty] {
            assert_eq!(garbage.salvage("提纲".into()), "");
        }
        for partial in [Suspect::Truncated, Suspect::Sparse] {
            assert_eq!(partial.salvage("Okay.".into()), "Okay.");
        }
    }

    #[test]
    fn words_and_pieces_join_as_their_scripts_do() {
        assert_eq!(text_units("Hello, world."), 2);
        assert_eq!(text_units("把这个 bug 修一下"), 7);
        assert_eq!(text_units(" 。 ... "), 0);
        assert_eq!(text_units("मैं कल आऊँगा।"), 3);
        let mut text = String::new();
        for piece in ["Hello there.", " ", "how are you?"] {
            append_piece(&mut text, piece);
        }
        assert_eq!(text, "Hello there. how are you?");
        let mut text = String::new();
        for piece in ["你好。", "我们开会", "吧。"] {
            append_piece(&mut text, piece);
        }
        assert_eq!(text, "你好。我们开会吧。");
    }

    /// A 60 s input once came back as the one word "language". With a fake
    /// recogniser that has the real one's context limit, every syllable
    /// survives and that word never does.
    #[test]
    fn a_minute_of_speech_is_decoded_whole_and_never_as_language() {
        let mut audio = syllables(RATE, 60_000);
        for from in [9_000, 17_100, 26_300, 33_000, 44_000, 52_500] {
            quiet(&mut audio, RATE, from, 250, 0.0);
        }
        let mut pieces = Vec::new();
        let mut decode = |piece: &[f32]| -> Result<Option<(String, usize)>> {
            pieces.push(piece.len());
            if piece.len() > at(RATE, 38_000) {
                return Ok(Some(("language".into(), 1)));
            }
            let words = onsets(piece);
            Ok(Some((vec!["la"; words].join(" "), words)))
        };
        let text = transcribe_in_pieces(&mut decode, &audio, RATE, 20_000, true).unwrap();
        assert!(!text.contains("language"));
        assert_eq!(pieces.len(), 3, "{pieces:?}");
        assert!(pieces.iter().all(|&len| len <= at(RATE, 20_000)));
        assert_eq!(text.split_whitespace().count(), onsets(&audio));
    }

    #[test]
    fn a_wrong_script_answer_is_replaced_by_its_halves() {
        let audio = syllables(RATE, 2_000);
        let mut calls = 0;
        let mut decode = |piece: &[f32]| -> Result<Option<(String, usize)>> {
            calls += 1;
            Ok(Some(if piece.len() == audio.len() {
                ("提纲".into(), 2)
            } else {
                ("Sounds good.".into(), 3)
            }))
        };
        let text = transcribe_in_pieces(&mut decode, &audio, RATE, 20_000, true).unwrap();
        assert_eq!(text, "Sounds good. Sounds good.");
        assert_eq!(calls, 3);
    }

    #[test]
    fn bare_language_is_dropped_when_every_retry_fails_too() {
        let audio = syllables(RATE, 20_000);
        let mut calls = 0;
        let mut decode = |_: &[f32]| -> Result<Option<(String, usize)>> {
            calls += 1;
            Ok(Some(("language".into(), 1)))
        };
        let text = transcribe_in_pieces(&mut decode, &audio, RATE, 20_000, true).unwrap();
        assert_eq!(text, "");
        assert_eq!(calls, 1 + 2 + 4, "halves, then quarters, then stop");
    }

    #[test]
    fn a_cut_off_answer_stands_when_its_halves_recover_less() {
        let audio = syllables(RATE, 20_000);
        let words = "word ".repeat(40);
        let mut decode = |piece: &[f32]| -> Result<Option<(String, usize)>> {
            Ok(Some(if piece.len() == audio.len() {
                (words.trim().to_string(), 240)
            } else {
                ("word".into(), 1)
            }))
        };
        let text = transcribe_in_pieces(&mut decode, &audio, RATE, 20_000, true).unwrap();
        assert_eq!(text, words.trim());
    }

    #[test]
    fn nothing_from_steady_noise_is_accepted_without_a_retry() {
        let mut calls = 0;
        let mut decode = |_: &[f32]| -> Result<Option<(String, usize)>> {
            calls += 1;
            Ok(Some((String::new(), 0)))
        };
        let text = transcribe_in_pieces(&mut decode, &hum(RATE, 18_000), RATE, 20_000, true);
        assert_eq!(text.unwrap(), "");
        assert_eq!(calls, 1);
    }

    #[test]
    fn hindi_and_other_routes_use_shorter_pieces() {
        assert_eq!(qwen3_chunk_ms("en"), 20_000);
        assert_eq!(qwen3_chunk_ms("zh"), 20_000);
        assert_eq!(qwen3_chunk_ms("hi"), 15_000);
        assert_eq!(qwen3_chunk_ms(""), 15_000);
    }
}
