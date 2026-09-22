//! Explicit, offline synthetic dialogue replay. No hardware, network, user data,
//! or installed-app state. Keep all inputs/results for diagnosis, even on failure.
use super::*;
use vocalcode_core::Asr;

struct Clip {
    spec: Value,
    source: AudioSource,
    start_ms: u64,
    samples: Vec<f32>,
    diagnostic: Value,
}

fn decode(path: &Path) -> Vec<f32> {
    let mut resampler = LinearMonoResampler::default();
    let mut samples = Vec::new();
    decode_audio_file(path, |block| {
        samples.extend(resampler.push(&block)?);
        Ok(())
    })
    .unwrap();
    samples.extend(resampler.finish());
    samples
}

fn normalized(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn write_report(path: &Path, value: &Value) {
    std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

#[test]
#[ignore = "requires synthetic fixtures and retained output of the AEC double-talk probe"]
fn synthetic_double_talk_asr_probe() {
    let input = PathBuf::from(std::env::var_os("VOCALCODE_QA_DIALOGUE_DIR").unwrap());
    let models = PathBuf::from(std::env::var_os("VOCALCODE_QA_MODEL_DIR").unwrap());
    let near = decode(&input.join("zh_negation.wav"));
    let render = decode(&input.join("en_correction.wav"));
    let cleaned_path = std::env::var_os("VOCALCODE_QA_AEC_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| input.join("aec-double-talk-cleaned.wav"));
    let cleaned = decode(&cleaned_path);
    let mut mixed: Vec<_> = near[..cleaned.len()].iter().map(|s| s * 0.85).collect();
    for (delay, gain) in [(1120, 0.42), (1424, 0.14)] {
        for i in delay..mixed.len() {
            mixed[i] += render[i - delay] * gain;
        }
    }
    mixed.iter_mut().for_each(|s| *s = s.clamp(-1.0, 1.0));
    let mut model = vocalcode_platform::SherpaSenseVoiceAsr::new(
        &models.join("model.int8.onnx").to_string_lossy(),
        &models.join("tokens.txt").to_string_lossy(),
        "zh",
        4,
        "synthetic overlap QA",
    )
    .unwrap();
    let mut results = Vec::new();
    for (label, samples) in [
        ("near_only", &near[..cleaned.len()]),
        ("mixed_before_aec", mixed.as_slice()),
        ("cleaned_after_aec", cleaned.as_slice()),
    ] {
        let text = model.transcribe(samples, 16_000).unwrap();
        println!("DOUBLE_TALK {label}: {text}");
        results.push(json!({"label": label, "text": text}));
    }
    let path = input.join(format!("double-talk-asr-{}.json", now_ms()));
    write_report(
        &path,
        &json!({"synthetic_only":true,"processed_audio":cleaned_path,"observations":results}),
    );
    println!("DOUBLE_TALK_REPORT {}", path.display());
    let actual = normalized(results[2]["text"].as_str().unwrap());
    // The old suppressor deleted this substantive phrase, including budget
    // context. Protect it explicitly; do not mistake nonempty output for quality.
    assert!(
        actual.contains("还没有批准这项预算"),
        "critical phrase lost: {actual}"
    );
    assert!(actual.contains("不要"), "negation lost: {actual}");
    // This is not a claim that every overlapping word is recognized correctly.
}

fn trace_drafts(drafts: &[Draft], trace: &mut Vec<Value>) {
    trace.extend(drafts.iter().map(|d| {
        json!({"source":d.source,"start_ms":d.segment.start_ms,
            "end_ms":d.segment.end_ms,"samples":d.segment.samples.len()})
    }));
}

#[test]
#[ignore = "explicit synthetic short-clip diagnostic"]
fn synthetic_short_clip_probe() {
    let input = PathBuf::from(std::env::var_os("VOCALCODE_QA_DIALOGUE_DIR").unwrap());
    let models = PathBuf::from(std::env::var_os("VOCALCODE_QA_MODEL_DIR").unwrap());
    let mut model = vocalcode_platform::SherpaSenseVoiceAsr::new(
        &models.join("model.int8.onnx").to_string_lossy(),
        &models.join("tokens.txt").to_string_lossy(),
        "zh",
        4,
        "short QA",
    )
    .unwrap();
    let root = input.join(format!("short-probe-{}", now_ms()));
    std::fs::create_dir(&root).unwrap();
    let mut gate = crate::noise_filter::meeting_detector(&root).unwrap();
    let mut results = Vec::new();
    for id in ["short_no", "short_ok", "short_bu", "short_shi"] {
        let mut audio = vec![0.0; 16_000];
        audio.extend(decode(&input.join(format!("{id}.wav"))));
        audio.extend(vec![0.0; 16_000]);
        let mut segmenter = SpeechSegmenter::default();
        let mut segments = segmenter.push(&audio);
        segments.extend(segmenter.finish());
        for segment in segments {
            for padding in [0, 200, 400, 800] {
                let mut samples = segment.samples.clone();
                samples.extend(vec![0.0; padding * 16]);
                let text = model.transcribe(&samples, 16_000).unwrap();
                let decision = gate.classify_meeting(&segment.samples, 16_000);
                println!(
                    "SHORT {id} padding={padding} len={} gate={decision:?}: {text}",
                    segment.samples.len() / 16
                );
                results.push(json!({"id":id,"padding_ms":padding,"gate":format!("{decision:?}"),"text":text}));
            }
        }
    }
    write_report(&root.join("report.json"), &json!(results));
}

#[test]
#[ignore = "requires explicit synthetic dialogue directory and downloaded local SenseVoice model"]
fn synthetic_dialogue_real_meeting_replay() {
    let input = PathBuf::from(
        std::env::var_os("VOCALCODE_QA_DIALOGUE_DIR").expect("explicit synthetic input directory"),
    );
    let models = PathBuf::from(
        std::env::var_os("VOCALCODE_QA_MODEL_DIR").expect("explicit local model directory"),
    );
    let plan: Value =
        serde_json::from_slice(&std::fs::read(input.join("fixture.json")).unwrap()).unwrap();
    let root = input.join(format!("replay-{}", now_ms()));
    std::fs::create_dir(&root).unwrap(); // Refuse to overwrite prior evidence.
    let mut detector = crate::noise_filter::meeting_detector(&root).expect("bundled local VAD");
    let mut model = vocalcode_platform::SherpaSenseVoiceAsr::new(
        &models.join("model.int8.onnx").to_string_lossy(),
        &models.join("tokens.txt").to_string_lossy(),
        "zh", // Match this user's current meeting configuration, including English turns.
        4,
        "synthetic meeting QA",
    )
    .unwrap();
    let mut clips = Vec::new();
    for spec in plan["utterances"].as_array().unwrap() {
        let id = spec["id"].as_str().unwrap();
        assert!(id.bytes().all(|c| c.is_ascii_lowercase() || c == b'_'));
        let gain = spec["gain"].as_f64().unwrap() as f32;
        let samples: Vec<_> = decode(&input.join(format!("{id}.wav")))
            .into_iter()
            .map(|s| s * gain)
            .collect();
        let direct_started = Instant::now();
        let direct = model.transcribe(&samples, 16_000).unwrap();
        let direct_ms = direct_started.elapsed().as_millis();
        let mut segmenter = SpeechSegmenter::default();
        // Match a clip preceded and followed by silence, not an already-cropped file.
        let mut isolated = vec![0.0; 16_000];
        isolated.extend_from_slice(&samples);
        isolated.extend(vec![0.0; 16_000]);
        let mut candidates = segmenter.push(&isolated);
        candidates.extend(segmenter.finish());
        let candidate_diagnostics: Vec<_> = candidates
            .iter()
            .map(|s| {
                json!({"start_ms":s.start_ms,"end_ms":s.end_ms,
                "gate":format!("{:?}",detector.classify(&s.samples,16_000))})
            })
            .collect();
        println!(
            "DIRECT {id}: {direct} ({direct_ms} ms, {} energy segments)",
            candidates.len()
        );
        let diagnostic = json!({"id":id,"reference":spec["text"],"gain":gain,
            "duration_ms":samples.len() as u64/16,"direct_asr":direct,"direct_asr_ms":direct_ms,
            "whole_clip_gate":format!("{:?}",detector.classify(&samples,16_000)),
            "energy_segments":candidate_diagnostics});
        clips.push(Clip {
            spec: spec.clone(),
            source: match spec["source"].as_str().unwrap() {
                "microphone" => AudioSource::Microphone,
                "system" => AudioSource::System,
                _ => panic!("unsupported fixture source"),
            },
            start_ms: spec["start_ms"].as_u64().unwrap(),
            samples,
            diagnostic,
        });
    }
    write_report(
        &root.join("direct-clips.json"),
        &json!(clips.iter().map(|c| &c.diagnostic).collect::<Vec<_>>()),
    );

    let (sender, receiver) = mpsc::sync_channel::<MeetingAsrRequest>(1);
    let responder = thread::spawn(move || {
        while let Ok(request) = receiver.recv() {
            let result = model
                .transcribe(&request.samples, request.sample_rate)
                .map_err(|e| e.to_string());
            let _ = request.reply.send(result);
        }
    });
    let mut results = Vec::new();
    for echo in [false, true] {
        results.push(replay(&root, &plan, &clips, &sender, echo));
    }
    drop(sender);
    responder.join().unwrap();
    let report = json!({"synthetic_only":true,"hardware_capture":false,"network":false,
        "virtual_clock":true,"model":"SenseVoice int8","language":"zh","threads":4,
        "source_sha256": source_fingerprint(),
        "scenarios":results});
    write_report(&root.join("report.json"), &report);
    println!("DIALOGUE_REPORT {}", root.join("report.json").display());
    // Save evidence first, then surface quality failures as failures (not a passed smoke test).
    for result in &results {
        assert_eq!(result["punctuation_only_segments"], 0, "{result}");
        assert_eq!(result["noise_or_silence_segments"], 0, "{result}");
        assert_eq!(result["cross_track_leakage_segments"], 0, "{result}");
        assert_eq!(
            result["short_answers_correct"], 4,
            "short answers lost; inspect retained report"
        );
        assert_eq!(
            result["quiet_turns_retained"], 2,
            "quiet voice lost; inspect retained report"
        );
        assert!(result["stopped_at_ms"].is_number(), "auto-end never fired");
    }
}

fn source_fingerprint() -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for source in [
        include_str!("meeting.rs"),
        include_str!("../../vocalcode-meeting/src/audio.rs"),
        include_str!("../../vocalcode-meeting/src/echo.rs"),
        include_str!("../../vocalcode-meeting/src/echo_guard.rs"),
        include_str!("../../vocalcode-platform/src/speech_gate.rs"),
    ] {
        digest.update(source.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn replay(
    root: &Path,
    plan: &Value,
    clips: &[Clip],
    sender: &mpsc::SyncSender<MeetingAsrRequest>,
    echo: bool,
) -> Value {
    let name = if echo { "speaker_echo" } else { "headphones" };
    let directory = root.join(name);
    std::fs::create_dir(&directory).unwrap();
    let total = plan["duration_ms"].as_u64().unwrap() as usize * 16;
    let mut microphone = vec![0.0_f32; total];
    let mut system = vec![0.0_f32; total];
    for clip in clips {
        let track = if clip.source == AudioSource::Microphone {
            &mut microphone
        } else {
            &mut system
        };
        let start = clip.start_ms as usize * 16;
        assert!(start + clip.samples.len() <= total);
        for (dst, src) in track[start..].iter_mut().zip(&clip.samples) {
            *dst += src;
        }
    }
    let mut seed = 0x12345678_u32;
    for interval in plan["noise_intervals"].as_array().unwrap() {
        let start = interval["start_ms"].as_u64().unwrap() as usize * 16;
        let end = interval["end_ms"].as_u64().unwrap() as usize * 16;
        for (i, sample) in microphone[start..end].iter_mut().enumerate() {
            *sample += match interval["kind"].as_str().unwrap() {
                "white_noise" => {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    ((seed as f64 / u32::MAX as f64) * 2.0 - 1.0) as f32 * 0.02
                }
                "keyboard_clicks" => {
                    if i % 4000 < 32 {
                        if i % 2 == 0 {
                            0.2
                        } else {
                            -0.2
                        }
                    } else {
                        0.0
                    }
                }
                "hum" => (std::f32::consts::TAU * 60.0 * i as f32 / 16000.0).sin() * 0.01,
                _ => panic!("unknown synthetic noise"),
            };
        }
    }
    if echo {
        for (delay, gain) in [(1120, 0.42), (1424, 0.14)] {
            for i in delay..total {
                microphone[i] += system[i - delay] * gain;
            }
        }
    }
    microphone.iter_mut().for_each(|s| *s = s.clamp(-1.0, 1.0));
    // Preserve the synthesized pre-AEC tracks as well as the production post-AEC chunks.
    for (source, samples) in [
        (AudioSource::Microphone, &microphone),
        (AudioSource::System, &system),
    ] {
        let mut writer = ChunkedPcmWriter::new(&directory.join("input"), source).unwrap();
        writer.push(samples).unwrap();
        writer.finish().unwrap();
    }
    let store = MeetingStore::open(directory.join("meetings")).unwrap();
    let mut meeting = store
        .create(NewMeeting {
            title: format!("Synthetic QA: {name}"),
            now_ms: now_ms(),
            source: MeetingSource::Live {
                microphone: true,
                system_audio: true,
            },
            language: "zh".into(),
            audio_retention: AudioRetention::KeepUntilDeleted,
        })
        .unwrap();
    meeting.speakers.push(Speaker {
        id: "you".into(),
        label: "You".into(),
        source: AudioSource::Microphone,
    });
    let bridge = Bridge::default();
    bridge.0.active.store(true, Ordering::Release);
    bridge.set_active_meeting(Some(meeting.id.clone()));
    let mut tracks =
        TrackSet::new(&store.audio_directory(&meeting.id).unwrap(), true, true).unwrap();
    tracks.detector = crate::noise_filter::meeting_detector(root);
    assert!(tracks.detector.is_some());
    let mut transcription = LiveTranscription::default();
    let mut auto_end = AutoEnd::new(5, 1);
    let mut notices = Vec::new();
    let mut stopped_at = None;
    let mut last_speech_ms = 0;
    let mut drafts = Vec::new();
    let started = Instant::now();
    for start in (0..total).step_by(1600) {
        let end = (start + 1600).min(total);
        let now = end as u64 / 16;
        for (source, samples) in [
            (StreamedAudioSource::System, &system),
            (StreamedAudioSource::Microphone, &microphone),
        ] {
            let ready = tracks
                .push(StreamedAudioBlock {
                    source,
                    samples: samples[start..end].to_vec(),
                    sample_rate: 16000,
                    channels: 1,
                    start_ms: start as u64 / 16,
                })
                .unwrap();
            trace_drafts(&ready, &mut drafts);
            transcription.enqueue(ready).unwrap();
        }
        meeting.duration_ms = now;
        transcription
            .tick(&store, &bridge, sender, &mut meeting)
            .unwrap();
        let speech = std::mem::take(&mut tracks.speech_seen);
        if speech {
            last_speech_ms = now;
        }
        let mut stop = auto_end.tick(now, speech, tracks.monitor_healthy());
        if stop && tracks.pending_speech() {
            stop = auto_end.tick(now, true, tracks.monitor_healthy());
        }
        if let Some(notice) = auto_end.notice(now) {
            if notices.is_empty() {
                notices
                    .push(json!({"id":notice.id,"shown_at_ms":now,"simulated_visible_ack":true}));
                auto_end.action(notice.id, AutoEndAction::Visible, now);
            }
        }
        tracks.collect_warnings(&mut meeting);
        if stop {
            stopped_at = Some(now);
            meeting.end_reason = Some("silence_auto_end".into());
            break;
        }
        if start % (16000 * 60) == 0 {
            println!(
                "REPLAY {name} virtual_ms={now} segments={}",
                meeting.segments.len()
            );
        }
    }
    let tail = tracks.finish().unwrap();
    trace_drafts(&tail, &mut drafts);
    transcription.enqueue(tail).unwrap();
    tracks.collect_warnings(&mut meeting);
    meeting.status = MeetingStatus::Processing;
    store.save(&meeting).unwrap();
    let drain = Instant::now();
    while !transcription.is_empty() {
        assert!(
            drain.elapsed() < Duration::from_secs(180),
            "ASR worker stalled"
        );
        transcription
            .tick(&store, &bridge, sender, &mut meeting)
            .unwrap();
        thread::sleep(Duration::from_millis(10));
    }
    complete_meeting(&store, &bridge, &mut meeting).unwrap();
    let saved = store.load(&meeting.id).unwrap();
    assert_eq!(saved.status, MeetingStatus::Completed);
    assert!(saved.summary.is_some());
    assert_eq!(saved.segments, meeting.segments);
    for kind in [
        ExportKind::Text,
        ExportKind::Markdown,
        ExportKind::Json,
        ExportKind::Srt,
    ] {
        export_to(
            &store,
            &meeting.id,
            kind,
            &directory.join(format!("transcript-{kind:?}.txt")),
        )
        .unwrap();
    }
    let mut short_correct = 0;
    let mut quiet_retained = 0;
    let turns:Vec<_>=clips.iter().map(|clip| {
        let end=clip.start_ms+clip.samples.len() as u64/16;
        let found:Vec<_>=meeting.segments.iter().filter(|s| s.source==clip.source && s.start_ms<end && s.end_ms>clip.start_ms).collect();
        let text=found.iter().map(|s|s.text.as_str()).collect::<Vec<_>>().join(" ");
        let correct=clip.spec["accepted"].as_array().map(|accepted|accepted.iter().any(|a|normalized(a.as_str().unwrap())==normalized(&text)));
        if correct==Some(true) {short_correct+=1;}
        if clip.spec["gain"].as_f64().unwrap()<0.1 && !text.is_empty() {quiet_retained+=1;}
        json!({"id":clip.spec["id"],"reference":clip.spec["text"],"actual":text,"short_answer_correct":correct,"segments":found,"direct":clip.diagnostic})
    }).collect();
    let punctuation = meeting
        .segments
        .iter()
        .filter(|s| vocalcode_meeting::quality::punctuation_only(&s.text))
        .count();
    let non_speech: Vec<_> = meeting
        .segments
        .iter()
        .filter(|s| {
            !clips.iter().any(|c| {
                s.start_ms < c.start_ms + c.samples.len() as u64 / 16 + 300
                    && s.end_ms > c.start_ms.saturating_sub(300)
            })
        })
        .collect();
    let echo_stats = tracks.echo.as_ref().unwrap().stats();
    let leakage: Vec<_> = meeting
        .segments
        .iter()
        .filter(|s| {
            !clips.iter().any(|c| {
                c.source == s.source
                    && s.start_ms < c.start_ms + c.samples.len() as u64 / 16 + 300
                    && s.end_ms > c.start_ms.saturating_sub(300)
            }) && !non_speech.iter().any(|n| n.id == s.id)
        })
        .collect();
    let result = json!({"name":name,"wall_ms":started.elapsed().as_millis(),"virtual_duration_ms":meeting.duration_ms,
        "last_detected_speech_ms":last_speech_ms,"notices":notices,"stopped_at_ms":stopped_at,
        "aec_active":echo_stats.active,"aec_degradation":echo_stats.degradation,
        "protected_capture_frames":echo_stats.protected_capture_frames,
        "warnings":meeting.warnings,"filtered_noise_segments":meeting.filtered_noise_segments,
        "punctuation_only_segments":punctuation,"noise_or_silence_segments":non_speech.len(),
        "unexpected_segments":non_speech,"short_answers_correct":short_correct,"quiet_turns_retained":quiet_retained,
        "cross_track_leakage_segments":leakage.len(),"cross_track_leakage":leakage,
        "turns":turns,"asr_drafts":drafts,"segments":meeting.segments});
    write_report(&directory.join("result.json"), &result);
    println!("RESULT {name}: short={short_correct}/4 quiet={quiet_retained}/2 noise={} punctuation={punctuation} stopped={stopped_at:?}",non_speech.len());
    result
}
