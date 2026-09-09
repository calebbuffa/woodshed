//! A pinned native dependency, and the manifest that lists them.

/// One pinned native dependency: an exact version, a fixed URL, and the
/// SHA-256 the download must match.
///
/// There is deliberately no way to express "latest". A build that can change
/// underneath you is not reproducible, and a download that is not verified is
/// not safe.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "manifest", derive(serde::Deserialize))]
pub struct Dep {
    /// Identifier used for cache paths and messages. Filled in from the table
    /// name when loaded via [`Manifest`].
    #[cfg_attr(feature = "manifest", serde(skip))]
    pub name: String,
    /// Exact upstream version.
    pub version: String,
    /// Fixed download URL.
    pub url: String,
    /// Expected SHA-256 of the archive, as lowercase hex.
    pub sha256: String,
    /// Files that must exist in the extracted tree for it to count as
    /// complete. Paths are relative to the project root; a trailing `/` means
    /// "directory".
    ///
    /// This is how an interrupted extraction is told apart from a finished
    /// one, so name a build-system file and a core source file rather than
    /// something incidental. Empty means "any successful extraction will do",
    /// which is weaker but sometimes all you can say.
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
            markers: Vec::new(),
            file: None,
        }
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
        format!("{}-{}", self.name, self.version)
    }

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

/// A set of pinned dependencies loaded from a TOML manifest.
///
/// The file holds one table per dependency, keyed by name:
///
/// ```toml
/// [geos]
/// version = "3.14.1"
/// url = "https://download.osgeo.org/geos/geos-3.14.1.tar.bz2"
/// sha256 = "3c20919cda9a505db07b5216baa980bacdaa0702da715b43f176fb07eff7e716"
/// markers = ["CMakeLists.txt", "capi/geos_c.h.in"]
/// ```
#[cfg(feature = "manifest")]
#[derive(Clone, Debug)]
pub struct Manifest {
    deps: std::collections::BTreeMap<String, Dep>,
}

#[cfg(feature = "manifest")]
impl Manifest {
    /// Load a manifest from an explicit path.
    pub fn load(path: impl AsRef<std::path::Path>) -> crate::Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|error| crate::Error::io(format!("cannot read {}", path.display()), error))?;
        Self::parse(&text)
    }

    /// Load `native/versions.toml` relative to a crate's manifest directory.
    /// That is the conventional location; [`Manifest::load`] accepts any path.
    pub fn load_default(manifest_dir: impl AsRef<std::path::Path>) -> crate::Result<Self> {
        Self::load(manifest_dir.as_ref().join("native").join("versions.toml"))
    }

    /// Parse manifest text.
    pub fn parse(text: &str) -> crate::Result<Self> {
        let tables: std::collections::BTreeMap<String, Dep> =
            toml::from_str(text).map_err(|error| crate::Error::Manifest {
                detail: error.to_string(),
            })?;
        let deps = tables
            .into_iter()
            .map(|(name, mut dep)| {
                dep.name = name.clone();
                (name, dep)
            })
            .collect();
        Ok(Self { deps })
    }

    /// Look up a dependency by name.
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

    #[test]
    fn source_dir_name_pairs_name_and_version() {
        assert_eq!(dep().source_dir_name(), "geos-3.14.1");
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
