#![cfg(feature = "git")]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use woodshed::{BuildKey, Cache, Dep, git::Repository};

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "user.name=Woodshed Test",
            "-c",
            "user.email=woodshed@example.test",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

struct Repositories {
    _directory: tempfile::TempDir,
    parent: PathBuf,
    child: PathBuf,
    parent_revision: String,
    child_revision: String,
}

impl Repositories {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("parent");
        let child = directory.path().join("child");
        for repository in [&parent, &child] {
            fs::create_dir(repository).unwrap();
            git(repository, &["init", "--quiet"]);
            fs::write(repository.join("CMakeLists.txt"), "# fixture source\n").unwrap();
            git(repository, &["add", "."]);
            git(repository, &["commit", "--quiet", "-m", "fixture"]);
        }
        let child_revision = git(&child, &["rev-parse", "HEAD"]);
        fs::write(
            parent.join(".gitmodules"),
            "[submodule \"arbitrary.path\"]\n path = vendor/child\n url = ../child\n",
        )
        .unwrap();
        git(&parent, &["add", ".gitmodules"]);
        git(
            &parent,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{child_revision},vendor/child"),
            ],
        );
        git(&parent, &["commit", "--quiet", "-m", "dependency"]);
        git(&parent, &["tag", "v1.0.0"]);
        let parent_revision = git(&parent, &["rev-parse", "HEAD"]);
        Self {
            _directory: directory,
            parent,
            child,
            parent_revision,
            child_revision,
        }
    }
}

#[test]
fn git_tag_file_and_gitlink_metadata_come_from_exact_commit() {
    let repositories = Repositories::new();
    let parent = repositories.parent.to_str().unwrap();
    let snapshot = Repository::fetch(parent, "v1.0.0").unwrap();
    assert_eq!(snapshot.revision(), repositories.parent_revision);
    assert_eq!(
        snapshot.file("CMakeLists.txt").unwrap(),
        "# fixture source\n"
    );
    let child = snapshot.gitlink("vendor/child").unwrap();
    assert_eq!(child.revision, repositories.child_revision);
    assert_eq!(Path::new(&child.repository), repositories.child);
    assert!(snapshot.gitlink("CMakeLists.txt").is_err());
    assert!(snapshot.gitlink("vendor").is_err());
    assert!(snapshot.gitlink("missing").is_err());
    assert!(snapshot.file("../CMakeLists.txt").is_err());
    assert!(Repository::pinned(parent, "v1.0.0").is_err());
    assert!(Repository::fetch(parent, "--upload-pack=not-allowed").is_err());
}

#[test]
fn cached_git_snapshot_is_immutable_and_reused_offline_without_git_metadata() {
    let repositories = Repositories::new();
    let directory = tempfile::tempdir().unwrap();
    let cache = Cache::at(directory.path());
    let dep = Dep::git(
        "fixture",
        "1",
        repositories.parent.to_str().unwrap(),
        &repositories.parent_revision,
    )
    .with_markers(["CMakeLists.txt"]);
    let source = cache.source(&dep).unwrap();
    assert!(!source.join(".git").exists());
    assert_eq!(
        fs::read_to_string(source.join("CMakeLists.txt")).unwrap(),
        "# fixture source\n"
    );
    assert!(!source.join("vendor/child/CMakeLists.txt").exists());
    fs::write(
        repositories.parent.join("CMakeLists.txt"),
        "# changed branch\n",
    )
    .unwrap();
    git(
        &repositories.parent,
        &["commit", "--quiet", "-a", "-m", "changed"],
    );
    assert_eq!(cache.clone().offline(true).source(&dep).unwrap(), source);
    assert_eq!(
        fs::read_to_string(source.join("CMakeLists.txt")).unwrap(),
        "# fixture source\n"
    );
    fs::remove_file(source.join(".woodshed-git")).unwrap();
    assert!(cache.clone().offline(true).source(&dep).is_err());
    assert_eq!(cache.source(&dep).unwrap(), source);
    assert!(cache.archive(&dep).is_err());
}

#[test]
fn git_identity_changes_cache_and_build_keys_but_not_archive_compatibility() {
    let repositories = Repositories::new();
    let directory = tempfile::tempdir().unwrap();
    let cache = Cache::at(directory.path());
    let first = Dep::git(
        "same",
        "label",
        repositories.parent.to_str().unwrap(),
        &repositories.parent_revision,
    );
    let second = Dep::git(
        "same",
        "label",
        repositories.child.to_str().unwrap(),
        &repositories.child_revision,
    );
    assert_ne!(
        BuildKey::new("git").dep(&first).as_str(),
        BuildKey::new("git").dep(&second).as_str()
    );
    let mut relabeled = first.clone();
    relabeled.version = "different-release-metadata".into();
    assert_ne!(
        BuildKey::new("git").dep(&first).as_str(),
        BuildKey::new("git").dep(&relabeled).as_str()
    );
    assert_ne!(
        cache.source(&first).unwrap(),
        cache.source(&second).unwrap()
    );
    let archive = Dep::new("archive", "1", "https://example.test/archive.zip", "abc");
    assert_eq!(
        BuildKey::new("archive").dep(&archive).as_str(),
        BuildKey::new("archive").mix("1").mix("abc").as_str()
    );
}

#[test]
fn missing_git_marker_or_bad_revision_never_produces_a_complete_source() {
    let repositories = Repositories::new();
    let directory = tempfile::tempdir().unwrap();
    let cache = Cache::at(directory.path());
    let dep = Dep::git(
        "fixture",
        "1",
        repositories.parent.to_str().unwrap(),
        &repositories.parent_revision,
    )
    .with_markers(["missing"]);
    assert!(cache.source(&dep).is_err());
    assert!(cache.clone().offline(true).source(&dep).is_err());
    let floating = Dep::git(
        "fixture",
        "1",
        repositories.parent.to_str().unwrap(),
        "HEAD",
    );
    assert!(cache.source(&floating).is_err());
}

#[cfg(feature = "pin")]
#[test]
fn generic_add_and_bump_resolve_live_local_refs_then_build_from_immutable_commits() {
    let repositories = Repositories::new();
    let directory = tempfile::tempdir().unwrap();
    let manifest = directory.path().join("woodshed.toml");
    let source = woodshed::project::Source {
        version: Some("1.0.0".into()),
        git: Some(repositories.parent.to_string_lossy().into_owned()),
        reference: Some("v1.0.0".into()),
        markers: vec!["CMakeLists.txt".into()],
        ..Default::default()
    };
    let plan = woodshed::project::plan_add(
        &manifest,
        "fixture",
        woodshed::project::Dependency::Source(Box::new(source)),
        &mut woodshed::pin::Online::default(),
    )
    .unwrap();
    plan.apply().unwrap();
    let pins = woodshed::project::Project::load(&manifest)
        .unwrap()
        .locked()
        .unwrap()
        .sources;
    assert_eq!(
        pins.get("fixture").unwrap().git.as_ref().unwrap().revision,
        repositories.parent_revision
    );
    fs::write(
        repositories.parent.join("CMakeLists.txt"),
        "# newer source\n",
    )
    .unwrap();
    git(
        &repositories.parent,
        &["commit", "--quiet", "-a", "-m", "new release"],
    );
    git(&repositories.parent, &["tag", "v2.0.0"]);
    let plan = woodshed::project::plan_bump(
        &manifest,
        "fixture",
        "2.0.0",
        &woodshed::pin::Selection {
            reference: Some("v2.0.0".into()),
            ..Default::default()
        },
        &mut woodshed::pin::Online::default(),
    )
    .unwrap();
    plan.apply().unwrap();
    let pins = woodshed::project::Project::load(manifest)
        .unwrap()
        .locked()
        .unwrap()
        .sources;
    let source = Cache::at(directory.path().join("cache"))
        .source(pins.get("fixture").unwrap())
        .unwrap();
    assert_eq!(
        fs::read_to_string(source.join("CMakeLists.txt")).unwrap(),
        "# newer source\n"
    );
}
