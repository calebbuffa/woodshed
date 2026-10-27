//! Immutable Git snapshots and upstream metadata, behind feature `git`.
//!
//! Uses the host Git executable. Fetches never initialize submodules, run hooks,
//! or execute repository-provided code. Builds must pass full commit IDs.
use crate::{Error, Result, dep::validate_revision};
use std::{
    path::{Component, Path},
    process::{Command, Output},
};

fn tool(detail: impl Into<String>) -> Error {
    Error::Tool {
        what: "Git source".into(),
        detail: detail.into(),
    }
}

fn run(command: &mut Command) -> Result<Output> {
    let output = command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .map_err(|error| Error::io("run Git (install Git and ensure it is on PATH)", error))?;
    if !output.status.success() {
        return Err(tool(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(output)
}

fn path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.contains('\\')
        || path.contains(':')
        || !Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(tool(format!("invalid repository-relative path {path:?}")));
    }
    Ok(())
}

/// A fetched commit and its temporary, bare object database.
/// Dropping this object removes only its own temporary directory.
pub struct Repository {
    directory: tempfile::TempDir,
    repository: String,
    revision: String,
}

impl Repository {
    /// Resolve a ref, fetching only that ref's shallow history.
    ///
    /// Maintainer use only for floating refs. Build callers must use [`Self::pinned`].
    pub fn fetch(repository: &str, reference: &str) -> Result<Self> {
        if std::env::var("WOODSHED_OFFLINE").is_ok_and(|value| value == "1") {
            return Err(Error::Offline {
                detail:
                    "cannot fetch Git metadata; use cached immutable sources or an offline resolver"
                        .into(),
            });
        }
        if repository.is_empty()
            || repository.starts_with('-')
            || reference.is_empty()
            || reference.starts_with('-')
            || reference.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(tool(
                "repository and ref must be nonempty, non-option Git arguments",
            ));
        }
        let directory = tempfile::tempdir()
            .map_err(|error| Error::io("create Git staging directory", error))?;
        run(Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(directory.path()))?;
        let mut snapshot = Self {
            directory,
            repository: repository.into(),
            revision: String::new(),
        };
        run(snapshot.command().args([
            "fetch",
            "--quiet",
            "--depth=1",
            "--no-tags",
            "--no-recurse-submodules",
            "--",
            repository,
            reference,
        ]))?;
        snapshot.revision = String::from_utf8(
            run(snapshot
                .command()
                .args(["rev-parse", "--verify", "FETCH_HEAD^{commit}"]))?
            .stdout,
        )
        .map_err(|error| tool(format!("commit is not UTF-8: {error}")))?
        .trim()
        .to_owned();
        validate_revision(&snapshot.revision)?;
        Ok(snapshot)
    }

    /// Fetch and verify an immutable commit. A moving tag or branch is rejected.
    pub fn pinned(repository: &str, revision: &str) -> Result<Self> {
        validate_revision(revision)?;
        let snapshot = Self::fetch(repository, revision)?;
        if !snapshot.revision.eq_ignore_ascii_case(revision) {
            return Err(tool(format!(
                "requested {revision}, fetched {}",
                snapshot.revision
            )));
        }
        Ok(snapshot)
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        command.arg("--git-dir").arg(self.directory.path()).args([
            "-c",
            "core.hooksPath=",
            "-c",
            "core.autocrlf=false",
            "-c",
            "protocol.ext.allow=never",
        ]);
        command
    }

    /// Resolved immutable commit ID.
    pub fn revision(&self) -> &str {
        &self.revision
    }

    /// Read a UTF-8 file directly from the commit, without executing checkout filters.
    pub fn file(&self, relative: &str) -> Result<String> {
        path(relative)?;
        let output = run(self
            .command()
            .args(["show", &format!("{}:{relative}", self.revision)]))?;
        String::from_utf8(output.stdout)
            .map_err(|error| tool(format!("{relative} is not UTF-8: {error}")))
    }

    /// Read a gitlink and its repository from the same immutable commit.
    pub fn gitlink(&self, relative: &str) -> Result<crate::GitSource> {
        path(relative)?;
        let output = run(self
            .command()
            .args(["ls-tree", "-z", &self.revision, "--", relative]))?;
        let text = String::from_utf8(output.stdout).map_err(|error| tool(error.to_string()))?;
        let entries = text
            .split('\0')
            .filter(|entry| !entry.is_empty())
            .collect::<Vec<_>>();
        if entries.len() != 1 {
            return Err(tool(format!("expected exactly one gitlink at {relative}")));
        }
        let (metadata, found_path) = entries[0]
            .split_once('\t')
            .ok_or_else(|| tool("malformed gitlink"))?;
        let fields = metadata.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 3
            || fields[0] != "160000"
            || fields[1] != "commit"
            || found_path != relative
        {
            return Err(tool(format!(
                "{relative} is not an exact submodule gitlink"
            )));
        }
        validate_revision(fields[2])?;
        let output = run(self.command().args([
            "config",
            "--null",
            "--blob",
            &format!("{}:.gitmodules", self.revision),
            "--get-regexp",
            r"^submodule\..*\.path$",
        ]))?;
        let config = String::from_utf8(output.stdout).map_err(|error| tool(error.to_string()))?;
        let keys = config
            .split('\0')
            .filter_map(|entry| entry.split_once('\n'))
            .filter(|(_, value)| *value == relative)
            .map(|(key, _)| key)
            .collect::<Vec<_>>();
        if keys.len() != 1 {
            return Err(tool(format!(
                "expected exactly one .gitmodules path for {relative}"
            )));
        }
        let key = format!(
            "{}.url",
            keys[0].strip_suffix(".path").expect("submodule path key")
        );
        let output = run(self.command().args([
            "config",
            "--null",
            "--blob",
            &format!("{}:.gitmodules", self.revision),
            "--get-all",
            &key,
        ]))?;
        let urls = String::from_utf8(output.stdout).map_err(|error| tool(error.to_string()))?;
        let urls = urls
            .split('\0')
            .filter(|url| !url.is_empty())
            .collect::<Vec<_>>();
        if urls.len() != 1 {
            return Err(tool(format!(
                "expected exactly one repository URL for {relative}"
            )));
        }
        let repository = resolve_submodule_url(&self.repository, urls[0])?;
        Ok(crate::GitSource {
            repository,
            revision: fields[2].into(),
        })
    }

    /// Export tracked files, excluding Git metadata and uninitialized submodules.
    pub fn export(&self, destination: &Path) -> Result<()> {
        // git archive does not run smudge filters or copy checkout hooks/configuration.
        let archive = tempfile::Builder::new()
            .suffix(".tar.gz")
            .tempfile()
            .map_err(|error| Error::io("create Git export", error))?;
        run(self
            .command()
            .arg("archive")
            .arg("--format=tar.gz")
            .arg("--output")
            .arg(archive.path())
            .arg(&self.revision))?;
        crate::archive::extract(archive.path(), destination)
    }
}

fn resolve_submodule_url(parent: &str, child: &str) -> Result<String> {
    if !child.starts_with("./") && !child.starts_with("../") {
        if child.starts_with('-') || child.is_empty() {
            return Err(tool("invalid submodule repository"));
        }
        return Ok(child.into());
    }
    if Path::new(parent).is_absolute() {
        let mut result = std::path::PathBuf::from(parent);
        for component in Path::new(child).components() {
            match component {
                Component::ParentDir => {
                    if !result.pop() {
                        return Err(tool("relative submodule URL escapes repository location"));
                    }
                }
                Component::CurDir => {}
                Component::Normal(part) => result.push(part),
                _ => return Err(tool("invalid relative submodule URL")),
            }
        }
        return result
            .into_os_string()
            .into_string()
            .map_err(|_| tool("submodule repository is not UTF-8"));
    }
    let mut base = parent.trim_end_matches('/').to_owned();
    let mut child = child;
    while let Some(rest) = child.strip_prefix("../") {
        let (head, _) = base
            .rsplit_once('/')
            .ok_or_else(|| tool("cannot resolve relative submodule URL"))?;
        base = head.into();
        child = rest;
    }
    let child = child.strip_prefix("./").unwrap_or(child);
    Ok(format!("{base}/{child}"))
}
