//! Prepare the embedded frontend bundle (`frontend_dist/`) before the crate compiles.
//!
//! Two modes:
//! - **Dev** — the `frontend/` submodule is checked out and Node is available: build the Vue app
//!   with Vite and sync the output into `frontend_dist/` (which `rust-embed` embeds). `frontend_dist/`
//!   is a generated, git-ignored directory; it is never committed.
//! - **Consumer** — building from the crates.io tarball (no submodule, no Node): use the
//!   `frontend_dist/` that was vendored into the published `.crate` at publish time. No Node needed.
//!
//! `FASTSL_SKIP_FRONTEND_BUILD=1` forces the consumer path even when the submodule is present.

use std::path::Path;
use std::process::Command;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dist = root.join("frontend_dist");
    let submodule = root.join("frontend");
    let template = submodule.join("swanboard").join("template");

    // Only re-run when the frontend source changes; plain Rust edits must not trigger an npm build.
    println!("cargo:rerun-if-changed=frontend/vue");
    println!("cargo:rerun-if-changed=frontend/package.json");
    println!("cargo:rerun-if-changed=frontend/vite.config.js");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FASTSL_SKIP_FRONTEND_BUILD");

    let skip = std::env::var_os("FASTSL_SKIP_FRONTEND_BUILD").is_some();
    let have_submodule = submodule.join("package.json").exists();

    if skip || !have_submodule {
        // Consumer path: rely on the vendored bundle.
        assert!(
            dist.join("index.html").exists(),
            "no frontend bundle at {} and the `frontend/` submodule is unavailable.\n\
             For a dev build: `git submodule update --init --recursive` (needs Node.js).\n\
             (Published crates ship a prebuilt bundle, so this should not happen for `cargo install`.)",
            dist.display()
        );
        return;
    }

    // Dev path: (re)build the frontend and sync the output into the git-ignored frontend_dist/.
    if !submodule.join("node_modules").exists() {
        run(&mut npm(&["install", "--no-audit", "--no-fund"], &submodule), "npm install");
    }
    run(&mut npm(&["run", "build.release"], &submodule), "npm run build.release");
    assert!(
        template.join("index.html").exists(),
        "frontend build finished but produced no bundle at {}",
        template.display()
    );
    sync_dir(&template, &dist);
}

/// Build an `npm` command. On Windows `npm` is `npm.cmd`, which must be run via `cmd /C`
/// (Rust's `Command` does not resolve `.cmd` shims), so native Windows builds work too.
fn npm(args: &[&str], dir: &Path) -> Command {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg("npm").args(args);
        c
    } else {
        let mut c = Command::new("npm");
        c.args(args);
        c
    };
    cmd.current_dir(dir);
    cmd
}

fn run(cmd: &mut Command, what: &str) {
    let status = cmd.status().unwrap_or_else(|e| {
        panic!("failed to run `{what}`: {e}. Is Node.js/npm installed and on PATH?")
    });
    if !status.success() {
        panic!("`{what}` failed with {status}");
    }
}

/// Replace `dst` with a fresh copy of `src`.
fn sync_dir(src: &Path, dst: &Path) {
    let _ = std::fs::remove_dir_all(dst);
    copy_dir(src, dst);
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}
