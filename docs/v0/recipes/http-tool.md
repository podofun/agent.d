# Recipe: HTTP tool

Call an external JSON API from an action using `ctx.http.client`, parse the response with `:json()`, and expose the result to callers. Covers the `net:<host>` grant and how to invoke the action from the terminal.

This recipe uses a public weather API as the example target — replace the URL and fields with any JSON API you need.

## Configuration layout

```
my-http-tool/
├── init.lua
├── tools/
│   └── weather.lua
└── grants.toml
```

## The tool

**Required permission:** `net:api.open-meteo.com`

```lua
-- tools/weather.lua
agentd.tool({
  name = "weather",
  requires = { "net:api.open-meteo.com" },
})

agentd.action({
  name = "weather.current",
  requires = { "net:api.open-meteo.com" },
  handler = function(args, ctx)
    assert(type(args.latitude) == "number", "latitude (number) is required")
    assert(type(args.longitude) == "number", "longitude (number) is required")

    local client = ctx.http.client({
      base_url = "https://api.open-meteo.com",
      headers  = { Accept = "application/json" },
    })

    -- Build the full path with query parameters embedded as a string.
    local path = string.format(
      "/v1/forecast?latitude=%s&longitude=%s&current_weather=true",
      args.latitude,
      args.longitude
    )
    local res = client:get(path)

    if res.status ~= 200 then
      error(("weather API returned %d: %s"):format(res.status, res.body))
    end

    local data = res:json()
    local cw   = data.current_weather
    return {
      temperature    = cw.temperature,
      windspeed      = cw.windspeed,
      weathercode    = cw.weathercode,
      time           = cw.time,
    }
  end,
})
```

### What each part does

| Call | What it does | Permission |
|---|---|---|
| `ctx.http.client({ base_url, headers })` | Creates a reusable client pinned to a base URL with default headers | `net:<host>` |
| `client:get(path)` | Issues a GET; returns a `Response` | (same as client) |
| `res.status` | HTTP status code (integer) | — |
| `res.body` | Raw response body string | — |
| `res:json()` | Parses `res.body` as JSON; returns a Lua table | — |

::: tip Header auth
For APIs that require a token, add it in the `headers` table:
```lua
ctx.http.client({
  base_url = "https://api.example.com",
  headers  = { Authorization = "Bearer " .. ctx.secret.get("my_api_key") },
})
```
Store the key once with `agentctl secret set my_api_key` and declare `secret:my_api_key` in `requires`. See [ctx.secret](/v0/reference/ctx/secrets).
:::

## Entry point

```lua
-- init.lua
import("tools/weather.lua")
```

## grants.toml

```toml
[tool.weather]
granted = ["net:api.open-meteo.com"]
```

The `net:<host>` slug must match the hostname exactly (no scheme, no path). Wildcards are supported for broad grants (`net:*`), but prefer the narrowest specifier possible.

## How to run

```bash [release]
agentd --init my-http-tool/init.lua --grants my-http-tool/grants.toml
```

```bash [cargo]
cargo run -p daemon -- --init my-http-tool/init.lua --grants my-http-tool/grants.toml
```

## Invoke from the terminal

```bash
agentctl call weather.current -d latitude=48.85 -d longitude=2.35
```

`-d key=value` arguments are parsed as JSON, so numeric values are sent as numbers. The response is a `{ result, duration_ms }` envelope:

```json
{
  "result": {
    "temperature": 18.4,
    "windspeed": 12.1,
    "weathercode": 3,
    "time": "2025-06-19T14:00"
  },
  "duration_ms": 210
}
```

Add `--result-only` to strip the envelope:

```bash
agentctl call weather.current -d latitude=48.85 -d longitude=2.35 --result-only
```

## Verify

1. `agentctl health` returns `ok`.
2. `agentctl tools` lists `weather.current`.
3. `agentctl call weather.current -d latitude=48.85 -d longitude=2.35` returns a JSON object with temperature and windspeed fields.
4. Calling with a missing argument returns an error from the `assert` in the handler.

## Adapting to other APIs

- **POST with a JSON body:** use `client:post(path, { field = value })` — the body table is JSON-encoded automatically.
- **Custom method or full control:** use `client:request({ method = "PATCH", url = "/…", json = {…} })`.
- **Timeout:** pass `timeout_ms` to `ctx.http.client` or to individual call options.

## See also

- [ctx.http reference](/v0/reference/ctx/http)
- [ctx.secret reference](/v0/reference/ctx/secrets)
- [Permission slugs](/v0/security/permission-slugs)
- [Writing tools](/v0/writing/tools)
