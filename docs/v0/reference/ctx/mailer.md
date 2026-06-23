# ctx.mailer — Email (SMTP)

`ctx.mailer` sends email over SMTP. You create a mailer — a plain object holding the SMTP connection details, just like an HTTP client or a WebSocket connection — and call `:send` on it. The mailer is backed by a pooled, TLS-capable transport, so a single mailer is safe to share across coroutines and reuse for many messages.

**Required permission:** `net:<host>` — the SMTP server host, same slug family as HTTP and WebSocket. Checked once, when the mailer is created.

## Signatures

```lua
ctx.mailer.create({
  host:       string,
  from:       string,            -- default From mailbox, e.g. "Bot <bot@example.com>"
  port?:      integer,           -- default depends on security: 465 tls, 587 starttls, 25 plaintext
  user?:      string,            -- SMTP username (set with pass, or set neither)
  pass?:      string,            -- SMTP password
  security?:  "starttls" | "tls" | "plaintext",  -- default "starttls"
  timeout_ms?: integer,          -- default 30000
}) -> Mailer
```

### Mailer methods

```lua
mailer:send(mail: Mail) -> { ok: true, message_id: string | nil }
```

`:send` yields cooperatively while the SMTP round-trip is in flight, so other coroutines keep running. On failure it raises a Lua error with the SMTP/transport message.

## Types

### Create options

| Field | Type | Description |
|---|---|---|
| `host` | `string` | SMTP server hostname. Drives the `net:<host>` grant. **Required.** |
| `from` | `string` | Default `From` mailbox used when a message omits its own `from`. **Required.** |
| `port` | `integer` | TCP port. Defaults per `security`: `465` (tls), `587` (starttls), `25` (plaintext). |
| `user` | `string` | SMTP username. Must be set together with `pass`; setting only one is an error. |
| `pass` | `string` | SMTP password. |
| `security` | `string` | `"starttls"` (default), `"tls"` (implicit TLS), or `"plaintext"` (no TLS — dev/test only). |
| `timeout_ms` | `integer` | Per-operation timeout in milliseconds. Default `30000`. |

### Mail

| Field | Type | Description |
|---|---|---|
| `to` | `string[]` | Primary recipients. |
| `cc` | `string[]` | Carbon-copy recipients. |
| `bcc` | `string[]` | Blind-copy recipients — delivered, but never written into the message headers. |
| `from` | `string \| nil` | Overrides the mailer's default `from` for this message. |
| `reply_to` | `string \| nil` | `Reply-To` mailbox. |
| `subject` | `string` | Subject line. **Required.** |
| `text` | `string \| nil` | Plain-text body. |
| `html` | `string \| nil` | HTML body. |
| `attachments` | `Attachment[] \| nil` | File attachments. |

Body shape follows from which fields are set: `text` only → `text/plain`; `html` only → `text/html`; both → `multipart/alternative`; any attachments → wrapped in `multipart/mixed`. At least one of `text` or `html` is required.

### Attachment

| Field | Type | Description |
|---|---|---|
| `filename` | `string` | Name shown to the recipient. |
| `content_type` | `string` | MIME type, e.g. `"application/pdf"`. |
| `bytes` | `string` | Raw attachment bytes (a binary-safe Lua string — read a file with `ctx.fs`). |

## Permission

```toml
[action.send_report]
granted = ["net:smtp.example.com", "secret:smtp_user", "secret:smtp_pass"]
```

## Examples

```lua
-- Send a simple notification from an action
agentd.action("notify", function(args, ctx)
  local mailer = ctx.mailer.create({
    host     = "smtp.example.com",
    user     = ctx.secret.get("smtp_user"),
    pass     = ctx.secret.get("smtp_pass"),
    from     = "Bot <bot@example.com>",
    security = "starttls",
  })

  local res = mailer:send({
    to      = { args.to },
    subject = args.subject,
    text    = args.body,
  })

  return res.message_id
end)
```

```lua
-- HTML + plain-text alternative with an attachment
local mailer = ctx.mailer.create({
  host = "smtp.example.com",
  user = ctx.secret.get("smtp_user"),
  pass = ctx.secret.get("smtp_pass"),
  from = "Reports <reports@example.com>",
})

mailer:send({
  to       = { "ops@example.com" },
  cc       = { "lead@example.com" },
  subject  = "Daily report",
  text     = "See attached.",
  html     = "<p>See <b>attached</b>.</p>",
  reply_to = "support@example.com",
  attachments = {
    { filename = "report.pdf", content_type = "application/pdf", bytes = ctx.fs.read("/path/to/report.pdf") },
  },
})
```

::: tip Queueing and rate-limiting
A mailer is a plain shareable object, so the natural way to throttle or queue sending is a dedicated **service** that owns one mailer and consumes mail jobs off a [`channel`](/v0/reference/ctx/concurrency). Producers push plain-data jobs onto the channel; the service applies its own rate limit and calls `mailer:send`. This keeps all sending serialized through one place without sharing mutable state.
:::

## See also

- [ctx.http](/v0/reference/ctx/http)
- [ctx.secret](/v0/reference/ctx/secrets)
- [Concepts: services](/v0/concepts/services)
- [Security: permission slugs](/v0/security/permission-slugs)
