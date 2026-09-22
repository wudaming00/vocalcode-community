import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
const html=readFileSync(new URL('../../vocalcode-app/src/webui.html',import.meta.url),'utf8');
const start=html.indexOf('    if(s.license){');
const end=html.indexOf('  window.vocalcodeActivated',start);
assert.ok(start>=0 && end>start);
const source=html.slice(start,end).replace(/\s*};\s*$/,'');
function render(kind,pro){
  const nodes=new Map();
  const node=id=>{if(!nodes.has(id))nodes.set(id,{style:{},disabled:false,textContent:''});return nodes.get(id);};
  vm.runInNewContext(source,{s:{license:'available',license_kind:kind,license_days:10},hasPro:pro,t:x=>x,document:{getElementById:node},renderMeetings(){}});
  return node;
}
test('community displays free local features without commerce or paid updater',()=>{
  const node=render('community',true);
  assert.equal(node('planBadge').textContent,'FREE');
  assert.equal(node('licBuy').style.display,'none');
  assert.equal(node('licOwned').style.display,'none');
  assert.equal(node('licCommunity').style.display,'');
  assert.equal(node('updBtn').disabled,true);
  assert.equal(node('correctionWindow').disabled,false);
  assert.match(node('updNote').textContent,/manual updates/);
});
test('legacy paid and basic UI behavior is unchanged',()=>{
  let node=render('licensed',true);
  assert.equal(node('planBadge').textContent,'PRO');
  assert.equal(node('licOwned').style.display,'');
  assert.equal(node('licCommunity').style.display,'none');
  assert.equal(node('updBtn').disabled,false);
  node=render('basic',false);
  assert.equal(node('licBuy').style.display,'');
  assert.equal(node('correctionWindow').disabled,true);
});
