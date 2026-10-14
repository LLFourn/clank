include!("build_stamp_inputs.rs");

fn main() {
    let manifest = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace = manifest
        .parent()
        .and_then(|crates| crates.parent())
        .expect("crates/cli sits two below the workspace")
        .to_path_buf();
    println!(
        "cargo::rerun-if-changed={}",
        workspace.join("crates").display()
    );
    for file in STAMP_MANIFESTS {
        println!("cargo::rerun-if-changed={}", workspace.join(file).display());
    }
    for var in stamp_rerun_vars() {
        println!("cargo::rerun-if-env-changed={var}");
    }
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let version = std::process::Command::new(rustc)
        .arg("-vV")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default();
    let env: Vec<(String, String)> = std::env::vars().collect();
    let stamp = stamp_of(&workspace, &env, &version).expect("the build inputs are readable");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(
        out.join("build_stamp.rs"),
        format!("pub(crate) const STAMP: [u8; 16] = {stamp:?};\n"),
    )
    .unwrap();
}
