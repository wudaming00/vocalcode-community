//! Isolated UI-only smoke: no engine, credentials, microphone or user profile.
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
        window::WindowBuilder,
    };
    let profile = std::env::temp_dir().join(format!(
        "vocalcode-reminder-ui-{}-{}",
        std::process::id(),
        jiff::Timestamp::now().as_millisecond()
    ));
    std::fs::create_dir(&profile)?;
    let mut event_loop = EventLoopBuilder::<meeting_prompt::Event>::with_user_event().build();
    let uia_input = std::env::args().any(|arg| arg == "--uia-input");
    let sentinel = WindowBuilder::new()
        .with_title("VocalCode reminder focus test — no recording")
        .with_inner_size(tao::dpi::LogicalSize::new(860., 680.))
        .build(&event_loop)?;
    let main_profile = profile.join("main");
    std::fs::create_dir(&main_profile)?;
    let mut main_context = wry::WebContext::new(Some(main_profile));
    let (main_sender, main_receiver) = std::sync::mpsc::channel::<String>();
    let main_webview = wry::WebViewBuilder::new_with_web_context(&mut main_context)
        .with_html(include_str!("../src/webui.html"))
        .with_ipc_handler(move |request| {
            let _ = main_sender.send(request.body().clone());
        })
        .build(&sentinel)?;
    let proxy = event_loop.create_proxy();
    let mut prompt =
        meeting_prompt::MeetingPrompt::new(&event_loop, profile.clone(), move |event| {
            let _ = proxy.send_event(event);
        })?;
    let started = Instant::now();
    let mut ready = false;
    let mut main_ready = false;
    let mut review_checks = 0;
    let mut gate = meeting_reminder::ReminderGate::default();
    let mut stage = 0;
    let mut due = Instant::now();
    let mut checks = 0;
    let (layout_sender, layout_receiver) = std::sync::mpsc::channel::<String>();
    let mut layouts = 0;
    let mut failed = false;
    let mut action_checks = 0;
    let mut expected_action = None;
    let mut visibility_checks = 0;
    #[cfg(windows)]
    let mut expected_focus: usize = 0;
    event_loop.run_return(|event, _, flow| {
        *flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(100));
        if let tao::event::Event::UserEvent(meeting_prompt::Event::Ready) = event {
            prompt.ready();
            ready = true;
            sentinel.set_focus();
            due = Instant::now() + Duration::from_millis(300);
        }
        if let tao::event::Event::UserEvent(meeting_prompt::Event::Action(id, action)) = event {
            let matches = expected_action.take() == Some((id, action));
            failed |= !matches;
            // Real gate + IPC dispatcher + main-page navigation, not just hide.
            let result = prompt.apply_action(&mut gate, id, action, started.elapsed().as_millis() as u64);
            failed |= result == meeting_reminder::ActionResult::Ignored;
            if let meeting_reminder::ActionResult::Review(candidate) = result {
                failed |= meeting_prompt::open_review(&sentinel, &main_webview, candidate).is_err();
                let _ = main_webview.evaluate_script(r#"setTimeout(()=>window.ipc.postMessage(JSON.stringify({type:'qa_review',panel:document.querySelector('.panel.on')?.dataset.panel,title:document.getElementById('meetingTitle').value})),100);"#);
                #[cfg(windows)] {
                    use tao::platform::windows::WindowExtWindows;
                    let raised = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() } == sentinel.hwnd() as _;
                    println!("REMINDER_REVIEW foreground={raised}");
                    failed |= !raised;
                }
            }
            #[cfg(windows)]
            {
                let hidden = !prompt.inspect_native_visibility();
                println!(
                    "REMINDER_ACTION id={id} action={action:?} matched={matches} hidden={hidden}"
                );
                failed |= !hidden;
                visibility_checks += 1;
            }
            action_checks += 1;
            if uia_input {
                due = Instant::now() + Duration::from_millis(100);
            }
        }
        while let Ok(message) = main_receiver.try_recv() {
            let value: serde_json::Value = serde_json::from_str(&message).unwrap_or_default();
            match value["type"].as_str().unwrap_or("") {
                "ready" => {
                    main_ready = true;
                    let mut config = serde_json::to_value(vocalcode_core::Config::default()).unwrap();
                    config["os"] = serde_json::json!("windows");
                    config["onboarded"] = serde_json::json!(true);
                    config["version"] = serde_json::json!("test");
                    let _ = main_webview.evaluate_script(&format!("window.vocalcodeInit({config});"));
                }
                "qa_review" => {
                    println!("REMINDER_REVIEW_PAGE {value}");
                    failed |= value["panel"] != "meetings" || value["title"] != "Local UI smoke — not a real meeting";
                    review_checks += 1;
                }
                "meeting_start" => {
                    eprintln!("FAIL: Review unexpectedly requested recording");
                    failed = true;
                }
                _ => {}
            }
        }
        while let Ok(layout) = layout_receiver.try_recv() {
            println!("REMINDER_LAYOUT {layout}");
            let value: serde_json::Value = serde_json::from_str(&layout).unwrap_or_default();
            failed |= value["overflow"].as_bool() != Some(false);
            layouts += 1;
        }
        if ready && main_ready && Instant::now() >= due && stage < 16 {
            if expected_action.is_some() {
                // Do not let the next card make a lost/delayed IPC look passed.
                failed = true;
                *flow = ControlFlow::Exit;
                return;
            }
            #[cfg(windows)]
            {
                use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
                if stage % 2 == 0 {
                    expected_focus = unsafe { GetForegroundWindow() } as usize;
                } else if !uia_input {
                    // Other real applications may gain focus while the test is
                    // waiting. Only the reminder activating itself is a failure.
                    // UIA Invoke is explicit input and may activate WebView2;
                    // automatic show is still checked synchronously below.
                    failed |= prompt.inspect_native_foreground();
                }
            }
            if stage % 2 == 0 {
                let language = ["en", "zh", "ja", "ko", "es", "fr", "de", "en"][stage / 2];
                let candidate = meeting_reminder::MeetingCandidate {
                    app_key: "fixture".into(),
                    instance_key: format!("fixture-{stage}"),
                    app_name: "Google Meet".into(),
                    suggested_title: "Local UI smoke — not a real meeting".into(),
                    strength: meeting_reminder::Strength::Confirmed,
                    foreground: true,
                    conference_key: None,
                };
                gate.update(started.elapsed().as_millis() as u64, &[candidate], true, false, &[]);
                prompt.sync(gate.current(), language);
                println!("REMINDER_SHOWN language={language}");
                #[cfg(windows)]
                {
                    let visible = prompt.inspect_native_visibility();
                    println!("REMINDER_VISIBLE visible={visible}");
                    failed |= !visible;
                    visibility_checks += 1;
                    let kept = unsafe {
                        windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow()
                    } as usize
                        == expected_focus;
                    println!("REMINDER_FOCUS preserved_on_show={kept}");
                    failed |= !kept;
                    checks += 1;
                }
            } else {
                let sender = layout_sender.clone();
                prompt.inspect_layout(move |value| {
                    let _ = sender.send(value);
                });
                let (trigger, action) = match stage / 2 {
                    0 => ("close", meeting_reminder::Action::Dismiss),
                    1 => ("dismiss", meeting_reminder::Action::Dismiss),
                    2 => ("snooze", meeting_reminder::Action::Snooze),
                    3 => ("escape", meeting_reminder::Action::Dismiss),
                    4 => ("review", meeting_reminder::Action::Review),
                    7 => ("timeout", meeting_reminder::Action::Dismiss),
                    _ => ("", meeting_reminder::Action::Dismiss),
                };
                if !trigger.is_empty() {
                    expected_action = Some((gate.current().unwrap().id, action));
                    if action == meeting_reminder::Action::Review {
                        // Explicit Review must bring a hidden main window back.
                        sentinel.set_visible(false);
                    }
                    if uia_input && matches!(trigger, "close" | "dismiss" | "snooze" | "review") {
                        println!("REMINDER_INPUT id={} button={trigger}", gate.current().unwrap().id);
                    } else {
                        failed |= prompt.exercise_smoke_action(trigger).is_err();
                    }
                } else {
                    if stage / 2 == 5 {
                        // A document-ready reset must not lose a visible HWND.
                        prompt.ready();
                    }
                    prompt.sync(None, "en");
                    gate.pause();
                    prompt.hide(); // repeated hide is safe and remains hidden
                    #[cfg(windows)]
                    {
                        let hidden = !prompt.inspect_native_visibility();
                        println!("REMINDER_HIDDEN reset={} hidden={hidden}", stage / 2 == 5);
                        failed |= !hidden;
                        visibility_checks += 1;
                    }
                }
            }
            stage += 1;
            due = Instant::now() + if uia_input && expected_action.is_some() && stage < 16 {
                Duration::from_secs(8)
            } else {
                Duration::from_millis(700)
            };
        }
        if (stage >= 16 && layouts >= 8 && action_checks >= 6 && review_checks == 1)
            || started.elapsed() > Duration::from_secs(40)
        {
            failed |= stage < 16 || layouts < 8 || action_checks < 6 || review_checks != 1;
            *flow = ControlFlow::Exit;
        }
    });
    drop(prompt);
    drop(main_webview);
    drop(main_context);
    drop(sentinel);
    println!(
        "REMINDER_SMOKE focus={checks} layouts={layouts} actions={action_checks} visibility={visibility_checks} failed={failed}; temporary profile retained: {}",
        profile.display()
    );
    anyhow::ensure!(!failed, "Meeting reminder native UI smoke failed");
    Ok(())
}
#[cfg(any(test, not(debug_assertions)))]
fn main() {}
