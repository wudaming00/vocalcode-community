//! Local, read-only ASR/segmentation probe. No microphone or text injection.
//! Args: sensevoice-dir manifest.tsv language clip-count [threads]
//! The manifest is WAV-path TAB reference. Joins public/synthetic clips with
//! 400 ms silence. Measures decode work, NOT live end-to-end product latency.
use std::{env, fs, path::Path, time::Instant};
use vocalcode_core::{
    segmentation::{join_separator, pause_boundary},
    Asr,
};
use vocalcode_platform::SherpaSenseVoiceAsr;

fn units(text: &str, language: &str) -> Vec<String> {
    let text: String = text
        .chars()
        .flat_map(char::to_lowercase)
        .map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect();
    if language == "zh" {
        text.chars()
            .filter(|c| !c.is_whitespace())
            .map(|c| c.to_string())
            .collect()
    } else {
        text.split_whitespace().map(str::to_owned).collect()
    }
}
fn error_rate(reference: &str, text: &str, language: &str) -> f64 {
    let reference = units(reference, language);
    let text = units(text, language);
    let mut row: Vec<_> = (0..=text.len()).collect();
    for (i, a) in reference.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, b) in text.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (above + 1)
                .min(row[j] + 1)
                .min(diagonal + usize::from(a != b));
            diagonal = above;
        }
    }
    row[text.len()] as f64 / reference.len().max(1) as f64
}
fn main() -> anyhow::Result<()> {
    let args: Vec<_> = env::args().collect();
    anyhow::ensure!(
        (5..=6).contains(&args.len()),
        "sensevoice-dir manifest.tsv language clip-count [threads]"
    );
    let count: usize = args[4].parse()?;
    anyhow::ensure!((1..=20).contains(&count), "clip count must be 1..20");
    let threads = args.get(5).map(|v| v.parse()).transpose()?.unwrap_or(4);
    let path = |name: &str| {
        Path::new(&args[1])
            .join(name)
            .to_string_lossy()
            .into_owned()
    };
    let mut asr = SherpaSenseVoiceAsr::new(
        &path("model.int8.onnx"),
        &path("tokens.txt"),
        if args[3] == "zh" { "zh" } else { "auto" },
        threads,
        "long-dictation-probe",
    )?;
    let manifest = fs::read_to_string(&args[2])?;
    let mut audio = Vec::new();
    let mut reference = String::new();
    let mut clips = 0;
    for line in manifest.lines().take(count) {
        let (file, text) = line
            .split_once('\t')
            .ok_or_else(|| anyhow::anyhow!("expected WAV TAB reference"))?;
        let mut reader = hound::WavReader::open(file)?;
        let spec = reader.spec();
        anyhow::ensure!(
            spec.channels == 1 && spec.sample_rate == 16000,
            "expected 16 kHz mono WAV"
        );
        if clips > 0 {
            audio.extend(vec![0.; 6400]);
            reference.push(' ');
        }
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
        audio.extend(samples);
        reference.push_str(text);
        clips += 1;
    }
    anyhow::ensure!(
        clips == count && audio.len() <= 16000 * 180,
        "missing clips or probe exceeds three minutes"
    );
    // Warm-up is separate and does not enter either decode measurement.
    let _ = asr.transcribe(&audio[..audio.len().min(16000 * 3)], 16000)?;
    let start = Instant::now();
    let whole = asr.transcribe(&audio, 16000)?;
    let whole_ms = start.elapsed().as_secs_f64() * 1000.;
    let mut cases = Vec::new();
    for (mode, min_ms) in [("on_release_preparation", 8000), ("progressive", 300)] {
        let mut cursor = 0;
        let mut output = String::new();
        let mut segments = Vec::new();
        let mut scanned: usize = 0;
        // Supply samples at the desktop's 80 ms poll cadence. This is a
        // deterministic boundary replay, not wall-clock capture scheduling.
        for available in (1280..audio.len()).step_by(1280) {
            let scan_start = cursor.max(scanned.saturating_sub(16000));
            let minimum_end = cursor + min_ms as usize * 16;
            let remaining_ms = minimum_end.saturating_sub(scan_start) / 16;
            scanned = available;
            if let Some(end) =
                pause_boundary(&audio[scan_start..available], 16000, remaining_ms as u32)
            {
                let end = scan_start + end - cursor;
                let begin = Instant::now();
                let text = asr.transcribe(&audio[cursor..cursor + end], 16000)?;
                segments.push(serde_json::json!({"samples":end,"decode_ms":begin.elapsed().as_secs_f64()*1000.}));
                output.push_str(join_separator(&output, text.trim()));
                output.push_str(text.trim());
                cursor += end;
                scanned = cursor;
            }
        }
        let tail = Instant::now();
        if cursor < audio.len() {
            let text = asr.transcribe(&audio[cursor..], 16000)?;
            output.push_str(join_separator(&output, text.trim()));
            output.push_str(text.trim());
        }
        let tail_ms = tail.elapsed().as_secs_f64() * 1000.;
        cases.push(serde_json::json!({"mode":mode,"segments":segments,"tail_audio_ms":(audio.len()-cursor) as f64/16.,"tail_decode_ms":tail_ms,"error_rate":error_rate(&reference,&output,&args[3]),"text":output}));
    }
    println!(
        "{}",
        serde_json::json!({"language":args[3],"threads":threads,"clips":clips,"audio_seconds":audio.len() as f64/16000.,"whole_decode_ms":whole_ms,"whole_error_rate":error_rate(&reference,&whole,&args[3]),"whole_text":whole,"cases":cases,"metric":"ASR work only; not end-to-end latency; concatenated clips with 400 ms artificial pauses"})
    );
    Ok(())
}
