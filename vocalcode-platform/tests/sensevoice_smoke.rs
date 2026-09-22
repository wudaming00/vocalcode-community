//! End-to-end smoke test for the SenseVoice Rust wrapper.
//!
//! Ignored by default: it needs the ~229 MB SenseVoice model on disk, which the
//! app downloads at runtime and CI does not carry. It exists so the exact
//! `SherpaSenseVoiceAsr::new` + `transcribe` path a Korean/Japanese user runs
//! can be exercised against real audio without touching a live install.
//!
//! Run it by pointing three env vars at a local model + a wav and passing
//! `--ignored`:
//!
//! ```text
//! VOCALCODE_SV_MODEL=…/model.int8.onnx \
//! VOCALCODE_SV_TOKENS=…/tokens.txt \
//! VOCALCODE_SV_WAV=…/kt00.wav \
//!   cargo test -p vocalcode-platform --test sensevoice_smoke -- --ignored --nocapture
//! ```

use vocalcode_core::traits::Asr;
use vocalcode_platform::SherpaSenseVoiceAsr;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Read a mono f32 buffer from a wav, downmixing and scaling exactly like the
/// app's `transcribe` self-test does, so this measures the same pipeline.
fn mono_samples(path: &str) -> (Vec<f32>, u32) {
    let mut reader = hound::WavReader::open(path).expect("open wav");
    let spec = reader.spec();
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap_or(0.0)).collect(),
        hound::SampleFormat::Int => {
            let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.unwrap_or(0) as f32 / scale)
                .collect()
        }
    };
    let channels = spec.channels as usize;
    let mono = if channels > 1 {
        interleaved
            .chunks(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect()
    } else {
        interleaved
    };
    (mono, spec.sample_rate)
}

#[test]
#[ignore = "needs a local SenseVoice model + wav; see the module docs"]
fn sensevoice_decodes_local_audio_to_nonempty_text() {
    let (Some(model), Some(tokens), Some(wav)) = (
        env("VOCALCODE_SV_MODEL"),
        env("VOCALCODE_SV_TOKENS"),
        env("VOCALCODE_SV_WAV"),
    ) else {
        panic!(
            "set VOCALCODE_SV_MODEL, VOCALCODE_SV_TOKENS, and VOCALCODE_SV_WAV \
             to a local model.int8.onnx, tokens.txt, and a wav file"
        );
    };

    let mut asr = SherpaSenseVoiceAsr::new(&model, &tokens, "auto", 4, "SenseVoice · smoke")
        .expect("load SenseVoice model");
    let (samples, sample_rate) = mono_samples(&wav);
    let text = asr.transcribe(&samples, sample_rate).expect("transcribe");

    eprintln!("SenseVoice output: {text:?}");
    assert!(
        !text.trim().is_empty(),
        "SenseVoice returned no text for {wav}"
    );
}
