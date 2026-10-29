# woodshed

**Add a native C/C++ dependency, lock its version, and reuse its build.**
Woodshed handles verified downloads, cache directories, and build locks.
You choose the library and how to build it.

## Choose your starting point

| What you have                                | Start here                                                           | What you supply                                                           |
| -------------------------------------------- | -------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| A source release with a CMake install target | [Source quick start](#quick-start-build-a-source-release)            | Repository/version, library names, and any project-specific CMake options |
| A package in an existing vcpkg installation  | [vcpkg recipe](#use-vcpkg-instead)                                   | Port, target triplet, and exported CMake package/target names             |
| A package in Conan 2                         | [Conan recipe](#use-conan-instead)                                   | Reference, compiler profiles, and exported CMake package/target names     |
| An already-installed system library          | [`probe` and optional features](#reference-optional-building-blocks) | Your system-library discovery policy                                      |

**Prerequisites:** Rust 1.89+, a native C/C++ compiler/SDK, and CMake (3.21+ for
package providers). Git is needed when locking GitHub/Git sources. Woodshed
does not install these tools. The recipes below are native builds; cross-builds
also need an explicit matching toolchain or package-manager profile.

## Quick start: build a source release

This example builds and links fmt for a Rust crate's native integration. Read the
three steps in about a minute; the first download/build can take longer.
Run commands in the application's directory.

### 1. Add and lock the dependency

Install the maintenance CLI once:

```text
cargo install woodshed --version 0.4.0 --features cli
```

Then add fmt and select its documented CMake export:

```text
woodshed add fmt --github fmtlib/fmt --version 11.1.4 --cmake-package fmt --target fmt::fmt --define FMT_TEST=OFF --define FMT_DOC=OFF
```

That command creates **both** `woodshed.toml` and `woodshed.lock`.

```toml
[dependencies.fmt]
version = "11.1.4"
github = "fmtlib/fmt"
build = { cmake = "fmt", targets = ["fmt::fmt"], defines = { FMT_TEST = "OFF", FMT_DOC = "OFF" } }
```

The generated lock records the exact commit, archive URL, and checksum.
**Keep both files in source control. Do not copy checksums by hand.**
If you prefer writing the TOML yourself, run `woodshed lock` afterward.

### 2. Connect it to Cargo

Preview the Cargo wiring, then apply it:

```text
woodshed setup --dry-run
woodshed setup
```

Setup adds the necessary build-dependency features and creates a universal
`build.rs`. **It never overwrites an existing custom build script** or silently
changes a workspace-inherited/custom dependency source. If you already have a
build script, add the dependency and call the bridge yourself:

```toml
[build-dependencies]
woodshed = { version = "0.4", features = ["build"] }
```

Create `build.rs` beside it:

```rust,no_run
fn main() -> woodshed::Result<()> {
    woodshed::build()
}
```

For an unpublished local checkout, use
`woodshed setup --woodshed-path "<path-to-woodshed>"` instead. No registry release
is needed for that setup.

### 3. Build normally

```text
cargo build
```

Woodshed now owns source verification, the build lock, Release-mode CMake
installation, exported-target link discovery, cache identity, artifact
completeness checks, and Cargo rerun directives. Subsequent matching builds reuse
the native installation. You do not list archive filenames or write CMake.

## Use vcpkg instead

Choose this when vcpkg already provides the library and you do not want to own
its source-build recipe. You still need an existing `VCPKG_ROOT`, the matching
compiler/SDK, CMake 3.21+, and the package's documented CMake exports.

This example is specifically **Windows x64 MSVC with the default dynamic CRT**.
Other targets need a matching triplet; do not copy `x64-windows` for Linux.

Install the CLI with the provider enabled, then add the package:

```text
cargo install woodshed --version 0.4.0 --features cli,vcpkg
woodshed add zstd --version 1.5.7 --provider vcpkg --triplet x64-windows --cmake-package zstd --target zstd::libzstd_shared --dynamic
```

This writes and locks the following declaration:

```toml
[dependencies.zstd]
provider = "vcpkg"
package = "zstd"
version = "1.5.7"
triplet = "x64-windows"
cmake = { package = "zstd", targets = ["zstd::libzstd_shared"], linkage = "dynamic" }
```

Run `woodshed setup --dry-run`, then `woodshed setup`. It enables `build` and
`vcpkg` and creates the same one-line bridge as the source recipe. For manual
integration, use:

```toml
[build-dependencies]
woodshed = { version = "0.4", features = ["build", "vcpkg"] }
```

Use this `build.rs` (no handwritten CMake):

```rust,no_run
fn main() -> woodshed::Result<()> {
    woodshed::build()
}
```

`cargo build` installs the locked package and emits its transitive link metadata.
Use your own FFI bindings to call it. **Dynamic deployment is separate:** this
provider does not stage DLLs automatically. Use `woodshed::build_from(path)` to
obtain each dependency's `installation.runtime_directories`
to arrange the loader path for local execution or package the required DLLs with
your application. See [provider details](#native-package-providers-no-handwritten-consumer-cmake).

## Use Conan instead

Choose this when you already use Conan 2 and know the compiler profiles for your
target. Woodshed invokes the existing CLI; you write no Conan recipe or CMake
consumer. You need Conan 2, CMake 3.21+, the compiler/SDK, and CMake CONFIG exports.

For a **native Linux x86_64 GCC 13** example, create `profiles/native`:

```ini
[settings]
os=Linux
arch=x86_64
compiler=gcc
compiler.version=13
compiler.libcxx=libstdc++11
compiler.cppstd=17
build_type=Release
```

Use your actual compiler version/settings, not a guessed profile. Other platforms
need different settings; cross-compilation needs separate host and build profiles.
Profiles must be self-contained, without includes or Jinja.

Create `woodshed.toml`:

```toml
[dependencies.zlib]
provider = "conan"
package = "zlib/1.3.1"
host-profile = "profiles/native"
build-profile = "profiles/native"
options = ["zlib/*:shared=False"]
build-missing = true
cmake = { package = "ZLIB", targets = ["ZLIB::ZLIB"], linkage = "static" }
```

Lock the declaration using the provider-enabled CLI:

```text
cargo install woodshed --version 0.4.0 --features cli,conan
woodshed lock
```

Run `woodshed setup --dry-run`, then `woodshed setup`. For manual integration,
use this build dependency:

```toml
[build-dependencies]
woodshed = { version = "0.4", features = ["build", "conan"] }
```

Create `build.rs`:

```rust,no_run
fn main() -> woodshed::Result<()> {
    woodshed::build()
}
```

Run `cargo build --release` to match this profile's build type. Keep the profile
with the declaration/lock; changing it requires regenerating the lock.
`build-missing = true` permits Conan to compile missing binaries rather than
requiring a prebuilt package. Conan still owns its compiler/package compatibility
and binary cache policy.

## Everyday commands and common fixes

| Goal or problem                                        | What to do                                                                             |
| ------------------------------------------------------ | -------------------------------------------------------------------------------------- |
| Add a GitHub source release                            | `woodshed add NAME --github OWNER/REPO --version VERSION`                              |
| Update a source release or vcpkg port                  | `woodshed bump NAME VERSION`                                                           |
| Change a Conan reference/options/profile               | Edit its declaration/profile, then run `woodshed lock`                                 |
| Check that the lock matches, without network or writes | `woodshed lock --locked`                                                               |
| Preview an update                                      | `woodshed bump NAME VERSION --dry-run`                                                 |
| Preview Cargo wiring without changing files            | `woodshed setup --dry-run`                                                             |
| Enable the universal build bridge                      | `woodshed setup` (add `--woodshed-path PATH` for a local checkout)                     |
| Missing or stale lock                                  | Run `woodshed lock` deliberately; normal builds never repair it                        |
| Tag is not `vVERSION`                                  | Set `ref = "release-{version}"` (or the upstream tag format), then lock                |
| Wrong library/target name                              | Use the upstream install/export documentation; package names and link names can differ |
| Missing SDK, compiler, CMake, or provider CLI          | Install/configure that prerequisite; Woodshed does not substitute a host toolchain     |
| Need an offline build                                  | Set `WOODSHED_OFFLINE=1`; required sources/packages must already be cached             |
| Need a cross-build                                     | Declare `build.toolchain` for sources, or matching package-manager profiles/triplets   |

**Two files to keep:** `woodshed.toml` is what you want; `woodshed.lock` is the
exact verified selection. Ignore the retained `woodshed.toml.pin.lock` writer
lock sidecar and `Cargo.toml.woodshed.lock` setup-writer sidecar. Do not delete
the source lock to work around an error.

### Declarative source-build options

The simple form is `build = { cmake = "Package", targets = ["Package::Target"] }`.
Static linkage and a Release native build are the defaults. An expanded recipe
looks like:

```toml
[dependencies.example.build]
cmake = "Example"
targets = ["Example::core"]
linkage = "static"
defines = { EXAMPLE_BUILD_TESTS = "OFF" }
toolchain = "toolchains/target.cmake"
inputs = ["toolchains/sdk-settings.cmake"]
env = ["EXAMPLE_SDK_MODE"]
```

`toolchain` and `inputs` are optional paths relative to `woodshed.toml`. Cross
builds require an explicit toolchain. List any extra files read by your toolchain
or project options in `inputs`: Woodshed hashes those bytes and tells Cargo when
to rerun. List additional inherited, output-affecting environment variables in
`env`; they are tracked, not assigned. Standard compiler/linker flags, search paths,
SDK environment and `SOURCE_DATE_EPOCH` are already tracked. Woodshed cannot infer
arbitrary files or environment reads inside upstream CMake.
`CMAKE_*` and `BUILD_SHARED_LIBS` overrides are rejected in `defines`; use the
dedicated toolchain/linkage fields, or the low-level Rust API for custom policies.
Source recipes disable CMake package registries and standard FetchContent network
population/updates. Projects requiring vendored sources must supply them or use
upstream options for installed dependencies; Woodshed is not a network sandbox for
arbitrary build scripts.

The build key includes source identity, recipe, target, CRT, declared input bytes,
tracked compiler/SDK environment, and the bytes of selected compiler, linker,
generator and launcher executables. MSVC target recipes default to Rust's CRT;
host tools default to the dynamic CRT independently of Rust's target. An explicit
`msvc-runtime = "static"` or `"dynamic"` overrides that choice, independently of
library `linkage`; allocation ownership across CRT boundaries remains your responsibility.
A successful source installation records full tree and metadata hashes;
missing/changed files trigger repair, including same-size, same-mtime changes.
This detects local corruption, not untrusted-build provenance.
Changing options requires a matching regenerated lock, even when source pins
are unchanged.

`woodshed setup` is a separate, explicit action: `add` does not edit your Cargo
files. Setup preserves comments, merges existing compatible dependency features,
supports `--cargo-manifest PATH`, and refuses custom/disabled build scripts,
workspace-inherited dependencies, or conflicting dependency sources rather than
guessing how to change them. `--dry-run` previews both files. Each file replacement
is atomic, but the pair is not: interruption can leave an unconfigured bridge;
rerun setup to finish. Ordinary manifest-write failures remove a newly created
bridge. Existing scripts can call `woodshed::build()?` manually.

### Features, targets, local sources, and native dependency graphs

Source builds, vcpkg, and Conan declarations share `optional = true` and
`role = "host"` (the default role is `"target"`). Source recipes can declare
`depends = ["other"]` on source or provider packages. Dependency gates are resolved by Woodshed before invoking any
build system or provider; inactive nodes are not downloaded, installed, or built.

```toml
[features]
default = ["fast"]
fast = ["dep:codec", "engine/simd"]

[dependencies.engine]
path = "native/engine"
depends = ["generator"]
optional-depends = ["codec"]
default-features = false

[dependencies.engine.build]
cmake = "Engine"
targets = ["Engine::Engine"]

[dependencies.engine.build.features.simd]
ENGINE_ENABLE_SIMD = "ON"

[dependencies.codec]
github = "example/codec"
version = "1.0"
optional = true
build = { cmake = "Codec", targets = ["Codec::Codec"] }

[dependencies.generator]
path = "native/generator"
role = "host"
optional = true
build = { tools = ["codegen"] }

[target.'cfg(target_os = "linux")'.dependencies.platform]
path = "native/linux"
build = { cmake = "Platform", targets = ["Platform::Platform"] }
```

`path` is relative to `woodshed.toml`; local sources need no fake version,
checksum, Git ref, or remote access. Their current file contents (excluding
`.git`) affect installation identity, and Cargo watches the source directory.
Keep local source directories separate from generated build/cache outputs.
Source trees are built out of tree. Changes during compilation fail explicitly
instead of publishing a receipt under the wrong content identity.

Manifest features expand to other features, `dep:name` dependency gates, or
`name/native-feature` for a source recipe. Optional dependencies have an implicit
same-name feature unless mentioned through `dep:name`. Required graph edges
activate optional prerequisites too. Source recipe `build.features` maps native
features to upstream project-specific CMake definitions; `build.default-features`
lists native features enabled by default, while a dependency's `default-features`
and `features` select them. Feature definitions override base `build.defines`;
two enabled features requesting different values for the same option are errors.
No upstream option names or capability sets are inferred.

Use `optional-depends = ["codec"]` for an edge that should exist only when that
dependency is independently enabled. Unlike `depends`, it does not activate the
dependency. This combines feature-gated native packages with correct build
ordering, prefix propagation, and downstream invalidation without handwritten
consumer CMake glue.

`woodshed setup` adds missing, empty Cargo feature declarations with the same
names, retaining existing feature definitions. The one-line bridge reads enabled
matching Cargo features, including Cargo's `default` feature, so `cargo build
--no-default-features --features fast` controls this graph. If integrating
manually without a Cargo `default` feature declaration, Woodshed's defaults stay
enabled. For explicit control use `build_from_with_options(path, &PlanOptions)`.
Native features of vcpkg ports and Conan options remain explicit in their locked
provider declarations; manifest feature forwarding into provider-specific native
features is rejected rather than silently changing the provider's locked graph.
Provider recipes retain their provider-owned dependency graphs: external
`depends`/`optional-depends` on a vcpkg/Conan declaration are rejected, not silently
ignored. A source recipe can consume any selected provider package.

Target tables accept exact Rust triples or Rust `cfg(...)` predicates, evaluated
against the Rust **target**, not the build host. All remote variants are locked
by explicit maintenance commands, including optional and inactive variants.
Multiple matching declarations for one name are errors; use disjoint target
tables instead of relying on table-order overrides. An active edge to a
target-inapplicable dependency fails with the dependency name and target.
The Cargo bridge uses actual `CARGO_CFG_*` values, including target features;
explicit API callers can supply `PlanOptions::target_cfg`, otherwise cfg values
come from `rustc --print cfg --target`. Build-profile/crate predicates such as
`cfg(feature = "...")`, `test`, and `debug_assertions` are rejected: express
dependency gates in `[features]` instead.

Woodshed schedules prerequisites before dependents and includes prerequisite
installation identities in downstream keys. Same-role dependency package roots
populate `CMAKE_PREFIX_PATH`; host tools populate `CMAKE_PROGRAM_PATH` and the
child-process `PATH`. `build.tools` names executables installed under `bin/`
without the platform `.exe` suffix. Tool-only sources need no CMake package
exports. Host nodes build for HOST, never emit target Rust link directives, and
cannot depend on target nodes. Target cross-builds still require an explicit
toolchain; no host compiler or provider substitution occurs.

For library builds, select the upstream's installed CMake CONFIG package and
exported targets in the manifest. Woodshed generates its own metadata consumer;
users do not need to write consumer CMake glue. An upstream without install rules,
package exports, or build-system support for a requested option still requires an
upstream recipe/change or a custom low-level integration. Arbitrary project build
logic cannot be inferred. vcpkg/Conan retain their own locked build recipes.

Static/shared linkage remains an explicit per-recipe choice. Graph link emission
deduplicates transitive libraries after dependents and rejects conflicting native
library files or link kinds. Typed imported targets retain per-file static/shared
linkage, including mixed dependency graphs and Windows import libraries.
Unknown imported `.lib` types still require the recipe's explicit linkage contract;
Woodshed cannot infer their ABI from a filename. Selected shared runtime files are staged under
Cargo's `OUT_DIR` for `cargo run`/`cargo test`, and exposed to `woodshed bundle`.
Extra plugins/resources and undeclared operating-system runtime dependencies
still need explicit deployment declarations; this is not binary-loader closure
inference.

Inspect selection without downloading or building:

```text
woodshed tree --target aarch64-unknown-linux-gnu --features fast
woodshed explain engine --no-default-features --features fast --json
woodshed add engine --path native/engine --cmake-package Engine --target Engine::Engine
```

The Rust API exposes `Project::resolve(&PlanOptions) -> ResolvedPlan`, with
prerequisite-first nodes carrying lock IDs, host/target roles and triples,
effective native features/defines, original typed declarations, and activation
reasons. `tree`/`explain` report selection, not cache-hit predictions or ABI proof.

For custom superbuilds, source declarations can omit `build` entirely. Load
`Project::load(manifest_path)`, validate its adjacent lock with `project.locked()`,
then call `project.resolve(&options)`. For each selected remote source, use
`lock.sources[&node.id]` (not the display name) with `Cache::source` and
`Cache::verify_source`. Local sources instead use the declared `path`, relative
to the manifest. This selects and acquires sources without invoking the package
build orchestrator. The caller owns its superbuild/install policy and must
verify every consumed pinned source, including prepared dependency mounts.

Manifest features may be empty (`cpu = []`) or compose other features
(`dialect = ["frontend", "dep:dialect-source"]`). Like Cargo, feature activation
is additive: composition enables prerequisites; listing a dependency in two
different features does not require both features to be enabled.

After changing only feature gates, graph edges or source build recipes, preserve
all existing revisions/checksums without remote resolution by supplying the
previous declarations that match the lock:

```text
woodshed lock --manifest woodshed.toml --preserve-pins-from previous-woodshed.toml
```

The equivalent Rust API is `project::plan_relock(manifest_path, previous_text)`,
returning an `Update` to preview and `apply()`. It validates the old lock and
rejects changed source selectors, dependency IDs, provider selections or profile
contents. It never invokes a resolver or downloads sources. Normal `woodshed lock`
explicitly resolves all declarations; ordinary builds never refresh locks.

### Understanding rebuilds and managing artifacts

Every successful source build or prebuilt import records its decision in
`diagnostics/`. Reports compare with the previous **successful request for the
same manifest/declaration ID**, including requests that selected a different hard
workspace. They distinguish `built`, `imported`, `repaired` and `reused`, identify
changed input fields (for example `defines`, `dependencies`, `configured-tools`,
or `env:CFLAGS`), and show the exact artifact and optional workspace fingerprints.
They do not predict a future build or claim that CMake will recompile every object.

```text
woodshed cache-explain codec --manifest woodshed.toml --json
woodshed cache inspect
woodshed cache verify FULL_ARTIFACT_FINGERPRINT
woodshed cache prune FULL_ARTIFACT_FINGERPRINT
woodshed cache prune FULL_ARTIFACT_FINGERPRINT --apply
```

`Cache::explain_build(manifest, declaration_id)` returns `BuildExplanation`.
Only field hashes are persisted; environment values are not copied into reports.
`Cache::inspect_artifacts()` and `verify_artifact(id)` check complete content and
metadata without repairing it. Pruning defaults to a preview, requires explicit
full IDs, refuses unowned trees, and never prunes sources or provider caches.
It serializes against active high-level builds. Artifacts referenced by Cargo
output directories remain retained until those directories are removed (for
example by `cargo clean`); retaining older configurations is intentionally
conservative. Custom integrations use `pin_artifact(id, name)` /
`unpin_artifact(name)` (CLI `cache pin` / `cache unpin`) while retaining paths.
High-level builds without `OUT_DIR` retain a durable `standalone:FULL_ID` pin;
release that named pin explicitly when the standalone installation is no longer used.
Do not prune unpinned installations used outside the high-level Cargo bridge.
Use `Cache::from_environment()` in CLI tools to avoid emitting Cargo directives.

### Sharing native declarations across a Cargo workspace

Put reusable selections in a root `woodshed.toml`:

```toml
[workspace.dependencies.codec]
path = "native/codec"
default-features = false
build = { cmake = "Codec", targets = ["Codec::Codec"], features = { simd = { SIMD = "ON" } } }
```

Then inherit them from member `woodshed.toml` files:

```toml
[dependencies.codec]
workspace = true
optional = true
features = ["simd"]
```

The nearest ancestor `woodshed.toml` containing `[workspace]` is the inheritance
boundary; a root manifest can inherit its own entries too. Target-specific tables
can also inherit. Source, recipe-input, toolchain and Conan-profile paths remain
relative to the root declaration, not the member. Member overrides are restricted
to `optional`, additive `features`, and `default-features`; disabling defaults
requires the root to declare `default-features = false`, as in Cargo.
Other source/build policy changes must be made explicitly at the root.

Resolution and each member's lock contain the effective declaration. Root changes
invalidate affected member locks, and the Cargo bridge watches inherited manifests.
Updates preserve `workspace = true` rather than silently flattening member intent;
concurrent root edits are rejected. Edit a shared selection at its root and relock
members rather than using a member bump to detach it. Feature selection remains
per member's resolved native graph; this does not merge unrelated Cargo invocations
or infer ABI compatibility across different package requests.

### Toolchain doctor

```text
woodshed doctor
woodshed doctor --target aarch64-unknown-linux-gnu --toolchain cross.cmake --json
```

`doctor(&DoctorOptions)` configures and compiles a fresh C library and links it
into a C++ executable using Woodshed's normal generator/launcher selection. It
reports the actual C/C++ compilers, linker, generator and launchers, captured
diagnostics and actionable failure guidance. It never runs the executable,
installs tools, changes a native cache, or substitutes a host compiler for an
explicit cross target. Cross checks require a toolchain file. Reports may contain
native-tool paths/output: review them before sharing.

### Live progress and build decisions

Woodshed reports phase starts/ends, elapsed time, lock waits, source/archive cache
verification, and native command exit status to stderr. Long phases emit a
heartbeat every ten seconds, including while a compiler or verification is busy.
Dependency graph builds report their position and emit installation reuse,
build/import and repair decisions. Before rebuilding, changed identity field names
explain why (for example `defines`, `dependencies`, or `env:CFLAGS`); raw environment
values are not printed. These are Woodshed decisions, not guesses about which
individual compiler objects Ninja will rebuild.

Cargo captures build-script stderr: use `cargo build -vv` to expose it. For
reliable live observation without relying on Cargo's display, set an explicit log:

```powershell
$env:WOODSHED_LOG = "$PWD\native-build.log"
cargo build
# In another terminal:
Get-Content .\native-build.log -Wait -Tail 30
```

`WOODSHED_LOG` appends timestamped, process-labelled progress records immediately;
its parent directory must exist. Processes sharing the log serialize complete
records. `WOODSHED_PROGRESS=0` disables progress on stderr without disabling the
log. Logging failures print an explicit diagnostic to stderr; they do not turn an
otherwise valid build into a failure. The log contains Woodshed events, not the
complete child-process output; capture `cargo build -vv 2>&1 | Tee-Object build.log`
for compiler diagnostics too. Phase `ended` is a duration marker, not a success
claim; command exit records and the build result determine success.

Custom consumers can call `woodshed::progress::report("message") -> io::Result<()>`
to use the same sinks. Consumer-specific superbuild/cache policy must report its
own decisions; Woodshed cannot infer them. No terminal percentages are invented
for operations whose total work is unknown.

## Reference: optional building blocks

Every piece is independently useful and independently optional, gated behind its
own feature. A crate that only needs checksummed downloads takes `Cache`; one
that drives its own build system takes `Toolchain` and `process::run` and
ignores `CmakeBuild`; one that only wants to find an existing install takes
`probe`. Nothing forces you up a layer.

| Feature                              | Gives you                                                                                             |
| ------------------------------------ | ----------------------------------------------------------------------------------------------------- |
| `manifest`                           | Human `woodshed.toml` declarations and generated `woodshed.lock`                                      |
| `download`                           | HTTPS fetching, with a system `curl` fallback for corporate proxies                                   |
| `git`                                | Immutable Git snapshots and submodule/file metadata using the host Git executable                     |
| `tar-gz`, `tar-bz2`, `tar-xz`, `zip` | Archive extraction, one format each                                                                   |
| `cmake`                              | `CmakeBuild` - configure, build, install, with the MSVC runtime under control                         |
| `build`                              | Declarative CMake source builds, export discovery, and the universal `woodshed::build()` Cargo bridge |
| `process`                            | `process::run` - Ctrl+C tears down the whole build tree                                               |
| `vcpkg`                              | Existing-install probe and explicit pinned package installation                                       |
| `conan`                              | Explicit Conan 2 graph locking/installation through its CLI                                           |
| `pkg-config`                         | Probe for an already-installed library                                                                |
| `pin`, `cli`                         | Source/provider lock maintenance and the Clap-based `woodshed` command                                |
| `bundle`                             | Build and assemble an application with transitive native deployment resources                         |

### Static vs. dynamic

woodshed leaves the choice to you but makes it hard to get wrong: `Linkage::{Static, Dynamic}`
drives `CmakeBuild::linkage` (`BUILD_SHARED_LIBS`), `BuildKey::linkage` (separate cache
dirs), `emit_link` (link directives; for a dynamic build it also stages the shared libraries in
the build script's `OUT_DIR` and adds it as a `native=` path, which is the one place Cargo puts
on the loader path for `cargo run`/`cargo test`, and exports `libdir`/`bindir` metadata
for dependents),
`link_cpp_runtime_for` (only static libraries need the C++ runtime linked in),
`copy_shared_libs` (ship the DLLs/`.so` files next to your binary) and
`emit_exe_rpath` (let the shipped executable find them).

#### Shipping a dynamic build

In the application's `build.rs` call `woodshed::emit_exe_rpath()`: Linux and macOS
executables then load libraries from their own directory (Windows always does). Package the
executable together with the files in the library's `DEP_<LINKS>_LIBDIR` / `_BINDIR`, or
copy them with `copy_shared_libs`. Link arguments from a dependency's build script never
reach the application's executable, so a library crate cannot do this for you.

### Stable mirrors and additive source workspaces

`Cache::mirror(source, key)` synchronizes the existing `mirrors/<key>` path
by file content instead of deleting and recopying it. Unchanged files retain
their timestamps even if input timestamps change. Removed files/directories
and file/directory transitions are reflected in the mirror.
`Cache::mirror_to(source, destination)` applies the same operation to an existing
or caller-chosen path. The destination must be exclusively owned: entries not
in the source are deleted. Source symlinks are materialized as before; destination
links are replaced without writing through them. Cyclic directory links and
overlapping source/destination roots are rejected.

`Cache::prepare(key, source, mounts)` still describes an immutable set of inputs:
changing the source or mounts under a completed key is an error. For writable
source trees that acquire optional pinned dependencies lazily, use:

```rust,ignore
let key = woodshed::BuildKey::new("pinned-source-family").dep(&upstream);
let source = cache.source(&upstream)?;
let prepared = cache.prepare_incremental(&key, &source, &[])?;
// Later, possibly from a different build configuration:
let prepared = cache.prepare_incremental(
    &key, &source, &[(std::path::PathBuf::from("third_party/library"), dependency)],
)?;
```

The root remains at `prepared/<key>`; new mounts do not change its absolute
path or recopy patched root files. Omitted mounts remain installed.
`Cache::prepare_incremental_to(source, destination, mounts)` instead adopts
an existing directory **without recopying it**, or copies the source if the
destination does not exist. This lets callers add dependencies to existing trees
without changing CMake's stored source path.

### Maintaining dependencies

```text
cargo install woodshed --features cli
woodshed add geos --version 3.14.1 --url "https://download.osgeo.org/geos/{name}-{version}.tar.bz2"
woodshed bump geos 3.15.0     # rewrites version, re-downloads, updates sha256
woodshed verify               # re-hash every pin
woodshed bump geos 3.15.0 --dry-run
woodshed add example --version 1.2.0 --git https://example.org/example.git --ref v1.2.0
woodshed add zlib --version 1.3.1 --github madler/zlib
woodshed lock --locked
```

`{version}` and `{name}` in `url` and `file` are substituted when a manifest loads, so a
template URL makes a bump a one-field change. Only these commands download to compute a
new expected checksum; `build.rs` never does. Provisioning still checks archive bytes.

`add` and `bump` share one transactional planner. A bump resolves the selected
source and all transitively declared dependent sources, then computes archive
checksums. Nothing is written until the entire plan succeeds. `--dry-run`
prints declarations and lock without writing. Comments, markers,
resolution policies, and unrelated entries are preserved. The result is
staged beside the manifest. Each file replacement is atomic, including on Windows.
There is no two-file atomic rename: a process interruption between replacements
leaves a detectable stale lock. Ordinary lock-write failures roll back the
declarations. Builds fail closed until a matching lock is restored.
Edits made since planning are rejected rather than overwritten.
Concurrent Woodshed commits are serialized by a retained `<manifest>.pin.lock`
sidecar (ignore it in version control). A first `add` creates the manifest and
its directory only when applying a successful plan; dry runs create neither.

CLI argument parsing/help uses Clap. `cli` does not imply `git`: archive-only
users do not need Git support. Git commands give an actionable feature error
when installed without `--features git`.

### Follow a dependency declared in upstream CMake

If the upstream project contains:

```cmake
set(CHILD_REV "0123456789abcdef0123456789abcdef01234567")
FetchContent_Declare(child URL https://example.org/child/${CHILD_REV}.tar.gz)
```

Declare where those names live:

```toml
[dependencies.child.resolve]
kind = "file"
parent = "parent"
revision = { path = "CMakeLists.txt", cmake-variable = "CHILD_REV" }
url = { path = "CMakeLists.txt", cmake-url = "child" }
```

`parent` is the dependency whose source contains those files. Bumping it updates
the child automatically. If the two declarations are in different files, give
each its own `path`. Woodshed handles quotes, whitespace, comments, and multiline
commands, and substitutes `${CHILD_REV}` automatically. No duplicated URL,
checksum, child version, or placeholder rule is needed.

These selectors **read literal metadata; they never execute upstream CMake**.
A missing name, duplicate declaration (even in separate conditional branches),
list, or computed revision produces an error instead of choosing a value.
URLs must be single HTTP(S) archive URLs; variables other than the selected
revision are not evaluated. For unusual nonliteral formats, `pattern` remains
an advanced escape hatch with exactly one capturing group. Choose exactly one
of `cmake-variable`, `cmake-url`, or `pattern` per file selector.

#### Git sources and release archives

`--git` selects a repository rather than a published source archive.
`--ref` can select a tag, branch, or commit. Declarations retain selection intent;
the generated lock records the resolved full commit ID. Builds never resolve moving refs:

```toml
[dependencies.example]
version = "1.2.0"
git = "https://example.org/example.git"
tag = "v1.2.0"
markers = ["CMakeLists.txt"]
```

`Cache::source` exports tracked files without `.git`, checkout hooks/filters, or
automatic submodule initialization. Cache and build identities include the
repository and immutable commit. Completed snapshots work offline; missing or
incomplete snapshots fail explicitly. Enable `git` in the consuming build
dependency only when it needs Git sources. Extracted archives live under `sources/archive-<sha256>`,
so different locked bytes never share a tree just because name/version labels
match. No previous directories are scanned or adopted. Completed Git and archive
trees have full content/type/permission fingerprints; modifying cached inputs is
not supported. Copy writable inputs with `Cache::prepare` or
`Cache::prepare_incremental` instead. Archive extraction is staged, verified and
recorded under a lock; missing/modified content is restored from a verified cached
archive, including offline. A valid extracted tree can be reused without keeping
its downloaded archive. A damaged Git snapshot requires repopulating its exact
commit with network/repository access; ordinary builds never resolve moving refs.

GitHub source archives retain Git provenance in the generated lock for dependency discovery:

```toml
[dependencies.parent]
github = "example/parent"
version = "1.2.0"
file = "parent-{version}.tar.gz"
```

`woodshed bump parent 1.3.0` resolves `v1.3.0` once, updates the immutable
revision and archive checksum, and updates any declared dependents. `--ref`
overrides the ref for that operation. Locked archives use the resolved upstream
commit, not the human release label.

#### Dependent sources

Declare relationships once in the manifest rather than scripting each release:

```toml
[dependencies.child.resolve]
kind = "gitlink"
parent = "parent"
path = "third_party/child"
archive = "{repository}/archive/{revision}.tar.gz"
```

The gitlink and `.gitmodules` URL are read from the **same parent commit**.
`{repository}` is the upstream submodule URL without a trailing `.git`.
Omit `archive` to provision the child from Git instead. Submodule paths and
relative repository URLs are resolved by the generic Git adapter. It never
recursively fetches every submodule: only declared sources are selected.

For sources pinned in an upstream text file, use a narrow, explicit capture:

```toml
[dependencies.child.resolve]
kind = "file"
parent = "parent"
placeholder = "${CHILD_REV}"
[dependencies.child.resolve.revision]
path = "cmake/dependencies.cmake"
pattern = '(?m)^set\(CHILD_REV "([0-9a-f]{40})"\)$'
[dependencies.child.resolve.url]
path = "cmake/dependencies.cmake"
pattern = '(?m)^URL "(https://[^"]+)"$'
```

Each pattern must contain exactly one capturing group and match exactly once.
The optional URL rule tracks upstream URL changes; `placeholder` names the
literal upstream token replaced by the resolved revision. Without a URL rule,
the existing URL template is retargeted. Unknown parents, dependency cycles,
ambiguous captures, floating commit pins, and missing metadata fail explicitly.
Direct bumps of managed children fail; bump their parent instead.

Woodshed does **not** infer arbitrary CMake dependency graphs or choose build
targets. Projects own their source-build mappings and build integration.

## The one opinion

**Expensive build trees go in a content-hashed cache directory, never
`OUT_DIR`, and are reused only after an explicit completeness check.**

Cargo deletes `OUT_DIR` whenever the build script changes, so building a
ten-minute dependency there means every one-line edit to `build.rs` costs ten
minutes - and silently discards work that was still valid. `Cache::build_dir`
keys the directory on everything that affects the output, so sharing happens
exactly when it is correct.

## The CRT problem

Statically linking a C++ library is not the same as producing a portable
executable. On MSVC two settings must agree: CMake's
`CMAKE_MSVC_RUNTIME_LIBRARY` and Rust's `-C target-feature=+crt-static`. A
mismatch does not fail during the C++ build; it fails much later, at link time,
with unresolved `__imp_*` symbols that say nothing about the actual cause.

`CmakeBuild` makes those two impossible to diverge - but which side to converge
on is your decision:

- `MsvcRuntime::MatchRust` (default) - correct when no allocation crosses the
  language boundary. A `+crt-static` build then ships as a single executable
  with no MSVC redistributable.
- `MsvcRuntime::AlwaysDynamic` - required when allocations do cross, since Rust
  and the native library must share one heap.

Only you know which is true of your library, so woodshed asks rather than
guessing.

## Environment

| Variable             | Effect                                                 |
| -------------------- | ------------------------------------------------------ |
| `WOODSHED_CACHE`     | Cache location. Defaults to `$CARGO_HOME/woodshed`.    |
| `WOODSHED_OFFLINE=1` | Never download; fail with an actionable error instead. |

## License

MIT OR Apache-2.0
