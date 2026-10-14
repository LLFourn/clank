// Shared by `build.rs`, which computes the stamp, and by the stamp tests,
// which hold it to its input closure; so it is plain functions, included
// in both.

/// The build-script environment that is part of a build's identity:
/// exact names, then name prefixes.
const STAMP_VARS: &[&str] = &[
    "PROFILE",
    "TARGET",
    "OPT_LEVEL",
    "DEBUG",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTFLAGS",
];
const STAMP_VAR_PREFIXES: &[&str] = &["CARGO_FEATURE_", "CARGO_CFG_", "CARGO_PROFILE_"];

/// Profile keys Cargo documents, for the `CARGO_PROFILE_<P>_<KEY>`
/// overrides a rerun must notice.
const PROFILE_KEYS: &[&str] = &[
    "OPT_LEVEL",
    "DEBUG",
    "DEBUG_ASSERTIONS",
    "OVERFLOW_CHECKS",
    "LTO",
    "PANIC",
    "INCREMENTAL",
    "CODEGEN_UNITS",
    "RPATH",
    "STRIP",
    "SPLIT_DEBUGINFO",
];

/// Environment values hashed that Cargo would not rerun the build
/// script for on its own.
fn stamp_rerun_vars() -> Vec<String> {
    let mut vars: Vec<String> = ["RUSTC", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"]
        .map(String::from)
        .to_vec();
    for profile in ["DEV", "RELEASE", "TEST", "BENCH"] {
        for key in PROFILE_KEYS {
            vars.push(format!("CARGO_PROFILE_{profile}_{key}"));
        }
    }
    vars
}

/// Every regular file under `crates`, by path relative to `workspace`,
/// sorted; `target` and hidden directories excluded.
fn stamp_files(workspace: &std::path::Path) -> Vec<std::path::PathBuf> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "target" {
                continue;
            }
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => walk(&path, out),
                Ok(kind) if kind.is_file() => out.push(path),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(&workspace.join("crates"), &mut out);
    let mut out: Vec<_> = out
        .into_iter()
        .filter_map(|p| p.strip_prefix(workspace).ok().map(|p| p.to_path_buf()))
        .collect();
    out.sort();
    out
}

/// Files outside `crates/` that are part of a build's identity, where
/// present.
const STAMP_MANIFESTS: &[&str] = &["Cargo.toml", "Cargo.lock", ".cargo/config.toml", ".cargo/config"];

/// The stamp of a build from `workspace` under `env`, with `rustc` the
/// compiler's `-vV`. Each item is framed as a tag, a length-prefixed
/// name and a length-prefixed value, so no two input sets serialise
/// alike.
fn stamp_of(
    workspace: &std::path::Path,
    env: &[(String, String)],
    rustc: &str,
) -> std::io::Result<[u8; 16]> {
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    let mut item = |tag: u8, name: &[u8], value: &[u8]| {
        hash.update([tag]);
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name);
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value);
    };
    for file in stamp_files(workspace) {
        let content = std::fs::read(workspace.join(&file))?;
        item(b'f', file.to_string_lossy().as_bytes(), &content);
    }
    for manifest in STAMP_MANIFESTS {
        if let Ok(content) = std::fs::read(workspace.join(manifest)) {
            item(b'm', manifest.as_bytes(), &content);
        }
    }
    let mut vars: Vec<&(String, String)> = env
        .iter()
        .filter(|(name, _)| {
            STAMP_VARS.contains(&name.as_str())
                || STAMP_VAR_PREFIXES.iter().any(|p| name.starts_with(p))
        })
        .collect();
    vars.sort();
    for (name, value) in vars {
        item(b'c', name.as_bytes(), value.as_bytes());
    }
    item(b'r', b"rustc", rustc.as_bytes());
    let digest = hash.finalize();
    let mut stamp = [0u8; 16];
    stamp.copy_from_slice(&digest[..16]);
    Ok(stamp)
}
