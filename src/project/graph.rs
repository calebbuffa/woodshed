use super::*;
use std::collections::{BTreeSet, VecDeque};

/// Explicit selection context. Resolving a plan never resolves remote sources.
#[derive(Clone, Debug)]
pub struct PlanOptions {
    /// Rust target triple for native target dependencies.
    pub target: String,
    /// Rust host triple for executable build tools.
    pub host: String,
    /// Manifest feature names to enable.
    pub features: Vec<String>,
    /// Enable the manifest's `default` feature.
    pub default_features: bool,
    /// Enable every manifest and implicit optional-dependency feature.
    pub all_features: bool,
    /// Actual Rust target cfg values, if known (e.g. Cargo's CARGO_CFG_*).
    /// Otherwise queried from rustc for the selected triple.
    pub target_cfg: Option<Vec<String>>,
}

impl PlanOptions {
    /// Select default features for an explicit host and target.
    pub fn new(host: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            target: target.into(),
            features: Vec::new(),
            default_features: true,
            all_features: false,
            target_cfg: None,
        }
    }
}

/// A selected graph node, after feature expansion and target filtering.
#[derive(Clone, Debug, Serialize)]
pub struct ResolvedDependency {
    /// Name used by dependency edges and diagnostics.
    pub name: String,
    /// Unique source/provider lock identity, including target variant.
    pub id: String,
    /// Host or target build role.
    pub role: BuildRole,
    /// Actual triple this node must be built for.
    pub target: String,
    /// Lock IDs of prerequisites, in deterministic order.
    pub depends: Vec<String>,
    /// Effective source recipe features (provider features remain provider-owned).
    pub features: Vec<String>,
    /// Effective project-specific CMake definitions.
    pub defines: BTreeMap<String, String>,
    /// Why the node is active.
    pub reasons: Vec<String>,
    /// Original typed source/provider intent; native feature effects are above.
    pub declaration: Dependency,
}

/// Deterministic prerequisite-first build plan suitable for inspection and execution.
#[derive(Clone, Debug, Serialize)]
pub struct ResolvedPlan {
    /// Expanded manifest features.
    pub features: Vec<String>,
    /// Selected nodes in topological order, independent of table ordering.
    pub dependencies: Vec<ResolvedDependency>,
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn validate_target_predicate(expression: &cargo_platform::CfgExpr) -> Result<()> {
    use cargo_platform::{Cfg, CfgExpr};
    match expression {
        CfgExpr::Not(expression) => validate_target_predicate(expression),
        CfgExpr::All(expressions) | CfgExpr::Any(expressions) => {
            for expression in expressions {
                validate_target_predicate(expression)?;
            }
            Ok(())
        }
        _ => {
            if let CfgExpr::Value(Cfg::Name(name) | Cfg::KeyPair(name, _)) = expression
                && matches!(
                    name.as_str(),
                    "feature" | "test" | "debug_assertions" | "proc_macro"
                )
            {
                return Err(invalid(format!(
                    "cfg({name}) is not a target-platform predicate; use manifest features for dependency gates"
                )));
            }
            Ok(())
        }
    }
}

impl Declarations {
    pub(super) fn validate_graph_declarations(&self) -> Result<()> {
        let mut cargo_names = BTreeMap::new();
        for name in self.feature_names() {
            let variable = name.replace('-', "_").to_uppercase();
            if let Some(previous) = cargo_names.insert(variable, name.clone())
                && previous != name
            {
                return Err(invalid(format!(
                    "features {previous} and {name} map to the same Cargo environment variable"
                )));
            }
        }
        let all = self.all_dependencies();
        let mut names = self.dependencies.keys().cloned().collect::<BTreeSet<_>>();
        for (condition, table) in &self.target {
            let platform = condition
                .parse::<cargo_platform::Platform>()
                .map_err(|error| invalid(format!("target.{condition}: {error}")))?;
            if let cargo_platform::Platform::Cfg(expression) = platform {
                validate_target_predicate(&expression)?;
            }
            names.extend(table.dependencies.keys().cloned());
            for name in table.dependencies.keys() {
                if !identifier(name) {
                    return Err(invalid(format!("invalid dependency name {name:?}")));
                }
            }
        }
        for (name, dependency) in &all {
            if matches!(dependency, Dependency::Package(_))
                && (!dependency.selection().depends.is_empty()
                    || !dependency.selection().optional_depends.is_empty())
            {
                return Err(invalid(format!(
                    "{name}: vcpkg/Conan own their locked build graphs; external depends/optional-depends require a source recipe instead"
                )));
            }
            if dependency
                .selection()
                .depends
                .iter()
                .chain(&dependency.selection().optional_depends)
                .any(|edge| !names.contains(edge))
            {
                return Err(invalid(format!(
                    "{name}: depends contains an unknown dependency"
                )));
            }
            if let Dependency::Source(source) = dependency {
                if let Some(recipe) = &source.build {
                    if let Some(workspace) = &recipe.workspace {
                        for option in &workspace.reconfigure {
                            if !recipe.defines.contains_key(option)
                                && !recipe
                                    .features
                                    .values()
                                    .any(|defines| defines.contains_key(option))
                            {
                                return Err(invalid(format!(
                                    "{name}: workspace reconfigure option {option} must be declared in build.defines or build.features"
                                )));
                            }
                        }
                        for edge in &workspace.dependencies {
                            if !dependency.selection().depends.contains(edge)
                                && !dependency.selection().optional_depends.contains(edge)
                            {
                                return Err(invalid(format!(
                                    "{name}: workspace dependency {edge} must be declared in depends/optional-depends"
                                )));
                            }
                            if self
                                .dependencies
                                .get(edge)
                                .is_some_and(|dep| dep.selection().role == BuildRole::Host)
                                || self
                                    .target
                                    .values()
                                    .filter_map(|table| table.dependencies.get(edge))
                                    .any(|dep| dep.selection().role == BuildRole::Host)
                            {
                                return Err(invalid(format!(
                                    "{name}: host prerequisite {edge} must remain a hard workspace input"
                                )));
                            }
                        }
                    }
                    for feature in recipe.features.keys() {
                        if !identifier(feature) {
                            return Err(invalid(format!(
                                "{name}: invalid native feature {feature:?}"
                            )));
                        }
                    }
                    for feature in source.features.iter().chain(&recipe.default_features) {
                        if !recipe.features.contains_key(feature) {
                            return Err(invalid(format!(
                                "{name}: unknown native feature {feature}"
                            )));
                        }
                    }
                    for defines in recipe.features.values() {
                        let mut check = (**recipe).clone();
                        check.defines = defines.clone();
                        check.features.clear();
                        check.validate()?;
                    }
                } else if !source.features.is_empty() || !source.default_features {
                    return Err(invalid(format!(
                        "{name}: native features require a source build recipe"
                    )));
                }
            }
        }
        for (feature, entries) in &self.features {
            if !identifier(feature) {
                return Err(invalid(format!("invalid feature {feature:?}")));
            }
            for entry in entries {
                let dependency = entry
                    .strip_prefix("dep:")
                    .or_else(|| entry.split_once('/').map(|(name, _)| name));
                if let Some(name) = dependency {
                    if !names.contains(name) {
                        return Err(invalid(format!(
                            "feature {feature}: unknown dependency {name}"
                        )));
                    }
                    if let Some((_, native)) = entry.split_once('/') {
                        for dep in self.dependencies.get(name).into_iter().chain(
                            self.target
                                .values()
                                .filter_map(|table| table.dependencies.get(name)),
                        ) {
                            match dep {
                                Dependency::Source(source)
                                    if source.build.as_ref().is_some_and(|recipe| {
                                        recipe.features.contains_key(native)
                                    }) => {}
                                Dependency::Source(_) => {
                                    return Err(invalid(format!(
                                        "feature {feature}: {name} has no native recipe feature {native}"
                                    )));
                                }
                                Dependency::Package(_) => {
                                    return Err(invalid(format!(
                                        "feature {feature}: provider features must be declared in its locked provider selection, not forwarded"
                                    )));
                                }
                            }
                        }
                    }
                    if let Some((_, native)) = entry.split_once('/')
                        && (!identifier(native) || entry.starts_with("dep:"))
                    {
                        return Err(invalid(format!(
                            "feature {feature}: invalid native feature {entry}"
                        )));
                    }
                } else if !self.features.contains_key(entry) && !names.contains(entry) {
                    return Err(invalid(format!(
                        "feature {feature}: unknown feature {entry}"
                    )));
                }
            }
        }
        Ok(())
    }
}

fn target_cfg(triple: &str) -> Result<Vec<cargo_platform::Cfg>> {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = std::process::Command::new(rustc)
        .args(["--print", "cfg", "--target", triple])
        .output()
        .map_err(|error| Error::io("query Rust target configuration", error))?;
    if !output.status.success() {
        return Err(invalid(format!(
            "cannot inspect target {triple}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| invalid(error.to_string()))?
        .lines()
        .map(|line| {
            line.parse()
                .map_err(|error| invalid(format!("invalid Rust cfg {line}: {error}")))
        })
        .collect()
}

impl Project {
    /// Compute selected dependencies without fetching, updating locks, or building.
    ///
    /// Rust cfg predicates use `target_cfg` when supplied, otherwise
    /// `rustc --print cfg --target`; exact triples need no subprocess.
    /// Overlapping active variants are errors, never overrides.
    pub fn resolve(&self, options: &PlanOptions) -> Result<ResolvedPlan> {
        if options.target.is_empty() || options.host.is_empty() {
            return Err(invalid(
                "plan requires explicit nonempty host and target triples",
            ));
        }
        let declarations = &self.declarations;
        declarations.validate()?;
        let available = declarations.feature_names();
        for feature in &options.features {
            if !available.contains(feature) {
                return Err(invalid(format!(
                    "unknown manifest feature {feature}; available: {}",
                    available.iter().cloned().collect::<Vec<_>>().join(", ")
                )));
            }
        }
        let cfg = if declarations
            .target
            .keys()
            .any(|key| key.starts_with("cfg("))
        {
            match &options.target_cfg {
                Some(values) => values
                    .iter()
                    .map(|value| {
                        value.parse::<cargo_platform::Cfg>().map_err(|error| {
                            invalid(format!("invalid target cfg {value:?}: {error}"))
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
                None => target_cfg(&options.target)?,
            }
        } else {
            Vec::new()
        };
        let mut candidates = declarations
            .dependencies
            .iter()
            .map(|(name, dependency)| (name.clone(), (name.clone(), dependency)))
            .collect::<BTreeMap<_, _>>();
        for (condition, table) in &declarations.target {
            let platform = condition
                .parse::<cargo_platform::Platform>()
                .map_err(|error| invalid(error.to_string()))?;
            if platform.matches(&options.target, &cfg) {
                for (name, dependency) in &table.dependencies {
                    if candidates
                        .insert(
                            name.clone(),
                            (Declarations::target_id(condition, name), dependency),
                        )
                        .is_some()
                    {
                        return Err(invalid(format!(
                            "{name}: multiple declarations match target {}; use disjoint target tables",
                            options.target
                        )));
                    }
                }
            }
        }
        let suppressed = declarations
            .features
            .values()
            .flatten()
            .filter_map(|entry| entry.strip_prefix("dep:"))
            .collect::<BTreeSet<_>>();
        let mut optional = declarations
            .dependencies
            .iter()
            .filter(|(_, dependency)| dependency.selection().optional)
            .map(|(name, _)| name.clone())
            .collect::<BTreeSet<_>>();
        for table in declarations.target.values() {
            optional.extend(
                table
                    .dependencies
                    .iter()
                    .filter(|(_, dep)| dep.selection().optional)
                    .map(|(name, _)| name.clone()),
            );
        }
        let mut queue = VecDeque::from(options.features.clone());
        if options.default_features && declarations.features.contains_key("default") {
            queue.push_back("default".into());
        }
        if options.all_features {
            queue.extend(declarations.features.keys().cloned());
            queue.extend(
                optional
                    .iter()
                    .filter(|name| !suppressed.contains(name.as_str()))
                    .cloned(),
            );
        }
        let mut expanded = BTreeSet::new();
        let mut activated = BTreeMap::<String, BTreeSet<String>>::new();
        let mut native = BTreeMap::<String, BTreeSet<String>>::new();
        for (name, (_, dependency)) in &candidates {
            if !dependency.selection().optional {
                activated
                    .entry(name.clone())
                    .or_default()
                    .insert("required dependency".into());
            }
        }
        while let Some(feature) = queue.pop_front() {
            if let Some(name) = feature.strip_prefix("dep:") {
                activated
                    .entry(name.into())
                    .or_default()
                    .insert(format!("feature {feature}"));
            } else if let Some((name, value)) = feature.split_once('/') {
                activated
                    .entry(name.into())
                    .or_default()
                    .insert(format!("feature {feature}"));
                native.entry(name.into()).or_default().insert(value.into());
            } else if expanded.insert(feature.clone()) {
                if let Some(entries) = declarations.features.get(&feature) {
                    queue.extend(entries.clone());
                } else if optional.contains(&feature) && !suppressed.contains(feature.as_str()) {
                    activated
                        .entry(feature.clone())
                        .or_default()
                        .insert(format!("optional feature {feature}"));
                } else {
                    return Err(invalid(format!("unknown or unavailable feature {feature}")));
                }
            }
        }
        let mut pending = activated.keys().cloned().collect::<VecDeque<_>>();
        while let Some(name) = pending.pop_front() {
            let Some((_, dependency)) = candidates.get(&name) else {
                // A manifest feature may name a dependency for another target.
                let exists = declarations
                    .target
                    .values()
                    .any(|table| table.dependencies.contains_key(&name));
                if exists {
                    continue;
                }
                return Err(invalid(format!("unknown dependency {name}")));
            };
            for edge in &dependency.selection().depends {
                if !candidates.contains_key(edge) {
                    return Err(invalid(format!(
                        "{name}: dependency {edge} is unavailable for target {}",
                        options.target
                    )));
                }
                if !activated.contains_key(edge) {
                    pending.push_back(edge.clone());
                }
                activated
                    .entry(edge.clone())
                    .or_default()
                    .insert(format!("required by {name}"));
            }
        }
        let active = activated
            .keys()
            .filter(|name| candidates.contains_key(*name))
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut nodes = BTreeMap::new();
        for (name, reasons) in activated {
            let Some((id, dependency)) = candidates.get(&name) else {
                continue;
            };
            let role = dependency.selection().role;
            let mut selected_features = native.remove(&name).unwrap_or_default();
            let mut defines = BTreeMap::new();
            if let Dependency::Source(source) = dependency {
                selected_features.extend(source.features.iter().cloned());
                if let Some(recipe) = &source.build {
                    if source.default_features {
                        selected_features.extend(recipe.default_features.clone());
                    }
                    defines = recipe.defines.clone();
                    let mut feature_defines = BTreeMap::new();
                    for feature in &selected_features {
                        let values = recipe.features.get(feature).ok_or_else(|| {
                            invalid(format!("{name}: unknown native feature {feature}"))
                        })?;
                        for (key, value) in values {
                            if feature_defines
                                .get(key)
                                .is_some_and(|existing| existing != value)
                            {
                                return Err(invalid(format!(
                                    "{name}: conflicting values for CMake option {key}"
                                )));
                            }
                            feature_defines.insert(key.clone(), value.clone());
                        }
                    }
                    defines.extend(feature_defines);
                } else if !selected_features.is_empty() {
                    return Err(invalid(format!(
                        "{name}: native features require a source build recipe"
                    )));
                }
            } else if !selected_features.is_empty() {
                return Err(invalid(format!(
                    "{name}: provider features must be declared in its locked provider selection, not forwarded from manifest features"
                )));
            }
            let mut depends = Vec::new();
            for edge in dependency.selection().depends.iter().chain(
                dependency
                    .selection()
                    .optional_depends
                    .iter()
                    .filter(|edge| active.contains(*edge)),
            ) {
                let (edge_id, edge_dep) = &candidates[edge];
                if role == BuildRole::Host && edge_dep.selection().role == BuildRole::Target {
                    return Err(invalid(format!(
                        "host dependency {name} cannot depend on target dependency {edge}"
                    )));
                }
                depends.push(edge_id.clone());
            }
            depends.sort();
            depends.dedup();
            nodes.insert(
                id.clone(),
                ResolvedDependency {
                    name,
                    id: id.clone(),
                    role,
                    target: if role == BuildRole::Host {
                        options.host.clone()
                    } else {
                        options.target.clone()
                    },
                    depends,
                    features: selected_features.into_iter().collect(),
                    defines,
                    reasons: reasons.into_iter().collect(),
                    declaration: (*dependency).clone(),
                },
            );
        }
        let mut result = Vec::new();
        let mut done = BTreeSet::new();
        while !nodes.is_empty() {
            let ready = nodes
                .iter()
                .filter(|(_, node)| node.depends.iter().all(|id| done.contains(id)))
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            if ready.is_empty() {
                return Err(invalid(format!(
                    "native dependency cycle: {}",
                    nodes
                        .values()
                        .map(|node| node.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            for id in ready {
                done.insert(id.clone());
                result.push(nodes.remove(&id).expect("ready node"));
            }
        }
        Ok(ResolvedPlan {
            features: expanded.into_iter().collect(),
            dependencies: result,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(text: &str, features: &[&str]) -> Result<ResolvedPlan> {
        let project = Project::parse("woodshed.toml", text)?;
        let mut options = PlanOptions::new("x86_64-pc-windows-msvc", "aarch64-unknown-linux-gnu");
        options.features = features.iter().map(|value| value.to_string()).collect();
        project.resolve(&options)
    }

    #[test]
    fn acquisition_only_sources_support_composed_and_empty_features() {
        let text = r#"
        [features]
        cpu = []
        frontend = ["dep:frontend"]
        dialect = ["frontend", "dep:dialect"]
        [dependencies.root]
        path = "root"
        optional-depends = ["frontend", "dialect"]
        [dependencies.frontend]
        path = "frontend"
        optional = true
        [dependencies.dialect]
        path = "dialect"
        optional = true
        depends = ["frontend"]
        "#;
        let cpu = plan(text, &["cpu"]).unwrap();
        assert_eq!(cpu.dependencies.len(), 1);
        assert_eq!(cpu.dependencies[0].name, "root");
        let selected = plan(text, &["cpu", "dialect"]).unwrap();
        assert_eq!(
            selected
                .dependencies
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            ["frontend", "dialect", "root"]
        );
        assert!(selected.dependencies.iter().all(|node| {
            matches!(&node.declaration, Dependency::Source(source) if source.build.is_none())
        }));
        assert_eq!(selected.dependencies[2].depends, ["dialect", "frontend"]);
    }

    #[test]
    fn optional_features_defaults_and_native_defines_are_resolved() {
        let text = r#"
        [features]
        default = ["fast"]
        fast = ["dep:codec", "codec/simd"]
        [dependencies.codec]
        path = "codec"
        optional = true
        default-features = false
        build = { cmake = "Codec", targets = ["Codec::Codec"], defines = { SIMD = "OFF" }, features = { simd = { SIMD = "ON" }, tests = { TESTS = "ON" } }, default-features = ["tests"] }
        [dependencies.unused]
        path = "unused"
        optional = true
        "#;
        let project = Project::parse("woodshed.toml", text).unwrap();
        let options = PlanOptions::new("host", "target");
        let result = project.resolve(&options).unwrap();
        assert_eq!(result.dependencies.len(), 1);
        assert_eq!(result.dependencies[0].name, "codec");
        assert_eq!(result.dependencies[0].features, ["simd"]);
        assert_eq!(result.dependencies[0].defines["SIMD"], "ON");
        assert!(!result.dependencies[0].defines.contains_key("TESTS"));
        let mut options = options;
        options.default_features = false;
        assert!(project.resolve(&options).unwrap().dependencies.is_empty());
        options.features = vec!["unused".into()];
        assert_eq!(
            project.resolve(&options).unwrap().dependencies[0].name,
            "unused"
        );
        options.features = vec!["codec".into()];
        assert!(
            project.resolve(&options).is_err(),
            "dep: suppresses implicit optional feature"
        );
        options.features.clear();
        options.all_features = true;
        assert_eq!(project.resolve(&options).unwrap().dependencies.len(), 2);
        let conflict = text
            .replace("TESTS = \"ON\"", "SIMD = \"OFF\"")
            .replace("default-features = false", "default-features = true");
        assert!(
            Project::parse("woodshed.toml", &conflict)
                .unwrap()
                .resolve(&PlanOptions::new("host", "target"))
                .is_err()
        );
    }

    #[test]
    fn target_variants_have_independent_lock_ids_and_do_not_activate_elsewhere() {
        let text = r#"
        [features]
        extra = ["dep:platform"]
        [target.aarch64-unknown-linux-gnu.dependencies.platform]
        path = "linux"
        optional = true
        [target.x86_64-pc-windows-msvc.dependencies.platform]
        path = "windows"
        optional = true
        "#;
        let project = Project::parse("woodshed.toml", text).unwrap();
        assert_eq!(project.declarations.all_dependencies().len(), 2);
        let result = plan(text, &["extra"]).unwrap();
        assert_eq!(result.dependencies.len(), 1);
        assert_eq!(
            result.dependencies[0].id,
            Declarations::target_id("aarch64-unknown-linux-gnu", "platform")
        );
        assert!(
            matches!(&result.dependencies[0].declaration, Dependency::Source(source) if source.path == Some("linux".into()))
        );
        let mut options = PlanOptions::new("host", "wasm32-unknown-unknown");
        options.features = vec!["extra".into(), "platform".into()];
        // dep: disables the implicit platform feature, but an explicit named
        // feature referring to an unavailable target dependency is a no-op.
        options.features = vec!["extra".into()];
        assert!(project.resolve(&options).unwrap().dependencies.is_empty());
    }

    #[test]
    fn cfg_predicates_use_target_not_host() {
        let text = r#"
        [target.'cfg(all(target_os = "linux", target_arch = "aarch64"))'.dependencies.native]
        path = "linux"
        [target.'cfg(windows)'.dependencies.native]
        path = "windows"
        "#;
        let result = plan(text, &[]).unwrap();
        assert_eq!(result.dependencies.len(), 1);
        assert!(
            matches!(&result.dependencies[0].declaration, Dependency::Source(source) if source.path == Some("linux".into()))
        );
    }

    #[test]
    fn graph_activates_optional_prerequisites_in_topological_order_and_tracks_roles() {
        let text = r#"
        [dependencies.a]
        path = "consumer"
        depends = ["z"]
        [dependencies.z]
        path = "generator"
        optional = true
        role = "host"
        "#;
        let result = plan(text, &[]).unwrap();
        assert_eq!(
            result
                .dependencies
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            ["z", "a"]
        );
        assert_eq!(result.dependencies[0].target, "x86_64-pc-windows-msvc");
        assert_eq!(result.dependencies[1].target, "aarch64-unknown-linux-gnu");
        assert_eq!(result.dependencies[0].reasons, ["required by a"]);
    }

    #[test]
    fn optional_edges_order_only_selected_dependencies_and_use_actual_target_cfg() {
        let text = r#"
[features]
extra = ["dep:z"]
[dependencies.a]
path = "a"
optional-depends = ["z"]
[dependencies.z]
path = "z"
optional = true
[target.'cfg(target_feature = "simd128")'.dependencies.simd]
path = "simd"
"#;
        let project = Project::parse("woodshed.toml", text).unwrap();
        let mut options = PlanOptions::new("host", "wasm32-unknown-unknown");
        options.target_cfg = Some(vec!["target_feature=\"simd128\"".into()]);
        let result = project.resolve(&options).unwrap();
        assert_eq!(
            result
                .dependencies
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "simd"]
        );
        assert!(result.dependencies[0].depends.is_empty());
        options.features = vec!["extra".into()];
        let result = project.resolve(&options).unwrap();
        let a = result
            .dependencies
            .iter()
            .position(|node| node.name == "a")
            .unwrap();
        let z = result
            .dependencies
            .iter()
            .position(|node| node.name == "z")
            .unwrap();
        assert!(z < a);
        assert_eq!(result.dependencies[a].depends, ["z"]);
        options.target_cfg = Some(vec![]);
        assert!(
            !project
                .resolve(&options)
                .unwrap()
                .dependencies
                .iter()
                .any(|node| node.name == "simd")
        );
    }

    #[test]
    fn graph_rejects_cycles_unavailable_edges_role_errors_and_ambiguous_targets() {
        for text in [
            "[dependencies.a]\npath='a'\ndepends=['b']\n[dependencies.b]\npath='b'\ndepends=['a']",
            "[dependencies.a]\npath='a'\ndepends=['missing']",
            "[dependencies.a]\npath='a'\nrole='host'\ndepends=['b']\n[dependencies.b]\npath='b'",
            "[dependencies.a]\npath='a'\n[target.aarch64-unknown-linux-gnu.dependencies.a]\npath='b'",
            "[dependencies.a]\npath='a'\ndepends=['b']\n[target.x86_64-pc-windows-msvc.dependencies.b]\npath='b'",
            "[features]\ninvalid=['dep:missing']\n[dependencies.a]\npath='a'",
            "[dependencies.a]\npath='a'\nbuild={cmake='A',targets=['A::A'],features={bad={CMAKE_INSTALL_PREFIX='no'}}}",
            "[dependencies.a]\npath='a'\nbuild={cmake='A',targets=['A::A'],workspace={name='a',reconfigure=['TYPO']}}",
            "[dependencies.a]\npath='a'\nbuild={cmake='A',targets=['A::A'],workspace={name='a',dependencies=['missing']}}",
            "[dependencies.a]\npath='a'\ndepends=['b']\nbuild={cmake='A',targets=['A::A'],workspace={name='a',dependencies=['b']}}\n[dependencies.b]\npath='b'\nrole='host'",
            "[dependencies.a]\npath='a'\nbuild={cmake='A',targets=['A::A'],env=['INVALID=VALUE']}",
            "[dependencies.a]\npath='a'\nbuild={cmake='A',targets=['A::A'],defines={FETCHCONTENT_FULLY_DISCONNECTED='OFF'}}",
            "[dependencies.a]\npath='a'\nurl='https://example.test/a.zip'\nversion='1'",
            "[dependencies.a]\npath='a'\nunknown=true",
            "[target.'cfg(feature = \"fast\")'.dependencies.a]\npath='a'",
            "[target.'cfg(any(unix, debug_assertions))'.dependencies.a]\npath='a'",
        ] {
            assert!(plan(text, &[]).is_err(), "{text}");
        }
    }

    #[test]
    fn providers_share_optional_role_and_graph_policy_without_substitution() {
        let text = r#"
        [features]
        native = ["dep:compression"]
        [dependencies.compression]
        provider = "vcpkg"
        package = "zlib"
        version = "1.3.1"
        triplet = "x64-windows"
        features = ["native-feature"]
        default-features = false
        optional = true
        role = "host"
        cmake = { package = "ZLIB", targets = ["ZLIB::ZLIB"] }
        "#;
        assert!(plan(text, &[]).unwrap().dependencies.is_empty());
        let result = plan(text, &["native"]).unwrap();
        assert_eq!(result.dependencies[0].role, BuildRole::Host);
        assert!(matches!(&result.dependencies[0].declaration,
                    Dependency::Package(package)
                    if matches!(package.as_ref(), Package::Vcpkg { features, default_features: false, .. } if features == &["native-feature"])));
        assert!(
            plan(
                &text.replace(
                    "native = [\"dep:compression\"]",
                    "native = [\"compression/not-locked\"]"
                ),
                &["native"]
            )
            .is_err()
        );
        let external_graph = text
            .replace("optional = true", "depends = ['source']\noptional = true")
            + "\n[dependencies.source]\npath = 'source'\n";
        assert!(plan(&external_graph, &["native"]).is_err());
    }
}
