import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
const html=readFileSync(new URL('../../vocalcode-app/src/webui.html',import.meta.url),'utf8');
const code=html.split('// NOISE FILTER STATUS:')[1].split('// END NOISE FILTER STATUS')[0];
const script=code.slice(code.indexOf('window.vocalcodeNoiseFilterStatus'));
function setup(){const el={textContent:''};Object.defineProperty(el,'innerHTML',{set(){throw Error('unsafe HTML');}});const ctx={window:{},document:{getElementById:id=>{assert.equal(id,'noiseFilterStatus');return el;}},t:s=>s};vm.createContext(ctx);vm.runInContext(script,ctx);return {el,show:ctx.window.vocalcodeNoiseFilterStatus};}
test('filter is an explicit config toggle, separate from words and meeting state',()=>{assert.match(html,/bindToggle\("noiseFilter","noise_filter"\)/);assert.match(html,/\["noiseFilter","noise_filter"\]/);assert.match(html,/id="noiseFilter"[^>]*aria-pressed="false"/);assert.doesNotMatch(script,/send\(|meeting_start|inject|\.click\(/);});
test('missing model and progressive bypass are visible, not reported as active filtering',()=>{const s=setup();s.show({state:'unavailable'});assert.match(s.el.textContent,/unavailable.*bypassed/);s.show({state:'progressive'});assert.match(s.el.textContent,/bypassed.*progressive/);s.show();assert.equal(s.el.textContent,'Off');});
test('compact CSS cannot hide the filter decision or failure state',()=>{assert.match(html,/\.row \.k #noiseFilterStatus\{display:block/);});
test('filter decisions use inert fixed labels and bounded counts',()=>{const s=setup();s.show({state:'rejected',rejected:2});assert.match(s.el.textContent,/nothing inserted.*2/);s.show({state:'<img src=x>',rejected:'<script>'});assert.equal(s.el.textContent,'Off');s.show({state:'short',rejected:-1});assert.equal(s.el.textContent,'Short clip preserved');});
