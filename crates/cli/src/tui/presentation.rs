use serde_json::Value;

pub(super) fn action_result(response: &Value) -> String {
    let result = response.get("result").unwrap_or(response);
    let mut body = match result {
        Value::String(text) => tool_output(text),
        _ => value(result),
    };
    if body.is_empty() {
        body = "Done".into();
    }
    if let Some(milliseconds) = response.get("duration_ms").and_then(Value::as_u64) {
        let duration = if milliseconds < 1_000 {
            format!("{milliseconds} ms")
        } else {
            format!("{:.1} s", milliseconds as f64 / 1_000.0)
        };
        body.push_str(&format!("\n\nCompleted in {duration}"));
    }
    body
}

pub(super) fn tool_output(text: &str) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(value @ (Value::Object(_) | Value::Array(_))) => self::value(&value),
        Ok(Value::String(inner)) => inner,
        _ => text.to_owned(),
    }
}

pub(super) fn tool_call(call: &Value) -> (String, String) {
    let name = call["name"].as_str().unwrap_or("Tool call").to_owned();
    let arguments = &call["arguments"];
    if arguments.is_null() || arguments.as_object().is_some_and(serde_json::Map::is_empty) {
        (name, String::new())
    } else {
        (name, value(arguments))
    }
}

pub(super) fn value(value: &Value) -> String {
    let mut lines = Vec::new();
    collect(value, 0, &mut lines);
    lines.join("\n")
}

fn collect(value: &Value, depth: usize, lines: &mut Vec<String>) {
    match value {
        Value::Null => {}
        Value::Bool(boolean) => lines.push(format!(
            "{}{}",
            indent(depth),
            if *boolean { "Yes" } else { "No" }
        )),
        Value::Number(number) => lines.push(format!("{}{number}", indent(depth))),
        Value::String(text) => {
            for line in text.lines() {
                lines.push(format!("{}{}", indent(depth), line));
            }
            if text.is_empty() {
                lines.push(String::new());
            }
        }
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::Object(_) | Value::Array(_) => {
                        lines.push(format!("{}•", indent(depth)));
                        collect(item, depth + 1, lines);
                    }
                    Value::Null => {}
                    _ => {
                        let rendered = self::value(item);
                        lines.push(format!("{}• {rendered}", indent(depth)));
                    }
                }
            }
        }
        Value::Object(fields) => {
            for (key, item) in fields {
                if item.is_null() {
                    continue;
                }
                let label = field_label(key);
                match item {
                    Value::Object(_) | Value::Array(_) => {
                        lines.push(format!("{}{}", indent(depth), label));
                        collect(item, depth + 1, lines);
                    }
                    Value::String(text) if text.contains('\n') => {
                        lines.push(format!("{}{}", indent(depth), label));
                        collect(item, depth + 1, lines);
                    }
                    _ => lines.push(format!("{}{}: {}", indent(depth), label, self::value(item))),
                }
            }
        }
    }
}

fn field_label(key: &str) -> String {
    let mut label = key.replace('_', " ");
    if let Some(first) = label.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    label
}

fn indent(depth: usize) -> String {
    "  ".repeat(depth)
}

#[cfg(test)]
mod tests {
    use super::{action_result, tool_call, tool_output, value};
    use serde_json::json;

    #[test]
    fn action_text_preserves_real_lines_without_json_escapes() {
        let response =
            json!({ "duration_ms": 2792, "result": "yes it worked\ndiff:\nOn branch main" });
        let output = action_result(&response);
        assert!(output.contains("yes it worked\ndiff:\nOn branch main"));
        assert!(output.contains("Completed in 2.8 s"));
        assert!(!output.contains("\\n"));
        assert!(!output.contains("\"result\""));
    }

    #[test]
    fn nested_values_read_like_details() {
        let output =
            value(&json!({ "name": "review", "skills": ["git", "rust"], "last_error": null }));
        assert!(output.contains("Name: review"));
        assert!(output.contains("• git"));
        assert!(!output.contains('{'));
    }

    #[test]
    fn serialized_tool_output_and_call_arguments_are_rendered() {
        assert_eq!(
            tool_output("{\"ok\":true,\"message\":\"ready\"}"),
            "Message: ready\nOk: Yes"
        );
        assert_eq!(
            tool_call(&json!({ "name": "notes.read", "arguments": { "path": "README.md" } })),
            ("notes.read".into(), "Path: README.md".into())
        );
        assert_eq!(
            tool_call(&json!({ "name": "notes.read", "arguments": {} })),
            ("notes.read".into(), String::new())
        );
        assert_eq!(tool_output("not JSON {here}"), "not JSON {here}");
        assert_eq!(
            tool_output("\"On branch main\\nnothing to commit\""),
            "On branch main\nnothing to commit"
        );
    }
}
