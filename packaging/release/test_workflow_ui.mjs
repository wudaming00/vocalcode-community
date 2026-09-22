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
    set innerHTML(_){throw Error('Unsafe markup');}
  }
  const nodes=new Map(),sent=[];
  const node=id=>{if(!nodes.has(id))nodes.set(id,new Element());return nodes.get(id);};
  const ctx={document:{getElementById:node,createElement:()=>new Element()},window:{},send:r=>{sent.push(r);return true;}};
  vm.createContext(ctx);vm.runInContext(controller,ctx);
  return {node,sent,ctx,receive(data){ctx.window.vocalcodeWorkflowResult({id:sent.at(-1).id,ok:true,data});}};
}
const prefs={schema:1,diagnostics:false,max_entries:100000,max_bytes:2147483648,cleanup:'light',profiles:[]};
test('effective diagnostic switch and last saved time are visible after load',()=>{
  const f=setup();f.node('workflowReload').onclick();f.receive({preferences:{...prefs,diagnostics:true},revision:'migrated',diagnostic_storage:{total:362,latest_record_unix_ms:1788626248791}});
  assert.match(f.node('workflowMessage').textContent,/Text history: ON/);assert.match(f.node('workflowMessage').textContent,/362 saved records/);
});
test('legacy correction pairs render as inert text',()=>{
  const f=setup();f.node('diagnosticLoad').onclick();f.receive({entries:[{recorded_unix_ms:1,kind:'learned_correction',result:'learned',corrections:[{from:'<script>bad</script>',to:'考虑'}]}],total:1,bytes:100,offset:0,unreadable:0});
  assert.equal(f.node('diagnosticRows').children[0].children[2].textContent,'<script>bad</script> → 考虑');
});
test('workflow changes need explicit revision-bound save; zero means unlimited',()=>{
  const f=setup();f.node('workflowReload').onclick();f.receive({preferences:structuredClone(prefs),revision:'first'});
  assert.equal(f.sent.length,1);
  f.node('workflowDiagnostics').checked=true;f.node('workflowMaxEntries').value='0';f.node('workflowMaxMiB').value='0';
  f.node('workflowSave').onclick();assert.equal(f.sent.at(-1).revision,'first');
  assert.equal(f.sent.at(-1).preferences.max_entries,0);
  assert.equal(f.sent.at(-1).preferences.max_bytes,0);
});
test('busy and stale results cannot acknowledge another save',()=>{
  const f=setup();f.node('workflowReload').onclick();f.node('workflowReload').onclick();assert.equal(f.sent.length,1);
  f.ctx.window.vocalcodeWorkflowResult({id:0,ok:false});f.node('workflowReload').onclick();assert.equal(f.sent.length,1);
  f.receive({preferences:structuredClone(prefs),revision:'new'});f.node('workflowReload').onclick();assert.equal(f.sent.length,2);
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
test('edited or discarded source does not receive an outdated rewrite',()=>{
  const f=setup();f.node('rewriteSource').value='old';f.node('rewritePreview').onclick();
  f.node('rewriteSource').value='new';f.receive({source:'old',candidate:'candidate',elapsed_ms:1});
  assert.equal(f.node('rewriteCandidate').value,'');assert.equal(f.node('rewriteSource').value,'new');
});

test('pause-word removal is off for old settings and needs explicit save',()=>{
  const f=setup();f.node('workflowReload').onclick();f.receive({preferences:structuredClone(prefs),revision:'old'});
  assert.equal(f.node('workflowFillers').checked,false);
  f.node('workflowFillers').checked=true;f.node('workflowFillers').onchange();
  assert.equal(f.sent.length,1);assert.equal(f.node('workflowDetails').open,true);
  f.node('workflowSave').onclick();assert.equal(f.sent.at(-1).preferences.remove_fillers,true);
  assert.equal(f.sent.at(-1).preferences.diagnostics,false);
  f.receive({preferences:{...prefs,remove_fillers:true},revision:'new',saved:true});
  assert.equal(f.node('workflowFillers').checked,true);
});

test('per-app pause-word override is separate from punctuation and progressive settings',()=>{
  const f=setup();f.node('workflowReload').onclick();f.receive({preferences:structuredClone(prefs),revision:'old'});
  f.node('workflowApp').value='Code.exe';f.node('workflowAppCleanup').value='original';
  f.node('workflowAppLive').value='inherit';f.node('workflowAppPaste').value='inherit';f.node('workflowAppFillers').value='false';
  f.node('workflowAdd').onclick();assert.equal(f.sent.length,1);
  f.node('workflowSave').onclick();const p=f.sent.at(-1).preferences.profiles[0];
  assert.equal(p.remove_fillers,false);assert.equal(p.cleanup,'original');assert.equal(p.progressive,null);
});

test('Chinese opt-in never inherits English opt-in and supports an explicit app override',()=>{
  const f=setup();f.node('workflowReload').onclick();f.receive({preferences:{...prefs,remove_fillers:true},revision:'old-en'});
  assert.equal(f.node('workflowChineseFillers').checked,false);
  f.node('workflowChineseFillers').checked=true;f.node('workflowChineseFillers').onchange();
  assert.equal(f.sent.length,1);
  f.node('workflowApp').value='Code.exe';f.node('workflowAppCleanup').value='light';
  f.node('workflowAppLive').value='inherit';f.node('workflowAppPaste').value='inherit';
  f.node('workflowAppFillers').value='inherit';f.node('workflowAppChineseFillers').value='false';
  f.node('workflowAdd').onclick();f.node('workflowSave').onclick();
  const p=f.sent.at(-1).preferences;assert.equal(p.remove_fillers,true);assert.equal(p.chinese_fillers,true);
  assert.equal(p.diagnostics,false);assert.equal(p.profiles[0].remove_fillers,null);assert.equal(p.profiles[0].chinese_fillers,false);
});
