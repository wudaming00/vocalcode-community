//! Placeholder ASR used to validate the capture→transcribe→inject wiring
//! before sherpa-onnx is integrated. Dumps the captured audio to a WAV so we
//! can confirm the microphone path works, and returns a diagnostic string.

use vocalcode_core::error::{Result, VocalCodeError};
use vocalcode_core::traits::Asr;

#[derive(Default)]
pub struct StubAsr;

impl Asr for StubAsr {
    fn transcribe(&mut self, samples: &[f32], sample_rate: u32) -> Result<String> {
        let path = "vocalcode-debug.wav";
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer =
            hound::WavWriter::create(path, spec).map_err(|e| VocalCodeError::Asr(e.to_string()))?;
        for &s in samples {
            let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            writer
                .write_sample(v)
                .map_err(|e| VocalCodeError::Asr(e.to_string()))?;
        }
        writer
            .finalize()
            .map_err(|e| VocalCodeError::Asr(e.to_string()))?;

        let secs = samples.len() as f32 / sample_rate as f32;
        log::error!(
            "STUB ASR active (no model loaded): {} samples, {secs:.2}s dumped to {path}",
            samples.len()
        );
        // Return empty so we never type diagnostic junk into the user's field.
        Ok(String::new())
    }

    fn model_label(&self) -> &str {
        "stub (no ASR yet)"
    }
}
