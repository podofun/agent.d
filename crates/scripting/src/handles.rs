//! Live handles on registered items. Every `agentd.<kind>{...}` call returns
//! a `Handle`; reading a property loads the item's current definition from
//! its registry, so a handle always shows what is registered now.

use std::collections::HashMap;

use agentd_runners::RunnerDef;
use agentd_runners::RunnerRegistry;
use agentd_services::ServiceDef;
use agentd_services::ServiceRegistry;
use agentd_skills::SkillDef;
use agentd_skills::SkillRegistry;
use mlua::{Function, Lua, LuaSerdeExt, MetaMethod, UserData, UserDataMethods, Value};
use serde::Serialize;
use serde::de::DeserializeOwned;

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

    fn retire(&mut self, kind: Kind, name: &str) {
        if let Some(id) = self.live.remove(&(kind, name.to_owned()))
            && let Some(slot) = self.slots.get_mut(&id)
        {
            slot.2 = Slot::Gone(HandleState::Removed);
        }
    }

    fn rename(&mut self, id: u64, new_name: &str) {
        if let Some(slot) = self.slots.get_mut(&id) {
            self.live.remove(&(slot.0, slot.1.clone()));
            self.live.insert((slot.0, new_name.to_owned()), id);
            slot.1 = new_name.to_owned();
        }
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

    /// The item's name for messages, even when the handle is stale.
    fn known_name(&self, lua: &Lua) -> String {
        lua.app_data_ref::<Handles>()
            .map(|handles| match handles.name_of(self.id) {
                Ok(name) | Err((name, _)) => name,
            })
            .unwrap_or_default()
    }

    fn property(&self, lua: &Lua, property: &str) -> mlua::Result<&'static str> {
        let known = self.kind.properties();
        known
            .iter()
            .copied()
            .find(|p| *p == property)
            .ok_or_else(|| {
                let name = self.known_name(lua);
                mlua::Error::external(format!(
                    "the {} `{name}` has no property `{property}`",
                    self.kind.noun()
                ))
            })
    }

    fn read(&self, lua: &Lua, key: &str) -> mlua::Result<Value> {
        if key == "remove" {
            let handle = *self;
            return lua
                .create_function(move |lua, _: mlua::MultiValue| {
                    remove(lua, handle.kind, &handle.name(lua)?)
                })
                .map(Value::Function);
        }
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

    fn write(&self, lua: &Lua, key: &str, value: Value) -> mlua::Result<()> {
        let property = self.property(lua, key)?;
        let name = self.name(lua)?;
        super::package_may_change(lua, self.kind, &name)?;
        match (self.kind, property) {
            (_, "name") => self.rename(lua, &name, value),
            (_, "handler") => set_handler(lua, self.kind, &name, value),
            (Kind::Runner, "compact") => {
                let compact = super::compact_policy(lua, value).map_err(|error| {
                    mlua::Error::external(format!(
                        "the runner `{name}` cannot take that value for `compact` ({})",
                        plain(&error)
                    ))
                })?;
                update::<RunnerDef>(lua, self.kind, &name, |def| {
                    def.compact = compact;
                    Ok(())
                })
            }
            (Kind::Action, "input" | "output") => set_schema(lua, &name, property, value),
            _ if self.kind.is_list(property) => {
                let items = list_items(lua, self.kind, &name, property, value)?;
                self.set_list(lua, &name, property, items)
            }
            _ => {
                let json: serde_json::Value = lua.from_value(value).map_err(|_| {
                    mlua::Error::external(format!(
                        "the {} `{name}` cannot take that value for `{property}`",
                        self.kind.noun()
                    ))
                })?;
                set_field(lua, self.kind, &name, property, json)
            }
        }
    }

    fn set_list(
        &self,
        lua: &Lua,
        name: &str,
        property: &str,
        items: Vec<String>,
    ) -> mlua::Result<()> {
        let items = match (self.kind, property) {
            // Inside a package, unqualified action names are scoped the way
            // registration scopes them.
            (Kind::Runner, "allowed_actions" | "actions") => {
                let package = super::active_package(lua);
                items
                    .iter()
                    .map(|item| super::scoped(package.as_deref(), item))
                    .collect()
            }
            _ => items,
        };
        set_field(lua, self.kind, name, property, serde_json::json!(items))
    }

    fn rename(&self, lua: &Lua, old: &str, value: Value) -> mlua::Result<()> {
        let noun = self.kind.noun();
        let new = match value {
            Value::String(text) if !text.to_str()?.is_empty() => text.to_str()?.to_owned(),
            _ => {
                return Err(mlua::Error::external(format!(
                    "the {noun} `{old}` needs a non-empty string as its name"
                )));
            }
        };
        let new = super::scoped(super::active_package(lua).as_deref(), &new);
        if new == old {
            return Ok(());
        }
        if definition(lua, self.kind, &new)?.is_some() {
            return Err(mlua::Error::external(format!(
                "the {noun} `{old}` cannot take the name `{new}` because a {noun} by that name already exists"
            )));
        }
        refuse_if_used(lua, self.kind, old, "renamed")?;
        move_item(lua, self.kind, old, &new)?;
        super::package_moved(lua, self.kind, old, Some(&new));
        lua.app_data_mut::<Handles>()
            .ok_or_else(missing)?
            .rename(self.id, &new);
        Ok(())
    }
}

/// Every registered name of `kind`, sorted.
fn names(lua: &Lua, kind: Kind) -> mlua::Result<Vec<String>> {
    let mut names = match kind {
        Kind::Runner => registry::<agentd_runners::RunnerRegistry>(lua)?.names(),
        Kind::Skill => registry::<agentd_skills::SkillRegistry>(lua)?.names(),
        Kind::Service => registry::<agentd_services::ServiceRegistry>(lua)?.names(),
        Kind::Action => {
            let catalog = registry::<SharedCatalog>(lua)?;
            let guard = catalog.read().map_err(lock)?;
            guard.action_meta.keys().cloned().collect()
        }
        Kind::Tool => {
            let catalog = registry::<SharedCatalog>(lua)?;
            let guard = catalog.read().map_err(lock)?;
            guard.tools.keys().cloned().collect()
        }
    };
    names.sort();
    Ok(names)
}

/// `agentd.<collection>.find(query)`: handles whose item matches `query`,
/// sorted by name. A function query keeps the handles it returns truthy for;
/// a table query keeps items where every key matches (see [`matches`]).
pub(crate) fn find(lua: &Lua, kind: Kind, query: Value) -> mlua::Result<Vec<Handle>> {
    let fields = match &query {
        Value::Function(_) => None,
        Value::Table(table) => {
            let mut fields = Vec::new();
            for pair in table.pairs::<String, Value>() {
                let (key, value) = pair?;
                if !kind.properties().contains(&key.as_str()) || key == "handler" {
                    return Err(mlua::Error::external(format!(
                        "a {} has no property `{key}` to search by",
                        kind.noun()
                    )));
                }
                fields.push((key, value));
            }
            Some(fields)
        }
        other => {
            return Err(mlua::Error::external(format!(
                "`find` takes a table of fields or a function, but got a {} value",
                other.type_name()
            )));
        }
    };
    let mut found = Vec::new();
    for name in names(lua, kind)? {
        let Some(handle) = handle_for(lua, kind, &name)? else {
            continue;
        };
        let keep = match (&query, &fields) {
            (Value::Function(predicate), _) => !matches!(
                predicate.call::<Value>(handle)?,
                Value::Nil | Value::Boolean(false)
            ),
            (_, Some(fields)) => {
                let mut all = true;
                for (key, wanted) in fields {
                    let actual = field(lua, kind, &name, key)?;
                    if !matches(lua, &actual, wanted)? {
                        all = false;
                        break;
                    }
                }
                all
            }
            _ => false,
        };
        if keep {
            found.push(handle);
        }
    }
    Ok(found)
}

/// Whether a property value matches a query value: strings match with `*`
/// wildcards, a string matches a list when any item matches, and anything
/// else matches by equality.
fn matches(lua: &Lua, actual: &serde_json::Value, wanted: &Value) -> mlua::Result<bool> {
    if let Value::String(pattern) = wanted {
        let pattern = pattern.to_str()?;
        return Ok(match actual {
            serde_json::Value::String(text) => wildcard(&pattern, text),
            serde_json::Value::Array(items) => items
                .iter()
                .any(|item| item.as_str().is_some_and(|text| wildcard(&pattern, text))),
            _ => false,
        });
    }
    let wanted: serde_json::Value = lua.from_value(wanted.clone())?;
    Ok(*actual == wanted)
}

/// `*` matches any run of characters; everything else matches itself.
fn wildcard(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first) || !text[first.len()..].ends_with(last) {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    true
}

/// `agentd.<collection>.remove(name)` and `handle:remove()`.
pub(crate) fn remove(lua: &Lua, kind: Kind, name: &str) -> mlua::Result<()> {
    if definition(lua, kind, name)?.is_none() {
        return Err(mlua::Error::external(format!(
            "there is no {} named `{name}`",
            kind.noun()
        )));
    }
    super::package_may_change(lua, kind, name)?;
    refuse_if_used(lua, kind, name, "removed")?;
    let mut stale = Vec::new();
    match kind {
        Kind::Runner => {
            registry::<agentd_runners::RunnerRegistry>(lua)?.remove(name);
        }
        Kind::Skill => {
            registry::<agentd_skills::SkillRegistry>(lua)?.remove(name);
        }
        Kind::Service => {
            registry::<agentd_services::ServiceRegistry>(lua)?.remove(name);
            let catalog = registry::<SharedCatalog>(lua)?;
            stale.extend(catalog.write().map_err(lock)?.services.remove(name));
        }
        Kind::Action => {
            let catalog = registry::<SharedCatalog>(lua)?;
            let mut guard = catalog.write().map_err(lock)?;
            guard.action_meta.remove(name);
            stale.extend(guard.actions.remove(name));
        }
        Kind::Tool => {
            let catalog = registry::<SharedCatalog>(lua)?;
            catalog.write().map_err(lock)?.tools.remove(name);
        }
    }
    for key in stale {
        lua.remove_registry_value(key)?;
    }
    lua.app_data_mut::<Handles>()
        .ok_or_else(missing)?
        .retire(kind, name);
    super::package_moved(lua, kind, name, None);
    Ok(())
}

/// Fail when other registered items still refer to `name`, naming them.
pub(crate) fn refuse_if_used(lua: &Lua, kind: Kind, name: &str, verb: &str) -> mlua::Result<()> {
    let users = users_of(lua, kind, name)?;
    if users.is_empty() {
        return Ok(());
    }
    let them = if users.len() == 1 { "it" } else { "them" };
    Err(mlua::Error::external(format!(
        "the {} `{name}` cannot be {verb} while {}, update or remove {them} first",
        kind.noun(),
        users.join(" and ")
    )))
}

/// What still refers to `name`, as clauses such as "the runner `coder` still lists `x`".
fn users_of(lua: &Lua, kind: Kind, name: &str) -> mlua::Result<Vec<String>> {
    let mut users = Vec::new();
    match kind {
        Kind::Action => {
            for runner in registry::<agentd_runners::RunnerRegistry>(lua)?.list() {
                if runner.allowed_actions.iter().any(|action| action == name) {
                    users.push(format!("the runner `{}` still lists `{name}`", runner.name));
                }
            }
            for skill in registry::<agentd_skills::SkillRegistry>(lua)?.list() {
                if skill.actions.iter().any(|action| action == name) {
                    users.push(format!("the skill `{}` still lists `{name}`", skill.name));
                }
            }
        }
        Kind::Skill => {
            for runner in registry::<agentd_runners::RunnerRegistry>(lua)?.list() {
                if runner.skills.iter().any(|skill| skill == name) {
                    users.push(format!("the runner `{}` still lists `{name}`", runner.name));
                }
            }
        }
        Kind::Tool => {
            let catalog = registry::<SharedCatalog>(lua)?;
            let guard = catalog.read().map_err(lock)?;
            let mut actions: Vec<&String> = guard
                .action_meta
                .iter()
                .filter(|(_, meta)| meta.tool.as_deref() == Some(name))
                .map(|(action, _)| action)
                .collect();
            actions.sort();
            for action in actions {
                users.push(format!("the action `{action}` still uses `{name}`"));
            }
        }
        Kind::Runner | Kind::Service => {}
    }
    Ok(users)
}

/// Re-register the item under a new name, keeping everything else.
fn move_item(lua: &Lua, kind: Kind, old: &str, new: &str) -> mlua::Result<()> {
    match kind {
        Kind::Runner => {
            let runners = registry::<agentd_runners::RunnerRegistry>(lua)?;
            if let Some(mut def) = runners.remove(old) {
                def.name = new.to_owned();
                runners.insert(def);
            }
        }
        Kind::Skill => {
            let skills = registry::<agentd_skills::SkillRegistry>(lua)?;
            if let Some(mut def) = skills.remove(old) {
                def.name = new.to_owned();
                skills.insert(def);
            }
        }
        Kind::Service => {
            let services = registry::<agentd_services::ServiceRegistry>(lua)?;
            if let Some(mut def) = services.remove(old) {
                def.name = new.to_owned();
                services.insert(def);
            }
            let catalog = registry::<SharedCatalog>(lua)?;
            let mut guard = catalog.write().map_err(lock)?;
            if let Some(key) = guard.services.remove(old) {
                guard.services.insert(new.to_owned(), key);
            }
        }
        Kind::Action => {
            let catalog = registry::<SharedCatalog>(lua)?;
            let mut guard = catalog.write().map_err(lock)?;
            if let Some(key) = guard.actions.remove(old) {
                guard.actions.insert(new.to_owned(), key);
            }
            if let Some(meta) = guard.action_meta.remove(old) {
                guard.action_meta.insert(new.to_owned(), meta);
            }
        }
        Kind::Tool => {
            let catalog = registry::<SharedCatalog>(lua)?;
            let mut guard = catalog.write().map_err(lock)?;
            if let Some(meta) = guard.tools.remove(old) {
                guard.tools.insert(new.to_owned(), meta);
            }
        }
    }
    Ok(())
}

/// Apply `change` to a copy of a registry definition and store it.
fn update<D: Clone + Serialize + DeserializeOwned + Named>(
    lua: &Lua,
    kind: Kind,
    name: &str,
    change: impl FnOnce(&mut D) -> mlua::Result<()>,
) -> mlua::Result<()> {
    let mut def = D::get(lua, name)?.ok_or_else(|| removed(kind, name))?;
    change(&mut def)?;
    D::put(lua, def)
}

/// Registry access shared by the definition types handles can change.
trait Named: Sized {
    fn get(lua: &Lua, name: &str) -> mlua::Result<Option<Self>>;
    fn put(lua: &Lua, def: Self) -> mlua::Result<()>;
}

impl Named for RunnerDef {
    fn get(lua: &Lua, name: &str) -> mlua::Result<Option<Self>> {
        Ok(registry::<agentd_runners::RunnerRegistry>(lua)?.get(name))
    }
    fn put(lua: &Lua, def: Self) -> mlua::Result<()> {
        registry::<agentd_runners::RunnerRegistry>(lua)?.insert(def);
        Ok(())
    }
}

impl Named for SkillDef {
    fn get(lua: &Lua, name: &str) -> mlua::Result<Option<Self>> {
        Ok(registry::<agentd_skills::SkillRegistry>(lua)?.get(name))
    }
    fn put(lua: &Lua, def: Self) -> mlua::Result<()> {
        registry::<agentd_skills::SkillRegistry>(lua)?.insert(def);
        Ok(())
    }
}

impl Named for ServiceDef {
    fn get(lua: &Lua, name: &str) -> mlua::Result<Option<Self>> {
        Ok(registry::<agentd_services::ServiceRegistry>(lua)?.get(name))
    }
    fn put(lua: &Lua, def: Self) -> mlua::Result<()> {
        registry::<agentd_services::ServiceRegistry>(lua)?.insert(def);
        Ok(())
    }
}

/// Replace one serde field of a definition, checking the new value the way
/// deserializing a definition does.
fn replace<D: Serialize + DeserializeOwned>(
    kind: Kind,
    name: &str,
    def: &mut D,
    property: &str,
    value: serde_json::Value,
) -> mlua::Result<()> {
    let mut object = serde_json::to_value(&*def).map_err(external)?;
    if let Some(fields) = object.as_object_mut() {
        fields.insert(property.to_owned(), value);
    }
    *def = serde_json::from_value(object).map_err(|error| {
        mlua::Error::external(format!(
            "the {} `{name}` cannot take that value for `{property}` ({error})",
            kind.noun()
        ))
    })?;
    Ok(())
}

fn set_field(
    lua: &Lua,
    kind: Kind,
    name: &str,
    property: &str,
    value: serde_json::Value,
) -> mlua::Result<()> {
    let property = match (kind, property) {
        (Kind::Runner, "actions") => "allowed_actions",
        (_, other) => other,
    };
    match kind {
        Kind::Runner => update::<RunnerDef>(lua, kind, name, |def| {
            replace(kind, name, def, property, value)
        }),
        Kind::Skill => update::<SkillDef>(lua, kind, name, |def| {
            // Registration trims the prompt and treats a missing one as empty.
            let value = match (property, value) {
                ("system", serde_json::Value::Null) => serde_json::json!(""),
                ("system", serde_json::Value::String(text)) => serde_json::json!(text.trim()),
                (_, value) => value,
            };
            replace(kind, name, def, property, value)
        }),
        Kind::Service => update::<ServiceDef>(lua, kind, name, |def| {
            replace(kind, name, def, property, value)?;
            match def.restart.as_deref() {
                None | Some("always" | "on_failure" | "never") => Ok(()),
                Some(other) => Err(mlua::Error::external(format!(
                    "the service `{name}` cannot use the restart policy `{other}`, use `always`, `on_failure` or `never`"
                ))),
            }
        }),
        Kind::Action => {
            let catalog = registry::<SharedCatalog>(lua)?;
            let mut guard = catalog.write().map_err(lock)?;
            let meta = guard
                .action_meta
                .get_mut(name)
                .ok_or_else(|| removed(kind, name))?;
            let invalid = |error: serde_json::Error| {
                mlua::Error::external(format!(
                    "the action `{name}` cannot take that value for `{property}` ({error})"
                ))
            };
            match property {
                // Registration infers a missing tool from the name, and scopes
                // a package's unqualified tool under the package.
                "tool" => {
                    let tool: Option<String> = serde_json::from_value(value).map_err(invalid)?;
                    let package = super::active_package(lua);
                    meta.tool = match tool {
                        Some(tool) => Some(super::scoped(package.as_deref(), &tool)),
                        None => name.split_once('.').map(|(tool, _)| tool.to_owned()),
                    };
                }
                "confirm" if value.is_null() => meta.confirm = false,
                "requires" => meta.requires = serde_json::from_value(value).map_err(invalid)?,
                "confirm" => meta.confirm = serde_json::from_value(value).map_err(invalid)?,
                "cwd" => meta.cwd = serde_json::from_value(value).map_err(invalid)?,
                _ => unreachable!("every action property is handled"),
            }
            Ok(())
        }
        Kind::Tool => {
            let catalog = registry::<SharedCatalog>(lua)?;
            let mut guard = catalog.write().map_err(lock)?;
            let meta = guard
                .tools
                .get_mut(name)
                .ok_or_else(|| removed(kind, name))?;
            meta.requires = serde_json::from_value(value).map_err(|error| {
                mlua::Error::external(format!(
                    "the tool `{name}` cannot take that value for `requires` ({error})"
                ))
            })?;
            Ok(())
        }
    }
}

fn set_handler(lua: &Lua, kind: Kind, name: &str, value: Value) -> mlua::Result<()> {
    let Value::Function(function) = value else {
        return Err(mlua::Error::external(format!(
            "the {} `{name}` needs a function as its handler",
            kind.noun()
        )));
    };
    let key = lua.create_registry_value(function)?;
    let catalog = registry::<SharedCatalog>(lua)?;
    let mut guard = catalog.write().map_err(lock)?;
    let slot = match kind {
        Kind::Action => guard.actions.get_mut(name),
        _ => guard.services.get_mut(name),
    }
    .ok_or_else(|| removed(kind, name))?;
    let old = std::mem::replace(slot, key);
    drop(guard);
    lua.remove_registry_value(old)
}

fn set_schema(lua: &Lua, name: &str, property: &str, value: Value) -> mlua::Result<()> {
    let schema = match value {
        Value::Nil => None,
        Value::Table(props) => Some(super::compile_object_schema(&props, property == "input")?),
        other => {
            return Err(mlua::Error::external(format!(
                "the action `{name}` needs a table of fields for `{property}`, but got a {} value",
                other.type_name()
            )));
        }
    };
    let catalog = registry::<SharedCatalog>(lua)?;
    let mut guard = catalog.write().map_err(lock)?;
    let meta = guard
        .action_meta
        .get_mut(name)
        .ok_or_else(|| removed(Kind::Action, name))?;
    if property == "input" {
        meta.input_schema = schema;
    } else {
        meta.output_schema = schema;
    }
    Ok(())
}

/// The strings of a value assigned to a list property: a Lua list, another
/// live list, or `nil` for an empty list.
fn list_items(
    lua: &Lua,
    kind: Kind,
    name: &str,
    property: &str,
    value: Value,
) -> mlua::Result<Vec<String>> {
    let not_strings = || {
        mlua::Error::external(format!(
            "the {} `{name}` needs a list of strings for `{property}`",
            kind.noun()
        ))
    };
    match value {
        Value::Nil => Ok(Vec::new()),
        Value::Table(table) => table
            .sequence_values::<Value>()
            .map(|item| match item? {
                Value::String(text) => Ok(text.to_str()?.to_owned()),
                _ => Err(not_strings()),
            })
            .collect(),
        Value::UserData(data) => data
            .borrow::<ListHandle>()
            .map_err(|_| not_strings())
            .and_then(|list| list.items(lua)),
        _ => Err(not_strings()),
    }
}

fn removed(kind: Kind, name: &str) -> mlua::Error {
    mlua::Error::external(format!("the {} `{name}` was removed", kind.noun()))
}

/// A Lua error's own message, without mlua's runtime or traceback framing.
fn plain(error: &mlua::Error) -> String {
    match error {
        mlua::Error::ExternalError(inner) => inner.to_string(),
        mlua::Error::CallbackError { cause, .. } => plain(cause),
        other => other.to_string(),
    }
}

fn lock<T>(error: std::sync::PoisonError<T>) -> mlua::Error {
    mlua::Error::external(error.to_string())
}

fn external(error: serde_json::Error) -> mlua::Error {
    mlua::Error::external(error.to_string())
}

impl UserData for Handle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |lua, this, key: String| {
            this.read(lua, &key)
        });
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |lua, this, (key, value): (String, Value)| this.write(lua, &key, value),
        );
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

impl ListHandle {
    /// `list[index] = value`: set an item, append at `#list + 1`, or with
    /// `nil` remove the item and shift the rest down, as `table.remove` does.
    fn set(&self, lua: &Lua, index: i64, value: Value) -> mlua::Result<()> {
        let name = self.owner.name(lua)?;
        super::package_may_change(lua, self.owner.kind, &name)?;
        let mut items = self.items(lua)?;
        let len = items.len() as i64;
        let noun = self.owner.kind.noun();
        let property = self.property;
        if !(1..=len + 1).contains(&index) {
            return Err(mlua::Error::external(format!(
                "the {noun} `{name}` list `{property}` has no index {index}, it has {len} items"
            )));
        }
        let at = (index - 1) as usize;
        match value {
            Value::Nil if index <= len => {
                items.remove(at);
            }
            Value::Nil => return Ok(()),
            Value::String(text) => {
                let text = text.to_str()?.to_owned();
                if index == len + 1 {
                    items.push(text);
                } else {
                    items[at] = text;
                }
            }
            other => {
                return Err(mlua::Error::external(format!(
                    "the {noun} `{name}` list `{property}` holds strings, but got a {} value",
                    other.type_name()
                )));
            }
        }
        self.owner.set_list(lua, &name, property, items)
    }
}

impl ListHandle {
    /// A list key as a 1-based index, or a plain error for anything else.
    fn index(&self, lua: &Lua, key: &Value) -> mlua::Result<i64> {
        match key {
            Value::Integer(index) => Ok(*index),
            Value::Number(number) if number.fract() == 0.0 => Ok(*number as i64),
            _ => Err(mlua::Error::external(format!(
                "the {} `{}` list `{}` is indexed by number",
                self.owner.kind.noun(),
                self.owner.known_name(lua),
                self.property
            ))),
        }
    }
}

impl UserData for ListHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |lua, this, key: Value| {
            let index = this.index(lua, &key)?;
            let items = this.items(lua)?;
            Ok(usize::try_from(index - 1)
                .ok()
                .and_then(|at| items.get(at).cloned()))
        });
        methods.add_meta_method(MetaMethod::Len, |lua, this, ()| Ok(this.items(lua)?.len()));
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |lua, this, (key, value): (Value, Value)| {
                let index = this.index(lua, &key)?;
                this.set(lua, index, value)
            },
        );
        // `pairs` walks a snapshot of the items, like `ipairs`.
        methods.add_meta_method(MetaMethod::Pairs, |lua, this, ()| {
            let items = this.items(lua)?;
            let next = lua.create_function(move |_, (_, at): (Value, Option<i64>)| {
                let at = at.unwrap_or(0) + 1;
                Ok(match items.get((at - 1) as usize) {
                    Some(item) => (Some(at), Some(item.clone())),
                    None => (None, None),
                })
            })?;
            Ok((next, Value::Nil, Value::Nil))
        });
        methods.add_meta_method(MetaMethod::ToString, |lua, this, ()| {
            Ok(format!("{{{}}}", this.items(lua)?.join(", ")))
        });
    }
}

/// One property of a registered item as JSON, `Null` when unset.
fn field(lua: &Lua, kind: Kind, name: &str, property: &str) -> mlua::Result<serde_json::Value> {
    let definition = definition(lua, kind, name)?.ok_or_else(|| removed(kind, name))?;
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

#[cfg(test)]
mod tests {
    use super::wildcard;

    #[test]
    fn wildcard_cases() {
        assert!(wildcard("git.*", "git.status"));
        assert!(wildcard("*", ""));
        assert!(wildcard("a*c", "abc"));
        assert!(wildcard("a*b*c", "a-b-c"));
        assert!(wildcard("exact", "exact"));
        assert!(!wildcard("git.*", "fs.read"));
        assert!(!wildcard("a*c", "ab"));
        assert!(!wildcard("a*a", "a"));
        assert!(!wildcard("exact", "exactly"));
    }
}
