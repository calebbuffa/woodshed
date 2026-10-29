//! Explicit native package providers. Never used as a silent source fallback.
use crate::{Error, project::Package};
#[cfg(feature = "pin")]
use crate::{Result, project::PackageLock};
#[cfg(feature = "pin")]
use std::path::Path;

#[cfg(feature = "pin")]
pub(crate) fn resolve(manifest: &Path, package: &Package) -> Result<PackageLock> {
    #[cfg(any(feature = "vcpkg", feature = "conan"))]
    {
        implementation::resolve(manifest, package)
    }
    #[cfg(not(any(feature = "vcpkg", feature = "conan")))]
    {
        let _ = manifest;
        Err(disabled(package))
    }
}

fn disabled(package: &Package) -> Error {
    Error::Tool {
        what: format!("{} package provider", package.provider()),
        detail: format!(
            "enable Woodshed's `{}` feature; no fallback provider was used",
            package.provider()
        ),
    }
}

#[cfg(feature = "package-providers")]
mod implementation;
#[cfg(feature = "build")]
pub(crate) use implementation::discover;
#[cfg(feature = "package-providers")]
pub use implementation::{InstallOptions, Installation, InstallationOwnership, install};
