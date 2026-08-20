//! Zellij tab/pane mirroring for `status --tui`. When running inside
//! zellij, the status pane (which already holds the whole snapshot)
//! mirrors the bar's lamp emoji onto the tab name and each agent's
//! status glyph onto its own pane name. The module IS the namespace —
//! the raw `zellij action …` ops are `zellij::rename_tab` etc. (no
//! `zellij_` stutter); the pure parsers are unit-tested without
//! spawning zellij; [`TabIndicator`] / [`PaneStatus`] own the dedup +
//! restore lifecycle.

use super::derive::{agent_status_emoji, awaited_reviewers};
use super::strip_leading_emoji;
use crate::cli::open_zellij::agent_pane_title;
use crate::cli::status::StatusSnapshot;

/// The current zellij tab's `(stable id, name)`, or `None` outside
/// zellij or if the query fails. Parses `zellij action
/// current-tab-info` (`id: N` / `name: X` lines).
fn current_tab() -> Option<(String, String)> {
    std::env::var_os("ZELLIJ")?;
    let out = std::process::Command::new("zellij")
        .args(["action", "current-tab-info"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_current_tab_info(&String::from_utf8_lossy(&out.stdout))
}

/// Parse `zellij action current-tab-info` into `(id, name)`. The
/// output is ONE FIELD PER LINE — verified live against zellij 0.44.3
/// (`od -c`): `name: <name>\nid: <n>\nposition: …`. Order-independent
/// (scans all lines); `None` if either field is absent (don't rename
/// a tab we can't identify). Pure, so the format is unit-tested
/// without spawning zellij (codex 3cc72aa).
fn parse_current_tab_info(stdout: &str) -> Option<(String, String)> {
    let mut id = None;
    let mut name = None;
    for line in stdout.lines() {
        if let Some(v) = line.strip_prefix("id:") {
            id = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("name:") {
            name = Some(v.trim().to_string());
        }
    }
    Some((id?, name?))
}

fn rename_tab(id: &str, name: &str) {
    // `.output()` (NOT `.status()`): capture + discard the child's
    // stdout/stderr so a rename error never bleeds onto the alt-screen
    // the TUI owns. The loop is event-driven, so an inherited error
    // line would PERSIST until the next watcher event, not flicker
    // (ruthless 9c38523).
    let _ = std::process::Command::new("zellij")
        .args(["action", "rename-tab-by-id", id, name])
        .output();
}

/// Mirrors the bar's emoji into the zellij tab name
/// (tui-tab-mirror-bar-emoji). Captures the tab id + its base name
/// (sans any stale leading glyph) ONCE; renames only on an emoji
/// CHANGE; restores the base name on drop (covers a normal/unwound
/// exit — a signal-killed exit leaves the last glyph, re-synced by the
/// next TUI launch). `None` (no-op) outside zellij. Lifecycle is the
/// pane's: this lives only as long as the `status --tui` process, so
/// there's no separate watcher to leak.
pub(super) struct TabIndicator {
    id: String,
    base: String,
    last: Option<String>,
}

impl TabIndicator {
    pub(super) fn new() -> Option<Self> {
        let (id, name) = current_tab()?;
        Some(Self {
            id,
            base: strip_leading_emoji(&name),
            last: None,
        })
    }

    pub(super) fn update(&mut self, emoji: &str) {
        if emoji.is_empty() || self.last.as_deref() == Some(emoji) {
            return;
        }
        rename_tab(&self.id, &format!("{emoji} {}", self.base));
        self.last = Some(emoji.to_string());
    }
}

impl Drop for TabIndicator {
    fn drop(&mut self) {
        if self.last.is_some() {
            rename_tab(&self.id, &self.base);
        }
    }
}

fn list_panes() -> Option<String> {
    let out = std::process::Command::new("zellij")
        .args(["action", "list-panes"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn rename_pane(id: &str, name: &str) {
    // `.output()` (NOT `.status()`): isolate the child's stdout/stderr
    // from the alt-screen — a stale pane id (closed between list-panes
    // and the rename) or any zellij hiccup must not bleed an error line
    // onto the TUI, which the event-driven loop would leave until the
    // next watcher event (ruthless 9c38523).
    let _ = std::process::Command::new("zellij")
        .args(["action", "rename-pane", "--pane-id", id, name])
        .output();
}

/// The agent panes in `zellij action list-panes` output
/// (`PANE_ID  TYPE  TITLE`, one per line) → `(pane_id, label, role)`.
/// A pane is an agent's iff its title (after stripping any leading
/// status glyph) is `"<label> (master)"` / `"<label> (reviewer)"` —
/// the exact format `agent_pane_title` emits. Non-agent panes
/// (status, plugin, the header row) don't match and are skipped.
fn parse_agent_panes(list_panes_stdout: &str) -> Vec<(String, String, clank_core::vocab::Role)> {
    use clank_core::vocab::Role;
    let mut out = Vec::new();
    for line in list_panes_stdout.lines() {
        let mut toks = line.split_whitespace();
        let Some(id) = toks.next() else { continue };
        toks.next(); // TYPE column
        let base = strip_leading_emoji(&toks.collect::<Vec<_>>().join(" "));
        for role in [Role::Master, Role::Reviewer] {
            if let Some(label) = base.strip_suffix(&format!(" ({})", role.as_str())) {
                out.push((id.to_string(), label.to_string(), role));
                break;
            }
        }
    }
    out
}

/// Per-refresh retitle data, derived ON THE LOOP (pure — no zellij)
/// and sent to the worker: each label's status emoji for BOTH possible
/// pane roles (the pane's actual role comes from the worker's cached
/// titles), plus the wanted set that justifies a pane-map re-query.
/// Pane titles have ONE owner — the worker — so these renames can
/// never race the reconciler's role stamps (codex ff9579f).
#[derive(Debug, PartialEq, Eq)]
pub(super) struct StatusGlyphs {
    /// label → (emoji as master, emoji as reviewer).
    emoji: std::collections::BTreeMap<String, (&'static str, &'static str)>,
    /// Labels the snapshot wants marked (master + awaited reviewers).
    wanted: Vec<String>,
}

impl StatusGlyphs {
    pub(super) fn of(snap: &StatusSnapshot) -> Self {
        use clank_core::vocab::Role;
        let emoji = snap
            .agents
            .iter()
            .map(|a| {
                (
                    a.label.clone(),
                    (
                        agent_status_emoji(snap, &a.label, Role::Master),
                        agent_status_emoji(snap, &a.label, Role::Reviewer),
                    ),
                )
            })
            .collect();
        let mut wanted: Vec<String> = awaited_reviewers(snap)
            .iter()
            .map(|l| l.as_str().to_string())
            .collect();
        if let Some(m) = snap.master.as_deref() {
            wanted.push(m.to_string());
        }
        Self { emoji, wanted }
    }
}

/// Mirrors each AGENT's status glyph onto its OWN pane name
/// (tui-agent-pane-status-emoji). The `status --tui` pane already
/// holds the whole snapshot and can rename any pane by id, so it owns
/// this centrally — the Stop hook stays clean. Renames a pane only
/// when its desired title CHANGES (dedup per id). `None` (no-op)
/// outside zellij; best-effort.
pub(super) struct PaneStatus {
    /// Pane id → last title rendered. Dedup: rename a pane only when
    /// its desired title changes.
    last: std::collections::HashMap<String, String>,
    /// Cached `(pane_id, label, role)` map. The pane→agent mapping is
    /// session-stable, so it's fetched via `list-panes` lazily and
    /// reused — NOT re-shelled once per render (status-tui-watch-cpu
    /// Fix 3: the per-render subprocess was loading the zellij server).
    panes: Vec<(String, String, clank_core::vocab::Role)>,
    primed: bool,
    /// Wanted labels we re-queried for and still found no pane — so a
    /// genuinely paneless agent triggers at most one re-query, not one
    /// per render.
    requeried_absent: std::collections::HashSet<String>,
}

impl PaneStatus {
    /// Lives on the [`ReconcileWorker`] thread (only spawned inside
    /// zellij), so no env gate here.
    fn new() -> Self {
        Self {
            last: std::collections::HashMap::new(),
            panes: Vec::new(),
            primed: false,
            requeried_absent: std::collections::HashSet::new(),
        }
    }

    /// The reconciler acted: cached (pane, label, ROLE) rows may name
    /// pre-relocation roles — drop them so the next retitle pass
    /// re-lists fresh titles instead of stamping stale roles back
    /// (codex ff9579f).
    fn invalidate(&mut self) {
        self.primed = false;
        self.requeried_absent.clear();
        self.last.clear();
        // Rows must go too: if the required re-list FAILS, update_with
        // falls through to iterating whatever is here — stale rows
        // would restamp pre-relocation roles (codex afb6d43).
        self.panes.clear();
    }

    /// Core of [`update`] with the zellij I/O injected, so the caching
    /// logic is testable without spawning (no-binary-spawning-tests).
    /// `list_panes` is called only when the cache needs (re)priming;
    /// `rename` only for panes whose title changed.
    fn update_with(
        &mut self,
        glyphs: &StatusGlyphs,
        mut list_panes: impl FnMut() -> Option<String>,
        mut rename: impl FnMut(&str, &str),
    ) {
        // Refresh the cached pane map only when needed: first run, or
        // when the snapshot wants to mark an agent we have no cached
        // pane for (a pane was likely added). Steady state reuses the
        // cache, so no `list-panes` subprocess fires per render.
        if (!self.primed || self.wants_uncached(&glyphs.wanted))
            && let Some(panes) = list_panes()
        {
            self.panes = parse_agent_panes(&panes);
            self.primed = true;
            // A fresh map supersedes the give-up memory; re-record any
            // wanted label that's STILL absent so we don't re-query for
            // it every render.
            self.requeried_absent.clear();
            for label in &glyphs.wanted {
                if !self.has_pane(label) {
                    self.requeried_absent.insert(label.clone());
                }
            }
        }
        // Build the rename list from the cached map first (immutable
        // borrow), then apply — keeps `self.panes` and `self.last`
        // borrows disjoint.
        let mut renames: Vec<(String, String)> = Vec::new();
        for (id, label, role) in &self.panes {
            // A cached pane whose label the roster no longer knows gets
            // no glyph — leave it; the reconciler owns its fate.
            let Some((master, reviewer)) = glyphs.emoji.get(label) else {
                continue;
            };
            let emoji = match role {
                clank_core::vocab::Role::Master => master,
                clank_core::vocab::Role::Reviewer => reviewer,
            };
            let title = format!("{emoji} {}", agent_pane_title(label, role.as_str()));
            if self.last.get(id).map(String::as_str) != Some(title.as_str()) {
                renames.push((id.clone(), title));
            }
        }
        for (id, title) in renames {
            rename(&id, &title);
            self.last.insert(id, title);
        }
    }

    fn has_pane(&self, label: &str) -> bool {
        self.panes.iter().any(|(_, l, _)| l == label)
    }

    /// A wanted agent has no cached pane and we haven't already given
    /// up re-querying for it — a pane likely appeared since we fetched.
    fn wants_uncached(&self, wanted: &[String]) -> bool {
        wanted
            .iter()
            .any(|label| !self.has_pane(label) && !self.requeried_absent.contains(label))
    }
}

/// Roster→pane reconciliation (tui-zellij-pane-reconcile): the TUI's
/// watcher-driven snapshot refresh is the ONE place zellij pane state
/// follows roster state, so EVERY write path — CLI commands, TUI
/// actions, hand-edited config — converges here. `converged` records
/// the last roster view VERIFIED live (not merely observed): a failed
/// listing or a pass that didn't reach the target leaves it unset, so
/// the next refresh retries instead of dropping the event forever
/// (codex a730882 concern 1). Zellij is not a watched input, so drift
/// created while the roster is stable heals on the next roster change
/// or TUI start, not instantly.
pub(super) struct PaneReconciler {
    converged: Option<RosterView>,
    /// Consecutive failed placement repairs for `failing`. Three of the
    /// four ways placement can fail cannot be fixed by trying again —
    /// reviewers split across tabs, the instrument pane already inside
    /// the stack (zellij has no `break-pane`), and a `stack-panes` the
    /// server silently rejected. Retrying those forever is what turned
    /// a wrong verdict into permanent focus churn, so repair is bounded
    /// independently of whether the predicate is right
    /// (placement-reads-zellij-stacks-correctly).
    failed_repairs: u32,
    failing: Option<RosterView>,
    /// Labels whose pane this reconciler created and has not yet seen
    /// in a listing, with the passes each is still believed for.
    ///
    /// A snapshot is a LAGGING observation: `new-pane` returns before
    /// the listing reports the pane, and `plan.add` is re-derived from
    /// a fresh snapshot every pass. Without this the pass that follows
    /// a creation sees the label as still missing and opens a SECOND
    /// pane for it — the duplicate reviewer this exists to prevent.
    pending: std::collections::BTreeMap<String, u32>,
}

/// Consecutive FAILING passes allowed for one roster before the tab is
/// left alone — covering restacking AND add / remove / relocate, since
/// any of them can fail verification forever and each acts on the tab
/// (codex on dfeff8c). Small on purpose: an action that works, works
/// on the first pass. `clank open` remains the escape hatch, as it
/// already is for a lone reviewer sharing the instrument pane's
/// stack.
const MAX_FAILED_PASSES: u32 = 2;

/// Passes a created-but-unlisted pane stays believed in. It MUST
/// expire: a `new-pane` that reported an id which never reaches a
/// listing would otherwise block its label from ever being opened
/// again, leaving that agent permanently unstarted. One lagging
/// listing is tolerated, then the label is retried.
const PENDING_CREATE_PASSES: u32 = 2;

/// The roster facts panes depend on: the member set and who is master.
/// Role flips between reviewer tiers keep the same pane, so tiers are
/// deliberately NOT part of the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RosterView {
    labels: std::collections::BTreeSet<String>,
    master: Option<String>,
}

impl RosterView {
    fn of(snap: &StatusSnapshot) -> Self {
        Self {
            labels: snap.agents.iter().map(|a| a.label.clone()).collect(),
            master: snap.master.clone(),
        }
    }
}

/// What one reconcile pass must do: open panes for roster members with
/// none, close panes whose label left the roster (closing kills the
/// pane's process tree — that is what guarantees the agent exits), and
/// re-layout on a master change. `live` is `(label, master_titled)`
/// pairs from the actual panes; the stage's CURRENT owner comes from
/// the titles, so a master swap done while no TUI was running is still
/// detected on startup (codex a730882 concern 2). Pure.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct PanePlan {
    pub(super) add: Vec<String>,
    /// One entry PER PANE to close: labels gone from the roster (each
    /// occurrence) plus the excess copies of duplicated in-roster
    /// labels (a two-TUI race can double-open; each `remove` closes
    /// one matching pane).
    pub(super) remove: Vec<String>,
    /// `(new_master, live_titled_master_if_any)` whenever the pane
    /// TITLED master is not the roster master — including when nothing
    /// is (a staged team replacement) and when the old one is leaving
    /// the roster (it stays the relocation source; its removal runs
    /// after the layout).
    pub(super) relocate: Option<(String, Option<String>)>,
}

impl PanePlan {
    fn is_converged(&self) -> bool {
        self.add.is_empty() && self.remove.is_empty() && self.relocate.is_none()
    }
}

pub(super) fn plan_panes(cur: &RosterView, live: &[(String, bool)]) -> PanePlan {
    // Desired state, modeled explicitly: EXACTLY one pane per roster
    // label, and the pane titled master is exactly the roster master.
    let mut count: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for (l, _) in live {
        *count.entry(l.as_str()).or_default() += 1;
    }
    let add = cur
        .labels
        .iter()
        .filter(|l| !count.contains_key(l.as_str()))
        .cloned()
        .collect();
    let mut remove = Vec::new();
    for (label, n) in &count {
        let excess = if cur.labels.contains(*label) {
            n - 1
        } else {
            *n
        };
        remove.extend(std::iter::repeat_n(label.to_string(), excess));
    }
    // The stage invariant: EXACTLY one master-titled pane, and it is
    // the roster master. Any stale titled pane (even alongside a
    // correctly-titled master — a partial rename can leave two) forces
    // a relocation that stages the roster master and demotes the first
    // stale claimant; a titled pane whose label is LEAVING the roster
    // still sources the demotion (its pane lives until the removes,
    // which run after the layout). A roster master that is missing,
    // reviewer-titled, or unopened is staged with no demotion source.
    let titled: Vec<&str> = live
        .iter()
        .filter(|(_, titled)| *titled)
        .map(|(l, _)| l.as_str())
        .collect();
    let relocate = match cur.master.as_deref() {
        None => None,
        Some(new) => {
            let stale = titled.iter().find(|l| **l != new).map(|l| l.to_string());
            if stale.is_none() && titled.contains(&new) {
                None
            } else {
                Some((new.to_string(), stale))
            }
        }
    };
    PanePlan {
        add,
        remove,
        relocate,
    }
}

/// The zellij side effects [`PaneReconciler`] drives, injected so the
/// convergence logic is testable without spawning zellij
/// (no-binary-spawning-tests). One pass takes ONE snapshot (the
/// measured-slow `--json` listing) and threads it through every op;
/// verification re-reads that same listing — the only source that
/// identifies a STARTED agent (zellij-one-listing-per-pass, revised
/// by zellij-pane-placement-and-cost). `Snap` is opaque to
/// the reconciler: the real impl holds raw panes, tests hold pairs.
pub(super) trait PaneIo {
    type Snap;
    fn snapshot(&mut self) -> Option<Self::Snap>;
    fn pairs(&mut self, snap: &Self::Snap) -> Vec<(String, bool)>;
    /// Post-action ground truth for convergence: the label/role pairs
    /// AND whether the reviewers are correctly placed, from ONE read.
    /// Both, because a pass that only removed or relocated still has
    /// to answer for placement — checking labels alone lets an
    /// unstacked tab be cached (codex on d5121e1) — and splitting
    /// them would cost a second session-wide listing.
    fn verify(&mut self, reviewers: &[String]) -> Option<(Vec<(String, bool)>, bool)>;
    /// The pane to restore focus to after the pass. Takes the pass's
    /// listing so the target costs no session-wide query of its own.
    fn capture_focus(&mut self, snap: &Self::Snap) -> Option<String>;
    fn restore_focus(&mut self, id: &str);
    /// Create the pane; reports the id when one was made, plus any
    /// reviewer pane found by TITLE that anchored it. The caller
    /// accumulates both and stacks the whole set once via
    /// [`Self::stack`] — a title-found anchor has no launch command to
    /// be named by, so `stack` cannot rediscover it.
    fn add(
        &mut self,
        label: &str,
        other_reviewers: &[String],
        departing: &[String],
        snap: &Self::Snap,
    ) -> crate::cli::open_zellij::ReviewerPaneAdd;
    /// Put this repo's reviewer panes, plus `extra_ids` created this
    /// pass, into one stack. Returns whether they ARE stacked
    /// afterwards — read back, not assumed.
    fn stack(&mut self, reviewers: &[String], snap: &Self::Snap, extra_ids: &[String]) -> bool;
    /// Whether the reviewers are ALREADY stacked in `snap`. Presence
    /// of every label does not imply correct placement, so the
    /// converged path consults this before caching.
    fn is_placed(&mut self, reviewers: &[String], snap: &Self::Snap) -> bool;
    fn relocate(
        &mut self,
        new_master: &str,
        old_master: Option<&str>,
        roster: &[String],
        snap: &Self::Snap,
    );
    fn remove_all(&mut self, labels: &[String], snap: &Self::Snap);
    /// Whether THIS process may drive pane reconciliation for the repo.
    ///
    /// Exactly one may, at a time — the same shape as the ingest lease
    /// (wal-single-ingest-writer). Two TUIs on one repo is a supported
    /// and OBSERVED state (the `full-app-sim-driver` worktree had two
    /// live `clank status --tui` processes), and the existing comment
    /// on cache invalidation already anticipates one converging the
    /// other's layout.
    ///
    /// What it does not survive is both ACTING. A worker serializes
    /// passes only within its own process, so two drivers doing
    /// list-then-create is a lock-free TOCTOU: both see a label
    /// missing, both create, and the tool refuses the second session
    /// with `already has an active writer`, leaving a corpse that the
    /// listing still counts as that label's pane.
    ///
    /// Scoped to RECONCILE: structural pane work (create, stack,
    /// relocate, close) and convergence caching. The RETITLE pass is
    /// deliberately outside the lease — pane titles are SESSION-local,
    /// so a loser in a different zellij session never receives the
    /// holder's stamps and must apply its own; within one session the
    /// two stamp identical titles from identical inputs.
    fn may_reconcile(&mut self) -> bool;
}

/// The real zellij-backed [`PaneIo`].
struct ZellijPaneIo<'a> {
    repo: &'a std::path::Path,
    /// Held for as long as this process drives the repo. Re-attempted
    /// while absent, so closing the holding TUI hands reconciliation
    /// to a surviving one rather than stranding it.
    lease: Option<ReconcileLease>,
}

/// flock RAII over `<repo>/.clank/zellij-reconcile.lock`.
///
/// Exclusive and NON-blocking: a second driver must degrade, not
/// queue — queuing would apply a pass computed against a roster the
/// holder has already changed.
///
/// flock dies with the process, so a killed TUI frees the lease with
/// no cleanup protocol. Rust opens files `O_CLOEXEC`, so the zellij
/// subprocesses this drives cannot carry the lease past their exec.
struct ReconcileLease {
    _file: std::fs::File,
}

impl ReconcileLease {
    fn acquire(repo: &std::path::Path) -> Option<Self> {
        use std::os::fd::AsRawFd;
        let dir = repo.join(".clank");
        std::fs::create_dir_all(&dir).ok()?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("zellij-reconcile.lock"))
            .ok()?;
        // SAFETY: valid owned fd; flock has no memory effects.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        (rc == 0).then_some(Self { _file: file })
    }
}

impl PaneIo for ZellijPaneIo<'_> {
    type Snap = Vec<crate::cli::open_zellij::ZellijPane>;
    fn snapshot(&mut self) -> Option<Self::Snap> {
        crate::cli::open_zellij::snapshot_panes()
    }
    fn pairs(&mut self, snap: &Self::Snap) -> Vec<(String, bool)> {
        crate::cli::open_zellij::agent_pane_pairs(snap, self.repo)
    }
    fn verify(&mut self, reviewers: &[String]) -> Option<(Vec<(String, bool)>, bool)> {
        // A FRESH listing, not `dump-layout`. The dump was the cheap
        // source (1.0s vs 5.6s on a 28-pane session) but it cannot
        // identify an agent that has STARTED: it reports the running
        // program, which the exec turns into `opencode`, `uv`,
        // `caffeinate`, a vendored codex path, even a child like
        // `rust-analyzer`. The listing keeps the CONFIGURED command,
        // so `clank agent start <label> --repo <repo>` is still there
        // — provenance and label in one string that the exec cannot
        // touch. Identifying by pane TITLE instead was tried and
        // rejected: the title carries no ownership marker, so any
        // pane in the repo named `x (reviewer)` would pass, and it
        // cannot round-trip the legal label domain (`AgentLabel`
        // permits emoji and spaces), which fails OPEN — a wrong label
        // that never converges — where this fails closed
        // (zellij-pane-placement-and-cost).
        //
        // Cost is bounded by WHEN this runs: a converged pass returns
        // on its first listing and never reaches here, so the second
        // read is paid only by passes that actually changed something.
        let snap = crate::cli::open_zellij::snapshot_panes()?;
        Some((
            crate::cli::open_zellij::agent_pane_pairs(&snap, self.repo),
            crate::cli::open_zellij::reviewers_are_stacked(&snap, self.repo, reviewers),
        ))
    }
    fn capture_focus(&mut self, snap: &Self::Snap) -> Option<String> {
        crate::cli::open_zellij::pass_focus_target(snap)
    }
    fn may_reconcile(&mut self) -> bool {
        if self.lease.is_none() {
            self.lease = ReconcileLease::acquire(self.repo);
        }
        self.lease.is_some()
    }
    fn restore_focus(&mut self, id: &str) {
        crate::cli::open_zellij::focus_pane(id);
    }
    fn add(
        &mut self,
        label: &str,
        other_reviewers: &[String],
        departing: &[String],
        snap: &Self::Snap,
    ) -> crate::cli::open_zellij::ReviewerPaneAdd {
        crate::cli::open_zellij::add_reviewer_pane(
            self.repo,
            label,
            other_reviewers,
            departing,
            snap,
        )
    }
    fn stack(&mut self, reviewers: &[String], snap: &Self::Snap, extra_ids: &[String]) -> bool {
        crate::cli::open_zellij::stack_reviewer_panes(self.repo, reviewers, snap, extra_ids)
    }
    fn is_placed(&mut self, reviewers: &[String], snap: &Self::Snap) -> bool {
        crate::cli::open_zellij::reviewers_are_stacked(snap, self.repo, reviewers)
    }
    fn relocate(
        &mut self,
        new_master: &str,
        old_master: Option<&str>,
        roster: &[String],
        snap: &Self::Snap,
    ) {
        crate::cli::open_zellij::relocate_for_promote(
            self.repo, new_master, old_master, roster, snap,
        );
    }
    fn remove_all(&mut self, labels: &[String], snap: &Self::Snap) {
        crate::cli::open_zellij::remove_reviewer_panes(self.repo, labels, snap);
    }
}

impl PaneReconciler {
    pub(super) fn new() -> Self {
        Self {
            converged: None,
            failed_repairs: 0,
            failing: None,
            pending: std::collections::BTreeMap::new(),
        }
    }

    /// Fold creations this reconciler has not yet seen listed into the
    /// live set, so a pass whose snapshot has not caught up does not
    /// open a SECOND pane for a label the previous pass just opened.
    ///
    /// Keyed by LABEL, not by the created id: `pairs` reports labels,
    /// so a label is what a listing can confirm. An entry is dropped
    /// the moment a listing shows it, and ages out otherwise — see
    /// [`PENDING_CREATE_PASSES`] for why it must.
    ///
    /// Believed panes are reported NOT master-titled: the relocation
    /// that stamps the title runs after the adds, so `false` is what a
    /// listing would say about a pane created this pass.
    fn believe_pending(&mut self, live: &[(String, bool)]) -> Vec<(String, bool)> {
        self.pending.retain(|label, left| {
            *left = left.saturating_sub(1);
            !live.iter().any(|(l, _)| l == label) && *left > 0
        });
        let mut out = live.to_vec();
        out.extend(self.pending.keys().map(|l| (l.clone(), false)));
        out
    }

    /// One reconciliation pass for `cur`, skipped when that exact view
    /// was already VERIFIED converged. Cheap in the steady state: one
    /// set comparison, no zellij calls. Runs on the [`ReconcileWorker`]
    /// thread, never the TUI loop (tui-reconcile-off-loop). Returns
    /// whether the pass issued actions.
    /// Record a failing pass for `cur`; `true` once the budget is
    /// spent and the tab should be left alone. Counted PER ROSTER: a
    /// roster change both invalidates the count and can make the
    /// action possible again.
    fn note_failure(&mut self, cur: &RosterView) -> bool {
        if self.failing.as_ref() == Some(cur) {
            self.failed_repairs += 1;
        } else {
            self.failing = Some(cur.clone());
            self.failed_repairs = 1;
        }
        self.failed_repairs >= MAX_FAILED_PASSES
    }

    fn note_success(&mut self) {
        self.failed_repairs = 0;
        self.failing = None;
    }

    fn reconcile(&mut self, cur: RosterView, io: &mut impl PaneIo) -> bool {
        if self.converged.as_ref() == Some(&cur) {
            return false;
        }
        // Not this process's to reconcile. Nothing is cached as
        // converged: the holder's work is not ours to claim, and if
        // the lease frees we must reconcile from whatever state it
        // left. Retitles continue — see [`PaneIo::may_reconcile`].
        if !io.may_reconcile() {
            return false;
        }
        // Listing failure → touch nothing AND stay unconverged, so the
        // next refresh retries (acting on a partial listing would
        // re-open every pane; forgetting the event would drop it).
        let Some(mut snap) = io.snapshot() else {
            return false;
        };
        let plan = plan_panes(&cur, &self.believe_pending(&io.pairs(&snap)));
        let reviewers: Vec<String> = cur
            .labels
            .iter()
            .filter(|l| Some(*l) != cur.master.as_ref())
            .cloned()
            .collect();
        if plan.is_converged() {
            // Every label is present — but presence is not PLACEMENT.
            // A tab whose reviewers are not stacked (a failed or
            // silently no-op `stack-panes` last pass, or a tab broken
            // before this code existed) looks converged by label and
            // would never be repaired (codex on 3d3ffce). Repair here,
            // and only cache convergence once placement is confirmed.
            if io.is_placed(&reviewers, &snap) {
                self.converged = Some(cur);
                return false;
            }
            let focus = io.capture_focus(&snap);
            let placed = io.stack(&reviewers, &snap, &[]);
            if let Some(id) = &focus {
                io.restore_focus(id);
            }
            // `stack-panes` can only BUILD a stack, never take a pane
            // out of one — measured: move-pane does nothing to a stack
            // member, break-pane does not exist, and stacking with a
            // fresh pane merges INTO the existing stack. So a lone
            // reviewer sharing the instrument pane's stack cannot be
            // repaired, and re-attempting it every refresh would
            // reinstate exactly the unbounded work this plan removes.
            //
            // Cache only what we KNOW is beyond us: with fewer than
            // two reviewers there is no action left to try, and the
            // roster changing (a second reviewer arriving) both
            // invalidates this and makes repair possible again.
            // Anything else stays unconverged and retries.
            if placed {
                self.note_success();
                self.converged = Some(cur);
                return true;
            }
            // Give up on what we cannot fix, rather than acting on
            // every refresh forever. `< 2` keeps its original meaning:
            // with one reviewer there is no action left to try at all.
            let spent = self.note_failure(&cur);
            if reviewers.len() < 2 || spent {
                self.converged = Some(cur);
            }
            return true;
        }
        // Pass-level focus transaction: capture the user's focus once
        // before the first action, restore once after the last
        // (zellij-one-listing-per-pass).
        let focus = io.capture_focus(&snap);
        // Adds before the relocate (a swapped-in master may be brand
        // new), removes last.
        // Adds do not stack; the whole desired set is stacked ONCE
        // below. Stacking per-add off the pass-start snapshot skipped
        // it entirely whenever two reviewers arrived together — each
        // call saw no existing reviewer and a one-element set (codex
        // on 2d273c6). Ids created this pass are accumulated because
        // that snapshot cannot name them.
        // Tracked BY LABEL: `plan.add` can include a brand-new MASTER
        // (a swap-in), and feeding that id to the reviewer stack would
        // sweep the master into it — the same class of bug as the
        // instrument pane being captured (codex on 3d3ffce).
        let mut created: Vec<(String, String)> = Vec::new();
        // Anchors are NOT label-keyed: they are existing panes, either
        // matched on a `(reviewer)` title (so a master pane can never
        // be one) or belonging to a reviewer leaving this pass.
        //
        // `plan.remove` is passed so a replacement can anchor on the
        // pane being vacated. Nothing here knows or needs to know that
        // a swap happened: any pass that both adds and removes gets
        // the same preservation, which is why `agent swap` needs no
        // signal of its own.
        let mut anchors: Vec<String> = Vec::new();
        for label in &plan.add {
            let added = io.add(label, &reviewers, &plan.remove, &snap);
            if let Some(id) = added.created {
                // Believed live until a listing confirms it, so the
                // next pass cannot open this label a second time.
                self.pending.insert(label.clone(), PENDING_CREATE_PASSES);
                created.push((label.clone(), id));
            }
            if let Some(id) = added.anchor {
                anchors.push(id);
            }
        }
        let mut created_reviewers: Vec<String> = created
            .iter()
            .filter(|(label, _)| reviewers.iter().any(|r| r == label))
            .map(|(_, id)| id.clone())
            .collect();
        // A title-found anchor has no launch command to be named by, so
        // `stack` cannot rediscover it from the snapshot — without this
        // the id list falls short of the two entries `stack-panes`
        // needs and the call is skipped entirely.
        //
        // Known and deliberate: the verifying read
        // (`reviewers_are_stacked`) still identifies members by exact
        // command, so it sees the newly stacked pane as a LONE reviewer
        // and reports converged as long as it is not stacked with the
        // instrument pane. Do not "fix" that blind spot by teaching the
        // read-back to match titles — that is classification, which
        // fails open on a label the glyph-strip cannot round-trip, and
        // an unconverged read here means repairing every refresh
        // forever (placement-reads-zellij-stacks-correctly).
        for id in anchors {
            if !created_reviewers.contains(&id) {
                created_reviewers.push(id);
            }
        }
        // Placement is part of the outcome, not a side effect. The
        // verifying read below decides it for EVERY pass — including
        // remove-only and relocate-only ones, which can leave a stack
        // wrong without adding anything.
        if !plan.add.is_empty() {
            io.stack(&reviewers, &snap, &created_reviewers);
        }
        if let Some((new_master, old_master)) = &plan.relocate {
            // Adds the relocation depends on aren't in the pass-start
            // snapshot (new-pane reports no id; compose skips on an
            // unclassified/absent master) — refresh ONCE after adds
            // (codex 631636e). Refresh failure: proceed with the stale
            // snapshot; compose may skip, verify catches it, the next
            // refresh retries.
            if !plan.add.is_empty()
                && let Some(fresh) = io.snapshot()
            {
                snap = fresh;
            }
            // The relocation's classification set is the union of the
            // desired roster and EVERY live agent label: all departing
            // panes (master or reviewer) are still live here — removes
            // run after the layout — and compose skips on any
            // unclassified live agent pane (codex ae6338a, 8c4906d).
            let mut all: std::collections::BTreeSet<String> = cur.labels.clone();
            all.extend(io.pairs(&snap).into_iter().map(|(l, _)| l));
            let all: Vec<String> = all.into_iter().collect();
            io.relocate(new_master, old_master.as_deref(), &all, &snap);
        }
        if !plan.remove.is_empty() {
            io.remove_all(&plan.remove, &snap);
        }
        if let Some(id) = &focus {
            io.restore_focus(id);
        }
        // Converged only when a VERIFYING read confirms the target
        // state — every action above is best-effort, so observation is
        // not achievement. The verify source is a FRESH listing —
        // see `verify_pairs` for why the cheaper dump cannot do this
        // job.
        if io
            .verify(&reviewers)
            .is_some_and(|(after, placed)| placed && plan_panes(&cur, &after).is_converged())
        {
            self.note_success();
            self.converged = Some(cur);
        } else if self.note_failure(&cur) {
            // An add / remove / relocate that never verifies acts on
            // the tab every refresh exactly like a failing restack did.
            // The budget covers it, or the acceptance criterion ("no
            // reachable state issues actions indefinitely") is false
            // for three of the four action kinds (codex on dfeff8c).
            self.converged = Some(cur);
        }
        true
    }
}

/// A message to the reconciliation worker. Both kinds coalesce
/// independently (latest of each per batch): a roster view triggers a
/// reconcile pass, glyph data a retitle pass.
pub(super) enum WorkerMsg {
    Roster(RosterView),
    Glyphs(StatusGlyphs),
}

/// The worker's per-batch state: the reconciler AND the retitler,
/// co-owned so pane titles have exactly ONE writer and a retitle can
/// never race a relocation's role stamps (codex ff9579f). A reconcile
/// pass that issued actions invalidates the retitler's cached
/// (pane, label, role) rows, so the following retitle re-lists fresh
/// titles instead of stamping stale roles back.
struct WorkerState {
    reconciler: PaneReconciler,
    panes: PaneStatus,
    /// The last roster view handled — cached (pane, label, role) rows
    /// are only trustworthy while the roster TARGET is unchanged.
    last_roster: Option<RosterView>,
}

impl WorkerState {
    fn new() -> Self {
        Self {
            reconciler: PaneReconciler::new(),
            panes: PaneStatus::new(),
            last_roster: None,
        }
    }

    fn handle(
        &mut self,
        roster: Option<RosterView>,
        glyphs: Option<StatusGlyphs>,
        io: &mut impl PaneIo,
        list_panes: impl FnMut() -> Option<String>,
        rename: impl FnMut(&str, &str),
    ) {
        if let Some(view) = roster {
            // Invalidate on any TARGET change, not just on our own
            // actions: another TUI may already have converged the live
            // layout (reconcile no-ops), yet our cached rows still
            // carry the previous roster's roles (codex afb6d43).
            let changed = self.last_roster.as_ref() != Some(&view);
            self.last_roster = Some(view.clone());
            let acted = self.reconciler.reconcile(view, io);
            if acted || changed {
                self.panes.invalidate();
            }
        }
        if let Some(g) = glyphs {
            self.panes.update_with(&g, list_panes, rename);
        }
    }
}

/// The off-loop zellij worker (tui-reconcile-off-loop): owns EVERY
/// zellij subprocess — reconciliation and retitles; the TUI loop only
/// sends. One worker serializes passes by construction. Shutdown is
/// deterministic: dropping the worker closes the channel and JOINS —
/// declare it BEFORE the alt-screen guard so reverse drop order joins
/// AFTER the terminal is restored on every exit path, and an
/// in-flight pass (including its focus restore) always completes. On
/// disconnect the worker drains what's queued into at most one final
/// batch, then exits.
pub(super) struct ReconcileWorker {
    tx: Option<std::sync::mpsc::Sender<WorkerMsg>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl ReconcileWorker {
    /// Spawns the worker — a no-op handle outside zellij (no thread,
    /// sends go nowhere).
    pub(super) fn spawn(repo: std::path::PathBuf) -> Self {
        if std::env::var_os("ZELLIJ").is_none() {
            return Self {
                tx: None,
                join: None,
            };
        }
        let (tx, rx) = std::sync::mpsc::channel::<WorkerMsg>();
        let join = std::thread::spawn(move || {
            let mut state = WorkerState::new();
            let mut io = ZellijPaneIo {
                repo: &repo,
                lease: None,
            };
            worker_loop(&rx, |roster, glyphs| {
                state.handle(roster, glyphs, &mut io, list_panes, rename_pane);
            });
        });
        Self {
            tx: Some(tx),
            join: Some(join),
        }
    }

    /// Hand the worker a fresh snapshot's roster view. Non-blocking:
    /// microseconds, no subprocesses — safe on the TUI loop.
    pub(super) fn observe(&self, snap: &StatusSnapshot) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(WorkerMsg::Roster(RosterView::of(snap)));
        }
    }

    /// Hand the worker this refresh's retitle data (derived on the
    /// loop, pure). Non-blocking.
    pub(super) fn update_glyphs(&self, snap: &StatusSnapshot) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(WorkerMsg::Glyphs(StatusGlyphs::of(snap)));
        }
    }
}

impl Drop for ReconcileWorker {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// The worker's receive loop, generic over the batch handler so the
/// coalescing contract is testable without zellij
/// (tui-reconcile-off-loop): messages QUEUED before a batch starts
/// collapse to the newest OF EACH KIND; arrivals DURING a batch
/// collapse into exactly one follow-up batch. Exits when the channel
/// disconnects (after at most one final drained batch).
fn worker_loop(
    rx: &std::sync::mpsc::Receiver<WorkerMsg>,
    mut batch: impl FnMut(Option<RosterView>, Option<StatusGlyphs>),
) {
    while let Ok(first) = rx.recv() {
        let mut roster = None;
        let mut glyphs = None;
        match first {
            WorkerMsg::Roster(v) => roster = Some(v),
            WorkerMsg::Glyphs(g) => glyphs = Some(g),
        }
        while let Ok(m) = rx.try_recv() {
            match m {
                WorkerMsg::Roster(v) => roster = Some(v),
                WorkerMsg::Glyphs(g) => glyphs = Some(g),
            }
        }
        batch(roster, glyphs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::status_tui::fixtures::{plan_state, reviewer_missing, snap};
    use clank_core::plan_view::WaitingOn;

    // ── tui-zellij-pane-reconcile: planning + convergence ──────

    fn view(labels: &[&str], master: Option<&str>) -> RosterView {
        RosterView {
            labels: labels.iter().map(|s| s.to_string()).collect(),
            master: master.map(str::to_string),
        }
    }

    fn live(pairs: &[(&str, bool)]) -> Vec<(String, bool)> {
        pairs.iter().map(|(l, m)| (l.to_string(), *m)).collect()
    }

    #[test]
    fn plan_adds_missing_members_and_removes_stale_panes() {
        // Live panes lag the roster in both directions: `new` has no
        // pane yet, `gone` has a pane but left the roster.
        let plan = plan_panes(
            &view(&["claude", "codex", "new"], Some("claude")),
            &live(&[("claude", true), ("codex", false), ("gone", false)]),
        );
        assert_eq!(plan.add, vec!["new".to_string()]);
        assert_eq!(plan.remove, vec!["gone".to_string()]);
        assert_eq!(plan.relocate, None);
        assert!(!plan.is_converged());
    }

    #[test]
    fn plan_converged_state_is_a_no_op() {
        let plan = plan_panes(
            &view(&["claude", "codex"], Some("claude")),
            &live(&[("codex", false), ("claude", true)]),
        );
        assert!(plan.is_converged());
    }

    #[test]
    fn plan_detects_a_master_swap_from_live_titles_alone() {
        // codex a730882 concern 2: promote ran while NO TUI was open —
        // the roster says codex, the stage title still says claude. A
        // fresh reconciler must infer the relocation from the titles.
        let plan = plan_panes(
            &view(&["claude", "codex"], Some("codex")),
            &live(&[("claude", true), ("codex", false)]),
        );
        assert_eq!(plan.add, Vec::<String>::new());
        assert_eq!(plan.remove, Vec::<String>::new());
        assert_eq!(
            plan.relocate,
            Some(("codex".to_string(), Some("claude".to_string())))
        );
    }

    #[test]
    fn plan_departing_old_master_is_still_the_relocation_source() {
        // codex c7be87f: the stale stage owner is LEAVING the roster —
        // it must still source the relocation (its pane is alive until
        // the removes run, which come after the layout), or the new
        // master converges reviewer-titled in the stack.
        let plan = plan_panes(
            &view(&["codex"], Some("codex")),
            &live(&[("claude", true), ("codex", false)]),
        );
        assert_eq!(plan.remove, vec!["claude".to_string()]);
        assert_eq!(
            plan.relocate,
            Some(("codex".to_string(), Some("claude".to_string())))
        );
    }

    #[test]
    fn plan_stages_the_master_when_nothing_is_master_titled() {
        // codex c7be87f: all panes reviewer-titled (e.g. a whole-team
        // replacement mid-convergence) — the roster master must still
        // be staged, with no demotion source.
        let plan = plan_panes(
            &view(&["claude", "codex"], Some("claude")),
            &live(&[("claude", false), ("codex", false)]),
        );
        assert_eq!(plan.relocate, Some(("claude".to_string(), None)));
        assert!(!plan.is_converged());
    }

    #[test]
    fn plan_demotes_a_stale_master_title_even_when_the_master_is_titled() {
        // codex ae6338a: a partial rename can leave TWO master-titled
        // panes. Whichever order they list in, the stale claimant is
        // demoted — and until then the layout must not verify.
        for lv in [
            live(&[("codex", true), ("claude", true)]),
            live(&[("claude", true), ("codex", true)]),
        ] {
            let plan = plan_panes(&view(&["claude", "codex"], Some("codex")), &lv);
            assert_eq!(
                plan.relocate,
                Some(("codex".to_string(), Some("claude".to_string()))),
                "stale title demoted regardless of listing order"
            );
            assert!(!plan.is_converged());
        }
    }

    #[test]
    fn plan_removes_excess_duplicate_panes_of_an_in_roster_label() {
        // codex c7be87f: a two-TUI race can double-open a member's
        // pane; multiplicity is part of the desired state (exactly one
        // pane per label), so the excess copy is closed and a
        // duplicated layout never verifies as converged.
        let plan = plan_panes(
            &view(&["claude", "codex"], Some("claude")),
            &live(&[("claude", true), ("codex", false), ("codex", false)]),
        );
        assert_eq!(plan.add, Vec::<String>::new());
        assert_eq!(plan.remove, vec!["codex".to_string()]);
        assert_eq!(plan.relocate, None);
        assert!(!plan.is_converged());
    }

    /// Scripted [`PaneIo`]: queues of `snapshot()`/`verify_pairs()`
    /// answers, an action log, and call counters for the
    /// listings-per-pass acceptance (zellij-one-listing-per-pass).
    struct FakeIo {
        snaps: std::collections::VecDeque<Option<Vec<(String, bool)>>>,
        verifies: std::collections::VecDeque<Option<Vec<(String, bool)>>>,
        log: Vec<String>,
        snapshots_taken: usize,
        verifies_taken: usize,
        focus_captures: usize,
        /// Scripted per-add creation results (default: created).
        add_results: std::collections::VecDeque<bool>,
        add_anchors: std::collections::VecDeque<Option<String>>,
        add_departing: Vec<Vec<String>>,
        /// Scripted lease answer; the real IO holds an flock.
        may_reconcile: bool,
        stack_results: std::collections::VecDeque<bool>,
        placed_results: std::collections::VecDeque<bool>,
        verify_placed: std::collections::VecDeque<bool>,
    }

    impl FakeIo {
        fn new(snaps: Vec<Option<Vec<(String, bool)>>>) -> Self {
            Self::with_verify(snaps, vec![])
        }

        fn with_verify(
            snaps: Vec<Option<Vec<(String, bool)>>>,
            verifies: Vec<Option<Vec<(String, bool)>>>,
        ) -> Self {
            Self {
                snaps: snaps.into(),
                verifies: verifies.into(),
                log: Vec::new(),
                snapshots_taken: 0,
                verifies_taken: 0,
                focus_captures: 0,
                add_results: std::collections::VecDeque::new(),
                add_anchors: std::collections::VecDeque::new(),
                add_departing: Vec::new(),
                may_reconcile: true,
                stack_results: std::collections::VecDeque::new(),
                placed_results: std::collections::VecDeque::new(),
                verify_placed: std::collections::VecDeque::new(),
            }
        }
    }

    impl PaneIo for FakeIo {
        type Snap = Vec<(String, bool)>;
        fn snapshot(&mut self) -> Option<Self::Snap> {
            self.snapshots_taken += 1;
            self.snaps.pop_front().unwrap_or(None)
        }
        fn pairs(&mut self, snap: &Self::Snap) -> Vec<(String, bool)> {
            snap.clone()
        }
        fn verify(&mut self, _reviewers: &[String]) -> Option<(Vec<(String, bool)>, bool)> {
            self.verifies_taken += 1;
            let placed = self.verify_placed.pop_front().unwrap_or(true);
            self.verifies
                .pop_front()
                .unwrap_or(None)
                .map(|p| (p, placed))
        }
        fn may_reconcile(&mut self) -> bool {
            self.may_reconcile
        }
        fn capture_focus(&mut self, _snap: &Self::Snap) -> Option<String> {
            self.focus_captures += 1;
            Some("user_pane".to_string())
        }
        fn restore_focus(&mut self, id: &str) {
            self.log.push(format!("focus {id}"));
        }
        fn add(
            &mut self,
            label: &str,
            _other: &[String],
            departing: &[String],
            _snap: &Self::Snap,
        ) -> crate::cli::open_zellij::ReviewerPaneAdd {
            self.log.push(format!("add {label}"));
            self.add_departing.push(departing.to_vec());
            crate::cli::open_zellij::ReviewerPaneAdd {
                created: self
                    .add_results
                    .pop_front()
                    .unwrap_or(true)
                    .then(|| format!("terminal_{label}")),
                anchor: self.add_anchors.pop_front().flatten(),
            }
        }
        fn stack(&mut self, _revs: &[String], _snap: &Self::Snap, extra: &[String]) -> bool {
            self.log.push(format!("stack [{}]", extra.join(",")));
            self.stack_results.pop_front().unwrap_or(true)
        }
        fn is_placed(&mut self, _revs: &[String], _snap: &Self::Snap) -> bool {
            self.placed_results.pop_front().unwrap_or(true)
        }
        fn relocate(
            &mut self,
            new_master: &str,
            old_master: Option<&str>,
            _roster: &[String],
            _snap: &Self::Snap,
        ) {
            self.log
                .push(format!("relocate {new_master}<-{old_master:?}"));
        }
        fn remove_all(&mut self, labels: &[String], _snap: &Self::Snap) {
            self.log.push(format!("remove {}", labels.join("+")));
        }
    }

    fn roster_snap(labels: &[(&str, bool)]) -> StatusSnapshot {
        // Reuse the shared snapshot fixture; only labels + master matter
        // to RosterView.
        let mut s = snap(vec![], vec![]);
        s.agents = labels
            .iter()
            .map(|(l, _)| crate::cli::status::AgentAutoRow {
                label: l.to_string(),
                role: if labels.iter().any(|(m, is)| m == l && *is) {
                    crate::cli::teams_config::RosterRole::Master
                } else {
                    crate::cli::teams_config::RosterRole::Commit
                },
                auto_mode: clank_core::vocab::AutoMode::On,
                tool: "claude".into(),
                invocation: "claude".into(),
                session: None,
            })
            .collect();
        s.master = labels
            .iter()
            .find(|(_, is)| *is)
            .map(|(l, _)| l.to_string());
        s
    }

    #[test]
    fn reconciler_retries_after_a_failed_listing_and_verifies_convergence() {
        // codex a730882 concern 1: a transient listing failure must NOT
        // count as converged — the SAME unchanged roster retries on the
        // next observe. And success is only recorded after a verifying
        // re-list shows the target state.
        let snap = roster_snap(&[("claude", true), ("codex", false)]);
        let mut r = PaneReconciler::new();

        // First observe: listing fails → nothing done, not converged.
        let mut io = FakeIo::new(vec![None]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert!(io.log.is_empty());
        assert_eq!(r.converged, None, "failure must not converge");

        // Second observe, same roster: retries; the pass runs (codex
        // pane missing → add) and the verifying read confirms.
        let mut io = FakeIo::with_verify(
            vec![Some(live(&[("claude", true)]))],
            vec![Some(live(&[("claude", true), ("codex", false)]))],
        );
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(
            io.log,
            vec!["add codex", "stack [terminal_codex]", "focus user_pane"]
        );
        assert_eq!((io.snapshots_taken, io.verifies_taken), (1, 1));
        assert!(r.converged.is_some(), "verified pass converges");

        // Third observe, same roster: steady state, zero io.
        let mut io = FakeIo::new(vec![]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert!(io.log.is_empty());
    }

    #[test]
    fn reconciler_stays_unconverged_when_actions_did_not_stick() {
        // Best-effort actions can silently fail: the verify re-list
        // still shows the stale state → stay unconverged so the next
        // refresh retries (observation is not achievement).
        let snap = roster_snap(&[("claude", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let stale = live(&[("claude", true)]);
        let mut io = FakeIo::with_verify(vec![Some(stale.clone())], vec![Some(stale)]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(
            io.log,
            vec!["add codex", "stack [terminal_codex]", "focus user_pane"]
        );
        assert_eq!(r.converged, None);
    }

    #[test]
    fn reconciler_does_not_reopen_a_pane_whose_listing_has_not_caught_up() {
        // The observed bug: `new-pane` returns before the listing
        // reports the pane, so the next pass re-derived `add` from a
        // snapshot that still showed the label missing and opened a
        // SECOND pane for it (two `ruthless` panes, one of them never
        // retitled). What a pass created is believed until a listing
        // confirms it.
        let snap = roster_snap(&[("claude", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let stale = live(&[("claude", true)]);

        // Pass 1 opens codex. The verifying re-list lags too, so
        // nothing in this pass confirms the pane exists.
        let mut io = FakeIo::with_verify(vec![Some(stale.clone())], vec![Some(stale.clone())]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(io.log.iter().filter(|l| *l == "add codex").count(), 1);
        assert_eq!(r.converged, None, "unconfirmed pass must retry");

        // Pass 2's snapshot STILL does not show it. No second pane.
        let mut io = FakeIo::new(vec![Some(stale)]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert!(
            !io.log.iter().any(|l| l == "add codex"),
            "a believed pane must not be re-created: {:?}",
            io.log
        );
    }

    #[test]
    fn a_confirmed_creation_stops_being_believed() {
        // Discharge, direction one: the listing catches up, so the
        // memory releases the label and stops shadowing reality.
        let snap = roster_snap(&[("claude", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let stale = live(&[("claude", true)]);
        let mut io = FakeIo::with_verify(vec![Some(stale.clone())], vec![Some(stale)]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert!(r.pending.contains_key("codex"), "created pane is believed");

        let mut io = FakeIo::new(vec![Some(live(&[("claude", true), ("codex", false)]))]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert!(
            r.pending.is_empty(),
            "a listed pane must stop being believed: {:?}",
            r.pending
        );
    }

    #[test]
    fn a_creation_that_never_appears_stops_blocking_its_label() {
        // Discharge, direction two: the memory MUST expire. A
        // `new-pane` that reported an id which never reaches a listing
        // would otherwise block its label forever, and the agent would
        // never be opened again.
        //
        // Each pass grows the roster so none is skipped as converged;
        // the snapshot never catches up.
        let mut r = PaneReconciler::new();
        let stale = live(&[("claude", true)]);
        let mut adds = 0;
        for extra in [Vec::new(), vec!["ruthless"], vec!["kimi", "ruthless"]] {
            let mut labels = vec![("claude", true), ("codex", false)];
            labels.extend(extra.iter().map(|l| (*l, false)));
            let snap = roster_snap(&labels);
            let mut io = FakeIo::with_verify(vec![Some(stale.clone())], vec![Some(stale.clone())]);
            r.reconcile(RosterView::of(&snap), &mut io);
            adds += io.log.iter().filter(|l| *l == "add codex").count();
        }
        assert_eq!(
            adds, 2,
            "codex is retried once the belief expires, not stranded"
        );
    }

    #[test]
    fn a_duplicate_and_unstacked_tab_converges() {
        // The state the reported tab settled into: two panes for one
        // roster label, and the reviewers not stacked. The excess pane
        // is closed and the survivor joins the stack.
        let snap = roster_snap(&[("claude", true), ("codex", false), ("ruthless", false)]);
        let dup = live(&[
            ("claude", true),
            ("codex", false),
            ("ruthless", false),
            ("ruthless", false),
        ]);
        let clean = live(&[("claude", true), ("codex", false), ("ruthless", false)]);
        let mut r = PaneReconciler::new();

        // Pass 1 closes the excess pane; the tab is still unstacked, so
        // the verify refuses convergence.
        let mut io = FakeIo::with_verify(vec![Some(dup)], vec![Some(clean.clone())]);
        io.verify_placed.push_back(false);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert!(
            io.log.iter().any(|l| l == "remove ruthless"),
            "the excess pane is closed: {:?}",
            io.log
        );
        assert_eq!(r.converged, None, "unstacked must not cache as converged");

        // Pass 2 has nothing to add or remove and repairs placement.
        let mut io = FakeIo::new(vec![Some(clean)]);
        io.placed_results.push_back(false);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert!(
            io.log.iter().any(|l| l.starts_with("stack ")),
            "placement is repaired: {:?}",
            io.log
        );
        assert!(r.converged.is_some(), "repaired tab converges");
    }

    #[test]
    fn reconciler_orders_adds_relocate_removes() {
        // A swapped-in BRAND-NEW master while the old one leaves: open
        // the new pane first, stage it (the departing pane is still
        // alive to demote from), close the departed LAST — and only a
        // verify showing the new master actually TITLED master
        // converges.
        let snap = roster_snap(&[("new-master", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let before = live(&[("old", true), ("codex", false)]);
        let after_add = live(&[("old", true), ("codex", false), ("new-master", false)]);
        let after = live(&[("new-master", true), ("codex", false)]);
        // Adds feed the relocation → the pass refreshes the snapshot
        // once after the adds (codex 631636e): TWO snapshots, one
        // verifying read.
        let mut io = FakeIo::with_verify(vec![Some(before), Some(after_add)], vec![Some(after)]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!((io.snapshots_taken, io.verifies_taken), (2, 1));
        assert_eq!(
            io.log,
            vec![
                "add new-master",
                // The new MASTER's id is NOT offered to the reviewer
                // stack — feeding it in would sweep the master into
                // the reviewers, the same class of bug as capturing
                // the instrument pane.
                "stack []",
                "relocate new-master<-Some(\"old\")",
                "remove old",
                "focus user_pane"
            ]
        );
        assert!(r.converged.is_some());
    }

    #[test]
    fn reconciler_rejects_a_reviewer_titled_master_at_verify() {
        // codex c7be87f: if the relocation didn't stick, the new master
        // is present but reviewer-titled — verification must NOT bless
        // that layout as converged.
        let snap = roster_snap(&[("new-master", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let before = live(&[("old", true), ("codex", false)]);
        let after = live(&[("new-master", false), ("codex", false)]);
        let mut io =
            FakeIo::with_verify(vec![Some(before.clone()), Some(before)], vec![Some(after)]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(r.converged, None, "master in the stack is not converged");
    }

    #[test]
    fn promote_shaped_pass_takes_one_snapshot_one_verify_one_focus() {
        // zellij-one-listing-per-pass: all panes pre-exist — the pass
        // must cost exactly ONE --json snapshot, ONE verifying listing,
        // and ONE focus capture/restore transaction.
        let snap = roster_snap(&[("codex", true), ("claude", false)]);
        let mut r = PaneReconciler::new();
        let before = live(&[("claude", true), ("codex", false)]);
        let after = live(&[("claude", false), ("codex", true)]);
        let mut io = FakeIo::with_verify(vec![Some(before)], vec![Some(after)]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(
            (io.snapshots_taken, io.verifies_taken, io.focus_captures),
            (1, 1, 1)
        );
        assert_eq!(
            io.log,
            vec!["relocate codex<-Some(\"claude\")", "focus user_pane"]
        );
        assert!(r.converged.is_some());
    }

    #[test]
    fn converged_at_start_pass_takes_one_snapshot_and_nothing_else() {
        // The pass-start listing IS ground truth when the plan is
        // empty: record convergence with no verify, no focus, no ops.
        let snap = roster_snap(&[("claude", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::new(vec![Some(live(&[("claude", true), ("codex", false)]))]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(
            (io.snapshots_taken, io.verifies_taken, io.focus_captures),
            (1, 0, 0)
        );
        assert!(io.log.is_empty());
        assert!(r.converged.is_some());
    }

    #[test]
    fn two_adds_from_a_reviewerless_snapshot_still_get_stacked() {
        // The regression codex caught on 2d273c6: stacking per-add off
        // the PASS-START snapshot meant each call saw no existing
        // reviewer and a one-element set, so neither stacked and two
        // reviewers sat unstacked. The ids created this pass must
        // reach the stack even though the snapshot cannot name them.
        let snap = roster_snap(&[("claude", true), ("r1", false), ("r2", false)]);
        let mut r = PaneReconciler::new();
        // Live state has the MASTER only — no reviewer to anchor on.
        let before = live(&[("claude", true)]);
        let mut io = FakeIo::with_verify(vec![Some(before)], vec![None]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert!(
            io.log
                .contains(&"stack [terminal_r1,terminal_r2]".to_string()),
            "both new panes must be stacked together; got {:?}",
            io.log
        );
    }

    #[test]
    fn a_remove_only_pass_still_answers_for_placement() {
        // codex on d5121e1: a pass that only REMOVES (or only
        // relocates) adds nothing, so it used to skip the placement
        // question entirely and cache on label/title alone — leaving
        // an unstacked tab remembered as converged.
        let snap = roster_snap(&[("claude", true), ("r1", false)]);
        // Live has a stale extra reviewer → the plan is remove-only.
        let before = live(&[("claude", true), ("r1", false), ("gone", false)]);
        let after = live(&[("claude", true), ("r1", false)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(before)], vec![Some(after)]);
        io.verify_placed = vec![false].into();
        r.reconcile(RosterView::of(&snap), &mut io);
        assert!(
            io.log.iter().any(|l| l.starts_with("remove")),
            "sanity: this pass removes; got {:?}",
            io.log
        );
        assert!(
            r.converged.is_none(),
            "an unstacked result must not converge just because nothing was added"
        );
    }

    #[test]
    fn repair_is_bounded_when_placement_can_never_succeed() {
        // THE churn. Three of the four ways placement can fail cannot
        // be fixed by trying again — reviewers split across tabs, the
        // instrument pane already inside the stack (no `break-pane`
        // exists), and a `stack-panes` the server silently rejected.
        // Retrying them every refresh is what moved the user's focus
        // to a pane where nothing was happening, indefinitely.
        //
        // Asserted on the number of ACTIONS, not the verdict: the
        // symptom was the repeated work, so a fix that keeps answering
        // "not placed" is fine as long as it stops POKING the tab.
        let snap = roster_snap(&[("claude", true), ("r1", false), ("r2", false)]);
        let all_live = live(&[("claude", true), ("r1", false), ("r2", false)]);
        let passes = 8;
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(all_live.clone()); passes], vec![None; passes]);
        // Placement NEVER succeeds, however many times it is tried.
        io.placed_results = vec![false; passes * 2].into();
        io.stack_results = vec![false; passes * 2].into();

        for _ in 0..passes {
            r.reconcile(RosterView::of(&snap), &mut io);
        }

        let attempts = io.log.iter().filter(|l| l.starts_with("stack ")).count();
        assert!(
            attempts <= MAX_FAILED_PASSES as usize,
            "repair must be bounded: {attempts} attempts over {passes} passes; log {:?}",
            io.log
        );
        assert!(
            r.converged.is_some(),
            "after the bound the tab is left alone until `clank open` rebuilds it"
        );
    }

    #[test]
    fn a_failing_add_is_bounded_too_not_only_a_failing_restack() {
        // The budget originally lived only in the placement branch, so
        // an add / remove / relocate that never verifies kept acting
        // on the tab every refresh — the same churn by another door,
        // and it falsified the plan's own acceptance criterion (codex
        // on dfeff8c).
        let snap = roster_snap(&[("claude", true), ("r1", false), ("r2", false)]);
        // The listing never shows r2, so the pass always plans an add
        // and the verifying read never confirms.
        let missing = live(&[("claude", true), ("r1", false)]);
        let passes = 8;
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(missing.clone()); passes], vec![None; passes]);

        for _ in 0..passes {
            r.reconcile(RosterView::of(&snap), &mut io);
        }

        let adds = io.log.iter().filter(|l| l.starts_with("add ")).count();
        assert!(
            adds <= MAX_FAILED_PASSES as usize,
            "a never-verifying add must be bounded: {adds} over {passes} passes; log {:?}",
            io.log
        );
    }

    #[test]
    fn a_roster_change_reopens_repair_after_the_bound() {
        // The bound must not wedge a tab permanently: a roster change
        // both invalidates the count and can make repair possible
        // again (a member arriving or leaving changes the layout).
        let snap_a = roster_snap(&[("claude", true), ("r1", false), ("r2", false)]);
        let live_a = live(&[("claude", true), ("r1", false), ("r2", false)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(live_a); 6], vec![None; 6]);
        io.placed_results = vec![false; 12].into();
        io.stack_results = vec![false; 12].into();
        for _ in 0..4 {
            r.reconcile(RosterView::of(&snap_a), &mut io);
        }
        let before = io.log.iter().filter(|l| l.starts_with("stack ")).count();

        // A third reviewer joins: new roster, fresh budget.
        let snap_b = roster_snap(&[
            ("claude", true),
            ("r1", false),
            ("r2", false),
            ("r3", false),
        ]);
        let live_b = live(&[
            ("claude", true),
            ("r1", false),
            ("r2", false),
            ("r3", false),
        ]);
        let mut io2 = FakeIo::with_verify(vec![Some(live_b); 3], vec![None; 3]);
        io2.placed_results = vec![false; 6].into();
        io2.stack_results = vec![false; 6].into();
        r.reconcile(RosterView::of(&snap_b), &mut io2);
        assert!(
            io2.log.iter().any(|l| l.starts_with("stack ")),
            "a changed roster retries; before={before}, log {:?}",
            io2.log
        );
    }

    #[test]
    fn a_second_pass_repairs_placement_instead_of_caching_it() {
        // codex on 3d3ffce: after a failed stack the panes EXIST, so
        // the next pass sees every label and would take the
        // label-only converged return — the broken placement would
        // never be retried, and a tab broken before this code existed
        // would be accepted as fine. Presence is not placement.
        let snap = roster_snap(&[("claude", true), ("r1", false), ("r2", false)]);
        let all_live = live(&[("claude", true), ("r1", false), ("r2", false)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(all_live)], vec![None]);
        // Labels all present, but NOT stacked; the repair fails too.
        io.placed_results = vec![false].into();
        io.stack_results = vec![false].into();
        let acted = r.reconcile(RosterView::of(&snap), &mut io);

        assert!(acted, "an unplaced tab is work, not a no-op");
        assert!(
            io.log.iter().any(|l| l.starts_with("stack ")),
            "the second pass must attempt repair; got {:?}",
            io.log
        );
        assert!(
            r.converged.is_none(),
            "a failed repair must not be cached as converged"
        );
    }

    #[test]
    fn an_unrepairable_lone_reviewer_does_not_spin() {
        // One reviewer stacked with the instrument pane cannot be
        // separated by any action zellij offers, so retrying it on
        // every refresh would reinstate the unbounded work this plan
        // exists to remove. It is cached — but only because there is
        // nothing left to try, and only until the roster changes.
        let snap = roster_snap(&[("claude", true), ("r1", false)]);
        let all_live = live(&[("claude", true), ("r1", false)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(all_live)], vec![None]);
        io.placed_results = vec![false].into();
        io.stack_results = vec![false].into();
        r.reconcile(RosterView::of(&snap), &mut io);
        assert!(
            r.converged.is_some(),
            "with one reviewer there is no repair to retry; spinning helps nobody"
        );

        // TWO reviewers is repairable, so a failure must NOT be cached.
        let snap2 = roster_snap(&[("claude", true), ("r1", false), ("r2", false)]);
        let live2 = live(&[("claude", true), ("r1", false), ("r2", false)]);
        let mut r2 = PaneReconciler::new();
        let mut io2 = FakeIo::with_verify(vec![Some(live2)], vec![None]);
        io2.placed_results = vec![false].into();
        io2.stack_results = vec![false].into();
        r2.reconcile(RosterView::of(&snap2), &mut io2);
        assert!(
            r2.converged.is_none(),
            "a repairable failure must stay unconverged and retry"
        );
    }

    #[test]
    fn a_placed_tab_converges_without_acting() {
        // The other half: when the labels are all present AND the
        // reviewers are stacked, the pass must stay a no-op — the
        // repair path must not fire on every refresh.
        let snap = roster_snap(&[("claude", true), ("r1", false)]);
        let all_live = live(&[("claude", true), ("r1", false)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(all_live)], vec![None]);
        io.placed_results = vec![true].into();
        let acted = r.reconcile(RosterView::of(&snap), &mut io);
        assert!(!acted);
        assert!(
            io.log.is_empty(),
            "no actions on a placed tab: {:?}",
            io.log
        );
        assert!(r.converged.is_some());
    }

    #[test]
    fn unconfirmed_placement_keeps_the_pass_unconverged() {
        // `stack-panes` exits 0 on a stale id without doing anything,
        // so its status proves nothing and placement is READ BACK. If
        // the reviewers are not stacked afterwards, the pass must not
        // cache convergence — otherwise a silent no-op is remembered
        // as success and never retried (codex on 2d273c6).
        let snap = roster_snap(&[("claude", true), ("r1", false)]);
        let mut r = PaneReconciler::new();
        let before = live(&[("claude", true)]);
        // Verify would say converged; placement says otherwise.
        let after = live(&[("claude", true), ("r1", false)]);
        let mut io = FakeIo::with_verify(vec![Some(before)], vec![Some(after)]);
        // The VERIFYING read is what decides: `stack-panes` exits 0 on
        // a stale id without doing anything, so its status proves
        // nothing and is not consulted.
        io.verify_placed = vec![false].into();
        r.reconcile(RosterView::of(&snap), &mut io);
        assert!(
            r.converged.is_none(),
            "placement not confirmed → the pass must stay unconverged so it retries"
        );
    }

    #[test]
    fn a_driver_without_the_lease_still_retitles() {
        // NOT an oversight in the lease: pane titles are SESSION-local.
        // A loser driving a different zellij session never sees the
        // holder's stamps, so gating this would leave its own session
        // permanently unglyphed. Within one session both write the
        // same title from the same inputs.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut state = WorkerState::new();
        let mut io = FakeIo::new(vec![Some(live(&[("claude", true)]))]);
        io.may_reconcile = false;
        let lists = AtomicUsize::new(0);
        let mut renames: Vec<(String, String)> = Vec::new();
        state.handle(
            Some(view(&["claude"], Some("claude"))),
            Some(StatusGlyphs {
                emoji: [("claude".to_string(), ("M", "R"))].into_iter().collect(),
                wanted: vec!["claude".into()],
            }),
            &mut io,
            || {
                lists.fetch_add(1, Ordering::SeqCst);
                Some("terminal_1  terminal  claude (master)".to_string())
            },
            |id, title| renames.push((id.to_string(), title.to_string())),
        );
        assert!(io.log.is_empty(), "no structural pane work: {:?}", io.log);
        assert_eq!(
            renames,
            vec![("terminal_1".to_string(), "M claude (master)".to_string())],
            "the retitle pass runs without the lease"
        );
        assert_eq!(lists.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_reconcile_lease_admits_exactly_one_holder_and_frees_on_drop() {
        // flock binds to the OPEN FILE DESCRIPTION, so a second
        // acquire conflicts even from this process -- which is what
        // makes it exclusive across the two TUIs it exists to
        // separate.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let first = ReconcileLease::acquire(repo).expect("first driver takes it");
        assert!(
            ReconcileLease::acquire(repo).is_none(),
            "a second driver must be refused, not queued"
        );
        drop(first);
        assert!(
            ReconcileLease::acquire(repo).is_some(),
            "closing the holder must hand reconciliation over, not strand it"
        );
    }

    #[test]
    fn a_driver_without_the_lease_does_not_reconcile_and_caches_nothing() {
        // Two TUIs on one repo is supported and observed. Their
        // workers serialize only within a process, so both doing
        // STRUCTURAL pane work is a lock-free TOCTOU that double-opens
        // a pane -- and the second tool refuses the session, leaving a
        // corpse the listing still counts.
        //
        // Scoped to reconcile: retitles are session-local and stay
        // dual on purpose, pinned by
        // `a_driver_without_the_lease_still_retitles`.
        let snap = roster_snap(&[("claude", true), ("r1", false)]);
        let before = live(&[("claude", true)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(before)], vec![None]);
        io.may_reconcile = false;
        assert!(!r.reconcile(RosterView::of(&snap), &mut io));
        assert!(io.log.is_empty(), "no zellij work: {:?}", io.log);
        // Nothing cached: the holder's convergence is not ours to
        // claim, and we must act on whatever it leaves if it exits.
        assert!(r.converged.is_none());
    }

    #[test]
    fn a_replacement_is_offered_the_departing_pane_as_an_anchor() {
        // The swap shape: r1 leaves and r2 arrives in ONE pass. r2 has
        // no peer on the new roster, so the only pane that knows which
        // tab it belongs in is r1's — still live, because removes run
        // after the layout.
        let snap = roster_snap(&[("claude", true), ("r2", false)]);
        let before = live(&[("claude", true), ("r1", false)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(before)], vec![None]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(io.add_departing, vec![vec!["r1".to_string()]]);
        // And the departure is still applied, after the layout.
        let add = io.log.iter().position(|l| l == "add r2");
        let rm = io.log.iter().position(|l| l.starts_with("remove"));
        assert!(
            add < rm,
            "the arriving pane must be placed before the departing one closes: {:?}",
            io.log
        );
    }

    #[test]
    fn an_add_with_no_departure_is_offered_nothing() {
        let snap = roster_snap(&[("claude", true), ("r1", false)]);
        let before = live(&[("claude", true)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(before)], vec![None]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(io.add_departing, vec![Vec::<String>::new()]);
    }

    #[test]
    fn a_title_found_anchor_joins_the_stack_call() {
        // `stack` names members by exact launch command, so a pane
        // found by TITLE is invisible to it. Unless the add hands the
        // anchor id over, the list holds one entry, `stack-panes` gets
        // no pair to work with, and the pane it was placed against
        // never joins the stack.
        let snap = roster_snap(&[("claude", true), ("r1", false)]);
        let before = live(&[("claude", true)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(before)], vec![None]);
        io.add_anchors = vec![Some("terminal_99".to_string())].into();
        r.reconcile(RosterView::of(&snap), &mut io);
        assert!(
            io.log.contains(&"stack [terminal_r1,terminal_99]".to_string()),
            "anchor must reach the stack set: {:?}",
            io.log
        );
    }

    #[test]
    fn an_anchor_already_created_this_pass_is_not_stacked_twice() {
        let snap = roster_snap(&[("claude", true), ("r1", false)]);
        let before = live(&[("claude", true)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(before)], vec![None]);
        io.add_anchors = vec![Some("terminal_r1".to_string())].into();
        r.reconcile(RosterView::of(&snap), &mut io);
        assert!(
            io.log.contains(&"stack [terminal_r1]".to_string()),
            "duplicate id must not be repeated: {:?}",
            io.log
        );
    }

    #[test]
    fn adds_are_independent_of_each_other() {
        // Replaces `later_adds_stack_only_after_a_successful_creation`
        // (codex 4df5f0c), whose subject is gone: adds no longer chain
        // through focus, because each one stacks by pane ID afterwards
        // rather than steering `new-pane --stacked`. What must hold now
        // is that one add tells the next nothing — including when the
        // first FAILS, which used to be load-bearing
        // (zellij-pane-placement-and-cost).
        let snap = roster_snap(&[("claude", true), ("r1", false), ("r2", false)]);
        let before = live(&[("claude", true)]);

        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(before.clone())], vec![None]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(
            io.log,
            vec![
                "add r1",
                "add r2",
                "stack [terminal_r1,terminal_r2]",
                "focus user_pane"
            ]
        );

        // A failed first add changes nothing about the second, and the
        // stack still runs over what DID get made — it contributes no
        // id, rather than aborting placement for the rest.
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(before)], vec![None]);
        io.add_results = vec![false].into();
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(
            io.log,
            vec!["add r1", "add r2", "stack [terminal_r2]", "focus user_pane"]
        );
    }

    #[test]
    fn worker_coalesces_in_flight_arrivals_into_exactly_one_follow_up() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Barrier, Mutex};
        let (tx, rx) = std::sync::mpsc::channel();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let log: Arc<Mutex<Vec<RosterView>>> = Arc::new(Mutex::new(Vec::new()));
        let (entered2, release2, log2) = (entered.clone(), release.clone(), log.clone());
        let join = std::thread::spawn(move || {
            let first = AtomicBool::new(true);
            worker_loop(&rx, move |roster, _glyphs| {
                if let Some(v) = roster {
                    log2.lock().unwrap().push(v);
                }
                if first.swap(false, Ordering::SeqCst) {
                    entered2.wait();
                    release2.wait();
                }
            })
        });
        tx.send(WorkerMsg::Roster(view(&["v1"], None))).unwrap();
        entered.wait(); // the first batch is now provably in-flight
        // Arrivals DURING the batch: cannot cancel it, must collapse
        // into exactly ONE follow-up against the newest view.
        tx.send(WorkerMsg::Roster(view(&["v2"], None))).unwrap();
        tx.send(WorkerMsg::Roster(view(&["v3"], None))).unwrap();
        tx.send(WorkerMsg::Roster(view(&["v4"], None))).unwrap();
        // Disconnect while the batch is STILL blocked: shutdown must
        // wait for it (and the drained follow-up), not detach.
        drop(tx);
        release.wait();
        join.join().expect("worker joins after the in-flight batch");
        let log = log.lock().unwrap();
        assert_eq!(
            log.as_slice(),
            &[view(&["v1"], None), view(&["v4"], None)],
            "the running batch plus exactly one follow-up on the newest"
        );
    }

    #[test]
    fn worker_drains_queued_messages_to_one_final_batch_per_kind() {
        use std::sync::{Arc, Mutex};
        // Queue two rosters AND two glyph updates, disconnect before
        // the worker starts: deterministic — one final batch with the
        // newest of EACH kind, then exit.
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(WorkerMsg::Roster(view(&["a"], None))).unwrap();
        tx.send(WorkerMsg::Glyphs(StatusGlyphs {
            emoji: std::collections::BTreeMap::new(),
            wanted: vec!["old".into()],
        }))
        .unwrap();
        tx.send(WorkerMsg::Roster(view(&["b"], None))).unwrap();
        tx.send(WorkerMsg::Glyphs(StatusGlyphs {
            emoji: std::collections::BTreeMap::new(),
            wanted: vec!["new".into()],
        }))
        .unwrap();
        drop(tx);
        let log: Arc<Mutex<Vec<(Option<RosterView>, Option<Vec<String>>)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let log2 = log.clone();
        let join = std::thread::spawn(move || {
            worker_loop(&rx, move |roster, glyphs| {
                log2.lock()
                    .unwrap()
                    .push((roster, glyphs.map(|g| g.wanted)));
            });
        });
        join.join().expect("worker exits when the channel closes");
        assert_eq!(
            log.lock().unwrap().as_slice(),
            &[(Some(view(&["b"], None)), Some(vec!["new".to_string()]))],
            "one batch, newest of each kind"
        );
    }

    #[test]
    fn retitles_after_a_reconcile_use_fresh_titles_not_cached_roles() {
        // codex ff9579f: the race this architecture removes — a retitle
        // with cached PRE-relocation roles landing after the reconciler
        // stamped new ones. Single owner + invalidation: after an
        // acting reconcile, the retitle pass re-lists and stamps the
        // POST-relocation roles.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut state = WorkerState::new();
        // Prime the retitler's cache with PRE-swap titles: claude is
        // master, codex reviewer.
        let pre = "terminal_1  terminal  claude (master)\nterminal_2  terminal  codex (reviewer)";
        let post = "terminal_1  terminal  claude (reviewer)\nterminal_2  terminal  codex (master)";
        let glyphs = || StatusGlyphs {
            emoji: [
                ("claude".to_string(), ("M", "R")),
                ("codex".to_string(), ("M", "R")),
            ]
            .into_iter()
            .collect(),
            wanted: vec!["claude".into(), "codex".into()],
        };
        let lists = AtomicUsize::new(0);
        let mut renames: Vec<(String, String)> = Vec::new();
        state.handle(
            None,
            Some(glyphs()),
            &mut FakeIo::new(vec![]),
            || {
                lists.fetch_add(1, Ordering::SeqCst);
                Some(pre.to_string())
            },
            |id, title| renames.push((id.to_string(), title.to_string())),
        );
        assert_eq!(lists.load(Ordering::SeqCst), 1, "primed once");
        renames.clear();

        // A roster batch swaps the master (reconciler acts: relocate),
        // then the SAME worker retitles: it must re-list (cache
        // invalidated) and stamp post-swap roles — never the cached
        // pre-swap ones.
        let live_post = live(&[("claude", false), ("codex", true)]);
        state.handle(
            Some(view(&["claude", "codex"], Some("codex"))),
            Some(glyphs()),
            &mut FakeIo::new(vec![
                Some(live(&[("claude", true), ("codex", false)])),
                Some(live_post),
            ]),
            || {
                lists.fetch_add(1, Ordering::SeqCst);
                Some(post.to_string())
            },
            |id, title| renames.push((id.to_string(), title.to_string())),
        );
        assert_eq!(
            lists.load(Ordering::SeqCst),
            2,
            "acting reconcile invalidates the cache → retitle re-lists"
        );
        assert!(
            renames
                .iter()
                .any(|(id, t)| id == "terminal_2" && t.contains("codex (master)")),
            "post-swap role stamped from fresh titles: {renames:?}"
        );
        assert!(
            !renames.iter().any(|(_, t)| t.contains("codex (reviewer)")),
            "stale cached role never re-stamped: {renames:?}"
        );
    }

    #[test]
    fn failed_re_list_after_an_acting_reconcile_renames_nothing() {
        // codex afb6d43: invalidation must clear the cached rows too —
        // if the required fresh list FAILS, the retitle pass must emit
        // ZERO renames rather than fall through to stale roles.
        let mut state = WorkerState::new();
        let pre = "terminal_1  terminal  claude (master)\nterminal_2  terminal  codex (reviewer)";
        let glyphs = || StatusGlyphs {
            emoji: [
                ("claude".to_string(), ("M", "R")),
                ("codex".to_string(), ("M", "R")),
            ]
            .into_iter()
            .collect(),
            wanted: vec!["claude".into(), "codex".into()],
        };
        let mut renames: Vec<(String, String)> = Vec::new();
        state.handle(
            None,
            Some(glyphs()),
            &mut FakeIo::new(vec![]),
            || Some(pre.to_string()),
            |id, t| renames.push((id.to_string(), t.to_string())),
        );
        renames.clear();

        // Acting reconcile (master swap), then the fresh list FAILS.
        state.handle(
            Some(view(&["claude", "codex"], Some("codex"))),
            Some(glyphs()),
            &mut FakeIo::new(vec![
                Some(live(&[("claude", true), ("codex", false)])),
                Some(live(&[("claude", false), ("codex", true)])),
            ]),
            || None,
            |id, t| renames.push((id.to_string(), t.to_string())),
        );
        assert!(
            renames.is_empty(),
            "no fresh list → no renames, never stale roles: {renames:?}"
        );
    }

    #[test]
    fn roster_change_invalidates_even_when_another_tui_already_converged() {
        // codex afb6d43: reconcile can no-op (a second TUI already
        // converged the live layout) while OUR cache still holds the
        // previous roster's roles — the glyph pass must re-list, not
        // rename the correct panes back to old roles.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut state = WorkerState::new();
        let pre = "terminal_1  terminal  claude (master)\nterminal_2  terminal  codex (reviewer)";
        let post = "terminal_1  terminal  claude (reviewer)\nterminal_2  terminal  codex (master)";
        let glyphs = || StatusGlyphs {
            emoji: [
                ("claude".to_string(), ("M", "R")),
                ("codex".to_string(), ("M", "R")),
            ]
            .into_iter()
            .collect(),
            wanted: vec!["claude".into(), "codex".into()],
        };
        let lists = AtomicUsize::new(0);
        let mut renames: Vec<(String, String)> = Vec::new();
        state.handle(
            Some(view(&["claude", "codex"], Some("claude"))),
            Some(glyphs()),
            &mut FakeIo::new(vec![Some(live(&[("claude", true), ("codex", false)]))]),
            || {
                lists.fetch_add(1, Ordering::SeqCst);
                Some(pre.to_string())
            },
            |id, t| renames.push((id.to_string(), t.to_string())),
        );
        renames.clear();

        // New roster arrives; the OTHER TUI already converged live
        // state (plan is empty → reconcile no-ops, acted = false).
        state.handle(
            Some(view(&["claude", "codex"], Some("codex"))),
            Some(glyphs()),
            &mut FakeIo::new(vec![Some(live(&[("claude", false), ("codex", true)]))]),
            || {
                lists.fetch_add(1, Ordering::SeqCst);
                Some(post.to_string())
            },
            |id, t| renames.push((id.to_string(), t.to_string())),
        );
        assert_eq!(
            lists.load(Ordering::SeqCst),
            2,
            "roster change re-lists even though reconcile no-oped"
        );
        assert!(
            !renames.iter().any(|(_, t)| t.contains("codex (reviewer)")),
            "already-correct panes never renamed back to old roles: {renames:?}"
        );
    }

    #[test]
    fn parse_current_tab_info_matches_zellij_044_output() {
        // The EXACT bytes from `zellij action current-tab-info` on
        // zellij 0.44.3 (verified via `od -c`): one field per line.
        let out = "name: clank\nid: 0\nposition: 0\n";
        assert_eq!(
            parse_current_tab_info(out),
            Some(("0".to_string(), "clank".to_string()))
        );
        // Order-independent.
        let reordered = "id: 3\nname: my tab\n";
        assert_eq!(
            parse_current_tab_info(reordered),
            Some(("3".to_string(), "my tab".to_string()))
        );
        // Missing id (or name) → None: never rename an unidentified tab.
        assert_eq!(parse_current_tab_info("position: 0\n"), None);
    }

    #[test]
    fn parse_agent_panes_from_live_list_panes() {
        use clank_core::vocab::Role;
        // Exact `zellij action list-panes` shape (0.44.3); terminal_1
        // carries a stale glyph that must be stripped.
        let out = "\
PANE_ID  TYPE  TITLE
plugin_0  plugin  (.) - zellij:link
terminal_0  terminal  claude (master)
terminal_1  terminal  👀 codex (reviewer)
terminal_2  terminal  status
terminal_3  terminal  ruthless (reviewer)
";
        assert_eq!(
            parse_agent_panes(out),
            vec![
                ("terminal_0".to_string(), "claude".to_string(), Role::Master),
                (
                    "terminal_1".to_string(),
                    "codex".to_string(),
                    Role::Reviewer
                ),
                (
                    "terminal_3".to_string(),
                    "ruthless".to_string(),
                    Role::Reviewer
                ),
            ]
        );
    }

    #[test]
    fn pane_status_caches_list_panes_and_renames_only_on_change() {
        // Fix 3 (status-tui-watch-cpu): the pane→agent map is fetched
        // ONCE and reused — no `list-panes` subprocess per render — and
        // a pane is renamed only when its emoji actually changes.
        use std::cell::{Cell, RefCell};

        let panes_out = "0 PANE claude (master)\n1 PANE codex (reviewer)\n";
        let list_calls = Cell::new(0usize);
        let renames = RefCell::new(Vec::<(String, String)>::new());
        let mut ps = PaneStatus {
            last: std::collections::HashMap::new(),
            panes: Vec::new(),
            primed: false,
            requeried_absent: std::collections::HashSet::new(),
        };
        let go = |ps: &mut PaneStatus, s: &StatusSnapshot| {
            ps.update_with(
                &StatusGlyphs::of(s),
                || {
                    list_calls.set(list_calls.get() + 1);
                    Some(panes_out.to_string())
                },
                |id, title| {
                    renames
                        .borrow_mut()
                        .push((id.to_string(), title.to_string()))
                },
            );
        };

        // Glyph derivation now reads the ROSTER (StatusGlyphs::of), so
        // the fixture must carry the team: claude master, codex commit.
        let with_team = |mut s: StatusSnapshot| -> StatusSnapshot {
            s.agents = vec![
                crate::cli::status::AgentAutoRow {
                    label: "claude".into(),
                    role: crate::cli::teams_config::RosterRole::Master,
                    auto_mode: clank_core::vocab::AutoMode::On,
                    tool: "claude".into(),
                    invocation: "claude".into(),
                    session: None,
                },
                crate::cli::status::AgentAutoRow {
                    label: "codex".into(),
                    role: crate::cli::teams_config::RosterRole::Commit,
                    auto_mode: clank_core::vocab::AutoMode::On,
                    tool: "codex".into(),
                    invocation: "codex".into(),
                    session: None,
                },
            ];
            s.master = Some("claude".into());
            s
        };

        // First render: one `list-panes`; both agent panes get a title.
        let awaited = with_team(snap(
            vec![plan_state("p", reviewer_missing("codex"))],
            vec![],
        ));
        go(&mut ps, &awaited);
        assert_eq!(list_calls.get(), 1, "primed with one list-panes");
        let after_first = renames.borrow().len();
        assert_eq!(after_first, 2, "both panes renamed on first render");

        // Same snapshot, many renders: NO further list-panes, NO renames.
        for _ in 0..5 {
            go(&mut ps, &awaited);
        }
        assert_eq!(list_calls.get(), 1, "list-panes reused from cache");
        assert_eq!(
            renames.borrow().len(),
            after_first,
            "unchanged emoji => no rename"
        );

        // Master's turn instead: the plan's wait state toggles BOTH
        // emojis at once — master 💤→🔨 and codex 👀→💤 — so two panes
        // rename. Still no re-query: both panes are cached.
        let idle = with_team(snap(
            vec![plan_state("p", WaitingOn::MasterToContinue)],
            vec![],
        ));
        go(&mut ps, &idle);
        assert_eq!(list_calls.get(), 1, "no re-query: panes already cached");
        assert_eq!(
            renames.borrow().len(),
            after_first + 2,
            "both flipped panes renamed"
        );
    }

    #[test]
    fn agent_pane_title_round_trips_through_parse() {
        use clank_core::vocab::Role;
        // The shared builder's output is recoverable by the parser —
        // pins layout + renamer to one format (no silent drift).
        for (role, label) in [(Role::Master, "alice"), (Role::Reviewer, "bob")] {
            let line = format!(
                "terminal_9  terminal  {}",
                agent_pane_title(label, role.as_str())
            );
            assert_eq!(
                parse_agent_panes(&line),
                vec![("terminal_9".to_string(), label.to_string(), role)]
            );
        }
    }
}
