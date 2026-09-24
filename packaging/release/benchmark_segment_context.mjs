// Sequential, synthetic-only production-engine replay. Never uses a microphone
// or native injector. Arguments: executable model-dir fixtures.json output-dir [repeats].
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import {createHash} from 'node:crypto';
import {spawnSync} from 'node:child_process';

const [executable, modelDir, fixturePath, outputDir, repeatArg = '3'] = process.argv.slice(2);
const repeats = Number(repeatArg);
if (!executable || !modelDir || !fixturePath || !outputDir || !Number.isInteger(repeats) || repeats < 1 || repeats > 10) {
  throw new Error('Usage: node benchmark_segment_context.mjs executable model-dir fixtures.json NEW-output-dir [1..10 repeats]');
}
if (fs.existsSync(outputDir)) throw new Error('Refusing to overwrite an existing experiment directory');
const fixtures = JSON.parse(fs.readFileSync(fixturePath, 'utf8'));
if (!Array.isArray(fixtures) || !fixtures.length || fixtures.some(f => !/^[a-z0-9_]+$/.test(f.id) || !['en','zh'].includes(f.language) || typeof f.text !== 'string')) throw new Error('Invalid synthetic fixture manifest');
fs.mkdirSync(outputDir, {recursive: true});
const policies = [
  ['whole', null], ['normal', 8000], ['normal', 12000],
  ['progressive', 300], ['progressive', 1500], ['progressive', 3000], ['progressive', 5000],
];
function units(text, language) {
  const value = text.normalize('NFKC').toLowerCase().replaceAll('’', "'");
  return language === 'zh' ? [...value].filter(c => /[\p{L}\p{N}]/u.test(c)) : value.replaceAll("'", '').match(/[\p{L}\p{N}]+/gu) ?? [];
}
function distance(a, b) {
  let row = Array.from({length: b.length + 1}, (_, i) => i);
  for (let i = 0; i < a.length; i++) {
    const next = [i + 1];
    for (let j = 0; j < b.length; j++) next.push(Math.min(row[j+1] + 1, next[j] + 1, row[j] + Number(a[i] !== b[j])));
    row = next;
  }
  return row[b.length];
}
const started = new Date().toISOString();
const hash = file => createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const manifest = {started, repeats, fixtureCount:fixtures.length, policies, platform:process.platform, logicalCPUs:os.cpus().length, cpu:os.cpus()[0]?.model, totalMemoryGiB:os.totalmem()/2**30, inferenceThreads:4, microphone:false, nativeInjection:false,
  executable_sha256:hash(executable), model_sha256:hash(path.join(modelDir,'model.int8.onnx')), tokens_sha256:hash(path.join(modelDir,'tokens.txt')), fixtures_sha256:hash(fixturePath),
  audio_sha256:fixtures.map(f=>({id:f.id,sha256:hash(path.resolve(path.dirname(fixturePath),f.id+'.wav'))})),
  scope:'Synthetic same-machine warm-model replay; background application load is not controlled. Not a cross-product or real-speaker benchmark.'};
fs.writeFileSync(path.join(outputDir, 'experiment.json'), JSON.stringify(manifest, null, 2));
const results = [];
for (let repeat = 0; repeat < repeats; repeat++) {
  // Rotate the order to reduce systematic time-of-run bias without concurrency.
  const order = policies.map((_, i) => policies[(i + repeat * 2) % policies.length]);
  for (const fixture of fixtures) for (const [mode, minimum] of order) {
    const name = `${fixture.id}-${mode}-${minimum ?? 'default'}-r${repeat+1}`;
    const reportPath = path.resolve(outputDir, `${name}.json`);
    const args = [path.resolve(modelDir), path.resolve(path.dirname(fixturePath), `${fixture.id}.wav`), fixture.language, mode, '0', '0', '0', reportPath, '--memory'];
    if (minimum !== null) args.push('--segment-min-ms', String(minimum));
    console.log(`[${new Date().toISOString()}] ${results.length+1}/${repeats*fixtures.length*policies.length} ${name}`);
    const child = spawnSync(path.resolve(executable), args, {cwd:path.dirname(path.resolve(executable)), windowsHide:true, encoding:'utf8', timeout:180000, maxBuffer:1024*1024});
    if (child.status !== 0 || child.error) {
      fs.writeFileSync(path.join(outputDir, `${name}.error.log`), `${child.error?.message ?? ''}\n${child.stdout ?? ''}\n${child.stderr ?? ''}`);
      throw new Error(`Replay failed for ${name}; inspect its local error log`);
    }
    const report = JSON.parse(fs.readFileSync(reportPath, 'utf8'));
    if (report.native_injection !== false || report.append_events_match !== true) throw new Error('Replay safety/append contract failed');
    const reference = units(fixture.text, fixture.language);
    if (!reference.length) throw new Error('Empty normalized reference; do not report a false error rate');
    const errors = distance(reference, units(report.trace.final_text, fixture.language));
    const row = {fixture:fixture.id, language:fixture.language, mode, minimum_ms:minimum, repeat:repeat+1, errors, reference_units:reference.length, error_percent:100*errors/reference.length, release_ms:report.release_to_result_ms, first_output_ms:report.inserts[0]?.at_ms ?? null, max_tick_ms:report.max_tick_ms, chunks:report.trace.asr_chunks, append_events_match:true, text:report.trace.final_text};
    results.push(row);
    fs.writeFileSync(path.join(outputDir, 'summary.json'), JSON.stringify(results, null, 2));
    console.log(`  ${fixture.language === 'zh' ? 'CER' : 'WER'} ${row.error_percent.toFixed(2)}%; release ${row.release_ms.toFixed(1)} ms; first ${row.first_output_ms?.toFixed(1)} ms; ${row.chunks} chunks`);
  }
}
fs.writeFileSync(path.join(outputDir, 'experiment.json'), JSON.stringify({...manifest, finished:new Date().toISOString(), completed:results.length}, null, 2));
console.log(`Completed ${results.length} synthetic replays; started ${started}`);
