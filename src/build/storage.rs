use super::*;
use crate::cache::{Lock, lock_path, remove_entry, tree_fingerprint};
use crate::providers::InstallationOwnership;
use std::{collections::BTreeSet, fs, io::Write};

const OWNER: &str = ".woodshed-owned";

#[derive(Serialize, Deserialize)]
pub(super) struct ArtifactReceipt {
    pub input: String,
    pub fingerprint: String,
    pub metadata: String,
    pub input_fields: BTreeMap<String, String>,
    pub installation: Installation,
    pub files: BTreeMap<PathBuf, String>,
}

pub(super) fn metadata_identity(
    installation: &Installation,
    files: &BTreeMap<PathBuf, String>,
    inputs: &BTreeMap<String, String>,
) -> Result<String> {
    Ok(BuildKey::explicit("installation-metadata")
        .mix(
            serde_json::to_vec(&(installation, files, inputs))
                .map_err(|error| invalid(error.to_string()))?,
        )
        .fingerprint())
}

fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).map_err(|error| Error::io("resolve native cache path", error))
}

pub(super) fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|error| invalid(error.to_string()))?;
    let mut staged = tempfile::NamedTempFile::new_in(path.parent().expect("cache parent"))
        .map_err(|error| Error::io("stage native cache state", error))?;
    staged
        .write_all(&bytes)
        .and_then(|()| staged.as_file().sync_all())
        .map_err(|error| Error::io("write native cache state", error))?;
    staged
        .persist(path)
        .map_err(|error| Error::io("commit native cache state", error.error))?;
    Ok(())
}

fn remove(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => remove_entry(path, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::io("inspect native cache entry", error)),
    }
}

pub(super) fn owned(path: &Path, identity: &str) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || crate::cache::is_link(&metadata) {
                return Err(invalid(format!(
                    "native cache owner is not a directory: {}",
                    path.display()
                )));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(Error::io("inspect native cache owner", error)),
    }
    match fs::read(path.join(OWNER)) {
        Ok(contents) if contents == identity.as_bytes() => Ok(true),
        Ok(_) => Err(invalid(format!(
            "native cache ownership differs: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if fs::read_dir(path)
                .map_err(|error| Error::io("inspect unowned cache directory", error))?
                .next()
                .is_none()
            {
                return Ok(false);
            }
            Err(invalid(format!(
                "refusing nonempty unowned native cache directory: {}",
                path.display()
            )))
        }
        Err(error) => Err(Error::io("read native cache ownership", error)),
    }
}

fn claim(path: &Path, identity: &str) -> Result<()> {
    if owned(path, identity)? {
        return Ok(());
    }
    fs::create_dir_all(path).map_err(|error| Error::io("create owned native cache", error))?;
    fs::write(path.join(OWNER), identity)
        .map_err(|error| Error::io("record native cache ownership", error))
}

/// Lock covers validation, staging, publication and metadata receipt commit.
pub(super) struct Artifact {
    pub directory: PathBuf,
    pub prefix: PathBuf,
    pub stage: PathBuf,
    pub metadata: PathBuf,
    identity: String,
    candidate: Option<String>,
    committed: bool,
    pub repaired: bool,
    _lock: Lock,
}

impl Artifact {
    pub fn open(cache: &Cache, key: &BuildKey) -> Result<Self> {
        let directory = absolute(&cache.artifact_dir(key))?;
        let identity = key.fingerprint();
        let stage = directory.with_file_name(format!("{identity}.partial"));
        let metadata = directory.with_file_name(format!("{identity}.metadata"));
        let lock = Lock::acquire(&lock_path(&directory, "artifact"))?;
        if owned(&stage, &identity)? {
            remove(&stage)?;
        }
        let prefix = directory.join("prefix");
        Ok(Self {
            directory,
            prefix,
            stage,
            metadata,
            identity,
            candidate: None,
            committed: false,
            repaired: false,
            _lock: lock,
        })
    }

    pub fn cached(&mut self) -> Result<Option<Installation>> {
        if !owned(&self.directory, &self.identity)? {
            if self.directory.exists() {
                fs::remove_dir(&self.directory)
                    .map_err(|error| Error::io("remove empty artifact slot", error))?;
            }
            return Ok(None);
        }
        let result = (|| {
            let bytes = fs::read(self.directory.join("woodshed-installation.json"))
                .map_err(|error| Error::io("read artifact receipt", error))?;
            let receipt: ArtifactReceipt = serde_json::from_slice(&bytes)
                .map_err(|error| invalid(format!("invalid artifact receipt: {error}")))?;
            if receipt.input != self.identity
                || receipt.installation.ownership != InstallationOwnership::Artifact
                || metadata_identity(&receipt.installation, &receipt.files, &receipt.input_fields)?
                    != receipt.metadata
                || receipt.files.is_empty()
                || tree_fingerprint(&self.prefix)? != receipt.fingerprint
            {
                return Err(invalid(
                    "source artifact content or identity differs from its receipt",
                ));
            }
            Ok(receipt.installation)
        })();
        match result {
            Ok(installation) => {
                self.committed = true;
                Ok(Some(installation))
            }
            Err(error) => {
                if matches!(&error, Error::Io { source, .. } if source.kind() != std::io::ErrorKind::NotFound)
                {
                    return Err(error);
                }
                eprintln!(
                    "woodshed: repairing incomplete/corrupt source artifact {}: {error}",
                    self.identity
                );
                remove(&self.directory)?;
                self.repaired = true;
                Ok(None)
            }
        }
    }

    pub fn begin(&self) -> Result<PathBuf> {
        claim(&self.stage, &self.identity)?;
        let install = self.stage.join("install");
        fs::create_dir(&install)
            .map_err(|error| Error::io("create private install stage", error))?;
        claim(&self.metadata, &self.identity)?;
        Ok(install)
    }

    pub fn publish(&mut self, cache: &Cache, install: &Path) -> Result<()> {
        crate::cache::snapshot_fingerprint(install)?;
        if inventory(install)?.is_empty() {
            return Err(invalid(
                "CMake installed no files; cannot publish a source artifact",
            ));
        }
        let snapshot = self.stage.join("snapshot");
        claim(&snapshot, &self.identity)?;
        cache.mirror_to(install, &snapshot.join("prefix"))?;
        let fingerprint = tree_fingerprint(&snapshot.join("prefix"))?;
        if self.directory.exists() {
            return Err(invalid(format!(
                "refusing to overwrite an existing immutable artifact: {}",
                self.directory.display()
            )));
        }
        fs::rename(&snapshot, &self.directory)
            .map_err(|error| Error::io("publish immutable installation snapshot", error))?;
        self.candidate = Some(fingerprint);
        Ok(())
    }

    pub fn commit(
        &mut self,
        mut installation: Installation,
        forbidden: &[PathBuf],
        input_fields: BTreeMap<String, String>,
    ) -> Result<Installation> {
        validate_metadata(&installation, forbidden)?;
        validate_headers(&self.prefix, forbidden)?;
        let fingerprint = tree_fingerprint(&self.prefix)?;
        if self.candidate.as_ref() != Some(&fingerprint) {
            return Err(invalid(
                "metadata evaluation modified an immutable installation snapshot",
            ));
        }
        installation.ownership = InstallationOwnership::Artifact;
        let files = inventory(&self.prefix)?;
        let metadata = metadata_identity(&installation, &files, &input_fields)?;
        atomic_json(
            &self.directory.join("woodshed-installation.json"),
            &ArtifactReceipt {
                input: self.identity.clone(),
                fingerprint,
                metadata,
                input_fields,
                files,
                installation: installation.clone(),
            },
        )?;
        self.committed = true;
        Ok(installation)
    }
}

impl Drop for Artifact {
    fn drop(&mut self) {
        if self.candidate.is_some()
            && !self.committed
            && let Err(error) = remove(&self.directory)
        {
            eprintln!("woodshed: cannot clean uncommitted source artifact: {error}");
        }
        let cleanup = (|| {
            if owned(&self.stage, &self.identity)? {
                remove(&self.stage)?;
            }
            Ok::<_, Error>(())
        })();
        if let Err(error) = cleanup {
            eprintln!("woodshed: cannot clean owned install stage: {error}");
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct WorkspaceState {
    identity: String,
    managed: BTreeSet<String>,
    #[serde(default)]
    tools: Vec<PathBuf>,
    #[serde(default)]
    hard: String,
}

pub(super) struct Workspace {
    pub root: PathBuf,
    pub build: PathBuf,
    pub source: PathBuf,
    state: WorkspaceState,
    _lock: Lock,
}

impl Workspace {
    pub fn open(cache: &Cache, key: &BuildKey) -> Result<Self> {
        let root = absolute(&cache.workspace_dir(key))?;
        let identity = key.fingerprint();
        let lock = Lock::acquire(&lock_path(&root, "workspace"))?;
        let was_owned = owned(&root, &identity)?;
        claim(&root, &identity)?;
        let build = root.join("build");
        let file = root.join("workspace.json");
        let prior = match fs::read(&file) {
            Ok(bytes) => serde_json::from_slice::<WorkspaceState>(&bytes)
                .map_err(|error| invalid(format!("invalid managed CMake state: {error}"))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(invalid("managed CMake state is missing"))
            }
            Err(error) => return Err(Error::io("read managed workspace state", error)),
        };
        let state = match prior {
            Ok(state) if state.identity == identity => {
                eprintln!("woodshed: reconfiguring compatible workspace {identity}");
                state
            }
            prior => {
                if was_owned && build.exists() {
                    eprintln!(
                        "woodshed: resetting owned workspace {identity}: {}",
                        prior.err().map_or_else(
                            || "hard identity mismatch".into(),
                            |error| error.to_string()
                        )
                    );
                    remove(&build)?;
                } else {
                    eprintln!(
                        "woodshed: creating isolated workspace {identity}; hard inputs define its boundary"
                    );
                }
                WorkspaceState {
                    identity,
                    ..WorkspaceState::default()
                }
            }
        };
        Ok(Self {
            source: root.join("source"),
            root,
            build,
            state,
            _lock: lock,
        })
    }

    pub fn configure(
        &mut self,
        builder: CmakeBuild,
        current: BTreeSet<String>,
    ) -> Result<CmakeBuild> {
        let mut builder = builder;
        for removed in self.state.managed.difference(&current) {
            builder = builder.arg(format!("-U{removed}"));
        }
        // Keep the union until success, so interruption never loses a managed key.
        self.state.managed.extend(current);
        atomic_json(&self.root.join("workspace.json"), &self.state)?;
        Ok(builder)
    }

    pub fn configured(&mut self, current: BTreeSet<String>) -> Result<()> {
        self.state.managed = current;
        atomic_json(&self.root.join("workspace.json"), &self.state)
    }

    pub fn tools(&self) -> &[PathBuf] {
        &self.state.tools
    }

    pub fn hard_boundary(&mut self, hard: &str) -> Result<()> {
        if !self.state.hard.is_empty() && self.state.hard != hard {
            eprintln!(
                "woodshed: compiler/tool hard inputs changed; resetting owned build {}",
                self.build.display()
            );
            remove(&self.build)?;
            self.state.managed.clear();
        }
        self.state.hard = hard.into();
        atomic_json(&self.root.join("workspace.json"), &self.state)
    }

    pub fn tool_selection(&mut self, tools: Vec<PathBuf>) -> Result<()> {
        self.state.tools = tools;
        atomic_json(&self.root.join("workspace.json"), &self.state)
    }
}

fn normalized(value: &str) -> String {
    if cfg!(windows) {
        let mut value = value.replace('\\', "/").to_lowercase();
        while value.contains("//") {
            value = value.replace("//", "/");
        }
        value
    } else {
        value.to_owned()
    }
}

fn refers_to_mutable(value: &str, forbidden: &[PathBuf]) -> bool {
    let value = normalized(value);
    forbidden
        .iter()
        .any(|root| value.contains(&normalized(&root.to_string_lossy())))
}

fn validate_metadata(installation: &Installation, forbidden: &[PathBuf]) -> Result<()> {
    for path in installation
        .prefixes
        .iter()
        .chain(&installation.include_dirs)
        .chain(installation.executables.values())
        .chain(installation.cmake_packages.values())
        .chain(&installation.runtime_directories)
    {
        let canonical = path.canonicalize().map_err(|error| {
            Error::io(
                format!("resolve installed metadata path {}", path.display()),
                error,
            )
        })?;
        if refers_to_mutable(&path.to_string_lossy(), forbidden)
            || refers_to_mutable(&canonical.to_string_lossy(), forbidden)
        {
            return Err(invalid(format!(
                "installed metadata references mutable inputs: {}; export installed headers/libraries instead of build-tree paths",
                path.display()
            )));
        }
    }
    for value in installation
        .link_arguments
        .iter()
        .chain(&installation.compile_flags)
        .chain(&installation.definitions)
    {
        if refers_to_mutable(value, forbidden) {
            return Err(invalid(format!(
                "installed link/compile metadata references mutable inputs: {value}"
            )));
        }
        let path = Path::new(value);
        if path.is_absolute() {
            let path = path
                .canonicalize()
                .map_err(|error| Error::io("resolve installed native link input", error))?;
            if refers_to_mutable(&path.to_string_lossy(), forbidden) {
                return Err(invalid(
                    "installed native link input resolves into a mutable workspace",
                ));
            }
        }
    }
    Ok(())
}

fn validate_headers(root: &Path, forbidden: &[PathBuf]) -> Result<()> {
    validate_header_directory(root, root, forbidden)
}

fn validate_header_directory(root: &Path, prefix: &Path, forbidden: &[PathBuf]) -> Result<()> {
    for entry in
        fs::read_dir(root).map_err(|error| Error::io("inspect installed metadata", error))?
    {
        let path = entry
            .map_err(|error| Error::io("inspect installed metadata entry", error))?
            .path();
        if path.is_dir() {
            validate_header_directory(&path, prefix, forbidden)?;
        } else if path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| {
                matches!(
                    extension,
                    "h" | "hpp" | "hh" | "hxx" | "inc" | "inl" | "cmake" | "pc" | "la"
                )
            })
        {
            let bytes = fs::read(&path)
                .map_err(|error| Error::io("read installed metadata/header", error))?;
            let text = String::from_utf8_lossy(&bytes);
            if refers_to_mutable(&text, forbidden)
                || quoted_mutable_reference(&text, &path, prefix, forbidden)
            {
                return Err(invalid(format!(
                    "installed metadata/header contains a mutable build/stage/source path: {}; upstream must provide relocatable install exports",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn quoted_mutable_reference(text: &str, file: &Path, prefix: &Path, forbidden: &[PathBuf]) -> bool {
    use std::path::Component;
    for literal in text.split('"').skip(1).step_by(2) {
        let value = literal
            .replace("\\\\", "\\")
            .replace(
                "${CMAKE_CURRENT_LIST_DIR}",
                &file.parent().expect("installed parent").to_string_lossy(),
            )
            .replace("${_IMPORT_PREFIX}", &prefix.to_string_lossy())
            .replace("${PACKAGE_PREFIX_DIR}", &prefix.to_string_lossy());
        let value = Path::new(&value);
        if !value.is_absolute()
            && !value
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            continue;
        }
        let joined = file.parent().expect("installed parent").join(value);
        let mut resolved = PathBuf::new();
        for component in joined.components() {
            match component {
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::CurDir => {}
                other => resolved.push(other.as_os_str()),
            }
        }
        if refers_to_mutable(&resolved.to_string_lossy(), forbidden) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publish(cache: &Cache, key: &BuildKey) -> PathBuf {
        let mut artifact = Artifact::open(cache, key).unwrap();
        assert!(artifact.cached().unwrap().is_none());
        let install = artifact.begin().unwrap();
        fs::write(install.join("sample.h"), "int sample(void);\n").unwrap();
        artifact.publish(cache, &install).unwrap();
        let prefix = artifact.prefix.clone();
        artifact
            .commit(
                Installation::tools(&prefix, PackageLinkage::Static),
                &[],
                BTreeMap::new(),
            )
            .unwrap();
        prefix
    }

    #[test]
    fn artifacts_verify_bytes_metadata_and_repair_owned_incomplete_publications() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path().join("cache"));
        let key = BuildKey::explicit("sample");
        let prefix = publish(&cache, &key);
        let mut artifact = Artifact::open(&cache, &key).unwrap();
        assert_eq!(
            artifact.cached().unwrap().unwrap().ownership,
            InstallationOwnership::Artifact
        );
        drop(artifact);
        let modified = fs::metadata(prefix.join("sample.h"))
            .unwrap()
            .modified()
            .unwrap();
        fs::write(prefix.join("sample.h"), "int BROKEN(void);\n").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(prefix.join("sample.h"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let mut artifact = Artifact::open(&cache, &key).unwrap();
        assert!(artifact.cached().unwrap().is_none());
        assert!(!prefix.exists());
        drop(artifact);
        publish(&cache, &key);
        let receipt = cache.artifact_dir(&key).join("woodshed-installation.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
        value["installation"]["definitions"] = serde_json::json!(["BROKEN"]);
        fs::write(&receipt, serde_json::to_vec(&value).unwrap()).unwrap();
        let mut artifact = Artifact::open(&cache, &key).unwrap();
        assert!(
            artifact.cached().unwrap().is_none(),
            "metadata corruption must not look like a cache hit"
        );
        drop(artifact);
        let mut interrupted = Artifact::open(&cache, &key).unwrap();
        let install = interrupted.begin().unwrap();
        fs::write(install.join("sample.h"), "unfinished").unwrap();
        interrupted.publish(&cache, &install).unwrap();
        interrupted.candidate = None; // Simulate process exit before receipt commit, without leaking a lock.
        drop(interrupted);
        let mut recovered = Artifact::open(&cache, &key).unwrap();
        assert!(recovered.cached().unwrap().is_none());
        assert!(!recovered.directory.exists());
    }

    #[test]
    fn refuses_unowned_trees_and_recovers_only_owned_staging() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path().join("cache"));
        let key = BuildKey::explicit("unowned");
        let path = cache.artifact_dir(&key);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("user"), "keep").unwrap();
        assert!(Artifact::open(&cache, &key).unwrap().cached().is_err());
        assert_eq!(fs::read(path.join("user")).unwrap(), b"keep");
        let workspace = cache.workspace_dir(&key);
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("user"), "keep").unwrap();
        assert!(Workspace::open(&cache, &key).is_err());
        assert!(workspace.join("user").exists());
        let key = BuildKey::explicit("staging");
        let stage = cache
            .artifact_dir(&key)
            .with_file_name(format!("{}.partial", key.fingerprint()));
        claim(&stage, &key.fingerprint()).unwrap();
        fs::write(stage.join("interrupted"), "partial").unwrap();
        let artifact = Artifact::open(&cache, &key).unwrap();
        assert!(!stage.exists());
        drop(artifact);
        fs::create_dir(&stage).unwrap();
        fs::write(stage.join("user"), "keep").unwrap();
        assert!(Artifact::open(&cache, &key).is_err());
        assert!(stage.join("user").exists());
    }

    #[test]
    fn workspace_hard_boundary_resets_objects_not_source_and_keeps_managed_journal() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path().join("cache"));
        let mut workspace = Workspace::open(&cache, &BuildKey::explicit("sample")).unwrap();
        workspace.hard_boundary("compiler-a").unwrap();
        fs::create_dir_all(&workspace.build).unwrap();
        fs::create_dir_all(&workspace.source).unwrap();
        fs::write(workspace.build.join("object"), "compiled").unwrap();
        fs::write(workspace.source.join("input"), "source").unwrap();
        workspace.hard_boundary("compiler-a").unwrap();
        assert!(workspace.build.join("object").exists());
        workspace.state.managed.insert("OLD_OPTION".into());
        let builder = CmakeBuild::new(
            &workspace.source,
            &crate::BuildDirs {
                build: workspace.build.clone(),
                prefix: workspace.root.join("prefix"),
            },
        );
        workspace
            .configure(builder, BTreeSet::from(["NEW_OPTION".into()]))
            .unwrap();
        let saved: WorkspaceState =
            serde_json::from_slice(&fs::read(workspace.root.join("workspace.json")).unwrap())
                .unwrap();
        assert_eq!(
            saved.managed,
            BTreeSet::from(["OLD_OPTION".into(), "NEW_OPTION".into()])
        );
        workspace.hard_boundary("compiler-b").unwrap();
        assert!(!workspace.build.exists());
        assert_eq!(fs::read(workspace.source.join("input")).unwrap(), b"source");
    }

    #[test]
    fn publication_lock_serializes_validation_and_reuse() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path().join("cache"));
        let key = BuildKey::explicit("sample");
        publish(&cache, &key);
        let guard = Artifact::open(&cache, &key).unwrap();
        let (started, ready) = std::sync::mpsc::channel();
        let (finished, result) = std::sync::mpsc::channel();
        let cache_root = cache.root().to_path_buf();
        let thread = std::thread::spawn(move || {
            started.send(()).unwrap();
            let mut artifact = Artifact::open(&Cache::at(cache_root), &key).unwrap();
            finished.send(artifact.cached().unwrap().is_some()).unwrap();
        });
        ready.recv().unwrap();
        assert!(matches!(
            result.recv_timeout(std::time::Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(guard);
        assert!(
            result
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
        );
        thread.join().unwrap();
    }

    #[test]
    fn rejects_installed_build_tree_headers_and_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let forbidden = directory.path().join("workspace");
        fs::create_dir(&forbidden).unwrap();
        let include = directory.path().join("include");
        fs::create_dir(&include).unwrap();
        fs::write(
            include.join("generated.h"),
            format!("#include \"{}/private.h\"", forbidden.display()),
        )
        .unwrap();
        assert!(validate_headers(&include, std::slice::from_ref(&forbidden)).is_err());
        let mut installation = Installation::tools(&include, PackageLinkage::Static);
        installation.include_dirs.push(forbidden.clone());
        assert!(validate_metadata(&installation, &[forbidden]).is_err());
        let forbidden = directory.path().join("workspace");
        fs::write(
            include.join("generated.h"),
            "#include \"../workspace/generated.h\"\n",
        )
        .unwrap();
        assert!(
            validate_headers(&include, &[forbidden]).is_err(),
            "relative escapes must not hide a mutable include"
        );
    }

    #[cfg(unix)]
    #[test]
    fn materializes_safe_install_links_and_rejects_escaping_links_and_cycles() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path().join("cache"));
        let key = BuildKey::explicit("links");
        let mut artifact = Artifact::open(&cache, &key).unwrap();
        let install = artifact.begin().unwrap();
        fs::write(install.join("file"), b"content").unwrap();
        symlink(install.join("file"), install.join("alias")).unwrap();
        artifact.publish(&cache, &install).unwrap();
        assert_eq!(fs::read(artifact.prefix.join("alias")).unwrap(), b"content");
        assert!(
            !fs::symlink_metadata(artifact.prefix.join("alias"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_file(install.join("alias")).unwrap();
        symlink(directory.path(), install.join("escape")).unwrap();
        assert!(crate::cache::snapshot_fingerprint(&install).is_err());
        fs::remove_file(install.join("escape")).unwrap();
        symlink(".", install.join("cycle")).unwrap();
        assert!(crate::cache::snapshot_fingerprint(&install).is_err());
    }
}
