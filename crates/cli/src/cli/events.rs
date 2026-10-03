//! `clank events` — the event inbox surface
//! (github-offline-catchup): list the WAL's entries, ack the handled
//! ones, show one record. Every source that writes an `EventLog` is
//! here, github and command sources alike — discovery is the
//! directory glob, so a new backend needs nothing from this file
//! (a-command-is-an-event-source-like-any-other). The agent is inferred from the session
//! binding like `feedback`; every write goes through the same
//! sidecar-locked append path the ingest side uses, so the CLI can
//! run while a wait is live.

use std::path::{Path, PathBuf};

use crate::cli::github::log::{EventLog, EventRow};
use clank_core::ids::AgentLabel;
use clank_core::wait::WaitItem;

mod reply;
use reply::ack_with_reply;

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
            match a.reply.as_deref() {
                Some(text) => {
                    let repo = super::resolve_repo(a.repo.as_deref())?;
                    ack_with_reply(&repo, &dir, rows, &a.ids, text).await
                }
                None => ack(&dir, rows, &a.ids),
            }
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
    // Before the WAL is read, not after: a message still sitting in the
    // old file inbox belongs in this listing, and the reader is where a
    // person finds out it exists.
    let _ = crate::inbox::migrate(&repo, &author);
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
pub(crate) fn join_prompts(
    config: &[clank_core::agent_config::GithubSource],
    dir: &Path,
) -> std::collections::HashMap<String, String> {
    let mut prompts = std::collections::HashMap::new();
    let mut declared = std::collections::HashSet::new();
    for g in config {
        let key = crate::cli::github::supervisor::source_key(g);
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
        match log.read_rows() {
            Ok(Some(read)) => out.extend(read.rows.into_iter().map(|row| Sourced {
                source: stem.to_string(),
                row,
            })),
            Ok(None) => {
                eprintln!("events: {name} is corrupt — skipping (the next wait quarantines it)")
            }
            Err(error) => {
                eprintln!("events: {name}: {error} — skipping; custody preserved for repair")
            }
        }
    }
    // Stable display order: source, then seq.
    out.sort_by(|a, b| (&a.source, a.row.seq).cmp(&(&b.source, b.row.seq)));
    Ok(out)
}

/// How an event is ADDRESSED: `[source@]seq[#N]`.
///
/// A type rather than six `format!`s of one grammar. It is built in
/// two places that must agree — `display_id` for what `clank events
/// list` prints, and the wake that tells an agent how to finish its
/// event (a-command-is-an-event-source-like-any-other) — and parsed
/// in a third. Hand-rolled at each, the only thing that notices a
/// drift is a test that happens to compare two of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selector {
    /// The WAL file stem. Absent when a bare seq is unambiguous
    /// across the directory; a producer stamping a wake always
    /// qualifies, because it cannot see the other logs.
    pub(crate) source: Option<String>,
    pub(crate) seq: u64,
    /// 1-based, and only when ONE source holds distinct events at
    /// this seq (duplicate-writer damage).
    pub(crate) ordinal: Option<usize>,
}

impl Selector {
    pub(crate) fn qualified(source: &str, seq: u64) -> Self {
        Self {
            source: Some(source.to_string()),
            seq,
            ordinal: None,
        }
    }

    fn with_ordinal(&self, ordinal: usize) -> Self {
        Self {
            ordinal: Some(ordinal),
            ..self.clone()
        }
    }
}

impl std::fmt::Display for Selector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(src) = &self.source {
            write!(f, "{src}@")?;
        }
        write!(f, "{}", self.seq)?;
        if let Some(n) = self.ordinal {
            write!(f, "#{n}")?;
        }
        Ok(())
    }
}

impl std::str::FromStr for Selector {
    type Err = anyhow::Error;

    fn from_str(id: &str) -> anyhow::Result<Self> {
        let (rest, ordinal) =
            match id.rsplit_once('#') {
                Some((r, n)) => (
                    r,
                    Some(n.parse::<usize>().map_err(|_| {
                        anyhow::anyhow!("invalid ordinal in `{id}` (expected seq#N)")
                    })?),
                ),
                None => (id, None),
            };
        let (source, seq) = match rest.rsplit_once('@') {
            Some((s, n)) => (Some(s.to_string()), n),
            None => (None, rest),
        };
        Ok(Selector {
            source,
            seq: seq.parse().map_err(|_| {
                anyhow::anyhow!("invalid event id `{id}` (expected [source@]seq[#N])")
            })?,
            ordinal,
        })
    }
}

/// Resolve one user-supplied selector against the rows. A bare seq
/// matching several SOURCES demands the source-qualified form; a seq
/// matching several DISTINCT events in ONE source (duplicate-writer
/// damage) demands the ordinal form — ordinals order by the rows'
/// logical identities, which is total and stable
/// (wal-single-ingest-writer).
fn resolve<'a>(rows: &'a [Sourced], id: &str) -> anyhow::Result<&'a Sourced> {
    let want: Selector = id.parse()?;
    let mut matches: Vec<&Sourced> = rows
        .iter()
        .filter(|s| {
            s.row.seq == want.seq && want.source.as_deref().is_none_or(|src| s.source == src)
        })
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
                .map(|s| Selector::qualified(&s.source, s.row.seq).to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    // One source: order same-seq distinct events by identity (total,
    // stable) for ordinal addressing.
    matches.sort_by(|a, b| a.row.ident.cmp(&b.row.ident));
    let unordered = Selector {
        ordinal: None,
        ..want.clone()
    };
    match (matches.len(), want.ordinal) {
        (1, None | Some(1)) => Ok(matches[0]),
        (_, Some(n)) if n >= 1 && n <= matches.len() => Ok(matches[n - 1]),
        (_, Some(n)) => anyhow::bail!("`{id}`: ordinal {n} out of range (1..={})", matches.len()),
        (_, None) => anyhow::bail!(
            "`{id}` matches {} distinct events at that seq (damaged log); address one: {}",
            matches.len(),
            (1..=matches.len())
                .map(|n| unordered.with_ordinal(n).to_string())
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

/// The SHORTEST selector that addresses this row here: a bare seq
/// when unique, source-qualified across sources, ordinal-qualified
/// when one source holds distinct events at the seq (damaged log).
fn display_id(rows: &[Sourced], s: &Sourced) -> Selector {
    let cross_source = rows
        .iter()
        .any(|o| o.row.seq == s.row.seq && o.source != s.source);
    let base = Selector {
        source: cross_source.then(|| s.source.clone()),
        seq: s.row.seq,
        ordinal: None,
    };
    if !seq_collides(rows, s) {
        return base;
    }
    let mut siblings: Vec<&Sourced> = rows
        .iter()
        .filter(|o| o.source == s.source && o.row.seq == s.row.seq)
        .collect();
    siblings.sort_by(|a, b| a.row.ident.cmp(&b.row.ident));
    base.with_ordinal(
        siblings
            .iter()
            .position(|o| o.row.ident == s.row.ident)
            .map(|i| i + 1)
            .unwrap_or(1),
    )
}

/// One row's one-line description, for any event source.
///
/// ONE arm, not one per backend. The github and command variants had
/// separate arms rendering the same row from differently-named
/// fields, and a third source would have needed a third
/// (a-command-is-an-event-source-like-any-other). What github alone
/// has rides `github`, which is the carve-out.
fn describe(item: &WaitItem) -> String {
    let clean = |text: &str| crate::cli::status_tui::text::one_line(text, usize::MAX);
    if let WaitItem::DraftChanged {
        pr,
        comment,
        path,
        change,
        revision,
        ..
    } = item
    {
        let about = match (comment, path.as_deref()) {
            (Some(id), Some(path)) => format!("  {}  comment {id}", clean(path)),
            _ => String::new(),
        };
        return format!("draft/{}  PR #{pr}{about}  (r{revision})", change.wire());
    }
    let Some(v) = item.event_view() else {
        return format!("{item:?}");
    };
    let mut out = clean(v.kind);
    if let Some(g) = &v.github
        && let Some(d) = g.detail
    {
        out.push_str(&format!("/{}", clean(d)));
    }
    out.push_str(&format!("  {}", clean(v.source)));
    if let Some(g) = &v.github
        && let Some(n) = g.number
    {
        out.push_str(&format!("#{n}"));
    }
    // A source reporting ITSELF broken is not news about the world,
    // and an agent that reads it as news acts on nothing.
    if v.failure {
        out.push_str("  (this SOURCE is broken, not the world)");
    }
    if v.requires_reply {
        out.push_str("  (reply required)");
    }
    if let Some(recurrence) = v.recurrence {
        out.push_str(&format!("  ({})", recurrence_summary(recurrence)));
    }
    if let Some(c) = v.content {
        out.push_str(&format!("  “{}”", clean(c)));
    }
    if let Some(g) = &v.github
        && let Some(a) = g.actor
    {
        out.push_str(&format!("  by {}", clean(a)));
    }
    out
}

pub(crate) fn age(at: u64) -> String {
    crate::age::short(crate::age::elapsed(at as i64))
}

/// Shared safe provenance summary; the original event content stays lossless.
pub(crate) fn recurrence_summary(r: &clank_core::agent_config::TimerRecurrence) -> String {
    let instant = |slot: u64| {
        let due = slot
            .checked_mul(r.generation.interval)
            .and_then(|n| r.generation.anchor.checked_add(n));
        due.and_then(|at| i64::try_from(at).ok())
            .and_then(|at| time::OffsetDateTime::from_unix_timestamp(at).ok())
            .and_then(|at| {
                at.format(&time::format_description::well_known::Rfc3339)
                    .ok()
            })
            .unwrap_or_else(|| {
                due.map_or_else(
                    || "unrepresentable".into(),
                    |at| format!("{at}s since epoch"),
                )
            })
    };
    format!(
        "recurring: {} scheduled occurrence{}; due {}..{}; every {}s",
        r.count,
        if r.count == 1 { "" } else { "s" },
        instant(r.first_slot),
        instant(r.last_slot),
        r.generation.interval
    )
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
        id: display_id(rows, s).to_string(),
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
        println!("no {} events", if all { "logged" } else { "unhandled" });
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
    if shown.iter().any(|s| needs_reply(&s.row.item)) {
        println!("events marked reply required cannot be handled with a plain ack");
    }
    Ok(())
}

fn ack(dir: &Path, rows: Vec<Sourced>, ids: &[String]) -> anyhow::Result<()> {
    // Resolve EVERYTHING first — an error acks nothing (no partial
    // batches to reason about).
    let mut targets = Vec::new();
    for id in ids {
        let s = resolve(&rows, id)?;
        // The SHARED resolution seam judges collision + handled
        // state; `None` here can only mean already handled (the row
        // was just resolved, so it cannot have vanished).
        match resolve_ack_target(dir, &rows, &s.source, s.row.seq, &s.row.ident)? {
            Some(t) => targets.push((t, id)),
            None => println!("{id}: already handled"),
        }
    }
    for (t, id) in targets {
        append_ack_target(&t)?;
        println!("{id}: handled");
    }
    Ok(())
}

fn needs_reply(item: &WaitItem) -> bool {
    item.event_view().is_some_and(|v| v.requires_reply)
}

/// Shared policy for user acknowledgements. This runs in the resolution phase
/// of both CLI and TUI batches, so a required reply aborts before any append.
fn ensure_plain_ack_allowed(dir: &Path, rows: &[Sourced], s: &Sourced) -> anyhow::Result<()> {
    if needs_reply(&s.row.item) || s.row.reply.is_some() {
        let mut selector = display_id(rows, s);
        selector.source = Some(s.source.clone());
        let author = dir
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str());
        let author_arg = author
            .map(|label| format!(" --author {}", crate::shell_quote::shell_quote(label)))
            .unwrap_or_default();
        anyhow::bail!(
            "{selector} requires a reply; use `clank events ack{author_arg} {} --reply <text>`",
            crate::shell_quote::shell_quote(&selector.to_string()),
        );
    }
    Ok(())
}

/// A FULLY RESOLVED ack write: the WAL dir plus the selector,
/// collision state already judged against that WAL's own rows. Both
/// ack surfaces (`clank events ack`, the page fanout) resolve
/// through [`resolve_ack_target`] and append from this one shape, so
/// collision rules cannot drift (tui-github-event-page, codex
/// 7e34106).
pub(crate) struct AckTarget {
    dir: std::path::PathBuf,
    source: String,
    seq: u64,
    ident: String,
}

/// Resolve one copy against ITS WAL's rows: `None` is an explicit
/// no-op (vanished or already handled), `Some` carries the selector,
/// and an event that needs a reply refuses a plain acknowledgement.
fn resolve_ack_target(
    dir: &Path,
    rows: &[Sourced],
    source: &str,
    seq: u64,
    ident: &str,
) -> anyhow::Result<Option<AckTarget>> {
    let Some(s) = rows
        .iter()
        .find(|s| s.source == source && s.row.seq == seq && s.row.ident == ident)
    else {
        return Ok(None);
    };
    if s.row.acked {
        return Ok(None);
    }
    ensure_plain_ack_allowed(dir, rows, s)?;
    Ok(Some(AckTarget {
        dir: dir.to_path_buf(),
        source: source.to_string(),
        seq,
        ident: ident.to_string(),
    }))
}

fn append_ack_target(t: &AckTarget) -> anyhow::Result<()> {
    let mut log = EventLog::open(&t.dir, &t.source)?;
    log.append_checked_ack(t.seq, &t.ident)?;
    drop(log);
    // Every other source is compacted by the loop that owns it. The
    // message source has no such loop, so both of its write paths have
    // to do it — this is the second one, and without it a backlog that
    // is sent and then acked with no later send stays in the WAL for
    // good.
    if t.source == crate::inbox::MESSAGES_SOURCE
        && let Some(label) = t.dir.parent().and_then(|p| p.file_name())
        && let Some(label) = label.to_str()
        && let Ok(label) = clank_core::ids::AgentLabel::parse(label)
        && let Some(repo) = t.dir.ancestors().nth(4)
    {
        crate::inbox::compact_after_ack(repo, &label);
    }
    Ok(())
}

/// Ack a merged component's members ACROSS agent WALs — the event
/// page's fanout (tui-github-event-page). TWO PHASES (codex
/// 7e34106): every involved agent's WALs are read and every member
/// resolved to a typed [`AckTarget`] FIRST — a read failure aborts
/// with nothing written — and only then does the append phase run.
/// Vanished and already-handled members resolve to explicit no-ops.
pub(crate) fn ack_members(
    repo: &Path,
    members: &[crate::cli::github::timeline::MemberKey],
) -> anyhow::Result<usize> {
    let mut by_agent: std::collections::BTreeMap<
        &str,
        Vec<&crate::cli::github::timeline::MemberKey>,
    > = Default::default();
    for m in members {
        by_agent.entry(m.agent.as_str()).or_default().push(m);
    }
    use anyhow::Context as _;
    // Phase 1: read + resolve everything.
    let mut targets: Vec<AckTarget> = Vec::new();
    for (agent, keys) in by_agent {
        let dir = crate::agent_store::agents_root(repo)
            .join(agent)
            .join("events");
        let rows = read_all(&dir)
            .with_context(|| format!("reading `{agent}`'s event WALs for the ack fanout"))?;
        for k in keys {
            if let Some(t) = resolve_ack_target(&dir, &rows, &k.source, k.seq, &k.ident)? {
                targets.push(t);
            }
        }
    }
    // Dedup identical selectors (a caller may pass duplicate keys).
    targets.sort_by(|a, b| {
        (&a.dir, &a.source, a.seq, &a.ident).cmp(&(&b.dir, &b.source, b.seq, &b.ident))
    });
    targets.dedup_by(|a, b| {
        (&a.dir, &a.source, a.seq, &a.ident) == (&b.dir, &b.source, b.seq, &b.ident)
    });
    // Phase 2: append.
    let n = targets.len();
    for t in &targets {
        append_ack_target(t)?;
    }
    Ok(n)
}

/// Terminal-safe text: non-newline control characters collapse to
/// spaces, newlines survive (the events-show cleanup, shared with
/// the TUI event page).
pub(crate) fn sanitize_multiline(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect()
}

/// The full body is useful when it exceeds the displayed single-line
/// content summary, even if both fields contain the same stored text.
fn shown_body(item: &WaitItem) -> Option<String> {
    let view = item.event_view()?;
    let full = sanitize_multiline(view.body?);
    let summary = view
        .content
        .map(|text| crate::cli::status_tui::text::one_line(text, usize::MAX));
    (summary.as_deref() != Some(full.as_str())).then_some(full)
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
    if needs_reply(&s.row.item) {
        println!("reply:     required; plain acknowledgement is refused");
    }
    if let Some(reply) = &s.row.reply {
        println!(
            "answer:    {}",
            sanitize_multiline(&reply.text).replace('\n', "\n           ")
        );
        println!("accepted:  {}", s.row.reply_accepted);
    }
    if let Some(url) = s.row.item.event_view().and_then(|v| v.github?.url) {
        println!("url:       {url}");
    }
    // The whole text, where the event line is only a summary of it.
    // `describe` is a LIST line and stays one; this is the surface for
    // reading what was actually sent, and a message has no other one.
    if let Some(clean) = shown_body(&s.row.item) {
        println!("body:      {}", clean.replace('\n', "\n           "));
    }
    if let Some(p) = prompts.get(&s.source) {
        // Full text (multi-line preserved), but non-newline control
        // characters must not reach the terminal raw; continuation
        // lines align under the value column.
        let clean = sanitize_multiline(p);
        println!("prompt:    {}", clean.replace('\n', "\n           "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::github::log::Transport;

    #[test]
    fn refused_recurring_generation_does_not_hide_healthy_event_logs() {
        let dir = tempdir();
        let mut healthy =
            crate::cli::github::log::EventLog::open(&dir, "timer-config-healthy").unwrap();
        let item = WaitItem::TimerEvent {
            name: "healthy".into(),
            id: "timer/1".into(),
            due_at: 1,
            content: "work".into(),
            selector: None,
            recurrence: None,
        };
        healthy
            .append_inbox(Transport::Timer, Some("timer/1"), None, Some(1), &item)
            .unwrap();
        let bad_key = format!("timer-config-recurring-{}", "a".repeat(64));
        std::fs::write(
            dir.join(format!("{bad_key}.jsonl")),
            "{\"t\":\"future\",\"v\":2,\"seq\":1}\n",
        )
        .unwrap();
        let rows = read_all(&dir).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, "timer-config-healthy");
    }

    #[test]
    fn timer_list_summary_is_safe_and_show_keeps_the_full_body() {
        let dir = tempdir();
        let content = "Check \u{1b}[31mdeployment\u{1b}[0m\t\nReport result\r\nFull body";
        let timer = WaitItem::TimerEvent {
            name: "deploy\nextra name".into(),
            recurrence: None,
            id: "timer/100".into(),
            due_at: 100,
            content: content.into(),
            selector: None,
        };
        let mut log = crate::cli::github::log::EventLog::open(&dir, "timer-config-deploy").unwrap();
        log.append_inbox(Transport::Timer, Some("timer/100"), None, Some(100), &timer)
            .unwrap();
        let rows = read_all(&dir).unwrap();
        let row = resolve(&rows, "timer-config-deploy@1").unwrap();
        let line = describe(&row.row.item);
        assert_eq!(line, "timer  deploy  “Check deployment”");
        assert!(!line.chars().any(char::is_control));
        let body = shown_body(&row.row.item).expect("multiline content must be shown as a body");
        assert!(body.contains("\nReport result \nFull body"), "{body:?}");
        assert!(!body.chars().any(|c| c.is_control() && c != '\n'));
        let json = serde_json::to_value(json_row(&rows, row, &Default::default())).unwrap();
        assert_eq!(json["item"]["content"], content);
        let plain = WaitItem::TimerEvent {
            name: "deploy".into(),
            recurrence: None,
            id: "timer/100".into(),
            due_at: 100,
            content: "one line".into(),
            selector: None,
        };
        assert!(
            shown_body(&plain).is_none(),
            "an identical plain summary needs no repeated body"
        );
    }

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
            content: None,
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

    /// Legacy rows, in the shape the retired source wrote.
    ///
    /// The three `finished` lines and the ack are copied VERBATIM from
    /// this repo's own `.clank/agents/claude/events/repo-lifecycle.jsonl`
    /// — a baselined row, an unacked row and its ack. The
    /// `adhoc_settled` line is SYNTHETIC: no ad-hoc gate ever settled
    /// in this repo, so there was none to copy. It is built from the
    /// serialization the source used (`kind` from the wake-kind table,
    /// `gate` snake_case per `CommitGateState`), and it is the weaker
    /// of the two rows for that reason.
    const LEGACY_LIFECYCLE_JSONL: &str = concat!(
        r#"{"t":"inbox","v":1,"seq":1,"at":1790560465,"baseline":true,"transport":"repo","action_key":"finished:adhoc-review:12ce5676a0d126a7a0c5ab684398ea14e6fe6858","item":{"kind":"finished","plan":"adhoc-review","finalized_at":"12ce5676a0d126a7a0c5ab684398ea14e6fe6858"}}"#,
        "\n",
        r#"{"t":"inbox","v":1,"seq":2,"at":1790560500,"transport":"repo","action_key":"adhoc:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:continued","item":{"kind":"adhoc_settled","sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","gate":"continued"}}"#,
        "\n",
        r#"{"t":"inbox","v":1,"seq":420,"at":1790759854,"transport":"repo","action_key":"finished:send-m-dash:7da6c6f48b8cf5a16ef553112ebbdcf6a20167c4","item":{"kind":"finished","plan":"send-m-dash","finalized_at":"7da6c6f48b8cf5a16ef553112ebbdcf6a20167c4"}}"#,
        "\n",
        r#"{"t":"ack","v":1,"seq":420,"at":1790759860}"#,
        "\n",
    );

    /// A repo upgraded across the retirement still lists its events.
    ///
    /// Every live repo has one of these on disk, most with unacked
    /// rows. The log format promises that a recognized `t` whose
    /// fields no longer parse is FOREIGN rather than corruption —
    /// this holds it to that with the real bytes, because the cost of
    /// being wrong is every upgraded repo reporting a corrupt log and
    /// an unacked row that can never be acked.
    #[test]
    fn a_retired_sources_rows_are_inert_and_do_not_poison_the_directory() {
        let dir = tempdir();
        std::fs::write(dir.join("repo-lifecycle.jsonl"), LEGACY_LIFECYCLE_JSONL).unwrap();

        let mut live = EventLog::open(&dir, "github-o-r-aaaa").unwrap();
        live.load();
        live.append_inbox(Transport::Poll, None, Some("k"), None, &item(1))
            .unwrap();

        // FOREIGN, not corrupt. `read_all` skips both, so asserting
        // only on the listing would pass for a log reported corrupt on
        // every `clank events` in every upgraded repo.
        let legacy = EventLog::open(&dir, "repo-lifecycle").unwrap();
        let read = legacy
            .read_rows()
            .unwrap()
            .expect("a retired variant is foreign, not corruption");
        assert!(read.rows.is_empty(), "no row is readable: {:?}", read.rows);
        assert_eq!(read.foreign, 3, "and all three are kept for custody");

        let rows = read_all(&dir).expect("a legacy log must not fail the listing");
        assert!(
            rows.iter().all(|r| r.source != "repo-lifecycle"),
            "a retired row must never surface again, acked or not: {rows:?}"
        );
        assert_eq!(
            rows.len(),
            1,
            "and the live source beside it still lists: {rows:?}"
        );
        assert_eq!(rows[0].source, "github-o-r-aaaa");
    }

    #[test]
    fn a_draft_change_uses_the_same_inbox_and_ack_as_other_events() {
        let dir = tempdir();
        let key = "draft-pr-7-run-11";
        let mut log = EventLog::open(&dir, key).unwrap();
        log.load();
        let draft = WaitItem::DraftChanged {
            pr: 7,
            instance: 11,
            comment: Some(42),
            path: Some("src/lib.rs".into()),
            change: clank_core::pr_review::DraftChange::Added,
            revision: 1,
            selector: None,
        };
        let seq = log
            .append_inbox(Transport::Draft, None, Some("draft:7:11:1"), None, &draft)
            .unwrap();
        let rows = read_all(&dir).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].row.acked);
        assert_eq!(rows[0].row.item, draft);
        assert!(describe(&rows[0].row.item).contains("PR #7"));
        let target = resolve_ack_target(&dir, &rows, key, seq, &rows[0].row.ident)
            .unwrap()
            .unwrap();
        append_ack_target(&target).unwrap();
        let rows = read_all(&dir).unwrap();
        assert!(rows[0].row.acked);
    }

    /// The selector grammar round-trips, and it is ONE grammar.
    ///
    /// It used to be six `format!`s and a hand-rolled parser. Nothing
    /// related them but a reader, and the two constructors that must
    /// agree — what `list` prints and what a wake hands an agent —
    /// sat in different files
    /// (a-command-is-an-event-source-like-any-other).
    #[test]
    fn a_selector_parses_back_to_what_printed_it() {
        for text in ["7", "src@7", "7#2", "src@7#2", "github-o-r-aaaa@1"] {
            let s: Selector = text.parse().expect(text);
            assert_eq!(s.to_string(), text, "round trip");
        }
        assert_eq!(
            Selector::qualified("command-s-0000abcd", 3).to_string(),
            "command-s-0000abcd@3",
        );
        // A source name may hold `@`; the LAST one separates, so the
        // seq is never swallowed by the name.
        let s: Selector = "a@b@9".parse().unwrap();
        assert_eq!((s.source.as_deref(), s.seq), (Some("a@b"), 9));
        for bad in ["", "x", "src@", "7#x", "@7#"] {
            assert!(
                bad.parse::<Selector>().is_err(),
                "{bad:?} is not a selector"
            );
        }
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
        assert_eq!(display_id(&rows, s).to_string(), "github-o-x-bbbb@1");

        // Ack through the CLI path; the ingest-side view flips.
        ack(&dir, rows, &["github-o-x-bbbb@1".to_string()]).unwrap();
        let rows = read_all(&dir).unwrap();
        let s = resolve(&rows, "github-o-x-bbbb@1").unwrap();
        assert!(s.row.acked);
        let mut fresh = EventLog::open(&dir, "github-o-x-bbbb").unwrap();
        assert!(fresh.load().unhandled.is_empty());
    }

    #[test]
    fn a_required_reply_survives_ingestion_wake_restart_and_producer_replay() {
        use crate::cli::command_source as cs;
        let dir = tempdir();
        let src = clank_core::agent_config::CommandSource {
            name: "questions".into(),
            command: vec!["true".into()],
            reply_command: None,
            receipt_command: None,
            glyph: None,
        };
        let key = src.source_key();
        let mut log = EventLog::open(&dir, &key).unwrap();
        let mut seen = cs::Seen::at(&dir, &key);
        let raw = r#"{"id":"utt/a","content":"Which option?","requires_reply":true}"#;
        let what = cs::ingest_line(&src, &seen, raw).unwrap();
        let mut wake = None;
        assert!(cs::apply(what, &key, &mut log, &mut seen, |item| {
            wake = Some(item);
            true
        }));
        let wake = wake.unwrap();
        let view = wake.event_view().unwrap();
        let selector = view.selector.unwrap().to_owned();
        assert!(view.requires_reply);
        let wire = serde_json::to_value(crate::cli::wait::render_json(&wake)).unwrap();
        assert_eq!(wire["requires_reply"], true);

        let rows = read_all(&dir).unwrap();
        let required = resolve(&rows, &selector).unwrap();
        assert!(describe(&required.row.item).contains("reply required"));
        let json = serde_json::to_value(json_row(&rows, required, &Default::default())).unwrap();
        assert_eq!(json["item"]["requires_reply"], true);
        let err = ack(&dir, rows, &[selector.clone()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires a reply"), "{err}");

        let mut fresh = EventLog::open(&dir, &key).unwrap();
        let loaded = fresh.load();
        assert_eq!(loaded.unhandled.len(), 1);
        assert!(needs_reply(&loaded.unhandled[0].item));
        let mut replay_seen = cs::Seen::at(&dir, &key);
        replay_seen.recover(loaded.feed_ids);
        let weaker = r#"{"id":"utt/a","requires_reply":false}"#;
        assert!(matches!(
            cs::ingest_line(&src, &replay_seen, weaker),
            Some(cs::Ingested::Duplicate)
        ));
        let rows = read_all(&dir).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(needs_reply(&rows[0].row.item));
        assert!(!rows[0].row.acked);
    }

    #[test]
    fn a_required_reply_aborts_cli_and_cross_agent_batches_before_any_ack() {
        use crate::cli::github::timeline::MemberKey;
        let root = tempdir();
        let repo = root.as_path();
        let dirs: Vec<_> = ["alpha", "beta"]
            .into_iter()
            .map(|agent| {
                crate::agent_store::agents_root(repo)
                    .join(agent)
                    .join("events")
            })
            .collect();
        let command = |id: &str, requires_reply| WaitItem::CommandEvent {
            name: "questions".into(),
            id: id.into(),
            event: "question".into(),
            content: "Which option?".into(),
            at: None,
            failure: false,
            requires_reply,
            reply_route: None,
            selector: None,
        };
        let key = "command-questions-abcd";
        for (dir, id, required) in [(&dirs[0], "ordinary", false), (&dirs[1], "required", true)] {
            let mut log = EventLog::open(dir, key).unwrap();
            log.append_inbox(
                Transport::Command,
                Some(id),
                None,
                None,
                &command(id, required),
            )
            .unwrap();
        }
        // CLI batch: even an ordinary event selected first must remain owed.
        let mut alpha = EventLog::open(&dirs[0], key).unwrap();
        alpha.load();
        alpha
            .append_inbox(
                Transport::Command,
                Some("required"),
                None,
                None,
                &command("required", true),
            )
            .unwrap();
        let before: Vec<_> = dirs
            .iter()
            .map(|d| std::fs::read(d.join(format!("{key}.jsonl"))).unwrap())
            .collect();
        let err = ack(
            &dirs[0],
            read_all(&dirs[0]).unwrap(),
            &["1".into(), "2".into()],
        )
        .unwrap_err();
        assert!(err.to_string().contains("requires a reply"));

        // The TUI's fanout uses the same policy across every member's WAL.
        let mut members = Vec::new();
        for (agent, dir) in ["alpha", "beta"].into_iter().zip(&dirs) {
            members.extend(read_all(dir).unwrap().into_iter().map(|s| MemberKey {
                agent: agent.into(),
                source: s.source,
                seq: s.row.seq,
                ident: s.row.ident,
            }));
        }
        let err = ack_members(repo, &members).unwrap_err();
        assert!(err.to_string().contains("requires a reply"));
        for (dir, bytes) in dirs.iter().zip(before) {
            assert_eq!(
                std::fs::read(dir.join(format!("{key}.jsonl"))).unwrap(),
                bytes
            );
            assert!(read_all(dir).unwrap().iter().all(|s| !s.row.acked));
        }
        // An ordinary event is still independently acknowledgeable.
        ack(&dirs[0], read_all(&dirs[0]).unwrap(), &["1".into()]).unwrap();
        let rows = read_all(&dirs[0]).unwrap();
        assert!(rows[0].row.acked);
        assert!(!rows[1].row.acked);
    }

    /// A command source's event walks the WHOLE public surface:
    /// listed, addressed by the selector the WAKE handed the agent,
    /// acked, and then gone for good — including against the
    /// producer replaying it.
    ///
    /// Through this file's own path, not by calling the WAL. The
    /// previous version wrote command events to a private store, so
    /// they could wake an agent that had no supported way to finish
    /// one: the `*.jsonl` glob never saw them (codex a43bd85). What
    /// proves that fixed is `resolve` accepting the exact string the
    /// wake carried, which nothing else here can show.
    #[test]
    fn a_command_event_is_listed_acked_and_stays_acked() {
        use crate::cli::command_source as cs;

        let dir = tempdir();
        let src = clank_core::agent_config::CommandSource {
            name: "deploys".into(),
            command: vec!["true".into()],
            reply_command: None,
            receipt_command: None,
            glyph: None,
        };
        let key = src.source_key();
        let mut log = EventLog::open(&dir, &key).unwrap();
        let mut seen = cs::Seen::at(&dir, &key);
        // Another source's WAL, so a bare seq would be ambiguous and
        // an unqualified selector could not address this event.
        EventLog::open(&dir, "github-o-r-aaaa")
            .unwrap()
            .append_inbox(Transport::Poll, Some("1"), None, None, &item(1))
            .unwrap();

        let raw = r#"{"id":"sha-abc","kind":"deployed","content":"prod is live"}"#;
        let what = cs::ingest_line(&src, &seen, raw).expect("not blank");
        let mut woke = None;
        cs::apply(what, &key, &mut log, &mut seen, |i| {
            woke = Some(i);
            true
        });
        let Some(WaitItem::CommandEvent { selector, .. }) = &woke else {
            panic!("the agent was woken with a command event")
        };
        let selector = selector.clone().expect("the wake says how to ack it");

        // LIST sees it, and renders it as an event rather than a
        // debug dump.
        let rows = read_all(&dir).unwrap();
        let s = resolve(&rows, &selector).expect("the wake's selector addresses the row");
        assert_eq!(s.source, key);
        assert!(!s.row.acked);
        assert_eq!(
            display_id(&rows, s).to_string(),
            selector,
            "list prints what the wake said",
        );
        let shown = describe(&s.row.item);
        assert!(
            shown.contains("deployed") && shown.contains("prod is live"),
            "{shown}",
        );
        // NOT the `{other:?}` catchall, which also contains both of
        // those and would satisfy the line above.
        assert!(
            !shown.contains("CommandEvent"),
            "a debug dump is not a rendering: {shown}",
        );

        // ACK through the CLI path.
        ack(
            &dir,
            read_all(&dir).unwrap(),
            std::slice::from_ref(&selector),
        )
        .unwrap();
        let rows = read_all(&dir).unwrap();
        assert!(resolve(&rows, &selector).unwrap().row.acked);
        // The other source is untouched.
        assert!(!resolve(&rows, "github-o-r-aaaa@1").unwrap().row.acked);

        // RE-ARM: nothing is owed any more…
        let log = EventLog::open(&dir, &key).unwrap();
        assert!(
            log.read_rows()
                .unwrap()
                .unwrap()
                .rows
                .iter()
                .all(|r| r.acked),
            "an acked event is not re-presented",
        );
        // …and the producer replaying it does not resurrect it.
        assert_eq!(
            cs::ingest_line(&src, &seen, raw).expect("not blank"),
            cs::Ingested::Duplicate,
        );
    }

    /// A SECOND arm's event gets its own sequence, and an ack of the
    /// first does not cover it.
    ///
    /// `EventLog::open` starts the allocator at 1 and only `load`
    /// restores it, so a supervisor that merely read rows re-issued
    /// seq 1 on every arm: two distinct events, one selector, and a
    /// plain ack on that seq marking BOTH handled — the second
    /// stopped being pending before anyone saw it (codex 43f5ed8).
    ///
    /// The re-arm test this sits beside only ever REPLAYED old rows,
    /// which is why it could not see this: nothing was appended
    /// after the re-arm.
    #[test]
    fn a_fresh_arm_allocates_past_the_acked_sequence() {
        use crate::cli::command_source as cs;

        let dir = tempdir();
        let src = clank_core::agent_config::CommandSource {
            name: "deploys".into(),
            command: vec!["true".into()],
            reply_command: None,
            receipt_command: None,
            glyph: None,
        };
        let key = src.source_key();

        // ARM 1: ingest A, then ack it through the public path.
        let mut log = EventLog::open(&dir, &key).unwrap();
        let mut seen = cs::Seen::at(&dir, &key);
        log.load();
        let mut sent = Vec::new();
        let what = cs::ingest_line(&src, &seen, r#"{"id":"A","content":"first"}"#).unwrap();
        cs::apply(what, &key, &mut log, &mut seen, |i| {
            sent.push(i);
            true
        });
        let sel_a = selector_of(&sent[0]);
        ack(&dir, read_all(&dir).unwrap(), std::slice::from_ref(&sel_a)).unwrap();

        // ARM 2: a FRESH supervisor, as a re-arm really is.
        let mut log = EventLog::open(&dir, &key).unwrap();
        let mut seen = cs::Seen::at(&dir, &key);
        let loaded = log.load();
        seen.recover(loaded.feed_ids);
        let mut sent = Vec::new();
        let what = cs::ingest_line(&src, &seen, r#"{"id":"B","content":"second"}"#).unwrap();
        cs::apply(what, &key, &mut log, &mut seen, |i| {
            sent.push(i);
            true
        });
        let sel_b = selector_of(&sent[0]);

        assert_ne!(sel_a, sel_b, "a second arm must not reuse the sequence");
        let rows = read_all(&dir).unwrap();
        assert!(resolve(&rows, &sel_a).unwrap().row.acked, "A stays handled");
        assert!(
            !resolve(&rows, &sel_b).unwrap().row.acked,
            "B is UNHANDLED: an ack of A must not reach an event nobody has seen",
        );
    }

    /// A crash between the log append and the index write does not
    /// duplicate the event.
    ///
    /// The WAL is authoritative for what was ingested; the index is
    /// derived. `apply` claimed the WAL's own identity retention
    /// covered this gap — it does not, because `append_inbox` never
    /// looks at producer identities (codex 43f5ed8). What closes it
    /// is reconciling the index FROM the log at arm time.
    #[test]
    fn a_wal_row_with_no_index_entry_is_not_ingested_twice() {
        use crate::cli::command_source as cs;

        let dir = tempdir();
        let src = clank_core::agent_config::CommandSource {
            name: "deploys".into(),
            command: vec!["true".into()],
            reply_command: None,
            receipt_command: None,
            glyph: None,
        };
        let key = src.source_key();
        let raw = r#"{"id":"A","content":"once"}"#;

        let mut log = EventLog::open(&dir, &key).unwrap();
        let mut seen = cs::Seen::at(&dir, &key);
        log.load();
        let what = cs::ingest_line(&src, &seen, raw).unwrap();
        cs::apply(what, &key, &mut log, &mut seen, |_| true);

        // THE CRASH: the row landed, the index write did not. Exactly
        // the state a death between the two leaves behind.
        std::fs::remove_dir_all(dir.join(format!("{key}.seen"))).unwrap();

        let mut log = EventLog::open(&dir, &key).unwrap();
        let mut seen = cs::Seen::at(&dir, &key);
        let loaded = log.load();
        assert!(
            loaded.feed_ids.iter().any(|f| f == "A"),
            "the WAL must still vouch for the id, or this tests nothing",
        );
        seen.recover(loaded.feed_ids);
        assert_eq!(
            cs::ingest_line(&src, &seen, raw).unwrap(),
            cs::Ingested::Duplicate,
            "the log already accounts for this id; a replay must not mint a second event",
        );
        assert_eq!(read_all(&dir).unwrap().len(), 1, "still one row");
    }

    fn selector_of(item: &WaitItem) -> String {
        match item {
            WaitItem::CommandEvent { selector, .. } => {
                selector.clone().expect("a logged event has a selector")
            }
            other => panic!("expected a command event, got {other:?}"),
        }
    }

    /// A WAL WRITTEN BEFORE THIS PLAN still loads, its pending work
    /// is still pending, and it projects through the shared shape.
    ///
    /// These are real persisted bytes, lifted from a live agent's log
    /// (`frostsnap/frostsnap`) rather than round-tripped through
    /// today's types — a fixture built by serializing the current
    /// struct proves only that it agrees with itself. The `baseline`
    /// flag is flipped off on one so there IS pending work to lose.
    ///
    /// Note what `content` holds on a github row: a typed object.
    /// That is why the two variants are not merged on the wire — a
    /// single `content` field cannot be both this and a command
    /// event's plain string, so every row here would stop parsing
    /// (a-command-is-an-event-source-like-any-other, milestone 2).
    #[test]
    fn a_legacy_wal_still_loads_with_its_pending_work_pending() {
        let dir = tempdir();
        let key = "github-frostsnap-frostsnap-3f0cf6c1";
        let rows = [
            r#"{"t":"obs","v":1,"at":1785163044,"feed_id":"12349692683","baseline":true}"#.to_string(),
            r#"{"t":"inbox","v":1,"seq":1,"at":1790139896,"event_at":1790057688,"baseline":true,"transport":"poll","feed_id":"15482061486","action_key":"review#5274674926","item":{"kind":"github_event","repo":"frostsnap/frostsnap","event":"pr_comment","detail":"review","number":600,"title":null,"actor":"musdom","url":null,"content":{"ref":"review","number":600,"id":5274674926}}}"#.to_string(),
            // The same shape, still OWED: baseline off, so it is work.
            r#"{"t":"inbox","v":1,"seq":2,"at":1790139896,"event_at":1789638204,"baseline":false,"transport":"poll","feed_id":"15209027983","action_key":"pr_issue_comment#5712275659","item":{"kind":"github_event","repo":"frostsnap/frostsnap","event":"pr_comment","detail":"issue_comment_on_pr","number":457,"title":"[device] Port to esp-hal v1.2.0","actor":"musdom","url":"https://github.com/frostsnap/frostsnap/pull/457","content":{"ref":"pr_issue_comment","id":5712275659}}}"#.to_string(),
        ];
        std::fs::write(
            dir.join(format!("{key}.jsonl")),
            format!("{}\n", rows.join("\n")),
        )
        .unwrap();

        let mut log = EventLog::open(&dir, key).unwrap();
        let loaded = log.load();
        assert!(
            !loaded.log_quarantined,
            "legacy bytes must not read as corrupt"
        );
        assert!(
            loaded.warm,
            "and must be a WARM start, not a fresh baseline"
        );
        assert_eq!(
            loaded.unhandled.len(),
            1,
            "the un-baselined row is still owed: {:?}",
            loaded.unhandled,
        );

        // It reaches the inspector, and through the SHARED shape.
        let all = read_all(&dir).unwrap();
        let owed = all
            .iter()
            .find(|s| !s.row.acked)
            .expect("one unhandled row");
        let v = owed
            .row
            .item
            .event_view()
            .expect("a github row is an event");
        assert_eq!((v.source, v.kind), ("frostsnap/frostsnap", "pr_comment"));
        assert_eq!(v.content, Some("[device] Port to esp-hal v1.2.0"));

        // The carve-out survived the projection.
        let g = v.github.expect("github rows carry github bits");
        assert_eq!(g.number, Some(457));
        assert_eq!(g.actor, Some("musdom"));
        assert_eq!(g.detail, Some("issue_comment_on_pr"));
        assert_eq!(
            g.url,
            Some("https://github.com/frostsnap/frostsnap/pull/457")
        );
        assert!(g.content.is_some(), "the typed content ref survived");

        // And what `clank events list` actually prints for it. The
        // carve-out has to survive all the way to the line, not just
        // into the projection — a shared renderer that quietly drops
        // github's number or actor looks fine in the view and wrong
        // on screen.
        let line = describe(&owed.row.item);
        assert!(line.contains("pr_comment/issue_comment_on_pr"), "{line}");
        assert!(line.contains("frostsnap/frostsnap#457"), "{line}");
        assert!(line.contains("[device] Port to esp-hal v1.2.0"), "{line}");
        assert!(line.contains("by musdom"), "{line}");

        // And the durable identities the inspector documents.
        assert_eq!(owed.row.feed_id.as_deref(), Some("15209027983"));
        assert_eq!(
            owed.row.action_key.as_deref(),
            Some("pr_issue_comment#5712275659"),
        );
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
        let ids: std::collections::BTreeSet<String> = rows
            .iter()
            .map(|s| display_id(&rows, s).to_string())
            .collect();
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
        ack(&dir, read_all(&dir).unwrap(), &[target.to_string()]).unwrap();
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
    fn ack_members_fans_out_across_agent_wals() {
        // tui-github-event-page: a merged component's members ack
        // across BOTH agents' WALs through the shared core, with the
        // discriminated selector where a WAL's seq is ambiguous, and
        // vanished/already-acked copies skipped.
        use crate::cli::github::timeline::MemberKey;
        let root = tempdir();
        let repo = root.as_path();
        let mk_agent = |agent: &str| {
            let dir = crate::agent_store::agents_root(repo)
                .join(agent)
                .join("events");
            std::fs::create_dir_all(&dir).unwrap();
            dir
        };
        let a = mk_agent("alpha");
        let b = mk_agent("beta");
        let mut la = EventLog::open(&a, "github-o-r-aaaa").unwrap();
        la.append_inbox(Transport::Poll, Some("f-1"), None, None, &item(1))
            .unwrap();
        let mut lb = EventLog::open(&b, "github-o-r-aaaa").unwrap();
        lb.append_inbox(Transport::Relay, Some("f-1"), None, None, &item(1))
            .unwrap();

        let rows_a = read_all(&a).unwrap();
        let rows_b = read_all(&b).unwrap();
        let key = |agent: &str, s: &Sourced| MemberKey {
            agent: agent.into(),
            source: s.source.clone(),
            seq: s.row.seq,
            ident: s.row.ident.clone(),
        };
        let members = vec![key("alpha", &rows_a[0]), key("beta", &rows_b[0])];
        assert_eq!(ack_members(repo, &members).unwrap(), 2, "both copies acked");
        assert!(read_all(&a).unwrap()[0].row.acked);
        assert!(read_all(&b).unwrap()[0].row.acked);
        // Idempotent: a second fanout acks nothing further.
        assert_eq!(ack_members(repo, &members).unwrap(), 0);
        // A vanished copy is skipped, not an error.
        let mut ghost = members.clone();
        ghost.push(MemberKey {
            agent: "alpha".into(),
            source: "github-o-r-aaaa".into(),
            seq: 99,
            ident: "f:nope".into(),
        });
        assert_eq!(ack_members(repo, &ghost).unwrap(), 0);
    }

    #[test]
    fn ack_members_discriminates_damaged_seqs_and_two_phases_on_failure() {
        use crate::cli::github::timeline::MemberKey;
        let root = tempdir();
        let repo = root.as_path();
        let a = crate::agent_store::agents_root(repo)
            .join("alpha")
            .join("events");
        std::fs::create_dir_all(&a).unwrap();
        let mut la = EventLog::open(&a, "github-o-r-aaaa").unwrap();
        la.append_inbox(Transport::Poll, Some("D1"), None, None, &item(1))
            .unwrap();
        // The damaged twin: same agent/source/seq, distinct ident.
        let path = a.join("github-o-r-aaaa.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        let dup = text
            .lines()
            .last()
            .unwrap()
            .replace("\"D1\"", "\"D2\"")
            .replace("\"number\":1", "\"number\":2");
        std::fs::write(&path, format!("{text}{dup}\n")).unwrap();

        let rows = read_all(&a).unwrap();
        assert_eq!(rows.len(), 2);
        // Fan out on ONE member: only the selected ident acks — the
        // discriminated selector at work; a broad seq ack would have
        // covered both.
        let selected = MemberKey {
            agent: "alpha".into(),
            source: rows[0].source.clone(),
            seq: rows[0].row.seq,
            ident: rows[0].row.ident.clone(),
        };
        assert_eq!(ack_members(repo, &[selected.clone()]).unwrap(), 1);
        let rows = read_all(&a).unwrap();
        let acked: Vec<bool> = rows.iter().map(|s| s.row.acked).collect();
        assert_eq!(
            acked.iter().filter(|&&b| b).count(),
            1,
            "exactly the selected member: {acked:?}"
        );

        // TWO-PHASE (codex 7e34106): a later agent's read failure
        // aborts BEFORE any write — alpha's remaining unhandled row
        // stays untouched.
        let beta_events = crate::agent_store::agents_root(repo)
            .join("beta")
            .join("events");
        std::fs::create_dir_all(beta_events.parent().unwrap()).unwrap();
        std::fs::write(&beta_events, "not a directory").unwrap();
        let remaining = rows
            .iter()
            .find(|s| !s.row.acked)
            .map(|s| MemberKey {
                agent: "alpha".into(),
                source: s.source.clone(),
                seq: s.row.seq,
                ident: s.row.ident.clone(),
            })
            .unwrap();
        let beta_key = MemberKey {
            agent: "beta".into(),
            source: "github-o-r-aaaa".into(),
            seq: 1,
            ident: "f:x".into(),
        };
        let err = ack_members(repo, &[remaining, beta_key]).unwrap_err();
        assert!(err.to_string().contains("beta"), "{err}");
        let acked_after: Vec<bool> = read_all(&a).unwrap().iter().map(|s| s.row.acked).collect();
        assert_eq!(acked, acked_after, "phase 1 failure wrote nothing");
    }

    #[test]
    fn watch_prompt_three_state_provenance() {
        // The full arm+join composition (codex 16ea191): configured
        // prompt wins; a configured watch REMOVED from config leaves
        // no prompt (its arm never wrote a sidecar — and removes any
        // leftover); genuine CLI-only metadata stays visible.
        use crate::cli::event_source::SourceOrigin;
        use crate::cli::github::supervisor::write_watch_sidecar;
        use clank_core::agent_config::{Delivery, GithubEventKind, GithubSource};
        let dir = tempdir();
        let gh = |repo: &str, prompt: Option<&str>| GithubSource {
            repo: repo.into(),
            events: vec![GithubEventKind::IssueOpened],
            poll_interval: None,
            watch: Vec::new(),
            include_own_actions: false,
            branches: Vec::new(),
            prs: Vec::new(),
            prompt: prompt.map(str::to_string),
            delivery: Delivery::Poll,
        };
        let key = |g: &GithubSource| crate::cli::github::supervisor::source_key(g);
        let configured = gh("o/a", Some("configured intent"));
        let cli_only = gh("o/c", Some("cli intent"));

        // Arm both: config-origin writes NO sidecar, CLI-origin does.
        write_watch_sidecar(&dir, &configured, SourceOrigin::Config, &key(&configured));
        write_watch_sidecar(&dir, &cli_only, SourceOrigin::Cli, &key(&cli_only));

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
        write_watch_sidecar(&dir, &configured, SourceOrigin::Config, &key(&configured));
        assert!(join_prompts(&[], &dir).get(&key(&configured)).is_none());

        // A SCOPED CLI watch's prompt is found under the stem its rows
        // live under. `join_prompts` indexes by file stem and
        // rendering looks up each row's source stem, so a sidecar
        // under the source's own key while its rows are under a
        // scoped one is simply unreachable — and two invocations of
        // one watch would overwrite the single shared file (codex on
        // 03cc11f).
        let scoped = format!("{}-inv-7", key(&cli_only));
        let other = format!("{}-inv-8", key(&cli_only));
        write_watch_sidecar(&dir, &cli_only, SourceOrigin::Cli, &scoped);
        write_watch_sidecar(
            &dir,
            &gh("o/c", Some("the other invocation")),
            SourceOrigin::Cli,
            &other,
        );
        let map = join_prompts(&[], &dir);
        assert_eq!(
            map.get(&scoped).map(String::as_str),
            Some("cli intent"),
            "a scoped watch's prompt is found under the stem its rows use"
        );
        assert_eq!(
            map.get(&other).map(String::as_str),
            Some("the other invocation"),
            "and a second invocation has its own, rather than overwriting"
        );

        // CLI promptless re-arm removes the CLI sidecar.
        write_watch_sidecar(&dir, &gh("o/c", None), SourceOrigin::Cli, &key(&cli_only));
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
            watch: Vec::new(),
            include_own_actions: false,
            branches: Vec::new(),
            prs: Vec::new(),
            prompt: prompt.map(str::to_string),
            delivery: Delivery::Poll,
        };
        let declared_prompted = gh("o/a", Some("from config"));
        let declared_promptless = gh("o/b", None);
        let cli_only = gh("o/c", Some("from --event"));
        let key = |g: &GithubSource| crate::cli::github::supervisor::source_key(g);
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
