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
    pub(super) fn new() -> Option<Self> {
        std::env::var_os("ZELLIJ").map(|_| Self {
            last: std::collections::HashMap::new(),
            panes: Vec::new(),
            primed: false,
            requeried_absent: std::collections::HashSet::new(),
        })
    }

    pub(super) fn update(&mut self, snap: &StatusSnapshot) {
        self.update_with(snap, list_panes, rename_pane);
    }

    /// Core of [`update`] with the zellij I/O injected, so the caching
    /// logic is testable without spawning (no-binary-spawning-tests).
    /// `list_panes` is called only when the cache needs (re)priming;
    /// `rename` only for panes whose title changed.
    fn update_with(
        &mut self,
        snap: &StatusSnapshot,
        mut list_panes: impl FnMut() -> Option<String>,
        mut rename: impl FnMut(&str, &str),
    ) {
        // Refresh the cached pane map only when needed: first run, or
        // when the snapshot wants to mark an agent we have no cached
        // pane for (a pane was likely added). Steady state reuses the
        // cache, so no `list-panes` subprocess fires per render.
        if (!self.primed || self.wants_uncached(snap))
            && let Some(panes) = list_panes()
        {
            self.panes = parse_agent_panes(&panes);
            self.primed = true;
            // A fresh map supersedes the give-up memory; re-record any
            // wanted label that's STILL absent so we don't re-query for
            // it every render.
            self.requeried_absent.clear();
            for label in self.wanted(snap) {
                if !self.has_pane(&label) {
                    self.requeried_absent.insert(label);
                }
            }
        }
        // Build the rename list from the cached map first (immutable
        // borrow), then apply — keeps `self.panes` and `self.last`
        // borrows disjoint.
        let mut renames: Vec<(String, String)> = Vec::new();
        for (id, label, role) in &self.panes {
            let emoji = agent_status_emoji(snap, label, *role);
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

    /// Labels the snapshot wants to mark as active: the master and any
    /// awaited reviewers. (Idle agents that already have a cached pane
    /// are handled by the cache; this set only drives the re-query.)
    fn wanted(&self, snap: &StatusSnapshot) -> Vec<String> {
        let mut v: Vec<String> = awaited_reviewers(snap)
            .iter()
            .map(|l| l.as_str().to_string())
            .collect();
        if let Some(m) = snap.master.as_deref() {
            v.push(m.to_string());
        }
        v
    }

    fn has_pane(&self, label: &str) -> bool {
        self.panes.iter().any(|(_, l, _)| l == label)
    }

    /// A wanted agent has no cached pane and we haven't already given
    /// up re-querying for it — a pane likely appeared since we fetched.
    fn wants_uncached(&self, snap: &StatusSnapshot) -> bool {
        self.wanted(snap)
            .into_iter()
            .any(|label| !self.has_pane(&label) && !self.requeried_absent.contains(&label))
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
}

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
/// (no-binary-spawning-tests) — same pattern as
/// [`PaneStatus::update_with`].
pub(super) trait PaneIo {
    fn live(&mut self) -> Option<Vec<(String, bool)>>;
    fn add(&mut self, label: &str, other_reviewers: &[String]);
    fn relocate(&mut self, new_master: &str, old_master: Option<&str>, roster: &[String]);
    fn remove(&mut self, label: &str);
}

/// The real zellij-backed [`PaneIo`].
struct ZellijPaneIo<'a> {
    repo: &'a std::path::Path,
}

impl PaneIo for ZellijPaneIo<'_> {
    fn live(&mut self) -> Option<Vec<(String, bool)>> {
        crate::cli::open_zellij::live_agent_panes(self.repo)
    }
    fn add(&mut self, label: &str, other_reviewers: &[String]) {
        crate::cli::open_zellij::add_reviewer_pane(self.repo, label, other_reviewers);
    }
    fn relocate(&mut self, new_master: &str, old_master: Option<&str>, roster: &[String]) {
        crate::cli::open_zellij::relocate_for_promote(self.repo, new_master, old_master, roster);
    }
    fn remove(&mut self, label: &str) {
        crate::cli::open_zellij::remove_reviewer_pane(self.repo, label);
    }
}

impl PaneReconciler {
    pub(super) fn new() -> Self {
        Self { converged: None }
    }

    /// Observe a fresh snapshot; reconcile unless this exact roster
    /// view was already VERIFIED converged. Cheap in the steady state:
    /// one set comparison, no zellij calls.
    pub(super) fn observe(&mut self, repo: &std::path::Path, snap: &StatusSnapshot) {
        if std::env::var_os("ZELLIJ").is_none() {
            return;
        }
        let mut io = ZellijPaneIo { repo };
        self.observe_with(snap, &mut io);
    }

    fn observe_with(&mut self, snap: &StatusSnapshot, io: &mut impl PaneIo) {
        let cur = RosterView::of(snap);
        if self.converged.as_ref() == Some(&cur) {
            return;
        }
        // Listing failure → touch nothing AND stay unconverged, so the
        // next refresh retries (acting on a partial listing would
        // re-open every pane; forgetting the event would drop it).
        let Some(live) = io.live() else {
            return;
        };
        let plan = plan_panes(&cur, &live);
        let reviewers: Vec<String> = cur
            .labels
            .iter()
            .filter(|l| Some(*l) != cur.master.as_ref())
            .cloned()
            .collect();
        // Adds before the relocate (a swapped-in master may be brand
        // new), removes last.
        for label in &plan.add {
            io.add(label, &reviewers);
        }
        if let Some((new_master, old_master)) = &plan.relocate {
            // The relocation's classification set is the union of the
            // desired roster and EVERY live agent label: all departing
            // panes (master or reviewer) are still live here — removes
            // run after the layout — and compose skips on any
            // unclassified live agent pane (codex ae6338a, 8c4906d).
            let mut all: std::collections::BTreeSet<String> = cur.labels.clone();
            all.extend(live.iter().map(|(l, _)| l.clone()));
            let all: Vec<String> = all.into_iter().collect();
            io.relocate(new_master, old_master.as_deref(), &all);
        }
        for label in &plan.remove {
            io.remove(label);
        }
        // Converged only when a VERIFYING re-list confirms the target
        // state — every action above is best-effort, so observation is
        // not achievement. An already-empty plan needs no second list.
        let verified = if plan.is_converged() {
            true
        } else {
            io.live()
                .is_some_and(|after| plan_panes(&cur, &after).is_converged())
        };
        if verified {
            self.converged = Some(cur);
        }
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

    /// Scripted [`PaneIo`]: a queue of `live()` answers + an action log.
    struct FakeIo {
        lives: std::collections::VecDeque<Option<Vec<(String, bool)>>>,
        log: Vec<String>,
    }

    impl FakeIo {
        fn new(lives: Vec<Option<Vec<(String, bool)>>>) -> Self {
            Self {
                lives: lives.into(),
                log: Vec::new(),
            }
        }
    }

    impl PaneIo for FakeIo {
        fn live(&mut self) -> Option<Vec<(String, bool)>> {
            self.lives.pop_front().unwrap_or(None)
        }
        fn add(&mut self, label: &str, _other: &[String]) {
            self.log.push(format!("add {label}"));
        }
        fn relocate(&mut self, new_master: &str, old_master: Option<&str>, _roster: &[String]) {
            self.log
                .push(format!("relocate {new_master}<-{old_master:?}"));
        }
        fn remove(&mut self, label: &str) {
            self.log.push(format!("remove {label}"));
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
        r.observe_with(&snap, &mut io);
        assert!(io.log.is_empty());
        assert_eq!(r.converged, None, "failure must not converge");

        // Second observe, same roster: retries; the pass runs (codex
        // pane missing → add) and the verify re-list confirms.
        let mut io = FakeIo::new(vec![
            Some(live(&[("claude", true)])),
            Some(live(&[("claude", true), ("codex", false)])),
        ]);
        r.observe_with(&snap, &mut io);
        assert_eq!(io.log, vec!["add codex"]);
        assert!(r.converged.is_some(), "verified pass converges");

        // Third observe, same roster: steady state, zero io.
        let mut io = FakeIo::new(vec![]);
        r.observe_with(&snap, &mut io);
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
        let mut io = FakeIo::new(vec![Some(stale.clone()), Some(stale)]);
        r.observe_with(&snap, &mut io);
        assert_eq!(io.log, vec!["add codex"]);
        assert_eq!(r.converged, None);
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
        let after = live(&[("new-master", true), ("codex", false)]);
        let mut io = FakeIo::new(vec![Some(before), Some(after)]);
        r.observe_with(&snap, &mut io);
        assert_eq!(
            io.log,
            vec![
                "add new-master",
                "relocate new-master<-Some(\"old\")",
                "remove old"
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
        let mut io = FakeIo::new(vec![Some(before), Some(after)]);
        r.observe_with(&snap, &mut io);
        assert_eq!(r.converged, None, "master in the stack is not converged");
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
                s,
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

        // First render: one `list-panes`; both agent panes get a title.
        let awaited = snap(vec![plan_state("p", reviewer_missing("codex"))], vec![]);
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
        let idle = snap(vec![plan_state("p", WaitingOn::MasterToContinue)], vec![]);
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
