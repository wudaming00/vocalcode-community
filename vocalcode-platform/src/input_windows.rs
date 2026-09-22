//! Native Windows trigger sources that are invisible to the keyboard/mouse
//! hook: XInput gamepads plus Raw Input HID and Consumer-page collections.
//!
//! Raw Input is observation-only here.  It never suppresses the device's normal
//! input; matching actions are dispatched through the same edge/state machine
//! as the global hook.  The rdev hook still owns keyboard and mouse suppression.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{addr_of, null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use vocalcode_core::config::GamepadButton;
use vocalcode_core::TriggerEventSender;
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidP_GetButtonCaps, HidP_GetCaps, HidP_GetUsagesEx, HidP_GetValueCaps, HidP_Input,
    HIDP_BUTTON_CAPS, HIDP_CAPS, HIDP_STATUS_SUCCESS, HIDP_VALUE_CAPS, PHIDP_PREPARSED_DATA,
    USAGE_AND_PAGE,
};
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_CLASS_ALREADY_EXISTS, ERROR_DEVICE_NOT_CONNECTED, HANDLE, HWND, LPARAM,
    LRESULT, WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::XboxController::{
    XInputGetState, XINPUT_GAMEPAD_A, XINPUT_GAMEPAD_B, XINPUT_GAMEPAD_BACK,
    XINPUT_GAMEPAD_DPAD_DOWN, XINPUT_GAMEPAD_DPAD_LEFT, XINPUT_GAMEPAD_DPAD_RIGHT,
    XINPUT_GAMEPAD_DPAD_UP, XINPUT_GAMEPAD_LEFT_SHOULDER, XINPUT_GAMEPAD_LEFT_THUMB,
    XINPUT_GAMEPAD_RIGHT_SHOULDER, XINPUT_GAMEPAD_RIGHT_THUMB, XINPUT_GAMEPAD_START,
    XINPUT_GAMEPAD_X, XINPUT_GAMEPAD_Y, XINPUT_STATE, XUSER_MAX_COUNT,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, GetRawInputDeviceInfoW, RegisterRawInputDevices, RAWINPUT, RAWINPUTDEVICE,
    RAWINPUTHEADER, RIDEV_DEVNOTIFY, RIDEV_INPUTSINK, RIDEV_PAGEONLY, RIDI_DEVICEINFO,
    RIDI_DEVICENAME, RIDI_PREPARSEDDATA, RID_DEVICE_INFO, RID_INPUT, RIM_TYPEHID,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, KillTimer, RegisterClassW, SetTimer, SetWindowLongPtrW, TranslateMessage,
    CREATESTRUCTW, GIDC_REMOVAL, GWLP_USERDATA, HWND_MESSAGE, MSG, WM_INPUT,
    WM_INPUT_DEVICE_CHANGE, WM_NCCREATE, WM_TIMER, WNDCLASSW,
};

use crate::hotkey::{
    fnv1a64, CaptureShared, NativeControl, NativeDevice, NativeDispatcher, SharedTriggers,
};

const RAW_ERROR: u32 = u32::MAX;
const RAW_INPUT_CLASS: &[u16] = &[
    b'V' as u16,
    b'o' as u16,
    b'c' as u16,
    b'a' as u16,
    b'l' as u16,
    b'C' as u16,
    b'o' as u16,
    b'd' as u16,
    b'e' as u16,
    b'R' as u16,
    b'a' as u16,
    b'w' as u16,
    b'I' as u16,
    b'n' as u16,
    b'p' as u16,
    b'u' as u16,
    b't' as u16,
    0,
];

static RAW_INPUT_RUNNING: AtomicBool = AtomicBool::new(false);

pub(crate) fn spawn(
    triggers: SharedTriggers,
    capture: Arc<CaptureShared>,
    ready: Arc<AtomicBool>,
    tx: TriggerEventSender,
    shutdown: Arc<AtomicBool>,
) -> Vec<thread::JoinHandle<()>> {
    let mut workers = Vec::with_capacity(2);
    let xinput_dispatch =
        NativeDispatcher::new(triggers.clone(), capture.clone(), ready.clone(), tx.clone());
    let xinput_shutdown = shutdown.clone();
    match thread::Builder::new()
        .name("vocalcode-xinput".into())
        .spawn(move || run_xinput(xinput_dispatch, xinput_shutdown))
    {
        Ok(worker) => workers.push(worker),
        Err(error) => log::warn!("could not start XInput listener: {error}"),
    }

    let raw_dispatch = NativeDispatcher::new(triggers, capture, ready, tx);
    match thread::Builder::new()
        .name("vocalcode-raw-input".into())
        .spawn(move || run_raw_input(raw_dispatch, shutdown))
    {
        Ok(worker) => workers.push(worker),
        Err(error) => log::warn!("could not start Raw Input listener: {error}"),
    }
    workers
}

// ---------------------------------------------------------------------------
// XInput
// ---------------------------------------------------------------------------

const XINPUT_BUTTONS: &[(u16, GamepadButton)] = &[
    (XINPUT_GAMEPAD_A, GamepadButton::South),
    (XINPUT_GAMEPAD_B, GamepadButton::East),
    (XINPUT_GAMEPAD_X, GamepadButton::West),
    (XINPUT_GAMEPAD_Y, GamepadButton::North),
    (XINPUT_GAMEPAD_LEFT_SHOULDER, GamepadButton::LeftShoulder),
    (XINPUT_GAMEPAD_RIGHT_SHOULDER, GamepadButton::RightShoulder),
    (XINPUT_GAMEPAD_LEFT_THUMB, GamepadButton::LeftThumb),
    (XINPUT_GAMEPAD_RIGHT_THUMB, GamepadButton::RightThumb),
    (XINPUT_GAMEPAD_START, GamepadButton::Start),
    (XINPUT_GAMEPAD_BACK, GamepadButton::Back),
    (XINPUT_GAMEPAD_DPAD_UP, GamepadButton::DpadUp),
    (XINPUT_GAMEPAD_DPAD_DOWN, GamepadButton::DpadDown),
    (XINPUT_GAMEPAD_DPAD_LEFT, GamepadButton::DpadLeft),
    (XINPUT_GAMEPAD_DPAD_RIGHT, GamepadButton::DpadRight),
];

fn run_xinput(mut dispatch: NativeDispatcher, shutdown: Arc<AtomicBool>) {
    let devices: Vec<_> = (0..XUSER_MAX_COUNT)
        .map(|slot| NativeDevice::new(format!("xinput:{slot}"), None, None, None))
        .collect();
    let mut previous: [Option<u16>; XUSER_MAX_COUNT as usize] = [None; XUSER_MAX_COUNT as usize];

    while !shutdown.load(Ordering::Acquire) {
        for slot in 0..XUSER_MAX_COUNT {
            let mut state = XINPUT_STATE::default();
            let status = unsafe { XInputGetState(slot, &mut state) };
            let index = slot as usize;
            if status == 0 {
                let buttons = state.Gamepad.wButtons;
                let before = previous[index].unwrap_or(0);
                if previous[index].is_none() {
                    log::info!("XInput controller connected in slot {slot}");
                }
                let changed = before ^ buttons;
                for &(mask, button) in XINPUT_BUTTONS {
                    if changed & mask != 0 {
                        dispatch.edge(
                            &devices[index],
                            NativeControl::Gamepad(button),
                            buttons & mask != 0,
                        );
                    }
                }
                previous[index] = Some(buttons);
            } else if status == ERROR_DEVICE_NOT_CONNECTED && previous[index].take().is_some() {
                log::info!("XInput controller disconnected from slot {slot}");
                dispatch.disconnect(devices[index].fingerprint);
            }
        }
        // 125 Hz keeps button latency below one display frame without spinning.
        thread::sleep(Duration::from_millis(8));
    }
}

// ---------------------------------------------------------------------------
// Raw Input HID
// ---------------------------------------------------------------------------

struct RawDeviceState {
    device: NativeDevice,
    usage_page: u16,
    usage: u16,
    preparsed: Vec<usize>,
    report_ids: HashSet<u8>,
    /// HID report IDs describe independent report layouts. Keep each layout's
    /// last snapshot separately so receiving report 2 does not release buttons
    /// that are still held in report 1.
    active_by_report: HashMap<u8, HashSet<NativeControl>>,
    ignored: bool,
}

impl RawDeviceState {
    fn ignored(handle: HANDLE) -> Self {
        let stable_id = format!("raw-handle:{:x}", handle as usize);
        Self {
            device: NativeDevice::new(stable_id, None, None, None),
            usage_page: 0,
            usage: 0,
            preparsed: Vec::new(),
            report_ids: HashSet::new(),
            active_by_report: HashMap::new(),
            ignored: true,
        }
    }
}

struct RawState {
    dispatch: NativeDispatcher,
    devices: HashMap<usize, RawDeviceState>,
}

fn run_raw_input(dispatch: NativeDispatcher, shutdown: Arc<AtomicBool>) {
    unsafe {
        let instance = GetModuleHandleW(null());
        if instance.is_null() {
            log::warn!("Raw Input disabled: GetModuleHandleW failed");
            return;
        }

        let class = WNDCLASSW {
            lpfnWndProc: Some(raw_window_proc),
            hInstance: instance,
            lpszClassName: RAW_INPUT_CLASS.as_ptr(),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            log::warn!("Raw Input disabled: could not register message-window class");
            return;
        }

        let state = Box::into_raw(Box::new(RawState {
            dispatch,
            devices: HashMap::new(),
        }));
        let window = CreateWindowExW(
            0,
            RAW_INPUT_CLASS.as_ptr(),
            RAW_INPUT_CLASS.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            instance,
            state.cast(),
        );
        if window.is_null() {
            drop(Box::from_raw(state));
            log::warn!("Raw Input disabled: could not create message window");
            return;
        }

        if !register_raw_devices(window) {
            SetWindowLongPtrW(window, GWLP_USERDATA, 0);
            let _ = DestroyWindow(window);
            drop(Box::from_raw(state));
            log::warn!("Raw Input disabled: device registration failed");
            return;
        }
        let shutdown_timer = SetTimer(null_mut(), 0, 100, None);
        if shutdown_timer == 0 {
            SetWindowLongPtrW(window, GWLP_USERDATA, 0);
            let _ = DestroyWindow(window);
            drop(Box::from_raw(state));
            log::warn!("Raw Input disabled: could not create shutdown timer");
            return;
        }
        RAW_INPUT_RUNNING.store(true, Ordering::Release);
        log::info!("Raw Input listener installed (Consumer, gamepad and vendor HID)");

        let mut message: MSG = zeroed();
        loop {
            let result = GetMessageW(&mut message, null_mut(), 0, 0);
            if result <= 0 {
                break;
            }
            if message.message == WM_TIMER
                && message.wParam == shutdown_timer
                && shutdown.load(Ordering::Acquire)
            {
                break;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        let _ = KillTimer(null_mut(), shutdown_timer);
        RAW_INPUT_RUNNING.store(false, Ordering::Release);
        SetWindowLongPtrW(window, GWLP_USERDATA, 0);
        let _ = DestroyWindow(window);
        drop(Box::from_raw(state));
    }
}

unsafe fn register_raw_devices(window: HWND) -> bool {
    let flags = RIDEV_INPUTSINK | RIDEV_DEVNOTIFY;
    let mut registrations = vec![
        // Generic Desktop joystick, gamepad and multi-axis controller.
        RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x04,
            dwFlags: flags,
            hwndTarget: window,
        },
        RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x05,
            dwFlags: flags,
            hwndTarget: window,
        },
        RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x08,
            dwFlags: flags,
            hwndTarget: window,
        },
        // Consumer Control (media keys, presentation remotes, pedals).
        RAWINPUTDEVICE {
            usUsagePage: 0x0c,
            usUsage: 0x01,
            dwFlags: flags,
            hwndTarget: window,
        },
    ];

    // Macro pads, pedals and specialist controllers commonly expose one of the
    // 256 vendor-defined usage pages as their top-level collection. Page-only
    // registration covers devices already present and hot-plugged later.
    registrations.extend((0xff00u16..=0xffff).map(|page| RAWINPUTDEVICE {
        usUsagePage: page,
        usUsage: 0,
        dwFlags: flags | RIDEV_PAGEONLY,
        hwndTarget: window,
    }));

    RegisterRawInputDevices(
        registrations.as_ptr(),
        registrations.len() as u32,
        size_of::<RAWINPUTDEVICE>() as u32,
    ) != 0
}

unsafe extern "system" fn raw_window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lparam as *const CREATESTRUCTW);
        SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
        return DefWindowProcW(window, message, wparam, lparam);
    }

    let state_ptr = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut RawState;
    if state_ptr.is_null() {
        return DefWindowProcW(window, message, wparam, lparam);
    }
    let state = &mut *state_ptr;
    match message {
        WM_INPUT => {
            process_raw_packet(state, lparam);
            0
        }
        WM_INPUT_DEVICE_CHANGE if wparam as u32 == GIDC_REMOVAL => {
            let key = lparam as usize;
            if let Some(device) = state.devices.remove(&key) {
                if !device.ignored {
                    state.dispatch.disconnect(device.device.fingerprint);
                    crate::hotkey::hook_diagnostic(
                        crate::hotkey::HookDiagnostic::WindowsRawLifecycle {
                            connected: false,
                            fingerprint: device.device.fingerprint,
                            vendor_id: device.device.vendor_id.unwrap_or(0),
                            product_id: device.device.product_id.unwrap_or(0),
                        },
                    );
                }
            }
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

unsafe fn process_raw_packet(state: &mut RawState, raw_handle: LPARAM) {
    let mut byte_count = 0u32;
    if GetRawInputData(
        raw_handle as _,
        RID_INPUT,
        null_mut(),
        &mut byte_count,
        size_of::<RAWINPUTHEADER>() as u32,
    ) == RAW_ERROR
        || byte_count < size_of::<RAWINPUT>() as u32
    {
        return;
    }

    // `usize` storage guarantees enough alignment for casting the buffer to a
    // RAWINPUT header. Vec<u8> does not make that alignment guarantee in Rust.
    let words = (byte_count as usize).div_ceil(size_of::<usize>());
    let mut storage = vec![0usize; words];
    let got = GetRawInputData(
        raw_handle as _,
        RID_INPUT,
        storage.as_mut_ptr().cast(),
        &mut byte_count,
        size_of::<RAWINPUTHEADER>() as u32,
    );
    if got == RAW_ERROR || got < size_of::<RAWINPUT>() as u32 {
        return;
    }

    let raw = storage.as_ptr() as *const RAWINPUT;
    if (*raw).header.dwType != RIM_TYPEHID || (*raw).header.hDevice.is_null() {
        return;
    }
    let device_handle = (*raw).header.hDevice;
    let key = device_handle as usize;
    let device = state.devices.entry(key).or_insert_with(|| {
        let device = load_raw_device(device_handle)
            .unwrap_or_else(|| RawDeviceState::ignored(device_handle));
        if !device.ignored {
            crate::hotkey::hook_diagnostic(crate::hotkey::HookDiagnostic::WindowsRawLifecycle {
                connected: true,
                fingerprint: device.device.fingerprint,
                vendor_id: device.device.vendor_id.unwrap_or(0),
                product_id: device.device.product_id.unwrap_or(0),
            });
        }
        device
    });
    if device.ignored {
        return;
    }

    let hid_ptr = addr_of!((*raw).data.hid);
    let report_size = (*hid_ptr).dwSizeHid as usize;
    let report_count = (*hid_ptr).dwCount as usize;
    if report_size == 0 || report_count == 0 {
        return;
    }
    let data_ptr = addr_of!((*hid_ptr).bRawData).cast::<u8>();
    let buffer_start = storage.as_ptr().cast::<u8>();
    let data_offset = data_ptr as usize - buffer_start as usize;
    let Some(total) = report_size.checked_mul(report_count) else {
        return;
    };
    let Some(data_end) = data_offset.checked_add(total) else {
        return;
    };
    if data_end > got as usize {
        return;
    }
    let reports = std::slice::from_raw_parts(data_ptr, total);

    // `dwCount` is a time-ordered sequence of complete HID reports, not a set
    // of simultaneous fragments. Decode/diff each report before moving to the
    // next one; unioning the batch loses a press followed by a release in the
    // same WM_INPUT packet.
    let edges = decode_report_edges(device, reports, report_size);
    for (control, pressed) in edges {
        state.dispatch.edge(&device.device, control, pressed);
    }
}

fn decode_report_edges(
    device: &mut RawDeviceState,
    reports: &[u8],
    report_size: usize,
) -> Vec<(NativeControl, bool)> {
    let mut edges = Vec::new();
    for report in reports.chunks_exact(report_size) {
        let report_id = report
            .first()
            .copied()
            .filter(|id| device.report_ids.contains(id))
            .unwrap_or(0);
        let mut next = HashSet::new();

        // Vendor-defined collections often publish descriptors that parse but
        // do not expose their macro/pedal controls as HID buttons. Their report
        // bits are the stable source of truth, so always use the fallback there.
        let semantic = device.usage_page < 0xff00 && decode_usages(device, report, &mut next);
        if !semantic {
            decode_vendor_bits(device, report, report_id, &mut next);
        }

        let active = device.active_by_report.entry(report_id).or_default();
        edges.extend(
            active
                .difference(&next)
                .copied()
                .map(|control| (control, false)),
        );
        edges.extend(
            next.difference(active)
                .copied()
                .map(|control| (control, true)),
        );
        *active = next;
    }
    edges
}

fn decode_vendor_bits(
    device: &RawDeviceState,
    report: &[u8],
    report_id: u8,
    output: &mut HashSet<NativeControl>,
) {
    // Descriptors on vendor devices are often deliberately opaque. A stable
    // report-bit fallback still makes pedals/macro buttons bindable. HID report
    // ID bytes are metadata rather than controls, but the ID remains part of
    // the persisted control identity so independent layouts cannot collide.
    // Crucially, the identity never includes this report's position in a
    // `dwCount` batch: that position changes from packet to packet.
    for (byte_index, byte) in report.iter().enumerate() {
        if byte_index == 0 && report_id != 0 {
            continue;
        }
        for bit in 0..8u32 {
            if byte & (1 << bit) != 0 {
                let control = (u32::from(report_id) << 24) | (byte_index as u32) << 3 | bit;
                output.insert(NativeControl::Hid {
                    usage_page: device.usage_page,
                    usage: device.usage,
                    control: control + 1,
                });
            }
        }
    }
}

fn decode_usages(
    device: &RawDeviceState,
    report: &[u8],
    output: &mut HashSet<NativeControl>,
) -> bool {
    if device.preparsed.is_empty() {
        return false;
    }
    let preparsed = device.preparsed.as_ptr() as PHIDP_PREPARSED_DATA;
    // This is deliberately generous. If a pathological descriptor needs more,
    // HidP returns BUFFER_TOO_SMALL and this packet uses the bit fallback.
    let mut usages = vec![USAGE_AND_PAGE::default(); 512];
    let mut count = usages.len() as u32;
    let status = unsafe {
        HidP_GetUsagesEx(
            HidP_Input,
            0,
            usages.as_mut_ptr(),
            &mut count,
            preparsed,
            report.as_ptr().cast(),
            report.len() as u32,
        )
    };
    if status != HIDP_STATUS_SUCCESS {
        return false;
    }
    for item in usages.into_iter().take(count as usize) {
        if item.UsagePage == 0x0c {
            // These standard usages also arrive through the low-level keyboard
            // hook as multimedia virtual keys. Let that path dispatch *and
            // suppress* them; emitting a Raw Input copy would fire twice.
            if !is_hook_consumer_usage(item.Usage) {
                output.insert(NativeControl::Consumer(item.Usage));
            }
        } else {
            output.insert(NativeControl::Hid {
                usage_page: item.UsagePage,
                usage: item.Usage,
                control: u32::from(item.Usage),
            });
        }
    }
    true
}

fn is_hook_consumer_usage(usage: u16) -> bool {
    matches!(
        usage,
        0x00e2 // mute
            | 0x00ea // volume down
            | 0x00e9 // volume up
            | 0x00b5 // next track
            | 0x00b6 // previous track
            | 0x00b7 // stop
            | 0x00cd // play/pause
            | 0x0224 // browser back
            | 0x0225 // browser forward
            | 0x0227 // browser refresh
            | 0x0226 // browser stop
            | 0x0221 // browser search
            | 0x022a // browser favourites
            | 0x0223 // browser home
    )
}

unsafe fn load_raw_device(handle: HANDLE) -> Option<RawDeviceState> {
    let path = raw_device_name(handle)?;
    // XInput controllers also expose a Raw Input HID interface whose path uses
    // the IG_ marker. XInput supplies the normalized button layout; listening
    // to both would fire every action twice.
    if path.to_ascii_uppercase().contains("IG_") {
        return Some(RawDeviceState::ignored(handle));
    }

    let mut info = RID_DEVICE_INFO {
        cbSize: size_of::<RID_DEVICE_INFO>() as u32,
        ..Default::default()
    };
    let mut info_size = size_of::<RID_DEVICE_INFO>() as u32;
    if GetRawInputDeviceInfoW(
        handle,
        RIDI_DEVICEINFO,
        (&mut info as *mut RID_DEVICE_INFO).cast(),
        &mut info_size,
    ) == RAW_ERROR
        || info.dwType != RIM_TYPEHID
    {
        return None;
    }
    let hid = info.Anonymous.hid;
    let normalized = path.to_ascii_lowercase();
    let stable_id = format!("raw:{:016x}", fnv1a64(normalized.as_bytes()));
    let preparsed = raw_preparsed_data(handle);
    let report_ids = raw_report_ids(&preparsed);
    Some(RawDeviceState {
        device: NativeDevice::new(
            stable_id,
            u16::try_from(hid.dwVendorId).ok().filter(|id| *id != 0),
            u16::try_from(hid.dwProductId).ok().filter(|id| *id != 0),
            None,
        ),
        usage_page: hid.usUsagePage,
        usage: hid.usUsage,
        preparsed,
        report_ids,
        active_by_report: HashMap::new(),
        ignored: false,
    })
}

unsafe fn raw_device_name(handle: HANDLE) -> Option<String> {
    let mut chars = 0u32;
    if GetRawInputDeviceInfoW(handle, RIDI_DEVICENAME, null_mut(), &mut chars) == RAW_ERROR
        || chars == 0
    {
        return None;
    }
    let mut buffer = vec![0u16; chars as usize + 1];
    if GetRawInputDeviceInfoW(
        handle,
        RIDI_DEVICENAME,
        buffer.as_mut_ptr().cast(),
        &mut chars,
    ) == RAW_ERROR
    {
        return None;
    }
    let end = buffer
        .iter()
        .position(|ch| *ch == 0)
        .unwrap_or(chars as usize);
    Some(String::from_utf16_lossy(&buffer[..end]))
}

unsafe fn raw_preparsed_data(handle: HANDLE) -> Vec<usize> {
    let mut byte_count = 0u32;
    if GetRawInputDeviceInfoW(handle, RIDI_PREPARSEDDATA, null_mut(), &mut byte_count) == RAW_ERROR
        || byte_count == 0
    {
        return Vec::new();
    }
    let words = (byte_count as usize).div_ceil(size_of::<usize>());
    let mut buffer = vec![0usize; words];
    if GetRawInputDeviceInfoW(
        handle,
        RIDI_PREPARSEDDATA,
        buffer.as_mut_ptr().cast::<c_void>(),
        &mut byte_count,
    ) == RAW_ERROR
    {
        Vec::new()
    } else {
        buffer
    }
}

/// Report IDs occupy byte zero when a descriptor declares them. Without this
/// list, the fallback decoder turns the ID's constant bits into phantom pedals.
fn raw_report_ids(preparsed: &[usize]) -> HashSet<u8> {
    let mut ids = HashSet::new();
    if preparsed.is_empty() {
        return ids;
    }
    let data = preparsed.as_ptr() as PHIDP_PREPARSED_DATA;
    let mut caps = HIDP_CAPS::default();
    if unsafe { HidP_GetCaps(data, &mut caps) } != HIDP_STATUS_SUCCESS {
        return ids;
    }

    let mut button_caps = vec![HIDP_BUTTON_CAPS::default(); caps.NumberInputButtonCaps as usize];
    let mut button_count = caps.NumberInputButtonCaps;
    if button_count != 0
        && unsafe {
            HidP_GetButtonCaps(
                HidP_Input,
                button_caps.as_mut_ptr(),
                &mut button_count,
                data,
            )
        } == HIDP_STATUS_SUCCESS
    {
        ids.extend(
            button_caps
                .into_iter()
                .take(button_count as usize)
                .map(|cap| cap.ReportID)
                .filter(|id| *id != 0),
        );
    }

    let mut value_caps = vec![HIDP_VALUE_CAPS::default(); caps.NumberInputValueCaps as usize];
    let mut value_count = caps.NumberInputValueCaps;
    if value_count != 0
        && unsafe { HidP_GetValueCaps(HidP_Input, value_caps.as_mut_ptr(), &mut value_count, data) }
            == HIDP_STATUS_SUCCESS
    {
        ids.extend(
            value_caps
                .into_iter()
                .take(value_count as usize)
                .map(|cap| cap.ReportID)
                .filter(|id| *id != 0),
        );
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xinput_button_map_has_no_duplicate_masks_or_controls() {
        let mut masks = HashSet::new();
        let mut buttons = HashSet::new();
        for (mask, button) in XINPUT_BUTTONS {
            assert!(masks.insert(mask), "duplicate XInput mask {mask:#x}");
            assert!(
                buttons.insert(button),
                "duplicate gamepad control {button:?}"
            );
        }
    }

    #[test]
    fn hid_batch_preserves_press_then_release_order() {
        let mut device = RawDeviceState {
            device: NativeDevice::new("raw:test".into(), Some(1), Some(2), None),
            usage_page: 0xff00,
            usage: 1,
            preparsed: Vec::new(),
            report_ids: HashSet::new(),
            active_by_report: HashMap::new(),
            ignored: false,
        };

        // Two one-byte reports in one RAWINPUT packet: the same pedal is
        // pressed and then released. Treating the batch as a union emitted only
        // the press and left the dispatcher wedged.
        let edges = decode_report_edges(&mut device, &[0b0000_0001, 0], 1);
        let control = NativeControl::Hid {
            usage_page: 0xff00,
            usage: 1,
            control: 1,
        };
        assert_eq!(edges, vec![(control, true), (control, false)]);
        assert!(device.active_by_report.get(&0).unwrap().is_empty());
    }

    #[test]
    fn vendor_control_identity_uses_report_id_not_batch_position() {
        let mut device = RawDeviceState {
            device: NativeDevice::new("raw:test".into(), Some(1), Some(2), None),
            usage_page: 0xff00,
            usage: 1,
            preparsed: Vec::new(),
            report_ids: HashSet::from([7]),
            active_by_report: HashMap::new(),
            ignored: false,
        };

        let first = decode_report_edges(&mut device, &[7, 0b0000_0100], 2);
        let release = decode_report_edges(&mut device, &[7, 0], 2);
        assert_eq!(first.len(), 1);
        assert_eq!(release, vec![(first[0].0, false)]);
        assert!(first[0].1);
    }
}
