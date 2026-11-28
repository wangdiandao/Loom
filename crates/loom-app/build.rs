//! Windows MSVC binary hardening build script for `loom-app`.
//!
//! 1. Sets `/DEPENDENTLOADFLAG:0x800` (`LOAD_LIBRARY_SEARCH_SYSTEM32`) so the Windows PE
//!    loader resolves static DLL imports strictly from `%SystemRoot%\System32` and never
//!    from the executable's own folder (preventing DLL sideloading when elevated from
//!    user-writable locations such as `Downloads`).
//! 2. Statically links the MSVC VC++ runtime (`libcmt.lib` + `libvcruntime.lib`) while
//!    keeping Windows' built-in Universal CRT (`ucrt.lib`) dynamic, so portable `loom-app.exe`
//!    runs on clean Windows 10/11 installations without requiring the VC++ Redistributable.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }

    configure_windows_msvc_hardening();
}

fn configure_windows_msvc_hardening() {
    // Enforce LOAD_LIBRARY_SEARCH_SYSTEM32 (0x800) for all static PE imports.
    println!("cargo:rustc-link-arg=/DEPENDENTLOADFLAG:0x800");

    // Link VC++ runtime statically while keeping OS-provided Universal CRT (`ucrt.lib`) dynamic.
    if write_empty_msvcrt_stub() {
        for lib in [
            "libvcruntimed.lib",
            "vcruntime.lib",
            "vcruntimed.lib",
            "libcmtd.lib",
            "msvcrt.lib",
            "msvcrtd.lib",
            "libucrt.lib",
            "libucrtd.lib",
        ] {
            println!("cargo:rustc-link-arg=/NODEFAULTLIB:{lib}");
        }
        for lib in ["libcmt.lib", "libvcruntime.lib", "ucrt.lib"] {
            println!("cargo:rustc-link-arg=/DEFAULTLIB:{lib}");
        }
    }
}

/// Writes a minimal valid empty COFF object named `msvcrt.lib` into `OUT_DIR` so rustc's
/// default `-lmsvcrt` request resolves cleanly when linking `libcmt.lib` + `libvcruntime.lib`.
fn write_empty_msvcrt_stub() -> bool {
    let machine: &[u8] = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => &[0x64, 0x86],
        Ok("aarch64") => &[0x64, 0xAA],
        Ok("x86") => &[0x4C, 0x01],
        _ => return false,
    };

    // Minimal valid COFF object with a single empty `.drectve` section (matches MSVC arch).
    const EMPTY_COFF_BODY: &[u8] = &[
        1, 0, 94, 3, 96, 98, 60, 0, 0, 0, 1, 0, 0, 0, 0, 0, 132, 1, 46, 100, 114, 101, 99, 116,
        118, 101, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 60, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 10, 16, 0, 46, 100, 114, 101, 99, 116, 118, 101, 0, 0, 0, 0, 1, 0, 0, 0, 3, 0, 4, 0,
        0, 0,
    ];

    let Ok(out_dir) = std::env::var("OUT_DIR") else {
        return false;
    };
    let stub_path = Path::new(&out_dir).join("msvcrt.lib");
    let mut bytes = Vec::with_capacity(machine.len() + EMPTY_COFF_BODY.len());
    bytes.extend_from_slice(machine);
    bytes.extend_from_slice(EMPTY_COFF_BODY);

    if std::fs::write(&stub_path, &bytes).is_err() {
        return false;
    }
    println!("cargo:rustc-link-search=native={out_dir}");
    true
}
