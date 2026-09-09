import { test } from 'node:test';
import assert from 'node:assert/strict';
import { SUPPORTED, packageFor, binaryPath } from '../native.js';

test('SUPPORTED lists exactly the four shipped platforms', () => {
  assert.deepEqual([...SUPPORTED].sort(), ['darwin-arm64', 'darwin-x64', 'linux-x64', 'win32-x64']);
});

test('packageFor maps a supported platform to its scoped package', () => {
  assert.equal(packageFor('linux-x64'), '@podofun/agentd-linux-x64');
  assert.equal(packageFor('win32-x64'), '@podofun/agentd-win32-x64');
});

test('packageFor rejects an unsupported platform with a one-sentence message', () => {
  assert.throws(() => packageFor('freebsd-x64'), {
    message: 'No prebuilt agent.d binaries for freebsd-x64. Build from source with cargo instead.',
  });
});

test('binaryPath resolves bin/<name> inside the platform package', () => {
  const seen = [];
  const resolve = id => { seen.push(id); return `/abs/${id}`; };
  const p = binaryPath('agentd', { platform: 'linux-x64', resolve });
  assert.equal(p, '/abs/@podofun/agentd-linux-x64/bin/agentd');
  assert.deepEqual(seen, ['@podofun/agentd-linux-x64/bin/agentd']);
});

test('binaryPath appends .exe on win32', () => {
  const resolve = id => `/abs/${id}`;
  assert.equal(
    binaryPath('agentctl', { platform: 'win32-x64', resolve }),
    '/abs/@podofun/agentd-win32-x64/bin/agentctl.exe',
  );
});

test('binaryPath rejects an unknown binary name', () => {
  assert.throws(() => binaryPath('nope', { platform: 'linux-x64', resolve: () => '' }), {
    message: 'Unknown binary: nope',
  });
});

test('binaryPath explains a missing optional dependency', () => {
  const resolve = () => { throw new Error('Cannot find module'); };
  assert.throws(() => binaryPath('agentd', { platform: 'darwin-arm64', resolve }), {
    message: 'Missing @podofun/agentd-darwin-arm64. Reinstall @podofun/agentd with optional dependencies enabled.',
  });
});

test('binaryPath rejects an unsupported platform before resolving', () => {
  let called = false;
  assert.throws(() => binaryPath('agentd', { platform: 'sunos-x64', resolve: () => { called = true; return ''; } }), {
    message: 'No prebuilt agent.d binaries for sunos-x64. Build from source with cargo instead.',
  });
  assert.equal(called, false);
});
