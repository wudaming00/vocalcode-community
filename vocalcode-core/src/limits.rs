//! Hard bounds for small, user-controlled documents and IPC payloads.
//!
//! These values are shared by the portable core and the native settings host so
//! a value accepted at one boundary cannot become an unbounded allocation or a
//! pathological hot-path workload at the next one.

/// Maximum UTF-8 size of one privileged Settings WebView IPC message.
pub const MAX_SETTINGS_IPC_BYTES: usize = 4 * 1024 * 1024;

/// Replacement dictionaries are intentionally small, ordered rule sets.
pub const MAX_DICTIONARY_RULES: usize = 512;
pub const MAX_DICTIONARY_SIDE_UTF8_BYTES: usize = 16 * 1024;
pub const MAX_DICTIONARY_DOCUMENT_BYTES: usize = 1024 * 1024;

/// A ten-minute transcript is normally only tens of KiB. This leaves ample
/// headroom while stopping chained replacements from growing exponentially.
pub const MAX_DICTIONARY_OUTPUT_BYTES: usize = 1024 * 1024;

/// Aggregate scan/allocation budget while applying one ordered dictionary.
/// `ci_replace` builds Unicode boundary metadata for each rule; bounding only
/// the final transcript still allowed hundreds of full 1 MiB rebuilds.
pub const MAX_DICTIONARY_WORK_BYTES: usize = 256 * 1024 * 1024;

/// Settings/config values that are lists or opaque platform identifiers.
pub const MAX_TRIGGERS_PER_ACTION: usize = 32;
pub const MAX_TRIGGER_KEY_UTF8_BYTES: usize = 64;
pub const MAX_TRIGGER_SELECTOR_UTF8_BYTES: usize = 2 * 1024;
pub const MAX_SERIALIZED_TRIGGER_UTF8_BYTES: usize = 16 * 1024;
pub const MAX_INPUT_DEVICE_UTF8_BYTES: usize = 4 * 1024;
pub const MAX_CONFIG_TOKEN_UTF8_BYTES: usize = 128;
pub const MAX_UI_LANGUAGE_UTF8_BYTES: usize = 32;

/// Config and totals are tiny structured documents, never model/artifact data.
pub const MAX_CONFIG_DOCUMENT_BYTES: usize = 1024 * 1024;
pub const MAX_TOTALS_DOCUMENT_BYTES: usize = 64 * 1024;
