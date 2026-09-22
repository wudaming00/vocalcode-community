//! One native model owner, separate from the input/capture event loop. The
//! decoder is not preemptible, but it must never prevent starting a microphone.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use vocalcode_core::{Asr, Result, TextCleaner, VocalCodeError};

type Reply = mpsc::SyncSender<Result<String>>;
type WorkerParts = (Worker, Box<dyn Asr>, Vec<Box<dyn TextCleaner>>);
pub(crate) type PendingMeeting = (
    mpsc::Receiver<Result<String>>,
    mpsc::SyncSender<std::result::Result<String, String>>,
);
enum Job {
    Decode(Vec<f32>, u32, Reply),
    Clean(String, Reply),
    Meeting(Vec<f32>, u32, Reply),
}

pub(crate) struct Worker {
    sender: mpsc::SyncSender<Job>,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

struct SpeechProxy {
    sender: mpsc::SyncSender<Job>,
    label: String,
}
struct CleanerProxy {
    sender: mpsc::SyncSender<Job>,
}

fn unavailable() -> VocalCodeError {
    VocalCodeError::Asr("Inference worker is busy or unavailable; no text was inserted.".into())
}
fn reply_channel() -> (Reply, mpsc::Receiver<Result<String>>) {
    mpsc::sync_channel(1)
}
fn clean(cleaners: &mut [Box<dyn TextCleaner>], text: String) -> String {
    if text.is_empty() {
        return text;
    }
    let mut text = text;
    for cleaner in cleaners {
        match cleaner.clean(&text) {
            Ok(value) => text = value,
            Err(error) => log::warn!("Local cleaner failed; preserving previous text: {error}"),
        }
    }
    text
}

impl Worker {
    pub(crate) fn start(
        mut asr: Box<dyn Asr>,
        mut cleaners: Vec<Box<dyn TextCleaner>>,
        mut noise_filter: Option<Box<dyn crate::noise_filter::Gate>>,
    ) -> std::result::Result<WorkerParts, String> {
        // At most one meeting is submitted by the event loop. A dictation can
        // queue behind it; the rest of the meeting backlog stays on disk.
        let (sender, receiver) = mpsc::sync_channel(2);
        let label = asr.model_label().to_string();
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = stopped.clone();
        let thread = thread::Builder::new()
            .name("vocalcode-inference".into())
            .spawn(move || {
                while !worker_stopped.load(Ordering::Acquire) {
                    let job = match receiver.recv_timeout(Duration::from_millis(20)) {
                        Ok(job) => job,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    match job {
                        Job::Decode(samples, rate, reply) => {
                            let allowed = noise_filter
                                .as_mut()
                                .is_none_or(|filter| filter.permit(&samples, rate));
                            let _ = reply.send(if allowed {
                                asr.transcribe(&samples, rate)
                            } else {
                                Ok(String::new())
                            });
                        }
                        Job::Clean(text, reply) => {
                            let _ = reply.send(Ok(clean(&mut cleaners, text)));
                        }
                        Job::Meeting(samples, rate, reply) => {
                            let result = asr
                                .transcribe(&samples, rate)
                                .map(|text| clean(&mut cleaners, text.trim().to_string()));
                            let _ = reply.send(result);
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok((
            Self {
                sender: sender.clone(),
                stopped,
                thread: Some(thread),
            },
            Box::new(SpeechProxy {
                sender: sender.clone(),
                label,
            }),
            vec![Box::new(CleanerProxy { sender })],
        ))
    }

    pub(crate) fn meeting(
        &self,
        samples: Vec<f32>,
        rate: u32,
    ) -> Result<mpsc::Receiver<Result<String>>> {
        if rate == 0 || samples.is_empty() || samples.len() > rate as usize * 60 {
            return Err(VocalCodeError::Audio(
                "Invalid or oversized meeting segment".into(),
            ));
        }
        let (reply, result) = reply_channel();
        self.sender
            .try_send(Job::Meeting(samples, rate, reply))
            .map_err(|_| unavailable())?;
        Ok(result)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        // No detached model/thread, including on configuration swaps. A native
        // decode already running must finish; queued jobs are dropped safely.
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                log::error!("Inference worker panicked");
            }
        }
    }
}
impl Asr for SpeechProxy {
    fn transcribe(&mut self, samples: &[f32], rate: u32) -> Result<String> {
        let (reply, result) = reply_channel();
        self.sender
            .try_send(Job::Decode(samples.to_vec(), rate, reply))
            .map_err(|_| unavailable())?;
        result.recv().map_err(|_| unavailable())?
    }
    fn model_label(&self) -> &str {
        &self.label
    }
}
impl TextCleaner for CleanerProxy {
    fn clean(&mut self, text: &str) -> Result<String> {
        let (reply, result) = reply_channel();
        self.sender
            .try_send(Job::Clean(text.to_string(), reply))
            .map_err(|_| unavailable())?;
        result.recv().map_err(|_| unavailable())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use vocalcode_core::{AudioCapture, Engine, Recording, TextInjector, TriggerEvent};
    struct DecisionGate(bool);
    impl crate::noise_filter::Gate for DecisionGate {
        fn permit(&mut self, _: &[f32], _: u32) -> bool {
            self.0
        }
    }
    struct ExactAsr;
    impl Asr for ExactAsr {
        fn transcribe(&mut self, samples: &[f32], rate: u32) -> Result<String> {
            assert_eq!(rate, 16000);
            assert_eq!(samples, &[0.125, -0.25, 0.0]);
            Ok("好 yes はい 네 हाँ".into())
        }
        fn model_label(&self) -> &str {
            "exact-waveform"
        }
    }
    #[test]
    fn speech_gate_never_trims_changes_language_or_filters_meetings() {
        for allowed in [false, true] {
            let (worker, mut proxy, _) = Worker::start(
                Box::new(ExactAsr),
                vec![],
                Some(Box::new(DecisionGate(allowed))),
            )
            .unwrap();
            // Incorrect or trimmed input would panic in the fake ASR. Rejected
            // input must never reach it, even if ASR would hallucinate words.
            let input = if allowed {
                vec![0.125, -0.25, 0.]
            } else {
                vec![0.; 16000]
            };
            assert_eq!(
                proxy.transcribe(&input, 16000).unwrap(),
                if allowed {
                    "好 yes はい 네 हाँ"
                } else {
                    ""
                }
            );
            let meeting = worker.meeting(vec![0.125, -0.25, 0.], 16000).unwrap();
            assert_eq!(
                meeting
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap(),
                "好 yes はい 네 हाँ"
            );
        }
    }
    struct BlockingAsr {
        entered: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    }
    impl Asr for BlockingAsr {
        fn transcribe(&mut self, _: &[f32], _: u32) -> Result<String> {
            self.entered.send(()).unwrap();
            self.release.recv().unwrap();
            Ok("decoded".into())
        }
        fn model_label(&self) -> &str {
            "blocked test model"
        }
    }
    struct Capture(Arc<AtomicBool>);
    impl AudioCapture for Capture {
        fn start(&mut self) -> Result<()> {
            self.0.store(true, Ordering::Release);
            Ok(())
        }
        fn stop(&mut self) -> Result<Recording> {
            self.0.store(false, Ordering::Release);
            Ok(Recording {
                samples: vec![0.1; 3200],
                sample_rate: 16000,
            })
        }
        fn is_recording(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
    }
    struct Injector;
    impl TextInjector for Injector {
        fn inject_text(&self, _: &str) -> Result<()> {
            panic!("meeting cannot inject")
        }
        fn send_enter(&self) -> Result<()> {
            panic!("meeting cannot send")
        }
        fn backspace(&self, _: usize) -> Result<()> {
            panic!("meeting cannot edit")
        }
    }
    #[test]
    fn a_blocked_meeting_decoder_does_not_block_talk_pressed() {
        let (entered, entry) = mpsc::sync_channel(1);
        let (release, wait) = mpsc::sync_channel(1);
        let (worker, asr, cleaners) = Worker::start(
            Box::new(BlockingAsr {
                entered,
                release: wait,
            }),
            vec![],
            None,
        )
        .unwrap();
        let started = Arc::new(AtomicBool::new(false));
        let mut engine = Engine::new(
            Box::new(Capture(started.clone())),
            asr,
            Box::new(Injector),
            100,
            16000,
            false,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(vec![])),
            cleaners,
        );
        let result = worker.meeting(vec![0.1; 3200], 16000).unwrap();
        entry.recv_timeout(Duration::from_secs(2)).unwrap();
        engine
            .handle(TriggerEvent::TalkPressed(
                vocalcode_core::traits::TriggerId::synthetic(1),
            ))
            .unwrap();
        assert!(started.load(Ordering::Acquire));
        assert!(matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)));
        release.send(()).unwrap();
        assert_eq!(
            result
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap(),
            "decoded"
        );
        engine.force_cancel().unwrap();
    }
    #[test]
    fn dropping_worker_disconnects_proxies_without_hanging() {
        struct Fake;
        impl Asr for Fake {
            fn transcribe(&mut self, _: &[f32], _: u32) -> Result<String> {
                Ok("ok".into())
            }
            fn model_label(&self) -> &str {
                "fake"
            }
        }
        let (worker, mut asr, mut cleaners) = Worker::start(Box::new(Fake), vec![], None).unwrap();
        assert_eq!(asr.transcribe(&[0.1; 3200], 16000).unwrap(), "ok");
        drop(worker);
        assert!(asr.transcribe(&[0.1; 3200], 16000).is_err());
        assert!(cleaners[0].clean("text").is_err());
    }
}
