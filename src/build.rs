use crate::{
    BuildKey, Cache, CmakeBuild, Error, Linkage, Result,
    project::{Dependency, PackageLinkage, Project, invalid},
    providers::{InstallOptions, Installation},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

const ENVIRONMENT: &[&str] = &[
    "CC",
    "CXX",
    "AR",
    "CFLAGS",
    "CXXFLAGS",
    "CPPFLAGS",
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
];

#[derive(Serialize, Deserialize)]
struct Receipt {
    installation: Installation,
    files: BTreeMap<PathBuf, u64>,
}

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
/// not automatically passed to Rust. Runtime deployment remains explicit.
pub fn build_from(path: &Path) -> Result<BTreeMap<String, Installation>> {
    let path =
        std::path::absolute(path).map_err(|error| Error::io("resolve declaration path", error))?;
    let project = Project::load(&path)?;
    let lock = project.locked()?;
    println!("cargo:rerun-if-changed={}", path.display());
    println!("cargo:rerun-if-changed={}", project.lock_path().display());
    for variable in ENVIRONMENT {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    let target = std::env::var("TARGET")
        .map_err(|_| invalid("TARGET is required in a Cargo build script"))?;
    let host =
        std::env::var("HOST").map_err(|_| invalid("HOST is required in a Cargo build script"))?;
    let cache = Cache::discover()?;
    let mut installations = BTreeMap::new();
    for (name, dependency) in &project.declarations.dependencies {
        let installation = match dependency {
            Dependency::Package(_) => {
                crate::providers::install(&path, name, &InstallOptions::cargo(name)?)?
            }
            Dependency::Source(source) => {
                let recipe = source.build.as_ref().ok_or_else(|| invalid(format!(
                    "{name}: add build = {{ cmake = \"Package\", targets = [\"Package::Target\"] }} or use the low-level Rust API"
                )))?;
                if target != host && recipe.toolchain.is_none() {
                    return Err(invalid(format!(
                        "{name}: cross-build {host} -> {target} requires build.toolchain; no host fallback"
                    )));
                }
                println!("cargo:rerun-if-env-changed=CMAKE_TOOLCHAIN_FILE");
                if std::env::var_os("CMAKE_TOOLCHAIN_FILE").is_some() {
                    return Err(invalid(
                        "declare build.toolchain instead of CMAKE_TOOLCHAIN_FILE so its bytes are tracked",
                    ));
                }
                let dep = &lock.sources[name];
                let source = cache.source(dep)?;
                let linkage = match recipe.linkage {
                    PackageLinkage::Static => Linkage::Static,
                    PackageLinkage::Dynamic => Linkage::Dynamic,
                };
                let mut key = BuildKey::new(name)
                    .dep(dep)
                    .linkage(linkage)
                    .mix("declarative-cmake-v1")
                    .mix(env!("CARGO_PKG_VERSION"))
                    .mix(include_bytes!("build.rs"))
                    .mix(include_bytes!("providers/implementation.rs"))
                    .mix(toml::to_string(recipe).map_err(|error| invalid(error.to_string()))?)
                    .mix(source.as_os_str().as_encoded_bytes())
                    .mix(crate::crt_static().to_string());
                for variable in ENVIRONMENT {
                    key = key.mix(variable).mix(
                        std::env::var_os(variable)
                            .unwrap_or_default()
                            .as_encoded_bytes(),
                    );
                }
                let toolchain = recipe
                    .toolchain
                    .as_ref()
                    .map(|file| {
                        let file =
                            std::path::absolute(path.parent().expect("absolute path").join(file))
                                .map_err(|error| Error::io("resolve CMake toolchain", error))?;
                        println!("cargo:rerun-if-changed={}", file.display());
                        let bytes = std::fs::read(&file)
                            .map_err(|error| Error::io("read CMake toolchain", error))?;
                        Ok::<_, Error>((file, bytes))
                    })
                    .transpose()?;
                if let Some((file, bytes)) = &toolchain {
                    key = key.mix(file.as_os_str().as_encoded_bytes()).mix(bytes);
                }
                for input in &recipe.inputs {
                    let file =
                        std::path::absolute(path.parent().expect("absolute path").join(input))
                            .map_err(|error| Error::io("resolve build input", error))?;
                    println!("cargo:rerun-if-changed={}", file.display());
                    let bytes = std::fs::read(&file).map_err(|error| {
                        Error::io(
                            format!("read declared build input {}", file.display()),
                            error,
                        )
                    })?;
                    key = key.mix(file.as_os_str().as_encoded_bytes()).mix(bytes);
                }
                for executable in ["cmake", "ninja", "cc", "c++", "cl", "sccache", "ccache"] {
                    if let Some(file) = crate::find_on_path(executable) {
                        println!("cargo:rerun-if-changed={}", file.display());
                        let metadata = std::fs::metadata(&file)
                            .map_err(|error| Error::io("inspect native build tool", error))?;
                        let modified = metadata
                            .modified()
                            .map_err(|error| Error::io("read native tool timestamp", error))?;
                        key = key
                            .mix(file.as_os_str().as_encoded_bytes())
                            .mix(metadata.len().to_le_bytes())
                            .mix(format!("{modified:?}"));
                    }
                }
                let dirs = cache.build_dir(&key);
                let _guard = dirs.lock()?;
                let receipt = dirs.build.join("woodshed-installation.json");
                let prior = read_receipt(&receipt)?;
                if let Some(prior) = prior
                    .as_ref()
                    .filter(|prior| complete(&dirs.prefix, &prior.files))
                {
                    eprintln!("woodshed: reusing native installation {name}");
                    prior.installation.clone()
                } else {
                    eprintln!("woodshed: building native dependency {name}");
                    if prior.is_some() {
                        std::fs::remove_file(&receipt).map_err(|error| {
                            Error::io("invalidate incomplete native receipt", error)
                        })?;
                    }
                    let mut builder = CmakeBuild::new(&source, &dirs).linkage(linkage);
                    for (key, value) in &recipe.defines {
                        builder = builder.define(key, value);
                    }
                    let mut defines = vec![(
                        "WOODSHED_PACKAGE_PREFIX".into(),
                        dirs.prefix.clone().into_os_string(),
                    )];
                    if let Some((file, _)) = &toolchain {
                        builder = builder.define("CMAKE_TOOLCHAIN_FILE", file.to_string_lossy());
                        defines
                            .push(("CMAKE_TOOLCHAIN_FILE".into(), file.clone().into_os_string()));
                    }
                    if target.ends_with("-msvc") {
                        defines.push((
                            "CMAKE_MSVC_RUNTIME_LIBRARY".into(),
                            OsString::from(if crate::crt_static() {
                                "MultiThreaded"
                            } else {
                                "MultiThreadedDLL"
                            }),
                        ));
                    }
                    builder.install()?;
                    let options = InstallOptions {
                        directory: dirs.build.join("woodshed-exports"),
                        target: target.clone(),
                        configuration: "Release".into(),
                    };
                    let mut installation = crate::providers::discover(
                        &recipe.package(),
                        &options,
                        &defines,
                        "metadata",
                    )?;
                    if linkage == Linkage::Dynamic {
                        installation.runtime_directories = ["bin", "lib", "lib64"]
                            .into_iter()
                            .map(|dir| dirs.prefix.join(dir))
                            .filter(|dir| dir.is_dir())
                            .collect();
                    }
                    installation.cargo_directives()?;
                    let files = inventory(&dirs.prefix)?;
                    if files.is_empty() {
                        return Err(invalid(format!(
                            "{name}: CMake installed no files; exported package cannot be reused"
                        )));
                    }
                    let text = serde_json::to_vec(&Receipt {
                        installation: installation.clone(),
                        files,
                    })
                    .map_err(|error| invalid(error.to_string()))?;
                    std::fs::write(&receipt, text)
                        .map_err(|error| Error::io("write native installation receipt", error))?;
                    installation
                }
            }
        };
        // Validate the entire graph before emitting any success-shaped link output.
        installation.cargo_directives()?;
        installations.insert(name.clone(), installation);
    }
    for installation in installations.values() {
        installation.emit_cargo()?;
    }
    // The exported C++ target graph can rely on the link language's implicit
    // runtime rather than listing it as an explicit CMake library argument.
    if project.declarations.dependencies.values().any(|dependency| {
        matches!(dependency, Dependency::Source(source) if source.build.as_ref().is_some_and(|recipe| matches!(recipe.linkage, PackageLinkage::Static)))
    }) {
        crate::link_cpp_runtime_for(Linkage::Static);
    }
    Ok(installations)
}

fn read_receipt(path: &Path) -> Result<Option<Receipt>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
            invalid(format!(
                "invalid native installation receipt {}: {error}",
                path.display()
            ))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::io("read native installation receipt", error)),
    }
}

fn complete(prefix: &Path, files: &BTreeMap<PathBuf, u64>) -> bool {
    !files.is_empty()
        && files.iter().all(|(path, length)| {
            std::fs::metadata(prefix.join(path))
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() == *length)
        })
}

fn inventory(prefix: &Path) -> Result<BTreeMap<PathBuf, u64>> {
    fn visit(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, u64>) -> Result<()> {
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
                    metadata.len(),
                );
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    visit(prefix, prefix, &mut files)?;
    Ok(files)
}
