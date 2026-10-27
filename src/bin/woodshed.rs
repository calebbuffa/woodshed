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
    for path in [
        "woodshed.toml",
        "native/woodshed.toml",
        "native/versions.toml",
    ] {
        if std::path::Path::new(path).is_file() {
            return path.into();
        }
    }
    "woodshed.toml".into()
}

fn declaration_format(path: &std::path::Path) -> Result<bool, String> {
    if path.file_name().is_some_and(|name| name == "woodshed.toml") {
        return Ok(true);
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.to_string()),
    };
    Ok(toml::from_str::<toml::Value>(&text)
        .map_err(|error| error.to_string())?
        .get("dependencies")
        .is_some())
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
enum Commands {
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
        #[arg(long)]
        version: String,
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
        #[arg(long, conflicts_with = "locked")]
        dry_run: bool,
    },
    /// Import legacy pins into declarations/lock without changing source identities.
    Migrate {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    /// Bundle one Cargo application and its declared native resources.
    Bundle {
        #[arg(long)]
        output: PathBuf,
        #[arg(last = true, required = true)]
        cargo_options: Vec<std::ffi::OsString>,
    },
}

fn finish(plan: woodshed::pin::Plan, dry_run: bool) -> Result<(), String> {
    if dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(plan.changes()).map_err(|error| error.to_string())?
        );
    } else {
        let count = plan.changes().len();
        plan.apply().map_err(|error| error.to_string())?;
        println!("updated {count} native source pin(s)");
    }
    Ok(())
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
        let build = if let Some(cmake) = package.cmake_package {
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
        return Ok(Dependency::Source(woodshed::project::Source {
            version: Some(version.into()),
            github: source.github,
            git: source.git,
            url: source.url,
            reference: source.reference,
            file: source.file,
            markers,
            build,
            ..Default::default()
        }));
    };
    if !package.defines.is_empty() || package.toolchain.is_some() {
        return Err(
            "--define/--toolchain are source-build options, not package-provider options".into(),
        );
    }
    if source.git.is_some()
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
    Ok(Dependency::Package(match provider {
        Provider::Vcpkg => {
            if package.host_profile.is_some()
                || package.build_profile.is_some()
                || package.build_missing
                || !package.options.is_empty()
            {
                return Err("Conan profile/build options cannot be used with vcpkg".into());
            }
            Package::Vcpkg {
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
    }))
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
            if declaration_format(&manifest)? {
                return finish_project(
                    woodshed::project::plan_add(
                        &manifest,
                        &name,
                        dependency(&name, &version, source, *package, markers)?,
                        &mut resolver,
                    )
                    .map_err(|error| error.to_string())?,
                    dry_run,
                );
            }
            if source.github.is_some()
                || package.provider.is_some()
                || package.cmake_package.is_some()
                || !package.target.is_empty()
                || package.dynamic
                || !package.defines.is_empty()
                || package.toolchain.is_some()
            {
                return Err("GitHub/package declarations require woodshed.toml; legacy versions.toml remains supported for explicit pins".into());
            }
            let plan = woodshed::pin::plan_add(
                &manifest,
                &name,
                &version,
                &source.into(),
                &markers,
                &mut resolver,
            )
            .map_err(|error| error.to_string())?;
            finish(plan, dry_run)
        }
        Commands::Bump {
            name,
            version,
            source,
            manifest,
            dry_run,
        } => {
            if source.github.is_some() {
                return Err("switching GitHub repositories uses woodshed.toml editing followed by woodshed lock".into());
            }
            if declaration_format(&manifest)? {
                return finish_project(
                    woodshed::project::plan_bump(
                        &manifest,
                        &name,
                        &version,
                        &source.into(),
                        &mut resolver,
                    )
                    .map_err(|error| error.to_string())?,
                    dry_run,
                );
            }
            let plan =
                woodshed::pin::plan_bump(&manifest, &name, &version, &source.into(), &mut resolver)
                    .map_err(|error| error.to_string())?;
            finish(plan, dry_run)
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
        } => {
            if locked {
                woodshed::project::Project::load(&manifest)
                    .and_then(|project| project.locked())
                    .map_err(|error| error.to_string())?;
                println!("native dependency lock matches declarations");
                Ok(())
            } else {
                finish_project(
                    woodshed::project::plan_lock(&manifest, &mut resolver)
                        .map_err(|error| error.to_string())?,
                    dry_run,
                )
            }
        }
        Commands::Migrate { manifest, dry_run } => finish_project(
            woodshed::project::plan_migrate(&manifest).map_err(|error| error.to_string())?,
            dry_run,
        ),
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

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
