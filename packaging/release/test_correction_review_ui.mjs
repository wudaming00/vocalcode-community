import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
const html=readFileSync(new URL('../../vocalcode-app/src/correction_review.html',import.meta.url),'utf8');
const script=html.match(/<script>([\s\S]*?)<\/script>/)[1];
function setup(){
  const nodes=new Map(),sent=[],timers=new Map();let sequence=0;
  const node=id=>{if(!nodes.has(id))nodes.set(id,{value:'',disabled:false,textContent:'',title:'',addEventListener(){},focus(){}});return nodes.get(id)};
  const ctx={document:{getElementById:node},window:{ipc:{postMessage:s=>sent.push(JSON.parse(s))}},setTimeout:(f,ms)=>{const id=++sequence;timers.set(id,{f,ms});return id},clearTimeout:id=>timers.delete(id)};
  vm.createContext(ctx);vm.runInContext(script,ctx);sent.length=0;
  return {node,sent,ctx,show:r=>ctx.window.vocalcodeCorrectionReview(r),expire(){const jobs=[...timers.values()];timers.clear();jobs.forEach(t=>{assert.equal(t.ms,4000);t.f()})},ack(ok=true,conflict=false){ctx.window.vocalcodeCorrectionSaveResult({request_id:sent.findLast(s=>s.type==='save_dict').request_id,ok,conflict,msg:'response'})}};
}
const proposal=()=>({ok:true,review_only:true,revision:'original',rules:[],changes:[{from:'in',to:'linkedin',previous:null}]});
test('risky proposal timeout and Skip never commit a rule',()=>{
  for(const action of ['timeout','skip']){const f=setup();f.show(proposal());assert.equal(f.node('keep').textContent,'Save rule');if(action==='timeout')f.expire();else f.node('undo').onclick();assert.deepEqual(f.sent,[{type:'correction_popup_close'}]);}
});
test('explicit proposal confirmation waits for matching successful save',()=>{
  const f=setup();f.show(proposal());f.node('keep').onclick();assert.equal(f.sent.length,1);assert.equal(f.sent[0].type,'save_dict');assert.deepEqual(f.sent[0].rules,[['in','linkedin']]);assert.equal(f.node('keep').disabled,true);
  f.ctx.window.vocalcodeCorrectionSaveResult({request_id:0,ok:true});assert.equal(f.sent.length,1);
  f.ack();assert.equal(f.sent.at(-1).type,'correction_popup_close');
});
test('safe auto-learned rule keeps four-second dismissal without another write',()=>{
  const f=setup();const r=proposal();r.review_only=false;r.rules=[['in','linkedin']];f.show(r);f.expire();assert.deepEqual(f.sent,[{type:'correction_popup_close'}]);
});
test('undo safe auto-learning waits for persistence',()=>{
  const f=setup();const r=proposal();r.review_only=false;r.rules=[['in','linkedin']];f.show(r);f.node('undo').onclick();assert.deepEqual(f.sent[0].rules,[]);assert.equal(f.sent.length,1);f.ack();assert.equal(f.sent.at(-1).type,'correction_popup_close');
});
test('batch corrections can all be inspected and edited before explicit save',()=>{
  const f=setup();const r=proposal();r.changes.push({from:'ts',to:'tavus'});f.show(r);f.node('next').onclick();assert.equal(f.node('from').value,'ts');assert.match(f.node('label').textContent,/2\/2/);f.node('to').value='TypeScript';f.node('keep').onclick();assert.deepEqual(f.sent[0].rules,[['in','linkedin'],['ts','TypeScript']]);
});
test('inverse correction suggestion disables original rule only after confirmation',()=>{
  const f=setup();f.show({ok:true,review_only:true,revision:'r',rules:[['考虑','Collie']],changes:[{from:'考虑',to:'考虑',previous:['考虑','Collie']}]});assert.equal(f.sent.length,0);f.node('keep').onclick();assert.deepEqual(f.sent[0].rules,[['考虑','考虑']]);
});
test('save failure remains visible; conflict never rebases a stale proposal',()=>{
  const f=setup();f.show(proposal());f.node('keep').onclick();f.ack(false,false);assert.match(f.node('label').textContent,/Not saved/);assert.equal(f.sent.length,1);f.node('keep').onclick();f.ack(false,true);assert.match(f.node('label').textContent,/rules changed/);assert.equal(f.node('keep').disabled,true);f.node('keep').onclick();assert.equal(f.sent.length,2);
});
test('new proposal waits behind pending save without closing or changing its revision',()=>{
  const f=setup();f.show(proposal());f.node('keep').onclick();const next=proposal();next.revision='new';next.changes=[{from:'map',to:'mem'}];f.show(next);f.ack();assert.equal(f.node('from').value,'map');assert.equal(f.sent.length,1);f.node('keep').onclick();assert.equal(f.sent[1].revision,'new');
});
