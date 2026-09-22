//! Native macOS HID input through `IOHIDManager`.
//!
//! `CGEventTap` remains the correct path for keyboard and mouse input because
//! it can suppress a bound control. It does not expose Bluetooth controllers,
//! presentation remotes, foot pedals, or vendor-defined macro devices. This
//! manager observes those parsed HID element values without seizing the device
//! and feeds them into the same native dispatcher used by Windows Raw Input.

use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::ptr::null;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use vocalcode_core::error::{Result, VocalCodeError};
use vocalcode_core::TriggerEventSender;

use crate::hid_input::{
    HidDeviceClass, HidElementSample, HidValueDecoder, HID_PAGE_BUTTON, HID_PAGE_CONSUMER,
    HID_PAGE_GENERIC_DESKTOP, HID_PAGE_VENDOR_MIN,
};
use crate::hotkey::{CaptureShared, NativeDevice, NativeDispatcher, SharedTriggers};

type CFAllocatorRef = *const c_void;
type CFIndex = isize;
type CFRunLoopRef = *mut c_void;
type CFStringRef = *const c_void;
type CFTypeRef = *const c_void;
type IOHIDDeviceRef = *mut c_void;
type IOHIDElementRef = *mut c_void;
type IOHIDManagerRef = *mut c_void;
type IOHIDValueRef = *mut c_void;
type IOReturn = i32;

type IOHIDDeviceCallback = Option<
    unsafe extern "C" fn(
        context: *mut c_void,
        result: IOReturn,
        sender: *mut c_void,
        device: IOHIDDeviceRef,
    ),
>;
type IOHIDValueCallback = Option<
    unsafe extern "C" fn(
        context: *mut c_void,
        result: IOReturn,
        sender: *mut c_void,
        value: IOHIDValueRef,
    ),
>;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOHIDManagerCreate(allocator: CFAllocatorRef, options: u32) -> IOHIDManagerRef;
    fn IOHIDManagerSetDeviceMatching(manager: IOHIDManagerRef, matching: *const c_void);
    fn IOHIDManagerRegisterDeviceMatchingCallback(
        manager: IOHIDManagerRef,
        callback: IOHIDDeviceCallback,
        context: *mut c_void,
    );
    fn IOHIDManagerRegisterDeviceRemovalCallback(
        manager: IOHIDManagerRef,
        callback: IOHIDDeviceCallback,
        context: *mut c_void,
    );
    fn IOHIDManagerRegisterInputValueCallback(
        manager: IOHIDManagerRef,
        callback: IOHIDValueCallback,
        context: *mut c_void,
    );
    fn IOHIDManagerScheduleWithRunLoop(
        manager: IOHIDManagerRef,
        run_loop: CFRunLoopRef,
        run_loop_mode: CFStringRef,
    );
    fn IOHIDManagerUnscheduleFromRunLoop(
        manager: IOHIDManagerRef,
        run_loop: CFRunLoopRef,
        run_loop_mode: CFStringRef,
    );
    fn IOHIDManagerOpen(manager: IOHIDManagerRef, options: u32) -> IOReturn;
    fn IOHIDManagerClose(manager: IOHIDManagerRef, options: u32) -> IOReturn;

    fn IOHIDValueGetElement(value: IOHIDValueRef) -> IOHIDElementRef;
    fn IOHIDValueGetIntegerValue(value: IOHIDValueRef) -> CFIndex;
    fn IOHIDElementGetDevice(element: IOHIDElementRef) -> IOHIDDeviceRef;
    fn IOHIDElementGetUsagePage(element: IOHIDElementRef) -> u32;
    fn IOHIDElementGetUsage(element: IOHIDElementRef) -> u32;
    fn IOHIDElementGetCookie(element: IOHIDElementRef) -> u32;
    fn IOHIDElementGetLogicalMin(element: IOHIDElementRef) -> CFIndex;
    fn IOHIDElementGetLogicalMax(element: IOHIDElementRef) -> CFIndex;
    fn IOHIDElementIsRelative(element: IOHIDElementRef) -> u8;

    fn IOHIDDeviceGetProperty(device: IOHIDDeviceRef, key: CFStringRef) -> CFTypeRef;
    fn IOHIDDeviceConformsTo(device: IOHIDDeviceRef, usage_page: u32, usage: u32) -> u8;
    fn IOHIDDeviceGetService(device: IOHIDDeviceRef) -> u32;
    fn IORegistryEntryGetRegistryEntryID(entry: u32, entry_id: *mut u64) -> IOReturn;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    static kCFRunLoopDefaultMode: CFStringRef;
    fn CFRelease(value: CFTypeRef);
    fn CFStringCreateWithCString(
        allocator: CFAllocatorRef,
        c_string: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFStringGetTypeID() -> usize;
    fn CFStringGetLength(string: CFStringRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
    fn CFStringGetCString(
        string: CFStringRef,
        buffer: *mut c_char,
        buffer_size: CFIndex,
        encoding: u32,
    ) -> u8;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(number: CFTypeRef, number_type: u32, value: *mut c_void) -> u8;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(boolean: CFTypeRef) -> u8;
    fn CFGetTypeID(value: CFTypeRef) -> usize;
}

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const CF_NUMBER_SINT64_TYPE: u32 = 4;

const KEY_VENDOR_ID: &[u8] = b"VendorID\0";
const KEY_PRODUCT_ID: &[u8] = b"ProductID\0";
const KEY_SERIAL_NUMBER: &[u8] = b"SerialNumber\0";
const KEY_PHYSICAL_DEVICE_UNIQUE_ID: &[u8] = b"PhysicalDeviceUniqueID\0";
const KEY_LOCATION_ID: &[u8] = b"LocationID\0";
const KEY_PRIMARY_USAGE_PAGE: &[u8] = b"PrimaryUsagePage\0";
const KEY_PRIMARY_USAGE: &[u8] = b"PrimaryUsage\0";
const KEY_DEVICE_USAGE_PAGE: &[u8] = b"DeviceUsagePage\0";
const KEY_DEVICE_USAGE: &[u8] = b"DeviceUsage\0";
const KEY_BUILT_IN: &[u8] = b"Built-In\0";

struct DeviceRecord {
    native: NativeDevice,
    class: HidDeviceClass,
    decoder: HidValueDecoder,
    built_in: bool,
    vendor_collection: bool,
    seen_supported_input: bool,
}

struct HidState {
    dispatch: NativeDispatcher,
    capture: Arc<CaptureShared>,
    devices: HashMap<usize, DeviceRecord>,
}

// IOHIDManager invokes these callbacks serially on the run loop where it was
// scheduled. The mutex also makes accidental future cross-thread scheduling
// safe, and is the sole gateway to the raw callback context.
struct CallbackState {
    inner: Mutex<HidState>,
}

/// Keeps the manager and callback context alive for the listener run loop.
pub(crate) struct MacHidManager {
    manager: IOHIDManagerRef,
    context: *mut CallbackState,
    run_loop: CFRunLoopRef,
}

impl Drop for MacHidManager {
    fn drop(&mut self) {
        unsafe {
            IOHIDManagerUnscheduleFromRunLoop(self.manager, self.run_loop, kCFRunLoopDefaultMode);
            let _ = IOHIDManagerClose(self.manager, 0);
            CFRelease(self.manager.cast_const().cast());
            drop(Box::from_raw(self.context));
        }
    }
}

pub(crate) fn install(
    triggers: SharedTriggers,
    capture: Arc<CaptureShared>,
    ready: Arc<AtomicBool>,
    tx: TriggerEventSender,
) -> Result<MacHidManager> {
    unsafe {
        let manager = IOHIDManagerCreate(null(), 0);
        if manager.is_null() {
            return Err(VocalCodeError::Hotkey(
                "could not create macOS HID manager".to_string(),
            ));
        }

        let context = Box::into_raw(Box::new(CallbackState {
            inner: Mutex::new(HidState {
                dispatch: NativeDispatcher::new(triggers, capture.clone(), ready, tx),
                capture,
                devices: HashMap::new(),
            }),
        }));

        // A null matcher enumerates every HID collection. The decoder explicitly
        // rejects keyboard-page values and mouse buttons, avoiding duplicate
        // delivery while still accepting vendor-defined pedals whose usage page
        // cannot be predicted before the device is attached.
        IOHIDManagerSetDeviceMatching(manager, null());
        IOHIDManagerRegisterDeviceMatchingCallback(manager, Some(device_matched), context.cast());
        IOHIDManagerRegisterDeviceRemovalCallback(manager, Some(device_removed), context.cast());
        IOHIDManagerRegisterInputValueCallback(manager, Some(input_value), context.cast());

        let run_loop = CFRunLoopGetCurrent();
        IOHIDManagerScheduleWithRunLoop(manager, run_loop, kCFRunLoopDefaultMode);
        let status = IOHIDManagerOpen(manager, 0);
        if status != 0 {
            IOHIDManagerUnscheduleFromRunLoop(manager, run_loop, kCFRunLoopDefaultMode);
            CFRelease(manager.cast_const().cast());
            drop(Box::from_raw(context));
            return Err(VocalCodeError::Hotkey(format!(
                "could not open macOS HID manager (IOReturn {status:#x})"
            )));
        }

        log::info!("macOS IOHID listener installed (gamepad, Consumer and vendor controls)");
        Ok(MacHidManager {
            manager,
            context,
            run_loop,
        })
    }
}

unsafe extern "C" fn device_matched(
    context: *mut c_void,
    result: IOReturn,
    _sender: *mut c_void,
    device: IOHIDDeviceRef,
) {
    if result != 0 || context.is_null() || device.is_null() {
        return;
    }
    let callback = &*(context as *const CallbackState);
    let Ok(mut state) = callback.inner.lock() else {
        return;
    };
    let key = device as usize;
    state
        .devices
        .entry(key)
        .or_insert_with(|| unsafe { load_device(device) });
}

unsafe extern "C" fn device_removed(
    context: *mut c_void,
    _result: IOReturn,
    _sender: *mut c_void,
    device: IOHIDDeviceRef,
) {
    if context.is_null() || device.is_null() {
        return;
    }
    let callback = &*(context as *const CallbackState);
    let Ok(mut state) = callback.inner.lock() else {
        return;
    };
    if let Some(record) = state.devices.remove(&(device as usize)) {
        if record.seen_supported_input {
            state.dispatch.disconnect(record.native.fingerprint);
            crate::hotkey::hook_diagnostic(crate::hotkey::HookDiagnostic::MacHidLifecycle {
                connected: false,
                fingerprint: record.native.fingerprint,
                vendor_id: record.native.vendor_id.unwrap_or(0),
                product_id: record.native.product_id.unwrap_or(0),
            });
        }
    }
}

unsafe extern "C" fn input_value(
    context: *mut c_void,
    result: IOReturn,
    _sender: *mut c_void,
    value: IOHIDValueRef,
) {
    if result != 0 || context.is_null() || value.is_null() {
        return;
    }
    let element = IOHIDValueGetElement(value);
    if element.is_null() {
        return;
    }
    let device = IOHIDElementGetDevice(element);
    if device.is_null() {
        return;
    }

    let Ok(usage_page) = u16::try_from(IOHIDElementGetUsagePage(element)) else {
        return;
    };
    let Ok(usage) = u16::try_from(IOHIDElementGetUsage(element)) else {
        return;
    };
    // Reject irrelevant axes/scancodes before taking the callback-state lock.
    if usage_page != HID_PAGE_BUTTON
        && usage_page != HID_PAGE_CONSUMER
        && usage_page < HID_PAGE_VENDOR_MIN
        && !(usage_page == HID_PAGE_GENERIC_DESKTOP && matches!(usage, 0x39 | 0x90..=0x93))
    {
        return;
    }

    let sample = HidElementSample {
        usage_page,
        usage,
        cookie: IOHIDElementGetCookie(element),
        value: IOHIDValueGetIntegerValue(value) as i64,
        logical_min: IOHIDElementGetLogicalMin(element) as i64,
        logical_max: IOHIDElementGetLogicalMax(element) as i64,
        relative: IOHIDElementIsRelative(element) != 0,
    };

    let callback = &*(context as *const CallbackState);
    let Ok(mut state) = callback.inner.lock() else {
        return;
    };
    let device_key = device as usize;
    state
        .devices
        .entry(device_key)
        .or_insert_with(|| unsafe { load_device(device) });

    // Decode while borrowing only the device record, then release that borrow
    // before forwarding edges through the dispatcher stored beside it.
    let was_capturing = state.capture.is_capturing();
    let (native, first_supported, edges) = {
        let Some(record) = state.devices.get_mut(&device_key) else {
            return;
        };
        // Accept private usages only from an external vendor-defined top-level
        // collection. Trackpads, Touch Bars and ordinary devices expose chatty
        // private elements that could otherwise win guided capture first.
        let unsupported_vendor =
            usage_page >= HID_PAGE_VENDOR_MIN && (record.built_in || !record.vendor_collection);
        let edges = if unsupported_vendor {
            Vec::new()
        } else {
            record.decoder.decode(record.class, sample)
        };
        let first_supported = !edges.is_empty() && !record.seen_supported_input;
        if !edges.is_empty() {
            record.seen_supported_input = true;
        }
        (record.native.clone(), first_supported, edges)
    };
    if first_supported {
        crate::hotkey::hook_diagnostic(crate::hotkey::HookDiagnostic::MacHidLifecycle {
            connected: true,
            fingerprint: native.fingerprint,
            vendor_id: native.vendor_id.unwrap_or(0),
            product_id: native.product_id.unwrap_or(0),
        });
    }

    // A diagonal hat switch creates two presses in one callback. During guided
    // capture consume only one of them; otherwise the second edge would execute
    // an existing binding immediately after the first edge completes capture.
    let mut captured_press_forwarded = false;
    for (control, pressed) in edges {
        if was_capturing && pressed {
            if captured_press_forwarded {
                continue;
            }
            captured_press_forwarded = true;
        }
        state.dispatch.edge(&native, control, pressed);
    }
}

unsafe fn load_device(device: IOHIDDeviceRef) -> DeviceRecord {
    let vendor_id = number_property(device, KEY_VENDOR_ID).and_then(nonzero_u16);
    let product_id = number_property(device, KEY_PRODUCT_ID).and_then(nonzero_u16);
    let primary_usage_page = number_property(device, KEY_PRIMARY_USAGE_PAGE)
        .or_else(|| number_property(device, KEY_DEVICE_USAGE_PAGE))
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(0);
    let primary_usage = number_property(device, KEY_PRIMARY_USAGE)
        .or_else(|| number_property(device, KEY_DEVICE_USAGE))
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(0);
    let class = device_class(device, primary_usage_page, primary_usage);
    let built_in = bool_property(device, KEY_BUILT_IN).unwrap_or(false);

    let published_serial = string_property(device, KEY_SERIAL_NUMBER).and_then(clean_identifier);
    let physical_unique_id = string_property(device, KEY_PHYSICAL_DEVICE_UNIQUE_ID)
        .and_then(clean_identifier)
        .or_else(|| {
            number_property(device, KEY_PHYSICAL_DEVICE_UNIQUE_ID)
                .filter(|value| *value != 0)
                .map(|value| format!("{value:016x}"))
        });
    let location = number_property(device, KEY_LOCATION_ID).filter(|value| *value != 0);
    let registry_id = registry_entry_id(device).filter(|value| *value != 0);

    // Many inexpensive USB/Bluetooth pedals omit a serial number. macOS's
    // physical-device id is the next-best durable identity; LocationID still
    // distinguishes identical units at fixed ports. The registry id is the
    // last resort and may require rebinding after reconnect.
    let selector_serial = published_serial
        .or_else(|| physical_unique_id.map(|value| format!("iohid-physical:{value}")))
        .or_else(|| location.map(|value| format!("iohid-location:{value:016x}")))
        .or_else(|| registry_id.map(|value| format!("iohid-registry:{value:016x}")));
    let identity = selector_serial
        .as_deref()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("pointer:{:x}", device as usize));
    let stable_id = format!(
        "iohid:{:04x}:{:04x}:{identity}:usage:{primary_usage_page:04x}:{primary_usage:04x}",
        vendor_id.unwrap_or(0),
        product_id.unwrap_or(0),
    );

    DeviceRecord {
        native: NativeDevice::new(stable_id, vendor_id, product_id, selector_serial),
        class,
        decoder: HidValueDecoder::default(),
        built_in,
        vendor_collection: primary_usage_page >= HID_PAGE_VENDOR_MIN,
        seen_supported_input: false,
    }
}

fn clean_identifier(value: String) -> Option<String> {
    let value = value.trim().to_string();
    let normalized = value.to_ascii_lowercase();
    let placeholder = matches!(normalized.as_str(), "unknown" | "none" | "n/a" | "(null)")
        || normalized
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .all(|ch| ch == '0');
    (!value.is_empty() && !placeholder).then_some(value)
}

unsafe fn device_class(
    device: IOHIDDeviceRef,
    primary_usage_page: u16,
    primary_usage: u16,
) -> HidDeviceClass {
    // Some Bluetooth controller drivers publish an unhelpful primary usage but
    // still list the correct application collection in UsagePairs.
    if [0x04, 0x05, 0x08]
        .into_iter()
        .any(|usage| IOHIDDeviceConformsTo(device, 0x01, usage) != 0)
    {
        HidDeviceClass::Gamepad
    } else if IOHIDDeviceConformsTo(device, 0x01, 0x06) != 0 {
        HidDeviceClass::Keyboard
    } else if IOHIDDeviceConformsTo(device, 0x01, 0x02) != 0 {
        HidDeviceClass::Mouse
    } else {
        HidDeviceClass::from_primary_usage(primary_usage_page, primary_usage)
    }
}

fn nonzero_u16(value: i64) -> Option<u16> {
    u16::try_from(value).ok().filter(|value| *value != 0)
}

unsafe fn registry_entry_id(device: IOHIDDeviceRef) -> Option<u64> {
    let service = IOHIDDeviceGetService(device);
    if service == 0 {
        return None;
    }
    let mut identity = 0u64;
    (IORegistryEntryGetRegistryEntryID(service, &mut identity) == 0).then_some(identity)
}

unsafe fn property(device: IOHIDDeviceRef, key: &[u8]) -> CFTypeRef {
    debug_assert_eq!(key.last(), Some(&0));
    let key_ref = CFStringCreateWithCString(
        null(),
        key.as_ptr().cast::<c_char>(),
        CF_STRING_ENCODING_UTF8,
    );
    if key_ref.is_null() {
        return null();
    }
    let value = IOHIDDeviceGetProperty(device, key_ref);
    CFRelease(key_ref.cast());
    value
}

unsafe fn number_property(device: IOHIDDeviceRef, key: &[u8]) -> Option<i64> {
    let value = property(device, key);
    if value.is_null() || CFGetTypeID(value) != CFNumberGetTypeID() {
        return None;
    }
    let mut number = 0i64;
    (CFNumberGetValue(
        value,
        CF_NUMBER_SINT64_TYPE,
        (&mut number as *mut i64).cast(),
    ) != 0)
        .then_some(number)
}

unsafe fn bool_property(device: IOHIDDeviceRef, key: &[u8]) -> Option<bool> {
    let value = property(device, key);
    if value.is_null() || CFGetTypeID(value) != CFBooleanGetTypeID() {
        return None;
    }
    Some(CFBooleanGetValue(value) != 0)
}

unsafe fn string_property(device: IOHIDDeviceRef, key: &[u8]) -> Option<String> {
    let value = property(device, key);
    if value.is_null() || CFGetTypeID(value) != CFStringGetTypeID() {
        return None;
    }
    let length = CFStringGetLength(value.cast());
    let maximum = CFStringGetMaximumSizeForEncoding(length, CF_STRING_ENCODING_UTF8);
    if maximum < 0 {
        return None;
    }
    let capacity = maximum.checked_add(1)?;
    let mut buffer = vec![0u8; usize::try_from(capacity).ok()?];
    if CFStringGetCString(
        value.cast(),
        buffer.as_mut_ptr().cast::<c_char>(),
        capacity,
        CF_STRING_ENCODING_UTF8,
    ) == 0
    {
        return None;
    }
    let end = buffer.iter().position(|byte| *byte == 0)?;
    Some(String::from_utf8_lossy(&buffer[..end]).into_owned())
}
