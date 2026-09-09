//! Finding a native library that is already installed.
//!
//! Building from source is the reliable default, but a user who has already
//! installed the library - through their distribution, vcpkg, or by hand -
//! should be able to use it. Each discovery method is a [`Probe`]; run them in
//! whatever order suits the crate with [`first_match`], and add a new one by
//! implementing the trait rather than editing the chain.

use std::env;
use std::path::{Path, PathBuf};

/// One way to discover a usable native installation.
///
/// On success a probe emits its own `cargo:` directives - link paths, link
/// flags, metadata - before returning `true`.
pub trait Probe {
    /// Attempt discovery. Returns whether the library was found and linked.
    fn try_use(&self) -> bool;
}

/// Run probes in order and stop at the first that succeeds.
pub fn first_match<'a, I>(probes: I) -> bool
where
    I: IntoIterator<Item = &'a dyn Probe>,
{
    probes.into_iter().any(Probe::try_use)
}

/// An environment variable naming an install prefix to validate and link.
///
/// `validate` checks that the prefix actually provides what the crate needs -
/// the right headers, a new enough version, the required optional features -
/// and explains what was wrong if not. A prefix that is set but unusable falls
/// through to the next probe with a warning rather than failing the build,
/// because a stale variable in a shell profile should not be fatal.
pub struct EnvDirProbe<V, L>
where
    V: Fn(&Path) -> Result<(), String>,
    L: Fn(&Path),
{
    /// The variable to read, such as `GEOS_DIR`.
    pub env_var: &'static str,
    /// Check that the prefix satisfies the crate's requirements.
    pub validate: V,
    /// Emit the `cargo:` directives that link against the prefix.
    pub link: L,
}

impl<V, L> Probe for EnvDirProbe<V, L>
where
    V: Fn(&Path) -> Result<(), String>,
    L: Fn(&Path),
{
    fn try_use(&self) -> bool {
        println!("cargo:rerun-if-env-changed={}", self.env_var);

        let Some(dir) = env::var_os(self.env_var) else {
            return false;
        };
        let dir = PathBuf::from(dir);

        match (self.validate)(&dir) {
            Ok(()) => {
                (self.link)(&dir);
                true
            }
            Err(reason) => {
                eprintln!(
                    "woodshed: {}={} is set but unusable: {reason}\n  \
                     falling back to the next detection method",
                    self.env_var,
                    dir.display()
                );
                false
            }
        }
    }
}

/// vcpkg discovery for a single package, on MSVC targets.
///
/// `on_found` receives the install prefix, so a crate that needs more than
/// headers and libraries - a runtime data directory, for instance - can emit
/// the extra metadata itself.
#[cfg(feature = "vcpkg")]
pub struct VcpkgProbe<F = fn(&Path)>
where
    F: Fn(&Path),
{
    /// The vcpkg port name.
    pub package: &'static str,
    /// Called with the install prefix after a successful find.
    pub on_found: Option<F>,
}

#[cfg(feature = "vcpkg")]
impl VcpkgProbe {
    /// Probe for `package` with no extra handling.
    pub fn new(package: &'static str) -> Self {
        Self {
            package,
            on_found: None,
        }
    }
}

#[cfg(feature = "vcpkg")]
impl<F: Fn(&Path)> Probe for VcpkgProbe<F> {
    fn try_use(&self) -> bool {
        // vcpkg only supplies MSVC-compatible libraries, and the target is what
        // decides that -- not the host this build script happens to run on.
        if crate::target_env() != "msvc" {
            return false;
        }

        println!("cargo:rerun-if-env-changed=VCPKG_ROOT");

        let Ok(lib) = vcpkg::Config::new().find_package(self.package) else {
            return false;
        };
        let Some(include) = lib.include_paths.first() else {
            return false;
        };

        eprintln!(
            "woodshed: using vcpkg {} at {}",
            self.package,
            include.display()
        );
        if let Some(on_found) = &self.on_found {
            on_found(include.parent().unwrap_or(include));
        }
        true
    }
}

/// pkg-config discovery for a single package, on Unix targets.
#[cfg(feature = "pkg-config")]
pub struct PkgConfigProbe<F = fn(&Path)>
where
    F: Fn(&Path),
{
    /// The pkg-config module name.
    pub package: &'static str,
    /// Called with the install prefix after a successful probe.
    pub on_found: Option<F>,
}

#[cfg(feature = "pkg-config")]
impl PkgConfigProbe {
    /// Probe for `package` with no extra handling.
    pub fn new(package: &'static str) -> Self {
        Self {
            package,
            on_found: None,
        }
    }
}

#[cfg(feature = "pkg-config")]
impl<F: Fn(&Path)> Probe for PkgConfigProbe<F> {
    fn try_use(&self) -> bool {
        // pkg-config describes the target's system libraries, so a Unix host
        // cross-compiling to Windows must not consult it.
        if !crate::target_is_unix() {
            return false;
        }

        println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");

        let Ok(found) = pkg_config::Config::new().probe(self.package) else {
            return false;
        };
        eprintln!(
            "woodshed: using system {} via pkg-config: {}",
            self.package, found.version
        );

        if let Some(on_found) = &self.on_found
            && let Some(prefix) = found.link_paths.first().and_then(|dir| dir.parent())
        {
            on_found(prefix);
        }
        true
    }
}
