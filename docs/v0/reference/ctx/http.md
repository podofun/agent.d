# ctx.http — HTTP

`ctx.http` makes outbound HTTP requests. You can call URLs directly or create a persistent `Client` pre-configured with a base URL, default headers, and a timeout.

**Required permission:** `net:<host>` — e.g. `net:api.github.com` or `net:*`.

## Signatures

```lua
ctx.http.get(url: string, opts?: HttpOpts) -> Response
ctx.http.post(url: string, body?: any, opts?: HttpOpts) -> Response
ctx.http.request({
  method:     string,
  url:        string,
  headers?:   table<string, string>,
  json?:      any,
  body?:      string,
  timeout_ms?: integer,
}) -> Response

ctx.http.client({
  base_url?:   string,
  headers?:    table<string, string>,
  timeout_ms?: integer,
}) -> Client
```

### Client methods

```lua
client:get(path: string, opts?: HttpOpts) -> Response
client:post(path: string, body?: any, opts?: HttpOpts) -> Response
client:request(req: table) -> Response
```

## Types

### HttpOpts

| Field | Type | Description |
|---|---|---|
| `headers` | `table<string, string>` | Additional request headers. |
| `timeout_ms` | `integer` | Request timeout in milliseconds. |
| `json` | `any` | Encode this value as the JSON body and set `Content-Type: application/json`. |

### Response

| Field/Method | Type | Description |
|---|---|---|
| `status` | `integer` | HTTP status code (e.g. `200`, `404`). |
| `headers` | `table<string, string>` | Response headers. |
| `body` | `string` | Raw response body. |
| `:json()` | `any` | Decode `body` as JSON and return the result. |

## Permission

The slug `net:<host>` must be granted for every hostname you contact. Wildcards are supported: `net:*` permits all outbound HTTP.

```toml
[tool.github]
granted = ["net:api.github.com"]
```

## Examples

```lua
-- Simple GET request
agentd.action("weather.current", function(args, ctx)
  local res = ctx.http.get("https://wttr.in/" .. args.city .. "?format=3")
  if res.status ~= 200 then
    error("weather API returned " .. res.status)
  end
  return res.body
end)
```

```lua
-- POST with JSON body and parse the response
agentd.action("notify.send", function(args, ctx)
  local res = ctx.http.post("https://hooks.example.com/notify", nil, {
    json = { text = args.message, channel = args.channel },
  })
  return { ok = res.status < 300 }
end)
```

```lua
-- Reusable client with a base URL and auth header
agentd.action("github.list_repos", function(args, ctx)
  local token = ctx.secret.get("github_token")
  local client = ctx.http.client({
    base_url  = "https://api.github.com",
    headers   = { Authorization = "Bearer " .. token },
    timeout_ms = 10000,
  })
  local res = client:get("/user/repos")
  return res:json()
end)
```

## See also

- [ctx.ws](/v0/reference/ctx/websocket)
- [ctx.secret](/v0/reference/ctx/secrets)
- [Security: permission slugs](/v0/security/permission-slugs)
- [Recipes: http-tool](/v0/recipes/http-tool)
