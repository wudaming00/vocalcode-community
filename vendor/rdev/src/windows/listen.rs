use crate::rdev::{Event, EventType, ListenError};
use crate::windows::common::{convert, set_key_hook, set_mouse_hook, HookError, HOOK, KEYBOARD};
use std::cell::RefCell;
use std::os::raw::c_int;
use std::ptr::null_mut;
use std::time::SystemTime;
use winapi::shared::minwindef::{LPARAM, LRESULT, WPARAM};
use winapi::um::winuser::{CallNextHookEx, GetMessageA, HC_ACTION};

thread_local! {
    // Windows dispatches these low-level hooks on the installing thread. TLS
    // therefore fits the callback's real ownership and, unlike a global Mutex,
    // does not require adding `Send` to rdev's cross-platform public API.
    static GLOBAL_CALLBACK: RefCell<Option<Box<dyn FnMut(Event)>>> = RefCell::new(None);
}

impl From<HookError> for ListenError {
    fn from(error: HookError) -> Self {
        match error {
            HookError::Mouse(code) => ListenError::MouseHookError(code),
            HookError::Key(code) => ListenError::KeyHookError(code),
        }
    }
}

unsafe extern "system" fn raw_callback(code: c_int, param: WPARAM, lpdata: LPARAM) -> LRESULT {
    if code == HC_ACTION {
        let opt = convert(param, lpdata);
        if let Some(event_type) = opt {
            let name = match &event_type {
                EventType::KeyPress(_key) => match (*KEYBOARD).lock() {
                    Ok(mut keyboard) => keyboard.get_name(lpdata),
                    Err(_) => None,
                },
                _ => None,
            };
            let event = Event {
                event_type,
                time: SystemTime::now(),
                name,
            };
            GLOBAL_CALLBACK.with(|slot| {
                let Some(mut callback) = slot.borrow_mut().take() else {
                    return;
                };
                callback(event);
                // Leave the slot vacant during invocation so re-entrant events
                // pass through instead of aliasing an active FnMut borrow.
                *slot.borrow_mut() = Some(callback);
            });
        }
    }
    CallNextHookEx(HOOK, code, param, lpdata)
}

pub fn listen<T>(callback: T) -> Result<(), ListenError>
where
    T: FnMut(Event) + 'static,
{
    GLOBAL_CALLBACK.with(|slot| *slot.borrow_mut() = Some(Box::new(callback)));
    unsafe {
        set_key_hook(raw_callback)?;
        set_mouse_hook(raw_callback)?;

        GetMessageA(null_mut(), null_mut(), 0, 0);
    }
    Ok(())
}
