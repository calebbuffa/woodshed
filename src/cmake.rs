//! Building a CMake project, with the MSVC runtime under control.
//!
//! # The CRT problem
//!
//! Statically linking a C++ library is not the same as producing a portable
//! executable. On MSVC two settings must agree: CMake's
//! `CMAKE_MSVC_RUNTIME_LIBRARY` and Rust's `-C target-feature=+crt-static`. A
//! mismatch does not fail during the C++ build; it fails much later, at link
//! time, with a wall of unresolved `__imp_*` symbols that says nothing about
//! the actual cause.
//!
//! [`CmakeBuild`] makes those two settings impossible to diverge - but which
//! side to converge on is the caller's decision, not this crate's. If the
//! native library allocates memory that Rust frees, or vice versa, the heaps
//! must be shared and the runtime DLL is mandatory. If every allocation is
//! freed by the library that made it, a static runtime is safe and removes the
//! redistributable dependency entirely. Only the caller knows which is true,
//! so [`MsvcRuntime`] asks.

use crate::{Result, Toolchain};
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Which MSVC C runtime the native library should link.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MsvcRuntime {
    /// Match the Rust target's CRT: static under `+crt-static`, else dynamic.
    ///
    /// Correct when no allocation crosses the language boundary - that is, the
    /// library frees everything it allocates, through its own free function.
    /// The payoff is that a `+crt-static` build ships as a single executable
    /// with no MSVC redistributable.
    #[default]
    MatchRust,
    /// Always link the runtime DLL, even under `+crt-static`.
    ///
    /// Required when allocations cross the boundary: Rust and the native
    /// library must then share one heap, and only the DLL runtime provides
    /// that. Freeing a pointer on the wrong heap corrupts memory in ways that
    /// surface far from the cause.
    AlwaysDynamic,
    /// Always link the static MSVC runtime; allocation ownership must be compatible.
    AlwaysStatic,
}

impl MsvcRuntime {
    pub(crate) fn cmake_value(self) -> &'static str {
        match self {
            Self::MatchRust if crate::crt_static() => "MultiThreaded",
            Self::AlwaysStatic => "MultiThreaded",
            Self::MatchRust | Self::AlwaysDynamic => "MultiThreadedDLL",
        }
    }
}

/// A CMake configure-and-install run.
///
/// Only the parts that are easy to get wrong are handled for you - the runtime
/// selection, the parallel build, the interruptible process tree, the stale
/// build tree check. Everything project-specific goes through [`define`] and
/// [`arg`], so nothing is off limits.
///
/// [`define`]: CmakeBuild::define
/// [`arg`]: CmakeBuild::arg
pub struct CmakeBuild {
    source: PathBuf,
    build: PathBuf,
    prefix: PathBuf,
    toolchain: Toolchain,
    runtime: MsvcRuntime,
    build_type: String,
    launcher_define: Option<String>,
    target: Option<String>,
    args: Vec<String>,
    target_triple: Option<String>,
    environment: Vec<(OsString, OsString)>,
}

impl CmakeBuild {
    /// Build the project at `source` into `dirs`.
    pub fn new(source: impl Into<PathBuf>, dirs: &crate::BuildDirs) -> Self {
        Self {
            source: source.into(),
            build: dirs.build.clone(),
            prefix: dirs.prefix.clone(),
            toolchain: Toolchain::detect(),
            runtime: MsvcRuntime::default(),
            build_type: "Release".into(),
            launcher_define: None,
            target: Some("install".into()),
            args: Vec::new(),
            target_triple: None,
            environment: Vec::new(),
        }
    }

    /// Install into a specific prefix instead of the one inside the build dir.
    #[must_use]
    pub fn prefix(mut self, prefix: impl Into<PathBuf>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Build a specific target. Defaults to `install`.
    ///
    /// A superbuild whose sub-projects install themselves has no top-level
    /// `install` target; pass `None` to build the default target instead.
    #[must_use]
    pub fn target(mut self, target: Option<impl Into<String>>) -> Self {
        self.target = target.map(Into::into);
        self
    }

    /// Choose how the MSVC runtime is selected. Ignored on other targets.
    #[must_use]
    pub fn msvc_runtime(mut self, runtime: MsvcRuntime) -> Self {
        self.runtime = runtime;
        self
    }

    /// Set `CMAKE_BUILD_TYPE`. Defaults to `Release`.
    #[must_use]
    pub fn build_type(mut self, build_type: impl Into<String>) -> Self {
        self.build_type = build_type.into();
        self
    }

    /// Use a specific toolchain instead of the detected one.
    #[must_use]
    pub fn toolchain(mut self, toolchain: Toolchain) -> Self {
        self.toolchain = toolchain;
        self
    }

    /// Select an explicit build triple for compiler/runtime and rpath policy.
    ///
    /// This does not select compilers or cross-compile by itself; cross builds
    /// still require a matching CMake toolchain file.
    #[must_use]
    pub fn target_triple(mut self, triple: impl Into<String>) -> Self {
        self.target_triple = Some(triple.into());
        self
    }

    /// Name the cache variable used for the compiler launcher.
    ///
    /// By default both `CMAKE_C_COMPILER_LAUNCHER` and
    /// `CMAKE_CXX_COMPILER_LAUNCHER` are set. Calling this replaces that
    /// default with only the specified variable, including project-specific
    /// launcher variables.
    #[must_use]
    pub fn compiler_launcher_define(mut self, define: impl Into<String>) -> Self {
        self.launcher_define = Some(define.into());
        self
    }

    /// Build static or shared libraries: sets `BUILD_SHARED_LIBS`.
    ///
    /// Shared builds also get an install rpath relative to the library
    /// (`$ORIGIN` on Linux, `@loader_path` on macOS). Without it a shared
    /// library cannot find its sibling libraries (for example `libgeos_c.so`
    /// needing `libgeos.so`), because the executable's rpath does not apply to
    /// transitive dependencies on Linux, and a copied library directory would
    /// not load.
    ///
    /// Also remember to mix the same [`Linkage`](crate::Linkage) into the
    /// [`BuildKey`](crate::BuildKey) with `BuildKey::linkage`.
    #[must_use]
    pub fn linkage(self, linkage: crate::Linkage) -> Self {
        let build = self.define("BUILD_SHARED_LIBS", linkage.build_shared_libs());
        let os = build
            .target_triple
            .as_ref()
            .map(|triple| {
                if triple.contains("windows") {
                    "windows"
                } else if triple.contains("apple") {
                    "macos"
                } else if triple.contains("linux") {
                    "linux"
                } else {
                    ""
                }
            })
            .map(str::to_owned)
            .unwrap_or_else(crate::target_os);
        match linkage.install_rpath(&os) {
            Some(rpath) => build.define("CMAKE_INSTALL_RPATH", rpath),
            None => build,
        }
    }

    /// Set a CMake cache variable.
    #[must_use]
    pub fn define(mut self, name: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        self.args
            .push(format!("-D{}={}", name.as_ref(), value.as_ref()));
        self
    }

    /// Pass a raw argument to the configure step.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Set an environment variable for both configure and build child processes.
    #[must_use]
    pub fn env(mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.environment.push((name.into(), value.into()));
        self
    }

    /// Configure the project without compiling or installing native targets.
    ///
    /// Useful for inspecting generated headers, CMake cache values, and File API
    /// replies before choosing which targets to build. The caller must hold the
    /// build directory lock until the returned build has finished. Configuration
    /// alone does not establish that installed artifacts are complete.
    pub fn configure(self) -> Result<ConfiguredCmakeBuild> {
        std::fs::create_dir_all(&self.build).map_err(|error| {
            crate::Error::io(format!("cannot create {}", self.build.display()), error)
        })?;
        self.toolchain
            .reset_build_dir_if_stale(&self.build, &self.source);

        let mut configure = Command::new("cmake");
        configure.envs(self.environment.iter().map(|(name, value)| (name, value)));
        configure
            .arg("-S")
            .arg(&self.source)
            .arg("-B")
            .arg(&self.build)
            .arg(format!("-DCMAKE_INSTALL_PREFIX={}", self.prefix.display()))
            .arg(format!("-DCMAKE_BUILD_TYPE={}", self.build_type));

        if self.target_triple.as_ref().map_or_else(
            || env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc"),
            |triple| triple.ends_with("-msvc"),
        ) {
            configure.arg(format!(
                "-DCMAKE_MSVC_RUNTIME_LIBRARY={}",
                self.runtime.cmake_value()
            ));
        }

        configure.args(&self.args);
        match &self.launcher_define {
            Some(define) => self.toolchain.apply_to_configure(&mut configure, define),
            None => self.toolchain.apply_to_configure_default(&mut configure),
        }
        crate::process::check(&mut configure, "cmake configure")?;

        Ok(ConfiguredCmakeBuild {
            build: self.build,
            prefix: self.prefix,
            build_type: self.build_type,
            target: self.target,
            toolchain: self.toolchain,
            environment: self.environment,
        })
    }

    /// Configure, build, and install. Returns the install prefix.
    pub fn install(self) -> Result<PathBuf> {
        self.configure()?.build()
    }

    /// Configure for the final prefix, then build and install into a private stage.
    ///
    /// The caller owns staging, publication and locks. This uses `--prefix`,
    /// not DESTDIR, so it also works on Windows. The configured final prefix
    /// remains available to upstream generated headers/configuration files.
    pub fn install_staged(self, stage: &std::path::Path) -> Result<PathBuf> {
        let configured = self.target(None::<String>).configure()?;
        let mut build = Command::new("cmake");
        build.envs(
            configured
                .environment
                .iter()
                .map(|(name, value)| (name, value)),
        );
        build
            .arg("--build")
            .arg(&configured.build)
            .arg("--config")
            .arg(&configured.build_type);
        configured.toolchain.apply_to_build(&mut build);
        crate::process::check(&mut build, "cmake build")?;
        let mut install = Command::new("cmake");
        install.envs(
            configured
                .environment
                .iter()
                .map(|(name, value)| (name, value)),
        );
        install
            .arg("--install")
            .arg(&configured.build)
            .arg("--config")
            .arg(&configured.build_type)
            .arg("--prefix")
            .arg(stage);
        crate::process::check(&mut install, "cmake staged install")?;
        let manifest = std::fs::read_to_string(configured.build.join("install_manifest.txt"))
            .map_err(|error| crate::Error::io("read CMake installation manifest", error))?;
        let stage = stage
            .canonicalize()
            .map_err(|error| crate::Error::io("resolve staged installation", error))?;
        for file in manifest.lines() {
            let path = std::path::Path::new(file).canonicalize().map_err(|error| {
                crate::Error::io(format!("resolve installed file {file}"), error)
            })?;
            if !path.starts_with(&stage) {
                return Err(crate::Error::Incomplete {
                    detail: format!(
                        "CMake installed outside its private staging prefix: {file}; absolute install destinations are not supported"
                    ),
                });
            }
        }
        Ok(configured.prefix)
    }

    /// The install prefix this build will produce.
    pub fn install_prefix(&self) -> &Path {
        &self.prefix
    }
}

/// A successfully configured CMake project, not yet necessarily compiled.
///
/// Created by [`CmakeBuild::configure`]. Keep the external build-directory lock
/// alive across configuration, inspection, and compilation.
pub struct ConfiguredCmakeBuild {
    build: PathBuf,
    prefix: PathBuf,
    build_type: String,
    target: Option<String>,
    toolchain: Toolchain,
    environment: Vec<(OsString, OsString)>,
}

impl ConfiguredCmakeBuild {
    /// Directory containing `CMakeCache.txt` and generated project files.
    pub fn build_directory(&self) -> &Path {
        &self.build
    }

    /// Prefix into which the selected target is expected to install.
    pub fn install_prefix(&self) -> &Path {
        &self.prefix
    }

    /// Compile the configured target and return its expected install prefix.
    ///
    /// Defaults to the `install` target selected by [`CmakeBuild`]. If the
    /// builder selected a different target, that target controls whether any
    /// installation occurs; callers must still check their required artifacts.
    pub fn build(self) -> Result<PathBuf> {
        let mut build = Command::new("cmake");
        build.envs(self.environment.iter().map(|(name, value)| (name, value)));
        build
            .arg("--build")
            .arg(&self.build)
            .arg("--config")
            .arg(&self.build_type);
        if let Some(target) = &self.target {
            build.arg("--target").arg(target);
        }
        self.toolchain.apply_to_build(&mut build);
        crate::process::check(&mut build, "cmake build")?;

        Ok(self.prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn configure_uses_default_or_explicit_launcher_variables() {
        let root = env::temp_dir().join(format!(
            "woodshed-launchers-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let source = root.join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("CMakeLists.txt"),
            "cmake_minimum_required(VERSION 3.15)\nproject(launcher_test NONE)\n",
        )
        .unwrap();
        for (index, define) in [
            None,
            Some("CMAKE_C_COMPILER_LAUNCHER"),
            Some("CMAKE_CXX_COMPILER_LAUNCHER"),
            Some("CUSTOM_LAUNCHER"),
        ]
        .into_iter()
        .enumerate()
        {
            let dirs = crate::BuildDirs {
                build: root.join(format!("build-{index}")),
                prefix: root.join("prefix"),
            };
            let toolchain = Toolchain::detect();
            let mut expected = Command::new("cmake");
            match define {
                Some(define) => toolchain.apply_to_configure(&mut expected, define),
                None => toolchain.apply_to_configure_default(&mut expected),
            }
            let mut build = CmakeBuild::new(&source, &dirs).toolchain(toolchain);
            if let Some(define) = define {
                build = build.compiler_launcher_define(define);
            }
            let _guard = dirs.lock().unwrap();
            build.configure().unwrap();
            let cache = std::fs::read_to_string(dirs.build.join("CMakeCache.txt")).unwrap();
            for name in [
                "CMAKE_C_COMPILER_LAUNCHER",
                "CMAKE_CXX_COMPILER_LAUNCHER",
                "CUSTOM_LAUNCHER",
            ] {
                let expected_value = expected
                    .get_args()
                    .find_map(|arg| arg.to_str()?.strip_prefix(&format!("-D{name}=")));
                let actual = cache
                    .lines()
                    .find_map(|line| line.strip_prefix(&format!("{name}:UNINITIALIZED=")));
                assert_eq!(actual, expected_value, "{name}, override {define:?}");
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn configure_exposes_metadata_without_building_targets() {
        let root = env::temp_dir().join(format!(
            "woodshed-configure-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let source = root.join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("CMakeLists.txt"),
            r#"
    cmake_minimum_required(VERSION 3.15)
    project(woodshed_configure_test NONE)
    file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/configured.txt" "configured")
    add_custom_target(artifacts
      COMMAND "${CMAKE_COMMAND}" -E touch "${CMAKE_CURRENT_BINARY_DIR}/compiled.txt")
    "#,
        )
        .unwrap();
        let dirs = crate::BuildDirs {
            build: root.join("build"),
            prefix: root.join("prefix"),
        };
        let guard = dirs.lock().unwrap();
        let configured = CmakeBuild::new(&source, &dirs)
            .target(Some("artifacts"))
            .configure()
            .unwrap();
        assert_eq!(configured.build_directory(), dirs.build);
        assert_eq!(configured.install_prefix(), dirs.prefix);
        assert!(dirs.build.join("configured.txt").is_file());
        assert!(!dirs.build.join("compiled.txt").exists());
        assert_eq!(configured.build().unwrap(), dirs.prefix);
        assert!(dirs.build.join("compiled.txt").is_file());
        drop(guard);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
