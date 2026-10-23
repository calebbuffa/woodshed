//! Build a Cargo executable and collect native deployment directories.
use crate::{Error, Result};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Build one executable and copy it, with declared native resources, into a new directory.
///
/// `arguments` are Cargo target selectors and build options, for example
/// `--manifest-path app/Cargo.toml --bin app --release`. Native dependencies
/// opt in by emitting `cargo:rustc-env=WOODSHED_DEPLOYMENT_DIR=<directory>`.
/// Those directories must contain only redistributable runtime files, laid out
/// relative to the executable. Identical files are shared; conflicting contents fail.
///
/// The final executable gets an executable-relative rpath on Linux and macOS.
/// This invokes `cargo rustc`, not `cargo build`, so exactly one executable must
/// be selected. No runtime dependency discovery or licence inference is attempted.
/// The caller must supply complete native deployment trees.
/// Cargo's current-directory configuration hierarchy and target environment are
/// respected. CLI `--config` overrides are rejected rather than misinterpreted.
/// The destination is created only after staging all files successfully.
pub fn bundle_cargo(destination: &Path, arguments: &[OsString]) -> Result<PathBuf> {
    if destination.exists() {
        return Err(Error::Incomplete {
            detail: format!(
                "bundle destination {} already exists; choose a new directory",
                destination.display()
            ),
        });
    }
    if arguments.iter().any(|argument| argument == "--") {
        return Err(Error::Incomplete {
            detail: "pass Cargo options only, without rustc arguments after --".into(),
        });
    }
    let cwd = std::env::current_dir().map_err(|error| Error::io("get Cargo directory", error))?;
    let config = cargo_config2::Config::load_with_cwd(&cwd).map_err(config_error)?;
    let target = resolve_target(&config, arguments)?;
    let os = config
        .cfg::<cargo_config2::cfg::TargetOs, _>(target.as_str())
        .map_err(config_error)?;
    let mut command = Command::new("cargo");
    command
        .arg("rustc")
        .args(arguments)
        .arg("--message-format=json-render-diagnostics");
    if os == "linux" {
        command.args(["--", "-C", "link-arg=-Wl,-rpath,$ORIGIN"]);
    } else if os == "macos" {
        command.args(["--", "-C", "link-arg=-Wl,-rpath,@executable_path"]);
    }
    let mut child = command
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| Error::io("start Cargo bundle build", error))?;
    let mut executables = BTreeSet::new();
    let mut resources = BTreeSet::new();
    let read = (|| -> Result<()> {
        for line in BufReader::new(child.stdout.take().expect("piped Cargo output")).lines() {
            let line = line.map_err(|error| Error::io("read Cargo output", error))?;
            let message: serde_json::Value =
                serde_json::from_str(&line).map_err(|error| Error::Tool {
                    what: "parse Cargo build output".into(),
                    detail: error.to_string(),
                })?;
            if message["reason"] == "compiler-artifact"
                && let Some(path) = message["executable"].as_str()
                && !message["profile"]["test"].as_bool().unwrap_or(false)
            {
                executables.insert(PathBuf::from(path));
            }
            if message["reason"] == "build-script-executed"
                && let Some(environment) = message["env"].as_array()
            {
                for variable in environment {
                    if variable[0] == "WOODSHED_DEPLOYMENT_DIR"
                        && let Some(directory) = variable[1].as_str()
                    {
                        resources.insert(PathBuf::from(directory));
                    }
                }
            }
        }
        Ok(())
    })();
    let status = child
        .wait()
        .map_err(|error| Error::io("wait for Cargo build", error))?;
    read?;
    if !status.success() {
        return Err(Error::Tool {
            what: "Cargo bundle build".into(),
            detail: status.to_string(),
        });
    }
    if executables.len() != 1 {
        return Err(Error::Incomplete {
            detail: format!(
                "expected one executable, found {}; select --bin or --example",
                executables.len()
            ),
        });
    }
    let executable = executables.into_iter().next().expect("one executable");
    for directory in &resources {
        if !directory.is_dir() {
            return Err(Error::Incomplete {
                detail: format!("missing deployment directory {}", directory.display()),
            });
        }
    }
    assemble_bundle(destination, &executable, &resources)
}

fn config_error(error: cargo_config2::Error) -> Error {
    Error::Tool {
        what: "resolve Cargo bundle target".into(),
        detail: error.to_string(),
    }
}

fn resolve_target(config: &cargo_config2::Config, arguments: &[OsString]) -> Result<String> {
    let mut explicit = Vec::new();
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        let argument = argument.to_string_lossy();
        if argument == "--config" || argument.starts_with("--config=") {
            return Err(Error::Incomplete {
                detail: "bundle does not support --config overrides; use .cargo/config.toml or Cargo environment variables".into(),
            });
        }
        if argument == "--target" {
            let value = arguments.next().ok_or_else(|| Error::Incomplete {
                detail: "--target requires a value".into(),
            })?;
            explicit.push(value.to_string_lossy().into_owned());
        } else if let Some(value) = argument.strip_prefix("--target=") {
            explicit.push(value.to_owned());
        }
    }
    let targets = config
        .build_target_for_cli(&explicit)
        .map_err(config_error)?;
    match targets.as_slice() {
        [] => Ok(config.host_triple().map_err(config_error)?.to_owned()),
        [target] => Ok(target.clone()),
        _ => Err(Error::Incomplete {
            detail: "bundle requires exactly one Cargo build target".into(),
        }),
    }
}

fn assemble_bundle(
    destination: &Path,
    executable: &Path,
    resources: &BTreeSet<PathBuf>,
) -> Result<PathBuf> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|error| Error::io("create bundle parent", error))?;
    let staging = tempfile::Builder::new()
        .prefix(".woodshed-bundle-")
        .tempdir_in(parent)
        .map_err(|error| Error::io("create bundle staging directory", error))?;
    // Use normal directory permissions, not TempDir's private Unix permissions.
    let contents = staging.path().join("bundle");
    let result = (|| {
        fs::create_dir(&contents).map_err(|error| Error::io("create staged bundle", error))?;
        let name = executable.file_name().ok_or_else(|| Error::Incomplete {
            detail: "application executable has no filename".into(),
        })?;
        fs::copy(executable, contents.join(name))
            .map_err(|error| Error::io("copy application executable", error))?;
        for directory in resources {
            copy_tree(directory, &contents)?;
        }
        if destination.symlink_metadata().is_ok() {
            return Err(Error::Incomplete {
                detail: format!(
                    "bundle destination {} already exists",
                    destination.display()
                ),
            });
        }
        fs::rename(&contents, destination)
            .map_err(|error| Error::io("commit application bundle", error))?;
        Ok(destination.join(name))
    })();
    staging
        .close()
        .map_err(|error| Error::io("clean bundle staging directory", error))?;
    result
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    for entry in fs::read_dir(source).map_err(|error| Error::io("read runtime resources", error))? {
        let entry = entry.map_err(|error| Error::io("read runtime resource", error))?;
        let target = destination.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|error| Error::io("inspect runtime resource", error))?;
        if kind.is_dir() {
            if target.exists() && !target.is_dir() {
                return Err(Error::Incomplete {
                    detail: format!("conflicting bundle directory {}", target.display()),
                });
            }
            if !target.exists() {
                fs::create_dir(&target)
                    .map_err(|error| Error::io("create resource directory", error))?;
            }
            copy_tree(&entry.path(), &target)?;
        } else {
            if target.exists() {
                if target.is_file()
                    && crate::sha256_file(&entry.path())? == crate::sha256_file(&target)?
                {
                    continue;
                }
                return Err(Error::Incomplete {
                    detail: format!("conflicting bundle file {}", target.display()),
                });
            }
            fs::copy(entry.path(), &target).map_err(|error| {
                Error::io(
                    format!("copy runtime resource {}", entry.path().display()),
                    error,
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_cargo_targets_with_cli_environment_and_config_precedence() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(".cargo")).unwrap();
        fs::write(
            root.path().join(".cargo/config.toml"),
            "[build]\ntarget = 'aarch64-apple-darwin'\n",
        )
        .unwrap();
        let config = cargo_config2::Config::load_with_options(
            root.path(),
            cargo_config2::ResolveOptions::default().env([(
                OsString::from("CARGO_HOME"),
                root.path().join("home").into_os_string(),
            )]),
        )
        .unwrap();
        assert_eq!(
            resolve_target(&config, &[]).unwrap(),
            "aarch64-apple-darwin"
        );
        assert_eq!(
            resolve_target(&config, &["--target=x86_64-unknown-linux-gnu".into()]).unwrap(),
            "x86_64-unknown-linux-gnu"
        );
        let config = cargo_config2::Config::load_with_options(
            root.path(),
            cargo_config2::ResolveOptions::default().env([
                (
                    OsString::from("CARGO_HOME"),
                    root.path().join("home").into_os_string(),
                ),
                (
                    OsString::from("CARGO_BUILD_TARGET"),
                    OsString::from("x86_64-pc-windows-msvc"),
                ),
            ]),
        )
        .unwrap();
        assert_eq!(
            resolve_target(&config, &[]).unwrap(),
            "x86_64-pc-windows-msvc"
        );
        assert_eq!(
            resolve_target(&config, &["--target".into(), "aarch64-apple-darwin".into()]).unwrap(),
            "aarch64-apple-darwin"
        );
        assert!(resolve_target(&config, &["--target".into()]).is_err());
        assert!(resolve_target(&config, &["--config=build.target='wrong'".into()]).is_err());
        assert!(
            resolve_target(
                &config,
                &[
                    "--target=x86_64-pc-windows-msvc".into(),
                    "--target=aarch64-apple-darwin".into()
                ]
            )
            .is_err()
        );
    }

    #[test]
    fn bundle_failure_does_not_leave_a_destination_or_staging_files() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("app");
        fs::write(&executable, b"application").unwrap();
        let resources = root.path().join("resources");
        fs::create_dir(&resources).unwrap();
        fs::write(resources.join("app"), b"conflicting contents").unwrap();
        let destination = root.path().join("dist");
        assert!(
            assemble_bundle(
                &destination,
                &executable,
                &BTreeSet::from([resources.clone()])
            )
            .is_err()
        );
        assert!(!destination.exists());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
        fs::remove_file(resources.join("app")).unwrap();
        fs::write(resources.join("runtime"), b"library").unwrap();
        let output =
            assemble_bundle(&destination, &executable, &BTreeSet::from([resources])).unwrap();
        assert_eq!(fs::read(output).unwrap(), b"application");
        assert_eq!(fs::read(destination.join("runtime")).unwrap(), b"library");
        assert!(assemble_bundle(&destination, &executable, &BTreeSet::new()).is_err());
        assert_eq!(fs::read(destination.join("app")).unwrap(), b"application");
    }

    #[test]
    fn copying_resources_preserves_layout_and_rejects_collisions() {
        let root = std::env::temp_dir().join(format!("woodshed-bundle-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let source = root.join("source");
        let destination = root.join("destination");
        fs::create_dir_all(source.join("resources")).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source.join("compiler.dll"), b"library").unwrap();
        fs::write(source.join("resources/device.bc"), b"bitcode").unwrap();
        copy_tree(&source, &destination).unwrap();
        assert_eq!(
            fs::read(destination.join("resources/device.bc")).unwrap(),
            b"bitcode"
        );
        copy_tree(&source, &destination).unwrap();
        fs::write(source.join("compiler.dll"), b"different library").unwrap();
        assert!(copy_tree(&source, &destination).is_err());
        fs::remove_file(source.join("compiler.dll")).unwrap();
        fs::create_dir(source.join("compiler.dll")).unwrap();
        assert!(copy_tree(&source, &destination).is_err());
        fs::remove_dir(source.join("compiler.dll")).unwrap();
        fs::remove_file(source.join("resources/device.bc")).unwrap();
        fs::remove_dir(source.join("resources")).unwrap();
        fs::write(source.join("resources"), b"not a directory").unwrap();
        assert!(copy_tree(&source, &destination).is_err());
        assert!(bundle_cargo(&destination, &[]).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bundles_an_application_with_transitive_native_resources() {
        let root = std::env::temp_dir().join(format!("woodshed-bundle-app-{}", std::process::id()));
        fs::create_dir_all(root.join("app/src")).unwrap();
        fs::create_dir_all(root.join("native/src")).unwrap();
        fs::write(root.join("native/Cargo.toml"),
            "[package]\nname='bundle-native-fixture'\nversion='0.0.0'\nedition='2021'\n[workspace]\n").unwrap();
        fs::write(
            root.join("native/src/lib.rs"),
            "pub fn ready() -> bool { true }\n",
        )
        .unwrap();
        fs::write(
            root.join("native/build.rs"),
            r#"
fn main() {
    let directory = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("deploy");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("resource.txt"), "bundled resource").unwrap();
    println!("cargo:rustc-env=WOODSHED_DEPLOYMENT_DIR={}", directory.display());
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("app/Cargo.toml"),
            concat!(
                "[package]\nname='bundle-app-fixture'\nversion='0.0.0'\nedition='2021'\n",
                "[dependencies]\nbundle-native-fixture={path='../native'}\n[workspace]\n"
            ),
        )
        .unwrap();
        fs::write(
            root.join("app/src/main.rs"),
            r#"
fn main() {
    assert!(bundle_native_fixture::ready());
    let resource = std::env::current_exe().unwrap().parent().unwrap().join("resource.txt");
    assert_eq!(std::fs::read_to_string(resource).unwrap(), "bundled resource");
}
"#,
        )
        .unwrap();
        let arguments = vec![
            "--manifest-path".into(),
            root.join("app/Cargo.toml").into_os_string(),
            "--target-dir".into(),
            root.join("target").into_os_string(),
            "--bin".into(),
            "bundle-app-fixture".into(),
            "--offline".into(),
        ];
        let bundle = root.join("bundle");
        let executable = bundle_cargo(&bundle, &arguments).unwrap();
        assert!(
            Command::new(&executable)
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        fs::rename(&bundle, root.join("relocated")).unwrap();
        let executable = root.join("relocated").join(executable.file_name().unwrap());
        assert!(
            Command::new(executable)
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
