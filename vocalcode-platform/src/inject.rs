//! Text insertion + "send" key via `enigo` (cross-platform input synthesis).
//!
//! A fresh `Enigo` is created per call so this type stays `Send`-free of any
//! OS handle and can be constructed once and shared on the engine thread.
//!
//! Two insertion modes, and the default is the synthesised one.
//!
//! "Typing" is not character-by-character on macOS: enigo routes it through
//! `CGEventKeyboardSetUnicodeString`, which carries the whole string on a
//! single keyboard event (in 20-character chunks, working around a truncation
//! bug in that API). It is already effectively instantaneous and handles 中文
//! correctly, so pasting buys nothing on speed or on CJK.
//!
//! What pasting does buy is reaching applications that ignore synthesised text
//! altogether. It costs a clipboard round trip: the previous contents are
//! stashed and put back, but that restore races whatever the target app is
//! doing, so it is opt-in rather than the default.

use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use vocalcode_core::error::{Result, VocalCodeError};
#[cfg(any(windows, target_os = "macos", test))]
use vocalcode_core::limits::MAX_DICTIONARY_SIDE_UTF8_BYTES;
use vocalcode_core::traits::TextInjector;

/// Serialises every clipboard mutation initiated by this process.
///
/// Both dictation-by-paste and Teach temporarily replace the clipboard and
/// restore the complete original object afterwards.  Without one shared lock,
/// two independent worker threads can interleave those snapshots and restore
/// them in the wrong order, losing the user's clipboard.  History -> Copy uses
/// the same lock so a direct write cannot be silently undone by an older
/// transaction finishing a moment later.
static CLIPBOARD_TRANSACTION: Mutex<()> = Mutex::new(());

fn lock_clipboard_transaction() -> MutexGuard<'static, ()> {
    // A panic in unrelated UI work must not permanently disable clipboard
    // recovery. The guarded state lives in the OS, not in this mutex, so there
    // is no Rust invariant to distrust after poison recovery.
    CLIPBOARD_TRANSACTION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Opaque identity of one foreground text target.
///
/// Delivery chooses the safe editable control that is focused when text is
/// ready, then this token pins that exact control for every insertion chunk and
/// destructive operation. A later focus change stops the in-progress action.
#[cfg(not(any(windows, target_os = "macos")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusToken(u64, u64);

/// UI Automation exposes the focused *element* inside Chromium, Electron and
/// WebView2 renderer processes. `GetGUIThreadInfo` only returns their shared
/// top-level host HWND, which cannot distinguish an editor from a password
/// field in the same window.
#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusToken {
    process_id: u32,
    foreground_window: u64,
    runtime_id: Vec<i32>,
}

#[cfg(target_os = "macos")]
struct MacFocusElement(core_foundation::base::CFTypeRef);

// AXUIElement is an immutable Core Foundation proxy. Retaining it lets the
// engine compare the exact remote control at delivery time with CFEqual rather
// than trusting a lossy hash or only the frontmost process ID.
#[cfg(target_os = "macos")]
unsafe impl Send for MacFocusElement {}
#[cfg(target_os = "macos")]
unsafe impl Sync for MacFocusElement {}

#[cfg(target_os = "macos")]
impl Clone for MacFocusElement {
    fn clone(&self) -> Self {
        use core_foundation::base::CFRetain;
        Self(unsafe { CFRetain(self.0) })
    }
}

#[cfg(target_os = "macos")]
impl Drop for MacFocusElement {
    fn drop(&mut self) {
        use core_foundation::base::CFRelease;
        unsafe { CFRelease(self.0) };
    }
}

#[cfg(target_os = "macos")]
impl std::fmt::Debug for MacFocusElement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("AXUIElement")
            .field(&(self.0 as usize))
            .finish()
    }
}

#[cfg(target_os = "macos")]
impl PartialEq for MacFocusElement {
    fn eq(&self, other: &Self) -> bool {
        use core_foundation::base::CFEqual;
        unsafe { CFEqual(self.0, other.0) != 0 }
    }
}

#[cfg(target_os = "macos")]
impl Eq for MacFocusElement {}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
pub struct FocusToken {
    /// `None` when the system named the owning application but refused to name
    /// the focused control. `current_focus` explains when that happens.
    element: Option<MacFocusElement>,
    process_id: u64,
}

// Compare the exact control only when both sides carry one. A token that names
// just the process still pins delivery to one application, which is all the
// pre-Accessibility implementation ever checked. Demanding an element on both
// sides would reject every utterance on a system whose system-wide focus query
// is unavailable, which is the failure this fallback exists to survive.
#[cfg(target_os = "macos")]
impl PartialEq for FocusToken {
    fn eq(&self, other: &Self) -> bool {
        if self.process_id != other.process_id {
            return false;
        }
        match (&self.element, &other.element) {
            (Some(left), Some(right)) => left == right,
            _ => true,
        }
    }
}

#[cfg(target_os = "macos")]
impl Eq for FocusToken {}

fn validate_focus(expected: Option<FocusToken>, actual: Option<FocusToken>) -> Result<()> {
    let Some(expected) = expected else {
        return Err(VocalCodeError::Inject(
            "could not identify the focused target when recording started; text was not inserted"
                .into(),
        ));
    };
    if actual != Some(expected) {
        return Err(VocalCodeError::Inject(
            "focused target changed before the operation completed; text was not inserted".into(),
        ));
    }
    Ok(())
}

fn select_current_delivery_target<T>(
    _recording_start: Option<&T>,
    current_safe_target: Option<T>,
) -> Option<T> {
    // The recording-start observation is deliberately not authoritative. UIA
    // providers may recreate a WebView2 element while the user stays in the
    // same composer, and users reasonably expect a deliberate focus move made
    // during transcription to choose the destination. `focused_text_target`
    // has already proved that the current control is editable and non-secure;
    // insertion re-arms to its exact identity and revalidates every chunk.
    current_safe_target
}

#[cfg(any(windows, test))]
struct WindowsFocusCandidate<'a> {
    foreground_window: u64,
    foreground_process_id: u32,
    element_process_id: u32,
    native_window: u64,
    native_root_window: u64,
    runtime_id: &'a [i32],
}

#[cfg(any(windows, test))]
fn windows_focus_candidate_is_safe(candidate: &WindowsFocusCandidate<'_>) -> bool {
    candidate.foreground_window != 0
        && candidate.foreground_process_id != 0
        && candidate.element_process_id != 0
        && candidate.native_window != 0
        && candidate.native_root_window == candidate.foreground_window
        && !candidate.runtime_id.is_empty()
}

#[cfg(windows)]
fn windows_native_window_ancestor(
    automation: &windows::Win32::UI::Accessibility::IUIAutomation,
    focused: &windows::Win32::UI::Accessibility::IUIAutomationElement,
) -> Option<windows::Win32::Foundation::HWND> {
    // Raw View includes the renderer-host bridge used by Chrome, Electron and
    // WebView2. Control View may omit the HWND-bearing host ancestor entirely.
    const MAX_ANCESTORS: usize = 256;

    let walker = unsafe { automation.RawViewWalker() }.ok()?;
    let mut current = focused.clone();
    for _ in 0..MAX_ANCESTORS {
        if let Ok(window) = unsafe { current.CurrentNativeWindowHandle() } {
            if !window.0.is_null() {
                return Some(window);
            }
        }
        current = unsafe { walker.GetParentElement(&current) }.ok()?;
    }
    None
}

#[cfg(windows)]
fn with_windows_focused<R>(
    operation: impl FnOnce(
        &windows::Win32::UI::Accessibility::IUIAutomationElement,
        FocusToken,
    ) -> Option<R>,
) -> Option<R> {
    use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetAncestor, GetForegroundWindow, GetWindowThreadProcessId, GA_ROOT,
    };

    struct ComApartment(bool);
    impl Drop for ComApartment {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // A thread already initialized as STA can still use UI Automation; it must
    // not balance another component's initialization with CoUninitialize.
    let _apartment = if initialized.is_ok() {
        ComApartment(true)
    } else if initialized == RPC_E_CHANGED_MODE {
        ComApartment(false)
    } else {
        return None;
    };

    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_null() {
        return None;
    }
    let mut foreground_process_id = 0_u32;
    if unsafe { GetWindowThreadProcessId(foreground, &mut foreground_process_id) } == 0
        || foreground_process_id == 0
    {
        return None;
    }

    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }.ok()?;
    let element = unsafe { automation.GetFocusedElement() }.ok()?;
    // Keep the renderer PID in the identity token, but never compare it with
    // the host-window PID: WebView2 deliberately places them in different
    // processes. HWND ancestry below is the window-membership authority.
    let process_id = unsafe { element.CurrentProcessId() }.ok()?;
    if process_id <= 0 {
        return None;
    }
    let runtime_id = unsafe { element.GetRuntimeId() }.ok()?;
    let runtime_id = unsafe { copy_runtime_id(runtime_id) }?;
    let native_window = windows_native_window_ancestor(&automation, &element)?;
    let native_root_window = unsafe { GetAncestor(native_window.0, GA_ROOT) };
    let candidate = WindowsFocusCandidate {
        foreground_window: foreground as usize as u64,
        foreground_process_id,
        element_process_id: process_id as u32,
        native_window: native_window.0 as usize as u64,
        native_root_window: native_root_window as usize as u64,
        runtime_id: &runtime_id,
    };
    if !windows_focus_candidate_is_safe(&candidate) {
        return None;
    }
    // Close both races while a provider services the UIA parent walk: the same
    // exact element must still be focused and its root HWND must still be the
    // foreground window. Another field in the same WebView is not equivalent.
    if unsafe { GetForegroundWindow() } != foreground {
        return None;
    }
    let confirmed = unsafe { automation.GetFocusedElement() }.ok()?;
    if !unsafe { automation.CompareElements(&element, &confirmed) }
        .ok()?
        .as_bool()
        || unsafe { GetForegroundWindow() } != foreground
    {
        return None;
    }
    let token = FocusToken {
        process_id: process_id as u32,
        foreground_window: foreground as usize as u64,
        runtime_id,
    };
    operation(&element, token)
}

#[cfg(windows)]
pub fn current_focus() -> Option<FocusToken> {
    with_windows_focused(|_, token| Some(token))
}

#[cfg(windows)]
unsafe fn copy_runtime_id(array: *mut windows::Win32::System::Com::SAFEARRAY) -> Option<Vec<i32>> {
    use windows::Win32::System::Ole::{
        SafeArrayDestroy, SafeArrayGetDim, SafeArrayGetElement, SafeArrayGetLBound,
        SafeArrayGetUBound,
    };

    struct OwnedSafeArray(*mut windows::Win32::System::Com::SAFEARRAY);
    impl Drop for OwnedSafeArray {
        fn drop(&mut self) {
            if !self.0.is_null() {
                let _ = unsafe { SafeArrayDestroy(self.0) };
            }
        }
    }

    if array.is_null() {
        return None;
    }
    let array = OwnedSafeArray(array);
    if unsafe { SafeArrayGetDim(array.0) } != 1 {
        return None;
    }
    let lower = unsafe { SafeArrayGetLBound(array.0, 1) }.ok()?;
    let upper = unsafe { SafeArrayGetUBound(array.0, 1) }.ok()?;
    if upper < lower || upper - lower > 255 {
        return None;
    }
    let mut values = Vec::with_capacity((upper - lower + 1) as usize);
    for index in lower..=upper {
        let mut value = 0_i32;
        unsafe {
            SafeArrayGetElement(array.0, &index, (&mut value as *mut i32).cast()).ok()?;
        }
        values.push(value);
    }
    Some(values)
}

#[cfg(target_os = "macos")]
pub fn current_focus() -> Option<FocusToken> {
    use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::c_void;

    type AxUiElementRef = *const c_void;
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXUIElementCreateSystemWide() -> AxUiElementRef;
        fn AXUIElementCreateApplication(pid: i32) -> AxUiElementRef;
        fn AXUIElementCopyAttributeValue(
            element: AxUiElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> i32;
        fn AXUIElementGetPid(element: AxUiElementRef, pid: *mut i32) -> i32;
        fn AXUIElementSetMessagingTimeout(element: AxUiElementRef, timeout: f32) -> i32;
    }

    fn copy_attribute(owner: AxUiElementRef, name: &str) -> Option<CFTypeRef> {
        if owner.is_null() {
            return None;
        }
        if unsafe { AXUIElementSetMessagingTimeout(owner, 0.25) } != 0 {
            return None;
        }
        let attribute = CFString::new(name);
        let mut value: CFTypeRef = std::ptr::null();
        let copy_error = unsafe {
            AXUIElementCopyAttributeValue(owner, attribute.as_concrete_TypeRef(), &mut value)
        };
        (copy_error == 0 && !value.is_null()).then_some(value)
    }

    // Bound remote messaging prevents an unresponsive target from hanging the
    // engine indefinitely. The audio stream starts only after this capture
    // succeeds, so the timeout cannot leave a live microphone.
    fn copy_focused_element(owner: AxUiElementRef) -> Option<CFTypeRef> {
        copy_attribute(owner, "AXFocusedUIElement")
    }

    // Preferred: the system-wide element names the focused control directly.
    let system = unsafe { AXUIElementCreateSystemWide() };
    let system_focused = copy_focused_element(system);
    if !system.is_null() {
        unsafe { CFRelease(system.cast()) };
    }
    if let Some(focused) = system_focused {
        let mut pid = 0_i32;
        let pid_error = unsafe { AXUIElementGetPid(focused.cast(), &mut pid) };
        if pid_error == 0 && pid > 0 {
            return Some(FocusToken {
                element: Some(MacFocusElement(focused)),
                process_id: pid as u64,
            });
        }
        unsafe { CFRelease(focused) };
    }

    // Fallback. Every attribute of the system-wide element answers
    // kAXErrorCannotComplete on some macOS builds, including machines whose
    // per-application Accessibility works normally. That used to fail the whole
    // utterance before a microphone ever opened, so ask the frontmost
    // application instead — the same target the pre-Accessibility
    // implementation resolved through System Events.
    let pid = frontmost_process_id()?;
    let application = unsafe { AXUIElementCreateApplication(pid) };
    if application.is_null() {
        return None;
    }
    let element = copy_focused_element(application).or_else(|| {
        // On macOS 26, Electron applications can answer no focused element on
        // the application object while still publishing it on the focused
        // window. Follow that window before falling back to the legacy
        // process-only identity.
        let window = copy_attribute(application, "AXFocusedWindow")?;
        let focused = copy_focused_element(window.cast());
        unsafe { CFRelease(window) };
        focused
    });
    unsafe { CFRelease(application.cast()) };
    // Keep a process-only token when macOS cannot name the control. Delivery
    // retains the legacy process-pinned fallback for affected Electron apps.
    Some(FocusToken {
        element: element.map(MacFocusElement),
        process_id: pid as u64,
    })
}

/// PID of the application the user is typing into, via AppKit rather than
/// Accessibility so it stays available when the system-wide element does not.
#[cfg(target_os = "macos")]
fn frontmost_process_id() -> Option<i32> {
    let application = objc2_app_kit::NSWorkspace::sharedWorkspace().frontmostApplication()?;
    // The generated `processIdentifier` accessor is gated behind objc2-app-kit's
    // `libc` feature purely for its `pid_t` return type. Send the selector so
    // one integer does not add a dependency to every release inventory.
    let pid: i32 = unsafe { objc2::msg_send![&*application, processIdentifier] };
    // Applications without a pid answer -1.
    (pid > 0).then_some(pid)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn current_focus() -> Option<FocusToken> {
    None
}

/// Maximum complete control value inspected while looking for a correction.
/// The eventual dictionary sides have a much smaller independent bound; this
/// larger ceiling only allows a short dictated phrase to sit inside a normal
/// chat composer that already contains some text.
const MAX_CORRECTION_FIELD_UTF8_BYTES: usize = 64 * 1024;

/// Where a finished transcript goes, decided when the words are ready rather
/// than when the utterance began.
///
/// A transcript follows only the exact editable control captured when recording
/// began. A moved, missing, read-only, or secure target uses the clipboard
/// recovery path; preserving the words must not silently change their audience.
#[derive(Debug)]
enum Delivery {
    /// Type into the control this token names.
    Type(FocusToken),
    /// Nothing focused will take dictation. The transcript goes to the
    /// clipboard, which is recoverable; discarding it is not.
    Clipboard,
}

/// What the user is told when a transcript could not be typed anywhere. It
/// names the recovery, because "text was not inserted" reads as "your words are
/// gone" to the person who just spoke a paragraph.
fn no_text_target_message() -> String {
    let paste = if cfg!(target_os = "macos") {
        "Cmd+V"
    } else {
        "Ctrl+V"
    };
    format!(
        "No text field was focused, so the transcript was copied to the clipboard \u{2014} paste it with {paste}."
    )
}

/// The focused control, when synthesised text would actually land there as
/// text.
///
/// `None` covers the two cases where typing would be worse than not typing: a
/// password field, and a window that has no text target at all, where the
/// characters would arrive as a burst of keyboard shortcuts.
#[cfg(windows)]
fn focused_text_target() -> Option<FocusToken> {
    use windows::Win32::UI::Accessibility::{
        IUIAutomationTextPattern, IUIAutomationValuePattern, UIA_DocumentControlTypeId,
        UIA_EditControlTypeId, UIA_TextPatternId, UIA_ValuePatternId,
    };

    with_windows_focused(|element, token| {
        if unsafe { element.CurrentIsPassword() }.ok()?.as_bool() {
            return None;
        }
        // A read-only ValuePattern is the one case where the provider says
        // outright that typing will not take, so believe it. Everything below is
        // an accept: the patterns and control types a provider exposes when text
        // goes there. A desktop, a game or a file list exposes none of them.
        if let Ok(value) =
            unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
        {
            if unsafe { value.CurrentIsReadOnly() }.ok()?.as_bool() {
                return None;
            }
            return Some(token);
        }
        let control_type = unsafe { element.CurrentControlType() }.ok()?;
        if control_type == UIA_EditControlTypeId || control_type == UIA_DocumentControlTypeId {
            return Some(token);
        }
        unsafe { element.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) }
            .ok()
            .map(|_| token)
    })
}

#[cfg(target_os = "macos")]
fn focused_text_target() -> Option<FocusToken> {
    use core_foundation::base::CFTypeRef;
    use core_foundation::string::CFStringRef;
    use std::ffi::c_void;

    type AxUiElementRef = *const c_void;
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXUIElementCopyAttributeValue(
            element: AxUiElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> i32;
        fn AXUIElementSetMessagingTimeout(element: AxUiElementRef, timeout: f32) -> i32;
    }

    fn string_attribute(element: AxUiElementRef, name: &str) -> Option<String> {
        use core_foundation::base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType};
        use core_foundation::string::{CFString, CFStringGetTypeID};

        let attribute = CFString::new(name);
        let mut value: CFTypeRef = std::ptr::null();
        if unsafe {
            AXUIElementCopyAttributeValue(element, attribute.as_concrete_TypeRef(), &mut value)
        } != 0
            || value.is_null()
        {
            return None;
        }
        if unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() } {
            unsafe { CFRelease(value) };
            return None;
        }
        Some(unsafe { CFString::wrap_under_create_rule(value.cast()) }.to_string())
    }

    let token = current_focus()?;
    // Some macOS/Electron combinations publish neither application-level nor
    // window-level AXFocusedUIElement even while their editor owns the caret.
    // Preserve the pre-1.1 behaviour in that case: pin delivery to the
    // frontmost process. Exact controls still receive the stricter role and
    // secure-field checks below.
    let Some(element) = token.element.as_ref() else {
        return Some(token);
    };
    let element = element.0.cast();
    if unsafe { AXUIElementSetMessagingTimeout(element, 0.25) } != 0 {
        return None;
    }
    if string_attribute(element, "AXSubrole").as_deref() == Some("AXSecureTextField") {
        return None;
    }
    let role = string_attribute(element, "AXRole")?;
    matches!(role.as_str(), "AXTextField" | "AXTextArea" | "AXComboBox").then_some(token)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn focused_text_target() -> Option<FocusToken> {
    // No accessibility layer is wired up here, so there is nothing to ask. The
    // foreground target is the answer, exactly as it was for the old check.
    current_focus()
}

#[cfg(any(windows, test))]
fn normalize_windows_editable_value(value: String, accessible_name: &str) -> String {
    // Chromium/ProseMirror can expose an empty composer through ValuePattern
    // as `\n<placeholder>` while CurrentName contains that same placeholder.
    // Visually the field is empty, and this is the post-Enter state the
    // correction watcher needs. Require the line-break wrapper as well as an
    // exact non-empty name match so ordinary controls whose value happens to
    // equal their accessible label are never erased.
    let trimmed = value.trim_matches(['\r', '\n']);
    let name = accessible_name.trim();
    if trimmed.len() != value.len() && !name.is_empty() && trimmed == name {
        String::new()
    } else {
        value
    }
}

#[cfg(windows)]
fn focused_editable_value(expected: &FocusToken) -> Option<String> {
    use windows::Win32::UI::Accessibility::{
        IUIAutomationTextPattern, IUIAutomationValuePattern, UIA_EditControlTypeId,
        UIA_TextPatternId, UIA_ValuePatternId,
    };

    with_windows_focused(|element, actual| {
        if &actual != expected || unsafe { element.CurrentIsPassword() }.ok()?.as_bool() {
            return None;
        }
        // Prefer the narrow ValuePattern. Some Chromium/Electron and rich edit
        // controls expose only TextPattern, so allow that fallback strictly on
        // a focused Edit control. The control-type gate prevents reading a
        // whole document merely because its caret happens to be focused, and
        // GetText's own bound prevents an untrusted provider from returning an
        // arbitrarily large BSTR.
        let value = if let Ok(pattern) =
            unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
        {
            unsafe { pattern.CurrentValue() }.ok()?.to_string()
        } else {
            if unsafe { element.CurrentControlType() }.ok() != Some(UIA_EditControlTypeId) {
                return None;
            }
            let pattern: IUIAutomationTextPattern =
                unsafe { element.GetCurrentPatternAs(UIA_TextPatternId) }.ok()?;
            let range = unsafe { pattern.DocumentRange() }.ok()?;
            unsafe { range.GetText(MAX_CORRECTION_FIELD_UTF8_BYTES as i32 + 1) }
                .ok()?
                .to_string()
        };
        if value.len() > MAX_CORRECTION_FIELD_UTF8_BYTES {
            return None;
        }
        let accessible_name = unsafe { element.CurrentName() }
            .ok()
            .map(|name| name.to_string())
            .unwrap_or_default();
        Some(normalize_windows_editable_value(value, &accessible_name))
    })
}

#[cfg(target_os = "macos")]
fn focused_editable_value(expected: &FocusToken) -> Option<String> {
    use core_foundation::base::CFTypeRef;
    use core_foundation::string::CFStringRef;
    use std::ffi::c_void;

    type AxUiElementRef = *const c_void;
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXUIElementCopyAttributeValue(
            element: AxUiElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> i32;
        fn AXUIElementSetMessagingTimeout(element: AxUiElementRef, timeout: f32) -> i32;
    }

    fn string_attribute(element: AxUiElementRef, name: &str) -> Option<String> {
        use core_foundation::base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType};
        use core_foundation::string::{CFString, CFStringGetTypeID};

        let attribute = CFString::new(name);
        let mut value: CFTypeRef = std::ptr::null();
        if unsafe {
            AXUIElementCopyAttributeValue(element, attribute.as_concrete_TypeRef(), &mut value)
        } != 0
            || value.is_null()
        {
            return None;
        }
        if unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() } {
            unsafe { CFRelease(value) };
            return None;
        }
        Some(unsafe { CFString::wrap_under_create_rule(value.cast()) }.to_string())
    }

    validate_focus(Some(expected.clone()), current_focus()).ok()?;
    // Reading a field back needs the exact control. An application-only token
    // cannot name one, so skip the correction rather than guessing at a field.
    let element = expected.element.as_ref()?.0.cast();
    if unsafe { AXUIElementSetMessagingTimeout(element, 0.25) } != 0 {
        return None;
    }
    let role = string_attribute(element, "AXRole")?;
    if !matches!(role.as_str(), "AXTextField" | "AXTextArea" | "AXComboBox") {
        return None;
    }
    if string_attribute(element, "AXSubrole").as_deref() == Some("AXSecureTextField") {
        return None;
    }
    let value = string_attribute(element, "AXValue")?;
    if value.len() > MAX_CORRECTION_FIELD_UTF8_BYTES {
        return None;
    }
    validate_focus(Some(expected.clone()), current_focus()).ok()?;
    Some(value)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn focused_editable_value(_expected: &FocusToken) -> Option<String> {
    None
}

/// A lifecycle event from the bounded, same-control correction watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorrectionEvent {
    Started {
        session: u64,
    },
    Learned {
        session: u64,
        pairs: Vec<(String, String)>,
    },
    Stopped {
        session: u64,
    },
}

#[derive(Debug)]
struct CorrectionRequest {
    session: u64,
    focus: FocusToken,
    inserted_text: String,
    window: Duration,
}

#[derive(Default)]
struct CorrectionMonitorState {
    generation: u64,
    request: Option<CorrectionRequest>,
    stop: bool,
}

struct CorrectionMonitorShared {
    state: Mutex<CorrectionMonitorState>,
    wake: Condvar,
}

/// Watches only the exact editable control that just accepted a VocalCode
/// transcript. One joinable worker owns all polling: a newer utterance replaces
/// the older request, so dictation can never accumulate detached watchers.
pub struct CorrectionMonitor {
    shared: Arc<CorrectionMonitorShared>,
    events: std::sync::mpsc::Receiver<CorrectionEvent>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Default for CorrectionMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl CorrectionMonitor {
    pub fn new() -> Self {
        let shared = Arc::new(CorrectionMonitorShared {
            state: Mutex::new(CorrectionMonitorState::default()),
            wake: Condvar::new(),
        });
        let (event_tx, events) = std::sync::mpsc::sync_channel(8);
        let worker_shared = shared.clone();
        let worker = thread::Builder::new()
            .name("vocalcode-correction-monitor".to_string())
            .spawn(move || correction_monitor_loop(worker_shared, event_tx))
            .map_err(|error| {
                log::error!("could not start correction monitor: {error}");
                error
            })
            .ok();
        Self {
            shared,
            events,
            worker,
        }
    }

    /// Replace any previous watch and return the new generation. A zero window
    /// is the user-facing Off setting and therefore behaves exactly like cancel.
    pub fn arm(&self, inserted: &str, window: Duration) -> u64 {
        let session = {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.generation = state.generation.wrapping_add(1).max(1);
            state.request = None;
            state.generation
        };
        // Capture the exact control immediately, but let the one existing
        // worker wait briefly for its accessibility value to catch up with the
        // just-completed synthetic input. SendInput/CGEvent delivery can become
        // visible a few frames after the injector reports success.
        let request = (!inserted.is_empty() && !window.is_zero())
            .then(current_focus)
            .flatten()
            .map(|focus| CorrectionRequest {
                session,
                focus,
                inserted_text: inserted.to_string(),
                window,
            });
        if let Some(request) = request {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !state.stop && state.generation == session {
                state.request = Some(request);
            }
        }
        self.shared.wake.notify_one();
        session
    }

    pub fn cancel(&self) -> u64 {
        self.arm("", Duration::ZERO)
    }

    pub fn generation(&self) -> u64 {
        self.shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generation
    }

    pub fn try_recv(&self) -> Option<CorrectionEvent> {
        self.events.try_recv().ok()
    }
}

impl Drop for CorrectionMonitor {
    fn drop(&mut self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.stop = true;
        state.request = None;
        drop(state);
        self.shared.wake.notify_all();
        if self
            .worker
            .take()
            .is_some_and(|worker| worker.join().is_err())
        {
            log::error!("correction monitor panicked during shutdown");
        }
    }
}

fn correction_monitor_loop(
    shared: Arc<CorrectionMonitorShared>,
    events: std::sync::mpsc::SyncSender<CorrectionEvent>,
) {
    let mut seen_generation = 0;
    loop {
        let request = {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !state.stop && state.generation == seen_generation {
                state = shared
                    .wake
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if state.stop {
                return;
            }
            seen_generation = state.generation;
            state.request.take()
        };
        let Some(request) = request else {
            continue;
        };
        watch_correction_request(&shared, request, &events);
    }
}

fn request_is_current(shared: &CorrectionMonitorShared, session: u64) -> bool {
    let state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    !state.stop && state.generation == session
}

fn wait_poll(shared: &CorrectionMonitorShared, session: u64) -> bool {
    let state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.stop || state.generation != session {
        return false;
    }
    let (state, _) = shared
        .wake
        .wait_timeout(state, Duration::from_millis(50))
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    !state.stop && state.generation == session
}

fn inserted_spans(value: &str, inserted: &str) -> Vec<std::ops::Range<usize>> {
    if inserted.is_empty() {
        return Vec::new();
    }
    value
        .match_indices(inserted)
        .map(|(byte_start, _)| {
            let start = value[..byte_start].chars().count();
            start..start + inserted.chars().count()
        })
        .collect()
}

#[cfg(test)]
fn unique_inserted_span(value: &str, inserted: &str) -> Option<std::ops::Range<usize>> {
    let mut spans = inserted_spans(value, inserted).into_iter();
    let span = spans.next()?;
    spans.next().is_none().then_some(span)
}

fn is_expandable_correction_word_char(value: char) -> bool {
    // Expansion only repairs a character diff that stopped inside the same
    // ASCII identifier/brand token (for example `voput c` vs `VocalC`). CJK
    // scripts do not use spaces between words, so treating every non-space as
    // one token would turn a one-word Chinese correction into a whole-sentence
    // global replacement rule.
    value.is_ascii_alphanumeric() || matches!(value, '_' | '-' | '\'')
}

// Automatic learning writes a global, case-insensitive substring rule.  That
// is useful for a name or a short piece of jargon, but far too powerful for a
// sentence rewrite: one accidental semantic edit could otherwise keep firing
// in unrelated future dictation.  Manual Teach remains the escape hatch for
// unusual phrases; the unattended watcher deliberately accepts less.
const MAX_AUTOMATIC_CORRECTION_CHARS: usize = 32;
const MAX_AUTOMATIC_CORRECTION_WORDS: usize = 3;
const MAX_AUTOMATIC_CORRECTION_CJK_CHARS: usize = 8;
const MAX_AUTOMATIC_CORRECTION_PAIRS: usize = 4;
const MAX_AUTOMATIC_DIFF_CHARS: usize = 512;

fn is_cjk_ideograph(value: char) -> bool {
    matches!(
        value,
        '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{20000}'..='\u{2FA1F}'
    )
}

fn is_sentence_punctuation(value: char) -> bool {
    matches!(
        value,
        '.' | ',' | '?' | '!' | ';' | ':' | '。' | '，' | '？' | '！' | '；' | '：' | '、'
    )
}

fn safe_automatic_correction_side(value: &str) -> bool {
    let chars = value.chars().count();
    chars > 0
        && chars <= MAX_AUTOMATIC_CORRECTION_CHARS
        && value.split_whitespace().count() <= MAX_AUTOMATIC_CORRECTION_WORDS
        && value
            .chars()
            .filter(|value| is_cjk_ideograph(*value))
            .count()
            <= MAX_AUTOMATIC_CORRECTION_CJK_CHARS
        && !value
            .chars()
            .any(|value| value.is_control() || is_sentence_punctuation(value))
}

fn safe_automatic_correction_pair(heard: &str, corrected: &str) -> bool {
    if !safe_automatic_correction_side(heard) || !safe_automatic_correction_side(corrected) {
        return false;
    }
    // A one-ideograph global substring rule is too broad to infer
    // automatically (for example `派 -> pi` rewrites every later use of 派).
    // Manual Teach remains available when that is genuinely intended.
    if [heard, corrected].iter().any(|value| {
        let mut chars = value.chars();
        chars.next().is_some_and(is_cjk_ideograph) && chars.next().is_none()
    }) {
        return false;
    }
    let shorter = heard.chars().count().min(corrected.chars().count());
    let longer = heard.chars().count().max(corrected.chars().count());
    // A very large expansion/contraction is normally composition rather than
    // repair.  The floor still permits short cross-script names such as
    // `靠迪 -> Collie`, whose character counts naturally differ.
    longer <= shorter.saturating_mul(4).max(8)
}

#[derive(Debug)]
struct CorrectionUnit {
    chars: std::ops::Range<usize>,
    text: String,
}

fn correction_units(chars: &[char]) -> Vec<CorrectionUnit> {
    let mut units = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let mut end = start + 1;
        let is_word = |value: char| {
            is_expandable_correction_word_char(value)
                || (!is_cjk_ideograph(value) && value.is_alphanumeric())
        };
        if is_word(chars[start]) {
            while end < chars.len() && is_word(chars[end]) {
                end += 1;
            }
        } else if chars[start].is_whitespace() {
            while end < chars.len() && chars[end].is_whitespace() {
                end += 1;
            }
        }
        units.push(CorrectionUnit {
            chars: start..end,
            text: chars[start..end].iter().collect(),
        });
        start = end;
    }
    units
}

fn correction_unit_range(
    units: &[CorrectionUnit],
    range: std::ops::Range<usize>,
    char_len: usize,
) -> std::ops::Range<usize> {
    let start = units
        .get(range.start)
        .map_or(char_len, |unit| unit.chars.start);
    let end = if range.end > range.start {
        units[range.end - 1].chars.end
    } else {
        start
    };
    start..end
}

/// Split one committed edit into independent short replacement rules. ASCII
/// words stay indivisible, while CJK remains character-addressable; this keeps
/// `voput code -> VocalCode` together but lets `靠迪和赛口 -> Collie和SQL`
/// become two rules. The bounded LCS is deliberately unavailable for very long
/// edits, where an unattended global dictionary mutation would be unsafe and
/// quadratic work would be inappropriate.
fn correction_pairs(
    baseline: &str,
    corrected: &str,
    inserted: std::ops::Range<usize>,
) -> Option<Vec<(String, String)>> {
    if baseline == corrected {
        return None;
    }
    let before = baseline.chars().collect::<Vec<_>>();
    let after = corrected.chars().collect::<Vec<_>>();
    if inserted.start > inserted.end || inserted.end > before.len() {
        return None;
    }
    let suffix_len = before.len() - inserted.end;
    if after.len() < inserted.start + suffix_len
        || before[..inserted.start] != after[..inserted.start]
        || before[inserted.end..] != after[after.len() - suffix_len..]
    {
        // An automatic global replacement is safe only when every edit stayed
        // inside this utterance. Surrounding pre-existing content is exact and
        // authoritative, including an accessibility placeholder suffix.
        return None;
    }
    let before_inner = &before[inserted.clone()];
    let after_inner = &after[inserted.start..after.len() - suffix_len];
    if before_inner.len() > MAX_AUTOMATIC_DIFF_CHARS || after_inner.len() > MAX_AUTOMATIC_DIFF_CHARS
    {
        return None;
    }

    let before_units = correction_units(before_inner);
    let after_units = correction_units(after_inner);
    let width = after_units.len() + 1;
    let mut lcs = vec![0_u16; (before_units.len() + 1) * width];
    for left in (0..before_units.len()).rev() {
        for right in (0..after_units.len()).rev() {
            lcs[left * width + right] = if before_units[left].text == after_units[right].text {
                1 + lcs[(left + 1) * width + right + 1]
            } else {
                lcs[(left + 1) * width + right].max(lcs[left * width + right + 1])
            };
        }
    }

    let mut hunks = Vec::new();
    let (mut left, mut right) = (0, 0);
    let mut changed_start = None;
    while left < before_units.len() || right < after_units.len() {
        let equal = left < before_units.len()
            && right < after_units.len()
            && before_units[left].text == after_units[right].text;
        if equal {
            if let Some((left_start, right_start)) = changed_start.take() {
                hunks.push((left_start..left, right_start..right));
            }
            left += 1;
            right += 1;
        } else {
            changed_start.get_or_insert((left, right));
            if right == after_units.len()
                || (left < before_units.len()
                    && lcs[(left + 1) * width + right] >= lcs[left * width + right + 1])
            {
                left += 1;
            } else {
                right += 1;
            }
        }
    }
    if let Some((left_start, right_start)) = changed_start {
        hunks.push((left_start..left, right_start..right));
    }
    if hunks.is_empty() || hunks.len() > MAX_AUTOMATIC_CORRECTION_PAIRS {
        return None;
    }

    let mut pairs: Vec<(String, String)> = Vec::with_capacity(hunks.len());
    for (before_hunk, after_hunk) in hunks {
        let before_chars = correction_unit_range(&before_units, before_hunk, before_inner.len());
        let after_chars = correction_unit_range(&after_units, after_hunk, after_inner.len());
        let heard = before_inner[before_chars].iter().collect::<String>();
        let corrected = after_inner[after_chars].iter().collect::<String>();
        let heard = heard.trim();
        let corrected = corrected.trim();
        if heard.is_empty()
            || corrected.is_empty()
            || heard == corrected
            || heard.contains(['\r', '\n'])
            || corrected.contains(['\r', '\n'])
            || heard.len() > MAX_DICTIONARY_SIDE_UTF8_BYTES
            || corrected.len() > MAX_DICTIONARY_SIDE_UTF8_BYTES
            || !safe_automatic_correction_pair(heard, corrected)
        {
            return None;
        }
        if let Some(existing) = pairs
            .iter()
            .find(|(from, _)| from.eq_ignore_ascii_case(heard))
        {
            if existing.1 != corrected {
                return None;
            }
            continue;
        }
        pairs.push((heard.to_string(), corrected.to_string()));
    }
    (!pairs.is_empty()).then_some(pairs)
}

fn correction_pairs_for_candidates(
    baseline: &str,
    latest: &str,
    inserted: &[std::ops::Range<usize>],
) -> Option<Vec<(String, String)>> {
    let mut learned = None;
    for candidate in inserted {
        let Some(pairs) = correction_pairs(baseline, latest, candidate.clone()) else {
            continue;
        };
        match &learned {
            None => learned = Some(pairs),
            Some(existing) if existing == &pairs => {}
            Some(_) => {
                learned = None;
                break;
            }
        }
    }
    learned
}

fn finish_correction(
    events: &std::sync::mpsc::SyncSender<CorrectionEvent>,
    session: u64,
    baseline: &str,
    latest: &str,
    inserted: &[std::ops::Range<usize>],
) {
    let event = correction_pairs_for_candidates(baseline, latest, inserted)
        .map_or(CorrectionEvent::Stopped { session }, |pairs| {
            CorrectionEvent::Learned { session, pairs }
        });
    let _ = events.try_send(event);
}

fn watch_correction_request(
    shared: &CorrectionMonitorShared,
    request: CorrectionRequest,
    events: &std::sync::mpsc::SyncSender<CorrectionEvent>,
) {
    let capture_deadline = Instant::now() + Duration::from_millis(1_000);
    let (baseline, inserted) = loop {
        if !request_is_current(shared, request.session) {
            return;
        }
        if let Some(value) = focused_editable_value(&request.focus) {
            let inserted = inserted_spans(&value, &request.inserted_text);
            if !inserted.is_empty() {
                break (value, inserted);
            }
        }
        if Instant::now() >= capture_deadline || !wait_poll(shared, request.session) {
            return;
        }
    };
    let deadline = Instant::now() + request.window;
    let edit_deadline = Duration::from_secs(120);
    let mut editing_since = None;
    let mut latest = baseline.clone();
    let mut submit_generation = crate::hotkey::correction_submit_generation();
    let mut pending_submit = None;

    loop {
        if !request_is_current(shared, request.session) || !wait_poll(shared, request.session) {
            return;
        }
        if editing_since.is_none() && Instant::now() >= deadline {
            return;
        }
        let observed_submit = crate::hotkey::correction_submit_generation();
        if observed_submit != submit_generation {
            submit_generation = observed_submit;
            pending_submit = Some(Instant::now());
        }
        let Some(value) = focused_editable_value(&request.focus) else {
            if editing_since.is_some() {
                if pending_submit.is_some() {
                    // React/ProseMirror editors commonly destroy the submitted
                    // DOM node before UI Automation can expose its empty value.
                    // A physical unmodified Enter observed while that exact
                    // node was still being watched is the authoritative commit
                    // signal; an ordinary click-away still has no signal and
                    // therefore remains fail-closed.
                    finish_correction(events, request.session, &baseline, &latest, &inserted);
                } else {
                    let _ = events.try_send(CorrectionEvent::Stopped {
                        session: request.session,
                    });
                }
            }
            return;
        };
        if value == baseline && editing_since.is_none() {
            continue;
        }
        if editing_since.is_none() {
            editing_since = Some(Instant::now());
            latest = value.clone();
            let _ = events.try_send(CorrectionEvent::Started {
                session: request.session,
            });
        }
        // Enter normally clears a chat/input bar. Preserve the last non-empty
        // edited value as the committed correction rather than comparing the
        // empty refreshed control with the original utterance.
        if value.is_empty() {
            finish_correction(events, request.session, &baseline, &latest, &inserted);
            return;
        }
        latest = value;
        if pending_submit.is_some_and(|pressed| pressed.elapsed() >= Duration::from_millis(300)) {
            // The same live control remained non-empty after Enter. In a
            // document editor that means Enter inserted a newline rather than
            // submitting; do not turn that into a global Dictionary rule.
            pending_submit = None;
        }
        // Pausing the user's 8-second window must not mean retaining a remote
        // accessibility control forever if they abandon the edit.
        if editing_since.is_some_and(|started| started.elapsed() >= edit_deadline) {
            let _ = events.try_send(CorrectionEvent::Stopped {
                session: request.session,
            });
            return;
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn clipboard_generation_is_owned(
    expected: isize,
    before: isize,
    after: isize,
    marker_matches: bool,
) -> bool {
    marker_matches && before == expected && after == expected
}

/// Pasteboard snapshots are a safety copy, not an invitation to duplicate an
/// arbitrarily large image or a pathological number of promised formats into
/// VocalCode's process. Refuse before mutating the clipboard; Engine will keep
/// failed dictation text in History, and Teach leaves the original untouched.
#[cfg(any(target_os = "macos", test))]
const MAC_CLIPBOARD_SNAPSHOT_MAX_ITEMS: usize = 128;
#[cfg(any(target_os = "macos", test))]
const MAC_CLIPBOARD_SNAPSHOT_MAX_TYPES: usize = 512;
#[cfg(any(target_os = "macos", test))]
const MAC_CLIPBOARD_SNAPSHOT_MAX_BYTES: usize = 64 * 1024 * 1024;

#[cfg(any(target_os = "macos", test))]
#[derive(Default)]
struct ClipboardSnapshotBudget {
    types: usize,
    bytes: usize,
}

#[cfg(any(target_os = "macos", test))]
impl ClipboardSnapshotBudget {
    fn accepts_items(items: usize) -> bool {
        items <= MAC_CLIPBOARD_SNAPSHOT_MAX_ITEMS
    }

    fn add_types(&mut self, count: usize) -> bool {
        let Some(total) = self.types.checked_add(count) else {
            return false;
        };
        if total > MAC_CLIPBOARD_SNAPSHOT_MAX_TYPES {
            return false;
        }
        self.types = total;
        true
    }

    fn add_bytes(&mut self, count: usize) -> bool {
        let Some(total) = self.bytes.checked_add(count) else {
            return false;
        };
        if total > MAC_CLIPBOARD_SNAPSHOT_MAX_BYTES {
            return false;
        }
        self.bytes = total;
        true
    }
}

#[cfg(target_os = "macos")]
fn clipboard_snapshot_too_large() -> VocalCodeError {
    VocalCodeError::Inject(
        "clipboard is too large or complex to preserve safely; no clipboard changes were made"
            .into(),
    )
}

#[cfg(target_os = "macos")]
struct MacClipboardSnapshot {
    items: Vec<objc2::rc::Retained<objc2_app_kit::NSPasteboardItem>>,
}

#[cfg(target_os = "macos")]
impl MacClipboardSnapshot {
    fn capture(pasteboard: &objc2_app_kit::NSPasteboard) -> Result<Self> {
        use objc2_app_kit::NSPasteboardItem;

        let Some(items) = pasteboard.pasteboardItems() else {
            if pasteboard.types().is_some_and(|types| !types.is_empty()) {
                return Err(VocalCodeError::Inject(
                    "could not snapshot the complete clipboard before paste".into(),
                ));
            }
            return Ok(Self { items: Vec::new() });
        };
        if !ClipboardSnapshotBudget::accepts_items(items.len()) {
            return Err(clipboard_snapshot_too_large());
        }

        let mut copies = Vec::with_capacity(items.len());
        let mut budget = ClipboardSnapshotBudget::default();
        for item in items.to_vec() {
            let copy = NSPasteboardItem::new();
            let types = item.types();
            if !budget.add_types(types.len()) {
                return Err(clipboard_snapshot_too_large());
            }
            for data_type in types.to_vec() {
                let data = item.dataForType(&data_type).ok_or_else(|| {
                    VocalCodeError::Inject(format!(
                        "could not snapshot clipboard type {data_type:?}"
                    ))
                })?;
                if !budget.add_bytes(data.length()) {
                    return Err(clipboard_snapshot_too_large());
                }
                if !copy.setData_forType(&data, &data_type) {
                    return Err(VocalCodeError::Inject(format!(
                        "could not snapshot clipboard type {data_type:?}"
                    )));
                }
            }
            copies.push(copy);
        }
        Ok(Self { items: copies })
    }

    fn restore(&self, pasteboard: &objc2_app_kit::NSPasteboard) -> Result<()> {
        use objc2::runtime::ProtocolObject;
        use objc2_app_kit::NSPasteboardWriting;
        use objc2_foundation::NSArray;

        let writers = self
            .items
            .iter()
            .cloned()
            .map(ProtocolObject::<dyn NSPasteboardWriting>::from_retained)
            .collect::<Vec<_>>();
        let writer_array = NSArray::from_retained_slice(&writers);
        pasteboard.clearContents();
        if writers.is_empty() || pasteboard.writeObjects(&writer_array) {
            Ok(())
        } else {
            Err(VocalCodeError::Inject(
                "clipboard restore failed; the transcript remains available in VocalCode History"
                    .into(),
            ))
        }
    }

    fn restore_if_generation(
        &self,
        pasteboard: &objc2_app_kit::NSPasteboard,
        expected_generation: isize,
    ) -> Result<bool> {
        let before = pasteboard.changeCount();
        if before != expected_generation {
            return Ok(false);
        }
        let immediately_before_restore = pasteboard.changeCount();
        if immediately_before_restore != expected_generation {
            return Ok(false);
        }
        self.restore(pasteboard)?;
        Ok(true)
    }

    fn replace_with_temporary_text(
        &self,
        pasteboard: &objc2_app_kit::NSPasteboard,
        text: &str,
    ) -> Result<MacClipboardReceipt> {
        use objc2::runtime::ProtocolObject;
        use objc2_app_kit::{NSPasteboardItem, NSPasteboardTypeString, NSPasteboardWriting};
        use objc2_foundation::{NSArray, NSString};
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT_MARKER: AtomicU64 = AtomicU64::new(1);
        const MARKER_TYPE: &str = "app.vocalcode.temporary-clipboard-transaction";

        let nonce = NEXT_MARKER.fetch_add(1, Ordering::Relaxed);
        let marker = format!(
            "{}-{nonce}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let item = NSPasteboardItem::new();
        let text = NSString::from_str(text);
        let marker_type = NSString::from_str(MARKER_TYPE);
        let marker_value = NSString::from_str(&marker);
        if !item.setString_forType(&text, unsafe { NSPasteboardTypeString })
            || !item.setString_forType(&marker_value, &marker_type)
        {
            return Err(VocalCodeError::Inject(
                "could not prepare temporary clipboard item".into(),
            ));
        }
        let writers = NSArray::from_retained_slice(&[
            ProtocolObject::<dyn NSPasteboardWriting>::from_retained(item),
        ]);
        let clear_generation = pasteboard.clearContents();
        if !pasteboard.writeObjects(&writers) {
            if pasteboard.changeCount() == clear_generation {
                self.restore(pasteboard)?;
            }
            return Err(VocalCodeError::Inject(
                "could not write temporary clipboard item".into(),
            ));
        }
        let receipt = MacClipboardReceipt {
            generation: pasteboard.changeCount(),
            marker,
        };
        if !receipt.is_owned(pasteboard) {
            return Err(VocalCodeError::Inject(
                "clipboard changed while preparing paste; text was not inserted".into(),
            ));
        }
        Ok(receipt)
    }
}

#[cfg(target_os = "macos")]
struct MacClipboardReceipt {
    generation: isize,
    marker: String,
}

#[cfg(target_os = "macos")]
impl MacClipboardReceipt {
    fn marker_matches(&self, pasteboard: &objc2_app_kit::NSPasteboard) -> bool {
        use objc2_foundation::NSString;

        const MARKER_TYPE: &str = "app.vocalcode.temporary-clipboard-transaction";
        let marker_type = NSString::from_str(MARKER_TYPE);
        pasteboard
            .stringForType(&marker_type)
            .is_some_and(|marker| {
                // Another process can write our public pasteboard type. Check
                // the encoded length before allocating its untrusted string.
                marker.lengthOfBytesUsingEncoding(objc2_foundation::NSUTF8StringEncoding)
                    == self.marker.len()
                    && marker.to_string() == self.marker
            })
    }

    fn is_owned(&self, pasteboard: &objc2_app_kit::NSPasteboard) -> bool {
        let before = pasteboard.changeCount();
        let marker_matches = self.marker_matches(pasteboard);
        let after = pasteboard.changeCount();
        clipboard_generation_is_owned(self.generation, before, after, marker_matches)
    }
}

#[cfg(target_os = "macos")]
fn send_macos_command_chord(virtual_key: u16, label: &str) -> Result<()> {
    use core_foundation::base::CFRelease;
    use std::ffi::c_void;

    type CgEventRef = *mut c_void;
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn CGEventCreateKeyboardEvent(
            source: *const c_void,
            virtual_key: u16,
            key_down: bool,
        ) -> CgEventRef;
        fn CGEventSetFlags(event: CgEventRef, flags: u64);
        fn CGEventPost(tap: u32, event: CgEventRef);
    }

    const CG_HID_EVENT_TAP: u32 = 0;
    const CG_EVENT_FLAG_MASK_COMMAND: u64 = 1 << 20;
    let down = unsafe { CGEventCreateKeyboardEvent(std::ptr::null(), virtual_key, true) };
    let up = unsafe { CGEventCreateKeyboardEvent(std::ptr::null(), virtual_key, false) };
    if down.is_null() || up.is_null() {
        if !down.is_null() {
            unsafe { CFRelease(down.cast()) };
        }
        if !up.is_null() {
            unsafe { CFRelease(up.cast()) };
        }
        return Err(VocalCodeError::Inject(format!(
            "could not create native macOS {label} events"
        )));
    }
    unsafe {
        CGEventSetFlags(down, CG_EVENT_FLAG_MASK_COMMAND);
        CGEventSetFlags(up, CG_EVENT_FLAG_MASK_COMMAND);
        CGEventPost(CG_HID_EVENT_TAP, down);
        CGEventPost(CG_HID_EVENT_TAP, up);
        CFRelease(down.cast());
        CFRelease(up.cast());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn send_macos_paste_chord() -> Result<()> {
    send_macos_command_chord(9, "paste")
}

#[cfg(target_os = "macos")]
fn send_macos_copy_chord() -> Result<()> {
    send_macos_command_chord(8, "copy")
}

#[derive(Default)]
pub struct EnigoInjector {
    /// Insert via the clipboard rather than synthesising each character.
    /// Defaults to false — see the module comment for why typing wins.
    paste: bool,
    expected_focus: Mutex<Option<FocusToken>>,
}

impl EnigoInjector {
    pub fn new(paste: bool) -> Self {
        Self {
            paste,
            expected_focus: Mutex::new(None),
        }
    }

    fn enigo() -> Result<Enigo> {
        Enigo::new(&Settings::default())
            .map_err(|e| VocalCodeError::Inject(format!("init input synth: {e}")))
    }

    fn expected_focus(&self) -> Result<Option<FocusToken>> {
        self.expected_focus
            .lock()
            .map(|focus| focus.as_ref().cloned())
            .map_err(|_| VocalCodeError::Inject("focus target lock poisoned".into()))
    }

    fn ensure_focus(&self) -> Result<()> {
        let expected = self.expected_focus()?;
        validate_focus(expected, current_focus())
    }

    fn arm_focus(&self, target: FocusToken) -> Result<()> {
        let mut focus = self
            .expected_focus
            .lock()
            .map_err(|_| VocalCodeError::Inject("focus target lock poisoned".into()))?;
        *focus = Some(target);
        Ok(())
    }

    /// Decide where a finished transcript goes.
    ///
    /// Delivery follows the safe editable control focused when the words are
    /// ready. The selected control is then pinned exactly for the duration of
    /// insertion, so a subsequent focus move cannot receive trailing chunks.
    fn delivery(&self) -> Result<Delivery> {
        let recording_start = self.expected_focus()?;
        Ok(
            select_current_delivery_target(recording_start.as_ref(), focused_text_target())
                .map_or(Delivery::Clipboard, Delivery::Type),
        )
    }

    /// Insert by clipboard: stash what was there, paste, then put it back.
    ///
    /// The restore matters more than it looks — dictating would otherwise
    /// silently destroy whatever the user had copied, and they would only find
    /// out at the next paste.
    #[cfg(target_os = "macos")]
    fn paste_text(&self, text: &str) -> Result<()> {
        let _clipboard_transaction = lock_clipboard_transaction();
        self.ensure_focus()?;
        let pasteboard = objc2_app_kit::NSPasteboard::generalPasteboard();
        // Deep-copy every advertised item/type before the first mutation. If a
        // delayed provider cannot render one format, fail closed while the
        // user's clipboard is still untouched.
        let snapshot = MacClipboardSnapshot::capture(&pasteboard)?;
        let receipt = snapshot.replace_with_temporary_text(&pasteboard, text)?;

        let action = (|| {
            // CFEqual compares the exact AX control retained at recording start,
            // catching field A -> field B switches within one Electron/browser
            // process. Clipboard ownership is then rechecked immediately before
            // the native Command+V event so a newer user copy is never pasted.
            self.ensure_focus()?;
            if !receipt.is_owned(&pasteboard) {
                return Err(VocalCodeError::Inject(
                    "clipboard changed before paste; text was not inserted".into(),
                ));
            }
            send_macos_paste_chord()?;
            std::thread::sleep(std::time::Duration::from_millis(180));
            Ok(())
        })();

        let restored = if receipt.is_owned(&pasteboard) {
            snapshot.restore(&pasteboard)
        } else {
            log::info!("clipboard changed during paste; preserving the newer value");
            Ok(())
        };
        match (action, restored) {
            (Err(action_error), Err(restore_error)) => Err(VocalCodeError::Inject(format!(
                "{action_error}; clipboard recovery also failed: {restore_error}"
            ))),
            (Ok(()), Err(restore_error)) => Err(restore_error),
            (Err(action_error), Ok(())) => Err(action_error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    /// Windows uses a real clipboard paste. The OLE guard retains the complete
    /// IDataObject, not just its text rendering, so HTML/RTF/images/file drops
    /// return after the target has consumed Ctrl+V.
    #[cfg(windows)]
    fn paste_text(&self, text: &str) -> Result<()> {
        let _clipboard_transaction = lock_clipboard_transaction();
        self.ensure_focus()?;
        let mut restore = WinClipboardRestore::capture()?;
        let action = (|| {
            let receipt = write_clipboard_text_unlocked(text)?;
            restore.expect_receipt(receipt.clone());
            let mut enigo = Self::enigo()?;
            self.ensure_focus()?;
            if !receipt.is_owned() {
                return Err(VocalCodeError::Inject(
                    "clipboard changed before paste; text was not inserted".into(),
                ));
            }
            send_chord(&mut enigo, Key::Control, 'v', "paste")?;
            // Clipboard reads are asynchronous in many target applications.
            std::thread::sleep(std::time::Duration::from_millis(180));
            Ok(())
        })();
        let restored = restore.restore();
        match (action, restored) {
            (Err(action_error), Err(restore_error)) => Err(VocalCodeError::Inject(format!(
                "{action_error}; clipboard recovery also failed: {restore_error}"
            ))),
            (Ok(()), Err(restore_error)) => Err(restore_error),
            (Err(action_error), Ok(())) => Err(action_error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    fn paste_text(&self, text: &str) -> Result<()> {
        self.type_text(text)
    }

    /// Synthesised insertion. On macOS this reaches
    /// `CGEventKeyboardSetUnicodeString`, so the string arrives whole rather
    /// than a keystroke at a time.
    fn type_text(&self, text: &str) -> Result<()> {
        self.ensure_focus()?;
        let mut enigo = Self::enigo()?;
        // Initialising the native backend may block on OS services. Revalidate
        // after it is ready so that delay cannot widen the wrong-focus race.
        self.ensure_focus()?;
        // Enigo/CGEvent splits long Unicode strings into 20-character events.
        // Own those chunks here so focus is revalidated between every event;
        // otherwise a control switch during a long insertion can send all
        // remaining chunks to the newly focused password field/chat/terminal.
        for chunk in unicode_chunks(text, 20) {
            self.ensure_focus()?;
            enigo
                .text(chunk)
                .map_err(|e| VocalCodeError::Inject(format!("type text: {e}")))?;
        }
        Ok(())
    }
}

fn unicode_chunks(text: &str, max_chars: usize) -> Vec<&str> {
    if text.is_empty() || max_chars == 0 {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut chars = 0;
    for (index, _) in text.char_indices() {
        if chars == max_chars {
            chunks.push(&text[start..index]);
            start = index;
            chars = 0;
        }
        chars += 1;
    }
    if start < text.len() {
        chunks.push(&text[start..]);
    }
    chunks
}

#[cfg(target_os = "macos")]
pub fn write_clipboard_text(text: &str) -> Result<()> {
    let _clipboard_transaction = lock_clipboard_transaction();
    write_clipboard_text_unlocked(text)
}

#[cfg(target_os = "macos")]
fn write_clipboard_text_unlocked(text: &str) -> Result<()> {
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSString};

    let pasteboard = NSPasteboard::generalPasteboard();
    let value = ProtocolObject::<dyn NSPasteboardWriting>::from_retained(NSString::from_str(text));
    let writers = NSArray::from_retained_slice(&[value]);
    pasteboard.clearContents();
    if pasteboard.writeObjects(&writers) {
        // Read the canonical string back before reporting success. This keeps
        // non-ASCII History copies on the same native UTF-16/NSString path and
        // catches a denied/failed pasteboard write without a helper process.
        if pasteboard
            .stringForType(unsafe { NSPasteboardTypeString })
            .is_some_and(|written| written.to_string() == text)
        {
            Ok(())
        } else {
            Err(VocalCodeError::Inject(
                "clipboard did not retain the copied text".into(),
            ))
        }
    } else {
        Err(VocalCodeError::Inject(
            "clipboard rejected the copied text".into(),
        ))
    }
}

impl TextInjector for EnigoInjector {
    fn begin_utterance(&self) -> Result<()> {
        // Remember the starting observation, but do not reserve it as the
        // destination. Delivery resolves the safe editable target when words
        // are ready, then pins that exact target during insertion.
        let captured = current_focus();
        let mut focus = self
            .expected_focus
            .lock()
            .map_err(|_| VocalCodeError::Inject("focus target lock poisoned".into()))?;
        *focus = captured;
        Ok(())
    }

    fn end_utterance(&self) {
        if let Ok(mut focus) = self.expected_focus.lock() {
            *focus = None;
        }
    }

    fn inject_text(&self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        match self.delivery()? {
            Delivery::Type(target) => {
                // Re-arm to the control actually about to be typed into. The
                // checks inside the typing and pasting paths then go on doing
                // the job they were written for — catching focus moving *during*
                // one insertion, which would spray the rest of a sentence into
                // whatever arrived — instead of re-litigating the move that
                // already happened and was already accepted here.
                self.arm_focus(target)?;
                if self.paste {
                    self.paste_text(text)
                } else {
                    self.type_text(text)
                }
            }
            Delivery::Clipboard => {
                write_clipboard_text(text)?;
                Err(VocalCodeError::Diverted(no_text_target_message()))
            }
        }
    }

    fn send_enter(&self) -> Result<()> {
        let mut enigo = Self::enigo()?;
        enigo
            .key(Key::Return, Direction::Click)
            .map_err(|e| VocalCodeError::Inject(format!("send enter: {e}")))
    }

    fn backspace(&self, n: usize) -> Result<()> {
        if n == 0 {
            return Ok(());
        }
        self.ensure_focus()?;
        let mut enigo = Self::enigo()?;
        for _ in 0..n {
            enigo
                .key(Key::Backspace, Direction::Click)
                .map_err(|e| VocalCodeError::Inject(format!("backspace: {e}")))?;
        }
        Ok(())
    }
}

#[cfg(windows)]
struct OpenClipboardGuard;

#[cfg(windows)]
impl OpenClipboardGuard {
    fn try_acquire() -> windows::core::Result<Self> {
        unsafe { windows::Win32::System::DataExchange::OpenClipboard(None)? };
        Ok(Self)
    }

    fn acquire() -> Result<Self> {
        for _ in 0..20 {
            if let Ok(clipboard) = Self::try_acquire() {
                return Ok(clipboard);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Err(VocalCodeError::Inject(
            "clipboard is busy (could not open it)".into(),
        ))
    }
}

#[cfg(windows)]
impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::System::DataExchange::CloseClipboard() };
    }
}

#[cfg(windows)]
#[derive(Clone)]
struct WinClipboardReceipt {
    marker_format: u32,
    marker: u64,
    expected_text: String,
    post_close_sequence: u32,
}

#[cfg(windows)]
impl WinClipboardReceipt {
    fn claim(marker_format: u32, marker: u64, expected_text: &str) -> Result<Self> {
        let _clipboard = OpenClipboardGuard::acquire()?;
        let marker_matches = read_clipboard_marker_open(marker_format) == Some(marker);
        let text_matches = clipboard_text_matches_open(expected_text);
        let post_close_sequence =
            unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() };
        if !marker_matches || !text_matches {
            return Err(VocalCodeError::Inject(
                "clipboard changed while preparing paste; text was not inserted".into(),
            ));
        }
        Ok(Self {
            marker_format,
            marker,
            expected_text: expected_text.to_owned(),
            post_close_sequence,
        })
    }

    fn is_owned(&self) -> bool {
        let Ok(_clipboard) = OpenClipboardGuard::acquire() else {
            return false;
        };
        let marker_matches = read_clipboard_marker_open(self.marker_format) == Some(self.marker);
        let text_matches = clipboard_text_matches_open(&self.expected_text);
        let current_sequence =
            unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() };
        if current_sequence != self.post_close_sequence && marker_matches && text_matches {
            // Clipboard managers may synthesize extra formats after CloseClipboard,
            // advancing the sequence without replacing our private ownership
            // marker or transcript. The marker+text pair is authoritative.
            log::debug!(
                "clipboard sequence advanced from {} to {} while VocalCode still owns its marker",
                self.post_close_sequence,
                current_sequence
            );
        }
        marker_matches && text_matches
    }

    fn observe(&self) -> Result<WinClipboardObservation> {
        let _clipboard = OpenClipboardGuard::acquire()?;
        Ok(WinClipboardObservation {
            text: read_dictionary_clipboard_text_open()?,
            marker_matches: read_clipboard_marker_open(self.marker_format) == Some(self.marker),
            sequence: unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() },
        })
    }
}

#[cfg(windows)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct WinClipboardObservation {
    text: Option<String>,
    marker_matches: bool,
    sequence: u32,
}

#[cfg(windows)]
fn vocalcode_marker_format() -> Result<u32> {
    use windows::core::PCWSTR;
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;

    let name: Vec<u16> = "VocalCode.TemporaryClipboardTransaction.v1"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let format = unsafe { RegisterClipboardFormatW(PCWSTR(name.as_ptr())) };
    if format == 0 {
        Err(VocalCodeError::Inject(
            "could not register clipboard ownership marker".into(),
        ))
    } else {
        Ok(format)
    }
}

#[cfg(windows)]
fn read_clipboard_marker_open(format: u32) -> Option<u64> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::GetClipboardData;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    let handle = unsafe { GetClipboardData(format) }.ok()?;
    let memory = HGLOBAL(handle.0);
    if unsafe { GlobalSize(memory) } != std::mem::size_of::<u64>() {
        return None;
    }
    let pointer = unsafe { GlobalLock(memory) } as *const u64;
    if pointer.is_null() {
        return None;
    }
    let marker = unsafe { pointer.read_unaligned() };
    let _ = unsafe { GlobalUnlock(memory) };
    Some(marker)
}

#[cfg(windows)]
fn clipboard_text_matches_open(expected: &str) -> bool {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::GetClipboardData;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    let Ok(handle) = (unsafe { GetClipboardData(CF_UNICODETEXT.0 as u32) }) else {
        return false;
    };
    let memory = HGLOBAL(handle.0);
    let byte_len = unsafe { GlobalSize(memory) };
    if byte_len % std::mem::size_of::<u16>() != 0 {
        return false;
    }
    let Some(required_units) = expected.encode_utf16().count().checked_add(1) else {
        return false;
    };
    let available_units = byte_len / std::mem::size_of::<u16>();
    if available_units < required_units {
        return false;
    }
    let pointer = unsafe { GlobalLock(memory) } as *const u16;
    if pointer.is_null() {
        return false;
    }
    // Only expose the exact prefix needed for the comparison. A replacement
    // clipboard may advertise a multi-gigabyte HGLOBAL; its size must never
    // become a Rust slice length or allocation merely to prove non-ownership.
    let slice = unsafe { std::slice::from_raw_parts(pointer, required_units) };
    let matches = slice[required_units - 1] == 0
        && slice[..required_units - 1]
            .iter()
            .copied()
            .eq(expected.encode_utf16());
    let _ = unsafe { GlobalUnlock(memory) };
    matches
}

#[cfg(windows)]
fn read_dictionary_clipboard_text_open() -> Result<Option<String>> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::GetClipboardData;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    let Ok(handle) = (unsafe { GetClipboardData(CF_UNICODETEXT.0 as u32) }) else {
        return Ok(None);
    };
    let memory = HGLOBAL(handle.0);
    let byte_len = unsafe { GlobalSize(memory) };
    if byte_len % std::mem::size_of::<u16>() != 0 {
        return Err(VocalCodeError::Inject(
            "the selected text has an invalid clipboard representation".into(),
        ));
    }
    let units = byte_len / std::mem::size_of::<u16>();
    // Every UTF-16 code unit contributes at least one UTF-8 byte (surrogate
    // pairs contribute four across two units). Include one unit for the NUL.
    if units > MAX_DICTIONARY_SIDE_UTF8_BYTES.saturating_add(1) {
        return Err(VocalCodeError::Inject(
            "the selected text is too large for a VocalCode dictionary entry".into(),
        ));
    }
    if units == 0 {
        return Ok(Some(String::new()));
    }
    let pointer = unsafe { GlobalLock(memory) } as *const u16;
    if pointer.is_null() {
        return Ok(None);
    }
    let slice = unsafe { std::slice::from_raw_parts(pointer, units) };
    let end = slice.iter().position(|&unit| unit == 0).unwrap_or(units);
    let text = String::from_utf16_lossy(&slice[..end]);
    let _ = unsafe { GlobalUnlock(memory) };
    if text.len() > MAX_DICTIONARY_SIDE_UTF8_BYTES {
        return Err(VocalCodeError::Inject(
            "the selected text is too large for a VocalCode dictionary entry".into(),
        ));
    }
    Ok(Some(text))
}

/// Set Unicode text without PowerShell or a console code page. Ownership of the
/// allocated block transfers to Windows only after `SetClipboardData` succeeds.
#[cfg(windows)]
pub fn write_clipboard_text(text: &str) -> Result<()> {
    let _clipboard_transaction = lock_clipboard_transaction();
    write_clipboard_text_unlocked(text).map(|_| ())
}

#[cfg(windows)]
fn write_clipboard_text_unlocked(text: &str) -> Result<WinClipboardReceipt> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::{EmptyClipboard, SetClipboardData};
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    static NEXT_MARKER: AtomicU64 = AtomicU64::new(1);

    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * std::mem::size_of::<u16>();
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }
        .map_err(|e| VocalCodeError::Inject(format!("clipboard allocate: {e}")))?;
    let pointer = unsafe { GlobalLock(memory) } as *mut u16;
    if pointer.is_null() {
        let _ = unsafe { GlobalFree(Some(memory)) };
        return Err(VocalCodeError::Inject(
            "clipboard memory lock failed".into(),
        ));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(wide.as_ptr(), pointer, wide.len());
        let _ = GlobalUnlock(memory);
    }

    let marker_format = match vocalcode_marker_format() {
        Ok(format) => format,
        Err(error) => {
            let _ = unsafe { GlobalFree(Some(memory)) };
            return Err(error);
        }
    };
    let marker = NEXT_MARKER.fetch_add(1, Ordering::Relaxed)
        ^ ((std::process::id() as u64) << 32)
        ^ std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
    let marker_memory = match unsafe { GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of::<u64>()) } {
        Ok(memory) => memory,
        Err(error) => {
            let _ = unsafe { GlobalFree(Some(memory)) };
            return Err(VocalCodeError::Inject(format!(
                "clipboard marker allocate: {error}"
            )));
        }
    };
    let marker_pointer = unsafe { GlobalLock(marker_memory) } as *mut u64;
    if marker_pointer.is_null() {
        let _ = unsafe { GlobalFree(Some(memory)) };
        let _ = unsafe { GlobalFree(Some(marker_memory)) };
        return Err(VocalCodeError::Inject(
            "clipboard marker memory lock failed".into(),
        ));
    }
    unsafe {
        marker_pointer.write_unaligned(marker);
        let _ = GlobalUnlock(marker_memory);
    }

    let _clipboard = match OpenClipboardGuard::acquire() {
        Ok(clipboard) => clipboard,
        Err(error) => {
            let _ = unsafe { GlobalFree(Some(memory)) };
            let _ = unsafe { GlobalFree(Some(marker_memory)) };
            return Err(error);
        }
    };
    if let Err(error) = unsafe { EmptyClipboard() } {
        let _ = unsafe { GlobalFree(Some(memory)) };
        let _ = unsafe { GlobalFree(Some(marker_memory)) };
        return Err(VocalCodeError::Inject(format!("clipboard clear: {error}")));
    }
    if let Err(error) = unsafe { SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(memory.0))) }
    {
        let _ = unsafe { GlobalFree(Some(memory)) };
        return Err(VocalCodeError::Inject(format!(
            "clipboard set text: {error}"
        )));
    }
    if let Err(error) = unsafe { SetClipboardData(marker_format, Some(HANDLE(marker_memory.0))) } {
        let _ = unsafe { GlobalFree(Some(marker_memory)) };
        return Err(VocalCodeError::Inject(format!(
            "clipboard set ownership marker: {error}"
        )));
    }
    // Sequence numbers can advance as Windows/clipboard managers synthesize
    // formats only after CloseClipboard. Close first, then reacquire and claim
    // the exact marker+text while the clipboard is locked.
    drop(_clipboard);
    WinClipboardReceipt::claim(marker_format, marker, text)
}

#[cfg(all(windows, test))]
fn read_clipboard_text_with_sequence() -> Option<(String, u32)> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::GetClipboardData;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    let _clipboard = OpenClipboardGuard::acquire().ok()?;
    let handle = unsafe { GetClipboardData(CF_UNICODETEXT.0 as u32) }.ok()?;
    let memory = HGLOBAL(handle.0);
    let units = unsafe { GlobalSize(memory) } / std::mem::size_of::<u16>();
    if units == 0 {
        let sequence =
            unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() };
        return Some((String::new(), sequence));
    }
    let pointer = unsafe { GlobalLock(memory) } as *const u16;
    if pointer.is_null() {
        return None;
    }
    let slice = unsafe { std::slice::from_raw_parts(pointer, units) };
    let end = slice.iter().position(|&unit| unit == 0).unwrap_or(units);
    let text = String::from_utf16_lossy(&slice[..end]);
    let _ = unsafe { GlobalUnlock(memory) };
    let sequence = unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() };
    Some((text, sequence))
}

#[cfg(all(windows, test))]
fn read_clipboard_text() -> Option<String> {
    read_clipboard_text_with_sequence().map(|(text, _)| text)
}

/// Retains the source application's complete IDataObject. Restoring this COM
/// object preserves every clipboard format it advertised, including delayed
/// HTML/RTF, DIB images and file-drop lists; a text-only snapshot cannot do so.
#[cfg(windows)]
struct WinClipboardRestore {
    /// `None` is a real snapshot: the clipboard was empty. Some Windows builds
    /// return an OLE error for that state, so empty cannot mean that the whole
    /// transaction failed.
    original: Option<windows::Win32::System::Com::IDataObject>,
    /// When set, preserve a newer clipboard written by the user/target instead
    /// of blindly restoring our older snapshot over it.
    restore_sequence: Option<u32>,
    /// Strong ownership proof for a VocalCode temporary paste. A private
    /// format+nonce survives synthesized format changes but not a real copy.
    restore_receipt: Option<WinClipboardReceipt>,
    ole_initialized: bool,
}

#[cfg(windows)]
fn clipboard_is_empty() -> Result<bool> {
    use windows::Win32::System::DataExchange::CountClipboardFormats;

    let _clipboard = OpenClipboardGuard::acquire()?;
    Ok(unsafe { CountClipboardFormats() } == 0)
}

#[cfg(windows)]
fn clear_clipboard_once() -> windows::core::Result<()> {
    use windows::Win32::System::DataExchange::EmptyClipboard;

    let _clipboard = OpenClipboardGuard::try_acquire()?;
    unsafe { EmptyClipboard() }
}

#[cfg(windows)]
const CLIPBOARD_RESTORE_ATTEMPTS: usize = 20;

#[cfg(windows)]
fn retry_clipboard_restore<E>(
    attempts: usize,
    mut operation: impl FnMut() -> std::result::Result<(), E>,
    mut between_attempts: impl FnMut(),
) -> std::result::Result<(), E> {
    debug_assert!(attempts > 0);
    for attempt in 0..attempts {
        match operation() {
            Ok(()) => return Ok(()),
            Err(error) if attempt + 1 == attempts => return Err(error),
            Err(_) => between_attempts(),
        }
    }
    unreachable!("positive retry count always returns from the loop")
}

/// Deliver whatever COM marshaling messages are queued on this thread, without
/// blocking. `OleInitialize` makes the calling thread an STA, and an STA that
/// held a cross-process `IDataObject` (a clipboard manager owning the clipboard
/// with delayed rendering) has RPC messages pending; tearing the apartment down
/// with `OleUninitialize` while they sit undelivered is how
/// `STATUS_FATAL_USER_CALLBACK_EXCEPTION` escapes a callback — the parallel
/// `cargo test` crash on the second Windows box, and the same latent risk on
/// any user machine running a clipboard manager.
#[cfg(windows)]
fn drain_ole_messages() {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(windows)]
impl WinClipboardRestore {
    fn capture() -> Result<Self> {
        use windows::Win32::System::Ole::{OleGetClipboard, OleInitialize};
        unsafe { OleInitialize(None) }
            .map_err(|e| VocalCodeError::Inject(format!("clipboard OLE init: {e}")))?;
        let empty = match clipboard_is_empty() {
            Ok(empty) => empty,
            Err(error) => {
                drain_ole_messages();
                unsafe { windows::Win32::System::Ole::OleUninitialize() };
                return Err(error);
            }
        };
        let original = if empty {
            // `OleGetClipboard` may synthesize a nominal data object even when
            // the native clipboard has zero formats. Remember the empty state
            // explicitly so restoration does not introduce OLE-owned formats.
            None
        } else {
            match unsafe { OleGetClipboard() } {
                Ok(original) => Some(original),
                Err(snapshot_error) => {
                    drain_ole_messages();
                    unsafe { windows::Win32::System::Ole::OleUninitialize() };
                    return Err(VocalCodeError::Inject(format!(
                        "clipboard snapshot: {snapshot_error}"
                    )));
                }
            }
        };
        Ok(Self {
            original,
            restore_sequence: None,
            restore_receipt: None,
            ole_initialized: true,
        })
    }

    fn expect_sequence(&mut self, sequence: u32) {
        self.restore_sequence = Some(sequence);
    }

    fn expect_receipt(&mut self, receipt: WinClipboardReceipt) {
        self.restore_receipt = Some(receipt);
        self.restore_sequence = None;
    }

    fn finish_apartment(&mut self) {
        if !self.ole_initialized {
            return;
        }
        // Release the cross-process object while its COM apartment is still
        // alive. IDataObject's Rust field destructor used to run only after
        // Drop returned — after OleUninitialize — which can surface fatal RPC
        // callback exceptions with clipboard managers.
        drain_ole_messages();
        drop(self.original.take());
        drain_ole_messages();
        unsafe { windows::Win32::System::Ole::OleUninitialize() };
        self.ole_initialized = false;
    }

    fn restore(&mut self) -> Result<()> {
        use windows::Win32::System::Ole::{OleFlushClipboard, OleSetClipboard};

        if !self.ole_initialized {
            return Ok(());
        }
        let receipt_was_replaced = self
            .restore_receipt
            .as_ref()
            .is_some_and(|receipt| !receipt.is_owned());
        let sequence_was_replaced = self.restore_receipt.is_none()
            && self.restore_sequence.is_some_and(|expected| {
                (unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() })
                    != expected
            });
        if receipt_was_replaced || sequence_was_replaced {
            log::info!("clipboard changed during paste; preserving the newer value");
            self.finish_apartment();
            return Ok(());
        }

        let original = self.original.as_ref().cloned();
        let restored = if let Some(original) = original.as_ref() {
            retry_clipboard_restore(
                CLIPBOARD_RESTORE_ATTEMPTS,
                || unsafe {
                    OleSetClipboard(original)?;
                    OleFlushClipboard()
                },
                || {
                    drain_ole_messages();
                    std::thread::sleep(std::time::Duration::from_millis(10));
                },
            )
        } else {
            retry_clipboard_restore(CLIPBOARD_RESTORE_ATTEMPTS, clear_clipboard_once, || {
                drain_ole_messages();
                std::thread::sleep(std::time::Duration::from_millis(10));
            })
        };
        drop(original);
        self.finish_apartment();
        restored.map_err(|error| {
            VocalCodeError::Inject(format!(
                "clipboard restore failed after {CLIPBOARD_RESTORE_ATTEMPTS} attempts: {error}; the transcript remains available in VocalCode History"
            ))
        })
    }
}

#[cfg(windows)]
impl Drop for WinClipboardRestore {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            // Drop covers early-return/panic paths where the caller cannot
            // receive a second error. Normal paste calls restore explicitly and
            // propagate the failure through Engine into History recovery.
            log::error!("{error}");
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn write_clipboard_text(_text: &str) -> Result<()> {
    let _clipboard_transaction = lock_clipboard_transaction();
    Err(VocalCodeError::Inject(
        "clipboard text is not implemented on this platform".into(),
    ))
}

/// Always attempts the modifier release, even if the key click fails. Returning
/// early between those calls used to leave Ctrl/Command logically held for the
/// rest of the desktop session.
#[cfg(windows)]
fn send_chord(enigo: &mut Enigo, modifier: Key, character: char, label: &str) -> Result<()> {
    if let Err(error) = enigo.key(modifier, Direction::Press) {
        // Even a failed synthesizer call can have partially reached the OS.
        let _ = enigo.key(modifier, Direction::Release);
        return Err(VocalCodeError::Inject(format!(
            "{label} modifier press: {error}"
        )));
    }
    let clicked = enigo.key(Key::Unicode(character), Direction::Click);
    let released = enigo.key(modifier, Direction::Release);
    clicked.map_err(|e| VocalCodeError::Inject(format!("{label} key: {e}")))?;
    released.map_err(|e| VocalCodeError::Inject(format!("{label} modifier release: {e}")))?;
    Ok(())
}

/// Copy whatever is selected in the frontmost app, and put the clipboard back.
///
/// There is no API for "read the other app's selection" — the only way in is to
/// ask the app to copy it, which means synthesising ⌘C and reading what lands.
/// So the clipboard is borrowed and returned, exactly as the paste-insert path
/// does, because losing what somebody had copied is not an acceptable price for
/// a convenience feature.
///
/// Returns `None` when the clipboard did not change, which is what "nothing was
/// selected" looks like from here. That also swallows the case where the
/// selection happens to be identical to the clipboard already — a miss, and the
/// right way to be wrong: doing nothing is recoverable by pressing again, while
/// teaching a stale clipboard would write a rule the user never asked for.
#[cfg(target_os = "macos")]
fn dictionary_string_from_ns(value: &objc2_foundation::NSString) -> Result<String> {
    let byte_len = value.lengthOfBytesUsingEncoding(objc2_foundation::NSUTF8StringEncoding);
    if byte_len > MAX_DICTIONARY_SIDE_UTF8_BYTES {
        return Err(VocalCodeError::Inject(
            "the selected text is too large for a VocalCode dictionary entry".into(),
        ));
    }
    let text = value.to_string();
    if text.len() > MAX_DICTIONARY_SIDE_UTF8_BYTES {
        return Err(VocalCodeError::Inject(
            "the selected text is too large for a VocalCode dictionary entry".into(),
        ));
    }
    Ok(text)
}

#[cfg(target_os = "macos")]
pub fn copy_selection() -> Result<Option<String>> {
    let _clipboard_transaction = lock_clipboard_transaction();
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};

    let expected_focus = current_focus().ok_or_else(|| {
        VocalCodeError::Inject("could not identify the focused selection target".into())
    })?;
    let pasteboard = NSPasteboard::generalPasteboard();
    let snapshot = MacClipboardSnapshot::capture(&pasteboard)?;
    let sentinel = format!(
        "vocalcode-no-selection-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let receipt = snapshot.replace_with_temporary_text(&pasteboard, &sentinel)?;

    let copied = (|| -> Result<Option<(isize, Option<String>)>> {
        validate_focus(Some(expected_focus.clone()), current_focus())?;
        if !receipt.is_owned(&pasteboard) {
            return Err(VocalCodeError::Inject(
                "clipboard changed before selection copy".into(),
            ));
        }
        send_macos_copy_chord()?;

        // Copying is asynchronous in many applications. Require the exact
        // focused element on both sides of each observation, one stable
        // changeCount around the read, and the same candidate twice. A single
        // foreign clipboard write must never be mistaken for the selection.
        let mut candidate: Option<(isize, Option<String>)> = None;
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            validate_focus(Some(expected_focus.clone()), current_focus())?;
            let before = pasteboard.changeCount();
            let text = match pasteboard.stringForType(unsafe { NSPasteboardTypeString }) {
                Some(value) => Some(dictionary_string_from_ns(&value)?),
                None => None,
            };
            let marker_matches = receipt.marker_matches(&pasteboard);
            let after = pasteboard.changeCount();
            validate_focus(Some(expected_focus.clone()), current_focus())?;
            if before != after {
                candidate = None;
                continue;
            }
            if marker_matches {
                if text.as_deref() != Some(sentinel.as_str()) {
                    return Err(VocalCodeError::Inject(
                        "clipboard marker and temporary text no longer agree".into(),
                    ));
                }
                candidate = None;
                continue;
            }
            if text.as_deref() == Some(sentinel.as_str()) {
                return Err(VocalCodeError::Inject(
                    "clipboard ownership changed during selection copy".into(),
                ));
            }
            let observed = (after, text);
            if candidate.as_ref() == Some(&observed) {
                validate_focus(Some(expected_focus.clone()), current_focus())?;
                return Ok(Some(observed));
            }
            candidate = Some(observed);
        }
        Ok(None)
    })();

    match copied {
        Ok(Some((generation, copied))) => {
            // A focus switch after the stable result is still ambiguous: keep
            // the new clipboard as the safest recovery value and teach nothing.
            validate_focus(Some(expected_focus), current_focus())?;
            if !snapshot.restore_if_generation(&pasteboard, generation)? {
                log::info!("clipboard changed after selection copy; preserving the newer value");
            }
            Ok(copied.and_then(|text| {
                let text = text.trim();
                (!text.is_empty() && text != sentinel).then(|| text.to_string())
            }))
        }
        Ok(None) => {
            if receipt.is_owned(&pasteboard) {
                snapshot.restore(&pasteboard)?;
            } else {
                log::info!("clipboard changed during selection copy; preserving the newer value");
            }
            Ok(None)
        }
        Err(action_error) => {
            let restored = if receipt.is_owned(&pasteboard) {
                snapshot.restore(&pasteboard)
            } else {
                Ok(())
            };
            match restored {
                Ok(()) => Err(action_error),
                Err(restore_error) => Err(VocalCodeError::Inject(format!(
                    "{action_error}; clipboard recovery also failed: {restore_error}"
                ))),
            }
        }
    }
}

/// Windows: borrow the complete OLE clipboard, synthesize Ctrl+C, then restore
/// it on every exit path. The old implementation wrote a sentinel before Enigo
/// initialization and returned without restoring it when initialization failed.
#[cfg(windows)]
pub fn copy_selection() -> Result<Option<String>> {
    let _clipboard_transaction = lock_clipboard_transaction();
    let expected_focus = current_focus().ok_or_else(|| {
        VocalCodeError::Inject("could not identify the focused selection target".into())
    })?;
    let mut restore = match WinClipboardRestore::capture() {
        Ok(restore) => restore,
        Err(error) => {
            return Err(error);
        }
    };
    let sentinel = format!(
        "vocalcode-no-selection-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let copied = (|| -> Result<Option<String>> {
        validate_focus(Some(expected_focus.clone()), current_focus())?;
        let receipt = write_clipboard_text_unlocked(&sentinel)?;
        restore.expect_receipt(receipt.clone());
        validate_focus(Some(expected_focus.clone()), current_focus())?;
        if !receipt.is_owned() {
            return Err(VocalCodeError::Inject(
                "clipboard changed before selection copy".into(),
            ));
        }
        let mut enigo = EnigoInjector::enigo()?;
        send_chord(&mut enigo, Key::Control, 'c', "copy selection")?;

        // The copy is the other app's work, on its own schedule. Poll instead
        // of sleeping once: a fast app answers in 50 ms and a slow one still
        // gets one second. Require the same focus around each read and the exact same
        // marker-free observation twice before attributing it to our Ctrl+C.
        let mut candidate: Option<WinClipboardObservation> = None;
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            validate_focus(Some(expected_focus.clone()), current_focus())?;
            let observed = receipt.observe()?;
            validate_focus(Some(expected_focus.clone()), current_focus())?;
            if observed.marker_matches {
                if observed.text.as_deref() != Some(sentinel.as_str()) {
                    return Err(VocalCodeError::Inject(
                        "clipboard marker and temporary text no longer agree".into(),
                    ));
                }
                candidate = None;
                continue;
            }
            if observed.text.as_deref() == Some(sentinel.as_str()) {
                return Err(VocalCodeError::Inject(
                    "clipboard ownership changed during selection copy".into(),
                ));
            }
            if candidate.as_ref() == Some(&observed) {
                validate_focus(Some(expected_focus.clone()), current_focus())?;
                restore.expect_sequence(observed.sequence);
                return Ok(observed
                    .text
                    .map(|text| text.trim().to_string())
                    .filter(|text| !text.is_empty()));
            }
            candidate = Some(observed);
        }
        if !receipt.is_owned() {
            return Err(VocalCodeError::Inject(
                "clipboard changed without a stable selection result".into(),
            ));
        }
        Ok(None)
    })();
    let restored = restore.restore();
    match (copied, restored) {
        (Err(action_error), Err(restore_error)) => Err(VocalCodeError::Inject(format!(
            "{action_error}; clipboard recovery also failed: {restore_error}"
        ))),
        (Ok(_), Err(restore_error)) => Err(restore_error),
        (Err(action_error), Ok(())) => Err(action_error),
        (Ok(copied), Ok(())) => Ok(copied),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn automatic_delivery_follows_the_current_safe_target() {
        assert_eq!(
            super::select_current_delivery_target(Some(&7), Some(7)),
            Some(7)
        );
        assert_eq!(
            super::select_current_delivery_target(Some(&7), Some(8)),
            Some(8),
            "a deliberate focus move during transcription chooses the destination"
        );
        assert_eq!(
            super::select_current_delivery_target(None, Some(8)),
            Some(8),
            "an unresolved recording-start target must not poison a valid delivery target"
        );
        assert_eq!(
            super::select_current_delivery_target::<u8>(Some(&7), None),
            None,
            "a missing, read-only, or secure current target must still be rejected"
        );
    }

    #[test]
    fn a_diverted_transcript_is_told_where_to_find_itself() {
        // The whole point of the clipboard fallback is that the user gets their
        // words back. A message that reports only the refusal ("text was not
        // inserted") leaves somebody who just dictated a paragraph believing it
        // is gone, which is the failure this path exists to prevent.
        let message = super::no_text_target_message();
        assert!(
            message.contains("clipboard"),
            "the notice must name where the words went: {message}"
        );
        let chord = if cfg!(target_os = "macos") {
            "Cmd+V"
        } else {
            "Ctrl+V"
        };
        assert!(
            message.contains(chord),
            "the notice must name this platform's paste chord: {message}"
        );
    }

    use super::*;

    #[test]
    fn correction_span_requires_one_unambiguous_insertion() {
        assert_eq!(
            unique_inserted_span("hello VocalCode", "VocalCode"),
            Some(6..15)
        );
        assert_eq!(unique_inserted_span("same same", "same"), None);
        assert_eq!(unique_inserted_span("nothing", "missing"), None);
        assert_eq!(inserted_spans("same same", "same"), vec![0..4, 5..9]);
    }

    #[test]
    fn repeated_inserted_text_can_be_localized_by_the_committed_edit() {
        let baseline = "same same";
        assert_eq!(
            correction_pairs_for_candidates(
                baseline,
                "SameX same",
                &inserted_spans(baseline, "same")
            ),
            Some(vec![("same".to_string(), "SameX".to_string())])
        );
    }

    #[test]
    fn windows_editable_value_treats_only_wrapped_accessible_placeholder_as_empty() {
        assert_eq!(
            normalize_windows_editable_value(
                "\nAsk for follow-up changes".to_string(),
                "Ask for follow-up changes"
            ),
            ""
        );
        assert_eq!(
            normalize_windows_editable_value(
                "靠迪\nAsk for follow-up changes".to_string(),
                "Ask for follow-up changes"
            ),
            "靠迪\nAsk for follow-up changes"
        );
        assert_eq!(
            normalize_windows_editable_value("Collie".to_string(), "Collie"),
            "Collie"
        );
        assert_eq!(
            normalize_windows_editable_value("\nMessage".to_string(), "Different label"),
            "\nMessage"
        );
    }

    #[test]
    fn correction_pairs_keep_only_the_changed_part_of_the_utterance() {
        let baseline = "prefix turn on voput code please suffix";
        let inserted = unique_inserted_span(baseline, "turn on voput code please").unwrap();
        assert_eq!(
            correction_pairs(baseline, "prefix turn on VocalCode please suffix", inserted),
            Some(vec![("voput code".to_string(), "VocalCode".to_string())])
        );

        let baseline = "靠迪\nAsk for follow-up changes";
        let inserted = unique_inserted_span(baseline, "靠迪").unwrap();
        assert_eq!(
            correction_pairs(baseline, "Collie\nAsk for follow-up changes", inserted),
            Some(vec![("靠迪".to_string(), "Collie".to_string())])
        );

        let baseline = "今天靠迪很好用";
        let inserted = unique_inserted_span(baseline, baseline).unwrap();
        assert_eq!(
            correction_pairs(baseline, "今天Collie很好用", inserted),
            Some(vec![("靠迪".to_string(), "Collie".to_string())])
        );
    }

    #[test]
    fn correction_pairs_split_multiple_safe_edits_and_deduplicate_repeats() {
        let baseline = "今天靠迪在用voput code连seaquel";
        let inserted = unique_inserted_span(baseline, baseline).unwrap();
        assert_eq!(
            correction_pairs(baseline, "今天Collie在用VocalCode连SQL", inserted),
            Some(vec![
                ("靠迪".to_string(), "Collie".to_string()),
                ("voput code".to_string(), "VocalCode".to_string()),
                ("seaquel".to_string(), "SQL".to_string()),
            ])
        );

        let baseline = "靠迪和靠迪";
        assert_eq!(
            correction_pairs(baseline, "Collie和Collie", 0..baseline.chars().count()),
            Some(vec![("靠迪".to_string(), "Collie".to_string())])
        );

        let baseline = "cafe noir et resume court";
        assert_eq!(
            correction_pairs(
                baseline,
                "café noir et résumé court",
                0..baseline.chars().count()
            ),
            Some(vec![
                ("cafe".to_string(), "café".to_string()),
                ("resume".to_string(), "résumé".to_string()),
            ])
        );
    }

    #[test]
    fn correction_pairs_refuse_edits_outside_the_inserted_text() {
        let baseline = "prefix dictated phrase suffix";
        let inserted = unique_inserted_span(baseline, "dictated phrase").unwrap();
        assert_eq!(
            correction_pairs(
                "prefix dictated phrase suffix",
                "PREFIX dictated phrase suffix",
                inserted
            ),
            None
        );
    }

    #[test]
    fn correction_pairs_refuse_empty_multiline_and_oversized_rules() {
        assert_eq!(correction_pairs("bad", "", 0..3), None);
        assert_eq!(correction_pairs("bad", "good\nnext", 0..3), None);
        let huge = "x".repeat(MAX_DICTIONARY_SIDE_UTF8_BYTES + 1);
        assert_eq!(
            correction_pairs(&huge, "fixed", 0..huge.chars().count()),
            None
        );

        let baseline = "a b c d e";
        assert_eq!(
            correction_pairs(baseline, "A B C D E", 0..baseline.chars().count()),
            None,
            "more than four independent rules must stay manual"
        );
        assert_eq!(correction_pairs("word", "word extra", 0..4), None);
    }

    #[test]
    fn automatic_correction_accepts_short_cross_script_names() {
        let baseline = "今天考col里很好用";
        let inserted = unique_inserted_span(baseline, baseline).unwrap();
        assert_eq!(
            correction_pairs(baseline, "今天Collie很好用", inserted),
            Some(vec![("考col里".to_string(), "Collie".to_string())])
        );
        assert!(safe_automatic_correction_pair("voput code", "VocalCode"));
        assert!(safe_automatic_correction_pair("靠迪", "Collie"));
        assert!(!safe_automatic_correction_pair("派", "pi"));
        assert!(!safe_automatic_correction_pair("pi", "派"));
    }

    #[test]
    fn automatic_correction_refuses_sentence_punctuation_and_semantic_rewrites() {
        for pair in [
            ("东西领域？", "技术领域"),
            ("wrong, phrase", "right phrase"),
            ("one two three four", "one short name"),
            ("东西领域我能做什么", "技术方向我能做哪些"),
            ("x", "an implausibly long expansion"),
        ] {
            assert!(
                !safe_automatic_correction_pair(pair.0, pair.1),
                "unexpectedly accepted {pair:?}"
            );
        }
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    #[test]
    fn focus_token_is_copyable_and_comparable() {
        let token = FocusToken(42, 7);
        assert_eq!(token, token);
    }

    // Every attribute of the system-wide Accessibility element answers
    // kAXErrorCannotComplete on some macOS builds, so `current_focus` falls
    // back to a token naming only the frontmost application. Delivery must
    // still reach that application — the whole utterance used to be discarded —
    // while a switch to another application still refuses it.
    #[cfg(target_os = "macos")]
    #[test]
    fn application_only_focus_tokens_compare_by_process() {
        let application_only = || FocusToken {
            element: None,
            process_id: 501,
        };
        let other_process = FocusToken {
            element: None,
            process_id: 502,
        };

        assert_eq!(application_only(), application_only());
        assert_ne!(application_only(), other_process);
        validate_focus(Some(application_only()), Some(application_only()))
            .expect("the same application must accept delivery");
        validate_focus(Some(application_only()), Some(other_process))
            .expect_err("another application must refuse delivery");
        validate_focus(Some(application_only()), None)
            .expect_err("an unknown target must refuse delivery");
    }

    #[cfg(windows)]
    fn test_focus_token(process_id: u32, element: i32) -> FocusToken {
        FocusToken {
            process_id,
            foreground_window: 9001,
            runtime_id: vec![42, element],
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_focus_token_compares_the_exact_uia_element() {
        let token = test_focus_token(42, 7);
        assert_eq!(token, token.clone());
        assert_ne!(token, test_focus_token(42, 8));
        let mut other_window = token.clone();
        other_window.foreground_window += 1;
        assert_ne!(token, other_window);
    }

    #[test]
    fn windows_focus_membership_accepts_a_same_process_win32_input() {
        let runtime_id = [42, 7];
        assert!(windows_focus_candidate_is_safe(&WindowsFocusCandidate {
            foreground_window: 9001,
            foreground_process_id: 100,
            element_process_id: 100,
            native_window: 9002,
            native_root_window: 9001,
            runtime_id: &runtime_id,
        }));
    }

    #[test]
    fn windows_focus_membership_accepts_a_cross_process_webview2_input() {
        let runtime_id = [42, 8];
        assert!(windows_focus_candidate_is_safe(&WindowsFocusCandidate {
            foreground_window: 9001,
            foreground_process_id: 100,
            element_process_id: 200,
            native_window: 9010,
            native_root_window: 9001,
            runtime_id: &runtime_id,
        }));
    }

    #[test]
    fn windows_focus_membership_rejects_an_element_from_another_root_window() {
        let runtime_id = [42, 9];
        assert!(!windows_focus_candidate_is_safe(&WindowsFocusCandidate {
            foreground_window: 9001,
            foreground_process_id: 100,
            element_process_id: 200,
            native_window: 9100,
            native_root_window: 9099,
            runtime_id: &runtime_id,
        }));
    }

    #[cfg(windows)]
    #[test]
    fn focus_validation_rejects_a_changed_or_missing_target() {
        let expected = Some(test_focus_token(1, 7));
        assert!(validate_focus(expected.clone(), Some(test_focus_token(1, 7))).is_ok());
        assert!(validate_focus(expected.clone(), Some(test_focus_token(2, 7))).is_err());
        assert!(validate_focus(expected.clone(), Some(test_focus_token(1, 8))).is_err());
        let mut other_window = test_focus_token(1, 7);
        other_window.foreground_window += 1;
        assert!(validate_focus(expected.clone(), Some(other_window)).is_err());
        assert!(validate_focus(expected.clone(), None).is_err());
        assert!(validate_focus(None, Some(test_focus_token(2, 7))).is_err());
        assert!(validate_focus(None, None).is_err());
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    #[test]
    fn focus_validation_rejects_a_changed_or_missing_target() {
        let expected = Some(FocusToken(1, 7));
        assert!(validate_focus(expected.clone(), Some(FocusToken(1, 7))).is_ok());
        assert!(validate_focus(expected.clone(), Some(FocusToken(2, 7))).is_err());
        assert!(validate_focus(expected.clone(), None).is_err());
        assert!(validate_focus(None, Some(FocusToken(2, 7))).is_err());
        assert!(validate_focus(None, None).is_err());
    }

    #[test]
    fn synthesized_text_chunks_preserve_unicode_boundaries_and_order() {
        let text = "中文😀abcdefghi中文😀abcdefghi中文😀";
        let chunks = unicode_chunks(text, 20);
        assert_eq!(chunks.concat(), text);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 20));
        assert_eq!(unicode_chunks(text, 0), Vec::<&str>::new());
    }

    #[test]
    fn mac_paste_contract_is_native_exact_and_generation_guarded() {
        let source = include_str!("inject.rs");
        assert!(source.contains("send_macos_paste_chord"));
        assert!(source.contains("CFEqual(self.0, other.0)"));
        assert!(source.contains("AXUIElementSetMessagingTimeout(system, 0.25)"));
        assert!(source.contains("receipt.is_owned(&pasteboard)"));
        let old_paste_script = ["MACOS_", "PASTE_SCRIPT"].concat();
        assert!(!source.contains(&old_paste_script));
    }

    #[test]
    fn clipboard_ownership_requires_one_stable_generation_and_the_marker() {
        assert!(clipboard_generation_is_owned(7, 7, 7, true));
        assert!(!clipboard_generation_is_owned(7, 8, 8, true));
        assert!(!clipboard_generation_is_owned(7, 7, 8, true));
        assert!(!clipboard_generation_is_owned(7, 7, 7, false));
    }

    #[test]
    fn mac_clipboard_snapshot_budget_rejects_count_size_and_overflow() {
        assert!(ClipboardSnapshotBudget::accepts_items(
            MAC_CLIPBOARD_SNAPSHOT_MAX_ITEMS
        ));
        assert!(!ClipboardSnapshotBudget::accepts_items(
            MAC_CLIPBOARD_SNAPSHOT_MAX_ITEMS + 1
        ));

        let mut types = ClipboardSnapshotBudget::default();
        assert!(types.add_types(MAC_CLIPBOARD_SNAPSHOT_MAX_TYPES));
        assert!(!types.add_types(1));

        let mut bytes = ClipboardSnapshotBudget::default();
        assert!(bytes.add_bytes(MAC_CLIPBOARD_SNAPSHOT_MAX_BYTES));
        assert!(!bytes.add_bytes(1));
        assert!(!ClipboardSnapshotBudget::default().add_bytes(usize::MAX));
    }

    #[test]
    fn macos_selection_copy_is_native_and_preserves_complete_items() {
        let source = include_str!("inject.rs");
        assert!(source.contains("MacClipboardSnapshot::capture(&pasteboard)"));
        assert!(source.contains("send_macos_copy_chord()?"));
        assert!(source.contains("snapshot.restore_if_generation(&pasteboard, generation)"));
        assert!(source.contains("item.dataForType(&data_type)"));
        assert!(source.contains("budget.add_bytes(data.length())"));
        assert!(source.contains("dictionary_string_from_ns(&value)?"));
        assert!(source.contains("candidate.as_ref() == Some(&observed)"));
        assert!(
            source
                .matches("validate_focus(Some(expected_focus.clone()), current_focus())")
                .count()
                >= 4
        );
        let old_script = ["MACOS_", "COPY_SELECTION_SCRIPT"].concat();
        assert!(!source.contains(&old_script));
    }

    #[test]
    fn windows_teach_claims_its_sentinel_and_revalidates_focus() {
        let source = include_str!("inject.rs");
        assert!(source.contains("restore.expect_receipt(receipt.clone())"));
        assert!(source.contains("let observed = receipt.observe()?"));
        assert!(source.contains("read_dictionary_clipboard_text_open()?"));
        assert!(source.contains("clipboard_text_matches_open(&self.expected_text)"));
        assert!(source.contains("candidate.as_ref() == Some(&observed)"));
        assert!(source.contains("validate_focus(Some(expected_focus), current_focus())?"));
        assert!(source.contains("clipboard changed without a stable selection result"));
    }

    #[cfg(windows)]
    #[test]
    fn clipboard_restore_retry_recovers_after_transient_failures() {
        let mut calls = 0;
        let mut pauses = 0;
        let result = retry_clipboard_restore(
            4,
            || {
                calls += 1;
                if calls < 3 {
                    Err("busy")
                } else {
                    Ok(())
                }
            },
            || pauses += 1,
        );
        assert_eq!(result, Ok(()));
        assert_eq!(calls, 3);
        assert_eq!(pauses, 2);
    }

    #[cfg(windows)]
    #[test]
    fn clipboard_restore_retry_stops_at_its_bound() {
        let mut calls = 0;
        let result = retry_clipboard_restore(
            3,
            || {
                calls += 1;
                Err::<(), _>("still busy")
            },
            || {},
        );
        assert_eq!(result, Err("still busy"));
        assert_eq!(calls, 3);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "mutates the real Windows clipboard; run only in an isolated interactive QA session"]
    fn unicode_clipboard_round_trip() {
        let _transaction = lock_clipboard_transaction();
        // Preserve the developer's real clipboard exactly while exercising the
        // native UTF-16 path. If OLE is unavailable in a headless test session,
        // skip without mutating anything.
        let Ok(_restore) = WinClipboardRestore::capture() else {
            return;
        };
        let value = "测试中文 · 안녕하세요 · café · 😀";
        write_clipboard_text_unlocked(value).unwrap();
        assert_eq!(read_clipboard_text().as_deref(), Some(value));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "mutates the real Windows clipboard; run only in an isolated interactive QA session"]
    fn a_newer_clipboard_value_is_not_overwritten_by_restore() {
        let _transaction = lock_clipboard_transaction();
        let Ok(_outer_restore) = WinClipboardRestore::capture() else {
            return;
        };
        let mut transaction = WinClipboardRestore::capture().unwrap();
        let receipt = write_clipboard_text_unlocked("vocalcode temporary paste").unwrap();
        transaction.expect_receipt(receipt);
        write_clipboard_text_unlocked("newer user copy").unwrap();
        drop(transaction);
        assert_eq!(read_clipboard_text().as_deref(), Some("newer user copy"));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "mutates the real Windows clipboard; run only in an isolated interactive QA session"]
    fn an_empty_clipboard_can_be_borrowed_and_restored() {
        let _transaction = lock_clipboard_transaction();
        let Ok(_outer_restore) = WinClipboardRestore::capture() else {
            return;
        };
        retry_clipboard_restore(CLIPBOARD_RESTORE_ATTEMPTS, clear_clipboard_once, || {
            std::thread::sleep(std::time::Duration::from_millis(10))
        })
        .unwrap();

        let mut empty_restore = WinClipboardRestore::capture().unwrap();
        let receipt = write_clipboard_text_unlocked("temporary VocalCode clipboard").unwrap();
        empty_restore.expect_receipt(receipt);
        empty_restore.restore().unwrap();
        assert!(clipboard_is_empty().unwrap());
    }
}

/// Linux has neither path yet.
#[cfg(not(any(target_os = "macos", windows)))]
pub fn copy_selection() -> Result<Option<String>> {
    Err(VocalCodeError::Inject(
        "selection copy is not implemented on this platform".into(),
    ))
}
