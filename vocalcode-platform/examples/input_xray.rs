//! Two-layer X-ray of the Windows keyboard pathway.
//!
//! Layer 1 (`RAW `): Raw Input for keyboard (0x01/0x06) and mouse (0x01/0x02)
//! collections — data as close to the device as user mode gets, tagged with
//! WHICH device handle produced it. If a key is absent here, the device never
//! sent it (or a kernel-level filter ate it before user mode).
//!
//! Layer 2 (`HOOK`): a bare WH_KEYBOARD_LL that does nothing but print and
//! pass through — no rdev, no locks, no name resolution. If a key is present
//! in RAW but absent here, something between the input stack and the hook
//! chain is filtering it.
//!
//! Run, press the keys under suspicion (both Ctrls, plus a control key like F5
//! that is known to arrive), read the interleaved log. Exits by itself.
//!
//! ```text
//! cargo run -p vocalcode-platform --example input_xray
//! ```

// The body is Windows-only, but an example still has to present a `main` on
// every platform: `cargo test --workspace` builds examples, so a crate-level
// `#![cfg(windows)]` here compiled the whole file away on macOS and failed the
// workspace test run with `main function not found`.
#[cfg(not(windows))]
fn main() {
    eprintln!("input_xray is a Windows-only diagnostic");
}

#[cfg(windows)]
fn main() {
    imp::main();
}

#[cfg(windows)]
mod imp {

    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Input::{
        GetRawInputData, GetRawInputDeviceInfoW, GetRawInputDeviceList, RegisterRawInputDevices,
        RAWINPUT, RAWINPUTDEVICE, RAWINPUTDEVICELIST, RAWINPUTHEADER, RIDEV_INPUTSINK,
        RIDI_DEVICENAME, RID_INPUT, RIM_TYPEHID, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
        RegisterClassW, SetWindowsHookExW, TranslateMessage, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL,
        WM_INPUT, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW,
    };

    const RUN_SECONDS: u64 = 150;

    fn started() -> &'static Instant {
        static T0: OnceLock<Instant> = OnceLock::new();
        T0.get_or_init(Instant::now)
    }

    fn stamp() -> String {
        format!("{:8.3}s", started().elapsed().as_secs_f64())
    }

    fn vk_name(vk: u32) -> String {
        match vk {
            0x11 => "VK_CONTROL(generic!)".into(),
            0xA2 => "LCtrl".into(),
            0xA3 => "RCtrl".into(),
            0xA0 => "LShift".into(),
            0xA1 => "RShift".into(),
            0x10 => "VK_SHIFT(generic!)".into(),
            0xA4 => "LAlt".into(),
            0xA5 => "RAlt".into(),
            0x12 => "VK_MENU(generic!)".into(),
            0x74 => "F5".into(),
            0x7B => "F12".into(),
            0x14 => "CapsLock".into(),
            0x20 => "Space".into(),
            0x0D => "Enter".into(),
            0x1B => "Esc".into(),
            0xE7 => "VK_PACKET".into(),
            0x5B => "LWin".into(),
            0x41..=0x5A => format!("Key{}", (vk as u8) as char),
            _ => format!("vk_{vk:#04x}"),
        }
    }

    // ---------------------------------------------------------------- layer 2

    unsafe extern "system" fn ll_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == 0 {
            let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
            let action = match wparam as u32 {
                WM_KEYDOWN => "DOWN",
                WM_KEYUP => "UP  ",
                WM_SYSKEYDOWN => "SYSDOWN",
                WM_SYSKEYUP => "SYSUP",
                _ => "?",
            };
            println!(
                "[{} HOOK] {action} vk={:#04x}({}) scan={:#04x} ext={} inj={} low_il_inj={}",
                stamp(),
                kb.vkCode,
                vk_name(kb.vkCode),
                kb.scanCode,
                (kb.flags & 0x01 != 0) as u8,
                (kb.flags & 0x10 != 0) as u8,
                (kb.flags & 0x02 != 0) as u8,
            );
        }
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    }

    fn spawn_hook_layer() {
        std::thread::spawn(|| unsafe {
            let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_hook), std::ptr::null_mut(), 0);
            if hook.is_null() {
                println!("[{} HOOK] FAILED to install WH_KEYBOARD_LL", stamp());
                return;
            }
            println!(
                "[{} HOOK] WH_KEYBOARD_LL installed (bare, pass-through)",
                stamp()
            );
            let mut message: MSG = std::mem::zeroed();
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        });
    }

    // ---------------------------------------------------------------- layer 1

    fn device_names() -> &'static Mutex<HashMap<isize, String>> {
        static NAMES: OnceLock<Mutex<HashMap<isize, String>>> = OnceLock::new();
        NAMES.get_or_init(|| Mutex::new(HashMap::new()))
    }

    unsafe fn device_name(handle: isize) -> String {
        if let Some(name) = device_names().lock().unwrap().get(&handle) {
            return name.clone();
        }
        let mut len = 0u32;
        GetRawInputDeviceInfoW(handle as _, RIDI_DEVICENAME, std::ptr::null_mut(), &mut len);
        let mut buf = vec![0u16; len as usize + 1];
        let got = GetRawInputDeviceInfoW(
            handle as _,
            RIDI_DEVICENAME,
            buf.as_mut_ptr().cast(),
            &mut len,
        );
        // GetRawInputDeviceInfoW reports failure as (UINT)-1, which is u32::MAX and
        // therefore passes `> 0`. Slicing by it panicked the probe on the first
        // device that failed to answer — and a panic in a thread that owns a
        // low-level hook takes the whole X-ray down before it reads anything.
        let name = if got > 0 && got != u32::MAX {
            let end = (got as usize).min(buf.len());
            String::from_utf16_lossy(&buf[..end])
                .trim_end_matches('\0')
                .to_string()
        } else {
            format!("handle:{handle:#x}")
        };
        // Compress the interface path to its distinctive middle.
        let short = name
            .split(['#', '\\'])
            .filter(|s| s.starts_with("VID") || s.starts_with("HID") || s.contains("VID_"))
            .collect::<Vec<_>>()
            .join("#");
        let label = if short.is_empty() { name } else { short };
        device_names().lock().unwrap().insert(handle, label.clone());
        label
    }

    unsafe extern "system" fn raw_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_INPUT {
            let mut size = 0u32;
            GetRawInputData(
                lparam as _,
                RID_INPUT,
                std::ptr::null_mut(),
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            );
            let mut buf = vec![0u8; size as usize];
            let got = GetRawInputData(
                lparam as _,
                RID_INPUT,
                buf.as_mut_ptr().cast(),
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            );
            if got != u32::MAX && got != 0 {
                let raw = &*(buf.as_ptr() as *const RAWINPUT);
                let dev = device_name(raw.header.hDevice as isize);
                match raw.header.dwType {
                    t if t == RIM_TYPEKEYBOARD => {
                        let kb = raw.data.keyboard;
                        println!(
                        "[{} RAW ] kbd  {} make={:#04x} flags={:#04x}({}{}) vk={:#04x}({}) msg={:#05x}",
                        stamp(),
                        dev,
                        kb.MakeCode,
                        kb.Flags,
                        if kb.Flags & 0x01 != 0 { "BREAK" } else { "MAKE" },
                        if kb.Flags & 0x02 != 0 { "+E0" } else { "" },
                        kb.VKey,
                        vk_name(kb.VKey as u32),
                        kb.Message,
                    );
                    }
                    t if t == RIM_TYPEMOUSE => {
                        let m = raw.data.mouse;
                        let buttons = m.Anonymous.Anonymous.usButtonFlags;
                        if buttons != 0 {
                            println!(
                                "[{} RAW ] mouse {} buttons={buttons:#06x}{}",
                                stamp(),
                                dev,
                                match buttons {
                                    0x0001 => " (LEFT down)",
                                    0x0002 => " (LEFT up)",
                                    0x0004 => " (RIGHT down)",
                                    0x0040 => " (X1 down)",
                                    0x0100 => " (X2 down)",
                                    _ => "",
                                }
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    unsafe fn list_devices() {
        let mut count = 0u32;
        GetRawInputDeviceList(
            std::ptr::null_mut(),
            &mut count,
            std::mem::size_of::<RAWINPUTDEVICELIST>() as u32,
        );
        let mut list = vec![std::mem::zeroed::<RAWINPUTDEVICELIST>(); count as usize];
        let got = GetRawInputDeviceList(
            list.as_mut_ptr(),
            &mut count,
            std::mem::size_of::<RAWINPUTDEVICELIST>() as u32,
        );
        if got == u32::MAX {
            println!("could not enumerate raw devices");
            return;
        }
        println!("--- raw input device roster ---");
        for d in &list[..got as usize] {
            let kind = match d.dwType {
                t if t == RIM_TYPEKEYBOARD => "KEYBOARD",
                t if t == RIM_TYPEMOUSE => "MOUSE   ",
                t if t == RIM_TYPEHID => "HID     ",
                _ => "?       ",
            };
            println!("  {kind} {}", device_name(d.hDevice as isize));
        }
        println!("--- roster end ---");
    }

    // Called from the crate-level `main` above; `mod imp` items default to
    // private, which on Windows made the whole example fail with E0603.
    pub(super) fn main() {
        let _ = started();
        println!("input_xray: two-layer keyboard pathway monitor, exits after {RUN_SECONDS}s");
        spawn_hook_layer();

        std::thread::spawn(|| unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let class_name: Vec<u16> = "vocalcode-xray\0".encode_utf16().collect();
            let class = WNDCLASSW {
                lpfnWndProc: Some(raw_proc),
                hInstance: instance,
                lpszClassName: class_name.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&class);
            let window = CreateWindowExW(
                0,
                class_name.as_ptr(),
                class_name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                -3isize as HWND, // HWND_MESSAGE
                std::ptr::null_mut(),
                instance,
                std::ptr::null_mut(),
            );
            if window.is_null() {
                println!("[{} RAW ] FAILED to create message window", stamp());
                return;
            }
            list_devices();
            let regs = [
                RAWINPUTDEVICE {
                    usUsagePage: 0x01,
                    usUsage: 0x06,
                    dwFlags: RIDEV_INPUTSINK,
                    hwndTarget: window,
                },
                RAWINPUTDEVICE {
                    usUsagePage: 0x01,
                    usUsage: 0x02,
                    dwFlags: RIDEV_INPUTSINK,
                    hwndTarget: window,
                },
            ];
            if RegisterRawInputDevices(
                regs.as_ptr(),
                regs.len() as u32,
                std::mem::size_of::<RAWINPUTDEVICE>() as u32,
            ) == 0
            {
                println!(
                    "[{} RAW ] FAILED to register keyboard/mouse raw input",
                    stamp()
                );
                return;
            }
            println!(
                "[{} RAW ] raw input registered (keyboard 01/06, mouse 01/02, INPUTSINK)",
                stamp()
            );
            let mut message: MSG = std::mem::zeroed();
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        });

        std::thread::sleep(std::time::Duration::from_secs(RUN_SECONDS));
        println!("input_xray: done");
    }
}
