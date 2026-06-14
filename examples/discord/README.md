# Discord chatbot example

A simple demonstration of a Discord chatbot with memory.

## Run

```
daemon --init examples/discord/init.lua --grants-file examples/discord/grants.toml
agentctl call discord.set_token -d token='<bot-token>' --result-only
```
