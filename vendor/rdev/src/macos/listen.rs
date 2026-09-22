use crate::macos::common::*;
use crate::rdev::{Event, ListenError};
use cocoa::base::nil;
use cocoa::foundation::NSAutoreleasePool;
use core_graphics::event::{CGEventTapLocation, CGEventType};
use std::cell::RefCell;
use std::os::raw::c_void;

thread_local! {
    // CGEventTap callbacks run on the tap's run-loop thread, making TLS the
    // actual ownership boundary for this non-Send callback.
    static GLOBAL_CALLBACK: RefCell<Option<Box<dyn FnMut(Event)>>> = RefCell::new(None);
}

#[link(name = "Cocoa", kind = "framework")]
extern "C" {}

unsafe extern "C" fn raw_callback(
    _proxy: CGEventTapProxy,
    _type: CGEventType,
    cg_event_ref: CGEventRef,
    _user_info: *mut c_void,
) -> CGEventRef {
    let Some(cg_event) = retained_event(cg_event_ref) else {
        return cg_event_ref;
    };
    let event = KEYBOARD_STATE
        .lock()
        .ok()
        .and_then(|mut keyboard| convert(_type, &cg_event, &mut keyboard));
    if let Some(event) = event {
        GLOBAL_CALLBACK.with(|slot| {
            let Some(mut callback) = slot.borrow_mut().take() else {
                return;
            };
            callback(event);
            *slot.borrow_mut() = Some(callback);
        });
    }
    cg_event_ref
}

pub fn listen<T>(callback: T) -> Result<(), ListenError>
where
    T: FnMut(Event) + 'static,
{
    unsafe {
        GLOBAL_CALLBACK.with(|slot| *slot.borrow_mut() = Some(Box::new(callback)));
        let _pool = NSAutoreleasePool::new(nil);
        let tap = CGEventTapCreate(
            CGEventTapLocation::HID, // HID, Session, AnnotatedSession,
            kCGHeadInsertEventTap,
            CGEventTapOption::ListenOnly,
            kCGEventMaskForAllEvents,
            raw_callback,
            nil,
        );
        if tap.is_null() {
            return Err(ListenError::EventTapError);
        }
        let _loop = CFMachPortCreateRunLoopSource(nil, tap, 0);
        if _loop.is_null() {
            return Err(ListenError::LoopSourceError);
        }

        let current_loop = CFRunLoopGetCurrent();
        CFRunLoopAddSource(current_loop, _loop, kCFRunLoopCommonModes);

        CGEventTapEnable(tap, true);
        CFRunLoopRun();
    }
    Ok(())
}
