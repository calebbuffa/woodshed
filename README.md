# woodshed

Build-script toolkit for vendoring, caching, and building native C/C++
dependencies from source.

A crate that wraps a native library has to answer the same questions every
time: where does the source come from, how do we know it is the right source,
where does the build go, what happens when two builds run at once, and how does
the result stay linkable. woodshed answers them once.

A woodshed is where you keep the raw material before it is any use _woodshedding_ is the unglamorous practice that makes a performance look effortless.

## Blocks, not a framework

Every piece is independently useful and independently optional, gated behind its
own feature. A crate that only needs checksummed downloads takes `Cache`; one
that drives its own build system takes `Toolchain` and `process::run` and
ignores `CmakeBuild`; one that only wants to find an existing install takes
`probe`. Nothing forces you up a layer.

| Feature                              | Gives you                                                                           |
| ------------------------------------ | ----------------------------------------------------------------------------------- |
| `manifest`                           | `Manifest` - pinned dependencies from `native/versions.toml`                        |
| `download`                           | HTTPS fetching, with a system `curl` fallback for corporate proxies                 |
| `tar-gz`, `tar-bz2`, `tar-xz`, `zip` | Archive extraction, one format each                                                 |
| `cmake`                              | `CmakeBuild` - configure, build, install, with the MSVC runtime under control       |
| `process`                            | `process::run` - Ctrl+C tears down the whole build tree                             |
| `vcpkg`, `pkg-config`                | Probes for an already-installed library                                             |
| `pin`, `cli`                         | `pin::{add, bump, verify}` and the `woodshed` command that maintain `versions.toml` |

### Static vs. dynamic

woodshed leaves the choice to you but makes it hard to get wrong: `Linkage::{Static, Dynamic}`
drives `CmakeBuild::linkage` (`BUILD_SHARED_LIBS`), `BuildKey::linkage` (separate cache
dirs), `emit_link` (link directives, plus `libdir`/`bindir` metadata for dependents),
`link_cpp_runtime_for` (only static libraries need the C++ runtime linked in) and
`copy_shared_libs` (ship the DLLs/`.so` files next to your binary).

### Maintaining pins

```text
cargo install woodshed --features cli
woodshed add geos --version 3.14.1 --url "https://download.osgeo.org/geos/{name}-{version}.tar.bz2"
woodshed bump geos 3.15.0     # rewrites version, re-downloads, updates sha256
woodshed verify               # re-hash every pin
```

`{version}` and `{name}` in `url` and `file` are substituted when a manifest loads, so a
template URL makes a bump a one-field change. Only these commands download to compute a
checksum; `build.rs` never does.

`Cache`, `BuildKey`, `Toolchain`, `probe::Probe`, `verify_sha256`, and
`link_cpp_runtime` are always available. Default features are `manifest`,
`download`, and `tar-gz`.

## The one opinion

**Expensive build trees go in a content-hashed cache directory, never
`OUT_DIR`, and are reused only after an explicit completeness check.**

Cargo deletes `OUT_DIR` whenever the build script changes, so building a
ten-minute dependency there means every one-line edit to `build.rs` costs ten
minutes - and silently discards work that was still valid. `Cache::build_dir`
keys the directory on everything that affects the output, so sharing happens
exactly when it is correct.

## Example

```rust,no_run
use woodshed::{BuildKey, Cache, CmakeBuild, Manifest};

fn main() -> woodshed::Result<()> {
    let manifest = Manifest::load_default(std::env::var("CARGO_MANIFEST_DIR").unwrap())?;
    let dep = manifest.get("geos")?;

    let cache = Cache::discover()?;
    let source = cache.source(dep)?;

    let key = BuildKey::new("geos")
        .dep(dep)
        .mix(woodshed::crt_static().to_string());
    let dirs = cache.build_dir(&key);

    // Reuse a finished build; rebuild anything less.
    if !dirs.is_complete(["include/geos_c.h"]) {
        CmakeBuild::new(&source, &dirs)
            .define("BUILD_SHARED_LIBS", "OFF")
            .install()?;
    }

    println!("cargo:rustc-link-search=native={}/lib", dirs.prefix.display());
    woodshed::link_cpp_runtime();
    Ok(())
}
```

with `native/versions.toml`:

```toml
[geos]
version = "3.14.1"
url = "https://download.osgeo.org/geos/geos-3.14.1.tar.bz2"
sha256 = "3c20919cda9a505db07b5216baa980bacdaa0702da715b43f176fb07eff7e716"
markers = ["CMakeLists.txt", "capi/geos_c.h.in"]
```

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
