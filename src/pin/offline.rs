//! Strictly offline metadata/archive adapter for update qualification.
use super::*;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ref {
    repository: String,
    reference: String,
    revision: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    repository: String,
    revision: String,
    path: String,
    file: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Link {
    repository: String,
    revision: String,
    path: String,
    source: GitSource,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Archive {
    name: String,
    file: PathBuf,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Metadata {
    refs: Vec<Ref>,
    files: Vec<File>,
    gitlinks: Vec<Link>,
    archives: Vec<Archive>,
}

/// Offline resolver loaded from a JSON metadata file and adjacent archive bytes.
///
/// Every lookup must have exactly one entry. Referenced files must be inside the
/// fixture directory, including after resolving symlinks. Missing fixtures never
/// fall back to a network transport.
pub struct Offline {
    root: PathBuf,
    metadata: Metadata,
}

impl Offline {
    /// Load `refs`, `files`, `gitlinks`, and `archives` arrays from JSON.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| Error::io("read offline resolution metadata", error))?;
        let metadata = serde_json::from_str(&text)
            .map_err(|error| invalid(format!("offline metadata: {error}")))?;
        let root = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize()
            .map_err(|error| Error::io("resolve offline fixture directory", error))?;
        Ok(Self { root, metadata })
    }

    fn file_path(&self, relative: &Path) -> Result<PathBuf> {
        if relative.as_os_str().is_empty()
            || !relative
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
        {
            return Err(invalid(
                "offline file must be relative to its fixture directory",
            ));
        }
        let path = self
            .root
            .join(relative)
            .canonicalize()
            .map_err(|error| Error::io("read offline fixture", error))?;
        if !path.starts_with(&self.root) {
            return Err(invalid("offline fixture escapes its directory"));
        }
        Ok(path)
    }
}

fn unique<T>(mut matches: impl Iterator<Item = T>, description: &str) -> Result<T> {
    let first = matches
        .next()
        .ok_or_else(|| invalid(format!("missing offline {description}")))?;
    if matches.next().is_some() {
        return Err(invalid(format!("ambiguous offline {description}")));
    }
    Ok(first)
}

impl Resolver for Offline {
    fn resolve_ref(&mut self, repository: &str, reference: &str) -> Result<String> {
        let value = unique(
            self.metadata
                .refs
                .iter()
                .filter(|entry| entry.repository == repository && entry.reference == reference),
            "ref",
        )?;
        crate::dep::validate_revision(&value.revision)?;
        Ok(value.revision.clone())
    }

    fn file(&mut self, repository: &str, revision: &str, path: &str) -> Result<String> {
        let entry = unique(
            self.metadata.files.iter().filter(|entry| {
                entry.repository == repository && entry.revision == revision && entry.path == path
            }),
            "file",
        )?;
        std::fs::read_to_string(self.file_path(&entry.file)?)
            .map_err(|error| Error::io("read offline source file", error))
    }

    fn gitlink(&mut self, repository: &str, revision: &str, path: &str) -> Result<GitSource> {
        let entry = unique(
            self.metadata.gitlinks.iter().filter(|entry| {
                entry.repository == repository && entry.revision == revision && entry.path == path
            }),
            "gitlink",
        )?;
        crate::dep::validate_revision(&entry.source.revision)?;
        Ok(entry.source.clone())
    }

    fn checksum(&mut self, dep: &Dep) -> Result<String> {
        let entry = unique(
            self.metadata
                .archives
                .iter()
                .filter(|entry| entry.name == dep.name),
            "archive",
        )?;
        crate::sha256_file(&self.file_path(&entry.file)?)
    }
}
