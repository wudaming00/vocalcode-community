use crate::macos::common::*;
use crate::rdev::{Event, GrabError};
use cocoa::base::nil;
use cocoa::foundation::NSAutoreleasePool;
use core_graphics::event::{CGEventTapLocation, CGEventType};
use std::cell::RefCell;
use std::os::raw::c_void;

thread_local! {
    // The event tap invokes this callback on its owning run-loop thread. TLS
    // preserves rdev's non-Send public callback API and avoids aliased access to
    // a `static mut`. The slot is empty during invocation so a re-entrant event
    // passes through instead of borrowing one FnMut twice.
    static GLOBAL_CALLBACK: RefCell<Option<Box<dyn FnMut(Event) -> Option<Event>>>> =
        RefCell::new(None);
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
        let suppress = GLOBAL_CALLBACK.with(|slot| {
            let Some(mut callback) = slot.borrow_mut().take() else {
                return false;
            };
            let suppress = callback(event).is_none();
            *slot.borrow_mut() = Some(callback);
            suppress
        });
        if suppress {
            cg_event.set_type(CGEventType::Null);
        }
    }
    cg_event_ref
}

pub fn grab<T>(callback: T) -> Result<(), GrabError>
where
    T: FnMut(Event) -> Option<Event> + 'static,
{
    unsafe {
        GLOBAL_CALLBACK.with(|slot| *slot.borrow_mut() = Some(Box::new(callback)));
        let _pool = NSAutoreleasePool::new(nil);
        let tap = CGEventTapCreate(
            CGEventTapLocation::HID, // HID, Session, AnnotatedSession,
            kCGHeadInsertEventTap,
            CGEventTapOption::Default,
            kCGEventMaskForAllEvents,
            raw_callback,
            nil,
        );
        if tap.is_null() {
            return Err(GrabError::EventTapError);
        }
        let _loop = CFMachPortCreateRunLoopSource(nil, tap, 0);
        if _loop.is_null() {
            return Err(GrabError::LoopSourceError);
        }

        let current_loop = CFRunLoopGetCurrent();
        CFRunLoopAddSource(current_loop, _loop, kCFRunLoopCommonModes);

        CGEventTapEnable(tap, true);
        CFRunLoopRun();
    }
    Ok(())
}
