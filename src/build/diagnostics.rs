use super::*;
use crate::cache::{Lock, lock_path};
use crate::project::ResolvedDependency;
use std::collections::BTreeSet;

/// Outcome of the last successful source installation request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuildOutcome {
    /// An existing installation passed content and metadata verification.
    Reused,
    /// No complete installation existed for these exact inputs.
    Built,
    /// A verified prebuilt package was imported without compiling its source.
    Imported,
    /// A corrupt or interrupted installation was rebuilt.
    Repaired,
}

/// A comparison with the previous successful request for this declaration.
///
/// Values are hashes, never inherited environment values or recipe contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildExplanation {
    /// Manifest dependency display name.
    pub dependency: String,
    /// Exact installation fingerprint.
    pub artifact: String,
    /// Mutable workspace hard-identity fingerprint.
    pub workspace: Option<String>,
    /// Whether this request built, repaired or reused the installation.
    pub outcome: BuildOutcome,
    /// Exact installation selected by the previous successful request, if any.
    pub previous_artifact: Option<String>,
    /// Sorted names of added, removed or changed hashed input fields.
    pub changed_inputs: Vec<String>,
    /// Hashes of the current identity fields; raw values are never recorded here.
    pub input_fields: BTreeMap<String, String>,
}

fn path(cache: &Cache, manifest: &Path, id: &str) -> Result<PathBuf> {
    let manifest = manifest
        .canonicalize()
        .map_err(|error| Error::io("resolve build explanation namespace", error))?;
    let key = BuildKey::explicit("build-explanation")
        .mix(manifest.as_os_str().as_encoded_bytes())
        .mix(id);
    Ok(cache
        .root()
        .join("diagnostics")
        .join(format!("{}.json", key.fingerprint())))
}

fn read(path: &Path) -> Result<Option<BuildExplanation>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| invalid(format!("invalid build explanation: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::io("read build explanation", error)),
    }
}

impl Cache {
    /// Read the last successful source-build decision without building or networking.
    /// `id` is the resolved declaration ID, including any target variant suffix.
    pub fn explain_build(&self, manifest: &Path, id: &str) -> Result<Option<BuildExplanation>> {
        let path = path(self, manifest, id)?;
        let _lock = Lock::acquire(&lock_path(&path, "diagnostic"))?;
        read(&path)
    }
}

pub(super) fn completed(
    cache: &Cache,
    manifest: &Path,
    node: &ResolvedDependency,
    artifact: &str,
    workspace: Option<&BuildKey>,
    outcome: BuildOutcome,
    input_fields: BTreeMap<String, String>,
) -> Result<()> {
    let path = path(cache, manifest, &node.id)?;
    super::management::pin_build(cache, artifact)?;
    let _lock = Lock::acquire(&lock_path(&path, "diagnostic"))?;
    let prior = read(&path)?;
    let changed_inputs = prior
        .as_ref()
        .map(|prior| changes(&prior.input_fields, &input_fields))
        .unwrap_or_default();
    let explanation = BuildExplanation {
        dependency: node.name.clone(),
        artifact: artifact.into(),
        workspace: workspace.map(BuildKey::fingerprint),
        outcome,
        previous_artifact: prior.map(|prior| prior.artifact),
        changed_inputs,
        input_fields,
    };
    crate::progress::notify(&format!(
        "{} {:?}; artifact {}; {}",
        node.name,
        explanation.outcome,
        artifact,
        if explanation.previous_artifact.is_none() {
            "first successful request".into()
        } else if explanation.changed_inputs.is_empty() {
            "exact inputs unchanged".into()
        } else {
            format!("changed inputs: {}", explanation.changed_inputs.join(", "))
        }
    ));
    storage::atomic_json(&path, &explanation)
}

pub(super) fn starting(
    cache: &Cache,
    manifest: &Path,
    node: &ResolvedDependency,
    fields: &BTreeMap<String, String>,
    repaired: bool,
) -> Result<()> {
    let prior = cache.explain_build(manifest, &node.id)?;
    let reason = if repaired {
        "cached installation corrupt or interrupted; repairing".into()
    } else if let Some(prior) = prior {
        let changed = changes(&prior.input_fields, fields);
        if changed.is_empty() {
            "exact inputs unchanged; verified installation unavailable".into()
        } else {
            format!("changed inputs: {}", changed.join(", "))
        }
    } else {
        "no previous successful request".into()
    };
    crate::progress::notify(&format!("{}: building/importing; {reason}", node.name));
    Ok(())
}

fn changes(previous: &BTreeMap<String, String>, current: &BTreeMap<String, String>) -> Vec<String> {
    previous
        .keys()
        .chain(current.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|key| previous.get(*key) != current.get(*key))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparisons_include_added_removed_and_changed_inputs_without_values() {
        let previous = BTreeMap::from([
            ("env:TOKEN".into(), "hash-a".into()),
            ("removed".into(), "old".into()),
            ("same".into(), "same".into()),
        ]);
        let current = BTreeMap::from([
            ("env:TOKEN".into(), "hash-b".into()),
            ("added".into(), "new".into()),
            ("same".into(), "same".into()),
        ]);
        assert_eq!(
            changes(&previous, &current),
            ["added", "env:TOKEN", "removed"]
        );
    }
}
