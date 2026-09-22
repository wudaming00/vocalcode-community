// Release-preparation consistency checks, not a substitute for a legal audit.
import {createHash} from 'node:crypto';

export const SOURCE_LICENSE = 'AGPL-3.0-only';
// SPDX license-list-data/text/AGPL-3.0-only.txt; normalize CRLF only.
export const LICENSE_TEXT_SHA256 = 'd8a6cc31abc16b6748c7a21f21611f5a1ec33f67d22ca23d7da1c19b95496bee';
export const FIRST_PARTY_CRATES = ['vocalcode-app', 'vocalcode-core', 'vocalcode-platform', 'vocalcode-meeting'];

export function verifyLicenseSelection(readBytes) {
  const read = name => {
    const bytes = readBytes(name);
    if (bytes === undefined || bytes === null) throw new Error(`Missing licensing input: ${name}`);
    return bytes.toString('utf8').replace(/\r\n/g, '\n');
  };
  const textHash = createHash('sha256').update(read('LICENSE')).digest('hex');
  if (textHash !== LICENSE_TEXT_SHA256) throw new Error('LICENSE does not match the reviewed AGPL v3 text');
  const root = read('Cargo.toml');
  const licences = [...root.matchAll(/^license\s*=\s*"([^"]+)"\s*$/gm)];
  if (licences.length !== 1 || licences[0][1] !== SOURCE_LICENSE) {
    throw new Error('Workspace source licence must be AGPL-3.0-only');
  }
  for (const crate of FIRST_PARTY_CRATES) {
    const manifest = read(`${crate}/Cargo.toml`);
    if (!/^license\.workspace\s*=\s*true\s*$/m.test(manifest) || /^license\s*=/m.test(manifest)) {
      throw new Error(`First-party crate must inherit the workspace licence: ${crate}`);
    }
  }
  for (const name of ['README.md', 'README.zh-CN.md', 'LICENSING.md', 'BRANDING.md', 'CONTRIBUTING.md']) {
    const text = read(name);
    if (!text.includes(SOURCE_LICENSE)) throw new Error(`Missing selected licence in ${name}`);
    if (/source licen[cs]e:\s*pending|licen[cs]e is still pending|源码许可证尚待确认/i.test(text)) {
      throw new Error(`Obsolete pending-licence notice in ${name}`);
    }
  }
  const notices = read('THIRD-PARTY-NOTICES.txt');
  if (!notices.includes(SOURCE_LICENSE) || /VocalCode's own proprietary code/.test(notices)) {
    throw new Error('Third-party notice introduction has stale first-party licensing');
  }
  return {source_license: SOURCE_LICENSE, license_text_sha256: textHash};
}
