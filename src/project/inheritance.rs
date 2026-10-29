use super::*;
use std::collections::BTreeMap;

pub(super) fn declarations(
    path: &Path,
    text: &str,
) -> Result<(Declarations, BTreeMap<PathBuf, String>)> {
    let mut document: toml::Value =
        toml::from_str(text).map_err(|error| invalid(error.to_string()))?;
    let workspace = document
        .as_table_mut()
        .and_then(|table| table.remove("workspace"));
    let mut inputs = BTreeMap::new();
    let mut shared = workspace
        .map(|workspace| shared_table(path, workspace))
        .transpose()?;
    if needs_inheritance(&document) && shared.is_none() {
        let absolute = std::path::absolute(path)
            .map_err(|error| Error::io("resolve workspace manifest", error))?;
        for parent in absolute
            .parent()
            .into_iter()
            .flat_map(Path::ancestors)
            .skip(1)
        {
            let candidate = parent.join("woodshed.toml");
            let contents = match std::fs::read_to_string(&candidate) {
                Ok(contents) => contents,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(Error::io("read shared Woodshed manifest", error)),
            };
            let mut value: toml::Value = toml::from_str(&contents)
                .map_err(|error| invalid(format!("{}: {error}", candidate.display())))?;
            if let Some(workspace) = value
                .as_table_mut()
                .and_then(|table| table.remove("workspace"))
            {
                shared = Some(shared_table(&candidate, workspace)?);
                inputs.insert(candidate, contents);
                break;
            }
        }
    }
    if let Some(dependencies) = document.get_mut("dependencies") {
        expand(dependencies, shared.as_ref())?;
    }
    if let Some(targets) = document
        .get_mut("target")
        .and_then(toml::Value::as_table_mut)
    {
        for (_, target) in targets.iter_mut() {
            if let Some(dependencies) = target.get_mut("dependencies") {
                expand(dependencies, shared.as_ref())?;
            }
        }
    }
    let declarations: Declarations = document
        .try_into()
        .map_err(|error: toml::de::Error| invalid(error.to_string()))?;
    declarations.validate()?;
    Ok((declarations, inputs))
}

fn needs_inheritance(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(table) => {
            table.contains_key("workspace") || table.values().any(needs_inheritance)
        }
        _ => false,
    }
}

fn shared_table(path: &Path, workspace: toml::Value) -> Result<toml::Table> {
    let mut workspace = workspace
        .as_table()
        .cloned()
        .ok_or_else(|| invalid("workspace must be a table"))?;
    let dependencies = workspace
        .remove("dependencies")
        .unwrap_or_else(|| toml::Value::Table(toml::Table::new()));
    if !workspace.is_empty() {
        return Err(invalid("workspace supports only the dependencies table"));
    }
    let mut dependencies = dependencies
        .as_table()
        .cloned()
        .ok_or_else(|| invalid("workspace.dependencies must be a table"))?;
    let root = std::path::absolute(path)
        .map_err(|error| Error::io("resolve shared dependency paths", error))?;
    let root = root.parent().expect("absolute manifest");
    for (name, value) in &mut dependencies {
        let table = value
            .as_table_mut()
            .ok_or_else(|| invalid(format!("workspace dependency {name} must be a table")))?;
        if table.contains_key("workspace") {
            return Err(invalid("workspace dependencies cannot recursively inherit"));
        }
        for key in ["path", "host-profile", "build-profile"] {
            rebase(table, key, root)?;
        }
        if let Some(build) = table.get_mut("build").and_then(toml::Value::as_table_mut) {
            rebase(build, "toolchain", root)?;
            if let Some(inputs) = build.get_mut("inputs") {
                let inputs = inputs
                    .as_array_mut()
                    .ok_or_else(|| invalid("build.inputs must be an array"))?;
                for input in inputs {
                    let value = input
                        .as_str()
                        .ok_or_else(|| invalid("build.inputs must contain paths"))?;
                    *input = toml::Value::String(root.join(value).to_string_lossy().into_owned());
                }
            }
        }
        let _: Dependency = value.clone().try_into().map_err(|error: toml::de::Error| {
            invalid(format!("workspace dependency {name}: {error}"))
        })?;
    }
    Ok(dependencies)
}

fn rebase(table: &mut toml::Table, key: &str, root: &Path) -> Result<()> {
    if let Some(value) = table.get_mut(key) {
        let path = value
            .as_str()
            .ok_or_else(|| invalid(format!("{key} must be a path string")))?;
        *value = toml::Value::String(
            root.join(path)
                .to_str()
                .ok_or_else(|| invalid("workspace paths must be UTF-8"))?
                .into(),
        );
    }
    Ok(())
}

fn expand(dependencies: &mut toml::Value, shared: Option<&toml::Table>) -> Result<()> {
    let dependencies = dependencies
        .as_table_mut()
        .ok_or_else(|| invalid("dependencies must be a table"))?;
    for (name, dependency) in dependencies {
        let Some(member) = dependency
            .as_table()
            .filter(|table| table.contains_key("workspace"))
        else {
            continue;
        };
        if member.get("workspace").and_then(toml::Value::as_bool) != Some(true) {
            return Err(invalid(format!("{name}: workspace must be true")));
        }
        if member.keys().any(|key| {
            !matches!(
                key.as_str(),
                "workspace" | "optional" | "features" | "default-features"
            )
        }) {
            return Err(invalid(format!(
                "{name}: inherited dependencies permit only optional, features and default-features overrides; change source/build policy in workspace.dependencies"
            )));
        }
        let mut merged = shared
            .and_then(|table| table.get(name))
            .and_then(toml::Value::as_table)
            .cloned()
            .ok_or_else(|| invalid(format!("{name}: missing workspace.dependencies entry")))?;
        if let Some(optional) = member.get("optional") {
            merged.insert("optional".into(), optional.clone());
        }
        if let Some(defaults) = member.get("default-features") {
            let enabled = defaults
                .as_bool()
                .ok_or_else(|| invalid("default-features must be boolean"))?;
            if !enabled
                && merged
                    .get("default-features")
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(true)
            {
                return Err(invalid(format!(
                    "{name}: cannot disable inherited default features; set default-features=false in workspace.dependencies"
                )));
            }
            merged.insert("default-features".into(), defaults.clone());
        }
        if let Some(features) = member.get("features") {
            let features = features
                .as_array()
                .ok_or_else(|| invalid("features must be an array"))?;
            let existing = merged
                .entry("features")
                .or_insert_with(|| toml::Value::Array(Vec::new()))
                .as_array_mut()
                .ok_or_else(|| invalid("workspace features must be an array"))?;
            for feature in features {
                if !existing.contains(feature) {
                    existing.push(feature.clone());
                }
            }
        }
        *dependency = toml::Value::Table(merged);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_paths_features_and_root_changes_are_resolved() {
        let root = tempfile::tempdir().unwrap();
        let manifest = root.path().join("woodshed.toml");
        let child = root.path().join("member").join("woodshed.toml");
        std::fs::create_dir(child.parent().unwrap()).unwrap();
        let shared = "[workspace.dependencies.codec]\npath='native/codec'\ndefault-features=false\nfeatures=['base']\nbuild={cmake='Codec',targets=['Codec::Codec'],features={base={},extra={}},toolchain='cross.cmake',inputs=['flags.txt']}\n";
        std::fs::write(&manifest, shared).unwrap();
        let text = "[dependencies.codec]\nworkspace=true\noptional=true\nfeatures=['extra']\n";
        let project = Project::parse(&child, text).unwrap();
        let Dependency::Source(source) = &project.declarations.dependencies["codec"] else {
            unreachable!()
        };
        assert_eq!(
            source.path.as_ref().unwrap(),
            &root.path().join("native/codec")
        );
        assert_eq!(source.features, ["base", "extra"]);
        assert!(source.selection.optional);
        assert_eq!(
            source.build.as_ref().unwrap().toolchain.as_ref().unwrap(),
            &root.path().join("cross.cmake")
        );
        assert!(project.workspace_inputs.contains_key(&manifest));
        let original = project.declarations.digest().unwrap();
        std::fs::write(&manifest, shared.replace("native/codec", "native/changed")).unwrap();
        assert_ne!(
            original,
            Project::parse(&child, text)
                .unwrap()
                .declarations
                .digest()
                .unwrap()
        );
        assert!(Project::parse(&child, &text.replace("optional=true", "path='override'")).is_err());
        assert!(Project::parse(&child, &(text.to_owned() + "default-features=true\n")).is_ok());
    }

    #[test]
    fn shared_defaults_cannot_be_disabled_and_target_tables_can_inherit() {
        let text = r#"
[workspace.dependencies.codec]
path = "native"
[target.x86_64-pc-windows-msvc.dependencies.codec]
workspace = true
optional = true
[features]
extra = ["dep:codec"]
"#;
        let project = Project::parse("woodshed.toml", text).unwrap();
        let mut options = PlanOptions::new("host", "x86_64-pc-windows-msvc");
        options.features = vec!["extra".into()];
        assert_eq!(project.resolve(&options).unwrap().dependencies.len(), 1);
        assert!(
            Project::parse(
                "woodshed.toml",
                &text.replace("optional = true", "default-features = false")
            )
            .is_err()
        );
        assert!(
            Project::parse(
                "woodshed.toml",
                &text.replace("workspace = true", "workspace = false")
            )
            .is_err()
        );
    }

    #[cfg(feature = "pin")]
    #[test]
    fn locking_preserves_inherited_intent_and_guards_parent_edits() {
        use crate::pin::Resolver;
        struct NoNetwork;
        impl Resolver for NoNetwork {
            fn resolve_ref(&mut self, _: &str, _: &str) -> Result<String> {
                panic!("no remote lookup for local inheritance")
            }
            fn checksum(&mut self, _: &Dep) -> Result<String> {
                panic!("no download for local inheritance")
            }
            fn file(&mut self, _: &str, _: &str, _: &str) -> Result<String> {
                panic!("no upstream discovery for local inheritance")
            }
            fn gitlink(&mut self, _: &str, _: &str, _: &str) -> Result<crate::GitSource> {
                panic!("no Git for local inheritance")
            }
        }
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("woodshed.toml");
        std::fs::write(&workspace, "[workspace.dependencies.codec]\npath='codec'\n").unwrap();
        let path = root.path().join("member").join("woodshed.toml");
        std::fs::create_dir(path.parent().unwrap()).unwrap();
        let original = "[dependencies.codec]\nworkspace=true\n";
        std::fs::write(&path, original).unwrap();
        let update = plan_lock(&path, &mut NoNetwork).unwrap();
        assert_eq!(update.manifest(), original);
        std::fs::write(
            &workspace,
            "[workspace.dependencies.codec]\npath='changed'\n",
        )
        .unwrap();
        assert!(update.apply().is_err());
        plan_lock(&path, &mut NoNetwork).unwrap().apply().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        Project::load(&path).unwrap().locked().unwrap();
        std::fs::write(
            &workspace,
            "[workspace.dependencies.codec]\npath='another'\n",
        )
        .unwrap();
        assert!(Project::load(&path).unwrap().locked().is_err());
    }
}
