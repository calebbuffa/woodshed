use super::*;
use crate::cache::{Lock, lock_path, remove_entry, tree_fingerprint};
use std::{collections::BTreeSet, fs};
use storage::{ArtifactReceipt, atomic_json, metadata_identity, owned};

/// Content verification and disk usage for one managed source installation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtifactSummary {
    /// Full exact artifact identity.
    pub id: String,
    /// Installed regular-file bytes, excluding metadata receipts.
    pub bytes: u64,
    /// Whether content, metadata and identity all match the receipt.
    pub verified: bool,
    /// Durable build references retaining this installation.
    pub references: Vec<String>,
}

/// Result of pruning explicitly enumerated, unreferenced artifacts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PruneReport {
    /// IDs removed, or that would be removed during a dry run.
    pub removed: Vec<String>,
    /// IDs retained because an existing build output or explicit pin references them.
    pub retained: Vec<String>,
    /// Total installed regular-file bytes removed or reclaimable.
    pub bytes: u64,
}

#[derive(Serialize, Deserialize)]
struct Reference {
    id: String,
    output: Option<PathBuf>,
}

pub(super) fn checked_id(id: &str) -> Result<()> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(invalid(
            "artifact identity must be a full lowercase SHA-256 fingerprint",
        ));
    }
    Ok(())
}

fn reference_path(cache: &Cache, name: &str) -> PathBuf {
    cache.root().join("references").join(format!(
        "{}.json",
        BuildKey::explicit("artifact-reference")
            .mix(name)
            .fingerprint()
    ))
}

pub(super) fn pin_build(cache: &Cache, id: &str) -> Result<()> {
    let output = std::env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .map(|path| {
            std::path::absolute(path)
                .map_err(|error| Error::io("resolve artifact build reference", error))
        })
        .transpose()?;
    pin_build_output(cache, id, output)
}

fn pin_build_output(cache: &Cache, id: &str, output: Option<PathBuf>) -> Result<()> {
    // Each output may use several native artifacts; retain all until Cargo removes it.
    let name = output
        .as_ref()
        .map(|path| format!("{}:{id}", path.display()))
        .unwrap_or_else(|| format!("pin:standalone:{id}"));
    let path = reference_path(cache, &name);
    let _lock = Lock::acquire(&lock_path(&path, "reference"))?;
    atomic_json(
        &path,
        &Reference {
            id: id.into(),
            output,
        },
    )
}

fn references(cache: &Cache) -> Result<std::collections::BTreeMap<String, Vec<String>>> {
    let mut result = BTreeMap::<String, Vec<String>>::new();
    let root = cache.root().join("references");
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(result),
        Err(error) => return Err(Error::io("read artifact references", error)),
    };
    for entry in entries {
        let path = entry
            .map_err(|error| Error::io("read artifact reference entry", error))?
            .path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let bytes = fs::read(&path).map_err(|error| Error::io("read artifact reference", error))?;
        let reference: Reference = serde_json::from_slice(&bytes).map_err(|error| {
            invalid(format!(
                "invalid artifact reference {}: {error}",
                path.display()
            ))
        })?;
        checked_id(&reference.id)?;
        let live = match &reference.output {
            None => true,
            Some(output) => match fs::metadata(output) {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(Error::io("inspect artifact reference output", error)),
            },
        };
        if live {
            result
                .entry(reference.id)
                .or_default()
                .push(path.file_stem().unwrap().to_string_lossy().into_owned());
        }
    }
    Ok(result)
}

pub(super) fn inspect(cache: &Cache, id: &str, references: Vec<String>) -> Result<ArtifactSummary> {
    checked_id(id)?;
    let root = cache.root().join("artifacts").join(id);
    if !owned(&root, id)? {
        return Err(invalid(format!("{id}: managed artifact does not exist")));
    }
    let receipt_path = root.join("woodshed-installation.json");
    let receipt = match fs::read(receipt_path) {
        Ok(bytes) => serde_json::from_slice::<ArtifactReceipt>(&bytes).ok(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(Error::io("inspect artifact receipt", error)),
    };
    let prefix = root.join("prefix");
    let bytes = if prefix.is_dir() { size(&prefix)? } else { 0 };
    let verified = if let Some(receipt) = receipt {
        receipt.input == id
            && receipt.installation.ownership == crate::providers::InstallationOwnership::Artifact
            && !receipt.files.is_empty()
            && metadata_identity(&receipt.installation, &receipt.files, &receipt.input_fields)?
                == receipt.metadata
            && prefix.is_dir()
            && tree_fingerprint(&prefix)? == receipt.fingerprint
    } else {
        false
    };
    Ok(ArtifactSummary {
        id: id.into(),
        bytes,
        verified,
        references,
    })
}

fn size(root: &Path) -> Result<u64> {
    let mut size = 0u64;
    for entry in
        fs::read_dir(root).map_err(|error| Error::io("inspect artifact disk usage", error))?
    {
        let entry = entry.map_err(|error| Error::io("inspect artifact file", error))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| Error::io("inspect artifact file type", error))?;
        if crate::cache::is_link(&metadata) {
            return Err(invalid(
                "managed artifact contains a link; refusing cache traversal",
            ));
        }
        size = size
            .checked_add(if metadata.is_dir() {
                self::size(&entry.path())?
            } else if metadata.is_file() {
                metadata.len()
            } else {
                return Err(invalid("unsupported artifact file type"));
            })
            .ok_or_else(|| invalid("artifact size overflow"))?;
    }
    Ok(size)
}

impl Cache {
    /// Verify one artifact without mutation, network access or corruption repair.
    pub fn verify_artifact(&self, id: &str) -> Result<ArtifactSummary> {
        checked_id(id)?;
        let _activity = Lock::acquire_shared(&self.root().join("activity.lock"))?;
        let root = self.root().join("artifacts").join(id);
        let _artifact = Lock::acquire(&lock_path(&root, "artifact"))?;
        inspect(self, id, references(self)?.remove(id).unwrap_or_default())
    }
}

impl Cache {
    /// Inspect and fully verify owned source artifacts, without repairing or deleting them.
    pub fn inspect_artifacts(&self) -> Result<Vec<ArtifactSummary>> {
        let _activity = Lock::acquire_shared(&self.root().join("activity.lock"))?;
        let references = references(self)?;
        let mut result = Vec::new();
        let entries = match fs::read_dir(self.root().join("artifacts")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(result),
            Err(error) => return Err(Error::io("list source artifacts", error)),
        };
        for entry in entries {
            let entry = entry.map_err(|error| Error::io("list source artifact entry", error))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if checked_id(&name).is_err() {
                continue;
            }
            let _lock = Lock::acquire(&lock_path(&entry.path(), "artifact"))?;
            result.push(inspect(
                self,
                &name,
                references.get(&name).cloned().unwrap_or_default(),
            )?);
        }
        result.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(result)
    }

    /// Protect an artifact for a custom integration until explicitly unpinned.
    pub fn pin_artifact(&self, id: &str, name: &str) -> Result<()> {
        checked_id(id)?;
        let _activity = Lock::acquire_shared(&self.root().join("activity.lock"))?;
        let root = self.root().join("artifacts").join(id);
        let _artifact = Lock::acquire(&lock_path(&root, "artifact"))?;
        if !inspect(self, id, Vec::new())?.verified {
            return Err(invalid("cannot pin an incomplete or corrupt artifact"));
        }
        let path = reference_path(self, &format!("pin:{name}"));
        let _lock = Lock::acquire(&lock_path(&path, "reference"))?;
        atomic_json(
            &path,
            &Reference {
                id: id.into(),
                output: None,
            },
        )
    }

    /// Release only the named explicit pin; automatic Cargo references are unaffected.
    pub fn unpin_artifact(&self, name: &str) -> Result<()> {
        let _activity = Lock::acquire_shared(&self.root().join("activity.lock"))?;
        let path = reference_path(self, &format!("pin:{name}"));
        let _lock = Lock::acquire(&lock_path(&path, "reference"))?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::io("release artifact pin", error)),
        }
    }

    /// Remove only listed, owned, unreferenced source artifacts.
    ///
    /// Serialized against high-level builds. Cargo output references retain every
    /// installation until that output is removed (for example by `cargo clean`).
    /// Custom integrations must hold explicit pins while using installation paths.
    pub fn prune_artifacts(&self, ids: &[String], dry_run: bool) -> Result<PruneReport> {
        for id in ids {
            checked_id(id)?;
        }
        let _activity = Lock::acquire(&self.root().join("activity.lock"))?;
        let references = references(self)?;
        let mut report = PruneReport {
            removed: Vec::new(),
            retained: Vec::new(),
            bytes: 0,
        };
        for id in ids.iter().collect::<BTreeSet<_>>() {
            let root = self.root().join("artifacts").join(id);
            let _artifact = Lock::acquire(&lock_path(&root, "artifact"))?;
            let summary = inspect(self, id, references.get(id).cloned().unwrap_or_default())?;
            if !summary.references.is_empty() {
                report.retained.push(id.clone());
                continue;
            }
            if !dry_run {
                let metadata = fs::symlink_metadata(&root)
                    .map_err(|error| Error::io("inspect artifact before pruning", error))?;
                remove_entry(&root, &metadata)?;
            }
            report.bytes = report
                .bytes
                .checked_add(summary.bytes)
                .ok_or_else(|| invalid("pruned size overflow"))?;
            report.removed.push(id.clone());
        }
        Ok(report)
    }
}

#[cfg(test)]
#[path = "management_tests.rs"]
mod tests;
