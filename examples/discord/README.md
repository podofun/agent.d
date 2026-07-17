# Discord chatbot example

A simple demonstration of a Discord chatbot with memory.

## Run

```
agentd --init examples/discord/init.lua --grants examples/discord/grants.toml
agentctl call discord.set_token -d token='<bot-token>' --result-only
```
