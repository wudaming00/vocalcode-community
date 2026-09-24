//! Native WebView2 layout/style probe that never shows a window, starts audio,
//! changes global focus, injects text, or accesses the user's product profile.
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
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::platform::run_return::EventLoopExtRunReturn;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    let profile = std::env::temp_dir().join(format!(
        "vocalcode-control-hidden-{}-{}",
        std::process::id(),
        jiff::Timestamp::now().as_millisecond()
    ));
    std::fs::create_dir(&profile)?;
    let original = unsafe { GetForegroundWindow() };
    let mut event_loop = EventLoopBuilder::<control_bar::Event>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let mut bar = control_bar::ControlBar::new(&event_loop, profile.clone(), move |event| {
        let _ = proxy.send_event(event);
    })?;
    let created_without_focus = original == unsafe { GetForegroundWindow() };
    let bridge = dictation_control::Bridge::default();
    let state = overlay::OverlayState::default();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut ready = false;
    let mut awaiting = false;
    let mut results = 0;
    let mut failed = !created_without_focus;
    let cases = [
        (false, "idle", "bottom", "en"),
        (true, "idle", "bottom", "en"),
        (true, "menu", "bottom", "en"),
        (true, "menu", "bottom", "zh"),
        (true, "menu", "left", "en"),
        (true, "menu", "right", "zh"),
        (true, "recording", "bottom", "en"),
        (false, "recording", "bottom", "en"),
        (false, "recording", "right", "zh"),
        (true, "recording", "right", "zh"),
        (false, "processing", "left", "zh"),
        (true, "recording-opening-32", "bottom", "zh"),
        (true, "recording-opening-220", "right", "en"),
        (true, "processing", "bottom", "en"),
        (false, "idle", "right", "zh"),
        (true, "idle", "right", "zh"),
        (true, "recording", "left", "zh"),
        (true, "recovery", "bottom", "zh"),
        (true, "recovery", "bottom", "en"),
        (true, "opening-0", "bottom", "en"),
        (true, "opening-32", "bottom", "en"),
        (true, "opening-80", "right", "zh"),
        (true, "opening-144", "left", "zh"),
        (true, "opening-220", "bottom", "en"),
        (true, "short-menu", "bottom", "en"),
        (true, "short-menu-end", "bottom", "zh"),
    ];
    let started = Instant::now();
    let mut next = Instant::now();
    event_loop.run_return(|event, _, flow| {
        *flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(100));
        if let tao::event::Event::UserEvent(event) = event {
            if matches!(event, control_bar::Event::Scale(_)) {
                ready = true;
            }
            let _ = bar.event(event, state.snapshot(), true, false, true, &bridge);
        }
        while let Ok(raw) = rx.try_recv() {
            let value: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
            let layout = if let Some(text) = value.as_str() {
                serde_json::from_str::<serde_json::Value>(text).unwrap_or_default()
            } else {
                value
            };
            let native = bar.inspect_native_contract();
            println!("CONTROL_HIDDEN case={results} native={native} layout={layout}");
            let moving = layout["moving"] == true;
            // During the reveal, the fixed-size menu is intentionally clipped
            // by the native HWND. Only the anchored peek may be interactive.
            failed |= (!moving && layout["overflow"] != false) || native["visible"] != false;
            for key in [
                "no_activate",
                "not_in_taskbar",
                "mouse_no_activate",
                "pointer_no_activate",
                "focus_preserved",
            ] {
                failed |= native[key] != true;
            }
            if let (Some(width), Some(height), Some(buttons)) = (
                layout["bodyWidth"].as_f64(),
                layout["bodyHeight"].as_f64(),
                layout["buttons"].as_array(),
            ) {
                failed |= buttons.iter().any(|b| {
                    if cases[results].1.starts_with("short-menu") && b["inMenu"] == true {
                        return false;
                    }
                    if moving && b["id"] != "peek" {
                        return b["disabled"]
                            != !cases[results].1.starts_with("recording-opening-");
                    }
                    b["x"].as_f64().is_none_or(|x| x < 0.)
                        || b["right"].as_f64().is_none_or(|x| x > width)
                        || b["y"].as_f64().is_none_or(|y| y < 0.)
                        || b["bottom"].as_f64().is_none_or(|y| y > height)
                });
                if cases[results].1.starts_with("short-menu") {
                    failed |= layout["menu"]["top"].as_f64().is_none_or(|v| v < 0.)
                        || layout["menu"]["bottom"].as_f64().is_none_or(|v| v > height)
                        || layout["menu"]["opacity"] != "1"
                        || layout["menu"]["scrollHeight"].as_f64().unwrap_or(0.)
                            <= layout["menu"]["height"].as_f64().unwrap_or(0.);
                    let target = if cases[results].1 == "short-menu-end" {
                        "snooze"
                    } else {
                        "history"
                    };
                    failed |= buttons
                        .iter()
                        .find(|b| b["id"] == target)
                        .is_none_or(|b| b["inClip"] != true);
                }
                if !moving && cases[results].1 == "idle" && cases[results].0 {
                    let ids: Vec<_> = buttons.iter().filter_map(|b| b["id"].as_str()).collect();
                    failed |= ids != ["peek", "main", "more"];
                }
                if cases[results].2 == "bottom"
                    && matches!(
                        cases[results].1,
                        "idle" | "menu" | "recording" | "processing"
                    )
                {
                    if let Some(peek) = buttons.iter().find(|b| b["id"] == "peek") {
                        let centre = (peek["x"].as_f64().unwrap_or(-999.)
                            + peek["right"].as_f64().unwrap_or(-999.))
                            / 2.;
                        failed |= (centre - width / 2.).abs() > 1.;
                    } else {
                        failed = true;
                    }
                }
            } else {
                failed = true;
            }
            results += 1;
            awaiting = false;
            next = Instant::now() + Duration::from_millis(200);
        }
        if ready && !awaiting && results < cases.len() && Instant::now() >= next {
            let (expanded, phase, edge, lang) = cases[results];
            let sender = tx.clone();
            if bar
                .inspect_hidden_layout(expanded, phase, edge, lang, move |value| {
                    let _ = sender.send(value);
                })
                .is_err()
            {
                failed = true;
                *flow = ControlFlow::Exit;
                return;
            }
            awaiting = true;
        }
        if results == cases.len() || started.elapsed() > Duration::from_secs(20) {
            failed |= results != cases.len();
            *flow = ControlFlow::Exit;
        }
    });
    println!("CONTROL_HIDDEN_RESULT created_without_focus={created_without_focus} cases={results} failed={failed}; temporary WebView profile retained: {}",profile.display());
    anyhow::ensure!(!failed, "hidden control bar contract failed");
    Ok(())
}
#[cfg(any(not(windows), test, not(debug_assertions)))]
fn main() {}
