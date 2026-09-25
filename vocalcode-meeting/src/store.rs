use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::{
    Meeting, MeetingError, MeetingId, MeetingSource, MeetingStatus, NewMeeting, Result,
    TranscriptSegment, SCHEMA_VERSION,
};

const METADATA_FILE: &str = "meeting.json";
const TRANSCRIPT_FILE: &str = "transcript.jsonl";
const AUDIO_DIRECTORY: &str = "audio";
const MAX_METADATA_BYTES: u64 = 4 * 1024 * 1024;
const MAX_TRANSCRIPT_BYTES: u64 = 512 * 1024 * 1024;
static NONCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeetingListEntry {
    pub id: MeetingId,
    pub title: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub duration_ms: u64,
    pub status: MeetingStatus,
    pub source: MeetingSource,
    pub language: String,
    pub segment_count: usize,
}

#[derive(Debug, Clone)]
pub struct MeetingStore {
    root: PathBuf,
}

impl MeetingStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let requested = root.as_ref();
        fs::create_dir_all(requested).map_err(|error| MeetingError::io(requested, error))?;
        let metadata =
            fs::symlink_metadata(requested).map_err(|error| MeetingError::io(requested, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(MeetingError::Invalid(
                "meeting store root must be a real directory".to_string(),
            ));
        }
        let root =
            fs::canonicalize(requested).map_err(|error| MeetingError::io(requested, error))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn create(&self, new_meeting: NewMeeting) -> Result<Meeting> {
        new_meeting.validate()?;
        for _ in 0..1_000 {
            let id = MeetingId::parse(format!(
                "{:013}-{}-{}",
                new_meeting.now_ms,
                process::id(),
                NONCE.fetch_add(1, Ordering::Relaxed)
            ))?;
            let directory = self.root.join(id.as_str());
            match fs::create_dir(&directory) {
                Ok(()) => {
                    let meeting = Meeting {
                        schema_version: SCHEMA_VERSION,
                        id,
                        title: new_meeting.title,
                        created_at_ms: new_meeting.now_ms,
                        updated_at_ms: new_meeting.now_ms,
                        started_at_ms: new_meeting.now_ms,
                        ended_at_ms: None,
                        duration_ms: 0,
                        segment_count: 0,
                        status: MeetingStatus::Recording,
                        source: new_meeting.source,
                        language: new_meeting.language,
                        audio_retention: new_meeting.audio_retention,
                        speakers: Vec::new(),
                        bookmarks: Vec::new(),
                        summary: None,
                        error: None,
                        warnings: Vec::new(),
                        end_reason: None,
                        filtered_noise_segments: 0,
                        segments: Vec::new(),
                    };
                    self.save(&meeting)?;
                    return Ok(meeting);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(MeetingError::io(directory, error)),
            }
        }
        Err(MeetingError::Invalid(
            "could not allocate a unique meeting identifier".to_string(),
        ))
    }

    pub fn save(&self, meeting: &Meeting) -> Result<()> {
        meeting.validate()?;
        let directory = self.checked_meeting_directory(&meeting.id)?;
        let metadata = serde_json::to_vec_pretty(meeting)?;
        if metadata.len() as u64 > MAX_METADATA_BYTES {
            return Err(MeetingError::TooLarge(
                "meeting metadata exceeds the local limit".to_string(),
            ));
        }
        atomic_write(&directory.join(METADATA_FILE), &metadata)
    }

    pub fn append_segment(&self, id: &MeetingId, segment: &TranscriptSegment) -> Result<()> {
        segment.validate()?;
        let directory = self.checked_meeting_directory(id)?;
        let path = directory.join(TRANSCRIPT_FILE);
        let mut encoded = serde_json::to_vec(segment)?;
        if encoded.len() > crate::model::MAX_TRANSCRIPT_SEGMENT_BYTES * 2 {
            return Err(MeetingError::TooLarge(
                "encoded transcript segment is too large".to_string(),
            ));
        }
        encoded.push(b'\n');
        let existing = fs::metadata(&path)
            .map(|value| value.len())
            .unwrap_or_default();
        if existing.saturating_add(encoded.len() as u64) > MAX_TRANSCRIPT_BYTES {
            return Err(MeetingError::TooLarge(
                "meeting transcript exceeds the local limit".to_string(),
            ));
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| MeetingError::io(&path, error))?;
        file.write_all(&encoded)
            .map_err(|error| MeetingError::io(&path, error))?;
        file.sync_data()
            .map_err(|error| MeetingError::io(&path, error))
    }

    /// Atomically replaces the durable transcript after bounded post-processing.
    ///
    /// Live capture remains append-only so a crash cannot lose committed speech.
    /// Completion-time cleanup (for example, removing cross-track echo) uses this
    /// method so readers never observe a partially rewritten JSONL file.
    pub fn replace_transcript(&self, id: &MeetingId, segments: &[TranscriptSegment]) -> Result<()> {
        let directory = self.checked_meeting_directory(id)?;
        let mut encoded = Vec::new();
        for segment in segments {
            segment.validate()?;
            let line = serde_json::to_vec(segment)?;
            if line.len() > crate::model::MAX_TRANSCRIPT_SEGMENT_BYTES * 2 {
                return Err(MeetingError::TooLarge(
                    "encoded transcript segment is too large".to_string(),
                ));
            }
            if encoded.len().saturating_add(line.len()).saturating_add(1)
                > MAX_TRANSCRIPT_BYTES as usize
            {
                return Err(MeetingError::TooLarge(
                    "meeting transcript exceeds the local limit".to_string(),
                ));
            }
            encoded.extend_from_slice(&line);
            encoded.push(b'\n');
        }
        atomic_write(&directory.join(TRANSCRIPT_FILE), &encoded)
    }

    pub fn load(&self, id: &MeetingId) -> Result<Meeting> {
        let directory = self.checked_meeting_directory(id)?;
        let mut meeting = read_metadata(&directory)?;
        if &meeting.id != id {
            return Err(MeetingError::Invalid(
                "meeting metadata identifier does not match its directory".to_string(),
            ));
        }
        let transcript_path = directory.join(TRANSCRIPT_FILE);
        meeting.segments = if transcript_path.exists() {
            read_transcript(&transcript_path)?
        } else {
            Vec::new()
        };
        // JSONL is the durable source of truth. A crash can occur after its
        // fsync and before the following atomic metadata save.
        meeting.segment_count = meeting.segments.len();
        meeting.validate()?;
        let mut ids = HashSet::new();
        if meeting
            .segments
            .iter()
            .any(|segment| !ids.insert(segment.id))
        {
            return Err(MeetingError::Invalid(
                "meeting transcript contains a duplicate segment id".to_string(),
            ));
        }
        Ok(meeting)
    }

    pub fn list(&self) -> Result<Vec<MeetingListEntry>> {
        let mut entries = Vec::new();
        let directories =
            fs::read_dir(&self.root).map_err(|error| MeetingError::io(&self.root, error))?;
        for item in directories {
            let item = item.map_err(|error| MeetingError::io(&self.root, error))?;
            let file_type = item
                .file_type()
                .map_err(|error| MeetingError::io(item.path(), error))?;
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let Some(name) = item.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(id) = MeetingId::parse(name) else {
                continue;
            };
            let directory = self.checked_meeting_directory(&id)?;
            let meeting = read_metadata(&directory)?;
            if meeting.id != id {
                return Err(MeetingError::Invalid(
                    "meeting metadata identifier does not match its directory".to_string(),
                ));
            }
            meeting.validate()?;
            entries.push(MeetingListEntry {
                id: meeting.id,
                title: meeting.title,
                created_at_ms: meeting.created_at_ms,
                updated_at_ms: meeting.updated_at_ms,
                duration_ms: meeting.duration_ms,
                status: meeting.status,
                source: meeting.source,
                language: meeting.language,
                segment_count: meeting.segment_count,
            });
        }
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at_ms));
        Ok(entries)
    }

    pub fn recover_interrupted(&self, now_ms: u64) -> Result<Vec<MeetingId>> {
        let mut recovered = Vec::new();
        for entry in self.list()? {
            if matches!(
                entry.status,
                MeetingStatus::Recording | MeetingStatus::Processing
            ) {
                let mut meeting = self.load(&entry.id)?;
                meeting.status = MeetingStatus::Interrupted;
                meeting.updated_at_ms = now_ms;
                // The app may restart days later. Recovery time is not recorded
                // meeting time; preserve only the last durable duration/evidence.
                meeting.duration_ms = meeting.duration_ms.max(
                    meeting
                        .segments
                        .iter()
                        .map(|segment| segment.end_ms)
                        .max()
                        .unwrap_or(0),
                );
                meeting.ended_at_ms =
                    Some(meeting.started_at_ms.saturating_add(meeting.duration_ms));
                let audio = self
                    .checked_meeting_directory(&meeting.id)?
                    .join(AUDIO_DIRECTORY);
                let has_audio = fs::symlink_metadata(&audio)
                    .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
                    && fs::read_dir(&audio).is_ok_and(|files| {
                        files.filter_map(std::result::Result::ok).any(|file| {
                            file.path().extension().is_some_and(|ext| ext == "wav")
                                && file.file_type().is_ok_and(|kind| kind.is_file())
                                && file.metadata().is_ok_and(|meta| meta.len() > 44)
                        })
                    });
                meeting.error = Some(if has_audio {
                    "VocalCode stopped before this meeting completed. Saved transcript and audio chunks are available locally; the final uncommitted audio may be missing."
                } else {
                    "VocalCode stopped before this meeting completed. Any saved transcript is available locally, but no recoverable audio chunks were found."
                }.to_string());
                self.save(&meeting)?;
                recovered.push(meeting.id);
            }
        }
        Ok(recovered)
    }

    pub fn audio_directory(&self, id: &MeetingId) -> Result<PathBuf> {
        let directory = self.checked_meeting_directory(id)?.join(AUDIO_DIRECTORY);
        fs::create_dir_all(&directory).map_err(|error| MeetingError::io(&directory, error))?;
        let metadata = fs::symlink_metadata(&directory)
            .map_err(|error| MeetingError::io(&directory, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(MeetingError::Invalid(
                "meeting audio path is not a real directory".to_string(),
            ));
        }
        Ok(directory)
    }

    pub fn delete_audio(&self, id: &MeetingId) -> Result<()> {
        let directory = self.checked_meeting_directory(id)?.join(AUDIO_DIRECTORY);
        if !directory.exists() {
            return Ok(());
        }
        ensure_real_directory(&directory)?;
        fs::remove_dir_all(&directory).map_err(|error| MeetingError::io(directory, error))
    }

    pub fn delete(&self, id: &MeetingId) -> Result<()> {
        let directory = self.checked_meeting_directory(id)?;
        fs::remove_dir_all(&directory).map_err(|error| MeetingError::io(directory, error))
    }

    fn checked_meeting_directory(&self, id: &MeetingId) -> Result<PathBuf> {
        MeetingId::parse(id.to_string())?;
        let directory = self.root.join(id.as_str());
        ensure_real_directory(&directory)?;
        let canonical =
            fs::canonicalize(&directory).map_err(|error| MeetingError::io(&directory, error))?;
        if canonical.parent() != Some(self.root.as_path()) {
            return Err(MeetingError::Invalid(
                "meeting directory escaped the local store".to_string(),
            ));
        }
        Ok(canonical)
    }
}

fn ensure_real_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| MeetingError::io(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(MeetingError::Invalid(
            "meeting path is not a real directory".to_string(),
        ));
    }
    Ok(())
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let metadata = fs::metadata(path).map_err(|error| MeetingError::io(path, error))?;
    if metadata.len() > limit {
        return Err(MeetingError::TooLarge(format!(
            "{} exceeds its local size limit",
            path.display()
        )));
    }
    let file = File::open(path).map_err(|error| MeetingError::io(path, error))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| MeetingError::io(path, error))?;
    if bytes.len() as u64 > limit {
        return Err(MeetingError::TooLarge(format!(
            "{} exceeds its local size limit",
            path.display()
        )));
    }
    Ok(bytes)
}

fn read_metadata(directory: &Path) -> Result<Meeting> {
    let metadata_path = directory.join(METADATA_FILE);
    let bytes = read_bounded(&metadata_path, MAX_METADATA_BYTES)?;
    serde_json::from_slice(&bytes).map_err(MeetingError::from)
}

fn read_transcript(path: &Path) -> Result<Vec<TranscriptSegment>> {
    let bytes = read_bounded(path, MAX_TRANSCRIPT_BYTES)?;
    let ends_with_newline = bytes.ends_with(b"\n");
    let lines: Vec<_> = bytes.split(|byte| *byte == b'\n').collect();
    let mut segments = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<TranscriptSegment>(line) {
            Ok(segment) => {
                segment.validate()?;
                segments.push(segment);
            }
            Err(_) if index + 1 == lines.len() && !ends_with_newline => {
                // A power loss can leave only the final JSONL record incomplete.
            }
            Err(error) => return Err(MeetingError::Json(error)),
        }
    }
    Ok(segments)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| MeetingError::Invalid("metadata path has no parent".to_string()))?;
    let temp = parent.join(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("meeting"),
        process::id(),
        NONCE.fetch_add(1, Ordering::Relaxed)
    ));
    let operation = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|error| MeetingError::io(&temp, error))?;
        file.write_all(bytes)
            .map_err(|error| MeetingError::io(&temp, error))?;
        file.sync_all()
            .map_err(|error| MeetingError::io(&temp, error))?;
        replace_file(&temp, path)
    })();
    if operation.is_err() {
        let _ = fs::remove_file(&temp);
    }
    operation
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination).map_err(|error| MeetingError::io(destination, error))
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination_wide: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let success = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if success == 0 {
        Err(MeetingError::io(
            destination,
            std::io::Error::last_os_error(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use crate::{AudioRetention, AudioSource, Speaker};

    fn test_store(label: &str) -> (TempDir, MeetingStore) {
        let root = TempDir::new(&format!("meeting-{label}"));
        let store = MeetingStore::open(&root).unwrap();
        (root, store)
    }

    fn create(store: &MeetingStore) -> Meeting {
        let mut meeting = store
            .create(NewMeeting {
                title: "Private planning call".to_string(),
                now_ms: 1_787_796_747_000,
                source: MeetingSource::Imported {
                    file_name: "call.wav".to_string(),
                },
                language: "auto".to_string(),
                audio_retention: AudioRetention::DeleteAfterTranscription,
            })
            .unwrap();
        meeting.speakers.push(Speaker {
            id: "speaker-1".to_string(),
            label: "Speaker 1".to_string(),
            source: AudioSource::Imported,
        });
        store.save(&meeting).unwrap();
        meeting
    }

    fn segment(id: u64) -> TranscriptSegment {
        TranscriptSegment {
            id,
            start_ms: id * 1_000,
            end_ms: id * 1_000 + 500,
            speaker_id: "speaker-1".to_string(),
            source: AudioSource::Imported,
            text: format!("Segment {id}"),
        }
    }

    #[test]
    fn create_append_save_and_reload() {
        let (_root, store) = test_store("roundtrip");
        let mut meeting = create(&store);
        store.append_segment(&meeting.id, &segment(1)).unwrap();
        meeting.segment_count = 1;
        meeting.status = MeetingStatus::Completed;
        meeting.ended_at_ms = Some(meeting.started_at_ms + 2_000);
        meeting.duration_ms = 2_000;
        store.save(&meeting).unwrap();
        let loaded = store.load(&meeting.id).unwrap();
        assert_eq!(loaded.status, MeetingStatus::Completed);
        assert_eq!(loaded.segments, vec![segment(1)]);
        assert_eq!(store.list().unwrap()[0].segment_count, 1);
    }

    #[test]
    fn completed_transcript_can_be_atomically_post_processed() {
        let (_root, store) = test_store("replace-transcript");
        let meeting = create(&store);
        store.append_segment(&meeting.id, &segment(1)).unwrap();
        store.append_segment(&meeting.id, &segment(2)).unwrap();

        store
            .replace_transcript(&meeting.id, &[segment(2)])
            .unwrap();

        let loaded = store.load(&meeting.id).unwrap();
        assert_eq!(loaded.segments, vec![segment(2)]);
        assert_eq!(loaded.segment_count, 1);
    }

    #[test]
    fn listing_uses_metadata_and_load_reconciles_a_crash_after_jsonl_fsync() {
        let (_root, store) = test_store("metadata-list");
        let meeting = create(&store);
        store.append_segment(&meeting.id, &segment(1)).unwrap();
        // Simulate power loss before meeting.json was updated. Listing stays
        // bounded and reports the last committed metadata value; loading the
        // detail recovers the durable JSONL record.
        assert_eq!(store.list().unwrap()[0].segment_count, 0);
        let loaded = store.load(&meeting.id).unwrap();
        assert_eq!(loaded.segment_count, 1);
        assert_eq!(loaded.segments, vec![segment(1)]);
    }

    #[test]
    fn ignores_only_an_incomplete_final_json_record() {
        let (root, store) = test_store("partial");
        let meeting = create(&store);
        store.append_segment(&meeting.id, &segment(1)).unwrap();
        let transcript = root.join(meeting.id.as_str()).join(TRANSCRIPT_FILE);
        let mut file = OpenOptions::new().append(true).open(&transcript).unwrap();
        file.write_all(b"{\"id\":2").unwrap();
        file.sync_all().unwrap();
        assert_eq!(store.load(&meeting.id).unwrap().segments.len(), 1);
    }

    #[test]
    fn recovers_recording_and_processing_states() {
        let (_root, store) = test_store("recovery");
        let meeting = create(&store);
        let recovered = store
            .recover_interrupted(meeting.started_at_ms + 5_000)
            .unwrap();
        assert_eq!(recovered, vec![meeting.id.clone()]);
        assert_eq!(
            store.load(&meeting.id).unwrap().status,
            MeetingStatus::Interrupted
        );
    }
}
