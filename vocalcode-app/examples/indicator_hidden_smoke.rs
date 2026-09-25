//! Diagnose the passive indicator using a hidden, isolated WebView. No audio,
//! user data, global input, clipboard or visible-window operations.
#[cfg(all(windows, not(test), debug_assertions))]
#[allow(dead_code)]
#[path = "../src/notice.rs"]
mod notice;
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
    let directory = std::env::temp_dir().join(format!(
        "vocalcode-indicator-hidden-{}-{}",
        std::process::id(),
        jiff::Timestamp::now().as_millisecond()
    ));
    std::fs::create_dir(&directory)?;
    let before = unsafe { GetForegroundWindow() };
    let mut event_loop = EventLoopBuilder::<()>::with_user_event().build();
    let mut indicator = overlay::Overlay::new(&event_loop, directory.clone())?;
    anyhow::ensure!(
        before == unsafe { GetForegroundWindow() },
        "creation changed focus"
    );
    let mut cases: Vec<(bool, &str, Option<String>, &str)> = vec![
        (false, "recording", None, "en"),
        (false, "transcribing", None, "en"),
        (false, "learning", None, "de"),
        (true, "recording", None, "zh"),
        (true, "transcribing", None, "en"),
        (true, "learning", None, "zh"),
    ];
    // Every notice in every language, in both styles: the estimated window
    // must hold the rendered sentence without ellipsizing it.
    use notice::{Lang, NotReady, Notice};
    let notices = [
        Notice::CopiedToClipboard,
        Notice::NotReady(NotReady::Downloading(100)),
        Notice::NotReady(NotReady::Model),
        Notice::NotReady(NotReady::Microphone),
        Notice::NotReady(NotReady::Busy),
        Notice::HeardNothing,
    ];
    for (lang, code) in [
        (Lang::En, "en"),
        (Lang::Zh, "zh"),
        (Lang::Es, "es"),
        (Lang::Fr, "fr"),
        (Lang::De, "de"),
    ] {
        for (index, notice) in notices.iter().enumerate() {
            cases.push((index % 2 == 1, "notice", Some(notice.text(lang)), code));
        }
    }
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let started = Instant::now();
    let mut next = started + Duration::from_millis(300);
    let mut awaiting = false;
    let mut count = 0;
    let mut failed = false;
    event_loop.run_return(|_, _, flow| {
        *flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(50));
        while let Ok(raw) = rx.try_recv() {
            let raw: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
            let value = raw
                .as_str()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .unwrap_or(raw);
            awaiting = false;
            next = Instant::now() + Duration::from_millis(150);
            if value["ready"] != true {
                continue;
            }
            println!("INDICATOR_HIDDEN case={count} {value}");
            failed |= before != unsafe { GetForegroundWindow() }
                || value["solid"] != true
                || value["native"]["visible"] != false
                || value["clipped"] != false;
            for key in ["no_activate", "click_through", "not_in_taskbar", "rounded"] {
                failed |= value["native"][key] != true;
            }
            if let (Some(width), Some(height), Some(content)) = (
                value["width"].as_f64(),
                value["height"].as_f64(),
                value["content"].as_array(),
            ) {
                for item in content.iter().chain(std::iter::once(&value["pill"])) {
                    failed |= item["x"].as_f64().is_none_or(|n| n < -0.5)
                        || item["y"].as_f64().is_none_or(|n| n < -0.5)
                        || item["right"].as_f64().is_none_or(|n| n > width + 0.5)
                        || item["bottom"].as_f64().is_none_or(|n| n > height + 0.5);
                }
            } else {
                failed = true;
            }
            count += 1;
        }
        if !awaiting && count < cases.len() && Instant::now() >= next {
            let (mini, phase, notice, language) = &cases[count];
            let sender = tx.clone();
            if indicator
                .inspect_hidden_layout(*mini, phase, notice.as_deref(), language, move |value| {
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
        if count == cases.len() || started.elapsed() > Duration::from_secs(60) {
            failed |= count != cases.len();
            *flow = ControlFlow::Exit;
        }
    });
    println!(
        "INDICATOR_HIDDEN_RESULT cases={count} failed={failed}; temporary profile retained: {}",
        directory.display()
    );
    anyhow::ensure!(!failed, "hidden indicator layout/style contract failed");
    Ok(())
}
#[cfg(any(not(windows), test, not(debug_assertions)))]
fn main() {}
