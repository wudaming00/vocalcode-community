//! Deterministic control-loop tests: no microphone, real text field, sleep or
//! external model. Replies are explicitly released by each test.
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use vocalcode_core::traits::TriggerId;
use vocalcode_core::{
    Asr, AudioCapture, Engine, Outcome, Recording, Result, TextInjector, TriggerEvent,
    VocalCodeError,
};

type Job = (Vec<f32>, mpsc::Sender<Result<String>>);
#[derive(Default)]
struct Work {
    queued: VecDeque<Job>,
    synchronous: Vec<Vec<f32>>,
}
struct ControlledAsr(Arc<Mutex<Work>>);
impl Asr for ControlledAsr {
    fn transcribe(&mut self, samples: &[f32], _: u32) -> Result<String> {
        self.0.lock().unwrap().synchronous.push(samples.to_vec());
        Ok("Remaining words.".into())
    }
    fn transcribe_async(
        &mut self,
        samples: &[f32],
        _: u32,
    ) -> Result<Option<mpsc::Receiver<Result<String>>>> {
        let (tx, rx) = mpsc::channel();
        self.0
            .lock()
            .unwrap()
            .queued
            .push_back((samples.to_vec(), tx));
        Ok(Some(rx))
    }
    fn model_label(&self) -> &str {
        "controlled"
    }
}
struct Capture {
    audio: Arc<Mutex<Vec<f32>>>,
    running: Arc<AtomicBool>,
    snapshots: Arc<Mutex<Vec<usize>>>,
}
impl AudioCapture for Capture {
    fn start(&mut self) -> Result<()> {
        self.running.store(true, Ordering::Release);
        Ok(())
    }
    fn stop(&mut self) -> Result<Recording> {
        self.running.store(false, Ordering::Release);
        self.snapshot_since(0)
    }
    fn is_recording(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }
    fn snapshot_since(&self, start: usize) -> Result<Recording> {
        let audio = self.audio.lock().unwrap();
        self.snapshots
            .lock()
            .unwrap()
            .push(audio.len().saturating_sub(start));
        Ok(Recording {
            samples: audio[start.min(audio.len())..].to_vec(),
            sample_rate: 16000,
        })
    }
}
struct Injector {
    calls: Arc<Mutex<Vec<String>>>,
    safe: Arc<AtomicBool>,
}
impl TextInjector for Injector {
    fn begin_utterance(&self) -> Result<()> {
        self.calls.lock().unwrap().push("begin".into());
        Ok(())
    }
    fn end_utterance(&self) {
        self.calls.lock().unwrap().push("end".into());
    }
    fn inject_text(&self, text: &str) -> Result<()> {
        if !self.safe.load(Ordering::Acquire) {
            return Err(VocalCodeError::Inject("target changed".into()));
        }
        self.calls.lock().unwrap().push(format!("insert:{text}"));
        Ok(())
    }
    fn send_enter(&self) -> Result<()> {
        panic!("dictation must not submit")
    }
    fn backspace(&self, _: usize) -> Result<()> {
        panic!("progressive text must not rewrite")
    }
}
struct Harness {
    engine: Engine,
    work: Arc<Mutex<Work>>,
    audio: Arc<Mutex<Vec<f32>>>,
    running: Arc<AtomicBool>,
    calls: Arc<Mutex<Vec<String>>>,
    safe: Arc<AtomicBool>,
    gate: Arc<AtomicBool>,
    snapshots: Arc<Mutex<Vec<usize>>>,
}
fn voice(ms: usize) -> Vec<f32> {
    (0..ms * 16)
        .map(|i| if i % 2 == 0 { 0.04 } else { -0.04 })
        .collect()
}
fn paused(ms: usize) -> Vec<f32> {
    let mut audio = voice(ms);
    audio.extend(vec![0.; 4800]);
    audio
}
impl Harness {
    fn new(live: bool, samples: Vec<f32>) -> Self {
        let work = Arc::new(Mutex::new(Work::default()));
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let audio = Arc::new(Mutex::new(samples));
        let running = Arc::new(AtomicBool::new(false));
        let safe = Arc::new(AtomicBool::new(true));
        let gate = Arc::new(AtomicBool::new(true));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut engine = Engine::new(
            Box::new(Capture {
                audio: audio.clone(),
                running: running.clone(),
                snapshots: snapshots.clone(),
            }),
            Box::new(ControlledAsr(work.clone())),
            Box::new(Injector {
                calls: calls.clone(),
                safe: safe.clone(),
            }),
            250,
            16000,
            live,
            gate.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(vec![])),
            vec![],
        );
        engine.set_trace_enabled(true);
        engine
            .handle(TriggerEvent::TalkPressed(TriggerId::synthetic(1)))
            .unwrap();
        Self {
            snapshots,
            engine,
            work,
            audio,
            running,
            calls,
            safe,
            gate,
        }
    }
    fn next(&self) -> Job {
        self.work
            .lock()
            .unwrap()
            .queued
            .pop_front()
            .expect("submitted job")
    }
    fn finish(&mut self) -> Result<Outcome> {
        self.engine
            .handle(TriggerEvent::TalkReleased(TriggerId::synthetic(1)))
    }
}

#[test]
fn normal_long_dictation_prepares_once_and_only_decodes_the_unprocessed_tail_on_release() {
    let mut input = paused(8000);
    input.extend(voice(2000));
    let mut h = Harness::new(false, input.clone());
    h.engine.tick_partial().unwrap();
    let (prefix, reply) = h.next();
    assert!(prefix.len() < input.len());
    for _ in 0..20 {
        h.engine.tick_partial().unwrap();
    }
    assert!(
        h.work.lock().unwrap().queued.is_empty(),
        "one bounded in-flight request"
    );
    assert!(h.running.load(Ordering::Acquire));
    reply.send(Ok("Prepared sentence.".into())).unwrap();
    h.engine.tick_partial().unwrap();
    assert_eq!(*h.calls.lock().unwrap(), ["begin"]);
    assert_eq!(
        h.finish().unwrap(),
        Outcome::Transcribed("Prepared sentence. Remaining words.".into())
    );
    let work = h.work.lock().unwrap();
    assert_eq!(work.synchronous.len(), 1);
    assert_eq!(
        [prefix, work.synchronous[0].clone()].concat(),
        input,
        "each audio sample decoded exactly once"
    );
    assert_eq!(
        *h.calls.lock().unwrap(),
        ["begin", "insert:Prepared sentence. Remaining words.", "end"]
    );
    let trace = h.engine.take_trace().unwrap();
    assert_eq!(trace.asr_chunks, 2);
    assert_eq!(trace.sample_count, input.len());
    assert!(trace.predecoded_audio_ms >= 8000);
}

#[test]
fn progressive_result_is_append_only_and_release_does_not_repeat_it() {
    let mut h = Harness::new(true, paused(800));
    h.engine.tick_partial().unwrap();
    let (_, first) = h.next();
    assert_eq!(*h.calls.lock().unwrap(), ["begin"]);
    first.send(Ok("First sentence.".into())).unwrap();
    h.engine.tick_partial().unwrap();
    h.audio.lock().unwrap().extend(paused(800));
    h.engine.tick_partial().unwrap();
    h.next().1.send(Ok("Second sentence.".into())).unwrap();
    h.engine.tick_partial().unwrap();
    assert_eq!(
        h.finish().unwrap(),
        Outcome::Transcribed("First sentence. Second sentence.".into())
    );
    assert_eq!(
        *h.calls.lock().unwrap(),
        [
            "begin",
            "insert:First sentence.",
            "insert: Second sentence.",
            "end"
        ]
    );
    assert!(h.work.lock().unwrap().synchronous.is_empty());
}

#[test]
fn cancellation_does_not_wait_for_inference_or_reuse_its_late_result() {
    let mut h = Harness::new(true, paused(800));
    h.engine.tick_partial().unwrap();
    let (_, reply) = h.next();
    h.engine.force_cancel().unwrap();
    assert!(!h.running.load(Ordering::Acquire));
    assert!(reply.send(Ok("cancelled words".into())).is_err());
    assert!(h.engine.take_trace().is_none());
    h.engine
        .handle(TriggerEvent::TalkPressed(TriggerId::synthetic(1)))
        .unwrap();
    assert_eq!(
        h.finish().unwrap(),
        Outcome::Transcribed("Remaining words.".into())
    );
    assert!(!h
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|text| text.contains("cancelled")));
}

#[test]
fn focus_is_rechecked_after_the_async_result_and_failure_preserves_text() {
    let mut h = Harness::new(true, paused(800));
    h.engine.tick_partial().unwrap();
    h.safe.store(false, Ordering::Release);
    h.next()
        .1
        .send(Ok("Keep this recoverable.".into()))
        .unwrap();
    assert!(h.engine.tick_partial().is_err());
    assert!(!h.engine.is_recording());
    assert_eq!(*h.calls.lock().unwrap(), ["begin", "end"]);
    assert_eq!(
        h.engine.take_recoverable_text().as_deref(),
        Some("Keep this recoverable.")
    );
}

#[test]
fn gate_closing_while_decode_runs_discards_the_result_and_trace() {
    let mut h = Harness::new(true, paused(800));
    h.engine.tick_partial().unwrap();
    h.next().1.send(Ok("must not leak".into())).unwrap();
    h.gate.store(false, Ordering::Release);
    assert!(h.engine.tick_partial().is_err());
    assert!(h.engine.take_trace().is_none());
    assert!(h.engine.take_recoverable_text().is_none());
    assert_eq!(*h.calls.lock().unwrap(), ["begin", "end"]);
}

#[test]
fn optional_preparation_failure_retries_audio_on_release_without_stopping_capture() {
    let input = paused(8000);
    let mut h = Harness::new(false, input.clone());
    h.engine.tick_partial().unwrap();
    h.next()
        .1
        .send(Err(VocalCodeError::Asr("simulated failure".into())))
        .unwrap();
    h.engine.tick_partial().unwrap();
    assert!(h.engine.is_recording());
    for _ in 0..3 {
        h.engine.tick_partial().unwrap();
    }
    assert!(h.work.lock().unwrap().queued.is_empty());
    assert_eq!(
        h.finish().unwrap(),
        Outcome::Transcribed("Remaining words.".into())
    );
    assert_eq!(h.work.lock().unwrap().synchronous.as_slice(), [input]);
}

#[test]
fn watchdog_recovers_prepared_and_pending_text_without_injecting_it() {
    let mut h = Harness::new(false, paused(8000));
    h.engine.tick_partial().unwrap();
    h.next().1.send(Ok("Prepared.".into())).unwrap();
    h.engine.tick_partial().unwrap();
    h.audio.lock().unwrap().extend(paused(8000));
    h.engine.tick_partial().unwrap();
    h.next().1.send(Ok("Pending.".into())).unwrap();
    h.engine.force_stop_to_history().unwrap();
    assert_eq!(
        h.engine.take_recoverable_text().as_deref(),
        Some("Prepared. Pending.")
    );
    assert_eq!(*h.calls.lock().unwrap(), ["begin", "end"]);
}

#[test]
fn short_normal_input_and_uninterrupted_quiet_speech_are_not_split() {
    for (live, input) in [(false, paused(1000)), (true, voice(10000))] {
        let mut h = Harness::new(live, input);
        h.engine.tick_partial().unwrap();
        assert!(h.work.lock().unwrap().queued.is_empty());
        assert_eq!(
            h.finish().unwrap(),
            Outcome::Transcribed("Remaining words.".into())
        );
    }
}

#[test]
fn normal_mode_preserves_fillers_when_later_text_requires_whole_utterance_bypass() {
    let mut h = Harness::new(false, paused(8000));
    h.engine.force_cancel().unwrap();
    h.engine.set_filler_removal(true, "en");
    h.engine
        .handle(TriggerEvent::TalkPressed(TriggerId::synthetic(1)))
        .unwrap();
    h.engine.tick_partial().unwrap();
    h.next().1.send(Ok("Um, first sentence.".into())).unwrap();
    h.engine.tick_partial().unwrap();
    h.audio.lock().unwrap().extend(voice(300));
    assert_eq!(
        h.finish().unwrap(),
        Outcome::Transcribed("Um, first sentence. Remaining words.".into())
    );
    // "words" in the final chunk makes the existing conservative cleaner
    // preserve the utterance. Cleaning the first chunk early would lose Um.
    assert_eq!(h.engine.take_trace().unwrap().filler_removed, 0);
}

#[test]
fn minimum_record_duration_still_applies_to_progressive_results() {
    let mut h = Harness::new(true, paused(300));
    h.engine.force_cancel().unwrap();
    assert!(h.engine.set_min_record_ms(1000));
    h.engine
        .handle(TriggerEvent::TalkPressed(TriggerId::synthetic(1)))
        .unwrap();
    h.engine.tick_partial().unwrap();
    assert!(h.work.lock().unwrap().queued.is_empty());
    assert_eq!(h.finish().unwrap(), Outcome::Transcribed(String::new()));
}

#[test]
fn long_uninterrupted_input_scans_only_a_bounded_rolling_window() {
    let mut h = Harness::new(false, vec![]);
    for _ in 0..500 {
        h.audio.lock().unwrap().extend(voice(80));
        h.engine.tick_partial().unwrap();
    }
    assert!(h.work.lock().unwrap().queued.is_empty());
    assert!(h.snapshots.lock().unwrap().iter().all(|&len| len <= 17_280));
    h.audio.lock().unwrap().extend(vec![0.; 6400]);
    h.engine.tick_partial().unwrap();
    let (audio, reply) = h.next();
    assert_eq!(
        audio,
        *h.audio.lock().unwrap(),
        "one full copy only once a pause arrives"
    );
    reply
        .send(Ok("Forty seconds of continuous speech.".into()))
        .unwrap();
    h.engine.tick_partial().unwrap();
    assert_eq!(
        h.finish().unwrap(),
        Outcome::Transcribed("Forty seconds of continuous speech.".into())
    );
    assert!(h.work.lock().unwrap().synchronous.is_empty());
}

#[test]
fn context_policy_is_bounded_and_cannot_change_during_capture() {
    let mut h = Harness::new(true, paused(800));
    assert!(!h.engine.set_segment_minimum_ms(Some(3000)));
    assert!(!h.engine.set_segment_minimum_ms(None));
    h.engine.force_cancel().unwrap();
    for invalid in [0, 299, 30001, u32::MAX] {
        assert!(!h.engine.set_segment_minimum_ms(Some(invalid)));
    }
    assert!(h.engine.set_segment_minimum_ms(Some(3000)));
    h.engine
        .handle(TriggerEvent::TalkPressed(TriggerId::synthetic(1)))
        .unwrap();
    h.engine.tick_partial().unwrap();
    assert!(
        h.work.lock().unwrap().queued.is_empty(),
        "short pauses preserve context"
    );
    h.audio.lock().unwrap().extend(paused(3000));
    h.engine.tick_partial().unwrap();
    let (samples, reply) = h.next();
    assert_eq!(
        samples,
        *h.audio.lock().unwrap(),
        "no audio lost while waiting"
    );
    reply.send(Ok("Complete context.".into())).unwrap();
    h.engine.tick_partial().unwrap();
    assert_eq!(
        h.finish().unwrap(),
        Outcome::Transcribed("Complete context.".into())
    );
    assert!(h.engine.set_segment_minimum_ms(None));
    *h.audio.lock().unwrap() = paused(800);
    h.engine
        .handle(TriggerEvent::TalkPressed(TriggerId::synthetic(1)))
        .unwrap();
    h.engine.tick_partial().unwrap();
    assert!(
        h.work.lock().unwrap().queued.len() == 1,
        "default progressive behavior restored"
    );
}

#[test]
fn longer_context_never_creates_a_hard_cut_in_uninterrupted_speech() {
    let mut h = Harness::new(true, voice(30000));
    h.engine.force_cancel().unwrap();
    assert!(h.engine.set_segment_minimum_ms(Some(1500)));
    h.engine
        .handle(TriggerEvent::TalkPressed(TriggerId::synthetic(1)))
        .unwrap();
    h.engine.tick_partial().unwrap();
    assert!(h.work.lock().unwrap().queued.is_empty());
    h.finish().unwrap();
    assert_eq!(h.work.lock().unwrap().synchronous[0].len(), 30000 * 16);
}
