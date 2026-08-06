# Receive a signed webhook

Use a webhook route to receive a JSON request. The route verifies the request signature before it calls an action.

## Create the action

Create an action that accepts `headers` and `payload`.

```lua
-- webhook.lua
agentd.action({
  name = "events.receive",
  strict = false,
  input = {
    headers = { type = "object", required = true },
    payload = { type = "object", required = true },
  },
  handler = function(args, ctx)
    ctx.log.info("received webhook " .. tostring(ctx.caller.session))
    return { received = true }
  end,
})
```

Use `strict = false`. Webhook headers and payloads contain keys that the sender defines.

Import the file from `init.lua`.

```lua
import("webhook.lua")
```

## Store the secret

Use the same secret in agent.d and in the sender. Store the secret in the OS keyring.

```bash
echo "$WEBHOOK_SECRET" | agentctl secret set webhook_secret
```

Do not put the secret in `config.toml`.

## Configure the route

Add the route to `config.toml`.

```toml
[webhooks.events]
action = "events.receive"
secret = "webhook_secret"
signature_header = "x-webhook-signature-256"
signature_prefix = "sha256="
id_header = "x-webhook-id"
```

This configuration creates `POST /webhooks/events`.

The `secret` value is the keyring name. The `signature_header` value identifies the signature header.

The `signature_prefix` field is optional. The default value is an empty string.

The `id_header` field is optional. If you set this field, each request must contain the header.

Restart agent.d after you change the route or its secret.

## Grant the action

Add the webhook interface to `grants.toml`.

```toml
[interface."webhook.events"]
allowed_actions = ["events.receive"]
```

## Configure the sender

Configure the sender with these values:

- URL: `https://agentd.example.com/webhooks/events`
- Method: `POST`
- Content type: `application/json`
- Signature algorithm: HMAC-SHA256
- Signature encoding: hexadecimal
- Signature header: `x-webhook-signature-256`
- Signature prefix: `sha256=`
- Request ID header: `x-webhook-id`

Calculate the signature from the exact request body. Do not calculate it from a changed or formatted copy.

## Read the action input

The action receives this input:

```lua
{
  headers = {
    ["content-type"] = "application/json",
    ["x-webhook-id"] = "request-123",
  },
  payload = { ... },
}
```

Header names use lowercase characters. agent.d removes these headers from the action input:

- The configured signature header
- `Authorization`
- `Cookie`
- `Proxy-Authorization`

`ctx.caller.interface` is `webhook.events`. `ctx.caller.session` is the request ID.

If you do not set `id_header`, agent.d creates the request ID.

## Check the response

| Condition | Response |
|---|---|
| The signature and action are valid | `202 Accepted` |
| The route does not exist | `404 Not Found` |
| The signature is missing or invalid | `401 Unauthorized` |
| The request ID is missing | `400 Bad Request` |
| The body is not valid JSON | `400 Bad Request` |
| The action fails | `500 Internal Server Error` |

agent.d writes action errors to the trace. The response does not contain the action error.

## See also

- [Webhook configuration](/v0/reference/configuration)
- [Permissions and grants](/v0/security/grants)
