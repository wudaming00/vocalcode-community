//! vocalcode-core — the platform-agnostic heart of VocalCode.
//!
//! Contains only traits, plain data types, the config schema, and the
//! push-to-talk state machine ([`engine::Engine`]). No OS calls live here;
//! each platform implements the traits in `vocalcode-platform` and hands the
//! implementations to the engine. Adding macOS or Linux means writing new
//! trait impls, not touching this crate.

pub mod config;
pub mod engine;
pub mod error;
pub mod fillers;
pub mod license;
pub mod limits;
pub mod migration;
pub mod resample;
pub mod segmentation;
pub mod traits;
pub mod trigger_bus;
pub mod writing;

pub use config::{Config, MouseExtra, Trigger};
pub use engine::{Engine, Outcome};
pub use error::{Result, VocalCodeError};
pub use license::{evaluate, verify_token, LicenseClaims, LicenseStatus};
pub use traits::{
    Asr, AudioCapture, HotkeyListener, Recording, TextCleaner, TextInjector, TriggerEvent,
};
pub use trigger_bus::{
    trigger_event_channel, TriggerEventReceiver, TriggerEventSender, TRIGGER_ACTION_QUEUE_CAPACITY,
};
