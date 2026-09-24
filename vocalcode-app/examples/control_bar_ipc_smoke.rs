//! Real hidden WebView IPC -> production control handler -> isolated mailbox.
//! Programmatic DOM clicks are not real mouse/focus or audio/injection tests.
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
    use control_bar::Event;
    use overlay::{Phase, Snapshot};
    use std::time::{Duration, Instant};
    use tao::{
        event_loop::{ControlFlow, EventLoopBuilder},
        platform::run_return::EventLoopExtRunReturn,
    };
    use vocalcode_core::{trigger_event_channel, TriggerEvent};
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    struct Case {
        name: &'static str,
        phase: Phase,
        button: &'static str,
        clicks: usize,
        shown: bool,
        page_ready: bool,
        host_ready: bool,
        recording: bool,
        stale: bool,
        stale_view: bool,
        collapsed: bool,
        enabled: bool,
        event: Option<&'static str>,
        panel: Option<&'static str>,
    }
    let base = || Case {
        name: "start",
        phase: Phase::Idle,
        button: "main",
        clicks: 1,
        shown: true,
        page_ready: true,
        host_ready: true,
        recording: false,
        stale: false,
        stale_view: false,
        collapsed: false,
        enabled: true,
        event: Some("start"),
        panel: None,
    };
    let cases = [
        base(),
        Case {
            name: "double start",
            clicks: 2,
            ..base()
        },
        Case {
            name: "stop",
            phase: Phase::Recording,
            recording: true,
            event: Some("stop"),
            ..base()
        },
        Case {
            name: "cancel",
            phase: Phase::Recording,
            recording: true,
            button: "cancel",
            event: Some("cancel"),
            ..base()
        },
        Case {
            name: "stale revision",
            stale: true,
            event: None,
            ..base()
        },
        Case {
            name: "native hidden authority",
            shown: false,
            event: None,
            ..base()
        },
        Case {
            name: "processing disabled",
            phase: Phase::Transcribing,
            event: None,
            ..base()
        },
        Case {
            name: "not ready opens settings",
            page_ready: false,
            host_ready: false,
            event: None,
            panel: Some("behaviour"),
            ..base()
        },
        Case {
            name: "history only",
            button: "history",
            event: None,
            panel: Some("history"),
            ..base()
        },
        Case {
            name: "meetings navigation only",
            button: "meetings",
            event: None,
            panel: Some("meetings"),
            ..base()
        },
        Case {
            name: "rewrite navigation only",
            button: "rewrite",
            event: None,
            panel: Some("rewrite"),
            ..base()
        },
        Case {
            name: "hidden menu cannot navigate during recording",
            phase: Phase::Recording,
            recording: true,
            button: "meetings",
            event: None,
            ..base()
        },
        Case {
            name: "snooze only",
            button: "snooze",
            event: None,
            ..base()
        },
        Case {
            name: "More is presentation only",
            button: "more",
            event: None,
            ..base()
        },
        Case {
            name: "More hidden during recording",
            button: "more",
            phase: Phase::Recording,
            recording: true,
            event: None,
            ..base()
        },
        Case {
            name: "stale ready flag",
            host_ready: false,
            event: None,
            ..base()
        },
        Case {
            name: "preference disabled",
            enabled: false,
            event: None,
            ..base()
        },
        Case {
            name: "stop survives preference off",
            phase: Phase::Recording,
            recording: true,
            enabled: false,
            event: Some("stop"),
            ..base()
        },
        Case {
            name: "old menu click after collapse",
            button: "meetings",
            collapsed: true,
            event: None,
            ..base()
        },
        Case {
            name: "old Start click after collapse",
            collapsed: true,
            event: None,
            ..base()
        },
        Case {
            name: "old Stop click after recording controls collapse",
            phase: Phase::Recording,
            recording: true,
            collapsed: true,
            event: None,
            ..base()
        },
        Case {
            name: "old Cancel click after recording controls collapse",
            phase: Phase::Recording,
            recording: true,
            button: "cancel",
            collapsed: true,
            event: None,
            ..base()
        },
        Case {
            name: "stale recording presentation cannot stop",
            phase: Phase::Recording,
            recording: true,
            stale_view: true,
            event: None,
            ..base()
        },
        Case {
            name: "previous menu generation cannot navigate",
            button: "history",
            stale_view: true,
            event: None,
            ..base()
        },
        Case {
            name: "previous presentation cannot start",
            stale_view: true,
            event: None,
            ..base()
        },
        Case {
            name: "disabled preference cannot navigate",
            button: "settings",
            enabled: false,
            event: None,
            ..base()
        },
    ];
    let profile = std::env::temp_dir().join(format!(
        "vocalcode-control-ipc-{}-{}",
        std::process::id(),
        jiff::Timestamp::now().as_millisecond()
    ));
    std::fs::create_dir(&profile)?;
    let original = unsafe { GetForegroundWindow() };
    let mut loop_ = EventLoopBuilder::<Event>::with_user_event().build();
    let proxy = loop_.create_proxy();
    let mut bar = control_bar::ControlBar::new(&loop_, profile.clone(), move |e| {
        let _ = proxy.send_event(e);
    })?;
    let (tx, rx) = trigger_event_channel();
    let bridge = dictation_control::Bridge::default();
    bridge.connect(tx);
    let (completed_tx, completed_rx) = std::sync::mpsc::channel();
    let (mut ready, mut active_case, mut count, mut failed) = (false, false, 0, false);
    let mut evaluated = false;
    let mut received = Vec::new();
    let mut panels = Vec::new();
    let started = Instant::now();
    let mut check_at = started;
    loop_.run_return(|event, _, flow| {
        *flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(20));
        if count >= cases.len() {
            *flow = ControlFlow::Exit;
            return;
        }
        let case = &cases[count];
        let snapshot = Snapshot {
            phase: case.phase,
            revision: 100 + count as u64,
        };
        if let tao::event::Event::UserEvent(event) = event {
            if matches!(event, Event::Scale(_)) {
                ready = true;
            }
            if let Some(panel) = bar.event(
                event,
                snapshot,
                case.host_ready,
                case.recording,
                case.enabled,
                &bridge,
            ) {
                panels.push(panel);
            }
        }
        while completed_rx.try_recv().is_ok() {
            evaluated = true;
        }
        while let Ok(event) = rx.try_recv() {
            if event != TriggerEvent::Wake {
                failed = true;
                continue;
            }
            if let Some(dispatch) = bridge.take(
                snapshot,
                case.host_ready,
                case.recording,
                case.enabled,
                Instant::now(),
            ) {
                received.push(match dispatch.event {
                    TriggerEvent::HandsFreeStart(_) => "start",
                    TriggerEvent::ForceStop => "stop",
                    TriggerEvent::Cancel => "cancel",
                    _ => "unexpected",
                });
            }
        }
        if active_case && evaluated && Instant::now() >= check_at {
            let expected = case.event.into_iter().collect::<Vec<_>>();
            let expected_panels = case.panel.into_iter().collect::<Vec<_>>();
            let native = bar.inspect_native_contract();
            let focus_unchanged = original == unsafe { GetForegroundWindow() };
            let ok = received == expected
                && panels == expected_panels
                && !bridge.is_busy()
                && native["visible"] == false
                && focus_unchanged;
            let ok = ok && (case.name != "More is presentation only" || native["menu_open"] == true);
            println!(
                "CONTROL_IPC case={} name={} commands={received:?} panels={panels:?} focus_unchanged={focus_unchanged} passed={ok}",
                count, case.name
            );
            failed |= !ok;
            count += 1;
            active_case = false;
            evaluated = false;
            received.clear();
            panels.clear();
        }
        if ready && !active_case && count < cases.len() {
            let case = &cases[count];
            let snapshot = Snapshot {
                phase: case.phase,
                revision: 100 + count as u64,
            };
            let sender = completed_tx.clone();
            if let Err(error) = bar.inspect_hidden_interaction(
                snapshot,
                snapshot.revision - u64::from(case.stale),
                case.page_ready,
                case.shown,
                case.button,
                case.clicks,
                case.stale_view,
                case.collapsed,
                move |_| {
                    let _ = sender.send(());
                },
            ) {
                eprintln!("hidden fixture error: {error}");
                failed = true;
                *flow = ControlFlow::Exit;
                return;
            }
            active_case = true;
            check_at = Instant::now() + Duration::from_millis(180);
        }
        if started.elapsed() > Duration::from_secs(20) {
            failed = true;
            *flow = ControlFlow::Exit;
        }
    });
    println!(
        "CONTROL_IPC_RESULT cases={count} failed={failed}; isolated profile retained: {}",
        profile.display()
    );
    anyhow::ensure!(
        !failed && count == cases.len(),
        "hidden IPC contract failed"
    );
    Ok(())
}

#[cfg(any(not(windows), test, not(debug_assertions)))]
fn main() {}
