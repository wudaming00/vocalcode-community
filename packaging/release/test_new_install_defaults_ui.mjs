import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
const html=readFileSync(new URL('../../vocalcode-app/src/webui.html',import.meta.url),'utf8');
const between=(from,to)=>{const start=html.indexOf(from),end=html.indexOf(to,start);assert.ok(start>=0&&end>start,from);return html.slice(start,end);};
const keepHistory=between('  // KEEP HISTORY:','  // END KEEP HISTORY');
const backButton=between('  // FIRST RUN BACK BUTTON:','  // END FIRST RUN BACK BUTTON');
const bindList=html.match(/  function bindList\(which\)\{[^\n]*\}\n/)[0];
const dicts=vm.runInNewContext('('+html.slice(html.indexOf('const DICTS = ')+'const DICTS = '.length,html.indexOf('\n  };',html.indexOf('const DICTS = '))+4)+')');

function setup(code,cfg){
  class Element{
    constructor(){this.value='';this.checked=false;this.text='';this.onchange=null;}
    set textContent(v){this.text=String(v);}get textContent(){return this.text;}
    set innerHTML(_){throw Error('unsafe markup');}
  }
  const nodes=new Map(),saves=[],caps=[];
  const node=id=>{if(!nodes.has(id))nodes.set(id,new Element());return nodes.get(id);};
  const ctx={cfg,t:s=>s,document:{getElementById:node},save:()=>saves.push(JSON.parse(JSON.stringify(cfg))),showCap:which=>caps.push(which)};
  vm.createContext(ctx);vm.runInContext(bindList+code,ctx);
  return {ctx,node,saves,caps};
}

test('first run offers the Back button as Enter, unticked on a new install',()=>{
  const step=between('<div class="fr-step" id="frLanguageStep">','</div>\n  </div>\n</div>');
  const box=step.match(/<input type="checkbox" id="frBackEnter"[^>]*>/);
  assert.ok(box,'the checkbox is in the language step');
  assert.doesNotMatch(box[0],/checked/);
  assert.match(step,/<label class="fr-option" for="frBackEnter">/);
  assert.match(step,/Use the mouse Back button as Enter/);
  // Choosing a language ends the step, so the offer comes before the choices.
  assert.ok(step.indexOf('id="frBackEnter"')<step.indexOf('id="frLanguageGrid"'));
  const f=setup(backButton,{send:[]});
  f.ctx.renderBackEnter();
  assert.equal(f.node('frBackEnter').checked,false);
  assert.deepEqual(f.saves,[]);
});

test('ticking binds exactly the Back button to send; unticking removes only it',()=>{
  const f=setup(backButton,{send:['F13'],talk:['mouse_x2']});
  const box=f.node('frBackEnter');
  box.checked=true;box.onchange({target:box});
  assert.deepEqual(f.ctx.cfg.send,['F13','mouse_x1']);
  assert.equal(f.saves.length,1);assert.deepEqual(f.caps,['send']);
  box.checked=true;box.onchange({target:box});
  assert.deepEqual(f.ctx.cfg.send,['F13','mouse_x1'],'never bound twice');
  box.checked=false;box.onchange({target:box});
  assert.deepEqual(f.ctx.cfg.send,['F13']);
  assert.deepEqual(f.ctx.cfg.talk,['mouse_x2'],'other shortcuts untouched');
  assert.equal(f.saves.length,3);
});

test('an install that already sends with Back sees the box ticked and keeps it',()=>{
  const f=setup(backButton,{send:['mouse_x1']});
  f.ctx.renderBackEnter();
  assert.equal(f.node('frBackEnter').checked,true);
  assert.deepEqual(f.saves,[],'rendering never changes the binding');
  // A rejected save is rolled back in cfg and re-rendered from it.
  f.ctx.cfg.send=[];f.ctx.renderBackEnter();
  assert.equal(f.node('frBackEnter').checked,false);
});

test('History offers Off / 24 hours / 7 days and shows what each keeps',()=>{
  const card=between('<div class="card" id="keepHistoryCard"','</div>\n        </div>');
  const options=[...card.matchAll(/<option value="([^"]+)">([^<]+)<\/option>/g)].map(m=>[m[1],m[2]]);
  assert.deepEqual(options,[['off','Off'],['24h','24 hours'],['7d','7 days']]);
  const panel=between('<div class="panel" data-panel="history">','<!-- LICENSE -->');
  assert.ok(panel.indexOf('id="keepHistoryCard"')<panel.indexOf('id="histCard"'),'visible above the list');
  assert.match(panel,/Text that could not be typed stays in this list until VocalCode quits, whatever Keep history is set to\./);
  const f=setup(keepHistory,{keep_history:'7d'});
  f.ctx.renderKeepHistory();
  assert.equal(f.node('keepHistory').value,'7d');
  assert.match(f.node('keepHistoryNote').textContent,/50 recent dictations.*encrypted.*7 days/);
  f.ctx.cfg.keep_history='off';f.ctx.renderKeepHistory();
  assert.equal(f.node('keepHistory').value,'off');
  assert.match(f.node('keepHistoryNote').textContent,/This session only.*deletes/);
});

test('changing retention is an ordinary settings save; unknown values are refused',()=>{
  const f=setup(keepHistory,{keep_history:'7d'});
  const select=f.node('keepHistory');
  select.value='24h';select.onchange({target:select});
  assert.equal(f.ctx.cfg.keep_history,'24h');
  assert.deepEqual(f.saves.map(c=>c.keep_history),['24h']);
  assert.match(f.node('keepHistoryNote').textContent,/24 hours/);
  select.value='forever';select.onchange({target:select});
  assert.equal(f.ctx.cfg.keep_history,'24h');
  assert.equal(select.value,'24h','the control snaps back to the saved value');
  assert.equal(f.saves.length,1);
  // Anything the host did not send reads as Off, never as a longer period.
  f.ctx.cfg.keep_history=undefined;f.ctx.renderKeepHistory();
  assert.equal(select.value,'off');
});

test('kept entries from another day say which day, above the time',()=>{
  const ctx={};vm.createContext(ctx);
  vm.runInContext(between('  function localHistoryTime(','  function renderHist('),ctx);
  const now=Math.floor(Date.now()/1000);
  assert.doesNotMatch(ctx.localHistoryTime(now),/\n/);
  const older=ctx.localHistoryTime(now-3*24*3600).split('\n');
  assert.equal(older.length,2);
  assert.equal(older[1],new Date((now-3*24*3600)*1000).toLocaleTimeString([],{hour:'2-digit',minute:'2-digit'}));
  assert.equal(ctx.localHistoryTime(-1),'—');
  assert.match(html,/\.hist-row \.t\{[^}]*white-space:pre-line/);
});

test('both controls follow every config render and every new string is translated',()=>{
  const render=between('  function renderConfigState(){','  function rollbackRejected(');
  assert.match(render,/renderBackEnter\(\);/);
  assert.match(render,/renderKeepHistory\(\);/);
  const strings=[
    'Use the mouse Back button as Enter',
    'Tap it to send what you dictated. Apps then stop receiving it as Back. You can change this any time in Shortcuts.',
    'Keep history','24 hours','7 days',
    ...Object.values(vm.runInNewContext('('+keepHistory.match(/var KEEP_HISTORY_NOTES=(\{[\s\S]*?\});/)[1]+')')),
    'Text that could not be typed stays in this list until VocalCode quits, whatever Keep history is set to. Local diagnostics, when on, also bring back their newest records. Nothing is uploaded. Copying, exporting or Paste text can expose transcripts to clipboard history, sync tools or other readers.'
  ];
  for(const language of ['zh','es','fr','de']){
    for(const english of strings){
      assert.equal(typeof dicts[language][english],'string',`${language}: ${english}`);
      assert.notEqual(dicts[language][english],english,`${language}: ${english}`);
    }
  }
});
