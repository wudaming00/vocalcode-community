import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
const html=readFileSync(new URL('../../vocalcode-app/src/control_bar.html',import.meta.url),'utf8');
const code=html.match(/<script>([\s\S]*?)<\/script>/)[1];
function setup(ratio,reducedMotion){
  class El {
    constructor(id){this.id=id;this.textContent='';this.disabled=false;this.hidden=false;this.attrs={};this.dataset={};this.events=new Map();}
    setAttribute(k,v){this.attrs[k]=v;}
    addEventListener(name,fn){this.events.set(name,fn);}
    set innerHTML(value){assert.equal(this.id,'mainIcon');assert.match(value,/^<(?:rect|path) /);this.icon=value;this.writes=(this.writes||0)+1;}
    click(){this.events.get('click')();}
  }
  const elements=new Map(),messages=[],timers=new Map();let timerId=0;const el=id=>{if(!elements.has(id))elements.set(id,new El(id));return elements.get(id);};
  const root=new El('root');
  const ctx={setTimeout:fn=>{timers.set(++timerId,fn);return timerId;},clearTimeout:id=>timers.delete(id),window:{ipc:{postMessage:s=>messages.push(JSON.parse(s))}},navigator:{language:'en-US'},document:{documentElement:root,
    body:{dataset:{},style:{values:{},setProperty(k,v){this.values[k]=v;}}},getElementById:el,querySelectorAll:()=>['peek','main','more','recover','cancel','history','meetings','rewrite','settings','snooze'].map(el),addEventListener(){}}};
  const windowEvents=new Map();ctx.window.addEventListener=(name,fn)=>windowEvents.set(name,fn);
  const media=new Map();
  if(reducedMotion!==undefined)ctx.window.matchMedia=query=>{
    if(!media.has(query))media.set(query,{matches:query.includes('reduced-motion')?reducedMotion:false,events:new Map(),addEventListener(name,fn){this.events.set(name,fn);},removeEventListener(name){this.events.delete(name);}});
    return media.get(query);
  };
  ctx.window.devicePixelRatio=ratio;
  vm.createContext(ctx);vm.runInContext(code,ctx);
  return {ctx,el,root,messages,windowEvents,timers,media,show:s=>ctx.window.vocalcodeControl({phase:'idle',revision:'1',view_revision:'7',ready:true,busy:false,expanded:false,level:0,language:'en',...s})};
}
test('ready and hover never start recording',()=>{
  const f=setup();assert.deepEqual(f.messages,[{type:'ready'}]);f.show();
  f.root.events.get('pointerenter')();f.root.events.get('pointerleave')();
  assert.ok(f.messages.slice(1).every(m=>m.type==='hover'));
});
test('clicks preserve a string generation and suppress double submission',()=>{
  const f=setup();f.show({revision:'9007199254740993',view_revision:'18446744073709551615',expanded:true});f.el('main').click();f.el('main').click();
  assert.deepEqual(f.messages.at(-1),{type:'action',revision:'9007199254740993',view_revision:'18446744073709551615',action:'start'});
  assert.equal(f.messages.length,2);assert.equal(f.el('main').disabled,true);
});
test('recording stop and cancel use distinct non-toggle commands',()=>{
  for(const [button,action] of [['main','stop'],['cancel','cancel']]){
    const f=setup();f.show({phase:'recording',expanded:true,revision:'4'});f.el(button).click();
    assert.equal(f.messages.at(-1).action,action);assert.equal(f.el('history').hidden,true);
  }
});

test('recording and processing use the tiny meter without a Listening banner',()=>{
  for(const phase of ['recording','processing']){
    const f=setup();f.show({phase,expanded:false});
    assert.equal(f.ctx.document.body.dataset.status,'false');
    for(const id of ['label','main','cancel','more','menu'])assert.equal(f.el(id).hidden,true);
    assert.equal(f.el('peek').hidden,false);
    assert.equal(f.el('peek').title,undefined); // No browser tooltip over the tiny meter.
    assert.match(f.el('peek').attrs['aria-label'],phase==='recording'?/Listening/:/Working/);
    f.el('peek').click();assert.deepEqual(f.messages.at(-1),{type:'hover',inside:true});
    assert.equal(f.messages.some(m=>m.type==='action'),false);
  }
});

test('hovered recording offers only stop meter and cancel without a status label',()=>{
  const f=setup();f.show({phase:'recording',expanded:true});
  assert.equal(f.ctx.document.body.dataset.status,'false');
  for(const id of ['main','peek','cancel'])assert.equal(f.el(id).hidden,false);
  for(const id of ['label','more','menu','recover'])assert.equal(f.el(id).hidden,true);
});
test('processing cannot start or pretend to cancel synchronous inference',()=>{
  const f=setup();f.show({phase:'processing',expanded:true});f.el('main').click();
  assert.equal(f.messages.length,1);assert.equal(f.el('main').disabled,true);assert.equal(f.el('cancel').hidden,true);
});
test('not-ready entry opens settings, never starts capture',()=>{
  const f=setup();f.show({ready:false,expanded:true});f.el('main').click();assert.equal(f.messages.at(-1).action,'settings');
});
test('authoritative refresh recovers rejected clicks and errors stay inert',()=>{
  const f=setup();f.show({expanded:true});f.el('main').click();f.show({expanded:true,error:'<img src=x onerror=bad()>',language:'zh'});
  assert.equal(f.el('main').disabled,false);assert.equal(f.el('label').textContent,'<img src=x onerror=bad()>');
  assert.equal(f.el('main').attrs['aria-label'],'开始免按键听写');
});
test('hover is only a slim toolbar; secondary actions require an explicit More click',()=>{
  const f=setup();f.show({edge:'right'});assert.equal(f.ctx.document.body.dataset.edge,'right');
  for(const id of ['main','more','recover','history','meetings','rewrite','settings','snooze','cancel'])assert.equal(f.el(id).hidden,true);
  f.show({expanded:true});
  for(const id of ['peek','main','more'])assert.equal(f.el(id).hidden,false);
  for(const id of ['menu','label','history','meetings','rewrite','settings','snooze'])assert.equal(f.el(id).hidden,true);
  f.el('more').click();assert.deepEqual(f.messages.at(-1),{type:'action',revision:'1',view_revision:'7',action:'more'});
  assert.equal(f.el('menu').hidden,true); // Wait for the native resize acknowledgement.
  f.show({expanded:true,menu:true});for(const id of ['main','more','history','meetings','rewrite','settings','snooze'])assert.equal(f.el(id).hidden,false);
  assert.equal(f.el('more').attrs['aria-expanded'],'true');
});

test('page scale handshake includes accessibility enlargement and deduplicates resize',()=>{
  const f=setup(1.27);assert.deepEqual(f.messages,[{type:'ready'},{type:'scale',ratio_milli:1270}]);
  f.windowEvents.get('resize')();assert.equal(f.messages.length,2);
  f.ctx.window.devicePixelRatio=2.54;f.windowEvents.get('resize')();
  assert.deepEqual(f.messages.at(-1),{type:'scale',ratio_milli:2540});
  for(const invalid of [NaN,Infinity,0,.1,20]){f.ctx.window.devicePixelRatio=invalid;f.windowEvents.get('resize')();}
  assert.equal(f.messages.length,3);
});

test('paint dimensions follow physical HWND pixels including fractional text scaling',()=>{
  const f=setup(1.27);f.show({width_physical:61,height_physical:23});
  const style=f.ctx.document.body.style.values;
  assert.equal(style['--surface-width'],(61/1.27)+'px');
  assert.equal(style['--surface-height'],(23/1.27)+'px');
  f.ctx.window.devicePixelRatio=2.54;f.show({width_physical:122,height_physical:46});
  assert.equal(style['--surface-width'],(61/1.27)+'px');
  for(const width_physical of [undefined,0,-1,NaN,Infinity]){
    f.show({width_physical,height_physical:23});assert.equal(style['--surface-width'],'100%');
  }
});

test('menu morph follows native shape and keeps its actions inert until settled',()=>{
  const f=setup(1.27);f.show({expanded:true,menu:true,moving:true,radius_milli:16600,menu_reveal_milli:350});
  const style=f.ctx.document.body.style.values;
  assert.equal(style['--surface-radius'],'16.6px');assert.equal(style['--menu-reveal'],'0.35');
  for(const id of ['history','meetings','rewrite','settings','snooze']){
    assert.equal(f.el(id).hidden,false);assert.equal(f.el(id).disabled,true);f.el(id).click();
  }
  assert.equal(f.messages.filter(m=>m.type==='action').length,0);
  f.show({expanded:true,menu:true,moving:false,radius_milli:14000,menu_reveal_milli:1000});
  assert.equal(f.el('meetings').disabled,false);assert.equal(style['--menu-reveal'],'1');
});

test('constrained menu keeps scroll position during refresh but resets on reopening',()=>{
  const f=setup();f.show({expanded:true,menu:true});assert.equal(f.el('menu').scrollTop,0);
  f.el('menu').scrollTop=80;f.show({expanded:true,menu:true,level:3});assert.equal(f.el('menu').scrollTop,80);
  f.show({expanded:true,menu:false});f.show({expanded:true,menu:true});assert.equal(f.el('menu').scrollTop,0);
  assert.match(html,/height:min\(164px,calc\(100% - 34px\)\)/);
  assert.match(html,/overflow-y:auto/);
});

test('missing acknowledgements unlock controls without automatic retry or false success',()=>{
  const f=setup();f.show({expanded:true,radius_milli:18000});f.el('main').click();
  assert.equal(f.el('main').disabled,true);assert.equal(f.timers.size,1);
  [...f.timers.values()][0]();assert.equal(f.el('main').disabled,false);assert.equal(f.timers.size,0);
  assert.match(f.el('label').textContent,/unconfirmed/);assert.equal(f.messages.length,2);
  assert.equal(f.ctx.document.body.dataset.notice,'true');
  assert.equal(f.ctx.document.body.style.values['--surface-radius'],'18px');
  assert.match(html,/body\[data-notice=true\]\{border-radius:var\(--surface-radius,14px\)/);
});

test('IPC exceptions and missing bridge do not leave the bar permanently disabled',()=>{
  for(const ipc of [undefined,{postMessage(){throw Error('unavailable');}}]){
    const f=setup();f.show({expanded:true,language:'zh'});f.ctx.window.ipc=ipc;f.el('main').click();
    assert.equal(f.el('main').disabled,false);assert.equal(f.timers.size,0);
    assert.match(f.el('label').textContent,/状态未确认/);
  }
});

test('an old timeout cannot alter a new session or its pending stop request',()=>{
  const f=setup();f.show({expanded:true});f.el('main').click();const old=[...f.timers.values()][0];
  f.show({phase:'recording',expanded:true,revision:'2'});assert.equal(f.timers.size,0);
  f.el('main').click();old();assert.equal(f.el('main').disabled,true);assert.equal(f.timers.size,1);
  f.show({phase:'processing',expanded:true,revision:'3'});assert.equal(f.timers.size,0);
  assert.equal(f.el('main').attrs['aria-label'],'Working…');
});

test('delivery recovery points to history without claiming full failure or replaying text',()=>{
  const f=setup();f.show({expanded:true,recovery:true});
  assert.equal(f.el('label').textContent,'Text kept in history');
  assert.match(f.el('label').title,/some words may already be inserted/);
  assert.equal(f.el('recover').hidden,false);assert.equal(f.el('settings').hidden,true);
  assert.equal(f.el('menu').hidden,true);assert.equal(f.el('more').hidden,true);
  assert.equal(f.el('main').attrs['aria-label'],'Start hands-free dictation');
  f.el('recover').click();assert.equal(f.messages.at(-1).action,'history');
  assert.equal('text' in f.messages.at(-1),false);
});

test('known host errors are localized and take precedence over recovery hints',()=>{
  const f=setup();f.show({expanded:true,recovery:true,language:'zh',error:'The recording state changed. Please try again.'});
  assert.equal(f.el('label').textContent,'录音状态已改变，请重试。');
  assert.equal(f.el('label').title,'录音状态已改变，请重试。');
  assert.equal(f.el('main').disabled,false);assert.equal(f.messages.length,1);
});

test('tiny capsule only opens controls; it never starts recording',()=>{
  const f=setup();f.show();f.el('peek').click();f.el('main').click();
  assert.deepEqual(f.messages,[{type:'ready'},{type:'hover',inside:true}]);
  assert.equal(f.timers.size,0);assert.equal(f.el('main').hidden,true);
});

test('menu has localized labels and navigation sends no text or audio request',()=>{
  for(const id of ['history','meetings','rewrite','settings']){
    const f=setup();f.show({expanded:true,menu:true,language:'zh'});
    assert.ok(f.el(id+'Text').textContent.length>0);f.el(id).click();
    assert.deepEqual(f.messages.at(-1),{type:'action',revision:'1',view_revision:'7',action:id});
  }
});

test('opening meetings or rewrite is navigation only, with no implicit work',()=>{
  const page=readFileSync(new URL('../../vocalcode-app/src/webui.html',import.meta.url),'utf8');
  const handler=page.match(/window\.vocalcodeControlOpen=function\(panel\)\{[\s\S]*?\n  \};/)[0];
  const panels=[],nodes=new Map();
  const node=id=>{if(!nodes.has(id))nodes.set(id,{open:false,focus(){this.focused=true;}});return nodes.get(id);};
  const ctx={window:{},showPanel:(...args)=>panels.push(args),document:{getElementById:node}};
  vm.createContext(ctx);vm.runInContext(handler,ctx);
  ctx.window.vocalcodeControlOpen('rewrite');assert.deepEqual(panels,[['history',false]]);
  assert.equal(node('rewriteDetails').open,true);assert.equal(node('rewriteSource').focused,true);
  ctx.window.vocalcodeControlOpen('meetings');assert.deepEqual(panels.at(-1),['meetings',true]);
  ctx.window.vocalcodeControlOpen('start_meeting');assert.equal(panels.length,2);
});

test('moving idle geometry disables actions but the peek can reopen the menu',()=>{
  const f=setup();f.show({expanded:true,moving:true,opening:true,reveal_milli:400});
  for(const id of ['main','more','history','meetings','rewrite','settings','snooze']){
    assert.equal(f.el(id).disabled,true);f.el(id).click();
  }
  assert.equal(f.messages.length,1);f.el('peek').click();
  assert.deepEqual(f.messages.at(-1),{type:'hover',inside:true});
  f.show({expanded:true,moving:false,reveal_milli:1000});f.el('main').click();
  assert.equal(f.messages.at(-1).action,'start');
});

test('stale More flag never shows navigation during recording or recovery',()=>{
  for(const state of [{phase:'recording'},{phase:'processing'},{recovery:true}]){
    const f=setup();f.show({expanded:true,menu:true,...state});
    for(const id of ['more','history','meetings','rewrite','settings','snooze']){
      assert.equal(f.el(id).hidden,true);f.el(id).click();
    }
    assert.equal(f.messages.length,1);
    assert.equal(f.el('menu').hidden,true);
  }
});

test('collapse clears secondary presentation and reopening stays minimal',()=>{
  const f=setup();f.show({expanded:true,menu:true});assert.equal(f.el('menu').hidden,false);
  f.show();f.show({expanded:true});
  assert.equal(f.el('menu').hidden,true);assert.equal(f.el('more').attrs['aria-expanded'],'false');
  f.el('history').click();assert.equal(f.messages.length,1);
});

test('motion and meter refresh do not recreate the icon or restart its spinner',()=>{
  const f=setup();
  for(let n=0;n<10;n++)f.show({expanded:true,moving:true,reveal_milli:n*100});
  assert.equal(f.el('mainIcon').writes,1);
  for(let n=0;n<10;n++)f.show({expanded:true,phase:'processing',level:n});
  assert.equal(f.el('mainIcon').writes,2);
});

test('recording stop and cancel are not delayed by a stale motion flag',()=>{
  for(const id of ['main','cancel']){
    const f=setup();f.show({expanded:true,moving:true,phase:'recording'});
    assert.equal(f.el(id).disabled,false);f.el(id).click();
    assert.equal(f.messages.at(-1).action,id==='main'?'stop':'cancel');
  }
});

test('native frame cadence is conditional and idle keeps the normal sleep budget',()=>{
  const rust=readFileSync(new URL('../../vocalcode-app/src/webui.rs',import.meta.url),'utf8');
  assert.match(rust,/bar\.next_wake\(Instant::now\(\)\)/);
  assert.match(rust,/\*existing = \(\*existing\)\.min\(deadline\)/);
  assert.match(rust,/ControlFlow::WaitUntil\(Instant::now\(\) \+ Duration::from_millis\(180\)\)/);
  assert.match(html,/prefers-reduced-motion: reduce/);
});

test('reduced motion preference is reported initially and when the OS changes it',()=>{
  const f=setup(1.27,true);
  assert.deepEqual(f.messages.at(-1),{type:'motion',reduced:true});
  const query=f.media.get('(prefers-reduced-motion: reduce)');
  query.matches=false;query.events.get('change')();
  assert.deepEqual(f.messages.at(-1),{type:'motion',reduced:false});
  assert.ok(f.messages.every(m=>m.type!=='action'));
});

test('edge alpha is enabled end-to-end and idle feedback has no repeating animation',()=>{
  const rust=readFileSync(new URL('../../vocalcode-app/src/control_bar.rs',import.meta.url),'utf8');
  assert.equal((rust.match(/\.with_transparent\(true\)/g)||[]).length,2);
  assert.doesNotMatch(rust,/with_background_color/);
  assert.match(html,/html,body\{[^}]*background:transparent/);
  assert.match(html,/body::before\{[^}]*inset:0[^}]*pointer-events:none/);
  assert.match(html,/animation:greet 320ms ease-out both/);
  assert.doesNotMatch(html,/animation:greet[^;}]*infinite/);
  assert.match(html,/@media\(prefers-reduced-motion:reduce\)\{\*\{animation:none!important;transition:none!important/);
  assert.doesNotMatch(html,/<button id="peek"[^>]*title=/);
});
