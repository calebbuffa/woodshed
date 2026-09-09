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
}

impl MsvcRuntime {
    fn cmake_value(self) -> &'static str {
        match self {
            Self::MatchRust if crate::crt_static() => "MultiThreaded",
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
    launcher_define: String,
    target: Option<String>,
    args: Vec<String>,
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
            launcher_define: "CMAKE_CXX_COMPILER_LAUNCHER".into(),
            target: Some("install".into()),
            args: Vec::new(),
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

    /// Name the cache variable used for the compiler launcher.
    ///
    /// Defaults to `CMAKE_CXX_COMPILER_LAUNCHER`; a C project wants
    /// `CMAKE_C_COMPILER_LAUNCHER`.
    #[must_use]
    pub fn compiler_launcher_define(mut self, define: impl Into<String>) -> Self {
        self.launcher_define = define.into();
        self
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

    /// Configure, build, and install. Returns the install prefix.
    pub fn install(self) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.build).map_err(|error| {
            crate::Error::io(format!("cannot create {}", self.build.display()), error)
        })?;
        self.toolchain
            .reset_build_dir_if_stale(&self.build, &self.source);

        let mut configure = Command::new("cmake");
        configure
            .arg("-S")
            .arg(&self.source)
            .arg("-B")
            .arg(&self.build)
            .arg(format!("-DCMAKE_INSTALL_PREFIX={}", self.prefix.display()))
            .arg(format!("-DCMAKE_BUILD_TYPE={}", self.build_type));

        if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            configure.arg(format!(
                "-DCMAKE_MSVC_RUNTIME_LIBRARY={}",
                self.runtime.cmake_value()
            ));
        }

        configure.args(&self.args);
        self.toolchain
            .apply_to_configure(&mut configure, &self.launcher_define);
        crate::process::check(&mut configure, "cmake configure")?;

        let mut build = Command::new("cmake");
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

    /// The install prefix this build will produce.
    pub fn install_prefix(&self) -> &Path {
        &self.prefix
    }
}
