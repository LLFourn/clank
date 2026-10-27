//! The OpenCode plugin's executable lifecycle regression, run under
//! `cargo test`: `tests/opencode_plugin_lifecycle.mjs` loads the shipped
//! plugin through a stub of OpenCode 2.x's loader and drives it with a
//! stub context and a fake `clank`. It needs `node`. There is no CI to
//! supply one, so a machine without it FAILS here rather than passing
//! unexercised; `CLANK_SKIP_NODE_HARNESS=1` opts out explicitly, and only
//! the module syntax (`cli::setup::tests::opencode_plugin_parses_as_a_module`)
//! is then checked.

use std::path::Path;

#[test]
fn the_plugin_runs_its_lifecycle_under_the_2x_loader() {
    let Ok(node) = which::which("node") else {
        if std::env::var_os("CLANK_SKIP_NODE_HARNESS").is_some_and(|v| v == "1") {
            eprintln!(
                "WARNING: SKIPPED the OpenCode plugin lifecycle harness \
                 (CLANK_SKIP_NODE_HARNESS=1): its 2.x export and lifecycle are UNTESTED"
            );
            return;
        }
        panic!(
            "`node` is not on PATH, so the OpenCode plugin's 2.x export and lifecycle \
             cannot be tested. Install Node, or set CLANK_SKIP_NODE_HARNESS=1 to skip \
             it knowingly."
        );
    };
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = std::process::Command::new(node)
        .arg(crate_dir.join("tests/opencode_plugin_lifecycle.mjs"))
        .arg(crate_dir.join("src/cli/setup_assets/opencode_plugin.js"))
        .env_remove("CLANK_BOOTSTRAP_SESSION_ID")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("node runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.trim_end().ends_with("OK"),
        "lifecycle harness failed ({}):\n{stdout}\n{stderr}",
        out.status
    );
}
