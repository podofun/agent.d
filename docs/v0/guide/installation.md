# Installation

Install agent.d either by downloading a pre-built release or by building from
source. This page covers both, putting the binaries on your PATH, and the
runtime locations the daemon uses on Linux, macOS, and Windows.

## Download a release

Pre-built binaries are published on the [GitHub releases page](https://github.com/podofun/agent.d/releases)
for each tagged version. This is the fastest way to get started — no toolchain required.

Pick the archive for your platform:

| Platform | Archive |
|---|---|
| Linux (x86-64) | `agentd-x86_64-linux.tar.gz` |
| macOS (Apple Silicon) | `agentd-aarch64-macos.tar.gz` |
| Windows (x86-64) | `agentd-x86_64-windows.zip` |

Each archive contains both binaries (`daemon` and `agentctl`, with `.exe`
extensions on Windows) plus the README and license. A `SHA256SUMS.txt` is
published alongside the archives so you can verify your download.

::: code-group

```bash [Linux / macOS]
# Download and extract (replace the archive with your platform's file)
tar -xzf agentd-x86_64-linux.tar.gz

# Verify the checksum against SHA256SUMS.txt
sha256sum -c SHA256SUMS.txt --ignore-missing

# Install onto your PATH
install -m 0755 daemon agentctl ~/.local/bin/
```

```powershell [Windows]
# Extract the archive
Expand-Archive agentd-x86_64-windows.zip -DestinationPath agentd

# Move the binaries somewhere on your PATH, e.g.
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\Programs\agentd" | Out-Null
Move-Item agentd\daemon.exe, agentd\agentctl.exe "$env:LOCALAPPDATA\Programs\agentd"
# Then add that folder to your PATH (System settings → Environment Variables)
```

:::

::: tip Windows: enable sandboxed networking
If your agents run network-enabled shell tools, run this once in an elevated terminal so the sandbox can confine their network:

```powershell
daemon --install-sandbox
```

The daemon itself then runs without Administrator. See [Shell sandbox](/v0/security/sandbox#windows-one-time-network-setup) for details.
:::

Then jump to the [quick start](/v0/guide/quick-start).

## Build from source

### Prerequisites

- **Rust 1.85 or newer.** Install or update via [rustup](https://rustup.rs):

  ```bash
  rustup update stable
  rustc --version   # should print 1.85.x or newer
  ```

- **Git** (to clone the repository).

### Build

```bash
git clone https://github.com/podofun/agent.d
cd agent.d
cargo build --release
```

The release build produces two binaries:

| Binary | Path | Purpose |
|---|---|---|
| `daemon` | `target/release/daemon` | The runtime server |
| `agentctl` | `target/release/agentctl` | The console client |

## Put the binaries on your PATH

Copy or symlink both binaries to a directory on your PATH:

```bash
cp target/release/daemon target/release/agentctl ~/.local/bin/
```

Verify:

```bash
daemon --help
agentctl --help
```

## Development alternative

During active development you can run either binary directly through Cargo without a separate install step:

```bash
# Run the daemon
cargo run -p daemon -- --init examples/init.lua --grants-file examples/grants.toml

# Run agentctl
cargo run -p agentd-cli -- health
```

::: tip Hot reload
Pass `--watch` to the daemon during development. It watches your `init.lua`, every file pulled in via `import()`, loaded skill `.md` sources, and `grants.toml`, and rebuilds the runtime in place on any change without losing durable memory or a connected approval operator.
:::

## Runtime locations

The daemon stores its config, state, and data under the conventional
per-user directories for your operating system. On Linux these follow the
[XDG Base Directory](https://specifications.freedesktop.org/basedir-spec/latest/)
spec; on macOS and Windows the daemon uses the platform-native equivalents
automatically.

| Purpose | Linux | macOS | Windows |
|---|---|---|---|
| **Config** — `config.toml`, `init.lua`, `grants.toml` | `$XDG_CONFIG_HOME/agentd/` (`~/.config/agentd/`) | `~/Library/Application Support/agentd/` | `%APPDATA%\agentd\` |
| **Data** — packages, `memory.redb` | `$XDG_DATA_HOME/agentd/` (`~/.local/share/agentd/`) | `~/Library/Application Support/agentd/` | `%APPDATA%\agentd\` |
| **State** — `token`, `admin-token`, `trace.jsonl` | `$XDG_STATE_HOME/agentd/` (`~/.local/state/agentd/`) | `~/Library/Application Support/agentd/` | `%LOCALAPPDATA%\agentd\` |

::: info Linux paths in this documentation
Examples elsewhere in these docs use the Linux/XDG form (e.g.
`$XDG_STATE_HOME/agentd/trace.jsonl`). On macOS and Windows, substitute the
matching directory from the table above. You can always override any path
explicitly with a CLI flag or environment variable — see
[Configuration](/v0/reference/configuration).
:::

The daemon auto-mints bearer tokens for `/ws` and `/control` on first run and writes them to the state directory as `token` and `admin-token` (mode `0600` on Unix). `agentctl` reads these files automatically for local use, so no manual token configuration is needed during development.

## Credential storage

Provider API keys (Anthropic, OpenAI, etc.) are stored and retrieved through the OS secret store via `ctx.secret`. This keeps credentials out of config files and environment variables. See [Providers: credentials](/v0/providers/credentials) for details on seeding keys.

## See also

- [Quick start](/v0/guide/quick-start)
- [Reference: configuration](/v0/reference/configuration)
- [Reference: CLI](/v0/reference/cli)
- [Providers: credentials](/v0/providers/credentials)
