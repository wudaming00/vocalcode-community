//! Bounded, read-only description of the application currently in front.
//!
//! Meeting reminders use this only as a local hint. No title or process name is
//! persisted or sent over the network, and this module never starts capture.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundApplication {
    pub process_id: u32,
    /// Stable, non-secret application identity: executable filename on Windows
    /// and bundle identifier on macOS when one is available.
    pub app_id: String,
    pub display_name: String,
    /// Bounded title of the frontmost window. Empty when the OS refuses it.
    pub window_title: String,
}

const MAX_WINDOW_TITLE_UNITS: usize = 512;
#[cfg(windows)]
const MAX_PROCESS_PATH_UNITS: usize = 1_024;

pub fn foreground_application() -> Option<ForegroundApplication> {
    describe_foreground(true)
}

/// Fast identity-only query for the hotkey path. In particular, never asks a
/// macOS accessibility provider for a window title before opening capture.
pub fn foreground_app_id() -> Option<String> {
    describe_foreground(false).map(|app| app.app_id)
}

#[cfg(windows)]
fn describe_foreground(include_title: bool) -> Option<ForegroundApplication> {
    use std::path::Path;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
        },
        UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId},
    };

    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    let window = unsafe { GetForegroundWindow() };
    if window.is_null() {
        return None;
    }
    let mut process_id = 0_u32;
    unsafe { GetWindowThreadProcessId(window, &mut process_id) };
    if process_id == 0 {
        return None;
    }

    let mut title = [0_u16; MAX_WINDOW_TITLE_UNITS];
    let title_length = if include_title {
        unsafe { GetWindowTextW(window, title.as_mut_ptr(), title.len() as i32) }
    } else {
        0
    };
    let window_title = if title_length > 0 {
        String::from_utf16_lossy(&title[..title_length as usize])
    } else {
        String::new()
    };

    let process =
        OwnedHandle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) });
    let executable = if process.0.is_null() {
        String::new()
    } else {
        let mut path = [0_u16; MAX_PROCESS_PATH_UNITS];
        let mut length = path.len() as u32;
        if unsafe { QueryFullProcessImageNameW(process.0, 0, path.as_mut_ptr(), &mut length) } != 0
            && length > 0
            && length as usize <= path.len()
        {
            Path::new(&String::from_utf16_lossy(&path[..length as usize]))
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string()
        } else {
            String::new()
        }
    };
    if executable.is_empty() {
        return None;
    }
    let display_name = executable
        .strip_suffix(".exe")
        .unwrap_or(&executable)
        .to_string();
    Some(ForegroundApplication {
        process_id,
        app_id: executable.to_ascii_lowercase(),
        display_name,
        window_title,
    })
}

#[cfg(target_os = "macos")]
fn describe_foreground(include_title: bool) -> Option<ForegroundApplication> {
    use core_foundation::{
        base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType},
        string::{CFString, CFStringGetTypeID, CFStringRef},
    };
    use std::ffi::c_void;

    type AxUiElementRef = *const c_void;
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXUIElementCreateApplication(pid: i32) -> AxUiElementRef;
        fn AXUIElementCopyAttributeValue(
            element: AxUiElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> i32;
        fn AXUIElementSetMessagingTimeout(element: AxUiElementRef, timeout: f32) -> i32;
    }

    fn copy_attribute(element: AxUiElementRef, name: &str) -> Option<CFTypeRef> {
        if element.is_null() || unsafe { AXUIElementSetMessagingTimeout(element, 0.20) } != 0 {
            return None;
        }
        let attribute = CFString::new(name);
        let mut value: CFTypeRef = std::ptr::null();
        let result = unsafe {
            AXUIElementCopyAttributeValue(element, attribute.as_concrete_TypeRef(), &mut value)
        };
        (result == 0 && !value.is_null()).then_some(value)
    }

    fn copy_string_attribute(element: AxUiElementRef, name: &str) -> Option<String> {
        let value = copy_attribute(element, name)?;
        if unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() } {
            unsafe { CFRelease(value) };
            return None;
        }
        Some(unsafe { CFString::wrap_under_create_rule(value.cast()) }.to_string())
    }

    let application = objc2_app_kit::NSWorkspace::sharedWorkspace().frontmostApplication()?;
    let pid: i32 = unsafe { objc2::msg_send![&*application, processIdentifier] };
    if pid <= 0 {
        return None;
    }
    let display_name = application
        .localizedName()
        .map(|name| name.to_string())
        .unwrap_or_default();
    let app_id = application
        .bundleIdentifier()
        .map(|identifier| identifier.to_string().to_ascii_lowercase())
        .filter(|identifier| !identifier.is_empty())
        .unwrap_or_else(|| display_name.to_ascii_lowercase());
    if app_id.is_empty() {
        return None;
    }

    let ax_application = if include_title {
        unsafe { AXUIElementCreateApplication(pid) }
    } else {
        std::ptr::null()
    };
    let window_title = if ax_application.is_null() {
        String::new()
    } else {
        let title = copy_attribute(ax_application, "AXFocusedWindow")
            .and_then(|window| {
                let title = copy_string_attribute(window.cast(), "AXTitle");
                unsafe { CFRelease(window) };
                title
            })
            .unwrap_or_default();
        unsafe { CFRelease(ax_application.cast()) };
        title.chars().take(MAX_WINDOW_TITLE_UNITS - 1).collect()
    };

    Some(ForegroundApplication {
        process_id: pid as u32,
        app_id,
        display_name,
        window_title,
    })
}

#[cfg(not(any(windows, target_os = "macos")))]
fn describe_foreground(_include_title: bool) -> Option<ForegroundApplication> {
    None
}
