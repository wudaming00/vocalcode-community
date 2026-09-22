use thiserror::Error;

/// Errors surfaced by the portable core. Platform layers map their own
/// OS errors into these so the core never sees a platform-specific type.
#[derive(Debug, Error)]
pub enum VocalCodeError {
    #[error("audio device error: {0}")]
    Audio(String),

    #[error("speech recognition error: {0}")]
    Asr(String),

    #[error("text injection error: {0}")]
    Inject(String),

    #[error("hotkey/input error: {0}")]
    Hotkey(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("license error: {0}")]
    License(String),

    /// The words survived; only their destination changed. Delivery could not
    /// type into any focused control, so the transcript went to the clipboard.
    /// It carries a complete, user-facing sentence and is deliberately not
    /// prefixed with a failure lead-in at the point it is displayed: telling
    /// somebody who just dictated a paragraph that an action failed, when their
    /// words are sitting on the clipboard, is how a recovery reads as a loss.
    #[error("{0}")]
    Diverted(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, VocalCodeError>;
