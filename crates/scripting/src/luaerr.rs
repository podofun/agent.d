//! Split an mlua error string into a clean human message and a cleaned
//! traceback.
//!
//! mlua renders errors in two shapes we care about:
//! - `RuntimeError`: `runtime error: <msg>\nstack traceback:\n\t<frame>...`
//! - `CallbackError`: `<cause>\n<traceback>` — no prefix, frames may carry a
//!   leading `> ` marker, and nested callbacks can embed a second
//!   `stack traceback:` header.
//!
//! Both collapse to: message before the first `stack traceback:`, frames
//! after it minus C internals and our own coroutine wrappers.

/// Frames dropped from tracebacks: C internals, our `yieldable_wrap`
/// coroutine shim, tail-call markers, and repeated traceback headers.
fn frame_is_noise(f: &str) -> bool {
    f.starts_with("[C]:")
        || f.contains("yieldable_wrap")
        || f.contains("(...tail calls...)")
        || f.starts_with("stack traceback:")
}

/// `[string "helpers.lua"]:313: in field 'structured'` → `helpers.lua:313  in structured`.
/// Frames pointing at anonymous functions (`in function <...>`) keep only
/// `file:line`. Frames that don't parse are passed through trimmed.
fn clean_frame(frame: &str) -> Option<String> {
    let frame = frame.trim();
    if frame.is_empty() || frame_is_noise(frame) {
        return None;
    }
    // Unwrap the `[string "<name>"]` chunk-name decoration if present.
    let (file, rest) = if let Some(rest) = frame.strip_prefix("[string \"") {
        let (file, rest) = rest.split_once("\"]")?;
        (file, rest)
    } else {
        // Plain `path/file.lua:12: in ...` frame.
        let idx = frame.find(':')?;
        frame.split_at(idx)
    };
    // rest = `:313: in field 'structured'` or `:53: in function <...>`.
    let rest = rest.strip_prefix(':')?;
    let (line, tail) = match rest.split_once(':') {
        Some((line, tail)) => (line, tail.trim()),
        None => (rest, ""),
    };
    let line: u32 = line.parse().ok()?;
    let what = tail.strip_prefix("in ").unwrap_or(tail);
    // `function <[string "init.lua"]:52>` is an anonymous function — the
    // file:line already locates it, drop the noise.
    if what.is_empty() || what.starts_with("function <") || what == "?" {
        return Some(format!("{file}:{line}"));
    }
    // `field 'structured'` / `function 'foo'` / `local 'bar'` → keep the name.
    let name = what
        .split_once('\'')
        .and_then(|(_, r)| r.split('\'').next())
        .unwrap_or(what);
    Some(format!("{file}:{line}  in {name}"))
}

/// Split a raw mlua error string into `(message, cleaned traceback frames)`.
/// Strings without a traceback pass through with an empty trace.
pub fn split_lua_error(raw: &str) -> (String, Vec<String>) {
    let (head, tail) = match raw.split_once("\nstack traceback:") {
        Some((h, t)) => (h, Some(t)),
        None => (raw, None),
    };
    let message = head
        .trim()
        .strip_prefix("runtime error: ")
        .unwrap_or_else(|| head.trim())
        .trim()
        .to_string();
    let trace = tail
        .map(|t| {
            t.lines()
                .map(|l| l.trim().trim_start_matches("> ").trim())
                .filter_map(clean_frame)
                .collect()
        })
        .unwrap_or_default();
    (message, trace)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &str = "runtime error: runner `gh_agent`: no provider resolved\nstack traceback:\n\t[C]: in function 'error'\n\t[string \"yieldable_wrap\"]:8: in function <[string \"yieldable_wrap\"]:3>\n\t(...tail calls...)\n\t[string \"helpers.lua\"]:313: in field 'structured'\n\t[string \"init.lua\"]:53: in function <[string \"init.lua\"]:52>\n\t(...tail calls...)";

    #[test]
    fn splits_message_from_traceback() {
        let (msg, trace) = split_lua_error(RAW);
        assert_eq!(msg, "runner `gh_agent`: no provider resolved");
        assert_eq!(trace, vec!["helpers.lua:313  in structured", "init.lua:53"]);
    }

    #[test]
    fn no_traceback_passthrough() {
        let (msg, trace) = split_lua_error("plain failure");
        assert_eq!(msg, "plain failure");
        assert!(trace.is_empty());
    }

    #[test]
    fn strips_runtime_error_prefix_without_traceback() {
        let (msg, trace) = split_lua_error("runtime error: boom");
        assert_eq!(msg, "boom");
        assert!(trace.is_empty());
    }

    // mlua CallbackError shape: `{cause}\n<traceback>` — no "runtime error:"
    // prefix, `>`-marked frame lines, possibly a second nested
    // "stack traceback:" header.
    #[test]
    fn callback_error_shape() {
        let raw = "runner `gh_agent`: no provider resolved\nstack traceback:\n\t> [C]: in ?\n\t[string \"init.lua\"]:53: in function <[string \"init.lua\"]:52>\nstack traceback:\n\t[C]: in function 'error'";
        let (msg, trace) = split_lua_error(raw);
        assert_eq!(msg, "runner `gh_agent`: no provider resolved");
        assert_eq!(trace, vec!["init.lua:53"]);
    }

    #[test]
    fn plain_path_frame_and_named_function() {
        let raw = "boom\nstack traceback:\n\tskills/review.lua:12: in function 'review'\n\t[C]: in ?";
        let (_, trace) = split_lua_error(raw);
        assert_eq!(trace, vec!["skills/review.lua:12  in review"]);
    }
}
