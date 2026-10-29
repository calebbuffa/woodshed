use super::*;

fn artifact(cache: &Cache, name: &str) -> (String, PathBuf) {
    let key = BuildKey::explicit(name);
    let mut artifact = storage::Artifact::open(cache, &key).unwrap();
    let install = artifact.begin().unwrap();
    fs::write(install.join("sample.h"), "int sample(void);\n").unwrap();
    artifact.publish(cache, &install).unwrap();
    artifact
        .commit(
            Installation::tools(&artifact.prefix, PackageLinkage::Static),
            &[],
            BTreeMap::new(),
        )
        .unwrap();
    (key.fingerprint(), artifact.prefix.clone())
}

#[test]
fn inspection_corruption_pins_and_explicit_pruning_are_safe() {
    let directory = tempfile::tempdir().unwrap();
    let cache = Cache::at(directory.path().join("cache"));
    let (id, prefix) = artifact(&cache, "one");
    assert!(cache.verify_artifact(&id).unwrap().verified);
    cache.pin_artifact(&id, "custom").unwrap();
    assert_eq!(
        cache
            .prune_artifacts(std::slice::from_ref(&id), false)
            .unwrap()
            .retained,
        std::slice::from_ref(&id)
    );
    cache.unpin_artifact("custom").unwrap();
    let preview = cache
        .prune_artifacts(std::slice::from_ref(&id), true)
        .unwrap();
    assert_eq!(preview.removed, std::slice::from_ref(&id));
    assert!(prefix.exists());
    fs::write(prefix.join("sample.h"), "corrupt contents\n").unwrap();
    assert!(!cache.verify_artifact(&id).unwrap().verified);
    assert!(cache.pin_artifact(&id, "broken").is_err());
    cache
        .prune_artifacts(std::slice::from_ref(&id), false)
        .unwrap();
    assert!(!prefix.exists());
    assert!(cache.prune_artifacts(&["..".into()], false).is_err());
    let unowned = "a".repeat(64);
    fs::create_dir(cache.root().join("artifacts").join(&unowned)).unwrap();
    fs::write(
        cache.root().join("artifacts").join(&unowned).join("user"),
        "keep",
    )
    .unwrap();
    assert!(
        cache
            .prune_artifacts(std::slice::from_ref(&unowned), false)
            .is_err()
    );
    assert!(
        cache
            .root()
            .join("artifacts")
            .join(unowned)
            .join("user")
            .exists()
    );
}

#[test]
fn standalone_build_pins_can_be_explicitly_released() {
    let directory = tempfile::tempdir().unwrap();
    let cache = Cache::at(directory.path().join("cache"));
    let (id, _) = artifact(&cache, "standalone");
    pin_build_output(&cache, &id, None).unwrap();
    assert_eq!(
        cache
            .prune_artifacts(std::slice::from_ref(&id), true)
            .unwrap()
            .retained,
        std::slice::from_ref(&id)
    );
    cache.unpin_artifact(&format!("standalone:{id}")).unwrap();
    assert_eq!(
        cache
            .prune_artifacts(std::slice::from_ref(&id), true)
            .unwrap()
            .removed,
        [id]
    );
}

#[test]
fn cargo_references_expire_only_when_the_output_disappears() {
    let directory = tempfile::tempdir().unwrap();
    let cache = Cache::at(directory.path().join("cache"));
    let (id, _) = artifact(&cache, "cargo");
    let output = directory.path().join("output");
    fs::create_dir(&output).unwrap();
    let path = reference_path(&cache, "cargo-fixture");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    atomic_json(
        &path,
        &Reference {
            id: id.clone(),
            output: Some(output.clone()),
        },
    )
    .unwrap();
    assert_eq!(
        cache
            .prune_artifacts(std::slice::from_ref(&id), false)
            .unwrap()
            .retained,
        std::slice::from_ref(&id)
    );
    fs::remove_dir(&output).unwrap();
    assert_eq!(
        cache
            .prune_artifacts(std::slice::from_ref(&id), false)
            .unwrap()
            .removed,
        [id]
    );
}

#[test]
fn active_builds_block_pruning_until_they_release_the_activity_lease() {
    let directory = tempfile::tempdir().unwrap();
    let cache = Cache::at(directory.path().join("cache"));
    let (id, _) = artifact(&cache, "active");
    let guard = Lock::acquire_shared(&cache.root().join("activity.lock")).unwrap();
    let (sent, received) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        sent.send(cache.prune_artifacts(&[id], true)).unwrap();
    });
    assert!(
        received
            .recv_timeout(std::time::Duration::from_millis(150))
            .is_err()
    );
    drop(guard);
    assert!(
        received
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .is_ok()
    );
    worker.join().unwrap();
}
