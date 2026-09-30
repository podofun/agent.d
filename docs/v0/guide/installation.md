# Installation

There are four ways to install. npm is the quickest if you already have Node, a release archive needs nothing at all, the container image suits servers, and building from source is for when you want to change the runtime itself. Every option gives you the same two programs, `agentd` and `agentctl`.

## Install with npm

If you have Node.js, this is the easiest option and the one we recommend. It works on Linux (x86-64), macOS (Apple Silicon and Intel), and Windows (x86-64), and npm downloads only the build for your platform.

```bash
npm install --global @podofun/agentd
agentd --version
```

Running the same command again upgrades you to the latest release. To stay on one version, pin it the usual npm way, as `@podofun/agentd@<version>`.

## Download a release

If you would rather not use npm, every version is published as a plain archive on the [releases page](https://github.com/podofun/agent.d/releases).

| Platform | Archive |
|---|---|
| Linux (x86-64) | `agentd-x86_64-linux.tar.gz` |
| macOS (Apple Silicon) | `agentd-aarch64-macos.tar.gz` |
| macOS (Intel) | `agentd-x86_64-macos.tar.gz` |
| Windows (x86-64) | `agentd-x86_64-windows.zip` |

Each release also includes a `SHA256SUMS.txt` file, so you can check that your download arrived intact before you install it.

::: code-group

```bash [Linux / macOS]
tar -xzf agentd-x86_64-linux.tar.gz
sha256sum -c SHA256SUMS.txt --ignore-missing
install -m 0755 agentd agentctl ~/.local/bin/
```

```powershell [Windows]
Expand-Archive agentd-x86_64-windows.zip -DestinationPath agentd
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\Programs\agentd" | Out-Null
Move-Item agentd\*.exe "$env:LOCALAPPDATA\Programs\agentd"
```

:::

On Windows, add that folder to your `PATH` afterwards. The Windows archive contains a third program, `agentd-netbroker.exe`, which the runtime uses to confine network access for the commands your agents run. Keep it in the same folder as `agentd.exe`.

## Run it in a container

For servers, a container image is published with every release at `ghcr.io/podofun/agent.d`. The repository's `docker-compose.yml` is a working starting point: it mounts a config folder into the container and serves on port 7777. Containers have no system keychain, so API keys are passed in as environment variables instead. [Running in a container](/v0/operations/deployment#running-in-a-container) covers both.

## Build from source

You need Git and Rust 1.85 or newer. If you already have Rust, `rustup update stable` brings it up to date.

```bash
git clone https://github.com/podofun/agent.d
cd agent.d
cargo build --release
cp target/release/agentd target/release/agentctl ~/.local/bin/
```

## Check that it works

Both programs answer `--version`:

```bash
agentd --version
agentctl --version
```

## Windows only: turn on network sandboxing

If your agents will run commands that need network access, run this once from a terminal opened as Administrator:

```powershell
agentd --install-sandbox
```

It sets up the part of the sandbox that confines those commands to the hosts you grant. After that, the runtime itself runs as a normal user. Linux and macOS need no setup. [The shell sandbox](/v0/security/sandbox) explains what gets confined.

## Where your files live

The runtime uses the standard places for each operating system. You create the setup folder when you write your first `init.lua` (the [tutorial](/v0/tutorial/) walks through it); the other two are created for you.

| What | Linux | macOS | Windows |
|---|---|---|---|
| Your setup: `init.lua`, `grants.toml`, `config.toml` | `~/.config/agentd/` | `~/Library/Application Support/agentd/` | `%APPDATA%\agentd\` |
| Stored data: installed packages, agent memory, chat history | `~/.local/share/agentd/` | `~/Library/Application Support/agentd/` | `%APPDATA%\agentd\` |
| Runtime state: access tokens, the trace log | `~/.local/state/agentd/` | `~/Library/Application Support/agentd/` | `%LOCALAPPDATA%\agentd\` |

On Linux these follow your `XDG_*` variables when you have set them. Examples elsewhere in these docs use the Linux paths, so substitute the matching folder on other systems. Any location can also be changed with a flag or environment variable, listed in the [configuration reference](/v0/reference/configuration).

On first start the runtime creates its own access tokens and saves them in the state folder, readable only by you. `agentctl` picks them up automatically, so on your own machine there is nothing to configure before you connect.

API keys for model providers are stored in your operating system's keychain rather than in a config file. [Credentials](/v0/providers/credentials) shows how to add them.

## Next step

Head to the [Quick start](/v0/guide/quick-start) to run your first agent.
