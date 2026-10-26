//! Zellij tab/pane mirroring for `clank tui`. When running inside
//! zellij, the status pane (which already holds the whole snapshot)
//! mirrors the bar's lamp emoji onto the tab name and each agent's
//! status glyph onto its own pane name. The module IS the namespace —
//! the raw `zellij action …` ops are `zellij::rename_tab` etc. (no
//! `zellij_` stutter); the pure parsers are unit-tested without
//! spawning zellij; [`TabMirror`] / [`PaneStatus`] own the tab's and
//! the panes' lifecycles.

use super::derive::agent_status_emoji;
use super::input::Presence;
use super::strip_leading_emoji;
use crate::cli::open_zellij::agent_pane_title;
use crate::cli::status::StatusSnapshot;
use clank_core::Tool;

fn process_table() -> std::io::Result<String> {
    let out = std::process::Command::new("ps")
        .args(["-axwwE", "-o", "pid=,command="])
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other("ps failed"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn session_token_on(line: &str) -> Option<&str> {
    for key in ["resume ", "--resume ", "--session "] {
        let Some(rest) = line.split(key).nth(1) else {
            continue;
        };
        let token = rest.split_whitespace().next()?;
        if looks_like_session(token) {
            return Some(token);
        }
    }
    None
}

fn looks_like_session(token: &str) -> bool {
    (token.len() >= 32
        && token.contains('-')
        && token.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-'))
        || (token.len() >= 12
            && token.starts_with("ses_")
            && token
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_'))
}

/// Only positive process evidence turns a missing pane into a lost
/// pane. A stale binding by itself keeps the ordinary `?` mark.
fn lost_bound_labels(
    bound: &[(String, Tool, String)],
    panes: &std::collections::BTreeSet<String>,
    processes: &std::collections::BTreeSet<String>,
    claude_background: &std::collections::BTreeSet<String>,
) -> std::collections::BTreeSet<String> {
    bound
        .iter()
        .filter(|(label, tool, id)| {
            !panes.contains(label)
                && (processes.contains(id)
                    || (*tool == Tool::Claude && claude_background.contains(id)))
        })
        .map(|(label, _, _)| label.clone())
        .collect()
}

/// The zellij tab operations the tab mirror needs, injected so its
/// lifecycle is testable without a zellij.
pub(super) trait TabIo {
    /// This session's tabs by stable id, with their names.
    fn tabs(&mut self) -> Option<Vec<(u32, String)>>;
    /// Whether zellij accepted the rename.
    fn rename(&mut self, id: u32, name: &str) -> bool;
}

struct ZellijTabIo;

/// No tabs at all: what the pane-only tests run against.
#[cfg(test)]
struct NoTabs;

#[cfg(test)]
impl TabIo for NoTabs {
    fn tabs(&mut self) -> Option<Vec<(u32, String)>> {
        None
    }
    fn rename(&mut self, _: u32, _: &str) -> bool {
        false
    }
}

impl TabIo for ZellijTabIo {
    fn tabs(&mut self) -> Option<Vec<(u32, String)>> {
        crate::cli::open_zellij::list_tabs()
    }
    fn rename(&mut self, id: u32, name: &str) -> bool {
        crate::cli::open_zellij::rename_tab(id, name)
    }
}

/// Mirrors the bar's glyph into the name of the tab holding this TUI
/// (tui-tab-mirror-bar-emoji), owned by the worker so it is looked at
/// on every batch, not only when the repo changes.
///
/// The tab is the one holding the TUI's own pane, proved from a pane
/// listing; never the active tab, which is the user's. Its actual name,
/// read by id, decides each rename, so a rename that failed, or one
/// something else undid, is put right on the next batch.
pub(super) struct TabMirror {
    /// The TUI's own pane (`terminal_<id>`); `None` disables the mirror.
    caller: Option<String>,
    /// The owned tab's id and its name without a leading glyph, once
    /// proved. Never replaced.
    owned: Option<(u32, String)>,
    wanted: Option<String>,
    /// Some rename was accepted, so exit owes the base name back.
    renamed: bool,
}

impl TabMirror {
    pub(super) fn new(caller: Option<String>) -> Self {
        Self {
            caller,
            owned: None,
            wanted: None,
            renamed: false,
        }
    }

    pub(super) fn want(&mut self, glyph: &str) {
        if !glyph.is_empty() {
            self.wanted = Some(glyph.to_string());
        }
    }

    /// Bring the owned tab's name to the wanted glyph. `owner` is the
    /// caller's tab as the batch's pane listing names it.
    pub(super) fn reconcile(
        &mut self,
        owner: impl FnOnce(&str) -> Option<u32>,
        io: &mut impl TabIo,
    ) {
        let Some(caller) = self.caller.as_deref() else {
            return;
        };
        let mut tabs = None;
        if self.owned.is_none() {
            let Some(id) = owner(caller) else { return };
            let Some(listed) = io.tabs() else { return };
            let Some((_, name)) = listed.iter().find(|(t, _)| *t == id) else {
                return;
            };
            self.owned = Some((id, strip_leading_emoji(name)));
            tabs = Some(listed);
        }
        let (Some((id, base)), Some(glyph)) = (&self.owned, &self.wanted) else {
            return;
        };
        let Some(tabs) = tabs.or_else(|| io.tabs()) else {
            return;
        };
        let want = format!("{glyph} {base}");
        match tabs.iter().find(|(t, _)| t == id) {
            Some((_, name)) if *name != want => {
                if io.rename(*id, &want) {
                    self.renamed = true;
                }
            }
            _ => {}
        }
    }

    /// Put the owned tab's base name back, if a rename was ever
    /// accepted. The worker's last act, after its final batch.
    pub(super) fn restore(&mut self, io: &mut impl TabIo) {
        if let (true, Some((id, base))) = (self.renamed, &self.owned) {
            io.rename(*id, base);
        }
    }
}

fn rename_pane(id: &str, name: &str) {
    crate::cli::open_zellij::rename_pane(id, name)
}

/// Per-refresh retitle data, derived ON THE LOOP (pure — no zellij)
/// and sent to the worker. Pane titles have ONE owner — the worker — so
/// these renames can never race the reconciler's (codex ff9579f).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StatusGlyphs {
    /// label → the title its pane should carry: the status glyph (which
    /// the ROSTER's role decides), then the label. A role or state change
    /// rewrites only the glyph, so the identity the title carries stays
    /// put.
    titles: std::collections::BTreeMap<String, String>,
    /// The bar's glyph, which the tab's name carries.
    tab: String,
}

impl StatusGlyphs {
    pub(super) fn of(snap: &StatusSnapshot) -> Self {
        let titles = snap
            .agents
            .iter()
            .map(|a| {
                let role = a.role.resolved();
                let emoji = agent_status_emoji(snap, &a.label, role);
                (
                    a.label.clone(),
                    format!("{emoji} {}", agent_pane_title(&a.label)),
                )
            })
            .collect();
        Self {
            titles,
            tab: super::render::bar_emoji(snap),
        }
    }
}

/// Mirrors each AGENT's status glyph onto its OWN pane name
/// (tui-agent-pane-status-emoji). The `clank tui` pane already
/// holds the whole snapshot and can rename any pane by id, so it owns
/// this centrally — the Stop hook stays clean. Renames a pane only
/// when its desired title CHANGES (dedup per id). `None` (no-op)
/// outside zellij; best-effort.
pub(super) struct PaneStatus {
    /// Pane id → last title rendered. Dedup: rename a pane only when
    /// its desired title changes.
    last: std::collections::HashMap<String, String>,
    /// `(pane_id, label)` from the batch's listing — the one the
    /// batch takes per REFRESH, never per render (status-tui-watch-cpu
    /// Fix 3: a per-render subprocess loaded the zellij server; a
    /// refresh is throttled and event-driven, and presence rides on
    /// the same listing).
    panes: Vec<(String, String)>,
}

impl PaneStatus {
    /// Lives on the [`ReconcileWorker`] thread (only spawned inside
    /// zellij), so no env gate here.
    fn new() -> Self {
        Self {
            last: std::collections::HashMap::new(),
            panes: Vec::new(),
        }
    }

    /// The reconciler acted: the last-stamped titles may name
    /// pre-relocation roles — forget them so every pane is restamped
    /// off the next fresh listing (codex ff9579f). Rows go too: if that
    /// listing FAILS, the pass falls through to whatever is here, and
    /// stale rows would restamp pre-relocation roles (codex afb6d43).
    fn invalidate(&mut self) {
        self.last.clear();
        self.panes.clear();
    }

    /// Core of the retitle pass with the zellij I/O injected, so the
    /// dedup is testable without spawning (no-binary-spawning-tests).
    /// `fresh` is the batch's listing when it answered; `rename` runs
    /// only for panes whose title changed.
    fn update_with(
        &mut self,
        glyphs: &StatusGlyphs,
        fresh: Option<Vec<(String, String)>>,
        mut rename: impl FnMut(&str, &str),
    ) {
        if let Some(panes) = fresh {
            self.panes = panes;
        }
        // Build the rename list from the cached map first (immutable
        // borrow), then apply — keeps `self.panes` and `self.last`
        // borrows disjoint.
        let mut renames: Vec<(String, String)> = Vec::new();
        for (id, label) in &self.panes {
            // A cached pane whose label the roster no longer knows gets
            // no title — leave it; the reconciler owns its fate.
            let Some(title) = glyphs.titles.get(label) else {
                continue;
            };
            if self.last.get(id).map(String::as_str) != Some(title.as_str()) {
                renames.push((id.clone(), title.clone()));
            }
        }
        for (id, title) in renames {
            rename(&id, &title);
            self.last.insert(id, title);
        }
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
    /// Consecutive passes for `failing` that did not verify. Any step
    /// — an add, a close, the override, the read-back — can fail in a
    /// way retrying does not fix (reviewers split across tabs, a
    /// server that refuses the layout), and retrying every refresh is
    /// what turned a wrong verdict into permanent focus churn. So the
    /// whole transaction is bounded, independently of whether the
    /// read-back's verdict is right
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
    /// Why the ownership record could not be read or written, as last
    /// tried.
    ownership_error: Option<String>,
    /// Pane names in the workspace that read as an agent no candidate
    /// claims, from the last pass that had no usable record: seen, and
    /// deliberately not closed.
    unclaimed: Labels,
    /// Roster labels the last pass did not open, and the panes that may
    /// already be theirs.
    withheld: (Labels, Vec<crate::cli::open_zellij::Ambiguity>),
}

type Labels = std::collections::BTreeSet<String>;

/// The labels a pane of the workspace may be identified as: the roster,
/// plus every label the ownership record holds. Desired membership stays
/// the roster alone ([`plan_panes`]), so a pane whose agent left the
/// roster is still recognised, counted as excess, and closed.
fn ownership_candidates(cur: &RosterView, record: &Result<Option<Labels>, String>) -> Labels {
    let mut out = cur.labels.clone();
    if let Ok(Some(owned)) = record {
        out.extend(owned.iter().cloned());
    }
    out
}

/// One listing as the reconciler reads it: the agent pairs, and the panes
/// whose names read as two candidates. Those are evidence neither for nor
/// against any label they name, so such a label is neither opened nor
/// dropped from the record.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Seen {
    pub(super) pairs: Vec<(String, bool)>,
    pub(super) ambiguous: Vec<crate::cli::open_zellij::Ambiguity>,
}

impl Seen {
    fn ambiguous_labels(&self) -> Labels {
        self.ambiguous
            .iter()
            .flat_map(|a| a.candidates.iter().cloned())
            .collect()
    }

    fn doubts(&self, label: &str) -> Vec<crate::cli::open_zellij::Ambiguity> {
        self.ambiguous
            .iter()
            .filter(|a| a.candidates.contains(label))
            .cloned()
            .collect()
    }
}

/// A verifying read-back: the listing, and whether the reviewers are
/// placed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Verified {
    pub(super) seen: Seen,
    pub(super) placed: bool,
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

    fn reviewers(&self) -> Vec<String> {
        self.labels
            .iter()
            .filter(|l| Some(*l) != self.master.as_ref())
            .cloned()
            .collect()
    }
}

/// What a targeted reopen found — the status line's whole vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReopenOutcome {
    Reopened,
    AlreadyOpen,
    /// zellij did not answer the listing.
    NoListing,
    /// The listing said the pane was missing and `new-pane` made
    /// nothing.
    NotCreated,
    /// The pane exists but the read-back did not find it where it
    /// belongs — a reviewer outside the stack, a master not staged.
    Unplaced,
    NotOnRoster,
    NotInSession,
    /// A pane may already be the label's: its name reads as this label
    /// and another.
    Ambiguous(Vec<crate::cli::open_zellij::Ambiguity>),
}

impl ReopenOutcome {
    pub(super) fn describe(&self, label: &str) -> String {
        match self {
            Self::Reopened => format!("reopened `{label}`'s pane"),
            Self::AlreadyOpen => format!("`{label}` already has a pane in this tab"),
            Self::NoListing => "zellij did not answer; nothing was opened".to_string(),
            Self::NotCreated => format!("zellij did not create a pane for `{label}`"),
            Self::Unplaced => format!(
                "opened `{label}`'s pane, but it did not land where it belongs — alt+[ or \
                 a roster change re-lays the tab"
            ),
            Self::NotOnRoster => format!("`{label}` is not on the roster"),
            Self::NotInSession => "not inside a zellij session; nothing to reopen into".to_string(),
            Self::Ambiguous(doubts) => format!(
                "`{label}` may already have a pane, so none was opened: {}. Rename that pane \
                 to one agent's label",
                crate::cli::open_zellij::describe_ambiguities(doubts)
            ),
        }
    }
}

/// What one reconcile pass must do: open panes for roster members with
/// none, close panes whose label left the roster (closing kills the
/// pane's process tree — that is what guarantees the agent exits), and
/// re-layout on a master change. `live` is `(label, staged)` pairs
/// from the actual panes; the stage's CURRENT owner comes from the
/// listing's geometry, so a master swap done while no TUI was running
/// is still detected on startup (codex a730882 concern 2). Pure.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct PanePlan {
    pub(super) add: Vec<String>,
    /// One entry PER PANE to close: labels gone from the roster (each
    /// occurrence) plus the excess copies of duplicated in-roster
    /// labels (a two-TUI race can double-open; each `remove` closes
    /// one matching pane).
    pub(super) remove: Vec<String>,
    /// `(new_master, staged_pane_if_any)` whenever the pane on the
    /// STAGE is not the roster master — including when nothing is (a
    /// staged team replacement). Read from geometry, never titles.
    pub(super) relocate: Option<(String, Option<String>)>,
    /// Roster labels with no pane but one that may be theirs: not
    /// opened, since that pane may be the one.
    pub(super) withheld: Labels,
}

impl PanePlan {
    fn is_converged(&self) -> bool {
        self.add.is_empty() && self.remove.is_empty() && self.relocate.is_none()
    }
}

pub(super) fn plan_panes(cur: &RosterView, seen: &Seen) -> PanePlan {
    let live = &seen.pairs;
    let ambiguous = seen.ambiguous_labels();
    // Desired state, modeled explicitly: EXACTLY one pane per roster
    // label, and the pane titled master is exactly the roster master.
    let mut count: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for (l, _) in live {
        *count.entry(l.as_str()).or_default() += 1;
    }
    let (withheld, add): (Labels, Labels) = cur
        .labels
        .iter()
        .filter(|l| !count.contains_key(l.as_str()))
        .cloned()
        .partition(|l| ambiguous.contains(l));
    let add = add.into_iter().collect();
    let mut remove = Vec::new();
    for (label, n) in &count {
        let excess = if cur.labels.contains(*label) {
            n - 1
        } else {
            *n
        };
        remove.extend(std::iter::repeat_n(label.to_string(), excess));
    }
    // The stage invariant: the pane on the stage is the roster master.
    // Any other pane there — including one whose label is leaving —
    // means a re-layout; a roster master that is missing, in the
    // stack, or unopened is staged with no stale occupant to name.
    let titled: Vec<&str> = live
        .iter()
        .filter(|(_, titled)| *titled)
        .map(|(l, _)| l.as_str())
        .collect();
    let relocate = match cur.master.as_deref() {
        None => None,
        // The stage may hold an ambiguous pane that is the master's.
        Some(new) if ambiguous.contains(new) => None,
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
        withheld,
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
    /// One listing seen through the repo's workspace, its panes named
    /// against `candidates`. `None` when zellij did not answer or the
    /// workspace is unresolved: either way, nothing may be acted on.
    fn snapshot(&mut self, candidates: &Labels) -> Option<Self::Snap>;
    fn seen(&mut self, snap: &Self::Snap) -> Seen;
    /// Post-action ground truth for convergence: the label/stage pairs
    /// AND whether the reviewers are correctly placed, from ONE read.
    /// Both, because a pass that only removed still has to answer for
    /// placement — checking labels alone lets an unstacked tab be
    /// cached (codex on d5121e1) — and splitting them would cost a
    /// second session-wide listing.
    fn verify(&mut self, candidates: &Labels, reviewers: &[String]) -> Option<Verified>;
    /// Pane names in `snap`'s workspace that read as an agent label no
    /// candidate claims.
    fn unclaimed(&mut self, snap: &Self::Snap) -> Labels;
    /// The ownership record: `Ok(None)` when there is none.
    fn owned(&mut self) -> Result<Option<Labels>, String>;
    /// Add `own` and drop `disown` from the ownership record, every other
    /// label kept.
    fn reown(&mut self, own: &Labels, disown: &Labels) -> Result<(), String>;
    /// The pane to restore focus to after the pass. Takes the pass's
    /// listing so the target costs no session-wide query of its own.
    fn capture_focus(&mut self, snap: &Self::Snap) -> Option<String>;
    fn restore_focus(&mut self, id: &str);
    /// Open the pane for `label` in the workspace's tab, named for it;
    /// reports the id when one was made. Where it lands is the layout's
    /// business, applied afterwards.
    fn add(&mut self, label: &str, snap: &Self::Snap) -> Option<String>;
    /// Whether the reviewers are ALREADY stacked in `snap`. Presence
    /// of every label does not imply correct placement, so the
    /// converged path consults this before caching — and before
    /// re-laying a tab that is already right.
    fn is_placed(&mut self, reviewers: &[String], snap: &Self::Snap) -> bool;
    /// The tab layout for `master` and `reviewers` AS RUNNING — the
    /// composition `clank open` launches from, at the tab's current
    /// orientation, with the user's template if one is configured.
    /// `None` when it cannot be composed (no tab of the repo's in the
    /// listing, a template that no longer parses).
    fn compose(&mut self, master: &str, reviewers: &[String], snap: &Self::Snap) -> Option<String>;
    /// Apply `kdl` to the repo's tab in place. Whether it was
    /// accepted; placement is read back, never assumed.
    fn override_layout(&mut self, kdl: &str, snap: &Self::Snap) -> bool;
    fn remove_all(&mut self, labels: &[String], snap: &Self::Snap);
    /// The labels whose pane's PROCESS is still running. `pairs` counts
    /// an exited pane as its label's — identity, so a corpse can be
    /// closed — and this is the other question: is the agent open.
    fn live_labels(&mut self, snap: &Self::Snap) -> std::collections::BTreeSet<String>;
    /// `(pane id, label)` for every pane of the workspace named for an
    /// agent — the retitler's map, read off the same listing presence
    /// uses.
    fn title_rows(&mut self, snap: &Self::Snap) -> Vec<(String, String)>;
    /// Bound agents with no pane but a process still holding their
    /// session. Read only; this never starts or reconnects an agent.
    fn lost_labels(&mut self, snap: &Self::Snap) -> std::collections::BTreeSet<String>;
    /// The tab holding the terminal pane `caller`, as `snap` names it.
    fn tab_of(&mut self, _snap: &Self::Snap, _caller: &str) -> Option<u32> {
        None
    }
}

/// One listing and the workspace it was read through.
pub(super) struct Scoped {
    panes: Vec<crate::cli::open_zellij::ZellijPane>,
    ws: crate::cli::open_zellij::Workspace,
}

impl Scoped {
    /// `panes` seen from the TUI: its own pane's tab, else the repo's one
    /// TUI pane's. `None` when neither names a tab.
    fn of(
        panes: Vec<crate::cli::open_zellij::ZellijPane>,
        repo: &std::path::Path,
        caller: Option<&str>,
        candidates: &Labels,
    ) -> Option<Self> {
        let ws =
            crate::cli::open_zellij::Workspace::local(&panes, repo, caller, candidates.clone())?;
        Some(Self { panes, ws })
    }

    fn seen(&self) -> Seen {
        Seen {
            pairs: crate::cli::open_zellij::agent_pane_pairs(&self.panes, &self.ws),
            ambiguous: crate::cli::open_zellij::ambiguities(&self.panes, &self.ws),
        }
    }

    fn stacked(&self, repo: &std::path::Path, reviewers: &[String]) -> bool {
        crate::cli::open_zellij::reviewers_are_stacked(&self.panes, &self.ws, repo, reviewers)
    }

    fn verified(&self, repo: &std::path::Path, reviewers: &[String]) -> Verified {
        Verified {
            seen: self.seen(),
            placed: self.stacked(repo, reviewers),
        }
    }

    fn unclaimed(&self, repo: &std::path::Path, caller: Option<&str>) -> Labels {
        crate::cli::open_zellij::unclaimed_labels(&self.panes, &self.ws, repo, caller)
    }

    fn live_labels(&self) -> Labels {
        crate::cli::open_zellij::live_agent_labels(&self.panes, &self.ws)
    }
}

/// The real zellij-backed [`PaneIo`].
struct ZellijPaneIo<'a> {
    repo: &'a std::path::Path,
    /// The TUI's own pane, whose tab is the workspace.
    caller: Option<String>,
}

impl PaneIo for ZellijPaneIo<'_> {
    type Snap = Scoped;
    fn snapshot(&mut self, candidates: &Labels) -> Option<Self::Snap> {
        Scoped::of(
            crate::cli::open_zellij::snapshot_panes()?,
            self.repo,
            self.caller.as_deref(),
            candidates,
        )
    }
    fn seen(&mut self, snap: &Self::Snap) -> Seen {
        snap.seen()
    }
    fn verify(&mut self, candidates: &Labels, reviewers: &[String]) -> Option<Verified> {
        // A FRESH listing, not `dump-layout`. The dump was the cheap
        // source (1.0s vs 5.6s on a 28-pane session) but it cannot
        // identify an agent that has STARTED: it reports the running
        // program, which the exec turns into `opencode`, `uv`,
        // `caffeinate`, a vendored codex path, even a child like
        // `rust-analyzer`. The listing keeps each pane's name. A name
        // identifies an agent only against the roster and the ownership
        // record, inside the repo's workspace, and is refused when it
        // reads as two candidates: a bare title would let any pane named
        // like an agent pass (zellij-pane-placement-and-cost).
        //
        // Cost is bounded by WHEN this runs: a converged pass returns
        // on its first listing and never reaches here, so the second
        // read is paid only by passes that actually changed something.
        Some(self.snapshot(candidates)?.verified(self.repo, reviewers))
    }
    fn unclaimed(&mut self, snap: &Self::Snap) -> Labels {
        snap.unclaimed(self.repo, self.caller.as_deref())
    }
    fn owned(&mut self) -> Result<Option<Labels>, String> {
        crate::cli::pane_owners::load(self.repo).map_err(|e| format!("{e:#}"))
    }
    fn reown(&mut self, own: &Labels, disown: &Labels) -> Result<(), String> {
        crate::cli::pane_owners::update(self.repo, own, disown)
            .map(drop)
            .map_err(|e| format!("{e:#}"))
    }
    fn capture_focus(&mut self, snap: &Self::Snap) -> Option<String> {
        crate::cli::open_zellij::pass_focus_target(&snap.panes)
    }
    fn restore_focus(&mut self, id: &str) {
        crate::cli::open_zellij::focus_pane(id);
    }
    fn add(&mut self, label: &str, snap: &Self::Snap) -> Option<String> {
        crate::cli::open_zellij::open_agent_pane(self.repo, label, &snap.panes, &snap.ws)
    }
    fn is_placed(&mut self, reviewers: &[String], snap: &Self::Snap) -> bool {
        snap.stacked(self.repo, reviewers)
    }
    fn tab_of(&mut self, snap: &Self::Snap, caller: &str) -> Option<u32> {
        crate::cli::open_zellij::owned_tab(&snap.panes, caller)
    }
    fn compose(&mut self, master: &str, reviewers: &[String], snap: &Self::Snap) -> Option<String> {
        let orientation = crate::cli::open_zellij::tab_orientation(&snap.panes, snap.ws.tab());
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        crate::cli::open_zellij::compose_live_layout(
            self.repo,
            home.as_deref(),
            master,
            reviewers,
            orientation,
        )
        .ok()
    }
    fn override_layout(&mut self, kdl: &str, snap: &Self::Snap) -> bool {
        crate::cli::open_zellij::override_tab_layout(self.repo, snap.ws.tab(), kdl)
    }
    fn remove_all(&mut self, labels: &[String], snap: &Self::Snap) {
        crate::cli::open_zellij::remove_reviewer_panes(labels, &snap.panes, &snap.ws);
    }
    fn live_labels(&mut self, snap: &Self::Snap) -> std::collections::BTreeSet<String> {
        snap.live_labels()
    }
    fn title_rows(&mut self, snap: &Self::Snap) -> Vec<(String, String)> {
        crate::cli::open_zellij::agent_panes_by_title(&snap.panes, &snap.ws)
    }
    fn lost_labels(&mut self, snap: &Self::Snap) -> std::collections::BTreeSet<String> {
        let mut panes = snap.live_labels();
        panes.extend(snap.seen().ambiguous_labels());
        let Ok(configs) = crate::agent_store::load_all_agent_configs(self.repo) else {
            return Default::default();
        };
        let bound: Vec<(String, Tool, String)> = configs
            .into_iter()
            .filter_map(|(label, cfg)| {
                let session = cfg.session?;
                Some((
                    label.as_str().to_string(),
                    session.tool,
                    session.id.to_string(),
                ))
            })
            .filter(|(label, _, _)| !panes.contains(label))
            .collect();
        if bound.is_empty() {
            return Default::default();
        }
        let processes: std::collections::BTreeSet<String> = process_table()
            .ok()
            .into_iter()
            .flat_map(|ps| {
                ps.lines()
                    .filter_map(session_token_on)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect();
        let claude_background = if bound
            .iter()
            .any(|(_, tool, id)| *tool == Tool::Claude && !processes.contains(id))
        {
            crate::cli::session_holder::claude_agents_listing("claude".to_string())
                .into_iter()
                .filter(|agent| agent.kind == "background")
                .map(|agent| agent.session_id)
                .collect()
        } else {
            Default::default()
        };
        lost_bound_labels(&bound, &panes, &processes, &claude_background)
    }
}

impl PaneReconciler {
    pub(super) fn new() -> Self {
        Self {
            converged: None,
            failed_repairs: 0,
            failing: None,
            pending: std::collections::BTreeMap::new(),
            ownership_error: None,
            unclaimed: Labels::new(),
            withheld: Default::default(),
        }
    }

    /// The ownership record as this pass reads it, noting why when it
    /// cannot be read.
    fn read_owned(&mut self, io: &mut impl PaneIo) -> Result<Option<Labels>, String> {
        let record = io.owned();
        self.ownership_error = record.as_ref().err().cloned();
        record
    }

    /// Bring the ownership record up to what this pass established.
    ///
    /// A label is owned once its pane is created, and adopted when a
    /// roster label's pane is seen. A label is dropped only on an
    /// answered, verified listing (`seen`) that shows no pane of it, no
    /// ambiguous pane naming it, and no creation of it still pending;
    /// roster labels are never dropped, since a pane opened elsewhere may
    /// not be listed yet and the roster makes them candidates anyway.
    /// An unreadable record is left as it is, and nothing is written
    /// when nothing changes.
    fn settle_ownership(
        &mut self,
        cur: &RosterView,
        record: &Result<Option<Labels>, String>,
        seen: Option<&Seen>,
        created: &Labels,
        io: &mut impl PaneIo,
    ) {
        let Ok(known) = record else {
            return;
        };
        let known = known.clone().unwrap_or_default();
        let mut own = created.clone();
        let mut disown = Labels::new();
        if let Some(seen) = seen {
            let ambiguous = seen.ambiguous_labels();
            let listed: Labels = seen.pairs.iter().map(|(l, _)| l.clone()).collect();
            own.extend(listed.iter().filter(|l| cur.labels.contains(*l)).cloned());
            disown = known
                .iter()
                .filter(|l| {
                    !cur.labels.contains(*l)
                        && !listed.contains(*l)
                        && !ambiguous.contains(*l)
                        && !self.pending.contains_key(*l)
                })
                .cloned()
                .collect();
        }
        own.retain(|l| !known.contains(l));
        if own.is_empty() && disown.is_empty() {
            return;
        }
        self.ownership_error = io.reown(&own, &disown).err();
    }

    /// What the operator should be told about pane ownership, if anything.
    fn ownership_notice(&self) -> Option<String> {
        let mut parts = Vec::new();
        let (labels, doubts) = &self.withheld;
        if !labels.is_empty() {
            let names: Vec<String> = labels.iter().map(|l| format!("`{l}`")).collect();
            parts.push(format!(
                "{}: not opened, since a pane may already be theirs ({}); rename it to one \
                 agent's label",
                names.join(", "),
                crate::cli::open_zellij::describe_ambiguities(doubts)
            ));
        }
        if !self.unclaimed.is_empty() {
            let names: Vec<String> = self.unclaimed.iter().map(|l| format!("`{l}`")).collect();
            parts.push(format!(
                "{}: not on roster, not closed (no pane-ownership record says clank opened it)",
                names.join(", ")
            ));
        }
        if let Some(e) = &self.ownership_error {
            parts.push(format!("pane-ownership record: {e}"));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
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
    /// Believed panes are reported NOT staged: the layout that places
    /// them runs after the adds, so `false` is what a listing would say
    /// about a pane created this pass.
    fn believe_pending(&mut self, seen: &Seen) -> Seen {
        self.confirm_listed(&seen.pairs);
        self.pending.retain(|_, left| {
            *left = left.saturating_sub(1);
            *left > 0
        });
        let mut out = seen.clone();
        out.pairs
            .extend(self.pending.keys().map(|l| (l.clone(), false)));
        out
    }

    /// A label a listing shows is no longer a belief. Called on EVERY
    /// listing the reconciler reads, the verifying read-back included:
    /// a pass that created a pane, saw it in the verify, and cached the
    /// view converged never reached `believe_pending` again, so the
    /// belief outlived the pane — and the first targeted reopen after
    /// a hand-close answered "already open" off that belief, over a
    /// fresh listing that said otherwise (codex on 1bf604d).
    fn confirm_listed(&mut self, listed: &[(String, bool)]) {
        self.pending
            .retain(|label, _| !listed.iter().any(|(l, _)| l == label));
    }

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

    /// One reconciliation pass for `cur`, skipped when that exact view
    /// was already VERIFIED converged. Cheap in the steady state: one
    /// set comparison, no zellij calls. Runs on the [`ReconcileWorker`]
    /// thread, never the TUI loop (tui-reconcile-off-loop). Returns
    /// whether the pass issued actions.
    ///
    /// Membership, then the layout, then the read-back: what the
    /// roster says exists is opened or closed, and the arrangement is
    /// the layout composed from the roster as running — the same
    /// function `clank open` launches from, applied in place. Pane
    /// shuffling with `stack-panes` and `move-pane` used to do the
    /// arranging while zellij kept the swap variants written at open,
    /// and every roster change made the two disagree: alt+[ after a
    /// promotion put the OLD master back on the stage
    /// (placement-is-a-layout-applied-not-panes-shuffled).
    fn reconcile(&mut self, cur: RosterView, io: &mut impl PaneIo) -> bool {
        if self.converged.as_ref() == Some(&cur) {
            return false;
        }
        let record = self.read_owned(io);
        let candidates = ownership_candidates(&cur, &record);
        // Listing failure → touch nothing AND stay unconverged, so the
        // next refresh retries (acting on a partial listing would
        // re-open every pane; forgetting the event would drop it).
        let Some(mut snap) = io.snapshot(&candidates) else {
            return false;
        };
        self.unclaimed = match &record {
            Ok(Some(_)) => Labels::new(),
            _ => io.unclaimed(&snap),
        };
        let listed = io.seen(&snap);
        let plan = plan_panes(&cur, &self.believe_pending(&listed));
        self.withheld = (
            plan.withheld.clone(),
            listed
                .ambiguous
                .iter()
                .filter(|a| !a.candidates.is_disjoint(&plan.withheld))
                .cloned()
                .collect(),
        );
        let reviewers = cur.reviewers();
        // Every label present AND placed: nothing to do, and no
        // re-layout — an override on a tab that is already right
        // would only move the user's focus for nothing.
        if plan.is_converged() && io.is_placed(&reviewers, &snap) {
            self.settle_ownership(&cur, &record, Some(&listed), &Labels::new(), io);
            self.converged = Some(cur);
            return false;
        }
        // Pass-level focus transaction: capture the user's focus once
        // before the first action, restore once after the last
        // (zellij-one-listing-per-pass).
        let focus = io.capture_focus(&snap);

        // MEMBERSHIP first: departed labels closed (killing the pane's
        // process tree is the agent-exit guarantee), missing ones
        // opened. An exited pane is still its label's — the ✗ and the
        // operator's reopen, never a relaunch from here.
        if !plan.remove.is_empty() {
            io.remove_all(&plan.remove, &snap);
        }
        let mut membership_changed = !plan.remove.is_empty();
        let mut created = Labels::new();
        for label in &plan.add {
            if io.add(label, &snap).is_some() {
                // Believed live until a listing confirms it, so the
                // next pass cannot open this label a second time.
                self.pending.insert(label.clone(), PENDING_CREATE_PASSES);
                created.insert(label.clone());
                membership_changed = true;
            }
        }
        // The LAYOUT, from what is running now. A slot naming a pane
        // that is not live makes zellij spawn a duplicate, so the
        // listing is refreshed after any membership change and the
        // roster is intersected with it. A refresh that fails leaves
        // the pass to verify and retry.
        if membership_changed && let Some(fresh) = io.snapshot(&candidates) {
            snap = fresh;
        }
        self.apply_layout(&cur, io, &snap);

        if let Some(id) = &focus {
            io.restore_focus(id);
        }
        // Converged only when a VERIFYING read confirms the target
        // state — every action above is best-effort, so observation is
        // not achievement.
        let verified = io.verify(&candidates, &reviewers);
        if let Some(v) = &verified {
            self.confirm_listed(&v.seen.pairs);
        }
        self.settle_ownership(
            &cur,
            &record,
            verified.as_ref().map(|v| &v.seen),
            &created,
            io,
        );
        if verified.is_some_and(|v| v.placed && plan_panes(&cur, &v.seen).is_converged()) {
            self.note_success();
            self.converged = Some(cur);
        } else if self.note_failure(&cur) {
            // A pass that never verifies acts on the tab every refresh
            // otherwise — the unbounded side-effect loop the budget
            // exists to end (codex on dfeff8c, 40760e0). Left alone
            // until the roster changes.
            self.converged = Some(cur);
        }
        true
    }

    /// Compose the tab layout for the roster AS RUNNING and apply it:
    /// master on the stage, reviewers in the stack, the swap variants
    /// alt+[ / alt+] will apply from now on — one action, one source
    /// of truth for placement. No running master, no override: the
    /// layout has no stage to give, and staging nobody would be a
    /// re-layout for nothing; the reopen that brings the master back
    /// applies one. Returns whether an override was issued and
    /// accepted.
    fn apply_layout<I: PaneIo>(&mut self, cur: &RosterView, io: &mut I, snap: &I::Snap) -> bool {
        let running = io.live_labels(snap);
        let Some(master) = cur.master.as_deref().filter(|m| running.contains(*m)) else {
            return false;
        };
        let reviewers: Vec<String> = cur
            .reviewers()
            .into_iter()
            .filter(|r| running.contains(r))
            .collect();
        let Some(kdl) = io.compose(master, &reviewers, snap) else {
            return false;
        };
        io.override_layout(&kdl, snap)
    }

    /// Bring back ONE label's pane — the per-agent "reopen pane" item
    /// (reopen-an-agents-pane-from-its-menu).
    ///
    /// A pane closed by hand changes nothing in the roster view, so a
    /// converged reconciler never looks again; this is the look. It is
    /// scoped to `label` because a full pass adds EVERY missing label
    /// and two can be missing at once — reopening one must not launch
    /// the other. So it never enters the retry loop and never touches
    /// `converged`: a following snapshot with the same view stays a
    /// no-op, which is also why nothing here is verified back into the
    /// cache (letting the loop see an unconverged tab would have it add
    /// the rest next round, the same leak one step removed).
    ///
    /// It IS list-then-create — safe because one TUI per repo is
    /// settled at startup ([`super::lease`]), so there is no second
    /// driver in any session to race with.
    pub(super) fn reopen(
        &mut self,
        cur: &RosterView,
        label: &str,
        io: &mut impl PaneIo,
    ) -> ReopenOutcome {
        if !cur.labels.contains(label) {
            return ReopenOutcome::NotOnRoster;
        }
        let record = self.read_owned(io);
        let candidates = ownership_candidates(cur, &record);
        let Some(mut snap) = io.snapshot(&candidates) else {
            return ReopenOutcome::NoListing;
        };
        let seen = io.seen(&snap);
        let doubts = seen.doubts(label);
        if !doubts.is_empty() {
            return ReopenOutcome::Ambiguous(doubts);
        }
        let mut plan = plan_panes(cur, &self.believe_pending(&seen));
        if !plan.add.iter().any(|l| l == label) {
            if io.live_labels(&snap).contains(label) {
                return ReopenOutcome::AlreadyOpen;
            }
            // Listed but not live: every pane this label has is a
            // corpse zellij kept open after the process ended. To the
            // reconciler that is still the label's pane (so `remove`
            // can find it); to the user the agent is plainly not open.
            // Close it and list again — `add` finds panes by that same
            // identity and would otherwise decline to make one.
            io.remove_all(&[label.to_string()], &snap);
            let Some(fresh) = io.snapshot(&candidates) else {
                return ReopenOutcome::NoListing;
            };
            snap = fresh;
            plan = plan_panes(cur, &self.believe_pending(&io.seen(&snap)));
            if !plan.add.iter().any(|l| l == label) {
                return ReopenOutcome::NotCreated;
            }
        }
        let reviewers = cur.reviewers();
        let focus = io.capture_focus(&snap);
        let created = io.add(label, &snap);
        // Placement is FOR the new pane; without one there is nothing to
        // place, and re-laying the tab anyway would rearrange live panes
        // on behalf of a pane that does not exist (codex on 54acac6).
        if created.is_none() {
            if let Some(id) = &focus {
                io.restore_focus(id);
            }
            return ReopenOutcome::NotCreated;
        }
        self.pending
            .insert(label.to_string(), PENDING_CREATE_PASSES);
        self.settle_ownership(cur, &record, None, &Labels::from([label.to_string()]), io);
        // The layout may only name what is running, and the new pane is
        // not in the pass-start listing: list again, then apply. A
        // reopened master is staged by the same override that stacks a
        // reopened reviewer.
        if let Some(fresh) = io.snapshot(&candidates) {
            snap = fresh;
        }
        self.apply_layout(cur, io, &snap);
        let as_master = cur.master.as_deref() == Some(label);
        // Reopened means READ BACK where it belongs, not merely created:
        // every action here is best-effort, and the notice must not
        // claim a placement the tab does not show. A master belongs on
        // the stage; a reviewer belongs in the stack. The same read-back
        // confirms the label, so the belief does not outlive the pane.
        let placed = io.verify(&candidates, &reviewers).is_some_and(|v| {
            self.confirm_listed(&v.seen.pairs);
            v.seen
                .pairs
                .iter()
                .any(|(l, staged)| l == label && (if as_master { *staged } else { v.placed }))
        });
        if let Some(id) = &focus {
            io.restore_focus(id);
        }
        if placed {
            ReopenOutcome::Reopened
        } else {
            ReopenOutcome::Unplaced
        }
    }
}

/// Whether zellij can be reached from this process, as last observed
/// by the worker.
///
/// Three states rather than two, because they call for different
/// actions from the operator: outside a session `clank open` would
/// START one, while inside a session that does not answer something
/// is wrong with zellij itself — and that case otherwise looks like
/// "clank did nothing" (zellij-is-the-workspace).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ZellijReach {
    /// `$ZELLIJ` is unset. Pane operations are unavailable by design.
    NotInSession,
    /// `$ZELLIJ` is set but no listing has returned yet. A distinct
    /// state rather than a guess: seeding `Connected` before any
    /// evidence showed a dead session as healthy on its first frame —
    /// the exact ambiguity this indicator exists to remove (codex on
    /// a9c9bd4).
    Unknown,
    /// In a session and the client answered the last listing.
    Connected,
    /// In a session but the client did NOT answer.
    Unreachable,
}

/// A `PaneIo` that records whether each listing it performs was
/// answered. Reachability is READ OFF work the batch was already
/// doing — a batch that lists zero times (converged, cache warm)
/// observes nothing, and the last value stands. This is what "costs no
/// probe of its own" has to mean in code, not in a comment (codex on
/// d855068).
struct Observed<'a, I: PaneIo> {
    inner: I,
    seen: &'a std::cell::Cell<Option<ZellijReach>>,
}

impl<I: PaneIo> Observed<'_, I> {
    fn note(&self, answered: bool) {
        self.seen.set(Some(if answered {
            ZellijReach::Connected
        } else {
            ZellijReach::Unreachable
        }));
    }
}

impl<I: PaneIo> PaneIo for Observed<'_, I> {
    type Snap = I::Snap;
    fn snapshot(&mut self, candidates: &Labels) -> Option<Self::Snap> {
        let s = self.inner.snapshot(candidates);
        self.note(s.is_some());
        s
    }
    fn seen(&mut self, snap: &Self::Snap) -> Seen {
        self.inner.seen(snap)
    }
    fn verify(&mut self, candidates: &Labels, reviewers: &[String]) -> Option<Verified> {
        let v = self.inner.verify(candidates, reviewers);
        self.note(v.is_some());
        v
    }
    fn unclaimed(&mut self, snap: &Self::Snap) -> Labels {
        self.inner.unclaimed(snap)
    }
    fn owned(&mut self) -> Result<Option<Labels>, String> {
        self.inner.owned()
    }
    fn reown(&mut self, own: &Labels, disown: &Labels) -> Result<(), String> {
        self.inner.reown(own, disown)
    }
    fn capture_focus(&mut self, snap: &Self::Snap) -> Option<String> {
        self.inner.capture_focus(snap)
    }
    fn restore_focus(&mut self, id: &str) {
        self.inner.restore_focus(id)
    }
    fn add(&mut self, label: &str, snap: &Self::Snap) -> Option<String> {
        self.inner.add(label, snap)
    }
    fn is_placed(&mut self, reviewers: &[String], snap: &Self::Snap) -> bool {
        self.inner.is_placed(reviewers, snap)
    }
    fn compose(&mut self, master: &str, reviewers: &[String], snap: &Self::Snap) -> Option<String> {
        self.inner.compose(master, reviewers, snap)
    }
    fn override_layout(&mut self, kdl: &str, snap: &Self::Snap) -> bool {
        self.inner.override_layout(kdl, snap)
    }
    fn remove_all(&mut self, labels: &[String], snap: &Self::Snap) {
        self.inner.remove_all(labels, snap)
    }
    fn live_labels(&mut self, snap: &Self::Snap) -> std::collections::BTreeSet<String> {
        self.inner.live_labels(snap)
    }
    fn title_rows(&mut self, snap: &Self::Snap) -> Vec<(String, String)> {
        self.inner.title_rows(snap)
    }
    fn lost_labels(&mut self, snap: &Self::Snap) -> std::collections::BTreeSet<String> {
        self.inner.lost_labels(snap)
    }
    fn tab_of(&mut self, snap: &Self::Snap, caller: &str) -> Option<u32> {
        self.inner.tab_of(snap, caller)
    }
}

/// A message to the reconciliation worker. Roster views and glyph data
/// coalesce independently (latest of each per batch): a roster view
/// triggers a reconcile pass, glyph data a retitle pass. Reopen
/// requests are DATA — each names an agent the user chose — so they
/// are kept, in order, never folded.
pub(super) enum WorkerMsg {
    Roster(RosterView),
    Glyphs(StatusGlyphs),
    Reopen(String),
    /// Probe pane presence now — the loop asks when the agent panel
    /// or an agent's page is entered, so what it shows is current.
    Probe,
    /// The loop is gone.
    Close,
}

/// One batch of work for the worker, coalesced from the queue.
#[derive(Default)]
pub(super) struct WorkerBatch {
    pub(super) roster: Option<RosterView>,
    pub(super) glyphs: Option<StatusGlyphs>,
    pub(super) reopens: Vec<String>,
    /// List panes now: the loop asked.
    pub(super) probe: bool,
}

/// What the worker tells the loop. One channel, drained without
/// blocking before every paint.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Report {
    Reach(ZellijReach),
    /// This repo's labels with a live pane; `None` when the listing
    /// did not answer — unknown, which is not absent.
    Presence(Option<std::collections::BTreeSet<String>>),
    /// A change in the labels whose bound process outlived its pane.
    Lost(std::collections::BTreeSet<String>),
    Reopened(String, ReopenOutcome),
    /// A change in what the operator should be told about pane
    /// ownership.
    Ownership(Option<String>),
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
    lost: std::collections::BTreeSet<String>,
    /// The last roster view handled — cached (pane, label, role) rows
    /// are only trustworthy while the roster TARGET is unchanged.
    last_roster: Option<RosterView>,
    tab: TabMirror,
    /// The newest retitle data the loop sent.
    glyphs: Option<StatusGlyphs>,
    /// The glyphs the last answered listing retitled with. A refresh
    /// that brings the same glyphs again is no reason to list.
    listed_for: Option<StatusGlyphs>,
    /// The ownership notice last reported.
    ownership: Option<String>,
}

impl WorkerState {
    fn new(caller: Option<String>) -> Self {
        Self {
            reconciler: PaneReconciler::new(),
            panes: PaneStatus::new(),
            lost: Default::default(),
            last_roster: None,
            tab: TabMirror::new(caller),
            glyphs: None,
            listed_for: None,
            ownership: None,
        }
    }

    /// Pane titles may name pre-relocation roles: the next glyphs,
    /// equal or not, retitle off a fresh listing.
    fn invalidate(&mut self) {
        self.panes.invalidate();
        self.listed_for = None;
    }

    /// Returns what the loop should hear: each reopen's outcome in
    /// request order, then presence when the batch listed.
    ///
    /// Outside the reconcile pass a batch lists AT MOST ONCE: the
    /// retitler's map and presence are two projections of the same
    /// listing, taken when the batch is a probe or a refresh (a
    /// refresh retitles, and presence rides on that listing — the
    /// plan's "a refresh batch probes too"; codex on e310988 and
    /// 44e937e).
    #[cfg(test)]
    fn handle(
        &mut self,
        batch: WorkerBatch,
        io: &mut impl PaneIo,
        rename: impl FnMut(&str, &str),
    ) -> Vec<Report> {
        self.handle_with(batch, io, rename, &mut NoTabs)
    }

    /// [`Self::handle`] with the tab's zellij operations. The tab is
    /// reconciled on every batch that lists, a probe included, so a
    /// stale name is found without the repo changing.
    fn handle_with(
        &mut self,
        batch: WorkerBatch,
        io: &mut impl PaneIo,
        rename: impl FnMut(&str, &str),
        tabs: &mut impl TabIo,
    ) -> Vec<Report> {
        let WorkerBatch {
            roster,
            glyphs,
            reopens,
            probe,
        } = batch;
        if let Some(view) = roster {
            // Invalidate on any TARGET change, not just on our own
            // actions: another TUI may already have converged the live
            // layout (reconcile no-ops), yet our cached rows still
            // carry the previous roster's roles (codex afb6d43).
            let changed = self.last_roster.as_ref() != Some(&view);
            self.last_roster = Some(view.clone());
            let acted = self.reconciler.reconcile(view, io);
            if acted || changed {
                self.invalidate();
            }
        }
        let mut reports: Vec<Report> = reopens
            .into_iter()
            .map(|label| {
                let outcome = match &self.last_roster {
                    Some(view) => self.reconciler.reopen(view, &label, io),
                    None => ReopenOutcome::NotOnRoster,
                };
                if outcome == ReopenOutcome::Reopened {
                    self.invalidate();
                }
                Report::Reopened(label, outcome)
            })
            .collect();
        if let Some(g) = glyphs {
            self.tab.want(&g.tab);
            self.glyphs = Some(g);
        }
        let retitle = self.glyphs.is_some() && self.glyphs != self.listed_for;
        let listed = probe || retitle;
        let listing = if listed {
            let record = io.owned();
            let candidates = match &self.last_roster {
                Some(view) => ownership_candidates(view, &record),
                None => record.ok().flatten().unwrap_or_default(),
            };
            io.snapshot(&candidates)
        } else {
            None
        };
        if let (true, Some(g)) = (listed, &self.glyphs) {
            let rows = listing.as_ref().map(|snap| io.title_rows(snap));
            self.panes.update_with(g, rows, rename);
            if listing.is_some() {
                self.listed_for = Some(g.clone());
            }
        }
        if let Some(snap) = &listing {
            self.tab.reconcile(|caller| io.tab_of(snap, caller), tabs);
        }
        if listed {
            reports.push(Report::Presence(
                listing.as_ref().map(|snap| io.live_labels(snap)),
            ));
            let lost = listing
                .as_ref()
                .map(|snap| io.lost_labels(snap))
                .unwrap_or_default();
            if lost != self.lost {
                self.lost = lost.clone();
                reports.push(Report::Lost(lost));
            }
        }
        let ownership = self.reconciler.ownership_notice();
        if ownership != self.ownership {
            self.ownership = ownership.clone();
            reports.push(Report::Ownership(ownership));
        }
        reports
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
    report_rx: Option<std::sync::mpsc::Receiver<Report>>,
    /// Last observed; `NotInSession` is fixed for the process's life,
    /// since `$ZELLIJ` does not change under a running TUI.
    reach: ZellijReach,
    /// The last presence report; `None` until one arrives or when the
    /// last listing did not answer.
    presence: Option<std::collections::BTreeSet<String>>,
    lost: std::collections::BTreeSet<String>,
    /// Reopen answers not yet handed to the loop — the worker's, and
    /// those that needed no worker (there is none outside zellij).
    answered: Vec<(String, ReopenOutcome)>,
    /// The last ownership notice the worker reported.
    ownership: Option<String>,
}

impl ReconcileWorker {
    /// Spawns the worker — a no-op handle outside zellij (no thread,
    /// sends go nowhere). `wake` is called after a batch that produced
    /// a report, so the loop paints changed presence or lost-process
    /// marks without waiting for its next event.
    pub(super) fn spawn(repo: std::path::PathBuf, wake: impl Fn() + Send + 'static) -> Self {
        if !crate::cli::open_zellij::in_session() {
            return Self {
                tx: None,
                join: None,
                report_rx: None,
                reach: ZellijReach::NotInSession,
                presence: None,
                lost: Default::default(),
                answered: Vec::new(),
                ownership: None,
            };
        }
        let (tx, rx) = std::sync::mpsc::channel::<WorkerMsg>();
        let (report_tx, report_rx) = std::sync::mpsc::channel::<Report>();
        let mut worker = Self::in_session(report_rx);
        let join = std::thread::spawn(move || {
            let caller = crate::cli::open_zellij::caller_pane_id();
            let mut state = WorkerState::new(caller.clone());
            let mut tabs = ZellijTabIo;
            let io = ZellijPaneIo {
                repo: &repo,
                caller,
            };
            let seen = std::cell::Cell::new(None);
            let mut io = Observed {
                inner: io,
                seen: &seen,
            };
            serve(
                &rx,
                &mut state,
                &mut io,
                &mut tabs,
                rename_pane,
                |reports| {
                    // Only a batch that actually listed has anything to
                    // say about reach; a cached batch keeps the loop's
                    // last value.
                    if let Some(reach) = seen.take() {
                        let _ = report_tx.send(Report::Reach(reach));
                    }
                    let wake_loop = !reports.is_empty();
                    for report in reports {
                        let _ = report_tx.send(report);
                    }
                    if wake_loop {
                        wake();
                    }
                },
            );
        });
        worker.tx = Some(tx);
        worker.join = Some(join);
        worker
    }

    /// The in-session worker before any thread or report exists —
    /// the ONE place the initial reach is decided.
    ///
    /// Pure, so a test can exercise the real seed: a test that built
    /// its own struct literal proved nothing about `spawn`, and the
    /// mutation it claimed to catch stayed green (codex on c842901).
    /// `$ZELLIJ` being set proves a variable, not a client, so nothing
    /// has been observed yet.
    fn in_session(report_rx: std::sync::mpsc::Receiver<Report>) -> Self {
        Self {
            tx: None,
            join: None,
            report_rx: Some(report_rx),
            reach: ZellijReach::Unknown,
            presence: None,
            lost: Default::default(),
            answered: Vec::new(),
            ownership: None,
        }
    }

    /// Take everything the worker has reported, without blocking.
    fn drain(&mut self) {
        if let Some(rx) = &self.report_rx {
            while let Ok(r) = rx.try_recv() {
                match r {
                    Report::Reach(reach) => self.reach = reach,
                    Report::Presence(live) => self.presence = live,
                    Report::Lost(lost) => self.lost = lost,
                    Report::Reopened(label, outcome) => self.answered.push((label, outcome)),
                    Report::Ownership(notice) => self.ownership = notice,
                }
            }
        }
    }

    /// Whether `label` has a live pane, as of the last presence report.
    pub(super) fn presence_of(&mut self, label: &str) -> Presence {
        self.drain();
        match &self.presence {
            None => Presence::Unknown,
            Some(live) if live.contains(label) => Presence::Live,
            Some(_) => Presence::Missing,
        }
    }

    /// The last presence report, for the frame.
    pub(super) fn presence(&mut self) -> Option<std::collections::BTreeSet<String>> {
        self.drain();
        self.presence.clone()
    }

    pub(super) fn lost(&mut self) -> std::collections::BTreeSet<String> {
        self.drain();
        self.lost.clone()
    }

    /// What the operator should be told about pane ownership, as of the
    /// last report.
    pub(super) fn ownership_notice(&mut self) -> Option<String> {
        self.drain();
        self.ownership.clone()
    }

    /// Ask the worker to list panes now. Non-blocking.
    pub(super) fn probe(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(WorkerMsg::Probe);
        }
    }

    /// Ask the worker to bring back `label`'s pane. Non-blocking; the
    /// answer arrives through [`Self::outcomes`]. Outside zellij there
    /// is no worker and the answer is immediate.
    pub(super) fn reopen(&mut self, label: &str) {
        match &self.tx {
            Some(tx) => {
                let _ = tx.send(WorkerMsg::Reopen(label.to_string()));
            }
            None => self
                .answered
                .push((label.to_string(), ReopenOutcome::NotInSession)),
        }
    }

    /// Every reopen answered since the last call, in order. Drains
    /// without blocking.
    pub(super) fn outcomes(&mut self) -> Vec<(String, ReopenOutcome)> {
        self.drain();
        std::mem::take(&mut self.answered)
    }

    /// The last reachability the worker reported. Drains the channel
    /// without blocking, so the render loop never waits on zellij.
    pub(super) fn reach(&mut self) -> ZellijReach {
        self.drain();
        self.reach
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
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(WorkerMsg::Close);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// The TUI's one watch on the per-repo runtime directory, held for its
/// whole life, in or out of zellij: it hears this repo's `clank remote`
/// requests for the loop. Nothing else in the directory is read.
pub(super) struct RuntimeWatch {
    _watch: Option<crate::proc_identity::exit_watch::ExitWatch<()>>,
    /// Why there is no watch, when there is none.
    pub(super) unavailable: Option<String>,
}

impl RuntimeWatch {
    /// Arm the watch. Each new request id goes to `requests`, then `wake`.
    pub(super) fn start(
        repo: &std::path::Path,
        requests: std::sync::mpsc::Sender<String>,
        wake: impl Fn() + Send + 'static,
    ) -> Self {
        let dir = crate::cli::monitor::ensure_runtime_dir()
            .map(|()| crate::cli::monitor::runtime_dir(unsafe { libc::getuid() }));
        Self::start_in(repo, dir, requests, wake)
    }

    fn start_in(
        repo: &std::path::Path,
        dir: Result<std::path::PathBuf, String>,
        requests: std::sync::mpsc::Sender<String>,
        wake: impl Fn() + Send + 'static,
    ) -> Self {
        use crate::proc_identity::exit_watch::{ExitWatch, Wake};
        let mut requests_seen = RequestScan::new(repo);
        let scan_dir = dir.clone().ok();
        let sink = move |wake_: Wake<()>| {
            if let (Wake::Dir, Some(dir)) = (wake_, &scan_dir) {
                for id in requests_seen.new_ids(dir) {
                    let _ = requests.send(id);
                    wake();
                }
            }
        };
        let (watch, unavailable) = match dir {
            Ok(dir) => match ExitWatch::new(Some(&dir), sink) {
                Ok(watch) => (Some(watch), None),
                Err(e) => (None, Some(format!("cannot watch {}: {e}", dir.display()))),
            },
            Err(e) => (None, Some(format!("no runtime directory: {e}"))),
        };
        Self {
            _watch: watch,
            unavailable,
        }
    }
}

/// Which of this repo's request files are new since the last look.
struct RequestScan {
    repo: std::path::PathBuf,
    seen: std::collections::HashSet<String>,
}

impl RequestScan {
    fn new(repo: &std::path::Path) -> Self {
        Self {
            repo: repo.to_path_buf(),
            seen: Default::default(),
        }
    }

    fn new_ids(&mut self, dir: &std::path::Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let present: std::collections::HashSet<String> = entries
            .filter_map(Result::ok)
            .filter_map(|e| crate::cli::remote_control::request_id(&self.repo, &e.path()))
            .collect();
        // Forget ids whose files are gone, so the set stays the size of
        // the directory.
        self.seen.retain(|id| present.contains(id));
        let mut fresh: Vec<String> = present
            .into_iter()
            .filter(|id| self.seen.insert(id.clone()))
            .collect();
        fresh.sort();
        fresh
    }
}

/// The worker's life: every batch until the channel closes, then the
/// tab's base name back, so nothing pending can rename after it.
fn serve<I: PaneIo>(
    rx: &std::sync::mpsc::Receiver<WorkerMsg>,
    state: &mut WorkerState,
    io: &mut I,
    tabs: &mut impl TabIo,
    mut rename: impl FnMut(&str, &str),
    mut reports: impl FnMut(Vec<Report>),
) {
    worker_loop(rx, |batch| {
        reports(state.handle_with(batch, io, &mut rename, tabs))
    });
    state.tab.restore(tabs);
}

/// The worker's receive loop, generic over the batch handler so the
/// coalescing contract is testable without zellij
/// (tui-reconcile-off-loop): messages QUEUED before a batch starts
/// collapse to the newest OF EACH KIND; arrivals DURING a batch
/// collapse into exactly one follow-up batch. Exits on `Close` or
/// disconnect, after at most one final drained batch. Nothing wakes it
/// but a message: an idle worker costs zellij nothing.
fn worker_loop(rx: &std::sync::mpsc::Receiver<WorkerMsg>, mut batch: impl FnMut(WorkerBatch)) {
    loop {
        let mut b = WorkerBatch::default();
        let mut closing = false;
        let mut take = |m: WorkerMsg, b: &mut WorkerBatch| match m {
            WorkerMsg::Roster(v) => b.roster = Some(v),
            WorkerMsg::Glyphs(g) => b.glyphs = Some(g),
            WorkerMsg::Reopen(label) => b.reopens.push(label),
            WorkerMsg::Probe => b.probe = true,
            WorkerMsg::Close => closing = true,
        };
        match rx.recv() {
            Ok(first) => take(first, &mut b),
            Err(_) => return,
        }
        while let Ok(m) = rx.try_recv() {
            take(m, &mut b);
        }
        batch(b);
        if closing {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::status_tui::fixtures::{plan_state, reviewer_missing, snap};
    use clank_core::plan_view::WaitingOn;

    #[test]
    fn lost_requires_a_bound_running_session_without_a_pane() {
        use std::collections::BTreeSet;
        let bound = vec![
            ("claude".to_string(), Tool::Claude, "c1".to_string()),
            ("codex".to_string(), Tool::Codex, "x1".to_string()),
            ("exited".to_string(), Tool::Codex, "x2".to_string()),
        ];
        let set = |items: &[&str]| {
            items
                .iter()
                .map(|s| (*s).to_string())
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(
            lost_bound_labels(&bound, &set(&["codex"]), &set(&["c1", "x1"]), &set(&[])),
            set(&["claude"]),
            "a live process with no pane is lost; a shown or exited one is not",
        );
        assert_eq!(
            lost_bound_labels(&bound, &set(&[]), &set(&[]), &set(&["c1"])),
            set(&["claude"]),
            "Claude's background session listing is also positive process evidence",
        );
    }

    #[test]
    fn session_token_for_lost_pane_check_requires_a_session() {
        assert_eq!(session_token_on("codex app-server ZELLIJ_PANE_ID=1"), None);
        assert_eq!(
            session_token_on("opencode --session ses_0af1b2c3d4e5f607"),
            Some("ses_0af1b2c3d4e5f607"),
        );
    }

    // ── zellij-is-the-workspace: reachability ──────────────────

    /// Outside a session the worker never spawns and reach is fixed:
    /// `$ZELLIJ` does not change under a running TUI.
    #[test]
    fn reach_is_not_in_session_when_there_is_no_worker() {
        let mut w = ReconcileWorker {
            tx: None,
            join: None,
            report_rx: None,
            reach: ZellijReach::NotInSession,
            presence: None,
            lost: Default::default(),
            answered: Vec::new(),
            ownership: None,
        };
        assert_eq!(w.reach(), ZellijReach::NotInSession);
    }

    /// The loop reads the LATEST report and never blocks: several
    /// batches may have run between two frames, and only the last one
    /// describes the present.
    #[test]
    fn reach_drains_to_the_latest_report_without_blocking() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut w = ReconcileWorker {
            tx: None,
            join: None,
            report_rx: Some(rx),
            reach: ZellijReach::Connected,
            presence: None,
            lost: Default::default(),
            answered: Vec::new(),
            ownership: None,
        };
        // Nothing reported yet: the last value stands, and this must
        // not wait for a report that is never coming.
        assert_eq!(w.reach(), ZellijReach::Connected);
        tx.send(Report::Reach(ZellijReach::Unreachable)).unwrap();
        tx.send(Report::Reach(ZellijReach::Connected)).unwrap();
        tx.send(Report::Reach(ZellijReach::Unreachable)).unwrap();
        assert_eq!(w.reach(), ZellijReach::Unreachable, "the latest wins");
        // A disconnected sender is not an error; the last value stands.
        drop(tx);
        assert_eq!(w.reach(), ZellijReach::Unreachable);
    }

    /// The indicator adds NO listing. Reachability is read off the
    /// listings a batch already performs; the first draft appended an
    /// unconditional `snapshot()` after `handle`, which turned the
    /// converged steady state from zero zellij calls into one per
    /// batch (codex on d855068). This pins the count.
    #[test]
    fn observing_reach_adds_no_snapshot() {
        let seen = std::cell::Cell::new(None);
        let inner = FakeIo::new(vec![
            Some(live(&[("claude", true)])),
            Some(live(&[("claude", true)])),
        ]);
        let mut io = Observed { inner, seen: &seen };
        let mut state = WorkerState::new(None);
        let view = view(&["claude"], Some("claude"));

        // First batch: not converged, so the reconciler lists once.
        state.handle(wb(Some(view.clone()), None, Vec::new()), &mut io, |_, _| {});
        assert_eq!(
            io.inner.snapshots_taken, 1,
            "the reconcile listing, and only it"
        );
        assert_eq!(
            seen.get(),
            Some(ZellijReach::Connected),
            "read off that listing"
        );

        // Second batch, same roster: converged, ZERO listings — and
        // therefore no observation, so the loop keeps its last value.
        seen.set(None);
        state.handle(wb(Some(view), None, Vec::new()), &mut io, |_, _| {});
        assert_eq!(
            io.inner.snapshots_taken, 1,
            "a converged batch lists nothing"
        );
        assert_eq!(seen.get(), None, "nothing listed, nothing observed");
    }

    /// Before the first listing returns, nothing may claim the client
    /// is there. `$ZELLIJ` set is a variable, not an answer, and a
    /// dead session leaves it set.
    #[test]
    fn a_fresh_worker_reports_unknown_not_connected() {
        // Through the PRODUCTION initializer, not a literal of our
        // own: only then does re-seeding `spawn` to `Connected`
        // fail here.
        let (_tx, rx) = std::sync::mpsc::channel::<Report>();
        let mut w = ReconcileWorker::in_session(rx);
        assert_eq!(w.reach(), ZellijReach::Unknown, "no evidence, no claim");
        assert_ne!(w.reach(), ZellijReach::Connected);
    }

    /// A listing that fails is what `Unreachable` MEANS.
    #[test]
    fn a_failed_listing_reads_as_unreachable() {
        let seen = std::cell::Cell::new(None);
        let inner = FakeIo::new(vec![None]);
        let mut io = Observed { inner, seen: &seen };
        let mut state = WorkerState::new(None);
        state.handle(
            wb(Some(view(&["claude"], Some("claude"))), None, Vec::new()),
            &mut io,
            |_, _| {},
        );
        assert_eq!(io.inner.snapshots_taken, 1);
        assert_eq!(seen.get(), Some(ZellijReach::Unreachable));
    }

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

    /// A listing of `pairs` with no ambiguous pane.
    fn clear(pairs: Vec<(String, bool)>) -> Seen {
        Seen {
            pairs,
            ambiguous: Vec::new(),
        }
    }

    #[test]
    fn plan_adds_missing_members_and_removes_stale_panes() {
        // Live panes lag the roster in both directions: `new` has no
        // pane yet, `gone` has a pane but left the roster.
        let plan = plan_panes(
            &view(&["claude", "codex", "new"], Some("claude")),
            &clear(live(&[("claude", true), ("codex", false), ("gone", false)])),
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
            &clear(live(&[("codex", false), ("claude", true)])),
        );
        assert!(plan.is_converged());
    }

    #[test]
    fn plan_detects_a_master_swap_from_the_stage_alone() {
        // codex a730882 concern 2: promote ran while NO TUI was open —
        // the roster says codex, the stage still holds claude. A fresh
        // reconciler must see the re-layout from the geometry.
        let plan = plan_panes(
            &view(&["claude", "codex"], Some("codex")),
            &clear(live(&[("claude", true), ("codex", false)])),
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
        // the plan still names the re-layout, or the new master
        // converges in the stack.
        let plan = plan_panes(
            &view(&["codex"], Some("codex")),
            &clear(live(&[("claude", true), ("codex", false)])),
        );
        assert_eq!(plan.remove, vec!["claude".to_string()]);
        assert_eq!(
            plan.relocate,
            Some(("codex".to_string(), Some("claude".to_string())))
        );
    }

    #[test]
    fn plan_stages_the_master_when_nothing_is_staged() {
        // codex c7be87f: nothing on the stage (e.g. a whole-team
        // replacement mid-convergence) — the roster master must still
        // be staged, with no stale occupant to name.
        let plan = plan_panes(
            &view(&["claude", "codex"], Some("claude")),
            &clear(live(&[("claude", false), ("codex", false)])),
        );
        assert_eq!(plan.relocate, Some(("claude".to_string(), None)));
        assert!(!plan.is_converged());
    }

    #[test]
    fn plan_relays_when_a_stale_pane_shares_the_stage() {
        // codex ae6338a: two panes reading as staged (a tie the
        // geometry read should never produce, but a plan must not
        // trust that). Whichever order they list in, the stale one is
        // named — and until then the layout must not verify.
        for lv in [
            live(&[("codex", true), ("claude", true)]),
            live(&[("claude", true), ("codex", true)]),
        ] {
            let plan = plan_panes(&view(&["claude", "codex"], Some("codex")), &clear(lv));
            assert_eq!(
                plan.relocate,
                Some(("codex".to_string(), Some("claude".to_string()))),
                "the stale occupant is named regardless of listing order"
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
            &clear(live(&[
                ("claude", true),
                ("codex", false),
                ("codex", false),
            ])),
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
        /// Labels whose panes are all EXITED: listed by `pairs`, absent
        /// from `live_labels`.
        dead: std::collections::BTreeSet<String>,
        lost: std::collections::BTreeSet<String>,
        /// Scripted retitler rows per listing; default derives them from
        /// the snapshot's pairs as `terminal_<label>`.
        title_rows: std::collections::VecDeque<Vec<(String, String)>>,
        placed_results: std::collections::VecDeque<bool>,
        verify_placed: std::collections::VecDeque<bool>,
        /// Every layout handed to `override_layout`, composed by the
        /// REAL composition so tests read the KDL `clank open` would.
        overrides: Vec<String>,
        override_results: std::collections::VecDeque<bool>,
        /// The home whose `~/.clank/config.json` supplies a template;
        /// `None` composes the built-in.
        home: Option<std::path::PathBuf>,
        orientation: crate::cli::open_zellij::Orientation,
        compose_fails: bool,
        /// The tab each listing names for the caller's pane; once these
        /// run out, `owner`.
        owners: std::collections::VecDeque<Option<u32>>,
        owner: Option<u32>,
        /// The ownership record as `owned` reads it.
        record: Result<Option<Labels>, String>,
        /// Every `reown` call, as `(own, disown)`.
        reowns: Vec<(Labels, Labels)>,
    }

    /// The repo every fake composes for. Composition needs a path
    /// string for the launch commands and a basename for the tab.
    const FAKE_REPO: &str = "/tmp/fake-repo";

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
                dead: std::collections::BTreeSet::new(),
                lost: std::collections::BTreeSet::new(),
                title_rows: std::collections::VecDeque::new(),
                placed_results: std::collections::VecDeque::new(),
                verify_placed: std::collections::VecDeque::new(),
                overrides: Vec::new(),
                override_results: std::collections::VecDeque::new(),
                home: None,
                orientation: crate::cli::open_zellij::Orientation::Landscape,
                compose_fails: false,
                owners: std::collections::VecDeque::new(),
                owner: None,
                record: Ok(Some(Labels::new())),
                reowns: Vec::new(),
            }
        }

        /// The launch command a layout slot must carry for `label`.
        fn launch(label: &str) -> String {
            crate::cli::open_zellij::agent_start_command(label, FAKE_REPO)
        }
    }

    impl PaneIo for FakeIo {
        type Snap = Vec<(String, bool)>;
        fn snapshot(&mut self, _: &Labels) -> Option<Self::Snap> {
            self.snapshots_taken += 1;
            self.snaps.pop_front().unwrap_or(None)
        }
        fn seen(&mut self, snap: &Self::Snap) -> Seen {
            clear(snap.clone())
        }
        fn verify(&mut self, _: &Labels, _reviewers: &[String]) -> Option<Verified> {
            self.verifies_taken += 1;
            let placed = self.verify_placed.pop_front().unwrap_or(true);
            self.verifies
                .pop_front()
                .unwrap_or(None)
                .map(|pairs| Verified {
                    seen: clear(pairs),
                    placed,
                })
        }
        fn unclaimed(&mut self, _: &Self::Snap) -> Labels {
            Labels::new()
        }
        fn owned(&mut self) -> Result<Option<Labels>, String> {
            self.record.clone()
        }
        fn reown(&mut self, own: &Labels, disown: &Labels) -> Result<(), String> {
            self.reowns.push((own.clone(), disown.clone()));
            let mut now = self.record.clone()?.unwrap_or_default();
            now.extend(own.iter().cloned());
            now.retain(|l| !disown.contains(l));
            self.record = Ok(Some(now));
            Ok(())
        }
        fn capture_focus(&mut self, _snap: &Self::Snap) -> Option<String> {
            self.focus_captures += 1;
            Some("user_pane".to_string())
        }
        fn restore_focus(&mut self, id: &str) {
            self.log.push(format!("focus {id}"));
        }
        fn add(&mut self, label: &str, _snap: &Self::Snap) -> Option<String> {
            self.log.push(format!("add {label}"));
            // A freshly opened pane is running, whatever its
            // predecessor was.
            self.dead.remove(label);
            self.add_results
                .pop_front()
                .unwrap_or(true)
                .then(|| format!("terminal_{label}"))
        }
        fn is_placed(&mut self, _revs: &[String], _snap: &Self::Snap) -> bool {
            self.placed_results.pop_front().unwrap_or(true)
        }
        fn compose(
            &mut self,
            master: &str,
            reviewers: &[String],
            _snap: &Self::Snap,
        ) -> Option<String> {
            self.log
                .push(format!("compose {master} [{}]", reviewers.join(",")));
            if self.compose_fails {
                return None;
            }
            crate::cli::open_zellij::compose_live_layout(
                std::path::Path::new(FAKE_REPO),
                self.home.as_deref(),
                master,
                reviewers,
                self.orientation,
            )
            .ok()
        }
        fn override_layout(&mut self, kdl: &str, _snap: &Self::Snap) -> bool {
            self.log.push("override".to_string());
            self.overrides.push(kdl.to_string());
            self.override_results.pop_front().unwrap_or(true)
        }
        fn remove_all(&mut self, labels: &[String], _snap: &Self::Snap) {
            self.log.push(format!("remove {}", labels.join("+")));
        }
        fn live_labels(&mut self, snap: &Self::Snap) -> std::collections::BTreeSet<String> {
            snap.iter()
                .map(|(l, _)| l.clone())
                .filter(|l| !self.dead.contains(l))
                .collect()
        }
        fn title_rows(&mut self, snap: &Self::Snap) -> Vec<(String, String)> {
            if let Some(rows) = self.title_rows.pop_front() {
                return rows;
            }
            snap.iter()
                .map(|(l, _)| (format!("terminal_{l}"), l.clone()))
                .collect()
        }
        fn lost_labels(&mut self, _snap: &Self::Snap) -> std::collections::BTreeSet<String> {
            self.lost.clone()
        }
        fn tab_of(&mut self, _snap: &Self::Snap, caller: &str) -> Option<u32> {
            assert_eq!(caller, "terminal_5", "only the TUI's own pane is looked up");
            self.owners.pop_front().unwrap_or(self.owner)
        }
    }

    /// A session's tabs. Each rename is logged; scripted outcomes say
    /// whether zellij accepts it and whether an accepted one applies.
    struct FakeTabs {
        tabs: std::collections::BTreeMap<u32, String>,
        listings: std::collections::VecDeque<bool>,
        accepts: std::collections::VecDeque<bool>,
        applies: std::collections::VecDeque<bool>,
        renames: Vec<(u32, String)>,
    }

    impl FakeTabs {
        /// The TUI's tab 1 and the user's tab 2.
        fn two() -> Self {
            Self {
                tabs: [(1, "anxiety-attack".to_string()), (2, "other".to_string())].into(),
                listings: Default::default(),
                accepts: Default::default(),
                applies: Default::default(),
                renames: Vec::new(),
            }
        }

        fn name(&self, id: u32) -> &str {
            &self.tabs[&id]
        }
    }

    impl TabIo for FakeTabs {
        fn tabs(&mut self) -> Option<Vec<(u32, String)>> {
            self.listings
                .pop_front()
                .unwrap_or(true)
                .then(|| self.tabs.iter().map(|(i, n)| (*i, n.clone())).collect())
        }
        fn rename(&mut self, id: u32, name: &str) -> bool {
            self.renames.push((id, name.to_string()));
            let accepted = self.accepts.pop_front().unwrap_or(true);
            if accepted && self.applies.pop_front().unwrap_or(true) {
                self.tabs.insert(id, name.to_string());
            }
            accepted
        }
    }

    fn mirror() -> TabMirror {
        let mut m = TabMirror::new(Some("terminal_5".to_string()));
        m.want("🔨");
        m
    }

    /// The tab holding the TUI's pane is the one named, even with the
    /// user on another tab from the first look; that other tab is never
    /// touched, then or when the mirror restores on exit.
    #[test]
    fn the_mirror_names_only_the_tab_holding_its_pane() {
        let mut tabs = FakeTabs::two();
        let mut m = mirror();
        m.reconcile(|_| Some(1), &mut tabs);
        assert_eq!(tabs.name(1), "🔨 anxiety-attack");
        m.restore(&mut tabs);
        assert_eq!(tabs.name(1), "anxiety-attack");
        assert!(
            tabs.renames.iter().all(|(id, _)| *id == 1),
            "{:?}",
            tabs.renames
        );
        assert_eq!(tabs.name(2), "other");
    }

    /// A rename zellij refused, or accepted without applying, is tried
    /// again with nothing new wanted; a name something else changed is
    /// put back; a right name is left alone.
    #[test]
    fn the_mirror_converges_on_the_tabs_actual_name() {
        let mut tabs = FakeTabs::two();
        tabs.accepts = [false].into();
        tabs.applies = [false].into();
        let mut m = mirror();
        m.reconcile(|_| Some(1), &mut tabs);
        assert_eq!(tabs.name(1), "anxiety-attack", "refused");
        m.reconcile(|_| Some(1), &mut tabs);
        assert_eq!(tabs.name(1), "anxiety-attack", "accepted, not applied");
        m.reconcile(|_| Some(1), &mut tabs);
        assert_eq!(tabs.name(1), "🔨 anxiety-attack");
        let before = tabs.renames.len();
        m.reconcile(|_| Some(1), &mut tabs);
        assert_eq!(tabs.renames.len(), before, "already right");
        tabs.tabs.insert(1, "renamed by hand".to_string());
        m.reconcile(|_| Some(1), &mut tabs);
        assert_eq!(tabs.name(1), "🔨 anxiety-attack");
    }

    /// An unanswered or empty-handed look is unknown: nothing is renamed
    /// and nothing is recorded, and a later good look converges.
    #[test]
    fn the_mirror_does_nothing_on_what_it_cannot_see() {
        let mut tabs = FakeTabs::two();
        tabs.listings = [false, true, false].into();
        let mut m = mirror();
        m.reconcile(|_| None, &mut tabs);
        m.reconcile(|_| Some(1), &mut tabs);
        assert!(tabs.renames.is_empty(), "no owner, then no listing");
        m.reconcile(|_| Some(1), &mut tabs);
        assert_eq!(tabs.name(1), "🔨 anxiety-attack", "owned and named");
        tabs.tabs.insert(1, "drifted".to_string());
        let before = tabs.renames.len();
        m.reconcile(|_| Some(1), &mut tabs);
        assert_eq!(tabs.renames.len(), before, "the listing failed");
        m.reconcile(|_| Some(1), &mut tabs);
        assert_eq!(tabs.name(1), "🔨 anxiety-attack");

        let mut unlisted = FakeTabs::two();
        let mut m = mirror();
        m.reconcile(|_| Some(9), &mut unlisted);
        assert!(
            unlisted.renames.is_empty(),
            "an owner the tab listing lacks"
        );
        m.reconcile(|_| Some(1), &mut unlisted);
        assert_eq!(unlisted.name(1), "🔨 anxiety-attack", "proved later");

        let mut tabs = FakeTabs::two();
        let mut nobody = TabMirror::new(None);
        nobody.want("🔨");
        nobody.reconcile(|_| Some(1), &mut tabs);
        nobody.restore(&mut tabs);
        assert!(tabs.renames.is_empty(), "no pane of its own, no mirror");
    }

    /// Once its tab is gone the mirror names no other, however the
    /// listings go on to answer.
    #[test]
    fn a_vanished_tab_is_never_replaced() {
        let mut tabs = FakeTabs::two();
        let mut m = mirror();
        m.reconcile(|_| Some(1), &mut tabs);
        tabs.tabs.remove(&1);
        let before = tabs.renames.len();
        m.reconcile(|_| Some(2), &mut tabs);
        m.restore(&mut tabs);
        assert_eq!(tabs.renames.len(), before + 1, "only the restore, by id");
        assert_eq!(tabs.renames.last().unwrap().0, 1);
        assert_eq!(tabs.name(2), "other");
    }

    /// Exit restores only a tab a rename ever reached.
    #[test]
    fn exit_restores_only_after_an_accepted_rename() {
        let mut tabs = FakeTabs::two();
        tabs.accepts = [false, false].into();
        let mut m = mirror();
        m.reconcile(|_| Some(1), &mut tabs);
        m.reconcile(|_| Some(1), &mut tabs);
        m.restore(&mut tabs);
        assert_eq!(tabs.renames.len(), 2, "two refused renames, no restore");
    }

    /// Through the worker: the first glyphs reach the tab, a refused
    /// rename is retried by a probe alone, and the worker's
    /// last act, after the final queued batch, is the restore.
    #[test]
    fn the_worker_keeps_the_tab_right_between_refreshes() {
        let mut state = WorkerState::new(Some("terminal_5".to_string()));
        let mut io = FakeIo::new(vec![Some(Vec::new()); 3]);
        io.owner = Some(1);
        let mut tabs = FakeTabs::two();
        tabs.accepts = [false].into();
        let mut first = glyphs(&[]);
        first.tab = "🔨".to_string();
        state.handle_with(
            wb(None, Some(first), Vec::new()),
            &mut io,
            |_, _| {},
            &mut tabs,
        );
        assert_eq!(tabs.name(1), "anxiety-attack", "refused");
        let probe = WorkerBatch {
            probe: true,
            ..Default::default()
        };
        state.handle_with(probe, &mut io, |_, _| {}, &mut tabs);
        assert_eq!(tabs.name(1), "🔨 anxiety-attack", "the probe retried it");

        let (tx, rx) = std::sync::mpsc::channel();
        let mut later = glyphs(&[]);
        later.tab = "💤".to_string();
        tx.send(WorkerMsg::Glyphs(later)).unwrap();
        drop(tx);
        serve(&rx, &mut state, &mut io, &mut tabs, |_, _| {}, |_| {});
        let tail: Vec<&str> = tabs
            .renames
            .iter()
            .rev()
            .take(2)
            .map(|(_, n)| n.as_str())
            .collect();
        assert_eq!(
            tail,
            ["anxiety-attack", "💤 anxiety-attack"],
            "the pending batch, then the restore"
        );
    }

    /// Every pane command in the layout, parsed rather than grepped,
    /// as `(is_stage, command)` — the stage being a `pane` at
    /// `size="65%"`, which the tab layout and both swap variants each
    /// carry once.
    fn pane_commands(kdl: &str) -> Vec<(bool, String)> {
        let doc: kdl::KdlDocument = kdl.parse().expect("composed layout parses");
        let mut out = Vec::new();
        fn walk(nodes: &[kdl::KdlNode], out: &mut Vec<(bool, String)>) {
            for n in nodes {
                if n.name().value() == "pane"
                    && let Some(children) = n.children()
                    && let Some(cmd) = children.get_arg("command").and_then(|v| v.as_string())
                {
                    let stage = n.get("size").and_then(|e| e.value().as_string()) == Some("65%");
                    let args: Vec<String> = children
                        .get_args("args")
                        .into_iter()
                        .filter_map(|v| v.as_string().map(str::to_string))
                        .collect();
                    out.push((stage, format!("{cmd} {}", args.join(" "))));
                }
                if let Some(children) = n.children() {
                    walk(children.nodes(), out);
                }
            }
        }
        walk(doc.nodes(), &mut out);
        out
    }

    fn stage_commands(kdl: &str) -> Vec<String> {
        pane_commands(kdl)
            .into_iter()
            .filter_map(|(stage, c)| stage.then_some(c))
            .collect()
    }

    /// Whether the layout launches `label` anywhere.
    fn launches(kdl: &str, label: &str) -> bool {
        pane_commands(kdl)
            .iter()
            .any(|(_, c)| c == &FakeIo::launch(label))
    }

    /// Glyph data as `StatusGlyphs::of` would build it for a roster
    /// whose master is `master`: every label's full pane title.
    fn glyphs_for(master: &str, rows: &[(&str, &str, &str)]) -> StatusGlyphs {
        StatusGlyphs {
            titles: rows
                .iter()
                .map(|(label, as_master, as_reviewer)| {
                    let (emoji, role) = if *label == master {
                        (as_master, "master")
                    } else {
                        (as_reviewer, "reviewer")
                    };
                    (
                        label.to_string(),
                        format!("{emoji} {}", agent_pane_title(label)),
                    )
                })
                .collect(),
            tab: String::new(),
        }
    }

    /// The common case: `claude` is master.
    fn glyphs(rows: &[(&str, &str, &str)]) -> StatusGlyphs {
        glyphs_for("claude", rows)
    }

    fn converged_worker(
        labels: &[&str],
        master: &str,
        live_now: Vec<(String, bool)>,
    ) -> WorkerState {
        let mut state = WorkerState::new(None);
        let mut io = FakeIo::new(vec![Some(live_now)]);
        state.handle(
            wb(Some(view(labels, Some(master))), None, Vec::new()),
            &mut io,
            |_, _| {},
        );
        assert!(io.log.is_empty(), "converged from the start: {:?}", io.log);
        state
    }

    fn reopen(state: &mut WorkerState, io: &mut FakeIo, label: &str) -> ReopenOutcome {
        let out = state.handle(wb(None, None, vec![label.to_string()]), io, |_, _| {});
        assert_eq!(out.len(), 1, "one request, one answer, nothing probed");
        match &out[0] {
            Report::Reopened(l, outcome) if l == label => outcome.clone(),
            other => panic!("expected {label}'s outcome, got {other:?}"),
        }
    }

    /// A batch as the loop hands it over, nothing probed.
    fn wb(
        roster: Option<RosterView>,
        glyphs: Option<StatusGlyphs>,
        reopens: Vec<String>,
    ) -> WorkerBatch {
        WorkerBatch {
            roster,
            glyphs,
            reopens,
            ..Default::default()
        }
    }

    fn roster_snap(labels: &[(&str, bool)]) -> StatusSnapshot {
        // Reuse the shared snapshot fixture; only labels + master matter
        // to RosterView.
        let mut s = snap(None, vec![]);
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
                auto_mode_explicit: None,
                tool: "claude".into(),
                invocation: "claude".into(),
                session: None,
                waiting_for_you: None,
                state: None,
                asking: None,
                repeated: None,

                attending: None,
            })
            .collect();
        s.master = labels
            .iter()
            .find(|(_, is)| *is)
            .map(|(l, _)| l.to_string());
        s
    }

    /// The roster the REAL swap writes is what drives convergence.
    ///
    /// This is the positive half of sole pane ownership: the incoming
    /// agent reaches a pane only on the reconcile that follows, and
    /// only because `apply_swap` wrote it to `.clank/config.json`.
    /// The negative half — that the swap performs no pane work of its
    /// own — is NOT asserted here: `apply_swap` takes no `PaneIo`, so
    /// watching an io it never receives would be tautology (codex on
    /// 5b8988f). Its absence from the signature is the enforcement,
    /// with `zellij_ownership_boundary` covering the API it could
    /// otherwise reach around it for.
    #[test]
    fn a_swap_reaches_the_panes_only_through_the_reconciler() {
        use crate::cli::status_tui::fixtures::{cand, detail_repo, library_home, two_agent_snap};
        use crate::cli::teams_config::RosterRole;

        let repo = detail_repo();
        let home = library_home(&["scout"]);
        let mut snapshot = two_agent_snap();
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(
            vec![
                Some(live(&[("claude", true), ("codex", false)])),
                Some(live(&[("claude", true), ("codex", false)])),
            ],
            vec![
                Some(live(&[("claude", true), ("codex", false)])),
                Some(live(&[("claude", true), ("scout", false)])),
            ],
        );
        r.reconcile(RosterView::of(&snapshot), &mut io);
        let settled = io.log.len();

        let picker = vec![cand("scout", "codex", "codex")];
        let mut err = None;
        super::super::apply_swap(
            1,
            0,
            &mut snapshot,
            &picker,
            repo.path(),
            Some(home.path()),
            &mut err,
        );
        assert!(err.is_none(), "the swap itself succeeded: {err:?}");

        // The roster the swap WROTE is what the reconciler reacts to;
        // building it by hand here would test nothing about the swap.
        let cfg = crate::agent_store::load_repo_config_required(repo.path()).unwrap();
        snapshot.agents = cfg
            .agents
            .iter()
            .map(|(l, a)| {
                super::super::fixtures::agent_row(
                    l.as_str(),
                    a.role,
                    clank_core::vocab::AutoMode::On,
                )
            })
            .collect();
        snapshot.master = cfg
            .agents
            .iter()
            .find(|(_, a)| a.role == RosterRole::Master)
            .map(|(l, _)| l.as_str().to_string());
        assert!(
            snapshot.agents.iter().any(|a| a.label == "scout"),
            "precondition: the swap put scout on the roster"
        );

        r.reconcile(RosterView::of(&snapshot), &mut io);
        let after = &io.log[settled..];
        assert!(
            after.iter().any(|l| l.contains("scout")),
            "the reconciler alone opens the incoming pane: {after:?}"
        );
    }

    // ── the-tui-knows-whether-a-pane-is-open: presence ──────────

    fn presence_of(reports: &[Report]) -> Option<Option<Vec<String>>> {
        reports.iter().find_map(|r| match r {
            Report::Presence(live) => Some(live.as_ref().map(|s| s.iter().cloned().collect())),
            _ => None,
        })
    }

    // ── the reconcile pass: membership, then the layout, then the read-back ──
    // (placement-is-a-layout-applied-not-panes-shuffled)

    fn assert_stage(kdl: &str, label: &str) {
        let stages = stage_commands(kdl);
        assert!(
            !stages.is_empty() && stages.iter().all(|c| c == &FakeIo::launch(label)),
            "every stage slot — tab and both swap variants — pins `{label}`: {stages:?}"
        );
    }

    /// The report: promote codex, and the layout the tab receives —
    /// the one alt+[ / alt+] will apply from now on — stages codex in
    /// its tab layout AND in both swap variants. Claude is named
    /// nowhere as a stage; it is a `children` occupant.
    #[test]
    fn a_promotion_re_lays_the_tab_with_the_new_master_on_every_stage() {
        let snap = roster_snap(&[("codex", true), ("claude", false)]);
        let mut r = PaneReconciler::new();
        let before = live(&[("claude", true), ("codex", false)]);
        let after = live(&[("claude", false), ("codex", true)]);
        let mut io = FakeIo::with_verify(vec![Some(before)], vec![Some(after)]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(
            io.log,
            vec!["compose codex [claude]", "override", "focus user_pane"],
            "no shuffling: one layout, applied"
        );
        let kdl = &io.overrides[0];
        assert_stage(kdl, "codex");
        assert_eq!(
            kdl.matches("swap_tiled_layout").count(),
            2,
            "both orientations travel with the override"
        );
        assert!(
            launches(kdl, "claude"),
            "claude is still launched by the tab layout, in the stack"
        );
        assert!(r.converged.is_some());
    }

    /// One listing, one verify, one focus transaction: the cost of a
    /// promotion is unchanged by the model (zellij-one-listing-per-pass).
    #[test]
    fn promote_shaped_pass_takes_one_snapshot_one_verify_one_focus() {
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
    }

    #[test]
    fn converged_at_start_pass_takes_one_snapshot_and_nothing_else() {
        // The pass-start listing IS ground truth when the plan is
        // empty and the tab is placed: no verify, no focus, no ops —
        // and no override, which would move focus for nothing.
        let snap = roster_snap(&[("claude", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::new(vec![Some(live(&[("claude", true), ("codex", false)]))]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(
            (io.snapshots_taken, io.verifies_taken, io.focus_captures),
            (1, 0, 0)
        );
        assert!(io.log.is_empty());
        assert!(io.overrides.is_empty());
        assert!(r.converged.is_some());
    }

    /// A missing label is opened BEFORE the layout is composed, the
    /// listing is taken again, and the layout names only what that
    /// listing shows running — the duplicate-spawn measured on 0.45
    /// (a slot with no live match) is unreachable by construction.
    #[test]
    fn an_add_is_opened_then_listed_then_named_by_the_layout() {
        let snap = roster_snap(&[("claude", true), ("r1", false), ("r2", false)]);
        let mut r = PaneReconciler::new();
        let before = live(&[("claude", true)]);
        // The re-list after the add shows r1 up; r2 never appeared.
        let relisted = live(&[("claude", true), ("r1", false)]);
        let mut io = FakeIo::with_verify(vec![Some(before), Some(relisted)], vec![None]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(
            io.log,
            vec![
                "add r1",
                "add r2",
                "compose claude [r1]",
                "override",
                "focus user_pane"
            ]
        );
        assert_eq!(io.snapshots_taken, 2, "re-listed after the adds");
        let kdl = &io.overrides[0];
        assert!(launches(kdl, "r1"));
        assert!(
            !launches(kdl, "r2"),
            "a label the re-list did not show is not a slot: {kdl}"
        );
    }

    /// Departed labels are closed BEFORE the layout, and the layout
    /// does not name them.
    #[test]
    fn a_departed_label_is_closed_before_the_layout_and_not_named() {
        let snap = roster_snap(&[("claude", true), ("r1", false)]);
        let before = live(&[("claude", true), ("r1", false), ("gone", false)]);
        let relisted = live(&[("claude", true), ("r1", false)]);
        let after = relisted.clone();
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(before), Some(relisted)], vec![Some(after)]);
        io.placed_results = vec![false].into();
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(
            io.log,
            vec![
                "remove gone",
                "compose claude [r1]",
                "override",
                "focus user_pane"
            ]
        );
        assert!(!launches(&io.overrides[0], "gone"));
    }

    /// An exited reviewer is neither closed, nor reopened, nor named:
    /// it stays with its ✗ for the operator's reopen. An exited MASTER
    /// means no override at all — a layout without a stage is a
    /// re-layout for nothing.
    #[test]
    fn an_exited_pane_is_left_alone_and_left_out_of_the_layout() {
        // Exited reviewer: membership sees it (no add, no remove), the
        // layout omits it.
        let snap = roster_snap(&[("claude", true), ("r1", false), ("r2", false)]);
        let all = live(&[("claude", true), ("r1", false), ("r2", false)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(all.clone())], vec![Some(all.clone())]);
        io.dead.insert("r2".into());
        io.placed_results = vec![false].into();
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(
            io.log,
            vec!["compose claude [r1]", "override", "focus user_pane"],
            "no add, no remove for the corpse"
        );
        assert!(!launches(&io.overrides[0], "r2"));

        // Exited master: membership only, no override issued.
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(all.clone())], vec![Some(all)]);
        io.dead.insert("claude".into());
        io.placed_results = vec![false].into();
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(io.log, vec!["focus user_pane"]);
        assert!(io.overrides.is_empty(), "no stage to give: no override");
    }

    /// The orientation the layout is composed at is the tab's, so a
    /// user who flipped with alt+[ stays flipped.
    #[test]
    fn the_layout_keeps_the_tabs_orientation() {
        let snap = roster_snap(&[("codex", true), ("claude", false)]);
        let before = live(&[("claude", true), ("codex", false)]);
        for (orientation, first_split) in [
            (
                crate::cli::open_zellij::Orientation::Landscape,
                "split_direction=\"vertical\"",
            ),
            (
                crate::cli::open_zellij::Orientation::Portrait,
                "split_direction=\"horizontal\"",
            ),
        ] {
            let mut r = PaneReconciler::new();
            let mut io = FakeIo::with_verify(vec![Some(before.clone())], vec![None]);
            io.orientation = orientation;
            r.reconcile(RosterView::of(&snap), &mut io);
            let kdl = &io.overrides[0];
            let tab = kdl.split("swap_tiled_layout").next().unwrap();
            assert!(
                tab.contains(first_split),
                "{orientation:?} main layout opens with {first_split}: {tab}"
            );
        }
    }

    /// A configured marker template is what a roster change applies:
    /// the user's chrome, the new master at the marker's stage, and no
    /// generated swap variants — never the built-in layout.
    #[test]
    fn a_roster_change_under_a_user_template_applies_that_template() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".clank")).unwrap();
        std::fs::write(
            home.path().join(".clank/config.json"),
            serde_json::json!({
                "zellij": {
                    "layout": "layout {\n    tab name=\"USER-CHROME\" {\n        pane size=1 borderless=true {\n            plugin location=\"zellij:tab-bar\"\n        }\n        clank_agents\n    }\n}\n"
                }
            })
            .to_string(),
        )
        .unwrap();
        let snap = roster_snap(&[("codex", true), ("claude", false)]);
        let before = live(&[("claude", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(before)], vec![None]);
        io.home = Some(home.path().to_path_buf());
        r.reconcile(RosterView::of(&snap), &mut io);
        let kdl = &io.overrides[0];
        assert!(
            kdl.contains("USER-CHROME"),
            "the template's own chrome: {kdl}"
        );
        assert_stage(kdl, "codex");
        assert!(
            !kdl.contains("swap_tiled_layout"),
            "a template carries no generated swaps: {kdl}"
        );
        assert!(
            !kdl.contains("default_tab_template"),
            "and never the built-in's chrome: {kdl}"
        );
    }

    /// The failure budget covers the WHOLE transaction: an override
    /// the tab refuses, or a read-back that never confirms, spends it,
    /// and at the limit the roster is left alone — no override on the
    /// next refresh — until the roster changes.
    #[test]
    fn a_failing_override_is_bounded_and_a_roster_change_resets_the_budget() {
        let snap = roster_snap(&[("claude", true), ("r1", false), ("r2", false)]);
        let all = live(&[("claude", true), ("r1", false), ("r2", false)]);
        let passes = 8;
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(all.clone()); passes], vec![None; passes]);
        io.placed_results = vec![false; passes].into();
        io.override_results = vec![false; passes].into();
        for _ in 0..passes {
            r.reconcile(RosterView::of(&snap), &mut io);
        }
        let overrides = io.log.iter().filter(|l| l.as_str() == "override").count();
        assert_eq!(
            overrides, MAX_FAILED_PASSES as usize,
            "the budget bounds the overrides; {passes} refreshes issued {overrides}"
        );
        assert!(r.converged.is_some(), "left alone");

        // A roster change is a new budget.
        let grown = roster_snap(&[
            ("claude", true),
            ("r1", false),
            ("r2", false),
            ("r3", false),
        ]);
        let mut io = FakeIo::with_verify(vec![Some(all.clone()), Some(all)], vec![None]);
        io.placed_results = vec![false].into();
        r.reconcile(RosterView::of(&grown), &mut io);
        assert!(
            io.log.iter().any(|l| l == "override"),
            "the new roster is acted on: {:?}",
            io.log
        );
    }

    /// A pass whose actions did not stick stays unconverged and the
    /// next refresh retries — up to the budget.
    #[test]
    fn reconciler_stays_unconverged_when_actions_did_not_stick() {
        let snap = roster_snap(&[("claude", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let before = live(&[("claude", true)]);
        let after = live(&[("claude", true)]);
        let mut io = FakeIo::with_verify(vec![Some(before)], vec![Some(after)]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert!(
            r.converged.is_none(),
            "not converged: the add did not stick"
        );
    }

    /// A failed listing touches nothing and stays unconverged; the
    /// next refresh retries and verifies.
    #[test]
    fn reconciler_retries_after_a_failed_listing_and_verifies_convergence() {
        let snap = roster_snap(&[("claude", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(
            vec![
                None,
                Some(live(&[("claude", true)])),
                Some(live(&[("claude", true), ("codex", false)])),
            ],
            vec![Some(live(&[("claude", true), ("codex", false)]))],
        );
        assert!(!r.reconcile(RosterView::of(&snap), &mut io));
        assert!(io.log.is_empty() && r.converged.is_none());
        assert!(r.reconcile(RosterView::of(&snap), &mut io));
        assert!(io.log.iter().any(|l| l == "add codex"));
        assert!(r.converged.is_some());
    }

    /// The pane on the stage must be the roster master for the read-back
    /// to converge: a re-layout that did not stick leaves the new master
    /// in the stack, and that is not converged.
    #[test]
    fn reconciler_rejects_a_master_in_the_stack_at_verify() {
        let snap = roster_snap(&[("new-master", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let before = live(&[("old", true), ("codex", false)]);
        let after = live(&[("new-master", false), ("codex", false)]);
        let mut io =
            FakeIo::with_verify(vec![Some(before.clone()), Some(before)], vec![Some(after)]);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(r.converged, None, "master in the stack is not converged");
    }

    /// Placement is part of the outcome for every pass: a remove-only
    /// pass whose read-back says the reviewers are not stacked does
    /// not converge.
    #[test]
    fn a_remove_only_pass_still_answers_for_placement() {
        let snap = roster_snap(&[("claude", true), ("r1", false)]);
        let before = live(&[("claude", true), ("r1", false), ("gone", false)]);
        let after = live(&[("claude", true), ("r1", false)]);
        let mut r = PaneReconciler::new();
        let mut io =
            FakeIo::with_verify(vec![Some(before), Some(after.clone())], vec![Some(after)]);
        io.verify_placed = vec![false].into();
        r.reconcile(RosterView::of(&snap), &mut io);
        assert!(io.log.iter().any(|l| l.starts_with("remove")));
        assert!(r.converged.is_none());
    }

    /// A tab whose labels are all present but not placed is re-laid,
    /// not cached.
    #[test]
    fn an_unplaced_tab_is_re_laid_instead_of_cached() {
        let snap = roster_snap(&[("claude", true), ("r1", false), ("r2", false)]);
        let all = live(&[("claude", true), ("r1", false), ("r2", false)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::with_verify(vec![Some(all.clone())], vec![Some(all)]);
        io.placed_results = vec![false].into();
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(
            io.log,
            vec!["compose claude [r1,r2]", "override", "focus user_pane"]
        );
        assert!(r.converged.is_some(), "the read-back confirmed it");
    }

    /// A pane created this pass is believed live until a listing shows
    /// it, so a lagging listing cannot open a second one.
    #[test]
    fn reconciler_does_not_reopen_a_pane_whose_listing_has_not_caught_up() {
        let snap = roster_snap(&[("claude", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let stale = live(&[("claude", true)]);
        let mut io = FakeIo::with_verify(
            vec![
                Some(stale.clone()),
                Some(stale.clone()),
                Some(stale.clone()),
            ],
            vec![Some(stale.clone()), Some(stale)],
        );
        r.reconcile(RosterView::of(&snap), &mut io);
        r.reconcile(RosterView::of(&snap), &mut io);
        assert_eq!(
            io.log.iter().filter(|l| l.as_str() == "add codex").count(),
            1,
            "believed live, so not opened twice: {:?}",
            io.log
        );
    }

    #[test]
    fn a_confirmed_creation_stops_being_believed() {
        let snap = roster_snap(&[("claude", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let both = live(&[("claude", true), ("codex", false)]);
        let mut io = FakeIo::with_verify(
            vec![Some(live(&[("claude", true)])), Some(both.clone())],
            vec![Some(both)],
        );
        r.reconcile(RosterView::of(&snap), &mut io);
        assert!(r.pending.is_empty(), "the verify listed it: belief retired");
    }

    /// The memory MUST expire: a `new-pane` that reported an id which
    /// never reaches a listing would otherwise block its label forever.
    /// Each pass grows the roster so none is skipped as converged; the
    /// listing never catches up.
    #[test]
    fn a_creation_that_never_appears_stops_blocking_its_label() {
        let mut r = PaneReconciler::new();
        let stale = live(&[("claude", true)]);
        let mut adds = 0;
        for extra in [Vec::new(), vec!["ruthless"], vec!["kimi", "ruthless"]] {
            let mut labels = vec![("claude", true), ("codex", false)];
            labels.extend(extra.iter().map(|l| (*l, false)));
            let snap = roster_snap(&labels);
            let mut io = FakeIo::with_verify(
                vec![Some(stale.clone()), Some(stale.clone())],
                vec![Some(stale.clone())],
            );
            r.reconcile(RosterView::of(&snap), &mut io);
            adds += io.log.iter().filter(|l| *l == "add codex").count();
        }
        assert_eq!(
            adds, 2,
            "codex is retried once the belief expires, not stranded"
        );
    }

    // ── reopen: one label, then the layout ──

    #[test]
    fn reopening_one_of_two_missing_panes_adds_only_that_one() {
        let snap = roster_snap(&[("claude", true), ("r1", false), ("r2", false)]);
        let mut r = PaneReconciler::new();
        let before = live(&[("claude", true)]);
        let relisted = live(&[("claude", true), ("r1", false)]);
        let mut io = FakeIo::with_verify(
            vec![Some(before), Some(relisted.clone())],
            vec![Some(relisted)],
        );
        let out = r.reopen(&RosterView::of(&snap), "r1", &mut io);
        assert_eq!(out, ReopenOutcome::Reopened);
        assert_eq!(
            io.log,
            vec![
                "add r1",
                "compose claude [r1]",
                "override",
                "focus user_pane"
            ],
            "r2 is not launched by r1's reopen"
        );
        assert!(!launches(&io.overrides[0], "r2"));
    }

    #[test]
    fn reopening_the_master_adds_and_stages_it_by_the_same_override() {
        let snap = roster_snap(&[("claude", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let before = live(&[("codex", false)]);
        let relisted = live(&[("codex", false), ("claude", false)]);
        let after = live(&[("codex", false), ("claude", true)]);
        let mut io = FakeIo::with_verify(vec![Some(before), Some(relisted)], vec![Some(after)]);
        assert_eq!(
            r.reopen(&RosterView::of(&snap), "claude", &mut io),
            ReopenOutcome::Reopened
        );
        assert_stage(&io.overrides[0], "claude");
    }

    #[test]
    fn a_failed_add_places_nothing() {
        let snap = roster_snap(&[("claude", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::new(vec![Some(live(&[("claude", true)]))]);
        io.add_results = vec![false].into();
        assert_eq!(
            r.reopen(&RosterView::of(&snap), "codex", &mut io),
            ReopenOutcome::NotCreated
        );
        assert!(io.overrides.is_empty(), "nothing to place, nothing re-laid");
    }

    #[test]
    fn reopened_is_the_read_back_not_the_creation() {
        let snap = roster_snap(&[("claude", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let before = live(&[("claude", true)]);
        let relisted = live(&[("claude", true), ("codex", false)]);
        // The read-back says the reviewer is NOT in the stack.
        let mut io = FakeIo::with_verify(
            vec![Some(before), Some(relisted.clone())],
            vec![Some(relisted)],
        );
        io.verify_placed = vec![false].into();
        assert_eq!(
            r.reopen(&RosterView::of(&snap), "codex", &mut io),
            ReopenOutcome::Unplaced
        );
    }

    #[test]
    fn reopening_a_pane_that_exists_adds_nothing() {
        let snap = roster_snap(&[("claude", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::new(vec![Some(live(&[("claude", true), ("codex", false)]))]);
        assert_eq!(
            r.reopen(&RosterView::of(&snap), "codex", &mut io),
            ReopenOutcome::AlreadyOpen
        );
        assert!(io.log.is_empty());
    }

    #[test]
    fn a_reopen_leaves_the_converged_cache_alone() {
        let snap = roster_snap(&[("claude", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let both = live(&[("claude", true), ("codex", false)]);
        let mut io = FakeIo::new(vec![Some(both.clone())]);
        r.reconcile(RosterView::of(&snap), &mut io);
        let cached = r.converged.clone();
        assert!(cached.is_some());
        let mut io = FakeIo::with_verify(
            vec![Some(live(&[("claude", true)])), Some(both.clone())],
            vec![Some(both)],
        );
        r.reopen(&RosterView::of(&snap), "codex", &mut io);
        assert_eq!(r.converged, cached, "a reopen never touches the cache");
    }

    #[test]
    fn a_reopen_reports_an_unanswered_listing_and_an_unmade_pane() {
        let snap = roster_snap(&[("claude", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let mut io = FakeIo::new(vec![None]);
        assert_eq!(
            r.reopen(&RosterView::of(&snap), "codex", &mut io),
            ReopenOutcome::NoListing
        );
        assert_eq!(
            r.reopen(&RosterView::of(&snap), "nobody", &mut io),
            ReopenOutcome::NotOnRoster
        );
    }

    /// A pane the reconciler made, believed live, then closed by hand:
    /// the first ask reopens it, because the verify that listed it
    /// retired the belief (codex on 1bf604d).
    #[test]
    fn a_pane_the_reconciler_made_and_the_user_closed_reopens_on_the_first_ask() {
        let snap = roster_snap(&[("claude", true), ("codex", false)]);
        let mut r = PaneReconciler::new();
        let both = live(&[("claude", true), ("codex", false)]);
        let mut io = FakeIo::with_verify(
            vec![Some(live(&[("claude", true)])), Some(both.clone())],
            vec![Some(both.clone())],
        );
        r.reconcile(RosterView::of(&snap), &mut io);
        assert!(r.pending.is_empty());
        // Hand-closed: the listing shows claude alone again.
        let mut io = FakeIo::with_verify(
            vec![Some(live(&[("claude", true)])), Some(both.clone())],
            vec![Some(both)],
        );
        assert_eq!(
            r.reopen(&RosterView::of(&snap), "codex", &mut io),
            ReopenOutcome::Reopened
        );
    }

    // ── retitles follow the roster ──

    /// After a promotion the retitler stamps the NEW roles from the
    /// roster's glyph data, off a fresh listing — nothing is read from
    /// the panes' old titles.
    #[test]
    fn retitles_after_a_reconcile_follow_the_roster() {
        let mut state = WorkerState::new(None);
        let before = live(&[("claude", true), ("codex", false)]);
        let after = live(&[("claude", false), ("codex", true)]);
        let mut io =
            FakeIo::with_verify(vec![Some(before), Some(after.clone())], vec![Some(after)]);
        let renames = std::cell::RefCell::new(Vec::<(String, String)>::new());
        let promoted = roster_snap(&[("codex", true), ("claude", false)]);
        state.handle(
            wb(
                Some(RosterView::of(&promoted)),
                Some(glyphs_for(
                    "codex",
                    &[("claude", "M", "R"), ("codex", "M", "R")],
                )),
                Vec::new(),
            ),
            &mut io,
            |id, title| renames.borrow_mut().push((id.into(), title.into())),
        );
        let mut got = renames.borrow().clone();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("terminal_claude".to_string(), "R claude".to_string()),
                ("terminal_codex".to_string(), "M codex".to_string()),
            ]
        );
    }

    /// A refresh whose listing fails after an acting reconcile renames
    /// nothing: the rows were invalidated and there is nothing fresh
    /// to stamp from (codex afb6d43).
    #[test]
    fn failed_re_list_after_an_acting_reconcile_renames_nothing() {
        let mut state = WorkerState::new(None);
        let before = live(&[("claude", true), ("codex", false)]);
        let after = live(&[("claude", false), ("codex", true)]);
        // Reconcile lists once; the retitle listing (the batch's second
        // snapshot) fails.
        let mut io = FakeIo::with_verify(vec![Some(before), None], vec![Some(after)]);
        let renames = std::cell::RefCell::new(Vec::<(String, String)>::new());
        let promoted = roster_snap(&[("codex", true), ("claude", false)]);
        state.handle(
            wb(
                Some(RosterView::of(&promoted)),
                Some(glyphs_for(
                    "codex",
                    &[("claude", "M", "R"), ("codex", "M", "R")],
                )),
                Vec::new(),
            ),
            &mut io,
            |id, title| renames.borrow_mut().push((id.into(), title.into())),
        );
        assert!(renames.borrow().is_empty(), "{:?}", renames.borrow());
    }

    /// A roster change invalidates the cached rows even when another
    /// TUI already converged the tab and this reconcile is a no-op.
    #[test]
    fn roster_change_invalidates_even_when_another_tui_already_converged() {
        let mut state = WorkerState::new(None);
        let both = live(&[("claude", true), ("codex", false)]);
        let renames = std::cell::RefCell::new(Vec::<(String, String)>::new());
        let first = roster_snap(&[("claude", true), ("codex", false)]);
        let mut io = FakeIo::new(vec![Some(both.clone()), Some(both.clone())]);
        state.handle(
            wb(
                Some(RosterView::of(&first)),
                Some(glyphs(&[("claude", "M", "R"), ("codex", "M", "R")])),
                Vec::new(),
            ),
            &mut io,
            |id, title| renames.borrow_mut().push((id.into(), title.into())),
        );
        let n = renames.borrow().len();
        assert_eq!(n, 2);
        // Another TUI promoted codex and converged the tab: this
        // reconcile finds it placed (no-op), but the roles changed.
        let promoted = roster_snap(&[("codex", true), ("claude", false)]);
        let staged = live(&[("claude", false), ("codex", true)]);
        let mut io = FakeIo::new(vec![Some(staged.clone()), Some(staged)]);
        state.handle(
            wb(
                Some(RosterView::of(&promoted)),
                Some(glyphs_for(
                    "codex",
                    &[("claude", "M", "R"), ("codex", "M", "R")],
                )),
                Vec::new(),
            ),
            &mut io,
            |id, title| renames.borrow_mut().push((id.into(), title.into())),
        );
        assert_eq!(renames.borrow().len(), n + 2, "both panes restamped");
    }

    /// A retitle that must list and a probe in the same batch share
    /// one listing.
    #[test]
    fn a_retitle_that_must_list_and_a_probe_share_one_listing() {
        let mut state = WorkerState::new(None);
        let both = live(&[("claude", true), ("codex", false)]);
        let mut io = FakeIo::new(vec![Some(both)]);
        let reports = state.handle(
            WorkerBatch {
                roster: None,
                glyphs: Some(glyphs(&[("claude", "M", "R"), ("codex", "M", "R")])),
                reopens: Vec::new(),
                probe: true,
                ..Default::default()
            },
            &mut io,
            |_, _| {},
        );
        assert_eq!(io.snapshots_taken, 1);
        assert!(matches!(reports.as_slice(), [Report::Presence(Some(_))]));
    }

    #[test]
    fn a_probe_request_and_a_refresh_each_list_once_and_a_bare_reopen_does_not() {
        let mut state = WorkerState::new(None);
        let mut io = FakeIo::new(vec![Some(live(&[("claude", true), ("r1", false)]))]);
        let reports = state.handle(
            WorkerBatch {
                probe: true,
                ..Default::default()
            },
            &mut io,
            |_, _| {},
        );
        assert_eq!(io.snapshots_taken, 1);
        assert_eq!(
            presence_of(&reports),
            Some(Some(vec!["claude".into(), "r1".into()]))
        );
        // A refresh carries glyphs; it lists anyway, and the presence
        // rides on that.
        let mut io = FakeIo::new(vec![Some(live(&[("claude", true)]))]);
        let refresh = glyphs(&[("claude", "M", "R")]);
        let reports = state.handle(wb(None, Some(refresh), Vec::new()), &mut io, |_, _| {});
        assert_eq!(presence_of(&reports), Some(Some(vec!["claude".into()])));
        // The same glyphs again are no reason to list.
        let mut io = FakeIo::new(vec![Some(live(&[("claude", true), ("r1", false)]))]);
        let reports = state.handle(
            wb(None, Some(glyphs(&[("claude", "M", "R")])), Vec::new()),
            &mut io,
            |_, _| {},
        );
        assert_eq!(io.snapshots_taken, 0);
        assert!(reports.is_empty());
        // Changed glyphs are, and presence rides on that listing.
        let reports = state.handle(
            wb(None, Some(glyphs(&[("claude", "W", "R")])), Vec::new()),
            &mut io,
            |_, _| {},
        );
        assert_eq!(io.snapshots_taken, 1);
        assert_eq!(
            presence_of(&reports),
            Some(Some(vec!["claude".into(), "r1".into()]))
        );
        // Nothing asked: nothing listed, nothing said.
        let mut io = FakeIo::new(vec![Some(live(&[("claude", true)]))]);
        let reports = state.handle(wb(None, None, Vec::new()), &mut io, |_, _| {});
        assert_eq!(io.snapshots_taken, 0);
        assert_eq!(presence_of(&reports), None);
    }

    #[test]
    fn presence_is_live_panes_only() {
        let mut state = WorkerState::new(None);
        let mut io = FakeIo::new(vec![Some(live(&[
            ("claude", true),
            ("r1", false),
            ("r2", false),
        ]))]);
        io.dead.insert("r2".into());
        let reports = state.handle(
            WorkerBatch {
                probe: true,
                ..Default::default()
            },
            &mut io,
            |_, _| {},
        );
        assert_eq!(
            presence_of(&reports),
            Some(Some(vec!["claude".into(), "r1".into()])),
            "an exited pane's label is not present"
        );
        // A listing that does not answer is unknown, not empty.
        let mut io = FakeIo::new(vec![None]);
        let reports = state.handle(
            WorkerBatch {
                probe: true,
                ..Default::default()
            },
            &mut io,
            |_, _| {},
        );
        assert_eq!(presence_of(&reports), Some(None));
    }

    #[test]
    fn a_probe_reports_lost_processes_without_changing_panes() {
        let mut state = WorkerState::new(None);
        let mut io = FakeIo::new(vec![Some(Vec::new()), Some(Vec::new())]);
        io.lost.insert("codex".to_string());
        let batch = || WorkerBatch {
            probe: true,
            ..Default::default()
        };
        let first = state.handle(batch(), &mut io, |_, _| {});
        assert!(first.contains(&Report::Lost(["codex".to_string()].into())));
        assert!(io.log.is_empty(), "a lost mark has no pane effects");

        io.lost.clear();
        let second = state.handle(batch(), &mut io, |_, _| {});
        assert!(second.contains(&Report::Lost(Default::default())));
        assert!(io.log.is_empty(), "clearing the mark has no pane effects");
    }

    #[test]
    fn a_hand_close_shows_as_missing_and_offers_reopen_with_nothing_else_moving() {
        // Two probes, no roster or refresh between them, the live set
        // shrinks: the mark goes ▶ → ? and the page GAINS its reopen
        // row. Then the pane is back: ? → ▶ and the row is gone. Both
        // directions, so this cannot pass with the row hidden
        // throughout (codex on 580150a).
        use super::super::input::{DetailAction, detail_actions};
        use super::super::render::auto_mark;
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::AutoMode;
        let (report_tx, report_rx) = std::sync::mpsc::channel::<Report>();
        let mut w = ReconcileWorker::in_session(report_rx);
        let mut state = WorkerState::new(None);
        let probe = || WorkerBatch {
            probe: true,
            ..Default::default()
        };
        let mut io = FakeIo::new(vec![Some(live(&[("claude", true), ("r1", false)]))]);
        for r in state.handle(probe(), &mut io, |_, _| {}) {
            report_tx.send(r).unwrap();
        }
        assert_eq!(w.presence_of("r1"), Presence::Live);
        assert_eq!(auto_mark(AutoMode::On, w.presence_of("r1")).1.trim(), "▶");
        assert!(
            !detail_actions(RosterRole::Commit, w.presence_of("r1"), false)
                .contains(&DetailAction::Reopen)
        );

        // The user closes r1's pane. Nothing else changes.
        let mut io = FakeIo::new(vec![Some(live(&[("claude", true)]))]);
        for r in state.handle(probe(), &mut io, |_, _| {}) {
            report_tx.send(r).unwrap();
        }
        assert_eq!(w.presence_of("r1"), Presence::Missing);
        assert_eq!(auto_mark(AutoMode::On, w.presence_of("r1")).1.trim(), "?");
        assert!(
            detail_actions(RosterRole::Commit, w.presence_of("r1"), false)
                .contains(&DetailAction::Reopen)
        );

        // Back again.
        let mut io = FakeIo::new(vec![Some(live(&[("claude", true), ("r1", false)]))]);
        for r in state.handle(probe(), &mut io, |_, _| {}) {
            report_tx.send(r).unwrap();
        }
        assert_eq!(w.presence_of("r1"), Presence::Live);
        assert_eq!(auto_mark(AutoMode::On, w.presence_of("r1")).1.trim(), "▶");
        assert!(
            !detail_actions(RosterRole::Commit, w.presence_of("r1"), false)
                .contains(&DetailAction::Reopen)
        );
    }

    #[test]
    fn a_dead_pane_is_closed_and_reopened_a_live_one_is_already_open() {
        // zellij keeps a pane open after its process ends; to the
        // reconciler it is still the label's pane, to the user the
        // agent is plainly not open.
        let all = live(&[("claude", true), ("r1", false), ("r2", false)]);
        let mut state = converged_worker(&["claude", "r1", "r2"], "claude", all.clone());
        let mut io = FakeIo::with_verify(
            vec![
                Some(all.clone()),
                Some(live(&[("claude", true), ("r1", false)])),
                Some(all.clone()),
            ],
            vec![Some(all.clone())],
        );
        io.dead.insert("r2".into());
        assert_eq!(reopen(&mut state, &mut io, "r2"), ReopenOutcome::Reopened);
        assert_eq!(
            io.log,
            vec![
                "remove r2",
                "add r2",
                "compose claude [r1,r2]",
                "override",
                "focus user_pane"
            ],
            "the corpse is closed, a fresh listing, the add, a fresh listing, the layout"
        );
        assert_eq!(
            io.snapshots_taken, 3,
            "listed after the close so add does not find the corpse, and after the add so the layout names it"
        );
        // Live: nothing closed.
        let mut state = converged_worker(&["claude", "r1", "r2"], "claude", all.clone());
        let mut io = FakeIo::new(vec![Some(all)]);
        assert_eq!(
            reopen(&mut state, &mut io, "r2"),
            ReopenOutcome::AlreadyOpen
        );
        assert!(io.log.is_empty(), "{:?}", io.log);
    }

    #[test]
    fn reopen_requests_are_delivered_in_order_never_folded() {
        // Two different labels queued before a batch are two answers;
        // folding to the latest would drop one the user asked for.
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(WorkerMsg::Reopen("r1".into())).unwrap();
        tx.send(WorkerMsg::Reopen("r2".into())).unwrap();
        tx.send(WorkerMsg::Reopen("r1".into())).unwrap();
        drop(tx);
        let mut batches = Vec::new();
        worker_loop(&rx, |WorkerBatch { reopens, .. }| batches.push(reopens));
        assert_eq!(batches, vec![vec!["r1", "r2", "r1"]]);
    }

    #[test]
    fn outside_zellij_a_reopen_is_answered_at_once() {
        let mut w = ReconcileWorker {
            tx: None,
            join: None,
            report_rx: None,
            reach: ZellijReach::NotInSession,
            presence: None,
            lost: Default::default(),
            answered: Vec::new(),
            ownership: None,
        };
        w.reopen("r1");
        assert_eq!(
            w.outcomes(),
            vec![("r1".to_string(), ReopenOutcome::NotInSession)]
        );
        assert!(w.outcomes().is_empty(), "drained");
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
            worker_loop(&rx, move |WorkerBatch { roster, .. }| {
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
        let named = |label: &str| glyphs(&[(label, "M", "R")]);
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(WorkerMsg::Roster(view(&["a"], None))).unwrap();
        tx.send(WorkerMsg::Glyphs(named("old"))).unwrap();
        tx.send(WorkerMsg::Roster(view(&["b"], None))).unwrap();
        tx.send(WorkerMsg::Glyphs(named("new"))).unwrap();
        drop(tx);
        let log: Arc<Mutex<Vec<(Option<RosterView>, Option<Vec<String>>)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let log2 = log.clone();
        let join = std::thread::spawn(move || {
            worker_loop(&rx, move |WorkerBatch { roster, glyphs, .. }| {
                log2.lock()
                    .unwrap()
                    .push((roster, glyphs.map(|g| g.titles.keys().cloned().collect())));
            });
        });
        join.join().expect("worker exits when the channel closes");
        assert_eq!(
            log.lock().unwrap().as_slice(),
            &[(Some(view(&["b"], None)), Some(vec!["new".to_string()]))],
            "one batch, newest of each kind"
        );
    }

    /// The retitler's map, from a listing as zellij reports it: the
    /// workspace's panes whose names read as a candidate, in any form a
    /// build wrote. A plugin, a pane in another tab, and a name that is
    /// no candidate are left out.
    #[test]
    fn the_retitlers_map_is_the_workspaces_named_panes() {
        let panes: Vec<crate::cli::open_zellij::ZellijPane> = serde_json::from_str(
            r#"[
              {"id":0,"is_plugin":true,"title":"(.) - zellij:link","tab_id":0},
              {"id":0,"title":"claude (master)","terminal_command":"clank agent start claude --repo /r","tab_id":0},
              {"id":1,"title":"👀 codex (reviewer)","tab_id":0},
              {"id":2,"title":"","terminal_command":"clank status --repo /r --tui","tab_id":0},
              {"id":3,"title":"ruthless","tab_id":1},
              {"id":4,"title":"zsh","tab_id":0}
            ]"#,
        )
        .unwrap();
        let ws = crate::cli::open_zellij::Workspace::anchored(
            &panes,
            std::path::Path::new("/r"),
            ["claude", "codex", "ruthless"].map(String::from).into(),
        )
        .unwrap();
        assert_eq!(
            crate::cli::open_zellij::agent_panes_by_title(&panes, &ws),
            vec![
                ("terminal_0".to_string(), "claude".to_string()),
                ("terminal_1".to_string(), "codex".to_string()),
            ],
        );
    }

    /// A role change rewrites the glyph and nothing else, so the title
    /// still names the same agent.
    #[test]
    fn a_role_change_rewrites_only_the_glyph() {
        let mut s = crate::cli::status_tui::fixtures::with_agents(
            crate::cli::status_tui::fixtures::snap(
                Some(crate::cli::status_tui::fixtures::plan_state(
                    "p",
                    crate::cli::status_tui::fixtures::reviewer_missing("codex"),
                )),
                vec![],
            ),
            &[
                ("claude", crate::cli::teams_config::RosterRole::Master),
                ("codex", crate::cli::teams_config::RosterRole::Commit),
            ],
        );
        let title = |s: &StatusSnapshot| StatusGlyphs::of(s).titles["codex"].clone();
        let reviewing = title(&s);
        s.agents[1].role = crate::cli::teams_config::RosterRole::Standby;
        let standby = title(&s);
        assert_eq!(
            (reviewing.as_str(), standby.as_str()),
            ("👀 codex", "💤 codex")
        );
        let roster: Labels = ["claude", "codex"].map(String::from).into();
        for t in [&reviewing, &standby] {
            assert_eq!(
                crate::cli::open_zellij::resolve_title(t, &roster),
                crate::cli::open_zellij::PaneIdentity::Agent("codex".into())
            );
        }
    }

    #[test]
    fn pane_status_renames_only_on_change() {
        // A pane is renamed only when its emoji actually changes: the
        // rows arrive fresh with every refresh's listing, and the dedup
        // is what keeps that from being a rename per refresh.
        use std::cell::RefCell;

        let rows = vec![
            ("0".to_string(), "claude".to_string()),
            ("1".to_string(), "codex".to_string()),
        ];
        let renames = RefCell::new(Vec::<(String, String)>::new());
        let mut ps = PaneStatus::new();
        let go = |ps: &mut PaneStatus, s: &StatusSnapshot| {
            ps.update_with(&StatusGlyphs::of(s), Some(rows.clone()), |id, title| {
                renames
                    .borrow_mut()
                    .push((id.to_string(), title.to_string()))
            });
        };

        // Titles are the ROSTER's (StatusGlyphs::of) — glyph and role
        // both — so the fixture must carry the team: claude master,
        // codex commit.
        let with_team = |mut s: StatusSnapshot| -> StatusSnapshot {
            s.agents = vec![
                crate::cli::status::AgentAutoRow {
                    label: "claude".into(),
                    role: crate::cli::teams_config::RosterRole::Master,
                    auto_mode: clank_core::vocab::AutoMode::On,
                    auto_mode_explicit: None,
                    tool: "claude".into(),
                    invocation: "claude".into(),
                    session: None,
                    waiting_for_you: None,
                    state: None,
                    asking: None,
                    repeated: None,

                    attending: None,
                },
                crate::cli::status::AgentAutoRow {
                    label: "codex".into(),
                    role: crate::cli::teams_config::RosterRole::Commit,
                    auto_mode: clank_core::vocab::AutoMode::On,
                    auto_mode_explicit: None,
                    tool: "codex".into(),
                    invocation: "codex".into(),
                    session: None,
                    waiting_for_you: None,
                    state: None,
                    asking: None,
                    repeated: None,

                    attending: None,
                },
            ];
            s.master = Some("claude".into());
            s
        };

        // First refresh: both agent panes get a title.
        let awaited = with_team(snap(
            Some(plan_state("p", reviewer_missing("codex"))),
            vec![],
        ));
        go(&mut ps, &awaited);
        let after_first = renames.borrow().len();
        assert_eq!(after_first, 2, "both panes renamed on first refresh");

        // Same snapshot, many refreshes: NO renames.
        for _ in 0..5 {
            go(&mut ps, &awaited);
        }
        assert_eq!(
            renames.borrow().len(),
            after_first,
            "unchanged emoji => no rename"
        );

        // Master's turn instead: the plan's wait state toggles BOTH
        // emojis at once — master 💤→🔨 and codex 👀→💤 — so two panes
        // rename.
        let idle = with_team(snap(
            Some(plan_state("p", WaitingOn::MasterToContinue)),
            vec![],
        ));
        go(&mut ps, &idle);
        assert_eq!(
            renames.borrow().len(),
            after_first + 2,
            "both flipped panes renamed"
        );

        // A refresh whose listing failed stamps off the rows it has.
        ps.update_with(&StatusGlyphs::of(&idle), None, |id, title| {
            renames
                .borrow_mut()
                .push((id.to_string(), title.to_string()))
        });
        assert_eq!(
            renames.borrow().len(),
            after_first + 2,
            "nothing changed, nothing renamed"
        );
    }

    /// A pane's title is written from the roster and never read back
    /// for its role: the same pane, titled for the wrong role by an
    /// earlier session, is restamped from what the roster says.
    #[test]
    fn a_pane_title_follows_the_roster_not_the_pane() {
        let renames = std::cell::RefCell::new(Vec::<(String, String)>::new());
        let mut ps = PaneStatus::new();
        let rows = vec![
            ("terminal_0".to_string(), "claude".to_string()),
            ("terminal_1".to_string(), "codex".to_string()),
        ];
        // codex is master now.
        ps.update_with(
            &glyphs_for("codex", &[("claude", "M", "R"), ("codex", "M", "R")]),
            Some(rows),
            |id, title| renames.borrow_mut().push((id.into(), title.into())),
        );
        assert_eq!(
            renames.borrow().as_slice(),
            &[
                ("terminal_0".to_string(), "R claude".to_string()),
                ("terminal_1".to_string(), "M codex".to_string()),
            ]
        );
    }

    /// A zellij whose session is a list of panes the test writes, read
    /// through the real workspace and name resolution, with the
    /// ownership record a real file. Closing removes panes from the
    /// list; opening adds one named for its label, unless `lagging`, when
    /// the pane is created but never listed.
    struct ListingIo {
        repo: tempfile::TempDir,
        record: std::path::PathBuf,
        panes: Vec<serde_json::Value>,
        next_id: u32,
        lagging: bool,
        closed: Vec<String>,
        opened: Vec<String>,
    }

    /// The TUI's own pane, whose tab is the workspace.
    const TUI_PANE: &str = "terminal_1";

    impl ListingIo {
        /// Tab 0 holds this repo's TUI pane and `titles` as panes
        /// `terminal_2…`; tab 1 holds another repo's TUI pane and its own
        /// `painter`.
        fn with(titles: &[&str]) -> Self {
            let repo = tempfile::tempdir().unwrap();
            let record = repo.path().join("pane-owners.json");
            let mut panes = vec![serde_json::json!({
                "id": 1, "title": "", "tab_id": 0,
                "terminal_command": format!("clank tui --repo {}", repo.path().display()),
            })];
            let mut next_id = 2;
            for title in titles {
                panes.push(serde_json::json!({"id": next_id, "title": title, "tab_id": 0}));
                next_id += 1;
            }
            panes.push(serde_json::json!({
                "id": 50, "title": "", "tab_id": 1,
                "terminal_command": "clank tui --repo /elsewhere",
            }));
            panes.push(serde_json::json!({"id": 51, "title": "painter", "tab_id": 1}));
            Self {
                repo,
                record,
                panes,
                next_id,
                lagging: false,
                closed: Vec::new(),
                opened: Vec::new(),
            }
        }

        fn own(&self, labels: &[&str]) {
            let labels: Labels = labels.iter().map(|l| l.to_string()).collect();
            crate::cli::pane_owners::update_at(&self.record, &labels, &Labels::new()).unwrap();
        }

        fn recorded(&self) -> Option<Labels> {
            crate::cli::pane_owners::load_at(&self.record).unwrap()
        }

        fn ids(&self) -> Vec<String> {
            self.panes
                .iter()
                .map(|p| format!("terminal_{}", p["id"]))
                .collect()
        }
    }

    impl PaneIo for ListingIo {
        type Snap = Scoped;
        fn snapshot(&mut self, candidates: &Labels) -> Option<Self::Snap> {
            let panes =
                serde_json::from_value(serde_json::Value::Array(self.panes.clone())).unwrap();
            Scoped::of(panes, self.repo.path(), Some(TUI_PANE), candidates)
        }
        fn seen(&mut self, snap: &Self::Snap) -> Seen {
            snap.seen()
        }
        fn verify(&mut self, candidates: &Labels, _: &[String]) -> Option<Verified> {
            let snap = self.snapshot(candidates)?;
            Some(Verified {
                seen: snap.seen(),
                placed: true,
            })
        }
        fn unclaimed(&mut self, snap: &Self::Snap) -> Labels {
            snap.unclaimed(self.repo.path(), Some(TUI_PANE))
        }
        fn owned(&mut self) -> Result<Option<Labels>, String> {
            crate::cli::pane_owners::load_at(&self.record).map_err(|e| format!("{e:#}"))
        }
        fn reown(&mut self, own: &Labels, disown: &Labels) -> Result<(), String> {
            crate::cli::pane_owners::update_at(&self.record, own, disown)
                .map(drop)
                .map_err(|e| format!("{e:#}"))
        }
        fn capture_focus(&mut self, _: &Self::Snap) -> Option<String> {
            None
        }
        fn restore_focus(&mut self, _: &str) {}
        fn add(&mut self, label: &str, _: &Self::Snap) -> Option<String> {
            let id = self.next_id;
            self.next_id += 1;
            if !self.lagging {
                self.panes
                    .push(serde_json::json!({"id": id, "title": label, "tab_id": 0}));
            }
            self.opened.push(label.to_string());
            Some(format!("terminal_{id}"))
        }
        fn is_placed(&mut self, _: &[String], _: &Self::Snap) -> bool {
            true
        }
        fn compose(&mut self, _: &str, _: &[String], _: &Self::Snap) -> Option<String> {
            None
        }
        fn override_layout(&mut self, _: &str, _: &Self::Snap) -> bool {
            false
        }
        fn remove_all(&mut self, labels: &[String], snap: &Self::Snap) {
            let ids = crate::cli::open_zellij::remove_target_ids(labels, &snap.panes, &snap.ws);
            self.panes
                .retain(|p| !ids.contains(&format!("terminal_{}", p["id"])));
            self.closed.extend(ids);
        }
        fn live_labels(&mut self, snap: &Self::Snap) -> Labels {
            snap.live_labels()
        }
        fn title_rows(&mut self, snap: &Self::Snap) -> Vec<(String, String)> {
            crate::cli::open_zellij::agent_panes_by_title(&snap.panes, &snap.ws)
        }
        fn lost_labels(&mut self, _: &Self::Snap) -> Labels {
            Labels::new()
        }
    }

    /// `painter` is recorded as owned and its pane `💤 painter`
    /// (terminal_3) runs beside `claude`, a shell and an editor; another
    /// repo's tab in the same session has a `painter` too. Removing
    /// painter from the roster without renaming its pane closes exactly
    /// terminal_3, and the read-back that shows it gone drops painter
    /// from the record.
    #[test]
    fn a_removed_agents_pane_is_found_through_the_record_and_closed() {
        let mut io = ListingIo::with(&["🔨 claude", "💤 painter", "zsh", "nvim notes"]);
        io.own(&["claude", "painter"]);
        PaneReconciler::new().reconcile(view(&["claude"], Some("claude")), &mut io);
        assert_eq!(io.closed, ["terminal_3"]);
        assert_eq!(
            io.ids(),
            [
                "terminal_1",
                "terminal_2",
                "terminal_4",
                "terminal_5",
                "terminal_50",
                "terminal_51"
            ],
            "the shell, the editor and the other repo's painter are untouched"
        );
        assert_eq!(io.recorded(), Some(["claude".to_string()].into()));
    }

    /// A whole-team replacement closes every old owned label's pane, opens
    /// the new team's, and leaves the record holding the new team only.
    #[test]
    fn a_whole_team_replacement_closes_the_old_and_opens_the_new() {
        let mut io = ListingIo::with(&["🔨 claude", "💤 painter (reviewer)", "zsh"]);
        io.own(&["claude", "painter"]);
        PaneReconciler::new().reconcile(view(&["ada", "bea"], Some("ada")), &mut io);
        let mut closed = io.closed.clone();
        closed.sort();
        assert_eq!(closed, ["terminal_2", "terminal_3"]);
        assert_eq!(io.opened, ["ada", "bea"]);
        assert_eq!(io.recorded(), Some(["ada", "bea"].map(String::from).into()));
    }

    /// With no record, a pass adopts the roster's panes into one; after a
    /// TUI restart (a new reconciler) that record still finds the pane of
    /// an agent removed while no TUI ran, and closes it.
    #[test]
    fn after_a_restart_the_record_still_finds_a_stale_pane() {
        let mut io = ListingIo::with(&["🔨 claude", "💤 painter"]);
        assert_eq!(io.recorded(), None);
        PaneReconciler::new().reconcile(view(&["claude", "painter"], Some("claude")), &mut io);
        assert!(io.closed.is_empty());
        assert_eq!(
            io.recorded(),
            Some(["claude", "painter"].map(String::from).into())
        );
        PaneReconciler::new().reconcile(view(&["claude"], Some("claude")), &mut io);
        assert_eq!(io.closed, ["terminal_3"]);
    }

    /// With no record, nothing off the roster is claimed: the stale pane
    /// is not closed, and the worker reports it as not on the roster and
    /// not closed.
    #[test]
    fn without_a_record_a_stale_pane_is_reported_not_closed() {
        let mut io = ListingIo::with(&["🔨 claude", "💤 painter"]);
        let mut state = WorkerState::new(None);
        let reports = state.handle(
            wb(Some(view(&["claude"], Some("claude"))), None, Vec::new()),
            &mut io,
            |_, _| {},
        );
        assert!(io.closed.is_empty());
        let notice = reports
            .iter()
            .find_map(|r| match r {
                Report::Ownership(Some(n)) => Some(n.clone()),
                _ => None,
            })
            .expect("an ownership notice");
        assert!(
            notice.contains("`painter`") && notice.contains("not on roster, not closed"),
            "{notice}"
        );
        assert!(!notice.contains("`claude`"), "{notice}");
    }

    /// A pane opened for `painter` that no listing shows yet is not a
    /// disappearance: removing painter from the roster while that
    /// creation is still believed keeps painter in the record.
    #[test]
    fn a_creation_not_yet_listed_is_not_dropped_from_the_record() {
        let mut io = ListingIo::with(&["🔨 claude"]);
        io.lagging = true;
        let mut r = PaneReconciler::new();
        r.reconcile(view(&["claude", "painter"], Some("claude")), &mut io);
        assert_eq!(io.opened, ["painter"]);
        assert_eq!(
            io.recorded(),
            Some(["claude", "painter"].map(String::from).into())
        );
        r.reconcile(view(&["claude"], Some("claude")), &mut io);
        assert!(
            io.recorded().unwrap().contains("painter"),
            "{:?}",
            io.recorded()
        );
    }

    /// Without the TUI's own pane or this repo's TUI pane in the listing
    /// there is no workspace: an owned agent's pane off the roster is not
    /// closed, a missing roster agent is not opened, and the record is
    /// not touched.
    #[test]
    fn an_unresolved_workspace_is_never_acted_on() {
        let mut io = ListingIo::with(&["🔨 claude", "💤 painter"]);
        io.panes.retain(|p| p["id"] != 1);
        io.own(&["claude", "painter"]);
        PaneReconciler::new().reconcile(view(&["claude", "bea"], Some("claude")), &mut io);
        assert!(io.closed.is_empty() && io.opened.is_empty());
        assert_eq!(
            io.recorded(),
            Some(["claude", "painter"].map(String::from).into())
        );
    }

    /// A pane whose name reads as two candidates is neither painter's nor
    /// absent: it is not closed, and painter stays recorded.
    #[test]
    fn an_ambiguous_pane_is_not_a_disappearance() {
        let mut io = ListingIo::with(&["🔨 claude", "💤 painter"]);
        io.own(&["claude", "painter", "💤 painter"]);
        PaneReconciler::new().reconcile(view(&["claude"], Some("claude")), &mut io);
        assert!(io.closed.is_empty());
        assert!(io.recorded().unwrap().contains("painter"));
    }

    /// A `💤 painter` pane beside a roster and record of `claude`,
    /// `painter` and `💤 painter` may be either agent's, so a pass opens
    /// neither, closes nothing, and says which pane is in doubt.
    #[test]
    fn an_ambiguous_pane_is_not_a_missing_one() {
        let labels = ["claude", "painter", "💤 painter"];
        let mut io = ListingIo::with(&["🔨 claude", "💤 painter"]);
        io.own(&labels);
        let mut state = WorkerState::new(None);
        let reports = state.handle(
            wb(Some(view(&labels, Some("claude"))), None, Vec::new()),
            &mut io,
            |_, _| {},
        );
        assert!(io.opened.is_empty(), "{:?}", io.opened);
        assert!(io.closed.is_empty(), "{:?}", io.closed);
        let notice = reports
            .iter()
            .find_map(|r| match r {
                Report::Ownership(Some(n)) => Some(n.clone()),
                _ => None,
            })
            .expect("an ownership notice");
        assert!(
            notice.contains("terminal_3")
                && notice.contains("`painter`")
                && notice.contains("`💤 painter`")
                && notice.contains("not opened"),
            "{notice}"
        );
    }

    /// The per-agent reopen asks the same question: a pane that may be
    /// painter's means none is opened, and the outcome names it.
    #[test]
    fn a_reopen_beside_an_ambiguous_pane_opens_nothing() {
        let labels = ["claude", "painter", "💤 painter"];
        let mut io = ListingIo::with(&["🔨 claude", "💤 painter"]);
        io.own(&labels);
        let out = PaneReconciler::new().reopen(&view(&labels, Some("claude")), "painter", &mut io);
        let ReopenOutcome::Ambiguous(doubts) = &out else {
            panic!("{out:?}");
        };
        assert_eq!(doubts.len(), 1);
        assert_eq!(doubts[0].pane, "terminal_3");
        assert!(out.describe("painter").contains("terminal_3"));
        assert!(io.opened.is_empty() && io.closed.is_empty());
    }

    /// Each of `msgs` as its own batch through the real `serve`.
    fn served(msgs: Vec<WorkerMsg>, state: &mut WorkerState, io: &mut FakeIo) -> Vec<Report> {
        let mut reports = Vec::new();
        for msg in msgs {
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(msg).unwrap();
            tx.send(WorkerMsg::Close).unwrap();
            serve(
                &rx,
                state,
                io,
                &mut NoTabs,
                |_, _| {},
                |r| reports.extend(r),
            );
        }
        reports
    }

    #[test]
    fn an_idle_worker_never_lists() {
        let (tx, rx) = std::sync::mpsc::channel::<WorkerMsg>();
        let mut io = FakeIo::new(vec![Some(live(&[("claude", true)])); 4]);
        let join = std::thread::spawn(move || {
            let mut state = WorkerState::new(None);
            serve(&rx, &mut state, &mut io, &mut NoTabs, |_, _| {}, |_| {});
            io.snapshots_taken
        });
        // Longer than the 5 s period this loop used to probe on.
        std::thread::sleep(std::time::Duration::from_millis(5500));
        assert!(!join.is_finished(), "it waits on its channel, not a clock");
        tx.send(WorkerMsg::Close).unwrap();
        assert_eq!(join.join().unwrap(), 0);
    }

    /// Once a listing has retitled with the glyphs it was sent, the same
    /// glyphs again, batch after batch, list nothing: only a probe, a glyph
    /// change or a roster change does.
    #[test]
    fn a_quiet_worker_lists_nothing_across_idle_batches() {
        let mut io = FakeIo::new(vec![Some(live(&[("claude", true)])); 8]);
        let mut state = WorkerState::new(None);
        let same = || WorkerMsg::Glyphs(glyphs(&[("claude", "💤", "👀")]));
        served(vec![same()], &mut state, &mut io);
        assert_eq!(io.snapshots_taken, 1, "the first glyphs retitle");
        served(vec![same(), same(), same()], &mut state, &mut io);
        assert_eq!(io.snapshots_taken, 1, "nothing changed, nothing listed");
    }

    /// A pane started by hand, with no change to the agent's config, that
    /// exits is shown gone at the next probe's listing, and not before.
    #[test]
    fn a_hand_started_panes_exit_shows_at_the_next_probe() {
        let mut io = FakeIo::new(vec![Some(live(&[("claude", true)])); 4]);
        let mut state = WorkerState::new(None);
        let reports = served(vec![WorkerMsg::Probe], &mut state, &mut io);
        assert_eq!(presence_of(&reports), Some(Some(vec!["claude".into()])));
        io.dead.insert("claude".into());
        io.lost = ["claude".to_string()].into();
        assert!(
            served(Vec::new(), &mut state, &mut io).is_empty(),
            "an exit alone lists nothing"
        );
        assert_eq!(io.snapshots_taken, 1);
        let reports = served(vec![WorkerMsg::Probe], &mut state, &mut io);
        assert_eq!(io.snapshots_taken, 2);
        assert_eq!(presence_of(&reports), Some(Some(Vec::new())));
        assert!(reports.contains(&Report::Lost(["claude".to_string()].into())));
    }

    /// The runtime directory is shared: a stale `pane-launch-*.json` file
    /// appearing in it is heard by nobody, while this repo's `clank remote`
    /// reopen request in the same directory reaches the loop.
    #[test]
    fn only_a_reopen_request_in_the_runtime_directory_reaches_the_loop() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let repo = std::path::Path::new("/w/proj");
        let (tx, requests) = std::sync::mpsc::channel();
        let woken = std::sync::Arc::new(AtomicUsize::new(0));
        let wakes = woken.clone();
        let watch = RuntimeWatch::start_in(repo, Ok(dir.path().to_path_buf()), tx, move || {
            wakes.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(watch.unavailable, None);
        let tag = crate::cli::monitor::runtime_path_in(dir.path(), repo, "")
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        std::fs::write(dir.path().join(format!("{tag}pane-launch-x.json")), "{}").unwrap();
        assert!(
            requests
                .recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "a launch file is nobody's request"
        );
        assert_eq!(woken.load(Ordering::SeqCst), 0);
        let request = crate::cli::remote_control::request_path(repo, "r1");
        std::fs::write(dir.path().join(request.file_name().unwrap()), "{}").unwrap();
        assert_eq!(
            requests.recv_timeout(std::time::Duration::from_secs(5)),
            Ok("r1".to_string())
        );
        assert_eq!(woken.load(Ordering::SeqCst), 1);
        drop(watch);
    }
}
