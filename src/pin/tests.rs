use super::*;

#[test]
fn archive_git_archive_roundtrip_retains_dependent_resolution() {
    let (_directory, path) = manifest();
    let selection = Selection {
        git: Some("https://example.test/root.git".into()),
        reference: Some("main".into()),
        ..Selection::default()
    };
    plan_bump(&path, "root", "2.0.0", &selection, &mut Fixture::default())
        .unwrap()
        .apply()
        .unwrap();
    let selection = Selection {
        url: Some("https://example.test/root/{revision}.tar.gz".into()),
        reference: Some("main".into()),
        ..Selection::default()
    };
    let plan = plan_bump(&path, "root", "3.0.0", &selection, &mut Fixture::default()).unwrap();
    assert_eq!(plan.changes().len(), 3);
    plan.apply().unwrap();
    let pins = Manifest::load(path).unwrap();
    let root = pins.get("root").unwrap();
    assert!(root.git.is_none());
    assert_eq!(root.upstream.as_ref().unwrap().revision, NEW);
    assert_eq!(root.url, format!("https://example.test/root/{NEW}.tar.gz"));
    assert_eq!(pins.get("leaf").unwrap().version, LEAF);
}

#[test]
fn first_add_creates_manifest_only_after_success_and_dry_run_creates_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native").join("versions.toml");
    let selection = Selection {
        url: Some("https://example.test/{version}.zip".into()),
        ..Selection::default()
    };
    let plan = plan_add(&path, "new", "1", &selection, &[], &mut Fixture::default()).unwrap();
    assert!(!path.parent().unwrap().exists());
    plan.apply().unwrap();
    assert_eq!(
        Manifest::load(&path).unwrap().get("new").unwrap().version,
        "1"
    );
}

#[test]
fn concurrent_woodshed_commits_do_not_overwrite_a_stale_plan() {
    let (_directory, path) = manifest();
    let first = plan_bump(
        &path,
        "root",
        "2.0.0",
        &Selection::default(),
        &mut Fixture::default(),
    )
    .unwrap();
    let second = plan_bump(
        &path,
        "root",
        "2.0.0",
        &Selection::default(),
        &mut Fixture::default(),
    )
    .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let run = |plan: Plan, barrier: std::sync::Arc<std::sync::Barrier>| {
        std::thread::spawn(move || {
            barrier.wait();
            plan.apply().is_ok()
        })
    };
    let a = run(first, barrier.clone());
    let b = run(second, barrier);
    assert_ne!(a.join().unwrap(), b.join().unwrap());
    assert_eq!(
        Manifest::load(&path).unwrap().get("root").unwrap().version,
        "2.0.0"
    );
}

#[test]
fn offline_resolver_rejects_missing_ambiguous_and_escaping_fixtures() {
    let directory = tempfile::tempdir().unwrap();
    let metadata = directory.path().join("metadata.json");
    std::fs::write(&metadata, "{}").unwrap();
    let mut resolver = Offline::load(&metadata).unwrap();
    assert!(resolver.resolve_ref("repo", "main").is_err());
    assert!(resolver.gitlink("repo", NEW, "child").is_err());
    assert!(resolver.file("repo", NEW, "file").is_err());
    assert!(
        resolver
            .checksum(&Dep::new("missing", "1", "url", "hash"))
            .is_err()
    );
    let item = format!(r#"{{"repository":"repo","reference":"main","revision":"{NEW}"}}"#);
    std::fs::write(&metadata, format!(r#"{{"refs":[{item},{item}]}}"#)).unwrap();
    assert!(
        Offline::load(&metadata)
            .unwrap()
            .resolve_ref("repo", "main")
            .is_err()
    );
    std::fs::write(
        &metadata,
        r#"{"archives":[{"name":"escape","file":"../outside"}]}"#,
    )
    .unwrap();
    assert!(
        Offline::load(&metadata)
            .unwrap()
            .checksum(&Dep::new("escape", "1", "url", "hash"))
            .is_err()
    );
}

const OLD: &str = "1111111111111111111111111111111111111111";
const NEW: &str = "2222222222222222222222222222222222222222";
const CHILD: &str = "3333333333333333333333333333333333333333";
const LEAF: &str = "4444444444444444444444444444444444444444";
const MANIFEST: &str = r#"# preserved comment
[root]
version = "1.0.0"
url = "https://example.test/root/{revision}.tar.gz"
sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
file = "root-{version}.tar.gz"
markers = ["CMakeLists.txt"]
[root.upstream]
repository = "https://example.test/root.git"
revision = "1111111111111111111111111111111111111111"
reference = "v{version}"

[child]
version = "3333333333333333333333333333333333333333"
url = "https://example.test/child/{version}.tar.gz"
sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
file = "child-old-short.tar.gz"
[child.resolve]
kind = "gitlink"
parent = "root"
path = "vendor/child"
archive = "{repository}/archive/{revision}.tar.gz"

[leaf]
version = "4444444444444444444444444444444444444444"
url = "https://example.test/leaf/{version}.zip"
sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[leaf.resolve]
kind = "file"
parent = "child"
placeholder = "${LEAF_REV}"
[leaf.resolve.revision]
path = "pins.txt"
pattern = 'revision = "([0-9a-f]{40})"'
[leaf.resolve.url]
path = "download.txt"
pattern = 'url = "(https://[^"]+)"'

[independent]
version = "1"
url = "https://example.test/{version}.zip"
sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#;

#[derive(Default)]
struct Fixture {
    checksums: Vec<String>,
    fail_checksum: bool,
    ambiguous: bool,
    bad_gitlink: bool,
}

impl Resolver for Fixture {
    fn resolve_ref(&mut self, repository: &str, reference: &str) -> Result<String> {
        assert_eq!(repository, "https://example.test/root.git");
        assert!(["v2.0.0", "main"].contains(&reference));
        Ok(NEW.into())
    }

    fn file(&mut self, repository: &str, revision: &str, path: &str) -> Result<String> {
        assert_eq!(repository, "https://example.test/child.git");
        assert_eq!(revision, CHILD);
        match path {
            "pins.txt" => {
                let text = format!("revision = \"{LEAF}\"");
                Ok(if self.ambiguous {
                    format!("{text}\n{text}")
                } else {
                    text
                })
            }
            "download.txt" => {
                Ok("url = \"https://example.test/leaf/archive/${LEAF_REV}.zip\"".into())
            }
            _ => Err(invalid("missing fixture file")),
        }
    }

    fn gitlink(&mut self, repository: &str, revision: &str, path: &str) -> Result<GitSource> {
        assert_eq!(repository, "https://example.test/root.git");
        assert_eq!(revision, NEW);
        assert_eq!(path, "vendor/child");
        Ok(GitSource {
            repository: "https://example.test/child.git".into(),
            revision: if self.bad_gitlink {
                "main".into()
            } else {
                CHILD.into()
            },
        })
    }

    fn checksum(&mut self, dep: &Dep) -> Result<String> {
        self.checksums.push(dep.name.clone());
        if self.fail_checksum && dep.name == "root" {
            return Err(invalid("fixture archive missing"));
        }
        Ok("b".repeat(64))
    }
}

fn manifest() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("versions.toml");
    std::fs::write(&path, MANIFEST).unwrap();
    (directory, path)
}

#[test]
fn bump_resolves_entire_transitive_group_before_atomic_apply() {
    let (_dir, path) = manifest();
    let mut resolver = Fixture::default();
    let plan = plan_bump(&path, "root", "2.0.0", &Selection::default(), &mut resolver).unwrap();
    assert_eq!(plan.changes().len(), 3);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), MANIFEST);
    assert_eq!(resolver.checksums, ["child", "leaf", "root"]);
    let preview = plan.manifest();
    let updated = Manifest::parse(&preview).unwrap();
    assert_eq!(
        updated.get("root").unwrap().url,
        format!("https://example.test/root/{NEW}.tar.gz")
    );
    assert_eq!(
        updated.get("child").unwrap().url,
        format!("https://example.test/child/archive/{CHILD}.tar.gz")
    );
    assert_eq!(
        updated.get("leaf").unwrap().url,
        format!("https://example.test/leaf/archive/{LEAF}.zip")
    );
    assert_eq!(updated.get("root").unwrap().markers, ["CMakeLists.txt"]);
    assert_eq!(
        updated.get("child").unwrap().archive_file(),
        format!("child-{CHILD}.tar.gz")
    );
    assert_eq!(updated.get("independent").unwrap().version, "1");
    assert!(preview.starts_with("# preserved comment"));
    plan.apply().unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), preview);
}

#[test]
fn metadata_failures_precede_downloads_and_leave_original_unchanged() {
    for mut resolver in [
        Fixture {
            ambiguous: true,
            ..Fixture::default()
        },
        Fixture {
            bad_gitlink: true,
            ..Fixture::default()
        },
    ] {
        let (_dir, path) = manifest();
        assert!(plan_bump(&path, "root", "2.0.0", &Selection::default(), &mut resolver).is_err());
        assert!(resolver.checksums.is_empty());
        assert_eq!(std::fs::read_to_string(path).unwrap(), MANIFEST);
    }
}

#[test]
fn checksum_failure_never_commits_partial_group() {
    let (_dir, path) = manifest();
    let mut resolver = Fixture {
        fail_checksum: true,
        ..Fixture::default()
    };
    assert!(plan_bump(&path, "root", "2.0.0", &Selection::default(), &mut resolver).is_err());
    assert_eq!(resolver.checksums, ["child", "leaf", "root"]);
    assert_eq!(std::fs::read_to_string(path).unwrap(), MANIFEST);
}

#[test]
fn stale_plan_refuses_to_overwrite_another_edit() {
    let (_dir, path) = manifest();
    let plan = plan_bump(
        &path,
        "root",
        "2.0.0",
        &Selection::default(),
        &mut Fixture::default(),
    )
    .unwrap();
    std::fs::write(&path, format!("{MANIFEST}\n# concurrent edit")).unwrap();
    assert!(
        plan.apply()
            .unwrap_err()
            .to_string()
            .contains("changed since planning")
    );
    assert!(
        std::fs::read_to_string(path)
            .unwrap()
            .ends_with("# concurrent edit")
    );
}

#[test]
fn root_can_switch_to_git_and_still_resolve_archive_dependents() {
    let (_dir, path) = manifest();
    let selection = Selection {
        git: Some("https://example.test/root.git".into()),
        reference: Some("main".into()),
        ..Selection::default()
    };
    let plan = plan_bump(&path, "root", "2.0.0", &selection, &mut Fixture::default()).unwrap();
    let updated = Manifest::parse(&plan.manifest()).unwrap();
    let root = updated.get("root").unwrap();
    assert_eq!(root.git.as_ref().unwrap().revision, NEW);
    assert!(root.url.is_empty() && root.sha256.is_empty());
    assert!(updated.get("child").unwrap().git.is_none());
}

#[test]
fn gitlink_without_archive_selects_git_source() {
    let (_dir, path) = manifest();
    let text = MANIFEST.replace("archive = \"{repository}/archive/{revision}.tar.gz\"\n", "");
    std::fs::write(&path, text).unwrap();
    let plan = plan_bump(
        &path,
        "root",
        "2.0.0",
        &Selection::default(),
        &mut Fixture::default(),
    )
    .unwrap();
    let updated = Manifest::parse(&plan.manifest()).unwrap();
    assert_eq!(
        updated.get("child").unwrap().git.as_ref().unwrap().revision,
        CHILD
    );
    assert_eq!(updated.get("leaf").unwrap().version, LEAF);
}

#[test]
fn graph_cycles_unknown_parents_and_managed_child_bumps_fail() {
    let (_dir, path) = manifest();
    assert!(
        plan_bump(
            &path,
            "child",
            "2",
            &Selection::default(),
            &mut Fixture::default()
        )
        .is_err()
    );
    assert!(
        Manifest::parse(&MANIFEST.replace("parent = \"root\"", "parent = \"unknown\"")).is_err()
    );
    assert!(Manifest::parse(&MANIFEST.replace("parent = \"root\"", "parent = \"leaf\"")).is_err());
}

#[test]
fn existing_archive_templates_and_simple_add_work_without_git() {
    let (_dir, path) = manifest();
    let mut resolver = Fixture::default();
    let plan = plan_bump(
        &path,
        "independent",
        "2",
        &Selection::default(),
        &mut resolver,
    )
    .unwrap();
    assert_eq!(
        plan.changes()[0].url.as_deref(),
        Some("https://example.test/2.zip")
    );
    plan.apply().unwrap();
    let selection = Selection {
        url: Some("https://example.test/{name}-{version}.zip".into()),
        ..Selection::default()
    };
    let plan = plan_add(
        &path,
        "new",
        "3",
        &selection,
        &["CMakeLists.txt".into()],
        &mut resolver,
    )
    .unwrap();
    assert_eq!(
        plan.changes()[0].url.as_deref(),
        Some("https://example.test/new-3.zip")
    );
    plan.apply().unwrap();
    assert_eq!(
        Manifest::load(&path).unwrap().get("new").unwrap().markers,
        ["CMakeLists.txt"]
    );
}

#[test]
fn empty_manifest_can_add_git_but_never_store_floating_ref() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("versions.toml");
    std::fs::write(&path, "").unwrap();
    let selection = Selection {
        git: Some("https://example.test/root.git".into()),
        reference: Some("main".into()),
        ..Selection::default()
    };
    let plan = plan_add(
        &path,
        "root",
        "2.0.0",
        &selection,
        &[],
        &mut Fixture::default(),
    )
    .unwrap();
    plan.apply().unwrap();
    assert_eq!(
        Manifest::load(&path)
            .unwrap()
            .get("root")
            .unwrap()
            .git
            .as_ref()
            .unwrap()
            .revision,
        NEW
    );
    assert!(Manifest::parse(&std::fs::read_to_string(path).unwrap().replace(NEW, "main")).is_err());
    assert!(crate::dep::validate_revision(OLD).is_ok());
}

#[test]
fn invalid_pattern_placeholder_and_source_combinations_fail_without_write() {
    for text in [
        MANIFEST.replace("pattern = 'revision = \"([0-9a-f]{40})\"'", "pattern = '('"),
        MANIFEST.replace(
            "placeholder = \"${LEAF_REV}\"",
            "placeholder = \"${UNMATCHED}\"",
        ),
        MANIFEST.replace("path = \"pins.txt\"", "path = \"../pins.txt\""),
    ] {
        let (_dir, path) = manifest();
        std::fs::write(&path, &text).unwrap();
        assert!(
            plan_bump(
                &path,
                "root",
                "2.0.0",
                &Selection::default(),
                &mut Fixture::default()
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), text);
    }
}
