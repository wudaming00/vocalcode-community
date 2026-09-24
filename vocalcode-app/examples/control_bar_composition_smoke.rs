//! Opt-in, brief visible compositor check on an owned white backdrop. Samples
//! only that backdrop; never records, injects input or uses the product profile.
#[cfg(all(windows, not(test), debug_assertions))]
#[allow(dead_code)]
#[path = "../src/control_bar.rs"]
mod control_bar;
#[cfg(all(windows, not(test), debug_assertions))]
#[allow(dead_code)]
#[path = "../src/dictation_control.rs"]
mod dictation_control;
#[cfg(all(windows, not(test), debug_assertions))]
#[allow(dead_code)]
#[path = "../src/overlay.rs"]
mod overlay;

#[cfg(all(windows, not(test), debug_assertions))]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};
    use tao::{
        event_loop::{ControlFlow, EventLoopBuilder},
        platform::run_return::EventLoopExtRunReturn,
    };
    use windows_sys::Win32::{Foundation::*, Graphics::Gdi::*, UI::WindowsAndMessaging::*};
    anyhow::ensure!(
        std::env::args().any(|a| a == "--visible-owned-backdrop"),
        "explicit visual probe flag required"
    );
    let compact = std::env::args().any(|a| a == "--compact");
    let dark = std::env::args().any(|a| a == "--dark");
    let phase = if std::env::args().any(|a| a == "--recording") {
        "recording"
    } else if std::env::args().any(|a| a == "--processing") {
        "processing"
    } else {
        "idle"
    };
    let profile = std::env::temp_dir().join(format!(
        "vocalcode-edge-{}-{}",
        std::process::id(),
        jiff::Timestamp::now().as_millisecond()
    ));
    std::fs::create_dir(&profile)?;
    let mut event_loop = EventLoopBuilder::<control_bar::Event>::with_user_event().build();
    let original = unsafe { GetForegroundWindow() };
    let proxy = event_loop.create_proxy();
    let mut bar = control_bar::ControlBar::new(&event_loop, profile.clone(), move |e| {
        let _ = proxy.send_event(e);
    })?;
    let bridge = dictation_control::Bridge::default();
    let state = overlay::OverlayState::default();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut backdrop = std::ptr::null_mut();
    let mut control = std::ptr::null_mut();
    let mut requested = false;
    let mut sample_at = None;
    let started = Instant::now();
    let mut passed = false;
    let mut sampled = false;
    println!(
        "COMPOSITION foreground_before={original:?} after_create={:?}",
        unsafe { GetForegroundWindow() }
    );
    event_loop.run_return(|event, _, flow| {
        if sampled { *flow = ControlFlow::Exit; return; }
        *flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(50));
        if let tao::event::Event::UserEvent(event) = event {
            let scaled = matches!(event, control_bar::Event::Scale(_));
            bar.event(event, state.snapshot(), true, false, true, &bridge);
            if scaled && !requested {
                requested = true;
                let sender = tx.clone();
                if bar.inspect_hidden_layout(!compact, phase, "bottom", "en", move |_| {let _ = sender.send(());}).is_err() { *flow = ControlFlow::Exit; }
            }
        }
        if rx.try_recv().is_ok() {
            unsafe {
                let monitor = MonitorFromWindow(original, MONITOR_DEFAULTTONEAREST);
                let mut info = MONITORINFO {cbSize:std::mem::size_of::<MONITORINFO>() as u32, ..Default::default()};
                if GetMonitorInfoW(monitor, &mut info) == 0 { *flow = ControlFlow::Exit; return; }
                let x = info.rcWork.left + 40;
                let y = info.rcWork.top + 80;
                let native = bar.inspect_native_contract();
                let width = native["client_width"].as_i64().unwrap_or(0) as i32 + 140;
                let height = native["client_height"].as_i64().unwrap_or(0) as i32 + 110;
                if x + width > info.rcWork.right || y + height > info.rcWork.bottom { *flow = ControlFlow::Exit; return; }
                // SS_WHITERECT = 6, SS_BLACKRECT = 4: owned, not desktop content.
                backdrop = CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST, windows_sys::w!("STATIC"), windows_sys::w!("VocalCode edge test"), WS_POPUP | if dark { 4 } else { 6 }, x, y, width, height, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null());
                if backdrop.is_null() { *flow = ControlFlow::Exit; return; }
                ShowWindow(backdrop, SW_SHOWNOACTIVATE);
                control = bar.inspect_composited_window(x + 70, y + 55);
                println!("COMPOSITION control={control:?} backdrop={backdrop:?} foreground_after_show={:?}", GetForegroundWindow());
            }
            sample_at = Some(Instant::now() + Duration::from_millis(900));
        }
        if sample_at.is_some_and(|at| Instant::now() >= at) {
            unsafe {
                let mut rect = RECT::default(); GetWindowRect(control, &mut rect);
                let screen = GetDC(std::ptr::null_mut());
                // The system's BLACKRECT brush can be dark grey in a theme.
                // Compare against our actual backdrop, not an assumed RGB zero.
                let background = GetPixel(screen, rect.left - 10, rect.top - 10);
                let dc = CreateCompatibleDC(screen);
                let width = rect.right - rect.left;
                let height = rect.bottom - rect.top;
                let bitmap = CreateCompatibleBitmap(screen, width, height);
                let previous = SelectObject(dc, bitmap);
                let captured = BitBlt(dc, 0, 0, width, height, screen, rect.left, rect.top, SRCCOPY) != 0;
                let corner = GetPixel(dc, 0, 0);
                let centre = GetPixel(dc, width / 2, height / 4);
                let mut blended = 0;
                let mut edge_blends = [0;4];
                let corners = [(0,0),(width-1,0),(0,height-1),(width-1,height-1)].map(|(x,y)|GetPixel(dc,x,y));
                for y in 1..(rect.bottom - rect.top) / 2 {
                    for x in 1..(rect.bottom - rect.top) / 2 {
                        let pixel = GetPixel(dc, x, y);
                        let red = pixel & 255;
                        if red > 65 && red < ((background & 255).saturating_sub(5)).min(235) { blended += 1; }
                        for (index,(px,py)) in [(x,y),(width-1-x,y),(x,height-1-y),(width-1-x,height-1-y)].into_iter().enumerate() {
                            let channel=GetPixel(dc,px,py)&255;
                            if channel>65 && channel<((background&255).saturating_sub(5)).min(235) {edge_blends[index]+=1;}
                        }
                    }
                }
                SelectObject(dc, previous); DeleteObject(bitmap); DeleteDC(dc);
                ReleaseDC(std::ptr::null_mut(), screen);
                let unchanged = original == GetForegroundWindow();
                passed = captured && background != 0xffff_ffff && corner == background
                    && (centre & 255) > 10 && (centre & 255) < 65
                    && blended >= 5 && corners.iter().all(|c|*c==background) && edge_blends.iter().all(|n|*n>=3) && unchanged;
                println!("COMPOSITION phase={phase} compact={compact} dark={dark} background={background:#x} corner={corner:#x} centre={centre:#x} antialiased_pixels={blended} quadrant_blends={edge_blends:?} corners={corners:?} foreground_after={:?} focus_unchanged={unchanged} passed={passed}",GetForegroundWindow());
            }
            sampled = true;
            *flow = ControlFlow::Exit;
        }
        if started.elapsed() > Duration::from_secs(15) { *flow = ControlFlow::Exit; }
    });
    unsafe {
        if !control.is_null() {
            ShowWindow(control, SW_HIDE);
        }
        if !backdrop.is_null() {
            DestroyWindow(backdrop);
        }
    }
    anyhow::ensure!(
        passed,
        "owned-backdrop compositing check failed; profile retained at {}",
        profile.display()
    );
    Ok(())
}
#[cfg(any(not(windows), test, not(debug_assertions)))]
fn main() {}
