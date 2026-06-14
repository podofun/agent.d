//! Package distribution: manifest, install index, git fetch, grant desugaring.

mod expand;
mod index;
mod install;
mod manifest;

// Uncommented as each module lands.
pub use expand::{LoadedPackage, expand_grants, expand_tilde};
pub use index::{IndexEntry, PackageIndex};
pub use install::{InstallError, install, update, update_check};
pub use manifest::{Manifest, ManifestError};
