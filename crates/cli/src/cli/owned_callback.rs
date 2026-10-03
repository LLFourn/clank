//! Shared bounded callback ownership. Callers decide acceptance and retries;
//! this module owns only stdin, child/group lifetime and recovery identity.
use anyhow::Context;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

pub(crate) struct Request<'a> {
    pub argv: &'a [String],
    pub cwd: &'a Path,
    pub state_dir: &'a Option<PathBuf>,
    pub stdin: &'a [u8],
}

pub(crate) async fn deliver(
    request: Request<'_>,
    call_deadline: tokio::time::Instant,
    deadline: tokio::time::Instant,
    owner_path: &Path,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    recover_owner(owner_path, call_deadline).await?;
    if tokio::time::Instant::now() >= call_deadline {
        anyhow::bail!(
            "backend callback recovery exhausted its invocation budget; retry on a later invocation"
        );
    }
    let mut command = tokio::process::Command::new(&request.argv[0]);
    command
        .args(&request.argv[1..])
        .current_dir(request.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    match request.state_dir {
        Some(dir) => {
            command.env("CLANK_SOURCE_STATE_DIR", dir);
        }
        None => {
            command.env_remove("CLANK_SOURCE_STATE_DIR");
        }
    }
    let mut child = command.spawn().context("cannot start backend callback")?;
    let mut group = crate::cli::wait::GroupKill::over(child.id());
    let pgid = group.0;
    let result = tokio::time::timeout_at(call_deadline, async {
        // No request bytes reach a child that cannot be recognised after our
        // caller is SIGKILLed. ProcToken uses kernel birth identity, not ps.
        let pid = child
            .id()
            .context("backend callback exited before it could be owned")?;
        let token = crate::proc_identity::token_for(pid as i32)
            .context("cannot capture callback process identity")?;
        save_owner(
            owner_path,
            &Owner {
                pgid: pid as i32,
                token,
            },
        )?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let write = async move {
            stdin.write_all(request.stdin).await?;
            drop(stdin); // EOF is part of the protocol; shutdown alone need not close a pipe.
            std::io::Result::Ok(())
        };
        let read = async {
            let mut tail = crate::cli::wait::RingTail::new(4096);
            let mut bytes = [0u8; 4096];
            loop {
                let got = stderr.read(&mut bytes).await?;
                if got == 0 {
                    break;
                }
                tail.push(&bytes[..got]);
            }
            std::io::Result::Ok(tail.into_string())
        };
        let (write, tail, status) = tokio::join!(write, read, child.wait());
        write?;
        let tail = tail?;
        let status = status?;
        if !status.success() {
            anyhow::bail!(
                "backend callback exited {status}: {}",
                crate::cli::events::sanitize_multiline(&tail).trim()
            );
        }
        Ok(())
    })
    .await;
    // End descendants even if their parent exited zero. Reserve time for the
    // reap inside the same absolute deadline, with no orphaned detached task.
    group.terminate();
    let _ = child.start_kill();
    let reaped = tokio::time::timeout_at(deadline, child.wait()).await;
    if !matches!(reaped, Ok(Ok(_))) {
        anyhow::bail!("backend callback cleanup exceeded its deadline; acceptance is uncertain");
    }
    if let Some(pgid) = pgid {
        wait_for_group_exit(pgid, deadline).await?;
    }
    remove_owner(owner_path)?;
    match result {
        Ok(result) => result,
        Err(_) => anyhow::bail!("backend callback did not complete within its invocation budget"),
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Owner {
    pub(crate) pgid: i32,
    pub(crate) token: crate::proc_identity::ProcToken,
}

pub(crate) fn save_owner(path: &Path, owner: &Owner) -> anyhow::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&serde_json::to_vec(owner)?)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(path.parent().context("missing callback owner directory")?)?.sync_all()?;
    Ok(())
}

fn read_owner(path: &Path) -> anyhow::Result<Option<Owner>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "{} is not a usable callback owner record; no callback started",
                path.display()
            )
        })?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn remove_owner(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Recovery runs only inside a new explicit submission holding the dispatch
/// lease. A missing leader over an occupied group cannot be safely identified.
async fn recover_owner(path: &Path, deadline: tokio::time::Instant) -> anyhow::Result<()> {
    let Some(owner) = read_owner(path)? else {
        return Ok(());
    };
    let pgid = crate::signal::Pgid::new(owner.pgid).context("invalid saved reply process group")?;
    if crate::signal::probe_group(pgid) == crate::signal::Probe::Gone {
        return remove_owner(path);
    }
    if !owner.token.still_holds(owner.pgid) {
        anyhow::bail!(
            "cannot identify the previous backend callback group; ownership stays uncertain; inspect {} before retrying",
            path.display()
        );
    }
    crate::signal::signal_group(pgid, libc::SIGKILL);
    wait_for_group_exit(pgid, deadline).await?;
    remove_owner(path)
}

/// A bounded cleanup wait, never an idle task or automatic callback retry.
async fn wait_for_group_exit(
    pgid: crate::signal::Pgid,
    deadline: tokio::time::Instant,
) -> anyhow::Result<()> {
    loop {
        if crate::signal::probe_group(pgid) == crate::signal::Probe::Gone {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "backend callback group cleanup exceeded its deadline; ownership stays uncertain"
            );
        }
        tokio::time::sleep_until(
            (tokio::time::Instant::now() + Duration::from_millis(10)).min(deadline),
        )
        .await;
    }
}
