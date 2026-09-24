import test from 'node:test';import assert from 'node:assert/strict';import{readFileSync}from'node:fs';import vm from'node:vm';
const html=readFileSync(new URL('../../vocalcode-app/src/webui.html',import.meta.url),'utf8');
const code=html.slice(html.indexOf('  let calendarBusy='),html.indexOf('  let workflowBusy='));
function setup(translate=s=>s){class El{constructor(){this.children=[];this.value='';this.textContent='';}append(...c){this.children.push(...c);}replaceChildren(){this.children=[];}focus(){}set innerHTML(_){throw Error('Unsafe markup');}}
const nodes=new Map(),sent=[];const node=id=>{if(!nodes.has(id))nodes.set(id,new El());return nodes.get(id);};
const ctx={document:{getElementById:node,createElement:()=>new El()},window:{},t:translate,meetingState:{active:false,detail:{id:'one'}},confirm:()=>true,send:r=>{sent.push(r);return true;}};
vm.createContext(ctx);vm.runInContext(code,ctx);return{node,sent,ctx,receive(data){ctx.window.vocalcodeCalendarResult({id:sent.at(-1).id,ok:true,data});}};}
test('reviewing a title cannot start capture; links are host-resolved keys',()=>{const f=setup();f.node('calendarLoad').onclick();f.receive({events:[{key:'key',title:'<script>bad()</script>',start_ms:1,end_ms:2,join_url:'https://meet.google.com/a'}]});const row=f.node('calendarRows').children[0];row.children[1].onclick();assert.equal(f.node('meetingTitle').value,'<script>bad()</script>');assert.ok(!f.sent.some(r=>r.type==='meeting_start'));row.children[2].onclick();assert.equal(f.sent.at(-1).key,'key');assert.ok(!('url'in f.sent.at(-1)));});
test('association requires loading the exact selected meeting revision',()=>{const f=setup();f.node('calendarLink').onclick();assert.equal(f.sent.length,0);f.node('calendarLinked').onclick();f.receive({meeting_id:'one',revision:'rev',linked_event:null});f.ctx.meetingState.detail.id='two';f.node('calendarLink').onclick();assert.equal(f.sent.length,1);f.ctx.meetingState.detail.id='one';f.node('calendarLink').onclick();assert.equal(f.sent.at(-1).revision,'rev');});
test('Chinese calendar controls explain metadata access and separate recording consent',()=>{
  assert.ok(html.includes('"Upcoming calendar meetings · optional Beta":"即将开始的日历会议 · 可选测试版"'));
  assert.ok(html.includes('日历中有会议，不代表你已经加入或同意录音。'));
  assert.ok(html.includes('不发送录音或笔记。只读访问'));
  for(const label of ['Sign in with Google','Cancel request','Disconnect locally','Refresh upcoming events','Load saved snapshot','Event to associate','Remove association']){
    assert.ok(new RegExp('"'+label+'":"[^"a-zA-Z]+').test(html),label);
  }
});
test('calendar dynamic status and actions use translation without translating event content',()=>{
  const f=setup(s=>'translated:'+s);
  f.node('calendarLoad').onclick();
  assert.equal(f.node('calendarMessage').textContent,'translated:Loading calendar…');
  f.receive({configured:false,events:[{key:'one',title:'Original title',start_ms:1,end_ms:2,join_url:'https://meet.google.com/a'}]});
  assert.equal(f.node('calendarMessage').textContent,'translated:Not configured; translated:no event snapshot yet.');
  const row=f.node('calendarRows').children[0];
  assert.ok(row.children[0].textContent.startsWith('Original title'));
  assert.equal(row.children[1].textContent,'translated:Use title & review sources');
  assert.equal(row.children[2].textContent,'translated:Open meeting');
});
