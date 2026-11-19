# Woodshed build and cache guide

See the [README](README.md) for a first build, vcpkg/Conan recipes, and common commands.

## Source recipes

```toml
[dependencies.engine]
path = "native/engine"

[dependencies.engine.build]
cmake = "Engine"
targets = ["Engine::Engine"]
linkage = "static"
defines = { ENGINE_BUILD_TESTS = "OFF" }
toolchain = "toolchains/target.cmake"
inputs = ["toolchains/sdk-settings.cmake"]
env = ["ENGINE_SDK_MODE"]
```

`path` needs no version or remote; the tree's contents (excluding `.git`) affect
its identity. Keep generated output outside it. A change during compilation
fails rather than publishing an artifact for the wrong inputs.

`toolchain` is optional for native builds and required for cross-builds. Paths
are relative to the declaration. List extra build files in `inputs` and
output-affecting environment variables in `env`; Woodshed hashes file contents
and tracks the variable values without setting them. Standard compiler/linker
flags, SDK environment, and `SOURCE_DATE_EPOCH` are already tracked. Use dedicated
fields rather than `CMAKE_*` or `BUILD_SHARED_LIBS` in `defines`. Package registries
and standard FetchContent downloads are disabled for source recipes; supply
vendored inputs or use upstream options for installed dependencies. This is not
a sandbox for arbitrary native build scripts.

## Native graphs and workspaces

```toml
[features]
default = ["fast"]
fast = ["dep:codec", "engine/simd"]

[dependencies.engine]
path = "native/engine"
depends = ["generator"]
optional-depends = ["codec"]
default-features = false
build = { cmake = "Engine", targets = ["Engine::Engine"], features = { simd = { ENGINE_ENABLE_SIMD = "ON" } } }

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

Features compose additively. Optional dependencies get an implicit same-name
feature unless selected with `dep:name`. `depends` activates prerequisites;
`optional-depends` only adds edges to independently enabled nodes. Inactive
nodes are not provisioned. Source native features select definitions that
override base `defines`; conflicting enabled values are errors. Set
`build.default-features` for native defaults and use dependency-level `features`
and `default-features` to select them. Provider graphs remain provider-owned:
vcpkg port features and Conan options belong in the locked provider declaration,
not in external graph edges or forwarded native features.

`woodshed setup` adds missing Cargo features. The bridge reads the active Cargo
features, so `cargo build --no-default-features --features fast` works as
expected. Manual integration without a Cargo `default` feature keeps Woodshed's
defaults; use `build_from_with_options` for explicit selection.

Target tables match exact Rust triples or target `cfg(...)` predicates, not the
build host or crate properties such as `feature` and `test`. Overlapping matches
and active edges to unavailable targets fail. All remote variants are locked,
including inactive ones. Prerequisites build first and affect downstream keys;
same-role package roots extend `CMAKE_PREFIX_PATH`. Host executables are
installed under `bin/`, without `.exe` in `build.tools`; they extend
`CMAKE_PROGRAM_PATH` and child `PATH`. Host nodes cannot depend on target nodes
and never emit target link directives.

Share declarations with `[workspace.dependencies.NAME]` and inherit with
`[dependencies.NAME]` and `workspace = true`. Paths remain relative to the root
declaration. Members can change `optional`, add `features`, or select defaults
within the root's policy. Edit shared declarations at the root and relock
affected members; `bump` does not detach inherited declarations.

## Locking source relationships

Use `--url` for release archives, `--git` for repository snapshots, or
`--github` for GitHub archives with commit provenance. `--ref` selects a tag,
branch, or commit; builds use the locked selection, never a moving ref.
`{name}` and `{version}` can appear in archive URLs and filenames.

To follow a submodule, declare it without recursively fetching all submodules:

```toml
[dependencies.child.resolve]
kind = "gitlink"
parent = "parent"
path = "third_party/child"
archive = "{repository}/archive/{revision}.tar.gz"
```

Woodshed reads the gitlink and `.gitmodules` from the same commit. Omit
`archive` to export the pinned child from Git. For pins kept in CMake files:

```toml
[dependencies.child.resolve]
kind = "file"
parent = "parent"
revision = { path = "CMakeLists.txt", cmake-variable = "CHILD_REV" }
url = { path = "CMakeLists.txt", cmake-url = "child" }
```

These selectors read literal metadata; they never execute CMake. Ambiguous or
computed values fail. Use `pattern` with exactly one capturing group and one
match for unusual formats; `placeholder` can name a literal revision token.
Bump the parent, not a managed child. The complete update is planned before
writes. File replacements are atomic individually; an interrupted two-file
update produces a stale lock rather than a silently changed build.

To relock only changed build recipes, features, or edges without resolving
sources, use `woodshed lock --preserve-pins-from previous-woodshed.toml` with
the prior declaration matching the lock. The corresponding Rust API is
`project::plan_relock`.

## Cache identity and incremental workspaces

Artifacts are keyed by locked inputs, recipes, dependency artifacts, target and
CRT, declared files, tracked environment, and selected tool binaries. Full
content checks detect changed or missing installation files even when size and
mtime match. This detects local corruption, not untrusted-build provenance.
Older receipts with incompatible inventory formats are not promoted; valid
downloaded archives can still be reused without changing dependency locks.

`cache-explain` describes the previous successful request, not the next build
or which objects Ninja will rebuild. It reports changed field names, not raw
environment values. `cache prune FULL_ARTIFACT_FINGERPRINT` previews removal;
add `--apply` to execute. It refuses unowned trees and retains artifacts
referenced by Cargo output directories or pins. Use `cache pin` / `cache unpin`
for custom consumers; builds without `OUT_DIR` retain a
`standalone:FULL_ARTIFACT_FINGERPRINT` pin until explicitly released. Pruning
does not touch source or provider caches.

`WOODSHED_LOG` appends process-labelled events; `WOODSHED_PROGRESS=0` silences
stderr only. Events include lock waits, phase timings, heartbeats, and command
status. Use `cargo build -vv` for compiler output; a phase-end event alone is
not proof of success.

For custom superbuilds, load `Project::load`, validate `project.locked()`, and
call `Project::resolve(&PlanOptions)`. Source declarations can omit `build`.
Acquire and verify each selected remote source with `Cache::source` and
`Cache::verify_source`, using `lock.sources[&node.id]`, not the display name.
Never modify cached sources. A complete extraction works without its archive;
repair needs the verified archive or the exact Git commit.

| API | Use it for |
| --- | --- |
| `Cache::mirror` / `mirror_to` | Content-based synchronization to an owned path; unchanged files retain timestamps, removed entries disappear. Source symlinks are materialized. |
| `Cache::prepare` | An immutable source/mount selection; reject changes under a completed key. |
| `Cache::prepare_incremental` / `prepare_incremental_to` | Add pinned mounts lazily without replacing an existing source root or removing old mounts. |
| `Cache::refresh_sources_to` | Synchronize an exact source and mount set across revisions. Preserve unchanged timestamps and symlinks, including dangling links; remove omitted mounts and discard local patches. Windows requires symlink privileges. |

Do not mix additive preparation and exact refresh on the same destination.
Refresh keeps existing links to unchanged immutable mounts; changed mounts
become private mirrors. Never write through mount links. Key mutable workspaces
by their target/toolchain/configuration family and installed artifacts by exact
inputs. Hold the workspace lock through refresh, configure, build, and install.
Published headers and metadata must not depend on a mutable source directory.

## Linking and deployment

Source and provider recipes choose linkage explicitly. Link emission orders
dependencies after dependents, deduplicates transitive libraries, and rejects
conflicting files or link kinds. MSVC `msvc-runtime = "static"` or `"dynamic"`
is independent of linkage; host tools default to the dynamic CRT. Unknown
`.lib` types still need an explicit linkage contract.

Use `woodshed bundle` for declared runtime resources. For manual shipping, use
`copy_shared_libs` and `emit_exe_rpath` in the application's own build script
where applicable, and use `build_from(path)` to inspect each installation's
`runtime_directories`. Extra plugins, resources, and system dependencies
are not inferred by scanning the binary's loader closure.
