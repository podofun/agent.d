//! In-process egress proxy for sandboxed shell children.
//!
//! The proxy is the only network path out of a sandboxed child. It reads the
//! destination host from the peeked client bytes (`host::extract_host`), checks
//! it against the policy's allowed `net:<host>` slugs with `Permission::covers`,
//! then relays or denies. No TLS termination, no MITM.

pub mod host;
