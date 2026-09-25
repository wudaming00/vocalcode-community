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
//! VOCALCODE_VOICE_ONLY=<optional comma-separated case-id substrings>
//! cargo test --release -p vocalcode-app voice_corpus -- --ignored --nocapture
//! ```
//! A route runs with the speech filter on, as the owner does; `+gate-off`
//! (`en:sensevoice+gate-off`) runs it with the filter off, the shipped
//! default. No-speech clips are replayed in both gate states on every route,
//! so "nothing is typed" never rests on an opt-in filter.
//! Scoring: packaging/voice-corpus/score.py. To measure a text-pipeline change
//! without models or audio, `voice_corpus_replay` re-runs the recognitions a
//! results file recorded.

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
    gate_on: bool,
    engine: Engine,
    clip: ClockedClip,
    busy: Arc<AtomicUsize>,
    events: Collector,
    gate: Arc<crate::noise_filter::Control>,
    _worker: crate::inference::Worker,
    _gate_base: PathBuf,
}

/// One `VOCALCODE_VOICE_ROUTES` entry: `lang:model`, optionally `+gate-off`.
#[derive(Debug, PartialEq)]
struct RouteSpec<'a> {
    language: &'a str,
    model: &'a str,
    gate: bool,
}

fn parse_route(spec: &str) -> RouteSpec<'_> {
    let spec = spec.trim();
    let (spec, gate) = match spec.strip_suffix("+gate-off") {
        Some(rest) => (rest, false),
        None => (spec, true),
    };
    let (language, model) = spec.split_once(':').expect("route lang:model[+gate-off]");
    RouteSpec {
        language,
        model,
        gate,
    }
}

/// Does a route in `language` replay this clip? No-speech clips are recorded
/// with language "any" and belong to every route. `only` narrows the run to
/// case ids containing any of its comma-separated parts; with no parts, it
/// narrows nothing.
fn replays(clip: &serde_json::Value, language: &str, only: Option<&str>) -> bool {
    let case = clip["case"].as_str().unwrap_or("");
    (clip["language"] == language || clip["language"] == "any")
        && only.is_none_or(|only| {
            let mut parts = only.split(',').filter(|part| !part.is_empty()).peekable();
            parts.peek().is_none() || parts.any(|part| case.contains(part))
        })
}

/// The replays of one clip: (pass name, writing rules on, speech gate on).
/// Every clip runs all rules on and all rules off with the route's gate; a
/// no-speech clip also runs both with the gate flipped, named for that state.
fn passes(no_speech: bool, route_gate: bool) -> Vec<(String, bool, bool)> {
    let mut passes = vec![
        ("all_on".to_string(), true, route_gate),
        ("baseline".to_string(), false, route_gate),
    ];
    if no_speech {
        let flipped = if route_gate { "gate-off" } else { "gate-on" };
        passes.push((format!("all_on+{flipped}"), true, !route_gate));
        passes.push((format!("baseline+{flipped}"), false, !route_gate));
    }
    passes
}

fn build_route(models: &Path, spec: &RouteSpec, index: usize) -> Route {
    let RouteSpec {
        language,
        model: model_id,
        gate: gate_on,
    } = *spec;
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
    gate.set_enabled(gate_on); // the owner runs with the speech filter on
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
    engine.set_snippets(corpus_snippets());
    Route {
        name: format!(
            "{language}:{model_id}{}",
            if gate_on { "" } else { "+gate-off" }
        ),
        language: language.to_string(),
        gate_on,
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

/// The settings the app applies at utterance start.
fn configure(engine: &mut Engine, language: &str, options: writing::Options, on: bool) {
    assert!(engine.set_trace_enabled(true));
    assert!(engine.set_filler_removal(on, language));
    assert!(engine.set_cleanup_enabled(true));
    assert!(engine.set_writing(options, language));
    // Double-tap locking measures the wall-clock hold; a virtual-clock
    // utterance lasts microseconds and would be read as a tap. It has its own
    // state-machine tests and a live speaker-to-microphone check.
    engine.set_double_tap_lock(false);
    assert!(engine.set_live_caption(false));
}

/// Writing options for one pass of a case: "all_on" enables every rule with
/// the case's style, "baseline" turns every rule off. A "+gate-…" suffix
/// only changes the speech gate.
fn pass_options(case: &serde_json::Value, pass: &str) -> (writing::Options, bool) {
    let on = pass.split('+').next() == Some("all_on");
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
    (options, on)
}

fn corpus_snippets() -> Arc<Mutex<Vec<Entry>>> {
    Arc::new(Mutex::new(vec![
        Entry {
            name: "signature".into(),
            text: "Best regards,\nDaming".into(),
        },
        Entry {
            name: "签名".into(),
            text: "此致\n敬礼".into(),
        },
    ]))
}

fn load_cases() -> HashMap<String, serde_json::Value> {
    let cases_path = std::env::var("VOCALCODE_VOICE_CASES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../packaging/voice-corpus/cases.json")
        });
    let cases: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cases_path).expect("cases"))
            .expect("cases json");
    cases["cases"]
        .as_array()
        .expect("cases array")
        .iter()
        .map(|c| (c["id"].as_str().unwrap().to_string(), c.clone()))
        .collect()
}

/// One complete dictation with the settings the app applies at utterance start.
fn dictate(
    route: &mut Route,
    samples: Vec<f32>,
    options: writing::Options,
    on: bool,
    gate: bool,
) -> (String, bool, vocalcode_core::engine::DictationTrace, u64) {
    let len = samples.len();
    *route.clip.samples.lock().unwrap() = samples;
    route.events.0.lock().unwrap().clear();
    route.gate.set_enabled(gate);
    let engine = &mut route.engine;
    configure(engine, &route.language, options, on);
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
    let cases = load_cases();
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
        let spec = parse_route(spec);
        let mut route = build_route(&models, &spec, index);
        let mine: Vec<&serde_json::Value> = clips
            .iter()
            .filter(|c| replays(c, spec.language, only.as_deref()))
            .collect();
        eprintln!("route {}: {} clips", route.name, mine.len());
        let route_started = Instant::now();
        for (n, clip) in mine.iter().enumerate() {
            let case_id = clip["case"].as_str().unwrap();
            let Some(case) = cases.get(case_id) else {
                continue;
            };
            let audio = read_wav(&corpus.join(clip["path"].as_str().unwrap()));
            let no_speech = case["feature"] == "no_speech";
            for (pass, on, gate) in passes(no_speech, route.gate_on) {
                let (options, _) = pass_options(case, &pass);
                let (typed, send, trace, elapsed) =
                    dictate(&mut route, audio.clone(), options, on, gate);
                let record = serde_json::json!({
                    "route": route.name, "path": clip["path"], "case": case_id,
                    "language": spec.language, "voice": clip["voice"], "provider": clip["provider"],
                    "variant": clip["variant"], "pass": pass, "heard": trace.raw_text,
                    "typed": typed, "send": send, "writing_edits": trace.writing_edits,
                    "filler_removed": trace.filler_removed, "asr_chunks": trace.asr_chunks,
                    "predecoded_audio_ms": trace.predecoded_audio_ms,
                    "finish_ms": trace.finish_ms,
                    "elapsed_ms": elapsed, "audio_seconds": clip["seconds"],
                    "speech_gate": if gate { "on" } else { "off" },
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

/// Stands in for the recogniser: answers every decode with the transcript a
/// recorded run heard.
struct Recorded(Arc<Mutex<String>>);

impl Asr for Recorded {
    fn transcribe(&mut self, _: &[f32], _: u32) -> Result<String> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn model_label(&self) -> &str {
        "recorded transcript"
    }
}

struct ReplayRoute {
    engine: Engine,
    heard: Arc<Mutex<String>>,
    clip: ClockedClip,
    events: Collector,
}

fn replay_route(language: &str, model_id: &str) -> ReplayRoute {
    let punct = crate::models::wants_punct(model_id, language).then(|| {
        PathBuf::from(std::env::var("VOCALCODE_QA_MODELS").expect("VOCALCODE_QA_MODELS"))
            .join("punct")
            .join("model.onnx")
    });
    let cleaners = crate::models::build_cleaners(
        punct,
        crate::models::wants_cjk_space_collapse(model_id, language),
    )
    .expect("cleaners");
    let heard = Arc::new(Mutex::new(String::new()));
    let clip = ClockedClip::default();
    *clip.samples.lock().unwrap() = vec![0.0; 16_000];
    let events = Collector::default();
    let mut engine = Engine::new(
        Box::new(clip.clone()),
        Box::new(Recorded(heard.clone())),
        Box::new(events.clone()),
        250,
        16_000,
        false,
        Arc::new(AtomicBool::new(true)),
        Arc::new(AtomicBool::new(false)),
        Arc::new(Mutex::new(crate::merge_rules(&[]))),
        cleaners,
    );
    engine.set_snippets(corpus_snippets());
    ReplayRoute {
        engine,
        heard,
        clip,
        events,
    }
}

/// Replays the recognitions recorded in earlier voice-corpus results through
/// the current cleaner chain, dictionary, snippets and Writing rules: no models
/// or audio needed, and the recogniser's output is held fixed. Diffing the
/// replays of two commits shows exactly what a text-pipeline change does to
/// every output the corpus has produced.
///
/// ```text
/// VOCALCODE_VOICE_REPLAY=<results.jsonl>[,<results.jsonl>...]
/// VOCALCODE_VOICE_RESULTS=<replayed results.jsonl to write>
/// VOCALCODE_QA_MODELS=<only needed for a route that uses the Chinese punctuator>
/// cargo test -p vocalcode-app voice_corpus_replay -- --ignored --nocapture
/// ```
#[test]
#[ignore = "needs recorded voice-corpus results; see the doc comment"]
fn voice_corpus_replay() {
    let inputs = std::env::var("VOCALCODE_VOICE_REPLAY").expect("VOCALCODE_VOICE_REPLAY");
    let results =
        PathBuf::from(std::env::var("VOCALCODE_VOICE_RESULTS").expect("VOCALCODE_VOICE_RESULTS"));
    let cases = load_cases();
    let mut routes: HashMap<String, ReplayRoute> = HashMap::new();
    let mut out = std::io::BufWriter::new(std::fs::File::create(&results).expect("results"));
    let (mut total, mut differ) = (0usize, 0usize);
    for input in inputs.split(',') {
        let text = std::fs::read_to_string(input.trim()).expect("recorded results");
        for line in text.lines() {
            // A run still being written ends with a partial line.
            let Ok(mut record) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(case) = record["case"].as_str().and_then(|id| cases.get(id)) else {
                continue;
            };
            let name = record["route"].as_str().expect("route").to_string();
            let (language, model_id) = name.split_once(':').expect("route lang:model");
            let route = routes
                .entry(name.clone())
                .or_insert_with(|| replay_route(language, model_id));
            // The trace joins decoded chunks with newlines; the engine joins
            // the same chunks as phrases.
            let mut heard = String::new();
            for chunk in record["heard"].as_str().unwrap_or("").split('\n') {
                let chunk = chunk.trim();
                heard.push_str(vocalcode_core::segmentation::join_separator(&heard, chunk));
                heard.push_str(chunk);
            }
            *route.heard.lock().unwrap() = heard;
            route.events.0.lock().unwrap().clear();
            let (options, on) = pass_options(case, record["pass"].as_str().unwrap_or(""));
            configure(&mut route.engine, language, options, on);
            let id = TriggerId::synthetic(21);
            route
                .engine
                .handle(TriggerEvent::TalkPressed(id))
                .expect("press");
            // Capture restarts at zero on press; the whole clip has "arrived"
            // by release, which decodes it once.
            route.clip.cursor.store(16_000, Ordering::Release);
            let typed = match route
                .engine
                .handle(TriggerEvent::TalkReleased(id))
                .expect("release")
            {
                Outcome::Transcribed(text) => text,
                Outcome::Idle => String::new(),
                other => panic!("unexpected outcome {other:?}"),
            };
            let trace = route.engine.take_trace().unwrap_or_default();
            let send = route.events.0.lock().unwrap().iter().any(|e| e == "enter");
            total += 1;
            differ += usize::from(record["typed"].as_str() != Some(typed.as_str()));
            record["typed"] = typed.into();
            record["send"] = send.into();
            record["writing_edits"] = trace.writing_edits.into();
            record["filler_removed"] = trace.filler_removed.into();
            writeln!(out, "{record}").unwrap();
        }
    }
    out.flush().unwrap();
    eprintln!("replayed {total} records; {differ} typed differently from the recording");
}

#[test]
fn route_specs_clip_selection_and_gate_passes() {
    assert_eq!(
        parse_route(" en:sensevoice "),
        RouteSpec {
            language: "en",
            model: "sensevoice",
            gate: true
        }
    );
    assert_eq!(
        parse_route("zh:sensevoice+gate-off"),
        RouteSpec {
            language: "zh",
            model: "sensevoice",
            gate: false
        }
    );

    let clip = |case: &str, language: &str| serde_json::json!({"case": case, "language": language});
    assert!(replays(&clip("zh-cs-api", "zh"), "zh", None));
    assert!(!replays(&clip("zh-cs-api", "zh"), "en", None));
    // No-speech clips belong to every route, whatever its language.
    assert!(replays(&clip("ns-fan-far", "any"), "en", None));
    assert!(replays(&clip("ns-fan-far", "any"), "zh", None));
    assert!(replays(&clip("ns-fan-far", "any"), "zh", Some("-cs-,ns-")));
    assert!(replays(&clip("zh-cs-api", "zh"), "zh", Some("-cs-,ns-")));
    assert!(!replays(&clip("zh-long-90", "zh"), "zh", Some("-cs-,ns-")));
    assert!(replays(&clip("zh-long-90", "zh"), "zh", Some("")));
    assert!(replays(&clip("zh-long-90", "zh"), "zh", Some(",")));

    let names = |list: Vec<(String, bool, bool)>| {
        list.into_iter()
            .map(|(name, rules, gate)| format!("{name}:{rules}:{gate}"))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(passes(false, true)),
        ["all_on:true:true", "baseline:false:true"]
    );
    assert_eq!(
        names(passes(true, true)),
        [
            "all_on:true:true",
            "baseline:false:true",
            "all_on+gate-off:true:false",
            "baseline+gate-off:false:false"
        ]
    );
    assert_eq!(
        names(passes(true, false)),
        [
            "all_on:true:false",
            "baseline:false:false",
            "all_on+gate-on:true:true",
            "baseline+gate-on:false:true"
        ]
    );
}
