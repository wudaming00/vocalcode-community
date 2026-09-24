//! Hidden, isolated WebView2 scale/layout soak. No engine, audio, user profile,
//! foreground activation, network navigation or text injection. Debug-only.
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
fn resources() -> serde_json::Value {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::{
            ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX},
            Threading::{
                GetCurrentProcess, GetGuiResources, GetProcessHandleCount, GetProcessTimes,
                GR_GDIOBJECTS, GR_USEROBJECTS,
            },
        },
    };
    let process = unsafe { GetCurrentProcess() };
    let mut memory = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    let mut handles = 0;
    unsafe {
        GetProcessMemoryInfo(
            process,
            (&mut memory as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            memory.cb,
        );
        GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user);
        GetProcessHandleCount(process, &mut handles);
    }
    let ms = |t: FILETIME| {
        ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)) as f64 / 10_000.
    };
    serde_json::json!({"scope":"QA host only, excluding WebView2 child processes","private_bytes":memory.PrivateUsage,
        "working_set_bytes":memory.WorkingSetSize,"cpu_ms":ms(kernel)+ms(user),"handles":handles,
        "gdi":unsafe{GetGuiResources(process, GR_GDIOBJECTS)},"user":unsafe{GetGuiResources(process, GR_USEROBJECTS)}})
}

#[cfg(all(windows, not(test), debug_assertions))]
fn decode(raw: &str) -> serde_json::Value {
    let v: serde_json::Value = serde_json::from_str(raw).unwrap_or_default();
    v.as_str()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(v)
}

#[cfg(all(windows, not(test), debug_assertions))]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::platform::run_return::EventLoopExtRunReturn;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    let args: Vec<_> = std::env::args().skip(1).collect();
    anyhow::ensure!(
        args.len() == 2 && args[0] == "--seconds",
        "usage: control_bar_soak --seconds 60..7200"
    );
    let seconds: u64 = args[1].parse()?;
    anyhow::ensure!((60..=7200).contains(&seconds));
    let profile = std::env::temp_dir().join(format!(
        "vocalcode-capsule-soak-{}-{}",
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
    anyhow::ensure!(
        original == unsafe { GetForegroundWindow() },
        "creating hidden fixture changed focus"
    );
    let bridge = dictation_control::Bridge::default();
    let state = overlay::OverlayState::default();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let cases = [
        (false, "idle", "bottom", "en"),
        (true, "idle", "bottom", "en"),
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
        (true, "processing", "right", "zh"),
        (true, "recovery", "left", "en"),
        (false, "idle", "right", "zh"),
        (true, "idle", "right", "zh"),
        (false, "idle", "left", "en"),
        (true, "idle", "left", "en"),
        (true, "opening-0", "bottom", "en"),
        (true, "opening-32", "bottom", "zh"),
        (true, "opening-80", "right", "en"),
        (true, "opening-144", "left", "zh"),
        (true, "opening-220", "bottom", "en"),
        (true, "menu-opening-0", "bottom", "en"),
        (true, "menu-opening-80", "bottom", "zh"),
        (true, "menu-opening-220", "right", "en"),
        (true, "menu-closing-0", "left", "zh"),
        (true, "menu-closing-64", "bottom", "en"),
        (true, "menu-closing-160", "bottom", "zh"),
        (true, "short-menu", "bottom", "en"),
        (true, "short-menu-end", "bottom", "zh"),
    ];
    let scales = [1., 1.25, 1.5, 2., 2.5, 3.];
    let started = Instant::now();
    let mut next = started;
    let mut report_at = started;
    let mut ready = false;
    let mut stage = 0;
    let mut count = 0usize;
    let mut failures = 0usize;
    let mut peak_latency_ms = 0u128;
    let mut request_at = started;
    let mut previous_scale = usize::MAX;
    let mut completed_rounds = 0;
    println!(
        "SOAK_START pid={} seconds={seconds} profile={} resources={}",
        std::process::id(),
        profile.display(),
        resources()
    );
    event_loop.run_return(|event, _, flow| {
        let now = Instant::now();
        *flow = ControlFlow::WaitUntil(now + Duration::from_millis(50));
        if let tao::event::Event::UserEvent(event) = event {
            if matches!(event, control_bar::Event::Scale(_)) { ready = true; }
            let _ = bar.event(event, state.snapshot(), true, false, true, &bridge);
        }
        while let Ok(raw) = rx.try_recv() {
            let latency = request_at.elapsed().as_millis();
            peak_latency_ms = peak_latency_ms.max(latency);
            let layout = decode(&raw);
            let native = bar.inspect_native_contract();
            let (expanded, phase, edge, _) = cases[count % cases.len()];
            let moving = layout["moving"] == true;
            // innerWidth is integer-rounded; use subpixel painted geometry.
            let width = layout["bodyWidth"].as_f64().unwrap_or(-1.);
            let height = layout["bodyHeight"].as_f64().unwrap_or(-1.);
            let dpr = layout["dpr"].as_f64().unwrap_or(-1.);
            let mut valid = native["visible"] == false && width > 0. && height > 0. && dpr > 0.;
            valid &= (width*dpr-native["client_width"].as_f64().unwrap_or(-999.)).abs()<0.1
                && (height*dpr-native["client_height"].as_f64().unwrap_or(-999.)).abs()<0.1;
            for key in ["no_activate","not_in_taskbar","mouse_no_activate","pointer_no_activate","focus_preserved"] { valid &= native[key] == true; }
            valid &= moving || layout["overflow"] == false;
            if let Some(buttons) = layout["buttons"].as_array() {
                for b in buttons {
                    if phase.starts_with("short-menu") && b["inMenu"]==true { continue; }
                    if moving && b["id"] != "peek" { valid &= b["disabled"] == !phase.starts_with("recording-opening-"); continue; }
                    valid &= b["x"].as_f64().is_some_and(|v| v>=0.) && b["right"].as_f64().is_some_and(|v| v<=width)
                        && b["y"].as_f64().is_some_and(|v| v>=0.) && b["bottom"].as_f64().is_some_and(|v| v<=height)
                        && b["name"].as_str().is_some_and(|v| !v.is_empty());
                }
                if phase.starts_with("short-menu") {
                    valid &= layout["menu"]["top"].as_f64().is_some_and(|v|v>=0.)
                        && layout["menu"]["bottom"].as_f64().is_some_and(|v|v<=height)
                        && layout["menu"]["opacity"]=="1"
                        && layout["menu"]["scrollHeight"].as_f64().unwrap_or(0.)>layout["menu"]["height"].as_f64().unwrap_or(0.);
                    let target=if phase=="short-menu-end" {"snooze"} else {"history"};
                    valid &= buttons.iter().find(|b| b["id"]==target).is_some_and(|b|b["inClip"]==true);
                }
                if !moving && phase == "idle" && expanded { valid &= buttons.iter().filter_map(|b| b["id"].as_str()).eq(["peek","main","more"]); }
                if !moving && phase == "recording" {
                    let expected = if expanded { vec!["peek", "main", "cancel"] } else { vec!["peek"] };
                    valid &= buttons.iter().filter_map(|b|b["id"].as_str()).eq(expected);
                    valid &= if expanded { width < 161. && height < 37. } else { width < 49. && height < 19. };
                }
                if phase == "processing" {
                    valid &= buttons.iter().filter_map(|b|b["id"].as_str()).eq(["peek"]);
                    valid &= width < 49. && height < 19.;
                }
                if !moving && edge == "bottom" && matches!(phase,"idle"|"menu"|"recording"|"processing") {
                    valid &= buttons.iter().find(|b| b["id"]=="peek").is_some_and(|b| ((b["x"].as_f64().unwrap_or(-99.)+b["right"].as_f64().unwrap_or(-99.))/2.-width/2.).abs()<=1.);
                }
            } else { valid = false; }
            if count < cases.len()*scales.len() || !valid {
                println!("SOAK_CASE index={count} valid={valid} latency_ms={latency} zoom={} native={native} layout={layout}",scales[(count/cases.len())%scales.len()]);
            }
            if !valid { failures += 1; }
            count += 1;
            completed_rounds = count/(cases.len()*scales.len());
            stage = 0;
            next = now + Duration::from_millis(100);
        }
        if ready && now >= next {
            let scale_index = (count/cases.len())%scales.len();
            if stage == 0 && previous_scale != scale_index {
                if bar.inspect_hidden_zoom(scales[scale_index]).is_err() { failures += 1; *flow=ControlFlow::Exit; return; }
                previous_scale=scale_index;
                next=now+Duration::from_millis(350);
            } else if stage == 0 {
                let (expanded,phase,edge,language)=cases[count%cases.len()];
                if bar.inspect_hidden_layout(expanded,phase,edge,language,|_|{}).is_err() { failures += 1; *flow=ControlFlow::Exit; return; }
                stage=1;
                next=now+Duration::from_millis(150);
            } else if stage == 1 {
                let sender=tx.clone();
                if bar.inspect_hidden_sample(move |v| {let _=sender.send(v);}).is_err() { failures += 1; *flow=ControlFlow::Exit; return; }
                request_at=now;
                stage=2;
            }
        }
        if now >= report_at {
            println!("SOAK_PROGRESS elapsed_s={} cases={count} rounds={completed_rounds} failures={failures} peak_callback_ms={peak_latency_ms} resources={}",started.elapsed().as_secs(),resources());
            report_at=now+Duration::from_secs(60);
        }
        if (!ready && started.elapsed()>Duration::from_secs(20)) || (stage==2 && request_at.elapsed()>Duration::from_secs(10)) { failures+=1; *flow=ControlFlow::Exit; }
        if started.elapsed()>=Duration::from_secs(seconds) || failures>5 { *flow=ControlFlow::Exit; }
    });
    println!("SOAK_RESULT elapsed_s={} cases={count} rounds={completed_rounds} failures={failures} peak_callback_ms={peak_latency_ms} resources={}",started.elapsed().as_secs(),resources());
    anyhow::ensure!(
        failures == 0 && count >= cases.len() * scales.len(),
        "hidden capsule soak failed or did not cover every case/scale"
    );
    Ok(())
}

#[cfg(any(not(windows), test, not(debug_assertions)))]
fn main() {}
