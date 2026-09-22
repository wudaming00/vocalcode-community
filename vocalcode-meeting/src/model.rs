use serde::{Deserialize, Serialize};

use crate::{MeetingError, Result};

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_TITLE_BYTES: usize = 512;
pub const MAX_LANGUAGE_BYTES: usize = 64;
pub const MAX_TRANSCRIPT_SEGMENT_BYTES: usize = 64 * 1024;
pub const MAX_SPEAKERS: usize = 64;
pub const MAX_BOOKMARKS: usize = 10_000;
pub const MAX_SPEAKER_LABEL_BYTES: usize = 128;
pub const MAX_BOOKMARK_LABEL_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MeetingId(String);

impl MeetingId {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let mut parts = value.split('-');
        let timestamp = parts.next().unwrap_or_default();
        let process = parts.next().unwrap_or_default();
        let nonce = parts.next().unwrap_or_default();
        if value.len() > 80
            || parts.next().is_some()
            || timestamp.len() != 13
            || process.is_empty()
            || nonce.is_empty()
            || !timestamp.bytes().all(|byte| byte.is_ascii_digit())
            || !process.bytes().all(|byte| byte.is_ascii_digit())
            || !nonce.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(MeetingError::Invalid(
                "unsafe meeting identifier".to_string(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MeetingId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeetingStatus {
    Recording,
    Processing,
    Completed,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MeetingSource {
    Live {
        microphone: bool,
        system_audio: bool,
    },
    Imported {
        /// Display name only.  Never persist the original absolute path.
        file_name: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioRetention {
    #[default]
    DeleteAfterTranscription,
    KeepUntilDeleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioSource {
    Microphone,
    System,
    Imported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Speaker {
    pub id: String,
    pub label: String,
    pub source: AudioSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub id: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub speaker_id: String,
    pub source: AudioSource,
    pub text: String,
}

impl TranscriptSegment {
    pub fn validate(&self) -> Result<()> {
        if self.end_ms < self.start_ms {
            return Err(MeetingError::Invalid(
                "transcript segment ends before it starts".to_string(),
            ));
        }
        if !safe_speaker_id(&self.speaker_id) {
            return Err(MeetingError::Invalid(
                "unsafe transcript speaker identifier".to_string(),
            ));
        }
        if self.text.trim().is_empty() || self.text.len() > MAX_TRANSCRIPT_SEGMENT_BYTES {
            return Err(MeetingError::Invalid(
                "empty or oversized transcript segment".to_string(),
            ));
        }
        if self.text.chars().any(|character| character == '\0') {
            return Err(MeetingError::Invalid(
                "transcript contains a NUL character".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub at_ms: u64,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteRef {
    pub segment_id: u64,
    pub at_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionItem {
    pub text: String,
    pub owner: Option<String>,
    pub due: Option<String>,
    pub source: NoteRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenQuestion {
    pub text: String,
    pub source: NoteRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MeetingSummary {
    pub overview: Vec<NoteRef>,
    pub decisions: Vec<NoteRef>,
    pub action_items: Vec<ActionItem>,
    pub open_questions: Vec<OpenQuestion>,
    pub generated_locally: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meeting {
    pub schema_version: u32,
    pub id: MeetingId,
    pub title: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub duration_ms: u64,
    /// Kept in metadata so listing meetings never has to scan a long JSONL
    /// transcript. `MeetingStore::load` reconciles this after crash recovery.
    #[serde(default)]
    pub segment_count: usize,
    pub status: MeetingStatus,
    pub source: MeetingSource,
    pub language: String,
    pub audio_retention: AudioRetention,
    pub speakers: Vec<Speaker>,
    pub bookmarks: Vec<Bookmark>,
    pub summary: Option<MeetingSummary>,
    pub error: Option<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub end_reason: Option<String>,
    #[serde(default)]
    pub filtered_noise_segments: u64,
    #[serde(skip, default)]
    pub segments: Vec<TranscriptSegment>,
}

impl Meeting {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(MeetingError::Invalid(format!(
                "unsupported meeting schema {}",
                self.schema_version
            )));
        }
        MeetingId::parse(self.id.to_string())?;
        if self.title.trim().is_empty()
            || self.title.len() > MAX_TITLE_BYTES
            || self.title.contains('\0')
        {
            return Err(MeetingError::Invalid(
                "meeting title is empty or too large".to_string(),
            ));
        }
        if self.language.is_empty()
            || self.language.len() > MAX_LANGUAGE_BYTES
            || self.language.contains('\0')
        {
            return Err(MeetingError::Invalid(
                "meeting language is empty or too large".to_string(),
            ));
        }
        if self.speakers.len() > MAX_SPEAKERS || self.bookmarks.len() > MAX_BOOKMARKS {
            return Err(MeetingError::TooLarge(
                "meeting has too many speakers or bookmarks".to_string(),
            ));
        }
        if let MeetingSource::Imported { file_name } = &self.source {
            validate_import_file_name(file_name)?;
        }
        let mut speaker_ids = std::collections::HashSet::new();
        for speaker in &self.speakers {
            if !safe_speaker_id(&speaker.id)
                || speaker.label.trim().is_empty()
                || speaker.label.len() > MAX_SPEAKER_LABEL_BYTES
                || speaker.label.contains('\0')
                || !speaker_ids.insert(speaker.id.as_str())
            {
                return Err(MeetingError::Invalid(
                    "meeting has an unsafe or duplicate speaker".to_string(),
                ));
            }
        }
        for bookmark in &self.bookmarks {
            if bookmark.at_ms > self.duration_ms
                || bookmark.label.trim().is_empty()
                || bookmark.label.len() > MAX_BOOKMARK_LABEL_BYTES
                || bookmark.label.contains('\0')
            {
                return Err(MeetingError::Invalid(
                    "meeting has an invalid bookmark".to_string(),
                ));
            }
        }
        for segment in &self.segments {
            segment.validate()?;
            if !self
                .speakers
                .iter()
                .any(|speaker| speaker.id == segment.speaker_id)
            {
                return Err(MeetingError::Invalid(
                    "transcript references an unknown speaker".to_string(),
                ));
            }
        }
        if !self.segments.is_empty() && self.segment_count != self.segments.len() {
            return Err(MeetingError::Invalid(
                "meeting segment count does not match its transcript".to_string(),
            ));
        }
        if !self.segments.is_empty() {
            if let Some(summary) = &self.summary {
                let refs = summary
                    .overview
                    .iter()
                    .chain(&summary.decisions)
                    .chain(summary.action_items.iter().map(|item| &item.source))
                    .chain(summary.open_questions.iter().map(|item| &item.source));
                for note in refs {
                    let valid = self.segments.iter().any(|segment| {
                        segment.id == note.segment_id
                            && segment.start_ms == note.at_ms
                            && segment.text.contains(&note.text)
                    });
                    if !valid {
                        return Err(MeetingError::Invalid(
                            "meeting summary is not backed by its transcript".to_string(),
                        ));
                    }
                }
                if summary
                    .action_items
                    .iter()
                    .any(|item| item.text != item.source.text)
                    || summary
                        .open_questions
                        .iter()
                        .any(|item| item.text != item.source.text)
                {
                    return Err(MeetingError::Invalid(
                        "meeting summary contains unreferenced text".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMeeting {
    pub title: String,
    pub now_ms: u64,
    pub source: MeetingSource,
    pub language: String,
    pub audio_retention: AudioRetention,
}

impl NewMeeting {
    pub fn validate(&self) -> Result<()> {
        if self.title.trim().is_empty()
            || self.title.len() > MAX_TITLE_BYTES
            || self.title.contains('\0')
        {
            return Err(MeetingError::Invalid(
                "meeting title is empty or too large".to_string(),
            ));
        }
        if self.language.is_empty()
            || self.language.len() > MAX_LANGUAGE_BYTES
            || self.language.contains('\0')
        {
            return Err(MeetingError::Invalid(
                "meeting language is empty or too large".to_string(),
            ));
        }
        if let MeetingSource::Imported { file_name } = &self.source {
            validate_import_file_name(file_name)?;
        }
        Ok(())
    }
}

fn safe_speaker_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn validate_import_file_name(file_name: &str) -> Result<()> {
    if file_name.trim().is_empty()
        || file_name.len() > 1024
        || file_name.contains('\0')
        || std::path::Path::new(file_name)
            .file_name()
            .and_then(|value| value.to_str())
            != Some(file_name)
    {
        return Err(MeetingError::Invalid(
            "import display name is empty, unsafe, or too large".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meeting_ids_reject_paths_and_extra_components() {
        for value in [
            "../../meeting",
            "1234567890123-1-2/three",
            "1234567890123-1-2-3",
            "123-1-2",
            "1234567890123-a-2",
            "1234567890123-1-22222222222222222222222222222222222222222222222222222222222222222222",
        ] {
            assert!(MeetingId::parse(value).is_err(), "{value}");
        }
        assert_eq!(
            MeetingId::parse("1787796747000-42-7").unwrap().as_str(),
            "1787796747000-42-7"
        );
    }

    #[test]
    fn transcript_bounds_are_checked() {
        let mut segment = TranscriptSegment {
            id: 1,
            start_ms: 200,
            end_ms: 100,
            speaker_id: "speaker-1".to_string(),
            source: AudioSource::Imported,
            text: "hello".to_string(),
        };
        assert!(segment.validate().is_err());
        segment.end_ms = 300;
        assert!(segment.validate().is_ok());
        segment.speaker_id = "../../other".to_string();
        assert!(segment.validate().is_err());
    }
}
