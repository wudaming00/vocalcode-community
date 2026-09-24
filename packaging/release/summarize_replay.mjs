// Read a synthetic experiment's summary; print aggregate numbers, never text.
// Usage: node summarize_replay.mjs summary.json
import {readFileSync} from 'node:fs';
const rows=JSON.parse(readFileSync(process.argv[2],'utf8'));
const groups=new Map();
for(const row of rows){
  const key=[row.fixture,row.mode??row.hint,row.minimum_ms??row.threads??'default'].join(' / ');
  if(!groups.has(key))groups.set(key,[]);groups.get(key).push(row);
}
function median(values){const sorted=values.toSorted((a,b)=>a-b),n=sorted.length;return n%2?sorted[(n-1)/2]:(sorted[n/2-1]+sorted[n/2])/2;}
const resources=rows.some(r=>r.process_resources);
console.log('| Sample / policy | n | Error % min–max | Median release ms | Median first output s |'+(resources?' CPU s median | Peak working set MiB range | Peak private commit MiB range |':''));
console.log('| --- | ---: | ---: | ---: | ---: |'+(resources?' ---: | ---: | ---: |':''));
for(const [key,rs] of groups){
  const errors=rs.map(r=>r.error_percent),first=rs.map(r=>r.first_output_ms).filter(Number.isFinite);
  let tail='';
  if(resources){
    const metric=name=>rs.map(r=>r.process_resources?.[name]).filter(Number.isFinite);
    const cpu=metric('cpu_ms');
    const memory=name=>{const values=metric(name).map(v=>v/1048576);return values.length?`${Math.min(...values).toFixed(1)}–${Math.max(...values).toFixed(1)}`:'—';};
    tail=` ${cpu.length?(median(cpu)/1000).toFixed(2):'—'} | ${memory('peak_working_set_bytes')} | ${memory('peak_private_commit_bytes')} |`;
  }
  console.log(`| ${key} | ${rs.length} | ${Math.min(...errors).toFixed(2)}–${Math.max(...errors).toFixed(2)} | ${median(rs.map(r=>r.release_ms)).toFixed(0)} | ${first.length?(median(first)/1000).toFixed(2):'—'} |${tail}`);
}
console.log(`Completed rows: ${rows.length}. WER (en) and CER (zh); synthetic references, not human verification.`);
if(resources)console.log('Resources are for each entire QA process, including startup/warm-up. CPU sums all threads; private commit is not resident RAM. Missing measurements stay blank.');
