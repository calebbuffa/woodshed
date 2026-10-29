#![cfg(feature = "cli")]

use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Output},
};

fn cli(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_woodshed"))
        .args(arguments)
        .output()
        .unwrap()
}

fn archive_server(body: &'static [u8]) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/fixture.zip", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("fixture request did not arrive: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&buffer[..count]);
        }
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
    });
    (url, worker)
}

#[test]
fn cli_has_one_project_format_and_never_discovers_or_updates_combined_pins() {
    let directory = tempfile::tempdir().unwrap();
    let native = directory.path().join("native");
    fs::create_dir(&native).unwrap();
    let old = native.join("versions.toml");
    let original = format!(
        "[example]\nversion='1'\nurl='https://example.test/a.zip'\nsha256='{}'\n",
        "a".repeat(64)
    );
    fs::write(&old, &original).unwrap();
    let run = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_woodshed"))
            .current_dir(directory.path())
            .args(arguments)
            .output()
            .unwrap()
    };
    assert!(
        !run(&["migrate", "--manifest", old.to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        !run(&["lock", "--manifest", old.to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        !run(&["bump", "example", "2", "--manifest", old.to_str().unwrap()])
            .status
            .success()
    );
    assert!(!run(&["lock", "--locked"]).status.success());
    fs::create_dir(directory.path().join("source")).unwrap();
    let result = run(&["add", "new", "--path", "source"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(directory.path().join("woodshed.toml").is_file());
    assert!(directory.path().join("woodshed.lock").is_file());
    assert_eq!(fs::read_to_string(old).unwrap(), original);
}

#[test]
fn progress_log_is_live_append_only_and_keeps_stdout_machine_readable() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("progress.log");
    fs::write(&log, "previous run\n").unwrap();
    let manifest = directory.path().join("woodshed.toml");
    let (url, server) = archive_server(b"progress fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_woodshed"))
        .args([
            "add",
            "example",
            "--version",
            "1",
            "--url",
            &url,
            "--dry-run",
            "--manifest",
        ])
        .arg(&manifest)
        .env("WOODSHED_LOG", &log)
        .env("WOODSHED_PROGRESS", "0")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let preview = String::from_utf8(output.stdout).unwrap();
    assert!(preview.contains("[dependencies.example]"));
    assert!(preview.contains("[sources.example]"));
    assert!(!preview.contains("woodshed:"));
    let text = fs::read_to_string(&log).unwrap();
    assert!(text.starts_with("previous run\n"));
    assert!(text.contains("starting download"));
    assert!(text.contains("ended download"));
    assert!(text.contains("pid="));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("woodshed: starting"));
}

#[test]
fn ordinary_cli_can_add_and_bump_git_sources_without_extra_features() {
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().join("upstream");
    fs::create_dir(&repository).unwrap();
    let git = |arguments: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args([
                "-c",
                "user.name=Woodshed Test",
                "-c",
                "user.email=woodshed@example.test",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "--quiet"]);
    fs::write(repository.join("CMakeLists.txt"), "# first\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "first"]);
    let first = git(&["rev-parse", "HEAD"]);
    fs::write(repository.join("CMakeLists.txt"), "# second\n").unwrap();
    git(&["commit", "--quiet", "-a", "-m", "second"]);
    let second = git(&["rev-parse", "HEAD"]);
    let manifest = directory.path().join("woodshed.toml");
    let run = |arguments: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_woodshed"))
            .args(arguments)
            .arg("--manifest")
            .arg(&manifest)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&[
        "add",
        "example",
        "--version",
        "1",
        "--git",
        repository.to_str().unwrap(),
        "--ref",
        &first,
    ]);
    let before = fs::read(directory.path().join("woodshed.lock")).unwrap();
    run(&["bump", "example", "2", "--ref", &second, "--dry-run"]);
    assert_eq!(
        fs::read(directory.path().join("woodshed.lock")).unwrap(),
        before
    );
    run(&["bump", "example", "2", "--ref", &second]);
    let project = woodshed::project::Project::load(&manifest).unwrap();
    let locked = project.locked().unwrap();
    let dep = &locked.sources["example"];
    assert_eq!(dep.version, "2");
    assert_eq!(dep.git.as_ref().unwrap().revision, second);
}

#[test]
fn pin_preserving_lock_refresh_is_offline_and_supports_dry_run() {
    let directory = tempfile::tempdir().unwrap();
    let manifest = directory.path().join("woodshed.toml");
    let previous = directory.path().join("previous.toml");
    let original = "[dependencies.native]\npath='native'\n";
    fs::write(&manifest, original).unwrap();
    fs::write(&previous, original).unwrap();
    let manifest = manifest.to_str().unwrap();
    assert!(cli(&["lock", "--manifest", manifest]).status.success());
    let lock = directory.path().join("woodshed.lock");
    let before = fs::read(&lock).unwrap();
    fs::write(
        manifest,
        format!("[features]\ncpu=[]\n{original}optional=true\n"),
    )
    .unwrap();
    let arguments = [
        "lock",
        "--manifest",
        manifest,
        "--preserve-pins-from",
        previous.to_str().unwrap(),
    ];
    let mut dry_run = arguments.to_vec();
    dry_run.push("--dry-run");
    assert!(cli(&dry_run).status.success());
    assert_eq!(fs::read(&lock).unwrap(), before);
    assert!(cli(&arguments).status.success());
    assert!(
        cli(&["lock", "--manifest", manifest, "--locked"])
            .status
            .success()
    );
    assert_ne!(fs::read(&lock).unwrap(), before);
}

#[test]
fn clap_help_and_source_argument_errors_are_explicit() {
    for args in [
        vec!["--help"],
        vec!["bump", "--help"],
        vec!["add", "--help"],
        vec!["bundle", "--help"],
        vec!["doctor", "--help"],
        vec!["cache", "--help"],
        vec!["cache-explain", "--help"],
    ] {
        assert!(cli(&args).status.success(), "{args:?}");
    }
    for args in [
        vec!["update-many"],
        vec!["bump", "example", "2", "--typo", "value"],
        vec![
            "add",
            "example",
            "--version",
            "1",
            "--git",
            "repo",
            "--url",
            "url",
        ],
        vec![
            "add",
            "example",
            "--version",
            "1",
            "--git",
            "repo",
            "--file",
            "unused.zip",
        ],
        vec!["bundle", "--output", "somewhere"],
        vec!["cache", "prune"],
    ] {
        assert!(!cli(&args).status.success(), "{args:?}");
    }
}

#[test]
fn normal_add_bump_and_dry_run_use_real_download_hashing_and_atomic_manifest_edits() {
    let directory = tempfile::tempdir().unwrap();
    let manifest = directory.path().join("native").join("woodshed.toml");
    let manifest_arg = manifest.to_str().unwrap();
    let (url, server) = archive_server(b"first archive bytes");
    let output = cli(&[
        "add",
        "example",
        "--version",
        "1",
        "--url",
        &url,
        "--marker",
        "CMakeLists.txt",
        "--manifest",
        manifest_arg,
    ]);
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let first = fs::read(&manifest).unwrap();
    let pins = woodshed::project::Project::load(&manifest)
        .unwrap()
        .locked()
        .unwrap()
        .sources;
    let archive = directory.path().join("archive");
    fs::write(&archive, b"first archive bytes").unwrap();
    assert_eq!(
        pins.get("example").unwrap().sha256,
        woodshed::sha256_file(&archive).unwrap()
    );
    let (url, server) = archive_server(b"second archive bytes");
    let output = cli(&[
        "bump",
        "example",
        "2",
        "--url",
        &url,
        "--manifest",
        manifest_arg,
        "--dry-run",
    ]);
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let preview = String::from_utf8(output.stdout).unwrap();
    assert!(preview.contains("version = \"2\""));
    assert!(preview.contains("[dependencies.example]"));
    assert!(preview.contains("[sources.example]"));
    assert_eq!(fs::read(&manifest).unwrap(), first);
    let (url, server) = archive_server(b"second archive bytes");
    let output = cli(&[
        "bump",
        "example",
        "2",
        "--url",
        &url,
        "--manifest",
        manifest_arg,
    ]);
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pins = woodshed::project::Project::load(&manifest)
        .unwrap()
        .locked()
        .unwrap()
        .sources;
    let dep = pins.get("example").unwrap();
    assert_eq!(dep.version, "2");
    assert_eq!(dep.markers, ["CMakeLists.txt"]);
    fs::write(&archive, b"second archive bytes").unwrap();
    assert_eq!(dep.sha256, woodshed::sha256_file(&archive).unwrap());
}

#[test]
fn new_projects_default_to_declarations_with_machine_generated_hashes_and_locked_checks() {
    let directory = tempfile::tempdir().unwrap();
    let run = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_woodshed"))
            .current_dir(directory.path())
            .args(arguments)
            .output()
            .unwrap()
    };
    let (url, server) = archive_server(b"declarative archive");
    let output = run(&["add", "example", "--version", "1", "--url", &url]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    let path = directory.path().join("woodshed.toml");
    let declarations = fs::read_to_string(&path).unwrap();
    assert!(declarations.contains("[dependencies.example]"));
    assert!(!declarations.contains("sha256"));
    assert!(directory.path().join("woodshed.lock").is_file());
    assert!(run(&["lock", "--locked"]).status.success());
    let before_lock = fs::read(directory.path().join("woodshed.lock")).unwrap();
    let (url, server) = archive_server(b"next declaration archive");
    let output = run(&["bump", "example", "2", "--url", &url, "--dry-run"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), declarations);
    assert_eq!(
        fs::read(directory.path().join("woodshed.lock")).unwrap(),
        before_lock
    );
    fs::write(
        &path,
        declarations.replace("version = \"1\"", "version = \"2\""),
    )
    .unwrap();
    let output = run(&["lock", "--locked"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not match"));
}

#[test]
fn provider_cli_requires_explicit_profile_target_metadata_and_disallows_source_mix() {
    let directory = tempfile::tempdir().unwrap();
    let manifest = directory.path().join("woodshed.toml");
    for arguments in [
        vec!["add", "zstd", "--version", "1.5.7", "--provider", "vcpkg"],
        vec![
            "add",
            "zstd",
            "--version",
            "1.5.7",
            "--provider",
            "vcpkg",
            "--url",
            "https://example.test/archive",
        ],
        vec![
            "add",
            "zstd",
            "--version",
            "1.5.7",
            "--provider",
            "conan",
            "--cmake-package",
            "zstd",
            "--target",
            "zstd::libzstd",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_woodshed"))
            .args(arguments)
            .arg("--manifest")
            .arg(&manifest)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!manifest.exists());
        assert!(!directory.path().join("woodshed.lock").exists());
    }
}
