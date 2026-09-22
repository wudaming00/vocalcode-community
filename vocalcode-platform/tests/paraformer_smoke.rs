//! Optional real-model smoke test for the Paraformer adapter.
//!
//! Run with:
//!   VOCALCODE_PARAFORMER_MODEL=<model.onnx> \
//!   VOCALCODE_PARAFORMER_TOKENS=<tokens.txt> \
//!   VOCALCODE_PARAFORMER_WAV=<wav> \
//!     cargo test -p vocalcode-platform --test paraformer_smoke -- --ignored --nocapture

use std::path::PathBuf;

use vocalcode_core::traits::Asr;
use vocalcode_platform::SherpaParaformerAsr;

#[test]
#[ignore = "requires a separately downloaded Paraformer model"]
fn paraformer_decodes_local_audio_to_nonempty_text() {
    let model = PathBuf::from(
        std::env::var_os("VOCALCODE_PARAFORMER_MODEL").expect("set VOCALCODE_PARAFORMER_MODEL"),
    );
    let tokens = PathBuf::from(
        std::env::var_os("VOCALCODE_PARAFORMER_TOKENS").expect("set VOCALCODE_PARAFORMER_TOKENS"),
    );
    let wav = PathBuf::from(
        std::env::var_os("VOCALCODE_PARAFORMER_WAV").expect("set VOCALCODE_PARAFORMER_WAV"),
    );

    let mut reader = hound::WavReader::open(&wav).expect("open wav");
    let spec = reader.spec();
    assert_eq!(spec.channels, 1, "smoke fixture must be mono");
    let samples = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .map(|sample| sample.expect("wav sample"))
            .collect::<Vec<_>>(),
        hound::SampleFormat::Int => {
            let scale = (1_i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|sample| sample.expect("wav sample") as f32 / scale)
                .collect::<Vec<_>>()
        }
    };

    let mut recognizer =
        SherpaParaformerAsr::new(&model.to_string_lossy(), &tokens.to_string_lossy(), 4)
            .expect("load Paraformer");
    let text = recognizer
        .transcribe(&samples, spec.sample_rate)
        .expect("transcribe Paraformer");
    assert!(!text.trim().is_empty(), "Paraformer returned empty text");
    println!("{text}");
}
