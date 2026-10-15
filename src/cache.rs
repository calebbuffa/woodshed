//! The cache: where archives, extracted sources, and build trees live.
//!
//! # Why build trees do not go in `OUT_DIR`
//!
//! Cargo deletes `OUT_DIR` whenever the build script changes. For a native
//! dependency that takes ten minutes to compile, that turns every one-line
//! edit to `build.rs` into a ten-minute wait - and worse, it silently discards
//! work that was still perfectly valid.
//!
//! So build trees live under the cache root in a directory named by a
//! [`BuildKey`] hash of everything that affects the output. Two builds share a
//! tree exactly when sharing is correct, a stale tree is never reused, and
//! editing the build script costs nothing.
//!
//! # Layout
//!
//! ```text
//! <root>/
//!   archives/  verified downloads, named by dependency
//!   sources/   extracted trees, one per name-version
//!   builds/    build trees + install prefixes, one per BuildKey
//! ```

use crate::{Dep, Error, Result};
use fs4::fs_std::FileExt;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long to wait for another process to release a cache lock.
const LOCK_TIMEOUT: Duration = Duration::from_secs(600);

/// A cache of native dependency archives, sources, and build trees.
///
/// The default location is `$CARGO_HOME/woodshed`, shared by every crate on
/// the machine, so two crates that pin the same dependency download it once.
#[derive(Clone, Debug)]
pub struct Cache {
    root: PathBuf,
    offline: bool,
}

impl Cache {
    /// Open the default cache.
    ///
    /// The location is `$WOODSHED_CACHE` if set, else `$CARGO_HOME/woodshed`.
    /// Offline mode is on when `$WOODSHED_OFFLINE=1`.
    ///
    /// Emits the `cargo:rerun-if-env-changed` lines for both variables, so a
    /// user who changes either gets a correct rebuild.
    pub fn discover() -> Result<Self> {
        println!("cargo:rerun-if-env-changed=WOODSHED_CACHE");
        println!("cargo:rerun-if-env-changed=WOODSHED_OFFLINE");

        let root = if let Some(dir) = env::var_os("WOODSHED_CACHE") {
            PathBuf::from(dir)
        } else {
            let home = env::var_os("CARGO_HOME")
                .map(PathBuf::from)
                .or_else(|| env::var_os("HOME").map(PathBuf::from))
                .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
                .ok_or_else(|| Error::Io {
                    context: "cannot locate a cache directory".into(),
                    source: std::io::Error::other(
                        "none of CARGO_HOME, HOME, or USERPROFILE is set; \
                         set WOODSHED_CACHE to choose a location explicitly",
                    ),
                })?;
            home.join("woodshed")
        };

        let offline = env::var("WOODSHED_OFFLINE").is_ok_and(|value| value == "1");
        Ok(Self { root, offline })
    }

    /// Open a cache at an explicit location.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            offline: false,
        }
    }

    /// Refuse to download; fail with an actionable error instead.
    ///
    /// Useful for air-gapped or hermetic builds where a network access should
    /// be a loud failure rather than a silent delay.
    #[must_use]
    pub fn offline(mut self, offline: bool) -> Self {
        self.offline = offline;
        self
    }

    /// The cache root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where verified archives are stored.
    pub fn archives_dir(&self) -> PathBuf {
        self.root.join("archives")
    }

    /// Where extracted source trees are stored.
    pub fn sources_dir(&self) -> PathBuf {
        self.root.join("sources")
    }

    /// Where build trees and install prefixes are stored.
    pub fn builds_dir(&self) -> PathBuf {
        self.root.join("builds")
    }

    /// A build directory unique to `key`, and its install prefix.
    ///
    /// Use this instead of `OUT_DIR` for anything expensive: it survives build
    /// script edits and `cargo clean`, and is short enough to stay clear of
    /// the Windows `MAX_PATH` limit that deep `OUT_DIR` paths run into.
    pub fn build_dir(&self, key: &BuildKey) -> BuildDirs {
        let dir = self.builds_dir().join(key.as_str());
        BuildDirs {
            prefix: dir.join("prefix"),
            build: dir,
        }
    }

    /// Ensure a dependency's archive is present and checksum-verified.
    ///
    /// A cached archive is verified before reuse, so a truncated or tampered
    /// file is replaced rather than trusted. Downloads land in a
    /// process-unique `.partial` file and are renamed into place only after
    /// verification, so a concurrent or interrupted build never observes a
    /// half-written archive.
    pub fn archive(&self, dep: &Dep) -> Result<PathBuf> {
        let dir = self.archives_dir();
        fs::create_dir_all(&dir)
            .map_err(|error| Error::io(format!("cannot create {}", dir.display()), error))?;

        let path = dir.join(dep.archive_file());
        let _lock = Lock::acquire(&lock_path(&path, "download"))?;

        if path.is_file() {
            match crate::verify_sha256(&path, &dep.sha256) {
                Ok(()) => return Ok(path),
                Err(error) if self.offline => {
                    return Err(Error::Offline {
                        detail: format!(
                            "the cached `{}` archive is invalid ({error})\n  \
                             delete {} and rerun with network access",
                            dep.name,
                            path.display()
                        ),
                    });
                }
                Err(error) => {
                    eprintln!(
                        "woodshed: cached `{}` archive is invalid; re-downloading ({error})",
                        dep.name
                    );
                    let _ = fs::remove_file(&path);
                }
            }
        }

        if self.offline {
            return Err(Error::Offline {
                detail: format!(
                    "the `{}` archive is missing\n  \
                     expected: {}\n  \
                     source:   {}\n  \
                     sha256:   {}\n  \
                     populate the cache, or rerun with network access",
                    dep.name,
                    path.display(),
                    dep.url,
                    dep.sha256
                ),
            });
        }

        self.download_verified(dep, &path)
    }

    #[cfg(feature = "download")]
    fn download_verified(&self, dep: &Dep, path: &Path) -> Result<PathBuf> {
        let partial = path.with_extension(format!("partial-{}", std::process::id()));
        eprintln!(
            "woodshed: downloading {} {} from {}",
            dep.name, dep.version, dep.url
        );

        let cleanup = |error| {
            let _ = fs::remove_file(&partial);
            error
        };
        crate::download::fetch(&dep.url, &partial).map_err(cleanup)?;
        crate::verify_sha256(&partial, &dep.sha256).map_err(cleanup)?;
        fs::rename(&partial, path).map_err(|error| {
            let _ = fs::remove_file(&partial);
            Error::io(format!("cannot move into {}", path.display()), error)
        })?;
        Ok(path.to_path_buf())
    }

    #[cfg(not(feature = "download"))]
    fn download_verified(&self, dep: &Dep, path: &Path) -> Result<PathBuf> {
        let _ = path;
        Err(Error::Download {
            url: dep.url.clone(),
            detail: "woodshed's `download` feature is not enabled".into(),
        })
    }

    /// Ensure a dependency's source tree is extracted and complete.
    ///
    /// Returns the project root: if the archive unpacks to a single top-level
    /// directory, that directory, otherwise the extraction dir itself.
    ///
    /// A tree whose [markers](Dep::markers) are missing is treated as the
    /// debris of an interrupted extraction, deleted, and unpacked again.
    pub fn source(&self, dep: &Dep) -> Result<PathBuf> {
        let archive = self.archive(dep)?;

        let sources = self.sources_dir();
        fs::create_dir_all(&sources)
            .map_err(|error| Error::io(format!("cannot create {}", sources.display()), error))?;

        let dir = sources.join(dep.source_dir_name());
        let _lock = Lock::acquire(&lock_path(&dir, "extract"))?;

        // Re-check after the lock: another process may have just finished.
        if dir.is_dir() {
            let root = project_root(&dir);
            if dep.markers_present(&root) {
                return Ok(root);
            }
            eprintln!(
                "woodshed: `{}` source tree is incomplete; re-extracting",
                dep.name
            );
            fs::remove_dir_all(&dir)
                .map_err(|error| Error::io(format!("cannot remove {}", dir.display()), error))?;
        }

        fs::create_dir_all(&dir)
            .map_err(|error| Error::io(format!("cannot create {}", dir.display()), error))?;
        crate::archive::extract(&archive, &dir)?;

        let root = project_root(&dir);
        if let Some(missing) = dep.missing_marker(&root) {
            return Err(Error::Extract {
                archive,
                detail: format!(
                    "`{missing}` is missing from {}\n  \
                     the archive may not match its pinned checksum's contents",
                    root.display()
                ),
            });
        }
        Ok(root)
    }

    /// Copy a source tree into a stable path under the cache, keyed by `key`.
    ///
    /// CMake permanently binds a build tree to the exact source path it was
    /// configured with. A build script whose CMake project lives inside the
    /// crate - a superbuild, say - would therefore lose its entire build tree
    /// whenever the checkout moved, including during `cargo publish`'s
    /// verification build. Mirroring to a path that depends only on content
    /// makes the location stable, so CMake rebuilds only what actually
    /// changed.
    pub fn mirror(&self, source: &Path, key: &BuildKey) -> Result<PathBuf> {
        let dest = self.root.join("mirrors").join(key.as_str());
        if dest.exists() {
            fs::remove_dir_all(&dest)
                .map_err(|error| Error::io(format!("cannot refresh {}", dest.display()), error))?;
        }
        copy_dir(source, &dest)?;
        Ok(dest)
    }
}

/// Recursively copy every entry under `source` into `dest`.
fn copy_dir(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)
        .map_err(|error| Error::io(format!("cannot create {}", dest.display()), error))?;

    let entries = fs::read_dir(source)
        .map_err(|error| Error::io(format!("cannot read {}", source.display()), error))?;

    for entry in entries {
        let entry =
            entry.map_err(|error| Error::io(format!("cannot read {}", source.display()), error))?;
        let path = entry.path();
        let target = dest.join(entry.file_name());

        if path.is_dir() {
            copy_dir(&path, &target)?;
        } else {
            fs::copy(&path, &target).map_err(|error| {
                Error::io(
                    format!("cannot copy {} to {}", path.display(), target.display()),
                    error,
                )
            })?;
        }
    }
    Ok(())
}

/// A build directory and the install prefix inside it.#[derive(Clone, Debug)]
pub struct BuildDirs {
    /// Where the build system does its work.
    pub build: PathBuf,
    /// Where the finished artifacts are installed.
    pub prefix: PathBuf,
}

impl BuildDirs {
    /// Whether every path in `required`, relative to the prefix, exists.
    ///
    /// Call this before reusing a prefix. A build that was interrupted leaves
    /// a directory that looks finished but is not, and linking against it
    /// fails much later with confusing errors.
    pub fn is_complete<I, S>(&self, required: I) -> bool
    where
        I: IntoIterator<Item = S>,
        S: AsRef<Path>,
    {
        required
            .into_iter()
            .all(|entry| self.prefix.join(entry).exists())
    }

    /// Claim this build directory until the returned guard is dropped.
    ///
    /// Two Cargo processes on one machine - commonly a build and a
    /// rust-analyzer check - will otherwise compile into the same tree at the
    /// same time and corrupt it. Take this before configuring, and hold it for
    /// the whole build.
    pub fn lock(&self) -> Result<BuildLock> {
        let path = lock_path(&self.build, "build");
        eprintln!("woodshed: claiming {}", self.build.display());
        Ok(BuildLock {
            _inner: Lock::acquire(&path)?,
        })
    }
}

/// Exclusive claim on a build directory, released when dropped.
#[must_use = "the claim is released as soon as this is dropped"]
pub struct BuildLock {
    _inner: Lock,
}

/// Identity of a build: a hash of everything that changes its output.
///
/// Anything that would make two builds produce different artifacts must be
/// mixed in, or a stale tree could be reused. The target triple and the
/// dependency's checksum are added for you; add anything else that matters,
/// such as feature flags or the CRT mode.
#[derive(Clone, Debug)]
pub struct BuildKey {
    hasher: sha2::Sha256,
    label: String,
}

impl BuildKey {
    /// Start a key for a named build, seeded with the target triple.
    pub fn new(label: impl Into<String>) -> Self {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(env::var("TARGET").unwrap_or_default().as_bytes());
        hasher.update(
            env::var("CARGO_CFG_TARGET_ENV")
                .unwrap_or_default()
                .as_bytes(),
        );
        hasher.update(env::var("CC").unwrap_or_default().as_bytes());
        hasher.update(env::var("CXX").unwrap_or_default().as_bytes());
        Self {
            hasher,
            label: label.into(),
        }
    }

    /// Mix in a dependency's pinned identity.
    #[must_use]
    pub fn dep(self, dep: &Dep) -> Self {
        self.mix(dep.version.as_bytes()).mix(dep.sha256.as_bytes())
    }

    /// Fold the linkage, and the install rpath it implies, into the key, so a
    /// static and a shared build of the same library never share a cache
    /// directory, and a prefix built before an rpath change is not reused.
    #[must_use]
    pub fn linkage(self, linkage: crate::Linkage) -> Self {
        let rpath = linkage.install_rpath(&crate::target_os()).unwrap_or("");
        self.mix(format!("linkage={};rpath={rpath}", linkage.as_str()))
    }

    /// Mix in arbitrary bytes.
    #[must_use]
    pub fn mix(mut self, bytes: impl AsRef<[u8]>) -> Self {
        use sha2::Digest;
        self.hasher.update(bytes.as_ref());
        self
    }

    /// The directory name for this key: the label plus a short digest.
    pub fn as_str(&self) -> String {
        use sha2::Digest;
        let digest = self.hasher.clone().finalize();
        let hex: String = digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("{}-{hex}", self.label)
    }
}

/// Resolve the project root of an extracted tree.
///
/// Most archives unpack to a single top-level directory; return that, so
/// callers do not have to know the upstream naming convention.
fn project_root(dir: &Path) -> PathBuf {
    let entries: Vec<PathBuf> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(std::result::Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect()
        })
        .unwrap_or_default();

    match entries.as_slice() {
        [only] => only.clone(),
        _ => dir.to_path_buf(),
    }
}

/// An advisory lock held for the lifetime of a cache operation.
///
/// Two `cargo build` invocations can run concurrently - commonly a build and a
/// rust-analyzer check - and must not download or extract into the same path
/// at the same time.
struct Lock {
    file: fs::File,
    #[allow(dead_code)]
    path: PathBuf,
}

/// Path of the lock guarding `path`, as a sibling `.<kind>.lock` file.
///
/// This appends rather than using `Path::with_extension`, which truncates at
/// the last dot and would collapse distinct targets onto one lock: both
/// `geos-3.14.1-a1b2c3d4` and `geos-3.14.1-99887766` reduce to
/// `geos-3.14.build.lock`.
fn lock_path(path: &Path, kind: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{kind}.lock"));
    path.with_file_name(name)
}

impl Lock {
    fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| Error::io(format!("cannot create {}", parent.display()), error))?;
        }

        // The lock file's contents are never read or written; only its
        // existence and the advisory lock on it matter. `truncate(false)`
        // states that, and avoids clobbering a concurrent holder's file.
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|error| Error::Lock {
                path: path.to_path_buf(),
                detail: error.to_string(),
            })?;

        let deadline = std::time::Instant::now() + LOCK_TIMEOUT;
        loop {
            match file.try_lock_exclusive() {
                Ok(true) => {
                    return Ok(Self {
                        file,
                        path: path.to_path_buf(),
                    });
                }
                Ok(false) | Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Ok(false) => {
                    return Err(Error::Lock {
                        path: path.to_path_buf(),
                        detail: format!("timed out after {LOCK_TIMEOUT:?}"),
                    });
                }
                Err(error) => {
                    return Err(Error::Lock {
                        path: path.to_path_buf(),
                        detail: error.to_string(),
                    });
                }
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // The file itself is deliberately left behind. Unlinking it would let a
        // third process create a fresh file at the same path and lock that,
        // while a second process still waits on the old, now-unlinked one --
        // both would then believe they hold the lock. Empty lock files are
        // cheap; a corrupted shared build tree is not.
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "woodshed-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn lock_path_keeps_dotted_names_distinct() {
        let a = lock_path(Path::new("/c/geos-3.14.1-a1b2c3d4"), "build");
        let b = lock_path(Path::new("/c/geos-3.14.1-99887766"), "build");
        assert_ne!(a, b, "distinct build trees must not share a lock");
        assert_eq!(a.file_name().unwrap(), "geos-3.14.1-a1b2c3d4.build.lock");
    }

    #[test]
    fn lock_path_does_not_truncate_compound_extensions() {
        let gz = lock_path(Path::new("/c/proj-9.4.1.tar.gz"), "download");
        let xz = lock_path(Path::new("/c/proj-9.4.1.tar.xz"), "download");
        assert_ne!(gz, xz);
    }

    #[test]
    fn lock_is_reentrant_across_sequential_holders() {
        let dir = temp_dir("lock-seq");
        let path = lock_path(&dir.join("thing-1.2.3"), "build");
        for _ in 0..3 {
            let lock = Lock::acquire(&path).expect("acquire");
            drop(lock);
        }
        assert!(
            path.exists(),
            "lock file must outlive its holder; unlinking it allows two \
             processes to hold locks on different inodes at one path"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_key_is_deterministic_and_sensitive() {
        let base = BuildKey::new("geos").mix("a").as_str();
        assert_eq!(base, BuildKey::new("geos").mix("a").as_str());
        assert_ne!(base, BuildKey::new("geos").mix("b").as_str());
        assert_ne!(base, BuildKey::new("proj").mix("a").as_str());
        assert!(base.starts_with("geos-"));
    }

    #[test]
    fn build_key_order_is_significant() {
        let ab = BuildKey::new("x").mix("a").mix("b").as_str();
        let ba = BuildKey::new("x").mix("b").mix("a").as_str();
        assert_ne!(ab, ba);
    }

    #[test]
    fn project_root_unwraps_single_directory() {
        let dir = temp_dir("root-single");
        let inner = dir.join("geos-3.14.1");
        fs::create_dir_all(&inner).unwrap();
        fs::write(dir.join("stray.txt"), b"ignored").unwrap();
        assert_eq!(project_root(&dir), inner);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_root_keeps_flat_or_ambiguous_trees() {
        let flat = temp_dir("root-flat");
        fs::write(flat.join("CMakeLists.txt"), b"").unwrap();
        assert_eq!(project_root(&flat), flat);

        let many = temp_dir("root-many");
        fs::create_dir_all(many.join("a")).unwrap();
        fs::create_dir_all(many.join("b")).unwrap();
        assert_eq!(project_root(&many), many);

        let _ = fs::remove_dir_all(&flat);
        let _ = fs::remove_dir_all(&many);
    }

    #[test]
    fn is_complete_requires_every_marker() {
        let dir = temp_dir("complete");
        let dirs = BuildDirs {
            build: dir.clone(),
            prefix: dir.clone(),
        };
        fs::create_dir_all(dir.join("lib")).unwrap();
        assert!(dirs.is_complete(["lib"]));
        assert!(!dirs.is_complete(["lib", "include/geos.h"]));
        assert!(dirs.is_complete(Vec::<String>::new()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_dir_reproduces_nested_trees() {
        let src = temp_dir("copy-src");
        let dst = temp_dir("copy-dst");
        fs::create_dir_all(src.join("nested/deep")).unwrap();
        fs::write(src.join("top.txt"), b"top").unwrap();
        fs::write(src.join("nested/deep/leaf.txt"), b"leaf").unwrap();

        copy_dir(&src, &dst).expect("copy");

        assert_eq!(fs::read(dst.join("top.txt")).unwrap(), b"top");
        assert_eq!(fs::read(dst.join("nested/deep/leaf.txt")).unwrap(), b"leaf");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
    }
}
