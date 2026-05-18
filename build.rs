//! Rebuild the Leptos SPA bundle before the daemon compiles so the
//! resulting binary embeds a fresh `frontend/dist/`. Eliminates the
//! "stale release daemon serves index.html that points at deleted
//! wasm hashes" footgun.
//!
//! Skip the trunk invocation by setting `TRINITY_SKIP_FRONTEND_BUILD=1`
//! (CI artifacts where the dist is pre-staged, or fast iteration on
//! daemon-only code where the existing `frontend/dist/` is fine).

use std::path::Path;
use std::process::Command;

fn main() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let frontend_dir = workspace_root.join("frontend");

    println!("cargo:rerun-if-changed=frontend/Cargo.toml");
    println!("cargo:rerun-if-changed=frontend/index.html");
    println!("cargo:rerun-if-changed=frontend/Trunk.toml");
    println!("cargo:rerun-if-env-changed=TRINITY_SKIP_FRONTEND_BUILD");
    rerun_if_dir_changed(&workspace_root.join("frontend/src"));
    rerun_if_dir_changed(&workspace_root.join("frontend/style"));
    rerun_if_dir_changed(&workspace_root.join("frontend/assets"));
    rerun_if_dir_changed(&workspace_root.join("crates/trinity-core/src"));

    if std::env::var("TRINITY_SKIP_FRONTEND_BUILD").is_ok() {
        println!("cargo:warning=TRINITY_SKIP_FRONTEND_BUILD set; reusing existing frontend/dist/");
        return;
    }

    // Trunk's CLI mishandles `NO_COLOR=1` ("invalid value '1' for
    // '--no-color'"). Cargo + clippy + various CI harnesses set
    // it. Strip the var when invoking trunk so daemon builds work
    // in any environment.
    let status = Command::new("trunk")
        .args(["build", "--release"])
        .current_dir(&frontend_dir)
        .env_remove("NO_COLOR")
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(s) => panic!(
            "`trunk build --release` failed with status {s}; \
             set TRINITY_SKIP_FRONTEND_BUILD=1 to bypass when iterating \
             on daemon-only changes"
        ),
        Err(e) => panic!(
            "could not run `trunk build --release` (is trunk installed? \
             `cargo install trunk`): {e}"
        ),
    }
}

/// Walk `dir` and emit a `cargo:rerun-if-changed=<path>` for every
/// file (so cargo invalidates this build.rs when frontend source
/// content changes, not just when files appear or disappear).
fn rerun_if_dir_changed(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.starts_with('.') || name == "target" {
            continue;
        }
        if path.is_dir() {
            rerun_if_dir_changed(&path);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
