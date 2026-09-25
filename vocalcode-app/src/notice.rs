//! Short one-line notices for someone who is dictating, and the tray tooltip.
//!
//! Every failure used to be reported only to the settings window, which is
//! hidden nearly all the time, as a toast that lasted under three seconds.
//! Someone holding the talk key saw nothing at all: a press while the model
//! was still downloading passed straight through, a transcript with no text
//! field to go to landed on the clipboard in silence, and a muted microphone
//! produced an empty result and not a word about it. These notices appear in
//! the passive indicator, where that person is already looking, and the tray
//! tooltip keeps the same facts available on hover.
//!
//! The copy lives here rather than in a page dictionary because both surfaces
//! are native: the tray has no document at all, and on Windows the indicator
//! window has to be sized to the sentence before it is shown.

use std::sync::Mutex;

use vocalcode_core::engine::Hearing;
use vocalcode_core::VocalCodeError;

use crate::overlay::Phase;

/// Why a talk press started nothing. The key itself is still passed through to
/// the foreground application, exactly as it always was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotReady {
    /// The speech model is downloading; whole percent done.
    Downloading(u8),
    /// No usable speech model: still loading, failed, or no language chosen.
    Model,
    /// The selected microphone could not be opened or stopped responding.
    Microphone,
    /// A previous dictation is still being transcribed, or settings are being
    /// applied. Pressing again in a moment works.
    Busy,
}

/// One message for the indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// No focused control would take text, so the transcript went to the
    /// clipboard. Not a failure: the words are safe, and this says where.
    CopiedToClipboard,
    NotReady(NotReady),
    /// A recording of real length produced no words at all.
    HeardNothing,
}

/// Utterances shorter than this are presses, not attempts to speak. They stay
/// silent, like the sub-minimum taps the engine has always discarded.
pub const HEARD_NOTHING_MIN_MS: u64 = 500;

/// -40 dBFS. Speech in the voice corpus, including its deliberately quiet
/// takes, peaks above 0.06; a muted or wrong input sits far below this.
pub const SILENT_PEAK: f32 = 0.01;

/// Whether a finished dictation deserves "didn't hear anything".
///
/// Only when nothing was typed, the press was long enough to be an attempt,
/// and either the model found no words at all or the whole capture was near
/// silence. A spoken command ("press enter") or a lone filler that cleanup
/// removed also ends with no text, but there the model did hear words from a
/// working microphone, so no notice is due.
pub fn heard_nothing(text: &str, hearing: Hearing, min_record_ms: u32) -> bool {
    text.trim().is_empty()
        && hearing.audio_ms >= HEARD_NOTHING_MIN_MS.max(u64::from(min_record_ms))
        && (!hearing.recognized || hearing.peak < SILENT_PEAK)
}

/// The notice an engine error warrants, if any. Other errors keep reaching the
/// settings page as before; these two are the ones someone dictating needs.
pub fn for_engine_error(error: &VocalCodeError) -> Option<Notice> {
    match error {
        VocalCodeError::Diverted(_) => Some(Notice::CopiedToClipboard),
        VocalCodeError::Audio(_) => Some(Notice::NotReady(NotReady::Microphone)),
        _ => None,
    }
}

/// Latest-wins mailbox from any thread to the UI loop.
///
/// One slot on purpose, unlike the settings page's error queue: a notice is on
/// screen for three seconds, and after a newer one the older is stale news.
#[derive(Default)]
pub struct Board(Mutex<Option<Notice>>);

impl Board {
    pub fn post(&self, notice: Notice) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(notice);
    }

    pub fn take(&self) -> Option<Notice> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).take()
    }
}

/// Turns the input hook's not-ready press count into "a press happened since
/// the last look". The hook only counts; it never waits on the UI.
pub struct PressWatch(u64);

impl PressWatch {
    pub fn new(current: u64) -> Self {
        Self(current)
    }

    pub fn saw_press(&mut self, current: u64) -> bool {
        let pressed = current != self.0;
        self.0 = current;
        pressed
    }
}

/// Everything the UI thread knows about readiness without asking the engine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Readiness {
    pub shutdown: bool,
    pub onboarded: bool,
    pub permissions_ok: bool,
    /// Percent of the model download in flight, if one is.
    pub download: Option<f64>,
    pub model_available: bool,
    /// The last model preparation failed and a retry is pending.
    pub model_failed: bool,
    pub microphone_failed: bool,
    /// The engine's own gate: every part ready and no decode in progress.
    pub ready: bool,
    /// The engine's phase, as the indicator shows it.
    pub phase: Phase,
}

fn whole_percent(percent: f64) -> u8 {
    if percent.is_finite() {
        // Rounded down: "100%" while bytes are still arriving reads as a hang.
        percent.clamp(0.0, 100.0).floor() as u8
    } else {
        0
    }
}

/// Why a talk press passed through, judged a moment after the press.
///
/// The most useful explanation wins: a download says how long to wait, and a
/// broken microphone is the thing to fix. When the cause has already cleared
/// by the time the UI looks (a decode finished), the press still happened and
/// still started nothing, so "try again" remains the right thing to say.
pub fn not_ready_reason(readiness: &Readiness) -> Option<NotReady> {
    if readiness.shutdown {
        return None;
    }
    if let Some(percent) = readiness.download {
        return Some(NotReady::Downloading(whole_percent(percent)));
    }
    if readiness.microphone_failed {
        return Some(NotReady::Microphone);
    }
    if !readiness.onboarded || !readiness.model_available {
        return Some(NotReady::Model);
    }
    Some(NotReady::Busy)
}

/// What the tray tooltip says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    Ready,
    Downloading(u8),
    Preparing,
    ChooseLanguage,
    Error(TrayError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayError {
    Permissions,
    Microphone,
    Model,
}

/// The tray's state. Unlike a notice this describes a condition, so a decode
/// in progress is still "Ready": the app is working, not stuck.
pub fn tray_state(readiness: &Readiness) -> TrayState {
    if let Some(percent) = readiness.download {
        return TrayState::Downloading(whole_percent(percent));
    }
    if !readiness.onboarded {
        return TrayState::ChooseLanguage;
    }
    if !readiness.permissions_ok {
        return TrayState::Error(TrayError::Permissions);
    }
    if readiness.microphone_failed {
        return TrayState::Error(TrayError::Microphone);
    }
    if readiness.model_failed {
        return TrayState::Error(TrayError::Model);
    }
    if !readiness.model_available {
        return TrayState::Preparing;
    }
    if readiness.ready || readiness.phase != Phase::Idle {
        return TrayState::Ready;
    }
    // Settings being applied, or the input listener restarting.
    TrayState::Preparing
}

/// The interface languages the app is translated into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Zh,
    Es,
    Fr,
    De,
}

impl Lang {
    /// The language for a saved interface preference. "auto" follows the
    /// operating system's display language, as the pages do through
    /// `navigator.language`.
    pub fn resolve(preference: &str) -> Self {
        match preference.trim() {
            "" | "auto" => system_language().map_or(Self::En, |tag| Self::from_tag(&tag)),
            tag => Self::from_tag(tag),
        }
    }

    fn from_tag(tag: &str) -> Self {
        let primary = tag.split(['-', '_', '.']).next().unwrap_or_default();
        match primary.to_ascii_lowercase().as_str() {
            "zh" => Self::Zh,
            "es" => Self::Es,
            "fr" => Self::Fr,
            "de" => Self::De,
            _ => Self::En,
        }
    }
}

/// The preference resolved once per change rather than on every UI tick.
#[derive(Default)]
pub struct LangCache(Option<(String, Lang)>);

impl LangCache {
    pub fn get(&mut self, preference: &str) -> Lang {
        match &self.0 {
            Some((cached, lang)) if cached == preference => *lang,
            _ => {
                let lang = Lang::resolve(preference);
                self.0 = Some((preference.to_string(), lang));
                lang
            }
        }
    }
}

#[cfg(windows)]
fn system_language() -> Option<String> {
    // The display language, which is what WebView2 reports as
    // `navigator.language` by default. PRIMARYLANGID is the low ten bits.
    let id = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() } & 0x3ff;
    Some(
        match id {
            0x04 => "zh",
            0x0a => "es",
            0x0c => "fr",
            0x07 => "de",
            _ => "en",
        }
        .to_string(),
    )
}

#[cfg(target_os = "macos")]
fn system_language() -> Option<String> {
    use objc2::runtime::AnyObject;
    unsafe {
        let languages: *mut AnyObject =
            objc2::msg_send![objc2::class!(NSLocale), preferredLanguages];
        if languages.is_null() {
            return None;
        }
        let first: *mut AnyObject = objc2::msg_send![languages, firstObject];
        if first.is_null() {
            return None;
        }
        let utf8: *const std::ffi::c_char = objc2::msg_send![first, UTF8String];
        if utf8.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr(utf8)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn system_language() -> Option<String> {
    std::env::var("LANG").ok()
}

/// How to paste on this platform, in this language's key names.
fn paste_shortcut(lang: Lang) -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘V"
    } else if lang == Lang::De {
        "Strg+V"
    } else {
        "Ctrl+V"
    }
}

impl Notice {
    pub fn text(self, lang: Lang) -> String {
        match self {
            Notice::CopiedToClipboard => {
                let paste = paste_shortcut(lang);
                match lang {
                    Lang::En => format!("No text field — copied to clipboard ({paste})"),
                    Lang::Zh => format!("没有输入框，已复制到剪贴板（{paste} 粘贴）"),
                    Lang::Es => format!("Sin campo de texto — copiado al portapapeles ({paste})"),
                    Lang::Fr => {
                        format!("Aucun champ de texte — copié dans le presse-papiers ({paste})")
                    }
                    Lang::De => format!("Kein Textfeld — in die Zwischenablage kopiert ({paste})"),
                }
            }
            Notice::NotReady(NotReady::Downloading(percent)) => match lang {
                Lang::En => format!("Model downloading — {percent}%"),
                Lang::Zh => format!("正在下载模型（{percent}%）"),
                Lang::Es => format!("Descargando el modelo — {percent}\u{a0}%"),
                Lang::Fr => format!("Téléchargement du modèle — {percent}\u{a0}%"),
                Lang::De => format!("Modell wird heruntergeladen — {percent}\u{a0}%"),
            },
            Notice::NotReady(NotReady::Model) => match lang {
                Lang::En => "Model not ready yet",
                Lang::Zh => "模型还没准备好",
                Lang::Es => "El modelo aún no está listo",
                Lang::Fr => "Le modèle n’est pas encore prêt",
                Lang::De => "Modell ist noch nicht bereit",
            }
            .to_string(),
            Notice::NotReady(NotReady::Microphone) => microphone_unavailable(lang).to_string(),
            Notice::NotReady(NotReady::Busy) => match lang {
                Lang::En => "Still working — try again in a moment",
                Lang::Zh => "正在处理上一段，请稍后再试",
                Lang::Es => "Todavía procesando — inténtalo de nuevo en un momento",
                Lang::Fr => "Traitement en cours — réessayez dans un instant",
                Lang::De => "Noch beschäftigt — bitte gleich noch einmal versuchen",
            }
            .to_string(),
            Notice::HeardNothing => match lang {
                Lang::En => "Didn't hear anything — check your microphone",
                Lang::Zh => "没有识别到语音，请检查麦克风",
                Lang::Es => "No se detectó voz — revisa el micrófono",
                Lang::Fr => "Aucune parole détectée — vérifiez votre micro",
                Lang::De => "Keine Sprache erkannt — bitte Mikrofon prüfen",
            }
            .to_string(),
        }
    }
}

fn microphone_unavailable(lang: Lang) -> &'static str {
    match lang {
        Lang::En => "Microphone unavailable",
        Lang::Zh => "麦克风不可用",
        Lang::Es => "Micrófono no disponible",
        Lang::Fr => "Microphone indisponible",
        Lang::De => "Mikrofon nicht verfügbar",
    }
}

/// Windows keeps a notification-area tip in a fixed 128-unit UTF-16 buffer,
/// and tray-icon copies at most 128 units without adding the terminator.
/// Stay one unit short so the terminator always survives.
const TOOLTIP_MAX_UTF16: usize = 127;

fn fit_tooltip(text: String) -> String {
    if text.encode_utf16().count() <= TOOLTIP_MAX_UTF16 {
        return text;
    }
    let mut fitted = String::new();
    let mut units = 0;
    for c in text.chars() {
        // Leave room for the ellipsis, one unit.
        if units + c.len_utf16() > TOOLTIP_MAX_UTF16 - 1 {
            break;
        }
        units += c.len_utf16();
        fitted.push(c);
    }
    fitted.push('…');
    fitted
}

/// The tray tooltip: the product name, then the state.
pub fn tray_tooltip(state: TrayState, lang: Lang) -> String {
    let detail = match state {
        TrayState::Ready => match lang {
            Lang::En => "Ready",
            Lang::Zh => "待命",
            Lang::Es => "Listo",
            Lang::Fr => "Prêt",
            Lang::De => "Bereit",
        }
        .to_string(),
        TrayState::Downloading(percent) => match lang {
            Lang::En => format!("Downloading speech model — {percent}%"),
            Lang::Zh => format!("正在下载语音模型（{percent}%）"),
            Lang::Es => format!("Descargando el modelo de voz — {percent}\u{a0}%"),
            Lang::Fr => format!("Téléchargement du modèle vocal — {percent}\u{a0}%"),
            Lang::De => format!("Sprachmodell wird heruntergeladen — {percent}\u{a0}%"),
        },
        TrayState::Preparing => match lang {
            Lang::En => "Preparing…",
            Lang::Zh => "准备中…",
            Lang::Es => "Preparando…",
            Lang::Fr => "Préparation…",
            Lang::De => "Wird vorbereitet…",
        }
        .to_string(),
        TrayState::ChooseLanguage => match lang {
            Lang::En => "Choose a language to begin",
            Lang::Zh => "选择一种语言开始",
            Lang::Es => "Elige un idioma para empezar",
            Lang::Fr => "Choisissez une langue pour commencer",
            Lang::De => "Wähle eine Sprache, um zu beginnen",
        }
        .to_string(),
        TrayState::Error(error) => {
            let reason = match error {
                TrayError::Permissions => match lang {
                    Lang::En => "Permissions not granted",
                    Lang::Zh => "缺少权限",
                    Lang::Es => "Faltan permisos",
                    Lang::Fr => "Autorisations manquantes",
                    Lang::De => "Berechtigungen fehlen",
                },
                TrayError::Microphone => microphone_unavailable(lang),
                TrayError::Model => match lang {
                    Lang::En => "Speech model failed to load",
                    Lang::Zh => "语音模型加载失败",
                    Lang::Es => "No se pudo cargar el modelo de voz",
                    Lang::Fr => "Échec du chargement du modèle vocal",
                    Lang::De => "Sprachmodell konnte nicht geladen werden",
                },
            };
            match lang {
                Lang::En => format!("Error: {reason}"),
                Lang::Zh => format!("出错：{reason}"),
                Lang::Es => format!("Error: {reason}"),
                Lang::Fr => format!("Erreur\u{a0}: {reason}"),
                Lang::De => format!("Fehler: {reason}"),
            }
        }
    };
    fit_tooltip(format!("VocalCode — {detail}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LANGS: [Lang; 5] = [Lang::En, Lang::Zh, Lang::Es, Lang::Fr, Lang::De];

    fn all_notices() -> Vec<Notice> {
        vec![
            Notice::CopiedToClipboard,
            Notice::NotReady(NotReady::Downloading(45)),
            Notice::NotReady(NotReady::Model),
            Notice::NotReady(NotReady::Microphone),
            Notice::NotReady(NotReady::Busy),
            Notice::HeardNothing,
        ]
    }

    fn idle_ready() -> Readiness {
        Readiness {
            shutdown: false,
            onboarded: true,
            permissions_ok: true,
            download: None,
            model_available: true,
            model_failed: false,
            microphone_failed: false,
            ready: true,
            phase: Phase::Idle,
        }
    }

    #[test]
    fn a_press_while_downloading_says_how_far_along_the_download_is() {
        let readiness = Readiness {
            download: Some(45.7),
            model_available: false,
            ready: false,
            microphone_failed: true,
            ..idle_ready()
        };
        assert_eq!(
            not_ready_reason(&readiness),
            Some(NotReady::Downloading(45))
        );
        assert_eq!(
            Notice::NotReady(NotReady::Downloading(45)).text(Lang::En),
            "Model downloading — 45%"
        );
    }

    #[test]
    fn download_percent_never_claims_done_early_or_leaks_nonsense() {
        assert_eq!(whole_percent(99.9), 99);
        assert_eq!(whole_percent(100.0), 100);
        assert_eq!(whole_percent(250.0), 100);
        assert_eq!(whole_percent(-3.0), 0);
        assert_eq!(whole_percent(f64::NAN), 0);
        assert_eq!(whole_percent(f64::INFINITY), 0);
    }

    #[test]
    fn not_ready_reason_prefers_the_microphone_then_the_model_then_busy() {
        let broken_mic = Readiness {
            microphone_failed: true,
            model_available: false,
            ready: false,
            ..idle_ready()
        };
        assert_eq!(not_ready_reason(&broken_mic), Some(NotReady::Microphone));

        let loading = Readiness {
            model_available: false,
            ready: false,
            ..idle_ready()
        };
        assert_eq!(not_ready_reason(&loading), Some(NotReady::Model));
        let before_language = Readiness {
            onboarded: false,
            ..loading
        };
        assert_eq!(not_ready_reason(&before_language), Some(NotReady::Model));

        let decoding = Readiness {
            ready: false,
            phase: Phase::Transcribing,
            ..idle_ready()
        };
        assert_eq!(not_ready_reason(&decoding), Some(NotReady::Busy));
        // The decode finished between the press and the look; the press still
        // started nothing and "try again" is still true.
        assert_eq!(not_ready_reason(&idle_ready()), Some(NotReady::Busy));

        let quitting = Readiness {
            shutdown: true,
            ..loading
        };
        assert_eq!(not_ready_reason(&quitting), None);
    }

    #[test]
    fn tray_follows_download_errors_and_readiness() {
        let ready = idle_ready();
        assert_eq!(tray_state(&ready), TrayState::Ready);
        assert_eq!(
            tray_tooltip(tray_state(&ready), Lang::En),
            "VocalCode — Ready"
        );

        let decoding = Readiness {
            ready: false,
            phase: Phase::Transcribing,
            ..ready
        };
        assert_eq!(tray_state(&decoding), TrayState::Ready, "busy is not stuck");

        let downloading = Readiness {
            download: Some(12.4),
            model_available: false,
            ready: false,
            ..ready
        };
        assert_eq!(tray_state(&downloading), TrayState::Downloading(12));
        assert_eq!(
            tray_tooltip(tray_state(&downloading), Lang::En),
            "VocalCode — Downloading speech model — 12%"
        );

        let mic = Readiness {
            microphone_failed: true,
            ready: false,
            ..ready
        };
        assert_eq!(
            tray_tooltip(tray_state(&mic), Lang::En),
            "VocalCode — Error: Microphone unavailable"
        );
        assert_eq!(
            tray_tooltip(tray_state(&mic), Lang::Zh),
            "VocalCode — 出错：麦克风不可用"
        );

        let model = Readiness {
            model_failed: true,
            model_available: false,
            ready: false,
            ..ready
        };
        assert_eq!(tray_state(&model), TrayState::Error(TrayError::Model));
        let loading = Readiness {
            model_available: false,
            ready: false,
            ..ready
        };
        assert_eq!(tray_state(&loading), TrayState::Preparing);
        let restarting = Readiness {
            ready: false,
            ..ready
        };
        assert_eq!(tray_state(&restarting), TrayState::Preparing);
        let first_run = Readiness {
            onboarded: false,
            model_available: false,
            ready: false,
            ..ready
        };
        assert_eq!(tray_state(&first_run), TrayState::ChooseLanguage);
        let permissions = Readiness {
            permissions_ok: false,
            ready: false,
            ..ready
        };
        assert_eq!(
            tray_state(&permissions),
            TrayState::Error(TrayError::Permissions)
        );
    }

    #[test]
    fn every_notice_and_tray_state_has_distinct_copy_in_every_language() {
        let trays = [
            TrayState::Ready,
            TrayState::Downloading(7),
            TrayState::Preparing,
            TrayState::ChooseLanguage,
            TrayState::Error(TrayError::Permissions),
            TrayState::Error(TrayError::Microphone),
            TrayState::Error(TrayError::Model),
        ];
        for lang in LANGS {
            let notices: Vec<_> = all_notices().into_iter().map(|n| n.text(lang)).collect();
            let tips: Vec<_> = trays.iter().map(|s| tray_tooltip(*s, lang)).collect();
            for list in [&notices, &tips] {
                for (i, text) in list.iter().enumerate() {
                    assert!(!text.trim().is_empty());
                    assert!(!text.contains('\n'), "a notice is one line: {text}");
                    assert!(
                        list[i + 1..].iter().all(|other| other != text),
                        "{lang:?} reuses {text}"
                    );
                }
            }
            if lang != Lang::En {
                for (notice, english) in all_notices().into_iter().zip(all_notices()) {
                    assert_ne!(
                        notice.text(lang),
                        english.text(Lang::En),
                        "{lang:?} left {notice:?} in English"
                    );
                }
            }
            for tip in tips {
                assert!(tip.starts_with("VocalCode — "));
                assert!(tip.encode_utf16().count() <= TOOLTIP_MAX_UTF16);
            }
        }
    }

    #[test]
    fn the_clipboard_notice_names_the_paste_shortcut_for_this_platform() {
        let english = Notice::CopiedToClipboard.text(Lang::En);
        let german = Notice::CopiedToClipboard.text(Lang::De);
        if cfg!(target_os = "macos") {
            assert!(english.contains("⌘V") && german.contains("⌘V"));
        } else {
            assert_eq!(english, "No text field — copied to clipboard (Ctrl+V)");
            assert!(german.contains("Strg+V"));
        }
    }

    #[test]
    fn long_tooltips_are_cut_inside_the_windows_buffer_with_a_terminator_to_spare() {
        let long = "界".repeat(200);
        let fitted = fit_tooltip(long);
        assert_eq!(fitted.encode_utf16().count(), TOOLTIP_MAX_UTF16);
        assert!(fitted.ends_with('…'));
        // Surrogate pairs are never split.
        let emoji = "🎙".repeat(100);
        let fitted = fit_tooltip(emoji);
        assert!(fitted.encode_utf16().count() <= TOOLTIP_MAX_UTF16);
        assert!(fitted.trim_end_matches('…').chars().all(|c| c == '🎙'));
        assert_eq!(fit_tooltip("short".into()), "short");
    }

    #[test]
    fn heard_nothing_needs_a_real_attempt_and_no_words_or_no_sound() {
        let silent = Hearing {
            audio_ms: 1_800,
            peak: 0.0004,
            recognized: false,
        };
        assert!(heard_nothing("", silent, 250));
        assert!(heard_nothing("  ", silent, 250));
        // A tap is not an attempt, and neither is anything the user's own
        // minimum recording length already discards.
        assert!(!heard_nothing(
            "",
            Hearing {
                audio_ms: 300,
                ..silent
            },
            250
        ));
        assert!(!heard_nothing(
            "",
            Hearing {
                audio_ms: 900,
                ..silent
            },
            1_000
        ));
        // Text arrived: nothing to explain, whatever the level was.
        assert!(!heard_nothing("hello", silent, 250));
        // Loud, but the model found nothing: still nothing was heard.
        let noise = Hearing {
            peak: 0.4,
            ..silent
        };
        assert!(heard_nothing("", noise, 250));
        // Words were heard from a working microphone and then consumed by a
        // spoken command or filler cleanup: no notice.
        let command = Hearing {
            recognized: true,
            ..noise
        };
        assert!(!heard_nothing("", command, 250));
        // Words "heard" in near silence are a model guess at hiss.
        let hiss = Hearing {
            recognized: true,
            ..silent
        };
        assert!(heard_nothing("", hiss, 250));
    }

    #[test]
    fn only_diversion_and_microphone_errors_become_notices() {
        assert_eq!(
            for_engine_error(&VocalCodeError::Diverted("copied".into())),
            Some(Notice::CopiedToClipboard)
        );
        assert_eq!(
            for_engine_error(&VocalCodeError::Audio("gone".into())),
            Some(Notice::NotReady(NotReady::Microphone))
        );
        assert_eq!(
            for_engine_error(&VocalCodeError::Inject("focus".into())),
            None
        );
        assert_eq!(for_engine_error(&VocalCodeError::Asr("x".into())), None);
    }

    #[test]
    fn the_board_keeps_only_the_newest_notice_and_hands_it_over_once() {
        let board = Board::default();
        assert_eq!(board.take(), None);
        board.post(Notice::HeardNothing);
        board.post(Notice::CopiedToClipboard);
        assert_eq!(board.take(), Some(Notice::CopiedToClipboard));
        assert_eq!(board.take(), None);
    }

    #[test]
    fn press_watch_reports_each_new_count_once() {
        let mut watch = PressWatch::new(7);
        assert!(!watch.saw_press(7));
        assert!(watch.saw_press(9));
        assert!(!watch.saw_press(9));
        assert!(watch.saw_press(10));
    }

    #[test]
    fn language_preferences_resolve_to_a_translated_language() {
        assert_eq!(Lang::resolve("zh"), Lang::Zh);
        assert_eq!(Lang::resolve("de"), Lang::De);
        assert_eq!(Lang::from_tag("zh-Hans-CN"), Lang::Zh);
        assert_eq!(Lang::from_tag("fr_CA.UTF-8"), Lang::Fr);
        assert_eq!(Lang::from_tag("ES"), Lang::Es);
        assert_eq!(Lang::from_tag("ja"), Lang::En);
        assert_eq!(Lang::from_tag(""), Lang::En);
        // "auto" asks the system, which always answers something supported.
        assert!(LANGS.contains(&Lang::resolve("auto")));
        let mut cache = LangCache::default();
        assert_eq!(cache.get("es"), Lang::Es);
        assert_eq!(cache.get("es"), Lang::Es);
        assert_eq!(cache.get("fr"), Lang::Fr);
    }
}
