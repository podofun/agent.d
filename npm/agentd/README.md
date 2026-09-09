# @podofun/agentd

The agent.d runtime and its command-line client, installed from npm.

```sh
npm i -g @podofun/agentd
agentd --version
```

This package holds only a small launcher. The binaries for your platform arrive
through one of these optional dependencies, chosen by npm at install time:

- `@podofun/agentd-linux-x64`
- `@podofun/agentd-darwin-x64`
- `@podofun/agentd-darwin-arm64`
- `@podofun/agentd-win32-x64`

If you install with optional dependencies disabled, `agentd` will tell you which
package it is missing. Other platforms build from source with `cargo`; see the
repository at https://github.com/podofun/agent.d.

Pre-releases publish under the `next` tag: `npm i -g @podofun/agentd@next`.
