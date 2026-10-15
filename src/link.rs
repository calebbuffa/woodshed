//! Static vs. dynamic linking of a native library, decided by the caller.
//!
//! woodshed has no opinion on which is right - that depends on the library's
//! licence (an LGPL library, for example, is usually shipped as a shared
//! library) and on how the final program is distributed. What it does is make
//! each choice hard to get wrong: the choice is part of the [`BuildKey`], so a
//! static and a shared build can never share a cache directory, and the link
//! directives, runtime handling and shipped files all follow from one value.
//!
//! [`BuildKey`]: crate::BuildKey

use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// How a native library is linked into the final program.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Linkage {
    /// Linked into the executable. Self-contained, but the library's licence
    /// terms then apply to the whole program.
    Static,
    /// A shared library (`.dll`, `.so`, `.dylib`) shipped next to the program.
    Dynamic,
}

impl Linkage {
    /// Stable lowercase name, used in cache keys and messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Dynamic => "dynamic",
        }
    }

    /// Whether this is [`Linkage::Dynamic`].
    pub fn is_dynamic(self) -> bool {
        self == Self::Dynamic
    }

    /// The value for CMake's `BUILD_SHARED_LIBS`.
    pub fn build_shared_libs(self) -> &'static str {
        if self.is_dynamic() { "ON" } else { "OFF" }
    }
}

/// Emit link directives for a built prefix.
///
/// `libs` are library names without prefix or extension, in dependency order
/// (a library before the libraries it needs), which matters for static
/// linking. Also exports Cargo metadata for dependents' build scripts:
/// `includedir` always, and for [`Linkage::Dynamic`] the `libdir` and `bindir`
/// that hold the shared libraries (read as `DEP_<LINKS>_LIBDIR` / `_BINDIR`),
/// so a packaging step can ship them with [`copy_shared_libs`].
pub fn emit_link(prefix: &Path, libs: &[&str], linkage: Linkage) {
    for dir in ["lib", "lib64"] {
        let dir = prefix.join(dir);
        if dir.is_dir() {
            println!("cargo:rustc-link-search=native={}", dir.display());
        }
    }
    let kind = if linkage.is_dynamic() {
        "dylib"
    } else {
        "static"
    };
    for lib in libs {
        println!("cargo:rustc-link-lib={kind}={lib}");
    }
    println!("cargo:includedir={}/include", prefix.display());
    if linkage.is_dynamic() {
        emit_runtime_search(prefix);
        println!("cargo:libdir={}", prefix.join("lib").display());
        println!("cargo:bindir={}", prefix.join("bin").display());
    }
}

/// Make a shared library loadable by the package's own tests, examples and
/// benches.
///
/// Windows installs DLLs in `bin`, and Cargo puts every `native=` search path
/// on `PATH` for `cargo test`/`run`, so `bin` is added. Unix targets get an
/// rpath to `lib`/`lib64`, since macOS records `@rpath/...` install names and
/// Linux uses versioned `.so` names. Build-script link args reach only this
/// package's targets, so dependents must still ship the libraries (see
/// [`copy_shared_libs`]).
fn emit_runtime_search(prefix: &Path) {
    if crate::target_is_unix() {
        for dir in ["lib", "lib64"] {
            let dir = prefix.join(dir);
            if dir.is_dir() {
                println!("cargo:rustc-link-arg=-Wl,-rpath,{}", dir.display());
            }
        }
    } else {
        let bin = prefix.join("bin");
        if bin.is_dir() {
            println!("cargo:rustc-link-search=native={}", bin.display());
        }
    }
}

/// Emit the C++ runtime link directive appropriate to `linkage`.
///
/// A static library's objects reference the C++ standard library, so it must
/// be linked in (see [`link_cpp_runtime`](crate::link_cpp_runtime)). A shared
/// library already records its own dependency on the runtime, so nothing is
/// emitted and the program does not pick up a second copy.
pub fn link_cpp_runtime_for(linkage: Linkage) {
    if linkage == Linkage::Static {
        crate::link_cpp_runtime();
    }
}

/// The shared libraries installed under `prefix`: DLLs in `bin`, and `.so` /
/// `.dylib` files (including versioned names) in `lib` and `lib64`.
pub fn shared_library_files(prefix: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in ["bin", "lib", "lib64"] {
        let Ok(entries) = std::fs::read_dir(prefix.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && is_shared_library(&path) {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn is_shared_library(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    name.ends_with(".dll")
        || name.ends_with(".dylib")
        || name.ends_with(".so")
        || name.contains(".so.")
}

/// Copy the shared libraries under `prefix` into `dest`, creating it if
/// needed. Returns the copied paths. Use this from a packaging step to put
/// them next to the executable.
pub fn copy_shared_libs(prefix: &Path, dest: &Path) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dest)
        .map_err(|e| Error::io(format!("cannot create {}", dest.display()), e))?;
    let mut copied = Vec::new();
    for file in shared_library_files(prefix) {
        let target = dest.join(file.file_name().expect("file has a name"));
        std::fs::copy(&file, &target).map_err(|e| {
            Error::io(
                format!("cannot copy {} to {}", file.display(), target.display()),
                e,
            )
        })?;
        copied.push(target);
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linkage_maps_to_cmake_and_names() {
        assert_eq!(Linkage::Static.build_shared_libs(), "OFF");
        assert_eq!(Linkage::Dynamic.build_shared_libs(), "ON");
        assert_eq!(Linkage::Static.as_str(), "static");
        assert!(Linkage::Dynamic.is_dynamic());
    }

    #[test]
    fn finds_and_copies_only_shared_libraries() {
        let root = std::env::temp_dir().join(format!("woodshed-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (prefix, dest) = (root.join("prefix"), root.join("out"));
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        std::fs::create_dir_all(prefix.join("lib")).unwrap();
        for f in [
            "bin/geos_c.dll",
            "lib/libgeos_c.so.1.20",
            "lib/libgeos.dylib",
            "lib/geos_c.lib",
            "lib/libgeos.a",
        ] {
            std::fs::write(prefix.join(f), b"x").unwrap();
        }

        let names: Vec<String> = shared_library_files(&prefix)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["geos_c.dll", "libgeos.dylib", "libgeos_c.so.1.20"]);

        let copied = copy_shared_libs(&prefix, &dest).unwrap();
        assert_eq!(copied.len(), 3);
        assert!(dest.join("geos_c.dll").is_file());
        assert!(!dest.join("libgeos.a").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
