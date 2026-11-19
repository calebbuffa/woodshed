# Woodshed

Lock, build, and cache native C/C++ dependencies for Cargo. Woodshed verifies
sources and reuses compatible installations outside Cargo's `OUT_DIR`. You
choose the library, its build options, and which targets to link.

**Prerequisites:** Rust 1.89+, a native C/C++ toolchain and SDK, and CMake
(3.21+ for vcpkg or Conan). Git is required for Git and GitHub source locking.
Woodshed does not install these tools. Cross-builds need a matching CMake
toolchain, vcpkg triplet, or Conan profiles.

## Quick start: build and link fmt from source

Run these commands from your crate's directory:

```sh
cargo install woodshed --version 0.4.1 --features cli
woodshed add fmt --github fmtlib/fmt --version 11.1.4 --cmake-package fmt --target fmt::fmt --define FMT_TEST=OFF --define FMT_DOC=OFF
woodshed setup --dry-run
woodshed setup
cargo build
```

`add` writes `woodshed.toml` (your declaration) and `woodshed.lock` (the exact
source selection and checksum). Commit both. Normal builds verify the lock
and never update it or learn new checksums.

`setup` adds the Cargo build dependency and bridge. It does not overwrite a
custom `build.rs` or replace a conflicting or workspace-inherited dependency.
For an unpublished Woodshed checkout, install the CLI with
`cargo install --path PATH --features cli` and use `woodshed setup --woodshed-path PATH`.

If you already have a `build.rs`, add the dependency yourself and call
`woodshed::build()?` from it:

```toml
[build-dependencies]
woodshed = { version = "0.4.1", features = ["build"] }
```

The generated bridge is:

```rust,no_run
fn main() -> woodshed::Result<()> {
    woodshed::build()
}
```

Enable `git`, `zip`, `tar-bz2`, or `tar-xz` on the build dependency when needed
for your sources. HTTPS downloads and `.tar.gz` extraction are enabled by default.
Woodshed discovers link metadata from installed CMake CONFIG packages and
exported targets; it does not generate FFI bindings or invent upstream install
rules. Source recipes default to a static Release build.

## Use a package manager instead

| Provider | Supply | Enable on the CLI and build dependency |
| --- | --- | --- |
| vcpkg | An existing `VCPKG_ROOT`, port, target triplet, and CMake package/targets | `vcpkg` |
| Conan 2 | The `conan` executable, package reference, explicit profiles, and CMake package/targets | `conan` |

**vcpkg example: Windows x64 MSVC, dynamic CRT and shared zstd.** Use a
triplet matching your actual target and CRT.

```sh
cargo install woodshed --version 0.4.1 --features cli,vcpkg
woodshed add zstd --version 1.5.7 --provider vcpkg --triplet x64-windows --cmake-package zstd --target zstd::libzstd_shared --dynamic
woodshed setup
cargo build
```

**Conan example: static zlib.** Replace `profiles/native` with your own
self-contained profile (no includes or Jinja). Use separate host and build
profiles for cross-compilation.

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

```sh
cargo install woodshed --version 0.4.1 --features cli,conan
woodshed lock
woodshed setup
cargo build --release
```

This assumes a Release profile. Commit it with the manifest and lock, and
relock when the profile or Conan options change. `build-missing = true`
allows Conan to compile missing binaries rather than requiring prebuilt ones.

## Maintain and troubleshoot

| Goal | Command or setting |
| --- | --- |
| Update a source release or vcpkg port | `woodshed bump NAME VERSION` (add `--dry-run` to preview) |
| Validate that declarations match the lock | `woodshed lock --locked` (no network or writes) |
| Refresh a stale lock after editing declarations | `woodshed lock` |
| Reverify locked source selections | `woodshed verify` |
| Inspect feature and target selection | `woodshed tree --features fast`; `woodshed explain NAME --json` |
| Check compiler, linker, and generator | `woodshed doctor` (cross-target checks also require `--toolchain`) |
| Explain the last successful build or reuse | `woodshed cache-explain NAME --json` |
| Inspect or verify cached installations | `woodshed cache inspect`; `woodshed cache verify FULL_ARTIFACT_FINGERPRINT` |
| Build offline | `WOODSHED_OFFLINE=1`; required inputs must already be cached |
| Relocate the cache | `WOODSHED_CACHE` (default: `$CARGO_HOME/woodshed`) |
| See live progress | `cargo build -vv`, or set `WOODSHED_LOG` to a file in an existing directory |

Ignore the `woodshed.toml.pin.lock` and `Cargo.toml.woodshed.lock` writer
sidecars, **not** `woodshed.lock`. If an upstream tag is not `vVERSION`, set
`ref = "release-{version}"` (or its actual format) in the declaration and relock.
For a missing package or target, check the upstream export names; they can
differ from link-library filenames.

On MSVC, target builds match Rust's CRT unless you set `msvc-runtime`
explicitly. This is separate from static/shared library linkage. Keep allocation
and freeing on the same side of a CRT boundary unless the ABI guarantees an
appropriate shared allocator.

For distribution, use `woodshed bundle --output PATH -- --release`, or stage
the native runtime files yourself. Cargo's runtime staging is for `cargo run`
and `cargo test`, not shipping. Declare extra plugins/resources explicitly.

## Reference

- [Build and cache guide](GUIDE.md): source options, native graphs, workspace
  inheritance, pinning, incremental workspaces, diagnostics, and deployment.
- [Rust API](https://docs.rs/woodshed): `Cache` for verified sources, `CmakeBuild`
  and `Toolchain` for custom builds, and `probe` with `pkg-config` or `vcpkg`
  for existing installations.
- `woodshed --help` and `woodshed COMMAND --help`: CLI options and subcommands.

## License

MIT OR Apache-2.0
