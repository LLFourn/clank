//! Shared native-process fixture. Only tests needing exec, pipes or lifecycle
//! ownership use this; ordinary behavior is called directly from Rust tests.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;

#[path = "process/claude.rs"]
mod claude;
#[path = "process/grok.rs"]
mod grok;

fn env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("missing {name}"))
}

fn emit(value: &Value) -> Result<()> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, value)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn context(args: &[String]) -> Result<Value> {
    Ok(json!({
        "pid":std::process::id(), "parent":rustix::process::getppid().map(|p| p.as_raw_pid()),
        "cwd":std::env::current_dir()?, "state":env("CLANK_SOURCE_STATE_DIR")?,
        "argv":&args[..2],
    }))
}

fn setup() -> Result<()> {
    let root = PathBuf::from(env("EXPECTED_ROOT")?);
    for name in [
        "clank-master",
        "clank-reviewer",
        "clank-standby",
        "clank-github",
    ] {
        ensure!(
            root.join("skills").join(name).join("SKILL.md").is_file(),
            "missing skill {name}"
        );
    }
    let tool = env("EXPECTED_TOOL")?;
    let required = match tool.as_str() {
        "claude" => "settings.json",
        "codex" => "hooks.json",
        "opencode" => "plugin/clank.js",
        _ => anyhow::bail!("unexpected tool {tool}"),
    };
    ensure!(root.join(required).is_file(), "missing {required}");
    match tool.as_str() {
        "claude" => {
            ensure!(
                std::env::current_dir()? == std::fs::canonicalize(env("EXPECTED_REPO")?)?,
                "wrong cwd"
            );
            let profile = std::env::var("CLAUDE_CONFIG_DIR")
                .ok()
                .filter(|value| !value.is_empty())
                .map(Ok)
                .unwrap_or_else(|| env("HOME"))?;
            let config: Value = serde_json::from_slice(&std::fs::read(
                PathBuf::from(profile).join(".claude.json"),
            )?)?;
            ensure!(
                config["projects"][env("EXPECTED_TRUST_ROOT")?]["hasTrustDialogAccepted"] == true,
                "workspace not trusted: {config}"
            );
        }
        "codex" => {
            let config = std::fs::read_to_string(root.join("config.toml"))?;
            ensure!(
                config.contains("trust_level = \"trusted\""),
                "workspace not trusted: {config}"
            );
            ensure!(
                !PathBuf::from(env("HOME")?)
                    .join(".codex/config.toml")
                    .exists(),
                "wrote default profile"
            );
        }
        _ => {}
    }
    std::fs::write(env("MARKER")?, tool)?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version") {
        println!("2.1.0 (Claude Code)");
        return Ok(());
    }
    match args.first().map(String::as_str) {
        Some("claude-exec") => claude::exec(&args[1..]),
        Some("claude-hook") => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(claude::hook(&args[1..])),
        Some("claude-question") => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                let mut terminated =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
                tokio::select! {
                    result = claude::run(&args[1..]) => result,
                    _ = terminated.recv() => anyhow::bail!("native fixture terminated"),
                }
            }),
        Some("grok") => grok::run(&args[1..]),
        Some("setup") => setup(),
        Some("events") if args.len() == 4 => {
            // A replacement producer reuses the fixture's endpoint after the
            // monitor has retired its predecessor.
            match std::fs::remove_file(&args[3]) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            let gate = std::os::unix::net::UnixDatagram::bind(&args[3])?;
            let ready = std::os::unix::net::UnixDatagram::unbound()?;
            ready.send_to(&serde_json::to_vec(&context(&args)?)?, &args[2])?;
            let mut buffer = [0u8; 4096];
            let count = gate.recv(&mut buffer)?;
            emit(&serde_json::from_slice(&buffer[..count])?)?;
            // The test owns termination; this is a long-lived event producer.
            loop {
                std::thread::park();
            }
        }
        Some("receipt") if args.len() == 3 => {
            let mut data = context(&args)?;
            data["receipt"] = serde_json::from_reader(std::io::stdin().lock())?;
            std::fs::write(&args[2], serde_json::to_vec(&data)?)?;
            println!("callback stdout");
            eprintln!("callback stderr");
            Ok(())
        }
        _ => anyhow::bail!("unknown process fixture arguments: {args:?}"),
    }
}
