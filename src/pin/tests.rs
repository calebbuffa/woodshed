use super::*;
use crate::project::{self, Dependency, Lock, Project, Source, Update};
use std::{fs, path::PathBuf};

const OLD: &str = "1111111111111111111111111111111111111111";
const NEW: &str = "2222222222222222222222222222222222222222";
const CHILD: &str = "3333333333333333333333333333333333333333";
const LEAF: &str = "4444444444444444444444444444444444444444";
const DECLARATIONS: &str = r#"# preserved comment
[dependencies.root]
github = "example/root"
version = "1.0.0"
file = "root-{version}.tar.gz"
markers = ["CMakeLists.txt"]

[dependencies.child.resolve]
kind = "gitlink"
parent = "root"
path = "vendor/child"
archive = "{repository}/archive/{revision}.tar.gz"

[dependencies.leaf.resolve]
kind = "file"
parent = "child"
placeholder = "${LEAF_REV}"
revision = { path = "pins.txt", pattern = 'revision = "([0-9a-f]{40})"' }
url = { path = "download.txt", pattern = 'url = "(https://[^"]+)"' }

[dependencies.independent]
version = "1"
url = "https://example.test/{version}.zip"
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
        assert_eq!(repository, "https://github.com/example/root.git");
        Ok(if reference == "v1.0.0" { OLD } else { NEW }.into())
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
        assert_eq!(repository, "https://github.com/example/root.git");
        assert!([OLD, NEW].contains(&revision));
        assert_eq!(path, "vendor/child");
        Ok(GitSource {
            repository: "https://example.test/child.git".into(),
            revision: if self.bad_gitlink { "main" } else { CHILD }.into(),
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
    let path = directory.path().join("woodshed.toml");
    fs::write(&path, DECLARATIONS).unwrap();
    project::plan_lock(&path, &mut Fixture::default())
        .unwrap()
        .apply()
        .unwrap();
    (directory, path)
}

fn preview(plan: &Update) -> Lock {
    Project::parse("woodshed.toml", plan.manifest())
        .unwrap()
        .parse_lock(plan.lock())
        .unwrap()
}

fn bump(path: &Path, resolver: &mut Fixture) -> Result<Update> {
    project::plan_bump(path, "root", "2.0.0", &Selection::default(), resolver)
}

#[test]
fn bump_resolves_transitive_sources_before_transactional_apply() {
    let (_directory, path) = manifest();
    let before = fs::read(path.with_file_name("woodshed.lock")).unwrap();
    let mut resolver = Fixture::default();
    let plan = bump(&path, &mut resolver).unwrap();
    assert_eq!(plan.changes().len(), 3);
    assert_eq!(resolver.checksums, ["child", "leaf", "root"]);
    assert_eq!(fs::read_to_string(&path).unwrap(), DECLARATIONS);
    assert_eq!(
        fs::read(path.with_file_name("woodshed.lock")).unwrap(),
        before
    );
    let lock = preview(&plan);
    assert_eq!(
        lock.sources["root"].url,
        format!("https://github.com/example/root/archive/{NEW}.tar.gz")
    );
    assert_eq!(
        lock.sources["child"].url,
        format!("https://example.test/child/archive/{CHILD}.tar.gz")
    );
    assert_eq!(
        lock.sources["leaf"].url,
        format!("https://example.test/leaf/archive/{LEAF}.zip")
    );
    assert_eq!(lock.sources["root"].markers, ["CMakeLists.txt"]);
    assert_eq!(lock.sources["independent"].version, "1");
    assert!(plan.manifest().starts_with("# preserved comment"));
    let declarations = plan.manifest().to_owned();
    plan.apply().unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), declarations);
}

#[test]
fn failed_resolution_or_checksum_never_commits_partial_graph() {
    for mut resolver in [
        Fixture {
            ambiguous: true,
            ..Fixture::default()
        },
        Fixture {
            bad_gitlink: true,
            ..Fixture::default()
        },
        Fixture {
            fail_checksum: true,
            ..Fixture::default()
        },
    ] {
        let (_directory, path) = manifest();
        let before = fs::read(path.with_file_name("woodshed.lock")).unwrap();
        assert!(bump(&path, &mut resolver).is_err());
        if !resolver.fail_checksum {
            assert!(resolver.checksums.is_empty());
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), DECLARATIONS);
        assert_eq!(
            fs::read(path.with_file_name("woodshed.lock")).unwrap(),
            before
        );
    }
}

#[test]
fn concurrent_and_stale_plans_cannot_overwrite_edits() {
    let (_directory, path) = manifest();
    let first = bump(&path, &mut Fixture::default()).unwrap();
    let second = bump(&path, &mut Fixture::default()).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let run = |plan: Update, barrier: std::sync::Arc<std::sync::Barrier>| {
        std::thread::spawn(move || {
            barrier.wait();
            plan.apply().is_ok()
        })
    };
    let a = run(first, barrier.clone());
    let b = run(second, barrier);
    assert_ne!(a.join().unwrap(), b.join().unwrap());
    let stale = bump(&path, &mut Fixture::default()).unwrap();
    let edited = format!("{}\n# concurrent edit", fs::read_to_string(&path).unwrap());
    fs::write(&path, &edited).unwrap();
    assert!(stale.apply().is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), edited);
}

#[test]
fn git_selection_preserves_archive_dependents_and_never_stores_floating_revision() {
    let (_directory, path) = manifest();
    let plan = project::plan_bump(
        &path,
        "root",
        "2.0.0",
        &Selection {
            git: Some("https://github.com/example/root.git".into()),
            reference: Some("main".into()),
            ..Selection::default()
        },
        &mut Fixture::default(),
    )
    .unwrap();
    let lock = preview(&plan);
    assert_eq!(lock.sources["root"].git.as_ref().unwrap().revision, NEW);
    assert!(lock.sources["root"].url.is_empty());
    assert!(lock.sources["child"].git.is_none());
    let project = Project::parse("woodshed.toml", plan.manifest()).unwrap();
    assert!(
        project
            .parse_lock(&plan.lock().replace(NEW, "main"))
            .is_err()
    );
}

#[test]
fn gitlink_without_archive_locks_a_git_snapshot_and_resolves_its_children() {
    let (_directory, path) = manifest();
    fs::write(
        &path,
        DECLARATIONS.replace("archive = \"{repository}/archive/{revision}.tar.gz\"\n", ""),
    )
    .unwrap();
    project::plan_lock(&path, &mut Fixture::default())
        .unwrap()
        .apply()
        .unwrap();
    let plan = bump(&path, &mut Fixture::default()).unwrap();
    let lock = preview(&plan);
    assert_eq!(lock.sources["child"].git.as_ref().unwrap().revision, CHILD);
    assert_eq!(lock.sources["leaf"].version, LEAF);
}

#[test]
fn graph_cycles_unknown_parents_and_managed_child_bumps_fail() {
    let (_directory, path) = manifest();
    assert!(
        project::plan_bump(
            &path,
            "child",
            "2",
            &Selection::default(),
            &mut Fixture::default()
        )
        .is_err()
    );
    for text in [
        DECLARATIONS.replace("parent = \"root\"", "parent = \"unknown\""),
        DECLARATIONS.replace("parent = \"root\"", "parent = \"leaf\""),
        DECLARATIONS.replace("pattern = 'revision = \"([0-9a-f]{40})\"'", "pattern = '('"),
        DECLARATIONS.replace(
            "placeholder = \"${LEAF_REV}\"",
            "placeholder = \"${UNMATCHED}\"",
        ),
        DECLARATIONS.replace("path = \"pins.txt\"", "path = \"../pins.txt\""),
    ] {
        fs::write(&path, &text).unwrap();
        assert!(project::plan_lock(&path, &mut Fixture::default()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }
}

#[test]
fn add_templates_and_first_add_create_only_declarations_and_lock() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native").join("woodshed.toml");
    let source = Dependency::Source(Box::new(Source {
        version: Some("3".into()),
        url: Some("https://example.test/{name}-{version}.zip".into()),
        markers: vec!["CMakeLists.txt".into()],
        ..Source::default()
    }));
    let plan = project::plan_add(&path, "new", source, &mut Fixture::default()).unwrap();
    assert!(!path.parent().unwrap().exists());
    assert_eq!(
        preview(&plan).sources["new"].url,
        "https://example.test/new-3.zip"
    );
    plan.apply().unwrap();
    let lock = Project::load(&path).unwrap().locked().unwrap();
    assert_eq!(lock.sources["new"].markers, ["CMakeLists.txt"]);
    assert!(path.with_file_name("woodshed.lock").is_file());
}

#[test]
fn offline_resolver_rejects_missing_ambiguous_and_escaping_fixtures() {
    let directory = tempfile::tempdir().unwrap();
    let metadata = directory.path().join("metadata.json");
    fs::write(&metadata, "{}").unwrap();
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
    fs::write(&metadata, format!(r#"{{"refs":[{item},{item}]}}"#)).unwrap();
    assert!(
        Offline::load(&metadata)
            .unwrap()
            .resolve_ref("repo", "main")
            .is_err()
    );
    fs::write(
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
