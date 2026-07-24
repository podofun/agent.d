# agentd-luals

`agentd-luals` generates Lua Language Server support files for an agentd project.

`write_project` writes the static agentd API definitions, project-specific action, runner, and skill names, typed Lua imports, and a merged `.luarc.json` file.

The generated files improve completion and type checking. They do not affect runtime behavior.
