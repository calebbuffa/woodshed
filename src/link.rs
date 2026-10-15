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
    /// The CMake `CMAKE_INSTALL_RPATH` a shared build needs so its libraries
    /// find each other from their own directory, or `None` where the platform
    /// needs none. Used for both the CMake arguments and the cache key, so the
    /// two cannot disagree.
    pub(crate) fn install_rpath(self, target_os: &str) -> Option<&'static str> {
        if !self.is_dynamic() {
            return None;
        }
        match target_os {
            "linux" => Some("$ORIGIN"),
            "macos" => Some("@loader_path"),
            _ => None,
        }
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
///
/// For [`Linkage::Dynamic`] the shared libraries are also staged in this build
/// script's `OUT_DIR` and that directory is added as a `native=` search path.
/// Cargo puts `native=` directories **inside the target directory** on the
/// loader path (`PATH`, `LD_LIBRARY_PATH`, `DYLD_FALLBACK_LIBRARY_PATH`) of
/// the programs it runs, and ignores any outside it (rust-lang/cargo#3366). The
/// build cache lives outside it, so without staging, `cargo run` and `cargo
/// test` could not load the library.
///
/// # Panics
///
/// If the shared libraries cannot be staged: continuing would produce a build
/// whose programs fail at start-up.
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
        let out_dir = std::env::var_os("OUT_DIR").expect("OUT_DIR is set for build scripts");
        let staged = stage_shared_libs(prefix, &Path::new(&out_dir).join("woodshed-shared"))
            .unwrap_or_else(|error| panic!("woodshed: cannot stage shared libraries: {error}"));
        println!("cargo:rustc-link-search=native={}", staged.display());
        println!("cargo:libdir={}", prefix.join("lib").display());
        println!("cargo:bindir={}", prefix.join("bin").display());
    }
}

/// Replace `dest` with a copy of the shared libraries installed under `prefix`.
/// Starting empty means a library from an earlier version never lingers.
fn stage_shared_libs(prefix: &Path, dest: &Path) -> Result<PathBuf> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)
            .map_err(|e| Error::io(format!("cannot clear {}", dest.display()), e))?;
    }
    copy_shared_libs(prefix, dest)?;
    Ok(dest.to_path_buf())
}

/// Make the final executable find shared libraries sitting next to it.
///
/// Call this from the build script of the *application* that ships a dynamic
/// build. Link arguments from a dependency's build script never reach the
/// application's executable, so a library cannot do this for its users.
/// Linux gets `$ORIGIN` and macOS `@executable_path`. Windows needs nothing:
/// the loader always searches the executable's directory.
pub fn emit_exe_rpath() {
    match crate::target_os().as_str() {
        "linux" => println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN"),
        "macos" => println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path"),
        _ => {}
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

        std::fs::write(dest.join("stale.dll"), b"x").unwrap();
        let staged = stage_shared_libs(&prefix, &dest).unwrap();
        assert!(staged.join("libgeos.dylib").is_file());
        assert!(!staged.join("stale.dll").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_rpath_follows_platform_and_linkage() {
        assert_eq!(Linkage::Dynamic.install_rpath("linux"), Some("$ORIGIN"));
        assert_eq!(
            Linkage::Dynamic.install_rpath("macos"),
            Some("@loader_path")
        );
        assert_eq!(Linkage::Dynamic.install_rpath("windows"), None);
        assert_eq!(Linkage::Static.install_rpath("linux"), None);
    }
}
