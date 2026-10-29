//! A pinned native dependency, and the manifest that lists them.

#[cfg(any(feature = "manifest", feature = "git"))]
pub(crate) fn validate_revision(revision: &str) -> crate::Result<()> {
    if ![40, 64].contains(&revision.len()) || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(crate::Error::Manifest {
            detail: format!("expected an immutable full Git commit ID, got {revision:?}"),
        });
    }
    Ok(())
}

/// One pinned native dependency: an exact version, a fixed URL, and the
/// SHA-256 the download must match.
///
/// There is deliberately no way to express "latest". A build that can change
/// underneath you is not reproducible, and a download that is not verified is
/// not safe.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "manifest", derive(serde::Serialize, serde::Deserialize))]
pub struct Dep {
    /// Identifier used for cache paths and messages. Filled in from the table
    /// name when loaded through a project lock.
    #[cfg_attr(feature = "manifest", serde(skip))]
    pub name: String,
    /// Exact upstream version.
    pub version: String,
    /// Fixed download URL. `{version}` and `{name}` are substituted when the
    /// dependency is resolved from project declarations, so a version bump is a single
    /// edit.
    #[cfg_attr(feature = "manifest", serde(default))]
    pub url: String,
    /// Expected SHA-256 of the archive, as lowercase hex.
    #[cfg_attr(feature = "manifest", serde(default))]
    pub sha256: String,
    /// Immutable Git source, mutually exclusive with archive URL/checksum.
    #[cfg_attr(feature = "manifest", serde(default))]
    pub git: Option<GitSource>,
    /// Upstream Git metadata used by maintainer commands for archive sources.
    /// Builds do not resolve tags or branches.
    #[cfg_attr(feature = "manifest", serde(default))]
    pub upstream: Option<Upstream>,
    /// How a maintainer bump of another dependency resolves this source.
    #[cfg_attr(feature = "manifest", serde(default))]
    pub resolve: Option<Resolution>,
    /// Files proving a complete source extraction; trailing `/` means directory.
    #[cfg_attr(feature = "manifest", serde(default))]
    pub markers: Vec<String>,
    /// Override the cached archive's filename. Defaults to the URL basename;
    /// set this when that basename is unstable or collides across versions.
    #[cfg_attr(feature = "manifest", serde(default))]
    pub file: Option<String>,
}

impl Dep {
    /// Define a dependency directly, for callers not using a TOML manifest.
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        url: impl Into<String>,
        sha256: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            url: url.into(),
            sha256: sha256.into(),
            git: None,
            upstream: None,
            resolve: None,
            markers: Vec::new(),
            file: None,
        }
    }

    /// Pin a Git source to an immutable commit. Requires feature `git` to provision.
    pub fn git(
        name: impl Into<String>,
        version: impl Into<String>,
        repository: impl Into<String>,
        revision: impl Into<String>,
    ) -> Self {
        let mut dep = Self::new(name, version, "", "");
        dep.git = Some(GitSource {
            repository: repository.into(),
            revision: revision.into(),
        });
        dep
    }

    /// Set the files that prove an extraction finished.
    #[must_use]
    pub fn with_markers<I, S>(mut self, markers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.markers = markers.into_iter().map(Into::into).collect();
        self
    }

    /// Override the cached archive filename.
    #[must_use]
    pub fn with_file(mut self, file: impl Into<String>) -> Self {
        self.file = Some(file.into());
        self
    }

    /// Replace `{version}` and `{name}` in `url` and `file`.
    pub fn expand_templates(&mut self) {
        let sub = |s: &str, v: &str, n: &str| s.replace("{version}", v).replace("{name}", n);
        self.url = sub(&self.url, &self.version, &self.name);
        if let Some(upstream) = &self.upstream {
            self.url = self.url.replace("{revision}", &upstream.revision);
        }
        if let Some(file) = &self.file {
            self.file = Some(sub(file, &self.version, &self.name));
        }
    }

    /// The filename this dependency's archive is cached under.
    pub fn archive_file(&self) -> String {
        if let Some(file) = &self.file {
            return file.clone();
        }
        self.url
            .rsplit('/')
            .next()
            .filter(|segment| !segment.is_empty())
            .map_or_else(|| self.name.clone(), ToString::to_string)
    }

    /// Directory name for this dependency's extracted source.
    pub(crate) fn source_dir_name(&self) -> String {
        if let Some(git) = &self.git {
            use sha2::{Digest, Sha256};
            let mut identity = Sha256::new();
            identity.update(git.repository.as_bytes());
            identity.update([0]);
            identity.update(git.revision.as_bytes());
            return format!("{}-git-{:x}", self.name, identity.finalize());
        }
        format!("archive-{}", self.sha256.to_ascii_lowercase())
    }
}

/// An immutable Git snapshot; floating refs are resolved only by maintainer commands.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "manifest", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "manifest", serde(deny_unknown_fields))]
pub struct GitSource {
    /// Repository URL (or explicit local repository path).
    pub repository: String,
    /// Full commit object ID, never a moving branch or tag.
    pub revision: String,
}

/// Git provenance and release-ref policy for an archive.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "manifest", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "manifest", serde(deny_unknown_fields))]
pub struct Upstream {
    /// Repository containing the release and its dependency metadata.
    pub repository: String,
    /// Recorded immutable commit used for `{revision}` in archive URLs.
    pub revision: String,
    /// Ref template used on bump, e.g. `v{version}`. Omit to use the new version.
    #[cfg_attr(feature = "manifest", serde(default))]
    pub reference: Option<String>,
}

/// Select exactly one value from an upstream UTF-8 file.
///
/// Choose `cmake-variable`, `cmake-url`, or the advanced `pattern` escape hatch.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "manifest", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "manifest", serde(deny_unknown_fields))]
pub struct FileMatch {
    /// Repository-relative file path.
    pub path: String,
    /// Exactly one extraction method.
    #[cfg_attr(feature = "manifest", serde(flatten))]
    pub select: Box<FileSelector>,
}

/// Readable metadata selectors, with regex available for unusual file formats.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "manifest", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "manifest", serde(rename_all = "kebab-case"))]
pub enum FileSelector {
    /// Regex with exactly one capturing group; exactly one match is required.
    Pattern(String),
    /// Name in a literal CMake `set(NAME value)` declaration.
    CmakeVariable(String),
    /// Dependency name in `FetchContent_Declare(name URL value)`.
    CmakeUrl(String),
}

impl FileMatch {
    /// Regex escape hatch, if selected.
    pub fn pattern(&self) -> Option<&str> {
        match self.select.as_ref() {
            FileSelector::Pattern(value) => Some(value),
            _ => None,
        }
    }

    /// Selected CMake variable name, if any.
    pub fn cmake_variable(&self) -> Option<&str> {
        match self.select.as_ref() {
            FileSelector::CmakeVariable(value) => Some(value),
            _ => None,
        }
    }

    /// Selected FetchContent dependency name, if any.
    pub fn cmake_url(&self) -> Option<&str> {
        match self.select.as_ref() {
            FileSelector::CmakeUrl(value) => Some(value),
            _ => None,
        }
    }
}

#[cfg(feature = "manifest")]
impl FileMatch {
    pub(crate) fn validate(&self) -> crate::Result<()> {
        for name in [self.cmake_variable(), self.cmake_url()]
            .into_iter()
            .flatten()
        {
            if name.is_empty()
                || name
                    .chars()
                    .any(|c| !(c.is_ascii_alphanumeric() || "_-.".contains(c)))
            {
                return Err(crate::Error::Manifest {
                    detail: format!(
                        "{}: CMake selector must name a variable or dependency, not an expression",
                        self.path
                    ),
                });
            }
        }
        Ok(())
    }
}

/// Declarative dependency relationships evaluated during a parent bump.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "manifest", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "manifest",
    serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)
)]
pub enum Resolution {
    /// Read an exact gitlink and its repository URL from `.gitmodules`.
    Gitlink {
        /// Name of the parent manifest entry.
        parent: String,
        /// Repository-relative submodule path.
        path: String,
        /// Archive URL template using `{repository}` and `{revision}`.
        /// Omit to provision this dependency from Git.
        #[cfg_attr(feature = "manifest", serde(default))]
        archive: Option<String>,
    },
    /// Read a pinned revision and optionally its archive URL from parent files.
    File {
        /// Name of the parent manifest entry.
        parent: String,
        /// Exact revision capture.
        revision: FileMatch,
        /// Optional upstream URL capture; otherwise retain the entry's URL template.
        #[cfg_attr(feature = "manifest", serde(default))]
        url: Option<FileMatch>,
        /// Literal upstream URL placeholder to substitute with the revision.
        #[cfg_attr(feature = "manifest", serde(default))]
        placeholder: Option<String>,
    },
}

impl Resolution {
    /// Name of the parent source.
    pub fn parent(&self) -> &str {
        match self {
            Self::Gitlink { parent, .. } | Self::File { parent, .. } => parent,
        }
    }
}

impl Dep {
    /// Whether every marker is present under `root`.
    pub(crate) fn markers_present(&self, root: &std::path::Path) -> bool {
        self.missing_marker(root).is_none()
    }

    /// The first missing marker, for error messages.
    pub(crate) fn missing_marker(&self, root: &std::path::Path) -> Option<&str> {
        self.markers
            .iter()
            .find(|marker| {
                let path = root.join(marker.trim_end_matches('/'));
                if marker.ends_with('/') {
                    !path.is_dir()
                } else {
                    !path.is_file()
                }
            })
            .map(String::as_str)
    }
}

/// Internal validation of normalized immutable source records.
#[cfg(feature = "manifest")]
#[derive(Clone, Debug)]
pub(crate) struct SourcePins {
    deps: std::collections::BTreeMap<String, Dep>,
}

#[cfg(feature = "manifest")]
impl SourcePins {
    /// Validate the normalized source records used by lock resolution.
    pub fn parse(text: &str) -> crate::Result<Self> {
        let tables: std::collections::BTreeMap<String, Dep> =
            toml::from_str(text).map_err(|error| crate::Error::Manifest {
                detail: error.to_string(),
            })?;
        let deps = tables
            .into_iter()
            .map(|(name, mut dep)| {
                dep.name = name.clone();
                dep.expand_templates();
                (name, dep)
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        for dep in deps.values() {
            if let Some(git) = &dep.git {
                validate_revision(&git.revision)?;
                if git.repository.is_empty() || !dep.url.is_empty() || !dep.sha256.is_empty() {
                    return Err(crate::Error::Manifest {
                        detail: format!(
                            "`[{}]` must select either git or archive, not both",
                            dep.name
                        ),
                    });
                }
            } else if dep.url.is_empty() || dep.sha256.is_empty() {
                return Err(crate::Error::Manifest {
                    detail: format!(
                        "`[{}]` needs an archive URL and checksum or git source",
                        dep.name
                    ),
                });
            }
            if let Some(upstream) = &dep.upstream {
                validate_revision(&upstream.revision)?;
            }
            let mut seen = std::collections::BTreeSet::new();
            let mut node = dep;
            while let Some(rule) = &node.resolve {
                if !seen.insert(node.name.as_str()) {
                    return Err(crate::Error::Manifest {
                        detail: format!("dependency resolution cycle at `{}`", node.name),
                    });
                }
                node = deps
                    .get(rule.parent())
                    .ok_or_else(|| crate::Error::Manifest {
                        detail: format!(
                            "`[{}]` has unknown resolution parent `{}`",
                            node.name,
                            rule.parent()
                        ),
                    })?;
            }
        }
        Ok(Self { deps })
    }

    /// Look up a dependency by name.
    #[cfg(any(feature = "pin", test))]
    pub fn get(&self, name: &str) -> crate::Result<&Dep> {
        self.deps.get(name).ok_or_else(|| crate::Error::Manifest {
            detail: format!("no `[{name}]` section"),
        })
    }

    /// Iterate every pinned dependency.
    pub fn iter(&self) -> impl Iterator<Item = &Dep> {
        self.deps.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep() -> Dep {
        Dep::new(
            "geos",
            "3.14.1",
            "https://example.org/dl/geos-3.14.1.tar.bz2",
            "ab",
        )
    }

    #[test]
    fn archive_file_comes_from_the_url() {
        assert_eq!(dep().archive_file(), "geos-3.14.1.tar.bz2");
    }

    #[test]
    fn explicit_file_overrides_the_url() {
        let sqlite = Dep::new("sqlite", "3450300", "https://example.org/dl?id=7", "ab")
            .with_file("sqlite-3450300.zip");
        assert_eq!(sqlite.archive_file(), "sqlite-3450300.zip");
    }

    #[test]
    fn archive_file_falls_back_to_the_name() {
        let odd = Dep::new("thing", "1.0", "https://example.org/dl/", "ab");
        assert_eq!(odd.archive_file(), "thing");
    }

    #[cfg(feature = "manifest")]
    #[test]
    fn manifest_expands_url_templates() {
        let manifest = SourcePins::parse(
            "[geos]\nversion = \"3.15.0\"\nurl = \"https://x.org/{name}/{name}-{version}.tar.bz2\"\n\
             sha256 = \"ab\"\nfile = \"{name}-{version}.tar.bz2\"\n",
        )
        .unwrap();
        let geos = manifest.get("geos").unwrap();
        assert_eq!(geos.url, "https://x.org/geos/geos-3.15.0.tar.bz2");
        assert_eq!(geos.archive_file(), "geos-3.15.0.tar.bz2");
    }

    #[test]
    fn archive_source_identity_uses_content_not_release_labels() {
        let original = dep();
        assert_eq!(original.source_dir_name(), "archive-ab");
        let mut changed = original.clone();
        changed.version = "different".into();
        changed.name = "alias".into();
        changed.sha256 = "AB".into();
        assert_eq!(original.source_dir_name(), changed.source_dir_name());
        changed.sha256 = "cd".into();
        assert_ne!(original.source_dir_name(), changed.source_dir_name());
    }

    #[test]
    fn markers_distinguish_files_from_directories() {
        let dir = std::env::temp_dir().join(format!("woodshed-markers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("CMakeLists.txt"), b"").unwrap();

        let ok = dep().with_markers(["CMakeLists.txt", "src/"]);
        assert!(ok.markers_present(&dir));

        // A trailing slash demands a directory, and CMakeLists.txt is a file.
        let wrong_kind = dep().with_markers(["CMakeLists.txt/"]);
        assert!(!wrong_kind.markers_present(&dir));
        assert_eq!(wrong_kind.missing_marker(&dir), Some("CMakeLists.txt/"));

        let absent = dep().with_markers(["CMakeLists.txt", "configure"]);
        assert_eq!(absent.missing_marker(&dir), Some("configure"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dep_without_markers_is_trivially_present() {
        assert!(dep().markers_present(std::path::Path::new("/nonexistent")));
    }
}
