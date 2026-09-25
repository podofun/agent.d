//! Browser rows and the query filter behind them. A row remembers the
//! section it came from so the All view can open anything it lists.

use serde_json::Value;

use super::app::{App, Tab};

pub(super) struct Row<'a> {
    pub(super) section: Tab,
    pub(super) value: &'a Value,
    pub(super) label: String,
}

const SECTIONS: [Tab; 5] = [
    Tab::Runners,
    Tab::Sessions,
    Tab::Actions,
    Tab::Services,
    Tab::Skills,
];

/// Rows of `tab` (every section for `Tab::All`) that match `query`, best
/// matches first.
pub(super) fn rows<'a>(app: &'a App, tab: Tab, query: &str) -> Vec<Row<'a>> {
    let query = query.trim().to_lowercase();
    let sections: &[Tab] = match tab {
        Tab::All => &SECTIONS,
        Tab::Chat => &[],
        other => std::slice::from_ref(&SECTIONS[other.index() - 2]),
    };
    let mut scored = Vec::new();
    for &section in sections {
        for value in app.section(section) {
            let label = label(section, value);
            let Some(rank) = rank(&query, &label, value) else {
                continue;
            };
            scored.push((
                rank,
                Row {
                    section,
                    value,
                    label,
                },
            ));
        }
    }
    scored.sort_by_key(|(rank, _)| *rank);
    scored.into_iter().map(|(_, row)| row).collect()
}

/// `Some(0)` when the query starts a word in the label or a searchable
/// field, `Some(1)` for any other hit, `None` for no match. Everything
/// matches an empty query.
fn rank(query: &str, label: &str, value: &Value) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    let mut best = None;
    for text in std::iter::once(label.to_owned()).chain(fields(value)) {
        let text = text.to_lowercase();
        for (at, _) in text.match_indices(query) {
            let word_start = at == 0
                || !text[..at]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_alphanumeric);
            if word_start {
                return Some(0);
            }
            best = Some(1);
        }
    }
    best
}

fn fields(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => vec![text.clone()],
        Value::Object(map) => ["name", "id", "label", "model"]
            .iter()
            .filter_map(|key| map.get(*key).and_then(Value::as_str))
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// The one-line label a row shows in the browser list.
pub(super) fn label(section: Tab, value: &Value) -> String {
    match section {
        Tab::Runners => format!(
            "{}  ·  {}",
            value["name"].as_str().unwrap_or("?"),
            value["model"].as_str().unwrap_or("default")
        ),
        Tab::Sessions => format!(
            "{}  ·  {} turns",
            value["label"]
                .as_str()
                .or_else(|| value["id"].as_str())
                .unwrap_or("?"),
            value["turn_count"].as_u64().unwrap_or(0)
        ),
        Tab::Actions => value.as_str().unwrap_or("?").to_owned(),
        Tab::Services => format!(
            "{}  ·  {}",
            value["name"].as_str().unwrap_or("?"),
            value["state"].as_str().unwrap_or("?")
        ),
        Tab::Skills => value["name"].as_str().unwrap_or("?").to_owned(),
        Tab::Chat | Tab::All => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn app() -> App {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.runners = vec![json!({ "name": "helper", "model": "mock/test" })];
        app.sessions = vec![json!({ "id": "abc123", "label": "work", "turn_count": 4 })];
        app.actions = vec![json!("digits"), json!("git.status")];
        app.services = vec![json!({ "name": "cron", "state": "running" })];
        app.skills = vec![json!({ "name": "reviewer" })];
        app
    }

    fn labels(rows: &[Row<'_>]) -> Vec<(Tab, String)> {
        rows.iter()
            .map(|row| (row.section, row.label.clone()))
            .collect()
    }

    #[test]
    fn empty_query_lists_every_section_in_order() {
        let app = app();
        assert_eq!(
            labels(&rows(&app, Tab::All, "")),
            [
                (Tab::Runners, "helper  ·  mock/test".into()),
                (Tab::Sessions, "work  ·  4 turns".into()),
                (Tab::Actions, "digits".into()),
                (Tab::Actions, "git.status".into()),
                (Tab::Services, "cron  ·  running".into()),
                (Tab::Skills, "reviewer".into()),
            ]
        );
    }

    #[test]
    fn matches_ignore_case_and_rank_word_starts_first() {
        let app = app();
        assert_eq!(
            labels(&rows(&app, Tab::All, "GIT")),
            [
                (Tab::Actions, "git.status".into()),
                (Tab::Actions, "digits".into()),
            ]
        );
    }

    #[test]
    fn hidden_fields_still_match() {
        let app = app();
        let found = rows(&app, Tab::All, "abc1");
        assert_eq!(labels(&found), [(Tab::Sessions, "work  ·  4 turns".into())]);
        assert_eq!(found[0].value["id"], "abc123");
    }

    #[test]
    fn a_section_tab_only_searches_that_section() {
        let app = app();
        assert_eq!(
            labels(&rows(&app, Tab::Actions, "dig")),
            [(Tab::Actions, "digits".into())]
        );
        assert_eq!(labels(&rows(&app, Tab::Sessions, "")).len(), 1);
    }
}
