//! Picking the fastest available generator and compiler cache.
//!
//! Nothing here is specific to any particular dependency, and every choice has
//! a fallback that exists on every machine, so a bare toolchain still builds.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The best toolchain available for driving CMake, detected once and reused
/// for both the configure and build steps.
#[derive(Clone, Debug)]
pub struct Toolchain {
    generator: Option<String>,
    compiler_launcher: Option<PathBuf>,
    jobs: usize,
}

impl Toolchain {
    /// Detect the fastest safe toolchain for the current target.
    pub fn detect() -> Self {
        Self::detect_with_msvc(env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc"))
    }

    /// Detect a toolchain for an explicit host or target triple.
    pub fn detect_for_target(target: &str) -> Self {
        Self::detect_with_msvc(target.ends_with("-msvc"))
    }

    fn detect_with_msvc(is_msvc: bool) -> Self {
        // Ninja needs `cl.exe`'s environment on MSVC, which the Visual Studio
        // generator sets up itself. Only prefer Ninja there if that
        // environment is already present.
        let msvc_env_ready = env::var_os("INCLUDE").is_some() && env::var_os("LIB").is_some();
        let ninja = find_on_path("ninja").filter(|_| !is_msvc || msvc_env_ready);

        // ccache's `cl.exe` support is unreliable; sccache supports it natively.
        // The compiler in play is decided by the target, not by the host.
        let compiler_launcher = find_on_path("sccache")
            .or_else(|| (!is_msvc).then(|| find_on_path("ccache")).flatten());

        Self {
            generator: env::var("CMAKE_GENERATOR")
                .ok()
                .filter(|generator| !generator.trim().is_empty())
                .or_else(|| ninja.map(|_| "Ninja".to_string())),
            compiler_launcher,
            jobs: thread::available_parallelism().map_or(1, |n| n.get()),
        }
    }

    /// Override the parallel job count.
    #[must_use]
    pub fn jobs(mut self, jobs: usize) -> Self {
        self.jobs = jobs.max(1);
        self
    }

    /// The number of parallel jobs that will be requested.
    pub fn job_count(&self) -> usize {
        self.jobs
    }

    /// Explicit or automatically selected CMake generator; None uses CMake's default.
    pub fn generator_name(&self) -> Option<&str> {
        self.generator.as_deref()
    }

    /// Recreate `build_dir` if it was configured against a different generator
    /// or a different source path.
    ///
    /// Both are hard CMake invariants that cannot be fixed by reconfiguring in
    /// place, so a mismatch means starting the build tree over. The source
    /// path check matters more than it looks: CMake binds a build tree to the
    /// exact path it was configured with, so a moved checkout - or `cargo
    /// publish`'s verification build - would otherwise fail outright.
    pub fn reset_build_dir_if_stale(&self, build_dir: &Path, source_dir: &Path) {
        let Ok(existing) = fs::read_to_string(build_dir.join("CMakeCache.txt")) else {
            return;
        };

        let existing_gen = existing
            .lines()
            .find_map(|line| line.strip_prefix("CMAKE_GENERATOR:INTERNAL="))
            .unwrap_or("");
        let desired = self.generator.as_deref().unwrap_or("");
        let generator_changed = if desired.is_empty() {
            existing_gen == "Ninja"
        } else {
            existing_gen != desired
        };

        let existing_home = existing
            .lines()
            .find_map(|line| line.strip_prefix("CMAKE_HOME_DIRECTORY:INTERNAL="))
            .unwrap_or("");
        let source_moved = !existing_home.is_empty()
            && !same_source_directory(Path::new(existing_home), source_dir);

        if generator_changed || source_moved {
            eprintln!(
                "woodshed: build tree configuration changed; recreating {}",
                build_dir.display()
            );
            force_recreate_dir(build_dir);
        }
    }
}

fn same_source_directory(existing: &Path, desired: &Path) -> bool {
    let existing = existing
        .canonicalize()
        .unwrap_or_else(|_| existing.to_path_buf());
    let desired = desired
        .canonicalize()
        .unwrap_or_else(|_| desired.to_path_buf());
    normalize_cmake_path(&existing.to_string_lossy(), cfg!(windows))
        == normalize_cmake_path(&desired.to_string_lossy(), cfg!(windows))
}

fn normalize_cmake_path(path: &str, windows: bool) -> String {
    if !windows {
        return path.to_owned();
    }
    let path = path.replace('\\', "/").to_lowercase();
    if let Some(unc) = path.strip_prefix("//?/unc/") {
        format!("//{unc}")
    } else {
        path.strip_prefix("//?/").unwrap_or(&path).to_owned()
    }
}

impl Toolchain {
    /// Append the generator and compiler-cache flags for both C and C++.
    ///
    /// Use [`Self::apply_to_configure`] instead for a single project-specific
    /// compiler launcher variable.
    pub fn apply_to_configure_default(&self, command: &mut Command) {
        self.apply_to_configure(command, "CMAKE_CXX_COMPILER_LAUNCHER");
        if let Some(launcher) = &self.compiler_launcher {
            command.arg(format!(
                "-DCMAKE_C_COMPILER_LAUNCHER={}",
                launcher.display()
            ));
        }
    }

    /// Append this choice's flags to a `cmake -S -B ...` command.
    ///
    /// `compiler_launcher_define` is the cache variable the project reads for
    /// its compiler launcher, which is project-specific - commonly
    /// `CMAKE_CXX_COMPILER_LAUNCHER`.
    pub fn apply_to_configure(&self, command: &mut Command, compiler_launcher_define: &str) {
        if let Some(generator) = &self.generator {
            eprintln!("woodshed: using {generator} generator");
            command.arg("-G").arg(generator);
        }
        if let Some(launcher) = &self.compiler_launcher {
            eprintln!("woodshed: using compiler cache {}", launcher.display());
            command.arg(format!(
                "-D{compiler_launcher_define}={}",
                launcher.display()
            ));
        }
    }

    /// Append `--parallel` to a `cmake --build ...` command, and set the
    /// environment variable that nested `ExternalProject_Add` steps inherit.
    pub fn apply_to_build(&self, command: &mut Command) -> usize {
        command.arg("--parallel").arg(self.jobs.to_string());
        command.env("CMAKE_BUILD_PARALLEL_LEVEL", self.jobs.to_string());
        self.jobs
    }
}

/// Locate an executable on `PATH`, trying the Windows `.exe` suffix too.
/// Find an executable named `name` on `PATH`.
///
/// On Windows the `.exe` suffix is implicit; on Unix a match must actually
/// carry an execute bit, or a same-named data file would shadow the tool.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    env::split_paths(&path_var).find_map(|dir| {
        let plain = dir.join(name);
        if is_executable(&plain) {
            return Some(plain);
        }
        let exe = dir.join(format!("{name}.exe"));
        is_executable(&exe).then_some(exe)
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Delete and recreate `dir`, tolerating transient Windows file locks.
///
/// A just-exited compiler, or an antivirus scan, can briefly hold a file
/// inside. Retry the delete, then fall back to renaming the tree aside - a
/// rename does not require the handles inside it to be closed.
fn force_recreate_dir(dir: &Path) {
    let mut last_error = None;
    for attempt in 0..10u32 {
        match fs::remove_dir_all(dir) {
            Ok(()) => {
                fs::create_dir_all(dir).expect("recreate build dir");
                return;
            }
            Err(error) => {
                last_error = Some(error);
                thread::sleep(Duration::from_millis(200 * u64::from(attempt + 1)));
            }
        }
    }

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let stale = PathBuf::from(format!("{}-stale-{stamp}", dir.display()));
    if fs::rename(dir, &stale).is_ok() {
        // Best effort: a leftover `-stale-*` directory only wastes disk space.
        let _ = fs::remove_dir_all(&stale);
        fs::create_dir_all(dir).expect("recreate build dir");
        return;
    }

    panic!(
        "woodshed: could not recreate build tree {} ({})\n  \
         another process (cmake, ninja, msbuild, an antivirus scan) may still have a \
         file open inside it - close it and rerun, or delete the directory manually",
        dir.display(),
        last_error
            .map(|error| error.to_string())
            .unwrap_or_default()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiler_cache_defaults_cover_c_and_cxx_and_overrides_are_exclusive() {
        let toolchain = Toolchain {
            generator: Some("Ninja".into()),
            compiler_launcher: Some(PathBuf::from("cache-tool")),
            jobs: 1,
        };
        let mut command = Command::new("cmake");
        toolchain.apply_to_configure_default(&mut command);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "-G",
                "Ninja",
                "-DCMAKE_CXX_COMPILER_LAUNCHER=cache-tool",
                "-DCMAKE_C_COMPILER_LAUNCHER=cache-tool",
            ]
        );
        for define in [
            "CMAKE_C_COMPILER_LAUNCHER",
            "CMAKE_CXX_COMPILER_LAUNCHER",
            "CUSTOM_LAUNCHER",
        ] {
            let mut command = Command::new("cmake");
            toolchain.apply_to_configure(&mut command, define);
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                [
                    std::ffi::OsStr::new("-G"),
                    std::ffi::OsStr::new("Ninja"),
                    std::ffi::OsStr::new(&format!("-D{define}=cache-tool")),
                ]
            );
        }
        let mut command = Command::new("cmake");
        Toolchain {
            compiler_launcher: None,
            ..toolchain
        }
        .apply_to_configure_default(&mut command);
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["-G", "Ninja"]);
    }

    #[test]
    fn cmake_paths_preserve_unix_case_and_backslashes() {
        assert_ne!(
            normalize_cmake_path("/tmp/Source", false),
            normalize_cmake_path("/tmp/source", false)
        );
        assert_ne!(
            normalize_cmake_path("/tmp/a\\b", false),
            normalize_cmake_path("/tmp/a/b", false)
        );
    }

    #[test]
    fn cmake_paths_match_windows_verbatim_and_unc_spellings() {
        assert_eq!(
            normalize_cmake_path(r"\\?\C:\Users\Example\Source", true),
            normalize_cmake_path("C:/Users/example/Source", true)
        );
        assert_eq!(
            normalize_cmake_path(r"\\?\UNC\Server\Share\Source", true),
            normalize_cmake_path("//server/share/source", true)
        );
        assert_ne!(
            normalize_cmake_path("C:/one", true),
            normalize_cmake_path("D:/one", true)
        );
    }

    #[test]
    fn identical_cmake_source_keeps_cached_build_files() {
        let root = env::temp_dir().join(format!(
            "woodshed-cmake-source-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let source = root.join("source");
        let build = root.join("build");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&build).unwrap();
        let home = source.to_string_lossy().replace('\\', "/");
        fs::write(
            build.join("CMakeCache.txt"),
            format!("CMAKE_GENERATOR:INTERNAL=Ninja\nCMAKE_HOME_DIRECTORY:INTERNAL={home}\n"),
        )
        .unwrap();
        let sentinel = build.join("compiled-object");
        fs::write(&sentinel, b"keep").unwrap();
        let toolchain = Toolchain {
            generator: Some("Ninja".into()),
            compiler_launcher: None,
            jobs: 1,
        };
        toolchain.reset_build_dir_if_stale(&build, &source);
        assert!(
            sentinel.is_file(),
            "equivalent source spellings must preserve the build"
        );
        fs::remove_file(sentinel).unwrap();
        fs::remove_file(build.join("CMakeCache.txt")).unwrap();
        fs::remove_dir(build).unwrap();
        fs::remove_dir(source).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn find_on_path_locates_a_real_tool() {
        // Every supported platform ships one of these.
        let found = find_on_path("cargo");
        assert!(found.is_some(), "cargo should be on PATH during tests");
    }

    #[test]
    fn find_on_path_ignores_missing_tools() {
        assert!(find_on_path("woodshed-definitely-not-a-real-tool").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn find_on_path_skips_non_executable_files() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("woodshed-path-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // A same-named data file must not shadow the real tool.
        let decoy = dir.join("woodshed-probe-tool");
        fs::write(&decoy, b"not a program").unwrap();
        fs::set_permissions(&decoy, fs::Permissions::from_mode(0o644)).unwrap();

        let original = env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![dir.clone()];
        dirs.extend(env::split_paths(&original));
        let joined = env::join_paths(dirs).unwrap();

        // SAFETY: single-threaded test, restored immediately below.
        unsafe { env::set_var("PATH", &joined) };
        let found_data_file = find_on_path("woodshed-probe-tool");
        fs::set_permissions(&decoy, fs::Permissions::from_mode(0o755)).unwrap();
        let found_executable = find_on_path("woodshed-probe-tool");
        unsafe { env::set_var("PATH", &original) };

        assert!(found_data_file.is_none(), "non-executable must not match");
        assert!(found_executable.is_some(), "executable must match");

        let _ = fs::remove_dir_all(&dir);
    }
}
