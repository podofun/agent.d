//! Lua sandboxing. Strip everything that escapes the agentd boundary.
//!
//! Lua user code only sees what we expose. No `io`, `os`, `debug`, `package`,
//! `require`, `dofile`, `loadfile`, `load`, `loadstring`, or `collectgarbage`.
//! The allowed surface is: `string`, `table`, `math`, `coroutine`, `utf8`,
//! plus the safe basics (`pairs`, `ipairs`, `next`, `select`, `type`,
//! `tostring`, `tonumber`, `error`, `pcall`, `xpcall`, `assert`, `rawequal`).
//!
//! Everything else is wiped from the globals table before user code runs.

use anyhow::Result;
use mlua::{Lua, Value};

/// Names that survive lockdown. Anything else in `_G` is set to `nil`.
const ALLOWED: &[&str] = &[
    // safe core
    "_VERSION",
    "pairs",
    "ipairs",
    "next",
    "select",
    "type",
    "tostring",
    "tonumber",
    "error",
    "pcall",
    "xpcall",
    "assert",
    "rawequal",
    // safe libs
    "string",
    "table",
    "math",
    "coroutine",
    "utf8",
    // agentd surface (installed elsewhere)
    "agentd",
    // bare globals from agentd-scripting
    "async",
    "await",
    "channel",
    "sleep",
    "json",
    "import",
    "timer",
    // fan-out/join helpers from helpers.lua
    "parallel",
    "parallel_map",
];

/// Names we want to ensure are removed even if the allow-list spec changes.
/// Explicit denial is helpful for security audits.
pub const FORBIDDEN: &[&str] = &[
    "io",
    "os",
    "package",
    "debug",
    "require",
    "dofile",
    "loadfile",
    "load",
    "loadstring",
    "collectgarbage",
    "module",
    "rawget",
    "rawset",
    "rawlen",
    "getmetatable",
    "setmetatable",
    "newproxy",
];

/// Strip the global table down to the allowed surface. Idempotent.
pub fn lock_down(lua: &Lua) -> Result<()> {
    let globals = lua.globals();

    // explicit forbidden names - nil them even if absent.
    for name in FORBIDDEN {
        globals.set(*name, Value::Nil)?;
    }

    // enumerate everything in _G; nil anything not whitelisted.
    let keys: Vec<String> = globals
        .pairs::<String, Value>()
        .filter_map(|p| p.ok().map(|(k, _)| k))
        .collect();
    for key in keys {
        if !ALLOWED.contains(&key.as_str()) {
            globals.set(key, Value::Nil)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_globals_are_removed() {
        let lua = Lua::new();
        lock_down(&lua).unwrap();
        for name in FORBIDDEN {
            let v: Value = lua.globals().get(*name).unwrap();
            assert!(matches!(v, Value::Nil), "`{name}` should be nil");
        }
    }

    #[test]
    fn allowed_globals_survive() {
        let lua = Lua::new();
        lock_down(&lua).unwrap();
        for name in &["string", "table", "math", "pairs", "type", "pcall"] {
            let v: Value = lua.globals().get(*name).unwrap();
            assert!(!matches!(v, Value::Nil), "`{name}` was wiped");
        }
    }

    #[test]
    fn user_code_cannot_use_io() {
        let lua = Lua::new();
        lock_down(&lua).unwrap();
        let err = lua.load("io.open('/etc/passwd', 'r')").exec().unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("nil") || msg.contains("io"), "got {msg}");
    }

    #[test]
    fn user_code_cannot_use_require() {
        let lua = Lua::new();
        lock_down(&lua).unwrap();
        let err = lua.load("require('os')").exec().unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("nil") || msg.contains("require"), "got {msg}");
    }

    #[test]
    fn safe_code_still_runs() {
        let lua = Lua::new();
        lock_down(&lua).unwrap();
        let n: i64 = lua.load("return 1 + 2").eval().unwrap();
        assert_eq!(n, 3);
        let s: String = lua.load("return string.upper('hi')").eval().unwrap();
        assert_eq!(s, "HI");
    }

    #[test]
    fn idempotent() {
        let lua = Lua::new();
        lock_down(&lua).unwrap();
        lock_down(&lua).unwrap();
        let v: Value = lua.globals().get("io").unwrap();
        assert!(matches!(v, Value::Nil));
    }
}
