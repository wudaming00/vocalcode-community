import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
const html=readFileSync(new URL('../../vocalcode-app/src/webui.html',import.meta.url),'utf8');
const section=html.split('// MODEL SETUP BANNER:')[1].split('// END MODEL SETUP BANNER')[0];
const script=section.slice(section.indexOf('var setupFailure'));

function setup({now=1_000_000, language='en', model=''}={}){
  const nodes=new Map();
  const node=id=>{
    if(!nodes.has(id)){
      const classes=new Set();
      const el={id,style:{},textContent:'',title:'',disabled:false,
        classList:{toggle(name,on){if(on)classes.add(name);else classes.delete(name);},contains:name=>classes.has(name)}};
      // Host error text must stay inert.
      Object.defineProperty(el,'innerHTML',{set(){throw Error('unsafe HTML');}});
      nodes.set(id,el);
    }
    return nodes.get(id);
  };
  const sent=[], saved=[], announced=[], timers=[];
  const cfg={language,model};
  const ctx={
    document:{getElementById:node},
    t:s=>s,
    cfg,
    MODEL_CHOICES:{en:[{id:'sensevoice',name:'SenseVoice'},{id:'parakeet-tdt-v3',name:'Parakeet TDT v3'}]},
    setTextIfChanged:(el,value)=>{el.textContent=String(value);},
    announceSetup:value=>announced.push(value),
    // Messages cross the vm realm; compare them as the JSON the host receives.
    send:message=>{sent.push(JSON.parse(JSON.stringify(message)));return true;},
    save:()=>saved.push({...cfg}),
    renderLangs(){},
    showLanguageApplyNote(){},
    setInterval:fn=>{timers.push(fn);return timers.length;},
    clearInterval:id=>{timers[id-1]=null;},
    Date:{now:()=>clock.now},
  };
  const clock={now};
  vm.createContext(ctx);
  vm.runInContext(script,ctx);
  return {node,sent,saved,announced,timers,cfg,clock,render:s=>vm.runInContext('renderSetup',ctx)(s),
    tick:()=>timers.forEach(fn=>fn&&fn())};
}

const failure={ready:false,model_error:'download encoder.onnx: no data received for 90s: Connection failed',
  retry_at_ms:1_025_000,model_downloaded_mb:312.4,model_total_mb:671.2,smaller_model:{id:'sensevoice',mb:239.5}};

test('a failed model download shows a red banner with what is kept, why, and when it retries',()=>{
  const ui=setup();
  ui.render(failure);
  const banner=ui.node('setup');
  assert.ok(banner.classList.contains('on'));
  assert.ok(banner.classList.contains('error'),'the existing .setup.error style must be applied');
  assert.equal(ui.node('setupMsg').textContent,'The speech model could not be set up');
  assert.equal(ui.node('setupDetail').style.display,'');
  assert.match(ui.node('setupDetail').textContent,/^312 of 671 MB downloaded — the next try continues from there\.\n/);
  assert.match(ui.node('setupDetail').textContent,/no data received for 90s: Connection failed$/);
  assert.equal(ui.node('dlTrack').style.display,'none');
  assert.equal(ui.node('dlPct').textContent,'Retrying in 25 s');
  ui.clock.now+=10_000; ui.tick();
  assert.equal(ui.node('dlPct').textContent,'Retrying in 15 s','the countdown runs on the page clock');
  ui.clock.now+=20_000; ui.tick();
  assert.equal(ui.node('dlPct').textContent,'Retrying…');
  assert.equal(ui.node('setupActions').style.display,'');
  assert.equal(ui.node('setupSmaller').style.display,'');
  assert.equal(ui.node('setupSmaller').title,'SenseVoice · 240 MB');
  assert.deepEqual(ui.announced.at(-1),'The speech model could not be set up');
});

test('Retry now asks the host once and waits for the next outcome',()=>{
  const ui=setup();
  ui.render(failure);
  ui.node('setupRetry').onclick.call(ui.node('setupRetry'));
  assert.deepEqual(ui.sent,[{type:'retry_model'}]);
  assert.equal(ui.node('setupRetry').disabled,true);
  assert.equal(ui.node('dlPct').textContent,'Retrying…');
  ui.render(failure);
  assert.equal(ui.node('setupRetry').disabled,true,'the same failure does not re-arm the button');
  ui.render({...failure,retry_at_ms:failure.retry_at_ms+60_000});
  assert.equal(ui.node('setupRetry').disabled,false,'a new failure does');
});

test('Use a smaller model saves the smaller route through the normal settings path',()=>{
  const ui=setup();
  ui.render(failure);
  ui.node('setupSmaller').onclick();
  assert.equal(ui.cfg.model,'sensevoice');
  assert.deepEqual(ui.saved,[{language:'en',model:'sensevoice'}]);
  assert.deepEqual(ui.sent,[],'switching model is a settings save, not a retry');
});

test('the smaller-model action appears only for a model the page offers',()=>{
  let ui=setup();
  ui.render({...failure,smaller_model:null});
  assert.equal(ui.node('setupSmaller').style.display,'none');
  ui=setup({language:'fr'});
  ui.render(failure);
  assert.equal(ui.node('setupSmaller').style.display,'none');
  ui.node('setupSmaller').onclick();
  assert.deepEqual(ui.saved,[]);
  ui=setup();
  ui.render({...failure,smaller_model:{id:'<img src=x>',mb:1}});
  assert.equal(ui.node('setupSmaller').style.display,'none');
});

test('nothing downloaded yet says so by omission, not with a zero',()=>{
  const ui=setup();
  ui.render({...failure,model_downloaded_mb:0});
  assert.equal(ui.node('setupDetail').textContent,failure.model_error);
});

test('the next attempt replaces the failure with its progress',()=>{
  const ui=setup();
  ui.render(failure);
  ui.render({ready:false,model_error:null,retry_at_ms:null,download:{label:'Parakeet TDT v3',pct:48,done:312,total:652}});
  const banner=ui.node('setup');
  assert.ok(!banner.classList.contains('error'));
  assert.equal(ui.node('setupActions').style.display,'none');
  assert.equal(ui.node('setupDetail').style.display,'none');
  assert.equal(ui.node('dlTrack').style.display,'');
  assert.equal(ui.node('setupMsg').textContent,'Downloading Parakeet TDT v3 …');
  assert.equal(ui.node('dlPct').textContent,'48%  312 / 652 MB');
  assert.ok(ui.timers.every(timer=>timer===null),'the countdown stops with the failure');
  ui.render({ready:true});
  assert.ok(!banner.classList.contains('on'));
});

test('every new banner string is translated',()=>{
  for (const key of ['Retry now','Use a smaller model','The speech model could not be set up','Retrying in {n} s','Retrying…',
    '{done} of {total} MB downloaded — the next try continues from there.']){
    assert.equal(html.split(JSON.stringify(key)+':').length-1,4,key);
  }
  assert.match(html,/<button type="button" id="setupRetry">Retry now<\/button>/);
});

test('the download bar is block-level, so its height and width actually draw',()=>{
  assert.match(html,/\.dltrack\{display:block;height:5px;/);
  assert.match(html,/\.dlbar\{display:block;height:100%;/);
});
