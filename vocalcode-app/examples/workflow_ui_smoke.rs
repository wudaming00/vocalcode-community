//! Native WebView smoke test with isolated temporary profile and fake IPC.
//! No installed app, user data, microphones, accounts, network or file pickers.
use serde_json::{json, Value};
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};
use tao::{
    dpi::LogicalSize,
    event_loop::{ControlFlow, EventLoopBuilder},
    platform::run_return::EventLoopExtRunReturn,
    window::WindowBuilder,
};
fn main() -> anyhow::Result<()> {
    let profile =
        std::env::temp_dir().join(format!("vocalcode-workflow-webview-{}", std::process::id()));
    std::fs::create_dir(&profile)?;
    let mut event_loop = EventLoopBuilder::<()>::new().build();
    let window = WindowBuilder::new()
        .with_title("VocalCode isolated UI test")
        .with_decorations(false)
        .with_visible(false)
        .with_inner_size(LogicalSize::new(860., 680.))
        .build(&event_loop)?;
    let (sender, receiver) = mpsc::channel::<String>();
    let mut context = wry::WebContext::new(Some(profile.clone()));
    let webview=wry::WebViewBuilder::new_with_web_context(&mut context)
        .with_initialization_script("window.addEventListener('error',e=>window.ipc.postMessage(JSON.stringify({type:'qa_error',message:e.message})));window.addEventListener('unhandledrejection',e=>window.ipc.postMessage(JSON.stringify({type:'qa_error',message:String(e.reason)})));")
        .with_html(include_str!("../src/webui.html"))
        .with_ipc_handler(move|request|{let _=sender.send(request.body().to_string());}).build(&window)?;
    let started = Instant::now();
    let mut due = Instant::now();
    let mut stage = 0;
    let mut ready = false;
    let mut reports = 0;
    let mut failed = false;
    event_loop.run_return(|_,_,flow|{
        *flow=ControlFlow::WaitUntil(Instant::now()+Duration::from_millis(100));
        while let Ok(message)=receiver.try_recv(){
            let v:Value=serde_json::from_str(&message).unwrap_or_default();
            let script=match v["type"].as_str().unwrap_or(""){
                "ready"=>{ready=true;let mut cfg=serde_json::to_value(vocalcode_core::Config::default()).unwrap();cfg["os"]=json!("windows");cfg["onboarded"]=json!(true);cfg["version"]=json!("test");format!("window.vocalcodeInit({cfg});")},
                "workflow"=>format!("window.vocalcodeWorkflowResult({});",json!({"id":v["id"],"ok":true,"data":{"revision":"fixture","preferences":{"schema":1,"diagnostics":false,"max_entries":100000,"max_bytes":2147483648u64,"cleanup":"light","profiles":[]}}})),
                "calendar"=>format!("window.vocalcodeCalendarResult({});",json!({"id":v["id"],"ok":true,"data":{"configured":false,"connected":false,"events":[]}})),
                "migration"=>format!("window.vocalcodeMigrationResult({});",json!({"id":v["id"],"ok":true,"data":{"state":{"dictionary":[],"dictionary_revision":"fixture","snippets":[],"snippets_revision":"fixture"}}})),
                "qa_error"=>{eprintln!("UI_ERROR {}",v["message"]);failed=true;String::new()},
                "qa_layout"=>{println!("UI_LAYOUT {v}");reports+=1;if v["overflow"].as_bool()==Some(true){failed=true;}String::new()},
                _=>String::new(),
            };
            if !script.is_empty(){let _=webview.evaluate_script(&script);}
        }
        if ready&&Instant::now()>=due&&stage<10{
            let panel=if stage>=8 {"meetings"} else {["behaviour","history","meetings","migration"][stage%4]};
            if stage==4 {window.set_inner_size(LogicalSize::new(720.,560.));}
            if panel=="history" {
                let items=json!([{"at":1787796747,"text":"We should retry.","recognition":"Um, we should, uh, retry.","filler_removed":2},{"at":1787796748,"text":"这件事情，需要再讨论。","recognition":"这件事情，嗯，需要再讨论。","filler_removed":1}]);
                let _=webview.evaluate_script(&format!("window.vocalcodeHistory({items});"));
            }
            if panel=="meetings" {
                let meeting=json!({"id":"1787796747000-1-1","title":"A long meeting title with Google Meet, Windows audio and multilingual discussion","created_at_ms":1787796747000u64,"started_at_ms":1787796747000u64,"duration_ms":12000,"status":if stage==6 {"processing"}else{"completed"},"stopping":stage==6,"speakers":[],"bookmarks":[],"segments":[{"id":1,"start_ms":1000,"end_ms":3000,"speaker_id":"you","text":"Um, we did not approve 12000 USD.","reading_text":"We did not approve 12000 USD.","filler_removed":1}]});
                let fixture=json!({"ready":true,"active":stage==6||stage==9,"transcribing":stage==6,"recording":meeting,"detail":meeting,"meetings":[meeting],"auto_end":if stage==9 {json!({"id":42,"remaining_seconds":29})} else {Value::Null}});
                let _=webview.evaluate_script(&format!("window.vocalcodeMeetings({fixture});"));
                if stage==6 {let _=webview.evaluate_script("document.querySelector('.meeting-reading').click();");}
            }
            let script=format!(r#"showPanel({panel:?});if({panel:?}==='behaviour')document.querySelector('[data-settings-tab="dictation"]').click();document.querySelectorAll('.panel.on details').forEach(e=>e.open=true);setTimeout(()=>{{const p=document.querySelector('.panel.on');window.ipc.postMessage(JSON.stringify({{type:'qa_layout',panel:{panel:?},width:innerWidth,height:innerHeight,overflow:!p||document.documentElement.scrollWidth>innerWidth+1||p.scrollWidth>p.clientWidth+2}}));}},150);"#);
            let _=webview.evaluate_script(&script);stage+=1;due=Instant::now()+Duration::from_millis(600);
        }
        if reports>=10||started.elapsed()>Duration::from_secs(20){if reports<10{failed=true;}*flow=ControlFlow::Exit;}
    });
    drop(webview);
    drop(context);
    drop(window);
    // The WebView2 helper may still be releasing its isolated profile. Do not
    // turn cleanup into a destructive retry against any broader directory.
    if let Err(error) = std::fs::remove_dir_all(&profile) {
        eprintln!("Temporary test profile retained: {error}");
    }
    anyhow::ensure!(!failed, "Native UI smoke test failed");
    Ok(())
}
