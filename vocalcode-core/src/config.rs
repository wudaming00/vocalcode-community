use std::collections::HashSet;

use serde::{Deserialize, Deserializer, Serialize};

use crate::limits::{
    MAX_CONFIG_TOKEN_UTF8_BYTES, MAX_INPUT_DEVICE_UTF8_BYTES, MAX_TRIGGERS_PER_ACTION,
    MAX_TRIGGER_KEY_UTF8_BYTES, MAX_TRIGGER_SELECTOR_UTF8_BYTES, MAX_UI_LANGUAGE_UTF8_BYTES,
};

pub const MAX_IGNORED_MEETING_APPS: usize = 32;
pub const MAX_APP_STYLES: usize = 100;
pub const MAX_APP_STYLE_ID_UTF8_BYTES: usize = 200;

/// Opt-in writing features for complete on-release dictation. Every rule is
/// off until chosen, so an upgrade never starts rewriting text on its own.
///
/// Kept as its own `[writing]` table: an older build that does not know it
/// ignores the table instead of refusing the whole settings file, which keeps
/// an update reversible.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WritingConfig {
    /// "new line" / "new paragraph" (换行 / 另起一段).
    pub commands: bool,
    /// "scratch that" (删掉上一句) retracts what was said just before it.
    pub backtrack: bool,
    /// "first … second …" (第一，… 第二，…) becomes a numbered list.
    pub lists: bool,
    /// "camel case user id" → userId; "open paren" → "(".
    pub code: bool,
    /// Ending a dictation with "press enter" (回车) sends it.
    pub press_enter: bool,
    /// Default style for every application: formal | casual | very_casual.
    pub style: String,
    /// Exact executable filename (Windows) or bundle ID (macOS) → style.
    pub app_styles: std::collections::BTreeMap<String, String>,
}

impl Default for WritingConfig {
    fn default() -> Self {
        Self {
            commands: false,
            backtrack: false,
            lists: false,
            code: false,
            press_enter: false,
            style: "formal".to_string(),
            app_styles: std::collections::BTreeMap::new(),
        }
    }
}

impl WritingConfig {
    /// The rules for one dictation into `app` (exact identity, compared
    /// case-insensitively; an empty `app` means unknown).
    pub fn options_for(&self, app: &str) -> crate::writing::Options {
        let style = (!app.is_empty())
            .then(|| {
                self.app_styles
                    .iter()
                    .find(|(id, _)| id.eq_ignore_ascii_case(app))
                    .map(|(_, style)| style.as_str())
            })
            .flatten()
            .unwrap_or(&self.style);
        crate::writing::Options {
            commands: self.commands,
            backtrack: self.backtrack,
            lists: self.lists,
            code: self.code,
            press_enter: self.press_enter,
            style: crate::writing::Style::parse(style).unwrap_or_default(),
        }
    }

    /// Whether any per-application style needs the foreground identity.
    pub fn needs_app_identity(&self) -> bool {
        !self.app_styles.is_empty()
    }

    pub fn validate(&self) -> Result<(), String> {
        if crate::writing::Style::parse(&self.style).is_none() {
            return Err("writing style must be formal, casual or very_casual".into());
        }
        if self.app_styles.len() > MAX_APP_STYLES {
            return Err(format!(
                "at most {MAX_APP_STYLES} application styles can be saved"
            ));
        }
        let mut seen = HashSet::new();
        for (app, style) in &self.app_styles {
            if app.is_empty()
                || app.trim() != app
                || app.len() > MAX_APP_STYLE_ID_UTF8_BYTES
                || !app
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' '))
            {
                return Err("application styles need an exact executable filename or bundle ID, not a path or window title".into());
            }
            if !seen.insert(app.to_ascii_lowercase()) {
                return Err("application styles contain the same application twice".into());
            }
            if crate::writing::Style::parse(style).is_none() {
                return Err("writing style must be formal, casual or very_casual".into());
            }
        }
        Ok(())
    }
}
pub const MAX_MEETING_APP_KEY_UTF8_BYTES: usize = 160;

/// A durable selector for the physical device that owns a trigger.
///
/// An empty selector means "any matching device".  `stable_id` is an opaque,
/// platform-produced identifier (for example a hash of a Windows Raw Input
/// device interface path); VID/PID/serial are kept alongside it so a binding
/// can survive a port change when the hardware exposes a serial number.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(default)]
pub struct DeviceSelector {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stable_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor_id: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_id: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
}

impl DeviceSelector {
    pub fn any() -> Self {
        Self::default()
    }

    pub fn is_any(&self) -> bool {
        self.stable_id.is_none()
            && self.vendor_id.is_none()
            && self.product_id.is_none()
            && self.serial.is_none()
    }

    /// Match a concrete input source. Missing selector fields are wildcards.
    pub fn matches(
        &self,
        stable_id: Option<&str>,
        vendor_id: Option<u16>,
        product_id: Option<u16>,
        serial: Option<&str>,
    ) -> bool {
        optional_field_matches(self.stable_id.as_deref(), stable_id)
            && optional_field_matches(self.vendor_id, vendor_id)
            && optional_field_matches(self.product_id, product_id)
            && optional_field_matches(self.serial.as_deref(), serial)
    }

    /// Whether at least one physical device could satisfy both selectors.
    ///
    /// A missing field is a wildcard.  When the two selectors constrain
    /// different fields (for example one has only a stable id and the other
    /// only a VID/PID), we conservatively report overlap: without the device
    /// inventory there may be a device that satisfies both.
    pub fn overlaps(&self, other: &Self) -> bool {
        optional_fields_overlap(self.stable_id.as_deref(), other.stable_id.as_deref())
            && optional_fields_overlap(self.vendor_id, other.vendor_id)
            && optional_fields_overlap(self.product_id, other.product_id)
            && optional_fields_overlap(self.serial.as_deref(), other.serial.as_deref())
    }
}

fn optional_field_matches<T: PartialEq>(wanted: Option<T>, actual: Option<T>) -> bool {
    match wanted {
        Some(wanted) => actual == Some(wanted),
        None => true,
    }
}

fn optional_fields_overlap<T: PartialEq>(left: Option<T>, right: Option<T>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}

/// Platform-neutral gamepad button names. Their serialized spelling is stable
/// even when Windows changes a controller's transient XInput user index.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum GamepadButton {
    South,
    East,
    West,
    North,
    LeftShoulder,
    RightShoulder,
    LeftThumb,
    RightThumb,
    Start,
    Back,
    Guide,
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight,
}

/// Which physical control acts as the push-to-talk "talk" trigger.
/// Kept as a small serializable enum so the platform layer maps it to the
/// real OS key/button, and the user can rebind to any device.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// A mouse extra button, e.g. the forward thumb button (X2).
    MouseButton(MouseExtra),
    /// A keyboard key identified by a platform-neutral name (e.g. "CapsLock").
    Key(String),
    /// A gamepad button, from XInput/GameInput or a generic HID gamepad.
    GamepadButton {
        #[serde(default)]
        device: DeviceSelector,
        button: GamepadButton,
    },
    /// A HID Consumer-page control such as play/pause or volume up.
    ConsumerControl {
        #[serde(default)]
        device: DeviceSelector,
        /// USB HID usage on usage page 0x0c.
        usage: u16,
    },
    /// A generic HID button/control not covered by keyboard, mouse or consumer
    /// controls. `control` is the usage (or a report-bit fallback when a device
    /// does not publish usable HID descriptors).
    HidButton {
        #[serde(default)]
        device: DeviceSelector,
        usage_page: u16,
        usage: u16,
        #[serde(default)]
        control: u32,
    },
}

/// Return true when two saved bindings can fire for the same physical input.
///
/// This is intentionally stronger than `PartialEq`: a device wildcard overlaps
/// a device-specific selector, HID `control = 0` overlaps every report control,
/// and legacy key aliases overlap their canonical spelling.  Settings must use
/// this predicate for cross-role conflict checks because the runtime dispatches
/// in talk -> send -> teach order and would otherwise silently shadow a binding.
pub fn triggers_overlap(left: &Trigger, right: &Trigger) -> bool {
    match (left, right) {
        (Trigger::MouseButton(left), Trigger::MouseButton(right)) => left == right,
        (Trigger::Key(left), Trigger::Key(right)) => canonical_key(left) == canonical_key(right),
        (
            Trigger::GamepadButton {
                device: left_device,
                button: left_button,
            },
            Trigger::GamepadButton {
                device: right_device,
                button: right_button,
            },
        ) => left_button == right_button && left_device.overlaps(right_device),
        (
            Trigger::ConsumerControl {
                device: left_device,
                usage: left_usage,
            },
            Trigger::ConsumerControl {
                device: right_device,
                usage: right_usage,
            },
        ) => left_usage == right_usage && left_device.overlaps(right_device),
        (
            Trigger::HidButton {
                device: left_device,
                usage_page: left_page,
                usage: left_usage,
                control: left_control,
            },
            Trigger::HidButton {
                device: right_device,
                usage_page: right_page,
                usage: right_usage,
                control: right_control,
            },
        ) => {
            left_page == right_page
                && left_usage == right_usage
                && (*left_control == 0 || *right_control == 0 || left_control == right_control)
                && left_device.overlaps(right_device)
        }
        _ => false,
    }
}

fn canonical_key(name: &str) -> &str {
    canonical_key_name(name).unwrap_or(name)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MouseExtra {
    /// Back thumb button.
    X1,
    /// Forward thumb button.
    X2,
}

impl Default for Trigger {
    /// Hold the forward thumb button to talk — the hand is already on the mouse.
    #[cfg(not(target_os = "macos"))]
    fn default() -> Self {
        Trigger::MouseButton(MouseExtra::X2)
    }

    /// Hold the right Option key to talk.
    ///
    /// Mice with thumb buttons are the exception on macOS, not the rule: every
    /// MacBook has a trackpad, and Apple's own Magic Mouse and Magic Trackpad
    /// have no extra buttons at all. Defaulting to a thumb button there ships an
    /// app that does nothing on first launch for most users, with no clue why.
    ///
    /// Right Option is present on every Mac keyboard, is almost never used on
    /// its own, and sits under the thumb. The thumb-button triggers still work
    /// for anyone who does have such a mouse — they are just not the default.
    #[cfg(target_os = "macos")]
    fn default() -> Self {
        Trigger::Key("AltRight".to_string())
    }
}

/// How long recent dictations are kept on this device, so History still has
/// them after a restart. Kept entries are encrypted with the same account /
/// Keychain protection as local diagnostics and capped at
/// [`HistoryRetention::MAX_ENTRIES`].
///
/// This governs what is written to disk, never the running session: the
/// in-memory list works exactly as it always has, and text that could not be
/// typed stays there until the app quits whatever is chosen here.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum HistoryRetention {
    /// Nothing is written; History holds this session only.
    #[serde(rename = "off")]
    Off,
    #[serde(rename = "24h")]
    Day,
    #[default]
    #[serde(rename = "7d")]
    Week,
}

impl HistoryRetention {
    /// The same bound as the in-memory session list.
    pub const MAX_ENTRIES: usize = 50;

    /// Oldest age still kept, or `None` when nothing is kept at all.
    pub fn max_age_secs(self) -> Option<u64> {
        match self {
            Self::Off => None,
            Self::Day => Some(24 * 60 * 60),
            Self::Week => Some(7 * 24 * 60 * 60),
        }
    }

    pub fn keeps(self) -> bool {
        self != Self::Off
    }

    /// The spelling used in `vocalcode.toml` and by the settings page.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Day => "24h",
            Self::Week => "7d",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [Self::Off, Self::Day, Self::Week]
            .into_iter()
            .find(|retention| retention.as_str() == value)
    }
}

// `remote = "Self"` turns the derived code into the inherent functions
// `Config::serialize` / `Config::deserialize`. The trait impls further down
// wrap them so that reading a document written by an older release first
// restores the defaults that release implied by leaving a key out; a plain
// derived impl would silently hand such a file today's defaults instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, remote = "Self")]
pub struct Config {
    /// Hold-to-talk triggers. Any one of them starts talking.
    ///
    /// A list rather than a single binding because which input devices are
    /// attached changes minute to minute — a laptop is docked with a mouse and
    /// a full keyboard at a desk, and bare on a train. Probing the hardware
    /// once at startup picks correctly for exactly one of those and is then
    /// silently wrong, with no way for the user to tell why nothing happens.
    ///
    /// Binding several costs nothing: a device that is not attached never emits
    /// events, so the unused entries simply lie dormant. Unplug the mouse and
    /// the keyboard binding takes over with no detection, no rescan, no state.
    #[serde(deserialize_with = "one_or_many")]
    pub talk: Vec<Trigger>,
    /// Tap-to-send triggers (types Enter into the target app). Empty = off,
    /// which is how a new install starts: first run offers the mouse Back
    /// button as an explicit, unticked choice. Configs written before v5 that
    /// leave this out keep the Back button they always had.
    #[serde(deserialize_with = "one_or_many")]
    pub send: Vec<Trigger>,
    /// Keys that grab the current selection into the dictionary. Empty by
    /// default: this steals a global shortcut from every other app, so it is
    /// opt-in rather than something we pick on the user's behalf.
    #[serde(default)]
    pub teach: Vec<Trigger>,
    // `model_dir` used to live here. It was written into every config file and
    // read by nothing — model paths come from `app_dir().join("models")`. A
    // setting that appears in the file the user is invited to hand-edit, and
    // then does nothing, is worse than no setting at all. Removed rather than
    // wired up because nobody asked to relocate the models, and serde ignores
    // the leftover key in configs that still carry it.
    /// Minimum recording length to bother transcribing (ms).
    pub min_record_ms: u32,
    /// Insert via clipboard paste instead of synthesised text.
    ///
    /// Off by default, and that is not timidity. On macOS the synthesised path
    /// is `CGEventKeyboardSetUnicodeString`, which inserts the whole string in
    /// one event and handles 中文 correctly — it is already as fast as pasting.
    /// Pasting costs a clipboard round trip whose restore races the target
    /// application, so it earns its place only for apps that ignore synthesised
    /// text outright.
    pub paste_insert: bool,
    /// How long to watch the exact input control after a successful insertion
    /// for a user correction. `0` disables automatic learning; otherwise the
    /// countdown pauses on the first edit and ends when the control is sent,
    /// cleared, or loses focus.
    #[serde(default = "default_correction_window_ms")]
    pub correction_window_ms: u32,
    /// Append stable phrases to the field at detected pauses while speaking.
    /// Already-inserted phrases are never rewritten. Off = one clean, atomic
    /// insert on release.
    pub live_caption: bool,
    /// Local acoustic gate: a clip with no speech in it is not transcribed.
    /// On-release dictation only, independent of language.
    ///
    /// On for new installs. Without it SenseVoice types junk for fan, keyboard
    /// or pink noise ("I.", "그.", "我。"); with it, 6 of 7 such clips stayed
    /// silent. Configs written before v5 that leave it out keep it off, which
    /// is what they had.
    pub noise_filter: bool,
    /// Legacy opt-in marker, retained across config saves until workflow
    /// preferences can migrate it. A new workflow choice always takes priority.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_diagnostic_history: Option<bool>,
    /// Play a short sound when recording starts and stops.
    ///
    /// On by default, and `#[serde(default = ...)]` so existing configs get it
    /// too: the indicator is the only other signal and it is on one screen, so
    /// anyone with two monitors has no way to tell a live microphone from a
    /// missed keypress until the sentence is already spoken. That is the case
    /// this answers, and it cannot answer it for people who never find the
    /// setting.
    #[serde(default = "default_cue_sounds")]
    pub cue_sounds: bool,
    /// Optional explicit ASR model id. Normal UI selections leave this empty
    /// and resolve the user-selected language through the release registry.
    pub model: String,
    /// Spoken-language preference. `"auto"` is the unchosen first-run sentinel;
    /// the app validates all explicit codes against its release registry.
    pub language: String,
    /// Input (microphone) device name; None/empty = system default.
    #[serde(default)]
    pub input_device: Option<String>,
    /// Launch VocalCode automatically at Windows login.
    #[serde(default)]
    pub autostart: bool,
    /// Offer a local reminder after a supported meeting window stays in front.
    /// Detection never starts capture; the reminder only opens Meetings.
    #[serde(default)]
    pub smart_meeting_reminders: bool,
    /// Application/service identities the person explicitly chose not to see
    /// again. These are opaque local identifiers, never window titles.
    #[serde(default)]
    pub ignored_meeting_apps: Vec<String>,
    /// Interface language: `auto`, `en`, `zh`, `es`, `fr`, or `de`. This is
    /// independent of the spoken-language/model preference above.
    #[serde(default = "default_ui_lang")]
    pub ui_lang: String,
    /// How the talk trigger behaves: "hold" | "toggle".
    ///
    /// `hold` is push-to-talk: record while the key is down. `toggle` latches —
    /// one press starts, the next stops — so a long dictation does not mean
    /// holding a key for a minute, and letting go early cannot cut you off.
    /// Default stays `hold`: it is what every existing install expects, and it
    /// is the mode where the app cannot be left recording by accident.
    #[serde(default = "default_talk_mode")]
    pub talk_mode: String,
    /// How the recording indicator appears: "classic" | "mini" | "off".
    ///
    /// Offering a choice rather than picking one is unusual here, but the
    /// indicator is the only part of the app that draws on top of the user's
    /// work, and how much of that is acceptable genuinely differs: dictating
    /// into a full-screen editor is not the same as dictating into a chat
    /// window on a 13" display. Every comparable product ships this setting.
    #[serde(default = "default_overlay_style")]
    pub overlay_style: String,
    /// Opt-in Windows desktop control. Does not imply an open microphone.
    #[serde(default)]
    pub desktop_control: bool,
    #[serde(default = "default_desktop_control_edge")]
    pub desktop_control_edge: String,
    /// Whether the user has been through the first-run language picker. Until
    /// they have, the app shows the picker and downloads nothing — so it never
    /// fetches a model for a language they don't speak. Existing installs are
    /// marked onboarded by `migrate`, so only fresh installs see the picker.
    #[serde(default)]
    pub onboarded: bool,
    /// Schema version, used only to retire settings that changed meaning.
    /// Absent in every config written before this existed, hence 0.
    #[serde(default)]
    pub config_version: u32,
    /// Hold mode: double-tap the talk key to lock a hands-free recording.
    #[serde(default = "default_double_tap_lock")]
    pub double_tap_lock: bool,
    /// Mute other system audio while dictating, restored afterwards.
    #[serde(default)]
    pub mute_while_dictating: bool,
    /// How long History keeps recent dictations on disk. A week for new
    /// installs; configs written before v5 that leave it out read as `off`,
    /// the session-only list they always had.
    pub keep_history: HistoryRetention,
    /// Opt-in spoken formatting, retraction, lists, coding words and style.
    /// Last, so it serialises as the trailing `[writing]` table.
    #[serde(default)]
    pub writing: WritingConfig,
}

fn default_double_tap_lock() -> bool {
    true
}

/// The current schema version. Bump when an existing key changes meaning —
/// including when leaving a key out starts to mean a different default.
pub const CONFIG_VERSION: u32 = 5;

/// The first version whose documents mean today's defaults for `send`,
/// `noise_filter` and `keep_history` when they leave them out.
const NEW_INSTALL_DEFAULTS_VERSION: u64 = 5;

impl Serialize for Config {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // The inherent, derived function (see `remote = "Self"`).
        Config::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for Config {
    /// Reads any document, including one from an older release, as that
    /// release meant it. Every path that reads settings — startup, the
    /// compare-before-save check, importing another installation's file —
    /// goes through here, so none can hand an existing user a new default by
    /// accident.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut document = serde_json::Value::deserialize(deserializer)?;
        if let serde_json::Value::Object(fields) = &mut document {
            restore_pre_v5_defaults(fields);
        }
        // The inherent, derived function (see `remote = "Self"`).
        Config::deserialize(&document).map_err(|error| {
            // Reading from the buffered document loses the parser's position,
            // so name the offending key instead: a hand-edited file should
            // still say what is wrong with it.
            let key = document.as_object().and_then(|fields| {
                fields.iter().find_map(|(key, value)| {
                    let alone = serde_json::Value::Object(
                        [(key.clone(), value.clone())].into_iter().collect(),
                    );
                    Config::deserialize(&alone).is_err().then_some(key)
                })
            });
            match key {
                Some(key) => serde::de::Error::custom(format_args!("`{key}`: {error}")),
                None => serde::de::Error::custom(error),
            }
        })
    }
}

/// v5 changed what three omitted keys mean. A document older than that (or
/// with no version at all) that leaves one out meant the value it would have
/// been given then, so that value is written back in before the current
/// defaults can apply. An explicit value is always kept as written.
fn restore_pre_v5_defaults(fields: &mut serde_json::Map<String, serde_json::Value>) {
    let version = fields
        .get("config_version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if version >= NEW_INSTALL_DEFAULTS_VERSION {
        return;
    }
    let implied = [
        ("send", serde_json::to_value(pre_v5_send())),
        ("noise_filter", Ok(serde_json::Value::Bool(false))),
        ("keep_history", serde_json::to_value(HistoryRetention::Off)),
    ];
    for (key, value) in implied {
        if let Ok(value) = value {
            fields.entry(key).or_insert(value);
        }
    }
}

impl Config {
    /// Reject pathological settings before they reach quadratic overlap checks,
    /// hot input callbacks, platform APIs, or another serialization pass.
    pub fn validate_bounds(&self) -> Result<(), String> {
        for (name, triggers) in [
            ("talk", self.talk.as_slice()),
            ("send", self.send.as_slice()),
            ("teach", self.teach.as_slice()),
        ] {
            if triggers.len() > MAX_TRIGGERS_PER_ACTION {
                return Err(format!(
                    "{name} has {} triggers; at most {MAX_TRIGGERS_PER_ACTION} are allowed",
                    triggers.len()
                ));
            }
            for trigger in triggers {
                validate_trigger_bounds(trigger)?;
            }
        }

        bounded_config_text("language", &self.language, MAX_CONFIG_TOKEN_UTF8_BYTES)?;
        bounded_config_text("model", &self.model, MAX_CONFIG_TOKEN_UTF8_BYTES)?;
        bounded_config_text("ui_lang", &self.ui_lang, MAX_UI_LANGUAGE_UTF8_BYTES)?;
        bounded_config_text("talk_mode", &self.talk_mode, MAX_CONFIG_TOKEN_UTF8_BYTES)?;
        bounded_config_text(
            "overlay_style",
            &self.overlay_style,
            MAX_CONFIG_TOKEN_UTF8_BYTES,
        )?;
        if !matches!(
            self.desktop_control_edge.as_str(),
            "bottom" | "left" | "right"
        ) {
            return Err("Desktop control edge must be bottom, left or right.".into());
        }
        if let Some(device) = self.input_device.as_deref() {
            bounded_config_text("input_device", device, MAX_INPUT_DEVICE_UTF8_BYTES)?;
        }
        if self.ignored_meeting_apps.len() > MAX_IGNORED_MEETING_APPS {
            return Err(format!(
                "ignored_meeting_apps has {} entries; at most {MAX_IGNORED_MEETING_APPS} are allowed",
                self.ignored_meeting_apps.len()
            ));
        }
        self.writing.validate()?;
        let mut unique_meeting_apps = HashSet::new();
        for app in &self.ignored_meeting_apps {
            if app.is_empty()
                || app.len() > MAX_MEETING_APP_KEY_UTF8_BYTES
                || !app.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':')
                })
            {
                return Err(format!(
                    "ignored meeting application identities must use 1 to {MAX_MEETING_APP_KEY_UTF8_BYTES} ASCII identifier bytes"
                ));
            }
            if !unique_meeting_apps.insert(app) {
                return Err("ignored_meeting_apps contains a duplicate identity".to_string());
            }
        }
        if self.correction_window_ms > 30_000
            || (self.correction_window_ms != 0 && self.correction_window_ms < 2_000)
        {
            return Err(
                "correction_window_ms must be 0 (off) or between 2000 and 30000".to_string(),
            );
        }
        Ok(())
    }

    /// Bring an older config forward.
    ///
    /// v0 → v1: `paste_insert` shipped defaulting to `true` while nothing read
    /// it, so every config on disk carries that value without anyone having
    /// chosen it. Now that the setting works, honouring it would silently move
    /// existing users onto clipboard insertion — a behaviour change none of
    /// them asked for. The inherited value is therefore discarded once.
    pub fn migrate(&mut self) -> bool {
        if self.config_version >= CONFIG_VERSION {
            return false;
        }
        if self.config_version == 0 && self.paste_insert {
            self.paste_insert = false;
        }
        // v1 → v2: onboarding added. Anyone with a config on disk has already
        // used the app, so mark them onboarded — don't interrupt them with the
        // first-run picker. Only genuinely fresh installs get it.
        if self.config_version < 2 {
            self.onboarded = true;
        }
        // v2 -> v3: normalize old/friendly key spellings before the settings UI
        // compares bindings. Previously `rightctrl` and `ControlRight` looked
        // different in JS but resolved to the same physical key in the hook, so
        // one role silently shadowed another. Preserve the first role in the
        // dispatch order (talk -> send -> teach) and discard later collisions.
        if self.config_version < 3 {
            for list in [&mut self.talk, &mut self.send, &mut self.teach] {
                for trigger in list.iter_mut() {
                    if let Trigger::Key(name) = trigger {
                        if let Some(canonical) = canonical_key_name(name) {
                            *name = canonical.to_string();
                        }
                    }
                }
                let mut within = HashSet::new();
                list.retain(|trigger| within.insert(trigger.clone()));
                // rdev's Windows backend reports both keypad Enter and the
                // main Return as the same key. Older builds advertised a
                // NumpadEnter binding that could be saved but never fired; do
                // not carry an inert entry into the new multi-trigger UI.
                #[cfg(windows)]
                list.retain(|trigger| {
                    !matches!(trigger, Trigger::Key(name) if name.eq_ignore_ascii_case("NumpadEnter"))
                });
            }
            let mut claimed = HashSet::new();
            self.talk.retain(|trigger| claimed.insert(trigger.clone()));
            self.send.retain(|trigger| claimed.insert(trigger.clone()));
            self.teach.retain(|trigger| claimed.insert(trigger.clone()));
            if self.talk.is_empty() {
                self.talk = default_talk();
            }
        }
        // v3 -> v4: Chinese used to resolve an empty model override to
        // Paraformer even though the release recommendation had moved to
        // SenseVoice. Only migrate the old automatic/default representation;
        // an explicit Paraformer selection is a user choice and remains
        // authoritative.
        if self.config_version < 4 && self.language == "zh" && self.model.is_empty() {
            self.model = "sensevoice".to_string();
        }
        // v4 -> v5: a new install no longer binds the mouse Back button to
        // Enter, and now gates non-speech noise and keeps a week of encrypted
        // History. Existing installs keep what they had. That cannot be decided here:
        // by now an omitted key and an explicit one look the same. It was
        // decided while reading the document (`restore_pre_v5_defaults`),
        // which is why nothing is left to do for this step.
        self.config_version = CONFIG_VERSION;
        true
    }
}

fn default_ui_lang() -> String {
    "auto".to_string()
}

fn default_overlay_style() -> String {
    "classic".to_string()
}

fn default_desktop_control_edge() -> String {
    "bottom".into()
}

fn default_correction_window_ms() -> u32 {
    8_000
}

/// Teaching is deliberately opt-in: every binding is global and consumed, so a
/// fresh install must not silently take CapsLock (or any other key) away from
/// the rest of the operating system.
fn default_teach() -> Vec<Trigger> {
    Vec::new()
}

/// Canonicalize legacy/friendly spellings shared by every platform.
/// Platform-only aliases (for example PC ScrollLock -> macOS F14) stay in the
/// backend, but names that identify the same physical key everywhere are made
/// identical here so conflict detection and persistence agree.
pub fn canonical_key_name(name: &str) -> Option<&'static str> {
    let folded = name.to_ascii_lowercase();
    Some(match folded.as_str() {
        "capslock" | "caps" => "CapsLock",
        "rightctrl" | "controlright" => "ControlRight",
        "leftctrl" | "controlleft" => "ControlLeft",
        "rightalt" | "altright" | "altgr" => "AltRight",
        "leftalt" | "altleft" => "AltLeft",
        "rightshift" | "shiftright" => "ShiftRight",
        "leftshift" | "shiftleft" => "ShiftLeft",
        "rightmeta" | "metaright" | "rightwin" => "MetaRight",
        "leftmeta" | "metaleft" | "leftwin" => "MetaLeft",
        "scrolllock" | "scroll" => "ScrollLock",
        "printscreen" => "PrintScreen",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "arrowleft" => "ArrowLeft",
        "arrowright" => "ArrowRight",
        "arrowup" => "ArrowUp",
        "arrowdown" => "ArrowDown",
        "backquote" => "Backquote",
        "space" => "Space",
        "tab" => "Tab",
        "f1" => "F1",
        "f2" => "F2",
        "f3" => "F3",
        "f4" => "F4",
        "f5" => "F5",
        "f6" => "F6",
        "f7" => "F7",
        "f8" => "F8",
        "f9" => "F9",
        "f10" => "F10",
        "f11" => "F11",
        "f12" => "F12",
        "f13" => "F13",
        "f14" => "F14",
        "f15" => "F15",
        "f16" => "F16",
        "f17" => "F17",
        "f18" => "F18",
        "f19" => "F19",
        "f20" => "F20",
        "f21" => "F21",
        "f22" => "F22",
        "f23" => "F23",
        "f24" => "F24",
        _ => return None,
    })
}

fn default_talk_mode() -> String {
    "hold".to_string()
}

fn default_cue_sounds() -> bool {
    true
}

/// Accept either a single trigger or a list of them.
///
/// Configs written before triggers became a list hold a bare `[talk]` table,
/// and hand-editing one binding is more natural than writing a one-element
/// array, so both spellings stay valid.
fn one_or_many<'de, D>(deserializer: D) -> Result<Vec<Trigger>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(Trigger),
        Many(Vec<Trigger>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(t) => vec![t],
        OneOrMany::Many(v) => v,
    })
}

/// The shipped hold-to-talk bindings, used until the user picks their own.
///
/// Ordered most- to least-specific. Every entry is dormant unless its device is
/// actually present, so this covers a docked desk setup and a bare laptop at
/// once without choosing between them.
#[cfg(not(target_os = "macos"))]
fn default_talk() -> Vec<Trigger> {
    // Right Alt is AltGr on European layouts and cannot be consumed globally.
    // Right Ctrl keeps every laptop usable while leaving the left Ctrl for
    // ordinary shortcuts; first-run onboarding asks the user to confirm or
    // replace it before normal use.
    vec![
        Trigger::MouseButton(MouseExtra::X2),
        Trigger::Key("ControlRight".to_string()),
    ]
}

fn bounded_config_text(name: &str, value: &str, maximum: usize) -> Result<(), String> {
    if value.len() > maximum {
        return Err(format!(
            "{name} is {} UTF-8 bytes; at most {maximum} are allowed",
            value.len()
        ));
    }
    Ok(())
}

/// Validate the string-bearing parts of one trigger. Numeric HID fields and
/// enum variants are bounded by their types.
pub fn validate_trigger_bounds(trigger: &Trigger) -> Result<(), String> {
    fn selector_field(name: &str, value: Option<&str>) -> Result<(), String> {
        if let Some(value) = value {
            if value.len() > MAX_TRIGGER_SELECTOR_UTF8_BYTES {
                return Err(format!(
                    "trigger {name} is {} UTF-8 bytes; at most {MAX_TRIGGER_SELECTOR_UTF8_BYTES} are allowed",
                    value.len()
                ));
            }
        }
        Ok(())
    }

    let device = match trigger {
        Trigger::Key(name) => {
            if name.is_empty() {
                return Err("trigger key name must not be empty".to_string());
            }
            bounded_config_text("trigger key name", name, MAX_TRIGGER_KEY_UTF8_BYTES)?;
            return Ok(());
        }
        Trigger::MouseButton(_) => return Ok(()),
        Trigger::GamepadButton { device, .. }
        | Trigger::ConsumerControl { device, .. }
        | Trigger::HidButton { device, .. } => device,
    };

    selector_field("stable_id", device.stable_id.as_deref())?;
    selector_field("serial", device.serial.as_deref())?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn default_talk() -> Vec<Trigger> {
    vec![
        // Forward thumb button, when a mouse that has one is attached.
        Trigger::MouseButton(MouseExtra::X2),
        // Full-size Apple keyboards: nothing else claims F13.
        Trigger::Key("F13".to_string()),
        // Always present, including on every MacBook's built-in keyboard.
        // Safe to swallow here in a way right Alt is not on Windows: macOS
        // treats left and right Option identically, so the left one still
        // types every special character.
        Trigger::Key("AltRight".to_string()),
    ]
}

/// Tap-to-send bindings for a new install: none. Send stands in for a key
/// every keyboard already has, and binding it swallows that button in every
/// other application — on a mouse, the Back button browsers and file managers
/// use. First run offers it as an explicit, unticked choice instead.
fn default_send() -> Vec<Trigger> {
    Vec::new()
}

/// What `send` defaulted to before config v5: the mouse Back button on
/// Windows, where a mouse is the norm and the hand is already on it, and
/// nothing on macOS. Only an older document that leaves `send` out gets it.
#[cfg(not(target_os = "macos"))]
fn pre_v5_send() -> Vec<Trigger> {
    vec![Trigger::MouseButton(MouseExtra::X1)]
}

#[cfg(target_os = "macos")]
fn pre_v5_send() -> Vec<Trigger> {
    Vec::new()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            talk: default_talk(),
            send: default_send(),
            teach: default_teach(),
            min_record_ms: 250,
            paste_insert: false,
            correction_window_ms: default_correction_window_ms(),
            live_caption: false, // default: clean insert on release, no live churn
            noise_filter: true,
            local_diagnostic_history: None,
            cue_sounds: default_cue_sounds(),
            model: String::new(), // resolve after the user's language choice
            language: "auto".to_string(),
            input_device: None, // system default mic
            autostart: false,
            smart_meeting_reminders: false,
            ignored_meeting_apps: Vec::new(),
            ui_lang: "auto".to_string(),
            talk_mode: default_talk_mode(),
            overlay_style: default_overlay_style(),
            desktop_control: false,
            desktop_control_edge: default_desktop_control_edge(),
            onboarded: false, // fresh install → show the first-run language picker
            config_version: CONFIG_VERSION,
            double_tap_lock: default_double_tap_lock(),
            mute_while_dictating: false,
            keep_history: HistoryRetention::default(),
            writing: WritingConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn writing_is_opt_in_reversible_and_resolves_per_app() {
        let old: Config = toml::from_str("language = \"en\"").unwrap();
        assert_eq!(old.writing, WritingConfig::default());
        assert!(old.writing.options_for("Slack.exe").is_noop());
        assert!(old.double_tap_lock);
        assert!(!old.mute_while_dictating);

        let mut c = Config::default();
        c.writing.commands = true;
        c.writing.style = "casual".into();
        c.writing
            .app_styles
            .insert("Outlook.exe".into(), "formal".into());
        c.writing
            .app_styles
            .insert("Slack.exe".into(), "very_casual".into());
        let text = toml::to_string(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.writing, c.writing);
        use crate::writing::Style;
        assert_eq!(
            back.writing.options_for("slack.EXE").style,
            Style::VeryCasual
        );
        assert_eq!(back.writing.options_for("OUTLOOK.exe").style, Style::Formal);
        assert_eq!(back.writing.options_for("Code.exe").style, Style::Casual);
        assert_eq!(back.writing.options_for("").style, Style::Casual);
        assert!(back.writing.options_for("Code.exe").commands);

        // An older build's view of the same file: the unknown table is ignored.
        #[derive(serde::Deserialize)]
        struct Older {
            language: String,
        }
        assert_eq!(toml::from_str::<Older>(&text).unwrap().language, c.language);
    }

    #[test]
    fn writing_rejects_paths_titles_and_unknown_styles() {
        let mut c = Config::default();
        c.writing.style = "loud".into();
        assert!(c.validate_bounds().is_err());
        c.writing.style = "formal".into();
        for bad in ["C:\\Apps\\Slack.exe", " Slack.exe", "", "Inbox — Outlook"] {
            let mut d = c.clone();
            d.writing.app_styles.insert(bad.into(), "casual".into());
            assert!(d.validate_bounds().is_err(), "{bad}");
        }
        let mut d = c.clone();
        d.writing
            .app_styles
            .insert("Slack.exe".into(), "shout".into());
        assert!(d.validate_bounds().is_err());
        let mut d = c.clone();
        d.writing
            .app_styles
            .insert("Slack.exe".into(), "casual".into());
        d.writing
            .app_styles
            .insert("SLACK.exe".into(), "formal".into());
        assert!(d.validate_bounds().is_err());
        c.writing
            .app_styles
            .insert("com.tinyspeck.slackmacgap".into(), "casual".into());
        assert!(c.validate_bounds().is_ok());
    }

    #[test]
    fn acoustic_filter_is_independent_and_off_only_for_older_files() {
        let old: Config = toml::from_str("").unwrap();
        assert!(!old.noise_filter);
        let enabled: Config = toml::from_str("noise_filter = true").unwrap();
        assert!(enabled.noise_filter);
        assert_eq!(enabled.language, old.language);
        assert_eq!(enabled.live_caption, old.live_caption);
        assert!(toml::from_str::<Config>("noise_filter = 'true'").is_err());
        assert!(
            toml::from_str::<Config>(&toml::to_string(&enabled).unwrap())
                .unwrap()
                .noise_filter
        );
    }

    #[test]
    fn correction_learning_defaults_to_eight_seconds_and_is_bounded() {
        let mut config = Config::default();
        assert_eq!(config.correction_window_ms, 8_000);
        config.correction_window_ms = 0;
        assert!(config.validate_bounds().is_ok());
        config.correction_window_ms = 1_999;
        assert!(config.validate_bounds().is_err());
        config.correction_window_ms = 30_001;
        assert!(config.validate_bounds().is_err());
    }

    #[test]
    fn meeting_reminders_are_opt_in_and_ignored_identities_are_bounded() {
        let mut config = Config::default();
        assert!(!config.smart_meeting_reminders);
        assert!(config.ignored_meeting_apps.is_empty());
        config
            .ignored_meeting_apps
            .push("browser:chrome.exe:google-meet".to_string());
        assert!(config.validate_bounds().is_ok());
        config
            .ignored_meeting_apps
            .push("contains a space".to_string());
        assert!(config.validate_bounds().is_err());
    }

    #[test]
    fn desktop_controls_are_opt_in_and_edges_survive_roundtrip() {
        let mut old: Config = toml::from_str("").unwrap();
        assert!(!old.desktop_control);
        assert_eq!(old.desktop_control_edge, "bottom");
        old.desktop_control = true;
        for edge in ["bottom", "left", "right"] {
            old.desktop_control_edge = edge.into();
            assert!(old.validate_bounds().is_ok());
            let loaded: Config = toml::from_str(&toml::to_string(&old).unwrap()).unwrap();
            assert!(loaded.desktop_control);
            assert_eq!(loaded.desktop_control_edge, edge);
        }
        old.desktop_control_edge = "offscreen".into();
        assert!(old.validate_bounds().is_err());
        assert!(toml::from_str::<Config>("desktop_control = 'true'").is_err());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn default_talk_has_a_keyboard_fallback() {
        assert_eq!(
            Config::default().talk,
            vec![
                Trigger::MouseButton(MouseExtra::X2),
                Trigger::Key("ControlRight".to_string()),
            ]
        );
    }

    /// Configs written before triggers became a list must still load.
    #[test]
    fn accepts_a_single_trigger_table() {
        let c: Config = toml::from_str("[talk]\nkey = \"AltRight\"\n").unwrap();
        assert_eq!(c.talk, vec![Trigger::Key("AltRight".to_string())]);
    }

    #[test]
    fn accepts_a_list_of_triggers() {
        let c: Config =
            toml::from_str("talk = [{ mouse_button = \"x2\" }, { key = \"F13\" }]\n").unwrap();
        assert_eq!(
            c.talk,
            vec![
                Trigger::MouseButton(MouseExtra::X2),
                Trigger::Key("F13".to_string()),
            ]
        );
    }

    /// An empty list is how "off" is expressed. Unlike a missing key it
    /// survives serialisation, so turning send off in the UI actually sticks.
    #[test]
    fn empty_send_means_off_and_round_trips() {
        let c = Config {
            send: Vec::new(),
            teach: default_teach(),
            ..Config::default()
        };
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert!(back.send.is_empty(), "send came back as {:?}", back.send);
    }

    /// Whatever the user picked has to survive a save/load cycle exactly —
    /// their choice must never be quietly replaced by the shipped default.
    #[test]
    fn user_bindings_round_trip() {
        let c = Config {
            talk: vec![Trigger::Key("F13".to_string())],
            send: vec![Trigger::Key("MetaRight".to_string())],
            ..Config::default()
        };
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.talk, c.talk);
        assert_eq!(back.send, c.send);
    }

    /// A multi-trigger list must survive the round trip too, or a docked setup
    /// would silently lose its mouse binding on the next launch.
    #[test]
    fn multi_trigger_list_round_trips() {
        let c = Config {
            talk: vec![
                Trigger::MouseButton(MouseExtra::X2),
                Trigger::Key("F13".to_string()),
                Trigger::Key("AltRight".to_string()),
            ],
            ..Config::default()
        };
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.talk, c.talk);
    }

    /// Unrelated fields still fall back to the defaults when omitted.
    #[test]
    fn other_fields_still_default() {
        let c: Config = toml::from_str("[talk]\nkey = \"F13\"\n").unwrap();
        assert_eq!(c.min_record_ms, Config::default().min_record_ms);
        assert_eq!(c.language, Config::default().language);
    }

    /// The shipped default must cover both a docked desk and a bare laptop.
    #[test]
    #[cfg(target_os = "macos")]
    fn macos_default_covers_mouse_and_keyboard() {
        let c = Config::default();
        assert!(c.talk.contains(&Trigger::MouseButton(MouseExtra::X2)));
        assert!(c.talk.contains(&Trigger::Key("AltRight".to_string())));
        // Send duplicates Return; not worth swallowing a key for.
        assert!(c.send.is_empty());
    }

    /// Right Alt is AltGr on European layouts — swallowing it system-wide would
    /// stop people typing their own language.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn windows_default_never_binds_right_alt() {
        assert!(!Config::default()
            .talk
            .contains(&Trigger::Key("AltRight".to_string())));
    }

    #[test]
    fn fresh_install_does_not_steal_a_teach_key() {
        assert!(Config::default().teach.is_empty());
    }

    #[test]
    fn structured_device_triggers_round_trip_without_losing_selector() {
        let selector = DeviceSelector {
            stable_id: Some("raw:0123456789abcdef".to_string()),
            vendor_id: Some(0x045e),
            product_id: Some(0x0b13),
            serial: Some("controller-a".to_string()),
        };
        let c = Config {
            talk: vec![
                Trigger::GamepadButton {
                    device: selector.clone(),
                    button: GamepadButton::South,
                },
                Trigger::ConsumerControl {
                    device: DeviceSelector::any(),
                    usage: 0x00cd,
                },
                Trigger::HidButton {
                    device: selector,
                    usage_page: 0xff00,
                    usage: 1,
                    control: 7,
                },
            ],
            ..Config::default()
        };
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.talk, c.talk);
    }

    #[test]
    fn selector_fields_are_optional_wildcards() {
        let any = DeviceSelector::any();
        assert!(any.matches(Some("raw:a"), Some(1), Some(2), Some("serial")));
        let exact = DeviceSelector {
            stable_id: Some("raw:a".into()),
            vendor_id: Some(1),
            ..DeviceSelector::default()
        };
        assert!(exact.matches(Some("raw:a"), Some(1), Some(99), None));
        assert!(!exact.matches(Some("raw:b"), Some(1), Some(99), None));
    }

    #[test]
    fn wildcard_and_specific_device_bindings_overlap() {
        let any_south = Trigger::GamepadButton {
            device: DeviceSelector::any(),
            button: GamepadButton::South,
        };
        let controller_south = Trigger::GamepadButton {
            device: DeviceSelector {
                stable_id: Some("raw:controller-a".into()),
                ..DeviceSelector::default()
            },
            button: GamepadButton::South,
        };
        let controller_east = Trigger::GamepadButton {
            device: DeviceSelector {
                stable_id: Some("raw:controller-a".into()),
                ..DeviceSelector::default()
            },
            button: GamepadButton::East,
        };
        assert!(triggers_overlap(&any_south, &controller_south));
        assert!(!triggers_overlap(&any_south, &controller_east));
    }

    #[test]
    fn disjoint_device_constraints_do_not_overlap() {
        let left = Trigger::ConsumerControl {
            device: DeviceSelector {
                vendor_id: Some(1),
                product_id: Some(2),
                ..DeviceSelector::default()
            },
            usage: 0xcd,
        };
        let right = Trigger::ConsumerControl {
            device: DeviceSelector {
                vendor_id: Some(9),
                product_id: Some(2),
                ..DeviceSelector::default()
            },
            usage: 0xcd,
        };
        assert!(!triggers_overlap(&left, &right));
    }

    #[test]
    fn aliases_and_hid_control_wildcards_overlap() {
        assert!(triggers_overlap(
            &Trigger::Key("rightctrl".into()),
            &Trigger::Key("ControlRight".into()),
        ));

        let hid = |control| Trigger::HidButton {
            device: DeviceSelector::any(),
            usage_page: 0xff00,
            usage: 1,
            control,
        };
        assert!(triggers_overlap(&hid(0), &hid(42)));
        assert!(!triggers_overlap(&hid(7), &hid(42)));
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    /// A config written before the setting worked must not be taken at its word.
    #[test]
    fn v0_discards_the_inherited_paste_flag() {
        let mut c: Config = toml::from_str("paste_insert = true\n").unwrap();
        assert_eq!(c.config_version, 0);
        assert!(c.migrate());
        assert!(!c.paste_insert, "inherited paste_insert should be cleared");
        assert_eq!(c.config_version, CONFIG_VERSION);
    }

    /// Once migrated, a deliberate choice is left alone.
    #[test]
    fn later_versions_are_untouched() {
        // A config already at the current version is left alone.
        let mut c: Config = toml::from_str(&format!(
            "paste_insert = true\nconfig_version = {CONFIG_VERSION}\n"
        ))
        .unwrap();
        assert!(!c.migrate(), "should report no change");
        assert!(c.paste_insert, "a user's own choice must survive");
    }

    /// Migration runs once; a second pass is a no-op.
    #[test]
    fn migration_is_idempotent() {
        let mut c: Config = toml::from_str("paste_insert = true\n").unwrap();
        assert!(c.migrate());
        assert!(!c.migrate());
    }

    #[test]
    fn v2_normalizes_aliases_and_removes_cross_role_collisions() {
        let mut c: Config = toml::from_str(
            "config_version = 2\n\
             talk = [{ key = \"rightctrl\" }]\n\
             send = [{ key = \"ControlRight\" }]\n\
             teach = [{ key = \"caps\" }, { key = \"CapsLock\" }]\n",
        )
        .unwrap();
        assert!(c.migrate());
        assert_eq!(c.talk, vec![Trigger::Key("ControlRight".into())]);
        assert!(c.send.is_empty(), "talk must retain precedence over send");
        assert_eq!(c.teach, vec![Trigger::Key("CapsLock".into())]);
        assert_eq!(c.config_version, CONFIG_VERSION);
    }

    #[test]
    fn v3_automatic_chinese_moves_to_sensevoice() {
        let mut c: Config = toml::from_str(
            "config_version = 3\nlanguage = \"zh\"\nmodel = \"\"\nonboarded = true\n",
        )
        .unwrap();
        assert!(c.migrate());
        assert_eq!(c.model, "sensevoice");
        assert_eq!(c.config_version, CONFIG_VERSION);
    }

    #[test]
    fn v3_explicit_chinese_model_is_preserved() {
        for selected in ["paraformer-zh", "sensevoice", "qwen3-asr-0.6b"] {
            let mut c: Config = toml::from_str(&format!(
                "config_version = 3\nlanguage = \"zh\"\nmodel = \"{selected}\"\nonboarded = true\n"
            ))
            .unwrap();
            assert!(c.migrate());
            assert_eq!(c.model, selected);
            assert_eq!(c.config_version, CONFIG_VERSION);
        }
    }

    #[test]
    fn v3_other_language_default_is_not_rewritten() {
        let mut c: Config = toml::from_str(
            "config_version = 3\nlanguage = \"fr\"\nmodel = \"\"\nonboarded = true\n",
        )
        .unwrap();
        assert!(c.migrate());
        assert!(c.model.is_empty());
        assert_eq!(c.config_version, CONFIG_VERSION);
    }
}

/// v5 changed three defaults for new installs only. Every existing file,
/// whatever it spells out, must keep behaving as it did.
#[cfg(test)]
mod new_install_defaults_tests {
    use super::*;

    /// Byte-for-byte what VocalCode Community 1.4.0 wrote on first launch.
    const WRITTEN_BY_1_4_0: &str = r#"teach = []
min_record_ms = 250
paste_insert = false
correction_window_ms = 8000
live_caption = false
noise_filter = false
cue_sounds = true
model = ""
language = "auto"
autostart = false
smart_meeting_reminders = false
ignored_meeting_apps = []
ui_lang = "auto"
talk_mode = "hold"
overlay_style = "classic"
desktop_control = false
desktop_control_edge = "bottom"
onboarded = false
config_version = 4
double_tap_lock = true
mute_while_dictating = false

[[talk]]
mouse_button = "x2"

[[talk]]
key = "ControlRight"

[[send]]
mouse_button = "x1"

[writing]
commands = false
backtrack = false
lists = false
code = false
press_enter = false
style = "formal"

[writing.app_styles]
"#;

    fn old_send() -> Vec<Trigger> {
        if cfg!(target_os = "macos") {
            Vec::new()
        } else {
            vec![Trigger::MouseButton(MouseExtra::X1)]
        }
    }

    fn assert_old_behaviour(config: &Config, context: &str) {
        assert_eq!(config.send, old_send(), "{context}: send");
        assert!(!config.noise_filter, "{context}: noise_filter");
        assert_eq!(
            config.keep_history,
            HistoryRetention::Off,
            "{context}: keep_history"
        );
    }

    /// Drop every line that sets one of `keys`, and the table header of an
    /// array-of-tables entry for them.
    fn without(source: &str, keys: &[&str]) -> String {
        let mut out = String::new();
        let mut skipping = false;
        for line in source.lines() {
            let header = line.trim_start_matches('[').trim_end_matches(']');
            if line.starts_with('[') {
                skipping = keys.contains(&header);
                if skipping {
                    continue;
                }
            }
            let key = line.split('=').next().unwrap_or("").trim();
            if skipping || keys.contains(&key) {
                continue;
            }
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    #[test]
    fn a_new_install_gets_the_new_defaults() {
        let fresh = Config::default();
        assert!(fresh.send.is_empty(), "no mouse button is taken by default");
        assert!(fresh.noise_filter, "the speech gate is on");
        assert_eq!(fresh.keep_history, HistoryRetention::Week);
        assert_eq!(fresh.config_version, CONFIG_VERSION);

        // What a new install writes reads back as itself.
        let text = toml::to_string_pretty(&fresh).unwrap();
        assert!(text.contains("keep_history = \"7d\""), "{text}");
        assert!(text.contains("noise_filter = true"), "{text}");
        let back: Config = toml::from_str(&text).unwrap();
        assert!(back.send.is_empty());
        assert!(back.noise_filter);
        assert_eq!(back.keep_history, HistoryRetention::Week);
    }

    #[test]
    fn a_current_file_that_leaves_them_out_means_the_new_defaults() {
        let c: Config = toml::from_str(&format!("config_version = {CONFIG_VERSION}\n")).unwrap();
        assert!(c.send.is_empty());
        assert!(c.noise_filter);
        assert_eq!(c.keep_history, HistoryRetention::Week);
    }

    #[test]
    fn the_file_1_4_0_wrote_keeps_its_behaviour_through_migration_and_save() {
        let mut c: Config = toml::from_str(WRITTEN_BY_1_4_0).unwrap();
        assert_eq!(c.config_version, 4);
        // 1.4.0 spelled the Back button out on every platform it bound it.
        assert_eq!(c.send, vec![Trigger::MouseButton(MouseExtra::X1)]);
        assert!(!c.noise_filter);
        assert_eq!(c.keep_history, HistoryRetention::Off);

        assert!(c.migrate());
        assert_eq!(c.config_version, CONFIG_VERSION);
        assert_eq!(c.send, vec![Trigger::MouseButton(MouseExtra::X1)]);
        assert!(!c.noise_filter);
        assert_eq!(c.keep_history, HistoryRetention::Off);

        // The first Settings save writes v5 with the old values spelled out,
        // so they no longer depend on reading an old document.
        let saved = toml::to_string_pretty(&c).unwrap();
        assert!(saved.contains("keep_history = \"off\""), "{saved}");
        assert!(saved.contains("noise_filter = false"), "{saved}");
        let mut back: Config = toml::from_str(&saved).unwrap();
        assert!(!back.migrate(), "already current");
        assert_eq!(back.send, vec![Trigger::MouseButton(MouseExtra::X1)]);
        assert!(!back.noise_filter);
        assert_eq!(back.keep_history, HistoryRetention::Off);
    }

    #[test]
    fn older_files_that_leave_the_keys_out_keep_the_old_defaults() {
        let bare = without(WRITTEN_BY_1_4_0, &["send", "noise_filter"]);
        assert!(
            !bare.contains("x1") && !bare.contains("noise_filter"),
            "{bare}"
        );
        let mut c: Config = toml::from_str(&bare).unwrap();
        assert_old_behaviour(&c, "1.4.0 file without the keys");
        c.migrate();
        assert_old_behaviour(&c, "1.4.0 file without the keys, migrated");

        // Every earlier schema, including files with no version at all.
        for version in [None, Some(0), Some(1), Some(2), Some(3), Some(4)] {
            let source = match version {
                Some(version) => format!("language = \"en\"\nconfig_version = {version}\n"),
                None => "language = \"en\"\n".to_string(),
            };
            let mut c: Config = toml::from_str(&source).unwrap();
            assert_old_behaviour(&c, &source);
            c.migrate();
            assert_old_behaviour(&c, &source);
            let back: Config = toml::from_str(&toml::to_string_pretty(&c).unwrap()).unwrap();
            assert_old_behaviour(&back, &source);
        }
    }

    #[test]
    fn explicit_values_in_older_files_are_kept_as_written() {
        for version in ["", "config_version = 2\n", "config_version = 4\n"] {
            let source = format!(
                "{version}send = []\nnoise_filter = true\nkeep_history = \"24h\"\nlanguage = \"en\"\n"
            );
            let mut c: Config = toml::from_str(&source).unwrap();
            c.migrate();
            assert!(c.send.is_empty(), "{source}");
            assert!(c.noise_filter, "an explicit opt-in survives: {source}");
            assert_eq!(c.keep_history, HistoryRetention::Day, "{source}");

            let source = format!("{version}send = [{{ key = \"F13\" }}]\nnoise_filter = false\n");
            let c: Config = toml::from_str(&source).unwrap();
            assert_eq!(c.send, vec![Trigger::Key("F13".into())], "{source}");
            assert!(!c.noise_filter, "{source}");
        }
    }

    /// The "Import from the previous VocalCode" flow reads the other
    /// installation's file as a table (to learn which keys it spells out) and
    /// as a `Config`, then migrates it. Its explicit values must arrive
    /// exactly as written, and its omitted keys must mean what they meant.
    #[test]
    fn an_imported_older_file_goes_through_the_same_migration() {
        let source = without(WRITTEN_BY_1_4_0, &["noise_filter"]);
        let written: toml::Table = toml::from_str(&source).unwrap();
        let mut config: Config = toml::from_str(&source).unwrap();
        config.validate_bounds().unwrap();
        config.migrate();
        assert!(written.contains_key("send"));
        assert_eq!(config.send, vec![Trigger::MouseButton(MouseExtra::X1)]);
        assert!(!written.contains_key("noise_filter"));
        assert!(
            !config.noise_filter,
            "omitted in an old file: the old default"
        );
        assert!(!written.contains_key("keep_history"));
        assert_eq!(config.keep_history, HistoryRetention::Off);
    }

    #[test]
    fn history_retention_spellings_round_trip_and_nothing_else_parses() {
        for retention in [
            HistoryRetention::Off,
            HistoryRetention::Day,
            HistoryRetention::Week,
        ] {
            assert_eq!(HistoryRetention::parse(retention.as_str()), Some(retention));
            let c = Config {
                keep_history: retention,
                ..Config::default()
            };
            let back: Config = toml::from_str(&toml::to_string_pretty(&c).unwrap()).unwrap();
            assert_eq!(back.keep_history, retention);
        }
        assert_eq!(HistoryRetention::Off.max_age_secs(), None);
        assert_eq!(HistoryRetention::Day.max_age_secs(), Some(86_400));
        assert_eq!(HistoryRetention::Week.max_age_secs(), Some(604_800));
        assert!(!HistoryRetention::Off.keeps() && HistoryRetention::Week.keeps());
        for bad in [
            "keep_history = \"30d\"",
            "keep_history = 7",
            "keep_history = \"\"",
        ] {
            assert!(toml::from_str::<Config>(bad).is_err(), "{bad}");
            assert!(
                toml::from_str::<Config>(&format!("config_version = 5\n{bad}")).is_err(),
                "{bad}"
            );
        }
        assert_eq!(HistoryRetention::parse("forever"), None);
    }

    /// Reading through the migration must not make a broken file look valid
    /// or hide what is wrong with it.
    #[test]
    fn invalid_documents_are_still_rejected_with_the_reason() {
        let error = toml::from_str::<Config>("language = 'en'\nnoise_filter = 'true'")
            .unwrap_err()
            .to_string();
        assert!(error.contains("expected a boolean"), "{error}");
        assert!(error.contains("`noise_filter`"), "{error}");
        let error = toml::from_str::<Config>("send = 'x1'")
            .unwrap_err()
            .to_string();
        assert!(error.contains("`send`"), "{error}");
        assert!(toml::from_str::<Config>("config_version = 'five'").is_err());
        assert!(toml::from_str::<Config>("config_version = -1").is_err());
        assert!(toml::from_str::<Config>("min_record_ms = 250.5").is_err());
    }
}

#[cfg(test)]
mod bounds_tests {
    use super::*;

    #[test]
    fn config_rejects_trigger_counts_before_pairwise_overlap_work() {
        let config = Config {
            talk: (0..=MAX_TRIGGERS_PER_ACTION)
                .map(|index| Trigger::Key(format!("F{index}")))
                .collect(),
            ..Config::default()
        };
        assert!(config.validate_bounds().unwrap_err().contains("at most"));
    }

    #[test]
    fn config_rejects_oversized_key_selector_and_device_strings() {
        let mut config = Config {
            talk: vec![Trigger::Key("k".repeat(MAX_TRIGGER_KEY_UTF8_BYTES + 1))],
            ..Config::default()
        };
        assert!(config
            .validate_bounds()
            .unwrap_err()
            .contains("trigger key name"));

        config.talk = vec![Trigger::GamepadButton {
            device: DeviceSelector {
                serial: Some("s".repeat(MAX_TRIGGER_SELECTOR_UTF8_BYTES + 1)),
                ..DeviceSelector::default()
            },
            button: GamepadButton::South,
        }];
        assert!(config.validate_bounds().unwrap_err().contains("serial"));

        config.talk.clear();
        config.input_device = Some("d".repeat(MAX_INPUT_DEVICE_UTF8_BYTES + 1));
        assert!(config
            .validate_bounds()
            .unwrap_err()
            .contains("input_device"));
    }

    #[test]
    fn exact_string_and_trigger_limits_are_accepted() {
        let config = Config {
            talk: (0..MAX_TRIGGERS_PER_ACTION)
                .map(|_| Trigger::Key("k".repeat(MAX_TRIGGER_KEY_UTF8_BYTES)))
                .collect(),
            input_device: Some("d".repeat(MAX_INPUT_DEVICE_UTF8_BYTES)),
            ui_lang: "u".repeat(MAX_UI_LANGUAGE_UTF8_BYTES),
            ..Config::default()
        };
        assert!(config.validate_bounds().is_ok());
    }
}
