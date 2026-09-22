// Mechanical inventory of public source bytes. Excludes itself (the JSON),
// ignored build output and Git metadata; never scans private sibling trees.
import {execFileSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import {fileURLToPath} from 'node:url';
const root = fileURLToPath(new URL('../../', import.meta.url));
const destination = new URL('../../SNAPSHOT-MANIFEST.json', import.meta.url);
const manifest = JSON.parse(readFileSync(destination, 'utf8'));
const paths = [...new Set(execFileSync('git', ['ls-files', '--cached', '--others', '--exclude-standard', '-z'], {cwd:root, encoding:'utf8'}).split('\0').filter(Boolean))].sort();
delete manifest.binary_release_ready;
manifest.schema = 'vocalcode-community-source-v1';
manifest.binary_release_policy = 'Signed installers are published only after the protected release workflow verifies both platforms; consult the release run for results.';
manifest.excluded = manifest.excluded.map(x => x === 'production workflows' ? 'legacy commerce deployment workflows' : x);
manifest.files = paths.filter(path => path !== 'SNAPSHOT-MANIFEST.json').map(path => {
  let bytes = readFileSync(new URL('../../' + path, import.meta.url));
  // Match .gitattributes text=auto eol=lf; preserve binary assets byte for byte.
  if (!bytes.includes(0)) {
    try { bytes = Buffer.from(new TextDecoder('utf-8', {fatal:true}).decode(bytes).replace(/\r\n/g, '\n')); }
    catch { /* binary/non-UTF8 input */ }
  }
  return {path, bytes:bytes.length, sha256:createHash('sha256').update(bytes).digest('hex')};
});
const expected = JSON.stringify(manifest, null, 2) + '\n';
if (process.argv.includes('--write')) writeFileSync(destination, expected);
else if (readFileSync(destination, 'utf8').replace(/\r\n/g, '\n') !== expected) throw new Error('Source manifest is stale; run node packaging/community/source-manifest.mjs --write');
console.log(`Verified ${manifest.files.length} public source files.`);
