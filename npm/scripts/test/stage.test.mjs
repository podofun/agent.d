import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, readdir, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { PLATFORMS, readVersion, launcherManifest, nativeManifest, stage } from '../stage.mjs';

const REPO = new URL('../../../', import.meta.url).pathname;

test('readVersion picks the workspace version out of Cargo.toml', () => {
  const toml = `[workspace]\nmembers = ["crates/*"]\n\n[workspace.package]\nversion = "1.2.3-rc.1"\nedition = "2021"\n`;
  assert.equal(readVersion(toml), '1.2.3-rc.1');
});

test('readVersion fails loudly with no version line', () => {
  assert.throws(() => readVersion('[workspace]\n'), { message: 'Could not find a version line in Cargo.toml.' });
});

test('PLATFORMS covers the four shipped targets with the right gates', () => {
  assert.deepEqual(Object.keys(PLATFORMS).sort(), ['darwin-arm64', 'darwin-x64', 'linux-x64', 'win32-x64']);
  assert.deepEqual(PLATFORMS['linux-x64'], { os: 'linux', cpu: 'x64', libc: ['glibc'], files: ['agentd', 'agentctl'] });
  assert.deepEqual(PLATFORMS['darwin-x64'], { os: 'darwin', cpu: 'x64', files: ['agentd', 'agentctl'] });
  assert.deepEqual(PLATFORMS['win32-x64'], { os: 'win32', cpu: 'x64', files: ['agentd.exe', 'agentctl.exe', 'agentd-netbroker.exe'] });
});

test('launcherManifest stamps the version and all four optional deps', () => {
  const m = launcherManifest({ name: '@podofun/agentd', version: '0.0.0', optionalDependencies: {}, bin: {}, scripts: { test: 'x' } }, '0.9.0');
  assert.equal(m.version, '0.9.0');
  assert.deepEqual(m.optionalDependencies, {
    '@podofun/agentd-linux-x64': '0.9.0',
    '@podofun/agentd-darwin-x64': '0.9.0',
    '@podofun/agentd-darwin-arm64': '0.9.0',
    '@podofun/agentd-win32-x64': '0.9.0',
  });
  assert.equal('scripts' in m, false, 'scripts are dropped from the published manifest');
  assert.equal('author' in m, false);
});

test('nativeManifest has the gate fields and nothing personal', () => {
  const m = nativeManifest('linux-x64', '0.9.0');
  assert.equal(m.name, '@podofun/agentd-linux-x64');
  assert.equal(m.version, '0.9.0');
  assert.deepEqual(m.os, ['linux']);
  assert.deepEqual(m.cpu, ['x64']);
  assert.deepEqual(m.libc, ['glibc']);
  assert.deepEqual(m.files, ['bin']);
  assert.equal(m.publishConfig.access, 'public');
  assert.equal('author' in m, false);
  assert.equal('libc' in nativeManifest('darwin-arm64', '0.9.0'), false);
});

test('stage(platform) copies binaries into bin/ and writes the manifest', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'stage-'));
  const binDir = join(dir, 'release');
  await mkdir(binDir);
  await writeFile(join(binDir, 'agentd'), '#!/bin/sh\necho agentd\n', { mode: 0o755 });
  await writeFile(join(binDir, 'agentctl'), '#!/bin/sh\necho agentctl\n', { mode: 0o755 });
  const out = join(dir, 'out');
  const r = await stage('linux-x64', binDir, out, { version: '0.9.0', repoRoot: REPO });
  assert.equal(r.name, '@podofun/agentd-linux-x64');
  assert.deepEqual((await readdir(join(out, 'bin'))).sort(), ['agentctl', 'agentd']);
  const mode = (await stat(join(out, 'bin', 'agentd'))).mode & 0o111;
  assert.notEqual(mode, 0, 'binary stays executable');
  const manifest = JSON.parse(await readFile(join(out, 'package.json'), 'utf8'));
  assert.equal(manifest.version, '0.9.0');
  assert.ok((await readFile(join(out, 'LICENSE'), 'utf8')).includes('podofun'));
});

test('stage(platform) fails when a binary is missing', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'stage-'));
  const binDir = join(dir, 'release');
  await mkdir(binDir);
  await writeFile(join(binDir, 'agentd'), '', { mode: 0o755 });
  await assert.rejects(stage('linux-x64', binDir, join(dir, 'out'), { version: '0.9.0', repoRoot: REPO }), {
    message: `Missing binary ${join(binDir, 'agentctl')} for linux-x64.`,
  });
});

test('stage(cli) copies the launcher files and stamps the manifest', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'stage-'));
  const out = join(dir, 'out');
  const r = await stage('cli', '-', out, { version: '0.9.0', repoRoot: REPO });
  assert.equal(r.name, '@podofun/agentd');
  assert.deepEqual((await readdir(join(out, 'bin'))).sort(), ['agentctl.js', 'agentd.js']);
  await stat(join(out, 'native.js'));
  await stat(join(out, 'LICENSE'));
  const manifest = JSON.parse(await readFile(join(out, 'package.json'), 'utf8'));
  assert.equal(manifest.version, '0.9.0');
  assert.equal(manifest.optionalDependencies['@podofun/agentd-win32-x64'], '0.9.0');
});

test('stage rejects an unknown kind', async () => {
  await assert.rejects(stage('plan9-mips', '-', '/nonexistent', { version: '0.9.0', repoRoot: REPO }), {
    message: 'Unknown package kind plan9-mips. Use cli, linux-x64, darwin-x64, darwin-arm64, or win32-x64.',
  });
});
