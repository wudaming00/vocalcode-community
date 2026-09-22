import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
const html=readFileSync(new URL('../../vocalcode-app/src/meeting_prompt.html',import.meta.url),'utf8');
const code=html.match(/<script>([\s\S]*?)<\/script>/)[1];
function setup(){
  class El {constructor(){this.textContent='';this.disabled=false;this.attrs={};}setAttribute(k,v){this.attrs[k]=v;}set innerHTML(_){throw Error('Unsafe markup');}}
  const nodes=new Map(),events=new Map(),sent=[],timers=new Map();let sequence=0,now=0;
  const el=id=>{if(!nodes.has(id))nodes.set(id,new El());return nodes.get(id);};
  const ctx={window:{ipc:{postMessage:s=>sent.push(JSON.parse(s))}},navigator:{language:'ko-KR'},document:{documentElement:{lang:''},getElementById:el,querySelectorAll:()=>['close','review','dismiss','snooze'].map(el),addEventListener:(event,handler)=>events.set(event,handler)},setTimeout:(fn,ms)=>{timers.set(++sequence,{fn,ms});return sequence;},clearTimeout:id=>timers.delete(id),Date:{now:()=>now}};
  vm.createContext(ctx);vm.runInContext(code,ctx);
  return{ctx,el,events,sent,timers,advance:n=>now+=n,show:p=>ctx.window.showReminder({id:1,app:'Google Meet',title:'Weekly sync',kind:'confirmed',language:'en',...p})};
}
test('readiness and showing a reminder never request capture',()=>{const s=setup();s.show();assert.deepEqual(s.sent,[{type:'ready'}]);assert.equal(s.timers.size,1);});

test('auto end is acknowledged once and countdown does not use the reminder dismissal timer',()=>{
  const s=setup();const p={id:40,minutes:5,seconds:30,language:'zh'};
  s.ctx.window.showAutoEnd(p);
  assert.deepEqual(s.sent.at(-1),{type:'auto_end_action',id:40,action:'visible'});
  assert.match(s.el('heading').textContent,/会议/);assert.equal(s.timers.size,0);
  s.ctx.window.showAutoEnd({...p,seconds:29});
  assert.equal(s.sent.length,2);assert.match(s.el('context').textContent,/29/);
  s.events.get('mouseleave')();assert.equal(s.timers.size,0);
});

test('all auto end buttons send scoped controls; close and escape keep recording',()=>{
  for(const [button,action] of [['review','continue'],['snooze','stop'],['dismiss','disable'],['close','continue'],['escape','continue']]) {
    const s=setup();s.ctx.window.showAutoEnd({id:41,minutes:5,seconds:30,language:'en'});
    if(button==='escape')s.events.get('keydown')({key:'Escape'});else s.el(button).onclick();
    assert.deepEqual(s.sent.at(-1),{type:'auto_end_action',id:41,action});
    assert.equal(s.el('review').disabled,true);
    s.ctx.window.hideReminder();s.show({id:9});s.el('review').onclick();
    assert.deepEqual(s.sent.at(-1),{type:'reminder_action',id:9,action:'review'});
  }
});

test('auto end IPC failures recover controls without claiming nothing is being recorded',()=>{
  const s=setup();s.ctx.window.showAutoEnd({id:41,minutes:5,seconds:30,language:'en'});
  s.el('review').onclick();[...s.timers.values()][0].fn();
  assert.equal(s.el('review').disabled,false);assert.match(s.el('privacy').textContent,/try again/);
  assert.doesNotMatch(s.el('privacy').textContent,/Nothing is recorded/);
});
test('review is token bound and double clicks emit only once',()=>{const s=setup();s.show({id:42});s.el('review').onclick();s.el('review').onclick();assert.deepEqual(s.sent.at(-1),{type:'reminder_action',id:42,action:'review'});assert.equal(s.sent.length,2);assert.equal(s.timers.size,1);assert.equal([...s.timers.values()][0].ms,2500);});
test('snooze and dismissal never masquerade as review',()=>{for(const [button,action]of[['snooze','snooze'],['close','dismiss'],['dismiss','dismiss']]){const s=setup();s.show();s.el(button).onclick();assert.equal(s.sent.at(-1).action,action);}});
test('hover pauses timeout and timeout dismisses without starting anything',()=>{const s=setup();s.show();s.advance(1200);s.events.get('mouseenter')();assert.equal(s.timers.size,0);s.events.get('mouseleave')();assert.equal([...s.timers.values()][0].ms,18800);[...s.timers.values()][0].fn();assert.equal(s.sent.at(-1).action,'dismiss');});
test('hidden reminders cancel callbacks and cannot emit stale actions',()=>{const s=setup();s.show();s.ctx.window.hideReminder();s.el('review').onclick();assert.deepEqual(s.sent,[{type:'ready'}]);assert.equal(s.timers.size,0);});
test('Escape dismisses once and a later reminder has working buttons again',()=>{const s=setup();s.show();s.events.get('keydown')({key:'Escape'});assert.equal(s.sent.at(-1).action,'dismiss');assert.equal(s.el('close').disabled,true);s.ctx.window.hideReminder();s.show({id:2});assert.equal(s.el('close').disabled,false);s.el('close').onclick();assert.deepEqual(s.sent.at(-1),{type:'reminder_action',id:2,action:'dismiss'});assert.equal(s.sent.length,3);});
test('dismissal cancels automatic dismissal and ignores clicks while awaiting acknowledgement',()=>{const s=setup();s.show();s.el('close').onclick();assert.equal(s.timers.size,1);assert.equal([...s.timers.values()][0].ms,2500);s.el('dismiss').onclick();s.el('snooze').onclick();s.events.get('keydown')({key:'Escape'});assert.equal(s.sent.length,2);});
test('UI copy covers Chinese Japanese Korean and default locale',()=>{for(const [language,heading]of[['zh','检测到会议'],['ja','会議を検出しました'],['ko','회의가 감지되었습니다'],['auto','회의가 감지되었습니다']]){const s=setup();s.show({language});assert.equal(s.el('heading').textContent,heading);}});
test('untrusted strings are rendered as bounded text and uncertainty is visible',()=>{const s=setup();s.show({app:'<img src=x onerror=alert(1)>',kind:'possible'});assert.equal(s.el('context').textContent,'<img src=x onerror=alert(1)>');assert.equal(s.el('heading').textContent,'Possible meeting');s.show({kind:'calendar',title:'a'.repeat(1000)});assert.equal(s.el('context').textContent.length,160);assert.equal(s.el('heading').textContent,'Scheduled meeting');});
test('invalid tokens do not create cards',()=>{const s=setup();for(const id of [0,-1,1.2,'2',Number.MAX_SAFE_INTEGER+1])s.show({id});assert.equal(s.timers.size,0);});
test('review on main page only opens controls, without starting capture',()=>{const main=readFileSync(new URL('../../vocalcode-app/src/webui.html',import.meta.url),'utf8');const action=main.slice(main.indexOf('  window.vocalcodeOpenMeetingReview=function'),main.indexOf('  document.getElementById("meetingReminderLater").onclick'));assert.match(action,/showPanel\("meetings",true\)/);assert.doesNotMatch(action,/send\(|meeting_start/);});

test('lost IPC acknowledgement restores actionable buttons with feedback',()=>{const s=setup();s.show();s.el('review').onclick();s.events.get('mouseleave')();assert.equal(s.timers.size,1);[...s.timers.values()][0].fn();assert.equal(s.el('review').disabled,false);assert.match(s.el('privacy').textContent,/try again/);assert.equal(s.timers.size,0);s.el('review').onclick();assert.equal(s.sent.length,3);assert.equal(s.sent.at(-1).id,1);});
test('IPC exceptions do not strand disabled controls',()=>{const s=setup();s.show({language:'zh'});s.ctx.window.ipc.postMessage=()=>{throw Error('unavailable');};s.el('snooze').onclick();assert.equal(s.el('close').disabled,false);assert.match(s.el('privacy').textContent,/重试/);assert.equal(s.timers.size,0);});
test('acknowledgement cancels watchdog and stale callbacks cannot alter a new card',()=>{const s=setup();s.show();s.el('review').onclick();const old=[...s.timers.values()][0].fn;s.ctx.window.hideReminder();assert.equal(s.timers.size,0);s.show({id:2});s.el('snooze').onclick();old();assert.equal(s.el('snooze').disabled,true);assert.equal(s.timers.size,1);assert.doesNotMatch(s.el('privacy').textContent,/try again/);s.ctx.window.hideReminder();assert.equal(s.timers.size,0);});
test('explicit native actions are not conditional on busy settings or recording state',()=>{const main=readFileSync(new URL('../../vocalcode-app/src/webui.rs',import.meta.url),'utf8');const action=main.slice(main.indexOf('UserEvent::MeetingPrompt(crate::meeting_prompt::Event::Action(id, action)) =>'),main.indexOf('if let Some(o) = overlay.as_mut()'));assert.match(action,/prompt\.apply_action/);assert.doesNotMatch(action,/try_lock|status\.listening|status\.meetings\.is_active/);});
