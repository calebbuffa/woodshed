//! Maintainer-only source resolution and transactional `add` / `bump`.
//!
//! Builds consume immutable pins. Only explicit maintainer commands resolve
//! floating refs, discover declared dependents, and compute new checksums.
use crate::{Dep, Error, FileMatch, GitSource, Resolution, Result, Upstream, dep::SourcePins};
use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
};
use toml_edit::{DocumentMut, Item, Table, value};

mod offline;
pub use offline::Offline;

fn invalid(detail: impl Into<String>) -> Error {
    Error::Manifest {
        detail: detail.into(),
    }
}

fn expand(template: &str, name: &str, version: &str, revision: &str) -> String {
    template
        .replace("{name}", name)
        .replace("{version}", version)
        .replace("{revision}", revision)
}

/// Download and hash one archive without retaining temporary files.
pub fn sha256_of_url(url: &str) -> Result<String> {
    if std::env::var("WOODSHED_OFFLINE").is_ok_and(|value| value == "1") {
        return Err(Error::Offline {
            detail: format!("cannot hash remote archive {url}; supply an offline Resolver"),
        });
    }
    let temporary =
        tempfile::NamedTempFile::new().map_err(|error| Error::io("create pin download", error))?;
    crate::download::fetch(url, temporary.path())?;
    crate::sha256_file(temporary.path())
}

/// Metadata/download adapter for maintainer plans.
///
/// Implement this for offline fixtures, authenticated metadata services, or
/// alternative Git hosts. Woodshed owns dependency traversal and validation.
pub trait Resolver {
    /// Resolve a requested ref to a full immutable commit ID.
    fn resolve_ref(&mut self, repository: &str, reference: &str) -> Result<String>;
    /// Read one UTF-8 file at an immutable commit.
    fn file(&mut self, repository: &str, revision: &str, path: &str) -> Result<String>;
    /// Read an exact submodule commit and its `.gitmodules` repository.
    fn gitlink(&mut self, repository: &str, revision: &str, path: &str) -> Result<GitSource>;
    /// Compute an archive's checksum from its bytes.
    fn checksum(&mut self, dep: &Dep) -> Result<String>;
    /// Resolve an explicit package-manager graph. Source-only resolvers need not implement this.
    fn package(
        &mut self,
        manifest: &Path,
        package: &crate::project::Package,
    ) -> Result<crate::project::PackageLock> {
        let _ = manifest;
        Err(invalid(format!(
            "{} provider resolution is unavailable in this resolver",
            package.provider()
        )))
    }
}

/// Default resolver: host Git for metadata, normal verified download transport
/// for archive checksums. Git access requires feature `git`.
#[derive(Default)]
pub struct Online {
    #[cfg(feature = "git")]
    repositories: BTreeMap<(String, String), crate::git::Repository>,
    #[cfg(feature = "git")]
    references: BTreeMap<(String, String), String>,
}

impl Online {
    #[cfg(feature = "git")]
    fn snapshot(&mut self, repository: &str, revision: &str) -> Result<&crate::git::Repository> {
        let key = (repository.to_owned(), revision.to_owned());
        if !self.repositories.contains_key(&key) {
            self.repositories.insert(
                key.clone(),
                crate::git::Repository::pinned(repository, revision)?,
            );
        }
        Ok(&self.repositories[&key])
    }
}

#[cfg(not(feature = "git"))]
fn git_disabled() -> Error {
    Error::Tool {
        what: "resolve Git metadata".into(),
        detail: "enable Woodshed's `git` feature".into(),
    }
}

impl Resolver for Online {
    fn resolve_ref(&mut self, repository: &str, reference: &str) -> Result<String> {
        #[cfg(feature = "git")]
        {
            let key = (repository.to_owned(), reference.to_owned());
            if let Some(revision) = self.references.get(&key) {
                return Ok(revision.clone());
            }
            let snapshot = crate::git::Repository::fetch(repository, reference)?;
            let revision = snapshot.revision().to_owned();
            self.repositories
                .insert((repository.into(), revision.clone()), snapshot);
            self.references.insert(key, revision.clone());
            Ok(revision)
        }
        #[cfg(not(feature = "git"))]
        {
            let _ = (repository, reference);
            Err(git_disabled())
        }
    }

    fn file(&mut self, repository: &str, revision: &str, path: &str) -> Result<String> {
        #[cfg(feature = "git")]
        {
            self.snapshot(repository, revision)?.file(path)
        }
        #[cfg(not(feature = "git"))]
        {
            let _ = (repository, revision, path);
            Err(git_disabled())
        }
    }

    fn gitlink(&mut self, repository: &str, revision: &str, path: &str) -> Result<GitSource> {
        #[cfg(feature = "git")]
        {
            self.snapshot(repository, revision)?.gitlink(path)
        }
        #[cfg(not(feature = "git"))]
        {
            let _ = (repository, revision, path);
            Err(git_disabled())
        }
    }

    fn checksum(&mut self, dep: &Dep) -> Result<String> {
        sha256_of_url(&dep.url)
    }

    fn package(
        &mut self,
        manifest: &Path,
        package: &crate::project::Package,
    ) -> Result<crate::project::PackageLock> {
        crate::providers::resolve(manifest, package)
    }
}

/// Source selection for `add` or `bump`.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    /// Override an archive URL, accepting `{name}`, `{version}`, `{revision}`.
    pub url: Option<String>,
    /// Select a Git repository instead of an archive.
    pub git: Option<String>,
    /// Override the upstream ref (resolved now, never during a build).
    pub reference: Option<String>,
    /// Optional cached archive filename template.
    pub file: Option<String>,
}

/// One source changed by a validated update plan.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Change {
    /// Manifest entry name.
    pub name: String,
    /// Selected release/version.
    pub version: String,
    /// Expanded archive URL, or absent for Git.
    pub url: Option<String>,
    /// Archive checksum, or absent for Git.
    pub sha256: Option<String>,
    /// Git source, if selected.
    pub git: Option<GitSource>,
    /// Recorded upstream commit and release policy.
    pub upstream: Option<Upstream>,
}

pub(crate) struct ResolvedSources {
    document: DocumentMut,
    changes: Vec<Change>,
}

impl ResolvedSources {
    pub(crate) fn changes(&self) -> &[Change] {
        &self.changes
    }

    pub(crate) fn manifest(&self) -> String {
        self.document.to_string()
    }
}

fn source_metadata(dep: &Dep) -> Result<(&str, &str)> {
    if let Some(git) = &dep.git {
        return Ok((&git.repository, &git.revision));
    }
    if let Some(upstream) = &dep.upstream {
        return Ok((&upstream.repository, &upstream.revision));
    }
    Err(invalid(format!(
        "`{}` needs upstream Git metadata to resolve dependent sources",
        dep.name
    )))
}

fn capture(
    resolver: &mut impl Resolver,
    repository: &str,
    revision: &str,
    rule: &FileMatch,
) -> Result<String> {
    if rule.path.is_empty()
        || rule.path.contains('\\')
        || rule.path.contains(':')
        || !Path::new(&rule.path)
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
    {
        return Err(invalid(format!(
            "invalid metadata file path {:?}",
            rule.path
        )));
    }
    rule.validate()?;
    let text = resolver.file(repository, revision, &rule.path)?;
    if rule.pattern().is_none() {
        return cmake_metadata::select(&text, rule)
            .map_err(|error| invalid(format!("{}: {error}", rule.path)));
    }
    let regex = regex::Regex::new(rule.pattern().expect("checked above"))
        .map_err(|error| invalid(format!("{}: {error}", rule.path)))?;
    if regex.captures_len() != 2 {
        return Err(invalid(format!(
            "{} pattern must have exactly one capturing group",
            rule.path
        )));
    }
    let matches = regex.captures_iter(&text).collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(invalid(format!(
            "{}: expected exactly one metadata match, found {}",
            rule.path,
            matches.len()
        )));
    }

    matches[0]
        .get(1)
        .filter(|value| !value.as_str().is_empty())
        .map(|value| value.as_str().to_owned())
        .ok_or_else(|| invalid(format!("{} has an empty metadata capture", rule.path)))
}

mod cmake_metadata;

fn serialize_table<T: serde::Serialize>(value: &T) -> Result<Item> {
    let text = toml::to_string(value).map_err(|error| invalid(error.to_string()))?;
    let document = text
        .parse::<DocumentMut>()
        .map_err(|error| invalid(error.to_string()))?;
    Ok(Item::Table(document.as_table().clone()))
}

fn record(table: &mut Table, dep: &Dep, raw_url: &str) -> Result<()> {
    table["version"] = value(&dep.version);
    if let Some(git) = &dep.git {
        table.remove("url");
        table.remove("sha256");
        table.remove("file");
        table["git"] = serialize_table(git)?;
    } else {
        table.remove("git");
        table["url"] = value(raw_url);
        table["sha256"] = value(&dep.sha256);
        if let Some(file) = &dep.file {
            table["file"] = value(file);
        }
    }
    if let Some(upstream) = &dep.upstream {
        table["upstream"] = serialize_table(upstream)?;
    } else {
        table.remove("upstream");
    }
    Ok(())
}

fn retarget(template: &str, old: &str, new: &str, key: &str) -> Result<String> {
    if template.contains("{version}")
        || template.contains("{name}")
        || template.contains("{revision}")
    {
        return Ok(template.into());
    }
    if old != new && !template.contains(old) {
        return Err(invalid(format!(
            "{key} does not contain old version {old:?}; supply an explicit override or template"
        )));
    }
    Ok(template.replace(old, new))
}

pub(crate) fn plan_bump_document(
    document: DocumentMut,
    pins: &SourcePins,
    name: &str,
    version: &str,
    selection: &Selection,
    resolver: &mut impl Resolver,
) -> Result<ResolvedSources> {
    let current = pins.get(name)?;
    if let Some(rule) = &current.resolve {
        return Err(invalid(format!(
            "`{name}` is managed by `{}`; bump its parent instead",
            rule.parent()
        )));
    }
    let table = document
        .get(name)
        .and_then(Item::as_table)
        .ok_or_else(|| invalid(format!("no [{name}] table")))?;
    let mut root = current.clone();
    if version.is_empty() {
        return Err(invalid("version cannot be empty"));
    }
    if selection.url.is_some() && selection.git.is_some() {
        return Err(invalid("choose either --url or --git"));
    }
    root.version = version.into();
    let mut raw_url = table
        .get("url")
        .and_then(Item::as_str)
        .unwrap_or("")
        .to_owned();
    if let Some(repository) = &selection.git {
        root.git = Some(GitSource {
            repository: repository.clone(),
            revision: String::new(),
        });
        root.upstream = None;
    }
    if let Some(url) = &selection.url {
        raw_url = url.clone();
        if let Some(git) = root.git.take() {
            root.upstream = Some(Upstream {
                repository: git.repository,
                revision: git.revision,
                reference: root.upstream.and_then(|upstream| upstream.reference),
            });
        }
    }
    if let Some(git) = &mut root.git {
        if selection.file.is_some() {
            return Err(invalid("--file applies only to archive sources"));
        }
        let reference = selection.reference.as_deref().unwrap_or(version);
        git.revision = resolver.resolve_ref(&git.repository, reference)?;
        crate::dep::validate_revision(&git.revision)?;
        root.url.clear();
        root.sha256.clear();
        root.file = None;
    } else {
        if let Some(upstream) = &mut root.upstream {
            let reference = selection.reference.clone().unwrap_or_else(|| {
                expand(
                    upstream.reference.as_deref().unwrap_or("{version}"),
                    name,
                    version,
                    "",
                )
            });
            let revision = resolver.resolve_ref(&upstream.repository, &reference)?;
            crate::dep::validate_revision(&revision)?;
            if !raw_url.contains("{revision}") {
                raw_url = raw_url.replace(&upstream.revision, &revision);
            }
            upstream.revision = revision;
        } else if selection.reference.is_some() {
            return Err(invalid(
                "--ref requires a Git source or upstream repository metadata",
            ));
        }
        if selection.url.is_none() {
            // Immutable URL revisions may differ from the human release version.
            if root.upstream.is_none() {
                raw_url = retarget(&raw_url, &current.version, version, "url")?;
            }
        }
        root.file = selection
            .file
            .clone()
            .or_else(|| table.get("file").and_then(Item::as_str).map(str::to_owned));
        if let Some(file) = &mut root.file
            && selection.file.is_none()
        {
            *file = retarget(file, &current.version, version, "file")?;
        }
    }
    finish(document, pins, root, raw_url, resolver)
}

fn finish(
    mut document: DocumentMut,
    pins: &SourcePins,
    root: Dep,
    raw_url: String,
    resolver: &mut impl Resolver,
) -> Result<ResolvedSources> {
    let mut sources = BTreeMap::from([(root.name.clone(), (root.clone(), raw_url))]);
    let mut pending = VecDeque::from([root.name]);
    while let Some(parent_name) = pending.pop_front() {
        let parent = &sources[&parent_name].0;
        let children = pins
            .iter()
            .filter(|dep| {
                dep.resolve
                    .as_ref()
                    .is_some_and(|rule| rule.parent() == parent_name)
            })
            .cloned()
            .collect::<Vec<_>>();
        if children.is_empty() {
            continue;
        }
        let (repository, revision) = source_metadata(parent)?;
        let repository = repository.to_owned();
        let revision = revision.to_owned();
        for mut child in children {
            let mut raw_url = document[&child.name]
                .get("url")
                .and_then(Item::as_str)
                .unwrap_or("")
                .to_owned();
            match child.resolve.as_ref().expect("dependent resolution") {
                Resolution::Gitlink { path, archive, .. } => {
                    let git = resolver.gitlink(&repository, &revision, path)?;
                    crate::dep::validate_revision(&git.revision)?;
                    if git.repository.is_empty() {
                        return Err(invalid("gitlink repository is empty"));
                    }
                    child.version = git.revision.clone();
                    if let Some(template) = archive {
                        raw_url = template
                            .replace("{repository}", git.repository.trim_end_matches(".git"));
                        child.git = None;
                        child.upstream = Some(Upstream {
                            repository: git.repository,
                            revision: git.revision,
                            reference: None,
                        });
                    } else {
                        child.git = Some(git);
                        child.upstream = None;
                        child.url.clear();
                        child.sha256.clear();
                        child.file = None;
                    }
                }
                Resolution::File {
                    revision: rule,
                    url,
                    placeholder,
                    ..
                } => {
                    child.version = capture(resolver, &repository, &revision, rule)?;
                    crate::dep::validate_revision(&child.version)?;
                    let inferred = rule
                        .cmake_variable()
                        .map(|variable| format!("${{{variable}}}"));
                    if let Some(rule) = url {
                        raw_url = capture(resolver, &repository, &revision, rule)?;
                        if let Some(placeholder) = placeholder {
                            if placeholder.is_empty() || !raw_url.contains(placeholder) {
                                return Err(invalid(format!(
                                    "{} URL lacks declared placeholder",
                                    child.name
                                )));
                            }
                            raw_url = raw_url.replace(placeholder, "{version}");
                        } else if let Some(inferred) = &inferred {
                            raw_url = raw_url.replace(inferred, "{version}");
                        }
                        if rule.cmake_url().is_some()
                            && (raw_url.contains("${")
                                || raw_url.contains("$<")
                                || raw_url.contains("$ENV{"))
                        {
                            return Err(invalid(format!(
                                "{} URL contains unresolved CMake expressions; only the selected revision variable can be substituted",
                                child.name
                            )));
                        }
                    } else {
                        raw_url = retarget(
                            &raw_url,
                            &pins.get(&child.name)?.version,
                            &child.version,
                            "dependent URL",
                        )?;
                    }
                    if let Some(git) = &mut child.git {
                        git.revision = child.version.clone();
                    }
                    if let Some(upstream) = &mut child.upstream {
                        upstream.revision = child.version.clone();
                    }
                }
            }
            if child.git.is_none() {
                // Use complete revisions, not colliding abbreviated old filenames.
                let suffix = [
                    ".tar.gz", ".tar.bz2", ".tar.xz", ".zip", ".tgz", ".tbz2", ".txz",
                ]
                .into_iter()
                .find(|suffix| raw_url.ends_with(suffix));
                if let Some(suffix) = suffix {
                    child.file = Some(format!("{}-{{version}}{suffix}", child.name));
                } else if let Some(file) = document[&child.name].get("file").and_then(Item::as_str)
                {
                    child.file = Some(retarget(
                        file,
                        &pins.get(&child.name)?.version,
                        &child.version,
                        "dependent file",
                    )?);
                }
            }
            pending.push_back(child.name.clone());
            sources.insert(child.name.clone(), (child, raw_url));
        }
    }
    let mut changes = Vec::new();
    // Complete all metadata resolution before starting any archive downloads.
    for (name, (mut dep, raw_url)) in sources {
        if dep.git.is_none() {
            let revision = dep
                .upstream
                .as_ref()
                .map_or(dep.version.as_str(), |upstream| upstream.revision.as_str());
            dep.url = expand(&raw_url, &dep.name, &dep.version, revision);
            if dep.url.is_empty() || dep.url.contains('{') || dep.url.contains('}') {
                return Err(invalid(format!(
                    "{name} has an empty or unresolved archive URL: {}",
                    dep.url
                )));
            }
            dep.sha256 = resolver.checksum(&dep)?;
            if dep.sha256.len() != 64 || !dep.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(invalid(format!(
                    "{name} resolver returned an invalid SHA-256"
                )));
            }
        }
        let table = document
            .entry(&name)
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| invalid(format!("invalid [{name}] table")))?;
        record(table, &dep, &raw_url)?;
        changes.push(Change {
            name,
            version: dep.version,
            url: dep.git.is_none().then_some(dep.url),
            sha256: dep.git.is_none().then_some(dep.sha256),
            git: dep.git,
            upstream: dep.upstream,
        });
    }
    SourcePins::parse(&document.to_string())?;
    Ok(ResolvedSources { document, changes })
}

/// Verify every archive checksum and exact Git commit's source markers.
pub fn verify(manifest: &Path) -> Result<Vec<(String, Result<()>)>> {
    let pins = crate::project::Project::load(manifest)?.locked()?;
    let mut results = Vec::new();
    for (id, dep) in &pins.sources {
        let outcome = if dep.git.is_some() {
            crate::Cache::discover()?.source(dep).map(|_| ())
        } else {
            sha256_of_url(&dep.url).and_then(|actual| {
                if actual.eq_ignore_ascii_case(&dep.sha256) {
                    Ok(())
                } else {
                    Err(Error::Checksum {
                        path: dep.url.clone().into(),
                        expected: dep.sha256.clone(),
                        actual,
                    })
                }
            })
        };
        results.push((id.clone(), outcome));
    }
    Ok(results)
}

#[cfg(test)]
mod tests;
