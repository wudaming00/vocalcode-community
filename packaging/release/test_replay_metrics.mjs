import test from 'node:test';
import assert from 'node:assert/strict';
import {units,distance,changes} from './replay_metrics.mjs';
test('replay normalization ignores case and punctuation but preserves semantic tokens',()=>{
  assert.deepEqual(units('I don’t approve $1,200.','en'),['i','dont','approve','1','200']);
  assert.deepEqual(units('未批准 Python １２。','zh'),[...'未批准python12']);
  assert.deepEqual(units('...','en'),[]);
  assert.notEqual(distance(units('not 12','en'),units('120','en')),0);
});
test('replay alignment counts substitutions, insertions and deletions consistently',()=>{
  for(const [a,b,expected] of [[[],[],0],[['a'],[],1],[[],['x'],1],[['a','b'],['a','c'],1],[['a','b','c'],['x','a','c','y'],3]]){
    assert.equal(distance(a,b),expected);assert.equal(changes(a,b).length,expected);
  }
  assert.throws(()=>changes(Array(2501).fill('a'),[]),/limited/);
});
test('short exhaustive alignments agree with linear-space edit distance',()=>{
  const sequences=[[]];for(let n=1;n<=4;n++)for(let mask=0;mask<2**n;mask++)sequences.push(Array.from({length:n},(_,i)=>(mask>>i)&1?'a':'b'));
  for(const a of sequences)for(const b of sequences)assert.equal(changes(a,b).length,distance(a,b));
});
