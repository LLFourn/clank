//! The daemon binary embeds `frontend/dist/` at compile time via
//! `include_dir!`. This build script doesn't run trunk — that's the
//! justfile's job. It only:
//!
//! 1. Emits `rerun-if-changed` so a fresh `trunk build` invalidates
//!    the daemon's cached compile.
//! 2. Sanity-checks that `frontend/dist/index.html` exists, so a bare
//!    `cargo build` (without a prior trunk run) fails fast with a
//!    useful pointer instead of a cryptic `include_dir!` error.

use std::path::Path;

fn main() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dist = workspace_root.join("frontend/dist");

    println!("cargo:rerun-if-changed=frontend/dist/index.html");
    rerun_if_dir_changed(&dist);

    if !dist.join("index.html").is_file() {
        panic!(
            "frontend/dist/index.html missing — the daemon embeds the wasm bundle \
             at compile time. Build the frontend first:\n\n\
             \tjust build           # release bundle + daemon\n\
             \tjust build-frontend  # frontend only\n\n\
             Or directly:  cd frontend && trunk build --release"
        );
    }
}

fn rerun_if_dir_changed(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            rerun_if_dir_changed(&path);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
