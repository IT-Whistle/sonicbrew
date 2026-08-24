//! build script for `gw-alsa-plugin`.
//!
//! Locates libasound via pkg-config on Linux/FreeBSD and emits the link line
//! (`-lasound`, so the plugin `.so` carries a DT_NEEDED on libasound — this is
//! how alsa-plugins' own modules resolve `snd_*` symbols when libasound
//! dlopens them). When alsa cannot be found — e.g. the Linux dev host, which
//! has no libasound2-dev — the `no_alsa_link` cfg is set instead: the extern
//! declarations are replaced by logging stubs, so the cdylib still builds and
//! exports `_snd_pcm_sonicbrew_open` (verifiable with `nm -D`), and opening a
//! live PCM fails gracefully at runtime instead of failing to load.

use std::env;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "linux" && target_os != "freebsd" {
        // No libasound off the Unix family; keep the pure-Rust half compiling.
        println!("cargo:rustc-cfg=no_alsa_link");
        return;
    }

    match probe_alsa() {
        Some(libdir) => {
            println!("cargo:rustc-link-search=native={libdir}");
            println!("cargo:rustc-link-lib=dylib=asound");
        }
        None => {
            println!("cargo:rustc-cfg=no_alsa_link");
            println!(
                "cargo:warning=gw-alsa-plugin: pkg-config did not find alsa; \
                 building with stub FFI (no_alsa_link) — the .so exports \
                 _snd_pcm_sonicbrew_open but cannot drive a live PCM"
            );
        }
    }
}

/// Returns the alsa libdir on success (`pkg-config --variable=libdir alsa`).
fn probe_alsa() -> Option<String> {
    let ok = Command::new("pkg-config")
        .args(["--exists", "alsa"])
        .status()
        .ok()?;
    if !ok.success() {
        return None;
    }
    let out = Command::new("pkg-config")
        .args(["--variable=libdir", "alsa"])
        .output()
        .ok()?;
    let libdir = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    if libdir.is_empty() {
        None
    } else {
        Some(libdir)
    }
}
