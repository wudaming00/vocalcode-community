//! Local-first meeting recording, transcription, notes, search, and export.
//!
//! This crate deliberately owns no network client and no text injector.  It is
//! reusable by the desktop host and by tests without granting either capability.

mod audio;
pub mod auto_end;
mod diarization;
mod echo;
mod echo_guard;
mod error;
mod export;
mod model;
pub mod quality;
mod search;
mod store;
mod summary;
#[cfg(test)]
mod test_support;

pub use audio::{
    decode_audio_file, AudioBlock, AudioChunk, AudioDecoderInfo, ChunkedPcmWriter,
    LinearMonoResampler, SpeechSegment, SpeechSegmenter, TARGET_SAMPLE_RATE,
};
pub use diarization::{OnlineSpeakerClusterer, Voiceprint};
pub use echo::{EchoCancellationOutput, EchoCancellationStats, RealtimeEchoCanceller};
pub use error::{MeetingError, Result};
pub use export::{export_json, export_markdown, export_srt, export_text, ExportFormat};
pub use model::{
    ActionItem, AudioRetention, AudioSource, Bookmark, Meeting, MeetingId, MeetingSource,
    MeetingStatus, MeetingSummary, NewMeeting, NoteRef, OpenQuestion, Speaker, TranscriptSegment,
    SCHEMA_VERSION,
};
pub use search::{search_meetings, SearchHit};
pub use store::{MeetingListEntry, MeetingStore};
pub use summary::{build_local_summary, SummaryOptions};
