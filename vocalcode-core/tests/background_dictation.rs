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
/// `speech` heard over a fan or an office: pink noise at -46 dBFS (Paul
/// Kellet's filter over deterministic white noise), louder than the fixed
/// 0.003 RMS pause ceiling on its own.
fn in_room(mut speech: Vec<f32>, mut seed: u32) -> Vec<f32> {
    let mut b = [0_f32; 7];
    let room: Vec<f32> = (0..speech.len())
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let w = seed as f32 / u32::MAX as f32 * 2. - 1.;
            b[0] = 0.99886 * b[0] + w * 0.0555179;
            b[1] = 0.99332 * b[1] + w * 0.0750759;
            b[2] = 0.96900 * b[2] + w * 0.153852;
            b[3] = 0.86650 * b[3] + w * 0.3104856;
            b[4] = 0.55000 * b[4] + w * 0.5329522;
            b[5] = -0.7616 * b[5] - w * 0.016898;
            let out = b.iter().sum::<f32>() + w * 0.5362;
            b[6] = w * 0.115926;
            out
        })
        .collect();
    let rms = (room.iter().map(|v| v * v).sum::<f32>() / room.len() as f32).sqrt();
    let gain = 10_f32.powf(-46. / 20.) / rms;
    for (s, n) in speech.iter_mut().zip(room) {
        *s += n * gain;
    }
    speech
}
/// 500 ms of room, 8 s of speech at an ordinary microphone level (-12 dBFS),
/// a 400 ms pause, 2 s more speech.
fn noisy_long_dictation() -> Vec<f32> {
    let spoken = |ms: usize| voice(ms).into_iter().map(|v| v * 6.25);
    let mut speech = vec![0.; 8000];
    speech.extend(spoken(8000));
    speech.extend(vec![0.; 6400]);
    speech.extend(spoken(2000));
    in_room(speech, 3)
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
    /// Deliver `input` at the app's 80 ms cadence until a phrase is submitted
    /// for background decoding. Returns how much of it was captured by then.
    fn feed_until_submitted(&mut self, input: &[f32]) -> usize {
        let mut fed = 0;
        while self.work.lock().unwrap().queued.is_empty() && fed < input.len() {
            let next = (fed + 1280).min(input.len());
            self.audio
                .lock()
                .unwrap()
                .extend_from_slice(&input[fed..next]);
            fed = next;
            self.engine.tick_partial().unwrap();
        }
        fed
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
fn normal_dictation_in_a_noisy_room_is_still_prepared_during_a_pause() {
    // The room alone is above the old fixed pause ceiling, which never found
    // this pause: the whole utterance was decoded after release.
    let input = noisy_long_dictation();
    let mut h = Harness::new(false, vec![]);
    let fed = h.feed_until_submitted(&input);
    let (prefix, reply) = h.next();
    assert!(
        (8740 * 16..=8900 * 16).contains(&prefix.len()),
        "cut inside the pause, not a word: {} ms",
        prefix.len() / 16
    );
    reply.send(Ok("Prepared sentence.".into())).unwrap();
    h.audio.lock().unwrap().extend_from_slice(&input[fed..]);
    h.engine.tick_partial().unwrap();
    assert_eq!(
        h.finish().unwrap(),
        Outcome::Transcribed("Prepared sentence. Remaining words.".into())
    );
    let work = h.work.lock().unwrap();
    assert!(work.queued.is_empty(), "the last two seconds are not split");
    assert_eq!(
        [prefix, work.synchronous[0].clone()].concat(),
        input,
        "each audio sample decoded exactly once"
    );
    assert_eq!(h.engine.take_trace().unwrap().asr_chunks, 2);
}

#[test]
fn the_rest_of_a_noisy_pause_is_not_decoded_on_its_own() {
    // The last phrase is prepared during the pause before release. What the
    // room makes until the key comes up is not handed to the recognizer, which
    // would type it as "Yeah."; a word spoken there still is.
    let spoken = |ms: usize| voice(ms).into_iter().map(|v| v * 6.25);
    let mut speech = vec![0.; 8000];
    speech.extend(spoken(9000));
    speech.extend(vec![0.; 24000]);
    for (word, expected) in [
        (0, "Prepared sentence."),
        (250, "Prepared sentence. Remaining words."),
    ] {
        let mut input = speech.clone();
        input.extend(spoken(word));
        let input = in_room(input, 5);
        let mut h = Harness::new(false, vec![]);
        let fed = h.feed_until_submitted(&input);
        let (prefix, reply) = h.next();
        assert!((9740 * 16..=9900 * 16).contains(&prefix.len()));
        reply.send(Ok("Prepared sentence.".into())).unwrap();
        h.audio.lock().unwrap().extend_from_slice(&input[fed..]);
        h.engine.tick_partial().unwrap();
        assert_eq!(h.finish().unwrap(), Outcome::Transcribed(expected.into()));
        let work = h.work.lock().unwrap();
        assert!(work.queued.is_empty());
        assert_eq!(work.synchronous.len(), usize::from(word > 0));
    }
}

#[test]
fn a_gated_microphone_learned_in_one_utterance_is_forgotten_by_the_next() {
    // A microphone that turns its pauses into zeros teaches a floor of zero,
    // which would hide every pause of a later dictation in a noisy room (on
    // another microphone, say) if it carried over.
    let mut gated = voice(1000);
    gated.extend(vec![0.; 4800]);
    gated.extend(voice(1000));
    let mut h = Harness::new(false, gated);
    h.engine.tick_partial().unwrap();
    assert_eq!(
        h.finish().unwrap(),
        Outcome::Transcribed("Remaining words.".into())
    );
    h.audio.lock().unwrap().clear();
    h.engine
        .handle(TriggerEvent::TalkPressed(TriggerId::synthetic(1)))
        .unwrap();
    let input = noisy_long_dictation();
    assert!(h.feed_until_submitted(&input) < input.len());
    assert!((8740 * 16..=8900 * 16).contains(&h.next().0.len()));
}

#[test]
fn progressive_text_in_a_noisy_room_does_not_type_the_rest_of_the_pause() {
    // Each phrase is typed at its pause. What the room makes after the last
    // one until release is not decoded (a recognizer types it as "Yeah."),
    // while a soft word said there (-40 dBFS, 6 dB above the room) still is.
    let spoken = |ms: usize| voice(ms).into_iter().map(|v| v * 6.25);
    let mut speech = vec![0.; 8000];
    speech.extend(spoken(1500));
    speech.extend(vec![0.; 24000]);
    for (word, expected) in [
        (0, "First sentence."),
        (250, "First sentence. Remaining words."),
    ] {
        let mut input = speech.clone();
        input.extend(voice(word).into_iter().map(|v| v * 0.25));
        let input = in_room(input, 7);
        let mut h = Harness::new(true, vec![]);
        let fed = h.feed_until_submitted(&input);
        let (phrase, reply) = h.next();
        assert!(
            (2240 * 16..=2400 * 16).contains(&phrase.len()),
            "cut inside the pause: {} ms",
            phrase.len() / 16
        );
        reply.send(Ok("First sentence.".into())).unwrap();
        h.audio.lock().unwrap().extend_from_slice(&input[fed..]);
        h.engine.tick_partial().unwrap();
        assert_eq!(
            *h.calls.lock().unwrap(),
            ["begin", "insert:First sentence."]
        );
        assert!(
            h.work.lock().unwrap().queued.is_empty(),
            "the room is no phrase"
        );
        assert_eq!(h.finish().unwrap(), Outcome::Transcribed(expected.into()));
        let mut calls = vec!["begin", "insert:First sentence."];
        if word > 0 {
            calls.push("insert: Remaining words.");
        }
        calls.push("end");
        assert_eq!(*h.calls.lock().unwrap(), calls);
        let work = h.work.lock().unwrap();
        assert!(work.queued.is_empty());
        assert_eq!(work.synchronous.len(), usize::from(word > 0));
        if word > 0 {
            assert_eq!(
                [phrase, work.synchronous[0].clone()].concat(),
                input,
                "the word is decoded with the room before it, once"
            );
        }
    }
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
