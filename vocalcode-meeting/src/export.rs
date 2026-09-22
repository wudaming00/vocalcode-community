use serde::Serialize;

use crate::{Meeting, Result, TranscriptSegment};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Markdown,
    Text,
    Json,
    Srt,
}

pub fn export_markdown(meeting: &Meeting) -> String {
    let mut output = String::new();
    output.push_str("# ");
    output.push_str(&escape_markdown(&meeting.title));
    output.push_str("\n\n");
    output.push_str(&format!(
        "- Duration: {}\n- Language: {}\n- Stored locally: yes\n\n",
        format_duration(meeting.duration_ms),
        escape_markdown(&meeting.language)
    ));
    if let Some(summary) = &meeting.summary {
        write_note_section(
            &mut output,
            "Summary",
            summary
                .overview
                .iter()
                .map(|note| (note.at_ms, note.text.as_str())),
        );
        write_note_section(
            &mut output,
            "Decisions",
            summary
                .decisions
                .iter()
                .map(|note| (note.at_ms, note.text.as_str())),
        );
        if !summary.action_items.is_empty() {
            output.push_str("## Action items\n\n");
            for item in &summary.action_items {
                output.push_str(&format!(
                    "- [{}](#t-{}) {}",
                    format_timestamp(item.source.at_ms),
                    item.source.at_ms,
                    escape_markdown(&item.text)
                ));
                if let Some(owner) = &item.owner {
                    output.push_str(&format!(" _(owner: {})_", escape_markdown(owner)));
                }
                if let Some(due) = &item.due {
                    output.push_str(&format!(" _(due: {})_", escape_markdown(due)));
                }
                output.push('\n');
            }
            output.push('\n');
        }
        write_note_section(
            &mut output,
            "Open questions",
            summary
                .open_questions
                .iter()
                .map(|item| (item.source.at_ms, item.text.as_str())),
        );
    }
    output.push_str("## Transcript\n\n");
    for segment in ordered_segments(meeting) {
        output.push_str(&format!(
            "<a id=\"t-{}\"></a>**[{}] {}:** {}\n\n",
            segment.start_ms,
            format_timestamp(segment.start_ms),
            escape_markdown(&speaker_label(meeting, segment)),
            escape_markdown(&segment.text)
        ));
    }
    output
}

pub fn export_text(meeting: &Meeting) -> String {
    let mut output = format!(
        "{}\nDuration: {}\nLanguage: {}\nStored locally: yes\n\n",
        meeting.title,
        format_duration(meeting.duration_ms),
        meeting.language
    );
    if let Some(summary) = &meeting.summary {
        write_plain_section(
            &mut output,
            "SUMMARY",
            summary
                .overview
                .iter()
                .map(|note| (note.at_ms, note.text.as_str())),
        );
        write_plain_section(
            &mut output,
            "DECISIONS",
            summary
                .decisions
                .iter()
                .map(|note| (note.at_ms, note.text.as_str())),
        );
        if !summary.action_items.is_empty() {
            output.push_str("ACTION ITEMS\n");
            for item in &summary.action_items {
                output.push_str(&format!(
                    "- [{}] {}\n",
                    format_timestamp(item.source.at_ms),
                    item.text
                ));
            }
            output.push('\n');
        }
        write_plain_section(
            &mut output,
            "OPEN QUESTIONS",
            summary
                .open_questions
                .iter()
                .map(|item| (item.source.at_ms, item.text.as_str())),
        );
    }
    output.push_str("TRANSCRIPT\n");
    for segment in ordered_segments(meeting) {
        output.push_str(&format!(
            "[{}] {}: {}\n",
            format_timestamp(segment.start_ms),
            speaker_label(meeting, segment),
            segment.text
        ));
    }
    output
}

pub fn export_srt(meeting: &Meeting) -> String {
    let mut output = String::new();
    for (index, segment) in ordered_segments(meeting).into_iter().enumerate() {
        output.push_str(&format!(
            "{}\n{} --> {}\n{}: {}\n\n",
            index + 1,
            format_srt_timestamp(segment.start_ms),
            format_srt_timestamp(segment.end_ms.max(segment.start_ms + 1)),
            speaker_label(meeting, segment).replace(['\r', '\n'], " "),
            segment.text.replace(['\r', '\n'], " ")
        ));
    }
    output
}

pub fn export_json(meeting: &Meeting) -> Result<String> {
    #[derive(Serialize)]
    struct MeetingExport<'a> {
        format: &'static str,
        meeting: &'a Meeting,
        transcript: Vec<&'a TranscriptSegment>,
    }
    Ok(serde_json::to_string_pretty(&MeetingExport {
        format: "vocalcode-meeting-v1",
        meeting,
        transcript: ordered_segments(meeting),
    })?)
}

fn ordered_segments(meeting: &Meeting) -> Vec<&TranscriptSegment> {
    let mut segments: Vec<_> = meeting.segments.iter().collect();
    segments.sort_by_key(|segment| (segment.start_ms, segment.id));
    segments
}

fn speaker_label(meeting: &Meeting, segment: &TranscriptSegment) -> String {
    meeting
        .speakers
        .iter()
        .find(|speaker| speaker.id == segment.speaker_id)
        .map(|speaker| speaker.label.clone())
        .unwrap_or_else(|| segment.speaker_id.clone())
}

fn write_note_section<'a>(
    output: &mut String,
    title: &str,
    notes: impl Iterator<Item = (u64, &'a str)>,
) {
    let notes: Vec<_> = notes.collect();
    if notes.is_empty() {
        return;
    }
    output.push_str(&format!("## {title}\n\n"));
    for (at_ms, text) in notes {
        output.push_str(&format!(
            "- [{}](#t-{at_ms}) {}\n",
            format_timestamp(at_ms),
            escape_markdown(text)
        ));
    }
    output.push('\n');
}

fn write_plain_section<'a>(
    output: &mut String,
    title: &str,
    notes: impl Iterator<Item = (u64, &'a str)>,
) {
    let notes: Vec<_> = notes.collect();
    if notes.is_empty() {
        return;
    }
    output.push_str(title);
    output.push('\n');
    for (at_ms, text) in notes {
        output.push_str(&format!("- [{}] {text}\n", format_timestamp(at_ms)));
    }
    output.push('\n');
}

fn format_timestamp(milliseconds: u64) -> String {
    let total_seconds = milliseconds / 1_000;
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds / 60) % 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn format_srt_timestamp(milliseconds: u64) -> String {
    let hours = milliseconds / 3_600_000;
    let minutes = (milliseconds / 60_000) % 60;
    let seconds = (milliseconds / 1_000) % 60;
    let millis = milliseconds % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02},{millis:03}")
}

fn format_duration(milliseconds: u64) -> String {
    format_timestamp(milliseconds)
}

fn escape_markdown(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AudioRetention, AudioSource, MeetingId, MeetingSource, MeetingStatus, Speaker,
        SCHEMA_VERSION,
    };

    fn meeting() -> Meeting {
        Meeting {
            schema_version: SCHEMA_VERSION,
            id: MeetingId::parse("1787796747000-42-7").unwrap(),
            title: "Demo *call*".to_string(),
            created_at_ms: 1,
            updated_at_ms: 1,
            started_at_ms: 1,
            ended_at_ms: Some(3_000),
            duration_ms: 3_000,
            segment_count: 2,
            status: MeetingStatus::Completed,
            source: MeetingSource::Live {
                microphone: true,
                system_audio: true,
            },
            language: "en".to_string(),
            audio_retention: AudioRetention::DeleteAfterTranscription,
            speakers: vec![Speaker {
                id: "you".to_string(),
                label: "You".to_string(),
                source: AudioSource::Microphone,
            }],
            bookmarks: Vec::new(),
            summary: None,
            error: None,
            warnings: Vec::new(),
            end_reason: None,
            filtered_noise_segments: 0,
            segments: vec![
                TranscriptSegment {
                    id: 2,
                    start_ms: 2_000,
                    end_ms: 3_000,
                    speaker_id: "you".to_string(),
                    source: AudioSource::Microphone,
                    text: "Second line".to_string(),
                },
                TranscriptSegment {
                    id: 1,
                    start_ms: 0,
                    end_ms: 1_000,
                    speaker_id: "you".to_string(),
                    source: AudioSource::Microphone,
                    text: "First line".to_string(),
                },
            ],
        }
    }

    #[test]
    fn exports_are_ordered_and_json_contains_transcript() {
        let meeting = meeting();
        let markdown = export_markdown(&meeting);
        assert!(markdown.starts_with("# Demo \\*call\\*"));
        assert!(markdown.find("First line").unwrap() < markdown.find("Second line").unwrap());
        let srt = export_srt(&meeting);
        assert!(srt.contains("00:00:00,000 --> 00:00:01,000"));
        let json = export_json(&meeting).unwrap();
        assert!(json.contains("\"transcript\""));
        assert!(json.contains("First line"));
    }
}
