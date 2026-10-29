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
//!   sources/   extracted trees, keyed by immutable source identity
//!   builds/    build trees + install prefixes, one per BuildKey
//! ```

use crate::{Dep, Error, Result};
use fs4::fs_std::FileExt;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

mod tree;
#[cfg(feature = "build")]
pub(crate) use tree::snapshot_fingerprint;
pub(crate) use tree::tree_fingerprint;

fn source_record(identity: &str, fingerprint: &str) -> String {
    format!("woodshed-source-v1\n{identity}\n{fingerprint}\n")
}

fn verified_source_tree(root: &Path, record: &Path, identity: &str) -> Result<bool> {
    let text = match fs::read(record) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(Error::io("read extracted source inventory", error)),
    };
    Ok(text == source_record(identity, &tree_fingerprint(root)?).as_bytes())
}

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
        println!("cargo:rerun-if-env-changed=WOODSHED_LOG");
        println!("cargo:rerun-if-env-changed=WOODSHED_PROGRESS");
        Self::from_environment()
    }

    /// Resolve cache/offline environment settings without emitting Cargo directives.
    /// Use this in CLI tools; build scripts should use [`Self::discover`].
    pub fn from_environment() -> Result<Self> {
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

    /// Mutable declarative workspaces; identities include the full hard fingerprint.
    pub fn workspace_dir(&self, key: &BuildKey) -> PathBuf {
        self.root.join("workspaces").join(key.fingerprint())
    }

    /// Source-owned immutable installation root, separate from mutable workspaces.
    pub fn artifact_dir(&self, key: &BuildKey) -> PathBuf {
        self.root.join("artifacts").join(key.fingerprint())
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

    /// Assemble a writable source tree with dependencies at relative paths.
    ///
    /// The root is copied, while dependencies are mounted as Unix symlinks or
    /// Windows directory junctions (which require no administrator privileges).
    /// Assembly does not modify the inputs. Mounted dependencies remain shared:
    /// builds must not write into them. On Windows, file symlinks are copied as
    /// files if the host does not grant symlink creation privileges.
    /// Use this for upstream projects that expect populated submodule directories.
    ///
    /// Trees live under `prepared/<key>` and have their own interprocess lock.
    /// Interrupted assembly is retried; a completion marker is written last.
    /// Include every source identity and mount path in `key`. Reusing a completed
    /// key with different inputs is an error rather than a silent cache collision.
    /// Existing nonempty directories are never removed to make room for a mount.
    pub fn prepare(
        &self,
        key: &BuildKey,
        source: &Path,
        mounts: &[(PathBuf, PathBuf)],
    ) -> Result<PathBuf> {
        use sha2::{Digest, Sha256};
        let source = source
            .canonicalize()
            .map_err(|error| Error::io(format!("cannot resolve {}", source.display()), error))?;
        let mut identity = Sha256::new();
        identity.update(source.as_os_str().as_encoded_bytes());
        let resolved = resolve_mounts(mounts)?;
        for (path, dependency) in &resolved {
            identity.update([0]);
            identity.update(path.as_os_str().as_encoded_bytes());
            identity.update([0]);
            identity.update(dependency.as_os_str().as_encoded_bytes());
        }
        let identity = format!("{:x}", identity.finalize());
        let directory = self.root.join("prepared").join(key.as_str());
        fs::create_dir_all(directory.parent().expect("prepared directory has a parent"))
            .map_err(|error| Error::io("cannot create prepared source directory", error))?;
        let _lock = Lock::acquire(&lock_path(&directory, "prepare"))?;
        let marker = directory.join(".woodshed-source-tree");
        if marker.exists() {
            if fs::read_to_string(&marker)
                .map_err(|error| Error::io("cannot read source-tree marker", error))?
                != identity
            {
                return Err(Error::io(
                    "source-tree key reused for different inputs",
                    std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        directory.display().to_string(),
                    ),
                ));
            }
        } else {
            copy_source_root(&source, &directory, Path::new(""), &resolved)?;
        }
        for (relative, dependency) in &resolved {
            install_mount(&directory, relative, dependency)?;
        }
        fs::write(&marker, identity)
            .map_err(|error| Error::io("cannot complete prepared source tree", error))?;
        Ok(directory)
    }

    /// Ensure a dependency's archive is present and checksum-verified.
    ///
    /// A cached archive is verified before reuse, so a truncated or tampered
    /// file is replaced rather than trusted. Downloads land in a
    /// process-unique `.partial` file and are renamed into place only after
    /// verification, so a concurrent or interrupted build never observes a
    /// half-written archive.
    pub fn archive(&self, dep: &Dep) -> Result<PathBuf> {
        self.locked_archive(dep).map(|(path, _lock)| path)
    }

    fn locked_archive(&self, dep: &Dep) -> Result<(PathBuf, Lock)> {
        if dep.git.is_some() {
            return Err(Error::Manifest {
                detail: format!(
                    "`{}` is a Git source, not an archive; use Cache::source",
                    dep.name
                ),
            });
        }
        let dir = self.archives_dir();
        fs::create_dir_all(&dir)
            .map_err(|error| Error::io(format!("cannot create {}", dir.display()), error))?;

        let path = dir.join(dep.archive_file());
        let _lock = Lock::acquire(&lock_path(&path, "download"))?;

        if path.is_file() {
            match crate::verify_sha256(&path, &dep.sha256) {
                Ok(()) => {
                    crate::progress::notify(&format!("{}: cached archive verified", dep.name));
                    return Ok((path, _lock));
                }
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

        self.download_verified(dep, &path).map(|path| (path, _lock))
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
    /// Archive trees are keyed by locked checksum, not release labels. A full
    /// content/type/permission fingerprint validates reuse; incomplete or changed
    /// trees are reconstructed from the checksum-verified archive. Extraction is
    /// staged and the completion record is committed last. Cached trees are
    /// immutable inputs: use `prepare` for builds that write into their sources.
    pub fn source(&self, dep: &Dep) -> Result<PathBuf> {
        let _progress = crate::progress::Phase::new(format!("acquire source {}", dep.name));
        if dep.git.is_some() {
            return self.git_source(dep);
        }
        if dep.sha256.len() != 64 || !dep.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::Manifest {
                detail: format!(
                    "{}: archive sources require a full SHA-256 checksum",
                    dep.name
                ),
            });
        }
        let sources = self.sources_dir();
        fs::create_dir_all(&sources)
            .map_err(|error| Error::io(format!("cannot create {}", sources.display()), error))?;

        let dir = sources.join(dep.source_dir_name());
        let _lock = Lock::acquire(&lock_path(&dir, "extract"))?;
        let stage = sidecar(&dir, "partial");
        if let Some(metadata) = path_metadata(&stage)? {
            remove_entry(&stage, &metadata)?;
        }
        let record = sidecar(&dir, "source");
        let identity = dep.sha256.to_ascii_lowercase();
        if dir.is_dir()
            && dep.markers_present(&project_root(&dir))
            && verified_source_tree(&dir, &record, &identity)?
        {
            crate::progress::notify(&format!("{}: cached source verified", dep.name));
            return Ok(project_root(&dir));
        }
        if path_metadata(&dir)?.is_some() {
            eprintln!(
                "woodshed: `{}` extracted source is incomplete or modified; restoring locked content",
                dep.name
            );
        }
        // Keep the download lock through extraction: different pinned archives
        // can share a filename, but must never replace bytes being extracted.
        let (archive, _archive_lock) = self.locked_archive(dep)?;
        fs::create_dir(&stage).map_err(|error| Error::io("stage archive extraction", error))?;
        {
            let _progress = crate::progress::Phase::new(format!("extract {}", dep.name));
            crate::archive::extract(&archive, &stage)?;
        }
        let root = project_root(&stage);
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
        let fingerprint = tree_fingerprint(&stage)?;
        if let Some(metadata) = path_metadata(&dir)? {
            remove_entry(&dir, &metadata)?;
        }
        fs::rename(&stage, &dir).map_err(|error| Error::io("publish extracted source", error))?;
        write_source_state(&record, source_record(&identity, &fingerprint).as_bytes())
            .map_err(|error| Error::io("commit extracted source inventory", error))?;
        Ok(project_root(&dir))
    }

    /// Validate cached pinned source contents without repairing or fetching.
    ///
    /// Useful after compilation to reject a build that modified shared inputs.
    pub fn verify_source(&self, dep: &Dep) -> Result<()> {
        let directory = self.sources_dir().join(dep.source_dir_name());
        let identity = match &dep.git {
            Some(git) => format!("{}\n{}\n", git.repository, git.revision),
            None => dep.sha256.to_ascii_lowercase(),
        };
        let root = if dep.git.is_some() {
            directory.clone()
        } else {
            project_root(&directory)
        };
        if !dep.markers_present(&root)
            || !verified_source_tree(&directory, &sidecar(&directory, "source"), &identity)?
        {
            return Err(Error::Incomplete {
                detail: format!(
                    "{}: shared pinned source changed during compilation; use a writable prepared tree instead",
                    dep.name
                ),
            });
        }
        Ok(())
    }

    #[cfg(feature = "git")]
    fn git_source(&self, dep: &Dep) -> Result<PathBuf> {
        let git = dep.git.as_ref().expect("Git source");
        crate::dep::validate_revision(&git.revision)?;
        let dir = self.sources_dir().join(dep.source_dir_name());
        fs::create_dir_all(self.sources_dir())
            .map_err(|error| Error::io("create Git source cache", error))?;
        let _lock = Lock::acquire(&lock_path(&dir, "git"))?;
        let marker = dir.join(".woodshed-git");
        let identity = format!("{}\n{}\n", git.repository, git.revision);
        let record = sidecar(&dir, "source");
        if marker.is_file()
            && fs::read_to_string(&marker)
                .map_err(|error| Error::io("read Git source marker", error))?
                == identity
            && dep.markers_present(&dir)
            && verified_source_tree(&dir, &record, &identity)?
        {
            return Ok(dir);
        }
        if self.offline {
            return Err(Error::Offline {
                detail: format!(
                    "immutable Git source {} at {} is missing or incomplete; populate {} first",
                    git.repository,
                    git.revision,
                    dir.display()
                ),
            });
        }
        let snapshot = crate::git::Repository::pinned(&git.repository, &git.revision)?;
        let stage = tempfile::Builder::new()
            .prefix(".woodshed-git-")
            .tempdir_in(self.sources_dir())
            .map_err(|error| Error::io("stage Git source", error))?;
        snapshot.export(stage.path())?;
        if let Some(missing) = dep.missing_marker(stage.path()) {
            return Err(Error::Incomplete {
                detail: format!("Git source `{}` lacks marker {missing}", dep.name),
            });
        }
        fs::write(stage.path().join(".woodshed-git"), &identity)
            .map_err(|error| Error::io("mark Git source complete", error))?;
        let fingerprint = tree_fingerprint(stage.path())?;
        if dir.exists() {
            fs::remove_dir_all(&dir)
                .map_err(|error| Error::io("remove incomplete Git snapshot", error))?;
        }
        fs::rename(stage.path(), &dir).map_err(|error| Error::io("install Git snapshot", error))?;
        write_source_state(&record, source_record(&identity, &fingerprint).as_bytes())
            .map_err(|error| Error::io("commit Git source inventory", error))?;
        Ok(dir)
    }

    #[cfg(not(feature = "git"))]
    fn git_source(&self, dep: &Dep) -> Result<PathBuf> {
        Err(Error::Tool {
            what: format!("provision Git source `{}`", dep.name),
            detail: "enable Woodshed's `git` feature".into(),
        })
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
        self.mirror_to(source, &dest)
    }

    /// Synchronize a source tree into an explicit, exclusively owned destination.
    ///
    /// Unchanged file contents keep their timestamps; removed entries and changed
    /// file types are synchronized. Source symlinks are materialized as in
    /// [`Self::mirror`]; destination links are never traversed while updating.
    /// Source and destination must be disjoint. A sibling lock serializes refreshes;
    /// callers must separately coordinate readers/builds with refreshes.
    /// Returns the caller's absolute destination spelling, without canonicalizing
    /// build-facing paths or introducing Windows verbatim prefixes.
    pub fn mirror_to(&self, source: &Path, destination: &Path) -> Result<PathBuf> {
        self.mirror_filtered(source, destination, |_| false)
    }

    pub(crate) fn mirror_filtered(
        &self,
        source: &Path,
        destination: &Path,
        ignore: fn(&std::ffi::OsStr) -> bool,
    ) -> Result<PathBuf> {
        let output = std::path::absolute(destination)
            .map_err(|error| Error::io("cannot resolve mirror destination", error))?;
        let (source, destination) = source_destination(source, destination)?;
        let _lock = Lock::acquire(&lock_path(&destination, "mirror"))?;
        let root = resolve_path(&destination)?;
        sync_dir(
            &source,
            &destination,
            &root,
            &mut std::collections::BTreeSet::new(),
            ignore,
        )?;
        Ok(output)
    }

    /// Prepare a stable writable root with additive pinned dependency mounts.
    ///
    /// Unlike [`Self::prepare`], the key identifies only the pinned source family:
    /// adding mounts does not change the root path. Previously recorded mounts
    /// remain when omitted and cannot be rebound, removed, or overlapped.
    /// Source and dependency identities are canonical paths, not content hashes;
    /// callers must supply immutable pinned source trees.
    pub fn prepare_incremental(
        &self,
        key: &BuildKey,
        source: &Path,
        mounts: &[(PathBuf, PathBuf)],
    ) -> Result<PathBuf> {
        self.prepare_incremental_to(
            source,
            &self.root.join("prepared").join(key.as_str()),
            mounts,
        )
    }

    /// Prepare or adopt an explicit stable root, adding mounts without recopying it.
    ///
    /// An existing directory is adopted as-is, preserving patches and generated
    /// files. New roots copy the source using the same symlink conventions as
    /// [`Self::prepare`]. A sibling identity journal and prepare lock retain the
    /// canonical source and every mount across calls, including interrupted calls.
    /// Conflicting links, files, nonempty directories, escaping paths, and mount
    /// ancestors that are links are rejected rather than overwritten.
    /// Keep the sibling `.incremental` directory alongside an adopted root.
    /// Returns the caller's absolute destination spelling; canonical paths are
    /// used only internally for identity, locking, and safety checks.
    pub fn prepare_incremental_to(
        &self,
        source: &Path,
        destination: &Path,
        mounts: &[(PathBuf, PathBuf)],
    ) -> Result<PathBuf> {
        let output = std::path::absolute(destination)
            .map_err(|error| Error::io("cannot resolve prepared destination", error))?;
        let (source, destination) = source_destination(source, destination)?;
        let _lock = Lock::acquire(&lock_path(&destination, "prepare"))?;
        let state = lock_path(&destination, "incremental");
        fs::create_dir_all(&state)
            .map_err(|error| Error::io("cannot create incremental source state", error))?;
        let source_marker = state.join("source");
        let identity = encode_path(&source);
        let initialized = source_marker
            .try_exists()
            .map_err(|error| Error::io("cannot inspect incremental source identity", error))?;
        if initialized
            && fs::read_to_string(&source_marker)
                .map_err(|error| Error::io("cannot read incremental source identity", error))?
                != identity
        {
            return Err(source_conflict(
                "source-tree key reused for different inputs",
                &destination,
            ));
        }
        let journal = state.join("mounts");
        let mut recorded = Vec::new();
        match fs::read_to_string(&journal) {
            Ok(text) => {
                if !text.is_empty() && !text.ends_with('\n') {
                    return Err(source_conflict("incomplete source mount journal", &journal));
                }
                for line in text.lines() {
                    let (path, target) = line
                        .split_once('\t')
                        .ok_or_else(|| source_conflict("invalid source mount journal", &journal))?;
                    recorded.push((decode_path(path)?, decode_path(target)?));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io("cannot read source mount journal", error)),
        }
        validate_mounts(&recorded)?;
        let old_count = recorded.len();
        let requested = resolve_mounts(mounts)?;
        for (path, target) in &requested {
            if let Some((_, existing)) = recorded
                .iter()
                .find(|(existing, _)| mount_path(existing) == mount_path(path))
            {
                if existing != target {
                    return Err(source_conflict("source mount identity changed", path));
                }
            } else {
                recorded.push((path.clone(), target.clone()));
            }
        }
        validate_mounts(&recorded)?;
        for (path, target) in &recorded {
            if target.starts_with(&destination) || destination.starts_with(target) {
                return Err(source_conflict("source mount overlaps prepared root", path));
            }
            check_mount(&destination, path, target)?;
        }
        if !initialized {
            // Record adoption before mutation; retries of a new tree must finish
            // copying, while retries of an adopted tree must never overwrite it.
            if destination.is_dir() {
                write_source_state(&state.join("copied"), b"")
                    .map_err(|error| Error::io("cannot record source adoption", error))?;
            }
            write_source_state(&source_marker, identity.as_bytes())
                .map_err(|error| Error::io("cannot record incremental source identity", error))?;
        }
        if recorded.len() != old_count {
            let text: String = recorded
                .iter()
                .map(|(path, target)| format!("{}\t{}\n", encode_path(path), encode_path(target)))
                .collect();
            write_source_state(&journal, text.as_bytes())
                .map_err(|error| Error::io("cannot persist source mounts", error))?;
        }
        if !state
            .join("copied")
            .try_exists()
            .map_err(|error| Error::io("cannot inspect source copy marker", error))?
        {
            copy_source_root(&source, &destination, Path::new(""), &recorded)?;
            write_source_state(&state.join("copied"), b"")
                .map_err(|error| Error::io("cannot complete incremental source copy", error))?;
        }
        if !destination.is_dir() {
            return Err(source_conflict("prepared root is missing", &destination));
        }
        for (path, target) in &recorded {
            install_mount(&destination, path, target)?;
        }
        Ok(output)
    }
}

fn source_conflict(context: &str, path: &Path) -> Error {
    Error::io(
        context,
        std::io::Error::new(std::io::ErrorKind::InvalidInput, path.display().to_string()),
    )
}

fn write_source_state(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_nanos();
    let temporary = lock_path(path, &format!("{}-{stamp}.partial", std::process::id()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn resolve_path(path: &Path) -> Result<PathBuf> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let absolute = std::path::absolute(path)
                .map_err(|error| Error::io("cannot resolve destination", error))?;
            let parent = absolute
                .parent()
                .ok_or_else(|| source_conflict("invalid destination", path))?;
            Ok(resolve_path(parent)?.join(
                absolute
                    .file_name()
                    .ok_or_else(|| source_conflict("invalid destination", path))?,
            ))
        }
        Err(error) => Err(Error::io("cannot resolve destination", error)),
    }
}

fn source_destination(source: &Path, destination: &Path) -> Result<(PathBuf, PathBuf)> {
    let source = source
        .canonicalize()
        .map_err(|error| Error::io("cannot resolve source directory", error))?;
    if !source.is_dir() {
        return Err(source_conflict("source is not a directory", &source));
    }
    if path_metadata(destination)?.is_some_and(|meta| is_link(&meta) || !meta.is_dir()) {
        return Err(source_conflict(
            "destination must be a real directory",
            destination,
        ));
    }
    let destination = resolve_path(destination)?;
    if source.starts_with(&destination) || destination.starts_with(&source) {
        return Err(source_conflict(
            "source and destination overlap",
            &destination,
        ));
    }
    Ok((source, destination))
}

fn path_metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::io(
            format!("cannot inspect {}", path.display()),
            error,
        )),
    }
}

pub(crate) fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

fn validate_mounts(mounts: &[(PathBuf, PathBuf)]) -> Result<()> {
    use std::path::Component;
    for (index, (path, target)) in mounts.iter().enumerate() {
        if path.as_os_str().is_empty()
            || !path
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
            || mounts[..index].iter().any(|(other, _)| {
                let path = mount_path(path);
                let other = mount_path(other);
                path.starts_with(&other) || other.starts_with(&path)
            })
            || !target.is_dir()
        {
            return Err(source_conflict("invalid or overlapping source mount", path));
        }
        #[cfg(windows)]
        for component in path.components() {
            let component = component.as_os_str().to_string_lossy();
            if component.ends_with(['.', ' ']) || component.contains(':') {
                return Err(source_conflict("ambiguous Windows source mount", path));
            }
        }
    }
    Ok(())
}

fn resolve_mounts(mounts: &[(PathBuf, PathBuf)]) -> Result<Vec<(PathBuf, PathBuf)>> {
    let resolved = mounts
        .iter()
        .map(|(path, target)| {
            target
                .canonicalize()
                .map(|target| (path.clone(), target))
                .map_err(|error| Error::io(format!("cannot resolve {}", target.display()), error))
        })
        .collect::<Result<Vec<_>>>()?;
    validate_mounts(&resolved)?;
    Ok(resolved)
}

fn check_mount(root: &Path, relative: &Path, target: &Path) -> Result<()> {
    let mut parent = root.to_path_buf();
    for component in relative.parent().into_iter().flat_map(Path::components) {
        parent.push(component);
        if path_metadata(&parent)?.is_some_and(|meta| is_link(&meta) || !meta.is_dir()) {
            return Err(source_conflict(
                "source mount ancestor is not a real directory",
                &parent,
            ));
        }
    }
    let destination = root.join(relative);
    if let Some(meta) = path_metadata(&destination)? {
        if is_link(&meta)
            && destination
                .canonicalize()
                .is_ok_and(|existing| existing == target)
        {
            return Ok(());
        }
        if is_link(&meta)
            || !meta.is_dir()
            || fs::read_dir(&destination)
                .map_err(|error| Error::io("cannot inspect source mount directory", error))?
                .next()
                .transpose()
                .map_err(|error| Error::io("cannot inspect source mount entry", error))?
                .is_some()
        {
            return Err(source_conflict(
                "source mount conflicts with existing destination",
                &destination,
            ));
        }
    }
    Ok(())
}

fn mount_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

fn install_mount(root: &Path, relative: &Path, target: &Path) -> Result<()> {
    check_mount(root, relative, target)?;
    let destination = root.join(relative);
    if path_metadata(&destination)?.is_some_and(|meta| is_link(&meta)) {
        return Ok(());
    }
    if destination.exists() {
        fs::remove_dir(&destination)
            .map_err(|error| Error::io("cannot replace empty mount placeholder", error))?;
    }
    fs::create_dir_all(destination.parent().expect("mount parent"))
        .map_err(|error| Error::io("cannot create source mount parent", error))?;
    #[cfg(windows)]
    junction::create(target, &destination)
        .map_err(|error| Error::io("cannot create source mount", error))?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, &destination)
        .map_err(|error| Error::io("cannot create source mount", error))?;
    #[cfg(not(any(windows, unix)))]
    return Err(source_conflict(
        "source mounts require Unix or Windows",
        &destination,
    ));
    Ok(())
}

fn encode_path(path: &Path) -> String {
    #[cfg(windows)]
    let bytes: Vec<u8> = {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect()
    };
    #[cfg(not(windows))]
    let bytes = path.as_os_str().as_encoded_bytes();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_path(text: &str) -> Result<PathBuf> {
    let invalid = || source_conflict("invalid path in source mount journal", Path::new(text));
    if !text.is_ascii() || !text.len().is_multiple_of(2) {
        return Err(invalid());
    }
    let bytes = (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).map_err(|_| invalid()))
        .collect::<Result<Vec<_>>>()?;
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        if !bytes.len().is_multiple_of(2) {
            return Err(invalid());
        }
        let wide = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        Ok(std::ffi::OsString::from_wide(&wide).into())
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(std::ffi::OsString::from_vec(bytes).into())
    }
    #[cfg(not(any(windows, unix)))]
    {
        Ok(String::from_utf8(bytes).map_err(|_| invalid())?.into())
    }
}

fn copy_source_root(
    source: &Path,
    destination: &Path,
    relative: &Path,
    mounts: &[(PathBuf, PathBuf)],
) -> Result<()> {
    if path_metadata(destination)?.is_some_and(|meta| is_link(&meta) || !meta.is_dir()) {
        return Err(source_conflict(
            "source-tree directory conflicts with destination",
            destination,
        ));
    }
    fs::create_dir_all(destination)
        .map_err(|error| Error::io("cannot create source-tree directory", error))?;
    for entry in
        fs::read_dir(source).map_err(|error| Error::io("cannot read source tree", error))?
    {
        let entry = entry.map_err(|error| Error::io("cannot read source-tree entry", error))?;
        let relative = relative.join(entry.file_name());
        if mounts.iter().any(|(mount, _)| relative.starts_with(mount)) {
            continue;
        }
        let target = destination.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|error| Error::io("cannot inspect source-tree entry", error))?;
        if kind.is_symlink() {
            let link = fs::read_link(entry.path())
                .map_err(|error| Error::io("cannot read source symlink", error))?;
            #[cfg(windows)]
            if target.exists()
                && (entry.path().canonicalize().is_ok_and(|resolved| {
                    target
                        .canonicalize()
                        .is_ok_and(|existing| existing == resolved)
                }) || (target.is_file()
                    && entry.path().is_file()
                    && crate::sha256_file(&target)? == crate::sha256_file(&entry.path())?))
            {
                continue;
            }
            if let Ok(existing) = fs::read_link(&target) {
                if existing == link {
                    continue;
                }
                return Err(Error::io(
                    "source-tree symlink conflicts with existing destination",
                    std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        target.display().to_string(),
                    ),
                ));
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link, &target).map_err(|error| {
                Error::io(
                    format!("cannot preserve symlink {}", target.display()),
                    error,
                )
            })?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::FileTypeExt;
                let result = if kind.is_symlink_dir() {
                    std::os::windows::fs::symlink_dir(&link, &target)
                } else {
                    std::os::windows::fs::symlink_file(&link, &target)
                };
                match result {
                    Ok(()) => {}
                    Err(error) if error.raw_os_error() == Some(1314) => {
                        let resolved = entry
                            .path()
                            .canonicalize()
                            .map_err(|error| Error::io("cannot resolve source symlink", error))?;
                        if kind.is_symlink_dir() {
                            junction::create(&resolved, &target).map_err(|error| {
                                Error::io("cannot mount source directory link", error)
                            })?;
                        } else {
                            fs::copy(&resolved, &target).map_err(|error| {
                                Error::io("cannot materialize source file link", error)
                            })?;
                        }
                    }
                    Err(error) => {
                        return Err(Error::io(
                            format!("cannot preserve symlink {}", target.display()),
                            error,
                        ));
                    }
                }
            }
        } else if kind.is_dir() {
            copy_source_root(&entry.path(), &target, &relative, mounts)?;
        } else {
            if path_metadata(&target)?.is_some_and(|meta| is_link(&meta) || !meta.is_file()) {
                return Err(source_conflict(
                    "source-tree file conflicts with destination",
                    &target,
                ));
            }
            if target.is_file()
                && crate::sha256_file(&entry.path())? == crate::sha256_file(&target)?
            {
                continue;
            }
            fs::copy(entry.path(), &target).map_err(|error| {
                Error::io(
                    format!(
                        "cannot copy {} to {}",
                        entry.path().display(),
                        target.display()
                    ),
                    error,
                )
            })?;
        }
    }
    Ok(())
}

/// Synchronize materialized entries, never following destination links.
#[cfg(test)]
fn copy_dir(source: &Path, dest: &Path) -> Result<()> {
    let root = resolve_path(dest)?;
    sync_dir(
        source,
        dest,
        &root,
        &mut std::collections::BTreeSet::new(),
        |_| false,
    )
}

fn sync_dir(
    source: &Path,
    dest: &Path,
    root: &Path,
    ancestors: &mut std::collections::BTreeSet<PathBuf>,
    ignore: fn(&std::ffi::OsStr) -> bool,
) -> Result<()> {
    let resolved = source
        .canonicalize()
        .map_err(|error| Error::io("cannot resolve mirror source directory", error))?;
    if resolved.starts_with(root)
        || root.starts_with(&resolved)
        || !ancestors.insert(resolved.clone())
    {
        return Err(source_conflict(
            "mirror source link cycles or overlaps destination",
            source,
        ));
    }
    if let Some(meta) = path_metadata(dest)?
        && (is_link(&meta) || !meta.is_dir())
    {
        remove_entry(dest, &meta)?;
    }
    fs::create_dir_all(dest)
        .map_err(|error| Error::io(format!("cannot create {}", dest.display()), error))?;

    let entries = fs::read_dir(source)
        .map_err(|error| Error::io(format!("cannot read {}", source.display()), error))?;

    for entry in entries {
        let entry =
            entry.map_err(|error| Error::io(format!("cannot read {}", source.display()), error))?;
        if ignore(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        let target = dest.join(entry.file_name());

        if path.is_dir() {
            sync_dir(&path, &target, root, ancestors, ignore)?;
        } else {
            if let Some(meta) = path_metadata(&target)? {
                if is_link(&meta) || !meta.is_file() {
                    remove_entry(&target, &meta)?;
                } else if crate::sha256_file(&path)? == crate::sha256_file(&target)? {
                    let permissions = fs::metadata(&path)
                        .map_err(|error| Error::io("inspect mirror input permissions", error))?
                        .permissions();
                    if permissions != meta.permissions() {
                        fs::set_permissions(&target, permissions)
                            .map_err(|error| Error::io("synchronize mirror permissions", error))?;
                    }
                    continue;
                }
            }
            #[cfg(windows)]
            if let Some(meta) = path_metadata(&target)? {
                make_writable(&target, &meta)
                    .map_err(|error| Error::io("cannot refresh read-only mirror file", error))?;
            }
            fs::copy(&path, &target).map_err(|error| {
                Error::io(
                    format!("cannot copy {} to {}", path.display(), target.display()),
                    error,
                )
            })?;
            // CopyFile on Windows preserves input mtimes, even when changed
            // bytes have the same timestamp as the previous mirror contents.
            #[cfg(windows)]
            let file = {
                use std::os::windows::fs::OpenOptionsExt;
                fs::OpenOptions::new().access_mode(0x100).open(&target)
            };
            #[cfg(not(windows))]
            let file = fs::File::open(&target);
            file.and_then(|file| file.set_modified(std::time::SystemTime::now()))
                .map_err(|error| Error::io("cannot timestamp refreshed mirror file", error))?;
        }
    }
    for entry in
        fs::read_dir(dest).map_err(|error| Error::io("cannot read mirror destination", error))?
    {
        let entry = entry.map_err(|error| Error::io("cannot read mirror entry", error))?;
        if ignore(&entry.file_name()) || path_metadata(&source.join(entry.file_name()))?.is_none() {
            let meta = fs::symlink_metadata(entry.path())
                .map_err(|error| Error::io("cannot inspect obsolete mirror entry", error))?;
            remove_entry(&entry.path(), &meta)?;
        }
    }
    ancestors.remove(&resolved);
    Ok(())
}

pub(crate) fn remove_entry(path: &Path, meta: &fs::Metadata) -> Result<()> {
    #[cfg(windows)]
    if !is_link(meta) {
        make_writable(path, meta)
            .map_err(|error| Error::io("cannot remove read-only mirror entry", error))?;
    }
    #[cfg(windows)]
    let directory_link = is_link(meta) && {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x10 != 0
    };
    #[cfg(not(windows))]
    let directory_link = false;
    let result = if directory_link {
        fs::remove_dir(path)
    } else if meta.is_dir() && !is_link(meta) {
        for entry in fs::read_dir(path)
            .map_err(|error| Error::io("cannot read obsolete mirror directory", error))?
        {
            let entry =
                entry.map_err(|error| Error::io("cannot read obsolete mirror entry", error))?;
            let meta = fs::symlink_metadata(entry.path())
                .map_err(|error| Error::io("cannot inspect obsolete mirror entry", error))?;
            remove_entry(&entry.path(), &meta)?;
        }
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(|error| Error::io(format!("cannot remove {}", path.display()), error))
}

#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)] // Windows attributes, not Unix mode bits.
fn make_writable(path: &Path, meta: &fs::Metadata) -> std::io::Result<()> {
    let mut permissions = meta.permissions();
    if permissions.readonly() {
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)?;
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

    /// Start an explicit identity without implicit process-environment inputs.
    ///
    /// The caller must supply every relevant input. Declarative orchestration
    /// uses structured identities rather than inheriting Cargo's target for host tools.
    pub fn explicit(label: impl Into<String>) -> Self {
        use sha2::Digest;
        Self {
            hasher: sha2::Sha256::new(),
            label: label.into(),
        }
    }

    /// Mix in a dependency's pinned identity.
    #[must_use]
    pub fn dep(self, dep: &Dep) -> Self {
        if let Some(git) = &dep.git {
            return self
                .mix("git")
                .mix(&dep.version)
                .mix(&git.repository)
                .mix(&git.revision);
        }
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

    /// Full SHA-256 identity for immutable artifacts and hard workspace boundaries.
    pub fn fingerprint(&self) -> String {
        use sha2::Digest;
        let mut hash = self.hasher.clone();
        hash.update(b"\0woodshed-label\0");
        hash.update((self.label.len() as u64).to_le_bytes());
        hash.update(self.label.as_bytes());
        format!("{:x}", hash.finalize())
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
pub(crate) struct Lock {
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
pub(crate) fn lock_path(path: &Path, kind: &str) -> PathBuf {
    sidecar(path, &format!("{kind}.lock"))
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{suffix}"));
    path.with_file_name(name)
}

impl Lock {
    #[cfg(feature = "build")]
    pub(crate) fn acquire_shared(path: &Path) -> Result<Self> {
        Self::acquire_mode(path, true)
    }

    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        Self::acquire_mode(path, false)
    }

    fn acquire_mode(path: &Path, shared: bool) -> Result<Self> {
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
        let mut waiting = None;
        loop {
            match if shared {
                FileExt::try_lock_shared(&file)
            } else {
                FileExt::try_lock_exclusive(&file)
            } {
                Ok(true) => {
                    return Ok(Self {
                        file,
                        path: path.to_path_buf(),
                    });
                }
                Ok(false) | Err(_) if std::time::Instant::now() < deadline => {
                    waiting.get_or_insert_with(|| {
                        crate::progress::Phase::new(format!("wait for lock {}", path.display()))
                    });
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

    #[cfg(feature = "tar-gz")]
    #[test]
    fn archive_sources_are_content_addressed_verified_and_repaired_offline() {
        fn archive(path: &Path, contents: &[u8]) -> String {
            let file = fs::File::create(path).unwrap();
            let compressed = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            let mut tar = tar::Builder::new(compressed);
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(contents.len() as u64);
            header.set_cksum();
            tar.append_data(&mut header, "package/input.h", contents)
                .unwrap();
            tar.into_inner().unwrap().finish().unwrap();
            crate::sha256_file(path).unwrap()
        }

        let directory = temp_dir("source-content");
        let cache = Cache::at(&directory).offline(true);
        fs::create_dir_all(cache.archives_dir()).unwrap();
        let archive_path = cache.archives_dir().join("package.tar.gz");
        let first = Dep::new(
            "same",
            "1",
            "https://example.test/package.tar.gz",
            archive(&archive_path, b"first!"),
        )
        .with_markers(["input.h"]);
        let unrelated = cache.sources_dir().join("same-1");
        fs::create_dir_all(&unrelated).unwrap();
        fs::write(unrelated.join("input.h"), b"unrelated").unwrap();
        let original = cache.source(&first).unwrap();
        assert_ne!(original, unrelated);
        assert_eq!(fs::read(unrelated.join("input.h")).unwrap(), b"unrelated");
        let first_modified = fs::metadata(original.join("input.h"))
            .unwrap()
            .modified()
            .unwrap();
        let second = Dep::new(
            "same",
            "1",
            "https://example.test/package.tar.gz",
            archive(&archive_path, b"second"),
        )
        .with_markers(["input.h"]);
        let updated = cache.source(&second).unwrap();
        assert_ne!(original, updated);
        assert_eq!(fs::read(original.join("input.h")).unwrap(), b"first!");
        assert_eq!(fs::read(updated.join("input.h")).unwrap(), b"second");
        assert_eq!(cache.source(&first).unwrap(), original);
        assert_eq!(
            fs::metadata(original.join("input.h"))
                .unwrap()
                .modified()
                .unwrap(),
            first_modified
        );
        let modified = fs::metadata(updated.join("input.h"))
            .unwrap()
            .modified()
            .unwrap();
        fs::write(updated.join("input.h"), b"BROKEN").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(updated.join("input.h"))
            .unwrap()
            .set_modified(modified)
            .unwrap();
        fs::write(updated.join("unexpected"), b"remove").unwrap();
        assert_eq!(cache.source(&second).unwrap(), updated);
        assert_eq!(fs::read(updated.join("input.h")).unwrap(), b"second");
        assert!(!updated.join("unexpected").exists());
        fs::remove_file(updated.join("input.h")).unwrap();
        cache.source(&second).unwrap();
        assert_eq!(fs::read(updated.join("input.h")).unwrap(), b"second");
        let incomplete = second.clone().with_markers(["absent.h"]);
        assert!(cache.source(&incomplete).is_err());
        assert_eq!(fs::read(updated.join("input.h")).unwrap(), b"second");
        fs::remove_file(&archive_path).unwrap();
        assert_eq!(cache.source(&second).unwrap(), updated);
        let record = sidecar(updated.parent().unwrap(), "source");
        fs::write(&record, b"invalid").unwrap();
        assert!(
            cache.source(&second).is_err(),
            "unverified extraction needs a verified archive"
        );
        assert_eq!(archive(&archive_path, b"second"), second.sha256);
        assert_eq!(cache.source(&second).unwrap(), updated);
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[test]
    fn managed_tree_fingerprints_preserve_relative_links_and_reject_escapes_and_cycles() {
        let directory = temp_dir("tree-links");
        let root = directory.join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("file"), b"content").unwrap();
        std::os::unix::fs::symlink("file", root.join("alias")).unwrap();
        let original = tree_fingerprint(&root).unwrap();
        assert_eq!(tree_fingerprint(&root).unwrap(), original);
        fs::write(root.join("file"), b"changed").unwrap();
        assert_ne!(tree_fingerprint(&root).unwrap(), original);
        fs::write(directory.join("outside"), b"outside").unwrap();
        std::os::unix::fs::symlink("../outside", root.join("escape")).unwrap();
        assert!(tree_fingerprint(&root).is_err());
        fs::remove_file(root.join("escape")).unwrap();
        std::os::unix::fs::symlink(".", root.join("cycle")).unwrap();
        assert!(tree_fingerprint(&root).is_err());
        fs::remove_dir_all(directory).unwrap();
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

    #[test]
    fn mirror_preserves_unchanged_timestamps_and_updates_by_content() {
        let root = temp_dir("mirror-content");
        let source = root.join("source");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("same"), b"same").unwrap();
        fs::write(source.join("changed"), b"before").unwrap();
        fs::write(source.join("nested/removed"), b"remove").unwrap();
        let cache = Cache::at(root.join("cache"));
        let key = BuildKey::new("mirror");
        let dest = cache.mirror(&source, &key).unwrap();
        let old = std::time::UNIX_EPOCH + Duration::from_secs(946684800);
        for path in [
            source.join("changed"),
            dest.join("changed"),
            dest.join("same"),
        ] {
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        fs::write(source.join("changed"), b"AFTER!").unwrap();
        fs::File::options()
            .write(true)
            .open(source.join("changed"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
        fs::remove_file(source.join("nested/removed")).unwrap();
        fs::write(source.join("added"), b"added").unwrap();
        assert_eq!(cache.mirror(&source, &key).unwrap(), dest);
        assert_eq!(
            fs::metadata(dest.join("same")).unwrap().modified().unwrap(),
            old
        );
        assert_eq!(fs::read(dest.join("changed")).unwrap(), b"AFTER!");
        assert_ne!(
            fs::metadata(dest.join("changed"))
                .unwrap()
                .modified()
                .unwrap(),
            old
        );
        assert!(!dest.join("nested/removed").exists());
        assert_eq!(fs::read(dest.join("added")).unwrap(), b"added");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mirror_handles_file_directory_transitions_and_explicit_paths() {
        let root = temp_dir("mirror-types");
        let source = root.join("source");
        let dest = root.join("existing-mirror");
        fs::create_dir_all(source.join("directory")).unwrap();
        fs::create_dir_all(dest.join("file")).unwrap();
        fs::write(source.join("file"), b"file").unwrap();
        fs::write(source.join("directory/child"), b"child").unwrap();
        fs::write(dest.join("directory"), b"previous file").unwrap();
        fs::write(dest.join("file/old"), b"previous directory").unwrap();
        fs::create_dir_all(dest.join("obsolete/nested")).unwrap();
        let cache = Cache::at(root.join("cache"));
        let result = cache.mirror_to(&source, &dest).unwrap();
        assert_eq!(result, std::path::absolute(&dest).unwrap());
        assert_eq!(fs::read(dest.join("file")).unwrap(), b"file");
        assert_eq!(fs::read(dest.join("directory/child")).unwrap(), b"child");
        assert!(!dest.join("obsolete").exists());
        assert!(cache.mirror_to(&source, &source).is_err());
        assert!(cache.mirror_to(&source, &source.join("inside")).is_err());
        assert!(cache.mirror_to(&source, &root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    fn directory_link(source: &Path, destination: &Path) {
        #[cfg(windows)]
        junction::create(source, destination).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(source, destination).unwrap();
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn build_facing_paths_preserve_destination_spelling() {
        let root = temp_dir("destination-spelling");
        let source = root.join("source");
        let real = root.join("real");
        let alias = root.join("alias");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&real).unwrap();
        fs::write(source.join("input"), b"input").unwrap();
        directory_link(&real, &alias);
        let cache = Cache::at(alias.join("cache"));
        let key = BuildKey::new("paths");
        let mirror = cache.mirror(&source, &key).unwrap();
        assert_eq!(mirror, alias.join("cache/mirrors").join(key.as_str()));
        assert_eq!(cache.mirror(&source, &key).unwrap(), mirror);
        let explicit = alias.join("explicit-mirror");
        assert_eq!(cache.mirror_to(&source, &explicit).unwrap(), explicit);
        let prepared = cache.prepare_incremental(&key, &source, &[]).unwrap();
        assert_eq!(prepared, alias.join("cache/prepared").join(key.as_str()));
        assert_eq!(
            cache.prepare_incremental(&key, &source, &[]).unwrap(),
            prepared
        );
        let explicit = alias.join("explicit-prepared");
        assert_eq!(
            cache
                .prepare_incremental_to(&source, &explicit, &[])
                .unwrap(),
            explicit
        );
        // Aliases share canonical identity and state, not independent workspaces.
        fs::write(prepared.join("input"), b"patched").unwrap();
        cache
            .prepare_incremental_to(
                &source,
                &real.join("cache/prepared").join(key.as_str()),
                &[],
            )
            .unwrap();
        assert_eq!(fs::read(prepared.join("input")).unwrap(), b"patched");
        assert_ne!(prepared, prepared.canonicalize().unwrap());
        #[cfg(windows)]
        for path in [mirror, prepared, explicit] {
            assert!(!path.as_os_str().to_string_lossy().starts_with(r"\\?\"));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn mirror_materializes_source_links_but_never_writes_through_destination_links() {
        let root = temp_dir("mirror-links");
        let source = root.join("source");
        let dest = root.join("destination");
        let external = root.join("external");
        fs::create_dir_all(source.join("directory")).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::create_dir_all(&external).unwrap();
        fs::write(external.join("keep"), b"keep").unwrap();
        fs::write(source.join("directory/new"), b"new").unwrap();
        directory_link(&external, &dest.join("directory"));
        directory_link(&external, &dest.join("obsolete"));
        directory_link(&external, &source.join("linked"));
        let cache = Cache::at(root.join("cache"));
        cache.mirror_to(&source, &dest).unwrap();
        assert_eq!(fs::read(external.join("keep")).unwrap(), b"keep");
        assert!(!external.join("new").exists());
        assert!(!is_link(
            &fs::symlink_metadata(dest.join("directory")).unwrap()
        ));
        assert!(!is_link(
            &fs::symlink_metadata(dest.join("linked")).unwrap()
        ));
        assert_eq!(fs::read(dest.join("linked/keep")).unwrap(), b"keep");
        assert!(!dest.join("obsolete").exists());
        directory_link(&source, &source.join("cycle"));
        assert!(cache.mirror_to(&source, &dest).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incremental_sources_keep_root_patches_and_accumulate_pinned_mounts() {
        let root = temp_dir("incremental");
        let source = root.join("source");
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir_all(source.join("deps/first")).unwrap();
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(source.join("root.txt"), b"upstream").unwrap();
        fs::write(first.join("first.h"), b"first").unwrap();
        fs::write(second.join("second.h"), b"second").unwrap();
        let cache = Cache::at(root.join("cache"));
        let key = BuildKey::new("family");
        let prepared = cache
            .prepare_incremental(&key, &source, &[("deps/first".into(), first.clone())])
            .unwrap();
        fs::write(prepared.join("root.txt"), b"patched").unwrap();
        let modified = fs::metadata(prepared.join("root.txt"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            cache
                .prepare_incremental(&key, &source, &[("deps/second".into(), second.clone()),])
                .unwrap(),
            prepared
        );
        assert_eq!(
            cache.prepare_incremental(&key, &source, &[]).unwrap(),
            prepared
        );
        assert_eq!(fs::read(prepared.join("root.txt")).unwrap(), b"patched");
        assert_eq!(
            fs::metadata(prepared.join("root.txt"))
                .unwrap()
                .modified()
                .unwrap(),
            modified
        );
        assert_eq!(
            fs::read(prepared.join("deps/first/first.h")).unwrap(),
            b"first"
        );
        assert_eq!(
            fs::read(prepared.join("deps/second/second.h")).unwrap(),
            b"second"
        );
        assert!(
            cache
                .prepare_incremental(&key, &source, &[("deps/first".into(), second.clone()),])
                .is_err()
        );
        assert!(cache.prepare_incremental(&key, &second, &[]).is_err());
        assert!(
            cache
                .prepare_incremental(&key, &source, &[("deps".into(), second.clone()),])
                .is_err()
        );
        assert!(
            cache
                .prepare_incremental(&key, &source, &[("deps/first/nested".into(), second),])
                .is_err()
        );
        assert_eq!(fs::read(source.join("root.txt")).unwrap(), b"upstream");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incremental_adoption_preserves_existing_files_and_rejects_conflicts() {
        let root = temp_dir("incremental-adopt");
        let source = root.join("source");
        let dependency = root.join("dependency");
        let destination = root.join("existing-tree");
        for path in [&source, &dependency, &destination] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(source.join("root.txt"), b"upstream").unwrap();
        fs::write(destination.join("root.txt"), b"patched existing tree").unwrap();
        fs::create_dir_all(destination.join("nonempty")).unwrap();
        fs::write(destination.join("nonempty/keep"), b"keep").unwrap();
        fs::write(destination.join("file"), b"keep").unwrap();
        let cache = Cache::at(root.join("cache"));
        for paths in [
            vec!["nonempty"],
            vec!["file"],
            vec!["../escape"],
            vec!["dep", "dep"],
            vec!["dep", "dep/nested"],
        ] {
            let mounts = paths
                .into_iter()
                .map(|path| (path.into(), dependency.clone()))
                .collect::<Vec<_>>();
            assert!(
                cache
                    .prepare_incremental_to(&source, &destination, &mounts)
                    .is_err()
            );
        }
        let prepared = cache
            .prepare_incremental_to(&source, &destination, &[("dep".into(), dependency.clone())])
            .unwrap();
        assert_eq!(prepared, std::path::absolute(&destination).unwrap());
        assert_eq!(
            fs::read(destination.join("root.txt")).unwrap(),
            b"patched existing tree"
        );
        assert_eq!(
            fs::read(destination.join("nonempty/keep")).unwrap(),
            b"keep"
        );
        directory_link(&dependency, &destination.join("linked-parent"));
        assert!(
            cache
                .prepare_incremental_to(
                    &source,
                    &destination,
                    &[("linked-parent/escape".into(), source.clone()),]
                )
                .is_err()
        );
        assert!(!dependency.join("escape").exists());
        // Even a previously recorded mount may not be silently substituted.
        let mount = destination.join("dep");
        remove_entry(&mount, &fs::symlink_metadata(&mount).unwrap()).unwrap();
        directory_link(&source, &mount);
        assert!(
            cache
                .prepare_incremental_to(&source, &destination, &[])
                .is_err()
        );
        assert_eq!(
            mount.canonicalize().unwrap(),
            source.canonicalize().unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incremental_sources_retry_incomplete_copy_and_repair_missing_mounts() {
        let root = temp_dir("incremental-retry");
        let source = root.join("source");
        let dependency = root.join("dependency");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&dependency).unwrap();
        fs::write(source.join("root.txt"), b"upstream").unwrap();
        let cache = Cache::at(root.join("cache"));
        let key = BuildKey::new("retry");
        let prepared = cache
            .prepare_incremental(&key, &source, &[("dep".into(), dependency)])
            .unwrap();
        fs::remove_file(lock_path(&prepared, "incremental").join("copied")).unwrap();
        fs::remove_file(prepared.join("root.txt")).unwrap();
        let mount = prepared.join("dep");
        remove_entry(&mount, &fs::symlink_metadata(&mount).unwrap()).unwrap();
        cache.prepare_incremental(&key, &source, &[]).unwrap();
        assert_eq!(fs::read(prepared.join("root.txt")).unwrap(), b"upstream");
        assert!(prepared.join("dep").is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incremental_sources_serialize_concurrent_additions() {
        let root = temp_dir("incremental-concurrent");
        let source = root.join("source");
        let dependency = root.join("dependency");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&dependency).unwrap();
        let cache = Cache::at(root.join("cache"));
        let key = BuildKey::new("concurrent");
        let prepared = cache.prepare_incremental(&key, &source, &[]).unwrap();
        std::thread::scope(|scope| {
            for name in ["first", "second"] {
                let cache = &cache;
                let key = &key;
                let source = &source;
                let dependency = &dependency;
                scope.spawn(move || {
                    cache
                        .prepare_incremental(key, source, &[(name.into(), dependency.clone())])
                        .unwrap();
                });
            }
        });
        cache.prepare_incremental(&key, &source, &[]).unwrap();
        assert!(prepared.join("first").is_dir());
        assert!(prepared.join("second").is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn source_state_path_encoding_roundtrips() {
        let path = PathBuf::from("a\tb\nc/with spaces");
        assert_eq!(decode_path(&encode_path(&path)).unwrap(), path);
        assert!(decode_path("xx").is_err());
        assert!(decode_path("0").is_err());
        assert!(decode_path("\u{e9}00").is_err());
    }

    #[test]
    #[cfg(windows)]
    fn mirror_refreshes_and_removes_readonly_files() {
        let root = temp_dir("mirror-readonly");
        let source = root.join("source");
        fs::create_dir_all(source.join("obsolete")).unwrap();
        fs::write(source.join("file"), b"before").unwrap();
        fs::write(source.join("obsolete/file"), b"remove").unwrap();
        for path in [source.join("file"), source.join("obsolete/file")] {
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_readonly(true);
            fs::set_permissions(path, permissions).unwrap();
        }
        let cache = Cache::at(root.join("cache"));
        let key = BuildKey::new("readonly");
        let dest = cache.mirror(&source, &key).unwrap();
        assert!(
            fs::metadata(dest.join("file"))
                .unwrap()
                .permissions()
                .readonly()
        );
        make_writable(
            &source.join("file"),
            &fs::metadata(source.join("file")).unwrap(),
        )
        .unwrap();
        fs::write(source.join("file"), b"after").unwrap();
        let mut permissions = fs::metadata(source.join("file")).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(source.join("file"), permissions).unwrap();
        remove_entry(
            &source.join("obsolete"),
            &fs::metadata(source.join("obsolete")).unwrap(),
        )
        .unwrap();
        cache.mirror(&source, &key).unwrap();
        assert_eq!(fs::read(dest.join("file")).unwrap(), b"after");
        assert!(!dest.join("obsolete").exists());
        remove_entry(&root, &fs::metadata(&root).unwrap()).unwrap();
    }

    #[test]
    #[cfg(windows)]
    fn incremental_mounts_reject_windows_aliases() {
        let root = temp_dir("incremental-aliases");
        let source = root.join("source");
        let dependency = root.join("dependency");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&dependency).unwrap();
        let cache = Cache::at(root.join("cache"));
        let key = BuildKey::new("aliases");
        for paths in [
            vec!["dep", "DEP"],
            vec!["DEP", "dep/nested"],
            vec!["dep."],
            vec!["dep "],
            vec!["dep:stream"],
        ] {
            let mounts = paths
                .into_iter()
                .map(|path| (path.into(), dependency.clone()))
                .collect::<Vec<_>>();
            assert!(cache.prepare_incremental(&key, &source, &mounts).is_err());
        }
        cache
            .prepare_incremental(&key, &source, &[("dep".into(), dependency.clone())])
            .unwrap();
        cache
            .prepare_incremental(&key, &source, &[("DEP".into(), dependency)])
            .unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prepared_sources_are_writable_without_mutating_cached_inputs() {
        let root = temp_dir("prepared");
        let source = root.join("upstream");
        let dependency = root.join("dependency");
        fs::create_dir_all(source.join("third_party")).unwrap();
        fs::create_dir_all(&dependency).unwrap();
        fs::write(source.join("CMakeLists.txt"), b"upstream").unwrap();
        fs::write(dependency.join("library.h"), b"dependency").unwrap();
        let mounts = [(PathBuf::from("third_party/library"), dependency.clone())];
        let cache = Cache::at(root.join("cache"));
        let key = BuildKey::new("prepared");
        let prepared = cache.prepare(&key, &source, &mounts).unwrap();
        assert_eq!(
            fs::read(prepared.join("third_party/library/library.h")).unwrap(),
            b"dependency"
        );
        fs::write(prepared.join("CMakeLists.txt"), b"patched").unwrap();
        assert_eq!(
            fs::read(source.join("CMakeLists.txt")).unwrap(),
            b"upstream"
        );
        assert_eq!(cache.prepare(&key, &source, &mounts).unwrap(), prepared);
        assert_eq!(
            fs::read(prepared.join("CMakeLists.txt")).unwrap(),
            b"patched"
        );
        let other = root.join("other");
        fs::create_dir_all(&other).unwrap();
        assert!(
            cache
                .prepare(
                    &key,
                    &source,
                    &[(PathBuf::from("third_party/library"), other)]
                )
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn prepared_sources_copy_relative_symlinks_and_retry() {
        let root = temp_dir("prepared-symlink");
        let source = root.join("upstream");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("original.txt"), b"linked source").unwrap();
        let link = source.join("nested/link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink("../original.txt", &link).unwrap();
        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_file("..\\original.txt", &link) {
            if error.raw_os_error() == Some(1314) {
                eprintln!("symlink fixture requires Windows Developer Mode: {error}");
                fs::remove_dir_all(root).unwrap();
                return;
            }
            panic!("create source symlink: {error}");
        }
        let cache = Cache::at(root.join("cache"));
        let key = BuildKey::new("symlink");
        let prepared = cache.prepare(&key, &source, &[]).unwrap();
        assert_eq!(
            fs::read(prepared.join("nested/link.txt")).unwrap(),
            b"linked source"
        );
        fs::remove_file(prepared.join(".woodshed-source-tree")).unwrap();
        #[cfg(windows)]
        {
            fs::remove_file(prepared.join("nested/link.txt")).unwrap();
            fs::write(prepared.join("nested/link.txt"), b"linked source").unwrap();
        }
        cache.prepare(&key, &source, &[]).unwrap();
        assert_eq!(
            fs::read(prepared.join("nested/link.txt")).unwrap(),
            b"linked source"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prepared_sources_reject_escaping_duplicate_and_overlapping_mounts() {
        let root = temp_dir("prepared-invalid");
        let source = root.join("upstream");
        fs::create_dir_all(&source).unwrap();
        let cache = Cache::at(root.join("cache"));
        let key = BuildKey::new("invalid");
        for paths in [
            vec![PathBuf::from("../outside")],
            vec![root.join("absolute")],
            vec![PathBuf::from("dep"), PathBuf::from("dep")],
            vec![PathBuf::from("dep"), PathBuf::from("dep/nested")],
        ] {
            let mounts = paths
                .into_iter()
                .map(|path| (path, source.clone()))
                .collect::<Vec<_>>();
            assert!(cache.prepare(&key, &source, &mounts).is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prepared_sources_preserve_conflicting_nonempty_directories() {
        let root = temp_dir("prepared-conflict");
        let source = root.join("upstream");
        let dependency = root.join("dependency");
        fs::create_dir_all(source.join("third_party/library")).unwrap();
        fs::create_dir_all(&dependency).unwrap();
        let cache = Cache::at(root.join("cache"));
        let key = BuildKey::new("conflict");
        let destination = cache
            .root()
            .join("prepared")
            .join(key.as_str())
            .join("third_party/library");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("keep.h"), b"keep").unwrap();
        assert!(
            cache
                .prepare(
                    &key,
                    &source,
                    &[(PathBuf::from("third_party/library"), dependency)]
                )
                .is_err()
        );
        assert_eq!(fs::read(destination.join("keep.h")).unwrap(), b"keep");
        fs::remove_dir_all(root).unwrap();
    }
}
