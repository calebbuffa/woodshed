use super::*;
use crate::pin::{Change, Resolver, Selection};
use std::io::Write;
use toml_edit::{DocumentMut, Item};

/// A resolved declaration/lock update. No files change until apply.
///
/// Replacement of each file is atomic. There is no cross-file atomic rename:
/// a crash between replacements leaves a detectable stale lock, never a build
/// using mismatched intent. Ordinary second-write errors roll back declarations.
pub struct Update {
    project: Project,
    original_lock: Option<String>,
    document: String,
    lock: String,
    changes: Vec<Change>,
}

impl Update {
    /// Fully resolved sources changed by this plan.
    pub fn changes(&self) -> &[Change] {
        &self.changes
    }
    /// Preview user declarations.
    pub fn manifest(&self) -> &str {
        &self.document
    }
    /// Preview generated lock.
    pub fn lock(&self) -> &str {
        &self.lock
    }

    /// Persist the complete plan, refusing concurrent edits to either file.
    pub fn apply(self) -> Result<()> {
        let parent = parent(&self.project.path);
        std::fs::create_dir_all(parent)
            .map_err(|error| Error::io("create dependency directory", error))?;
        let _guard =
            crate::cache::Lock::acquire(&crate::cache::lock_path(&self.project.path, "pin"))?;
        let lock_path = self.project.lock_path();
        if read_optional(&self.project.path)? != self.project.original
            || read_optional(&lock_path)? != self.original_lock
        {
            return Err(invalid(
                "declarations or lock changed since planning; rerun instead of overwriting",
            ));
        }
        for (path, contents) in &self.project.workspace_inputs {
            if read_optional(path)?.as_ref() != Some(contents) {
                return Err(invalid(
                    "workspace declarations changed since planning; rerun instead of overwriting",
                ));
            }
        }
        let declarations = stage(&self.project.path, &self.document)?;
        let lock = stage(&lock_path, &self.lock)?;
        let rollback = self
            .project
            .original
            .as_deref()
            .map(|text| stage(&self.project.path, text))
            .transpose()?;
        let changed = self.project.original.as_deref() != Some(&self.document);
        if changed {
            replace(declarations, &self.project.path)?;
        }
        if let Err(error) = replace(lock, &lock_path) {
            if changed {
                let restored = if let Some(rollback) = rollback {
                    replace(rollback, &self.project.path)
                } else {
                    std::fs::remove_file(&self.project.path)
                        .map_err(|error| Error::io("remove uncommitted declarations", error))
                };
                if let Err(restore) = restored {
                    return Err(invalid(format!(
                        "lock write failed: {error}; declaration rollback also failed: {restore}; run woodshed lock before building"
                    )));
                }
            }
            return Err(error);
        }
        Ok(())
    }
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn stage(path: &Path, text: &str) -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new_in(parent(path))
        .map_err(|error| Error::io("stage dependency update", error))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.as_file().sync_all())
        .map_err(|error| Error::io("write dependency update", error))?;
    Ok(file)
}

fn replace(file: tempfile::NamedTempFile, path: &Path) -> Result<()> {
    file.persist(path)
        .map_err(|error| Error::io(format!("replace {}", path.display()), error.error))?;
    Ok(())
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::io(format!("read {}", path.display()), error)),
    }
}

fn serialize(value: &impl Serialize) -> Result<String> {
    toml::to_string(value).map_err(|error| invalid(error.to_string()))
}

fn document(text: &str) -> Result<DocumentMut> {
    text.parse()
        .map_err(|error: toml_edit::TomlError| invalid(error.to_string()))
}

fn merge_table(existing: &mut toml_edit::Table, replacement: &toml_edit::Table) {
    existing.retain(|key, _| replacement.contains_key(key));
    for (key, item) in replacement {
        if let (Some(old), Some(new)) = (
            existing.get_mut(key).and_then(Item::as_table_mut),
            item.as_table(),
        ) {
            merge_table(old, new);
        } else {
            let mut item = item.clone();
            if let (Some(old), Some(new)) = (
                existing.get(key).and_then(Item::as_value),
                item.as_value_mut(),
            ) {
                *new.decor_mut() = old.decor().clone();
            }
            existing.insert(key, item);
        }
    }
}

/// Resolve every declaration, computing checksums and provider locks before writes.
pub fn plan_lock(path: &Path, resolver: &mut impl Resolver) -> Result<Update> {
    let project = Project::load(path)?;
    let roots = project
        .declarations
        .all_dependencies()
        .into_keys()
        .collect::<Vec<_>>();
    resolve(project, None, &roots, resolver)
}

/// Refresh graph/build declarations without resolving or changing any source pins.
///
/// The previous declaration text must match the existing lock. Source selection,
/// dependency IDs and provider inputs must remain unchanged.
pub fn plan_relock(path: &Path, previous: &str) -> Result<Update> {
    let project = Project::load(path)?;
    let prior = Project::parse(path, previous)?;
    let original_lock = std::fs::read_to_string(project.lock_path())
        .map_err(|error| Error::io("read lock for pin-preserving refresh", error))?;
    let mut lock = prior.parse_lock(&original_lock)?;
    let acquisition = |declarations: &Declarations| -> Result<String> {
        let dependencies = declarations
            .all_dependencies()
            .into_iter()
            .map(|(name, dependency)| {
                let mut dependency = dependency.clone();
                *dependency.selection_mut() = DependencyOptions::default();
                if let Dependency::Source(source) = &mut dependency {
                    source.features.clear();
                    source.default_features = true;
                    source.build = None;
                }
                (name, dependency)
            })
            .collect::<BTreeMap<_, _>>();
        serialize(&dependencies)
    };
    if acquisition(&prior.declarations)? != acquisition(&project.declarations)? {
        return Err(invalid(
            "pin-preserving refresh requires unchanged source/provider selection and dependency IDs; use woodshed lock to resolve changed inputs",
        ));
    }
    for (name, dependency) in prior.declarations.all_dependencies() {
        if let Dependency::Package(package) = dependency
            && lock.packages[&name].input != package.input(path)?
        {
            return Err(invalid(format!(
                "{name}: provider/profile inputs changed; cannot preserve its lock"
            )));
        }
    }
    for (name, dependency) in project.declarations.all_dependencies() {
        if let Dependency::Package(package) = dependency {
            lock.packages.get_mut(&name).expect("validated lock").input = package.input(path)?;
        }
    }
    lock.input = project.declarations.digest()?;
    let text = serialize(&lock)?;
    project.parse_lock(&text)?;
    Ok(Update {
        document: project.original.clone().expect("loaded declarations"),
        project,
        original_lock: Some(original_lock),
        lock: text,
        changes: Vec::new(),
    })
}

/// Add a source/package selection and regenerate its complete dependent graph.
pub fn plan_add(
    path: &Path,
    name: &str,
    dependency: Dependency,
    resolver: &mut impl Resolver,
) -> Result<Update> {
    let original = read_optional(path)?;
    let mut project = Project::parse(path, original.as_deref().unwrap_or(""))?;
    project.original = original;
    let prior = if project.original.is_some() {
        Some(project.locked()?)
    } else {
        None
    };
    if project
        .declarations
        .dependencies
        .insert(name.into(), dependency)
        .is_some()
    {
        return Err(invalid(format!("{name} already exists; use woodshed bump")));
    }
    project.declarations.validate()?;
    resolve(project, prior, &[name.into()], resolver)
}

/// Update a root's version/source and all sources managed by that root.
pub fn plan_bump(
    path: &Path,
    name: &str,
    version: &str,
    selection: &Selection,
    resolver: &mut impl Resolver,
) -> Result<Update> {
    let mut project = Project::load(path)?;
    let prior = project.locked()?;
    let dependency = project
        .declarations
        .dependencies
        .get_mut(name)
        .ok_or_else(|| invalid(format!("no dependency {name}")))?;
    match dependency {
        Dependency::Source(source) => {
            if source.path.is_some() {
                return Err(invalid("local path sources have no remote version to bump"));
            }
            if source.resolve.is_some() {
                return Err(invalid(format!(
                    "{name} is parent-managed; bump its parent"
                )));
            }
            source.version = Some(version.into());
            if selection.git.is_some() && selection.url.is_some() {
                return Err(invalid("select git or url, not both"));
            }
            if let Some(git) = &selection.git {
                source.git = Some(git.clone());
                source.github = None;
                source.url = None;
                source.file = None;
            }
            if let Some(url) = &selection.url {
                source.url = Some(url.clone());
                source.git = None;
                source.github = None;
                source.reference = None;
                source.branch = None;
                source.tag = None;
                source.rev = None;
            }
            if let Some(reference) = &selection.reference {
                source.reference = Some(reference.clone());
                source.branch = None;
                source.tag = None;
                source.rev = None;
            }
            if let Some(file) = &selection.file {
                source.file = Some(file.clone());
            }
        }
        Dependency::Package(package) => {
            if selection.git.is_some()
                || selection.url.is_some()
                || selection.file.is_some()
                || selection.reference.is_some()
            {
                return Err(invalid(
                    "package provider bumps accept a version, not source overrides",
                ));
            }
            match package.as_mut() {
                Package::Vcpkg {
                    version: current, ..
                } => *current = version.into(),
                Package::Conan { package, .. } => {
                    let (name, _) = package
                        .split_once('/')
                        .ok_or_else(|| invalid("invalid Conan reference"))?;
                    *package = format!("{name}/{version}");
                }
            }
        }
    }
    project.declarations.validate()?;
    resolve(project, Some(prior), &[name.into()], resolver)
}

fn resolve(
    project: Project,
    prior: Option<Lock>,
    roots: &[String],
    resolver: &mut impl Resolver,
) -> Result<Update> {
    let original_lock = read_optional(&project.lock_path())?;
    let mut sources = BTreeMap::new();
    let mut packages = prior
        .as_ref()
        .map(|lock| lock.packages.clone())
        .unwrap_or_default();
    for (name, dependency) in project.declarations.all_dependencies() {
        if let Dependency::Source(source) = dependency {
            if source.path.is_some() {
                continue;
            }
            let previous = prior.as_ref().and_then(|lock| lock.sources.get(&name));
            sources.insert(
                name.clone(),
                if !roots.contains(&name) {
                    previous
                        .cloned()
                        .map(Ok)
                        .unwrap_or_else(|| seed(&name, source, previous))?
                } else {
                    let mut seeded = seed(&name, source, previous)?;
                    if let Some(previous) = previous {
                        seeded.version = previous.version.clone();
                    }
                    seeded
                },
            );
        }
    }
    let mut text = serialize(&sources)?;
    let mut changes = Vec::new();
    for name in roots {
        match project.declarations.all_dependencies()[name] {
            Dependency::Source(source) if source.path.is_some() => {}
            Dependency::Source(source) if source.resolve.is_none() => {
                let reference = source
                    .rev
                    .clone()
                    .or_else(|| {
                        source
                            .branch
                            .as_ref()
                            .map(|branch| format!("refs/heads/{branch}"))
                    })
                    .or_else(|| source.tag.as_ref().map(|tag| format!("refs/tags/{tag}")))
                    .or_else(|| source.reference.clone());
                let pins = SourcePins::parse(&text)?;
                let version = source
                    .version
                    .as_deref()
                    .unwrap_or(&pins.get(name)?.version);
                let selection = Selection {
                    url: source.url.clone(),
                    git: source.git.clone(),
                    reference: reference.map(|reference| reference.replace("{version}", version)),
                    file: source.file.clone(),
                };
                let plan = crate::pin::plan_bump_document(
                    document(&text)?,
                    &pins,
                    name,
                    version,
                    &selection,
                    resolver,
                )?;
                changes.extend_from_slice(plan.changes());
                text = plan.manifest();
            }
            Dependency::Source(_) => {}
            Dependency::Package(package) => {
                let resolved = resolver.package(&project.path, package)?;
                if resolved.input != package.input(&project.path)? {
                    return Err(invalid(format!(
                        "{name}: provider resolved a graph for different declaration/profile inputs"
                    )));
                }
                packages.insert(name.clone(), resolved);
            }
        }
    }
    let sources = SourcePins::parse(&text)?
        .iter()
        .cloned()
        .map(|dep| (dep.name.clone(), dep))
        .collect();
    let lock = Lock {
        format: 1,
        input: project.declarations.digest()?,
        sources,
        packages,
    };
    let lock_text = format!(
        "# Generated by Woodshed. Do not edit checksums or resolved identities by hand.\n{}",
        serialize(&lock)?
    );
    let mut declaration_document = document(project.original.as_deref().unwrap_or(""))?;
    if declaration_document.get("dependencies").is_none() {
        declaration_document["dependencies"] = Item::Table(Default::default());
    }
    if !declaration_document["dependencies"].is_table() {
        declaration_document["dependencies"] = Item::Table(
            declaration_document["dependencies"]
                .clone()
                .into_table()
                .map_err(|_| invalid("dependencies must be a table"))?,
        );
    }
    for (name, dependency) in &project.declarations.dependencies {
        let old = project
            .original
            .as_deref()
            .map(|text| Project::parse(&project.path, text).map(|project| project.declarations))
            .transpose()?;
        if old
            .as_ref()
            .and_then(|value| value.dependencies.get(name))
            .map(serialize)
            .transpose()?
            != Some(serialize(dependency)?)
        {
            if declaration_document
                .get("dependencies")
                .and_then(|dependencies| dependencies.get(name))
                .and_then(|dependency| dependency.get("workspace"))
                .and_then(Item::as_bool)
                == Some(true)
            {
                return Err(invalid(format!(
                    "{name}: edit its workspace.dependencies selection and relock members instead of replacing inherited intent"
                )));
            }
            let replacement = document(&serialize(dependency)?)?.as_table().clone();
            let dependencies = declaration_document["dependencies"]
                .as_table_mut()
                .expect("dependencies table");
            if let Some(existing) = dependencies.get_mut(name).and_then(Item::as_table_mut) {
                merge_table(existing, &replacement);
            } else {
                dependencies[name] = Item::Table(replacement);
            }
        }
    }
    project.parse_lock(&lock_text)?;
    Ok(Update {
        project,
        original_lock,
        document: declaration_document.to_string(),
        lock: lock_text,
        changes,
    })
}
