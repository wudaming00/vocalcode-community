use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum MeetingError {
    #[error("meeting data is invalid: {0}")]
    Invalid(String),
    #[error("meeting data is too large: {0}")]
    TooLarge(String),
    #[error("unsupported audio: {0}")]
    UnsupportedAudio(String),
    #[error("audio decode failed: {0}")]
    AudioDecode(String),
    #[error("meeting storage failed at {path}: {source}")]
    Storage {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("meeting JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("meeting WAV operation failed: {0}")]
    Wav(#[from] hound::Error),
}

impl MeetingError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Storage {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, MeetingError>;
