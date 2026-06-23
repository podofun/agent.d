//! `ctx.mailer.create(opts)` binding — a plain SMTP mailer object (like an
//! http client; no name, no registry). `create` returns a Lua table handle
//! wrapping a [`MailerHandle`] userdata; its `:send(mail)` method is a Lua
//! closure that calls a C internal and `coroutine.yield`s the resulting
//! `OpMarker`, so the send is driven yieldably by the scheduler. Lua 5.4
//! cannot yield across a C-call boundary, which is why the method must be a
//! Lua closure rather than a userdata method.

use std::sync::Arc;

use agentd_net::mailer::{Attachment, Mail, Mailer, MailerConfig, Security};
use agentd_permissions::Permission;

use crate::{block_on, check_permission_inline, scheduler};

/// Raw userdata wrapping a connected mailer. No methods — `:send` lives on the
/// Lua-side table built in [`build_mailer_table`].
struct MailerHandle {
    mailer: Arc<Mailer>,
}

impl mlua::UserData for MailerHandle {}

/// `mailer.create(opts)` C internal: parse config, gate on `net:<host>`, and
/// connect. Returns the raw userdata; the Lua ctor wraps it in a table handle.
fn mailer_create_internal(lua: &mlua::Lua, opts: mlua::Table) -> mlua::Result<mlua::AnyUserData> {
    let host: String = opts.get("host")?;
    if host.is_empty() {
        return Err(mlua::Error::external("mailer.create: `host` is required"));
    }
    let from: String = opts.get("from")?;
    if from.is_empty() {
        return Err(mlua::Error::external("mailer.create: `from` is required"));
    }
    let port: Option<u16> = opts.get("port")?;
    let user: Option<String> = opts.get("user")?;
    let pass: Option<String> = opts.get("pass")?;
    let timeout_ms: Option<u64> = opts.get("timeout_ms")?;

    // Credentials must be supplied as a pair — a half-set pair is almost always
    // a config bug (the transport layer silently drops single-sided creds).
    if user.is_some() != pass.is_some() {
        return Err(mlua::Error::external(
            "mailer.create: `user` and `pass` must be set together",
        ));
    }

    let security = match opts.get::<Option<String>>("security")? {
        None => Security::StartTls,
        Some(s) => match s.as_str() {
            "starttls" => Security::StartTls,
            "tls" => Security::Tls,
            "plaintext" => Security::Plaintext,
            other => {
                return Err(mlua::Error::external(format!(
                    "mailer.create: unknown security `{other}` (expected starttls|tls|plaintext)"
                )));
            }
        },
    };

    check_permission_inline(lua, &Permission::new(format!("net:{host}")))?;

    let cfg = MailerConfig {
        host,
        port,
        user,
        pass,
        from,
        security,
        timeout_ms,
    };
    let mailer = Mailer::connect(cfg).map_err(|e| mlua::Error::external(e.to_string()))?;
    lua.create_userdata(MailerHandle {
        mailer: Arc::new(mailer),
    })
}

/// `mailer:send(mail)` C internal. Parses the mail table, then either yields an
/// `OpMarker` (inside a coroutine) or blocks on the send (top level), returning
/// a `{ ok = true, message_id = ... }` result table.
fn mailer_send_internal(
    lua: &mlua::Lua,
    (ud, mail_table): (mlua::AnyUserData, mlua::Table),
) -> mlua::Result<mlua::Value> {
    let mailer = ud.borrow::<MailerHandle>()?.mailer.clone();

    let to: Vec<String> = mail_table
        .get::<Option<Vec<String>>>("to")?
        .unwrap_or_default();
    let cc: Vec<String> = mail_table
        .get::<Option<Vec<String>>>("cc")?
        .unwrap_or_default();
    let bcc: Vec<String> = mail_table
        .get::<Option<Vec<String>>>("bcc")?
        .unwrap_or_default();
    let from: Option<String> = mail_table.get("from")?;
    let reply_to: Option<String> = mail_table.get("reply_to")?;
    let subject: String = mail_table
        .get::<Option<String>>("subject")?
        .ok_or_else(|| mlua::Error::external("mailer:send: `subject` is required"))?;
    let text: Option<String> = mail_table.get("text")?;
    let html: Option<String> = mail_table.get("html")?;

    // Attachment `bytes` arrive as a Lua string (binary-safe), not a number
    // array — read them explicitly, mirroring `ws_send_binary_internal`.
    let attachments = match mail_table.get::<Option<Vec<mlua::Table>>>("attachments")? {
        Some(rows) => rows
            .into_iter()
            .map(|t| {
                let filename: String = t.get("filename")?;
                let content_type: String = t.get("content_type")?;
                let bytes: mlua::String = t.get("bytes")?;
                Ok::<_, mlua::Error>(Attachment {
                    filename,
                    content_type,
                    bytes: bytes.as_bytes().to_vec(),
                })
            })
            .collect::<mlua::Result<Vec<_>>>()?,
        None => Vec::new(),
    };

    let mail = Mail {
        to,
        cc,
        bcc,
        from,
        reply_to,
        subject,
        text,
        html,
        attachments,
    };

    if scheduler::is_in_coroutine(lua) {
        return scheduler::build_marker(lua, scheduler::Op::MailerSend { mailer, mail });
    }
    let outcome = block_on(async move { mailer.send(mail).await })?
        .map_err(|e| mlua::Error::external(e.to_string()))?;
    let t = lua.create_table()?;
    t.set("ok", true)?;
    t.set("message_id", outcome.message_id)?;
    Ok(mlua::Value::Table(t))
}

/// Build the `ctx.mailer` namespace table: `{ create = function(opts) ... end }`.
pub fn build_mailer_table(lua: &mlua::Lua) -> mlua::Result<mlua::Table> {
    let t = lua.create_table()?;

    let create_internal = lua.create_function(mailer_create_internal)?;
    let send_internal = lua.create_function(mailer_send_internal)?;

    // Lua-side constructor. `create` gets the userdata from the C internal,
    // then returns a table handle whose `:send` calls the send internal and
    // yields on the userdata-shaped (`OpMarker`) return.
    let chunk_src = r#"
        local create_int, send_int = ...

        local function build(mailer)
          return {
            _mailer = mailer,
            send = function(self, mail)
              local r = send_int(self._mailer, mail)
              if type(r) == "userdata" then
                r = coroutine.yield(r)
                if type(r) == "table" and r.ok == false then
                  error(r.error or "mailer error", 0)
                end
              end
              return r
            end,
          }
        end

        return {
          create = function(opts)
            local mailer = create_int(opts)
            return build(mailer)
          end,
        }
    "#;
    let namespace: mlua::Table = lua
        .load(chunk_src)
        .set_name("mailer ctor")
        .call((create_internal, send_internal))?;
    for pair in namespace.pairs::<mlua::Value, mlua::Value>() {
        let (k, v) = pair?;
        t.set(k, v)?;
    }
    Ok(t)
}
