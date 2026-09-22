//! Replay-only regression tests. No microphone, clipboard, injector or user data.
use super::*;

fn fixture(label: &str) -> (PathBuf, MeetingStore, Meeting, Bridge) {
    let root = tests::test_root(label);
    let store = MeetingStore::open(root.join("meetings")).unwrap();
    let meeting = store
        .create(NewMeeting {
            title: label.into(),
            now_ms: 1_787_796_747_000,
            source: MeetingSource::Live {
                microphone: true,
                system_audio: false,
            },
            language: "en".into(),
            audio_retention: AudioRetention::KeepUntilDeleted,
        })
        .unwrap();
    let bridge = Bridge::default();
    bridge.0.active.store(true, Ordering::Release);
    bridge.set_active_meeting(Some(meeting.id.clone()));
    (root, store, meeting, bridge)
}

fn draft(index: u64) -> Draft {
    Draft {
        source: AudioSource::Microphone,
        speaker_id: "you".into(),
        segment: SpeechSegment {
            start_ms: index * 1_000,
            end_ms: (index + 1) * 1_000,
            samples: vec![0.2; 16_000],
        },
    }
}

#[test]
fn short_meeting_asr_padding_preserves_content_and_only_adds_silence() {
    let original = vec![0.125; 6_400];
    let padded = meeting_asr_samples(original.clone());
    assert_eq!(padded.len(), 24_000);
    assert_eq!(&padded[..original.len()], original.as_slice());
    assert!(padded[original.len()..].iter().all(|s| *s == 0.0));
    for count in [0, 1, 3_199, 16_000, 45 * 16_000] {
        assert_eq!(meeting_asr_samples(vec![0.125; count]), vec![0.125; count]);
    }
}

#[test]
fn punctuation_does_not_create_rows_but_multilingual_short_answers_do() {
    let (root, store, mut meeting, bridge) = fixture("punctuation-hygiene");
    for text in ["。", "...", "，！？", "।"] {
        save_transcription(&store, &bridge, &mut meeting, draft(0), text.into()).unwrap();
    }
    assert!(meeting.segments.is_empty());
    assert!(meeting.speakers.is_empty());
    for (i, text) in ["不。", "是", "OK", "No", "はい", "아니요", "नहीं", "7"]
        .iter()
        .enumerate()
    {
        save_transcription(
            &store,
            &bridge,
            &mut meeting,
            draft(i as u64),
            (*text).into(),
        )
        .unwrap();
    }
    assert_eq!(meeting.filtered_noise_segments, 4);
    assert_eq!(store.load(&meeting.id).unwrap().segments.len(), 8);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn old_punctuation_is_marked_only_in_derived_reading_view() {
    let (root, store, mut meeting, _) = fixture("old-punctuation");
    meeting.segments = vec![
        tests::transcript(1, 0, 45_000, AudioSource::Microphone, "。"),
        tests::transcript(2, 45_000, 90_000, AudioSource::Microphone, "情况刷。"),
        tests::transcript(3, 90_000, 90_700, AudioSource::Microphone, "不。"),
    ];
    let original = export_text(&meeting);
    let value = meeting_value(&meeting);
    assert_eq!(value["segments"][0]["noise_only"], true);
    assert_eq!(value["segments"][1]["review_recommended"], true);
    assert_eq!(value["segments"][2]["noise_only"], false);
    assert_eq!(value["segments"][2]["review_recommended"], false);
    assert_eq!(export_text(&meeting), original);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn real_meeting_detector_filters_noise_and_keeps_audio_for_recovery() {
    let (root, store, mut meeting, _) = fixture("meeting-vad");
    let audio = store.audio_directory(&meeting.id).unwrap();
    let mut tracks = TrackSet::new(&audio, true, false).unwrap();
    tracks.detector = crate::noise_filter::meeting_detector(&root);
    assert!(tracks.detector.is_some());
    assert!(!tracks.monitor_healthy()); // never treat missing callbacks as silence
    let mut seed = 7_u32;
    let samples: Vec<f32> = (0..48_000)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32 * 0.02
        })
        .collect();
    let mut output = tracks
        .push(StreamedAudioBlock {
            source: StreamedAudioSource::Microphone,
            samples,
            sample_rate: 16_000,
            channels: 1,
            start_ms: 0,
        })
        .unwrap();
    assert!(tracks.monitor_healthy());
    output.extend(tracks.finish().unwrap());
    assert!(output.is_empty());
    assert!(!tracks.speech_seen);
    tracks.collect_warnings(&mut meeting);
    assert!(meeting.filtered_noise_segments > 0);
    assert!(std::fs::read_dir(&audio).unwrap().count() > 0);
    let mut clusterer = OnlineSpeakerClusterer::default();
    let mut rejected = 0;
    let noise = SpeechSegment {
        start_ms: 0,
        end_ms: 3000,
        samples: vec![0.; 48_000],
    };
    assert!(filtered_drafts(
        AudioSource::System,
        vec![noise],
        &mut clusterer,
        &mut tracks.detector,
        &mut rejected
    )
    .is_empty());
    assert_eq!(rejected, 1);
    assert_eq!(clusterer.cluster_count(), 0);
    // Brief candidates are now retained by the segmenter, so the meeting gate
    // must classify them rather than sending short DC/noise straight to ASR.
    assert!(!permit_meeting_audio(&mut tracks.detector, &[0.01; 6_400]));
    assert!(!permit_meeting_audio(&mut tracks.detector, &[0.0; 3_200]));
    // Meeting-specific padding must not change the short push-to-talk bypass.
    assert_eq!(
        tracks
            .detector
            .as_mut()
            .unwrap()
            .classify(&[0.0; 3_200], 16_000),
        Decision::Short
    );
    assert!(!permit_meeting_audio(&mut tracks.detector, &[0.0; 3_200]));
    tracks.detector = None;
    assert!(!tracks.monitor_healthy());
    assert!(permit_meeting_audio(&mut tracks.detector, &[0.; 16_000]));
    drop(tracks);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn countdown_controls_are_bound_to_the_current_recording_and_bounded() {
    let bridge = Bridge::default();
    bridge.0.active.store(true, Ordering::Release);
    bridge.set_auto_end_notice(Some(AutoEndNotice {
        id: 7,
        idle_minutes: 5,
        remaining_seconds: 30,
    }));
    bridge.auto_end_action(6, AutoEndAction::Stop);
    assert!(bridge.0.auto_end_actions.lock().unwrap().is_empty());
    for _ in 0..100 {
        bridge.auto_end_action(7, AutoEndAction::Continue);
    }
    assert_eq!(bridge.0.auto_end_actions.lock().unwrap().len(), 1);
    bridge.set_active_meeting(None);
    assert!(bridge.auto_end_notice().is_none());
    assert!(bridge.0.auto_end_actions.lock().unwrap().is_empty());
    bridge.auto_end_action(7, AutoEndAction::Stop);
    assert!(bridge.0.auto_end_actions.lock().unwrap().is_empty());
}

#[test]
fn clean_reading_fields_never_modify_saved_transcript_or_export() {
    let (root, store, mut meeting, _) = fixture("filler-reading");
    meeting.segments.push(tests::transcript(
        1,
        100,
        2100,
        AudioSource::Microphone,
        "Um, we should, uh, retry.",
    ));
    let original = serde_json::to_value(&meeting).unwrap();
    let before = export_text(&meeting);
    let value = meeting_value(&meeting);
    assert_eq!(value["segments"][0]["reading_text"], "We should retry.");
    assert_eq!(value["segments"][0]["filler_removed"], 2);
    assert_eq!(value["segments"][0]["text"], "Um, we should, uh, retry.");
    assert_eq!(value["segments"][0]["start_ms"], 100);
    assert_eq!(value["segments"][0]["end_ms"], 2100);
    assert_eq!(serde_json::to_value(&meeting).unwrap(), original);
    assert_eq!(export_text(&meeting), before);
    meeting.language = "zh".into();
    meeting.segments[0].text = "这件事情，嗯，需要再讨论。".into();
    let chinese_before = export_text(&meeting);
    let chinese = meeting_value(&meeting);
    assert_eq!(
        chinese["segments"][0]["reading_text"],
        "这件事情，需要再讨论。"
    );
    assert_eq!(chinese["segments"][0]["text"], "这件事情，嗯，需要再讨论。");
    assert_eq!(export_text(&meeting), chinese_before);
    // Language guard also applies to the derived meeting view.
    meeting.language = "de".into();
    assert_eq!(meeting_value(&meeting)["segments"][0]["filler_removed"], 0);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn stalled_model_does_not_block_audio_checkpointing_or_stop_signal() {
    let (root, store, mut meeting, bridge) = fixture("stalled-live");
    let audio = store.audio_directory(&meeting.id).unwrap();
    let mut tracks = TrackSet::new(&audio, true, false).unwrap();
    let mut pipeline = LiveTranscription::default();
    let (sender, receiver) = mpsc::sync_channel(1);
    pipeline.enqueue(vec![draft(0)]).unwrap();
    pipeline
        .tick(&store, &bridge, &sender, &mut meeting)
        .unwrap();
    let request = receiver.try_recv().unwrap(); // Deliberately keep ASR blocked.
    for index in 0..120 {
        let samples = (0..1_600)
            .map(|n| (std::f32::consts::TAU * 220.0 * n as f32 / 16_000.0).sin() * 0.2)
            .collect();
        pipeline
            .enqueue(
                tracks
                    .push(StreamedAudioBlock {
                        source: StreamedAudioSource::Microphone,
                        samples,
                        sample_rate: 16_000,
                        channels: 1,
                        start_ms: index * 100,
                    })
                    .unwrap(),
            )
            .unwrap();
        pipeline
            .tick(&store, &bridge, &sender, &mut meeting)
            .unwrap();
    }
    // Checkpoint exists BEFORE finish(), despite an unanswered native request.
    assert!(audio.join("microphone-000000.wav").is_file());
    let stop = Arc::new(AtomicBool::new(false));
    bridge.set_stop(Some(stop.clone()));
    bridge.stop().unwrap();
    assert!(stop.load(Ordering::Acquire));
    pipeline.enqueue(tracks.finish().unwrap()).unwrap();
    meeting.status = MeetingStatus::Processing;
    meeting.duration_ms = 12_000;
    meeting.ended_at_ms = Some(meeting.started_at_ms + 12_000);
    request
        .reply
        .send(Ok("The local test passed.".into()))
        .unwrap();
    pipeline
        .tick(&store, &bridge, &sender, &mut meeting)
        .unwrap();
    assert_eq!(meeting.segments.len(), 1);
    assert_eq!(meeting.duration_ms, 12_000);
    assert_eq!(bridge.snapshot()["phase"], "processing");
    // Drain any tail speech, including its final samples.
    while !pipeline.is_empty() {
        if let Ok(request) = receiver.try_recv() {
            request.reply.send(Ok("Final sentence.".into())).unwrap();
        }
        pipeline
            .tick(&store, &bridge, &sender, &mut meeting)
            .unwrap();
    }
    complete_meeting(&store, &bridge, &mut meeting).unwrap();
    assert_eq!(
        store.load(&meeting.id).unwrap().status,
        MeetingStatus::Completed
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn saturated_asr_channel_and_backlog_are_bounded_without_dropping_a_draft() {
    let (root, store, mut meeting, bridge) = fixture("full-channel");
    let (sender, receiver) = mpsc::sync_channel(1);
    let (reply, _) = mpsc::sync_channel(1);
    sender
        .send(MeetingAsrRequest {
            samples: vec![],
            sample_rate: 16_000,
            reply,
        })
        .unwrap();
    let mut pipeline = LiveTranscription::default();
    pipeline.enqueue(vec![draft(0)]).unwrap();
    pipeline
        .tick(&store, &bridge, &sender, &mut meeting)
        .unwrap();
    assert_eq!(pipeline.queued[0].segment.samples.len(), 16_000);
    assert!(pipeline.pending.is_none());
    pipeline
        .enqueue((1..MAX_PENDING_SEGMENTS).map(|i| draft(i as u64)).collect())
        .unwrap();
    assert!(pipeline.enqueue(vec![draft(99)]).is_err());
    assert_eq!(pipeline.queued.len(), MAX_PENDING_SEGMENTS);
    drop(receiver);
    assert!(pipeline
        .tick(&store, &bridge, &sender, &mut meeting)
        .is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn retained_audio_contains_the_resampler_tail() {
    let (root, store, _, _) = fixture("audio-tail");
    let audio = root.join("audio-replay");
    let mut tracks = TrackSet::new(&audio, true, false).unwrap();
    tracks
        .push(StreamedAudioBlock {
            source: StreamedAudioSource::Microphone,
            samples: vec![0.1; 48_000],
            sample_rate: 48_000,
            channels: 1,
            start_ms: 0,
        })
        .unwrap();
    tracks.finish().unwrap();
    let mut count = 0;
    decode_audio_file(&audio.join("microphone-000000.wav"), |block| {
        count += block.samples.len();
        Ok(())
    })
    .unwrap();
    assert_eq!(count, 16_000);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn opposite_decisions_and_changed_numbers_are_not_echo() {
    for (left, right) in [
        (
            "We approve this release.",
            "We do not approve this release.",
        ),
        ("We approved 1200 USD.", "We approved 12000 USD."),
        ("The balance is -1200 USD.", "The balance is 1200 USD."),
        ("The balance is 12.00 USD.", "The balance is 1200 USD."),
        ("我们同意今天发布这个版本", "我们不同意今天发布这个版本"),
    ] {
        let input = [
            tests::transcript(1, 0, 3_000, AudioSource::Microphone, left),
            tests::transcript(2, 100, 3_000, AudioSource::System, right),
        ];
        assert_eq!(deduplicate_live_tracks(&input).len(), 2, "{left} / {right}");
    }
    let input = [
        tests::transcript(1, 0, 1_000, AudioSource::Microphone, "Please try again"),
        tests::transcript(2, 1_100, 2_000, AudioSource::System, "Please try again"),
    ];
    assert_eq!(deduplicate_live_tracks(&input).len(), 2);
}

#[test]
fn library_selection_does_not_replace_live_identity_and_completion_keeps_detail() {
    let (root, store, meeting, bridge) = fixture("live-identity");
    publish_active(&bridge, &store, &meeting, "recording", None);
    bridge.publish(
        json!({"active":true,"detail":{"id":"1787796747000-1-999","title":"Old meeting"}}),
    );
    assert_eq!(bridge.snapshot()["recording"]["title"], "live-identity");
    publish_active(&bridge, &store, &meeting, "completed", None);
    bridge.0.active.store(false, Ordering::Release);
    publish_store(&bridge, &store, None, &[], None, None);
    assert_eq!(bridge.snapshot()["detail"]["id"], meeting.id.as_str());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn recovery_does_not_add_offline_time_or_invent_retained_audio() {
    let (root, store, mut meeting, _) = fixture("recovery-duration");
    meeting.duration_ms = 35_000;
    store.save(&meeting).unwrap();
    store
        .recover_interrupted(meeting.started_at_ms + 86_400_000)
        .unwrap();
    let recovered = store.load(&meeting.id).unwrap();
    assert_eq!(recovered.duration_ms, 35_000);
    assert_eq!(recovered.ended_at_ms, Some(meeting.started_at_ms + 35_000));
    assert!(recovered.error.unwrap().contains("no recoverable audio"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn meeting_does_not_disable_the_dictation_hotkey_ready_gate() {
    let (root, _, _, bridge) = fixture("dictation-ready");
    let status = crate::webui::RuntimeStatus::with_meetings(bridge);
    assert!(status.meetings.is_active());
    let input = AtomicBool::new(true);
    assert!(crate::engine_ready(true, true, &input, &status));
    assert!(!crate::engine_ready(false, true, &input, &status));
    assert!(!crate::engine_ready(true, false, &input, &status));
    status.shutdown.store(true, Ordering::Release);
    assert!(!crate::engine_ready(true, true, &input, &status));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn runtime_shutdown_cancels_an_import_waiting_on_a_stalled_native_model() {
    let root = tests::test_root("import-shutdown");
    let mut writer = ChunkedPcmWriter::new(&root.join("input"), AudioSource::Imported).unwrap();
    let mut samples: Vec<_> = (0..24_000)
        .map(|n| (std::f32::consts::TAU * 220.0 * n as f32 / 16_000.0).sin() * 0.2)
        .collect();
    samples.extend(vec![0.0; 16_000]);
    writer.push(&samples).unwrap();
    let input = writer.finish().unwrap().remove(0).path;
    let (sender, receiver) = mpsc::sync_channel(1);
    let runtime = Runtime::start(&root, sender).unwrap();
    runtime
        .bridge()
        .import(input, "Interrupted import".into(), "en".into())
        .unwrap();
    let request = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
    let started = std::time::Instant::now();
    runtime.shutdown();
    assert!(started.elapsed() < Duration::from_secs(2));
    // The model was never killed: a late reply is simply no longer needed.
    assert!(request.reply.send(Ok("Late transcription".into())).is_err());
    let store = MeetingStore::open(root.join("meetings")).unwrap();
    let entry = store.list().unwrap().remove(0);
    assert_eq!(entry.status, MeetingStatus::Interrupted);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cancelled_import_does_not_block_on_full_request_channel() {
    let (sender, _receiver) = mpsc::sync_channel(1);
    let (reply, _) = mpsc::sync_channel(1);
    sender
        .send(MeetingAsrRequest {
            samples: vec![],
            sample_rate: 16_000,
            reply,
        })
        .unwrap();
    assert!(
        request_asr(&sender, vec![0.1; 16_000], &AtomicBool::new(true))
            .unwrap_err()
            .contains("cancelled")
    );
}

#[test]
#[ignore = "opt-in replay of explicit public audio using an already-downloaded local SenseVoice model"]
fn real_local_model_import_replay() {
    use vocalcode_core::Asr;
    let models = PathBuf::from(
        std::env::var_os("VOCALCODE_QA_MODEL_DIR").expect("explicit model directory"),
    );
    let inputs = std::env::var_os("VOCALCODE_QA_AUDIO").expect("explicit public WAV paths");
    let inputs: Vec<_> = std::env::split_paths(&inputs).collect();
    assert!(!inputs.is_empty());
    let mut model = vocalcode_platform::SherpaSenseVoiceAsr::new(
        &models.join("model.int8.onnx").to_string_lossy(),
        &models.join("tokens.txt").to_string_lossy(),
        "auto",
        4,
        "meeting QA",
    )
    .unwrap();
    let (sender, receiver) = mpsc::sync_channel::<MeetingAsrRequest>(1);
    let responder = thread::spawn(move || {
        while let Ok(request) = receiver.recv() {
            let result = model
                .transcribe(&request.samples, request.sample_rate)
                .map_err(|e| e.to_string());
            let _ = request.reply.send(result);
        }
    });
    let root = tests::test_root("real-asr");
    let store = MeetingStore::open(root.join("meetings")).unwrap();
    for (index, input) in inputs.into_iter().enumerate() {
        let bridge = Bridge::default();
        bridge.0.active.store(true, Ordering::Release);
        let started = std::time::Instant::now();
        run_import(
            &store,
            &bridge,
            &sender,
            Arc::new(AtomicBool::new(false)),
            input,
            format!("Public QA sample {index}"),
            "auto".into(),
        )
        .unwrap();
        let meeting = store.load(&store.list().unwrap()[0].id).unwrap();
        assert_eq!(meeting.status, MeetingStatus::Completed);
        assert!(!meeting.segments.is_empty());
        assert!(meeting.summary.is_some());
        println!("MEETING_REPLAY sample={index} audio_ms={} pipeline_ms={} segments={} text_characters={}",
            meeting.duration_ms,started.elapsed().as_millis(),meeting.segments.len(),
            meeting.segments.iter().map(|s|s.text.chars().count()).sum::<usize>());
        for kind in [
            ExportKind::Markdown,
            ExportKind::Text,
            ExportKind::Json,
            ExportKind::Srt,
        ] {
            export_to(
                &store,
                &meeting.id,
                kind,
                &root.join(format!("export-{index}-{kind:?}")),
            )
            .unwrap();
        }
    }
    drop(sender);
    responder.join().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
