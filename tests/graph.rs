#![cfg(all(feature = "cli", feature = "build"))]

use std::{fs, path::Path, process::Command};

fn checked(command: &mut Command) -> std::process::Output {
    let output = command.output().expect("start fixture command");
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
    command.current_dir(root).env("WOODSHED_OFFLINE", "1");
    command
}

fn write(root: &Path, path: &str, body: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

#[test]
fn relocatable_artifacts_export_and_import_without_rebuilding_sources() {
    let directory = tempfile::Builder::new().prefix("wp-").tempdir().unwrap();
    let root = directory.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\n[package]\nname='woodshed-portable-fixture'\nversion='0.0.0'\nedition='2024'\n",
    );
    write(
        root,
        "src/main.rs",
        "unsafe extern \"C\" { fn answer() -> i32; }\nfn main() { println!(\"{}\", unsafe { answer() }); }\n",
    );
    write(
        root,
        "woodshed.toml",
        "[dependencies.codec]\npath='native'\nbuild={cmake='Codec',targets=['Codec::codec']}\n",
    );
    write(root, "native/codec.c", "int answer(void) { return 42; }\n");
    write(root, "native/codec.h", "int answer(void);\n");
    write(
        root,
        "native/CMakeLists.txt",
        r#"
cmake_minimum_required(VERSION 3.21)
project(Codec LANGUAGES C)
add_library(codec STATIC codec.c)
target_include_directories(codec PUBLIC $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}> $<INSTALL_INTERFACE:include>)
install(TARGETS codec EXPORT Codec)
install(FILES codec.h DESTINATION include)
install(EXPORT Codec FILE CodecConfig.cmake NAMESPACE Codec:: DESTINATION lib/cmake/Codec)
"#,
    );
    checked(cli(root).arg("lock"));
    checked(
        cli(root)
            .args(["setup", "--woodshed-path"])
            .arg(env!("CARGO_MANIFEST_DIR")),
    );
    let cargo = || {
        let mut command = Command::new("cargo");
        command
            .current_dir(root)
            .args(["run", "--quiet"])
            .env("WOODSHED_CACHE", root.join("cache"))
            .env("WOODSHED_OFFLINE", "1")
            .env("CARGO_NET_OFFLINE", "true")
            .env(
                "CARGO_TARGET_DIR",
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("target")
                    .join("portable-fixture"),
            );
        command
    };
    assert_eq!(
        String::from_utf8_lossy(&checked(&mut cargo()).stdout).trim(),
        "42"
    );
    let cache = woodshed::Cache::at(root.join("cache"));
    let id = cache
        .explain_build(&root.join("woodshed.toml"), "codec")
        .unwrap()
        .unwrap()
        .artifact;
    let target = woodshed::rust_host().unwrap();
    let compatibility = woodshed::project::PrebuiltCompatibility {
        msvc_runtime: target
            .ends_with("-msvc")
            .then_some(woodshed::project::MsvcCrt::Dynamic),
        target,
        abi: "fixture-c-api-v1".into(),
        configuration: "Release".into(),
        linkage: woodshed::project::PackageLinkage::Static,
    };
    let metadata = woodshed::ArtifactMetadata {
        compatibility: compatibility.clone(),
        cmake: Some(woodshed::project::CmakePackage {
            package: "Codec".into(),
            targets: vec!["Codec::codec".into()],
            linkage: woodshed::project::PackageLinkage::Static,
        }),
        tools: Vec::new(),
    };
    cache
        .export_artifact(&id, &root.join("package"), &metadata, None)
        .unwrap();
    assert!(
        cache
            .export_artifact(&id, &root.join("package"), &metadata, None)
            .is_err()
    );
    fs::rename(root.join("package"), root.join("relocated")).unwrap();
    // The consumer must no longer have source files or their original installation.
    fs::rename(root.join("native"), root.join("source-not-used")).unwrap();
    fs::rename(
        root.join("cache").join("artifacts").join(&id),
        root.join("original-install-not-used"),
    )
    .unwrap();
    let mut source = woodshed::project::Source {
        path: Some("relocated".into()),
        prebuilt: Some(compatibility.clone()),
        ..Default::default()
    };
    let manifest = |source: &woodshed::project::Source| {
        toml::to_string(&woodshed::project::Declarations {
            dependencies: std::collections::BTreeMap::from([(
                "codec".into(),
                woodshed::project::Dependency::Source(Box::new(source.clone())),
            )]),
            ..Default::default()
        })
        .unwrap()
    };
    write(root, "woodshed.toml", &manifest(&source));
    checked(cli(root).arg("lock"));
    assert_eq!(
        String::from_utf8_lossy(&checked(&mut cargo()).stdout).trim(),
        "42"
    );
    let installed = cache.inspect_artifacts().unwrap();
    assert_eq!(installed.len(), 1);
    assert!(installed.iter().all(|artifact| artifact.verified));
    let imported = cache
        .explain_build(&root.join("woodshed.toml"), "codec")
        .unwrap()
        .unwrap();
    assert_eq!(imported.outcome, woodshed::BuildOutcome::Imported);
    assert!(imported.workspace.is_none());
    let imported_header = root
        .join("cache")
        .join("artifacts")
        .join(&imported.artifact)
        .join("prefix/include/codec.h");
    let original_header = fs::read(&imported_header).unwrap();
    let mut broken = original_header.clone();
    broken[0] ^= 1;
    fs::write(&imported_header, broken).unwrap();
    let build_script = fs::read_to_string(root.join("build.rs")).unwrap();
    write(root, "build.rs", &(build_script + "\n"));
    assert_eq!(
        String::from_utf8_lossy(&checked(&mut cargo()).stdout).trim(),
        "42"
    );
    assert_eq!(fs::read(&imported_header).unwrap(), original_header);
    assert_eq!(
        cache
            .explain_build(&root.join("woodshed.toml"), "codec")
            .unwrap()
            .unwrap()
            .outcome,
        woodshed::BuildOutcome::Repaired
    );
    source.prebuilt.as_mut().unwrap().abi = "different-abi".into();
    write(root, "woodshed.toml", &manifest(&source));
    checked(cli(root).arg("lock"));
    let wrong = cargo().output().unwrap();
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("compatibility metadata differs"));
    source.prebuilt = Some(compatibility);
    write(root, "woodshed.toml", &manifest(&source));
    checked(cli(root).arg("lock"));
    fs::write(root.join("relocated/prefix/include/codec.h"), "corrupt").unwrap();
    let corrupt = cargo().output().unwrap();
    assert!(!corrupt.status.success());
    assert!(String::from_utf8_lossy(&corrupt.stderr).contains("complete inventory"));
}

#[test]
fn local_graph_builds_host_tools_and_transitive_packages_with_cargo_features_offline() {
    let directory = tempfile::Builder::new().prefix("wg-").tempdir().unwrap();
    let root = directory.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\n[package]\nname='woodshed-graph-fixture'\nversion='0.0.0'\nedition='2024'\n",
    );
    write(
        root,
        "src/main.rs",
        "unsafe extern \"C\" { fn answer() -> i32; }\nfn main() { println!(\"{}\", unsafe { answer() }); }\n",
    );
    write(
        root,
        "woodshed.toml",
        r#"
[features]
default = ["fast"]
fast = ["dep:base", "app/fast"]
[dependencies.app]
path = "native/app"
depends = ["generator"]
optional-depends = ["base"]
build = { cmake = "App", targets = ["App::app"], features = { fast = { FAST = "ON" } }, workspace = { name = "app", reconfigure = ["FAST"], dependencies = ["base"] } }
[dependencies.base]
path = "native/base"
optional = true
build = { cmake = "Base", targets = ["Base::base"], workspace = { name = "base" } }
[dependencies.generator]
path = "native/generator"
role = "host"
optional = true
build = { tools = ["native_codegen"] }
[dependencies.unused]
path = "does-not-exist"
optional = true
build = { cmake = "Unused", targets = ["Unused::Unused"] }
[target.wasm32-unknown-unknown.dependencies.target_only]
path = "also-does-not-exist"
build = { cmake = "TargetOnly", targets = ["TargetOnly::TargetOnly"] }
"#,
    );
    write(
        root,
        "native/base/CMakeLists.txt",
        r#"
cmake_minimum_required(VERSION 3.21)
project(Base LANGUAGES C)
set(CMAKE_WINDOWS_EXPORT_ALL_SYMBOLS ON)
add_library(base base.c unchanged.c)
target_include_directories(base PUBLIC $<INSTALL_INTERFACE:include>)
install(TARGETS base EXPORT BaseTargets ARCHIVE DESTINATION lib LIBRARY DESTINATION lib RUNTIME DESTINATION bin)
install(FILES base.h DESTINATION include)
install(EXPORT BaseTargets NAMESPACE Base:: DESTINATION lib/cmake/Base)
install(FILES BaseConfig.cmake DESTINATION lib/cmake/Base)
"#,
    );
    write(
        root,
        "native/base/base.h",
        "#define API_BONUS 0\nint base(void);\n",
    );
    write(
        root,
        "native/base/unchanged.c",
        "int unchanged(void) { return 7; }\n",
    );
    write(
        root,
        "native/base/base.c",
        "int base(void) { return 40; }\n",
    );
    write(
        root,
        "native/base/BaseConfig.cmake",
        "include(\"${CMAKE_CURRENT_LIST_DIR}/BaseTargets.cmake\")\n",
    );
    write(
        root,
        "native/generator/CMakeLists.txt",
        r#"
cmake_minimum_required(VERSION 3.21)
project(Generator LANGUAGES C)
add_executable(native_codegen generator.c)
install(TARGETS native_codegen RUNTIME DESTINATION bin)
"#,
    );
    write(
        root,
        "native/generator/generator.c",
        r##"
#include <stdio.h>
int main(int argc, char **argv) {
    FILE *file;
    if (argc != 2 || !(file = fopen(argv[1], "w"))) return 1;
    fputs("#define GENERATED_VALUE 2\n", file);
    return fclose(file);
}
"##,
    );
    write(
        root,
        "native/app/CMakeLists.txt",
        r#"
cmake_minimum_required(VERSION 3.21)
project(App LANGUAGES C)
set(CMAKE_WINDOWS_EXPORT_ALL_SYMBOLS ON)
option(FAST "Feature selected from Cargo" OFF)
set(UPSTREAM_CACHE_STATE keep CACHE STRING "Upstream-owned state")
if(FAST)
  find_package(Base CONFIG REQUIRED)
endif()
find_program(GENERATOR native_codegen REQUIRED PATHS ${CMAKE_PROGRAM_PATH} NO_DEFAULT_PATH)
add_custom_command(OUTPUT "${CMAKE_CURRENT_BINARY_DIR}/generated.h"
  COMMAND "${GENERATOR}" "${CMAKE_CURRENT_BINARY_DIR}/generated.h"
  DEPENDS "${GENERATOR}")
add_library(app app.c "${CMAKE_CURRENT_BINARY_DIR}/generated.h")
target_include_directories(app PRIVATE "${CMAKE_CURRENT_BINARY_DIR}")
target_include_directories(app PUBLIC $<INSTALL_INTERFACE:include>)
target_compile_definitions(app PRIVATE FAST=$<BOOL:${FAST}>)
if(FAST)
  target_link_libraries(app PUBLIC Base::base)
endif()
install(TARGETS app EXPORT AppTargets ARCHIVE DESTINATION lib LIBRARY DESTINATION lib RUNTIME DESTINATION bin)
install(EXPORT AppTargets NAMESPACE App:: DESTINATION lib/cmake/App)
configure_file(AppConfig.cmake.in AppConfig.cmake @ONLY)
install(FILES "${CMAKE_CURRENT_BINARY_DIR}/AppConfig.cmake" DESTINATION lib/cmake/App)
configure_file(options.h.in options.h)
install(FILES "${CMAKE_CURRENT_BINARY_DIR}/options.h" "${CMAKE_CURRENT_BINARY_DIR}/generated.h" DESTINATION include)
"#,
    );
    write(root, "native/app/options.h.in", "#cmakedefine01 FAST\n");
    write(
        root,
        "native/app/app.c",
        "#include \"generated.h\"\n#if FAST\n#include \"base.h\"\nint answer(void) { return base() + GENERATED_VALUE + FAST + API_BONUS; }\n#else\nint answer(void) { return GENERATED_VALUE; }\n#endif\n",
    );
    write(
        root,
        "native/app/AppConfig.cmake.in",
        "include(CMakeFindDependencyMacro)\nif(@FAST@)\nfind_dependency(Base CONFIG REQUIRED)\nendif()\ninclude(\"${CMAKE_CURRENT_LIST_DIR}/AppTargets.cmake\")\n",
    );

    checked(cli(root).arg("lock"));
    let locked = fs::read(root.join("woodshed.lock")).unwrap();
    let plan = checked(cli(root).args(["tree", "--json"]));
    let plan: serde_json::Value = serde_json::from_slice(&plan.stdout).unwrap();
    let names = plan["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["base", "generator", "app"]);
    assert_eq!(plan["dependencies"][1]["role"], "host");
    let explanation = checked(cli(root).args(["explain", "app", "--json"]));
    let explanation: serde_json::Value = serde_json::from_slice(&explanation.stdout).unwrap();
    assert_eq!(explanation["dependencies"][0]["defines"]["FAST"], "ON");
    let inactive = cli(root).args(["explain", "unused"]).output().unwrap();
    assert!(!inactive.status.success());
    checked(cli(root).args(["setup", "--woodshed-path", env!("CARGO_MANIFEST_DIR")]));
    let cargo_toml = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(cargo_toml.contains("[features]"));
    let cargo = |extra: &[&str]| {
        let mut command = Command::new("cargo");
        command
            .current_dir(root)
            .args(["run", "--quiet"])
            .args(extra)
            .env("WOODSHED_LOG", root.join("progress.log"))
            .env("WOODSHED_CACHE", root.join("cache"))
            .env("WOODSHED_OFFLINE", "1")
            .env("CARGO_NET_OFFLINE", "true")
            .env(
                "CARGO_TARGET_DIR",
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("target")
                    .join("graph-fixture"),
            );
        command
    };
    let first = checked(&mut cargo(&[]));
    assert_eq!(String::from_utf8_lossy(&first.stdout).trim(), "43");
    let app_workspace = workspace_for(root, "app.c");
    let base_workspace = workspace_for(root, "base.c");
    let objects = entries(&base_workspace.join("build"));
    let unchanged = objects
        .iter()
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .contains("unchanged.c.")
                && matches!(
                    path.extension().and_then(|value| value.to_str()),
                    Some("obj" | "o")
                )
        })
        .unwrap()
        .clone();
    let object_time = fs::metadata(&unchanged).unwrap().modified().unwrap();
    let snapshots = artifact_bytes(root);
    let enabled_header = entries(&root.join("cache/artifacts"))
        .into_iter()
        .find(|path| path.ends_with("include/options.h"))
        .unwrap();
    assert!(
        fs::read_to_string(&enabled_header)
            .unwrap()
            .contains("#define FAST 1")
    );
    fs::rename(root.join("native/base"), root.join("native/base-disabled")).unwrap();
    let second = checked(&mut cargo(&["--no-default-features"]));
    assert_eq!(String::from_utf8_lossy(&second.stdout).trim(), "2");
    let report = woodshed::Cache::at(root.join("cache"))
        .explain_build(&root.join("woodshed.toml"), "app")
        .unwrap()
        .unwrap();
    assert_eq!(report.outcome, woodshed::BuildOutcome::Built);
    assert!(report.changed_inputs.contains(&"defines".into()));
    assert!(report.changed_inputs.contains(&"dependencies".into()));
    let progress = fs::read_to_string(root.join("progress.log")).unwrap();
    assert!(progress.contains("app: building/importing; changed inputs:"));
    assert!(progress.contains("defines"));
    assert!(progress.contains("dependencies"));
    assert_eq!(workspace_for(root, "app.c"), app_workspace);
    let configured = fs::read_to_string(app_workspace.join("build/CMakeCache.txt")).unwrap();
    assert!(configured.contains("FAST:BOOL=OFF"));
    assert!(!configured.contains("CMAKE_PREFIX_PATH:UNINITIALIZED="));
    assert!(!configured.contains("Base_DIR:"));
    assert!(configured.contains("UPSTREAM_CACHE_STATE:STRING=keep"));
    assert!(
        fs::read_to_string(&enabled_header)
            .unwrap()
            .contains("#define FAST 1")
    );
    assert!(
        entries(&root.join("cache/artifacts"))
            .iter()
            .filter(|path| path.ends_with("include/options.h"))
            .any(|path| fs::read_to_string(path).unwrap().contains("#define FAST 0"))
    );
    assert_unchanged(&snapshots);
    fs::rename(root.join("native/base-disabled"), root.join("native/base")).unwrap();
    let enabled = checked(&mut cargo(&["--no-default-features", "--features", "fast"]));
    assert_eq!(String::from_utf8_lossy(&enabled.stdout).trim(), "43");
    write(
        root,
        "native/base/base.c",
        "int base(void) { return 41; }\n",
    );
    write(
        root,
        "native/base/base.h",
        "#define API_BONUS 1\nint base(void);\n",
    );
    let changed = checked(&mut cargo(&[]));
    assert_eq!(String::from_utf8_lossy(&changed.stdout).trim(), "45");
    assert_eq!(workspace_for(root, "base.c"), base_workspace);
    assert_eq!(
        fs::metadata(&unchanged).unwrap().modified().unwrap(),
        object_time,
        "an unaffected native object must not be recompiled"
    );
    assert_unchanged(&snapshots);
    let cache = fs::read_to_string(app_workspace.join("build/CMakeCache.txt")).unwrap();
    let prefix = std::path::PathBuf::from(
        cache
            .lines()
            .find_map(|line| line.strip_prefix("CMAKE_INSTALL_PREFIX:PATH="))
            .unwrap(),
    );
    let header = prefix.join("include/options.h");
    let correct = fs::read(&header).unwrap();
    let modified = fs::metadata(&header).unwrap().modified().unwrap();
    let mut broken = correct.clone();
    broken[0] ^= 1;
    fs::write(&header, broken).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&header)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let build_script = fs::read_to_string(root.join("build.rs")).unwrap();
    write(root, "build.rs", &(build_script + "\n"));
    let repaired = checked(&mut cargo(&[]));
    assert_eq!(String::from_utf8_lossy(&repaired.stdout).trim(), "45");
    assert_eq!(fs::read(&header).unwrap(), correct);
    let report = woodshed::Cache::at(root.join("cache"))
        .explain_build(&root.join("woodshed.toml"), "app")
        .unwrap()
        .unwrap();
    assert_eq!(report.outcome, woodshed::BuildOutcome::Repaired);
    let progress = fs::read_to_string(root.join("progress.log")).unwrap();
    assert!(progress.contains(
        "app: building/importing; cached installation corrupt or interrupted; repairing"
    ));
    assert!(progress.contains("Reused; artifact"));
    assert!(report.changed_inputs.is_empty());
    assert_unchanged(&snapshots);
    assert_eq!(
        locked,
        fs::read(root.join("woodshed.lock")).unwrap(),
        "ordinary builds must not update remote selection"
    );
    let text = fs::read_to_string(root.join("woodshed.toml"))
        .unwrap()
        .replace(
            "cmake = \"Base\",",
            "cmake = \"Base\", linkage = \"dynamic\",",
        );
    fs::write(root.join("woodshed.toml"), text).unwrap();
    checked(cli(root).arg("lock"));
    let mixed = checked(&mut cargo(&[]));
    assert_eq!(String::from_utf8_lossy(&mixed.stdout).trim(), "45");
    let text = fs::read_to_string(root.join("woodshed.toml"))
        .unwrap()
        .replace(
            "cmake = \"App\",",
            "cmake = \"App\", linkage = \"dynamic\",",
        );
    fs::write(root.join("woodshed.toml"), text).unwrap();
    checked(cli(root).arg("lock"));
    let dynamic = checked(&mut cargo(&[]));
    assert_eq!(String::from_utf8_lossy(&dynamic.stdout).trim(), "45");
    let before_flags = fs::read_dir(root.join("cache/workspaces"))
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            path.join("source/app.c").is_file().then_some(path)
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        before_flags.len(),
        2,
        "static/shared builds need separate hard boundaries"
    );
    let mut flags = cargo(&[]);
    let existing = std::env::var("CFLAGS").unwrap_or_default();
    flags.env("CFLAGS", format!("{existing} -DWOODSHED_HARD_FLAG=1"));
    let isolated = checked(&mut flags);
    assert_eq!(String::from_utf8_lossy(&isolated.stdout).trim(), "45");
    let after_flags = fs::read_dir(root.join("cache/workspaces"))
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            path.join("source/app.c").is_file().then_some(path)
        })
        .collect::<std::collections::BTreeSet<_>>();
    let added = after_flags.difference(&before_flags).collect::<Vec<_>>();
    assert_eq!(added.len(), 1);
    assert!(
        fs::read_to_string(added[0].join("build/CMakeCache.txt"))
            .unwrap()
            .contains("-DWOODSHED_HARD_FLAG=1")
    );
    let receipts = fs::read_dir(root.join("cache/artifacts"))
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path().join("woodshed-installation.json");
            path.is_file().then(|| {
                serde_json::from_slice::<serde_json::Value>(&fs::read(path).unwrap()).unwrap()
            })
        })
        .collect::<Vec<_>>();
    assert!(
        receipts
            .iter()
            .any(|receipt| receipt["installation"]["executables"]["native_codegen"].is_string())
    );
    assert!(
        receipts.iter().any(|receipt| {
            let kinds = receipt["installation"]["library_linkage"]
                .as_object()
                .unwrap();
            kinds.values().any(|kind| kind == "static")
                && kinds.values().any(|kind| kind == "dynamic")
        }),
        "mixed static/shared dependency types must survive evaluated CMake metadata"
    );
    assert!(receipts.iter().all(|receipt| {
        assert_eq!(receipt["installation"]["ownership"], "artifact");
        assert!(
            receipt["input_fields"]["configured-tools"]
                .as_str()
                .is_some_and(|digest| digest.len() == 64)
        );
        receipt["files"]
            .as_object()
            .unwrap()
            .values()
            .all(|digest| digest.as_str().is_some_and(|digest| digest.len() == 64))
    }));
}

fn entries(root: &Path) -> Vec<std::path::PathBuf> {
    let mut result = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            result.extend(entries(&path));
        } else {
            result.push(path);
        }
    }
    result
}

fn workspace_for(root: &Path, marker: &str) -> std::path::PathBuf {
    let matches = fs::read_dir(root.join("cache/workspaces"))
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            path.join("source").join(marker).is_file().then_some(path)
        })
        .collect::<Vec<_>>();
    assert_eq!(matches.len(), 1);
    matches[0].clone()
}

fn artifact_bytes(root: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    entries(&root.join("cache/artifacts"))
        .into_iter()
        .filter(|path| {
            !path.components().any(|component| {
                component
                    .as_os_str()
                    .to_string_lossy()
                    .ends_with(".metadata")
            })
        })
        .map(|path| {
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect()
}

fn assert_unchanged(snapshot: &[(std::path::PathBuf, Vec<u8>)]) {
    for (path, bytes) in snapshot {
        assert_eq!(
            fs::read(path).unwrap(),
            *bytes,
            "immutable artifact changed: {}",
            path.display()
        );
    }
}

#[test]
fn cli_adds_local_sources_without_version_or_network_access() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    checked(cli(root).args([
        "add",
        "local",
        "--path",
        "native",
        "--cmake-package",
        "Local",
        "--target",
        "Local::Local",
    ]));
    let project = woodshed::project::Project::load(root.join("woodshed.toml")).unwrap();
    assert!(project.locked().unwrap().sources.is_empty());
    assert!(matches!(&project.declarations.dependencies["local"],
        woodshed::project::Dependency::Source(source) if source.path == Some("native".into()) && source.version.is_none()));
}
