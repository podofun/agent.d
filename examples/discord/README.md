# Discord chatbot example

A simple demonstration of a Discord chatbot with memory.

## Run

```
echo '<bot-token>' | agentctl secret set discord_token
agentd --init examples/discord/init.lua --grants examples/discord/grants.toml
```
