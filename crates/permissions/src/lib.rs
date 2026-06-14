//! Permission model. Default-deny. Five-layer intersection:
//!
//! ```text
//! tool-package-grants ∩ action-requires ∩ runner-allow ∩ interface-allow ∩ policy = Decision
//! ```
//!
//! Permissions are flat string slugs. A slug has shape `domain[:specifier]`:
//!
//! - `net:googleapis.com`
//! - `calendar.read`
//! - `calendar.write`
//! - `fs.read:/tmp/**`
//! - `oauth:google`
//! - `shell.exec`
//!
//! Matching: a holder slug satisfies a required slug iff the holder's pattern
//! covers the required value. Currently supported: exact match, prefix `*`
//! wildcard on the specifier (`fs.read:*` covers `fs.read:/anything`), and
//! glob `**` on path-like specifiers via simple prefix segments.

pub mod engine;
pub mod grants;
pub mod model;

pub use engine::{Decision, Engine, EngineError};
pub use grants::{Grants, GrantsFile, load_grants_file};
pub use grants::{InterfaceGrants, PackageGrants, RunnerGrants, ServiceGrants, ToolGrants};
pub use model::{
    ActionMeta, Caller, ExecutionId, InterfaceId, Permission, PermissionSet, Policy, RunnerId,
    ServiceId, SessionId, ToolMeta, UserId,
};
