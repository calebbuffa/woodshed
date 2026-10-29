use super::*;
use crate::{
    cache::{Lock, lock_path},
    project::{CmakePackage, PrebuiltCompatibility},
};
use std::fs;

const DESCRIPTOR: &str = "woodshed-artifact.json";

/// Explicit publisher metadata for a self-contained, relocatable native installation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactMetadata {
    /// Exact target/linkage/runtime and publisher-defined ABI contract.
    pub compatibility: PrebuiltCompatibility,
    /// Installed CONFIG package and targets; absent for tool-only artifacts.
    pub cmake: Option<CmakePackage>,
    /// Installed executable names under bin, without the platform executable suffix.
    #[serde(default)]
    pub tools: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Package {
    format: u32,
    metadata: ArtifactMetadata,
    files: BTreeMap<String, PortableEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
enum PortableEntry {
    Directory,
    File { sha256: String, executable: bool },
}

fn portable_inventory(
    prefix: &Path,
    metadata: &ArtifactMetadata,
) -> Result<BTreeMap<String, PortableEntry>> {
    let root = fs::symlink_metadata(prefix)
        .map_err(|error| Error::io("inspect portable package prefix", error))?;
    if !root.is_dir() || crate::cache::is_link(&root) {
        return Err(invalid("portable package prefix must be a real directory"));
    }
    fn visit(
        prefix: &Path,
        path: &Path,
        tools: &[String],
        entries: &mut BTreeMap<String, PortableEntry>,
    ) -> Result<()> {
        for entry in fs::read_dir(path)
            .map_err(|error| Error::io("read portable package directory", error))?
        {
            let entry = entry.map_err(|error| Error::io("read portable package entry", error))?;
            let path = entry.path();
            let relative = path.strip_prefix(prefix).expect("package child");
            let name = relative
                .components()
                .map(|component| {
                    component
                        .as_os_str()
                        .to_str()
                        .ok_or_else(|| invalid("portable package paths must be UTF-8"))
                })
                .collect::<Result<Vec<_>>>()?
                .join("/");
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| Error::io("inspect portable package entry", error))?;
            if crate::cache::is_link(&metadata) {
                return Err(invalid(
                    "portable packages must materialize links before distribution",
                ));
            }
            if metadata.is_dir() {
                entries.insert(name, PortableEntry::Directory);
                visit(prefix, &path, tools, entries)?;
            } else if metadata.is_file() {
                #[cfg(unix)]
                let executable = {
                    use std::os::unix::fs::PermissionsExt;
                    metadata.permissions().mode() & 0o111 != 0
                };
                #[cfg(not(unix))]
                let executable = tools.contains(&name);
                entries.insert(
                    name,
                    PortableEntry::File {
                        sha256: crate::sha256_file(&path)?,
                        executable,
                    },
                );
            } else {
                return Err(invalid(
                    "portable package contains an unsupported file type",
                ));
            }
        }
        Ok(())
    }
    let tools = metadata
        .tools
        .iter()
        .map(|tool| {
            format!(
                "bin/{}{}",
                tool,
                if metadata.compatibility.target.contains("-windows-") {
                    ".exe"
                } else {
                    ""
                }
            )
        })
        .collect::<Vec<_>>();
    let mut entries = BTreeMap::new();
    visit(prefix, prefix, &tools, &mut entries)?;
    Ok(entries)
}

fn matches_inventory(
    actual: &BTreeMap<String, PortableEntry>,
    expected: &BTreeMap<String, PortableEntry>,
) -> bool {
    actual.len() == expected.len()
        && actual
            .iter()
            .all(|(name, entry)| match (entry, expected.get(name)) {
                (PortableEntry::Directory, Some(PortableEntry::Directory)) => true,
                (
                    PortableEntry::File { sha256, executable },
                    Some(PortableEntry::File {
                        sha256: expected,
                        executable: expected_mode,
                    }),
                ) => sha256 == expected && (!cfg!(unix) || executable == expected_mode),
                _ => false,
            })
}

fn validate(metadata: &ArtifactMetadata) -> Result<()> {
    metadata.compatibility.validate()?;
    if metadata.cmake.is_none() && metadata.tools.is_empty() {
        return Err(invalid(
            "portable artifacts must declare an installed package or tools",
        ));
    }
    if let Some(package) = &metadata.cmake {
        package.validate()?;
        if package.linkage != metadata.compatibility.linkage {
            return Err(invalid(
                "portable package linkage differs from compatibility metadata",
            ));
        }
    }
    for tool in &metadata.tools {
        if tool.is_empty()
            || !tool
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte))
            || tool == "."
            || tool == ".."
        {
            return Err(invalid(
                "portable tool names must be plain executable names",
            ));
        }
    }
    Ok(())
}

fn bytes_contain(bytes: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && bytes.windows(needle.len()).any(|window| {
            if cfg!(windows) {
                window.eq_ignore_ascii_case(needle)
            } else {
                window == needle
            }
        })
}

fn reject_prefix_references(prefix: &Path, old: &Path) -> Result<()> {
    let spelling = old.to_string_lossy();
    let mut spellings = vec![spelling.into_owned()];
    if let Ok(canonical) = old.canonicalize() {
        spellings.push(canonical.to_string_lossy().into_owned());
    }
    spellings.extend(
        spellings
            .clone()
            .iter()
            .map(|value| value.replace('\\', "/")),
    );
    spellings.extend(
        spellings
            .clone()
            .iter()
            .map(|value| value.replace('\\', "\\\\")),
    );
    let needles = spellings
        .iter()
        .flat_map(|value| {
            [
                value.as_bytes().to_vec(),
                value
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>(),
            ]
        })
        .collect::<Vec<_>>();
    for file in inventory(prefix)?.keys() {
        let bytes = fs::read(prefix.join(file))
            .map_err(|error| Error::io("inspect artifact relocatability", error))?;
        if needles.iter().any(|needle| bytes_contain(&bytes, needle)) {
            return Err(invalid(format!(
                "artifact file {} embeds its old installation prefix; upstream must produce relocatable exports/binaries before export",
                file.display()
            )));
        }
    }
    Ok(())
}

fn installed(
    metadata: &ArtifactMetadata,
    prefix: &Path,
    directory: &Path,
    toolchain: Option<&Path>,
) -> Result<Installation> {
    let mut defines = vec![(
        "WOODSHED_PACKAGE_PREFIX".into(),
        prefix.as_os_str().to_os_string(),
    )];
    if let Some(toolchain) = toolchain {
        defines.push((
            "CMAKE_TOOLCHAIN_FILE".into(),
            toolchain.as_os_str().to_os_string(),
        ));
    }
    let mut installation = match &metadata.cmake {
        Some(package) => crate::providers::discover(
            package,
            &InstallOptions {
                directory: directory.into(),
                target: metadata.compatibility.target.clone(),
                configuration: metadata.compatibility.configuration.clone(),
            },
            &defines,
            "metadata",
        )?,
        None => Installation::tools(prefix, metadata.compatibility.linkage),
    };
    for tool in &metadata.tools {
        let name = if metadata.compatibility.target.contains("-windows-") {
            format!("{tool}.exe")
        } else {
            tool.clone()
        };
        let path = prefix.join("bin").join(name);
        if !path.is_file() {
            return Err(invalid(format!(
                "portable artifact is missing declared tool {}",
                path.display()
            )));
        }
        installation.executables.insert(tool.clone(), path);
    }
    if metadata.compatibility.linkage == PackageLinkage::Dynamic
        || installation
            .library_linkage
            .values()
            .any(|kind| *kind == PackageLinkage::Dynamic)
    {
        installation.runtime_directories = ["bin", "lib", "lib64"]
            .iter()
            .map(|name| prefix.join(name))
            .filter(|path| path.is_dir())
            .collect();
    }
    let canonical = prefix
        .canonicalize()
        .map_err(|error| Error::io("resolve portable artifact prefix", error))?;
    for path in installation
        .include_dirs
        .iter()
        .chain(installation.executables.values())
        .chain(installation.cmake_packages.values())
        .chain(&installation.runtime_directories)
        .chain(
            installation
                .link_arguments
                .iter()
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .collect::<Vec<_>>()
                .iter(),
        )
    {
        let path = path
            .canonicalize()
            .map_err(|error| Error::io("resolve portable artifact metadata", error))?;
        if !path.starts_with(&canonical) {
            return Err(invalid(format!(
                "portable artifacts must be self-contained; external installed metadata path {}",
                path.display()
            )));
        }
    }
    for token in installation
        .link_arguments
        .iter()
        .chain(&installation.compile_flags)
        .chain(&installation.definitions)
    {
        for value in token.split(['=', ',', ';']) {
            let value = value.trim_matches(['"', '\'']);
            let value = ["-isystem", "-I", "-L", "/LIBPATH:"]
                .iter()
                .find_map(|prefix| value.strip_prefix(prefix))
                .unwrap_or(value);
            let path = Path::new(value);
            if path.is_absolute() {
                let path = path
                    .canonicalize()
                    .map_err(|error| Error::io("resolve portable artifact flag path", error))?;
                if !path.starts_with(&canonical) {
                    return Err(invalid(format!(
                        "portable artifact flags reference an external installation path: {token}"
                    )));
                }
            }
        }
    }
    Ok(installation)
}

impl Cache {
    /// Export a verified source artifact as a portable package directory.
    ///
    /// Refuses embedded prefix references and exports that depend on external
    /// installation paths. The caller explicitly attests the ABI label; Woodshed
    /// verifies the recorded target/configuration and re-evaluates installed exports.
    /// The destination must not exist and is atomically published.
    pub fn export_artifact(
        &self,
        id: &str,
        destination: &Path,
        metadata: &ArtifactMetadata,
        toolchain: Option<&Path>,
    ) -> Result<()> {
        super::management::checked_id(id)?;
        validate(metadata)?;
        if metadata.compatibility.target != crate::rust_host()?
            && metadata.cmake.is_some()
            && toolchain.is_none()
        {
            return Err(invalid(
                "cross-target artifact export requires an explicit metadata toolchain",
            ));
        }
        let _activity = Lock::acquire_shared(&self.root().join("activity.lock"))?;
        let root = self.root().join("artifacts").join(id);
        let _artifact = Lock::acquire(&lock_path(&root, "artifact"))?;
        if !super::management::inspect(self, id, Vec::new())?.verified {
            return Err(invalid("cannot export an incomplete or corrupt artifact"));
        }
        let receipt: storage::ArtifactReceipt = serde_json::from_slice(
            &fs::read(root.join("woodshed-installation.json"))
                .map_err(|error| Error::io("read artifact export receipt", error))?,
        )
        .map_err(|error| invalid(error.to_string()))?;
        if receipt.installation.requested_linkage() != metadata.compatibility.linkage {
            return Err(invalid(
                "artifact export linkage differs from its verified installation",
            ));
        }
        for (field, value) in [
            (
                "target",
                serde_json::to_value(&metadata.compatibility.target),
            ),
            (
                "configuration",
                serde_json::to_value(&metadata.compatibility.configuration),
            ),
        ] {
            let value = value.map_err(|error| invalid(error.to_string()))?;
            let expected = format!(
                "{:x}",
                sha2::Sha256::digest(
                    serde_json::to_vec(&value).map_err(|error| invalid(error.to_string()))?
                )
            );
            if receipt.input_fields.get(field) != Some(&expected) {
                return Err(invalid(format!(
                    "artifact export {field} differs from its verified build receipt"
                )));
            }
            let runtime = metadata
                .compatibility
                .msvc_runtime
                .map(|runtime| match runtime {
                    crate::project::MsvcCrt::Static => "MultiThreaded",
                    crate::project::MsvcCrt::Dynamic => "MultiThreadedDLL",
                });
            if receipt.input_fields.get("runtime") != Some(&field_hash(&runtime)?) {
                return Err(invalid(
                    "artifact export MSVC runtime differs from its verified build receipt",
                ));
            }
        }
        let destination = std::path::absolute(destination)
            .map_err(|error| Error::io("resolve artifact export destination", error))?;
        if crate::cache::resolve_path(&destination)?
            .starts_with(crate::cache::resolve_path(self.root())?)
        {
            return Err(invalid(
                "portable export destination must be outside the managed cache",
            ));
        }
        let _destination = Lock::acquire(&lock_path(&destination, "export"))?;
        if destination
            .try_exists()
            .map_err(|error| Error::io("inspect artifact export destination", error))?
        {
            return Err(invalid(
                "artifact export destination already exists; refusing overwrite",
            ));
        }
        let parent = destination.parent().expect("absolute destination");
        fs::create_dir_all(parent)
            .map_err(|error| Error::io("create artifact export parent", error))?;
        let stage = tempfile::Builder::new()
            .prefix(".woodshed-export-")
            .tempdir_in(parent)
            .map_err(|error| Error::io("stage artifact export", error))?;
        let package = stage.path().join("package");
        fs::create_dir(&package)
            .map_err(|error| Error::io("create portable package stage", error))?;
        let prefix = package.join("prefix");
        self.mirror_to(&root.join("prefix"), &prefix)?;
        let fingerprint = crate::cache::tree_fingerprint(&prefix)?;
        if fingerprint != receipt.fingerprint {
            return Err(invalid(
                "source installation changed while exporting its portable snapshot",
            ));
        }
        reject_prefix_references(&prefix, &root.join("prefix"))?;
        let probe = stage.path().join("probe");
        installed(metadata, &prefix, &probe, toolchain)?;
        if crate::cache::tree_fingerprint(&prefix)? != fingerprint {
            return Err(invalid(
                "portable metadata evaluation modified the exported installation",
            ));
        }
        storage::atomic_json(
            &package.join(DESCRIPTOR),
            &Package {
                format: 1,
                metadata: metadata.clone(),
                files: portable_inventory(&prefix, metadata)?,
            },
        )?;
        fs::rename(&package, destination)
            .map_err(|error| Error::io("publish portable artifact package", error))
    }
}

pub(super) fn install(request: source::Request<'_>) -> Result<(String, Installation)> {
    let expected = request.source.prebuilt.as_ref().expect("prebuilt request");
    expected.validate()?;
    if expected.target != request.node.target {
        return Err(invalid(
            "prebuilt target does not match the selected graph role/target",
        ));
    }
    if !request.dependencies.is_empty() || !request.host_tools.is_empty() {
        return Err(invalid(
            "portable artifacts are self-contained; external build dependencies belong in source recipes",
        ));
    }
    let parent = request.manifest.parent().expect("absolute manifest");
    let original = match &request.source.path {
        Some(path) => std::path::absolute(parent.join(path))
            .map_err(|error| Error::io("resolve local artifact package", error))?,
        None => request.cache.source(
            request
                .pinned
                .ok_or_else(|| invalid("prebuilt archive lock is missing"))?,
        )?,
    };
    let package_bytes = fs::read(original.join(DESCRIPTOR))
        .map_err(|error| Error::io("read verified prebuilt descriptor", error))?;
    let package: Package = serde_json::from_slice(&package_bytes)
        .map_err(|error| invalid(format!("invalid prebuilt descriptor: {error}")))?;
    validate(&package.metadata)?;
    if package.format != 1 || &package.metadata.compatibility != expected {
        return Err(invalid(
            "prebuilt compatibility metadata differs from the explicit manifest contract",
        ));
    }
    let input_prefix = original.join("prefix");
    if package.files.is_empty()
        || !matches_inventory(
            &portable_inventory(&input_prefix, &package.metadata)?,
            &package.files,
        )
    {
        return Err(invalid(
            "prebuilt file content/type/permissions do not match its complete inventory",
        ));
    }
    let recipe = request.source.build.as_ref();
    if let Some(recipe) = recipe
        && (serde_json::to_value((!recipe.cmake.is_empty()).then(|| recipe.package()))
            .map_err(|error| invalid(error.to_string()))?
            != serde_json::to_value(&package.metadata.cmake)
                .map_err(|error| invalid(error.to_string()))?
            || recipe.tools != package.metadata.tools
            || recipe.linkage != expected.linkage
            || recipe
                .msvc_runtime
                .is_some_and(|runtime| Some(runtime) != expected.msvc_runtime))
    {
        return Err(invalid(
            "prebuilt metadata recipe differs from its package descriptor",
        ));
    }
    let toolchain = recipe
        .and_then(|recipe| recipe.toolchain.as_ref())
        .map(|path| parent.join(path));
    if expected.target != request.host && toolchain.is_none() && package.metadata.cmake.is_some() {
        return Err(invalid(
            "cross-target prebuilt metadata requires an explicit build.toolchain; no host configuration fallback",
        ));
    }
    if expected.target.ends_with("-msvc") {
        let required = recipe
            .and_then(|recipe| recipe.msvc_runtime)
            .unwrap_or_else(|| {
                if request.node.role == BuildRole::Target && crate::crt_static() {
                    crate::project::MsvcCrt::Static
                } else {
                    crate::project::MsvcCrt::Dynamic
                }
            });
        if expected.msvc_runtime != Some(required) {
            return Err(invalid(
                "prebuilt MSVC runtime differs from the consuming graph role",
            ));
        }
    }
    println!("cargo:rerun-if-changed={}", original.display());
    let runtime = expected.msvc_runtime.map(|runtime| match runtime {
        crate::project::MsvcCrt::Static => "MultiThreaded",
        crate::project::MsvcCrt::Dynamic => "MultiThreadedDLL",
    });
    let mut fields = BTreeMap::from([
        ("target".into(), field_hash(&expected.target)?),
        ("configuration".into(), field_hash(&expected.configuration)?),
        ("runtime".into(), field_hash(&runtime)?),
        ("package".into(), field_hash(&package_bytes)?),
        ("namespace".into(), field_hash(&request.manifest)?),
        ("declaration".into(), field_hash(&request.node.id)?),
    ]);
    fields.insert(
        "implementation".into(),
        field_hash(&concat!(
            include_str!("portable.rs"),
            include_str!("storage.rs"),
            include_str!("../providers/implementation.rs"),
            include_str!("../providers/metadata.cmake")
        ))?,
    );
    for variable in ENVIRONMENT {
        println!("cargo:rerun-if-env-changed={variable}");
        fields.insert(
            format!("env:{variable}"),
            field_hash(&std::env::var_os(variable).map(|value| value.as_encoded_bytes().to_vec()))?,
        );
    }
    for tool in ["cmake", "ninja", "cl", "cc", "c++"] {
        if let Some(path) = crate::find_on_path(tool) {
            println!("cargo:rerun-if-changed={}", path.display());
            fields.insert(
                format!("tool:{tool}"),
                field_hash(&(path.clone(), crate::sha256_file(&path)?))?,
            );
        }
    }
    if let Some(recipe) = recipe {
        for path in &recipe.inputs {
            let path = parent.join(path);
            println!("cargo:rerun-if-changed={}", path.display());
            fields.insert(
                format!("input:{}", path.display()),
                field_hash(&crate::sha256_file(&path)?)?,
            );
        }
    }
    if let Some(toolchain) = &toolchain {
        println!("cargo:rerun-if-changed={}", toolchain.display());
        fields.insert(
            "toolchain".into(),
            field_hash(&(toolchain, crate::sha256_file(toolchain)?))?,
        );
    }
    let key = BuildKey::explicit("prebuilt-install")
        .mix(serde_json::to_vec(&fields).map_err(|error| invalid(error.to_string()))?);
    let mut artifact = storage::Artifact::open(request.cache, &key)?;
    if let Some(installation) = artifact.cached()? {
        diagnostics::completed(
            request.cache,
            request.manifest,
            request.node,
            &key.fingerprint(),
            None,
            BuildOutcome::Reused,
            fields,
        )?;
        return Ok((key.fingerprint(), installation));
    }
    diagnostics::starting(
        request.cache,
        request.manifest,
        request.node,
        &fields,
        artifact.repaired,
    )?;
    let stage = artifact.begin()?;
    request.cache.mirror_to(&input_prefix, &stage)?;
    if !matches_inventory(
        &portable_inventory(&stage, &package.metadata)?,
        &package.files,
    ) || !matches_inventory(
        &portable_inventory(&input_prefix, &package.metadata)?,
        &package.files,
    ) {
        return Err(invalid(
            "prebuilt inputs changed while preparing installation",
        ));
    }
    artifact.publish(request.cache, &stage)?;
    let installation = installed(
        &package.metadata,
        &artifact.prefix,
        &artifact.metadata,
        toolchain.as_deref(),
    )?;
    if !matches_inventory(
        &portable_inventory(&input_prefix, &package.metadata)?,
        &package.files,
    ) {
        return Err(invalid("prebuilt input changed during metadata evaluation"));
    }
    if let Some(dep) = request.pinned {
        request.cache.verify_source(dep)?;
    }
    let installation = artifact.commit(
        installation,
        &[original, artifact.stage.clone(), artifact.metadata.clone()],
        fields.clone(),
    )?;
    diagnostics::completed(
        request.cache,
        request.manifest,
        request.node,
        &key.fingerprint(),
        None,
        if artifact.repaired {
            BuildOutcome::Repaired
        } else {
            BuildOutcome::Imported
        },
        fields,
    )?;
    Ok((key.fingerprint(), installation))
}

use sha2::Digest;

fn field_hash(value: &impl Serialize) -> Result<String> {
    Ok(format!(
        "{:x}",
        sha2::Sha256::digest(
            serde_json::to_vec(value).map_err(|error| invalid(error.to_string()))?
        )
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_inventory_uses_platform_independent_paths_and_records_empty_directories() {
        let directory = tempfile::tempdir().unwrap();
        let prefix = directory.path();
        fs::create_dir_all(prefix.join("include").join("empty")).unwrap();
        fs::write(
            prefix.join("include").join("sample.h"),
            "int sample(void);\n",
        )
        .unwrap();
        let metadata = ArtifactMetadata {
            compatibility: PrebuiltCompatibility {
                target: "aarch64-unknown-linux-gnu".into(),
                abi: "c-api-v1".into(),
                configuration: "Release".into(),
                linkage: PackageLinkage::Static,
                msvc_runtime: None,
            },
            cmake: None,
            tools: Vec::new(),
        };
        let entries = portable_inventory(prefix, &metadata).unwrap();
        assert_eq!(entries["include/empty"], PortableEntry::Directory);
        assert!(entries.contains_key("include/sample.h"));
        let serialized = serde_json::to_string(&entries).unwrap();
        assert!(!serialized.contains('\\'));
        let expected = serde_json::from_str(&serialized).unwrap();
        assert!(matches_inventory(&entries, &expected));
        fs::write(
            prefix.join("include").join("sample.h"),
            "int other_(void);\n",
        )
        .unwrap();
        assert!(!matches_inventory(
            &portable_inventory(prefix, &metadata).unwrap(),
            &expected
        ));
        fs::remove_dir(prefix.join("include").join("empty")).unwrap();
        assert!(
            !portable_inventory(prefix, &metadata)
                .unwrap()
                .contains_key("include/empty")
        );
    }

    #[test]
    fn prefix_references_are_rejected_in_text_and_binary_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let prefix = directory.path().join("prefix");
        fs::create_dir(&prefix).unwrap();
        fs::write(prefix.join("header.h"), "int sample(void);\n").unwrap();
        reject_prefix_references(&prefix, &prefix).unwrap();
        fs::write(
            prefix.join("header.h"),
            format!("#define PREFIX \"{}\"", prefix.display()),
        )
        .unwrap();
        assert!(reject_prefix_references(&prefix, &prefix).is_err());
        fs::write(prefix.join("header.h"), "int sample(void);\n").unwrap();
        let bytes = prefix
            .to_string_lossy()
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        fs::write(prefix.join("binary"), bytes).unwrap();
        assert!(reject_prefix_references(&prefix, &prefix).is_err());
    }

    #[test]
    fn compatibility_requires_an_explicit_abi_and_crt_contract() {
        let mut metadata = ArtifactMetadata {
            compatibility: PrebuiltCompatibility {
                target: "x86_64-pc-windows-msvc".into(),
                abi: "c-runtime-contract".into(),
                configuration: "Release".into(),
                linkage: PackageLinkage::Static,
                msvc_runtime: Some(crate::project::MsvcCrt::Dynamic),
            },
            cmake: None,
            tools: vec!["generator".into()],
        };
        validate(&metadata).unwrap();
        metadata.compatibility.abi.clear();
        assert!(validate(&metadata).is_err());
        metadata.compatibility.abi = "explicit".into();
        metadata.compatibility.msvc_runtime = None;
        assert!(validate(&metadata).is_err());
        metadata.compatibility.target = "aarch64-unknown-linux-gnu".into();
        validate(&metadata).unwrap();
        metadata.tools = vec!["../escape".into()];
        assert!(validate(&metadata).is_err());
    }
}
