use super::*;
use crate::Result;
#[cfg(all(feature = "pin", any(feature = "vcpkg", feature = "conan")))]
use crate::project::PackageLock;
use crate::project::{CmakePackage, Dependency, PackageLinkage, Project, invalid};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(all(feature = "pin", feature = "vcpkg"))]
use serde_json::json;
use std::{
    collections::BTreeSet,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Serialize, Deserialize)]
#[serde(tag = "provider", deny_unknown_fields)]
#[cfg(any(feature = "vcpkg", feature = "conan"))]
enum Data {
    Vcpkg {
        manifest: Value,
        tool: String,
        root_revision: String,
    },
    Conan {
        lock: String,
        tool: String,
    },
}

/// Explicit configuration for the generated metadata-only CMake consumer.
pub struct InstallOptions {
    /// Isolated package installation and metadata build directory.
    pub directory: PathBuf,
    /// Cargo target triple; checked against TARGET when called by a build script.
    pub target: String,
    /// Release/Debug configuration to query.
    pub configuration: String,
}

impl InstallOptions {
    /// Isolate package outputs under Cargo OUT_DIR and use Cargo's target/profile.
    pub fn cargo(name: &str) -> Result<Self> {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        {
            return Err(invalid(
                "provider output name must be a dependency identifier",
            ));
        }
        let required = |name| {
            std::env::var(name).map_err(|_| {
                invalid(format!(
                    "{name} is required; use InstallOptions explicitly outside build scripts"
                ))
            })
        };
        Ok(Self {
            directory: PathBuf::from(required("OUT_DIR")?)
                .join("woodshed")
                .join(name),
            target: required("TARGET")?,
            configuration: if required("DEBUG")? == "true" {
                "Debug"
            } else {
                "Release"
            }
            .into(),
        })
    }
}

/// Metadata reported by CMake's evaluated imported-target graph.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Installation {
    /// Transitive include directories for bindings/native consumers.
    pub include_dirs: Vec<PathBuf>,
    /// Transitive preprocessor definitions.
    pub definitions: Vec<String>,
    /// Native compile flags, not Rust flags.
    pub compile_flags: Vec<String>,
    /// Evaluated ordered linker tokens, including library paths/system libraries.
    pub link_arguments: Vec<String>,
    /// Package runtime files/directories, for explicit downstream deployment.
    pub runtime_directories: Vec<PathBuf>,
    linkage: PackageLinkage,
}

impl Installation {
    /// Translate common library arguments to propagating Cargo link directives.
    ///
    /// Refuses arbitrary linker flags instead of silently dropping them or using
    /// rustc-link-arg (which would not propagate through a binding rlib). Consumers
    /// needing custom linker flags can use link_arguments at their final link.
    pub fn cargo_directives(&self) -> Result<Vec<String>> {
        let mut result = Vec::new();
        let kind = match self.linkage {
            PackageLinkage::Static => "static",
            PackageLinkage::Dynamic => "dylib",
        };
        let mut arguments = self.link_arguments.iter();
        while let Some(argument) = arguments.next() {
            let directive = if let Some(path) = argument
                .strip_prefix("-L")
                .or_else(|| argument.strip_prefix("/LIBPATH:"))
            {
                format!("cargo:rustc-link-search=native={path}")
            } else if argument == "-framework" {
                format!(
                    "cargo:rustc-link-lib=framework={}",
                    arguments
                        .next()
                        .ok_or_else(|| invalid("missing framework name"))?
                )
            } else if let Some(name) = argument.strip_prefix("-l") {
                format!("cargo:rustc-link-lib={name}")
            } else if argument == "-pthread" {
                "cargo:rustc-link-lib=pthread".into()
            } else {
                let path = Path::new(argument);
                if path.is_absolute() {
                    let name = path
                        .file_name()
                        .and_then(|value| value.to_str())
                        .ok_or_else(|| invalid("non-UTF-8 native library filename"))?;
                    let extension = path
                        .extension()
                        .and_then(|value| value.to_str())
                        .unwrap_or("");
                    let shared = matches!(extension, "so" | "dylib")
                        || name.split_once(".so.").is_some_and(|(_, suffix)| {
                            !suffix.is_empty()
                                && suffix
                                    .bytes()
                                    .all(|byte| byte.is_ascii_digit() || byte == b'.')
                        });
                    if !matches!(extension, "a" | "lib") && !shared {
                        return Err(invalid(format!(
                            "unsupported native link input {argument:?}; consume Installation::link_arguments at the final linker"
                        )));
                    }
                    result.push(format!(
                        "cargo:rustc-link-search=native={}",
                        path.parent().expect("absolute library").display()
                    ));
                    let kind = if extension == "a" {
                        "static"
                    } else if shared {
                        "dylib"
                    } else {
                        kind
                    };
                    format!("cargo:rustc-link-lib={kind}:+verbatim={name}")
                } else if argument.ends_with(".lib") && !argument.contains(['/', '\\']) {
                    format!("cargo:rustc-link-lib={}", argument.trim_end_matches(".lib"))
                } else {
                    return Err(invalid(format!(
                        "unsupported native linker argument {argument:?}; consume Installation::link_arguments at the final linker"
                    )));
                }
            };
            if directive.contains(['\n', '\r']) {
                return Err(invalid("newline in native linker metadata"));
            }
            result.push(directive);
        }
        if result
            .iter()
            .any(|directive| directive.contains(['\n', '\r']))
        {
            return Err(invalid("newline in native linker metadata"));
        }
        Ok(result)
    }

    /// Emit only after the entire set of link directives has been validated.
    pub fn emit_cargo(&self) -> Result<()> {
        for directive in self.cargo_directives()? {
            println!("{directive}");
        }
        Ok(())
    }
}

#[cfg(any(feature = "vcpkg", feature = "conan"))]
fn output(command: &mut Command, what: &str) -> Result<String> {
    let result = command.output().map_err(|error| Error::io(what, error))?;
    if !result.status.success() {
        return Err(Error::Tool {
            what: what.into(),
            detail: format!(
                "{}: {}",
                result.status,
                String::from_utf8_lossy(&result.stderr)
            ),
        });
    }
    String::from_utf8(result.stdout).map_err(|error| Error::Tool {
        what: what.into(),
        detail: error.to_string(),
    })
}

fn check(command: &mut Command, what: &str) -> Result<()> {
    crate::process::check(command, what)
}
fn write(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text)
        .map_err(|error| Error::io(format!("write {}", path.display()), error))
}
fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .map_err(|error| Error::io(format!("read {}", path.display()), error))
}
fn json_read(path: &Path) -> Result<Value> {
    serde_json::from_str(&read(path)?).map_err(|error| invalid(error.to_string()))
}
#[cfg(feature = "vcpkg")]
fn json_write(path: &Path, data: &impl Serialize) -> Result<()> {
    write(
        path,
        &serde_json::to_string_pretty(data).map_err(|error| invalid(error.to_string()))?,
    )
}

#[cfg(feature = "vcpkg")]
fn vcpkg_root() -> Result<PathBuf> {
    for variable in [
        "VCPKG_OVERLAY_PORTS",
        "VCPKG_OVERLAY_TRIPLETS",
        "VCPKG_CHAINLOAD_TOOLCHAIN_FILE",
    ] {
        if std::env::var_os(variable).is_some() {
            return Err(invalid(format!(
                "{variable} is not supported by the locked vcpkg adapter; use immutable builtin registry/triplet inputs"
            )));
        }
    }
    std::env::var_os("VCPKG_ROOT").map(PathBuf::from).ok_or_else(|| invalid("vcpkg provider requires VCPKG_ROOT; Woodshed does not modify or bootstrap a shared vcpkg installation"))
}

#[cfg(feature = "vcpkg")]
fn vcpkg_exe(root: &Path) -> PathBuf {
    root.join(if cfg!(windows) { "vcpkg.exe" } else { "vcpkg" })
}

#[cfg(feature = "vcpkg")]
fn validate_vcpkg_root(root: &Path) -> Result<()> {
    check(
        Command::new("git").arg("-C").arg(root).args([
            "diff", "--quiet", "HEAD", "--", "scripts", "triplets", "ports", "versions",
        ]),
        "validate clean vcpkg inputs",
    )
}

#[cfg(feature = "conan")]
fn conan_exe() -> OsString {
    std::env::var_os("CONAN").unwrap_or_else(|| "conan".into())
}

#[cfg(feature = "conan")]
fn conan_args(manifest: &Path, package: &Package) -> Result<Vec<OsString>> {
    let Package::Conan {
        package,
        host_profile,
        build_profile,
        options,
        ..
    } = package
    else {
        return Err(invalid("expected Conan package"));
    };
    let parent = manifest.parent().unwrap_or(Path::new("."));
    let mut args = vec![
        "--requires".into(),
        package.into(),
        "--profile:host".into(),
        parent
            .join(host_profile)
            .canonicalize()
            .map_err(|error| Error::io("locate Conan host profile", error))?
            .into(),
        "--profile:build".into(),
        parent
            .join(build_profile)
            .canonicalize()
            .map_err(|error| Error::io("locate Conan build profile", error))?
            .into(),
    ];
    for option in options {
        args.push("--options".into());
        args.push(option.into());
    }
    Ok(args)
}

#[cfg(all(feature = "pin", any(feature = "vcpkg", feature = "conan")))]
pub(super) fn resolve(manifest: &Path, package: &Package) -> Result<PackageLock> {
    if std::env::var("WOODSHED_OFFLINE").is_ok_and(|value| value == "1") {
        return Err(Error::Offline {
            detail:
                "provider lock resolution is explicit online maintenance; reuse a matching lock"
                    .into(),
        });
    }
    let data = match package {
        #[cfg(feature = "vcpkg")]
        Package::Vcpkg {
            package,
            version,
            triplet,
            baseline,
            features,
            default_features,
            ..
        } => {
            let root = vcpkg_root()?;
            validate_vcpkg_root(&root)?;
            let root_revision = output(
                Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(["rev-parse", "HEAD"]),
                "read vcpkg checkout revision",
            )?
            .trim()
            .to_owned();
            crate::dep::validate_revision(&root_revision)?;
            let tool = output(
                Command::new(vcpkg_exe(&root)).arg("version"),
                "read vcpkg version",
            )?;
            let baseline = baseline.as_ref().unwrap_or(&root_revision);
            let manifest = json!({
                "builtin-baseline": baseline, "dependencies": [{"name": package, "features": features, "default-features": default_features}],
                "overrides": [{"name": package, "version": version}],
            });
            let scratch = tempfile::tempdir()
                .map_err(|error| Error::io("stage vcpkg lock resolution", error))?;
            json_write(&scratch.path().join("vcpkg.json"), &manifest)?;
            check(
                Command::new(vcpkg_exe(&root))
                    .arg("install")
                    .arg("--dry-run")
                    .arg(format!("--triplet={triplet}"))
                    .arg(format!("--x-manifest-root={}", scratch.path().display()))
                    .arg(format!(
                        "--x-install-root={}",
                        scratch.path().join("installed").display()
                    )),
                "validate pinned vcpkg graph",
            )?;
            Data::Vcpkg {
                manifest,
                tool,
                root_revision,
            }
        }
        #[cfg(feature = "conan")]
        Package::Conan { .. } => {
            let tool = output(
                Command::new(conan_exe()).arg("--version"),
                "read Conan version",
            )?;
            if !tool.trim().starts_with("Conan version 2.") {
                return Err(invalid("Conan provider requires Conan 2"));
            }
            let scratch = tempfile::tempdir()
                .map_err(|error| Error::io("stage Conan lock resolution", error))?;
            let path = scratch.path().join("conan.lock");
            check(
                Command::new(conan_exe())
                    .args(["lock", "create"])
                    .args(conan_args(manifest, package)?)
                    .arg("--lockfile=")
                    .arg("--lockfile-out")
                    .arg(&path),
                "resolve Conan lock",
            )?;
            let lock = read(&path)?;
            let _: Value = serde_json::from_str(&lock)
                .map_err(|error| invalid(format!("invalid Conan lock: {error}")))?;
            Data::Conan { lock, tool }
        }
        #[allow(unreachable_patterns)]
        _ => return Err(disabled(package)),
    };
    Ok(PackageLock {
        input: package.input(manifest)?,
        data: serde_json::to_string(&data).map_err(|error| invalid(error.to_string()))?,
    })
}

/// Install a locked package graph and discover its evaluated CMake link metadata.
///
/// Configuration comes from the declared provider only. Source/system fallback
/// and host-profile inference are deliberately absent.
#[cfg(any(feature = "vcpkg", feature = "conan"))]
pub fn install(manifest: &Path, name: &str, options: &InstallOptions) -> Result<Installation> {
    let absolute_options = InstallOptions {
        directory: std::path::absolute(&options.directory)
            .map_err(|error| Error::io("resolve provider output directory", error))?,
        target: options.target.clone(),
        configuration: options.configuration.clone(),
    };
    let options = &absolute_options;
    let project = Project::load(manifest)?;
    let lock = project.locked()?;
    let package = match project.declarations.dependencies.get(name) {
        Some(Dependency::Package(package)) => package,
        _ => {
            return Err(invalid(format!(
                "{name} is not a package-provider dependency"
            )));
        }
    };
    if std::env::var_os("OUT_DIR").is_some() {
        println!("cargo:rerun-if-changed={}", project.path.display());
        println!("cargo:rerun-if-changed={}", project.lock_path().display());
        if let Package::Conan {
            host_profile,
            build_profile,
            ..
        } = package
        {
            for path in [host_profile, build_profile] {
                println!(
                    "cargo:rerun-if-changed={}",
                    manifest
                        .parent()
                        .unwrap_or(Path::new("."))
                        .join(path)
                        .display()
                );
            }
        }
        for name in [
            "VCPKG_ROOT",
            "CONAN",
            "WOODSHED_OFFLINE",
            "CMAKE_GENERATOR",
            "CMAKE_GENERATOR_PLATFORM",
            "CMAKE_GENERATOR_TOOLSET",
            "CC",
            "CXX",
        ] {
            println!("cargo:rerun-if-env-changed={name}");
        }
    }
    if options.target.is_empty()
        || !matches!(
            options.configuration.as_str(),
            "Release" | "Debug" | "RelWithDebInfo" | "MinSizeRel"
        )
    {
        return Err(invalid(
            "provider installation requires an explicit target and standard CMake configuration",
        ));
    }
    if let Ok(target) = std::env::var("TARGET")
        && target != options.target
    {
        return Err(invalid(format!(
            "provider target {} does not match Cargo TARGET {target}",
            options.target
        )));
    }
    std::fs::create_dir_all(&options.directory)
        .map_err(|error| Error::io("create provider installation", error))?;
    let _guard = crate::cache::Lock::acquire(&options.directory.join("provider.lock"))?;
    let data: Data = serde_json::from_str(&lock.packages[name].data)
        .map_err(|error| invalid(format!("invalid provider lock: {error}")))?;
    if !matches!(
        (package, &data),
        (Package::Vcpkg { .. }, Data::Vcpkg { .. }) | (Package::Conan { .. }, Data::Conan { .. })
    ) {
        return Err(invalid(
            "locked provider kind does not match the selected package",
        ));
    }
    let mut defines = Vec::new();
    let mut runtime_directories: Vec<PathBuf> = Vec::new();
    match (package, data) {
        #[cfg(feature = "vcpkg")]
        (
            Package::Vcpkg { triplet, .. },
            Data::Vcpkg {
                manifest,
                tool,
                root_revision,
            },
        ) => {
            let root = vcpkg_root()?;
            let actual_revision = output(
                Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(["rev-parse", "HEAD"]),
                "validate vcpkg checkout",
            )?;
            if actual_revision.trim() != root_revision
                || output(
                    Command::new(vcpkg_exe(&root)).arg("version"),
                    "validate vcpkg tool",
                )? != tool
            {
                return Err(invalid(
                    "vcpkg tool/root changed since locking; regenerate the lock explicitly",
                ));
            }
            validate_vcpkg_root(&root)?;
            validate_triplet(triplet, &options.target)?;
            json_write(&options.directory.join("vcpkg.json"), &manifest)?;
            let installed = options.directory.join("installed");
            let mut command = Command::new(vcpkg_exe(&root));
            command
                .arg("install")
                .arg(format!("--triplet={triplet}"))
                .arg(format!("--x-manifest-root={}", options.directory.display()))
                .arg(format!("--x-install-root={}", installed.display()))
                .arg(format!(
                    "--x-buildtrees-root={}",
                    options.directory.join("buildtrees").display()
                ))
                .arg(format!(
                    "--x-packages-root={}",
                    options.directory.join("packages").display()
                ));
            if offline() {
                command.arg("--no-downloads");
            }
            check(&mut command, "install locked vcpkg graph")?;
            defines.push((
                "CMAKE_TOOLCHAIN_FILE".into(),
                root.join("scripts")
                    .join("buildsystems")
                    .join("vcpkg.cmake")
                    .into_os_string(),
            ));
            defines.push(("VCPKG_TARGET_TRIPLET".into(), triplet.into()));
            defines.push((
                "VCPKG_INSTALLED_DIR".into(),
                installed.clone().into_os_string(),
            ));
            defines.push(("VCPKG_MANIFEST_INSTALL".into(), "OFF".into()));
            defines.push((
                "WOODSHED_PACKAGE_PREFIX".into(),
                installed.join(triplet).into_os_string(),
            ));
            let runtime = installed.join(triplet);
            runtime_directories.push(if options.configuration == "Debug" {
                runtime.join("debug").join("bin")
            } else {
                runtime.join("bin")
            });
        }
        #[cfg(feature = "conan")]
        (Package::Conan { build_missing, .. }, Data::Conan { lock, tool }) => {
            if output(
                Command::new(conan_exe()).arg("--version"),
                "validate Conan tool",
            )? != tool
            {
                return Err(invalid(
                    "Conan version changed since locking; regenerate the lock explicitly",
                ));
            }
            let path = options.directory.join("conan.lock");
            write(&path, &lock)?;
            let graph = options.directory.join("graph.json");
            let mut command = Command::new(conan_exe());
            command
                .arg("install")
                .args(conan_args(manifest, package)?)
                .arg("--lockfile")
                .arg(path)
                .arg("--output-folder")
                .arg(&options.directory)
                .args(["--generator", "CMakeDeps", "--generator", "CMakeToolchain"])
                .arg("--settings:host")
                .arg(format!("build_type={}", options.configuration))
                .arg(if *build_missing {
                    "--build=missing"
                } else {
                    "--build=never"
                })
                .args(["--format=json", "--out-file"])
                .arg(&graph);
            if offline() {
                command.arg("--no-remote");
            }
            check(&mut command, "install locked Conan graph")?;
            let graph = json_read(&graph)?;
            validate_conan_target(&graph, &options.target)?;
            for node in graph["graph"]["nodes"]
                .as_object()
                .ok_or_else(|| invalid("missing Conan graph nodes"))?
                .values()
            {
                if node["context"].as_str() != Some("host") {
                    continue;
                }
                if let (Some(folder), Some(components)) = (
                    node["package_folder"].as_str(),
                    node["cpp_info"].as_object(),
                ) {
                    for component in components.values() {
                        for field in ["bindirs", "libdirs"] {
                            if let Some(directories) = component[field].as_array() {
                                for directory in directories {
                                    runtime_directories.push(Path::new(folder).join(
                                        directory.as_str().ok_or_else(|| {
                                            invalid("invalid Conan runtime directory")
                                        })?,
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            defines.push((
                "CMAKE_TOOLCHAIN_FILE".into(),
                options
                    .directory
                    .join("conan_toolchain.cmake")
                    .into_os_string(),
            ));
            defines.push((
                "WOODSHED_PACKAGE_PREFIX".into(),
                options.directory.clone().into_os_string(),
            ));
        }
        #[allow(unreachable_patterns)]
        _ => return Err(disabled(package)),
    }
    let mut identity = crate::BuildKey::new("package-metadata")
        .mix(&lock.packages[name].data)
        .mix(&lock.packages[name].input)
        .mix(&options.target)
        .mix(&options.configuration);
    for (key, value) in &defines {
        identity = identity.mix(key).mix(value.as_encoded_bytes());
    }
    for name in [
        "CMAKE_GENERATOR",
        "CMAKE_GENERATOR_PLATFORM",
        "CMAKE_GENERATOR_TOOLSET",
        "CC",
        "CXX",
        "CARGO_CFG_TARGET_FEATURE",
    ] {
        if let Some(value) = std::env::var_os(name) {
            identity = identity.mix(name).mix(value.as_encoded_bytes());
        }
    }
    let mut installation = discover(package.cmake(), options, &defines, &identity.as_str())?;
    runtime_directories.retain(|path| path.is_dir());
    runtime_directories.sort();
    runtime_directories.dedup();
    installation.runtime_directories = runtime_directories;
    Ok(installation)
}

/// Report a disabled provider without falling back to an unrelated source build.
#[cfg(not(any(feature = "vcpkg", feature = "conan")))]
pub fn install(manifest: &Path, name: &str, _: &InstallOptions) -> Result<Installation> {
    let project = Project::load(manifest)?;
    project.locked()?;
    match project.declarations.dependencies.get(name) {
        Some(Dependency::Package(package)) => Err(disabled(package)),
        _ => Err(invalid(format!("{name}: not a declared package provider"))),
    }
}

#[cfg(any(feature = "vcpkg", feature = "conan"))]
fn offline() -> bool {
    std::env::var("WOODSHED_OFFLINE").is_ok_and(|value| value == "1")
}

#[cfg(feature = "vcpkg")]
fn validate_triplet(triplet: &str, target: &str) -> Result<()> {
    let architecture = if target.starts_with("x86_64-") {
        "x64"
    } else if target.starts_with("aarch64-") {
        "arm64"
    } else if target.starts_with("i686-") {
        "x86"
    } else if target.starts_with("armv7-") {
        "arm"
    } else {
        return Err(invalid(format!(
            "unsupported vcpkg target architecture {target}; no host fallback"
        )));
    };
    let platform = if target.contains("-windows-") {
        "windows"
    } else if target.contains("-linux-") {
        "linux"
    } else if target.contains("-apple-darwin") {
        "osx"
    } else {
        return Err(invalid(format!(
            "unsupported vcpkg target platform {target}"
        )));
    };
    if platform == "windows" && !target.ends_with("-msvc") {
        return Err(invalid(
            "this vcpkg adapter supports MSVC Windows triplets, not a GNU target with an MSVC package",
        ));
    }
    if platform == "linux"
        && !["-gnu", "-gnueabi", "-gnueabihf"]
            .iter()
            .any(|suffix| target.ends_with(suffix))
    {
        return Err(invalid(
            "the builtin Linux triplet expects glibc; custom musl/Android ABI profiles are not inferred",
        ));
    }
    let base = format!("{architecture}-{platform}");
    if triplet != base
        && !(platform == "windows"
            && (triplet == format!("{base}-static") || triplet == format!("{base}-static-md")))
    {
        return Err(invalid(format!(
            "vcpkg triplet {triplet} is unsupported or does not match target {target}; custom triplet ABI validation is not supported"
        )));
    }
    if target.ends_with("-msvc") {
        let static_crt = crate::crt_static();
        let triplet_static_crt = triplet.ends_with("-static");
        if static_crt != triplet_static_crt {
            return Err(invalid(
                "vcpkg triplet CRT does not match Rust crt-static; use -static or -static-md consistently",
            ));
        }
    }
    Ok(())
}

#[cfg(feature = "conan")]
fn validate_conan_target(graph: &Value, target: &str) -> Result<()> {
    let settings = &graph["graph"]["nodes"]["0"]["settings"];
    let architecture = if target.starts_with("x86_64-") {
        "x86_64"
    } else if target.starts_with("aarch64-") {
        "armv8"
    } else if target.starts_with("i686-") {
        "x86"
    } else {
        return Err(invalid("unsupported Conan target architecture"));
    };
    let platform = if target.contains("-windows-") {
        "Windows"
    } else if target.contains("-linux-") {
        "Linux"
    } else if target.contains("-apple-darwin") {
        "Macos"
    } else {
        return Err(invalid("unsupported Conan target platform"));
    };
    if settings["arch"].as_str() != Some(architecture) || settings["os"].as_str() != Some(platform)
    {
        return Err(invalid(format!(
            "Conan host profile does not match Rust target {target}"
        )));
    }
    if target.ends_with("-msvc")
        && settings["compiler.runtime"].as_str()
            != Some(if crate::crt_static() {
                "static"
            } else {
                "dynamic"
            })
    {
        return Err(invalid(
            "Conan compiler.runtime does not match Rust CRT linkage",
        ));
    }
    Ok(())
}

pub(crate) fn discover(
    package: &CmakePackage,
    options: &InstallOptions,
    defines: &[(String, OsString)],
    identity: &str,
) -> Result<Installation> {
    let source = options.directory.join("consumer");
    let build = options.directory.join(identity);
    std::fs::create_dir_all(&source)
        .map_err(|error| Error::io("create package metadata consumer", error))?;
    let query = build.join(".cmake").join("api").join("v1").join("query");
    std::fs::create_dir_all(&query)
        .map_err(|error| Error::io("create CMake File API query", error))?;
    write(&query.join("codemodel-v2"), "")?;
    write(&source.join("main.cpp"), "int main() { return 0; }\n")?;
    let text = format!(
        "cmake_minimum_required(VERSION 3.21)\nproject(woodshed_package LANGUAGES C CXX)\nfile(TO_CMAKE_PATH \"${{WOODSHED_PACKAGE_PREFIX}}\" WOODSHED_PACKAGE_PREFIX)\nfind_package({} CONFIG REQUIRED PATHS \"${{WOODSHED_PACKAGE_PREFIX}}\" NO_DEFAULT_PATH)\nadd_executable(woodshed_metadata main.cpp)\ntarget_link_libraries(woodshed_metadata PRIVATE {})\n",
        package.package,
        package.targets.join(" ")
    );
    write(&source.join("CMakeLists.txt"), &text)?;
    let mut command = Command::new("cmake");
    command
        .arg("-S")
        .arg(&source)
        .arg("-B")
        .arg(&build)
        .arg(format!("-DCMAKE_BUILD_TYPE={}", options.configuration))
        .arg("-DCMAKE_FIND_USE_PACKAGE_REGISTRY=OFF")
        .arg("-DCMAKE_FIND_USE_SYSTEM_PACKAGE_REGISTRY=OFF");
    for (key, value) in defines {
        let mut define = OsString::from(format!("-D{key}="));
        define.push(value);
        command.arg(define);
    }
    check(&mut command, "evaluate package imported targets")?;
    let reply = build.join(".cmake").join("api").join("v1").join("reply");
    let mut indices = std::fs::read_dir(&reply)
        .map_err(|error| Error::io("read CMake reply", error))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| Error::io("list CMake reply", error))?;
    indices.retain(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("index-"))
    });
    indices.sort();
    let index = json_read(
        indices
            .last()
            .ok_or_else(|| invalid("CMake did not report a File API index"))?,
    )?;
    let model = json_read(
        &reply.join(
            index["reply"]["codemodel-v2"]["jsonFile"]
                .as_str()
                .ok_or_else(|| invalid("missing CMake codemodel"))?,
        ),
    )?;
    let configurations = model["configurations"]
        .as_array()
        .ok_or_else(|| invalid("missing CMake configurations"))?;
    let configuration = configurations
        .iter()
        .find(|value| value["name"].as_str() == Some(&options.configuration))
        .ok_or_else(|| invalid("CMake did not report the requested build configuration"))?;
    let target = configuration["targets"]
        .as_array()
        .and_then(|values| {
            values
                .iter()
                .find(|value| value["name"] == "woodshed_metadata")
        })
        .ok_or_else(|| invalid("CMake did not report the package metadata target"))?;
    let target = json_read(
        &reply.join(
            target["jsonFile"]
                .as_str()
                .ok_or_else(|| invalid("missing CMake target file"))?,
        ),
    )?;
    metadata(
        &target,
        package.linkage,
        options.target.contains("-windows-"),
    )
}

fn metadata(target: &Value, linkage: PackageLinkage, windows: bool) -> Result<Installation> {
    let mut includes = BTreeSet::new();
    let mut definitions = BTreeSet::new();
    let mut compile_flags = Vec::new();
    for group in target["compileGroups"]
        .as_array()
        .ok_or_else(|| invalid("missing CMake compile metadata"))?
    {
        if let Some(values) = group["includes"].as_array() {
            for value in values {
                includes.insert(PathBuf::from(
                    value["path"]
                        .as_str()
                        .ok_or_else(|| invalid("invalid include directory"))?,
                ));
            }
        }
        if let Some(values) = group["defines"].as_array() {
            for value in values {
                definitions.insert(
                    value["define"]
                        .as_str()
                        .ok_or_else(|| invalid("invalid definition"))?
                        .to_owned(),
                );
            }
        }
        if let Some(values) = group["compileCommandFragments"].as_array() {
            for value in values {
                compile_flags.extend(tokens(
                    value["fragment"]
                        .as_str()
                        .ok_or_else(|| invalid("invalid compile fragment"))?,
                    windows,
                )?);
            }
        }
    }
    let mut link_arguments = Vec::new();
    for fragment in target["link"]["commandFragments"]
        .as_array()
        .ok_or_else(|| invalid("missing CMake link metadata"))?
    {
        let role = fragment["role"]
            .as_str()
            .ok_or_else(|| invalid("invalid linker role"))?;
        if role != "flags" || fragment.get("backtrace").is_some() {
            link_arguments.extend(tokens(
                fragment["fragment"]
                    .as_str()
                    .ok_or_else(|| invalid("invalid linker fragment"))?,
                windows,
            )?);
        }
    }
    Ok(Installation {
        include_dirs: includes.into_iter().collect(),
        definitions: definitions.into_iter().collect(),
        compile_flags,
        link_arguments,
        runtime_directories: Vec::new(),
        linkage,
    })
}

fn tokens(fragment: &str, windows: bool) -> Result<Vec<String>> {
    // Windows paths cannot contain literal quotes. Preserve their backslashes
    // while using CMake's shell-style quoted token boundaries.
    let escaped = if windows {
        fragment.replace('\\', "\\\\")
    } else {
        fragment.into()
    };
    shlex::split(&escaped)
        .ok_or_else(|| invalid(format!("cannot tokenize CMake link fragment {fragment:?}")))
}

#[cfg(test)]
mod tests;
