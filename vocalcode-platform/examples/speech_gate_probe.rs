//! Local-only model smoke/benchmark; no microphone, injector or network.
use std::{path::Path, time::Instant};
use vocalcode_platform::speech_gate::SpeechGate;

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(args.len() >= 2, "speech_gate_probe model.onnx [wav ...]");
    let started = Instant::now();
    let mut gate = SpeechGate::load(Path::new(&args[1]))?;
    println!(
        "{}",
        serde_json::json!({"kind":"load","ms":started.elapsed().as_secs_f64()*1000.})
    );
    let mut emit = |id: &str, audio: &[f32]| {
        let start = Instant::now();
        let decision = gate.classify(audio, 16_000);
        println!(
            "{}",
            serde_json::json!({"id":id,"seconds":audio.len() as f64/16000.,"decision":format!("{decision:?}"),"ms":start.elapsed().as_secs_f64()*1000.})
        );
    };
    let mut seed = 42_u32;
    let white: Vec<_> = (0..48_000)
        .map(|_| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            ((seed >> 8) as f32 / 16777216. - 0.5) * 0.04
        })
        .collect();
    let hum: Vec<_> = (0..48_000)
        .map(|i| (i as f32 * std::f32::consts::TAU * 60. / 16000.).sin() * 0.01)
        .collect();
    let mut clicks = vec![0.; 48_000];
    for i in (0..48_000).step_by(4000) {
        for j in 0..32 {
            clicks[i + j] = if j % 2 == 0 { 0.2 } else { -0.2 };
        }
    }
    emit("synthetic-silence", &vec![0.; 48_000]);
    emit("synthetic-white-noise", &white);
    emit("synthetic-hum", &hum);
    emit("synthetic-clicks", &clicks);
    for file in &args[2..] {
        let mut wav = hound::WavReader::open(file)?;
        let spec = wav.spec();
        anyhow::ensure!(
            spec.channels == 1 && spec.sample_rate == 16_000,
            "need mono 16 kHz PCM WAV"
        );
        let samples: Vec<f32> = if spec.sample_format == hound::SampleFormat::Float {
            wav.samples::<f32>().collect::<Result<_, _>>()?
        } else {
            let scale = (1_i64 << (spec.bits_per_sample - 1)) as f32;
            wav.samples::<i32>()
                .map(|v| v.map(|v| v as f32 / scale))
                .collect::<Result<_, _>>()?
        };
        let id = Path::new(file).file_stem().unwrap().to_string_lossy();
        emit(&format!("{id}-original"), &samples);
        emit(
            &format!("{id}-quiet-20db"),
            &samples.iter().map(|v| v * 0.1).collect::<Vec<_>>(),
        );
        let mixed: Vec<_> = samples
            .iter()
            .enumerate()
            .map(|(i, v)| (v + white[i % white.len()] * 0.25).clamp(-1., 1.))
            .collect();
        emit(&format!("{id}-plus-noise"), &mixed);
        // State reset regression: speech may not make following silence pass.
        emit("silence-after-speech", &vec![0.; 48_000]);
    }
    Ok(())
}
