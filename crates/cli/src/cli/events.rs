//! `clank events` — the github event inbox surface
//! (github-offline-catchup): list the WAL's entries, ack the handled
//! ones, show one record. The agent is inferred from the session
//! binding like `feedback`; every write goes through the same
//! sidecar-locked append path the ingest side uses, so the CLI can
//! run while a wait is live.

use std::path::{Path, PathBuf};

use crate::cli::github_event_log::{EventLog, EventRow};
use clank_core::ids::AgentLabel;
use clank_core::wait::WaitItem;

pub async fn run(args: super::EventsArgs) -> anyhow::Result<()> {
    match args.command {
        super::EventsCmd::List(a) => {
            let dir = events_dir(a.repo.as_deref(), a.author.as_deref())?;
            let rows = read_all(&dir)?;
            list(rows, a.all, a.json)
        }
        super::EventsCmd::Ack(a) => {
            let dir = events_dir(a.repo.as_deref(), a.author.as_deref())?;
            let rows = read_all(&dir)?;
            ack(&dir, rows, &a.ids)
        }
        super::EventsCmd::Show(a) => {
            let dir = events_dir(a.repo.as_deref(), a.author.as_deref())?;
            let rows = read_all(&dir)?;
            show(rows, &a.id, a.json)
        }
    }
}

/// `.clank/agents/<label>/events` for the resolved agent.
fn events_dir(repo: Option<&Path>, author: Option<&str>) -> anyhow::Result<PathBuf> {
    let repo = super::resolve_repo(repo)?;
    let author = match author {
        Some(raw) => {
            AgentLabel::parse(raw).map_err(|e| anyhow::anyhow!("invalid --author `{raw}`: {e}"))?
        }
        None => crate::agent_env::resolve_identity_from_env(&repo)?,
    };
    Ok(crate::agent_store::agents_root(&repo)
        .join(author.as_str())
        .join("events"))
}

/// One listed row, addressable across sources: `source` is the WAL
/// file stem, `seq` the per-source sequence.
#[derive(Debug)]
struct Sourced {
    source: String,
    row: EventRow,
}

/// Every source log's rows, discovery by directory glob. A corrupt
/// log is reported and skipped — inspection must not block on it (the
/// ingest side owns quarantine).
fn read_all(dir: &Path) -> anyhow::Result<Vec<Sourced>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue; // state/lock/corrupt/tmp sidecars
        };
        let log = EventLog::open(dir, stem)?;
        match log.read_rows()? {
            Some(read) => out.extend(read.rows.into_iter().map(|row| Sourced {
                source: stem.to_string(),
                row,
            })),
            None => {
                eprintln!("events: {name} is corrupt — skipping (the next wait quarantines it)")
            }
        }
    }
    // Stable display order: source, then seq.
    out.sort_by(|a, b| (&a.source, a.row.seq).cmp(&(&b.source, b.row.seq)));
    Ok(out)
}

/// Resolve one user-supplied id (`seq` or `<source>@<seq>`) against
/// the rows. A bare seq matching several sources is an error naming
/// the qualified forms.
fn resolve<'a>(rows: &'a [Sourced], id: &str) -> anyhow::Result<&'a Sourced> {
    let (source, seq) = match id.rsplit_once('@') {
        Some((s, n)) => (Some(s), n),
        None => (None, id),
    };
    let seq: u64 = seq
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid event id `{id}` (expected a seq or source@seq)"))?;
    let matches: Vec<&Sourced> = rows
        .iter()
        .filter(|s| s.row.seq == seq && source.is_none_or(|src| s.source == src))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => anyhow::bail!("no event `{id}` — see `clank events list --all`"),
        many => anyhow::bail!(
            "`{id}` is ambiguous; qualify it: {}",
            many.iter()
                .map(|s| format!("{}@{}", s.source, s.row.seq))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Bare seq when unique across sources, qualified otherwise.
fn display_id(rows: &[Sourced], s: &Sourced) -> String {
    let dup = rows
        .iter()
        .filter(|o| o.row.seq == s.row.seq)
        .take(2)
        .count()
        > 1;
    if dup {
        format!("{}@{}", s.source, s.row.seq)
    } else {
        s.row.seq.to_string()
    }
}

fn describe(item: &WaitItem) -> String {
    match item {
        WaitItem::GithubEvent {
            repo,
            event,
            detail,
            number,
            title,
            actor,
            ..
        } => {
            let mut out = event.clone();
            if let Some(d) = detail {
                out.push_str(&format!("/{d}"));
            }
            out.push_str(&format!("  {repo}"));
            if let Some(n) = number {
                out.push_str(&format!("#{n}"));
            }
            if let Some(t) = title {
                out.push_str(&format!("  “{t}”"));
            }
            if let Some(a) = actor {
                out.push_str(&format!("  by {a}"));
            }
            out
        }
        other => format!("{other:?}"),
    }
}

fn age(at: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = now.saturating_sub(at);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// The one JSON envelope for both `list -j` and `show -j`: the
/// addressable id + source alongside the full row (durable identities
/// included) — the documented inspection contract (codex c009500).
#[derive(serde::Serialize)]
struct JsonRow<'a> {
    id: String,
    source: &'a str,
    #[serde(flatten)]
    row: &'a EventRow,
}

fn json_row<'a>(rows: &[Sourced], s: &'a Sourced) -> JsonRow<'a> {
    JsonRow {
        id: display_id(rows, s),
        source: &s.source,
        row: &s.row,
    }
}

fn list(rows: Vec<Sourced>, all: bool, json: bool) -> anyhow::Result<()> {
    let shown: Vec<&Sourced> = rows.iter().filter(|s| all || !s.row.acked).collect();
    if json {
        let out: Vec<JsonRow> = shown.iter().map(|s| json_row(&rows, s)).collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if shown.is_empty() {
        println!(
            "no {} github events",
            if all { "logged" } else { "unhandled" }
        );
        return Ok(());
    }
    for s in &shown {
        let status = if s.row.acked {
            "acked    "
        } else {
            "UNHANDLED"
        };
        println!(
            "{:>10}  {:>4}  {}  {}",
            display_id(&rows, s),
            age(s.row.at),
            status,
            describe(&s.row.item)
        );
    }
    println!("\nack with: clank events ack <id> …   (unacked events keep waking the wait)");
    Ok(())
}

fn ack(dir: &Path, rows: Vec<Sourced>, ids: &[String]) -> anyhow::Result<()> {
    // Resolve EVERYTHING first — an error acks nothing (no partial
    // batches to reason about).
    let mut targets = Vec::new();
    for id in ids {
        let s = resolve(&rows, id)?;
        if s.row.acked {
            println!("{id}: already handled");
            continue;
        }
        targets.push((s.source.clone(), s.row.seq, id));
    }
    for (source, seq, id) in targets {
        let mut log = EventLog::open(dir, &source)?;
        log.append_ack(seq)?;
        println!("{id}: handled");
    }
    Ok(())
}

fn show(rows: Vec<Sourced>, id: &str, json: bool) -> anyhow::Result<()> {
    let s = resolve(&rows, id)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&json_row(&rows, s))?);
        return Ok(());
    }
    println!("id:        {}", display_id(&rows, s));
    println!("source:    {}", s.source);
    println!("logged:    {} ago", age(s.row.at));
    println!("transport: {:?}", s.row.transport);
    println!(
        "status:    {}",
        if s.row.acked { "handled" } else { "UNHANDLED" }
    );
    // The durable identities: which feed event this was (poll) and
    // which action key deduped it (either transport) — the state that
    // explains dedup/catch-up behavior (codex c009500).
    if let Some(fid) = &s.row.feed_id {
        println!("feed id:   {fid}");
    }
    if let Some(key) = &s.row.action_key {
        println!("action:    {key}");
    }
    println!("event:     {}", describe(&s.row.item));
    if let WaitItem::GithubEvent { url: Some(url), .. } = &s.row.item {
        println!("url:       {url}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::github_event_log::Transport;

    fn item(n: u64) -> WaitItem {
        WaitItem::GithubEvent {
            repo: "o/r".into(),
            event: "pr_comment".into(),
            detail: Some("review".into()),
            number: Some(n),
            title: Some("t".into()),
            actor: Some("a".into()),
            url: Some("https://x".into()),
        }
    }

    fn tempdir() -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "clank-events-cli-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_all_merges_sources_and_ack_resolves_ids() {
        let dir = tempdir();
        let mut a = EventLog::open(&dir, "github-o-r-aaaa").unwrap();
        let sa = a
            .append_inbox(Transport::Poll, Some("1"), None, None, &item(1))
            .unwrap();
        let mut b = EventLog::open(&dir, "github-o-x-bbbb").unwrap();
        let sb = b
            .append_inbox(Transport::Relay, None, Some("k#2"), None, &item(2))
            .unwrap();
        assert_eq!((sa, sb), (1, 1), "per-source seqs collide by design");

        let rows = read_all(&dir).unwrap();
        assert_eq!(rows.len(), 2);
        // Bare `1` is ambiguous across the two sources…
        let err = resolve(&rows, "1").unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "{err}");
        assert!(err.contains("github-o-r-aaaa@1"), "{err}");
        // …and the qualified form resolves; display ids qualify too.
        let s = resolve(&rows, "github-o-x-bbbb@1").unwrap();
        assert_eq!(s.source, "github-o-x-bbbb");
        assert_eq!(display_id(&rows, s), "github-o-x-bbbb@1");

        // Ack through the CLI path; the ingest-side view flips.
        ack(&dir, rows, &["github-o-x-bbbb@1".to_string()]).unwrap();
        let rows = read_all(&dir).unwrap();
        let s = resolve(&rows, "github-o-x-bbbb@1").unwrap();
        assert!(s.row.acked);
        let mut fresh = EventLog::open(&dir, "github-o-x-bbbb").unwrap();
        assert!(fresh.load().unhandled.is_empty());
    }

    #[test]
    fn json_envelope_carries_identities_id_and_source() {
        // The documented inspection contract (codex c009500): both
        // list-shaped and show-shaped JSON carry the addressable
        // id + source AND the record's durable identities.
        let dir = tempdir();
        let mut a = EventLog::open(&dir, "github-o-r-aaaa").unwrap();
        a.append_inbox(
            Transport::Poll,
            Some("f-77"),
            Some("issue#7"),
            None,
            &item(7),
        )
        .unwrap();
        let rows = read_all(&dir).unwrap();
        let s = resolve(&rows, "1").unwrap();
        let v = serde_json::to_value(json_row(&rows, s)).unwrap();
        assert_eq!(v["id"], "1");
        assert_eq!(v["source"], "github-o-r-aaaa");
        assert_eq!(v["feed_id"], "f-77");
        assert_eq!(v["action_key"], "issue#7");
        assert_eq!(v["acked"], false);
        assert_eq!(v["transport"], "poll");
        assert_eq!(v["item"]["kind"], "github_event");
        assert_eq!(v["item"]["number"], 7);
        // A relay row has no feed_id — the field is absent, not null.
        let mut b = EventLog::open(&dir, "github-o-x-bbbb").unwrap();
        b.append_inbox(Transport::Relay, None, Some("k#1"), None, &item(1))
            .unwrap();
        let rows = read_all(&dir).unwrap();
        let s = resolve(&rows, "github-o-x-bbbb@1").unwrap();
        let v = serde_json::to_value(json_row(&rows, s)).unwrap();
        assert_eq!(v["id"], "github-o-x-bbbb@1", "collision qualifies the id");
        assert!(v.get("feed_id").is_none());
        assert_eq!(v["action_key"], "k#1");
        assert_eq!(v["transport"], "relay");
    }

    #[test]
    fn resolve_errors_are_actionable() {
        let dir = tempdir();
        let mut a = EventLog::open(&dir, "github-o-r-aaaa").unwrap();
        a.append_inbox(Transport::Poll, Some("1"), None, None, &item(1))
            .unwrap();
        let rows = read_all(&dir).unwrap();
        // Unique bare seq resolves without qualification.
        assert_eq!(resolve(&rows, "1").unwrap().row.seq, 1);
        assert!(resolve(&rows, "9").is_err());
        assert!(resolve(&rows, "not-a-seq").is_err());
        // An empty/missing dir is an empty listing, not an error.
        assert!(read_all(&dir.join("missing")).unwrap().is_empty());
    }
}
