//! Voice-corpus replay through the production dictation pipeline.
//!
//! Test-only and `#[ignore]`d: it needs local model files and generated audio.
//! Every clip is fed through exactly what the desktop app assembles
//! (main.rs start-up and per-utterance code): `models::build_asr` with the
//! route's production language hint and thread count, the route's cleaner
//! chain, the inference worker with the local speech gate, the built-in
//! recognition dictionary, snippets, and the per-utterance Engine settings.
//! Audio is driven by a virtual clock in 80 ms ticks — the app's cadence — and
//! each background decode finishes before the clock moves on, so a run is
//! reproducible and faster than real time.
//!
//! ```text
//! VOCALCODE_QA_MODELS=<dir with one sub-directory per model id>
//! VOCALCODE_VOICE_CORPUS=<generate.py output, containing clips.json>
//! VOCALCODE_VOICE_ROUTES=zh:sensevoice,en:sensevoice,en:qwen3-asr-0.6b,en:parakeet-tdt-v3
//! VOCALCODE_VOICE_RESULTS=<results.jsonl to write>
//! cargo test --release -p vocalcode-app voice_corpus -- --ignored --nocapture
//! ```
//! Scoring: packaging/voice-corpus/score.py.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vocalcode_core::{
    migration::Entry, traits::TriggerId, writing, Asr, AudioCapture, Engine, Outcome, Recording,
    Result, TextInjector, TriggerEvent,
};

const TICK_SAMPLES: usize = 1280; // 80 ms at 16 kHz, the app's live tick

/// Audio that "arrives" as the virtual clock advances.
#[derive(Clone, Default)]
struct ClockedClip {
    samples: Arc<Mutex<Vec<f32>>>,
    cursor: Arc<AtomicUsize>,
    running: Arc<AtomicBool>,
}

impl AudioCapture for ClockedClip {
    fn start(&mut self) -> Result<()> {
        self.cursor.store(0, Ordering::Release);
        self.running.store(true, Ordering::Release);
        Ok(())
    }
    fn stop(&mut self) -> Result<Recording> {
        self.running.store(false, Ordering::Release);
        let all = self.samples.lock().unwrap();
        let end = self.cursor.load(Ordering::Acquire).min(all.len());
        Ok(Recording {
            samples: all[..end].to_vec(),
            sample_rate: 16_000,
        })
    }
    fn is_recording(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }
    fn snapshot(&self) -> Result<Recording> {
        self.snapshot_since(0)
    }
    fn snapshot_since(&self, start: usize) -> Result<Recording> {
        let all = self.samples.lock().unwrap();
        let end = self.cursor.load(Ordering::Acquire).min(all.len());
        Ok(Recording {
            samples: all[start.min(end)..end].to_vec(),
            sample_rate: 16_000,
        })
    }
}

/// Counts decodes in flight so the clock can wait for the worker, the way
/// a real CPU faster than real time would have finished.
struct Counting {
    inner: Box<dyn Asr>,
    busy: Arc<AtomicUsize>,
}

impl Asr for Counting {
    fn transcribe(&mut self, samples: &[f32], rate: u32) -> Result<String> {
        self.busy.fetch_add(1, Ordering::AcqRel);
        let result = self.inner.transcribe(samples, rate);
        self.busy.fetch_sub(1, Ordering::AcqRel);
        result
    }
    fn model_label(&self) -> &str {
        self.inner.model_label()
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

struct Route {
    name: String,
    language: String,
    engine: Engine,
    clip: ClockedClip,
    busy: Arc<AtomicUsize>,
    events: Collector,
    gate: Arc<crate::noise_filter::Control>,
    _worker: crate::inference::Worker,
    _gate_base: PathBuf,
}

fn build_route(models: &Path, language: &str, model_id: &str, index: usize) -> Route {
    let spec = crate::models::MODELS
        .iter()
        .find(|m| m.id == model_id)
        .unwrap_or_else(|| panic!("unknown model {model_id}"));
    let threads = crate::models::recommended_threads(
        model_id,
        language,
        vocalcode_platform::HardwareProfile::detect(),
    ) as i32;
    let asr = crate::models::build_asr(spec, &models.join(model_id), language, threads)
        .unwrap_or_else(|e| panic!("load {model_id}: {e}"));
    let busy = Arc::new(AtomicUsize::new(0));
    let asr: Box<dyn Asr> = Box::new(Counting {
        inner: asr,
        busy: busy.clone(),
    });
    let punct = crate::models::wants_punct(model_id, language)
        .then(|| models.join("punct").join("model.onnx"));
    let cleaners = crate::models::build_cleaners(
        punct,
        crate::models::wants_cjk_space_collapse(model_id, language),
    )
    .expect("cleaners");
    let gate = Arc::new(crate::noise_filter::Control::default());
    gate.set_enabled(true); // the owner runs with the speech filter on
    gate.set_progressive(false);
    let gate_base = std::env::temp_dir().join(format!(
        "vocalcode-voice-corpus-gate-{}-{index}",
        std::process::id()
    ));
    std::fs::create_dir_all(&gate_base).expect("gate base");
    let (worker, asr, cleaners) = crate::inference::Worker::start(
        asr,
        cleaners,
        Some(Box::new(crate::noise_filter::Filter::new(
            gate_base.clone(),
            gate.clone(),
        ))),
    )
    .expect("inference worker");
    let clip = ClockedClip::default();
    let events = Collector::default();
    let mut engine = Engine::new(
        Box::new(clip.clone()),
        asr,
        Box::new(events.clone()),
        250,
        16_000,
        false,
        Arc::new(AtomicBool::new(true)),
        Arc::new(AtomicBool::new(false)),
        Arc::new(Mutex::new(crate::merge_rules(&[]))),
        cleaners,
    );
    engine.set_snippets(Arc::new(Mutex::new(vec![
        Entry {
            name: "signature".into(),
            text: "Best regards,\nDaming".into(),
        },
        Entry {
            name: "签名".into(),
            text: "此致\n敬礼".into(),
        },
    ])));
    Route {
        name: format!("{language}:{model_id}"),
        language: language.to_string(),
        engine,
        clip,
        busy,
        events,
        gate,
        _worker: worker,
        _gate_base: gate_base,
    }
}

fn read_wav(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    // Canonical 44-byte PCM16 mono 16 kHz header written by ffmpeg; find "data".
    let data = bytes
        .windows(4)
        .position(|w| w == b"data")
        .unwrap_or_else(|| panic!("{}: no data chunk", path.display()));
    assert_eq!(&bytes[22..24], &[1, 0], "{}: mono", path.display());
    assert_eq!(
        u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
        16_000,
        "{}: 16 kHz",
        path.display()
    );
    bytes[data + 8..]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect()
}

fn wait_idle(busy: &AtomicUsize) {
    let deadline = Instant::now() + Duration::from_secs(120);
    // A submission is queued before the worker starts it; give it a moment.
    std::thread::sleep(Duration::from_millis(1));
    while busy.load(Ordering::Acquire) > 0 {
        assert!(Instant::now() < deadline, "decode never finished");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// One complete dictation with the settings the app applies at utterance start.
fn dictate(
    route: &mut Route,
    samples: Vec<f32>,
    options: writing::Options,
    on: bool,
) -> (String, bool, vocalcode_core::engine::DictationTrace, u64) {
    let len = samples.len();
    *route.clip.samples.lock().unwrap() = samples;
    route.events.0.lock().unwrap().clear();
    let engine = &mut route.engine;
    assert!(engine.set_trace_enabled(true));
    assert!(engine.set_filler_removal(on, &route.language));
    assert!(engine.set_cleanup_enabled(true));
    assert!(engine.set_writing(options, &route.language));
    // Double-tap locking measures the wall-clock hold; a virtual-clock
    // utterance lasts microseconds and would be read as a tap. It has its own
    // state-machine tests and a live speaker-to-microphone check.
    engine.set_double_tap_lock(false);
    assert!(engine.set_live_caption(false));
    let id = TriggerId::synthetic(21);
    let started = Instant::now();
    engine.handle(TriggerEvent::TalkPressed(id)).expect("press");
    let mut cursor = 0;
    while cursor < len {
        cursor = (cursor + TICK_SAMPLES).min(len);
        route.clip.cursor.store(cursor, Ordering::Release);
        engine.tick_partial().expect("tick");
        wait_idle(&route.busy);
    }
    // Collect a segment that finished during the last tick, as the next
    // real tick would, then release.
    engine.tick_partial().expect("tick");
    let outcome = engine
        .handle(TriggerEvent::TalkReleased(id))
        .expect("release");
    let elapsed = started.elapsed().as_millis() as u64;
    let typed = match outcome {
        Outcome::Transcribed(text) => text,
        Outcome::Idle => String::new(),
        other => panic!("unexpected outcome {other:?}"),
    };
    let send = route.events.0.lock().unwrap().iter().any(|e| e == "enter");
    (
        typed,
        send,
        engine.take_trace().unwrap_or_default(),
        elapsed,
    )
}

#[test]
#[ignore = "needs local models and a generated voice corpus; see module docs"]
fn voice_corpus() {
    let models = PathBuf::from(std::env::var("VOCALCODE_QA_MODELS").expect("VOCALCODE_QA_MODELS"));
    let corpus =
        PathBuf::from(std::env::var("VOCALCODE_VOICE_CORPUS").expect("VOCALCODE_VOICE_CORPUS"));
    let results = PathBuf::from(
        std::env::var("VOCALCODE_VOICE_RESULTS")
            .unwrap_or_else(|_| corpus.join("results.jsonl").to_string_lossy().into_owned()),
    );
    let routes = std::env::var("VOCALCODE_VOICE_ROUTES")
        .unwrap_or_else(|_| "zh:sensevoice,en:sensevoice".into());
    let cases_path = std::env::var("VOCALCODE_VOICE_CASES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../packaging/voice-corpus/cases.json")
        });
    let cases: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cases_path).expect("cases"))
            .expect("cases json");
    let cases: HashMap<String, serde_json::Value> = cases["cases"]
        .as_array()
        .expect("cases array")
        .iter()
        .map(|c| (c["id"].as_str().unwrap().to_string(), c.clone()))
        .collect();
    let clips: Vec<serde_json::Value> = serde_json::from_str(
        &std::fs::read_to_string(
            std::env::var("VOCALCODE_VOICE_CLIPS")
                .map(PathBuf::from)
                .unwrap_or_else(|_| corpus.join("clips.json")),
        )
        .expect("clips.json"),
    )
    .expect("clips json");
    let only = std::env::var("VOCALCODE_VOICE_ONLY").ok();
    let mut out = std::io::BufWriter::new(std::fs::File::create(&results).expect("results"));
    for (index, spec) in routes.split(',').enumerate() {
        let (language, model_id) = spec.trim().split_once(':').expect("route lang:model");
        let mut route = build_route(&models, language, model_id, index);
        let mine: Vec<&serde_json::Value> = clips
            .iter()
            .filter(|c| c["language"] == language)
            .filter(|c| {
                only.as_deref()
                    .is_none_or(|o| c["case"].as_str().unwrap_or("").contains(o))
            })
            .collect();
        eprintln!("route {}: {} clips", route.name, mine.len());
        let route_started = Instant::now();
        for (n, clip) in mine.iter().enumerate() {
            let case_id = clip["case"].as_str().unwrap();
            let Some(case) = cases.get(case_id) else {
                continue;
            };
            let audio = read_wav(&corpus.join(clip["path"].as_str().unwrap()));
            for pass in ["all_on", "baseline"] {
                let on = pass == "all_on";
                let style = case["style"]
                    .as_str()
                    .and_then(writing::Style::parse)
                    .unwrap_or_default();
                let options = writing::Options {
                    commands: on,
                    backtrack: on,
                    lists: on,
                    code: on,
                    press_enter: on,
                    style: if on { style } else { writing::Style::Formal },
                };
                let (typed, send, trace, elapsed) = dictate(&mut route, audio.clone(), options, on);
                let record = serde_json::json!({
                    "route": route.name, "path": clip["path"], "case": case_id,
                    "language": language, "voice": clip["voice"], "provider": clip["provider"],
                    "variant": clip["variant"], "pass": pass, "heard": trace.raw_text,
                    "typed": typed, "send": send, "writing_edits": trace.writing_edits,
                    "filler_removed": trace.filler_removed, "asr_chunks": trace.asr_chunks,
                    "elapsed_ms": elapsed, "audio_seconds": clip["seconds"],
                    "gate": route.gate.snapshot().state,
                });
                writeln!(out, "{record}").unwrap();
            }
            if n % 50 == 0 {
                eprintln!(
                    "  {}/{} ({:.0}s)",
                    n + 1,
                    mine.len(),
                    route_started.elapsed().as_secs_f64()
                );
                out.flush().unwrap();
            }
        }
        out.flush().unwrap();
        eprintln!(
            "route {} done in {:.0}s",
            route.name,
            route_started.elapsed().as_secs_f64()
        );
    }
}
