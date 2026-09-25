// Contract tests for the real inline previous-edition import controller. No
// browser, user profile or native host: the page is driven through its own
// result callback, exactly as the host drives it.
import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const html=readFileSync(new URL('../../vocalcode-app/src/webui.html',import.meta.url),'utf8');
const start=html.indexOf('  // PREVIOUS EDITION IMPORT.');
const end=html.indexOf('  // END PREVIOUS EDITION IMPORT',start);
assert.ok(start>0&&end>start);
const controller=html.slice(start,end);

function setup(){
  class Element {
    constructor(){this.dataset={};this.hidden=false;this.checked=false;this.disabled=false;this.textContent='';this.classes=new Set();
      this.classList={toggle:(name,on)=>{on?this.classes.add(name):this.classes.delete(name);}};}
    set innerHTML(_value){throw Error('Folder paths and host messages must stay inert text');}
  }
  const nodes=new Map();
  const node=id=>{if(!nodes.has(id))nodes.set(id,new Element());return nodes.get(id);};
  const sent=[],saves=[],reloads=[],panels=[];
  const cfg={language:'en',talk_mode:'hold',autostart:false};
  const systemTab={clicked:false,click(){this.clicked=true;}};
  const ctx={
    document:{getElementById:node,querySelector:selector=>selector.includes('system')?systemTab:null},
    window:{},t:x=>x,cfg,
    cloneCfg:value=>JSON.parse(JSON.stringify(value)),
    renderConfigState(){},save:immediate=>saves.push(immediate),
    migrationRequest:op=>reloads.push('migration:'+op),workflowRequest:op=>reloads.push('workflow:'+op),
    showPanel:panel=>panels.push(panel),
    send:msg=>{sent.push(msg);return true;},
  };
  vm.createContext(ctx);vm.runInContext(controller,ctx);
  const receive=(op,data,extra={})=>ctx.window.vocalcodeLegacyImportResult({id:sent.at(-1).id,op,ok:true,data,...extra});
  const request=op=>vm.runInContext(`legacyRequest(${JSON.stringify(op)})`,ctx);
  return {ctx,node,sent,saves,reloads,panels,systemTab,cfg,receive,request};
}

const found={available:true,path:'C:\\Users\\Ana\\AppData\\Local\\VocalCode',settings:true,rules:42,snippets:2,meetings:5,meetings_present:0,meetings_in_progress:1,
  stats:{dictations:900,words:12000,days:40},stats_imported:false,models:{files:3,bytes:700*1048576},problems:[],community_empty:true,answered:false,login:true};

test('a scan fills the checklist, leaves models opt-in and offers Home once',()=>{
  const f=setup();f.request('scan');
  assert.equal(f.sent.at(-1).type,'legacy_import');
  assert.equal(f.node('legacyImport').disabled,true,'busy while scanning');
  f.receive('scan',found);
  assert.equal(f.node('legacySection').hidden,false);
  assert.equal(f.node('homeLegacy').hidden,false);
  assert.equal(f.node('legacyCountDictionary').textContent,'42 rules · 2 snippets');
  assert.equal(f.node('legacyCountMeetings').textContent,'5 meetings · 1 still recording there');
  assert.equal(f.node('legacyCountModels').textContent,'700 MB — no need to download again');
  assert.match(f.node('legacyPath').textContent,/AppData\\Local\\VocalCode$/);
  for(const part of ['Settings','Dictionary','Meetings','Stats'])assert.equal(f.node('legacyPart'+part).checked,true,part);
  assert.equal(f.node('legacyPartModels').checked,false);
  assert.equal(f.node('legacyImport').disabled,false);
  assert.equal(f.node('legacyLoginOff').hidden,false);

  for(const quiet of [{community_empty:false},{answered:true},{settings:false,rules:0,snippets:0,meetings:0,stats:null}]){
    const g=setup();g.request('scan');g.receive('scan',{...found,...quiet});
    assert.equal(g.node('homeLegacy').hidden,true,JSON.stringify(quiet));
  }
  const none=setup();none.request('scan');none.receive('scan',{available:false,login:false});
  assert.equal(none.node('legacySection').hidden,true);
  assert.equal(none.node('homeLegacy').hidden,true);
});

test('import sends only ticked parts and reports what came over, keeping the old folder',()=>{
  const f=setup();f.request('scan');f.receive('scan',{...found,stats_imported:true});
  assert.equal(f.node('legacyPartStats').disabled,true,'usage counts are only ever added once');
  f.node('legacyPartMeetings').checked=false;
  f.node('legacyImport').onclick();
  const message=f.sent.at(-1);
  assert.equal(message.op,'import');
  assert.deepEqual({...message.parts},{settings:true,dictionary:true,meetings:false,stats:false,models:false});
  f.receive('import',{imported:true,settings:{language:'zh',talk_mode:'toggle'},workflows:'imported',
    dictionary:{added:40,duplicates:1,conflicts:1,ignored:0},snippets:{added:2},state:{},errors:[]});
  const summary=f.node('legacyMessage').textContent;
  assert.match(summary,/^Imported: settings · app profiles · 40 new rules · 2 new snippets\./);
  assert.match(summary,/1 of your rules differ/);
  assert.match(summary,/The previous VocalCode's folder was not changed\.$/);
  // Settings go through the page's ordinary, acknowledged save.
  assert.equal(f.cfg.language,'zh');assert.equal(f.cfg.talk_mode,'toggle');assert.equal(f.cfg.autostart,false);
  assert.deepEqual(f.saves,[true]);
  assert.deepEqual(f.reloads,['migration:load','workflow:load']);
  assert.equal(f.node('homeLegacy').hidden,true);
  assert.equal(f.node('legacyImport').disabled,true,'nothing is ticked after an import');
});

test('nothing new, partial failures and host errors are stated plainly',()=>{
  const f=setup();f.request('scan');f.receive('scan',found);
  f.node('legacyImport').onclick();
  f.receive('import',{imported:true,dictionary:{added:0,duplicates:42,conflicts:0},meetings:{copied:0,present:5},errors:['Meetings: disk full']});
  assert.match(f.node('legacyMessage').textContent,/^Nothing new to import\. .*Not imported: Meetings: disk full$/);
  f.node('legacyImport').onclick();
  f.ctx.window.vocalcodeLegacyImportResult({id:f.sent.at(-1).id,op:'import',ok:false,message:'<b>No data folder</b>'});
  assert.equal(f.node('legacyMessage').textContent,'<b>No data folder</b>');
  assert.equal(f.node('legacyScan').disabled,false,'a failure hands the controls back');
});

test('stale results are ignored, and Not now while busy is still remembered',()=>{
  const f=setup();f.request('scan');
  const scanId=f.sent.at(-1).id;
  f.ctx.window.vocalcodeLegacyImportResult({id:scanId-1,op:'scan',ok:true,data:found});
  assert.equal(f.node('legacySection').hidden,false,'untouched by a stale reply');
  assert.equal(f.node('legacyImport').disabled,true);
  f.node('homeLegacyDismiss').onclick();
  assert.equal(f.node('homeLegacy').hidden,true);
  assert.equal(f.sent.length,1,'one request at a time');
  // The scan that was in flight when Not now was clicked must not bring the
  // card back, and the queued dismissal is sent as soon as it can be.
  f.receive('scan',{...found});
  assert.equal(f.node('homeLegacy').hidden,true);
  assert.equal(f.sent.at(-1).op,'dismiss');
  f.receive('dismiss',{dismissed:true});
  assert.equal(f.node('homeLegacy').hidden,true);
});

test('Review import opens Settings → System; turning off login is an explicit click',()=>{
  const f=setup();f.request('scan');f.receive('scan',found);
  f.node('homeLegacyReview').onclick();
  assert.deepEqual(f.panels,['behaviour']);assert.equal(f.systemTab.clicked,true);
  assert.equal(f.sent.filter(m=>m.op==='disable_login').length,0,'never automatic');
  f.node('legacyLoginOff').onclick();
  assert.equal(f.sent.at(-1).op,'disable_login');
  f.receive('disable_login',{login_result:{run:'kept',shortcut:'absent'},login:true});
  assert.match(f.node('legacyMessage').textContent,/different program/);
  f.node('legacyLoginOff').onclick();
  f.receive('disable_login',{login_result:{run:'removed',shortcut:'removed'},login:false});
  assert.equal(f.node('legacyMessage').textContent,'The previous VocalCode will no longer start at login.');
  assert.equal(f.node('legacyLoginOff').hidden,true);
  assert.equal(f.node('legacyLoginNote').textContent,'It does not start at login.');
});

test('every string the import shows is translated, and status drives the banner',()=>{
  const from=html.indexOf('const DICTS = ')+'const DICTS = '.length;
  const to=html.indexOf('\n  };',from)+4;
  const dicts=vm.runInNewContext('('+html.slice(from,to)+')');
  const literals=[...controller.matchAll(/\bt\("([^"]+)"\)|\bt[nl]\("([^"]+)"/g)].map(m=>m[1]||m[2]);
  const ternaries=[...controller.matchAll(/t\([^)]*\?"([^"]+)":"([^"]+)"\)/g)].flatMap(m=>[m[1],m[2]]);
  const markup=html.slice(html.indexOf('<div class="home-legacy"'),html.indexOf('<div class="home-summary"'))
    +html.slice(html.indexOf('<div id="legacySection"'),html.indexOf('<!-- WRITING -->'))
    +html.slice(html.indexOf('<div class="perm" id="legacyRunning"'),html.indexOf('<!-- TRIGGERS -->'));
  const text=[...markup.matchAll(/>([^<>]*[A-Za-z][^<>]*)</g)].map(m=>m[1].trim()).filter(Boolean);
  const strings=new Set([...literals,...ternaries,...text]);
  assert.ok(strings.size>=40,String(strings.size));
  for(const language of ['zh','es','fr','de'])
    for(const s of strings)assert.ok(Object.prototype.hasOwnProperty.call(dicts[language],s),language+': '+s);
  for(const language of ['zh','es','fr','de'])
    for(const placeholder of ['{n}','{list}','{path}'])
      for(const key of Object.keys(dicts[language]).filter(k=>k.includes(placeholder)))
        assert.ok(dicts[language][key].includes(placeholder),language+': '+key);
  assert.match(html,/getElementById\("legacyRunning"\)\.classList\.toggle\("on", s\.legacy_running===true\)/);
  assert.match(html,/legacyRequest\("scan"\);/);
});

test('a pasted replacements file can be named as its own layout',()=>{
  assert.match(html,/<option value="rules">VocalCode rules: heard =&gt; written<\/option>/);
  const from=html.indexOf('const DICTS = ')+'const DICTS = '.length;
  const to=html.indexOf('\n  };',from)+4;
  const dicts=vm.runInNewContext('('+html.slice(from,to)+')');
  for(const language of ['zh','es','fr','de'])assert.ok(dicts[language]['VocalCode rules: heard => written'],language);
});
