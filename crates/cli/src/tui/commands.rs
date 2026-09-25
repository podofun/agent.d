use serde_json::Value;

pub(super) struct Suggestion {
    pub(super) label: String,
    pub(super) description: String,
    pub(super) completion: Option<String>,
    pub(super) execute: bool,
}

struct Command {
    name: &'static str,
    description: &'static str,
    takes_argument: bool,
}

const COMMANDS: &[Command] = &[
    Command {
        name: "help",
        description: "Show chat help",
        takes_argument: false,
    },
    Command {
        name: "new",
        description: "Start a new chat",
        takes_argument: false,
    },
    Command {
        name: "runner",
        description: "Switch runner",
        takes_argument: true,
    },
    Command {
        name: "open",
        description: "Resume a session",
        takes_argument: true,
    },
    Command {
        name: "rename",
        description: "Name the current chat",
        takes_argument: true,
    },
    Command {
        name: "call",
        description: "Run an action",
        takes_argument: true,
    },
    Command {
        name: "quit",
        description: "Exit chat",
        takes_argument: false,
    },
];

pub(super) fn matching(
    input: &str,
    runners: &[Value],
    actions: &[Value],
    sessions: &[Value],
) -> Vec<Suggestion> {
    let Some(command) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if let Some((verb, query)) = command.split_once(' ') {
        if query.chars().any(char::is_whitespace) {
            return Vec::new();
        }
        return match verb {
            "runner" => runner_suggestions(query, runners),
            "call" => action_suggestions(query, actions),
            "open" => session_suggestions(query, sessions),
            _ => Vec::new(),
        };
    }
    COMMANDS
        .iter()
        .filter(|item| item.name.starts_with(command))
        .map(|item| Suggestion {
            label: format!("/{}", item.name),
            description: item.description.into(),
            completion: Some(format!(
                "/{}{}",
                item.name,
                if item.takes_argument { " " } else { "" }
            )),
            execute: !item.takes_argument,
        })
        .collect()
}

pub(super) fn picker_title(input: &str) -> Option<&'static str> {
    if input.starts_with("/runner ") {
        Some(" Runners · type to search ")
    } else if input.starts_with("/call ") {
        Some(" Actions · type to search ")
    } else if input.starts_with("/open ") {
        Some(" Chats · type to search ")
    } else {
        None
    }
}

fn matches_query(candidate: &str, query: &str) -> bool {
    candidate.to_lowercase().contains(&query.to_lowercase())
}

fn runner_suggestions(query: &str, runners: &[Value]) -> Vec<Suggestion> {
    let suggestions = runners
        .iter()
        .filter_map(|runner| {
            let name = runner["name"].as_str()?;
            matches_query(name, query).then(|| Suggestion {
                label: name.into(),
                description: runner["model"].as_str().unwrap_or("Runner").into(),
                completion: Some(format!("/runner {name}")),
                execute: true,
            })
        })
        .collect();
    empty_hint(
        suggestions,
        runners.is_empty(),
        "No runners configured",
        "No matching runners",
    )
}

fn action_suggestions(query: &str, actions: &[Value]) -> Vec<Suggestion> {
    let suggestions = actions
        .iter()
        .filter_map(|action| {
            let name = action.as_str()?;
            matches_query(name, query).then(|| Suggestion {
                label: name.into(),
                description: "Action".into(),
                completion: Some(format!("/call {name} ")),
                execute: false,
            })
        })
        .collect();
    empty_hint(
        suggestions,
        actions.is_empty(),
        "No actions registered",
        "No matching actions",
    )
}

fn session_suggestions(query: &str, sessions: &[Value]) -> Vec<Suggestion> {
    let suggestions = sessions
        .iter()
        .filter_map(|session| {
            let id = session["id"].as_str()?;
            let label = session["label"].as_str().unwrap_or(id);
            (matches_query(id, query) || matches_query(label, query)).then(|| Suggestion {
                label: label.into(),
                description: format!(
                    "{} · {} turns",
                    session["runner"].as_str().unwrap_or("Chat"),
                    session["turn_count"].as_u64().unwrap_or(0)
                ),
                completion: Some(format!("/open {id}")),
                execute: true,
            })
        })
        .collect();
    empty_hint(
        suggestions,
        sessions.is_empty(),
        "No previous chats",
        "No matching chats",
    )
}

fn empty_hint(
    suggestions: Vec<Suggestion>,
    list_empty: bool,
    empty_label: &str,
    no_matches: &str,
) -> Vec<Suggestion> {
    if suggestions.is_empty() {
        vec![Suggestion {
            label: if list_empty { empty_label } else { no_matches }.into(),
            description: String::new(),
            completion: None,
            execute: false,
        }]
    } else {
        suggestions
    }
}

#[cfg(test)]
mod tests {
    use super::matching;

    #[test]
    fn suggests_commands_and_daemon_items() {
        let runners = vec![
            serde_json::json!({ "name": "helper", "model": "mock/test" }),
            serde_json::json!({ "name": "reviewer" }),
        ];
        let actions = vec![
            serde_json::json!("git.status"),
            serde_json::json!("notes.read"),
        ];
        let sessions = vec![
            serde_json::json!({ "id": "abc123", "label": "work", "runner": "helper", "turn_count": 4 }),
        ];
        assert_eq!(matching("/", &runners, &actions, &sessions).len(), 7);
        assert_eq!(
            matching("/ren", &runners, &actions, &sessions)[0]
                .completion
                .as_deref(),
            Some("/rename ")
        );
        assert_eq!(
            matching("/ru", &runners, &actions, &sessions)[0].label,
            "/runner"
        );
        assert_eq!(
            matching("/runner h", &runners, &actions, &sessions)[0]
                .completion
                .as_deref(),
            Some("/runner helper")
        );
        assert_eq!(
            matching("/call git", &runners, &actions, &sessions)[0]
                .completion
                .as_deref(),
            Some("/call git.status ")
        );
        assert_eq!(
            matching("/open wo", &runners, &actions, &sessions)[0]
                .completion
                .as_deref(),
            Some("/open abc123")
        );
        assert!(matching("/runner helper extra", &runners, &actions, &sessions).is_empty());
        assert!(matching("hello /ru", &runners, &actions, &sessions).is_empty());
    }

    #[test]
    fn empty_daemon_list_explains_missing_options() {
        let suggestions = matching("/runner ", &[], &[], &[]);
        assert_eq!(suggestions[0].label, "No runners configured");
        assert!(suggestions[0].completion.is_none());
    }
}
