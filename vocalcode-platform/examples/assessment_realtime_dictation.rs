//! Explicit local QA only: generated WAV -> production worker/core -> owned
//! Windows scratch input. No microphone, global hotkeys, clipboard or user data.
//! This is NOT the installed desktop app/capture/event-loop benchmark.
#[allow(dead_code)]
#[path = "../../vocalcode-app/src/inference.rs"]
mod inference;

// Only the worker's gate interface is needed; use the real platform detector.
mod noise_filter {
    pub trait Gate: Send {
        fn permit(&mut self, samples: &[f32], rate: u32) -> bool;
    }
}

#[cfg(windows)]
mod probe {
    use std::{
        env, fs,
        path::Path,
        sync::{atomic::AtomicBool, Arc, Mutex},
        thread,
        time::{Duration, Instant},
    };
    use vocalcode_core::{
        traits::TriggerId, Asr, AudioCapture, Engine, Outcome, Recording, Result, TextInjector,
        TriggerEvent, VocalCodeError,
    };
    use vocalcode_platform::{
        inject::{current_focus, FocusToken},
        speech_gate::SpeechGate,
        AcronymCollapser, EnigoInjector, Normalizer, SherpaSenseVoiceAsr, T2sCleaner,
    };
    use windows_sys::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{
            GetAncestor, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId, IsWindow,
            SendMessageW, GA_ROOT, WM_GETTEXT, WM_GETTEXTLENGTH,
        },
    };

    fn filetime_ms(time: windows_sys::Win32::Foundation::FILETIME) -> f64 {
        ((u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)) as f64 / 10_000.0
    }

    fn process_resources() -> serde_json::Value {
        use windows_sys::Win32::{
            Foundation::FILETIME,
            System::{
                ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX},
                Threading::{GetCurrentProcess, GetProcessTimes},
            },
        };
        let process = unsafe { GetCurrentProcess() };
        let mut memory = PROCESS_MEMORY_COUNTERS_EX {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        let memory_ok = unsafe {
            GetProcessMemoryInfo(
                process,
                (&mut memory as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
                memory.cb,
            )
        } != 0;
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        let cpu_ok =
            unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) }
                != 0;
        serde_json::json!({
            "working_set_bytes":memory_ok.then_some(memory.WorkingSetSize),
            "peak_working_set_bytes":memory_ok.then_some(memory.PeakWorkingSetSize),
            "private_commit_bytes":memory_ok.then_some(memory.PrivateUsage),
            "peak_private_commit_bytes":memory_ok.then_some(memory.PeakPagefileUsage),
            "cpu_ms":cpu_ok.then(||filetime_ms(kernel)+filetime_ms(user)),
            "scope":"Entire QA process since startup, including model load/warm-up and replay. CPU time sums all process threads; commit is not resident RAM. No other process is inspected."
        })
    }

    struct Replay {
        audio: Arc<Vec<f32>>,
        started: Option<Instant>,
    }
    impl AudioCapture for Replay {
        fn start(&mut self) -> Result<()> {
            self.started = Some(Instant::now());
            Ok(())
        }
        fn is_recording(&self) -> bool {
            self.started.is_some()
        }
        fn stop(&mut self) -> Result<Recording> {
            let recording = self.snapshot_since(0)?;
            self.started = None;
            Ok(recording)
        }
        fn snapshot_since(&self, from: usize) -> Result<Recording> {
            let available = self
                .started
                .map_or(0, |t| (t.elapsed().as_secs_f64() * 16000.) as usize)
                .min(self.audio.len());
            Ok(Recording {
                samples: self.audio[from.min(available)..available].to_vec(),
                sample_rate: 16000,
            })
        }
    }
    struct Gate(SpeechGate);
    impl crate::noise_filter::Gate for Gate {
        fn permit(&mut self, samples: &[f32], rate: u32) -> bool {
            self.0.classify(samples, rate).permits_asr()
        }
    }
    struct Scratch {
        root: HWND,
        edit: HWND,
        pid: u32,
        focus: FocusToken,
        inner: EnigoInjector,
        start: Instant,
        events: Arc<Mutex<Vec<serde_json::Value>>>,
    }
    impl Scratch {
        fn check(&self) -> Result<()> {
            let mut root_pid = 0;
            let mut edit_pid = 0;
            unsafe {
                GetWindowThreadProcessId(self.root, &mut root_pid);
                GetWindowThreadProcessId(self.edit, &mut edit_pid);
                if IsWindow(self.edit) == 0
                    || root_pid != self.pid
                    || edit_pid != self.pid
                    || GetAncestor(self.edit, GA_ROOT) != self.root
                    || GetForegroundWindow() != self.root
                {
                    return Err(VocalCodeError::Inject(
                        "QA scratch window lost; aborting".into(),
                    ));
                }
            }
            if current_focus().as_ref() != Some(&self.focus) {
                return Err(VocalCodeError::Inject(
                    "QA exact input focus changed; aborting".into(),
                ));
            }
            Ok(())
        }
    }
    fn field_text(edit: HWND) -> String {
        unsafe {
            let len = SendMessageW(edit, WM_GETTEXTLENGTH, 0, 0).max(0) as usize;
            let mut buf = vec![0_u16; len.min(100000) + 1];
            let read = SendMessageW(edit, WM_GETTEXT, buf.len(), buf.as_mut_ptr() as isize).max(0)
                as usize;
            String::from_utf16_lossy(&buf[..read.min(buf.len())])
        }
    }
    impl TextInjector for Scratch {
        fn begin_utterance(&self) -> Result<()> {
            self.check()?;
            self.inner.begin_utterance()
        }
        fn end_utterance(&self) {
            self.inner.end_utterance();
        }
        fn inject_text(&self, text: &str) -> Result<()> {
            self.check()?;
            let started = Instant::now();
            self.inner.inject_text(text)?;
            let elapsed = started.elapsed().as_secs_f64() * 1000.;
            self.check()?;
            self.events.lock().unwrap().push(serde_json::json!({"at_ms":self.start.elapsed().as_secs_f64()*1000.,"inject_ms":elapsed,"text":text}));
            Ok(())
        }
        fn send_enter(&self) -> Result<()> {
            Err(VocalCodeError::Inject("QA never submits".into()))
        }
        fn backspace(&self, _: usize) -> Result<()> {
            Err(VocalCodeError::Inject("QA never rewrites".into()))
        }
    }
    struct Collector {
        start: Instant,
        events: Arc<Mutex<Vec<serde_json::Value>>>,
    }
    impl TextInjector for Collector {
        fn inject_text(&self, text: &str) -> Result<()> {
            self.events.lock().unwrap().push(serde_json::json!({"at_ms":self.start.elapsed().as_secs_f64()*1000.,"inject_ms":0.,"text":text}));
            Ok(())
        }
        fn send_enter(&self) -> Result<()> {
            Err(VocalCodeError::Inject("memory QA never submits".into()))
        }
        fn backspace(&self, _: usize) -> Result<()> {
            Err(VocalCodeError::Inject("memory QA never rewrites".into()))
        }
    }
    pub fn run() -> anyhow::Result<()> {
        let args: Vec<_> = env::args().collect();
        anyhow::ensure!(
            args.len() == 9 || ((10..=16).contains(&args.len()) && args.len()%2==0 && args[9] == "--memory"),
            "model-dir wav en|zh whole|normal|progressive pid root-hwnd edit-hwnd output-json [--memory [--segment-min-ms N] [--decoder-language auto|zh|en] [--threads 1..8]]"
        );
        let memory = args.len() >= 10;
        let mut segment_minimum_ms = None;
        let mut decoder_language = None;
        let mut thread_override = None;
        for pair in args.get(10..).unwrap_or(&[]).chunks_exact(2) {
            match pair[0].as_str() {
                "--segment-min-ms" if segment_minimum_ms.is_none() => {
                    segment_minimum_ms = Some(pair[1].parse::<u32>()?)
                }
                "--decoder-language"
                    if decoder_language.is_none()
                        && ["auto", "zh", "en"].contains(&pair[1].as_str()) =>
                {
                    decoder_language = Some(pair[1].as_str())
                }
                "--threads" if thread_override.is_none() => {
                    let threads = pair[1].parse::<i32>()?;
                    anyhow::ensure!((1..=8).contains(&threads), "QA thread count must be 1..8");
                    thread_override = Some(threads);
                }
                _ => anyhow::bail!("unknown, duplicate or invalid QA option"),
            }
        }
        let decoder_language =
            decoder_language.unwrap_or(if args[3] == "zh" { "zh" } else { "auto" });
        let threads = thread_override.unwrap_or(4);
        let mode = args[4].as_str();
        anyhow::ensure!(
            ["whole", "normal", "progressive"].contains(&mode),
            "invalid mode"
        );
        anyhow::ensure!(["en", "zh"].contains(&args[3].as_str()), "invalid language");
        anyhow::ensure!(!Path::new(&args[8]).exists(), "refusing existing report");
        let root = args[6].parse::<usize>()? as HWND;
        let edit = args[7].parse::<usize>()? as HWND;
        let pid = args[5].parse()?;
        if !memory {
            let mut title = [0_u16; 128];
            let len = unsafe { GetWindowTextW(root, title.as_mut_ptr(), 128) }.max(0) as usize;
            anyhow::ensure!(
                String::from_utf16_lossy(&title[..len]) == "VocalCode synthetic dictation QA",
                "refusing non-QA window"
            );
        }
        let mut reader = hound::WavReader::open(&args[2])?;
        let spec = reader.spec();
        anyhow::ensure!(
            spec.channels == 1 && spec.sample_rate == 16000 && spec.bits_per_sample == 16,
            "expected 16 kHz mono PCM16 WAV"
        );
        let audio = Arc::new(
            reader
                .samples::<i16>()
                .map(|s| s.map(|v| v as f32 / 32768.))
                .collect::<std::result::Result<Vec<_>, _>>()?,
        );
        // Long stress replays are allowed only without native injection. A
        // scratch-window run keeps its original two-minute safety bound.
        let maximum_seconds = if memory { 600 } else { 120 };
        anyhow::ensure!(
            !audio.is_empty() && audio.len() <= 16000 * maximum_seconds,
            "audio exceeds the replay duration bound"
        );
        let path = |name: &str| {
            Path::new(&args[1])
                .join(name)
                .to_string_lossy()
                .into_owned()
        };
        let mut asr = SherpaSenseVoiceAsr::new(
            &path("model.int8.onnx"),
            &path("tokens.txt"),
            decoder_language,
            threads,
            "SenseVoice QA",
        )?;
        let _ = asr.transcribe(&audio[..audio.len().min(16000 * 3)], 16000)?;
        let gate: Option<Box<dyn crate::noise_filter::Gate>> = if mode == "progressive" {
            None
        } else {
            Some(Box::new(Gate(SpeechGate::load(
                &Path::new(&args[1])
                    .parent()
                    .unwrap()
                    .join("speech-gate/silero-v5.onnx"),
            )?)))
        };
        let (_worker, proxy, cleaners) = crate::inference::Worker::start(
            Box::new(asr),
            vec![
                Box::new(Normalizer),
                Box::new(AcronymCollapser),
                Box::new(T2sCleaner),
            ],
            gate,
        )
        .map_err(anyhow::Error::msg)?;
        let events = Arc::new(Mutex::new(Vec::new()));
        let started = Instant::now();
        let injector: Box<dyn TextInjector> = if memory {
            Box::new(Collector {
                start: started,
                events: events.clone(),
            })
        } else {
            let focus = current_focus().ok_or_else(|| anyhow::anyhow!("focus the QA input"))?;
            let scratch = Scratch {
                root,
                edit,
                pid,
                focus,
                inner: EnigoInjector::new(false),
                start: started,
                events: events.clone(),
            };
            scratch.check()?;
            anyhow::ensure!(field_text(edit).is_empty(), "scratch field must be empty");
            Box::new(scratch)
        };
        let mut engine = Engine::new(
            Box::new(Replay {
                audio: audio.clone(),
                started: None,
            }),
            proxy,
            injector,
            250,
            16000,
            mode == "progressive",
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(vec![])),
            cleaners,
        );
        engine.set_trace_enabled(true);
        anyhow::ensure!(
            engine.set_segment_minimum_ms(segment_minimum_ms),
            "invalid QA minimum context window"
        );
        engine.handle(TriggerEvent::TalkPressed(TriggerId::synthetic(9)))?;
        let play_started = Instant::now();
        let duration = Duration::from_secs_f64(audio.len() as f64 / 16000.);
        let mut max_tick_ms = 0_f64;
        while play_started.elapsed() < duration {
            thread::sleep(
                Duration::from_millis(80).min(duration.saturating_sub(play_started.elapsed())),
            );
            if !memory && unsafe { GetForegroundWindow() } != root {
                engine.force_cancel()?;
                anyhow::bail!("QA window left foreground; cancelled");
            }
            if mode != "whole" {
                let tick = Instant::now();
                engine.tick_partial()?;
                max_tick_ms = max_tick_ms.max(tick.elapsed().as_secs_f64() * 1000.);
            }
        }
        let release = Instant::now();
        let outcome = engine.handle(TriggerEvent::TalkReleased(TriggerId::synthetic(9)))?;
        let release_ms = release.elapsed().as_secs_f64() * 1000.;
        let Outcome::Transcribed(text) = outcome else {
            anyhow::bail!("unexpected outcome")
        };
        let trace = engine
            .take_trace()
            .ok_or_else(|| anyhow::anyhow!("missing trace"))?;
        // Flush queued native text messages before reading the actual edit control.
        let wait = Instant::now();
        let mut observed = if memory {
            String::new()
        } else {
            field_text(edit)
        };
        while !memory && observed != text && wait.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(10));
            observed = field_text(edit);
        }
        let events = events.lock().unwrap();
        let joined: String = events.iter().filter_map(|v| v["text"].as_str()).collect();
        let field_matches = (!memory).then_some(observed == text);
        let result = serde_json::json!({"mode":mode,"segment_minimum_ms":segment_minimum_ms,"language":args[3],"decoder_language":decoder_language,"threads":threads,"audio_seconds":audio.len() as f64/16000.,"release_to_result_ms":release_ms,"max_tick_ms":max_tick_ms,"native_injection":!memory,"actual_input_matches":field_matches,"append_events_match":joined==text,"trace":trace,"inserts":*events,"observed":observed,"process_resources":process_resources(),"scope":"Real-time WAV replay; production core, inference worker, SenseVoice. Memory mode replaces native injector; never installed microphone/hotkey loop."});
        fs::write(&args[8], serde_json::to_vec_pretty(&result)?)?;
        println!(
            "{}",
            serde_json::json!({"mode":mode,"audio_seconds":result["audio_seconds"],"release_to_result_ms":release_ms,"asr_chunks":trace.asr_chunks,"inserts":events.len(),"actual_input_matches":field_matches,"append_events_match":joined==text})
        );
        anyhow::ensure!(
            (memory || observed == text) && joined == text,
            "native field did not exactly match the engine result"
        );
        Ok(())
    }

    #[cfg(test)]
    mod resource_tests {
        use super::*;
        use windows_sys::Win32::Foundation::FILETIME;

        #[test]
        fn cpu_time_preserves_the_high_word_and_uses_100_ns_units() {
            assert_eq!(
                filetime_ms(FILETIME {
                    dwLowDateTime: 10_000_000,
                    dwHighDateTime: 0
                }),
                1000.0
            );
            assert_eq!(
                filetime_ms(FILETIME {
                    dwLowDateTime: 0,
                    dwHighDateTime: 1
                }),
                429_496.7296
            );
        }

        #[test]
        fn own_process_resources_are_available_and_peak_covers_current() {
            let values = process_resources();
            for (peak, current) in [
                ("peak_working_set_bytes", "working_set_bytes"),
                ("peak_private_commit_bytes", "private_commit_bytes"),
            ] {
                let current = values[current].as_u64().expect("current memory");
                assert!(current > 0);
                assert!(values[peak].as_u64().expect("peak memory") >= current);
            }
            assert!(values["cpu_ms"].as_f64().is_some_and(|value| value >= 0.0));
        }
    }
}
fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        probe::run()
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("Windows scratch input probe only")
    }
}
