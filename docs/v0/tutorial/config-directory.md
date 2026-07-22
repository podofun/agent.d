# Step 1: Make the Configuration Directory

agent.d uses one configuration directory. This directory contains all your actions, skills, runners, services, and grants.

On Linux, the default directory is `~/.config/agentd`. This tutorial uses this directory.

Put all agent.d configuration files in this one directory. Do not make a different configuration directory for each agent.

## Make the directories

Run these commands:

```bash
mkdir -p ~/.config/agentd/tools
mkdir -p ~/.config/agentd/skills
mkdir -p ~/.config/agentd/runners
cd ~/.config/agentd
```

After Step 4, the directory will have this structure:

```text
agentd/
├── init.lua
├── grants.toml
├── tools/
│   └── git.lua
├── skills/
│   └── reviewer.md
└── runners/
    └── backend_reviewer.lua
```

## Write `init.lua`

At startup, agent.d evaluates `~/.config/agentd/init.lua`. This file loads the other configuration files.

Create `~/.config/agentd/init.lua` with this content:

```lua
import("tools/git.lua")

agentd.skills.dir("skills")

import("runners/backend_reviewer.lua")
```

The first line loads the Git action. The second line loads all Markdown skill files in `skills/`.

The last line loads the runner. File order is important because the runner uses the action and skill.

Paths in `import()` are relative to `init.lua`. agent.d rejects absolute paths and paths that contain `..`.

The `tools`, `skills`, and `runners` names are conventions. You can use other names inside the configuration directory.

Do not start agent.d now. The imported files do not exist until you complete the next steps.

## Next step

[Step 2: Write the Git action](/v0/tutorial/first-tool)

## See also

- [Write init.lua](/v0/writing/init)
- [Runtime concepts](/v0/concepts/runtime)
