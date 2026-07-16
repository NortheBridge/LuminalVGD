// SPDX-License-Identifier: AGPL-3.0-only
//! No-op for portable builds. With `--features shell` it configures the
//! UMDF binary via wdk-build and generates IddCx bindings from the eWDK
//! headers (environment: docs/BUILDING.md).

fn main() {
    #[cfg(feature = "shell")]
    shell::configure().expect(
        "shell build configuration failed — build inside the eWDK environment \
         with LIBCLANG_PATH set (see docs/BUILDING.md)",
    );
}

#[cfg(feature = "shell")]
mod shell {
    use std::{env, fs, path::PathBuf};

    /// IddCx header/lib version we compile against. 1.10 (Win11 23H2) has
    /// everything the phase-2/4 shell calls; the min-required define below
    /// equals its minor version, so all functions are statically available
    /// (no runtime availability checks needed).
    const IDDCX_VERSION: &str = "1.10";

    pub fn configure() -> Result<(), Box<dyn std::error::Error>> {
        // UMDF link configuration (WdfDriverStubUm, subsystem, entry point).
        wdk_build::configure_wdk_binary_build()?;

        let kits: PathBuf = env::var("WDKContentRoot")
            .map_err(|_| "WDKContentRoot not set")?
            .trim_end_matches('\\')
            .into();
        let ver = env::var("Version_Number")
            .or_else(|_| env::var("WindowsSDKVersion"))
            .map_err(|_| "Version_Number/WindowsSDKVersion not set")?
            .trim_end_matches('\\')
            .to_string();

        let inc = kits.join("Include").join(&ver);
        let iddcx_inc = inc.join("um").join("iddcx").join(IDDCX_VERSION);
        let umdf_src = kits.join("Include").join("wdf").join("umdf").join("2.33");
        let iddcx_lib = kits
            .join("Lib")
            .join(&ver)
            .join("um")
            .join("x64")
            .join("iddcx")
            .join(IDDCX_VERSION);

        println!("cargo:rustc-link-search={}", iddcx_lib.display());
        println!("cargo:rustc-link-lib=static=iddcxstub");

        let out = PathBuf::from(env::var("OUT_DIR")?);
        let umdf_inc = patch_umdf_headers(&umdf_src, &out)?;
        let wrapper = out.join("iddcx_wrapper.h");
        fs::write(
            &wrapper,
            "#include <windows.h>\n#include <wdf.h>\n#include <IddCx.h>\n",
        )?;

        let bindings = bindgen::Builder::default()
            .header(wrapper.to_string_lossy())
            .clang_args([
                format!("-I{}", inc.join("um").display()),
                format!("-I{}", inc.join("shared").display()),
                format!("-I{}", inc.join("ucrt").display()),
                format!("-I{}", iddcx_inc.display()),
                format!("-I{}", umdf_inc.display()),
                "--target=x86_64-pc-windows-msvc".into(),
                // IddCx.h is a C++-flavored header (bare struct-type
                // references); parse as C++ like the MSBuild toolchain does.
                "-x".into(),
                "c++".into(),
                "-std=c++17".into(),
                "-fms-extensions".into(),
                "-fms-compatibility".into(),
                "-DUMDF_VERSION_MAJOR=2".into(),
                "-DUMDF_VERSION_MINOR=33".into(),
                // MSBuild's IddCx integration defines the version triple;
                // min == minor keeps every function statically available.
                "-DIDDCX_VERSION_MAJOR=1".into(),
                "-DIDDCX_VERSION_MINOR=10".into(),
                "-DIDDCX_MINIMUM_VERSION_REQUIRED=10".into(),
            ])
            .allowlist_type("IDD.*|IDARG.*|PFN_IDD.*|DISPLAYCONFIG.*")
            .allowlist_var("IddFunctionCount|IddDriverGlobals|NO_PREFERRED_MODE|IDDCX.*|Idd.*TableIndex")
            // WDF types come from wdk-sys (single source of truth for the
            // WDF ABI); the raw_line glob below resolves the names.
            .blocklist_type("_?P?C?WDF.*")
            .blocklist_type("NTSTATUS")
            .blocklist_item("IddFunctions")
            .raw_line("use wdk_sys::*;")
            .generate()?;
        bindings.write_to_file(out.join("iddcx.rs"))?;
        Ok(())
    }

    /// The WDF headers forward-declare enums as `enum _X : int;` (C++
    /// path) but define them without the fixed underlying type. MSVC
    /// accepts the mismatch; clang's C++ front end rejects it. Copy the
    /// UMDF include dir and stamp the fixed type onto the definitions.
    fn patch_umdf_headers(
        src: &std::path::Path,
        out: &std::path::Path,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let dst = out.join("umdf-2.33-patched");
        fs::create_dir_all(&dst)?;

        // Pass 1: collect tags forward-declared with a fixed type.
        let mut tags = Vec::new();
        let entries: Vec<_> = fs::read_dir(src)?.collect::<Result<_, _>>()?;
        for entry in &entries {
            let text = fs::read_to_string(entry.path())?;
            for line in text.lines() {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("enum ") {
                    if let Some(tag) = rest.strip_suffix(" : int;") {
                        tags.push(tag.trim().to_string());
                    }
                }
            }
        }

        // Pass 2: copy, giving each collected definition the same type.
        for entry in &entries {
            let mut text = fs::read_to_string(entry.path())?;
            for tag in &tags {
                text = text.replace(
                    &format!("typedef enum {tag} {{"),
                    &format!("typedef enum {tag} : int {{"),
                );
            }
            fs::write(dst.join(entry.file_name()), text)?;
        }
        println!("cargo:rerun-if-changed={}", src.display());
        Ok(dst)
    }
}
