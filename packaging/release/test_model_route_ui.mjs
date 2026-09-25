import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
const html=readFileSync(new URL('../../vocalcode-app/src/webui.html',import.meta.url),'utf8');
const models=readFileSync(new URL('../../vocalcode-app/src/models.rs',import.meta.url),'utf8');
const from=html.indexOf('  var MODEL_CHOICES = {');
const to=html.indexOf('  function renderLanguageButtons(',from);
const routeFrom=html.indexOf('  function updateModelRoute(){',to);
const routeTo=html.indexOf('  function renderModelChoices(){',routeFrom);
assert.ok(from>=0 && to>from && routeFrom>to && routeTo>routeFrom);
const script=html.slice(from,to)+html.slice(routeFrom,routeTo);
const dictFrom=html.indexOf('const DICTS = ')+'const DICTS = '.length;
const dicts=vm.runInNewContext('('+html.slice(dictFrom,html.indexOf('\n  };',dictFrom)+4)+')');
// The engine's own table, as the host sends it in `language_routes`.
const table=models.slice(models.indexOf('pub const SUPPORTED_LANGUAGES'),models.indexOf('];',models.indexOf('pub const SUPPORTED_LANGUAGES')));
const engineRoutes=Object.fromEntries([...table.matchAll(/\("([a-z]{2})", "[^"]*", "([^"]+)"\)/g)].map(m=>[m[1],m[2]]));
assert.equal(engineRoutes.en,'parakeet-tdt-v3');

function page(cfg,dict){
  const nodes=new Map();
  const node=id=>{
    if(!nodes.has(id)){const classes=new Set();nodes.set(id,{textContent:'',style:{},classList:{add:c=>classes.add(c),toggle:(c,on)=>on?classes.add(c):classes.delete(c),contains:c=>classes.has(c)}});}
    return nodes.get(id);
  };
  const ctx={cfg,t:s=>dict&&Object.prototype.hasOwnProperty.call(dict,s)?dict[s]:s,document:{getElementById:node},setTimeout(){}};
  vm.createContext(ctx);vm.runInContext(script,ctx);
  return {ctx,node};
}

test('an empty model resolves through the engine route table, not a page copy',()=>{
  const {ctx}=page({language:'en',model:'',onboarded:true,language_routes:engineRoutes});
  for(const [code,model] of Object.entries(engineRoutes)) assert.equal(ctx.languageRoute(code,''),model,code);
  assert.equal(ctx.languageRoute('th',''),'');
  assert.equal(ctx.languageRoute('en','qwen3-asr-0.6b'),'qwen3-asr-0.6b');
  assert.equal(ctx.languageRoute('en','typo'),'unknown:typo');
  ctx.cfg={language:'en',model:''};
  assert.equal(ctx.languageRoute('en',''),'','no table, no guessed model');
  assert.doesNotMatch(script,/code\s*===\s*"en"/);
});

test('picking English stores the hardware recommendation explicitly',()=>{
  for(const recommended of ['parakeet-tdt-v3','sensevoice']){
    const {ctx}=page({language:'zh',model:'sensevoice',onboarded:true,language_routes:engineRoutes,hardware:{recommendations:{en:recommended}}});
    ctx.selectLanguage('en');
    assert.equal(ctx.cfg.language,'en');
    assert.equal(ctx.cfg.model,recommended,'a Compact machine must persist sensevoice, not fall through to the empty-model route');
  }
});

test('a stored English model is shown as the user\'s choice and never replaced',()=>{
  const cfg={language:'en',model:'sensevoice',onboarded:true,language_routes:engineRoutes,hardware:{recommendations:{en:'parakeet-tdt-v3'}}};
  const {ctx,node}=page(cfg,dicts.zh);
  ctx.updateModelRoute();
  assert.equal(ctx.cfg.model,'sensevoice');
  assert.equal(node('modelRouteName').textContent,'SenseVoice');
  assert.equal(node('modelRouteBadge').textContent,dicts.zh.Selected);
  assert.equal(node('modelRouteBadge').classList.contains('recommended'),false);
  assert.equal(node('modelRouteDetail').textContent,dicts.zh['Fastest and lightest · 229 MB']);

  ctx.cfg.model='';
  ctx.updateModelRoute();
  assert.equal(node('modelRouteName').textContent,'Parakeet TDT v3');
  assert.equal(node('modelRouteBadge').textContent,dicts.zh.Recommended);
  assert.equal(node('modelRouteBadge').classList.contains('recommended'),true);
});

test('English model descriptions state the measured trade-offs in every interface language',()=>{
  const {ctx}=page({});
  const details=Object.fromEntries(ctx.MODEL_CHOICES.en.map(choice=>[choice.id,choice.detail]));
  assert.match(details['parakeet-tdt-v3'],/English.*punctuation and coding words/);
  assert.match(details.sensevoice,/^Fastest and lightest/);
  assert.match(details['qwen3-asr-0.6b'],/short English dictation; slow on long audio/);
  for(const key of [...Object.values(details),'Recommended','Selected']){
    for(const language of ['zh','es','fr','de']){
      assert.equal(typeof dicts[language][key],'string',`${language}: ${key}`);
      assert.notEqual(dicts[language][key],key,`${language} leaves ${key} untranslated`);
    }
  }
  assert.doesNotMatch(html,/flag\.textContent="recommended"/);
});
