//! Explicit local QA only: spoken WAV -> real SenseVoice -> production engine
//! with every Writing rule on -> an in-memory injector. Shows, per clip, what
//! the model heard and what VocalCode would type (and whether it would press
//! Enter). No microphone, hotkeys, clipboard, focus or user data are touched.
//!
//! ```text
//! cargo run -p vocalcode-platform --example writing_replay -- \
//!     <sensevoice-model-dir> <report.json> <en|zh>:<clip.wav> [...]
//! ```

#[cfg(windows)]
mod replay {
    use std::sync::{atomic::AtomicBool, Arc, Mutex};
    use vocalcode_core::{
        traits::TriggerId, writing, AudioCapture, Engine, Outcome, Recording, Result, TextInjector,
        TriggerEvent,
    };
    use vocalcode_platform::{AcronymCollapser, Normalizer, SherpaSenseVoiceAsr};

    struct Clip(Arc<Vec<f32>>);
    impl AudioCapture for Clip {
        fn start(&mut self) -> Result<()> {
            Ok(())
        }
        fn stop(&mut self) -> Result<Recording> {
            Ok(Recording {
                samples: self.0.to_vec(),
                sample_rate: 16_000,
            })
        }
        fn is_recording(&self) -> bool {
            false
        }
    }

    #[derive(Clone, Default)]
    struct Collector(Arc<Mutex<Vec<String>>>);
    impl TextInjector for Collector {
        fn inject_text(&self, text: &str) -> Result<()> {
            self.0.lock().unwrap().push(format!("type:{text}"));
            Ok(())
        }
        fn send_enter(&self) -> Result<()> {
            self.0.lock().unwrap().push("enter".into());
            Ok(())
        }
        fn backspace(&self, _: usize) -> Result<()> {
            Ok(())
        }
    }

    fn read_wav(path: &str) -> anyhow::Result<Vec<f32>> {
        let mut reader = hound::WavReader::open(path)?;
        let spec = reader.spec();
        anyhow::ensure!(
            spec.channels == 1 && spec.sample_rate == 16_000 && spec.bits_per_sample == 16,
            "{path}: expected 16 kHz mono PCM16"
        );
        Ok(reader
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn run() -> anyhow::Result<()> {
        let args: Vec<String> = std::env::args().collect();
        anyhow::ensure!(
            args.len() >= 4,
            "usage: writing_replay <sensevoice-model-dir> <report.json> <en|zh>:<clip.wav> [...]"
        );
        let dir = std::path::Path::new(&args[1]);
        let model = dir.join("model.int8.onnx");
        let tokens = dir.join("tokens.txt");
        let options = writing::Options {
            commands: true,
            backtrack: true,
            lists: true,
            code: true,
            press_enter: true,
            style: writing::Style::Formal,
        };
        let mut report = Vec::new();
        for spec in &args[3..] {
            let (language, wav) = spec
                .split_once(':')
                .filter(|(l, _)| matches!(*l, "en" | "zh"))
                .ok_or_else(|| anyhow::anyhow!("clip must be en:<wav> or zh:<wav>"))?;
            let audio = Arc::new(read_wav(wav)?);
            let asr = SherpaSenseVoiceAsr::new(
                &model.to_string_lossy(),
                &tokens.to_string_lossy(),
                language,
                4,
                "SenseVoice writing replay",
            )?;
            let collector = Collector::default();
            let mut engine = Engine::new(
                Box::new(Clip(audio.clone())),
                Box::new(asr),
                Box::new(collector.clone()),
                250,
                16_000,
                false,
                Arc::new(AtomicBool::new(true)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(Mutex::new(Vec::new())),
                vec![Box::new(AcronymCollapser), Box::new(Normalizer)],
            );
            engine.set_trace_enabled(true);
            engine.set_writing(options, language);
            let id = TriggerId::synthetic(7);
            engine.handle(TriggerEvent::TalkPressed(id))?;
            let outcome = engine.handle(TriggerEvent::TalkReleased(id))?;
            let Outcome::Transcribed(text) = outcome else {
                anyhow::bail!("{wav}: unexpected outcome {outcome:?}");
            };
            let trace = engine
                .take_trace()
                .ok_or_else(|| anyhow::anyhow!("missing trace"))?;
            let events = collector.0.lock().unwrap().clone();
            println!(
                "── {wav} ({language}, {:.1}s)",
                audio.len() as f64 / 16000.0
            );
            println!("   heard : {}", trace.raw_text);
            println!("   typed : {}", text.replace('\n', "⏎\n           "));
            println!(
                "   events: {:?}   writing edits: {}",
                events, trace.writing_edits
            );
            report.push(serde_json::json!({
                "clip": wav,
                "language": language,
                "audio_seconds": audio.len() as f64 / 16000.0,
                "heard": trace.raw_text,
                "typed": text,
                "events": events,
                "writing_edits": trace.writing_edits,
            }));
        }
        std::fs::write(&args[2], serde_json::to_vec_pretty(&report)?)?;
        Ok(())
    }
}

fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    return replay::run();
    #[cfg(not(windows))]
    anyhow::bail!("writing_replay is a Windows-only local QA harness");
}
