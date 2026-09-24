// Headless synthetic replay of language hints and CPU threads. No microphone,
// native injection, real dictation history or automatic model downloads.
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import {createHash} from 'node:crypto';
import {spawnSync} from 'node:child_process';
import {units,distance} from './replay_metrics.mjs';
const [executable,modelDir,fixturePath,outputDir,repeatArg='2',comparison='settings']=process.argv.slice(2);
const repeats=Number(repeatArg);
if(!executable||!modelDir||!fixturePath||!outputDir||!Number.isInteger(repeats)||repeats<1||repeats>3||!['settings','resources'].includes(comparison))throw Error('Usage: executable model-dir synthetic-fixtures.json NEW-output-dir [1..3 repeats] [settings|resources]');
if(fs.existsSync(outputDir))throw Error('Refusing an existing output directory');
const fixtures=JSON.parse(fs.readFileSync(fixturePath,'utf8'));
if(!Array.isArray(fixtures)||!fixtures.length||fixtures.some(f=>!/^[a-z0-9_]+$/.test(f.id)||!['en','zh'].includes(f.language)||typeof f.text!=='string'||!f.text.trim()))throw Error('Invalid synthetic fixture manifest');
// Language comparisons hold threads fixed. Thread comparisons hold hint auto.
// One whole-utterance decode isolates ASR settings from segmentation changes.
const settings=comparison==='resources'?[['auto',4],['auto',8]]:[['auto',4],['zh',4],['en',4],['auto',1],['auto',2],['auto',8]];
fs.mkdirSync(outputDir,{recursive:true});
const hash=file=>createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const manifest={started:new Date().toISOString(),repeats,comparison,settings,fixtures:fixtures.map(f=>f.id),executable_sha256:hash(executable),model_sha256:hash(path.join(modelDir,'model.int8.onnx')),fixtures_sha256:hash(fixturePath),cpu:os.cpus()[0]?.model,logicalCPUs:os.cpus().length,nativeInjection:false,microphone:false,scope:'Synthetic whole-utterance warm-model replay. CPU/background load uncontrolled; generation prompts are reference text, not human-verified transcripts. Process resources include model startup/warm-up, not just measured release latency.'};
fs.writeFileSync(path.join(outputDir,'experiment.json'),JSON.stringify(manifest,null,2));
const results=[];
for(let r=0;r<repeats;r++)for(const f of fixtures)for(let i=0;i<settings.length;i++){
  const [hint,threads]=settings[(i+(comparison==='resources'?r:r*2))%settings.length];
  const name=`${f.id}-${hint}-t${threads}-r${r+1}`;
  const reportPath=path.resolve(outputDir,name+'.json');
  const args=[path.resolve(modelDir),path.resolve(path.dirname(fixturePath),f.id+'.wav'),f.language,'whole','0','0','0',reportPath,'--memory','--decoder-language',hint,'--threads',String(threads)];
  console.log(`[${new Date().toISOString()}] ${results.length+1}/${fixtures.length*settings.length*repeats} ${name}`);
  const child=spawnSync(path.resolve(executable),args,{cwd:path.dirname(path.resolve(executable)),windowsHide:true,encoding:'utf8',timeout:180000,maxBuffer:1024*1024});
  if(child.status!==0||child.error){fs.writeFileSync(path.join(outputDir,name+'.error.log'),`${child.error?.message??''}\n${child.stdout??''}\n${child.stderr??''}`);throw Error(`Replay failed: ${name}`);}
  const report=JSON.parse(fs.readFileSync(reportPath,'utf8'));
  if(report.native_injection!==false||report.append_events_match!==true||report.decoder_language!==hint||report.threads!==threads)throw Error('Safety/settings contract mismatch');
  if(comparison==='resources'&&(!Number.isFinite(report.process_resources?.cpu_ms)||!Number.isFinite(report.process_resources?.peak_working_set_bytes)))throw Error('This comparison needs the updated Windows process-resource probe');
  const reference=units(f.text,f.language);if(!reference.length)throw Error('Empty normalized reference');
  const errors=distance(reference,units(report.trace.final_text,f.language));
  const row={fixture:f.id,language:f.language,hint,threads,repeat:r+1,errors,reference_units:reference.length,error_percent:100*errors/reference.length,release_ms:report.release_to_result_ms,audio_seconds:report.audio_seconds,real_time_factor:report.release_to_result_ms/(1000*report.audio_seconds),process_resources:report.process_resources??null,text:report.trace.final_text};
  results.push(row);fs.writeFileSync(path.join(outputDir,'summary.json'),JSON.stringify(results,null,2));
  console.log(`  errors=${errors}/${reference.length}; release=${row.release_ms.toFixed(1)} ms; RTF=${row.real_time_factor.toFixed(4)}`);
}
fs.writeFileSync(path.join(outputDir,'experiment.json'),JSON.stringify({...manifest,finished:new Date().toISOString(),completed:results.length},null,2));
