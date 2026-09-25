import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
const html=readFileSync(new URL('../../vocalcode-app/src/webui.html',import.meta.url),'utf8');
function slice(from,to){
  const start=html.indexOf(from), end=html.indexOf(to,start);
  assert.ok(start>=0 && end>start,from);
  return html.slice(start,end);
}
function nodes(){
  const map=new Map();
  const node=id=>{
    if(!map.has(id)) map.set(id,{id,style:{},dataset:{},hidden:false,disabled:false,textContent:'',
      classList:{toggle(){},remove(){},add(){}}});
    return map.get(id);
  };
  return {map,node};
}
test('status rendering shows no plan, licence or Pro state, even if a host still sends one',()=>{
  const source=slice('  window.vocalcodeStatus = function(s){','  window.vocalcodeSettingsResult');
  const {map,node}=nodes();
  const ctx={document:{getElementById:node},window:{vocalcodeNoiseFilterStatus(){}},t:x=>x,
    setTextIfChanged(el,v){el.textContent=v;},announceSetup(){},setOnboarding(){},
    renderSetup(){} /* the model setup banner has its own tests */};
  vm.runInNewContext(source,ctx);
  const legacy={ready:true,license:'Basic',license_kind:'basic',license_days:3,pro:false,trial_setup_error:true};
  ctx.window.vocalcodeStatus({...legacy,crash_notice:{at:1,version:'1.4.0'}});
  assert.equal(node('crashNotice').hidden,false);
  for(const id of ['planBadge','usageLine','licText','licBadge','licBuy','licOwned','licCommunity','correctionWindow','meetingTitle'])
    assert.equal(map.has(id),false,id);
  ctx.window.vocalcodeStatus({...legacy,crash_notice:null});
  assert.equal(node('crashNotice').hidden,true);
});
test('About & help and the crash notice only name fixed host actions',()=>{
  const source=slice('  // About & help. The page only names','  // Uninstall.');
  const {node}=nodes(), sent=[];
  vm.runInNewContext(source,{document:{getElementById:node},send:m=>sent.push(JSON.parse(JSON.stringify(m))),Date:{now:()=>7}});
  for(const [id,target] of [['aboutSource','source'],['aboutPrivacy','privacy'],['aboutReport','report']]){
    node(id).onclick();
    assert.deepEqual(sent.at(-1),{type:'open_info',target});
  }
  node('aboutLogs').onclick(); assert.deepEqual(sent.at(-1),{type:'open_log_folder'});
  node('crashLogs').onclick(); assert.deepEqual(sent.at(-1),{type:'open_log_folder'});
  node('aboutDiagnostics').textContent='Copy diagnostics';
  node('aboutDiagnostics').onclick();
  assert.deepEqual(sent.at(-1),{type:'copy_diagnostics',id:'diagnostics-7'});
  assert.equal(node('aboutDiagnostics').dataset.copyRequest,'diagnostics-7');
  sent.length=0;
  node('crashReport').onclick();
  assert.deepEqual(sent,[{type:'open_info',target:'report'},{type:'crash_notice_dismiss'}]);
  assert.equal(node('crashNotice').hidden,true);
  assert.doesNotMatch(source,/https?:|mailto:/);
});
test('community page carries no commerce UI or copy',()=>{
  for(const forbidden of [/planBadge|planbtn|data-pro-badge|hasPro/,/licBuy|licOwned|licCommunity|licNote|rEmail|shareLink/,
    /data-panel="license"/,/type:"(activate|buy|restore)"/,/\$4\.99|4,99/,/support@vocalcode\.app|vocalcode\.app\/privacy/,
    /included in Pro|Upgrade to Pro|trial days|licensing still connect/,/vocalcodeActivated/])
    assert.doesNotMatch(html,forbidden);
  assert.match(html,/<div class="panel" data-panel="about">/);
  assert.match(html,/<p>Free and open source \(AGPL-3\.0\)<\/p>/);
});
test('every interface language translates every string',()=>{
  const from=html.indexOf('const DICTS = ')+'const DICTS = '.length;
  const to=html.indexOf('\n  };',from)+4;
  const dicts=vm.runInNewContext('('+html.slice(from,to)+')');
  const reference=Object.keys(dicts.zh).sort();
  assert.deepEqual(Object.keys(dicts).sort(),['de','es','fr','zh']);
  for (const language of ['es','fr','de']) assert.deepEqual(Object.keys(dicts[language]).sort(),reference,language);
  for (const key of ['About & help','Free and open source (AGPL-3.0)','Report a problem','Open log folder','Copy diagnostics',
    'VocalCode closed unexpectedly last time','Report on GitHub'])
    for (const language of ['zh','es','fr','de']) assert.ok(dicts[language][key],`${language}: ${key}`);
  for (const key of reference) assert.doesNotMatch(key,/\bPro\b|licen[cs]e|trial|checkout|purchase|\$4\.99/i,key);
});
