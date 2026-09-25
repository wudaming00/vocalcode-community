//! Read-only, offline quality probe. Never opens a microphone or an injector.
//! Args: model model-dir corpus-dir prefix language threads limit repetitions
use std::{env, fs, path::Path, time::Instant};
use vocalcode_core::traits::Asr;
use vocalcode_platform::{
    SherpaParaformerAsr, SherpaParakeetAsr, SherpaQwen3Asr, SherpaSenseVoiceAsr, SherpaWhisperAsr,
};

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = env::args().collect();
    anyhow::ensure!(
        args.len() == 9,
        "model model-dir corpus-dir prefix language threads limit repetitions"
    );
    let file = |name: &str| {
        Path::new(&args[2])
            .join(name)
            .to_string_lossy()
            .into_owned()
    };
    let threads: i32 = args[6].parse()?;
    let count: usize = args[7].parse()?;
    let repeats: usize = args[8].parse()?;
    anyhow::ensure!(repeats > 0 && count > 0, "empty benchmark");
    let started = Instant::now();
    let mut asr: Box<dyn Asr> = match args[1].as_str() {
        "sensevoice" => Box::new(SherpaSenseVoiceAsr::new(
            &file("model.int8.onnx"),
            &file("tokens.txt"),
            if args[5] == "zh" { "zh" } else { "auto" },
            threads,
            "assessment",
        )?),
        "paraformer" => Box::new(SherpaParaformerAsr::new(
            &file("model.onnx"),
            &file("tokens.txt"),
            threads,
        )?),
        "parakeet" => Box::new(SherpaParakeetAsr::new(
            &file("encoder.onnx"),
            &file("decoder.onnx"),
            &file("joiner.onnx"),
            &file("tokens.txt"),
            threads,
        )?),
        "qwen3" => Box::new(SherpaQwen3Asr::new(
            &file("conv_frontend.onnx"),
            &file("encoder.int8.onnx"),
            &file("decoder.int8.onnx"),
            &file("tokenizer"),
            &args[5],
            threads,
            "assessment",
        )?),
        "whisper-turbo" => Box::new(SherpaWhisperAsr::new(
            &file("turbo-encoder.int8.onnx"),
            &file("turbo-decoder.int8.onnx"),
            &file("turbo-tokens.txt"),
            &args[5],
        )?),
        _ => anyhow::bail!("unsupported model"),
    };
    println!(
        "{}",
        serde_json::json!({"kind":"load", "seconds":started.elapsed().as_secs_f64(),"model":args[1],"threads":threads})
    );
    let references = fs::read_to_string(Path::new(&args[3]).join("trans.txt"))?;
    for index in 0..count {
        let id = format!("{}_{index:04}", args[4]);
        let reference = references
            .lines()
            .find_map(|line| {
                let (key, text) = line.split_once(char::is_whitespace)?;
                (key == id).then(|| text.trim().to_owned())
            })
            .ok_or_else(|| anyhow::anyhow!("missing reference {id}"))?;
        let mut reader = hound::WavReader::open(Path::new(&args[3]).join(format!("{id}.wav")))?;
        let spec = reader.spec();
        anyhow::ensure!(
            spec.channels == 1 && spec.sample_rate == 16_000,
            "expected mono 16 kHz"
        );
        let samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
            hound::SampleFormat::Int => {
                let scale = (1_i64 << (spec.bits_per_sample - 1)) as f32;
                reader
                    .samples::<i32>()
                    .map(|s| s.map(|v| v as f32 / scale))
                    .collect::<Result<_, _>>()?
            }
        };
        let mut durations = Vec::new();
        let mut outputs = Vec::new();
        for _ in 0..repeats {
            let start = Instant::now();
            outputs.push(asr.transcribe(&samples, spec.sample_rate)?);
            durations.push(start.elapsed().as_secs_f64());
        }
        println!(
            "{}",
            serde_json::json!({"kind":"sample","id":id,"reference":reference,"hypothesis":outputs[0],"repeat_consistent":outputs.iter().all(|v| v == &outputs[0]),"seconds":durations,"audio_seconds":samples.len() as f64 / spec.sample_rate as f64})
        );
    }
    for (id, audio) in [
        ("silence-3s", vec![0.0; 48_000]),
        ("short-100ms", vec![0.0; 1_600]),
    ] {
        println!(
            "{}",
            serde_json::json!({"kind":"negative","id":id,"hypothesis":asr.transcribe(&audio,16_000)?})
        );
    }
    Ok(())
}
