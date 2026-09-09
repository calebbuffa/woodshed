//! Archive extraction, one format per feature.

use crate::{Error, Result};
use std::path::Path;

/// Extract `archive` into `dest`, dispatching on the filename extension.
///
/// Only formats whose features are enabled are supported; anything else is a
/// clear error naming the feature to turn on rather than a mysterious failure.
pub fn extract(archive: &Path, dest: &Path) -> Result<()> {
    let name = archive
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();

    if name.ends_with(".zip") {
        return extract_zip(archive, dest);
    }
    if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") || name.ends_with(".tbz") {
        return extract_tar_bz2(archive, dest);
    }
    if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        return extract_tar_xz(archive, dest);
    }
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        return extract_tar_gz(archive, dest);
    }

    Err(Error::Extract {
        archive: archive.to_path_buf(),
        detail: "unrecognized archive format".into(),
    })
}

#[cfg(feature = "tar-gz")]
fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<()> {
    let file = open(archive)?;
    unpack_tar(flate2::read::GzDecoder::new(file), archive, dest)
}

#[cfg(not(feature = "tar-gz"))]
fn extract_tar_gz(archive: &Path, _dest: &Path) -> Result<()> {
    Err(Error::UnsupportedFormat {
        archive: archive.to_path_buf(),
        feature: "tar-gz",
    })
}

#[cfg(feature = "tar-bz2")]
fn extract_tar_bz2(archive: &Path, dest: &Path) -> Result<()> {
    let file = open(archive)?;
    unpack_tar(bzip2::read::BzDecoder::new(file), archive, dest)
}

#[cfg(not(feature = "tar-bz2"))]
fn extract_tar_bz2(archive: &Path, _dest: &Path) -> Result<()> {
    Err(Error::UnsupportedFormat {
        archive: archive.to_path_buf(),
        feature: "tar-bz2",
    })
}

#[cfg(feature = "tar-xz")]
fn extract_tar_xz(archive: &Path, dest: &Path) -> Result<()> {
    let file = open(archive)?;
    unpack_tar(liblzma::read::XzDecoder::new(file), archive, dest)
}

#[cfg(not(feature = "tar-xz"))]
fn extract_tar_xz(archive: &Path, _dest: &Path) -> Result<()> {
    Err(Error::UnsupportedFormat {
        archive: archive.to_path_buf(),
        feature: "tar-xz",
    })
}

#[cfg(any(feature = "tar-gz", feature = "tar-bz2", feature = "tar-xz"))]
fn unpack_tar(reader: impl std::io::Read, archive: &Path, dest: &Path) -> Result<()> {
    tar::Archive::new(reader)
        .unpack(dest)
        .map_err(|error| Error::Extract {
            archive: archive.to_path_buf(),
            detail: error.to_string(),
        })
}

#[cfg(any(
    feature = "tar-gz",
    feature = "tar-bz2",
    feature = "tar-xz",
    feature = "zip"
))]
fn open(archive: &Path) -> Result<std::fs::File> {
    std::fs::File::open(archive)
        .map_err(|error| Error::io(format!("cannot open {}", archive.display()), error))
}

/// Unpack a ZIP archive.
///
/// Entries are validated against path traversal, and symlinks are rejected
/// outright: an archive that writes outside `dest` during a build script is
/// indistinguishable from an attack.
#[cfg(feature = "zip")]
fn extract_zip(archive: &Path, dest: &Path) -> Result<()> {
    use std::fs;

    let fail = |detail: String| Error::Extract {
        archive: archive.to_path_buf(),
        detail,
    };

    let mut zip = zip::ZipArchive::new(open(archive)?).map_err(|error| fail(error.to_string()))?;

    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|error| fail(format!("entry {index}: {error}")))?;

        let relative = entry
            .enclosed_name()
            .ok_or_else(|| fail(format!("entry `{}` escapes the archive root", entry.name())))?;
        let output = dest.join(relative);

        if entry.is_dir() {
            fs::create_dir_all(&output)
                .map_err(|error| Error::io(format!("cannot create {}", output.display()), error))?;
            continue;
        }
        if entry.is_symlink() {
            return Err(fail(format!("unsupported symlink `{}`", entry.name())));
        }

        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| Error::io(format!("cannot create {}", parent.display()), error))?;
        }
        let mut target = fs::File::create(&output)
            .map_err(|error| Error::io(format!("cannot create {}", output.display()), error))?;
        std::io::copy(&mut entry, &mut target)
            .map_err(|error| Error::io(format!("cannot write {}", output.display()), error))?;

        // ZIP records a Unix mode, and unlike tar the crate does not apply it.
        // Dropping it leaves `configure` and friends non-executable, which
        // fails the build only on Unix and only for zipped dependencies.
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            drop(target);
            fs::set_permissions(&output, fs::Permissions::from_mode(mode)).map_err(|error| {
                Error::io(format!("cannot set mode on {}", output.display()), error)
            })?;
        }
    }
    Ok(())
}

#[cfg(not(feature = "zip"))]
fn extract_zip(archive: &Path, _dest: &Path) -> Result<()> {
    Err(Error::UnsupportedFormat {
        archive: archive.to_path_buf(),
        feature: "zip",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "woodshed-archive-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_unknown_format_names_the_feature_to_enable() {
        let dir = temp_dir("unknown");
        let archive = dir.join("thing-1.0.rar");
        std::fs::write(&archive, b"").unwrap();

        let error = extract(&archive, &dir).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("rar") || message.contains("format"),
            "unhelpful error: {message}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "zip")]
    mod zip_archive {
        use super::*;
        use std::io::Write;

        /// Build a zip containing one 0o755 script and one 0o644 file.
        fn write_archive(path: &std::path::Path) {
            let file = std::fs::File::create(path).unwrap();
            let mut writer = zip::ZipWriter::new(file);

            let script: zip::write::FileOptions<'_, ()> =
                zip::write::FileOptions::default().unix_permissions(0o755);
            writer.start_file("pkg/configure", script).unwrap();
            writer.write_all(b"#!/bin/sh\n").unwrap();

            let plain: zip::write::FileOptions<'_, ()> =
                zip::write::FileOptions::default().unix_permissions(0o644);
            writer.start_file("pkg/README", plain).unwrap();
            writer.write_all(b"hello").unwrap();

            writer.finish().unwrap();
        }

        #[test]
        fn round_trips_contents() {
            let dir = temp_dir("zip-roundtrip");
            let archive = dir.join("pkg-1.0.zip");
            write_archive(&archive);

            let dest = dir.join("out");
            extract(&archive, &dest).unwrap();

            assert_eq!(std::fs::read(dest.join("pkg/README")).unwrap(), b"hello");
            assert!(dest.join("pkg/configure").is_file());
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// The tar readers restore modes on their own; the zip path has to do
        /// it explicitly, so an extracted `configure` stays runnable.
        #[cfg(unix)]
        #[test]
        fn preserves_the_executable_bit() {
            use std::os::unix::fs::PermissionsExt;

            let dir = temp_dir("zip-modes");
            let archive = dir.join("pkg-1.0.zip");
            write_archive(&archive);

            let dest = dir.join("out");
            extract(&archive, &dest).unwrap();

            let mode = |name: &str| {
                std::fs::metadata(dest.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777
            };
            assert_eq!(mode("pkg/configure"), 0o755, "script must stay executable");
            assert_eq!(mode("pkg/README"), 0o644);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn rejects_paths_that_escape_the_destination() {
            let dir = temp_dir("zip-traversal");
            let archive = dir.join("evil-1.0.zip");

            let file = std::fs::File::create(&archive).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            // `ZipWriter` normalises paths, so write the raw name.
            writer
                .start_file_from_path(std::path::Path::new("../escaped.txt"), options)
                .unwrap();
            writer.write_all(b"pwned").unwrap();
            writer.finish().unwrap();

            let dest = dir.join("out");
            let _ = extract(&archive, &dest);
            assert!(
                !dir.join("escaped.txt").exists(),
                "extraction must not write outside the destination"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
