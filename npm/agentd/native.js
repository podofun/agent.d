// Resolves and runs the native agent.d binaries shipped in the per-platform
// packages that @podofun/agentd lists as optional dependencies. npm installs
// exactly the one whose os/cpu gate matches this machine; everything here is
// about finding it and handing control over to it cleanly.
import { createRequire } from 'node:module';
import { spawn } from 'node:child_process';

const require = createRequire(import.meta.url);

export const SUPPORTED = Object.freeze(['linux-x64', 'darwin-x64', 'darwin-arm64', 'win32-x64']);

const BINARIES = new Set(['agentd', 'agentctl']);

export function packageFor(platform) {
  if (!SUPPORTED.includes(platform)) {
    throw new Error(`No prebuilt agent.d binaries for ${platform}. Build from source with cargo instead.`);
  }
  return `@podofun/agentd-${platform}`;
}

export function binaryPath(name, opts = {}) {
  if (!BINARIES.has(name)) throw new Error(`Unknown binary: ${name}`);
  const platform = opts.platform ?? `${process.platform}-${process.arch}`;
  const resolve = opts.resolve ?? require.resolve;
  const pkg = packageFor(platform);
  const file = platform.startsWith('win32-') ? `${name}.exe` : name;
  try {
    return resolve(`${pkg}/bin/${file}`);
  } catch (cause) {
    throw new Error(`Missing ${pkg}. Reinstall @podofun/agentd with optional dependencies enabled.`, { cause });
  }
}

export function runBinary(name) {
  let child;
  try {
    child = spawn(binaryPath(name), process.argv.slice(2), { stdio: 'inherit' });
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
    return;
  }
  // Forward the two signals a shell or supervisor sends, and mirror the
  // child's fate so `agentd` behaves exactly like the real binary would.
  const signals = ['SIGINT', 'SIGTERM'];
  const forward = signals.map(signal => () => child.kill(signal));
  signals.forEach((signal, i) => process.on(signal, forward[i]));
  child.on('error', error => {
    console.error(error.message);
    process.exitCode = 1;
  });
  child.on('close', (code, signal) => {
    signals.forEach((s, i) => process.off(s, forward[i]));
    if (signal) process.kill(process.pid, signal);
    else process.exitCode = code ?? 1;
  });
}
