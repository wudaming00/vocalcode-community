//! Local, read-only Qwen3-ASR long-input probe. No microphone or text injection.
//! Args: qwen3-dir language voice-corpus-dir cases.json voice[,voice...] [threads]
//!   or: qwen3-dir language manifest.tsv [threads]   (WAV TAB reference lines)
//! With `--each`, every clip is decoded as it is and reported on its own line
//! (for recordings that are already long), and nothing is joined.
//!
//! One speaker's clean clips (leading/trailing silence trimmed) are decoded
//! one by one, then joined with 150 ms gaps — shorter than the 240 ms pause
//! dictation would cut at, so this is how one continuous utterance reaches the
//! recogniser — into ~30, 60 and 90 s inputs, cycling through the clips when
//! there is too little audio. Each output line is one JSON record;
//! `error_rate` is WER for spaced scripts and CER for zh. In context Qwen3
//! writes spoken identifiers as code ("snake case max retry count" becomes
//! "SnakeCaseMaxRetryCount"), which strict WER counts as five errors, so
//! `error_rate_unjoined` scores again with such joins split back into words.
use std::{collections::HashMap, env, fs, path::Path, time::Instant};

use vocalcode_core::Asr;
use vocalcode_platform::SherpaQwen3Asr;

const RATE: usize = 16_000;
const GAP_MS: usize = 150;

fn unjoin(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let previous = i.checked_sub(1).map(|p| chars[p]);
        let next = chars.get(i + 1);
        if c.is_uppercase()
            && previous.is_some_and(|p| {
                p.is_lowercase() || (p.is_uppercase() && next.is_some_and(|n| n.is_lowercase()))
            })
        {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

fn units(text: &str, language: &str) -> Vec<String> {
    let lower: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    // As packaging/voice-corpus/score.py: "don't" is one word. Devanagari
    // signs stay inside their word; its danda ends a sentence.
    let text: String = lower
        .iter()
        .enumerate()
        .filter_map(|(i, &c)| {
            let apostrophe = matches!(c, '\'' | '’')
                && i > 0
                && lower[i - 1].is_alphanumeric()
                && lower.get(i + 1).is_some_and(|n| n.is_alphanumeric());
            let devanagari = matches!(c as u32, 0x0900..=0x0963 | 0x0966..=0x097f);
            if apostrophe {
                None
            } else if c.is_alphanumeric() || devanagari {
                Some(c)
            } else {
                Some(' ')
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

fn edits(reference: &[String], text: &[String]) -> usize {
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
    row[text.len()]
}

fn read_clip(path: &Path) -> anyhow::Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    anyhow::ensure!(
        spec.channels == 1 && spec.sample_rate as usize == RATE,
        "{}: expected 16 kHz mono",
        path.display()
    );
    let scale = (1_i64 << (spec.bits_per_sample - 1)) as f32;
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => reader
            .samples::<i32>()
            .map(|s| s.map(|v| v as f32 / scale))
            .collect::<Result<_, _>>()?,
    };
    // Trim the recording's own lead-in and tail so the joined gaps are what
    // the probe says they are; keep 30 ms either side of the speech.
    let frame = RATE / 100;
    let rms: Vec<f32> = samples
        .chunks(frame)
        .map(|f| (f.iter().map(|v| v * v).sum::<f32>() / f.len() as f32).sqrt())
        .collect();
    let peak = rms.iter().copied().fold(0.0, f32::max);
    let loud = |r: &f32| *r > (peak * 0.03).max(0.001);
    let first = rms.iter().position(loud).unwrap_or(0).saturating_sub(3);
    let last = (rms.iter().rposition(loud).unwrap_or(rms.len()) + 4).min(rms.len());
    Ok(samples[first * frame..(last * frame).min(samples.len())].to_vec())
}

struct Clip {
    audio: Vec<f32>,
    say: String,
}

/// Strict edits, edits with joined identifiers split, reference units.
fn score(reference: &str, text: &str, language: &str) -> (usize, usize, usize) {
    let reference = units(reference, language);
    (
        edits(&reference, &units(text, language)),
        edits(&reference, &units(&unjoin(text), language)),
        reference.len(),
    )
}

/// Clean clips of each requested voice, in case order.
fn corpus_sets(
    corpus: &Path,
    cases: &Path,
    language: &str,
    voices: &str,
) -> anyhow::Result<Vec<(String, Vec<Clip>)>> {
    let cases: serde_json::Value = serde_json::from_str(&fs::read_to_string(cases)?)?;
    let say: HashMap<&str, &str> = cases["cases"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("cases array"))?
        .iter()
        .filter_map(|c| Some((c["id"].as_str()?, c["say"].as_str()?)))
        .collect();
    let clips: Vec<serde_json::Value> =
        serde_json::from_str(&fs::read_to_string(corpus.join("clips.json"))?)?;
    let mut sets = Vec::new();
    for voice in voices.split(',') {
        let mut mine: Vec<&serde_json::Value> = clips
            .iter()
            .filter(|c| c["language"] == language && c["voice"] == voice)
            .filter(|c| c["variant"] == "clean")
            .collect();
        mine.sort_by_key(|c| c["case"].as_str().unwrap_or_default().to_string());
        let mut set = Vec::new();
        for clip in mine {
            let Some(&text) = clip["case"].as_str().and_then(|id| say.get(id)) else {
                continue;
            };
            set.push(Clip {
                audio: read_clip(&corpus.join(clip["path"].as_str().unwrap_or_default()))?,
                say: text.to_string(),
            });
        }
        anyhow::ensure!(!set.is_empty(), "no clean {language} clips for {voice}");
        sets.push((voice.to_string(), set));
    }
    Ok(sets)
}

fn manifest_set(manifest: &Path) -> anyhow::Result<Vec<(String, Vec<Clip>)>> {
    let mut set = Vec::new();
    for line in fs::read_to_string(manifest)?.lines() {
        let (file, text) = line
            .split_once('\t')
            .ok_or_else(|| anyhow::anyhow!("expected WAV TAB reference"))?;
        set.push(Clip {
            audio: read_clip(Path::new(file))?,
            say: text.to_string(),
        });
    }
    anyhow::ensure!(!set.is_empty(), "empty manifest");
    let name = manifest
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    Ok(vec![(name, set)])
}

fn main() -> anyhow::Result<()> {
    let mut args: Vec<_> = env::args().collect();
    let each = args.iter().any(|a| a == "--each");
    args.retain(|a| a != "--each");
    let manifest = args.get(3).is_some_and(|a| a.ends_with(".tsv"));
    anyhow::ensure!(
        if manifest {
            (4..=5).contains(&args.len())
        } else {
            (6..=7).contains(&args.len())
        },
        "qwen3-dir language (voice-corpus-dir cases.json voice[,voice...] | manifest.tsv) [threads] [--each]"
    );
    let language = args[2].as_str();
    let threads = args
        .get(if manifest { 4 } else { 6 })
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(8);
    let model = |name: &str| {
        Path::new(&args[1])
            .join(name)
            .to_string_lossy()
            .into_owned()
    };
    let mut asr = SherpaQwen3Asr::new(
        &model("conv_frontend.onnx"),
        &model("encoder.int8.onnx"),
        &model("decoder.int8.onnx"),
        &args[1],
        language,
        threads,
        "qwen3-long-audio-probe",
    )?;
    let sets = if manifest {
        manifest_set(Path::new(&args[3]))?
    } else {
        corpus_sets(Path::new(&args[3]), Path::new(&args[4]), language, &args[5])?
    };
    let joiner = if language == "zh" { "" } else { " " };
    // Warm-up is separate and does not enter any measurement.
    let _ = asr.transcribe(&vec![0.01; RATE * 2], RATE as u32)?;
    for (voice, set) in sets {
        let (mut wrong, mut unjoined, mut total) = (0, 0, 0);
        let (mut seconds, mut decode_ms) = (0.0, 0.0);
        let mut bare_language = 0;
        for clip in &set {
            let start = Instant::now();
            let text = asr.transcribe(&clip.audio, RATE as u32)?;
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            decode_ms += ms;
            seconds += clip.audio.len() as f64 / RATE as f64;
            bare_language += usize::from(text.trim() == "language");
            let (e, u, n) = score(&clip.say, &text, language);
            wrong += e;
            unjoined += u;
            total += n;
            if each {
                println!(
                    "{}",
                    serde_json::json!({"voice": voice, "input": "whole clip", "clips": 1,
                        "audio_seconds": clip.audio.len() as f64 / RATE as f64,
                        "error_rate": e as f64 / n.max(1) as f64,
                        "error_rate_unjoined": u as f64 / n.max(1) as f64, "errors": e,
                        "units": n, "bare_language": text.trim() == "language",
                        "decode_ms": ms, "text": text})
                );
            }
        }
        if each {
            continue;
        }
        println!(
            "{}",
            serde_json::json!({"voice": voice, "input": "short clips", "clips": set.len(),
                "audio_seconds": seconds, "error_rate": wrong as f64 / total.max(1) as f64,
                "error_rate_unjoined": unjoined as f64 / total.max(1) as f64,
                "errors": wrong, "units": total, "bare_language": bare_language,
                "decode_ms": decode_ms})
        );

        for target in [30.0, 60.0, 90.0] {
            let mut audio: Vec<f32> = Vec::new();
            let mut reference = String::new();
            let mut used = 0;
            while (audio.len() as f64 / RATE as f64) < target {
                let clip = &set[used % set.len()];
                if used > 0 {
                    audio.extend(std::iter::repeat_n(0.0, RATE * GAP_MS / 1000));
                    reference.push_str(joiner);
                }
                audio.extend_from_slice(&clip.audio);
                reference.push_str(&clip.say);
                used += 1;
            }
            let start = Instant::now();
            let text = asr.transcribe(&audio, RATE as u32)?;
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            let (e, u, n) = score(&reference, &text, language);
            println!(
                "{}",
                serde_json::json!({"voice": voice, "input": format!("{target:.0} s"),
                    "clips": used, "audio_seconds": audio.len() as f64 / RATE as f64,
                    "error_rate": e as f64 / n.max(1) as f64,
                    "error_rate_unjoined": u as f64 / n.max(1) as f64, "errors": e, "units": n,
                    "bare_language": text.trim() == "language", "decode_ms": ms, "text": text})
            );
        }
    }
    Ok(())
}
