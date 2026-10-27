use super::*;
use crate::pin::{Resolver, Selection};
use std::fs;

const COMMIT: &str = "0123456789012345678901234567890123456789";
const CHILD: &str = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";

#[derive(Default)]
struct FixtureResolver {
    calls: Vec<String>,
    fail: bool,
}
impl Resolver for FixtureResolver {
    fn resolve_ref(&mut self, repository: &str, reference: &str) -> Result<String> {
        self.calls.push(format!("{repository}:{reference}"));
        Ok(COMMIT.into())
    }
    fn file(&mut self, _: &str, _: &str, _: &str) -> Result<String> {
        Ok(CHILD.into())
    }
    fn gitlink(&mut self, _: &str, _: &str, _: &str) -> Result<GitSource> {
        Ok(GitSource {
            repository: "https://example.test/child.git".into(),
            revision: CHILD.into(),
        })
    }
    fn checksum(&mut self, dep: &Dep) -> Result<String> {
        self.calls.push(dep.url.clone());
        if self.fail {
            Err(invalid("fixture download failed"))
        } else {
            Ok(format!("{:x}", Sha256::digest(dep.url.as_bytes())))
        }
    }
}

const DECLARATIONS: &str = r#"
# User intent: no checksums, resolved commits, or child versions.
[dependencies.parent]
github = "example/parent"
version = "1.0"
ref = "v{version}"
markers = ["CMakeLists.txt"] # keep this inline note

[dependencies.child]
markers = ["include/child.h"]
[dependencies.child.resolve]
kind = "gitlink"
parent = "parent"
path = "third_party/child"
archive = "{repository}/archive/{revision}.tar.gz"
"#;

#[test]
fn declarative_source_recipes_reject_missing_targets_unsafe_options_and_unknown_fields() {
    let prefix = "[dependencies.demo]\ngithub = 'example/demo'\nversion = '1.0'\n";
    for build in [
        "build = { cmake = 'Demo', targets = ['Demo::core'] }",
        "build = { cmake = 'Demo', targets = ['Demo::core'], defines = { TESTS = 'OFF' }, toolchain = 'cross.cmake', inputs = ['sdk-version.txt'] }",
    ] {
        Project::parse("woodshed.toml", &format!("{prefix}{build}")).unwrap();
    }
    for build in [
        "build = { cmake = 'Demo', targets = [] }",
        "build = { cmake = 'Demo);evil(', targets = ['Demo::core'] }",
        "build = { cmake = 'Demo', targets = ['Demo::core'], defines = { CMAKE_INSTALL_PREFIX = '/tmp/other' } }",
        "build = { cmake = 'Demo', targets = ['Demo::core'], defines = { BUILD_SHARED_LIBS = 'ON' } }",
        "build = { cmake = 'Demo', targets = ['Demo::core'], toolchain = '' }",
        "build = { cmake = 'Demo', targets = ['Demo::core'], inputs = [''] }",
        "build = { cmake = 'Demo', target = ['Demo::core'] }",
    ] {
        assert!(
            Project::parse("woodshed.toml", &format!("{prefix}{build}")).is_err(),
            "{build}"
        );
    }
}

#[test]
fn readable_cmake_rules_validate_selectors_before_resolution() {
    let prefix = r#"
[dependencies.parent]
github = "example/parent"
version = "1.0"
[dependencies.child.resolve]
kind = "file"
parent = "parent"
url = { path = "deps.cmake", cmake-url = "child" }
"#;
    for selector in ["cmake-variable = 'REV'", "pattern = '([a-f0-9]+)'"] {
        Project::parse(
            "woodshed.toml",
            &format!("{prefix}\nrevision = {{ path = 'deps.cmake', {selector} }}"),
        )
        .unwrap();
    }
    for selector in [
        "",
        "cmake-variable = ''",
        "cmake-variable = '${REV}'",
        "cmake-variable = 'REV', pattern = '(.*)'",
        "cmake-url = 'child', cmake-variable = 'REV'",
        "cmake-varible = 'REV'",
    ] {
        assert!(
            Project::parse(
                "woodshed.toml",
                &format!("{prefix}\nrevision = {{ path = 'deps.cmake', {selector} }}")
            )
            .is_err(),
            "{selector}"
        );
    }
}

#[test]
fn named_cmake_rules_infer_revision_placeholder_and_fail_before_writing() {
    struct CmakeResolver {
        fixture: FixtureResolver,
        extra: &'static str,
        literal: bool,
    }
    impl Resolver for CmakeResolver {
        fn resolve_ref(&mut self, repository: &str, reference: &str) -> Result<String> {
            self.fixture.resolve_ref(repository, reference)
        }
        fn file(&mut self, _: &str, _: &str, _: &str) -> Result<String> {
            let revision = if self.literal {
                CHILD.to_owned()
            } else {
                format!("${{REV}}{}", self.extra)
            };
            Ok(format!(
                "set(REV \"{CHILD}\")\nFetchContent_Declare(child URL https://example.test/child/{revision}.zip)"
            ))
        }
        fn gitlink(&mut self, repository: &str, revision: &str, path: &str) -> Result<GitSource> {
            self.fixture.gitlink(repository, revision, path)
        }
        fn checksum(&mut self, dep: &Dep) -> Result<String> {
            self.fixture.checksum(dep)
        }
    }
    let (_directory, path) = create();
    let text = r#"
[dependencies.parent]
github = "example/parent"
version = "1.0"
[dependencies.child.resolve]
kind = "file"
parent = "parent"
revision = { path = "deps.cmake", cmake-variable = "REV" }
url = { path = "deps.cmake", cmake-url = "child" }
"#;
    fs::write(&path, text).unwrap();
    let mut resolver = CmakeResolver {
        fixture: FixtureResolver::default(),
        extra: "",
        literal: false,
    };
    plan_lock(&path, &mut resolver).unwrap().apply().unwrap();
    let manifest = Manifest::load(&path).unwrap();
    assert_eq!(manifest.get("child").unwrap().version, CHILD);
    assert_eq!(
        manifest.get("child").unwrap().url,
        format!("https://example.test/child/{CHILD}.zip")
    );
    let lock = fs::read_to_string(path.with_file_name("woodshed.lock")).unwrap();
    resolver.literal = true;
    let direct = plan_lock(&path, &mut resolver).unwrap();
    let direct: Lock = toml::from_str(direct.lock()).unwrap();
    assert_eq!(
        direct.sources["child"].url,
        manifest.get("child").unwrap().url
    );
    resolver.literal = false;
    for extra in ["${OTHER}", "$ENV{OTHER}", "$<CONFIG>"] {
        resolver.extra = extra;
        assert!(plan_lock(&path, &mut resolver).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        assert_eq!(
            fs::read_to_string(path.with_file_name("woodshed.lock")).unwrap(),
            lock
        );
    }
}

fn create() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("woodshed.toml");
    fs::write(&path, DECLARATIONS).unwrap();
    (directory, path)
}

#[test]
fn lock_resolves_release_and_children_without_maintainer_hashes_or_writes_until_apply() {
    let (_directory, path) = create();
    let mut resolver = FixtureResolver::default();
    let plan = plan_lock(&path, &mut resolver).unwrap();
    assert_eq!(plan.changes().len(), 2);
    assert_eq!(fs::read_to_string(&path).unwrap(), DECLARATIONS);
    assert!(!path.with_file_name("woodshed.lock").exists());
    assert_eq!(plan.manifest(), DECLARATIONS);
    plan.apply().unwrap();
    let pins = Manifest::load(&path).unwrap();
    assert_eq!(pins.get("parent").unwrap().version, "1.0");
    assert_eq!(pins.get("child").unwrap().version, CHILD);
    assert_eq!(
        pins.get("parent")
            .unwrap()
            .upstream
            .as_ref()
            .unwrap()
            .revision,
        COMMIT
    );
    assert!(
        resolver
            .calls
            .contains(&"https://github.com/example/parent.git:v1.0".into())
    );
    let lock = fs::read_to_string(path.with_file_name("woodshed.lock")).unwrap();
    assert_eq!(
        Project::parse_locked_sources(DECLARATIONS, &lock)
            .unwrap()
            .iter()
            .count(),
        2
    );
    assert_eq!(
        plan_lock(&path, &mut FixtureResolver::default())
            .unwrap()
            .lock(),
        lock
    );
}

#[test]
fn bump_updates_intent_and_locked_graph_and_preserves_comments() {
    let (_directory, path) = create();
    plan_lock(&path, &mut FixtureResolver::default())
        .unwrap()
        .apply()
        .unwrap();
    let update = plan_bump(
        &path,
        "parent",
        "2.0",
        &Selection::default(),
        &mut FixtureResolver::default(),
    )
    .unwrap();
    assert_eq!(update.changes().len(), 2);
    assert!(update.manifest().contains("# User intent"));
    assert!(update.manifest().contains("# keep this inline note"));
    assert!(!update.manifest().contains("sha256"));
    update.apply().unwrap();
    assert_eq!(
        Manifest::load(&path)
            .unwrap()
            .get("parent")
            .unwrap()
            .version,
        "2.0"
    );
    assert!(
        plan_bump(
            &path,
            "child",
            "3",
            &Selection::default(),
            &mut FixtureResolver::default()
        )
        .is_err()
    );
}

#[test]
fn resolution_failure_and_stale_plans_preserve_both_files() {
    let (_directory, path) = create();
    plan_lock(&path, &mut FixtureResolver::default())
        .unwrap()
        .apply()
        .unwrap();
    let lock_path = path.with_file_name("woodshed.lock");
    let before = fs::read(&path).unwrap();
    let lock_before = fs::read(&lock_path).unwrap();
    assert!(
        plan_bump(
            &path,
            "parent",
            "2",
            &Selection::default(),
            &mut FixtureResolver {
                fail: true,
                ..Default::default()
            }
        )
        .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read(&lock_path).unwrap(), lock_before);
    let update = plan_bump(
        &path,
        "parent",
        "2",
        &Selection::default(),
        &mut FixtureResolver::default(),
    )
    .unwrap();
    fs::write(&lock_path, "concurrent lock edit").unwrap();
    assert!(
        update
            .apply()
            .unwrap_err()
            .to_string()
            .contains("changed since planning")
    );
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn missing_stale_and_interrupted_locks_fail_closed_but_comments_do_not_invalidate() {
    let (_directory, path) = create();
    assert!(Manifest::load(&path).is_err());
    plan_lock(&path, &mut FixtureResolver::default())
        .unwrap()
        .apply()
        .unwrap();
    fs::write(&path, format!("# extra comment\n{DECLARATIONS}")).unwrap();
    Manifest::load(&path).unwrap();
    fs::write(&path, DECLARATIONS.replace("1.0", "2.0")).unwrap();
    assert!(
        Manifest::load(&path)
            .unwrap_err()
            .to_string()
            .contains("does not match")
    );
}

#[test]
fn add_can_create_a_project_and_switch_archive_git_archive_without_duplicate_pins() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("new").join("woodshed.toml");
    let source = Source {
        version: Some("1".into()),
        url: Some("https://example.test/{version}.tar.gz".into()),
        ..Default::default()
    };
    let update = plan_add(
        &path,
        "example",
        Dependency::Source(source),
        &mut FixtureResolver::default(),
    )
    .unwrap();
    assert!(!path.parent().unwrap().exists());
    update.apply().unwrap();
    plan_bump(
        &path,
        "example",
        "2",
        &Selection {
            git: Some("https://example.test/source.git".into()),
            reference: Some("main".into()),
            ..Default::default()
        },
        &mut FixtureResolver::default(),
    )
    .unwrap()
    .apply()
    .unwrap();
    assert!(
        Manifest::load(&path)
            .unwrap()
            .get("example")
            .unwrap()
            .git
            .is_some()
    );
    plan_bump(
        &path,
        "example",
        "3",
        &Selection {
            url: Some("https://example.test/{version}.tar.gz".into()),
            ..Default::default()
        },
        &mut FixtureResolver::default(),
    )
    .unwrap()
    .apply()
    .unwrap();
    assert!(
        Manifest::load(&path)
            .unwrap()
            .get("example")
            .unwrap()
            .git
            .is_none()
    );
}

#[test]
fn git_branch_requires_no_fake_version_and_resolves_only_during_locking() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("woodshed.toml");
    fs::write(
        &path,
        r#"[dependencies.experimental]
git = "https://example.test/source.git"
branch = "main"
"#,
    )
    .unwrap();
    let mut resolver = FixtureResolver::default();
    plan_lock(&path, &mut resolver).unwrap().apply().unwrap();
    assert!(
        resolver
            .calls
            .contains(&"https://example.test/source.git:refs/heads/main".into())
    );
    Manifest::load(&path).unwrap();
}

#[test]
fn rejects_ambiguous_sources_cycles_unknown_fields_and_nonimmutable_revisions() {
    for text in [
        r#"[dependencies.x]
version="1"
git="test"
url="test"
"#,
        r#"[dependencies.x]
version="1"
github="owner/repo"
rev="main"
"#,
        r#"[dependencies.x]
version="1"
github="owner/repo"
sha256="not-user-input"
"#,
        r#"[dependencies.x.resolve]
kind="gitlink"
parent="x"
path="nested"
"#,
    ] {
        assert!(Project::parse("woodshed.toml", text).is_err(), "{text}");
    }
}

#[test]
fn file_managed_sources_can_declare_a_stable_archive_template_without_dummy_pins() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("woodshed.toml");
    fs::write(
        &path,
        r#"[dependencies.parent]
    github = "example/parent"
    version = "1"
    [dependencies.child]
    url = "https://example.test/{version}.zip"
    [dependencies.child.resolve]
    kind = "file"
    parent = "parent"
    [dependencies.child.resolve.revision]
    path = "revision.txt"
    pattern = "([0-9a-f]{40})"
    "#,
    )
    .unwrap();
    plan_lock(&path, &mut FixtureResolver::default())
        .unwrap()
        .apply()
        .unwrap();
    assert_eq!(
        Manifest::load(&path).unwrap().get("child").unwrap().url,
        format!("https://example.test/{CHILD}.zip")
    );
}

#[test]
fn provider_profiles_are_hashed_and_missing_features_never_fall_back() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("woodshed.toml");
    let profile = directory.path().join("host");
    fs::write(&profile, "[settings]\nos=Windows\narch=x86_64\n").unwrap();
    let package = Package::Conan {
        package: "zlib/1.3.1".into(),
        host_profile: "host".into(),
        build_profile: "host".into(),
        options: Vec::new(),
        build_missing: false,
        cmake: CmakePackage {
            package: "ZLIB".into(),
            targets: vec!["ZLIB::ZLIB".into()],
            linkage: PackageLinkage::Static,
        },
    };
    let first = package.input(&path).unwrap();
    fs::write(&profile, "[settings]\nos=Linux\narch=x86_64\n").unwrap();
    assert_ne!(first, package.input(&path).unwrap());
    fs::write(&profile, "include(default)").unwrap();
    assert!(package.input(&path).is_err());
    assert!(
        FixtureResolver::default()
            .package(&path, &package)
            .unwrap_err()
            .to_string()
            .contains("unavailable")
    );
}

#[cfg(windows)]
#[test]
fn failed_second_file_replacement_rolls_back_user_declarations() {
    use std::os::windows::fs::OpenOptionsExt;
    let (_directory, path) = create();
    plan_lock(&path, &mut FixtureResolver::default())
        .unwrap()
        .apply()
        .unwrap();
    let lock_path = path.with_file_name("woodshed.lock");
    let before = fs::read(&path).unwrap();
    let lock_before = fs::read(&lock_path).unwrap();
    let update = plan_bump(
        &path,
        "parent",
        "2",
        &Selection::default(),
        &mut FixtureResolver::default(),
    )
    .unwrap();
    let _held = fs::OpenOptions::new()
        .read(true)
        .share_mode(3)
        .open(&lock_path)
        .unwrap();
    assert!(update.apply().is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read(&lock_path).unwrap(), lock_before);
}

#[test]
fn migration_preserves_exact_source_identities_and_build_keys_without_network() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("versions.toml");
    let text = format!(
        r#"[parent]
version = "1.0"
url = "https://github.com/example/parent/archive/{{revision}}.tar.gz"
sha256 = "{}"
file = "parent-{{version}}.tar.gz"
[parent.upstream]
repository = "https://github.com/example/parent.git"
revision = "{COMMIT}"
reference = "v{{version}}"
"#,
        "c".repeat(64)
    );
    fs::write(&path, &text).unwrap();
    let pins = Manifest::load(&path).unwrap();
    let before = crate::BuildKey::new("test")
        .dep(pins.get("parent").unwrap())
        .as_str()
        .to_owned();
    let update = plan_migrate(&path).unwrap();
    assert!(!path.with_file_name("woodshed.toml").exists());
    update.apply().unwrap();
    let migrated = Manifest::load(path.with_file_name("woodshed.toml")).unwrap();
    let after = crate::BuildKey::new("test")
        .dep(migrated.get("parent").unwrap())
        .as_str()
        .to_owned();
    assert_eq!(before, after);
    assert_eq!(
        pins.get("parent").unwrap().url,
        migrated.get("parent").unwrap().url
    );
    assert_eq!(
        pins.get("parent").unwrap().file,
        migrated.get("parent").unwrap().file
    );
    assert_eq!(fs::read_to_string(path).unwrap(), text);
}
