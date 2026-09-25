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
const dicts=(()=>{
  const from=html.indexOf('const DICTS = ')+'const DICTS = '.length;
  const to=html.indexOf('\n  };',from)+4;
  return vm.runInNewContext('('+html.slice(from,to)+')');
})();

function setup(language){
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
  let configId=0;
  const dict=language?dicts[language]:null;
  const ctx={
    document:{getElementById:node,querySelector:selector=>selector.includes('system')?systemTab:null},
    window:{},t:x=>dict&&dict[x]?dict[x]:x,cfg,configLatest:null,
    cloneCfg:value=>JSON.parse(JSON.stringify(value)),
    renderConfigState(){},
    // Like the page: an immediate save sends one request and remembers it.
    save:immediate=>{saves.push(immediate);ctx.configLatest={id:++configId};},
    migrationRequest:op=>reloads.push('migration:'+op),workflowRequest:op=>reloads.push('workflow:'+op),
    showPanel:panel=>panels.push(panel),
    send:msg=>{sent.push(msg);return true;},
  };
  vm.createContext(ctx);vm.runInContext(controller,ctx);
  const receive=(op,data,extra={})=>ctx.window.vocalcodeLegacyImportResult({id:sent.at(-1).id,op,ok:true,data,...extra});
  const request=op=>vm.runInContext(`legacyRequest(${JSON.stringify(op)})`,ctx);
  const configResult=(id,ok)=>vm.runInContext(`legacyConfigResult(${id},${ok})`,ctx);
  return {ctx,node,sent,saves,reloads,panels,systemTab,cfg,receive,request,configResult};
}

const found={available:true,path:'C:\\Users\\Ana\\AppData\\Local\\VocalCode',settings:true,profiles:'new',rules:42,snippets:2,meetings:5,meetings_present:0,meetings_in_progress:1,meetings_unreadable:0,
  stats:{dictations:900,words:12000,days:40},stats_imported:false,models:{files:3,bytes:700*1048576,invalid:0,busy:0},problems:[],this_empty:true,answered:false,login:true};

test('a scan fills the checklist, leaves models opt-in and offers Home once',()=>{
  const f=setup();f.request('scan');
  assert.equal(f.sent.at(-1).type,'legacy_import');
  assert.equal(f.node('legacyImport').disabled,true,'busy while scanning');
  f.receive('scan',found);
  assert.equal(f.node('legacySection').hidden,false);
  assert.equal(f.node('homeLegacy').hidden,false);
  assert.equal(f.node('homeLegacyFound').textContent,
    'Found there: 42 rules · 2 snippets · 5 meetings · settings · app profiles · usage counts. Copying them here leaves that folder untouched.');
  assert.equal(f.node('legacyCountDictionary').textContent,'42 rules · 2 snippets');
  assert.equal(f.node('legacyCountMeetings').textContent,'5 meetings · 1 still recording there');
  assert.equal(f.node('legacyCountModels').textContent,'700 MB — no need to download again');
  assert.equal(f.node('legacyCountSettings').textContent,'Found · replaces your current settings');
  assert.equal(f.node('legacyCountProfiles').textContent,'Found');
  assert.match(f.node('legacyPath').textContent,/AppData\\Local\\VocalCode$/);
  for(const part of ['Settings','Profiles','Dictionary','Meetings','Stats'])assert.equal(f.node('legacyPart'+part).checked,true,part);
  assert.equal(f.node('legacyPartModels').checked,false);
  assert.equal(f.node('legacyImport').disabled,false);
  assert.equal(f.node('legacyLoginOff').hidden,false);

  for(const quiet of [{this_empty:false},{answered:true},{settings:false,profiles:null,rules:0,snippets:0,meetings:0,stats:null}]){
    const g=setup();g.request('scan');g.receive('scan',{...found,...quiet});
    assert.equal(g.node('homeLegacy').hidden,true,JSON.stringify(quiet));
  }
  const none=setup();none.request('scan');none.receive('scan',{available:false,login:false});
  assert.equal(none.node('legacySection').hidden,true);
  assert.equal(none.node('homeLegacy').hidden,true);
});

test('the Home card names only what the previous folder has, with singular counts',()=>{
  const f=setup();f.request('scan');
  f.receive('scan',{...found,settings:false,profiles:null,rules:1,snippets:0,meetings:1,stats:null});
  assert.equal(f.node('homeLegacyFound').textContent,
    'Found there: 1 rule · 1 meeting. Copying them here leaves that folder untouched.');
  assert.equal(f.node('legacyCountStats').textContent,'Nothing found');
  assert.equal(f.node('legacyPartSettings').disabled,true);
});

test('an installation with its own setup keeps it unless Settings is ticked knowingly',()=>{
  const f=setup();f.request('scan');
  f.receive('scan',{...found,this_empty:false,profiles:'kept'});
  // Settings would replace what this person set up here: shown, never preselected.
  assert.equal(f.node('legacyPartSettings').checked,false);
  assert.equal(f.node('legacyPartSettings').disabled,false);
  assert.equal(f.node('legacyCountSettings').textContent,'Found · replaces your current settings');
  // Profiles saved here are never replaced, so there is nothing to tick.
  assert.equal(f.node('legacyPartProfiles').disabled,true);
  assert.equal(f.node('legacyCountProfiles').textContent,'Already set up here');
  for(const part of ['Dictionary','Meetings','Stats'])assert.equal(f.node('legacyPart'+part).checked,true,part);
  assert.equal(f.node('homeLegacy').hidden,true);

  f.node('legacyImport').onclick();
  assert.deepEqual({...f.sent.at(-1).parts},{settings:false,profiles:false,dictionary:true,meetings:true,stats:true,models:false});
  f.receive('import',{imported:true,dictionary:{added:3,duplicates:0,conflicts:0},meetings:{copied:5,present:0,in_progress:0,failed:0},stats:{dictations:900},errors:[]});
  assert.equal(f.cfg.talk_mode,'hold','nothing replaced the settings here');
  assert.deepEqual(f.saves,[]);
  assert.doesNotMatch(f.node('legacyMessage').textContent,/settings/i);

  // Ticked on purpose: applied through the ordinary save, and said so.
  const g=setup();g.request('scan');g.receive('scan',{...found,this_empty:false});
  g.node('legacyPartSettings').checked=true;
  g.node('legacyImport').onclick();
  assert.equal(g.sent.at(-1).parts.settings,true);
  g.receive('import',{imported:true,settings:{talk_mode:'toggle'},errors:[]});
  assert.equal(g.cfg.talk_mode,'toggle');
  assert.match(g.node('legacyMessage').textContent,/^Imported: settings\. The imported settings replaced the ones you had here\./);
  // The host refuses that save (a key collision, say): the summary is corrected.
  g.configResult(1,false);
  assert.match(g.node('legacyMessage').textContent,/^Nothing new to import\. The previous settings could not be applied, so yours are unchanged\./);
  assert.doesNotMatch(g.node('legacyMessage').textContent,/replaced/);
  // Answers for other saves change nothing.
  const h=setup();h.request('scan');h.receive('scan',found);h.node('legacyImport').onclick();
  h.receive('import',{imported:true,settings:{talk_mode:'toggle'},errors:[]});
  const summary=h.node('legacyMessage').textContent;
  h.configResult(7,false);h.configResult(1,true);h.configResult(1,false);
  assert.equal(h.node('legacyMessage').textContent,summary);
});

test('import sends only ticked parts and reports what came over, keeping the old folder',()=>{
  const f=setup();f.request('scan');f.receive('scan',{...found,stats_imported:true});
  assert.equal(f.node('legacyPartStats').disabled,true,'usage counts are only ever added once');
  f.node('legacyPartMeetings').checked=false;
  f.node('legacyImport').onclick();
  const message=f.sent.at(-1);
  assert.equal(message.op,'import');
  assert.deepEqual({...message.parts},{settings:true,profiles:true,dictionary:true,meetings:false,stats:false,models:false});
  f.receive('import',{imported:true,settings:{language:'zh',talk_mode:'toggle'},workflows:'imported',
    dictionary:{added:40,duplicates:1,conflicts:1,ignored:0},snippets:{added:1},state:{},errors:[]});
  const summary=f.node('legacyMessage').textContent;
  assert.match(summary,/^Imported: settings · app profiles · 40 new rules · 1 new snippet\./);
  assert.match(summary,/1 of your rules differs from the previous one; yours was kept\./);
  assert.match(summary,/The previous VocalCode's folder was not changed\.$/);
  // Settings go through the page's ordinary, acknowledged save.
  assert.equal(f.cfg.language,'zh');assert.equal(f.cfg.talk_mode,'toggle');assert.equal(f.cfg.autostart,false);
  assert.deepEqual(f.saves,[true]);
  assert.deepEqual(f.reloads,['migration:load','workflow:load']);
  assert.equal(f.node('homeLegacy').hidden,true);
  assert.equal(f.node('legacyImport').disabled,true,'nothing is ticked after an import');
});

test('meetings left behind are reported before and after an import',()=>{
  const f=setup();f.request('scan');
  f.receive('scan',{...found,meetings:3,meetings_unreadable:2,meetings_in_progress:0,meetings_present:1});
  assert.equal(f.node('legacyCountMeetings').textContent,'3 meetings · 1 already imported · 2 could not be read');
  // Every meeting unreadable: nothing to tick, and the row still says why.
  const g=setup();g.request('scan');g.receive('scan',{...found,meetings:0,meetings_unreadable:4,meetings_in_progress:0});
  assert.equal(g.node('legacyPartMeetings').disabled,true);
  assert.equal(g.node('legacyCountMeetings').textContent,'4 could not be read');

  f.node('legacyImport').onclick();
  f.receive('import',{imported:true,meetings:{copied:0,present:1,in_progress:1,failed:3},models:{files:0,bytes:0,invalid:1,busy:1},errors:[]});
  const text=f.node('legacyMessage').textContent;
  assert.match(text,/^Nothing new to import\. /);
  assert.match(text,/3 meetings could not be imported; they stay in the previous folder\./);
  assert.match(text,/1 meeting is still being recorded or processed there and was skipped\./);
  assert.match(text,/1 model file failed verification and was not used\./);
  assert.match(text,/Models that were downloading here were skipped\./);
  f.node('legacyPartMeetings').checked=true;f.node('legacyImport').onclick();
  f.receive('import',{imported:true,meetings:{copied:2,present:0,in_progress:0,failed:1},errors:[]});
  assert.match(f.node('legacyMessage').textContent,/^Imported: 2 meetings\. 1 meeting could not be imported; it stays in the previous folder\./);
});

test('nothing new, partial failures and host errors are stated plainly',()=>{
  const f=setup();f.request('scan');f.receive('scan',found);
  f.node('legacyImport').onclick();
  f.receive('import',{imported:true,dictionary:{added:0,duplicates:42,conflicts:0},meetings:{copied:0,present:5},errors:[{part:'meetings',message:'disk full'},{part:'record',message:'denied'}]});
  assert.match(f.node('legacyMessage').textContent,/^Nothing new to import\. .*Not imported: Meetings: disk full denied$/);
  f.node('legacyImport').onclick();
  f.ctx.window.vocalcodeLegacyImportResult({id:f.sent.at(-1).id,op:'import',ok:false,message:'<b>No data folder</b>'});
  assert.equal(f.node('legacyMessage').textContent,'<b>No data folder</b>');
  assert.equal(f.node('legacyScan').disabled,false,'a failure hands the controls back');
});

test('a Chinese window shows host problems in Chinese, part names included',()=>{
  const f=setup('zh');f.request('scan');
  f.receive('scan',{...found,problems:[{part:'stats',message:'The previous usage counts could not be read.'}]});
  assert.equal(f.node('legacyMessage').textContent,'无法读取：使用统计：无法读取旧版的使用统计。');
  f.node('legacyImport').onclick();
  f.receive('import',{imported:true,errors:[{part:'meetings',message:'The previous settings file could not be read.'}]});
  const text=f.node('legacyMessage').textContent;
  assert.match(text,/未导入：会议记录：无法读取旧版的设置文件。$/);
  assert.doesNotMatch(text,/Meetings|Settings/);
  f.node('legacyScan').onclick();
  f.ctx.window.vocalcodeLegacyImportResult({id:f.sent.at(-1).id,op:'scan',ok:false,message:'No data folder from the previous VocalCode was found.'});
  assert.equal(f.node('legacyMessage').textContent,'没有找到旧版 VocalCode 的数据文件夹。');
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
  assert.equal(f.node('legacyMessage').textContent,'Its login entry starts a different program, so it was left alone.');
  // A foreign Run value stays, but the old Startup shortcut did go: say both.
  f.node('legacyLoginOff').onclick();
  f.receive('disable_login',{login_result:{run:'kept',shortcut:'removed'},login:false});
  assert.equal(f.node('legacyMessage').textContent,
    "The previous VocalCode's Startup shortcut was removed. Its login entry starts a different program, so it was left alone.");
  const g=setup();g.request('scan');g.receive('scan',found);
  g.node('legacyLoginOff').onclick();
  g.receive('disable_login',{login_result:{run:'removed',shortcut:'removed'},login:false});
  assert.equal(g.node('legacyMessage').textContent,'The previous VocalCode will no longer start at login.');
  assert.equal(g.node('legacyLoginOff').hidden,true);
  assert.equal(g.node('legacyLoginNote').textContent,'It does not start at login.');
});

// Fixed sentences the host sends. The page translates them like its own.
const HOST_MESSAGES=[
  ['legacy_import.rs','No data folder from the previous VocalCode was found.'],
  ['legacy_import.rs','Choose what to import.'],
  ['legacy_import.rs','The import record in this installation is unreadable.'],
  ['legacy_import.rs','The previous settings file could not be read.'],
  ['legacy_import.rs','The previous dictionary could not be read.'],
  ['legacy_import.rs','The previous usage counts could not be read.'],
  ['legacy_import.rs','The import record in this installation is unreadable, so usage counts were not added: they must never be counted twice.'],
  ['workflows.rs','The previous app profiles could not be read.'],
  ['main.rs',"This installation's usage totals could not be read, so nothing was added to them."],
  ['activity.rs',"This installation's activity file could not be read, so nothing was merged into it."],
];

test('every string the import shows is translated, and status drives the banner',()=>{
  const literals=[...controller.matchAll(/\bt\("([^"]+)"\)|\bt[nl]\("([^"]+)"/g)].map(m=>m[1]||m[2]);
  const ternaries=[...controller.matchAll(/t\([^)]*\?"([^"]+)":"([^"]+)"\)/g)].flatMap(m=>[m[1],m[2]]);
  // Every count template, including each one-of form passed to tn().
  const counts=[...controller.matchAll(/"([^"]*\{n\}[^"]*)"/g)].map(m=>m[1]).filter(s=>s!=='{n}');
  const labels=[...controller.match(/LEGACY_PART_LABELS=\{([^}]*)\}/)[1].matchAll(/:"([^"]+)"/g)].map(m=>m[1]);
  const markup=html.slice(html.indexOf('<div class="home-legacy"'),html.indexOf('<div class="home-summary"'))
    +html.slice(html.indexOf('<div id="legacySection"'),html.indexOf('<!-- WRITING -->'))
    +html.slice(html.indexOf('<div class="perm" id="legacyRunning"'),html.indexOf('<!-- TRIGGERS -->'));
  const text=[...markup.matchAll(/>([^<>]*[A-Za-z][^<>]*)</g)].map(m=>m[1].trim()).filter(Boolean);
  for(const [file,message] of HOST_MESSAGES)
    assert.ok(readFileSync(new URL('../../vocalcode-app/src/'+file,import.meta.url),'utf8').includes(JSON.stringify(message)),file+': '+message);
  const strings=new Set([...literals,...ternaries,...counts,...labels,...text,...HOST_MESSAGES.map(m=>m[1])]);
  assert.ok(strings.size>=80,String(strings.size));
  for(const language of ['zh','es','fr','de'])
    for(const s of strings)assert.ok(Object.prototype.hasOwnProperty.call(dicts[language],s),language+': '+s);
  for(const language of ['zh','es','fr','de'])
    for(const placeholder of ['{n}','{list}','{path}','{part}','{message}'])
      for(const key of Object.keys(dicts[language]).filter(k=>k.includes(placeholder)))
        assert.ok(dicts[language][key].includes(placeholder),language+': '+key);
  assert.match(html,/getElementById\("legacyRunning"\)\.classList\.toggle\("on", s\.legacy_running===true\)/);
  assert.match(html,/legacyRequest\("scan"\);/);
  // Every settings answer reaches the import, so a refusal can correct it.
  assert.match(html,/legacyConfigResult\(id, ok\);\n  \};/);
});

test('a pasted replacements file can be named as its own layout',()=>{
  assert.match(html,/<option value="rules">VocalCode rules: heard =&gt; written<\/option>/);
  for(const language of ['zh','es','fr','de'])assert.ok(dicts[language]['VocalCode rules: heard => written'],language);
});
