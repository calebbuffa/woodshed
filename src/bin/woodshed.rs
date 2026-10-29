//! Native dependency maintenance and application bundling.
use clap::{Args, Parser, Subcommand};
use std::{path::PathBuf, process::ExitCode};
use woodshed::pin::{Online, Selection};

#[derive(Parser)]
#[command(version, about = "Add, bump, and provision pinned native dependencies")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Args)]
struct Source {
    /// Local source directory, relative to woodshed.toml.
    #[arg(long, conflicts_with_all = ["url", "git", "github", "reference", "file"])]
    path: Option<PathBuf>,
    /// Archive URL template.
    #[arg(long, conflicts_with_all = ["git", "github"])]
    url: Option<String>,
    /// Git repository; resolve --ref (or version) to an immutable commit.
    #[arg(long, conflicts_with_all = ["url", "github"])]
    git: Option<String>,
    /// GitHub source release, as owner/repository (no checksum entry needed).
    #[arg(long, conflicts_with_all = ["url", "git"])]
    github: Option<String>,
    /// Upstream tag, branch, or commit to resolve now.
    #[arg(long = "ref")]
    reference: Option<String>,
    /// Cached archive filename template.
    #[arg(long, conflicts_with = "git")]
    file: Option<String>,
}

#[derive(Args)]
struct PlanArgs {
    #[arg(long, default_value_os_t = default_manifest())]
    manifest: PathBuf,
    /// Rust host triple; defaults to rustc's host.
    #[arg(long)]
    host: Option<String>,
    /// Rust target triple; defaults to the host.
    #[arg(long)]
    target: Option<String>,
    #[arg(long, value_delimiter = ',')]
    features: Vec<String>,
    #[arg(long)]
    no_default_features: bool,
    #[arg(long)]
    all_features: bool,
    #[arg(long)]
    json: bool,
}

#[derive(clap::ValueEnum, Clone)]
enum Provider {
    Vcpkg,
    Conan,
}

#[derive(Args)]
struct PackageOptions {
    /// Use a native package manager instead of source provisioning.
    #[arg(long)]
    provider: Option<Provider>,
    /// Port name or exact Conan reference; defaults to the dependency name.
    #[arg(long, requires = "provider")]
    package: Option<String>,
    /// vcpkg target triplet.
    #[arg(long, requires = "provider")]
    triplet: Option<String>,
    /// Exported CMake CONFIG package name.
    #[arg(long)]
    cmake_package: Option<String>,
    /// Exported CMake target; repeat to select multiple ordered targets.
    #[arg(long)]
    target: Vec<String>,
    /// Installed executable name under bin/ (source builds only).
    #[arg(long = "tool")]
    tools: Vec<String>,
    #[arg(long, requires = "provider")]
    host_profile: Option<PathBuf>,
    #[arg(long, requires = "provider")]
    build_profile: Option<PathBuf>,
    #[arg(long, requires = "provider")]
    build_missing: bool,
    #[arg(long)]
    dynamic: bool,
    /// Project-specific CMake option NAME=VALUE (source builds only).
    #[arg(long = "define")]
    defines: Vec<String>,
    /// Source-build CMake toolchain file relative to woodshed.toml.
    #[arg(long)]
    toolchain: Option<PathBuf>,
    #[arg(long = "feature", requires = "provider")]
    features: Vec<String>,
    #[arg(long = "option", requires = "provider")]
    options: Vec<String>,
    #[arg(long, requires = "provider")]
    no_default_features: bool,
}

fn default_manifest() -> PathBuf {
    for path in ["woodshed.toml", "native/woodshed.toml"] {
        if std::path::Path::new(path).is_file() {
            return path.into();
        }
    }
    "woodshed.toml".into()
}

impl From<Source> for Selection {
    fn from(source: Source) -> Self {
        Self {
            url: source.url,
            git: source.git,
            reference: source.reference,
            file: source.file,
        }
    }
}

#[derive(Subcommand)]
enum CacheCommand {
    /// Export a verified installation with explicit publisher compatibility/export metadata.
    Export {
        id: String,
        #[arg(long)]
        output: PathBuf,
        /// JSON ArtifactMetadata with explicit target/ABI/configuration/linkage/CRT.
        #[arg(long)]
        metadata: PathBuf,
        /// Explicit CMake toolchain for cross-target metadata evaluation.
        #[arg(long)]
        toolchain: Option<PathBuf>,
    },
    /// List owned artifacts with byte usage, references and full content verification.
    Inspect,
    /// Verify one artifact without repair.
    Verify { id: String },
    /// Prune only explicit unreferenced artifact IDs; preview unless --apply is given.
    Prune {
        #[arg(required = true)]
        ids: Vec<String>,
        #[arg(long)]
        apply: bool,
    },
    /// Retain an artifact for a named custom integration.
    Pin { id: String, name: String },
    /// Release a named explicit retention pin.
    Unpin { name: String },
}

#[derive(Subcommand)]
enum Commands {
    /// Inspect, verify and safely prune immutable source installations.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// Compile and link an isolated C/C++ fixture with the selected native toolchain.
    Doctor {
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        toolchain: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Explain the last successful source-build decision, without rebuilding.
    CacheExplain {
        /// Resolved dependency ID, including any target variant suffix.
        id: String,
        #[arg(long, default_value_os_t = default_manifest())]
        manifest: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Opt in to Cargo wiring; never overwrites an existing custom build.rs.
    Setup {
        #[arg(long, default_value = "Cargo.toml")]
        cargo_manifest: PathBuf,
        /// Use a local Woodshed checkout instead of a registry dependency.
        #[arg(long)]
        woodshed_path: Option<PathBuf>,
        /// Preview both files without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Add a pinned archive or Git source.
    Add {
        name: String,
        #[arg(long, required_unless_present_any = ["path", "reference"])]
        version: Option<String>,
        #[command(flatten)]
        source: Source,
        #[command(flatten)]
        package: Box<PackageOptions>,
        #[arg(long = "marker")]
        markers: Vec<String>,
        #[arg(long, default_value_os_t = default_manifest())]
        manifest: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    /// Bump a root and all declared dependent sources in one transaction.
    Bump {
        name: String,
        version: String,
        #[command(flatten)]
        source: Source,
        #[arg(long, default_value_os_t = default_manifest())]
        manifest: PathBuf,
        /// Resolve/hash the complete plan and print it without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Verify pinned archives and Git snapshots.
    Verify {
        #[arg(long, default_value_os_t = default_manifest())]
        manifest: PathBuf,
    },
    /// Resolve user declarations into an immutable woodshed.lock.
    Lock {
        #[arg(long, default_value_os_t = default_manifest())]
        manifest: PathBuf,
        /// Check the existing lock without changing files or resolving sources.
        #[arg(long)]
        locked: bool,
        /// Previous declarations matching the lock; preserve pins without network access.
        #[arg(long, conflicts_with = "locked")]
        preserve_pins_from: Option<PathBuf>,
        #[arg(long, conflicts_with = "locked")]
        dry_run: bool,
    },
    /// Show selected native dependencies in prerequisite-first execution order.
    Tree(PlanArgs),
    /// Explain selected features, dependency edges and source/provider intent.
    Explain {
        /// Restrict explanation to one selected dependency.
        name: Option<String>,
        #[command(flatten)]
        selection: PlanArgs,
    },
    /// Bundle one Cargo application and its declared native resources.
    Bundle {
        #[arg(long)]
        output: PathBuf,
        #[arg(last = true, required = true)]
        cargo_options: Vec<std::ffi::OsString>,
    },
}

fn finish_project(plan: woodshed::project::Update, dry_run: bool) -> Result<(), String> {
    if dry_run {
        println!("{}\n{}", plan.manifest(), plan.lock());
    } else {
        plan.apply().map_err(|error| error.to_string())?;
        println!("updated native dependency declarations and lock");
    }
    Ok(())
}

fn dependency(
    name: &str,
    version: &str,
    source: Source,
    package: PackageOptions,
    markers: Vec<String>,
) -> Result<woodshed::project::Dependency, String> {
    use woodshed::project::{CmakePackage, Dependency, Package, PackageLinkage};
    let Some(provider) = package.provider else {
        let build = if package.cmake_package.is_some() || !package.tools.is_empty() {
            let cmake = package.cmake_package.unwrap_or_default();
            let mut defines = std::collections::BTreeMap::new();
            for define in package.defines {
                let (key, value) = define
                    .split_once('=')
                    .ok_or("--define expects NAME=VALUE")?;
                if defines.insert(key.into(), value.into()).is_some() {
                    return Err(format!("duplicate --define {key}"));
                }
            }
            Some(Box::new(woodshed::project::CmakeRecipe {
                cmake,
                targets: package.target,
                linkage: if package.dynamic {
                    PackageLinkage::Dynamic
                } else {
                    PackageLinkage::Static
                },
                defines,
                toolchain: package.toolchain,
                inputs: Vec::new(),
                env: Vec::new(),
                tools: package.tools,
                features: Default::default(),
                default_features: Vec::new(),
                workspace: None,
                msvc_runtime: None,
            }))
        } else {
            if !package.target.is_empty()
                || package.dynamic
                || !package.defines.is_empty()
                || package.toolchain.is_some()
            {
                return Err("source build options require --cmake-package and --target".into());
            }
            None
        };
        return Ok(Dependency::Source(Box::new(woodshed::project::Source {
            version: (!version.is_empty()).then(|| version.into()),
            path: source.path,
            github: source.github,
            git: source.git,
            url: source.url,
            reference: source.reference,
            file: source.file,
            markers,
            build,
            ..Default::default()
        })));
    };
    if !package.defines.is_empty() || package.toolchain.is_some() || !package.tools.is_empty() {
        return Err(
            "--define/--toolchain are source-build options, not package-provider options".into(),
        );
    }
    if source.git.is_some()
        || source.path.is_some()
        || source.github.is_some()
        || source.url.is_some()
        || source.file.is_some()
        || source.reference.is_some()
        || !markers.is_empty()
    {
        return Err(
            "package providers cannot be combined with source-selection options or source markers"
                .into(),
        );
    }
    let cmake = CmakePackage {
        package: package
            .cmake_package
            .ok_or("--provider requires --cmake-package")?,
        targets: package.target,
        linkage: if package.dynamic {
            PackageLinkage::Dynamic
        } else {
            PackageLinkage::Static
        },
    };
    let package_name = package.package.unwrap_or_else(|| name.into());
    Ok(Dependency::Package(Box::new(match provider {
        Provider::Vcpkg => {
            if package.host_profile.is_some()
                || package.build_profile.is_some()
                || package.build_missing
                || !package.options.is_empty()
            {
                return Err("Conan profile/build options cannot be used with vcpkg".into());
            }
            Package::Vcpkg {
                selection: Default::default(),
                package: package_name,
                version: version.into(),
                triplet: package.triplet.ok_or("vcpkg requires --triplet")?,
                baseline: None,
                features: package.features,
                default_features: !package.no_default_features,
                cmake,
            }
        }
        Provider::Conan => {
            if package.triplet.is_some() {
                return Err("--triplet is a vcpkg option".into());
            }
            if !package.features.is_empty() || package.no_default_features {
                return Err("vcpkg feature options cannot be used with Conan".into());
            }
            if package_name.contains('/') {
                return Err("use --package NAME and --version VERSION for Conan, not two competing version selectors".into());
            }
            Package::Conan {
                selection: Default::default(),
                package: format!("{package_name}/{version}"),
                host_profile: package
                    .host_profile
                    .ok_or("Conan requires --host-profile")?,
                build_profile: package
                    .build_profile
                    .ok_or("Conan requires --build-profile")?,
                options: package.options,
                build_missing: package.build_missing,
                cmake,
            }
        }
    })))
}
fn run(cli: Cli) -> Result<(), String> {
    let mut resolver = Online::default();
    match cli.command {
        Commands::Setup {
            cargo_manifest,
            woodshed_path,
            dry_run,
        } => {
            let plan = woodshed::setup::plan(&cargo_manifest, woodshed_path.as_deref())
                .map_err(|error| error.to_string())?;
            if dry_run {
                println!(
                    "# Cargo.toml\n{}\n# build.rs\n{}",
                    plan.cargo_toml(),
                    plan.build_rs()
                );
            } else {
                plan.apply().map_err(|error| error.to_string())?;
                println!("Configured Cargo build dependency and universal build.rs bridge");
            }
            Ok(())
        }
        Commands::Add {
            name,
            version,
            source,
            package,
            markers,
            manifest,
            dry_run,
        } => {
            let version = version.as_deref().unwrap_or("");
            if source.path.is_some() && !version.is_empty() {
                return Err("path sources do not declare a remote --version".into());
            }
            finish_project(
                woodshed::project::plan_add(
                    &manifest,
                    &name,
                    dependency(&name, version, source, *package, markers)?,
                    &mut resolver,
                )
                .map_err(|error| error.to_string())?,
                dry_run,
            )
        }
        Commands::Bump {
            name,
            version,
            source,
            manifest,
            dry_run,
        } => {
            if source.path.is_some() || source.github.is_some() {
                return Err("switching GitHub repositories uses woodshed.toml editing followed by woodshed lock".into());
            }
            finish_project(
                woodshed::project::plan_bump(
                    &manifest,
                    &name,
                    &version,
                    &source.into(),
                    &mut resolver,
                )
                .map_err(|error| error.to_string())?,
                dry_run,
            )
        }
        Commands::Verify { manifest } => {
            let mut failed = 0;
            for (name, outcome) in
                woodshed::pin::verify(&manifest).map_err(|error| error.to_string())?
            {
                match outcome {
                    Ok(()) => println!("ok {name}"),
                    Err(error) => {
                        failed += 1;
                        eprintln!("failed {name}: {error}");
                    }
                }
            }
            if failed == 0 {
                Ok(())
            } else {
                Err(format!("{failed} source(s) failed verification"))
            }
        }
        Commands::Lock {
            manifest,
            locked,
            dry_run,
            preserve_pins_from,
        } => {
            if locked {
                woodshed::project::Project::load(&manifest)
                    .and_then(|project| project.locked())
                    .map_err(|error| error.to_string())?;
                println!("native dependency lock matches declarations");
                Ok(())
            } else if let Some(previous) = preserve_pins_from {
                let previous =
                    std::fs::read_to_string(previous).map_err(|error| error.to_string())?;
                finish_project(
                    woodshed::project::plan_relock(&manifest, &previous)
                        .map_err(|error| error.to_string())?,
                    dry_run,
                )
            } else {
                finish_project(
                    woodshed::project::plan_lock(&manifest, &mut resolver)
                        .map_err(|error| error.to_string())?,
                    dry_run,
                )
            }
        }
        Commands::Cache { command } => {
            let cache = woodshed::Cache::from_environment().map_err(|error| error.to_string())?;
            let value = match command {
                CacheCommand::Export {
                    id,
                    output,
                    metadata,
                    toolchain,
                } => {
                    let metadata: woodshed::ArtifactMetadata = serde_json::from_slice(
                        &std::fs::read(metadata).map_err(|error| error.to_string())?,
                    )
                    .map_err(|error| error.to_string())?;
                    cache
                        .export_artifact(&id, &output, &metadata, toolchain.as_deref())
                        .map_err(|error| error.to_string())?;
                    return Ok(());
                }
                CacheCommand::Inspect => serde_json::to_value(
                    cache
                        .inspect_artifacts()
                        .map_err(|error| error.to_string())?,
                ),
                CacheCommand::Verify { id } => {
                    let report = cache
                        .verify_artifact(&id)
                        .map_err(|error| error.to_string())?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
                    );
                    return if report.verified {
                        Ok(())
                    } else {
                        Err("artifact content verification failed".into())
                    };
                }
                CacheCommand::Prune { ids, apply } => serde_json::to_value(
                    cache
                        .prune_artifacts(&ids, !apply)
                        .map_err(|error| error.to_string())?,
                ),
                CacheCommand::Pin { id, name } => {
                    cache
                        .pin_artifact(&id, &name)
                        .map_err(|error| error.to_string())?;
                    return Ok(());
                }
                CacheCommand::Unpin { name } => {
                    cache
                        .unpin_artifact(&name)
                        .map_err(|error| error.to_string())?;
                    return Ok(());
                }
            }
            .map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        Commands::Doctor {
            target,
            toolchain,
            json,
        } => {
            let target = target
                .map(Ok)
                .unwrap_or_else(woodshed::rust_host)
                .map_err(|error| error.to_string())?;
            let report = woodshed::doctor(&woodshed::DoctorOptions { target, toolchain })
                .map_err(|error| error.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
                );
            } else {
                println!(
                    "target: {}\nphase: {}\nhealthy: {}",
                    report.target, report.phase, report.healthy
                );
                for (name, value) in &report.tools {
                    println!("{name}: {value}");
                }
                if !report.healthy {
                    println!("{}", report.diagnostics);
                }
                for advice in &report.advice {
                    println!("{advice}");
                }
            }
            if report.healthy {
                Ok(())
            } else {
                Err("native toolchain doctor failed".into())
            }
        }
        Commands::CacheExplain { id, manifest, json } => {
            let report = woodshed::Cache::from_environment()
                .and_then(|cache| cache.explain_build(&manifest, &id))
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("{id}: no successful source-build request recorded"))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
                );
            } else {
                println!("{}: {:?}", report.dependency, report.outcome);
                println!("artifact: {}", report.artifact);
                println!(
                    "workspace: {}",
                    report
                        .workspace
                        .as_deref()
                        .unwrap_or("none (prebuilt import)")
                );
                println!(
                    "compared with: {}",
                    report
                        .previous_artifact
                        .as_deref()
                        .unwrap_or("no previous successful request")
                );
                println!("changed inputs: {}", report.changed_inputs.join(", "));
            }
            Ok(())
        }
        Commands::Tree(selection) => show_plan(selection, None),
        Commands::Explain { name, selection } => show_plan(selection, name.as_deref()),
        Commands::Bundle {
            output,
            cargo_options,
        } => {
            let executable = woodshed::bundle_cargo(&output, &cargo_options)
                .map_err(|error| error.to_string())?;
            println!("Bundled {}", executable.display());
            Ok(())
        }
    }
}

fn show_plan(arguments: PlanArgs, selected: Option<&str>) -> Result<(), String> {
    let host = match arguments.host {
        Some(host) => host,
        None => {
            let output = std::process::Command::new(
                std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()),
            )
            .arg("-vV")
            .output()
            .map_err(|error| error.to_string())?;
            if !output.status.success() {
                return Err(format!(
                    "cannot inspect Rust host: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| line.strip_prefix("host: "))
                .ok_or("rustc did not report its host")?
                .to_owned()
        }
    };
    let project =
        woodshed::project::Project::load(&arguments.manifest).map_err(|error| error.to_string())?;
    let mut options = woodshed::project::PlanOptions::new(
        &host,
        arguments.target.unwrap_or_else(|| host.clone()),
    );
    options.features = arguments.features;
    options.default_features = !arguments.no_default_features;
    options.all_features = arguments.all_features;
    let mut plan = project
        .resolve(&options)
        .map_err(|error| error.to_string())?;
    if let Some(name) = selected {
        plan.dependencies.retain(|node| node.name == name);
        if plan.dependencies.is_empty() {
            return Err(format!(
                "{name}: dependency is unknown or inactive for this feature/target selection"
            ));
        }
    }
    if arguments.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&plan).map_err(|error| error.to_string())?
        );
    } else {
        println!("features: {}", plan.features.join(", "));
        for node in &plan.dependencies {
            println!("{} ({:?}, {})", node.name, node.role, node.target);
            println!("  lock: {}", node.id);
            println!("  because: {}", node.reasons.join("; "));
            if !node.depends.is_empty() {
                println!("  depends: {}", node.depends.join(", "));
            }
            if !node.features.is_empty() {
                println!("  native features: {}", node.features.join(", "));
            }
            println!(
                "  selection: {}",
                toml::to_string(&node.declaration)
                    .map_err(|error| error.to_string())?
                    .replace('\n', "; ")
            );
        }
    }
    Ok(())
}
fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
