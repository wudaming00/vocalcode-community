// Construct a five-minute stress fixture from existing synthetic speech, then
// replay it through the real core in memory-only mode. This is concatenated
// audio, not a new natural conversation or an independent accuracy dataset.
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import {createHash} from 'node:crypto';
import {spawnSync} from 'node:child_process';
import {units,distance} from './replay_metrics.mjs';

const [executable,modelDir,fixturePath,outputDir,repeatArg='2']=process.argv.slice(2);
const repeats=Number(repeatArg);
if(!executable||!modelDir||!fixturePath||!outputDir||!Number.isInteger(repeats)||repeats<1||repeats>3)throw Error('Usage: executable model-dir synthetic-fixtures.json NEW-output-dir [1..3 repeats]');
if(fs.existsSync(outputDir))throw Error('Refusing an existing output directory');
const fixtures=JSON.parse(fs.readFileSync(fixturePath,'utf8'));
if(!Array.isArray(fixtures)||fixtures.some(f=>!/^[a-z0-9_]+$/.test(f.id)||typeof f.text!=='string'||!f.text.trim()))throw Error('Invalid synthetic fixture manifest');
const english=fixtures.filter(f=>f.language==='en');
if(english.length<3||english.length>10)throw Error('Expected 3..10 existing English synthetic samples');
function pcm(file){
  const data=fs.readFileSync(file);
  if(data.length<44||data.toString('ascii',0,4)!=='RIFF'||data.toString('ascii',8,12)!=='WAVE')throw Error('Expected RIFF WAV');
  let format=null,samples=null;
  for(let at=12;at+8<=data.length;){
    const size=data.readUInt32LE(at+4),begin=at+8,end=begin+size;
    if(end>data.length)throw Error('Truncated WAV chunk');
    const tag=data.toString('ascii',at,at+4);
    if(tag==='fmt '){if(size<16)throw Error('Short format');format=[data.readUInt16LE(begin),data.readUInt16LE(begin+2),data.readUInt32LE(begin+4),data.readUInt16LE(begin+14)];}
    if(tag==='data'){if(samples)throw Error('Multiple data chunks');samples=data.subarray(begin,end);}
    at=end+(size%2);
  }
  if(JSON.stringify(format)!=='[1,1,16000,16]'||!samples?.length||samples.length%2)throw Error('Expected mono PCM16 16 kHz');
  return samples;
}
const components=[];
for(let round=0;round<2;round++)for(const f of english){
  components.push({id:f.id,text:f.text,samples:pcm(path.resolve(path.dirname(fixturePath),f.id+'.wav'))});
}
const audio=Buffer.concat(components.flatMap((f,i)=>i?[Buffer.alloc(19200),f.samples]:[f.samples])); // 600 ms silence between sources
const seconds=audio.length/32000;
if(seconds<120||seconds>540)throw Error('Constructed duration must be 2..9 minutes');
const header=Buffer.alloc(44);header.write('RIFF');header.writeUInt32LE(36+audio.length,4);header.write('WAVE',8);header.write('fmt ',12);header.writeUInt32LE(16,16);header.writeUInt16LE(1,20);header.writeUInt16LE(1,22);header.writeUInt32LE(16000,24);header.writeUInt32LE(32000,28);header.writeUInt16LE(2,32);header.writeUInt16LE(16,34);header.write('data',36);header.writeUInt32LE(audio.length,40);
fs.mkdirSync(outputDir,{recursive:true});
const wav=path.resolve(outputDir,'concatenated_english.wav');fs.writeFileSync(wav,Buffer.concat([header,audio]));
const hash=file=>createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const manifest={started:new Date().toISOString(),repeats,seconds,components:components.map(f=>f.id),sourceText:components.map(f=>f.text).join(' '),gap_ms:600,executable_sha256:hash(executable),model_sha256:hash(path.join(modelDir,'model.int8.onnx')),audio_sha256:hash(wav),cpu:os.cpus()[0]?.model,nativeInjection:false,microphone:false,scope:'Concatenated synthetic stress fixture; not independent/natural speech. Background CPU load uncontrolled. Whole-utterance five-minute ASR is intentionally not exercised: production uses paused predecode, and a monolithic attention allocation can be much larger.'};
fs.writeFileSync(path.join(outputDir,'experiment.json'),JSON.stringify(manifest,null,2));
const reference=units(manifest.sourceText,'en'),policies=[['normal',8000],['progressive',300],['progressive',3000]],results=[];
for(let r=0;r<repeats;r++)for(let i=0;i<policies.length;i++){
  const [mode,minimum]=policies[(i+r)%policies.length],name=`long_english-${mode}-${minimum}-r${r+1}`,reportPath=path.resolve(outputDir,name+'.json');
  console.log(`[${new Date().toISOString()}] ${results.length+1}/${policies.length*repeats} ${name}; audio=${seconds.toFixed(1)} s`);
  const args=[path.resolve(modelDir),wav,'en',mode,'0','0','0',reportPath,'--memory','--segment-min-ms',String(minimum)];
  const child=spawnSync(path.resolve(executable),args,{cwd:path.dirname(path.resolve(executable)),windowsHide:true,encoding:'utf8',timeout:Math.ceil((seconds+120)*1000),maxBuffer:1024*1024});
  if(child.status!==0||child.error){fs.writeFileSync(path.join(outputDir,name+'.error.log'),`${child.error?.message??''}\n${child.stdout??''}\n${child.stderr??''}`);throw Error(`Replay failed: ${name}`);}
  const report=JSON.parse(fs.readFileSync(reportPath,'utf8'));
  if(report.native_injection!==false||report.append_events_match!==true)throw Error('Safety/append contract mismatch');
  const errors=distance(reference,units(report.trace.final_text,'en'));
  const row={fixture:'concatenated_english',language:'en',mode,minimum_ms:minimum,repeat:r+1,errors,reference_units:reference.length,error_percent:100*errors/reference.length,release_ms:report.release_to_result_ms,first_output_ms:report.inserts[0]?.at_ms??null,max_tick_ms:report.max_tick_ms,chunks:report.trace.asr_chunks,append_events_match:true,process_resources:report.process_resources??null,text:report.trace.final_text};
  results.push(row);fs.writeFileSync(path.join(outputDir,'summary.json'),JSON.stringify(results,null,2));
  console.log(`  WER ${row.error_percent.toFixed(2)}%; release ${row.release_ms.toFixed(1)} ms; max tick ${row.max_tick_ms.toFixed(1)} ms; ${row.chunks} chunks`);
}
fs.writeFileSync(path.join(outputDir,'experiment.json'),JSON.stringify({...manifest,finished:new Date().toISOString(),completed:results.length},null,2));
