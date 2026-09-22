//! Isolated native popup QA. No engine, microphone, real meetings or profile.
#[cfg(all(not(test), debug_assertions))]
#[allow(dead_code)]
#[path = "../src/meeting_prompt.rs"]
mod meeting_prompt;
#[cfg(all(not(test), debug_assertions))]
#[allow(dead_code)]
#[path = "../src/meeting_reminder.rs"]
mod meeting_reminder;

#[cfg(all(not(test), debug_assertions))]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};
    use tao::{
        event_loop::{ControlFlow, EventLoopBuilder},
        platform::run_return::EventLoopExtRunReturn,
    };
    use vocalcode_meeting::auto_end::{Action, AutoEnd};
    let profile = std::env::temp_dir().join(format!(
        "vocalcode-auto-end-smoke-{}-{}",
        std::process::id(),
        jiff::Timestamp::now().as_millisecond()
    ));
    std::fs::create_dir(&profile)?;
    let mut event_loop = EventLoopBuilder::<meeting_prompt::Event>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let mut prompt =
        meeting_prompt::MeetingPrompt::new(&event_loop, profile.clone(), move |event| {
            let _ = proxy.send_event(event);
        })?;
    let uia = std::env::args().any(|arg| arg == "--uia-input");
    let (layout_tx, layout_rx) = std::sync::mpsc::channel::<String>();
    let started = Instant::now();
    let mut ready = false;
    let mut stage = 0;
    let mut shown = false;
    let mut awaiting = false;
    let mut due = Instant::now();
    let mut ack = 0;
    let mut actions = 0;
    let mut layouts = 0;
    let mut failed = false;
    let mut policy = AutoEnd::new(5, 100);
    let cases = [
        ("review", Action::Continue, "en"),
        ("snooze", Action::Stop, "en"),
        ("dismiss", Action::Disable, "zh"),
        ("close", Action::Continue, "zh"),
        ("escape", Action::Continue, "en"),
    ];
    event_loop.run_return(|event, _, flow| {
        *flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(30));
        match event {
            tao::event::Event::UserEvent(meeting_prompt::Event::Ready) => {
                prompt.ready();
                ready = true;
            }
            tao::event::Event::UserEvent(meeting_prompt::Event::AutoEnd(id, action)) => {
                failed |= !prompt.accepts_auto_end(id);
                if action == Action::Visible {
                    policy.action(id, action, 300_000);
                    ack += 1;
                } else {
                    failed |= !awaiting || action != cases[stage].1;
                    let stop = policy.action(id, action, 301_000);
                    failed |= stop != (action == Action::Stop);
                    if action != Action::Stop {
                        failed |= policy.notice(301_000).is_some();
                    }
                    prompt.hide();
                    #[cfg(windows)]
                    {
                        failed |= prompt.inspect_native_visibility();
                    }
                    println!("AUTO_END_ACTION id={id} action={action:?} hidden=true");
                    actions += 1;
                    stage += 1;
                    shown = false;
                    awaiting = false;
                    due = Instant::now() + Duration::from_millis(150);
                }
            }
            tao::event::Event::UserEvent(meeting_prompt::Event::Action(..)) => {
                failed = true;
            }
            _ => {}
        }
        while let Ok(layout) = layout_rx.try_recv() {
            let value: serde_json::Value = serde_json::from_str(&layout).unwrap_or_default();
            failed |= value["overflow"] != false;
            println!("AUTO_END_LAYOUT {layout}");
            layouts += 1;
        }
        if ready && stage < cases.len() && Instant::now() >= due {
            if !shown {
                policy = AutoEnd::new(5, 100 + stage as u64);
                for now in (0..=300_000).step_by(1_000) {
                    failed |= policy.tick(now, false, true);
                }
                #[cfg(windows)]
                let previous_focus = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
                prompt.sync_auto_end(&policy.notice(300_000).unwrap(), cases[stage].2);
                #[cfg(windows)]
                {
                    let focus_preserved = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() } == previous_focus;
                    let visible = prompt.inspect_native_visibility();
                    println!("AUTO_END_SHOW stage={stage} visible={visible} focus_preserved={focus_preserved}");
                    failed |= !visible || !focus_preserved;
                }
                shown = true;
                due = Instant::now() + Duration::from_millis(400);
            } else if !awaiting {
                let sender = layout_tx.clone();
                prompt.inspect_layout(move |layout| {
                    let _ = sender.send(layout);
                });
                awaiting = true;
                if uia && cases[stage].0 != "escape" {
                    // Same narrowly-scoped UIA harness as ordinary reminders.
                    println!(
                        "REMINDER_INPUT id={} button={}",
                        100 + stage,
                        cases[stage].0
                    );
                } else {
                    failed |= prompt.exercise_smoke_action(cases[stage].0).is_err();
                }
                due = Instant::now() + Duration::from_secs(8);
            } else {
                failed = true;
                *flow = ControlFlow::Exit;
            }
        }
        if (stage == cases.len() && layouts == cases.len())
            || started.elapsed() > Duration::from_secs(35)
        {
            failed |= stage != cases.len() || layouts != cases.len() || ack != cases.len();
            *flow = ControlFlow::Exit;
        }
    });
    drop(prompt);
    println!("AUTO_END_SMOKE actions={actions} layouts={layouts} ack={ack} failed={failed}; temporary profile retained: {}", profile.display());
    anyhow::ensure!(!failed, "Auto-end native UI smoke failed");
    Ok(())
}
#[cfg(any(test, not(debug_assertions)))]
fn main() {}
