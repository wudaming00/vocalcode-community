//! Isolated, hidden native WebView check. Fake proposals and fake save replies;
//! no installed app, real dictionary, microphone, text injection or network.
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
        std::env::temp_dir().join(format!("vocalcode-correction-qa-{}", std::process::id()));
    std::fs::create_dir(&profile)?;
    let mut events = EventLoopBuilder::<()>::new().build();
    let window = WindowBuilder::new()
        .with_title("VocalCode isolated correction test")
        .with_visible(false)
        .with_decorations(false)
        .with_inner_size(LogicalSize::new(640., 76.))
        .build(&events)?;
    let (tx, rx) = mpsc::channel::<String>();
    let mut context = wry::WebContext::new(Some(profile));
    let view=wry::WebViewBuilder::new_with_web_context(&mut context)
        .with_initialization_script("addEventListener('error',e=>window.ipc.postMessage(JSON.stringify({type:'qa_error',msg:e.message})));")
        .with_html(include_str!("../src/correction_review.html"))
        .with_ipc_handler(move|r|{let _=tx.send(r.body().to_string());}).build(&window)?;
    let started = Instant::now();
    let mut stage = 0;
    let mut due = Instant::now();
    let mut ready = false;
    let mut layouts = 0;
    let mut saved = false;
    let mut closed = false;
    let mut error = false;
    events.run_return(|_,_,flow| {
        *flow=ControlFlow::WaitUntil(Instant::now()+Duration::from_millis(50));
        while let Ok(message)=rx.try_recv(){
            let value:Value=serde_json::from_str(&message).unwrap_or_default();
            match value["type"].as_str().unwrap_or("") {
                "ready"=>ready=true,
                "qa_error"=>{eprintln!("UI_ERROR {value}");error=true;},
                "qa_layout"=>{println!("UI_LAYOUT {value}");layouts+=1;if value["overflow"]==true {error=true;}},
                "save_dict"=>{
                    saved=value["rules"]==json!([["考虑","考虑"],["in","linkedin"]])&&value["revision"]=="fixture";
                    let reply=json!({"ok":true,"request_id":value["request_id"]});
                    let _=view.evaluate_script(&format!("window.vocalcodeCorrectionSaveResult({reply});"));
                },
                "correction_popup_close"=>closed=true,
                _=>{},
            }
        }
        if ready&&Instant::now()>=due {
            if stage<2 {
                if stage==1 {window.set_inner_size(LogicalSize::new(480.,76.));}
                let payload=json!({"ok":true,"review_only":true,"revision":"fixture","rules":[["考虑","Collie"]],"changes":[{"from":"in","to":"linkedin","previous":null},{"from":"考虑","to":"考虑","previous":["考虑","Collie"]}]});
                let _=view.evaluate_script(&format!("window.vocalcodeCorrectionReview({payload});setTimeout(()=>{{window.ipc.postMessage(JSON.stringify({{type:'qa_layout',width:innerWidth,height:innerHeight,overflow:document.documentElement.scrollWidth>innerWidth+1||[...document.querySelectorAll('input,button')].some(e=>e.getBoundingClientRect().right>innerWidth+1||e.getBoundingClientRect().bottom>innerHeight+1)}}));}},100);"));
            } else if stage==2 {let _=view.evaluate_script("document.getElementById('next').click();document.getElementById('keep').click();");}
            stage+=1;due=Instant::now()+Duration::from_millis(400);
        }
        if (closed&&saved&&layouts==2)||error||started.elapsed()>Duration::from_secs(15){*flow=ControlFlow::Exit;}
    });
    anyhow::ensure!(
        !error && saved && closed && layouts == 2,
        "Correction native smoke failed"
    );
    println!("CORRECTION_NATIVE_OK layouts=2 explicit_save=true acknowledged_close=true");
    // Keep this uniquely created isolated profile; never delete a running WebView's data.
    Ok(())
}
