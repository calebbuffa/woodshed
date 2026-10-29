use super::*;
use serde_json::json;

#[cfg(feature = "conan")]
#[test]
fn conan_preserves_package_prefixes_for_host_tools_and_transitive_consumers() {
    let graph = json!({"graph": {"nodes": {
        "0": {"context": "host", "package_folder": null},
        "1": {"context": "host", "package_folder": "packages/one",
            "cpp_info": {"root": {"bindirs": ["bin"], "libdirs": ["lib"]}}},
        "2": {"context": "build", "package_folder": "build-only"},
        "3": {"context": "host", "package_folder": "packages/two"}
    }}});
    let (prefixes, runtime) = conan_package_paths(&graph).unwrap();
    assert_eq!(
        prefixes,
        [PathBuf::from("packages/one"), PathBuf::from("packages/two")]
    );
    assert_eq!(
        runtime,
        [
            PathBuf::from("packages/one/bin"),
            PathBuf::from("packages/one/lib")
        ]
    );
    let mut invalid = graph;
    invalid["graph"]["nodes"]["1"]["cpp_info"]["root"]["bindirs"] = json!([42]);
    assert!(conan_package_paths(&invalid).is_err());
}

#[test]
fn evaluated_metadata_preserves_library_order_paths_definitions_and_compile_flags() {
    let data = json!({
        "compileGroups": [{"includes": [{"path": "C:\\SDK Path\\include"}],
            "defines": [{"define": "TEST_ABI=1"}], "compileCommandFragments": [{"fragment": "/MD /std:c++17"}]}],
        "link": {"commandFragments": [
            {"role": "flags", "fragment": "/machine:x64"},
            {"role": "libraries", "fragment": "\"C:\\SDK Path\\lib\\first.lib\" \"C:\\SDK Path\\lib\\second.lib\" kernel32.lib"},
            {"role": "flags", "fragment": "/NODEFAULTLIB:LIBCMT", "backtrace": 12}
        ]}
    });
    let installation = metadata(&data, PackageLinkage::Static, true).unwrap();
    assert_eq!(installation.definitions, ["TEST_ABI=1"]);
    assert_eq!(installation.compile_flags, ["/MD", "/std:c++17"]);
    assert_eq!(
        installation.link_arguments,
        [
            "C:\\SDK Path\\lib\\first.lib",
            "C:\\SDK Path\\lib\\second.lib",
            "kernel32.lib",
            "/NODEFAULTLIB:LIBCMT"
        ]
    );
    assert!(
        installation
            .cargo_directives()
            .unwrap_err()
            .to_string()
            .contains("unsupported")
    );
    let mut installation = installation;
    installation.link_arguments.pop();
    if cfg!(windows) {
        let directives = installation.cargo_directives().unwrap();
        assert!(directives[1].contains("first.lib"));
        assert!(directives[3].contains("second.lib"));
    }
}

#[test]
fn unsupported_linker_flags_fail_before_partial_cargo_emission() {
    let installation = Installation {
        cmake_packages: BTreeMap::new(),
        library_linkage: BTreeMap::new(),
        ownership: InstallationOwnership::External,
        prefixes: Vec::new(),
        executables: BTreeMap::new(),
        include_dirs: Vec::new(),
        definitions: Vec::new(),
        compile_flags: Vec::new(),
        link_arguments: vec!["-lfirst".into(), "-Wl,--whole-archive".into()],
        runtime_directories: Vec::new(),
        linkage: PackageLinkage::Static,
    };
    assert!(installation.cargo_directives().is_err());
    assert!(tokens("\"unterminated", false).is_err());
}

#[test]
fn generated_consumer_discovers_exported_transitive_targets_without_git_or_installs() {
    let directory = tempfile::tempdir().unwrap();
    let prefix = directory.path().join("package");
    std::fs::create_dir_all(prefix.join("include")).unwrap();
    let extension = if cfg!(windows) { "lib" } else { "a" };
    write(&prefix.join(format!("first.{extension}")), "").unwrap();
    write(&prefix.join(format!("second.{extension}")), "").unwrap();
    write(
        &prefix.join("WoodshedFixtureConfig.cmake"),
        &format!(
            r#"
add_library(WoodshedFixture::second STATIC IMPORTED)
set_target_properties(WoodshedFixture::second PROPERTIES
  IMPORTED_LOCATION "${{CMAKE_CURRENT_LIST_DIR}}/second.{extension}")
add_library(WoodshedFixture::first STATIC IMPORTED)
set_target_properties(WoodshedFixture::first PROPERTIES
  IMPORTED_LOCATION "${{CMAKE_CURRENT_LIST_DIR}}/first.{extension}"
  INTERFACE_INCLUDE_DIRECTORIES "${{CMAKE_CURRENT_LIST_DIR}}/include"
  INTERFACE_COMPILE_DEFINITIONS "WOOSHED_FIXTURE_ABI=3"
  INTERFACE_LINK_LIBRARIES WoodshedFixture::second)
"#
        ),
    )
    .unwrap();
    let options = InstallOptions {
        directory: directory.path().join("output"),
        target: if cfg!(windows) {
            "x86_64-pc-windows-msvc"
        } else {
            "x86_64-unknown-linux-gnu"
        }
        .into(),
        configuration: "Release".into(),
    };
    std::fs::create_dir_all(&options.directory).unwrap();
    let package = CmakePackage {
        package: "WoodshedFixture".into(),
        targets: vec!["WoodshedFixture::first".into()],
        linkage: PackageLinkage::Static,
    };
    let installation = discover(
        &package,
        &options,
        &[(
            "WOODSHED_PACKAGE_PREFIX".into(),
            prefix.clone().into_os_string(),
        )],
        "fixture-metadata",
    )
    .unwrap();
    assert_eq!(installation.include_dirs, [prefix.join("include")]);
    assert_eq!(installation.definitions, ["WOOSHED_FIXTURE_ABI=3"]);
    let libraries = installation
        .link_arguments
        .iter()
        .filter(|argument| argument.ends_with(&format!(".{extension}")))
        .collect::<Vec<_>>();
    assert!(libraries[0].ends_with(&format!("first.{extension}")));
    assert!(libraries[1].ends_with(&format!("second.{extension}")));
    installation.cargo_directives().unwrap();
}

#[cfg(feature = "vcpkg")]
#[test]
fn vcpkg_rejects_wrong_target_and_unsupported_architecture() {
    assert!(validate_triplet("x64-windows", "aarch64-pc-windows-msvc", BuildRole::Target).is_err());
    assert!(
        validate_triplet(
            "x64-linux",
            "riscv64gc-unknown-linux-gnu",
            BuildRole::Target
        )
        .is_err()
    );
    assert!(validate_triplet("x64-linux", "x86_64-unknown-linux-gnu", BuildRole::Target).is_ok());
    assert!(
        validate_triplet(
            "arm-linux",
            "armv7-unknown-linux-gnueabihf",
            BuildRole::Target
        )
        .is_ok()
    );
    assert!(validate_triplet("x64-windows", "x86_64-pc-windows-gnu", BuildRole::Target).is_err());
    assert!(validate_triplet("x64-linux", "x86_64-unknown-linux-musl", BuildRole::Target).is_err());
    assert!(
        validate_triplet(
            "x64-linux-custom",
            "x86_64-unknown-linux-gnu",
            BuildRole::Target
        )
        .is_err()
    );
    assert!(
        validate_triplet(
            "x64-windows-static",
            "x86_64-pc-windows-msvc",
            BuildRole::Host
        )
        .is_ok()
    );
    assert!(validate_triplet("x64-windows", "x86_64-pc-windows-msvc", BuildRole::Host).is_ok());
}

#[cfg(feature = "conan")]
#[test]
fn conan_rejects_missing_wrong_target_and_crt_profile_settings() {
    let graph = json!({"graph": {"nodes": {"0": {"settings": {"os": "Linux", "arch": "x86_64"}}}}});
    assert!(validate_conan_target(&graph, "x86_64-unknown-linux-gnu", BuildRole::Target).is_ok());
    assert!(validate_conan_target(&graph, "aarch64-unknown-linux-gnu", BuildRole::Target).is_err());
    assert!(
        validate_conan_target(&json!({}), "x86_64-unknown-linux-gnu", BuildRole::Target).is_err()
    );
    assert!(validate_conan_target(&graph, "x86_64-pc-windows-msvc", BuildRole::Target).is_err());
    let host = json!({"graph": {"nodes": {"0": {"settings": {"os": "Windows", "arch": "x86_64", "compiler.runtime": "static"}}}}});
    assert!(validate_conan_target(&host, "x86_64-pc-windows-msvc", BuildRole::Host).is_ok());
}

#[cfg(all(feature = "pin", feature = "vcpkg"))]
#[test]
#[ignore = "explicit live vcpkg/toolchain qualification; requires VCPKG_ROOT and native SDK"]
fn real_vcpkg_zstd_install_discovers_links_and_executes_rust_consumer_without_user_cmake() {
    let directory = tempfile::tempdir().unwrap();
    let manifest = directory.path().join("woodshed.toml");
    write(
        &manifest,
        r#"[dependencies.zstd]
provider = "vcpkg"
package = "zstd"
version = "1.5.7"
triplet = "x64-windows"
[dependencies.zstd.cmake]
package = "zstd"
targets = ["zstd::libzstd_shared"]
linkage = "dynamic"
"#,
    )
    .unwrap();
    crate::project::plan_lock(&manifest, &mut crate::pin::Online::default())
        .unwrap()
        .apply()
        .unwrap();
    let options = InstallOptions {
        directory: directory.path().join("provider"),
        target: "x86_64-pc-windows-msvc".into(),
        configuration: "Release".into(),
    };
    let installation = install(&manifest, "zstd", &options).unwrap();
    assert!(
        installation
            .include_dirs
            .iter()
            .any(|path| path.join("zstd.h").is_file())
    );
    assert!(!installation.runtime_directories.is_empty());
    let source = directory.path().join("consumer.rs");
    let binary = directory.path().join("consumer.exe");
    write(&source, "unsafe extern \"C\" { fn ZSTD_versionNumber() -> u32; }\nfn main() { assert!(unsafe { ZSTD_versionNumber() } >= 10507); }\n").unwrap();
    let mut command = Command::new("rustc");
    command.arg(&source).arg("-o").arg(&binary);
    for directive in installation.cargo_directives().unwrap() {
        if let Some(path) = directive.strip_prefix("cargo:rustc-link-search=") {
            command.arg("-L").arg(path);
        } else if let Some(library) = directive.strip_prefix("cargo:rustc-link-lib=") {
            command.arg("-l").arg(library);
        } else {
            panic!("unexpected directive {directive}");
        }
    }
    check(&mut command, "link real vcpkg Rust consumer").unwrap();
    let mut paths = installation.runtime_directories;
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    check(
        Command::new(binary).env("PATH", std::env::join_paths(paths).unwrap()),
        "execute real vcpkg Rust consumer",
    )
    .unwrap();
}
