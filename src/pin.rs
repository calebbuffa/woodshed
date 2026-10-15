//! Maintaining `native/versions.toml`: add a dependency, bump its version, and
//! check the pins - computing the SHA-256 for you.
//!
//! This is the only part of woodshed that downloads on purpose outside a
//! build. `build.rs` never calls it: builds stay offline and deterministic,
//! and only a person running `woodshed add` / `bump` can change a pin. Edits
//! preserve the file's comments and layout.

use crate::{Dep, Error, Result};
use std::path::Path;
use toml_edit::{Array, DocumentMut, Item, Table, value};

fn manifest_err(detail: impl Into<String>) -> Error {
    Error::Manifest {
        detail: detail.into(),
    }
}

fn read(path: &Path) -> Result<DocumentMut> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::io(format!("cannot read {}", path.display()), e))?;
    text.parse::<DocumentMut>()
        .map_err(|e| manifest_err(format!("{}: {e}", path.display())))
}

fn write(path: &Path, doc: &DocumentMut) -> Result<()> {
    std::fs::write(path, doc.to_string())
        .map_err(|e| Error::io(format!("cannot write {}", path.display()), e))
}

fn expand(template: &str, name: &str, version: &str) -> String {
    template
        .replace("{version}", version)
        .replace("{name}", name)
}

/// Download `url` and return the SHA-256 of what came back.
pub fn sha256_of_url(url: &str) -> Result<String> {
    let tmp = std::env::temp_dir().join(format!("woodshed-pin-{}.part", std::process::id()));
    let result = crate::download::fetch(url, &tmp).and_then(|()| crate::sha256_file(&tmp));
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Add a new `[name]` entry, downloading `url` (templates allowed) to compute
/// its checksum. Fails if the entry already exists; use [`bump`] to change it.
pub fn add(
    manifest: &Path,
    name: &str,
    version: &str,
    url: &str,
    markers: &[String],
    file: Option<&str>,
) -> Result<String> {
    let mut doc = read(manifest)?;
    if doc.contains_key(name) {
        return Err(manifest_err(format!(
            "`[{name}]` already exists; use `bump` to change its version"
        )));
    }
    let sha = sha256_of_url(&expand(url, name, version))?;

    let mut table = Table::new();
    table["version"] = value(version);
    table["url"] = value(url);
    table["sha256"] = value(sha.as_str());
    if !markers.is_empty() {
        let mut array = Array::new();
        markers.iter().for_each(|m| array.push(m.as_str()));
        table["markers"] = value(array);
    }
    if let Some(file) = file {
        table["file"] = value(file);
    }
    doc.insert(name, Item::Table(table));
    write(manifest, &doc)?;
    Ok(sha)
}

/// Point `[name]` at `new_version`: rewrite the version (and the URL and file
/// name when they are literal rather than templates), re-download, and update
/// the checksum. `url` overrides the URL for upstreams whose layout is not a
/// function of the version.
pub fn bump(manifest: &Path, name: &str, new_version: &str, url: Option<&str>) -> Result<String> {
    let mut doc = read(manifest)?;
    let table = doc
        .get_mut(name)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| manifest_err(format!("no `[{name}]` section")))?;
    let old_version = table
        .get("version")
        .and_then(Item::as_str)
        .ok_or_else(|| manifest_err(format!("`[{name}]` has no version")))?
        .to_string();

    let retarget = |table: &mut Table, key: &str| -> Result<()> {
        let Some(current) = table.get(key).and_then(Item::as_str).map(String::from) else {
            return Ok(());
        };
        if current.contains("{version}") || current.contains("{name}") {
            return Ok(());
        }
        if !current.contains(&old_version) {
            return Err(manifest_err(format!(
                "`[{name}].{key}` does not contain the old version `{old_version}`; \
                 pass the new URL explicitly"
            )));
        }
        table[key] = value(current.replace(&old_version, new_version));
        Ok(())
    };

    if let Some(url) = url {
        table["url"] = value(url);
    } else {
        retarget(table, "url")?;
    }
    retarget(table, "file")?;
    table["version"] = value(new_version);

    let template = table["url"].as_str().unwrap_or_default().to_string();
    let sha = sha256_of_url(&expand(&template, name, new_version))?;
    table["sha256"] = value(sha.as_str());
    write(manifest, &doc)?;
    Ok(sha)
}

/// Re-download every pinned archive and compare it with its recorded checksum.
/// Returns `(name, outcome)` for each dependency.
pub fn verify(manifest: &Path) -> Result<Vec<(String, Result<()>)>> {
    let manifest = crate::Manifest::load(manifest)?;
    Ok(manifest
        .iter()
        .map(|dep: &Dep| {
            let outcome = sha256_of_url(&dep.url).and_then(|actual| {
                if actual.eq_ignore_ascii_case(&dep.sha256) {
                    Ok(())
                } else {
                    Err(Error::Checksum {
                        path: dep.url.clone().into(),
                        expected: dep.sha256.clone(),
                        actual,
                    })
                }
            });
            (dep.name.clone(), outcome)
        })
        .collect())
}
