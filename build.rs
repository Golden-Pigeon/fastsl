//! Build the embedded frontend from the `frontend/` submodule before the crate compiles.
//!
//! The frontend source (Vue3 + Vite) lives in the `frontend/` git submodule
//! (a fork of SwanHubX/SwanLab-Dashboard). `npm run build.release` emits the bundle to
//! `frontend/swanboard/template/`, which `FrontendAssets` (rust-embed) then embeds.
//!
//! Cargo only re-runs this script when the frontend sources change (see the
//! `rerun-if-changed` lines), so ordinary Rust edits do not trigger an npm build.
//! Set `FASTSL_SKIP_FRONTEND_BUILD=1` to reuse an already-built bundle without invoking
//! Node (e.g. offline rebuilds); the build still fails if no bundle is present.

use std::path::Path;
use std::process::Command;

fn main() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let frontend = Path::new(manifest).join("frontend");
    let template = frontend.join("swanboard").join("template");
    let index = template.join("index.html");

    // Rebuild the embedded bundle only when the frontend actually changes.
    println!("cargo:rerun-if-changed=frontend/vue");
    println!("cargo:rerun-if-changed=frontend/package.json");
    println!("cargo:rerun-if-changed=frontend/vite.config.js");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FASTSL_SKIP_FRONTEND_BUILD");

    // The submodule must be checked out.
    if !frontend.join("package.json").exists() {
        panic!(
            "frontend submodule is not initialized ({}). Run: git submodule update --init --recursive",
            frontend.display()
        );
    }

    // Escape hatch: reuse the existing bundle without a Node toolchain.
    if std::env::var_os("FASTSL_SKIP_FRONTEND_BUILD").is_some() {
        if !index.exists() {
            panic!(
                "FASTSL_SKIP_FRONTEND_BUILD is set but no bundle exists at {}",
                index.display()
            );
        }
        return;
    }

    // npm install only when dependencies are missing (mirrors upstream build_pypi.py).
    if !frontend.join("node_modules").exists() {
        run(
            Command::new("npm")
                .args(["install", "--no-audit", "--no-fund"])
                .current_dir(&frontend),
            "npm install",
        );
    }
    run(
        Command::new("npm")
            .args(["run", "build.release"])
            .current_dir(&frontend),
        "npm run build.release",
    );

    if !index.exists() {
        panic!(
            "frontend build finished but produced no bundle at {}",
            index.display()
        );
    }
}

fn run(cmd: &mut Command, what: &str) {
    let status = cmd.status().unwrap_or_else(|e| {
        panic!("failed to run `{what}`: {e}. Is Node.js/npm installed and on PATH?")
    });
    if !status.success() {
        panic!("`{what}` failed with {status}");
    }
}
