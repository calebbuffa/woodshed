//! Fetching archives over HTTPS.

use crate::{Error, Result};
use std::fs;
use std::path::Path;

/// Download `url` to `dest`.
///
/// Tries a pure-Rust HTTP client first so no external tooling is required,
/// then falls back to the system `curl`. The fallback matters in corporate
/// environments where proxy and TLS trust settings live in the OS
/// configuration that a bundled client does not see.
pub fn fetch(url: &str, dest: &Path) -> Result<()> {
    let mut detail = match fetch_ureq(url, dest) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };

    match fetch_curl(url, dest) {
        Ok(()) => return Ok(()),
        Err(error) => detail = format!("{detail}; {error}"),
    }

    Err(Error::Download {
        url: url.to_string(),
        detail,
    })
}

fn fetch_ureq(url: &str, dest: &Path) -> std::result::Result<(), String> {
    let response = ureq::get(url)
        .call()
        .map_err(|error| format!("http: {error}"))?;
    let mut file = fs::File::create(dest).map_err(|error| format!("create: {error}"))?;
    std::io::copy(&mut response.into_body().as_reader(), &mut file)
        .map_err(|error| format!("write: {error}"))?;
    Ok(())
}

fn fetch_curl(url: &str, dest: &Path) -> std::result::Result<(), String> {
    let status = std::process::Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--output",
        ])
        .arg(dest)
        .arg(url)
        .status()
        .map_err(|error| format!("curl: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("curl: exited {status}"))
    }
}
