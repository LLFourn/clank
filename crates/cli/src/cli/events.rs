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
            let (dir, prompts) = events_context(a.repo.as_deref(), a.author.as_deref())?;
            let rows = read_all(&dir)?;
            list(rows, &prompts, a.all, a.json)
        }
        super::EventsCmd::Ack(a) => {
            let (dir, _) = events_context(a.repo.as_deref(), a.author.as_deref())?;
            let rows = read_all(&dir)?;
            ack(&dir, rows, &a.ids)
        }
        super::EventsCmd::Show(a) => {
            let (dir, prompts) = events_context(a.repo.as_deref(), a.author.as_deref())?;
            let rows = read_all(&dir)?;
            show(rows, &prompts, &a.id, a.json)
        }
    }
}

/// The events dir PLUS the source-key → watch-prompt map from the
/// agent's CURRENT config (github-watch-prompts): the prompt is
/// presentation config joined at render time — never stored in the
/// WAL — so events logged before a prompt existed still show it,
/// and edits retitle the standing intent. `source_key` excludes the
/// prompt from source identity, so editing one never re-keys a WAL.
fn events_context(
    repo: Option<&Path>,
    author: Option<&str>,
) -> anyhow::Result<(PathBuf, std::collections::HashMap<String, String>)> {
    let repo = super::resolve_repo(repo)?;
    let author = match author {
        Some(raw) => {
            AgentLabel::parse(raw).map_err(|e| anyhow::anyhow!("invalid --author `{raw}`: {e}"))?
        }
        None => crate::agent_env::resolve_identity_from_env(&repo)?,
    };
    let dir = crate::agent_store::agents_root(&repo)
        .join(author.as_str())
        .join("events");
    let githubs: Vec<clank_core::agent_config::GithubSource> =
        match crate::agent_store::load_agent_config(&repo, &author) {
            Ok(Some(cfg)) => cfg
                .wait_events
                .iter()
                .filter_map(|s| match s {
                    clank_core::agent_config::WaitEventSource::Github(g) => Some(g.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
    let prompts = join_prompts(&githubs, &dir);
    Ok((dir, prompts))
}

/// The source-key → prompt map, two tiers: the agent config is
/// authoritative for sources it DECLARES (a declared-but-promptless
/// source shows nothing, even over a stale sidecar); `.meta.json`
/// sidecars written at arm time cover everything else — notably
/// sources armed only via CLI `--event`, which the config never
/// sees (github-watch-prompts, codex 6a907c6).
fn join_prompts(
    config: &[clank_core::agent_config::GithubSource],
    dir: &Path,
) -> std::collections::HashMap<String, String> {
    let mut prompts = std::collections::HashMap::new();
    let mut declared = std::collections::HashSet::new();
    for g in config {
        let key = crate::cli::github_events::source_key(g);
        if let Some(p) = &g.prompt {
            prompts.insert(key.clone(), p.clone());
        }
        declared.insert(key);
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(key) = name.to_str().and_then(|n| n.strip_suffix(".meta.json")) else {
                continue;
            };
            if declared.contains(key) {
                continue;
            }
            if let Ok(raw) = std::fs::read_to_string(entry.path())
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
                && let Some(p) = v.get("prompt").and_then(|p| p.as_str())
            {
                prompts.insert(key.to_string(), p.to_string());
            }
        }
    }
    prompts
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

/// Resolve one user-supplied id (`[source@]seq[#N]`) against the
/// rows. A bare seq matching several SOURCES demands the
/// source-qualified form; a seq matching several DISTINCT events in
/// ONE source (duplicate-writer damage) demands the ordinal form
/// `seq#N` — ordinals order by the rows' logical identities, which is
/// total and stable (wal-single-ingest-writer).
fn resolve<'a>(rows: &'a [Sourced], id: &str) -> anyhow::Result<&'a Sourced> {
    let (rest, ordinal) = match id.rsplit_once('#') {
        Some((r, n)) => (
            r,
            Some(
                n.parse::<usize>()
                    .map_err(|_| anyhow::anyhow!("invalid ordinal in `{id}` (expected seq#N)"))?,
            ),
        ),
        None => (id, None),
    };
    let (source, seq) = match rest.rsplit_once('@') {
        Some((s, n)) => (Some(s), n),
        None => (None, rest),
    };
    let seq: u64 = seq
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid event id `{id}` (expected [source@]seq[#N])"))?;
    let mut matches: Vec<&Sourced> = rows
        .iter()
        .filter(|s| s.row.seq == seq && source.is_none_or(|src| s.source == src))
        .collect();
    if matches.is_empty() {
        anyhow::bail!("no event `{id}` — see `clank events list --all`");
    }
    let sources: std::collections::BTreeSet<&str> =
        matches.iter().map(|s| s.source.as_str()).collect();
    if sources.len() > 1 {
        anyhow::bail!(
            "`{id}` is ambiguous; qualify it: {}",
            matches
                .iter()
                .map(|s| format!("{}@{}", s.source, s.row.seq))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    // One source: order same-seq distinct events by identity (total,
    // stable) for ordinal addressing.
    matches.sort_by(|a, b| a.row.ident.cmp(&b.row.ident));
    match (matches.len(), ordinal) {
        (1, None | Some(1)) => Ok(matches[0]),
        (_, Some(n)) if n >= 1 && n <= matches.len() => Ok(matches[n - 1]),
        (_, Some(n)) => anyhow::bail!("`{id}`: ordinal {n} out of range (1..={})", matches.len()),
        (_, None) => anyhow::bail!(
            "`{id}` matches {} distinct events at that seq (damaged log); address one: {}",
            matches.len(),
            (1..=matches.len())
                .map(|n| format!("{rest}#{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Whether this row shares its (source, seq) with OTHER distinct
/// events — the case where an ack must be discriminated.
fn seq_collides(rows: &[Sourced], s: &Sourced) -> bool {
    rows.iter()
        .filter(|o| o.source == s.source && o.row.seq == s.row.seq)
        .count()
        > 1
}

/// Bare seq when unique; `source@seq` across sources; `…#N` when one
/// source holds distinct events at the seq (damaged log).
fn display_id(rows: &[Sourced], s: &Sourced) -> String {
    let cross_source = rows
        .iter()
        .any(|o| o.row.seq == s.row.seq && o.source != s.source);
    let base = if cross_source {
        format!("{}@{}", s.source, s.row.seq)
    } else {
        s.row.seq.to_string()
    };
    if seq_collides(rows, s) {
        let mut siblings: Vec<&Sourced> = rows
            .iter()
            .filter(|o| o.source == s.source && o.row.seq == s.row.seq)
            .collect();
        siblings.sort_by(|a, b| a.row.ident.cmp(&b.row.ident));
        let n = siblings
            .iter()
            .position(|o| o.row.ident == s.row.ident)
            .map(|i| i + 1)
            .unwrap_or(1);
        format!("{base}#{n}")
    } else {
        base
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
    /// The source watch's CURRENT prompt (presentation-time join;
    /// github-watch-prompts).
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
    #[serde(flatten)]
    row: &'a EventRow,
}

fn json_row<'a>(
    rows: &[Sourced],
    s: &'a Sourced,
    prompts: &'a std::collections::HashMap<String, String>,
) -> JsonRow<'a> {
    JsonRow {
        id: display_id(rows, s),
        source: &s.source,
        instructions: prompts.get(&s.source).map(String::as_str),
        row: &s.row,
    }
}

fn list(
    rows: Vec<Sourced>,
    prompts: &std::collections::HashMap<String, String>,
    all: bool,
    json: bool,
) -> anyhow::Result<()> {
    let shown: Vec<&Sourced> = rows.iter().filter(|s| all || !s.row.acked).collect();
    if json {
        let out: Vec<JsonRow> = shown.iter().map(|s| json_row(&rows, s, prompts)).collect();
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
        // The watch's standing instructions ride under every event
        // it produced (github-watch-prompts).
        if let Some(p) = prompts.get(&s.source) {
            println!("{:>10}  ↳ {}", "", crate::cli::wait::one_line(p, 300));
        }
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
        targets.push((
            s.source.clone(),
            s.row.seq,
            s.row.ident.clone(),
            seq_collides(&rows, s),
            id,
        ));
    }
    for (source, seq, ident, discriminate, id) in targets {
        let mut log = EventLog::open(dir, &source)?;
        if discriminate {
            log.append_ack_ident(seq, &ident)?;
        } else {
            log.append_ack(seq)?;
        }
        println!("{id}: handled");
    }
    Ok(())
}

fn show(
    rows: Vec<Sourced>,
    prompts: &std::collections::HashMap<String, String>,
    id: &str,
    json: bool,
) -> anyhow::Result<()> {
    let s = resolve(&rows, id)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json_row(&rows, s, prompts))?
        );
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
    if let Some(p) = prompts.get(&s.source) {
        // Full text (multi-line preserved), but non-newline control
        // characters must not reach the terminal raw; continuation
        // lines align under the value column.
        let clean: String = p
            .chars()
            .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
            .collect();
        println!("prompt:    {}", clean.replace('\n', "\n           "));
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
            instructions: None,
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
        let prompts = std::collections::HashMap::from([(
            "github-o-r-aaaa".to_string(),
            "triage and reply".to_string(),
        )]);
        let v = serde_json::to_value(json_row(&rows, s, &prompts)).unwrap();
        assert_eq!(v["id"], "1");
        // Presentation-time join: the CURRENT config's prompt rides
        // rows logged before it existed (github-watch-prompts).
        assert_eq!(v["instructions"], "triage and reply");
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
        let v = serde_json::to_value(json_row(&rows, s, &prompts)).unwrap();
        assert_eq!(v["id"], "github-o-x-bbbb@1", "collision qualifies the id");
        assert!(v.get("feed_id").is_none());
        // No prompt for THIS source: the field is absent, not null.
        assert!(v.get("instructions").is_none());
        assert_eq!(v["action_key"], "k#1");
        assert_eq!(v["transport"], "relay");
    }

    #[test]
    fn damaged_same_seq_events_use_ordinals_and_discriminated_acks() {
        // wal-single-ingest-writer: two DISTINCT events at one seq in
        // one source — display and resolution go ordinal, and acking
        // one writes the discriminated selector clearing only it.
        let dir = tempdir();
        let mut a = EventLog::open(&dir, "github-o-r-aaaa").unwrap();
        a.append_inbox(Transport::Poll, Some("F1"), None, None, &item(1))
            .unwrap();
        let path = dir.join("github-o-r-aaaa.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        let line = text
            .lines()
            .last()
            .unwrap()
            .replace("\"F1\"", "\"F2\"")
            .replace("\"number\":1", "\"number\":2");
        std::fs::write(&path, format!("{text}{line}\n")).unwrap();

        let rows = read_all(&dir).unwrap();
        assert_eq!(rows.len(), 2, "both real events surface");
        // Display ids are ordinal-qualified.
        let ids: std::collections::BTreeSet<String> =
            rows.iter().map(|s| display_id(&rows, s)).collect();
        assert_eq!(
            ids,
            ["1#1".to_string(), "1#2".into()].into_iter().collect(),
            "{ids:?}"
        );
        // A bare seq errors naming the ordinal forms.
        let err = resolve(&rows, "1").unwrap_err().to_string();
        assert!(err.contains("1#1") && err.contains("1#2"), "{err}");
        // Ordinals resolve deterministically (identity order).
        let one = resolve(&rows, "1#1").unwrap();
        let two = resolve(&rows, "1#2").unwrap();
        assert_ne!(one.row.ident, two.row.ident);
        assert!(resolve(&rows, "1#3").is_err());
        // Acking one ordinal clears exactly that event.
        let target = display_id(&rows, two);
        ack(&dir, read_all(&dir).unwrap(), &[target]).unwrap();
        let rows = read_all(&dir).unwrap();
        let acked: Vec<bool> = {
            let mut v: Vec<(&String, bool)> =
                rows.iter().map(|s| (&s.row.ident, s.row.acked)).collect();
            v.sort();
            v.into_iter().map(|(_, a)| a).collect()
        };
        assert_eq!(
            acked.iter().filter(|a| **a).count(),
            1,
            "exactly one event handled: {rows:?}"
        );
        // The written ack carries the discriminator.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.lines()
                .any(|l| l.contains("\"t\":\"ack\"") && l.contains("\"ident\"")),
            "discriminated selector on disk"
        );
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

    #[test]
    fn watch_prompt_three_state_provenance() {
        // The full arm+join composition (codex 16ea191): configured
        // prompt wins; a configured watch REMOVED from config leaves
        // no prompt (its arm never wrote a sidecar — and removes any
        // leftover); genuine CLI-only metadata stays visible.
        use crate::cli::github_events::{SourceOrigin, write_watch_sidecar};
        use clank_core::agent_config::{Delivery, GithubEventKind, GithubSource};
        let dir = tempdir();
        let gh = |repo: &str, prompt: Option<&str>| GithubSource {
            repo: repo.into(),
            events: vec![GithubEventKind::IssueOpened],
            poll_interval: None,
            include_own_actions: false,
            branches: Vec::new(),
            prompt: prompt.map(str::to_string),
            delivery: Delivery::Poll,
        };
        let key = |g: &GithubSource| crate::cli::github_events::source_key(g);
        let configured = gh("o/a", Some("configured intent"));
        let cli_only = gh("o/c", Some("cli intent"));

        // Arm both: config-origin writes NO sidecar, CLI-origin does.
        write_watch_sidecar(&dir, &configured, SourceOrigin::Config);
        write_watch_sidecar(&dir, &cli_only, SourceOrigin::Cli);

        // State 1: configured prompt wins (from config, not disk).
        let map = join_prompts(&[configured.clone()], &dir);
        assert_eq!(map.get(&key(&configured)).unwrap(), "configured intent");
        // State 3: CLI-only metadata visible alongside.
        assert_eq!(map.get(&key(&cli_only)).unwrap(), "cli intent");

        // State 2: the configured watch is DELETED from config — its
        // intent must NOT resurrect.
        let map = join_prompts(&[], &dir);
        assert!(map.get(&key(&configured)).is_none(), "no resurrection");
        assert_eq!(map.get(&key(&cli_only)).unwrap(), "cli intent");

        // Self-heal: a pre-provenance sidecar for a config watch is
        // removed by the next config-origin arm.
        std::fs::write(
            dir.join(format!("{}.meta.json", key(&configured))),
            r#"{"prompt":"stale pre-provenance"}"#,
        )
        .unwrap();
        write_watch_sidecar(&dir, &configured, SourceOrigin::Config);
        assert!(join_prompts(&[], &dir).get(&key(&configured)).is_none());

        // CLI promptless re-arm removes the CLI sidecar.
        write_watch_sidecar(&dir, &gh("o/c", None), SourceOrigin::Cli);
        assert!(join_prompts(&[], &dir).get(&key(&cli_only)).is_none());
    }

    #[test]
    fn join_prompts_two_tiers_config_wins_sidecar_covers_cli_sources() {
        use clank_core::agent_config::{Delivery, GithubEventKind, GithubSource};
        let dir = tempdir();
        let gh = |repo: &str, prompt: Option<&str>| GithubSource {
            repo: repo.into(),
            events: vec![GithubEventKind::IssueOpened],
            poll_interval: None,
            include_own_actions: false,
            branches: Vec::new(),
            prompt: prompt.map(str::to_string),
            delivery: Delivery::Poll,
        };
        let declared_prompted = gh("o/a", Some("from config"));
        let declared_promptless = gh("o/b", None);
        let cli_only = gh("o/c", Some("from --event"));
        let key = |g: &GithubSource| crate::cli::github_events::source_key(g);
        // Sidecars: a STALE one for the declared-promptless source
        // (must be suppressed — config is authoritative for declared
        // sources) and a live one for the CLI-only source.
        std::fs::write(
            dir.join(format!("{}.meta.json", key(&declared_promptless))),
            r#"{"prompt":"stale intent"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("{}.meta.json", key(&cli_only))),
            r#"{"prompt":"from --event"}"#,
        )
        .unwrap();

        let map = join_prompts(
            &[declared_prompted.clone(), declared_promptless.clone()],
            &dir,
        );
        assert_eq!(map.get(&key(&declared_prompted)).unwrap(), "from config");
        assert!(
            map.get(&key(&declared_promptless)).is_none(),
            "declared-promptless suppresses the stale sidecar"
        );
        assert_eq!(
            map.get(&key(&cli_only)).unwrap(),
            "from --event",
            "CLI-only sources join through the sidecar"
        );
    }
}
