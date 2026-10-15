//! Build-script toolkit for native C/C++ dependencies.
//!
//! A crate that wraps a native library has to answer the same questions every
//! time: where does the source come from, how do we know it is the right
//! source, where does the build go, what happens when two builds run at once,
//! and how does the result stay linkable. woodshed answers them once.
//!
//! The name is literal - a woodshed is where you keep the raw material before
//! it is any use to anyone - and idiomatic: *woodshedding* is the unglamorous
//! practice that makes a performance look effortless.
//!
//! # Blocks, not a framework
//!
//! Every piece is independently useful and independently optional, gated
//! behind its own feature. A crate that only needs checksummed downloads takes
//! [`Cache`]; one that drives its own build system takes [`Toolchain`] and
//! [`process::run`] and ignores [`CmakeBuild`]; one that only wants to find an
//! existing install takes [`probe`]. Nothing forces you up a layer.
//!
//! # The one opinion
//!
//! Expensive build trees go in a content-hashed cache directory, never
//! `OUT_DIR`, and are reused only after an explicit completeness check.
//!
//! Cargo deletes `OUT_DIR` whenever the build script changes, so building a
//! ten-minute dependency there means every one-line edit to `build.rs` costs
//! ten minutes. [`Cache::build_dir`] keys the directory on everything that
//! affects the output - target, compilers, and whatever else you
//! [`mix`](BuildKey::mix) in - so sharing happens exactly when it is correct.
//!
//! # Host and target
//!
//! A build script is compiled for the *host*, so `cfg!(unix)` and
//! `cfg!(target_env = "msvc")` inside one describe the machine running the
//! build, not the machine being built for. They agree until someone
//! cross-compiles. woodshed reads the target from Cargo's environment instead -
//! see [`target_env`], [`target_os`], and [`target_is_unix`] - so
//! [`probe`] and [`Toolchain`] make the right choice either way.
//!
//! # Example
//!
//! ```no_run
//! # fn main() -> woodshed::Result<()> {
//! use woodshed::{BuildKey, Cache, CmakeBuild, Manifest};
//!
//! let manifest = Manifest::load_default(std::env::var("CARGO_MANIFEST_DIR").unwrap())?;
//! let dep = manifest.get("geos")?;
//!
//! let cache = Cache::discover()?;
//! let source = cache.source(dep)?;
//!
//! let key = BuildKey::new("geos").dep(dep).mix(woodshed::crt_static().to_string());
//! let dirs = cache.build_dir(&key);
//!
//! // Reuse a finished build; rebuild anything less.
//! if !dirs.is_complete(["include/geos_c.h"]) {
//!     CmakeBuild::new(&source, &dirs)
//!         .define("BUILD_SHARED_LIBS", "OFF")
//!         .install()?;
//! }
//!
//! println!("cargo:rustc-link-search=native={}/lib", dirs.prefix.display());
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

mod archive;
mod cache;
mod dep;
mod error;
mod link;
pub mod probe;
mod toolchain;

#[cfg(feature = "cmake")]
mod cmake;
#[cfg(feature = "download")]
mod download;
#[cfg(feature = "pin")]
pub mod pin;
#[cfg(feature = "process")]
pub mod process;

pub use cache::{BuildDirs, BuildKey, BuildLock, Cache};
pub use dep::Dep;
pub use error::{Error, Result};
pub use link::{Linkage, copy_shared_libs, emit_link, link_cpp_runtime_for, shared_library_files};
pub use toolchain::{Toolchain, find_on_path};

#[cfg(feature = "cmake")]
pub use cmake::{CmakeBuild, MsvcRuntime};
#[cfg(feature = "manifest")]
pub use dep::Manifest;

use std::path::Path;

/// The environment of the target being built for, such as `msvc`, `gnu`, or
/// `musl`.
///
/// A build script is compiled for the *host*, so `cfg!(target_env = "msvc")`
/// inside one describes the machine running the build, not the machine being
/// built for. Those agree until someone cross-compiles, and then they do not.
/// Cargo passes the real target in the environment; read it from here.
pub fn target_env() -> String {
    std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default()
}

/// The operating system of the target being built for, such as `windows`,
/// `linux`, or `macos`.
///
/// See [`target_env`] for why this is read from the environment rather than
/// from a `cfg`.
pub fn target_os() -> String {
    std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default()
}

/// The family of the target being built for: `unix`, `windows`, or `wasm`.
///
/// See [`target_env`] for why this is read from the environment rather than
/// from a `cfg`.
pub fn target_family() -> String {
    std::env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default()
}

/// Whether the target being built for is Unix-like.
pub fn target_is_unix() -> bool {
    // Cargo separates multi-valued cfgs, such as a target that is both `unix`
    // and `wasm`, with the unit separator rather than a comma.
    target_family()
        .split(['\x1f', ','])
        .any(|family| family == "unix")
}

/// Whether the Rust target links the C runtime statically.
///
/// Native code must make the same choice, or the two halves of the program
/// disagree about which runtime they are using.
pub fn crt_static() -> bool {
    std::env::var("CARGO_CFG_TARGET_FEATURE")
        .is_ok_and(|features| features.split(',').any(|feature| feature == "crt-static"))
}

/// Emit the link directive for the C++ standard library.
///
/// A C++ library's objects reference the C++ standard library, which Rust does
/// not link by default. On MSVC the runtime is chosen by
/// `CMAKE_MSVC_RUNTIME_LIBRARY` and pulled in by the object files themselves;
/// every other platform needs it named explicitly. Static linking is used
/// where available so the executable carries no `libstdc++` dependency.
pub fn link_cpp_runtime() {
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    match (target_os.as_str(), target_env.as_str()) {
        (_, "msvc") => {}
        ("macos" | "ios", _) => println!("cargo:rustc-link-lib=c++"),
        (_, "musl") => println!("cargo:rustc-link-lib=static=stdc++"),
        _ if crt_static() => println!("cargo:rustc-link-lib=static=stdc++"),
        _ => println!("cargo:rustc-link-lib=stdc++"),
    }
}

/// The SHA-256 of a file, as lowercase hex. Streams the file.
pub fn sha256_file(path: &Path) -> Result<String> {
    use sha2::Digest;

    let mut file = std::fs::File::open(path)
        .map_err(|error| Error::io(format!("cannot open {}", path.display()), error))?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .map_err(|error| Error::io(format!("cannot read {}", path.display()), error))?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Verify a file's SHA-256 against `expected`, given as lowercase hex.
///
/// Streams the file, so an archive of any size costs a constant amount of
/// memory.
pub fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let actual = sha256_file(path)?;
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(Error::Checksum {
            path: path.to_path_buf(),
            expected: expected.to_string(),
            actual,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set target env vars, run `body`, then restore them.
    ///
    /// Build-script helpers read the target from the environment, so this is
    /// how the cross-compilation cases are reachable from a test.
    fn with_target_env<T>(vars: &[(&str, &str)], body: impl FnOnce() -> T) -> T {
        let saved: Vec<_> = vars
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        for (key, value) in vars {
            // SAFETY: tests touching the environment run single-threaded.
            unsafe { std::env::set_var(key, value) };
        }
        let result = body();
        for (key, value) in saved {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
        result
    }

    #[test]
    fn target_helpers_read_the_target_not_the_host() {
        with_target_env(
            &[
                ("CARGO_CFG_TARGET_ENV", "musl"),
                ("CARGO_CFG_TARGET_OS", "linux"),
                ("CARGO_CFG_TARGET_FAMILY", "unix"),
            ],
            || {
                assert_eq!(target_env(), "musl");
                assert_eq!(target_os(), "linux");
                assert!(target_is_unix());
            },
        );
    }

    #[test]
    fn a_windows_target_is_not_unix_even_on_a_unix_host() {
        with_target_env(
            &[
                ("CARGO_CFG_TARGET_ENV", "msvc"),
                ("CARGO_CFG_TARGET_FAMILY", "windows"),
            ],
            || {
                assert!(!target_is_unix());
                assert_eq!(target_env(), "msvc");
            },
        );
    }

    #[test]
    fn multi_family_targets_still_count_as_unix() {
        // Cargo uses the unit separator for multi-valued cfgs.
        with_target_env(&[("CARGO_CFG_TARGET_FAMILY", "unix\x1fwasm")], || {
            assert!(target_is_unix());
        });
        with_target_env(&[("CARGO_CFG_TARGET_FAMILY", "unix,wasm")], || {
            assert!(target_is_unix());
        });
    }

    #[test]
    fn crt_static_reads_the_target_feature_list() {
        with_target_env(
            &[("CARGO_CFG_TARGET_FEATURE", "sse2,crt-static,fxsr")],
            || {
                assert!(crt_static());
            },
        );
        with_target_env(&[("CARGO_CFG_TARGET_FEATURE", "sse2,fxsr")], || {
            assert!(!crt_static());
        });
        // A substring must not be mistaken for the whole feature.
        with_target_env(&[("CARGO_CFG_TARGET_FEATURE", "crt-static-ish")], || {
            assert!(!crt_static());
        });
    }

    #[test]
    fn verify_sha256_accepts_a_correct_digest_and_rejects_a_wrong_one() {
        let path = std::env::temp_dir().join(format!("woodshed-sha-{}", std::process::id()));
        std::fs::write(&path, b"woodshed").unwrap();

        // Computed by the same primitive the crate uses, so this pins the
        // wiring rather than the hash function.
        let expected = {
            use sha2::Digest;
            let digest = sha2::Sha256::digest(b"woodshed");
            digest
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };

        assert!(verify_sha256(&path, &expected).is_ok());
        assert!(verify_sha256(&path, &expected.to_uppercase()).is_ok());

        let wrong = "0".repeat(64);
        assert!(matches!(
            verify_sha256(&path, &wrong),
            Err(Error::Checksum { .. })
        ));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn verify_sha256_reports_a_missing_file() {
        let missing = std::env::temp_dir().join("woodshed-nonexistent-file");
        let _ = std::fs::remove_file(&missing);
        assert!(verify_sha256(&missing, &"0".repeat(64)).is_err());
    }
}
