use crate::{
    BuildKey, Cache, CmakeBuild, Error, Linkage, Result,
    project::{BuildRole, Dependency, PackageLinkage, PlanOptions, Project, invalid},
    providers::{InstallOptions, Installation},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

mod diagnostics;
mod management;
mod portable;
pub use management::{ArtifactSummary, PruneReport};
pub use portable::ArtifactMetadata;
mod source;
mod storage;
pub use diagnostics::{BuildExplanation, BuildOutcome};

const ENVIRONMENT: &[&str] = &[
    "CC",
    "CXX",
    "AR",
    "LD",
    "RANLIB",
    "RC",
    "CFLAGS",
    "CXXFLAGS",
    "CPPFLAGS",
    "SOURCE_DATE_EPOCH",
    "LDFLAGS",
    "CMAKE_GENERATOR",
    "CMAKE_GENERATOR_PLATFORM",
    "CMAKE_GENERATOR_TOOLSET",
    "PATH",
    "INCLUDE",
    "LIB",
    "LIBPATH",
    "SDKROOT",
    "MACOSX_DEPLOYMENT_TARGET",
    "VCToolsInstallDir",
    "WindowsSdkDir",
    "WindowsSDKVersion",
    "CPATH",
    "C_INCLUDE_PATH",
    "CPLUS_INCLUDE_PATH",
    "LIBRARY_PATH",
    "LD_LIBRARY_PATH",
    "DYLD_LIBRARY_PATH",
    "PKG_CONFIG_PATH",
    "PKG_CONFIG_LIBDIR",
    "PKG_CONFIG_SYSROOT_DIR",
    "CMAKE_PREFIX_PATH",
    "CMAKE_LIBRARY_PATH",
    "CMAKE_INCLUDE_PATH",
    "CMAKE_FRAMEWORK_PATH",
    "CMAKE_APPBUNDLE_PATH",
    "CMAKE_OSX_ARCHITECTURES",
    "CMAKE_OSX_SYSROOT",
    "CMAKE_GENERATOR_INSTANCE",
    "CMAKE_TOOLCHAIN_FILE",
    "CL",
    "_CL_",
    "LINK",
    "_LINK_",
    "VSINSTALLDIR",
    "UCRTVersion",
    "UniversalCRTSdkDir",
];

/// Build the locked declarations beside Cargo.toml (or under native/).
///
/// The `build` feature is required. Each source needs an explicit `build`
/// recipe; package providers need their corresponding feature. No resolution
/// or lock repair occurs during a Cargo build.
pub fn build() -> Result<()> {
    let root = std::env::var_os("CARGO_MANIFEST_DIR")
        .ok_or_else(|| invalid("woodshed::build() must run in a Cargo build script"))?;
    let root = PathBuf::from(root);
    let path = if root.join("woodshed.toml").is_file() {
        root.join("woodshed.toml")
    } else {
        root.join("native").join("woodshed.toml")
    };
    build_from(&path).map(|_| ())
}

/// Build a selected declaration file and return evaluated per-dependency metadata.
///
/// Emits Cargo link directives only after every dependency has valid metadata.
/// Native compiler flags/includes are returned for binding generation; they are
/// not automatically passed to Rust. Selected shared runtime libraries are
/// staged in Cargo's OUT_DIR for cargo run/test and woodshed bundle. Additional
/// runtime resources and undeclared operating-system dependencies remain explicit.
pub fn build_from(path: &Path) -> Result<BTreeMap<String, Installation>> {
    let target = std::env::var("TARGET")
        .map_err(|_| invalid("TARGET is required in a Cargo build script"))?;
    let host =
        std::env::var("HOST").map_err(|_| invalid("HOST is required in a Cargo build script"))?;
    let mut options = PlanOptions::new(host, target);
    let mut cfg = Vec::new();
    for (variable, value) in std::env::vars_os() {
        let Some(name) = variable
            .to_str()
            .and_then(|name| name.strip_prefix("CARGO_CFG_"))
        else {
            continue;
        };
        let name = name.to_lowercase();
        if name == "feature" {
            continue;
        }
        let value = value
            .into_string()
            .map_err(|_| invalid(format!("CARGO_CFG_{} must be UTF-8", name.to_uppercase())))?;
        if value.is_empty() {
            cfg.push(name);
        } else {
            cfg.extend(value.split(',').map(|value| format!("{name}={value:?}")));
        }
    }
    if !cfg.is_empty() {
        options.target_cfg = Some(cfg);
    }
    let project = Project::load(path)?;
    if let Some(directory) = std::env::var_os("CARGO_MANIFEST_DIR") {
        let cargo = std::fs::read_to_string(Path::new(&directory).join("Cargo.toml"))
            .map_err(|error| Error::io("read Cargo feature declarations", error))?;
        let cargo: toml::Value = toml::from_str(&cargo)
            .map_err(|error| invalid(format!("invalid Cargo.toml: {error}")))?;
        if cargo
            .get("features")
            .and_then(|features| features.get("default"))
            .is_some()
        {
            options.default_features = false;
        }
    }
    for feature in project.declarations.feature_names() {
        let variable = format!("CARGO_FEATURE_{}", feature.replace('-', "_").to_uppercase());
        if std::env::var_os(variable).is_some() {
            options.features.push(feature);
        }
    }
    build_from_with_options(path, &options)
}

/// Build an explicit feature/host/target selection from an immutable lock.
///
/// Only target-role outputs emit Rust link directives. Host-role prerequisites
/// are exposed to CMake via CMAKE_PROGRAM_PATH, never linked into the target.
pub fn build_from_with_options(
    path: &Path,
    selection: &PlanOptions,
) -> Result<BTreeMap<String, Installation>> {
    let path =
        std::path::absolute(path).map_err(|error| Error::io("resolve declaration path", error))?;
    let project = Project::load(&path)?;
    let lock = project.locked()?;
    println!("cargo:rerun-if-changed={}", path.display());
    println!("cargo:rerun-if-changed={}", project.lock_path().display());
    for workspace in project.workspace_inputs.keys() {
        println!("cargo:rerun-if-changed={}", workspace.display());
    }
    for variable in ENVIRONMENT {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    let plan = project.resolve(selection)?;
    let host = &selection.host;
    let cache = Cache::discover()?;
    let _activity = crate::cache::Lock::acquire_shared(&cache.root().join("activity.lock"))?;
    let mut installations: BTreeMap<String, Installation> = BTreeMap::new();
    let mut identities = BTreeMap::<String, String>::new();
    let declarations = project.declarations.all_dependencies();
    for (index, node) in plan.dependencies.iter().enumerate() {
        let _progress = crate::progress::Phase::new(format!(
            "dependency {}/{}: {}",
            index + 1,
            plan.dependencies.len(),
            node.id
        ));
        let name = &node.name;
        let target = &node.target;
        let dependency = declarations[&node.id];
        let mut prefixes = Vec::new();
        let mut programs = Vec::new();
        let mut prerequisites = BTreeMap::new();
        let mut host_tools = BTreeMap::new();
        let mut cmake_packages = BTreeMap::new();
        for id in &node.depends {
            let prior_node = plan
                .dependencies
                .iter()
                .find(|prior| &prior.id == id)
                .expect("resolved prerequisite");
            let installation = &installations[&prior_node.name];
            if prior_node.role == node.role {
                for (package, path) in &installation.cmake_packages {
                    if let Some(prior) = cmake_packages.insert(package.clone(), path.clone())
                        && prior != *path
                    {
                        return Err(invalid(format!(
                            "conflicting selected CMake package {package}: {} and {}",
                            prior.display(),
                            path.display()
                        )));
                    }
                }
                prerequisites.insert(
                    prior_node.name.clone(),
                    identities[&prior_node.name].clone(),
                );
                prefixes.extend(installation.prefixes.clone());
            } else {
                host_tools.insert(
                    prior_node.name.clone(),
                    identities[&prior_node.name].clone(),
                );
                programs.extend(
                    installation
                        .prefixes
                        .iter()
                        .map(|prefix| prefix.join("bin")),
                );
            }
        }
        let installation_id;
        let mut installation = match dependency {
            Dependency::Package(_) => {
                installation_id = BuildKey::explicit(name)
                    .mix(&lock.packages[&node.id].input)
                    .mix(&lock.packages[&node.id].data)
                    .mix(target)
                    .mix(
                        serde_json::to_vec(&prerequisites)
                            .map_err(|error| invalid(error.to_string()))?,
                    )
                    .fingerprint();
                let options = InstallOptions {
                    directory: cache.root().join("providers").join(&installation_id),
                    target: target.clone(),
                    configuration: "Release".into(),
                };
                crate::providers::install(&path, &node.id, &options)?
            }
            Dependency::Source(source) => {
                let request = source::Request {
                    cache: &cache,
                    manifest: &path,
                    host,
                    node,
                    source,
                    pinned: lock.sources.get(&node.id),
                    dependencies: &prerequisites,
                    host_tools: &host_tools,
                    cmake_packages: &cmake_packages,
                    prefixes: &prefixes,
                    programs: &programs,
                };
                let result = if source.prebuilt.is_some() {
                    portable::install(request)?
                } else {
                    source::install(request)?
                };
                installation_id = result.0;
                result.1
            }
        };
        for prefix in prefixes {
            if !installation.prefixes.contains(&prefix) {
                installation.prefixes.push(prefix);
            }
        }
        // Validate the entire graph before emitting any success-shaped link output.
        if node.role == BuildRole::Target {
            installation.cargo_directives()?;
        }
        let identity = BuildKey::explicit(name)
            .mix(installation_id)
            .mix(serde_json::to_vec(&installation).map_err(|error| invalid(error.to_string()))?)
            .fingerprint();
        identities.insert(name.clone(), identity);
        installations.insert(name.clone(), installation);
    }
    // Link dependents before prerequisites. Within each package retain CMake's
    // evaluated link order rather than alphabetically sorting linker inputs.
    let mut groups = Vec::new();
    let mut native_files = BTreeMap::new();
    for node in plan
        .dependencies
        .iter()
        .rev()
        .filter(|node| node.role == BuildRole::Target)
    {
        for argument in &installations[&node.name].link_arguments {
            let path = Path::new(argument);
            if !path.is_absolute() {
                continue;
            }
            let canonical = path.canonicalize().map_err(|error| {
                Error::io(
                    format!("resolve native link input {}", path.display()),
                    error,
                )
            })?;
            let mut filename = path
                .file_name()
                .ok_or_else(|| invalid("native link input has no filename"))?
                .to_string_lossy()
                .into_owned();
            if cfg!(windows) {
                filename.make_ascii_lowercase();
            }
            if let Some(existing) = native_files.insert(filename.clone(), canonical.clone())
                && existing != canonical
            {
                return Err(invalid(format!(
                    "native library {filename} resolves to conflicting files {} and {}; select one dependency version/linkage",
                    existing.display(),
                    canonical.display()
                )));
            }
        }
        groups.push(installations[&node.name].cargo_directives()?);
    }
    let directives = merge_link_directives(&groups)?;
    if let Some(out) = std::env::var_os("OUT_DIR") {
        let runtime = plan
            .dependencies
            .iter()
            .filter(|node| node.role == BuildRole::Target)
            .flat_map(|node| installations[&node.name].runtime_directories.iter())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let staged = stage_runtime(&cache, Path::new(&out), &runtime)?;
        if let Some(staged) = staged {
            println!("cargo:rustc-link-search=native={}", staged.display());
            println!(
                "cargo:rustc-env=WOODSHED_DEPLOYMENT_DIR={}",
                staged.display()
            );
        }
    }
    for directive in directives {
        println!("{directive}");
    }
    // The exported C++ target graph can rely on the link language's implicit
    // runtime rather than listing it as an explicit CMake library argument.
    if plan.dependencies.iter().any(|node| {
        node.role == BuildRole::Target && matches!(declarations[&node.id], Dependency::Source(source) if source.build.as_ref().is_some_and(|recipe| matches!(recipe.linkage, PackageLinkage::Static)))
    }) {
        crate::link_cpp_runtime_for(Linkage::Static);
    }
    Ok(installations)
}

fn inventory(prefix: &Path) -> Result<BTreeMap<PathBuf, String>> {
    fn visit(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, String>) -> Result<()> {
        for entry in
            std::fs::read_dir(dir).map_err(|error| Error::io("read native installation", error))?
        {
            let entry = entry.map_err(|error| Error::io("read installed file", error))?;
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|error| Error::io("inspect installed file", error))?;
            if kind.is_dir() {
                visit(root, &path, files)?;
            } else {
                let metadata = std::fs::metadata(&path)
                    .map_err(|error| Error::io("inspect installed artifact", error))?;
                if !metadata.is_file() {
                    return Err(invalid(format!(
                        "unsupported installed artifact {}",
                        path.display()
                    )));
                }
                files.insert(
                    path.strip_prefix(root).expect("inside prefix").to_owned(),
                    crate::sha256_file(&path)?,
                );
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    visit(prefix, prefix, &mut files)?;
    Ok(files)
}

fn cmake_paths(paths: &[PathBuf]) -> Result<String> {
    paths
        .iter()
        .map(|path| {
            path.to_str()
                .map(|path| path.replace('\\', "/").replace(';', "\\;"))
                .ok_or_else(|| {
                    invalid(format!(
                        "CMake search path is not UTF-8: {}",
                        path.display()
                    ))
                })
        })
        .collect::<Result<Vec<_>>>()
        .map(|paths| paths.join(";"))
}

fn source_tree_identity(root: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::collections::BTreeSet;
    fn visit(path: &Path, hash: &mut Sha256, ancestors: &mut BTreeSet<PathBuf>) -> Result<()> {
        let metadata = std::fs::metadata(path)
            .map_err(|error| Error::io(format!("inspect local input {}", path.display()), error))?;
        if metadata.is_file() {
            hash.update(b"file\0");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                hash.update(metadata.permissions().mode().to_le_bytes());
            }
            hash.update(crate::sha256_file(path)?);
            return Ok(());
        }
        if !metadata.is_dir() {
            return Err(invalid(format!(
                "unsupported local source entry {}",
                path.display()
            )));
        }
        let canonical = path
            .canonicalize()
            .map_err(|error| Error::io("resolve local source directory", error))?;
        if !ancestors.insert(canonical.clone()) {
            return Err(invalid(format!(
                "cyclic local source directory {}",
                path.display()
            )));
        }
        hash.update(b"directory\0");
        let mut entries = std::fs::read_dir(path)
            .map_err(|error| Error::io("read local source directory", error))?
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|error| Error::io("read local source entry", error))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if entry.file_name() == ".git" {
                continue;
            }
            let name = entry.file_name();
            let bytes = name.as_encoded_bytes();
            hash.update(bytes.len().to_le_bytes());
            hash.update(bytes);
            visit(&entry.path(), hash, ancestors)?;
        }
        ancestors.remove(&canonical);
        Ok(())
    }
    let mut hash = Sha256::new();
    visit(root, &mut hash, &mut BTreeSet::new())?;
    Ok(format!("{:x}", hash.finalize()))
}

fn merge_link_directives(groups: &[Vec<String>]) -> Result<Vec<String>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut libraries = BTreeMap::new();
    let mut result = Vec::new();
    for directive in groups.iter().flatten().rev() {
        if let Some(library) = directive.strip_prefix("cargo:rustc-link-lib=") {
            let name = library.rsplit_once('=').map_or(library, |(_, name)| name);
            if let Some(previous) = libraries.insert(name, library)
                && previous != library
            {
                return Err(invalid(format!(
                    "conflicting native link kinds/modifiers for {name}: {previous} versus {library}"
                )));
            }
        }
        if seen.insert(directive) {
            result.push(directive.clone());
        }
    }
    result.reverse();
    Ok(result)
}

fn stage_runtime(
    cache: &Cache,
    out: &Path,
    directories: &std::collections::BTreeSet<PathBuf>,
) -> Result<Option<PathBuf>> {
    let stage = tempfile::Builder::new()
        .prefix("woodshed-runtime-")
        .tempdir_in(out)
        .map_err(|error| Error::io("stage native runtime libraries", error))?;
    let mut files = BTreeMap::<std::ffi::OsString, (PathBuf, String)>::new();
    for directory in directories {
        for entry in std::fs::read_dir(directory).map_err(|error| {
            Error::io(
                format!("read native runtime directory {}", directory.display()),
                error,
            )
        })? {
            let entry = entry.map_err(|error| Error::io("read native runtime entry", error))?;
            if !crate::link::is_shared_library(&entry.path()) {
                continue;
            }
            if !entry.path().is_file() {
                return Err(invalid(format!(
                    "native runtime library is not a file: {}",
                    entry.path().display()
                )));
            }
            let digest = crate::sha256_file(&entry.path())?;
            if let Some((previous, hash)) = files.get(&entry.file_name()) {
                if *hash != digest {
                    return Err(invalid(format!(
                        "conflicting runtime library {} and {}",
                        previous.display(),
                        entry.path().display()
                    )));
                }
            } else {
                std::fs::copy(entry.path(), stage.path().join(entry.file_name()))
                    .map_err(|error| Error::io("copy native runtime library", error))?;
                files.insert(entry.file_name(), (entry.path(), digest));
            }
        }
    }
    let destination = out.join("woodshed-runtime");
    if files.is_empty() && !destination.exists() {
        return Ok(None);
    }
    let destination = cache.mirror_to(stage.path(), &destination)?;
    Ok((!files.is_empty()).then_some(destination))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitive_link_libraries_are_emitted_once_after_all_dependents() {
        let groups = [
            vec![
                "cargo:rustc-link-lib=static:+verbatim=b.lib".into(),
                "cargo:rustc-link-lib=static:+verbatim=shared.lib".into(),
            ],
            vec![
                "cargo:rustc-link-lib=static:+verbatim=a.lib".into(),
                "cargo:rustc-link-lib=static:+verbatim=shared.lib".into(),
            ],
        ];
        assert_eq!(
            merge_link_directives(&groups).unwrap(),
            [
                "cargo:rustc-link-lib=static:+verbatim=b.lib",
                "cargo:rustc-link-lib=static:+verbatim=a.lib",
                "cargo:rustc-link-lib=static:+verbatim=shared.lib",
            ]
        );
        assert!(
            merge_link_directives(&[
                vec!["cargo:rustc-link-lib=static:+verbatim=shared.lib".into()],
                vec!["cargo:rustc-link-lib=dylib:+verbatim=shared.lib".into()],
            ])
            .is_err()
        );
    }

    #[test]
    fn receipts_detect_same_size_corruption_and_local_inputs_use_content() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("input"), b"before").unwrap();
        let files = inventory(root).unwrap();
        assert_eq!(inventory(root).unwrap(), files);
        let identity = source_tree_identity(root).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), b"ignored").unwrap();
        assert_eq!(source_tree_identity(root).unwrap(), identity);
        std::fs::write(root.join("input"), b"AFTER!").unwrap();
        assert_ne!(
            files[Path::new("input")],
            crate::sha256_file(&root.join("input")).unwrap()
        );
        assert_ne!(source_tree_identity(root).unwrap(), identity);
    }

    #[test]
    fn runtime_staging_is_incremental_removes_stale_files_and_rejects_collisions() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        let out = directory.path().join("out");
        for path in [&first, &second, &out] {
            std::fs::create_dir(path).unwrap();
        }
        std::fs::write(first.join("example.dll"), b"same").unwrap();
        std::fs::write(second.join("example.dll"), b"same").unwrap();
        std::fs::write(first.join("not-a-runtime.lib"), b"ignore").unwrap();
        let cache = Cache::at(directory.path().join("cache"));
        let paths = std::collections::BTreeSet::from([first.clone(), second.clone()]);
        let staged = stage_runtime(&cache, &out, &paths).unwrap().unwrap();
        let modified = std::fs::metadata(staged.join("example.dll"))
            .unwrap()
            .modified()
            .unwrap();
        stage_runtime(&cache, &out, &paths).unwrap();
        assert_eq!(
            std::fs::metadata(staged.join("example.dll"))
                .unwrap()
                .modified()
                .unwrap(),
            modified
        );
        assert!(!staged.join("not-a-runtime.lib").exists());
        std::fs::write(second.join("example.dll"), b"DIFF").unwrap();
        assert!(stage_runtime(&cache, &out, &paths).is_err());
        assert_eq!(std::fs::read(staged.join("example.dll")).unwrap(), b"same");
        assert!(
            stage_runtime(&cache, &out, &Default::default())
                .unwrap()
                .is_none()
        );
        assert!(!staged.join("example.dll").exists());
    }
}
