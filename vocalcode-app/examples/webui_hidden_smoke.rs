//! Real WebView2 responsive-layout QA with a synthetic, read-only host. Never
//! shows the window or invokes audio, models, accounts, clipboard or profiles.
#[cfg(all(windows, not(test), debug_assertions))]
fn main() -> anyhow::Result<()> {
    use serde_json::json;
    use std::time::{Duration, Instant};
    use tao::platform::{run_return::EventLoopExtRunReturn, windows::WindowBuilderExtWindows};
    use tao::{
        event_loop::{ControlFlow, EventLoopBuilder},
        window::WindowBuilder,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    let language = std::env::args().nth(1).unwrap_or_else(|| "en".into());
    anyhow::ensure!(
        ["en", "zh"].contains(&language.as_str()),
        "choose en or zh; each run starts a fresh document"
    );
    let profile = std::env::temp_dir().join(format!(
        "vocalcode-webui-hidden-{}-{}",
        std::process::id(),
        jiff::Timestamp::now().as_millisecond()
    ));
    std::fs::create_dir(&profile)?;
    let before = unsafe { GetForegroundWindow() };
    let mut event_loop = EventLoopBuilder::<()>::with_user_event().build();
    let window = WindowBuilder::new()
        .with_title("VocalCode isolated UI layout QA")
        .with_visible(false)
        .with_focused(false)
        .with_focusable(false)
        .with_skip_taskbar(true)
        .with_decorations(false)
        .build(&event_loop)?;
    let mut context = wry::WebContext::new(Some(profile.clone()));
    let webview = wry::WebViewBuilder::new_with_web_context(&mut context)
        .with_html(include_str!("../src/webui.html"))
        .with_focused(false)
        .with_navigation_handler(|url| url == "about:blank" || url.starts_with("data:text/html;"))
        .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
        // All page commands are ignored. This fixture has no product backend.
        .with_ipc_handler(|_| {})
        .build(&window)?;
    anyhow::ensure!(
        before == unsafe { GetForegroundWindow() },
        "creation changed focus"
    );
    let cases = [
        (900., 600., "history", false),
        (700., 480., "history", false),
        (540., 360., "history", false),
        (900., 600., "behaviour", false),
        (700., 480., "behaviour", false),
        (900., 600., "meetings", false),
        (700., 480., "meetings", false),
        (540., 360., "meetings", false),
        (900., 600., "meetings", true),
        (700., 480., "meetings", true),
        (540., 360., "meetings", true),
        (900., 600., "dictionary", false),
        (900., 600., "about", false),
        (700., 480., "about", false),
        (540., 360., "about", false),
    ];
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let started = Instant::now();
    let mut next = started + Duration::from_millis(500);
    let mut awaiting = false;
    let mut count = 0;
    let mut failed = false;
    event_loop.run_return(|_,_,flow|{
        *flow=ControlFlow::WaitUntil(Instant::now()+Duration::from_millis(50));
        while let Ok(raw)=rx.try_recv(){
            let raw:serde_json::Value=serde_json::from_str(&raw).unwrap_or_default();
            let value=raw.as_str().and_then(|s|serde_json::from_str::<serde_json::Value>(s).ok()).unwrap_or(raw);
            awaiting=false;next=Instant::now()+Duration::from_millis(150);
            if value["ready"]!=true{continue;}
            println!("WEBUI_HIDDEN case={count} {value}");
            failed|=before!=unsafe{GetForegroundWindow()}||window.is_visible()||value["horizontal_overflow"]!=false
                ||value["nested_overflow"].as_array().is_none_or(|v|!v.is_empty())
                ||value["clipped_controls"].as_array().is_none_or(|v|!v.is_empty())||value["language"]!=language
                ||value["stop_reachable"]!=true||value["commerce_ui"]!=false||value["crash_notice_shown"]!=true;
            count+=1;
        }
        if !awaiting&&count<cases.len()&&Instant::now()>=next{
            let (width,height,panel,active)=cases[count];
            window.set_inner_size(tao::dpi::LogicalSize::new(width,height));
            let config=json!({"os":"windows","version":"QA simulation","onboarded":true,"ui_lang":language,"panel":panel,
                "talk":["mouse_x2","key_right_ctrl"],"send":[],"teach":[],"language":"zh","model":"sensevoice","talk_mode":"hold",
                "live_caption":false,"noise_filter":true,"overlay_style":"classic","correction_window_ms":4000,"devices":[],"dict":[],
                "dict_revision":"fixture","desktop_control":true,"desktop_control_edge":"bottom","desktop_control_available":true});
            // No plan or licence fields: the page has no commerce UI to feed.
            // A synthetic crash notice exercises the banner at every size.
            let status=json!({"ready":true,"onboarded":true,"permissions_ok":true,"listening":false,"meeting_active":false,
                "meeting_transcribing":false,"model":"QA simulation","crash_notice":{"at":1758600000,"version":"QA simulation"},
                "history":[{"at":1758600000,"text":"Synthetic QA: do not deploy Collie yet. Keep the budget at 1200 USD.",
                    "recognition":"Um, synthetic QA: do not deploy Collie yet. Keep the budget at 1200 USD.","filler_removed":1}],
                "totals":{"dictations":0,"words":0,"chars":0}});
            let script=format!(r#"(()=>{{
                if(typeof window.vocalcodeInit!=='function')return JSON.stringify({{ready:false}});
                window.vocalcodeInit({config});window.vocalcodeStatus({status});
                // Installed model names and calendar titles can be much wider
                // than an empty select. Never use personal service metadata.
                const model=document.getElementById('rewriteModel');
                model.replaceChildren(new Option('synthetic-provider/'+('long-model-name-'.repeat(10))+':quantized','fixture'));
                document.getElementById('rewriteProviderNotes').textContent='Synthetic provider: '+('verylongversion'.repeat(10));
                document.getElementById('calendarEvent').replaceChildren(new Option('Synthetic event '+('long meeting title '.repeat(10)),'fixture'));
                const meeting={{id:'synthetic-qa',title:'Synthetic meeting with a deliberately long title — no microphone is running',created_at_ms:1758600000000,duration_ms:125000,status:'completed',
                    speakers:[{{id:'a',label:'Synthetic participant with a long name'}},{{id:'b',label:'QA participant'}}],
                    segments:Array.from({{length:24}},(_,i)=>({{id:i,start_ms:i*5000,speaker_id:i%2?'a':'b',text:'Synthetic transcript '+i+': do not deploy Collie yet. Keep the budget at 1200 USD.',reading_text:'Synthetic transcript '+i+': keep the original audio private.'}})),
                    summary:{{overview:[{{at_ms:0,text:'Synthetic summary only; no real conversation is loaded.'}}],decisions:[{{at_ms:5000,text:'Review the draft before sharing.'}}],action_items:[{{source:{{at_ms:10000}},text:'Check the local preview.',owner:'QA participant'}}],open_questions:[]}},
                    bookmarks:[{{at_ms:15000,label:'Synthetic bookmark'}}],warnings:[]}};
                window.vocalcodeMeetings({{ready:true,active:{active},recording:{{title:'Synthetic meeting — no microphone is running',duration_ms:12500,status:'recording'}},meetings:[meeting],detail:meeting}});
                const panel=document.querySelector('.panel.on');
                panel.querySelectorAll('details.card,details.hist-original').forEach(e=>e.open=true);
                const clipped=[];
                for(const b of panel.querySelectorAll('button,input,select,textarea')){{
                    if(b.closest('details:not([open])'))continue;
                    // Reachable scroll content is not clipped UI. Exercise each
                    // control after scrolling it into view before measuring.
                    b.scrollIntoView({{block:'nearest',inline:'nearest'}});
                    const r=b.getBoundingClientRect();if(r.width<1||r.height<1)continue;
                    for(let p=b.parentElement;p&&p!==panel;p=p.parentElement){{
                        if(getComputedStyle(p).overflow!=='hidden')continue;
                        const a=p.getBoundingClientRect();
                        if(r.left<a.left-2||r.right>a.right+2||r.top<a.top-2||r.bottom>a.bottom+2){{clipped.push(b.id||b.textContent);break;}}
                    }}
                }}
                const content=document.querySelector('.content');
                const nested=Array.from(panel.querySelectorAll('.meeting-detail,.meeting-list,.hist-list')).filter(e=>e.clientWidth>0&&e.scrollWidth>e.clientWidth+2).map(e=>({{class:e.className,width:e.clientWidth,scroll:e.scrollWidth}}));
                let stopReachable=true;
                if({active}){{
                    content.scrollTop=content.scrollHeight;
                    const stop=document.getElementById('meetingStop').getBoundingClientRect(),view=content.getBoundingClientRect();
                    stopReachable=stop.height>0&&stop.top>=view.top-2&&stop.bottom<=view.bottom+2;
                    content.scrollTop=0;
                }}
                const commerce=!!document.querySelector('#planBadge,#licBuy,[data-pro-badge],[data-panel="license"]');
                const crashShown=!document.getElementById('crashNotice').hidden;
                return JSON.stringify({{ready:true,panel:panel.dataset.panel,language:document.documentElement.lang,commerce_ui:commerce,crash_notice_shown:crashShown,
                    viewport:[visualViewport.width,visualViewport.height],dpr:devicePixelRatio,
                    horizontal_overflow:content.scrollWidth>content.clientWidth+2,nested_overflow:nested,clipped_controls:clipped,stop_reachable:stopReachable}});
            }})()"#);
            let sender=tx.clone();
            if webview.evaluate_script_with_callback(&script,move|value|{let _=sender.send(value);}).is_err(){failed=true;*flow=ControlFlow::Exit;return;}
            awaiting=true;
        }
        if count==cases.len()||started.elapsed()>Duration::from_secs(30){failed|=count!=cases.len();*flow=ControlFlow::Exit;}
    });
    println!(
        "WEBUI_HIDDEN_RESULT cases={count} failed={failed}; isolated profile retained: {}",
        profile.display()
    );
    anyhow::ensure!(!failed, "responsive layout contract failed");
    Ok(())
}
#[cfg(any(not(windows), test, not(debug_assertions)))]
fn main() {}
