// Read only explicitly supplied synthetic fixtures and synthetic results.
// Shows one representative replay per policy; never reads an app profile.
import {readFileSync} from 'node:fs';
import {units,changes} from './replay_metrics.mjs';
const [fixturePath,summaryPath]=process.argv.slice(2);
if(!fixturePath||!summaryPath)throw Error('Usage: synthetic-fixtures.json synthetic-summary.json');
const fixtures=JSON.parse(readFileSync(fixturePath,'utf8')),rows=JSON.parse(readFileSync(summaryPath,'utf8'));
const seen=new Set();
for(const row of rows){
  const key=[row.fixture,row.mode??row.hint,row.minimum_ms??row.threads??'default'].join('/');
  if(seen.has(key))continue;seen.add(key);
  const fixture=fixtures.find(f=>f.id===row.fixture);if(!fixture)throw Error('Missing reference fixture');
  const delta=changes(units(fixture.text,fixture.language),units(row.text,fixture.language));
  if(!delta.length)continue;
  console.log(JSON.stringify({sample_policy:key,repeat:row.repeat,errors:delta.length,changes:delta.slice(0,30)}));
}
