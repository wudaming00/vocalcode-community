//! macOS integration: TCC (privacy) permission checks and prompts.
//!
//! VocalCode needs three grants on macOS, and two of them fail *silently* —
//! which is the single worst first-run experience we can ship, so we check them
//! explicitly and tell the user what to do:
//!
//! | Permission | Needed by | Failure mode without it |
//! |---|---|---|
//! | Input Monitoring | `rdev::grab` (a `CGEventTap`) | tap never fires — the talk key does nothing |
//! | Accessibility | `enigo` (synthesised keystrokes) | text is never typed into the target app |
//! | Microphone | `cpal` | capture returns silence |
//!
//! Only the microphone produces a prompt on its own (driven by
//! `NSMicrophoneUsageDescription` in Info.plist). The other two must be
//! requested, and the grant is bound to the app's code signature — see the note
//! on `request_accessibility`.

use std::ffi::c_void;

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::{CFString, CFStringRef};
use objc2::runtime::Bool;
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    /// `AXIsProcessTrustedWithOptions(NULL)` checks without prompting; passing
    /// `kAXTrustedCheckOptionPrompt: true` shows the system grant dialog.
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOHIDCheckAccess(request: u32) -> u32;
    fn IOHIDRequestAccess(request: u32) -> bool;
}

/// `kIOHIDRequestTypeListenEvent` — the Input Monitoring right, which is what a
/// listening `CGEventTap` requires.
const IOHID_REQUEST_TYPE_LISTEN_EVENT: u32 = 1;
/// `kIOHIDAccessTypeGranted`.
const IOHID_ACCESS_TYPE_GRANTED: u32 = 0;

/// Snapshot of the grants VocalCode depends on.
#[derive(Debug, Clone, Copy)]
pub struct Permissions {
    /// Accessibility — required to synthesise keystrokes (`enigo`).
    pub accessibility: bool,
    /// Input Monitoring — required for the global push-to-talk tap (`rdev`).
    pub input_monitoring: bool,
    /// Microphone — required for CPAL to receive non-silent samples.
    pub microphone: bool,
}

impl Permissions {
    /// True when everything the engine needs has been granted.
    pub fn all_granted(&self) -> bool {
        self.accessibility && self.input_monitoring && self.microphone
    }
}

fn microphone_authorization_status() -> Option<AVAuthorizationStatus> {
    // SAFETY: AVMediaTypeAudio is an immutable AVFoundation framework
    // constant. The binding models weak-linking as Option; macOS 11+, our
    // deployment target, always exports it, but a missing symbol remains a
    // fail-closed permission state instead of being dereferenced.
    let media_type = unsafe { AVMediaTypeAudio }?;
    // SAFETY: AVMediaTypeAudio is one of the two values accepted by this class
    // method and the returned enum has no ownership obligations.
    Some(unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) })
}

fn request_microphone_access() {
    // SAFETY: see `microphone_authorization_status`; a missing weak symbol is
    // handled without messaging AVFoundation.
    let Some(media_type) = (unsafe { AVMediaTypeAudio }) else {
        log::error!("AVFoundation did not expose AVMediaTypeAudio");
        return;
    };
    let completion = block2::RcBlock::new(|granted: Bool| {
        if granted.as_bool() {
            log::info!("Microphone permission granted");
        } else {
            log::warn!("Microphone permission was not granted");
        }
    });
    // SAFETY: the typed binding copies the Objective-C block for the lifetime
    // of the asynchronous request. The closure owns no borrowed Rust data and
    // may run on AVFoundation's arbitrary completion queue.
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &completion)
    };
}

/// Request the first-time microphone grant, or open System Settings after a
/// denial/restriction. Opening the pane before the first request can show no
/// VocalCode entry at all, which gives the user a button that cannot fix the
/// problem.
pub fn request_or_open_microphone() {
    match microphone_authorization_status() {
        Some(AVAuthorizationStatus::NotDetermined) => request_microphone_access(),
        Some(AVAuthorizationStatus::Authorized) => {}
        _ => open_settings(Pane::Microphone),
    }
}

/// Check every grant without showing any prompt.
pub fn check() -> Permissions {
    Permissions {
        accessibility: unsafe { AXIsProcessTrustedWithOptions(std::ptr::null()) },
        input_monitoring: unsafe {
            IOHIDCheckAccess(IOHID_REQUEST_TYPE_LISTEN_EVENT) == IOHID_ACCESS_TYPE_GRANTED
        },
        microphone: microphone_authorization_status() == Some(AVAuthorizationStatus::Authorized),
    }
}

/// Ask for Accessibility, showing the system dialog if it has not been granted.
///
/// The grant is keyed on the app's **code signature** (or its path, if
/// unsigned). A rebuilt binary therefore looks like a different app to TCC and
/// silently loses the permission — during development, either sign with a
/// stable identity or re-grant after each rebuild. This is also why the release
/// build must be signed with a Developer ID and notarised: an ad-hoc signature
/// changes on every build.
pub fn request_accessibility() -> bool {
    let key = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
    let options = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);
    unsafe { AXIsProcessTrustedWithOptions(options.as_CFTypeRef()) }
}

/// Ask for Input Monitoring, showing the system dialog if not yet granted.
pub fn request_input_monitoring() -> bool {
    unsafe { IOHIDRequestAccess(IOHID_REQUEST_TYPE_LISTEN_EVENT) }
}

/// A System Settings → Privacy & Security pane.
pub enum Pane {
    Accessibility,
    InputMonitoring,
    Microphone,
}

impl Pane {
    fn anchor(&self) -> &'static str {
        match self {
            Pane::Accessibility => "Privacy_Accessibility",
            Pane::InputMonitoring => "Privacy_ListenEvent",
            Pane::Microphone => "Privacy_Microphone",
        }
    }
}

/// Open the relevant Privacy & Security pane so the user can flip the switch.
/// Needed because a *denied* permission never prompts again — the dialog only
/// appears once, after which the only route is System Settings.
pub fn open_settings(pane: Pane) {
    let url = format!(
        "x-apple.systempreferences:com.apple.preference.security?{}",
        pane.anchor()
    );
    if let Err(e) = std::process::Command::new("/usr/bin/open")
        .arg(&url)
        .spawn()
    {
        log::warn!("open settings pane: {e}");
    }
}

/// Request anything still missing at startup and log what is outstanding.
/// Returns the state after prompting.
pub fn ensure_permissions() -> Permissions {
    let before = check();
    if !before.microphone {
        match microphone_authorization_status() {
            Some(AVAuthorizationStatus::NotDetermined) => request_microphone_access(),
            Some(AVAuthorizationStatus::Denied | AVAuthorizationStatus::Restricted) => {
                log::warn!("Microphone not granted — speech audio cannot be captured");
            }
            _ => log::error!("Microphone authorization status is unavailable"),
        }
    }
    if !before.input_monitoring {
        log::warn!("Input Monitoring not granted — the push-to-talk key will not be seen");
        request_input_monitoring();
    }
    if !before.accessibility {
        log::warn!("Accessibility not granted — transcribed text cannot be typed");
        request_accessibility();
    }
    let after = check();
    if after.all_granted() {
        log::info!("macOS permissions: microphone + accessibility + input monitoring granted");
    }
    after
}

// ---------------------------------------------------------------------------
// Services: right-click → Services → "Add to VocalCode dictionary"
// ---------------------------------------------------------------------------

/// Where a word arriving from the Services menu is handed to.
///
/// A `OnceLock` rather than an ivar on the Objective-C object: the provider is a
/// single process-wide instance that outlives everything, and threading a Rust
/// pointer through an ivar buys nothing but a way to get the lifetime wrong.
static TEACH_SINK: std::sync::OnceLock<Box<dyn Fn(String) + Send + Sync>> =
    std::sync::OnceLock::new();

/// A dictionary entry is a short phrase, not an arbitrary pasteboard payload.
/// Ask NSString for its encoded length before touching `UTF8String`: CStr would
/// otherwise scan an attacker-controlled selection without any upper bound.
const SERVICE_TEXT_MAX_UTF8_BYTES: usize = vocalcode_core::limits::MAX_DICTIONARY_SIDE_UTF8_BYTES;
const NS_UTF8_STRING_ENCODING: usize = 4;

fn normalize_service_utf8(bytes: &[u8]) -> Option<String> {
    if bytes.len() > SERVICE_TEXT_MAX_UTF8_BYTES {
        return None;
    }
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

unsafe fn set_service_error(
    error: *mut *mut objc2::runtime::AnyObject,
    message: &'static std::ffi::CStr,
) {
    if error.is_null() {
        return;
    }
    let text: *mut objc2::runtime::AnyObject = objc2::msg_send![
        objc2::class!(NSString),
        stringWithUTF8String: message.as_ptr()
    ];
    if !text.is_null() {
        *error = text;
    }
}

unsafe fn service_text(
    pboard: *mut objc2::runtime::AnyObject,
    error: *mut *mut objc2::runtime::AnyObject,
) -> Option<String> {
    use objc2::runtime::AnyObject;

    if pboard.is_null() {
        return None;
    }
    let ty: *mut AnyObject = objc2::msg_send![
        objc2::class!(NSString),
        stringWithUTF8String: c"public.utf8-plain-text".as_ptr()
    ];
    let value: *mut AnyObject = objc2::msg_send![pboard, stringForType: ty];
    if value.is_null() {
        return None;
    }
    let byte_len: usize = objc2::msg_send![
        value,
        lengthOfBytesUsingEncoding: NS_UTF8_STRING_ENCODING
    ];
    if byte_len > SERVICE_TEXT_MAX_UTF8_BYTES {
        set_service_error(
            error,
            c"The selected text is too large for a VocalCode dictionary entry.",
        );
        return None;
    }
    if byte_len == 0 {
        return None;
    }
    let utf8: *const std::ffi::c_char = objc2::msg_send![value, UTF8String];
    if utf8.is_null() {
        set_service_error(error, c"The selected text could not be read as UTF-8.");
        return None;
    }
    let bytes = std::slice::from_raw_parts(utf8.cast::<u8>(), byte_len);
    normalize_service_utf8(bytes)
}

/// `- (void)addToDictionary:(NSPasteboard *)pboard userData:(NSString *)data error:(NSString **)err`
///
/// The selector must match `NSMessage` in Info.plist exactly, argument names and
/// all; a mismatch is not an error anywhere, the menu item simply does nothing.
extern "C" fn add_to_dictionary(
    _this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
    pboard: *mut objc2::runtime::AnyObject,
    _user_data: *mut objc2::runtime::AnyObject,
    error: *mut *mut objc2::runtime::AnyObject,
) {
    // Rust must never unwind through Objective-C's method trampoline. Release
    // builds abort on panic, so the installed sink is also written to avoid
    // panicking; this catch is the final guard for unwind-enabled debug/tests
    // and future callback changes.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(text) = (unsafe { service_text(pboard, error) }) else {
            return;
        };
        if let Some(sink) = TEACH_SINK.get() {
            sink(text);
        }
    }));
    if result.is_err() {
        unsafe {
            set_service_error(
                error,
                c"VocalCode could not accept the selected text. Please try the in-app Teach action.",
            )
        };
    }
}

/// Register the provider that backs the Services menu item.
///
/// Called once at startup. Failing is not worth reporting to the user: the
/// in-app Teach button does the same job, and a Services menu that is missing
/// looks like macOS being macOS rather than like the app being broken — which is
/// also why that button exists rather than this being the only door.
pub fn install_dictionary_service(on_text: impl Fn(String) + Send + Sync + 'static) {
    use objc2::runtime::{AnyClass, AnyObject, ClassBuilder};

    if TEACH_SINK.set(Box::new(on_text)).is_err() {
        return; // already installed
    }
    unsafe {
        let Some(superclass) = AnyClass::get(c"NSObject") else {
            log::warn!("services: no NSObject; not registering");
            return;
        };
        let Some(mut builder) = ClassBuilder::new(c"VocalCodeServiceProvider", superclass) else {
            log::warn!("services: could not declare the provider class");
            return;
        };
        builder.add_method(
            objc2::sel!(addToDictionary:userData:error:),
            add_to_dictionary
                as extern "C" fn(
                    *mut AnyObject,
                    objc2::runtime::Sel,
                    *mut AnyObject,
                    *mut AnyObject,
                    *mut *mut AnyObject,
                ),
        );
        let cls = builder.register();
        let provider: *mut AnyObject = objc2::msg_send![cls, new];
        let app: *mut AnyObject = objc2::msg_send![objc2::class!(NSApplication), sharedApplication];
        if app.is_null() {
            log::warn!("services: no NSApplication yet");
            return;
        }
        let _: () = objc2::msg_send![app, setServicesProvider: provider];
        // Ask the system to re-read our NSServices now. Without it the item only
        // appears after the services cache happens to refresh, which can be a
        // login away — the difference between "it works" and "it works
        // tomorrow".
        NSUpdateDynamicServices();
        log::info!("services: dictionary provider registered");
    }
}

#[link(name = "AppKit", kind = "framework")]
extern "C" {
    fn NSUpdateDynamicServices();
}

// ---------------------------------------------------------------------------
// The Edit menu, which is what makes ⌘V work at all
// ---------------------------------------------------------------------------

/// Install a minimal main menu so the standard editing shortcuts reach the
/// WebView.
///
/// Without it **⌘V does nothing anywhere in the window** — verified with a
/// native synthetic key event and by watching the field stay empty. WKWebView
/// does not implement paste itself; it relies on
/// the responder chain reaching a menu item whose action is `paste:`, and this
/// app had no menu bar at all. That is not only the dictionary's problem: the
/// licence key field is in the same window, and a licence key is the one thing
/// every buyer pastes.
///
/// Built by hand rather than pulled from a menu crate because it is four items
/// with standard AppKit selectors, and the responder chain does the work — we
/// are not handling these, only giving the shortcuts somewhere to land.
pub fn install_edit_menu() {
    use objc2::runtime::AnyObject;

    unsafe fn nsstring(s: &str) -> *mut objc2::runtime::AnyObject {
        let c = std::ffi::CString::new(s).unwrap_or_default();
        objc2::msg_send![objc2::class!(NSString), stringWithUTF8String: c.as_ptr()]
    }

    unsafe {
        let app: *mut AnyObject = objc2::msg_send![objc2::class!(NSApplication), sharedApplication];
        if app.is_null() {
            log::warn!("edit menu: no NSApplication");
            return;
        }
        let main_menu: *mut AnyObject = objc2::msg_send![objc2::class!(NSMenu), new];

        // An app menu has to come first — the first item of the main menu is
        // always treated as the application menu, and without it the Edit menu
        // would take that slot and its shortcuts would not be found.
        let app_item: *mut AnyObject = objc2::msg_send![objc2::class!(NSMenuItem), new];
        let app_menu: *mut AnyObject = objc2::msg_send![objc2::class!(NSMenu), new];
        let quit: *mut AnyObject = objc2::msg_send![objc2::class!(NSMenuItem), alloc];
        let quit: *mut AnyObject = objc2::msg_send![
            quit,
            initWithTitle: nsstring("Quit VocalCode"),
            action: objc2::sel!(terminate:),
            keyEquivalent: nsstring("q")
        ];
        let _: () = objc2::msg_send![app_menu, addItem: quit];
        let _: () = objc2::msg_send![app_item, setSubmenu: app_menu];
        let _: () = objc2::msg_send![main_menu, addItem: app_item];

        let edit_item: *mut AnyObject = objc2::msg_send![objc2::class!(NSMenuItem), new];
        let edit_menu: *mut AnyObject = objc2::msg_send![objc2::class!(NSMenu), alloc];
        let edit_menu: *mut AnyObject =
            objc2::msg_send![edit_menu, initWithTitle: nsstring("Edit")];
        for (title, selector, key) in [
            ("Undo", "undo:", "z"),
            ("Redo", "redo:", "Z"),
            ("Cut", "cut:", "x"),
            ("Copy", "copy:", "c"),
            ("Paste", "paste:", "v"),
            ("Select All", "selectAll:", "a"),
        ] {
            let name = std::ffi::CString::new(selector).unwrap_or_default();
            let sel = objc2::runtime::Sel::register(&name);
            let item: *mut AnyObject = objc2::msg_send![objc2::class!(NSMenuItem), alloc];
            let item: *mut AnyObject = objc2::msg_send![
                item,
                initWithTitle: nsstring(title),
                action: sel,
                keyEquivalent: nsstring(key)
            ];
            let _: () = objc2::msg_send![edit_menu, addItem: item];
        }
        let _: () = objc2::msg_send![edit_item, setSubmenu: edit_menu];
        let _: () = objc2::msg_send![main_menu, addItem: edit_item];

        let _: () = objc2::msg_send![app, setMainMenu: main_menu];
        log::info!("edit menu installed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_dictionary_text_is_trimmed_and_bounded_before_allocation() {
        assert_eq!(
            normalize_service_utf8("  握口扣  ".as_bytes()).as_deref(),
            Some("握口扣")
        );
        assert!(normalize_service_utf8(&[]).is_none());
        assert!(normalize_service_utf8(&vec![b'a'; SERVICE_TEXT_MAX_UTF8_BYTES]).is_some());
        assert!(normalize_service_utf8(&vec![b'a'; SERVICE_TEXT_MAX_UTF8_BYTES + 1]).is_none());
    }
}
