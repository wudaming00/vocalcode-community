import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const html=readFileSync(new URL('../../vocalcode-app/src/webui.html',import.meta.url),'utf8');
const start=html.indexOf('  var runtimeErrorQueue=[]');
const end=html.indexOf('  window.vocalcodeCopyResult',start);
assert.ok(start>=0 && end>start, 'runtime error queue source not found');
const source=html.slice(start,end);

function setup(){
  const shown=[];
  const timers=[];
  const ctx={
    window:{},
    t:s=>s,
    toast:(msg,ms)=>shown.push({msg,ms}),
    setTimeout:(fn,ms)=>{timers.push({fn,ms});return timers.length;},
  };
  vm.createContext(ctx);
  vm.runInContext(source,ctx);
  // Fire the next pending timer, as the browser would after its delay.
  const tick=()=>{const next=timers.shift(); assert.ok(next,'no timer pending'); next.fn(); return next.ms;};
  return {report:ctx.window.vocalcodeRuntimeError, shown, timers, tick};
}

test('errors that arrive together are each shown in order, none replaced',()=>{
  const ui=setup();
  ui.report('Microphone stopped responding: device removed');
  ui.report('VocalCode could not complete that action: audio device error');
  assert.deepEqual(ui.shown.map(s=>s.msg),['Microphone stopped responding: device removed']);
  ui.tick();
  assert.deepEqual(ui.shown.map(s=>s.msg),[
    'Microphone stopped responding: device removed',
    'VocalCode could not complete that action: audio device error',
  ]);
  ui.tick();
  // Idle again: the next error shows immediately.
  ui.report('Could not load the speech model: offline');
  assert.equal(ui.shown.length,3);
});

test('each error stays up long enough to read, and longer for longer text',()=>{
  const ui=setup();
  ui.report('Short.');
  ui.tick();
  ui.report('x'.repeat(160));
  assert.ok(ui.shown[0].ms>=4000, 'even a short error outlasts the 2.8 s toast');
  assert.ok(ui.shown[1].ms>ui.shown[0].ms);
  assert.ok(ui.shown[1].ms<=10000);
  assert.ok(ui.timers[0].ms>ui.shown[1].ms, 'the next error waits for this one to fade');
});

test('the page queue is bounded and does not repeat a waiting message',()=>{
  const ui=setup();
  ui.report('first');
  for(let i=0;i<20;i++) ui.report('flood '+i);
  ui.report('flood 19');
  let turns=0;
  while(ui.timers.length){ ui.tick(); turns++; }
  const later=ui.shown.slice(1).map(s=>s.msg);
  assert.equal(later.length,8);
  assert.deepEqual(later,Array.from({length:8},(_,i)=>'flood '+(12+i)));
  assert.equal(turns,9);
});

test('an empty report still says something, as text',()=>{
  const ui=setup();
  ui.report('');
  assert.equal(ui.shown[0].msg,'VocalCode could not complete that action.');
  assert.doesNotMatch(source,/innerHTML/);
});
