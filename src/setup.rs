//! Opt-in Cargo wiring. Existing build scripts are never rewritten.
use crate::{
    Error, Result,
    project::{Dependency, Project, invalid},
};
use std::{
    io::Write,
    path::{Path, PathBuf},
};
use toml_edit::{Array, DocumentMut, Item, Table, value};

const BRIDGE: &str = "fn main() -> woodshed::Result<()> {\n    woodshed::build()\n}\n";

/// A previewable, stale-guarded Cargo integration.
pub struct Setup {
    cargo: PathBuf,
    original: String,
    updated: String,
    build: PathBuf,
    original_build: Option<String>,
    inputs: Vec<(PathBuf, String)>,
}

impl Setup {
    /// Preview the resulting Cargo.toml.
    pub fn cargo_toml(&self) -> &str {
        &self.updated
    }

    /// Preview the universal build.rs bridge.
    pub fn build_rs(&self) -> &str {
        BRIDGE
    }

    /// Write only after verifying that neither file changed since planning.
    ///
    /// Each replacement is atomic. An ordinary Cargo.toml replacement failure
    /// removes the newly created bridge. Process interruption can leave an
    /// unconfigured bridge; rerun setup to complete it.
    pub fn apply(self) -> Result<()> {
        let _guard =
            crate::cache::Lock::acquire(&crate::cache::lock_path(&self.cargo, "woodshed"))?;
        if read(&self.cargo)? != self.original || optional(&self.build)? != self.original_build {
            return Err(invalid(
                "Cargo.toml or build.rs changed since setup planning; rerun setup",
            ));
        }
        for (path, original) in &self.inputs {
            if read(path)? != *original {
                return Err(invalid(
                    "declarations or native lock changed since setup planning; rerun setup",
                ));
            }
        }
        let mut cargo =
            tempfile::NamedTempFile::new_in(self.cargo.parent().expect("manifest parent"))
                .map_err(|error| Error::io("stage Cargo setup", error))?;
        cargo
            .write_all(self.updated.as_bytes())
            .map_err(|error| Error::io("write staged Cargo.toml", error))?;
        cargo
            .as_file()
            .sync_all()
            .map_err(|error| Error::io("sync Cargo setup", error))?;
        if self.original_build.is_none() {
            let mut build =
                tempfile::NamedTempFile::new_in(self.build.parent().expect("build parent"))
                    .map_err(|error| Error::io("stage build bridge", error))?;
            build
                .write_all(BRIDGE.as_bytes())
                .map_err(|error| Error::io("write build bridge", error))?;
            build
                .as_file()
                .sync_all()
                .map_err(|error| Error::io("sync build bridge", error))?;
            build
                .persist_noclobber(&self.build)
                .map_err(|error| Error::io("create build.rs without overwriting", error.error))?;
        }
        if let Err(error) = cargo.persist(&self.cargo) {
            if self.original_build.is_none() {
                std::fs::remove_file(&self.build).map_err(|restore| {
                    invalid(format!(
                        "Cargo setup failed: {}; could not remove new bridge: {restore}",
                        error.error
                    ))
                })?;
            }
            return Err(Error::io("replace Cargo.toml", error.error));
        }
        Ok(())
    }
}

/// Plan Cargo wiring for a locked root woodshed.toml or native/woodshed.toml.
///
/// `woodshed_path` selects an existing local Woodshed checkout instead of the
/// registry. Existing scripts, disabled/custom build paths, workspace-inherited
/// dependencies, and conflicting dependency sources require manual integration.
pub fn plan(cargo: &Path, woodshed_path: Option<&Path>) -> Result<Setup> {
    let cargo =
        std::path::absolute(cargo).map_err(|error| Error::io("resolve Cargo manifest", error))?;
    let root = cargo.parent().expect("absolute manifest");
    let original = read(&cargo)?;
    let mut document = original
        .parse::<DocumentMut>()
        .map_err(|error| invalid(error.to_string()))?;
    let package = document
        .get("package")
        .and_then(Item::as_table)
        .ok_or_else(|| invalid("setup needs a package Cargo.toml, not a virtual workspace"))?;
    if let Some(build) = package.get("build")
        && build.as_str() != Some("build.rs")
    {
        return Err(invalid(
            "custom/disabled package.build is preserved; add woodshed::build() and the build dependency manually",
        ));
    }
    let build = root.join("build.rs");
    let original_build = optional(&build)?;
    if original_build.as_deref().is_some_and(|text| text != BRIDGE) {
        return Err(invalid(
            "existing build.rs was not changed; enable Woodshed's build feature and call woodshed::build()? from your existing main",
        ));
    }
    let declarations = if root.join("woodshed.toml").is_file() {
        root.join("woodshed.toml")
    } else {
        root.join("native").join("woodshed.toml")
    };
    let project = Project::load(&declarations)?;
    let lock = project.locked()?;
    let inputs = vec![
        (declarations.clone(), read(&declarations)?),
        (project.lock_path(), read(&project.lock_path())?),
    ];
    let mut features = std::collections::BTreeSet::from(["build".to_owned()]);
    for (name, dependency) in project.declarations.all_dependencies() {
        match dependency {
            Dependency::Source(source) => {
                if source.build.is_none() {
                    return Err(invalid(format!(
                        "{name}: declare a build recipe before setup; existing source-only integrations use the Rust API"
                    )));
                }
                if source.git.is_some() {
                    features.insert("git".into());
                }
            }
            Dependency::Package(package) => {
                features.insert(package.provider().into());
            }
        }
    }
    for dep in lock.sources.values() {
        if dep.git.is_some() {
            features.insert("git".into());
            continue;
        }
        let archive = dep.archive_file();
        let feature = [
            (".tar.gz", "tar-gz"),
            (".tgz", "tar-gz"),
            (".tar.bz2", "tar-bz2"),
            (".tbz2", "tar-bz2"),
            (".tar.xz", "tar-xz"),
            (".txz", "tar-xz"),
            (".zip", "zip"),
        ]
        .into_iter()
        .find(|(suffix, _)| archive.ends_with(suffix))
        .map(|(_, feature)| feature)
        .ok_or_else(|| {
            invalid(format!(
                "{}: unsupported archive filename {archive}; select a supported cached file name",
                dep.name
            ))
        })?;
        features.insert(feature.into());
        features.insert("download".into());
    }
    document["package"]["build"] = value("build.rs");
    let native_features = project.declarations.feature_names();
    if !native_features.is_empty() {
        if document.get("features").is_none() {
            document["features"] = Item::Table(Table::new());
        }
        let cargo_features = document["features"]
            .as_table_mut()
            .ok_or_else(|| invalid("Cargo features must be a table"))?;
        for name in native_features {
            if !cargo_features.contains_key(&name) {
                cargo_features[&name] = value(Array::new());
            }
        }
    }
    if document.get("build-dependencies").is_none() {
        document["build-dependencies"] = Item::Table(Table::new());
    }
    let dependencies = document["build-dependencies"]
        .as_table_mut()
        .ok_or_else(|| invalid("build-dependencies must be a table"))?;
    if dependencies.get("woodshed").is_none() {
        dependencies["woodshed"] = Item::Table(Table::new());
        dependencies["woodshed"]["version"] = value("0.3");
    }
    let dep = &mut dependencies["woodshed"];
    if let Some(version) = dep.as_str().map(str::to_owned) {
        *dep = Item::Table(Table::new());
        dep["version"] = value(version);
    }
    if !dep.is_table_like() {
        return Err(invalid(
            "existing Woodshed dependency must be a version string or table",
        ));
    }
    if dep
        .get("package")
        .and_then(Item::as_str)
        .is_some_and(|package| package != "woodshed")
        || dep.get("optional").and_then(Item::as_bool) == Some(true)
    {
        return Err(invalid(
            "renamed/optional Woodshed dependencies require manual build-script integration",
        ));
    }
    if dep.get("workspace").is_some() || dep.get("git").is_some() || dep.get("registry").is_some() {
        return Err(invalid(
            "workspace/git/custom-registry Woodshed dependency is preserved; enable its build/provider features manually",
        ));
    }
    if dep
        .get("version")
        .and_then(Item::as_str)
        .is_some_and(|version| {
            !["0.3", "^0.3", "0.3.0", "^0.3.0", "0.3.1", "^0.3.1"].contains(&version)
        })
    {
        return Err(invalid(
            "existing Woodshed version is preserved; upgrade to 0.3 explicitly before setup",
        ));
    }
    if let Some(path) = woodshed_path {
        let path = std::path::absolute(path)
            .map_err(|error| Error::io("resolve Woodshed checkout", error))?;
        let manifest = read(&path.join("Cargo.toml"))?;
        let manifest: toml::Value =
            toml::from_str(&manifest).map_err(|error| invalid(error.to_string()))?;
        if manifest["package"]["name"].as_str() != Some("woodshed") {
            return Err(invalid("--woodshed-path must name a Woodshed checkout"));
        }
        let text = path
            .to_str()
            .ok_or_else(|| invalid("Woodshed checkout path must be UTF-8"))?;
        if dep
            .get("path")
            .and_then(Item::as_str)
            .is_some_and(|old| old != text)
        {
            return Err(invalid(
                "existing Woodshed path is preserved; edit it deliberately before setup",
            ));
        }
        dep["path"] = value(text);
    }
    if let Some(existing) = dep.get("features") {
        for feature in existing
            .as_array()
            .ok_or_else(|| invalid("woodshed.features must be an array"))?
        {
            features.insert(
                feature
                    .as_str()
                    .ok_or_else(|| invalid("woodshed.features must contain strings"))?
                    .to_owned(),
            );
        }
    }
    let mut array = toml_edit::Array::new();
    for feature in features {
        array.push(feature);
    }
    dep["features"] = value(array);
    Ok(Setup {
        cargo,
        original,
        updated: document.to_string(),
        build,
        original_build,
        inputs,
    })
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .map_err(|error| Error::io(format!("read {}", path.display()), error))
}

fn optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::io(format!("read {}", path.display()), error)),
    }
}
