use crate::rdev::{Event, GrabError};
use crate::windows::common::{convert, set_key_hook, set_mouse_hook, HookError, HOOK};
use std::cell::RefCell;
use std::ptr::null_mut;
use std::time::SystemTime;
use winapi::um::winuser::{CallNextHookEx, GetMessageA, HC_ACTION};

thread_local! {
    // WH_KEYBOARD_LL/WH_MOUSE_LL dispatch the callback on the thread that
    // installed the hook. Keeping the non-Send FnMut in that thread's storage
    // preserves rdev's public callback bounds and removes the aliased
    // `static mut`. Taking it out before invocation also makes a re-entrant
    // hook event pass through instead of deadlocking or creating two &mut refs.
    static GLOBAL_CALLBACK: RefCell<Option<Box<dyn FnMut(Event) -> Option<Event>>>> =
        RefCell::new(None);
}

unsafe extern "system" fn raw_callback(code: i32, param: usize, lpdata: isize) -> isize {
    if code == HC_ACTION {
        let opt = convert(param, lpdata);
        if let Some(event_type) = opt {
            // VocalCode patch: never resolve the key's text name inside the
            // low-level hook. `get_name` -> `AttachThreadInput`(foreground) +
            // `GetKeyboardState` + `ToUnicodeEx` runs on every key press with
            // the whole system's keyboard pipeline serialised behind it; a busy
            // foreground thread stalls every keystroke on the machine. Nothing
            // in VocalCode reads `Event::name`.
            let name = None;
            let event = Event {
                event_type,
                time: SystemTime::now(),
                name,
            };
            let suppress = GLOBAL_CALLBACK.with(|slot| {
                let Some(mut callback) = slot.borrow_mut().take() else {
                    return false;
                };
                let suppress = callback(event).is_none();
                *slot.borrow_mut() = Some(callback);
                suppress
            });
            if suppress {
                // https://stackoverflow.com/questions/42756284/blocking-windows-mouse-click-using-setwindowshookex
                // https://android.developreference.com/article/14560004/Blocking+windows+mouse+click+using+SetWindowsHookEx()
                // https://cboard.cprogramming.com/windows-programming/99678-setwindowshookex-wm_keyboard_ll.html
                // let _result = CallNextHookEx(HOOK, code, param, lpdata);
                return 1;
            }
        }
    }
    CallNextHookEx(HOOK, code, param, lpdata)
}
impl From<HookError> for GrabError {
    fn from(error: HookError) -> Self {
        match error {
            HookError::Mouse(code) => GrabError::MouseHookError(code),
            HookError::Key(code) => GrabError::KeyHookError(code),
        }
    }
}

pub fn grab<T>(callback: T) -> Result<(), GrabError>
where
    T: FnMut(Event) -> Option<Event> + 'static,
{
    GLOBAL_CALLBACK.with(|slot| *slot.borrow_mut() = Some(Box::new(callback)));
    unsafe {
        set_key_hook(raw_callback)?;
        set_mouse_hook(raw_callback)?;

        GetMessageA(null_mut(), null_mut(), 0, 0);
    }
    Ok(())
}
