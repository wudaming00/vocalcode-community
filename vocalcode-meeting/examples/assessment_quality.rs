//! Semantic counterexamples; prints observations rather than blessing current behavior.
use vocalcode_meeting::{
    build_local_summary, export_json, export_markdown, export_srt, export_text, AudioBlock,
    AudioSource, LinearMonoResampler, MeetingStore, SummaryOptions, TranscriptSegment,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for (id, text) in [
        ("negative-decision-zh", "我们还没有决定采用哪个方案。"),
        ("uncertain-decision-zh", "还不确定是否采用这个方案。"),
        ("negative-approval-en", "The deployment was not approved."),
        ("negative-action-en", "We will not deploy this release."),
        ("due-zh", "我来在周五之前完成测试。"),
        ("question-zh", "这个问题怎么解决呢？"),
        (
            "positive-control-en",
            "We decided to ship. I will prepare it by Friday.",
        ),
    ] {
        let segment = TranscriptSegment {
            id: 1,
            start_ms: 0,
            end_ms: 6_000,
            speaker_id: "speaker-1".into(),
            source: AudioSource::Imported,
            text: text.into(),
        };
        let summary = build_local_summary(&[segment], SummaryOptions::default());
        println!(
            "{}",
            serde_json::json!({"case":id,"input":text,"summary":summary})
        );
    }
    let mut levels = Vec::new();
    for hz in [1_000.0f32, 12_000.0] {
        let samples: Vec<_> = (0..48_000)
            .map(|n| (std::f32::consts::TAU * hz * n as f32 / 48_000.0).sin() * 0.1)
            .collect();
        let mut resampler = LinearMonoResampler::default();
        let mut output = resampler.push(&AudioBlock {
            samples,
            sample_rate: 48_000,
            channels: 1,
            start_ms: 0,
        })?;
        output.extend(resampler.finish());
        let rms = (output.iter().map(|v| v * v).sum::<f32>() / output.len() as f32).sqrt();
        levels.push(
            serde_json::json!({"input_hz":hz,"output_rms":rms,"output_samples":output.len()}),
        );
    }
    println!(
        "{}",
        serde_json::json!({"case":"downsample-alias-rejection","levels":levels})
    );
    // Optional read-only check of existing recordings. Never print private text,
    // save changes, or export content to files. Refuse to create a new store.
    if let Some(root) = std::env::args_os().nth(1) {
        if !std::path::Path::new(&root).is_dir() {
            return Err("assessment requires an existing meeting directory".into());
        }
        let store = MeetingStore::open(root)?;
        for (index, entry) in store.list()?.into_iter().enumerate() {
            let meeting = store.load(&entry.id)?;
            let json = export_json(&meeting)?;
            let _: serde_json::Value = serde_json::from_str(&json)?;
            let beyond_duration = meeting
                .segments
                .iter()
                .filter(|s| s.end_ms > meeting.duration_ms)
                .count();
            println!(
                "{}",
                serde_json::json!({
                    "case":"existing-meeting-readonly", "index":index,
                    "duration_ms":meeting.duration_ms, "segments":meeting.segments.len(),
                    "speakers":meeting.speakers.len(), "validated":true,
                    "segments_beyond_duration":beyond_duration,
                    "export_bytes":{"json":json.len(),"markdown":export_markdown(&meeting).len(),
                        "text":export_text(&meeting).len(),"srt":export_srt(&meeting).len()}
                })
            );
        }
    }
    Ok(())
}
