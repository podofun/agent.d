// Stages one npm package into a directory that `npm pack` can turn into a
// tarball. Pure and offline: reads built binaries and the committed launcher
// sources, writes a directory. Publishing is the workflow's job.
//
//   node npm/scripts/stage.mjs <cli|linux-x64|darwin-x64|darwin-arm64|win32-x64> <binDir> <outDir>
import { cp, mkdir, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const HERE = fileURLToPath(new URL('.', import.meta.url));
const DEFAULT_REPO_ROOT = resolve(HERE, '..', '..');
const LAUNCHER_DIR = resolve(HERE, '..', 'agentd');

export const PLATFORMS = Object.freeze({
  'linux-x64': { os: 'linux', cpu: 'x64', libc: ['glibc'], files: ['agentd', 'agentctl'] },
  'darwin-x64': { os: 'darwin', cpu: 'x64', files: ['agentd', 'agentctl'] },
  'darwin-arm64': { os: 'darwin', cpu: 'arm64', files: ['agentd', 'agentctl'] },
  'win32-x64': { os: 'win32', cpu: 'x64', files: ['agentd.exe', 'agentctl.exe', 'agentd-netbroker.exe'] },
});

const KINDS = ['cli', ...Object.keys(PLATFORMS)];

export function readVersion(cargoToml) {
  const m = cargoToml.match(/^version\s*=\s*"([^"]+)"/m);
  if (!m) throw new Error('Could not find a version line in Cargo.toml.');
  return m[1];
}

export function launcherManifest(template, version) {
  const { scripts, ...rest } = template;
  const optionalDependencies = Object.fromEntries(
    Object.keys(PLATFORMS).map(p => [`@podofun/agentd-${p}`, version]),
  );
  return { ...rest, version, optionalDependencies };
}

export function nativeManifest(platform, version) {
  const p = PLATFORMS[platform];
  return {
    name: `@podofun/agentd-${platform}`,
    version,
    description: `Native agent.d binaries for ${platform}`,
    license: 'MIT',
    repository: { type: 'git', url: 'https://github.com/podofun/agent.d.git', directory: 'npm/agentd' },
    homepage: 'https://github.com/podofun/agent.d',
    os: [p.os],
    cpu: [p.cpu],
    ...(p.libc ? { libc: p.libc } : {}),
    files: ['bin'],
    publishConfig: { access: 'public' },
  };
}

async function exists(path) {
  try { await stat(path); return true; } catch { return false; }
}

export async function stage(kind, binDir, outDir, opts) {
  if (!KINDS.includes(kind)) {
    throw new Error(`Unknown package kind ${kind}. Use ${KINDS.slice(0, -1).join(', ')}, or ${KINDS.at(-1)}.`);
  }
  const repoRoot = opts.repoRoot ?? DEFAULT_REPO_ROOT;
  const version = opts.version;
  await rm(outDir, { recursive: true, force: true });
  await mkdir(join(outDir, 'bin'), { recursive: true });
  await cp(join(repoRoot, 'LICENSE'), join(outDir, 'LICENSE'));

  let manifest;
  if (kind === 'cli') {
    const template = JSON.parse(await readFile(join(LAUNCHER_DIR, 'package.json'), 'utf8'));
    manifest = launcherManifest(template, version);
    await cp(join(LAUNCHER_DIR, 'native.js'), join(outDir, 'native.js'));
    await cp(join(LAUNCHER_DIR, 'bin'), join(outDir, 'bin'), { recursive: true });
    const readme = join(LAUNCHER_DIR, 'README.md');
    if (await exists(readme)) await cp(readme, join(outDir, 'README.md'));
  } else {
    manifest = nativeManifest(kind, version);
    for (const file of PLATFORMS[kind].files) {
      const src = join(binDir, file);
      if (!(await exists(src))) throw new Error(`Missing binary ${src} for ${kind}.`);
      await cp(src, join(outDir, 'bin', file));
    }
  }
  await writeFile(join(outDir, 'package.json'), JSON.stringify(manifest, null, 2) + '\n');
  return { name: manifest.name, version, outDir };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [kind, binDir, outDir] = process.argv.slice(2);
  if (!kind || !binDir || !outDir) {
    console.error('Usage: node npm/scripts/stage.mjs <cli|platform> <binDir> <outDir>');
    process.exit(2);
  }
  const version = readVersion(await readFile(join(DEFAULT_REPO_ROOT, 'Cargo.toml'), 'utf8'));
  const r = await stage(kind, binDir, outDir, { version });
  console.log(`staged ${r.name}@${r.version} -> ${r.outDir}`);
}
