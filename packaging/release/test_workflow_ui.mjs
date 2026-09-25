import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
const html=readFileSync(new URL('../../vocalcode-app/src/webui.html',import.meta.url),'utf8');
const controller=html.slice(html.indexOf('  let workflowBusy='),html.indexOf('  let migrationBusy='));
function setup(){
  class Element {
    constructor(){this.children=[];this.value='';this.checked=false;this.disabled=false;this.textContent='';}
    append(...items){this.children.push(...items);}
    replaceChildren(...items){this.children=items;}
    focus(){this.focused=true;}scrollIntoView(){}
    set innerHTML(_){throw Error('Unsafe markup');}
  }
  const nodes=new Map(),sent=[],timers=new Map(),delays=new Map(),toasts=[];let timerId=0;
  const node=id=>{if(!nodes.has(id))nodes.set(id,new Element());return nodes.get(id);};
  const ctx={t:s=>s,confirm:()=>false,showPanel(){},showSaved(){ctx.saved++;},saved:0,toast:m=>toasts.push(m),utf8Bytes:s=>Buffer.byteLength(s,'utf8'),setTimeout:(fn,ms)=>{timers.set(++timerId,fn);delays.set(timerId,ms);return timerId;},clearTimeout:id=>{timers.delete(id);delays.delete(id);},document:{getElementById:node,createElement:()=>new Element()},window:{},send:r=>{sent.push(r);return true;}};
  vm.createContext(ctx);vm.runInContext(controller,ctx);
  return {node,sent,ctx,timers,toasts,
    receive(data){ctx.window.vocalcodeWorkflowResult({id:sent.at(-1).id,ok:true,data});},
    // What vocalcodeInit does once the page is up.
    load(){ctx.workflowRequest('load');},
    // Let the save debounce elapse; the 120 s acknowledgement watchdogs keep running.
    settle(){for(const [id,fn] of [...timers]){if(delays.get(id)<1000){timers.delete(id);delays.delete(id);fn();}}},
    change(id,value){const el=node(id);if(typeof value==='boolean')el.checked=value;else el.value=value;el.onchange();}};
}
const prefs={schema:1,diagnostics:false,max_entries:100000,max_bytes:2147483648,cleanup:'light',profiles:[]};
// A stand-in for workflows.rs: revision-bound saves over one stored document,
// which outlives any one page, the way the file on disk outlives a reload.
function fakeHost(initial={...prefs,remove_fillers:false,chinese_fillers:false}){
  let stored={preferences:structuredClone(initial),revision:'r0'},writes=0;
  return {
    get stored(){return stored;},
    answer(f){
      const r=f.sent.at(-1);
      if(r.op==='save'){
        if(r.revision!==stored.revision)return f.ctx.window.vocalcodeWorkflowResult({id:r.id,ok:false,message:'Workflow settings were changed elsewhere.'});
        stored={preferences:structuredClone(r.preferences),revision:'r'+(++writes)};
      }
      f.ctx.window.vocalcodeWorkflowResult({id:r.id,ok:true,data:{preferences:structuredClone(stored.preferences),revision:stored.revision,saved:r.op==='save'}});
    },
    writeElsewhere(change){stored={preferences:{...stored.preferences,...change},revision:'r'+(++writes)};},
  };
}
test('effective diagnostic switch and last saved time are visible after load',()=>{
  const f=setup();f.load();f.receive({preferences:{...prefs,diagnostics:true},revision:'migrated',diagnostic_storage:{total:362,latest_record_unix_ms:1788626248791}});
  assert.match(f.node('workflowMessage').textContent,/Text history is on\./);assert.match(f.node('workflowMessage').textContent,/Saved records: 362/);
});
test('legacy correction pairs render as inert text',()=>{
  const f=setup();f.node('diagnosticLoad').onclick();f.receive({entries:[{recorded_unix_ms:1,kind:'learned_correction',result:'learned',corrections:[{from:'<script>bad</script>',to:'考虑'}]}],total:1,bytes:100,offset:0,unreadable:0});
  assert.equal(f.node('diagnosticRows').children[0].children[2].textContent,'<script>bad</script> → 考虑');
});
test('workflow changes save themselves, bound to the loaded revision; zero means unlimited',()=>{
  const f=setup();f.load();f.receive({preferences:structuredClone(prefs),revision:'first'});
  assert.equal(f.sent.length,1);
  f.change('workflowDiagnostics',true);f.change('workflowMaxEntries','0');f.change('workflowMaxMiB','0');
  assert.equal(f.sent.length,1,'debounced, not one write per change event');
  f.settle();assert.equal(f.sent.length,2);
  const save=f.sent.at(-1);assert.equal(save.op,'save');assert.equal(save.revision,'first');
  assert.equal(save.preferences.diagnostics,true);
  assert.equal(save.preferences.max_entries,0);
  assert.equal(save.preferences.max_bytes,0);
});
test('an emptied or fractional limit is not saved as unlimited',()=>{
  const f=setup();f.load();f.receive({preferences:structuredClone(prefs),revision:'first'});
  for(const value of ['','  ','2.5','-1','1e400']){
    f.change('workflowMaxEntries',value);f.settle();
    assert.equal(f.sent.length,1,JSON.stringify(value));
    assert.equal(String(f.node('workflowMaxEntries').value),'100000');
  }
  assert.match(f.node('workflowMessage').textContent,/whole number/);
});
test('busy and stale results cannot acknowledge another request',()=>{
  const f=setup();f.load();f.load();assert.equal(f.sent.length,1);
  f.ctx.window.vocalcodeWorkflowResult({id:0,ok:false});f.load();assert.equal(f.sent.length,1);
  f.receive({preferences:structuredClone(prefs),revision:'new'});f.load();assert.equal(f.sent.length,2);
});
test('saved transcript markup is inert and copying uses the native copy contract',()=>{
  const f=setup();f.node('diagnosticLoad').onclick();
  f.receive({entries:[{recorded_unix_ms:1,result:'failed',model:'fake',app_id:'test.exe',raw_text:'<script>evil()</script>',final_text:'recover me'}],offset:0,total:21,bytes:10,unreadable:0});
  const row=f.node('diagnosticRows').children[0];assert.match(row.children[2].textContent,/<script>/);
  row.children.at(-1).onclick();assert.equal(f.sent.at(-1).type,'copy');assert.equal(f.sent.at(-1).text,'recover me');
  f.node('diagnosticNext').onclick();assert.equal(f.sent.at(-1).offset,20);
});
test('rewrite only changes scratchpad on acceptance and undo refuses later edits',()=>{
  const f=setup();f.node('rewriteSource').value='We did not approve 1200 USD.';
  f.node('rewriteAction').value='polish';f.node('rewriteModel').value='local';f.node('rewritePreview').onclick();
  f.receive({candidate:'We approved $1,200.',source:'We did not approve 1200 USD.',elapsed_ms:1,warnings:['Check changed negation: not']});
  assert.equal(f.node('rewriteSource').value,'We did not approve 1200 USD.');
  assert.match(f.node('rewriteMessage').textContent,/negation/);
  f.node('rewriteAccept').onclick();assert.equal(f.node('rewriteSource').value,'We approved $1,200.');
  f.node('rewriteSource').value='My manual revision';f.node('rewriteUndo').onclick();assert.equal(f.node('rewriteSource').value,'My manual revision');
  assert.ok(!f.sent.some(r=>r.type.includes('inject')));
});

test('acceptance and undo update the byte counter without a synthetic input event',()=>{
  const f=setup();f.node('rewriteSource').value='原文';f.node('rewritePreview').onclick();
  f.receive({candidate:'新的原文',source:'原文',elapsed_ms:1});
  f.node('rewriteAccept').onclick();assert.equal(f.node('rewriteSize').textContent,'12 / 3000 UTF-8 bytes');
  f.node('rewriteUndo').onclick();assert.equal(f.node('rewriteSource').value,'原文');
  assert.equal(f.node('rewriteSize').textContent,'6 / 3000 UTF-8 bytes');
  assert.match(f.node('rewriteMessage').textContent,/Restored/);
});

test('acceptance and undo clear cloud consent even without a keyboard input event',()=>{
  const f=setup();f.node('rewriteSource').value='original';f.node('rewritePreview').onclick();
  f.receive({candidate:'candidate',source:'original',elapsed_ms:1});
  f.node('rewriteCloudConsent').checked=true;f.node('rewriteAccept').onclick();
  assert.equal(f.node('rewriteCloudConsent').checked,false);
  assert.equal(f.node('rewriteCandidate').value,'');
  f.node('rewriteProvider').value='claude';f.node('rewriteCloudConsent').checked=true;
  f.node('rewriteUndo').onclick();assert.equal(f.node('rewriteSource').value,'original');
  assert.equal(f.node('rewriteCloudConsent').checked,false);
  f.node('rewritePreview').onclick();assert.equal(f.sent.length,1);
  assert.match(f.node('rewriteMessage').textContent,/Confirm cloud processing/);
});

test('undo during a later preview invalidates its candidate without another model request',()=>{
  const f=setup();f.node('rewriteSource').value='original';f.node('rewritePreview').onclick();
  f.receive({candidate:'accepted',source:'original',elapsed_ms:1});f.node('rewriteAccept').onclick();
  f.node('rewritePreview').onclick();f.node('rewriteUndo').onclick();
  f.receive({candidate:'late candidate',source:'accepted',elapsed_ms:1});
  assert.equal(f.node('rewriteSource').value,'original');
  assert.equal(f.node('rewriteCandidate').value,'');assert.equal(f.sent.length,2);
});

test('history review only loads an explicit local draft and resets cloud consent',()=>{
  const f=setup();f.node('rewriteCloudConsent').checked=true;
  assert.equal(f.ctx.reviewInScratchpad('不要发布 1200 USD。'),true);
  assert.equal(f.node('rewriteSource').value,'不要发布 1200 USD。');
  assert.equal(f.node('rewriteCloudConsent').checked,false);
  assert.equal(f.node('rewriteDetails').open,true);assert.equal(f.node('rewriteSource').focused,true);
  assert.deepEqual(f.sent,[]);
});

test('history review protects an existing draft and refuses silent truncation',()=>{
  const f=setup();f.node('rewriteSource').value='My unsaved edits';
  assert.equal(f.ctx.reviewInScratchpad('different'),false);
  assert.equal(f.node('rewriteSource').value,'My unsaved edits');
  f.ctx.confirm=()=>true;assert.equal(f.ctx.reviewInScratchpad('different'),true);
  assert.equal(f.ctx.reviewInScratchpad('中'.repeat(1001)),false);
  assert.equal(f.node('rewriteSource').value,'different');assert.deepEqual(f.sent,[]);
});

test('loading another history draft discards an in-flight candidate without sending again',()=>{
  const f=setup();f.node('rewriteSource').value='first';f.node('rewritePreview').onclick();
  f.ctx.confirm=()=>true;f.ctx.reviewInScratchpad('second');
  f.receive({source:'first',candidate:'outdated',elapsed_ms:1});
  assert.equal(f.node('rewriteSource').value,'second');assert.equal(f.node('rewriteCandidate').value,'');
  assert.equal(f.sent.length,1);
});

test('expanded review and calendar cards cannot flex-shrink away their controls',()=>{
  assert.match(html,/\.panel\[data-panel="history"\]>\.card\{flex-shrink:0\}/);
  assert.match(html,/\.panel\[data-panel="meetings"\]>details\.card\{flex-shrink:0\}/);
  assert.match(html,/\.meeting-workspace\{[^}]*min-height:220px;flex:1 0 220px/);
  assert.ok(html.indexOf('id="meetingStop"')<html.indexOf('id="calendarConfigure"'));
});

test('long installed model names and calendar options stay inside their cards',()=>{
  assert.match(html,/\.migration-tools>select\{min-width:0;max-width:100%\}/);
  assert.match(html,/#rewriteProviderNotes\{white-space:pre-line;overflow-wrap:anywhere\}/);
  assert.match(html,/#rewriteMessage,#calendarMessage,#calendarAssociation\{overflow-wrap:anywhere\}/);
});
test('edited or discarded source does not receive an outdated rewrite',()=>{
  const f=setup();f.node('rewriteSource').value='old';f.node('rewritePreview').onclick();
  f.node('rewriteSource').value='new';f.receive({source:'old',candidate:'candidate',elapsed_ms:1});
  assert.equal(f.node('rewriteCandidate').value,'');assert.equal(f.node('rewriteSource').value,'new');
});

test('provider discovery never sends scratchpad text or changes to a cloud provider',()=>{
  const f=setup();f.node('rewriteSource').value='private source';f.node('rewriteModels').onclick();
  assert.equal(f.sent.at(-1).op,'rewrite_providers');assert.equal(f.sent.at(-1).text,undefined);
  f.receive({providers:[{id:'ollama',available:false,message:'No local service'},{id:'claude',available:true,version:'test',message:'Cloud'}],models:[]});
  assert.equal(f.node('rewriteProvider').value,'ollama');assert.equal(f.node('rewriteCloudConsent').checked,false);
});

test('cloud processing requires fresh per-request consent and never falls back',()=>{
  const f=setup();f.node('rewriteProvider').value='claude';f.node('rewriteProvider').onchange();
  f.node('rewriteSource').value='A synthetic sentence';f.node('rewritePreview').onclick();assert.equal(f.sent.length,0);
  assert.match(f.node('rewriteMessage').textContent,/Confirm cloud/);
  f.node('rewriteCloudConsent').checked=true;f.node('rewritePreview').onclick();
  assert.equal(f.sent.at(-1).provider,'claude');assert.equal(f.sent.at(-1).cloud_consent,true);
  assert.equal(f.node('rewriteCloudConsent').checked,false);
  f.ctx.window.vocalcodeWorkflowResult({id:f.sent.at(-1).id,ok:false,message:'No quota'});
  f.node('rewritePreview').onclick();assert.equal(f.sent.length,1);assert.equal(f.node('rewriteSource').value,'A synthetic sentence');
});

test('changing provider discards an in-flight candidate without accepting it',()=>{
  const f=setup();f.node('rewriteSource').value='source';f.node('rewritePreview').onclick();
  f.node('rewriteProvider').value='claude';f.node('rewriteProvider').onchange();
  f.receive({source:'source',candidate:'old local candidate',elapsed_ms:1,provider:'ollama'});
  assert.equal(f.node('rewriteCandidate').value,'');assert.equal(f.node('rewriteSource').value,'source');
});

test('source edits after generation and repeated acceptance cannot overwrite newer work',()=>{
  const f=setup();f.node('rewriteSource').value='first';f.node('rewritePreview').onclick();
  f.receive({source:'first',candidate:'second',elapsed_ms:1});
  f.node('rewriteSource').value='my later edits';f.node('rewriteAccept').onclick();
  assert.equal(f.node('rewriteSource').value,'my later edits');
  f.node('rewritePreview').onclick();f.receive({source:'my later edits',candidate:'new candidate',elapsed_ms:1});
  f.node('rewriteAccept').onclick();f.node('rewriteAccept').onclick();f.node('rewriteUndo').onclick();
  assert.equal(f.node('rewriteSource').value,'my later edits');
});

test('source limit counts UTF-8 bytes for Chinese and emoji before any provider request',()=>{
  const f=setup();
  for(const source of ['中'.repeat(1001),'🙂'.repeat(751),'\0','  ']){
    f.node('rewriteSource').value=source;f.node('rewriteSource').oninput();f.node('rewritePreview').onclick();
    assert.equal(f.sent.length,0);
  }
  f.node('rewriteSource').value='中'.repeat(1000);f.node('rewritePreview').onclick();
  assert.equal(f.sent.length,1);assert.equal(f.node('rewriteSize').textContent,'3000 / 3000 UTF-8 bytes');
});

test('changing model or operation invalidates the old candidate and renews consent',()=>{
  for(const id of ['rewriteAction','rewriteModel','rewriteCliModel']){
    const f=setup();f.node('rewriteSource').value='source';f.node('rewritePreview').onclick();
    f.node('rewriteCloudConsent').checked=true;f.node(id).onchange();
    assert.equal(f.node('rewriteCloudConsent').checked,false);
    f.receive({source:'source',candidate:'old-model candidate',elapsed_ms:1});
    assert.equal(f.node('rewriteCandidate').value,'');
  }
});

test('editing the source invalidates its candidate and requires new cloud consent',()=>{
  const f=setup();f.node('rewriteSource').value='source';f.node('rewritePreview').onclick();
  f.receive({source:'source',candidate:'previous candidate',elapsed_ms:1});
  f.node('rewriteProvider').value='claude';f.node('rewriteCloudConsent').checked=true;
  f.node('rewriteSource').value='a different private draft';f.node('rewriteSource').oninput();
  assert.equal(f.node('rewriteCloudConsent').checked,false);
  assert.equal(f.node('rewriteCandidate').value,'');
  f.node('rewritePreview').onclick();assert.equal(f.sent.length,1);
  assert.match(f.node('rewriteMessage').textContent,/Confirm cloud/);
});

test('fidelity warnings remain inert text and use the selected interface language',()=>{
  const f=setup();
  f.ctx.t=s=>s==='Check changed number/unit/code: '?'请检查数字：':s;
  f.node('rewriteSource').value='Budget 1200 USD.';f.node('rewritePreview').onclick();
  f.receive({source:'Budget 1200 USD.',candidate:'Budget 12000 USD.',elapsed_ms:1,warnings:['Check changed number/unit/code: 1200','<script>untrusted()</script>']});
  assert.match(f.node('rewriteMessage').textContent,/请检查数字：1200/);
  assert.match(f.node('rewriteMessage').textContent,/<script>untrusted\(\)<\/script>/);
});

test('lost workflow acknowledgements recover without retry and ignore stale results',()=>{
  const f=setup();f.node('rewriteSource').value='source';f.node('rewritePreview').onclick();
  const oldId=f.sent.at(-1).id,oldTimer=[...f.timers.values()][0];oldTimer();
  assert.equal(f.timers.size,0);assert.equal(f.sent.length,1);
  assert.match(f.node('rewriteMessage').textContent,/No confirmation/);
  f.ctx.window.vocalcodeWorkflowResult({id:oldId,ok:true,data:{source:'source',candidate:'stale',elapsed_ms:1}});
  assert.equal(f.node('rewriteCandidate').value,'');
  f.node('rewritePreview').onclick();const next=f.sent.at(-1).id;assert.ok(next>oldId);
  oldTimer();assert.equal(f.timers.size,1);
  f.receive({source:'source',candidate:'new',elapsed_ms:1});assert.equal(f.node('rewriteCandidate').value,'new');assert.equal(f.timers.size,0);
});

test('workflow IPC exceptions release the page without reporting an accepted request',()=>{
  const f=setup();f.ctx.send=()=>{throw Error('unavailable');};f.node('rewriteModels').onclick();
  assert.equal(f.timers.size,0);assert.match(f.node('rewriteMessage').textContent,/Could not send/);
  f.ctx.send=r=>{f.sent.push(r);return true;};f.node('rewriteModels').onclick();assert.equal(f.sent.length,1);
});

test('pause-word removal is off for old settings and saves as soon as it is switched',()=>{
  const f=setup();f.load();f.receive({preferences:structuredClone(prefs),revision:'old'});
  assert.equal(f.node('workflowFillers').checked,false);
  f.change('workflowFillers',true);f.settle();
  assert.equal(f.sent.length,2);assert.equal(f.sent.at(-1).op,'save');
  assert.equal(f.sent.at(-1).preferences.remove_fillers,true);
  assert.equal(f.sent.at(-1).preferences.diagnostics,false);
  f.receive({preferences:{...prefs,remove_fillers:true},revision:'new',saved:true});
  assert.equal(f.node('workflowFillers').checked,true);assert.equal(f.ctx.saved,1);
  assert.match(f.node('workflowMessage').textContent,/Applies to the next dictation/);
});
test('toggling filler removal persists across reload',()=>{
  const host=fakeHost();
  const first=setup();first.load();host.answer(first);
  first.change('workflowFillers',true);first.settle();host.answer(first);
  assert.equal(host.stored.preferences.remove_fillers,true);
  // A fresh page, as after closing and reopening Settings or restarting.
  const reopened=setup();reopened.load();host.answer(reopened);
  assert.equal(reopened.node('workflowFillers').checked,true);
  reopened.change('workflowFillers',false);reopened.settle();host.answer(reopened);
  const again=setup();again.load();host.answer(again);
  assert.equal(again.node('workflowFillers').checked,false);
  assert.equal(host.stored.preferences.remove_fillers,false);
});
test('punctuation mode and Chinese pause words save without any other step',()=>{
  const host=fakeHost();
  const f=setup();f.load();host.answer(f);
  f.change('workflowCleanup','original');f.change('workflowChineseFillers',true);
  f.settle();assert.equal(f.sent.length,2,'two quick changes, one write');host.answer(f);
  assert.equal(host.stored.preferences.cleanup,'original');assert.equal(host.stored.preferences.chinese_fillers,true);
  const reopened=setup();reopened.load();host.answer(reopened);
  assert.equal(reopened.node('workflowCleanup').value,'original');
  assert.equal(reopened.node('workflowChineseFillers').checked,true);
});
test('a change made during another request is saved after it, on the rotated revision',()=>{
  const host=fakeHost();
  const f=setup();f.load();host.answer(f);
  // A rewrite preview holds the host's one workflow slot for a while.
  f.node('rewriteSource').value='source';f.node('rewritePreview').onclick();
  f.change('workflowFillers',true);f.settle();
  assert.equal(f.sent.at(-1).op,'rewrite_preview','nothing is sent over a busy request');
  f.receive({source:'source',candidate:'candidate',elapsed_ms:1});
  assert.equal(f.sent.at(-1).op,'save');assert.equal(f.sent.at(-1).revision,'r0');
  // And an edit made while that save is in flight follows it.
  f.change('workflowChineseFillers',true);f.settle();assert.equal(f.sent.at(-1).preferences.chinese_fillers,false);
  assert.equal(f.node('workflowFillers').checked,true);
  host.answer(f);
  const second=f.sent.at(-1);assert.equal(second.op,'save');assert.equal(second.revision,'r1');
  assert.equal(second.preferences.remove_fillers,true);assert.equal(second.preferences.chinese_fillers,true);
  host.answer(f);assert.equal(host.stored.revision,'r2');
  assert.equal(f.node('workflowChineseFillers').checked,true);
});
test('a save refused over another writer reloads what is saved and says so',()=>{
  const host=fakeHost();
  const f=setup();f.load();host.answer(f);
  host.writeElsewhere({cleanup:'original'});
  f.change('workflowFillers',true);f.settle();host.answer(f);
  assert.equal(host.stored.preferences.remove_fillers,false,'the other writer is not overwritten');
  assert.equal(f.sent.at(-1).op,'load','the page reads the saved state back by itself');
  assert.equal(f.toasts.length,1);assert.match(f.toasts[0],/changed elsewhere.*make the change again/);
  host.answer(f);
  assert.equal(f.node('workflowFillers').checked,false);assert.equal(f.node('workflowCleanup').value,'original');
  assert.match(f.node('workflowMessage').textContent,/make the change again/);
  f.change('workflowFillers',true);f.settle();host.answer(f);
  assert.equal(host.stored.preferences.remove_fillers,true);assert.equal(host.stored.preferences.cleanup,'original');
});
test('a change made before the first load is saved once it arrives',()=>{
  const host=fakeHost();
  const f=setup();f.load();
  f.change('workflowFillers',true);f.settle();assert.equal(f.sent.length,1);
  host.answer(f);assert.equal(f.sent.at(-1).op,'save');host.answer(f);
  assert.equal(host.stored.preferences.remove_fillers,true);
});
test('an unreadable settings file does not turn a change into a request loop',()=>{
  const unreadable='Unreadable workflow settings; the file was left untouched.';
  const f=setup();f.load();f.ctx.window.vocalcodeWorkflowResult({id:f.sent.at(-1).id,ok:false,message:unreadable});
  f.change('workflowFillers',true);f.settle();
  assert.equal(f.sent.at(-1).op,'load');
  f.ctx.window.vocalcodeWorkflowResult({id:f.sent.at(-1).id,ok:false,message:unreadable});
  assert.equal(f.sent.length,2);f.settle();assert.equal(f.sent.length,2);
  assert.match(f.toasts.at(-1),/Unreadable/);
});
test('workflow hints and profile rows go through the interface language',()=>{
  const host=fakeHost();
  const f=setup();f.ctx.t=s=>'«'+s+'»';f.load();host.answer(f);
  f.node('workflowApp').value='Code.exe';f.node('workflowAppCleanup').value='original';
  for(const id of ['workflowAppLive','workflowAppPaste','workflowAppFillers','workflowAppChineseFillers'])f.node(id).value='inherit';
  f.node('workflowAdd').onclick();f.settle();host.answer(f);
  assert.equal(f.node('workflowMessage').textContent,'«Saved. Applies to the next dictation.»');
  const row=f.node('workflowProfiles').children[0];
  assert.equal(row.children[0].textContent,'Code.exe · «Original recognition» · «Progressive»: «Global setting» · «Compatibility paste»: «Global setting» · «English pause words»: «Global setting» · «Chinese pause words»: «Global setting»');
  assert.equal(row.children[1].textContent,'«Remove»');
  row.children[1].onclick();f.settle();assert.deepEqual(f.sent.at(-1).preferences.profiles,[]);
});

test('per-app pause-word override is separate from punctuation and progressive settings',()=>{
  const f=setup();f.load();f.receive({preferences:structuredClone(prefs),revision:'old'});
  f.node('workflowApp').value='Code.exe';f.node('workflowAppCleanup').value='original';
  f.node('workflowAppLive').value='inherit';f.node('workflowAppPaste').value='inherit';f.node('workflowAppFillers').value='false';
  f.node('workflowAdd').onclick();assert.equal(f.sent.length,1);
  f.settle();const p=f.sent.at(-1).preferences.profiles[0];
  assert.equal(p.remove_fillers,false);assert.equal(p.cleanup,'original');assert.equal(p.progressive,null);
});

test('Chinese opt-in never inherits English opt-in and supports an explicit app override',()=>{
  const f=setup();f.load();f.receive({preferences:{...prefs,remove_fillers:true},revision:'old-en'});
  assert.equal(f.node('workflowChineseFillers').checked,false);
  f.node('workflowChineseFillers').checked=true;f.node('workflowChineseFillers').onchange();
  assert.equal(f.sent.length,1);
  f.node('workflowApp').value='Code.exe';f.node('workflowAppCleanup').value='light';
  f.node('workflowAppLive').value='inherit';f.node('workflowAppPaste').value='inherit';
  f.node('workflowAppFillers').value='inherit';f.node('workflowAppChineseFillers').value='false';
  f.node('workflowAdd').onclick();f.settle();assert.equal(f.sent.length,2);
  const p=f.sent.at(-1).preferences;assert.equal(p.remove_fillers,true);assert.equal(p.chinese_fillers,true);
  assert.equal(p.diagnostics,false);assert.equal(p.profiles[0].remove_fillers,null);assert.equal(p.profiles[0].chinese_fillers,false);
});
test('custom rewrite instruction is required, travels with the request, and edits invalidate it',()=>{
  const f=setup();f.node('rewriteSource').value='Can we ship on Friday?';
  f.node('rewriteModel').value='local';f.node('rewriteAction').value='custom';f.node('rewriteAction').onchange();
  assert.equal(f.node('rewriteInstructionRow').hidden,false);
  f.node('rewriteInstruction').value='   ';f.node('rewritePreview').onclick();
  assert.equal(f.sent.length,0);assert.match(f.node('rewriteMessage').textContent,/what to change/);
  f.node('rewriteInstruction').value='make it more formal';f.node('rewritePreview').onclick();
  assert.equal(f.sent.at(-1).action,'custom');assert.equal(f.sent.at(-1).instruction,'make it more formal');
  f.node('rewriteInstruction').oninput();
  f.receive({candidate:'Could we release on Friday?',source:'Can we ship on Friday?',elapsed_ms:1,warnings:[]});
  assert.equal(f.node('rewriteCandidate').value,'','a changed instruction discards the late reply');
  f.node('rewriteAction').value='translate_zh';f.node('rewriteAction').onchange();
  assert.equal(f.node('rewriteInstructionRow').hidden,true);
});
test('scratchpad offers translation and custom instructions',()=>{
  for(const option of ['value="translate_en"','value="translate_zh"','value="custom"','id="rewriteInstruction"']) assert.ok(html.includes(option),option);
});
test('a save refused for any other reason puts the saved values back at once and does not promise a reload',()=>{
  const host=fakeHost();
  const f=setup();f.load();host.answer(f);
  f.change('workflowDiagnostics',true);f.settle();
  assert.equal(f.sent.at(-1).op,'save');
  f.ctx.window.vocalcodeWorkflowResult({id:f.sent.at(-1).id,ok:false,message:'Secure storage is unavailable.'});
  assert.equal(f.node('workflowDiagnostics').checked,false,'the control shows what is saved');
  assert.equal(f.sent.at(-1).op,'save','nothing moved on disk, so nothing is read again');
  assert.deepEqual(f.toasts,['Secure storage is unavailable.']);
  assert.doesNotMatch(f.node('workflowMessage').textContent,/make the change again/);
});
test('after a conflict the controls show the saved values even if the reload fails',()=>{
  const host=fakeHost();
  const f=setup();f.load();host.answer(f);
  host.writeElsewhere({cleanup:'original'});
  f.change('workflowFillers',true);f.settle();host.answer(f);
  assert.equal(f.node('workflowFillers').checked,false);
  assert.equal(f.sent.at(-1).op,'load');
  f.ctx.window.vocalcodeWorkflowResult({id:f.sent.at(-1).id,ok:false,message:'Unreadable workflow settings; the file was left untouched.'});
  assert.equal(f.node('workflowFillers').checked,false);
});
test('edits held behind a request that never answered are still saved',()=>{
  const host=fakeHost();
  const f=setup();f.load();host.answer(f);
  f.node('rewriteSource').value='source';f.node('rewritePreview').onclick();
  f.change('workflowFillers',true);f.settle();
  assert.equal(f.sent.at(-1).op,'rewrite_preview');
  const [watchdog]=[...f.timers.values()];watchdog();
  assert.equal(f.sent.at(-1).op,'save');assert.equal(f.sent.at(-1).preferences.remove_fillers,true);
  host.answer(f);assert.equal(host.stored.preferences.remove_fillers,true);
});
test('a first read that never answers drops edits instead of asking again forever',()=>{
  const f=setup();f.load();
  f.change('workflowFillers',true);f.settle();assert.equal(f.sent.length,1);
  const [watchdog]=[...f.timers.values()];watchdog();
  assert.equal(f.sent.length,1);assert.equal(f.toasts.length,1);
  f.settle();assert.equal(f.sent.length,1);
});
test('an application profile cannot be added before the saved ones are known',()=>{
  const host=fakeHost({...prefs,remove_fillers:false,chinese_fillers:false,profiles:[{app_id:'Word.exe',cleanup:'original',progressive:null,paste:null,remove_fillers:null,chinese_fillers:null}]});
  const f=setup();f.load();
  f.node('workflowApp').value='Code.exe';f.node('workflowAppCleanup').value='light';
  for(const id of ['workflowAppLive','workflowAppPaste','workflowAppFillers','workflowAppChineseFillers'])f.node(id).value='inherit';
  f.node('workflowAdd').onclick();f.settle();
  assert.match(f.node('workflowMessage').textContent,/still loading/);
  host.answer(f);assert.equal(f.sent.length,1,'nothing was queued to overwrite them');
  f.node('workflowAdd').onclick();f.settle();host.answer(f);
  assert.deepEqual(host.stored.preferences.profiles.map(p=>p.app_id),['Word.exe','Code.exe']);
});
test('the diagnostic export count is spoken in the interface language',()=>{
  const f=setup();f.ctx.t=s=>s==='Exported {n} readable records. Keep the file private.'?'已导出 {n} 条可读记录。':s;
  f.node('diagnosticExport').onclick();f.receive({exported:7});
  assert.equal(f.node('diagnosticMessage').textContent,'已导出 7 条可读记录。');
});
