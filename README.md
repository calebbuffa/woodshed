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
cargo install woodshed --version 0.3.0 --features cli,git
```

Then add fmt and select its documented CMake export:

```text
woodshed add fmt --github fmtlib/fmt --version 11.1.4 --cmake-package fmt --target fmt::fmt --define FMT_TEST=OFF --define FMT_DOC=OFF
```

That command creates **both** `woodshed.toml` and `woodshed.lock`. The human
file contains:

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
woodshed = { version = "0.3", features = ["build"] }
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

**Updating later is one command:** `woodshed bump fmt <new-version>`.
Review the changed declarations/lock, then build normally. If the new release
changes its API or build layout, adjust your binding/build recipe too.

This builds the native dependency; it does not invent a Rust API for a C++
library. Supply your FFI/native shim or binding generator separately. Libraries
without installed CMake CONFIG exports use Woodshed's low-level Rust API instead
of this declarative path.

## Use vcpkg instead

Choose this when vcpkg already provides the library and you do not want to own
its source-build recipe. You still need an existing `VCPKG_ROOT`, the matching
compiler/SDK, CMake 3.21+, and the package's documented CMake exports.

This example is specifically **Windows x64 MSVC with the default dynamic CRT**.
Other targets need a matching triplet; do not copy `x64-windows` for Linux.

Install the CLI with the provider enabled, then add the package:

```text
cargo install woodshed --version 0.3.0 --features cli,git,vcpkg
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
woodshed = { version = "0.3", features = ["build", "vcpkg"] }
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
cargo install woodshed --version 0.3.0 --features cli,conan
woodshed lock
```

Run `woodshed setup --dry-run`, then `woodshed setup`. For manual integration,
use this build dependency:

```toml
[build-dependencies]
woodshed = { version = "0.3", features = ["build", "conan"] }
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
```

`toolchain` and `inputs` are optional paths relative to `woodshed.toml`. Cross
builds require an explicit toolchain. List any extra files read by your toolchain
or project options in `inputs`: Woodshed hashes those bytes and tells Cargo when
to rerun. It cannot infer arbitrary files read by upstream CMake.
`CMAKE_*` and `BUILD_SHARED_LIBS` overrides are rejected in `defines`; use the
dedicated toolchain/linkage fields, or the low-level Rust API for custom policies.

The build key includes source identity, recipe, target, CRT, declared input bytes,
tracked compiler/SDK environment, and discovered build-tool paths/timestamps.
A successful installation records all installed file paths/sizes and evaluated
export metadata; missing or size-changed files trigger rebuilding. This is a
completeness check, not a cryptographic integrity audit of locally built output.
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

The rest of this guide is reference material. Start here only when the recipes
above do not cover your integration:
[source selection](#dependency-intent-not-checksum-bookkeeping),
[upstream CMake pins](#follow-a-dependency-declared-in-upstream-cmake),
[provider details](#native-package-providers-no-handwritten-consumer-cmake),
[CRT choices](#the-crt-problem), and [application packaging](#application-bundles).

## Reference: optional building blocks

Every piece is independently useful and independently optional, gated behind its
own feature. A crate that only needs checksummed downloads takes `Cache`; one
that drives its own build system takes `Toolchain` and `process::run` and
ignores `CmakeBuild`; one that only wants to find an existing install takes
`probe`. Nothing forces you up a layer.

| Feature                              | Gives you                                                                                             |
| ------------------------------------ | ----------------------------------------------------------------------------------------------------- |
| `manifest`                           | Human `woodshed.toml` declarations, generated `woodshed.lock`, and legacy pins                        |
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

### Inspect CMake configuration before compiling

`CmakeBuild::configure()` returns a `ConfiguredCmakeBuild` after a successful
configure step, without building native targets. Inspect generated headers,
`CMakeCache.txt`, or requested CMake File API replies through
`build_directory()`, then call `build()` to continue. `install()` remains the
configure-and-build convenience method. Keep the `BuildDirs::lock()` guard
alive across both phases; configuration alone is not artifact completeness.

This separation supports maintainer binding generation and metadata discovery
without first compiling a complete native compiler/toolchain. It does not
guess dependency versions or fetch unpinned upstream dependencies.

`Toolchain::detect()` respects an explicit `CMAKE_GENERATOR`. Reuse checks
canonicalize source directories and recognize Windows verbatim/UNC spellings
without discarding Unix path case sensitivity.

### Maintaining dependencies

```text
cargo install woodshed --features cli,git
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
prints declarations and lock (JSON changes for legacy pins) without writing. Comments, markers,
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

#### Legacy source-manifest examples

The following examples describe the compatible combined `versions.toml` format
and the immutable records stored in generated locks. In new declarations,
selection goes under `[dependencies.NAME]`; parent-managed children only need
markers and `resolve`, not independent versions, source URLs, or checksums.

#### Git sources and release archives

`--git` selects a repository rather than a published source archive.
`--ref` can select a tag, branch, or commit, but the manifest always records
the resolved full commit ID. Builds never resolve moving refs:

```toml
[example]
version = "1.2.0"
markers = ["CMakeLists.txt"]
[example.git]
repository = "https://example.org/example.git"
revision = "0123456789abcdef0123456789abcdef01234567"
```

`Cache::source` exports tracked files without `.git`, checkout hooks/filters, or
automatic submodule initialization. Cache and build identities include the
repository and immutable commit. Completed snapshots work offline; missing or
incomplete snapshots fail explicitly. Enable `git` in the consuming build
dependency only when it needs Git sources. Existing archive cache identities
are unchanged.

An archive can also record its Git provenance for dependency discovery:

```toml
[parent]
version = "1.2.0"
url = "https://example.org/parent/archive/{revision}.tar.gz"
sha256 = "<64 hexadecimal characters>"
file = "parent-{version}.tar.gz"
[parent.upstream]
repository = "https://example.org/parent.git"
revision = "0123456789abcdef0123456789abcdef01234567"
reference = "v{version}"
```

`woodshed bump parent 1.3.0` resolves `v1.3.0` once, updates the immutable
revision and archive checksum, and updates any declared dependents. `--ref`
overrides the ref for that operation. `{revision}` in archive URLs uses the
recorded upstream commit, not the human release label.

#### Dependent sources

Declare relationships once in the manifest rather than scripting each release:

```toml
[child]
version = "123456789abcdef0123456789abcdef012345678"
url = "https://example.org/child/archive/{version}.tar.gz"
sha256 = "<64 hexadecimal characters>"
[child.resolve]
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
[child.resolve]
kind = "file"
parent = "parent"
placeholder = "${CHILD_REV}"
[child.resolve.revision]
path = "cmake/dependencies.cmake"
pattern = '(?m)^set\(CHILD_REV "([0-9a-f]{40})"\)$'
[child.resolve.url]
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

### Native package providers: no handwritten consumer CMake

Opt in to a provider instead of a source recipe. These are **explicit installs**,
not the older best-effort probes and not a fallback if another provider fails:

```toml
[dependencies.zstd]
provider = "vcpkg"
package = "zstd"
version = "1.5.7"
triplet = "x64-windows"
[dependencies.zstd.cmake]
package = "zstd"
targets = ["zstd::libzstd_shared"]
linkage = "dynamic"

[dependencies.openssl]
provider = "conan"
package = "openssl/3.4.1"
host-profile = "profiles/host"
build-profile = "profiles/build"
options = ["openssl/*:shared=False"]
build-missing = true
[dependencies.openssl.cmake]
package = "OpenSSL"
targets = ["OpenSSL::SSL", "OpenSSL::Crypto"]
```

Enable `vcpkg`/`conan` in the build dependency and maintenance CLI as needed:

```text
cargo install woodshed --features cli,git,vcpkg,conan
woodshed add zstd --version 1.5.7 --provider vcpkg --triplet x64-windows --cmake-package zstd --target zstd::libzstd_shared --dynamic
```

For builds, `providers::install` takes the declaration path, dependency name, and
`InstallOptions::cargo(name)` (or explicit target/configuration/output options).
It returns `Installation`: include directories, definitions, native compile flags,
ordered linker arguments, and runtime directories. `emit_cargo()` validates the
entire link set before emitting propagating `rustc-link-lib`/search directives.
Unsupported custom linker flags fail explicitly; applications needing them can
consume the complete ordered `link_arguments` at the final linker instead.
Runtime deployment remains explicit: package runtime directories are not silently
added to a process-global loader path or copied into a distribution.

Woodshed generates a small CMake consumer and queries its File API to evaluate
exported transitive targets. Users supply the package/target names documented by
the package, not CMake source. CMake CONFIG exports are required in this initial
adapter; module-only packages need their own integration. A native SDK and CMake
3.21+ are still prerequisites.

vcpkg requires an existing `VCPKG_ROOT`. Locking captures its current immutable
registry baseline (or the declaration's explicit `baseline`), exact version
override, features, and tool/root identity; installation uses a separate manifest
and install tree without modifying the shared installation. Triplet architecture,
platform, and MSVC CRT must agree with the Cargo target. Port defaults can be
disabled with `default-features = false`; `features` lists optional port features.
The adapter accepts standard MSVC Windows, glibc Linux, and macOS triplets
(including Windows `-static`/`-static-md` variants). GNU Windows, musl, and custom
triplet ABIs are not inferred and fail explicitly.

Conan requires an existing Conan 2 CLI (`CONAN` can select its executable).
Woodshed invokes Conan's own lock/install commands and CMakeDeps/CMakeToolchain,
and writes no Python recipe or generator. Explicit host/build profile contents
are fingerprinted: missing or changed profiles fail before installation. Profiles
must be self-contained (flatten includes/Jinja before locking). Conan's recipe
revisions/graph and tool version are locked; Conan retains responsibility for
binary package IDs, compiler compatibility, and its own caches. Host architecture,
OS, and MSVC runtime are checked against the requested Rust target.

`WOODSHED_OFFLINE=1` forbids resolving a new provider lock. Installation delegates
offline restrictions to vcpkg `--no-downloads` or Conan `--no-remote`, and errors if
their caches are insufficient. This is not a network sandbox for arbitrary
third-party recipes.

#### Library and offline use

`pin::plan_add` and `pin::plan_bump` accept a `Selection` and a `Resolver`.
Inspect `Plan::changes()` or `Plan::manifest()` before calling `Plan::apply()`.
`pin::Online` supplies host-Git metadata and archive downloads.
`pin::Offline::load` reads strict JSON fixtures without network fallback:

```json
{
    "refs": [
        {
            "repository": "https://example.org/parent.git",
            "reference": "v1.3.0",
            "revision": "0123456789abcdef0123456789abcdef01234567"
        }
    ],
    "files": [
        {
            "repository": "https://example.org/parent.git",
            "revision": "0123456789abcdef0123456789abcdef01234567",
            "path": "pins.txt",
            "file": "pins.txt"
        }
    ],
    "gitlinks": [
        {
            "repository": "https://example.org/parent.git",
            "revision": "0123456789abcdef0123456789abcdef01234567",
            "path": "third_party/child",
            "source": {
                "repository": "https://example.org/child.git",
                "revision": "123456789abcdef0123456789abcdef012345678"
            }
        }
    ],
    "archives": [
        { "name": "parent", "file": "parent.archive" },
        { "name": "child", "file": "child.archive" }
    ]
}
```

File references are relative to the JSON directory and must remain inside it.
Archive bytes are hashed, not trusted checksum sidecars. Each metadata lookup
must have exactly one match. Implement `Resolver` for another authenticated
metadata service or a project's semantic checks; the planner still owns
traversal, immutable pin validation, and the transaction.

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

## Application bundles

Enable `bundle` to use `woodshed::bundle_cargo`, or `cli` for:

```text
woodshed bundle --output dist -- --manifest-path app/Cargo.toml --bin app --release
```

The destination must be new. This builds one executable with `cargo rustc`,
adds an executable-relative rpath on Linux/macOS, and copies native deployment
trees declared by build scripts:

```rust,ignore
println!("cargo:rustc-env=WOODSHED_DEPLOYMENT_DIR={}", runtime_directory.display());
```

The bundler discovers these declarations through Cargo's JSON build messages,
including transitive dependencies. Native integrations must populate the
directory with all required redistributable libraries, helpers, and resources
in their executable-relative layout. Build-only archives/import libraries do
not belong there. The tool does not infer missing native dependencies or
redistribution permissions.

Select exactly one executable with `--bin` or `--example`. Target selection
respects `--target`, `CARGO_BUILD_TARGET`, and Cargo's configuration hierarchy
from the invocation directory (not the `--manifest-path` directory), in that
order. Multi-target builds and CLI `--config` overrides fail explicitly; use
configuration files or Cargo environment variables instead.

Identical resource files from multiple dependencies are shared; conflicting
contents, file/directory collisions, and missing directories fail explicitly.
Files are staged in a temporary sibling directory and renamed into place only
on success. Ordinary failures clean up staging without leaving a partial
destination. An interrupted process may leave a `.woodshed-bundle-*` staging
directory, never a ready-looking output directory. Run cross-built bundles on
their destination platform.

## Environment

| Variable             | Effect                                                 |
| -------------------- | ------------------------------------------------------ |
| `WOODSHED_CACHE`     | Cache location. Defaults to `$CARGO_HOME/woodshed`.    |
| `WOODSHED_OFFLINE=1` | Never download; fail with an actionable error instead. |

## License

MIT OR Apache-2.0
