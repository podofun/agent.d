//! Fallback backend: no native sandbox on this platform.
use crate::policy::{SandboxError, SandboxPolicy};

pub fn apply(_policy: &SandboxPolicy) -> Result<(), SandboxError> {
    Err(SandboxError::Unsupported)
}

pub fn is_supported() -> bool {
    false
}
