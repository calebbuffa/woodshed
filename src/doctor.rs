//! An isolated compile/link check using the same generator and launcher selection as builds.

use crate::{Error, Result, Toolchain};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, process::Command};

/// Explicit target and cross-toolchain selection for a diagnostic build.
#[derive(Clone, Debug)]
pub struct DoctorOptions {
    /// Rust target triple; native cross builds require an explicit toolchain file.
    pub target: String,
    /// CMake cross-toolchain file, if needed.
    pub toolchain: Option<PathBuf>,
}

/// Result of configuring and linking both C and C++ code. No executable is run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DoctorReport {
    /// Selected Rust target triple.
    pub target: String,
    /// Whether configure and compile/link both succeeded.
    pub healthy: bool,
    /// Last attempted phase: configure or build.
    pub phase: String,
    /// Actual selected compiler, linker and generator values from CMake.
    pub tools: BTreeMap<String, String>,
    /// Captured diagnostics; review before sharing because native tools may print paths.
    pub diagnostics: String,
    /// Actionable next steps when the check fails.
    pub advice: Vec<String>,
}

/// Discover rustc's host triple, without guessing from the running executable.
pub fn rust_host() -> Result<String> {
    let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("-vV")
        .output()
        .map_err(|error| Error::io("run rustc to inspect host triple", error))?;
    if output.status.success()
        && let Some(host) = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
    {
        return Ok(host.into());
    }
    Err(crate::project::invalid(format!(
        "rustc did not report its host: {}",
        String::from_utf8_lossy(&output.stderr)
    )))
}

/// Configure and link an isolated C/C++ fixture. Never provisions tools or changes a cache.
pub fn doctor(options: &DoctorOptions) -> Result<DoctorReport> {
    let host = rust_host()?;
    if std::env::var_os("CMAKE_TOOLCHAIN_FILE").is_some() && options.toolchain.is_none() {
        return Err(crate::project::invalid(
            "pass --toolchain explicitly instead of an ambient CMAKE_TOOLCHAIN_FILE",
        ));
    }
    if options.target != host && options.toolchain.is_none() {
        return Err(crate::project::invalid(format!(
            "cross-target doctor {host} -> {} requires --toolchain; no host compiler fallback",
            options.target
        )));
    }
    let toolchain = options
        .toolchain
        .as_ref()
        .map(|path| {
            if !path.is_file() {
                return Err(crate::project::invalid(format!(
                    "doctor toolchain is not a file: {}",
                    path.display()
                )));
            }
            std::path::absolute(path).map_err(|error| Error::io("resolve doctor toolchain", error))
        })
        .transpose()?;
    let root = tempfile::Builder::new()
        .prefix("wd-")
        .tempdir()
        .map_err(|error| Error::io("create toolchain diagnostic fixture", error))?;
    let write = |name: &str, body: &str| {
        std::fs::write(root.path().join(name), body)
            .map_err(|error| Error::io("write toolchain diagnostic fixture", error))
    };
    write(
        "CMakeLists.txt",
        "cmake_minimum_required(VERSION 3.20)\nproject(WoodshedDoctor LANGUAGES C CXX)\nadd_library(c_part STATIC part.c)\nadd_executable(check main.cpp)\ntarget_link_libraries(check PRIVATE c_part)\n",
    )?;
    write("part.c", "int native_answer(void) { return 42; }\n")?;
    write(
        "main.cpp",
        "extern \"C\" int native_answer(void);\nint main() { return native_answer() == 42 ? 0 : 1; }\n",
    )?;
    let cmake = crate::find_on_path("cmake").ok_or_else(|| {
        crate::project::invalid(
            "CMake is missing from PATH; install CMake and restart the build environment",
        )
    })?;
    let detected = Toolchain::detect_for_target(&options.target);
    let build = root.path().join("build");
    let mut configure = Command::new(&cmake);
    configure
        .arg("-S")
        .arg(root.path())
        .arg("-B")
        .arg(&build)
        .arg("-DCMAKE_BUILD_TYPE=Release");
    detected.apply_to_configure_default(&mut configure);
    if let Some(path) = toolchain {
        configure.arg(format!("-DCMAKE_TOOLCHAIN_FILE={}", path.display()));
    }
    let output = configure
        .output()
        .map_err(|error| Error::io("start doctor CMake configure", error))?;
    let mut report = DoctorReport {
        target: options.target.clone(),
        healthy: false,
        phase: "configure".into(),
        tools: BTreeMap::new(),
        diagnostics: format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        advice: Vec::new(),
    };
    let cache = build.join("CMakeCache.txt");
    if cache.exists() {
        let text = std::fs::read_to_string(cache)
            .map_err(|error| Error::io("read doctor CMake tools", error))?;
        for line in text.lines() {
            if let Some((name, value)) = line.split_once('=') {
                let name = name.split(':').next().unwrap_or_default();
                if matches!(
                    name,
                    "CMAKE_C_COMPILER"
                        | "CMAKE_CXX_COMPILER"
                        | "CMAKE_LINKER"
                        | "CMAKE_GENERATOR"
                        | "CMAKE_MAKE_PROGRAM"
                        | "CMAKE_C_COMPILER_LAUNCHER"
                        | "CMAKE_CXX_COMPILER_LAUNCHER"
                ) {
                    report.tools.insert(name.into(), value.into());
                }
            }
        }
    }
    if output.status.success() {
        report.phase = "build".into();
        let output = Command::new(&cmake)
            .arg("--build")
            .arg(&build)
            .args(["--config", "Release"])
            .output()
            .map_err(|error| Error::io("start doctor compile/link", error))?;
        report
            .diagnostics
            .push_str(&String::from_utf8_lossy(&output.stdout));
        report
            .diagnostics
            .push_str(&String::from_utf8_lossy(&output.stderr));
        report.healthy = output.status.success();
    }
    if !report.healthy {
        report.advice.push("Inspect the configure/compiler/linker diagnostics above; doctor never substitutes another target or compiler.".into());
        if options.target.ends_with("-msvc") {
            report.advice.push("Use a Visual Studio Developer shell with the matching C++ workload and Windows SDK; Ninja requires PATH, INCLUDE and LIB from that environment.".into());
        } else {
            report.advice.push("Check that C and C++ compilers, the selected generator, and target system libraries are installed; use an explicit CMake toolchain for cross-compilation.".into());
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_target_requires_toolchain_before_native_configuration() {
        assert!(
            doctor(&DoctorOptions {
                target: "different-target".into(),
                toolchain: None
            })
            .unwrap_err()
            .to_string()
            .contains("requires --toolchain")
        );
    }
}
