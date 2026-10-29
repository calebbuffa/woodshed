//! User dependency intent (`woodshed.toml`) and generated immutable lockfiles.
//!
//! Build scripts only read and verify locks. Resolution belongs to explicit
//! maintainer commands; checksums and resolved commits never need to be typed.
use crate::{Dep, Error, Resolution, Result, dep::SourcePins};
#[cfg(feature = "pin")]
use crate::{GitSource, Upstream};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub(crate) fn invalid(detail: impl Into<String>) -> Error {
    Error::Manifest {
        detail: detail.into(),
    }
}

/// Source selection and optional dependency-discovery policy.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Source {
    /// Explicit compatibility contract for a verified installed artifact, not source compilation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prebuilt: Option<PrebuiltCompatibility>,
    /// Mutable local source directory, relative to woodshed.toml.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Native recipe features to enable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// Enable the recipe's default native features.
    #[serde(
        default = "yes",
        rename = "default-features",
        skip_serializing_if = "is_true"
    )]
    pub default_features: bool,
    /// Shared graph selection options.
    #[serde(flatten)]
    pub selection: DependencyOptions,
    /// Requested release label. Dependent sources omit this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// GitHub source release (`owner/repository`), using an immutable commit archive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub github: Option<String>,
    /// Git repository to provision as an immutable snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    /// Explicit archive URL template for other release hosts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Tag/ref template, e.g. `v{version}`; resolved only by lock/update.
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// A moving branch to resolve when explicitly updating the lock.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Select an explicit tag instead of a ref template.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Select a full immutable Git revision directly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// Override archive cache filename.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Required files in the provisioned source.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub markers: Vec<String>,
    /// Once-declared upstream dependent-source discovery.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolve: Option<Resolution>,
    /// Optional declarative source build; omit when using the low-level Rust API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<Box<CmakeRecipe>>,
}

impl Default for Source {
    fn default() -> Self {
        Self {
            prebuilt: None,
            path: None,
            features: Vec::new(),
            default_features: true,
            selection: DependencyOptions::default(),
            version: None,
            github: None,
            git: None,
            url: None,
            reference: None,
            branch: None,
            tag: None,
            rev: None,
            file: None,
            markers: Vec::new(),
            resolve: None,
            build: None,
        }
    }
}

/// Compatibility asserted by a prebuilt publisher and explicitly selected by the consumer.
/// ABI labels are opaque declarations, never guessed from filenames.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrebuiltCompatibility {
    /// Exact Rust target triple.
    pub target: String,
    /// Publisher-defined compiler/standard-library/system ABI contract.
    pub abi: String,
    /// Exact native build configuration.
    pub configuration: String,
    /// Root library linkage.
    pub linkage: PackageLinkage,
    /// Required MSVC runtime, mandatory for MSVC targets and absent elsewhere.
    #[serde(
        default,
        rename = "msvc-runtime",
        skip_serializing_if = "Option::is_none"
    )]
    pub msvc_runtime: Option<MsvcCrt>,
}

impl PrebuiltCompatibility {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.target.trim().is_empty()
            || self.abi.trim().is_empty()
            || self.configuration != "Release"
            || self.target.ends_with("-msvc") != self.msvc_runtime.is_some()
        {
            return Err(invalid(
                "prebuilt compatibility requires target, explicit ABI, Release configuration and an MSVC runtime exactly for MSVC targets",
            ));
        }
        Ok(())
    }
}

/// Build and discover installed exports from a CMake source project.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CmakeRecipe {
    /// Exported CMake CONFIG package name, not a linker filename.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cmake: String,
    /// Ordered exported CMake targets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    /// Installed executables under bin/, without platform-specific .exe suffix.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// Linkage requested from the project and exported targets.
    #[serde(default)]
    pub linkage: PackageLinkage,
    /// Project-specific CMake cache options.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub defines: BTreeMap<String, String>,
    /// Explicit toolchain file relative to woodshed.toml.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<PathBuf>,
    /// Additional local files whose bytes affect the build, relative to the manifest.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<PathBuf>,
    /// Additional inherited environment variables affecting this recipe's output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    /// Native features mapped to explicit project-specific CMake definitions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub features: BTreeMap<String, BTreeMap<String, String>>,
    /// Recipe features enabled unless default-features is false.
    #[serde(
        default,
        rename = "default-features",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub default_features: Vec<String>,
    /// Explicit incremental reconfiguration contract; absent means isolated builds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspacePolicy>,
    /// Optional MSVC CRT choice; target defaults match Rust, host tools default dynamic.
    #[serde(
        default,
        rename = "msvc-runtime",
        skip_serializing_if = "Option::is_none"
    )]
    pub msvc_runtime: Option<MsvcCrt>,
}

/// Explicit MSVC CRT policy, independent of static/shared library linkage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MsvcCrt {
    /// Statically linked MSVC runtime.
    Static,
    /// Shared MSVC runtime.
    Dynamic,
}

/// Package opt-in to compatible CMake reconfiguration, never artifact compatibility.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePolicy {
    /// Grouping label within this manifest and dependency, not a global cache key.
    pub name: String,
    /// Effective project-specific definitions allowed to vary within a workspace.
    #[serde(default)]
    pub reconfigure: Vec<String>,
    /// Named native prerequisites allowed to change; host tools remain hard inputs.
    #[serde(default)]
    pub dependencies: Vec<String>,
}

impl CmakeRecipe {
    pub(crate) fn package(&self) -> CmakePackage {
        CmakePackage {
            package: self.cmake.clone(),
            targets: self.targets.clone(),
            linkage: self.linkage,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.cmake.is_empty() && self.targets.is_empty() {
            if self.tools.is_empty() {
                return Err(invalid(
                    "source build requires CMake package/targets or installed tools",
                ));
            }
        } else {
            self.package().validate()?;
        }
        if self.tools.iter().any(|tool| {
            tool.is_empty()
                || !tool
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte))
                || tool == "."
                || tool == ".."
        }) {
            return Err(invalid(
                "build.tools must name executables under bin/, not paths or commands",
            ));
        }
        for key in self.defines.keys() {
            if key.is_empty()
                || !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                || key.starts_with("CMAKE_")
                || key == "BUILD_SHARED_LIBS"
                || matches!(
                    key.as_str(),
                    "FETCHCONTENT_FULLY_DISCONNECTED" | "FETCHCONTENT_UPDATES_DISCONNECTED"
                )
            {
                return Err(invalid(format!(
                    "build.defines.{key}: use project-specific options; Woodshed owns CMAKE_*, BUILD_SHARED_LIBS and FetchContent network policy (use linkage/toolchain or a custom low-level integration)"
                )));
            }
        }
        if self
            .toolchain
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(invalid(
                "build.toolchain must name an existing toolchain file",
            ));
        }
        if self.inputs.iter().any(|path| path.as_os_str().is_empty()) {
            return Err(invalid(
                "build.inputs must contain file paths, not empty entries",
            ));
        }
        if self.env.iter().any(|name| {
            name.is_empty()
                || !name.bytes().enumerate().all(|(index, byte)| {
                    byte == b'_'
                        || byte.is_ascii_alphabetic()
                        || (index > 0 && byte.is_ascii_digit())
                })
        }) {
            return Err(invalid(
                "build.env must name environment variables, not assignments",
            ));
        }
        if let Some(workspace) = &self.workspace {
            if workspace.name.is_empty()
                || !workspace
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
            {
                return Err(invalid(
                    "build.workspace.name must be a nonempty grouping identifier",
                ));
            }
            for key in &workspace.reconfigure {
                let mut check = self.clone();
                check.workspace = None;
                check.defines = BTreeMap::from([(key.clone(), String::new())]);
                check.validate()?;
            }
        }
        Ok(())
    }
}

/// A package managed by an existing native package manager.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "provider",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case",
    deny_unknown_fields
)]
pub enum Package {
    /// A vcpkg port, installed using an immutable registry baseline.
    Vcpkg {
        /// Shared graph selection options.
        #[serde(flatten)]
        selection: DependencyOptions,
        /// Port name.
        package: String,
        /// Exact port version (not a range).
        version: String,
        /// Explicit target triplet; never inferred from the host.
        triplet: String,
        /// Optional exact builtin registry baseline; otherwise lock current root HEAD.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        baseline: Option<String>,
        /// Port features.
        #[serde(default)]
        features: Vec<String>,
        /// Include the port's default features.
        #[serde(default = "yes")]
        #[serde(rename = "default-features")]
        default_features: bool,
        /// Exported CMake package and targets, used without writing CMake.
        cmake: CmakePackage,
    },
    /// Conan 2 package, using explicit host/build profile files.
    Conan {
        /// Shared graph selection options.
        #[serde(flatten)]
        selection: DependencyOptions,
        /// Exact Conan reference, e.g. `zlib/1.3.1`.
        package: String,
        /// Host profile path relative to woodshed.toml.
        host_profile: PathBuf,
        /// Build profile path relative to woodshed.toml.
        build_profile: PathBuf,
        /// Conan options such as `zlib/*:shared=False`.
        #[serde(default)]
        options: Vec<String>,
        /// Permit builds of missing packages (otherwise binaries only).
        #[serde(default)]
        build_missing: bool,
        /// Exported CMake package and targets.
        cmake: CmakePackage,
    },
}

fn yes() -> bool {
    true
}

/// Native package export metadata, not a handwritten CMake project.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CmakePackage {
    /// `find_package` CONFIG name.
    pub package: String,
    /// Imported target names to link, in order.
    pub targets: Vec<String>,
    /// Explicit library linkage, also used to validate Rust's link directives.
    #[serde(default)]
    pub linkage: PackageLinkage,
}

impl CmakePackage {
    pub(crate) fn validate(&self) -> Result<()> {
        let identifier = |value: &str| {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.:+-".contains(&byte))
        };
        if !identifier(&self.package)
            || self.targets.is_empty()
            || self.targets.iter().any(|target| !identifier(target))
        {
            return Err(invalid(
                "cmake package and nonempty targets must be CMake identifiers, not executable CMake code",
            ));
        }
        Ok(())
    }
}

/// Library linkage expected from a package.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PackageLinkage {
    /// Statically linked package libraries.
    #[default]
    Static,
    /// Dynamically linked package libraries.
    Dynamic,
}

/// Either a source declaration or an explicitly selected package provider.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Dependency {
    /// Build sources through the binding crate's build integration.
    Source(Box<Source>),
    /// Use a native package manager's own package/build recipes.
    Package(Box<Package>),
}

/// Options shared by every source and package provider.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DependencyOptions {
    /// Activate only through a feature or an explicit dependency edge.
    #[serde(skip_serializing_if = "is_false")]
    pub optional: bool,
    /// Source-recipe prerequisites, named in this manifest. Providers own their graphs.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub depends: Vec<String>,
    /// Source prerequisites only when independently activated by features/required edges.
    #[serde(rename = "optional-depends", skip_serializing_if = "Vec::is_empty")]
    pub optional_depends: Vec<String>,
    /// Build for the host rather than the Rust target (e.g. code generators).
    #[serde(default, skip_serializing_if = "BuildRole::is_target")]
    pub role: BuildRole,
}

fn is_false(value: &bool) -> bool {
    !value
}

fn is_true(value: &bool) -> bool {
    *value
}

/// Whether native outputs run on the build host or link into the Rust target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuildRole {
    /// Libraries for Cargo's TARGET.
    #[default]
    Target,
    /// Tools/libraries executed on Cargo's HOST.
    Host,
}

impl BuildRole {
    fn is_target(&self) -> bool {
        *self == Self::Target
    }
}

impl Dependency {
    /// Provider-independent selection and graph policy.
    pub fn selection(&self) -> &DependencyOptions {
        match self {
            Self::Source(source) => &source.selection,
            Self::Package(package) => package.selection(),
        }
    }

    /// Mutable provider-independent selection policy.
    pub fn selection_mut(&mut self) -> &mut DependencyOptions {
        match self {
            Self::Source(source) => &mut source.selection,
            Self::Package(package) => match package.as_mut() {
                Package::Vcpkg { selection, .. } | Package::Conan { selection, .. } => selection,
            },
        }
    }
}

/// Dependencies enabled only for one target triple or Rust cfg expression.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TargetDependencies {
    /// Target-conditional native dependencies.
    pub dependencies: BTreeMap<String, Dependency>,
}

/// User-facing dependency declarations.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Declarations {
    /// Source/package selections keyed by local dependency name.
    pub dependencies: BTreeMap<String, Dependency>,
    /// Familiar feature names expanding to feature names or `dep:name`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub features: BTreeMap<String, Vec<String>>,
    /// Target-conditional declarations, all pinned by the same lock.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub target: BTreeMap<String, TargetDependencies>,
}

/// Provider-owned immutable graph and recipe data.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageLock {
    /// Hash of the declaration and profile contents used for resolution.
    pub input: String,
    /// Provider-specific locked graph. Not interpreted by source builds.
    pub data: String,
}

/// Generated lockfile; commit this alongside woodshed.toml.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    /// Woodshed lock schema.
    pub format: u32,
    /// Semantic declaration digest. Formatting/comments do not invalidate locks.
    pub input: String,
    /// Fully resolved source pins keyed by unique declaration/target ID.
    /// `Dep::name` contains the original dependency name, without the target ID suffix.
    #[serde(default)]
    pub sources: BTreeMap<String, Dep>,
    /// Fully resolved package-provider locks.
    #[serde(default)]
    pub packages: BTreeMap<String, PackageLock>,
}

/// A declaration file, loaded without resolving or accessing the network.
pub struct Project {
    /// Path to woodshed.toml.
    pub path: PathBuf,
    /// Parsed user intent.
    pub declarations: Declarations,
    /// Ancestor workspace declarations used by inherited dependencies.
    /// Build integrations must watch these paths as well as this manifest.
    pub workspace_inputs: BTreeMap<PathBuf, String>,
    #[cfg(feature = "pin")]
    pub(crate) original: Option<String>,
}

impl Project {
    /// Parse and validate an existing declaration file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|error| Error::io("read woodshed.toml", error))?;
        Self::parse(path, &text)
    }

    /// Parse declarations without reading a lock or contacting a server.
    /// Explicit inherited entries may read ancestor workspace declarations.
    pub fn parse(path: impl AsRef<Path>, text: &str) -> Result<Self> {
        let (declarations, workspace_inputs) = inheritance::declarations(path.as_ref(), text)?;
        Ok(Self {
            path: path.as_ref().into(),
            declarations,
            workspace_inputs,
            #[cfg(feature = "pin")]
            original: Some(text.into()),
        })
    }

    /// Standard adjacent generated lock path.
    pub fn lock_path(&self) -> PathBuf {
        self.path.with_file_name("woodshed.lock")
    }

    /// Read a complete matching lock. This never resolves missing information.
    pub fn locked(&self) -> Result<Lock> {
        let path = self.lock_path();
        let text = std::fs::read_to_string(&path).map_err(|error| {
            Error::io(
                format!(
                    "read {}; run `woodshed lock --manifest {}`",
                    path.display(),
                    self.path.display()
                ),
                error,
            )
        })?;
        let lock = self.parse_lock(&text)?;
        for (name, dependency) in self.declarations.all_dependencies() {
            if let Dependency::Package(package) = dependency {
                let expected = package.input(&self.path)?;
                if lock.packages[&name].input != expected {
                    return Err(invalid(format!(
                        "{name} provider/profile inputs changed; run woodshed lock"
                    )));
                }
            }
        }
        Ok(lock)
    }

    /// Validate lock text against these declarations, including immutable source
    /// identities and normalized dependency names. For embedded manifests use
    /// `Project::parse(path, declarations)?.parse_lock(lock_text)?`.
    ///
    /// Unlike `locked`, this does not read or validate provider profile files.
    pub fn parse_lock(&self, text: &str) -> Result<Lock> {
        let mut lock: Lock = toml::from_str(text)
            .map_err(|error| invalid(format!("invalid woodshed.lock: {error}")))?;
        if lock.format != 1 || lock.input != self.declarations.digest()? {
            return Err(invalid(
                "woodshed.lock does not match dependency declarations; run `woodshed lock` (builds never update it)",
            ));
        }
        let expected_sources = self
            .declarations
            .all_dependencies()
            .into_iter()
            .filter_map(|(name, dep)| {
                matches!(dep, Dependency::Source(source) if source.path.is_none()).then_some(name)
            })
            .collect::<Vec<_>>();
        let expected_packages = self
            .declarations
            .all_dependencies()
            .into_iter()
            .filter_map(|(name, dep)| matches!(dep, Dependency::Package(_)).then_some(name))
            .collect::<Vec<_>>();
        if lock.sources.keys().cloned().collect::<Vec<_>>() != expected_sources
            || lock.packages.keys().cloned().collect::<Vec<_>>() != expected_packages
        {
            return Err(invalid(
                "woodshed.lock has missing or unexpected dependency entries",
            ));
        }
        let sources = SourcePins::parse(
            &toml::to_string(&lock.sources).map_err(|error| invalid(error.to_string()))?,
        )?;
        for dep in sources.iter() {
            let Dependency::Source(source) = self.declarations.all_dependencies()[&dep.name] else {
                return Err(invalid("source lock points to a package declaration"));
            };
            if source.resolve != dep.resolve
                || source.markers != dep.markers
                || (source.resolve.is_none()
                    && source
                        .version
                        .as_ref()
                        .is_some_and(|version| version != &dep.version))
            {
                return Err(invalid(format!(
                    "{} locked source policy/version does not match declarations",
                    dep.name
                )));
            }
            if let Some(repository) = &source.git {
                if dep
                    .git
                    .as_ref()
                    .is_none_or(|git| &git.repository != repository)
                {
                    return Err(invalid(format!(
                        "{} lock does not match selected Git repository",
                        dep.name
                    )));
                }
            } else if let Some(github) = &source.github {
                let repository = github_repository(github)?;
                if dep.git.is_some()
                    || dep.upstream.as_ref().is_none_or(|upstream| {
                        upstream.repository != repository
                            || dep.url
                                != format!(
                                    "https://github.com/{github}/archive/{}.tar.gz",
                                    upstream.revision
                                )
                    })
                {
                    return Err(invalid(format!(
                        "{} lock does not match selected GitHub source archive",
                        dep.name
                    )));
                }
                if let Some(expected) = &source.rev {
                    let revision = dep
                        .git
                        .as_ref()
                        .map(|git| &git.revision)
                        .or_else(|| dep.upstream.as_ref().map(|upstream| &upstream.revision));
                    if revision != Some(expected) {
                        return Err(invalid(format!(
                            "{} locked revision does not match declared rev",
                            dep.name
                        )));
                    }
                }
            } else if let Some(url) = &source.url
                && (dep.git.is_some()
                    || dep.url
                        != url
                            .replace("{version}", &dep.version)
                            .replace("{name}", &dep.name))
            {
                return Err(invalid(format!(
                    "{} lock does not match selected archive URL",
                    dep.name
                )));
            }
            if dep.git.is_none()
                && (dep.sha256.len() != 64
                    || !dep.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()))
            {
                return Err(invalid(format!(
                    "{} lock has invalid archive checksum",
                    dep.name
                )));
            }
        }
        for name in self.declarations.dependencies.keys() {
            if let Some(dep) = lock.sources.get_mut(name) {
                dep.name.clone_from(name);
            }
        }
        for (condition, table) in &self.declarations.target {
            for name in table.dependencies.keys() {
                if let Some(dep) = lock
                    .sources
                    .get_mut(&Declarations::target_id(condition, name))
                {
                    dep.name.clone_from(name);
                }
            }
        }
        Ok(lock)
    }
}

mod inheritance;

impl Declarations {
    /// Public feature names, including unsuppressed implicit optional features.
    pub fn feature_names(&self) -> std::collections::BTreeSet<String> {
        let suppressed = self
            .features
            .values()
            .flatten()
            .filter_map(|entry| entry.strip_prefix("dep:"))
            .collect::<std::collections::BTreeSet<_>>();
        let mut names = self
            .features
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        for dependencies in std::iter::once(&self.dependencies)
            .chain(self.target.values().map(|table| &table.dependencies))
        {
            names.extend(
                dependencies
                    .iter()
                    .filter(|(name, dep)| {
                        dep.selection().optional && !suppressed.contains(name.as_str())
                    })
                    .map(|(name, _)| name.clone()),
            );
        }
        names
    }

    /// All declarations, including inactive target variants, under stable lock IDs.
    pub fn all_dependencies(&self) -> BTreeMap<String, &Dependency> {
        let mut dependencies = self
            .dependencies
            .iter()
            .map(|(name, dep)| (name.clone(), dep))
            .collect::<BTreeMap<_, _>>();
        for (condition, table) in &self.target {
            for (name, dependency) in &table.dependencies {
                dependencies.insert(Self::target_id(condition, name), dependency);
            }
        }
        dependencies
    }

    /// Stable lock entry name for a target-specific declaration.
    pub fn target_id(condition: &str, name: &str) -> String {
        let digest = format!("{:x}", Sha256::digest(condition.as_bytes()));
        format!("{name}--target-{}", &digest[..16])
    }

    /// Stable intent fingerprint, independent of TOML layout and comments.
    pub fn digest(&self) -> Result<String> {
        let text = toml::to_string(self).map_err(|error| invalid(error.to_string()))?;
        Ok(format!("{:x}", Sha256::digest(text.as_bytes())))
    }

    fn validate(&self) -> Result<()> {
        let count = self.dependencies.len()
            + self
                .target
                .values()
                .map(|table| table.dependencies.len())
                .sum::<usize>();
        if self.all_dependencies().len() != count {
            return Err(invalid(
                "dependency lock identity collision; rename conflicting declarations",
            ));
        }
        for (name, dependency) in self.all_dependencies() {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
            {
                return Err(invalid(format!("invalid dependency name {name:?}")));
            }
            if let Dependency::Source(source) = dependency {
                if let Some(prebuilt) = &source.prebuilt {
                    prebuilt.validate()?;
                    if !source.selection.depends.is_empty()
                        || !source.selection.optional_depends.is_empty()
                    {
                        return Err(invalid(format!(
                            "{name}: portable prebuilt packages are self-contained; external dependency edges require a source recipe"
                        )));
                    }
                    if !source.features.is_empty()
                        || source.git.is_some()
                        || source.github.is_some()
                        || source.resolve.is_some()
                    {
                        return Err(invalid(format!(
                            "{name}: prebuilt artifacts use an explicit archive URL or local package directory, not Git snapshots or forwarded native features"
                        )));
                    }
                    if let Some(build) = &source.build
                        && (!build.defines.is_empty()
                            || !build.features.is_empty()
                            || !build.default_features.is_empty()
                            || build.workspace.is_some()
                            || !build.env.is_empty())
                    {
                        return Err(invalid(format!(
                            "{name}: prebuilt metadata recipes cannot reconfigure installed binaries"
                        )));
                    }
                }
                if let Some(build) = &source.build {
                    build.validate()?;
                }
                let count = [&source.github, &source.git, &source.url]
                    .into_iter()
                    .filter(|value| value.is_some())
                    .count();
                if let Some(path) = &source.path {
                    if path.as_os_str().is_empty()
                        || count != 0
                        || source.resolve.is_some()
                        || source.version.is_some()
                        || source.file.is_some()
                        || [&source.reference, &source.branch, &source.tag, &source.rev]
                            .iter()
                            .any(|value| value.is_some())
                    {
                        return Err(invalid(format!(
                            "{name}: path sources declare path, not remote version/ref/resolve fields"
                        )));
                    }
                    continue;
                }
                let refs = [&source.reference, &source.branch, &source.tag, &source.rev]
                    .into_iter()
                    .filter(|value| value.is_some())
                    .count();
                if refs > 1 {
                    return Err(invalid(format!(
                        "{name}: select only one of ref, tag, branch, rev"
                    )));
                }
                if let Some(rule) = &source.resolve {
                    if let Resolution::File { revision, url, .. } = rule {
                        revision.validate()?;
                        if let Some(url) = url {
                            url.validate()?;
                        }
                    }
                    if source.github.is_some()
                        || source.git.is_some()
                        || source.version.is_some()
                        || refs > 0
                    {
                        return Err(invalid(format!(
                            "{name}: parent-managed sources declare resolve, not an independent version/repository/ref"
                        )));
                    }
                    match rule {
                        Resolution::Gitlink { .. } if source.url.is_some() => {
                            return Err(invalid(format!(
                                "{name}: gitlink archive selection belongs in resolve.archive"
                            )));
                        }
                        Resolution::File { url, .. } if url.is_none() != source.url.is_some() => {
                            return Err(invalid(format!(
                                "{name}: choose either a file URL capture or a declared URL template"
                            )));
                        }
                        _ => {}
                    }
                } else if count != 1
                    || (source.version.as_deref().is_none_or(str::is_empty)
                        && !(source.git.is_some() && refs == 1))
                {
                    return Err(invalid(format!(
                        "{name}: select version and exactly one of github, git, url"
                    )));
                }
                if source.url.is_some() && refs > 0 {
                    return Err(invalid(format!(
                        "{name}: ref selection requires git or github"
                    )));
                }
                if let Some(github) = &source.github {
                    github_repository(github)?;
                }
                if let Some(revision) = &source.rev {
                    crate::dep::validate_revision(revision)?;
                }
                let mut seen = std::collections::BTreeSet::new();
                let mut node = name.as_str();
                loop {
                    if !seen.insert(node) {
                        return Err(invalid(format!("source resolution cycle at {node}")));
                    }
                    match self.all_dependencies().get(node).copied() {
                        Some(Dependency::Source(source)) => match &source.resolve {
                            Some(rule) => node = rule.parent(),
                            None => break,
                        },
                        _ => {
                            return Err(invalid(format!(
                                "{name}: unknown/non-source resolution parent {node}"
                            )));
                        }
                    }
                }
            } else if let Dependency::Package(package) = dependency {
                package.validate()?;
            }
        }
        self.validate_graph_declarations()?;
        Ok(())
    }
}

mod graph;
pub use graph::{PlanOptions, ResolvedDependency, ResolvedPlan};

pub(crate) fn github_repository(value: &str) -> Result<String> {
    let parts = value.split('/').collect::<Vec<_>>();
    if parts.len() != 2
        || parts.iter().any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
        })
    {
        return Err(invalid("github must be owner/repository, not an asset URL"));
    }
    Ok(format!("https://github.com/{value}.git"))
}

impl Package {
    /// Common dependency graph policy for this provider.
    pub fn selection(&self) -> &DependencyOptions {
        match self {
            Self::Vcpkg { selection, .. } | Self::Conan { selection, .. } => selection,
        }
    }

    /// Provider name, used for feature diagnostics.
    pub fn provider(&self) -> &'static str {
        match self {
            Self::Vcpkg { .. } => "vcpkg",
            Self::Conan { .. } => "conan",
        }
    }

    /// Exported package/target metadata.
    pub fn cmake(&self) -> &CmakePackage {
        match self {
            Self::Vcpkg { cmake, .. } | Self::Conan { cmake, .. } => cmake,
        }
    }

    /// Digest provider intent and profile bytes, not mutable host defaults.
    pub fn input(&self, manifest: &Path) -> Result<String> {
        let mut hash = Sha256::new();
        hash.update(toml::to_string(self).map_err(|error| invalid(error.to_string()))?);
        if let Self::Conan {
            host_profile,
            build_profile,
            ..
        } = self
        {
            for path in [host_profile, build_profile] {
                let path = manifest.parent().unwrap_or(Path::new(".")).join(path);
                hash.update([0]);
                let bytes = std::fs::read(&path).map_err(|error| {
                    Error::io(format!("read Conan profile {}", path.display()), error)
                })?;
                let text = std::str::from_utf8(&bytes)
                    .map_err(|error| invalid(format!("Conan profile must be UTF-8: {error}")))?;
                if text.contains("include(") || text.contains("{{") || text.contains("{%") {
                    return Err(invalid(
                        "locked Conan profiles must be self-contained; includes and Jinja require flattening before locking",
                    ));
                }
                hash.update(bytes);
            }
        }
        Ok(format!("{:x}", hash.finalize()))
    }

    fn validate(&self) -> Result<()> {
        let cmake = self.cmake();
        cmake.validate()?;
        let identifier = |value: &str| {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.:+-".contains(&byte))
        };
        if !identifier(&cmake.package)
            || cmake.targets.is_empty()
            || cmake.targets.iter().any(|target| !identifier(target))
        {
            return Err(invalid(
                "cmake.package and nonempty cmake.targets must be CMake identifiers, not executable CMake code",
            ));
        }
        match self {
            Self::Vcpkg {
                package,
                version,
                triplet,
                baseline,
                features,
                ..
            } => {
                if !identifier(package)
                    || !identifier(triplet)
                    || version.is_empty()
                    || version.contains(['[', ']', '*', ' '])
                    || features.iter().any(|feature| !identifier(feature))
                {
                    return Err(invalid(
                        "vcpkg requires a port, exact version, target triplet, and valid feature names",
                    ));
                }
                if let Some(baseline) = baseline {
                    crate::dep::validate_revision(baseline)?;
                }
            }
            Self::Conan {
                package,
                host_profile,
                build_profile,
                ..
            } => {
                if !package.contains('/')
                    || package.contains(['[', ']', '*', ' '])
                    || package.starts_with('-')
                    || host_profile.as_os_str().is_empty()
                    || build_profile.as_os_str().is_empty()
                {
                    return Err(invalid(
                        "Conan requires an exact package/version reference and explicit host/build profile paths",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Build a resolved source record from declaration intent and a prior lock.
#[cfg(feature = "pin")]
fn seed(name: &str, source: &Source, prior: Option<&Dep>) -> Result<Dep> {
    let version = source
        .version
        .clone()
        .or_else(|| source.branch.clone())
        .or_else(|| source.tag.clone())
        .or_else(|| source.rev.clone())
        .or_else(|| prior.map(|dep| dep.version.clone()))
        .unwrap_or_else(|| "0".repeat(40));
    let mut dep = Dep::new(
        name,
        &version,
        "https://unresolved.invalid/archive",
        "0".repeat(64),
    );
    dep.markers = source.markers.clone();
    dep.resolve = source.resolve.clone();
    dep.file = source.file.clone();
    if source.resolve.is_some() {
        if let Some(prior) = prior {
            dep.version = prior.version.clone();
            dep.url = prior.url.clone();
            dep.sha256 = prior.sha256.clone();
            dep.git = prior.git.clone();
            dep.upstream = prior.upstream.clone();
            if dep.file.is_none() {
                dep.file = prior.file.clone();
            }
        }
        if let Some(url) = &source.url {
            dep.url = url.clone();
        }
    } else if let Some(repository) = &source.git {
        dep.url.clear();
        dep.sha256.clear();
        dep.git = Some(GitSource {
            repository: repository.clone(),
            revision: source.rev.clone().unwrap_or_else(|| "0".repeat(40)),
        });
    } else if let Some(github) = &source.github {
        dep.url = format!("https://github.com/{github}/archive/{{revision}}.tar.gz");
        dep.file = Some(
            source
                .file
                .clone()
                .unwrap_or_else(|| format!("{name}-{{version}}.tar.gz")),
        );
        dep.upstream = Some(Upstream {
            repository: github_repository(github)?,
            revision: source.rev.clone().unwrap_or_else(|| "0".repeat(40)),
            reference: Some(
                source
                    .reference
                    .clone()
                    .unwrap_or_else(|| "v{version}".into()),
            ),
        });
    } else if let Some(url) = &source.url {
        dep.url = url.clone();
    }
    Ok(dep)
}

#[cfg(feature = "pin")]
mod update;
#[cfg(feature = "pin")]
pub use update::{Update, plan_add, plan_bump, plan_lock, plan_relock};

#[cfg(all(test, feature = "pin"))]
mod tests;
