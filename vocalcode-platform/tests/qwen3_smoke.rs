//! Optional real-model smoke test for the Qwen3-ASR adapter.
//!
//! Run with:
//!   VOCALCODE_QWEN3_MODEL_DIR=<dir> VOCALCODE_QWEN3_WAV=<16-kHz-wav> \
//!     cargo test -p vocalcode-platform --test qwen3_smoke -- --ignored --nocapture

use std::path::PathBuf;

use vocalcode_core::traits::Asr;
use vocalcode_platform::SherpaQwen3Asr;

#[test]
#[ignore = "requires the separately downloaded Qwen3-ASR model"]
fn qwen3_decodes_local_audio_to_nonempty_text() {
    let root = PathBuf::from(
        std::env::var_os("VOCALCODE_QWEN3_MODEL_DIR").expect("set VOCALCODE_QWEN3_MODEL_DIR"),
    );
    let wav =
        PathBuf::from(std::env::var_os("VOCALCODE_QWEN3_WAV").expect("set VOCALCODE_QWEN3_WAV"));
    let mut reader = hound::WavReader::open(wav).expect("open wav");
    assert_eq!(reader.spec().sample_rate, 16_000);
    assert_eq!(reader.spec().channels, 1);
    let samples = reader
        .samples::<i16>()
        .map(|sample| sample.expect("wav sample") as f32 / i16::MAX as f32)
        .collect::<Vec<_>>();
    let tokenizer = if root.join("tokenizer").is_dir() {
        root.join("tokenizer")
    } else {
        root.clone()
    };
    let mut recognizer = SherpaQwen3Asr::new(
        &root.join("conv_frontend.onnx").to_string_lossy(),
        &root.join("encoder.int8.onnx").to_string_lossy(),
        &root.join("decoder.int8.onnx").to_string_lossy(),
        &tokenizer.to_string_lossy(),
        8,
        "Qwen3-ASR smoke test",
    )
    .expect("load Qwen3-ASR");
    let text = recognizer
        .transcribe(&samples, 16_000)
        .expect("transcribe Qwen3-ASR");
    assert!(!text.trim().is_empty(), "Qwen3-ASR returned empty text");
    println!("{text}");
}
