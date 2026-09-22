// Contract tests for the real inline migration controller, not a separate
// reimplementation. No browser, user profile, clipboard, or native host involved.
import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const html=readFileSync(new URL('../../vocalcode-app/src/webui.html',import.meta.url),'utf8');
const start=html.indexOf('  let migrationBusy=');
const end=html.indexOf('  send({type:"ready"});',start);
assert.ok(start>0&&end>start);
const controller=html.slice(start,end);

function setup(){
  class Element {
    constructor(){this.children=[];this.dataset={};this.value='';this.checked=false;this.disabled=false;this.hidden=false;this.textContent='';}
    append(...children){this.children.push(...children);}
    appendChild(child){this.append(child);}
    replaceChildren(...children){this.children=children;}
    set innerHTML(_value){throw Error('Untrusted import must never use innerHTML');}
  }
  const nodes=new Map();
  const node=id=>{if(!nodes.has(id))nodes.set(id,new Element());return nodes.get(id);};
  node('migrationKind').value='dictionary';
  node('migrationLayout').value='auto';
  const sent=[],dictUpdates=[];
  const ctx={document:{getElementById:node,createElement:()=>new Element()},window:{vocalcodeDict:(...args)=>dictUpdates.push(args)},t:x=>x,dictInFlight:null,dictSaveQueued:false,send:msg=>{sent.push(msg);return true;},setTimeout:()=>1};
  vm.createContext(ctx);vm.runInContext(controller,ctx);
  const receive=(data,extra={})=>ctx.window.vocalcodeMigrationResult({id:sent.at(-1).id,ok:true,data,...extra});
  return {ctx,node,sent,dictUpdates,receive};
}

test('navigation hooks exist and HTML IDs are unique',()=>{
  assert.match(html,/class="setbtn act" data-go="migration"/);
  const ids=[...html.matchAll(/\bid="([^"\s]+)"/g)].map(match=>match[1]);
  assert.equal(ids.length,new Set(ids).size);
});

test('preview is required; payload strings stay inert; commit is token-bound',()=>{
  const f=setup();
  vm.runInContext('migrationControls()',f.ctx);
  assert.equal(f.node('migrationCommit').disabled,true);
  f.node('migrationText').value='wrong,Correct';
  f.node('migrationPreview').onclick();
  assert.equal(f.sent.at(-1).op,'preview');
  assert.equal(f.node('migrationText').disabled,true);
  f.receive({token:3,kind:'dictionary',preview:{added:1,duplicates:0,conflicts:0,rows:[{entry:{name:'<img onerror=evil()>',text:'<script>evil()</script>'},status:'new'}]}});
  assert.equal(f.node('migrationCommit').disabled,false);
  assert.equal(f.node('migrationRows').children[0].children[2].textContent,'<script>evil()</script>');
  f.node('migrationCommit').onclick();
  assert.equal(f.sent.at(-1).op,'commit');
  assert.equal(f.sent.at(-1).token,3);
  f.receive({saved:true,undo_token:3});
  assert.equal(f.node('migrationCommit').disabled,true);
  assert.equal(f.node('migrationUndo').disabled,false);
  f.node('migrationUndo').onclick();
  assert.equal(f.sent.at(-1).token,3);
  f.receive({undone:true});
  assert.equal(f.node('migrationUndo').disabled,true);
});

test('stale results do not unlock a different request; cancellation unlocks',()=>{
  const f=setup();f.node('migrationPick').onclick();
  f.ctx.window.vocalcodeMigrationResult({id:0,ok:false,message:'old'});
  assert.equal(f.node('migrationPick').disabled,true);
  f.receive({}, {cancelled:true});
  assert.equal(f.node('migrationPick').disabled,false);
  f.node('migrationPick').onclick();
  f.receive({}, {ok:false,message:'Malformed input'});
  assert.equal(f.node('migrationMessage').textContent,'Malformed input');
  assert.equal(f.node('migrationCommit').disabled,true);
});

test('an in-flight dictionary edit is not overwritten by import refresh',()=>{
  const f=setup();f.ctx.dictInFlight={id:9};
  f.node('migrationReload').onclick();
  f.receive({state:{dictionary:[{name:'a',text:'A'}],dictionary_revision:'new',snippets:[],snippets_revision:'empty'}});
  assert.equal(f.dictUpdates.length,0);
  f.ctx.dictInFlight=null;f.node('migrationReload').onclick();
  f.receive({state:{dictionary:[{name:'a',text:'A'}],dictionary_revision:'new',snippets:[],snippets_revision:'empty'}});
  assert.equal(f.dictUpdates.length,1);
});

test('changing the selected kind invalidates a previous preview',()=>{
  const f=setup();f.node('migrationPreview').onclick();
  f.receive({token:5,kind:'dictionary',preview:{added:1,rows:[{entry:{name:'a',text:'A'},status:'new'}]}});
  f.node('migrationKind').value='snippets';f.node('migrationKind').onchange();
  assert.equal(f.node('migrationPreviewCard').hidden,true);
  assert.equal(f.node('migrationCommit').disabled,true);
});

test('copied table layout is passed explicitly and changing it invalidates preview',()=>{
  const f=setup();f.node('migrationLayout').value='tsv';
  f.node('migrationText').value='heard\tcorrect';f.node('migrationPreview').onclick();
  assert.equal(f.sent.at(-1).layout,'tsv');
  assert.equal(f.sent.at(-1).text,'heard\tcorrect');
  f.receive({token:7,kind:'dictionary',preview:{added:1,rows:[{entry:{name:'a',text:'A'},status:'new'}]}});
  f.node('migrationLayout').value='words';f.node('migrationLayout').onchange();
  assert.equal(f.node('migrationCommit').disabled,true);
});

test('new migration styles reference existing theme tokens',()=>{
  const styles=html.slice(html.indexOf('  .migration-tools{'),html.indexOf('</style>'));
  for(const match of styles.matchAll(/var\((--[a-z-]+)\)/g))assert.ok(html.includes(match[1]+':'),match[1]);
});
