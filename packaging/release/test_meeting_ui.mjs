import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
const html=readFileSync(new URL('../../vocalcode-app/src/webui.html',import.meta.url),'utf8');
const controller=html.slice(html.indexOf('  var meetingState='),html.indexOf('  // ── History'));
function setup(){
  class Element {
    constructor(tag=''){this.tagName=tag;this.attributes={};this.children=[];this.style={};this.dataset={};this.value='';this.checked=false;this.disabled=false;this.text='';this.classList={add(){},remove(){},toggle(){}};}
    set textContent(v){this.text=String(v);this.children=[];} get textContent(){return this.text;}
    appendChild(e){this.children.push(e);} setAttribute(k,v){this.attributes[k]=v;} focus(){}
    set innerHTML(_){throw Error('Unsafe meeting markup');}
  }
  const nodes=new Map(),sent=[],timers=[];
  const node=id=>{if(!nodes.has(id))nodes.set(id,new Element());return nodes.get(id);};
  const ctx={document:{getElementById:node,createElement:tag=>new Element(tag)},window:{},t:x=>x,
    send:m=>sent.push(m),toast(){},showPanel(){},cfg:{},persist(){},setTimeout:()=>1,clearTimeout(){},setInterval:f=>timers.push(f),prompt:()=>'',confirm:()=>false};
  vm.createContext(ctx);vm.runInContext(controller,ctx);
  return {node,sent,ctx,tick:()=>timers.forEach(f=>f()),receive:s=>ctx.window.vocalcodeMeetings(s)};
}
const meeting={id:'1787796747000-1-1',title:'Live test',created_at_ms:1787796747000,started_at_ms:1787796747000,duration_ms:12000,status:'recording',segments:[]};

test('speaker rename is keyboard-accessible and bound to the selected meeting',()=>{
  const f=setup();f.receive({active:false,detail:{...meeting,segments:[{id:1,start_ms:0,text:'Synthetic text',speaker_id:'a'}],speakers:[{id:'a',label:'<img onerror=bad()>'}]}});
  const speaker=f.node('meetingDetail').children.at(-1).children[0].children[1];
  assert.equal(speaker.tagName,'button');assert.equal(speaker.type,'button');
  assert.match(speaker.attributes['aria-label'],/rename this speaker/);
  assert.equal(speaker.textContent,'<img onerror=bad()>');assert.deepEqual(f.sent,[]);
  f.ctx.prompt=()=> ' QA participant ';speaker.onclick();
  assert.equal(f.sent.at(-1).type,'meeting_rename_speaker');assert.equal(f.sent.at(-1).id,meeting.id);
  assert.equal(f.sent.at(-1).speaker_id,'a');assert.equal(f.sent.at(-1).label,'QA participant');
});

test('narrow meetings stack the list above a real transcript and wrap tools',()=>{
  assert.match(html,/@media\(max-width:640px\)\{[\s\S]*?\.meeting-workspace\{grid-template-columns:minmax\(0,1fr\);grid-template-rows:130px minmax\(260px,1fr\)/);
  assert.doesNotMatch(html,/\.meeting-tools\{flex-wrap:nowrap\}/);
  assert.match(html,/\.meeting-detail\{[^}]*overflow-wrap:anywhere/);
});
test('browsing an old meeting does not change the live banner',()=>{
  const f=setup(); f.receive({active:true,recording:meeting,detail:{...meeting,title:'Old meeting'},meetings:[]});
  assert.equal(f.node('meetingLiveTitle').textContent,'Live test');
  assert.equal(f.node('meetingStop').disabled,false);
});
test('Stop freezes the duration while model work finishes',()=>{
  const f=setup();f.receive({active:true,transcribing:true,recording:{...meeting,status:'processing',stopping:true},detail:meeting});
  f.tick();assert.equal(f.node('meetingLiveMeta').textContent,'00:12 · Processing');
  assert.equal(f.node('meetingStop').disabled,true);
});
test('an audio import remains cancellable, unlike already-stopped live capture',()=>{
  const f=setup();f.receive({active:true,recording:{...meeting,status:'processing',stopping:false},detail:meeting});
  assert.equal(f.node('meetingStop').disabled,false);
  f.tick();assert.equal(f.node('meetingLiveMeta').textContent,'00:12 · Import audio');
  f.node('meetingStop').onclick();assert.equal(f.sent.at(-1).type,'meeting_stop');
});
test('saved transcripts and titles are inert text and use only explicit copy',()=>{
  const f=setup();f.receive({active:false,detail:{...meeting,title:'<img onerror=bad()>',status:'completed',segments:[{id:1,start_ms:0,text:'<script>bad()</script>',speaker_id:'you'}]}});
  assert.equal(f.sent.length,0);
  const transcript=f.node('meetingDetail').children.at(-1), row=transcript.children[0];
  assert.equal(row.children[2].textContent,'<script>bad()</script>');
  row.children[3].onclick();assert.equal(f.sent.at(-1).type,'copy');
  assert.equal(f.sent.at(-1).text,'<script>bad()</script>');
});

test('reading toggle copies derived text but exports remain original and selection resets view',()=>{
  const f=setup();
  const detail={...meeting,status:'completed',segments:[{id:1,start_ms:0,text:'Um, retry.',reading_text:'Retry.',filler_removed:1,speaker_id:'you'}]};
  const source=JSON.stringify(detail);f.receive({active:false,detail});
  const tools=()=>f.node('meetingDetail').children.find(e=>e.className==='meeting-tools');
  let row=f.node('meetingDetail').children.at(-1).children[0];
  assert.equal(row.children[2].textContent,'Retry.');row.children[3].onclick();
  assert.equal(f.sent.at(-1).text,'Retry.');
  const picker=tools().children[0];picker.value='txt';picker.onchange();
  assert.equal(f.sent.at(-1).type,'meeting_export');assert.equal(f.sent.at(-1).format,'txt');
  assert.equal(JSON.stringify(detail),source);
  tools().children.find(e=>e.className.includes('meeting-reading')).onclick();
  row=f.node('meetingDetail').children.at(-1).children[0];assert.equal(row.children[2].textContent,'Um, retry.');
  f.receive({active:false,detail:{...detail,id:'different'}});
  row=f.node('meetingDetail').children.at(-1).children[0];assert.equal(row.children[2].textContent,'Retry.');
});

test('default view hides punctuation only and keeps short answers with an original toggle',()=>{
  const f=setup();const detail={...meeting,status:'completed',segments:[
    {id:1,start_ms:0,text:'。',noise_only:true},
    {id:2,start_ms:1000,text:'不。',noise_only:false},
    {id:3,start_ms:2000,text:'OK',noise_only:false}]};
  f.receive({active:false,detail});
  const rows=()=>f.node('meetingDetail').children.at(-1).children;
  assert.equal(rows().length,2);assert.equal(rows()[0].children[2].textContent,'不。');
  f.node('meetingDetail').children.find(e=>e.className==='meeting-tools').children.find(e=>e.className.includes('meeting-reading')).onclick();
  assert.equal(rows().length,3);assert.equal(rows()[0].children[2].textContent,'。');
});

test('auto end setting is explicit and countdown continuation is token bound',()=>{
  const f=setup();f.node('meetingMic').checked=true;f.node('meetingAutoEnd').value='0';
  f.node('meetingStart').onclick();assert.equal(f.sent.at(-1).auto_end_minutes,0);
  f.receive({active:true,recording:meeting,detail:meeting,auto_end:{id:42,remaining_seconds:27}});
  assert.equal(f.node('meetingAutoEndNotice').hidden,false);
  assert.match(f.node('meetingAutoEndText').textContent,/27s/);
  f.node('meetingContinue').onclick();assert.equal(f.sent.at(-1).type,'meeting_auto_end_continue');assert.equal(f.sent.at(-1).id,42);
  f.receive({active:false,detail:meeting});assert.equal(f.node('meetingAutoEndNotice').hidden,true);
});

test('meetings are never gated on a paid plan',()=>{
  // No plan flag exists in the page, so the controller must run without one.
  const f=setup();assert.equal('hasPro' in f.ctx,false);
  f.receive({active:false,meetings:[]});
  for(const id of ['meetingTitle','meetingMic','meetingSystem','meetingKeepAudio','meetingAutoEnd']) assert.equal(f.node(id).disabled,false,id);
  f.node('meetingMic').checked=true;f.node('meetingStart').onclick();assert.equal(f.sent.at(-1).type,'meeting_start');
  f.node('meetingImport').onclick();assert.equal(f.sent.at(-1).type,'meeting_import');
  assert.doesNotMatch(controller,/hasPro|included in Pro|showPanel\("license"/);
});
