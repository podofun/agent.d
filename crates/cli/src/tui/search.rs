//! Browser rows and the query filter behind them. A row remembers the
//! section it came from so the All view can open anything it lists.

use serde_json::Value;

use super::app::{App, Tab};
use super::presentation;

pub(super) struct Row<'a> {
    pub(super) section: Tab,
    pub(super) value: &'a Value,
    /// What the item is called, shown first and never shortened.
    pub(super) name: String,
    /// Secondary facts such as a model or runner. Empty when there are none.
    pub(super) detail: String,
    /// A short fact shown at the right edge, such as how long ago a session
    /// was used. Empty when there is none.
    pub(super) aside: String,
}

/// How many rows of each section the All view lists; the section's own tab has the rest.
pub(super) const ALL_VIEW_LIMIT: usize = 5;

/// Browser sections in the order people reach for them, matching `Tab::ALL`.
const SECTIONS: [Tab; 5] = [
    Tab::Sessions,
    Tab::Runners,
    Tab::Skills,
    Tab::Actions,
    Tab::Services,
];

/// Rows of `tab` that match `query`, best matches first. `Tab::All` groups
/// them by section and keeps the first [`ALL_VIEW_LIMIT`] of each.
pub(super) fn rows<'a>(app: &'a App, tab: Tab, query: &str) -> Vec<Row<'a>> {
    let query = query.trim().to_lowercase();
    let sections: &[Tab] = match tab {
        Tab::All => &SECTIONS,
        Tab::Chat => &[],
        other => std::slice::from_ref(&SECTIONS[other.index() - 2]),
    };
    let mut found = Vec::new();
    for &section in sections {
        let mut scored = Vec::new();
        let mut values: Vec<&Value> = app.section(section).iter().collect();
        if section == Tab::Sessions {
            values.sort_by_key(|value| std::cmp::Reverse(value["updated_at"].as_u64()));
        }
        for value in values {
            let name = name(section, value);
            let Some(rank) = rank(&query, &name, value) else {
                continue;
            };
            scored.push((
                rank,
                Row {
                    section,
                    value,
                    detail: detail(section, value),
                    aside: aside(section, value),
                    name,
                },
            ));
        }
        scored.sort_by_key(|(rank, _)| *rank);
        let limit = if tab == Tab::All {
            ALL_VIEW_LIMIT
        } else {
            usize::MAX
        };
        found.extend(scored.into_iter().take(limit).map(|(_, row)| row));
    }
    found
}

/// `Some(0)` when the query starts a word in the label or a searchable
/// field, `Some(1)` for any other hit, `None` for no match. Everything
/// matches an empty query.
fn rank(query: &str, name: &str, value: &Value) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    let mut best = None;
    for text in std::iter::once(name.to_owned()).chain(fields(value)) {
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
        Value::Object(map) => ["name", "id", "label", "model", "runner"]
            .iter()
            .filter_map(|key| map.get(*key).and_then(Value::as_str))
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// What a row is called in the browser list.
pub(super) fn name(section: Tab, value: &Value) -> String {
    let text = match section {
        Tab::Sessions => value["label"].as_str().unwrap_or("untitled"),
        Tab::Actions => value.as_str().unwrap_or("?"),
        Tab::Runners | Tab::Services | Tab::Skills => value["name"].as_str().unwrap_or("?"),
        Tab::Chat | Tab::All => "",
    };
    text.to_owned()
}

/// The quieter facts shown after a row's name.
fn detail(section: Tab, value: &Value) -> String {
    match section {
        Tab::Sessions => value["runner"].as_str().unwrap_or("").to_owned(),
        Tab::Runners => value["model"]
            .as_str()
            .unwrap_or("default model")
            .to_owned(),
        Tab::Services => value["state"].as_str().unwrap_or("").to_owned(),
        Tab::Skills => value["description"]
            .as_str()
            .and_then(|text| text.lines().next())
            .unwrap_or("")
            .to_owned(),
        Tab::Actions | Tab::Chat | Tab::All => String::new(),
    }
}

fn aside(section: Tab, value: &Value) -> String {
    match section {
        Tab::Sessions => value["updated_at"]
            .as_u64()
            .map(presentation::ago)
            .unwrap_or_default(),
        _ => String::new(),
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
            .map(|row| (row.section, row.name.clone()))
            .collect()
    }

    #[test]
    fn empty_query_lists_every_section_in_order() {
        let app = app();
        assert_eq!(
            labels(&rows(&app, Tab::All, "")),
            [
                (Tab::Sessions, "work".into()),
                (Tab::Runners, "helper".into()),
                (Tab::Skills, "reviewer".into()),
                (Tab::Actions, "digits".into()),
                (Tab::Actions, "git.status".into()),
                (Tab::Services, "cron".into()),
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
        assert_eq!(labels(&found), [(Tab::Sessions, "work".into())]);
        assert_eq!(found[0].detail, "");
        assert_eq!(found[0].value["id"], "abc123");
    }

    #[test]
    fn sessions_list_most_recent_first() {
        let mut app = app();
        app.sessions = vec![
            json!({ "id": "a", "label": "old", "runner": "chat", "updated_at": 10 }),
            json!({ "id": "b", "label": "new", "runner": "chat", "updated_at": 20 }),
        ];
        let found = rows(&app, Tab::Sessions, "");
        assert_eq!(
            labels(&found),
            [(Tab::Sessions, "new".into()), (Tab::Sessions, "old".into())]
        );
        assert_eq!(found[1].detail, "chat");
        assert_eq!(found[1].aside, presentation::ago(10));
    }

    #[test]
    fn the_all_view_keeps_the_first_few_of_each_section() {
        let mut app = app();
        app.actions = (0..9).map(|i| json!(format!("a{i}"))).collect();
        let all = rows(&app, Tab::All, "");
        let actions = all.iter().filter(|row| row.section == Tab::Actions).count();
        assert_eq!(actions, ALL_VIEW_LIMIT);
        assert_eq!(all.last().unwrap().section, Tab::Services);
        assert_eq!(rows(&app, Tab::Actions, "").len(), 9);
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
