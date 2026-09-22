import test from 'node:test';
import assert from 'node:assert/strict';
import {existsSync, readFileSync} from 'node:fs';
import {fileURLToPath} from 'node:url';
import {resolve} from 'node:path';
import {FIRST_PARTY_CRATES, LICENSE_TEXT_SHA256, SOURCE_LICENSE, verifyLicenseSelection} from './licensing.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const templates = new Set(['README.md', 'README.zh-CN.md', 'CONTRIBUTING.md', 'MAINTAINERS.md', 'ROADMAP.md', 'PUBLICATION_BLOCKERS.md', 'BUILDING.md', 'SECURITY.md']);
function read(name) {
  const direct = resolve(root, name);
  const path = templates.has(name) && !existsSync(direct)
    ? resolve(root, 'packaging/community/templates', name) : direct;
  return readFileSync(path);
}
const override = (path, content) => name => name === path ? content : read(name);

test('first-party source and unchanged AGPL v3 text agree', () => {
  assert.deepEqual(verifyLicenseSelection(read), {source_license: SOURCE_LICENSE, license_text_sha256: LICENSE_TEXT_SHA256});
});
test('licence selection accepts a CRLF checkout without altering licence terms', () => {
  const text = read('LICENSE').toString().replace(/\r\n/g, '\n').replace(/\n/g, '\r\n');
  assert.equal(verifyLicenseSelection(override('LICENSE', Buffer.from(text))).source_license, SOURCE_LICENSE);
});
test('missing, shortened, and noncommercial-modified licence texts fail closed', () => {
  for (const content of [undefined, Buffer.from('AGPL-3.0-only'), Buffer.concat([read('LICENSE'), Buffer.from('\nCommercial use is prohibited.\n')])]) {
    assert.throws(() => verifyLicenseSelection(override('LICENSE', content)), /LICENSE|Missing licensing input/);
  }
});
test('proprietary, later-version, and permissive workspace substitutions are rejected', () => {
  for (const id of ['LicenseRef-VocalCode-Proprietary', 'AGPL-3.0-or-later', 'MIT']) {
    assert.throws(() => verifyLicenseSelection(override('Cargo.toml', Buffer.from(read('Cargo.toml').toString().replace(SOURCE_LICENSE, id)))), /Workspace source licence/);
  }
});
test('every desktop crate must inherit the selected licence', () => {
  for (const crate of FIRST_PARTY_CRATES) {
    const path = `${crate}/Cargo.toml`;
    const text = read(path).toString().replace('license.workspace = true', 'license = "MIT"');
    assert.throws(() => verifyLicenseSelection(override(path, Buffer.from(text))), /inherit the workspace licence/);
  }
});
test('stale pending wording cannot reappear in public documentation', () => {
  const text = Buffer.concat([read('README.md'), Buffer.from('\nSource licence: pending owner confirmation.\n')]);
  assert.throws(() => verifyLicenseSelection(override('README.md', text)), /Obsolete pending-licence notice/);
});
test('maintainer identity, support boundary, and public/private contact routes are documented', () => {
  const text = read('MAINTAINERS.md').toString();
  assert.match(text, /Daming Wu/);
  assert.match(text, /https:\/\/github\.com\/wudaming00/);
  assert.match(text, /not a private-message/);
  assert.match(text, /best-effort/);
  assert.match(text, /https:\/\/github\.com\/wudaming00\/vocalcode-community\/issues/);
  assert.match(text, /https:\/\/github\.com\/wudaming00\/vocalcode-community\/security\/advisories\/new/);
  assert.doesNotMatch(text, /mailto:/);
});
test('first-party public documentation links resolve in source and exported snapshot', () => {
  for (const name of [...templates, 'LICENSING.md', 'BRANDING.md']) {
    const text = read(name).toString();
    const links = [...text.matchAll(/\]\(([^)\s]+)\)/g), ...text.matchAll(/(?:href|src)="([^"]+)"/g)];
    for (const [, link] of links) {
      if (/^(?:https?:|mailto:|#)/.test(link)) continue;
      const target = decodeURIComponent(link.split('#')[0]);
      const path = target === 'docs/assets/vocalcode-community.svg' && !existsSync(resolve(root, target))
        ? resolve(root, 'packaging/community/assets/vocalcode-community.svg')
        : templates.has(target) && !existsSync(resolve(root, target))
          ? resolve(root, 'packaging/community/templates', target) : resolve(root, target);
      assert.ok(existsSync(path), `${name} -> ${link}`);
    }
  }
});
