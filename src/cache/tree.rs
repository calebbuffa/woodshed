use super::{Error, Path, PathBuf, Result, fs, is_link};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// Fingerprint managed trees without allowing links to escape their owner.
pub(crate) fn tree_fingerprint(root: &Path) -> Result<String> {
    fingerprint(root, false)
}

#[cfg(feature = "build")]
pub(crate) fn snapshot_fingerprint(root: &Path) -> Result<String> {
    fingerprint(root, true)
}

/// Resolves a relative link against its parent without touching the filesystem.
fn lexical_target(path: &Path, link: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut target = path.parent()?.to_path_buf();
    for component in link.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !target.pop() {
                    return None;
                }
            }
            Component::Normal(part) => target.push(part),
            Component::Prefix(_) | Component::RootDir => return None,
        }
    }
    Some(target)
}

fn fingerprint(root: &Path, allow_absolute: bool) -> Result<String> {
    let _progress = crate::progress::Phase::new(format!("verify tree {}", root.display()));
    fn visit(
        path: &Path,
        root: &Path,
        hash: &mut Sha256,
        ancestors: &mut BTreeSet<PathBuf>,
        allow_absolute: bool,
    ) -> Result<()> {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            Error::io(format!("inspect cached input {}", path.display()), error)
        })?;
        let metadata = if is_link(&metadata) {
            let link =
                fs::read_link(path).map_err(|error| Error::io("read cached source link", error))?;
            if link.is_absolute() && !allow_absolute {
                return Err(Error::Incomplete {
                    detail: format!("absolute link in managed source tree: {}", path.display()),
                });
            }
            // Windows cannot follow links with `/` separators, and dangling links
            // have no target; both are fingerprinted by their text alone.
            let resolved = path.canonicalize().ok();
            let contained = match &resolved {
                Some(target) => target.starts_with(root),
                None => lexical_target(path, &link).is_some_and(|target| target.starts_with(root)),
            };
            if !contained {
                return Err(Error::Incomplete {
                    detail: format!("link escapes managed source tree: {}", path.display()),
                });
            }
            hash.update(b"link\0");
            let bytes = link.as_os_str().as_encoded_bytes();
            hash.update(bytes.len().to_le_bytes());
            hash.update(bytes);
            if resolved.is_none() {
                return Ok(());
            }
            fs::metadata(path).map_err(|error| Error::io("inspect cached link target", error))?
        } else {
            metadata
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            hash.update(metadata.permissions().mode().to_le_bytes());
        }
        #[cfg(not(unix))]
        hash.update([u8::from(metadata.permissions().readonly())]);
        if metadata.is_file() {
            hash.update(b"file\0");
            hash.update(crate::sha256_file(path)?);
            return Ok(());
        }
        if !metadata.is_dir() {
            return Err(Error::Incomplete {
                detail: format!(
                    "unsupported entry in managed source tree: {}",
                    path.display()
                ),
            });
        }
        let canonical = path
            .canonicalize()
            .map_err(|error| Error::io("resolve cached source directory", error))?;
        if !ancestors.insert(canonical.clone()) {
            return Err(Error::Incomplete {
                detail: format!("cyclic link in managed source tree: {}", path.display()),
            });
        }
        hash.update(b"directory\0");
        let mut entries = fs::read_dir(path)
            .map_err(|error| Error::io("read cached source directory", error))?
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|error| Error::io("read cached source entry", error))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            let bytes = name.as_encoded_bytes();
            hash.update(bytes.len().to_le_bytes());
            hash.update(bytes);
            visit(&entry.path(), root, hash, ancestors, allow_absolute)?;
        }
        ancestors.remove(&canonical);
        Ok(())
    }

    let metadata = fs::symlink_metadata(root)
        .map_err(|error| Error::io("inspect managed source root", error))?;
    if !metadata.is_dir() || is_link(&metadata) {
        return Err(Error::Incomplete {
            detail: format!(
                "managed source root is not an owned directory: {}",
                root.display()
            ),
        });
    }
    let canonical = root
        .canonicalize()
        .map_err(|error| Error::io("resolve managed source root", error))?;
    let mut hash = Sha256::new();
    visit(
        &canonical,
        &canonical,
        &mut hash,
        &mut BTreeSet::new(),
        allow_absolute,
    )?;
    Ok(format!("{:x}", hash.finalize()))
}
