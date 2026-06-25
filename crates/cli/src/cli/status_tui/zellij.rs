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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::status_tui::fixtures::{plan_state, reviewer_missing, snap};
    use clank_core::plan_view::WaitingOn;

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
