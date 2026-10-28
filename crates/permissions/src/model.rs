use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Flat slug like `net:googleapis.com`. Holder-side slugs may use `*` / `**`
/// wildcards in the specifier.
///
/// A `net:` host is stored in one canonical form (lowercase, international
/// names in their ASCII form, no trailing dot, IP addresses unbracketed with
/// IPv4 never written as IPv6), so a grant, a denial, and a
/// request for the same host always compare equal however each was written.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Permission(#[serde(deserialize_with = "deserialize_slug")] pub String);

fn deserialize_slug<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    String::deserialize(d).map(|s| Permission::new(s).0)
}

impl Permission {
    pub fn new(s: impl Into<String>) -> Self {
        let s = s.into();
        match s.split_once(':') {
            Some(("net", host)) => Self(format!("net:{}", canonical_host(host))),
            _ => Self(s),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Split into `(domain, specifier)`.
    pub fn parts(&self) -> (&str, Option<&str>) {
        match self.0.split_once(':') {
            Some((d, s)) => (d, Some(s)),
            None => (self.0.as_str(), None),
        }
    }

    /// Does this holder permission satisfy the given required permission?
    /// File paths compare without regard to letter case on macOS and
    /// Windows, whose default file systems ignore case, so a grant or a
    /// denial covers the same file however its case is written.
    pub fn covers(&self, required: &Permission) -> bool {
        self.covers_with_case(required, FILE_PATHS_IGNORE_CASE)
    }

    /// [`Permission::covers`] with the file-path case rule given explicitly.
    pub fn covers_with_case(&self, required: &Permission, file_paths_ignore_case: bool) -> bool {
        let (hd, hs) = self.parts();
        let (rd, rs) = required.parts();
        if hd != rd {
            return false;
        }
        match (hs, rs) {
            (None, None) => true,
            (None, Some(_)) | (Some(_), None) => false,
            (Some(h), Some(r)) if hd.starts_with("fs.") => {
                // A grant on a folder is a plain prefix test, so a required
                // path that climbs out of it must fail closed. Every caller
                // resolves paths before asking; one that does not gets nothing.
                if !normal_path(r) {
                    return false;
                }
                if file_paths_ignore_case {
                    match_spec(&h.to_lowercase(), &r.to_lowercase())
                } else {
                    match_spec(h, r)
                }
            }
            (Some(h), Some(r)) => match_spec(h, r),
        }
    }
}

/// A file path with no `.` or `..` components, either separator.
fn normal_path(path: &str) -> bool {
    !path
        .split(['/', '\\'])
        .any(|component| component == "." || component == "..")
}

/// A path that only makes sense on Windows: a drive letter or a UNC share.
/// Only these fold `\` into `/`; on Unix a backslash is an ordinary byte in a
/// file name, and folding it would let `proj\evil` pass as `proj/evil`.
fn windows_path(spec: &str) -> bool {
    let b = spec.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') || spec.starts_with("\\\\")
}

/// Whether this platform's default file systems ignore letter case in paths
/// (APFS on macOS, NTFS on Windows). Linux file systems do not.
const FILE_PATHS_IGNORE_CASE: bool = cfg!(any(target_os = "macos", windows));

/// Lowercase, IDNA to ASCII, trailing dot removed. An IP address loses its
/// URL brackets, and an IPv4 address written as IPv6 (`::ffff:a.b.c.d`)
/// becomes plain IPv4, so an address matches however it was written. A
/// wildcard pattern is only lowercased.
fn canonical_host(host: &str) -> String {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return ip.to_canonical().to_string();
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    if !host.is_ascii()
        && !host.contains('*')
        && let Ok(url::Host::Domain(ascii)) = url::Host::parse(host)
    {
        return ascii;
    }
    host.to_ascii_lowercase()
}

fn match_spec(holder: &str, required: &str) -> bool {
    if holder == "*" || holder == "**" {
        return true;
    }
    if holder == required {
        return true;
    }
    // Path specifiers may arrive with either separator (a Windows grant writes
    // `C:\dir\**`, while a resolved slug uses native backslashes). Treat `\` and
    // `/` as equivalent so the `/`-based glob logic below matches either form.
    // No-op for specifiers without backslashes (hosts, bare binaries, ...).
    if (holder.contains('\\') || required.contains('\\'))
        && (windows_path(holder) || windows_path(required))
    {
        return match_spec(&holder.replace('\\', "/"), &required.replace('\\', "/"));
    }
    if let Some(prefix) = holder.strip_suffix("/**") {
        return required == prefix
            || required
                .strip_prefix(prefix)
                .is_some_and(|tail| tail.starts_with('/'));
    }
    if let Some(prefix) = holder.strip_suffix("/*") {
        return required
            .strip_prefix(prefix)
            .and_then(|tail| tail.strip_prefix('/'))
            .is_some_and(|name| !name.contains('/'));
    }
    if let Some(prefix) = holder.strip_suffix('*') {
        return required.starts_with(prefix);
    }
    false
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PermissionSet(pub BTreeSet<Permission>);

impl<S: Into<String>> FromIterator<S> for PermissionSet {
    fn from_iter<I: IntoIterator<Item = S>>(iter: I) -> Self {
        Self(iter.into_iter().map(Permission::new).collect())
    }
}

impl PermissionSet {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, p: Permission) {
        self.0.insert(p);
    }

    pub fn contains(&self, p: &Permission) -> bool {
        self.0.iter().any(|h| h.covers(p))
    }

    /// All required permissions are covered by at least one holder.
    pub fn covers_all(&self, required: &PermissionSet) -> bool {
        required.0.iter().all(|r| self.contains(r))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Permission> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);
        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }
        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
    };
}

string_id!(RunnerId);
string_id!(InterfaceId);
string_id!(ServiceId);
string_id!(SessionId);
string_id!(UserId);
string_id!(ExecutionId);

// ---------- Tool + action metadata ----------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolMeta {
    pub name: String,
    /// Permissions the tool package declares it WANTS (manifest). Never a grant
    /// by itself — the grants file is the only thing that confers grants.
    /// Engine uses this only for diagnostics / cross-checking.
    #[serde(default)]
    pub requires: PermissionSet,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActionMeta {
    pub name: String,
    /// Tool this action belongs to (the part before the first `.` by convention).
    #[serde(default)]
    pub tool: Option<String>,
    /// Permissions this action requires to execute.
    #[serde(default)]
    pub requires: PermissionSet,
    /// Action requires per-call human confirmation (interface decides UX).
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Caller {
    pub runner: Option<RunnerId>,
    pub interface: Option<InterfaceId>,
    pub service: Option<ServiceId>,
    pub session: Option<SessionId>,
    pub user: Option<UserId>,
    /// Correlates every trace event produced while serving one top-level
    /// request. Minted at the interface boundary and carried verbatim into
    /// every child runner run + recursive action call, so a single scan and
    /// its N agent dispatches share one id in the trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionId>,
    /// The run this call belongs to, minted when the run starts. Set by the
    /// executor only, never read from the wire.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The durable session the run belongs to. Set by the executor only,
    /// never read from the wire. (`session` is the connection id.)
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The session's working folder, when it has one. Set by the executor
    /// only, never read from the wire.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Who answers this run's questions, resolved once when the run starts.
    /// Set by the executor only, never read from the wire. `None` (no run)
    /// behaves as [`ApprovalSetting::Human`].
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalSetting>,
    /// Folders an enclosing action call is already capturing file changes
    /// in, so a call nested inside it leaves those folders to it. Set by the
    /// executor only, never read from or written to the wire.
    #[serde(skip)]
    pub capturing: Vec<std::path::PathBuf>,
    /// Whether the enclosing capture scans those folders. When it does not,
    /// a nested call that runs programs scans them itself. Set by the
    /// executor only.
    #[serde(skip)]
    pub capturing_scans: bool,
}

/// Who answers a run's questions. Ordered from strict to loose, so the
/// stricter of two settings is the smaller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalSetting {
    /// Anything not granted is refused, and nothing is asked.
    Nobody,
    /// Questions go to connected approvers. The default.
    Human,
    /// Questions go to the runner named in `[approval].classifier`, which
    /// answers allow, refuse, or unsure. Unsure goes to a human.
    Classifier,
}

impl ApprovalSetting {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalSetting::Nobody => "nobody",
            ApprovalSetting::Human => "human",
            ApprovalSetting::Classifier => "classifier",
        }
    }
}

impl std::fmt::Display for ApprovalSetting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Caller {
    pub fn interface(id: impl Into<InterfaceId>) -> Self {
        Self {
            interface: Some(id.into()),
            ..Default::default()
        }
    }
    pub fn service(id: impl Into<ServiceId>) -> Self {
        Self {
            service: Some(id.into()),
            ..Default::default()
        }
    }
    pub fn with_runner(mut self, r: impl Into<RunnerId>) -> Self {
        self.runner = Some(r.into());
        self
    }
    pub fn with_service(mut self, s: impl Into<ServiceId>) -> Self {
        self.service = Some(s.into());
        self
    }
    pub fn with_user(mut self, u: impl Into<UserId>) -> Self {
        self.user = Some(u.into());
        self
    }
    pub fn with_session(mut self, s: impl Into<SessionId>) -> Self {
        self.session = Some(s.into());
        self
    }
    pub fn with_execution(mut self, e: impl Into<ExecutionId>) -> Self {
        self.execution = Some(e.into());
        self
    }
    /// The execution id as a plain string, for trace stamping.
    pub fn execution_str(&self) -> Option<String> {
        self.execution.as_ref().map(|e| e.as_str().to_string())
    }
}

/// `[denial]`: what nobody may do or reach, whatever the grants say. A denial
/// is decided before any question, so it is never offered for approval.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Denial {
    /// Action names nobody may call.
    #[serde(default)]
    pub actions: BTreeSet<String>,
    /// Permissions nobody may use.
    #[serde(default)]
    pub permissions: PermissionSet,
}

impl Denial {
    pub fn denies_action(&self, name: &str) -> bool {
        self.actions.contains(name)
    }
    /// A denied file path also denies everything inside it, so an operator
    /// who writes `fs.read:/home/me/.ssh` (with or without a trailing slash)
    /// gets what they meant without having to know the `/**` form.
    pub fn denies_permission(&self, p: &Permission) -> bool {
        self.permissions.iter().any(|h| {
            if h.covers(p) {
                return true;
            }
            let (domain, spec) = h.parts();
            let Some(spec) = spec else { return false };
            if !domain.starts_with("fs.") || spec.ends_with('*') {
                return false;
            }
            let folder = spec.trim_end_matches(['/', '\\']);
            Permission::new(format!("{domain}:{folder}/**")).covers(p)
        })
    }
}

/// `[approval]`: how questions get answered.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    /// The runner that answers questions for runs whose approval is
    /// `classifier`. Validated where runners are known, on every load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier: Option<String>,
    /// Confirm-gated actions that are never asked about. An "allow forever"
    /// on a confirm question adds the action here.
    #[serde(default)]
    pub skip: BTreeSet<String>,
}

impl Approval {
    pub fn skips(&self, name: &str) -> bool {
        self.skip.contains(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_hosts_are_stored_in_one_canonical_form() {
        let same = |a: &str, b: &str| assert_eq!(Permission::new(a).as_str(), b, "{a}");
        same("net:Tracker.Example.COM", "net:tracker.example.com");
        same("net:tracker.example.com.", "net:tracker.example.com");
        same("net:bücher.example", "net:xn--bcher-kva.example");
        same("net:API.Example.*", "net:api.example.*");
        same("net:127.0.0.1", "net:127.0.0.1");
        same("net:*", "net:*");
        same("net", "net");
    }

    #[test]
    fn ip_addresses_are_stored_in_one_canonical_form() {
        let same = |a: &str, b: &str| assert_eq!(Permission::new(a).as_str(), b, "{a}");
        same("net:[::ffff:a9fe:a9fe]", "net:169.254.169.254");
        same("net:::ffff:169.254.169.254", "net:169.254.169.254");
        same("net:[2001:DB8:0:0::1]", "net:2001:db8::1");
        same("net:[::1]", "net:::1");
        same("net:169.254.169.254", "net:169.254.169.254");
        same("net:169.254.*", "net:169.254.*");
    }

    #[test]
    fn an_ipv4_denial_covers_the_same_address_written_as_ipv6() {
        let deny = Permission::new("net:169.254.*");
        assert!(deny.covers(&Permission::new("net:[::ffff:a9fe:a9fe]")));
        assert!(Permission::new("net:127.0.0.1").covers(&Permission::new("net:[::ffff:7f00:1]")));
    }

    #[test]
    fn file_paths_match_regardless_of_case_where_the_disk_ignores_case() {
        let deny = Permission::new("fs.read:/Proj/.env");
        let asked = Permission::new("fs.read:/proj/.ENV");
        assert!(deny.covers_with_case(&asked, true));
        assert!(!deny.covers_with_case(&asked, false));
        let grant = Permission::new("fs.write:/Proj/**");
        assert!(grant.covers_with_case(&Permission::new("fs.write:/proj/src/a.rs"), true));
    }

    #[test]
    fn only_file_paths_ignore_case() {
        assert!(
            !Permission::new("shell.exec:Git")
                .covers_with_case(&Permission::new("shell.exec:git"), true)
        );
        assert!(
            !Permission::new("secret:API").covers_with_case(&Permission::new("secret:api"), true)
        );
    }

    #[test]
    fn case_rule_follows_the_platform() {
        let deny = Permission::new("fs.read:/Proj/.env");
        let asked = Permission::new("fs.read:/proj/.ENV");
        assert_eq!(deny.covers(&asked), cfg!(any(target_os = "macos", windows)));
    }

    #[test]
    fn only_net_specifiers_are_normalized() {
        for slug in [
            "fs.read:/Proj/X.txt",
            "shell.exec:Git",
            "secret:API_KEY",
            "ai:Anthropic",
        ] {
            assert_eq!(Permission::new(slug).as_str(), slug);
        }
    }

    #[test]
    fn a_differently_written_host_is_still_covered() {
        let grant = Permission::new("net:Tracker.Example.com.");
        assert!(grant.covers(&Permission::new("net:tracker.example.com")));
        assert!(
            Permission::new("net:tracker.example.com")
                .covers(&Permission::new("net:TRACKER.example.com."))
        );
    }

    #[test]
    fn deserialized_permissions_are_normalized_too() {
        let set: PermissionSet = serde_json::from_str(r#"["net:Tracker.Example.com."]"#).unwrap();
        assert_eq!(
            set.iter().next().unwrap().as_str(),
            "net:tracker.example.com"
        );
    }

    #[test]
    fn permission_covers_exact() {
        assert!(Permission::new("calendar.read").covers(&Permission::new("calendar.read")));
        assert!(!Permission::new("calendar.read").covers(&Permission::new("calendar.write")));
    }

    #[test]
    fn permission_wildcard() {
        assert!(Permission::new("net:*").covers(&Permission::new("net:googleapis.com")));
        assert!(
            Permission::new("fs.read:/tmp/**").covers(&Permission::new("fs.read:/tmp/foo/bar"))
        );
        assert!(!Permission::new("fs.read:/tmp/**").covers(&Permission::new("fs.read:/other")));
        assert!(Permission::new("fs.read:/tmp/*").covers(&Permission::new("fs.read:/tmp/foo")));
        assert!(
            !Permission::new("fs.read:/tmp/*").covers(&Permission::new("fs.read:/tmp/foo/bar"))
        );
    }

    #[test]
    fn permission_domain_mismatch_never_covers() {
        assert!(!Permission::new("calendar.read").covers(&Permission::new("net:googleapis.com")));
    }

    #[test]
    fn permission_windows_paths_match_either_separator() {
        // A Windows grant uses backslashes; a resolved fs slug does too. The
        // `/`-based glob logic must still cover them, and `\`/`/` are equivalent.
        assert!(
            Permission::new(r"fs.read:C:\Users\me\**")
                .covers(&Permission::new(r"fs.read:C:\Users\me\proj\a.txt"))
        );
        assert!(
            Permission::new(r"fs.write:C:\data\*")
                .covers(&Permission::new(r"fs.write:C:\data\out.txt"))
        );
        // Mixed separators (grant authored with `/`, slug with `\`).
        assert!(
            Permission::new("fs.read:C:/Users/me/**")
                .covers(&Permission::new(r"fs.read:C:\Users\me\a.txt"))
        );
        // Confinement still holds: a sibling dir is not covered.
        assert!(
            !Permission::new(r"fs.read:C:\Users\me\**")
                .covers(&Permission::new(r"fs.read:C:\Users\other\a.txt"))
        );
    }

    #[test]
    fn unix_backslash_is_a_file_name_byte_not_a_separator() {
        assert!(
            !Permission::new("fs.write:/home/me/proj/**")
                .covers(&Permission::new(r"fs.write:/home/me/proj\evil"))
        );
    }

    #[test]
    fn a_path_that_climbs_out_is_never_covered() {
        let grant = Permission::new("fs.write:/work/**");
        assert!(!grant.covers(&Permission::new("fs.write:/work/../etc/passwd")));
        assert!(!grant.covers(&Permission::new("fs.write:/work/./a")));
        assert!(grant.covers(&Permission::new("fs.write:/work/a..b/..c")));
    }

    #[test]
    fn a_denied_folder_covers_what_is_inside_it() {
        let denial = Denial {
            permissions: PermissionSet::from_iter(["fs.read:/home/me/.ssh", "fs.write:/etc/"]),
            ..Default::default()
        };
        assert!(denial.denies_permission(&Permission::new("fs.read:/home/me/.ssh")));
        assert!(denial.denies_permission(&Permission::new("fs.read:/home/me/.ssh/id_rsa")));
        assert!(denial.denies_permission(&Permission::new("fs.write:/etc/passwd")));
        assert!(!denial.denies_permission(&Permission::new("fs.read:/home/me/.sshx")));
        assert!(!denial.denies_permission(&Permission::new("fs.write:/home/me/.ssh/id_rsa")));
    }

    #[test]
    fn set_covers_all() {
        let holder = PermissionSet::from_iter(["calendar.read", "calendar.write"]);
        let need = PermissionSet::from_iter(["calendar.write"]);
        assert!(holder.covers_all(&need));
        let bigger = PermissionSet::from_iter(["calendar.write", "calendar.delete"]);
        assert!(!holder.covers_all(&bigger));
    }

    #[test]
    fn empty_required_always_satisfied() {
        let holder = PermissionSet::empty();
        assert!(holder.covers_all(&PermissionSet::empty()));
    }

    #[test]
    fn shell_unrestricted_is_plain_slug() {
        // The sandbox escape hatch is an ordinary, non-wildcard grant; its
        // meaning (skip the native sandbox) lives in the shell binding, not the
        // permission engine.
        let held = Permission::new("shell.unrestricted");
        assert!(held.covers(&Permission::new("shell.unrestricted")));
        assert!(!held.covers(&Permission::new("shell.exec")));
    }
}
