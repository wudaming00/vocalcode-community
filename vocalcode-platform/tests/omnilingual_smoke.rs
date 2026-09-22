//! Optional real-model smoke test for the Omnilingual ASR adapter.
//!
//! Run with:
//!   VOCALCODE_OMNI_MODEL_DIR=<dir> VOCALCODE_OMNI_WAV=<wav> \
//!     cargo test -p vocalcode-platform --test omnilingual_smoke -- --ignored --nocapture

use std::path::PathBuf;

use vocalcode_core::traits::Asr;
use vocalcode_platform::SherpaOmnilingualAsr;

fn mono_samples(path: &PathBuf) -> (Vec<f32>, u32) {
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
#[ignore = "requires the separately downloaded Omnilingual ASR model"]
fn omnilingual_decodes_local_audio_to_nonempty_text() {
    let root = PathBuf::from(
        std::env::var_os("VOCALCODE_OMNI_MODEL_DIR").expect("set VOCALCODE_OMNI_MODEL_DIR"),
    );
    let wav =
        PathBuf::from(std::env::var_os("VOCALCODE_OMNI_WAV").expect("set VOCALCODE_OMNI_WAV"));
    let (samples, sample_rate) = mono_samples(&wav);
    let mut recognizer = SherpaOmnilingualAsr::new(
        &root.join("model.int8.onnx").to_string_lossy(),
        &root.join("tokens.txt").to_string_lossy(),
        4,
        "Omnilingual ASR smoke test",
    )
    .expect("load Omnilingual ASR");
    let text = recognizer
        .transcribe(&samples, sample_rate)
        .expect("transcribe Omnilingual ASR");
    assert!(
        !text.trim().is_empty(),
        "Omnilingual ASR returned empty text"
    );
    println!("{text}");
}
