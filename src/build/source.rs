use super::*;
use crate::{
    BuildDirs, MsvcRuntime,
    project::{MsvcCrt, ResolvedDependency, Source},
};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use storage::{Artifact, Workspace};

type Inputs = BTreeMap<String, serde_json::Value>;

pub(super) struct Request<'a> {
    pub cache: &'a Cache,
    pub manifest: &'a Path,
    pub host: &'a str,
    pub node: &'a ResolvedDependency,
    pub source: &'a Source,
    pub pinned: Option<&'a crate::Dep>,
    pub dependencies: &'a BTreeMap<String, String>,
    pub host_tools: &'a BTreeMap<String, String>,
    pub cmake_packages: &'a BTreeMap<String, PathBuf>,
    pub prefixes: &'a [PathBuf],
    pub programs: &'a [PathBuf],
}

fn record(inputs: &mut Inputs, name: &str, value: impl Serialize) -> Result<()> {
    let mut value = serde_json::to_value(value).map_err(|error| invalid(error.to_string()))?;
    value.sort_all_objects();
    inputs.insert(name.into(), value);
    Ok(())
}

fn key(label: &str, inputs: &Inputs) -> Result<BuildKey> {
    Ok(BuildKey::explicit(label)
        .mix(serde_json::to_vec(inputs).map_err(|error| invalid(error.to_string()))?))
}

fn input_fields(
    inputs: &Inputs,
    tools: &BTreeMap<PathBuf, String>,
) -> Result<BTreeMap<String, String>> {
    let mut inputs = inputs.clone();
    record(&mut inputs, "configured-tools", tools)?;
    inputs
        .into_iter()
        .map(|(name, value)| {
            let bytes = serde_json::to_vec(&value).map_err(|error| invalid(error.to_string()))?;
            Ok((name, format!("{:x}", Sha256::digest(bytes))))
        })
        .collect()
}

fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).map_err(|error| Error::io("resolve source build path", error))
}

fn files(paths: &[PathBuf]) -> Result<BTreeMap<PathBuf, String>> {
    paths
        .iter()
        .map(|path| Ok((path.clone(), crate::sha256_file(path)?)))
        .collect()
}

fn configured_tools(build: &Path) -> Result<Vec<PathBuf>> {
    let text = std::fs::read_to_string(build.join("CMakeCache.txt"))
        .map_err(|error| Error::io("read configured compiler identities", error))?;
    let mut paths = BTreeSet::new();
    for line in text
        .lines()
        .filter(|line| !line.starts_with("//") && !line.starts_with('#'))
    {
        let Some((declaration, value)) = line.split_once('=') else {
            continue;
        };
        let name = declaration.split(':').next().unwrap_or_default();
        if !name.starts_with("CMAKE_")
            || !(name.ends_with("_COMPILER")
                || name.ends_with("_COMPILER_AR")
                || name.ends_with("_COMPILER_RANLIB")
                || name.ends_with("_COMPILER_LAUNCHER")
                || matches!(
                    name,
                    "CMAKE_AR"
                        | "CMAKE_LINKER"
                        | "CMAKE_RANLIB"
                        | "CMAKE_MT"
                        | "CMAKE_MAKE_PROGRAM"
                ))
        {
            continue;
        }
        let command = value.split(';').next().unwrap_or_default();
        if command.is_empty() || command == ":" || command.ends_with("-NOTFOUND") {
            continue;
        }
        let path = PathBuf::from(command);
        let path = if path.is_absolute() {
            path
        } else {
            crate::find_on_path(command).ok_or_else(|| {
                invalid(format!("configured native tool is unavailable: {command}"))
            })?
        };
        if !path.is_file() {
            return Err(invalid(format!(
                "configured native tool is not a file: {}",
                path.display()
            )));
        }
        paths.insert(path);
    }
    if paths.is_empty() {
        return Err(invalid(
            "CMake did not record any native tools; cannot identify a safe artifact",
        ));
    }
    Ok(paths.into_iter().collect())
}

pub(super) fn install(request: Request<'_>) -> Result<(String, Installation)> {
    let Request {
        cache,
        manifest,
        host,
        node,
        source,
        pinned,
        dependencies,
        host_tools,
        cmake_packages,
        prefixes,
        programs,
    } = request;
    let recipe = source.build.as_ref().ok_or_else(|| {
        invalid(format!(
            "{}: declare an explicit source build recipe",
            node.name
        ))
    })?;
    if node.target != host && recipe.toolchain.is_none() {
        return Err(invalid(format!(
            "{}: cross-build {host} -> {} requires build.toolchain; no host fallback",
            node.name, node.target
        )));
    }
    println!("cargo:rerun-if-env-changed=CMAKE_TOOLCHAIN_FILE");
    if std::env::var_os("CMAKE_TOOLCHAIN_FILE").is_some() {
        return Err(invalid(
            "declare build.toolchain instead of CMAKE_TOOLCHAIN_FILE so its bytes are tracked",
        ));
    }
    let parent = manifest.parent().expect("absolute manifest");
    let local = source
        .path
        .as_ref()
        .map(|path| absolute(&parent.join(path)))
        .transpose()?;
    let original = match &local {
        Some(path) => {
            if !path.is_dir() {
                return Err(invalid(format!(
                    "local source is not a directory: {}",
                    path.display()
                )));
            }
            let canonical = path
                .canonicalize()
                .map_err(|error| Error::io("resolve local source", error))?;
            for output in [
                Some(cache.root().to_path_buf()),
                std::env::var_os("OUT_DIR").map(PathBuf::from),
            ]
            .into_iter()
            .flatten()
            {
                let output = absolute(&output)?;
                let output = output.canonicalize().unwrap_or(output);
                if output.starts_with(&canonical) || canonical.starts_with(&output) {
                    return Err(invalid(
                        "local source overlaps Woodshed/Cargo output; choose a separate source directory",
                    ));
                }
            }
            for marker in &source.markers {
                if !path.join(marker).exists() {
                    return Err(invalid(format!("missing local source marker {marker}")));
                }
            }
            println!("cargo:rerun-if-changed={}", path.display());
            path.clone()
        }
        None => absolute(
            &cache.source(pinned.ok_or_else(|| invalid("source lock entry is missing"))?)?,
        )?,
    };
    let content = source_tree_identity(&original)?;
    let runtime = match recipe.msvc_runtime {
        Some(MsvcCrt::Static) => MsvcRuntime::AlwaysStatic,
        Some(MsvcCrt::Dynamic) => MsvcRuntime::AlwaysDynamic,
        None if node.role == BuildRole::Host => MsvcRuntime::AlwaysDynamic,
        None => MsvcRuntime::MatchRust,
    };
    let linkage = match recipe.linkage {
        PackageLinkage::Static => Linkage::Static,
        PackageLinkage::Dynamic => Linkage::Dynamic,
    };
    let toolchain_file = recipe
        .toolchain
        .as_ref()
        .map(|path| absolute(&parent.join(path)))
        .transpose()?;
    let mut declared_files = recipe
        .inputs
        .iter()
        .map(|path| absolute(&parent.join(path)))
        .collect::<Result<Vec<_>>>()?;
    declared_files.extend(toolchain_file.iter().cloned());
    for file in &declared_files {
        println!("cargo:rerun-if-changed={}", file.display());
    }
    let declared = files(&declared_files)?;
    let toolchain = crate::Toolchain::detect_for_target(&node.target);
    let mut inputs = Inputs::new();
    record(&mut inputs, "schema", "source-install-v1")?;
    record(
        &mut inputs,
        "namespace",
        manifest
            .canonicalize()
            .map_err(|error| Error::io("resolve project namespace", error))?,
    )?;
    record(&mut inputs, "package", &node.id)?;
    record(&mut inputs, "cache-root", absolute(cache.root())?)?;
    record(&mut inputs, "source-path", &original)?;
    record(&mut inputs, "source-pin", pinned)?;
    record(&mut inputs, "source-content", &content)?;
    record(&mut inputs, "recipe", recipe)?;
    record(&mut inputs, "defines", &node.defines)?;
    record(&mut inputs, "features", &node.features)?;
    record(&mut inputs, "role", node.role)?;
    record(&mut inputs, "host", host)?;
    record(&mut inputs, "target", &node.target)?;
    record(&mut inputs, "configuration", "Release")?;
    let defaults = BTreeMap::from([
        (
            "CMAKE_FIND_USE_PACKAGE_REGISTRY".to_owned(),
            "OFF".to_owned(),
        ),
        (
            "CMAKE_FIND_USE_SYSTEM_PACKAGE_REGISTRY".into(),
            "OFF".into(),
        ),
        ("FETCHCONTENT_FULLY_DISCONNECTED".into(), "ON".into()),
        ("FETCHCONTENT_UPDATES_DISCONNECTED".into(), "ON".into()),
    ]);
    record(&mut inputs, "native-defaults", &defaults)?;
    record(
        &mut inputs,
        "runtime",
        node.target
            .ends_with("-msvc")
            .then(|| runtime.cmake_value()),
    )?;
    record(&mut inputs, "generator", toolchain.generator_name())?;
    record(&mut inputs, "declared-inputs", &declared)?;
    record(&mut inputs, "dependencies", dependencies)?;
    record(&mut inputs, "host-tools", host_tools)?;
    for variable in ENVIRONMENT
        .iter()
        .copied()
        .chain(recipe.env.iter().map(String::as_str))
    {
        println!("cargo:rerun-if-env-changed={variable}");
        record(
            &mut inputs,
            &format!("env:{variable}"),
            std::env::var_os(variable).map(|value| value.as_encoded_bytes().to_vec()),
        )?;
    }
    for program in ["cmake", "cc", "c++", "cl", "sccache", "ccache"] {
        if let Some(path) = crate::find_on_path(program) {
            println!("cargo:rerun-if-changed={}", path.display());
            record(&mut inputs, &format!("tool:{program}"), files(&[path])?)?;
        }
    }
    for program in programs {
        println!("cargo:rerun-if-changed={}", program.display());
        record(
            &mut inputs,
            &format!("host-bin:{}", program.display()),
            if program.is_dir() {
                Some(source_tree_identity(program)?)
            } else {
                None
            },
        )?;
    }
    record(
        &mut inputs,
        "implementation",
        format!(
            "{:x}",
            Sha256::digest(
                concat!(
                    include_str!("source.rs"),
                    include_str!("storage.rs"),
                    include_str!("../cmake.rs"),
                    include_str!("../toolchain.rs"),
                    include_str!("../providers/implementation.rs"),
                    include_str!("../providers/metadata.cmake")
                )
                .as_bytes()
            )
        ),
    )?;
    let mut hard = inputs.clone();
    if let Some(policy) = &recipe.workspace {
        hard.remove("source-content");
        hard.remove("features");
        hard.remove("implementation");
        let mut recipe = recipe.clone();
        recipe
            .defines
            .retain(|name, _| !policy.reconfigure.contains(name));
        recipe.features.clear();
        recipe.default_features.clear();
        record(&mut hard, "recipe", recipe)?;
        record(
            &mut hard,
            "defines",
            node.defines
                .iter()
                .filter(|(name, _)| !policy.reconfigure.contains(name))
                .collect::<BTreeMap<_, _>>(),
        )?;
        record(
            &mut hard,
            "dependencies",
            dependencies
                .iter()
                .filter(|(name, _)| !policy.dependencies.contains(name))
                .collect::<BTreeMap<_, _>>(),
        )?;
    }
    let workspace_key = key("source-workspace", &hard)?;
    let mut workspace = Workspace::open(cache, &workspace_key)?;
    let prior_tools = workspace.tools().to_vec();
    for tool in &prior_tools {
        println!("cargo:rerun-if-changed={}", tool.display());
    }
    let mut actual_tools = files(&prior_tools)?;
    let artifact_key = |tools: &BTreeMap<PathBuf, String>| {
        let mut exact = inputs.clone();
        record(&mut exact, "configured-tools", tools)?;
        key("source-artifact", &exact)
    };
    let mut repaired = false;
    if !prior_tools.is_empty() {
        let mut artifact = Artifact::open(cache, &artifact_key(&actual_tools)?)?;
        if let Some(installation) = artifact.cached()? {
            diagnostics::completed(
                cache,
                manifest,
                node,
                &artifact_key(&actual_tools)?.fingerprint(),
                Some(&workspace_key),
                BuildOutcome::Reused,
                input_fields(&inputs, &actual_tools)?,
            )?;
            return Ok((artifact_key(&actual_tools)?.fingerprint(), installation));
        }
        repaired = artifact.repaired;
    }
    let hard_fingerprint = |tools: &BTreeMap<PathBuf, String>| {
        let mut inputs = hard.clone();
        record(&mut inputs, "configured-tools", tools)?;
        Ok::<_, Error>(key("source-hard-boundary", &inputs)?.fingerprint())
    };
    if !prior_tools.is_empty() {
        workspace.hard_boundary(&hard_fingerprint(&actual_tools)?)?;
    }
    cache.mirror_filtered(&original, &workspace.source, |name| name == ".git")?;
    if source_tree_identity(&workspace.source)? != content {
        return Err(invalid(
            "source changed while preparing its stable snapshot; rerun the build",
        ));
    }
    let source_path = workspace.source.clone();
    let build_path = workspace.build.clone();
    let mut definitions = defaults;
    for (name, value) in &node.defines {
        if definitions.contains_key(name) {
            return Err(invalid(format!(
                "build.defines.{name} is owned by Woodshed's locked-source policy"
            )));
        }
        definitions.insert(name.clone(), value.clone());
    }
    for (name, path) in cmake_packages {
        let variable = format!("{name}_DIR");
        let value = path
            .to_str()
            .ok_or_else(|| invalid("CMake package location must be UTF-8"))?
            .to_owned();
        if definitions
            .insert(variable.clone(), value.clone())
            .is_some_and(|prior| prior != value)
        {
            return Err(invalid(format!(
                "build.defines.{variable} conflicts with the selected graph dependency"
            )));
        }
    }
    for (variable, paths) in [
        ("CMAKE_PREFIX_PATH", prefixes),
        ("CMAKE_PROGRAM_PATH", programs),
    ] {
        if !paths.is_empty() {
            definitions.insert(variable.into(), cmake_paths(paths)?);
        }
    }
    if let Some(file) = &toolchain_file {
        definitions.insert(
            "CMAKE_TOOLCHAIN_FILE".into(),
            file.to_str()
                .ok_or_else(|| invalid("CMake toolchain path must be UTF-8"))?
                .into(),
        );
    }
    let managed = definitions.keys().cloned().collect::<BTreeSet<_>>();
    let builder = |prefix: &Path| -> Result<CmakeBuild> {
        let dirs = BuildDirs {
            build: build_path.clone(),
            prefix: prefix.to_path_buf(),
        };
        let mut builder = CmakeBuild::new(&source_path, &dirs)
            .target_triple(&node.target)
            .toolchain(toolchain.clone())
            .linkage(linkage)
            .msvc_runtime(runtime);
        for (name, value) in &definitions {
            builder = builder.define(name, value);
        }
        if !programs.is_empty() {
            let paths = programs
                .iter()
                .cloned()
                .chain(std::env::split_paths(
                    &std::env::var_os("PATH").unwrap_or_default(),
                ))
                .collect::<Vec<_>>();
            builder = builder.env(
                "PATH",
                std::env::join_paths(paths)
                    .map_err(|error| invalid(format!("invalid host-tool path: {error}")))?,
            );
        }
        Ok(builder)
    };
    let pending = workspace.root.join("unpublished-prefix");
    workspace
        .configure(builder(&pending)?, managed.clone())?
        .configure()?;
    let selected = configured_tools(&workspace.build)?;
    for tool in &selected {
        println!("cargo:rerun-if-changed={}", tool.display());
    }
    let selected_files = files(&selected)?;
    if !prior_tools.is_empty() && actual_tools != selected_files {
        workspace.hard_boundary(&hard_fingerprint(&selected_files)?)?;
        workspace
            .configure(builder(&pending)?, managed.clone())?
            .configure()?;
        if files(&configured_tools(&workspace.build)?)? != selected_files {
            return Err(invalid(
                "native tool selection changed across clean configuration",
            ));
        }
    }
    actual_tools = selected_files;
    workspace.hard_boundary(&hard_fingerprint(&actual_tools)?)?;
    workspace.tool_selection(selected)?;
    let exact = artifact_key(&actual_tools)?;
    let mut artifact = Artifact::open(cache, &exact)?;
    if let Some(installation) = artifact.cached()? {
        diagnostics::completed(
            cache,
            manifest,
            node,
            &exact.fingerprint(),
            Some(&workspace_key),
            BuildOutcome::Reused,
            input_fields(&inputs, &actual_tools)?,
        )?;
        return Ok((exact.fingerprint(), installation));
    }
    repaired |= artifact.repaired;
    diagnostics::starting(
        cache,
        manifest,
        node,
        &input_fields(&inputs, &actual_tools)?,
        repaired,
    )?;
    let stage = artifact.begin()?;
    workspace
        .configure(builder(&artifact.prefix)?, managed.clone())?
        .install_staged(&stage)?;
    workspace.configured(managed)?;
    if files(&configured_tools(&workspace.build)?)? != actual_tools
        || files(&declared_files)? != declared
    {
        return Err(invalid(
            "native tools or declared inputs changed during compilation; rerun the build",
        ));
    }
    if source_tree_identity(&workspace.source)? != content
        || source_tree_identity(&original)? != content
    {
        return Err(invalid(
            "source changed during compilation; source recipes must build out of tree",
        ));
    }
    if let Some(dep) = pinned {
        cache.verify_source(dep)?;
    }
    artifact.publish(cache, &stage)?;
    let mut exported = definitions
        .iter()
        .map(|(name, value)| (name.clone(), OsString::from(value)))
        .collect::<Vec<_>>();
    exported.push((
        "WOODSHED_PACKAGE_PREFIX".into(),
        artifact.prefix.clone().into_os_string(),
    ));
    if node.target.ends_with("-msvc") {
        exported.push((
            "CMAKE_MSVC_RUNTIME_LIBRARY".into(),
            runtime.cmake_value().into(),
        ));
    }
    let options = InstallOptions {
        directory: artifact.metadata.clone(),
        target: node.target.clone(),
        configuration: "Release".into(),
    };
    let mut installation = if recipe.cmake.is_empty() {
        Installation::tools(&artifact.prefix, recipe.linkage)
    } else {
        crate::providers::discover(&recipe.package(), &options, &exported, "metadata")?
    };
    for tool in &recipe.tools {
        let filename = if node.target.contains("windows") {
            format!("{tool}.exe")
        } else {
            tool.clone()
        };
        let executable = artifact.prefix.join("bin").join(filename);
        if !executable.is_file() {
            return Err(invalid(format!(
                "declared installed tool is missing: {}",
                executable.display()
            )));
        }
        installation.executables.insert(tool.clone(), executable);
    }
    if linkage == Linkage::Dynamic
        || installation
            .library_linkage
            .values()
            .any(|kind| *kind == PackageLinkage::Dynamic)
    {
        installation.runtime_directories = ["bin", "lib", "lib64"]
            .into_iter()
            .map(|path| artifact.prefix.join(path))
            .filter(|path| path.is_dir())
            .collect();
    }
    if node.role == BuildRole::Target {
        installation.cargo_directives()?;
    }
    let mut forbidden = vec![
        workspace.root.clone(),
        original,
        artifact.stage.clone(),
        artifact.metadata.clone(),
    ];
    forbidden.extend(
        forbidden
            .clone()
            .iter()
            .filter_map(|path| path.canonicalize().ok()),
    );
    let fields = input_fields(&inputs, &actual_tools)?;
    let installation = artifact.commit(installation, &forbidden, fields.clone())?;
    diagnostics::completed(
        cache,
        manifest,
        node,
        &exact.fingerprint(),
        Some(&workspace_key),
        if repaired {
            BuildOutcome::Repaired
        } else {
            BuildOutcome::Built
        },
        fields,
    )?;
    Ok((exact.fingerprint(), installation))
}
