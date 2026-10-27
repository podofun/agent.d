//! Live handles on registered items. Every `agentd.<kind>{...}` call returns
//! a `Handle`; reading a property loads the item's current definition from
//! its registry, so a handle always shows what is registered now.

use std::collections::HashMap;

use agentd_runners::RunnerRegistry;
use agentd_services::ServiceRegistry;
use agentd_skills::SkillRegistry;
use mlua::{Function, Lua, MetaMethod, UserData, UserDataMethods, Value};

use super::SharedCatalog;

/// The kinds of things Lua can register.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Runner,
    Skill,
    Service,
    Action,
    Tool,
}

impl Kind {
    pub(crate) fn noun(self) -> &'static str {
        match self {
            Kind::Runner => "runner",
            Kind::Skill => "skill",
            Kind::Service => "service",
            Kind::Action => "action",
            Kind::Tool => "tool",
        }
    }

    /// Every property a handle of this kind has. `actions` on a runner is
    /// the same list as `allowed_actions`, matching what registration accepts.
    fn properties(self) -> &'static [&'static str] {
        match self {
            Kind::Runner => &[
                "name",
                "system",
                "model",
                "skills",
                "allowed_actions",
                "actions",
                "cwd",
                "compact",
            ],
            Kind::Skill => &["name", "description", "system", "actions", "source"],
            Kind::Service => &[
                "name",
                "tool",
                "source",
                "restart",
                "backoff_ms",
                "backoff_max_ms",
                "handler",
            ],
            Kind::Action => &[
                "name", "tool", "requires", "confirm", "cwd", "input", "output", "handler",
            ],
            Kind::Tool => &["name", "requires"],
        }
    }

    fn is_list(self, property: &str) -> bool {
        matches!(
            (self, property),
            (Kind::Runner, "skills" | "allowed_actions" | "actions")
                | (Kind::Skill, "actions")
                | (Kind::Action | Kind::Tool, "requires")
        )
    }
}

/// Why a handle no longer points at a registered item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandleState {
    Replaced,
    Removed,
}

enum Slot {
    Live,
    Gone(HandleState),
}

/// Lua app data mapping handle ids to the items they point at.
#[derive(Default)]
pub(crate) struct Handles {
    next: u64,
    live: HashMap<(Kind, String), u64>,
    slots: HashMap<u64, (Kind, String, Slot)>,
}

impl Handles {
    /// A new id for a freshly registered item. Any earlier handle on the same
    /// name now reports that it was replaced.
    pub(crate) fn register(&mut self, kind: Kind, name: &str) -> u64 {
        self.next += 1;
        let id = self.next;
        if let Some(old) = self.live.insert((kind, name.to_owned()), id)
            && let Some(slot) = self.slots.get_mut(&old)
        {
            slot.2 = Slot::Gone(HandleState::Replaced);
        }
        self.slots.insert(id, (kind, name.to_owned(), Slot::Live));
        id
    }

    pub(crate) fn name_of(&self, id: u64) -> Result<String, (String, HandleState)> {
        match self.slots.get(&id) {
            Some((_, name, Slot::Live)) => Ok(name.clone()),
            Some((_, name, Slot::Gone(state))) => Err((name.clone(), *state)),
            None => Err((String::new(), HandleState::Removed)),
        }
    }

    fn live_id(&self, kind: Kind, name: &str) -> Option<u64> {
        self.live.get(&(kind, name.to_owned())).copied()
    }
}

/// A live handle on one registered item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Handle {
    pub(crate) kind: Kind,
    pub(crate) id: u64,
}

/// A live list property of a handle, such as a runner's `skills`.
#[derive(Clone, Copy)]
pub(crate) struct ListHandle {
    owner: Handle,
    property: &'static str,
}

/// Register `name` and return its handle.
pub(crate) fn register(lua: &Lua, kind: Kind, name: &str) -> mlua::Result<Handle> {
    let mut handles = lua.app_data_mut::<Handles>().ok_or_else(missing)?;
    Ok(Handle {
        kind,
        id: handles.register(kind, name),
    })
}

/// The live handle for a registered name, or `None` if nothing by that name
/// is registered.
pub(crate) fn handle_for(lua: &Lua, kind: Kind, name: &str) -> mlua::Result<Option<Handle>> {
    if definition(lua, kind, name)?.is_none() {
        return Ok(None);
    }
    let existing = lua
        .app_data_ref::<Handles>()
        .ok_or_else(missing)?
        .live_id(kind, name);
    match existing {
        Some(id) => Ok(Some(Handle { kind, id })),
        // Registered by a path that predates handles (a Rust caller); adopt it.
        None => register(lua, kind, name).map(Some),
    }
}

fn missing() -> mlua::Error {
    mlua::Error::external(
        "the handle table is not available in this Lua state, this is a bug in agentd, please report it",
    )
}

impl Handle {
    /// The item's current name, or an error saying why the handle is stale.
    pub(crate) fn name(&self, lua: &Lua) -> mlua::Result<String> {
        let handles = lua.app_data_ref::<Handles>().ok_or_else(missing)?;
        handles.name_of(self.id).map_err(|(name, state)| {
            let noun = self.kind.noun();
            mlua::Error::external(match state {
                HandleState::Replaced => {
                    format!("the {noun} `{name}` was replaced by a later registration, use the handle that registration returned")
                }
                HandleState::Removed => format!("the {noun} `{name}` was removed"),
            })
        })
    }

    fn property(&self, lua: &Lua, property: &str) -> mlua::Result<&'static str> {
        let known = self.kind.properties();
        known
            .iter()
            .copied()
            .find(|p| *p == property)
            .ok_or_else(|| {
                let name = self.name(lua).unwrap_or_default();
                mlua::Error::external(format!(
                    "the {} `{name}` has no property `{property}`",
                    self.kind.noun()
                ))
            })
    }

    fn read(&self, lua: &Lua, key: &str) -> mlua::Result<Value> {
        let property = self.property(lua, key)?;
        let name = self.name(lua)?;
        if property == "handler" {
            return handler(lua, self.kind, &name).map(Value::Function);
        }
        if self.kind.is_list(property) {
            return lua
                .create_userdata(ListHandle {
                    owner: *self,
                    property,
                })
                .map(Value::UserData);
        }
        let value = field(lua, self.kind, &name, property)?;
        super::json_to_lua_value(lua, &value, &lua.create_table()?, true)
    }
}

impl UserData for Handle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |lua, this, key: String| {
            this.read(lua, &key)
        });
        methods.add_meta_method(MetaMethod::ToString, |lua, this, ()| {
            Ok(format!("{} {}", this.kind.noun(), this.name(lua)?))
        });
        methods.add_meta_method(MetaMethod::Eq, |_, this, other: mlua::AnyUserData| {
            Ok(other.borrow::<Handle>().is_ok_and(|other| *other == *this))
        });
    }
}

impl ListHandle {
    fn items(&self, lua: &Lua) -> mlua::Result<Vec<String>> {
        let name = self.owner.name(lua)?;
        let value = field(lua, self.owner.kind, &name, self.property)?;
        Ok(value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default())
    }
}

impl UserData for ListHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |lua, this, index: i64| {
            let items = this.items(lua)?;
            Ok(usize::try_from(index - 1)
                .ok()
                .and_then(|at| items.get(at).cloned()))
        });
        methods.add_meta_method(MetaMethod::Len, |lua, this, ()| Ok(this.items(lua)?.len()));
        methods.add_meta_method(MetaMethod::ToString, |lua, this, ()| {
            Ok(format!("{{{}}}", this.items(lua)?.join(", ")))
        });
    }
}

/// One property of a registered item as JSON, `Null` when unset.
fn field(lua: &Lua, kind: Kind, name: &str, property: &str) -> mlua::Result<serde_json::Value> {
    let definition = definition(lua, kind, name)?.ok_or_else(|| {
        mlua::Error::external(format!("the {} `{name}` was removed", kind.noun()))
    })?;
    let property = match (kind, property) {
        (Kind::Runner, "actions") => "allowed_actions",
        (_, other) => other,
    };
    Ok(definition
        .get(property)
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

/// The registered definition of `name` as a JSON object, or `None`.
pub(crate) fn definition(
    lua: &Lua,
    kind: Kind,
    name: &str,
) -> mlua::Result<Option<serde_json::Value>> {
    let json = |value: serde_json::Result<serde_json::Value>| {
        value.map_err(|error| mlua::Error::external(error.to_string()))
    };
    Ok(match kind {
        Kind::Runner => registry::<RunnerRegistry>(lua)?
            .get(name)
            .map(|def| json(serde_json::to_value(def)))
            .transpose()?,
        Kind::Skill => registry::<SkillRegistry>(lua)?
            .get(name)
            .map(|def| json(serde_json::to_value(def)))
            .transpose()?,
        Kind::Service => registry::<ServiceRegistry>(lua)?
            .get(name)
            .map(|def| json(serde_json::to_value(def)))
            .transpose()?,
        Kind::Action => {
            let catalog = registry::<SharedCatalog>(lua)?;
            let guard = catalog
                .read()
                .map_err(|error| mlua::Error::external(error.to_string()))?;
            guard.action_meta.get(name).map(|meta| {
                serde_json::json!({
                    "name": name,
                    "tool": meta.tool,
                    "requires": meta.requires,
                    "confirm": meta.confirm,
                    "cwd": meta.cwd,
                    "input": meta.input_schema,
                    "output": meta.output_schema,
                })
            })
        }
        Kind::Tool => {
            let catalog = registry::<SharedCatalog>(lua)?;
            let guard = catalog
                .read()
                .map_err(|error| mlua::Error::external(error.to_string()))?;
            guard
                .tools
                .get(name)
                .map(|meta| serde_json::json!({ "name": name, "requires": meta.requires }))
        }
    })
}

fn handler(lua: &Lua, kind: Kind, name: &str) -> mlua::Result<Function> {
    let catalog = registry::<SharedCatalog>(lua)?;
    let guard = catalog
        .read()
        .map_err(|error| mlua::Error::external(error.to_string()))?;
    let key = match kind {
        Kind::Action => guard.actions.get(name),
        Kind::Service => guard.services.get(name),
        _ => None,
    }
    .ok_or_else(|| mlua::Error::external(format!("the {} `{name}` was removed", kind.noun())))?;
    lua.registry_value(key)
}

fn registry<T: Clone + 'static>(lua: &Lua) -> mlua::Result<T> {
    lua.app_data_ref::<T>().map(|r| r.clone()).ok_or_else(|| {
        mlua::Error::external(
            "a registry is not available in this Lua state, this is a bug in agentd, please report it",
        )
    })
}
