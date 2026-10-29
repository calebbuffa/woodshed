#![cfg(all(feature = "cli", feature = "build"))]

use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

struct Server {
    url: String,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn new(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/fixture.tar.gz", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                            .unwrap();
                        let mut buffer = [0; 4096];
                        let mut request = Vec::new();
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
                        stream.write_all(&body).unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    Err(error) => panic!("{error}"),
                }
            }
        });
        Self {
            url,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn checked(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn cli(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_woodshed"));
    command.current_dir(root);
    command
}

fn archive() -> Vec<u8> {
    let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    for (name, body) in [
        (
            "CMakeLists.txt",
            r#"
cmake_minimum_required(VERSION 3.21)
project(WoodshedDemo LANGUAGES CXX)
set(DEMO_VALUE 41 CACHE STRING "Test output")
file(APPEND "${CMAKE_BINARY_DIR}/configure-count.txt" "configure\n")
add_library(second STATIC second.cpp)
target_compile_definitions(second PRIVATE DEMO_VALUE=${DEMO_VALUE})
add_library(first STATIC first.cpp)
target_link_libraries(first PUBLIC second)
target_include_directories(first PUBLIC $<INSTALL_INTERFACE:include>)
install(TARGETS first second EXPORT DemoTargets ARCHIVE DESTINATION lib)
install(FILES demo.h DESTINATION include)
install(EXPORT DemoTargets NAMESPACE Demo:: DESTINATION lib/cmake/WoodshedDemo)
install(FILES WoodshedDemoConfig.cmake DESTINATION lib/cmake/WoodshedDemo)
"#,
        ),
        (
            "first.cpp",
            "int second();\nextern \"C\" int demo_value() { return second(); }\n",
        ),
        (
            "second.cpp",
            "#include <string>\nint second() { return std::stoi(std::to_string(DEMO_VALUE)); }\n",
        ),
        ("demo.h", "int demo_value(void);\n"),
        (
            "WoodshedDemoConfig.cmake",
            "include(\"${CMAKE_CURRENT_LIST_DIR}/DemoTargets.cmake\")\n",
        ),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, format!("fixture/{name}"), body.as_bytes())
            .unwrap();
    }
    archive.into_inner().unwrap().finish().unwrap()
}

fn write_empty_lock(root: &Path) {
    let declarations = woodshed::project::Declarations::default();
    fs::write(root.join("woodshed.toml"), "").unwrap();
    let lock = woodshed::project::Lock {
        format: 1,
        input: declarations.digest().unwrap(),
        sources: Default::default(),
        packages: Default::default(),
    };
    fs::write(root.join("woodshed.lock"), toml::to_string(&lock).unwrap()).unwrap();
}

#[test]
fn setup_is_previewable_idempotent_stale_guarded_and_preserves_existing_scripts() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let cargo = root.join("Cargo.toml");
    let original = "# preserve me\n[package]\nname='consumer'\nversion='0.1.0'\nedition='2024'\n";
    fs::write(&cargo, original).unwrap();
    write_empty_lock(root);
    let preview = checked(cli(root).args(["setup", "--dry-run"]));
    assert!(String::from_utf8_lossy(&preview.stdout).contains("woodshed::build()"));
    assert_eq!(fs::read_to_string(&cargo).unwrap(), original);
    assert!(!root.join("build.rs").exists());
    let plan = woodshed::setup::plan(&cargo, None).unwrap();
    fs::write(&cargo, format!("{original}\n# concurrent edit\n")).unwrap();
    assert!(plan.apply().is_err());
    fs::write(&cargo, original).unwrap();
    checked(cli(root).arg("setup"));
    let configured = fs::read_to_string(&cargo).unwrap();
    checked(cli(root).arg("setup"));
    assert_eq!(configured, fs::read_to_string(&cargo).unwrap());
    assert!(configured.contains("# preserve me"));
    fs::write(root.join("build.rs"), "fn main() { /* custom */ }\n").unwrap();
    let failed = cli(root).arg("setup").output().unwrap();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("existing build.rs"));
    assert_eq!(configured, fs::read_to_string(&cargo).unwrap());
    assert_eq!(
        fs::read_to_string(root.join("build.rs")).unwrap(),
        "fn main() { /* custom */ }\n"
    );
    fs::remove_file(root.join("build.rs")).unwrap();
    fs::write(
        &cargo,
        format!("{original}\n[build-dependencies]\nwoodshed = {{ workspace = true }}\n"),
    )
    .unwrap();
    assert!(woodshed::setup::plan(&cargo, None).is_err());
    for dependency in [
        "woodshed = '0.2'",
        "woodshed = { version = '0.3', optional = true }",
        "woodshed = { package = 'something-else', version = '0.3' }",
        "woodshed = { git = 'https://example.test/woodshed' }",
    ] {
        fs::write(
            &cargo,
            format!("{original}\n[build-dependencies]\n{dependency}\n"),
        )
        .unwrap();
        assert!(woodshed::setup::plan(&cargo, None).is_err(), "{dependency}");
        assert!(!root.join("build.rs").exists());
    }
    fs::write(&cargo, format!("{original}\n[build-dependencies]\nwoodshed = {{ version = '0.3', features = ['cmake'], default-features = false }}\n")).unwrap();
    let plan = woodshed::setup::plan(&cargo, None).unwrap();
    assert!(plan.cargo_toml().contains("default-features = false"));
    assert!(plan.cargo_toml().contains("\"cmake\""));
    assert!(plan.cargo_toml().contains("\"build\""));
    fs::write(root.join("woodshed.toml"), "# changed declarations\n").unwrap();
    assert!(plan.apply().is_err());
    assert!(!root.join("build.rs").exists());
}

#[test]
fn ordinary_downstream_build_links_runs_reuses_repairs_and_changes_option_identity() {
    let server = Server::new(archive());
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("Cargo.toml"), "[workspace]\n[package]\nname='woodshed-declarative-consumer'\nversion='0.1.0'\nedition='2024'\n").unwrap();
    fs::write(root.join("src/main.rs"), "unsafe extern \"C\" { fn demo_value() -> i32; }\nfn main() { println!(\"{}\", unsafe { demo_value() }); }\n").unwrap();
    checked(cli(root).args([
        "add",
        "demo",
        "--version",
        "1.0",
        "--url",
        &server.url,
        "--cmake-package",
        "WoodshedDemo",
        "--target",
        "Demo::first",
        "--define",
        "DEMO_VALUE=41",
    ]));
    let before_cargo = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(!root.join("build.rs").exists());
    checked(cli(root).args([
        "setup",
        "--dry-run",
        "--woodshed-path",
        env!("CARGO_MANIFEST_DIR"),
    ]));
    assert_eq!(
        before_cargo,
        fs::read_to_string(root.join("Cargo.toml")).unwrap()
    );
    assert!(!root.join("build.rs").exists());
    checked(cli(root).args(["setup", "--woodshed-path", env!("CARGO_MANIFEST_DIR")]));
    let cache = root.join("cache");
    let cargo = |subcommand: &str| {
        let mut command = Command::new("cargo");
        command
            .current_dir(root)
            .arg(subcommand)
            .arg("--quiet")
            .env("WOODSHED_CACHE", &cache)
            .env("CARGO_NET_OFFLINE", "true")
            .env("CARGO_INCREMENTAL", "0")
            .env("CARGO_TARGET_DIR", root.join("target"));
        command
    };
    let first = checked(&mut cargo("run"));
    assert_eq!(String::from_utf8_lossy(&first.stdout).trim(), "41");
    let builds = || {
        fs::read_dir(cache.join("artifacts"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir() && path.join("woodshed-installation.json").is_file())
            .collect::<Vec<_>>()
    };
    let initial = builds();
    assert_eq!(initial.len(), 1);
    let workspace = fs::read_dir(cache.join("workspaces"))
        .unwrap()
        .map(|entry| entry.unwrap().path().join("build/configure-count.txt"))
        .find(|path| path.is_file())
        .unwrap();
    let count = fs::read(&workspace).unwrap();
    // Force Cargo to invoke the bridge again, not just reuse its own output.
    let build_script = fs::read_to_string(root.join("build.rs")).unwrap();
    fs::write(root.join("build.rs"), format!("{build_script}\n")).unwrap();
    let second = checked(&mut cargo("run"));
    assert_eq!(String::from_utf8_lossy(&second.stdout).trim(), "41");
    assert_eq!(fs::read(&workspace).unwrap(), count);
    fs::remove_file(initial[0].join("prefix/include/demo.h")).unwrap();
    fs::write(root.join("build.rs"), format!("{build_script}\n\n")).unwrap();
    let repaired = checked(&mut cargo("run"));
    assert_eq!(String::from_utf8_lossy(&repaired.stdout).trim(), "41");
    assert!(initial[0].join("prefix/include/demo.h").is_file());
    let manifest = fs::read_to_string(root.join("woodshed.toml")).unwrap();
    fs::write(
        root.join("woodshed.toml"),
        manifest.replace("\"41\"", "\"42\""),
    )
    .unwrap();
    let stale = cargo("build").output().unwrap();
    assert!(!stale.status.success());
    checked(cli(root).arg("lock"));
    let changed = checked(&mut cargo("run"));
    assert_eq!(String::from_utf8_lossy(&changed.stdout).trim(), "42");
    assert_eq!(builds().len(), 2);
}
