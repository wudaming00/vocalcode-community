import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
const html=readFileSync(new URL('../../vocalcode-app/src/webui.html',import.meta.url),'utf8');
const controller=html.slice(html.indexOf('  function localHistoryTime('),html.indexOf('  // Last statement in the script'));
function setup(){
  class Element{
    constructor(){this.children=[];this.style={};this.dataset={};this.text='';}
    set textContent(v){this.text=String(v);this.children=[];}get textContent(){return this.text;}
    appendChild(e){this.children.push(e);}setAttribute(){}
    set innerHTML(_){throw Error('Unsafe history markup');}
  }
  const nodes=new Map(),sent=[];const node=id=>{if(!nodes.has(id))nodes.set(id,new Element());return nodes.get(id);};
  const ctx={document:{getElementById:node,createElement:()=>new Element()},window:{},hist:[],t:x=>x,send:m=>sent.push(m),teachWord(){}};
  vm.createContext(ctx);vm.runInContext(controller,ctx);
  return {node,sent,receive:items=>ctx.window.vocalcodeHistory(items)};
}
test('original recognition stays inert and copying never injects or mutates final text',()=>{
  const f=setup();f.receive([{at:1,text:'Retry.',recognition:'uh <script>bad()</script>',filler_removed:1}]);
  const row=f.node('histRows').children[0],review=row.children[1].children[0];
  assert.equal(f.sent.length,0);assert.match(review.children[0].textContent,/1/);
  assert.equal(review.children[1].textContent,'uh <script>bad()</script>');
  review.children[2].onclick();assert.equal(f.sent.at(-1).type,'copy');
  assert.equal(f.sent.at(-1).text,'uh <script>bad()</script>');
  row.children.at(-1).onclick();assert.equal(f.sent.at(-1).text,'Retry.');
});
test('pause-only review is recoverable; legacy entries do not promise a missing original',()=>{
  const f=setup();f.receive([{at:1,text:'',recognition:'um uh',filler_removed:2},[2,'legacy']]);
  const [row,old]=f.node('histRows').children;
  assert.equal(row.children.at(-1).disabled,true);
  row.children[1].children[0].children[2].onclick();assert.equal(f.sent.at(-1).text,'um uh');
  assert.equal(old.children[1].children.length,0);assert.equal(old.children[1].textContent,'legacy');
});
