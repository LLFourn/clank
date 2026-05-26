use std::path::{Path, PathBuf};

use super::{QueueArgs, QueueCmd, resolve_repo};

pub async fn run(args: QueueArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    match args.command {
        None => list(&repo),
        Some(QueueCmd::Add(a)) => add(&repo, &a.name, a.priority),
        Some(QueueCmd::Remove(r)) => remove(&repo, &r.name),
        Some(QueueCmd::Promote(p)) => promote(&repo, &p.name),
    }
}

fn queue_dir(repo: &Path) -> PathBuf {
    repo.join(".clank/queue")
}

pub struct QueueEntry {
    pub priority: u16,
    pub name: String,
    pub path: PathBuf,
}

pub fn scan_queue(repo: &Path) -> Vec<QueueEntry> {
    let dir = queue_dir(repo);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<QueueEntry> = Vec::new();
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(s) = fname.to_str() else { continue };
        let Some(stem) = s.strip_suffix(".md") else {
            continue;
        };
        if stem.len() < 5 || stem.as_bytes()[3] != b'-' {
            continue;
        }
        let Ok(priority) = stem[..3].parse::<u16>() else {
            continue;
        };
        let name = stem[4..].to_string();
        out.push(QueueEntry {
            priority,
            name,
            path: entry.path(),
        });
    }
    out.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.name.cmp(&b.name)));
    out
}

fn list(repo: &Path) -> anyhow::Result<()> {
    let entries = scan_queue(repo);
    if entries.is_empty() {
        println!("queue is empty");
        return Ok(());
    }
    for e in &entries {
        println!("{:03}-{}", e.priority, e.name);
    }
    Ok(())
}

fn add(repo: &Path, stub_name: &str, priority: u16) -> anyhow::Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME not set"))?;
    if priority > 999 {
        anyhow::bail!("priority must be 0-999");
    }
    let stub_path = home.join(format!(".clank/stubs/{stub_name}.md"));
    if !stub_path.exists() {
        anyhow::bail!("stub `{stub_name}` not found at {}", stub_path.display());
    }
    let dir = queue_dir(repo);
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(format!("{priority:03}-{stub_name}.md"));
    std::fs::copy(&stub_path, &dest)?;
    println!("queued `{stub_name}` at priority {priority:03}");
    Ok(())
}

fn remove(repo: &Path, name: &str) -> anyhow::Result<()> {
    let entries = scan_queue(repo);
    let entry = entries
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| anyhow::anyhow!("`{name}` not in queue"))?;
    std::fs::remove_file(&entry.path)?;
    println!("removed `{name}` from queue");
    Ok(())
}

fn promote(repo: &Path, name: &str) -> anyhow::Result<()> {
    let entries = scan_queue(repo);
    let entry = entries
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| anyhow::anyhow!("`{name}` not in queue"))?;
    let dest = repo.join(format!(".clank/plans/{name}.md"));
    if dest.exists() {
        anyhow::bail!("plan `{name}` already exists in plans/");
    }
    std::fs::create_dir_all(dest.parent().unwrap())?;
    std::fs::rename(&entry.path, &dest)?;
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["add", "--", &format!(".clank/plans/{name}.md")])
        .status()?;
    if !status.success() {
        anyhow::bail!("git add failed");
    }
    let msg = format!("[{name}] intro");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "commit",
            "--quiet",
            &format!(".clank/plans/{name}.md"),
            "-m",
            &msg,
        ])
        .status()?;
    if !status.success() {
        anyhow::bail!("git commit failed");
    }
    println!("promoted `{name}` to active plan");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn scan_queue_returns_sorted_entries() {
        let dir = tempfile::tempdir().unwrap();
        let q = dir.path().join(".clank/queue");
        write(&q.join("200-beta.md"), "# beta\n");
        write(&q.join("100-alpha.md"), "# alpha\n");
        write(&q.join("100-gamma.md"), "# gamma\n");
        let entries = scan_queue(dir.path());
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "alpha");
        assert_eq!(entries[0].priority, 100);
        assert_eq!(entries[1].name, "gamma");
        assert_eq!(entries[2].name, "beta");
    }

    #[test]
    fn scan_queue_ignores_malformed_filenames() {
        let dir = tempfile::tempdir().unwrap();
        let q = dir.path().join(".clank/queue");
        write(&q.join("100-good.md"), "ok\n");
        write(&q.join("bad.md"), "no prefix\n");
        write(&q.join("1000-overflow.md"), "4 digits\n");
        write(&q.join("notes.txt"), "not md\n");
        let entries = scan_queue(dir.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "good");
    }

    #[test]
    fn priority_over_999_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let result = add(dir.path(), "foo", 1000);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("0-999"));
    }
}
