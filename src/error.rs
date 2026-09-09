//! Typed errors for every way provisioning can fail.
//!
//! Build-script failures are read by people who did not write the build
//! script, usually once, while annoyed. Every variant therefore renders a
//! message saying what was attempted, what went wrong, and what to do next.

use std::fmt;
use std::path::PathBuf;

/// The result of a provisioning step.
pub type Result<T> = std::result::Result<T, Error>;

/// Something went wrong while provisioning a native dependency.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// An I/O operation failed.
    Io {
        /// What was being attempted.
        context: String,
        /// The underlying failure.
        source: std::io::Error,
    },
    /// A file's contents did not match its pinned checksum.
    Checksum {
        /// The file that was verified.
        path: PathBuf,
        /// The pinned checksum.
        expected: String,
        /// What the file actually hashed to.
        actual: String,
    },
    /// Every download method failed.
    Download {
        /// The URL that was requested.
        url: String,
        /// What each method reported.
        detail: String,
    },
    /// Offline mode is on and the cache cannot satisfy the request.
    Offline {
        /// What is missing, and how to supply it.
        detail: String,
    },
    /// An archive could not be unpacked, or unpacked incompletely.
    Extract {
        /// The archive that was being unpacked.
        archive: PathBuf,
        /// What went wrong.
        detail: String,
    },
    /// A dependency manifest could not be read or understood.
    Manifest {
        /// What was wrong with it.
        detail: String,
    },
    /// A cache lock could not be acquired within the timeout.
    Lock {
        /// The lock file.
        path: PathBuf,
        /// Why acquisition failed.
        detail: String,
    },
    /// An external tool (CMake, a compiler) failed or is missing.
    Tool {
        /// Which step was running.
        what: String,
        /// How it failed.
        detail: String,
    },
    /// A tree or install prefix exists but is missing required files.
    Incomplete {
        /// What is missing, and where.
        detail: String,
    },
    /// An archive format was encountered whose support was not compiled in.
    UnsupportedFormat {
        /// The archive that could not be handled.
        archive: PathBuf,
        /// The woodshed feature that would handle it.
        feature: &'static str,
    },
}

impl Error {
    /// Attach context to an [`std::io::Error`].
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => write!(f, "{context}: {source}"),
            Self::Checksum {
                path,
                expected,
                actual,
            } => write!(
                f,
                "checksum mismatch for {}\n  expected: {expected}\n  actual:   {actual}",
                path.display()
            ),
            Self::Download { url, detail } => write!(
                f,
                "failed to download {url}: {detail}\n  \
                 check network connectivity, or configure an HTTP proxy"
            ),
            Self::Offline { detail } => write!(f, "offline mode: {detail}"),
            Self::Extract { archive, detail } => {
                write!(f, "cannot extract {}: {detail}", archive.display())
            }
            Self::Manifest { detail } => write!(f, "invalid dependency manifest: {detail}"),
            Self::Lock { path, detail } => write!(
                f,
                "cannot acquire cache lock {}: {detail}\n  \
                 another build may hold it; if none is running, delete the file",
                path.display()
            ),
            Self::Tool { what, detail } => write!(f, "{what}: {detail}"),
            Self::Incomplete { detail } => write!(f, "{detail}"),
            Self::UnsupportedFormat { archive, feature } => write!(
                f,
                "cannot extract {}: support is not compiled in\n  \
                 enable woodshed's `{feature}` feature",
                archive.display()
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
