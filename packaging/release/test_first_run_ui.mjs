import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
const html=readFileSync(new URL('../../vocalcode-app/src/webui.html',import.meta.url),'utf8');
const between=(from,to)=>{const a=html.indexOf(from),b=html.indexOf(to,a);assert.ok(a>=0&&b>a,from);return html.slice(a,b);};
const firstRun=between('  // FIRST RUN\n','  // END FIRST RUN');
const pageTalk=between('  const pageTalkHeld=new Set();','  // Escape is the one universal way out.');
const capture=between('  function capCells(which)','  document.querySelectorAll(".setbtn[data-for]")');

class Element{
  constructor(id,doc){this.id=id;this.doc=doc;this.hidden=false;this.disabled=false;this.textContent='';this.value='';this.style={};this.dataset={};this.attrs={};this.children=[];this.focusCount=0;this.tagName='DIV';
    const classes=new Set();this.classList={add:c=>classes.add(c),remove:c=>classes.delete(c),contains:c=>classes.has(c),toggle:(c,on)=>{if(on===undefined)on=!classes.has(c);if(on)classes.add(c);else classes.delete(c);return on;}};}
  setAttribute(k,v){this.attrs[k]=String(v);}getAttribute(k){return this.attrs[k];}
  focus(){this.focusCount++;this.doc.activeElement=this;}
  appendChild(child){this.children.push(child);return child;}
}
function page({talk=['mouse_x2','key:ControlRight'],os='windows',history=[]}={}){
  const doc={nodes:new Map(),activeElement:null,body:null};
  doc.body=new Element('body',doc);doc.activeElement=doc.body;
  const node=id=>{if(!doc.nodes.has(id))doc.nodes.set(id,new Element(id,doc));return doc.nodes.get(id);};
  for(const id of ['frLanguageStep','frKeyStep','frTryStep'])node(id).hidden=id!=='frLanguageStep';
  node('frTryBox').tagName='TEXTAREA';
  const app=new Element('app',doc),backs=[new Element('back2',doc),new Element('back3',doc)];
  const timers=[],calls={save:[],tours:0,panelFocus:0};
  const ctx={
    document:{getElementById:node,createElement:tag=>{const el=new Element(null,doc);el.tagName=tag.toUpperCase();return el;},querySelector:s=>s==='.app'?app:null,querySelectorAll:s=>s==='[data-fr-back]'?backs:[],get activeElement(){return doc.activeElement;},get body(){return doc.body;}},
    cfg:{talk,talk_mode:'hold',onboarded:false,language:'auto'},OS:os,hist:history,tourIndex:-1,
    t:s=>s,labelFor:c=>({mouse_x2:'Mouse fwd (X2)','key:ControlRight':'Right Ctrl','key:F8':'F8'}[c]||c),
    setTimeout:fn=>{timers.push(fn);return timers.length;},
    setTextIfChanged:(el,v)=>{el.textContent=String(v);},
    selectLanguage(code){ctx.cfg.language=code;return true;},
    save:immediate=>calls.save.push(immediate),renderLangs(){},showLanguageApplyNote(){},
    startTour(){calls.tours++;},focusActivePanelHeading(){calls.panelFocus++;},
  };
  ctx.bindList=which=>ctx.cfg[which];
  vm.createContext(ctx);vm.runInContext(firstRun,ctx);
  const visible=()=>['frLanguageStep','frKeyStep','frTryStep'].filter(id=>!node(id).hidden);
  const open=()=>node('firstrun').classList.contains('show');
  const flush=()=>{while(timers.length)timers.shift()();};
  return {ctx,node,app,backs,calls,visible,open,flush,
    status(s){ctx.renderFirstRunTry(s);},
    dictated(text,at=100){ctx.hist=[{at,text},...ctx.hist];}};
}

test('a fresh install opens on the language step, and choosing one moves on to the talk key',()=>{
  const f=page();f.ctx.setOnboarding(true);
  assert.ok(f.open());assert.deepEqual(f.visible(),['frLanguageStep']);assert.equal(f.app.inert,true);
  f.ctx.pickLang('en');
  assert.deepEqual(f.calls.save,[true],'the language is saved through the acknowledged transaction');
  assert.equal(f.ctx.cfg.onboarded,true);
  assert.deepEqual(f.visible(),['frKeyStep']);
  assert.equal(f.node('firstrun').getAttribute('aria-labelledby'),'frKeyTitle');
  assert.equal(f.calls.tours,0,'the tour waits until first run is over');
});

test('host status cannot close or restart the later steps while its save catches up',()=>{
  const f=page();f.ctx.setOnboarding(true);f.ctx.pickLang('en');
  for(const on of [true,false,true,false]){f.ctx.setOnboarding(on);assert.deepEqual(f.visible(),['frKeyStep']);assert.ok(f.open());}
  // Back to the language step: status saying "onboarded" must not close it either.
  f.backs[0].onclick();assert.deepEqual(f.visible(),['frLanguageStep']);
  f.ctx.setOnboarding(false);assert.ok(f.open());
  f.ctx.pickLang('zh');assert.deepEqual(f.visible(),['frKeyStep']);
});

test('a rejected language save reopens the language step',()=>{
  const f=page();f.ctx.setOnboarding(true);f.ctx.pickLang('en');f.node('frKeyNext').onclick();
  // What vocalcodeConfigResult does when the host refuses the language.
  vm.runInContext('firstRunAhead=false; onboardingStep=0; setOnboarding(true);',f.ctx);
  assert.deepEqual(f.visible(),['frLanguageStep']);assert.ok(f.open());
});

test('the Right Ctrl warning shows only while it is the only keyboard key that talks',()=>{
  const cases=[
    [['mouse_x2','key:ControlRight'],'windows',false],
    [['key:ControlRight'],'windows',false],
    [['mouse_x2','key:ControlRight','key:F8'],'windows',true],
    [['mouse_x2'],'windows',true],
    [['mouse_x2','key:F8'],'windows',true],
    // On a Mac a Right Ctrl binding was made by pressing one.
    [['key:ControlRight'],'macos',true],
  ];
  for(const [talk,os,hidden] of cases){
    const f=page({talk,os});f.ctx.setOnboarding(true);f.ctx.pickLang('en');
    assert.equal(f.node('frTalkWarn').hidden,hidden,JSON.stringify(talk)+' '+os);
  }
  const f=page();f.ctx.setOnboarding(true);f.ctx.pickLang('en');
  assert.equal(f.node('frTalkWarn').hidden,false);
  f.ctx.cfg.talk=[...f.ctx.cfg.talk,'key:F8'];f.ctx.renderFirstRunKeyWarning();
  assert.equal(f.node('frTalkWarn').hidden,true,'adding a key through the capture clears it');
});

test('Try it shows the download, then takes only a new dictation as success',()=>{
  const f=page({history:[{at:1,text:'dictated before first run'}]});
  f.ctx.setOnboarding(true);f.ctx.pickLang('en');
  f.status({ready:false,permissions_ok:true,download:{label:'Parakeet TDT v3',pct:42.4,done:270,total:640}});
  f.node('frKeyNext').onclick();f.flush();
  assert.deepEqual(f.visible(),['frTryStep']);
  assert.equal(f.node('frTryDl').hidden,false);
  assert.equal(f.node('frTryDlLabel').textContent,'Downloading Parakeet TDT v3 …');
  assert.equal(f.node('frTryDlPct').textContent,'42%');
  assert.equal(f.node('frTryDlBar').style.width,'42.4%');
  assert.equal(f.node('frTryBox').disabled,true);assert.equal(f.node('frTryKeys').hidden,true);
  assert.equal(f.node('frTryDone').disabled,true,'finishing waits for a dictation that worked');
  assert.match(f.node('frTryCopy').textContent,/still getting ready/);

  f.status({ready:true,permissions_ok:true});
  assert.equal(f.node('frTryDl').hidden,true);
  assert.equal(f.node('frTryBox').disabled,false);
  assert.equal(f.ctx.document.activeElement,f.node('frTryBox'),'the cursor is put in the box once it can be used');
  assert.match(f.node('frTryCopy').textContent,/hold your talk key/);
  assert.equal(f.node('frTryKeys').hidden,false);
  assert.deepEqual(f.node('frTryKeys').children.map(c=>c.textContent),['Mouse fwd (X2)','Right Ctrl']);
  assert.equal(f.node('frTryDone').disabled,true,'History from before this step is not a success');

  f.dictated('   ');f.status({ready:true,permissions_ok:true});
  assert.equal(f.node('frTryDone').disabled,true,'an empty result is not a success');
  f.dictated('Hello from my own voice');f.status({ready:true,permissions_ok:true});
  assert.equal(f.node('frTryHeard').hidden,false);
  assert.equal(f.node('frTryHeard').textContent,'It works. VocalCode heard: “Hello from my own voice”');
  assert.equal(f.node('frTryDone').disabled,false);

  f.node('frTryDone').onclick();
  assert.equal(f.open(),false);assert.equal(f.app.inert,false);assert.equal(f.calls.tours,1);
  f.ctx.setOnboarding(false);assert.equal(f.open(),false);
});

test('Skip is always available, even while the model downloads',()=>{
  const f=page();f.ctx.setOnboarding(true);f.ctx.pickLang('en');f.node('frKeyNext').onclick();
  f.status({ready:false,permissions_ok:true,download:null});
  assert.match(f.node('frTryDlLabel').textContent,/Preparing the speech model/);
  f.node('frTryDone').onclick();assert.ok(f.open(),'Finish does nothing before a success');
  f.node('frTrySkip').onclick();
  assert.equal(f.open(),false);assert.equal(f.calls.tours,1);
});

test('missing macOS permissions are named instead of a box that cannot work',()=>{
  const f=page({os:'macos'});f.ctx.setOnboarding(true);f.ctx.pickLang('en');f.node('frKeyNext').onclick();
  f.status({ready:true,permissions_ok:false});
  assert.match(f.node('frTryCopy').textContent,/needs permission/);
  assert.equal(f.node('frTryBox').disabled,true);assert.equal(f.node('frTryDl').hidden,true);
});

test('toggle mode is described as press, speak, press again',()=>{
  const f=page();f.ctx.cfg.talk_mode='toggle';f.ctx.setOnboarding(true);f.ctx.pickLang('en');f.node('frKeyNext').onclick();
  f.status({ready:true,permissions_ok:true});
  assert.match(f.node('frTryCopy').textContent,/press it again/);
});

test('status pushes for the step on screen do not pull focus back to its title',()=>{
  const f=page();f.ctx.setOnboarding(true);f.flush();
  const title=f.node('firstRunTitle');assert.equal(title.focusCount,1);
  for(let i=0;i<5;i++){f.ctx.setOnboarding(true);f.flush();}
  assert.equal(title.focusCount,1);
});

test('first run no longer points at an element that does not exist',()=>{
  assert.ok(!html.includes('frTalkHint'));
  for(const id of ['frKeyStep','frTalkCap','frTalkWarn','frKeyNext','frTryStep','frTryBox','frTryCopy','frTryDl','frTryDlLabel','frTryDlPct','frTryDlBar','frTryHeard','frTrySkip','frTryDone','frKeyTitle','frTryTitle']){
    assert.ok(html.includes(`id="${id}"`),id);
  }
  const tour=between('var TOUR_STEPS=[','];');
  assert.ok((tour.match(/\{selector:/g)||[]).length<=2,tour);
});

function talkPage({focused='TEXTAREA',talk=['mouse_x2','key:ControlRight']}={}){
  const listeners={keydown:[],keyup:[],blur:[]},sent=[];
  const active={tagName:focused,type:'text',disabled:false,readOnly:false,isContentEditable:false};
  const ctx={
    window:{addEventListener:(type,fn)=>listeners[type].push(fn)},
    document:{activeElement:active},send:m=>sent.push(JSON.parse(JSON.stringify(m))),bindList:w=>w==='talk'?talk:[],pageCaptureWhich:null,
  };
  vm.createContext(ctx);vm.runInContext(pageTalk,ctx);
  const fire=(type,code,extra={})=>{const e={code,repeat:false,prevented:false,preventDefault(){this.prevented=true;},...extra};listeners[type].forEach(fn=>fn(e));return e;};
  return {ctx,sent,active,fire};
}

test('the talk key held in a text field of this window is forwarded, once per edge',()=>{
  const f=talkPage();
  const down=f.fire('keydown','ControlRight');
  assert.ok(down.prevented);assert.deepEqual(f.sent,[{type:'page_talk',code:'ControlRight',pressed:true}]);
  const repeat=f.fire('keydown','ControlRight',{repeat:true});
  assert.ok(repeat.prevented,'auto-repeat must not type into the field');assert.equal(f.sent.length,1);
  f.fire('keyup','ControlRight');
  assert.deepEqual(f.sent.at(-1),{type:'page_talk',code:'ControlRight',pressed:false});
  f.fire('keyup','ControlRight');assert.equal(f.sent.length,2,'a keyup with no forwarded press is not sent');
});

test('the page forwards nothing without a focused text field, a bound key, or during capture',()=>{
  const button=talkPage({focused:'BUTTON'});button.fire('keydown','ControlRight');button.fire('keyup','ControlRight');
  assert.deepEqual(button.sent,[],'nowhere for the words to go');
  const unbound=talkPage();unbound.fire('keydown','ControlLeft');unbound.fire('keydown','KeyA');
  assert.deepEqual(unbound.sent,[]);
  const disabled=talkPage();disabled.active.disabled=true;disabled.fire('keydown','ControlRight');
  assert.deepEqual(disabled.sent,[]);
  const capturing=talkPage();capturing.ctx.pageCaptureWhich='talk';capturing.fire('keydown','ControlRight');
  assert.deepEqual(capturing.sent,[],'the binding prompt owns the key');
  const input=talkPage({focused:'INPUT'});input.fire('keydown','ControlRight');
  assert.equal(input.sent.length,1);
});

test('losing window focus mid-hold releases what the page started',()=>{
  const f=talkPage();f.fire('keydown','ControlRight');
  f.fire('blur');assert.deepEqual(f.sent.at(-1),{type:'page_talk',code:'ControlRight',pressed:false});
  f.fire('keyup','ControlRight');assert.equal(f.sent.length,2);
});

test('both talk rows, Shortcuts and first run, show every capture prompt and binding',()=>{
  // Setting textContent clears the children, as in the DOM.
  const cells=[{},{}].map(()=>({text:'',children:[],cls:new Set(),classList:null,appendChild(c){this.children.push(c);return c;},
    set textContent(v){this.text=String(v);this.children=[];},get textContent(){return this.text;}}));
  for(const cell of cells)cell.classList={add:c=>cell.cls.add(c),remove:c=>cell.cls.delete(c)};
  const timers=[];let refreshed=0,warned=0;
  const ctx={
    document:{querySelectorAll:s=>s==='[data-cap="talk"]'?cells:[],createElement:()=>({children:[],appendChild(c){this.children.push(c);return c;},setAttribute(){}}),createTextNode:text=>({text})},
    setTimeout:fn=>{timers.push(fn);return timers.length;},clearTimeout(){},t:s=>s,labelFor:c=>c,
    bindList:()=>['mouse_x2','key:ControlRight'],refreshHomeKey(){refreshed++;},renderFirstRunKeyWarning(){warned++;},
  };
  vm.createContext(ctx);vm.runInContext(capture,ctx);
  ctx.capPrompt('talk','Press a key or device button…');
  for(const cell of cells){assert.equal(cell.textContent,'Press a key or device button…');assert.ok(cell.cls.has('capturing'));}
  ctx.showCap('talk');
  for(const cell of cells){assert.equal(cell.children.length,2);assert.ok(!cell.cls.has('capturing'));}
  assert.equal(refreshed,1);assert.equal(warned,1);
  ctx.capNote('talk','Already bound here; no need to add it again.');
  for(const cell of cells)assert.equal(cell.textContent,'Already bound here; no need to add it again.');
  timers.shift()();for(const cell of cells)assert.equal(cell.children.length,2);
});
