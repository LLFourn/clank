//! `clank status` — read-only summary of the current repo's Clank
//! state. Folds the repo, projects per-plan via `clank-core::plan_view`,
//! then renders. Never blocks.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use super::{StatusArgs, repo_basename, resolve_repo};
use crate::cli::plan_resolve::parse_arg;
use crate::git_io::DirtyStats;
use crate::lifecycle::PlanKey;
use crate::repo_state::RepoState;
use clank_core::plan_view::WaitingOn;
use clank_core::repo_state::FinishedPlan;
use clank_core::vocab::CommitGateState;
use clank_core::wait::PlanWorkState;

/// The snapshot IS the wire: `status -j` serializes the model the pane
/// renders, so the two can differ about presentation and not about
/// facts (status-is-one-model-rendered-two-ways).
///
/// DERIVED, so a fact added here is emitted without being written down
/// twice — the roster went missing for as long as it did because a
/// hand-written projection had to be remembered. What is written down
/// is only what cannot be defaulted: the log's machinery, skipped by
/// name, and the shapes the wire fixed before this was one model.
#[derive(serde::Serialize)]
pub struct StatusSnapshot {
    /// Where this was read. `--repo` means the reader need not be
    /// standing in it, and the pane says it too (codex on 772b84f).
    pub(crate) repo_path: PathBuf,
    #[serde(rename = "repo_basename")]
    pub(crate) basename: String,
    pub(crate) branch: Option<String>,
    pub(crate) head_sha: Option<String>,
    pub(crate) head_subject: Option<String>,
    /// `Some` iff the worktree is dirty — carries the +/− line
    /// counts vs HEAD and the untracked-file count
    /// (tui-event-driven-dirty-stats).
    /// Two keys on the wire: the flag it has always carried, and the
    /// counts, which are there only when it is true.
    #[serde(flatten, with = "dirty_wire")]
    pub(crate) dirty: Option<DirtyStats>,
    #[serde(serialize_with = "plans_wire")]
    pub(crate) plans: Vec<PlanWorkState>,
    /// A list on the wire, of nought or one. It is `Some` only where
    /// there is no active plan — which `select_plans_and_finished`
    /// decides, so nothing here has to decide it again.
    #[serde(rename = "finished_plans", serialize_with = "as_list")]
    pub(crate) last_finished: Option<FinishedPlan>,
    #[serde(serialize_with = "blocks_wire")]
    pub(crate) blocks: Vec<crate::cli::block::BlockEntry>,
    /// Queued plans (name + priority) in priority order — the TUI's
    /// QUEUE section renders and reprioritises these; counts everywhere
    /// else derive from it.
    /// Three keys on the wire: the count and the names consumers
    /// read, and the entries that order them and tell two of one name
    /// apart. All absent when nothing is queued.
    #[serde(flatten, with = "queue_wire")]
    pub(crate) queue: Vec<QueueItemView>,
    /// The team's master label, when a team resolves (render path:
    /// degrades to None on a teamless repo). The TUI's idle+queue
    /// headline names master as the agent whose turn it is.
    pub(crate) master: Option<String>,
    /// The team roster (master first, then reviewers; deduped) with
    /// each agent's EFFECTIVE auto-mode — the armed/disarmed state the
    /// TUI agent panel renders and toggles. Empty on a teamless
    /// repo (mirrors `master: None`). Built in the snapshot (not read
    /// ad hoc in render) so the nothing-changed gate covers it: every
    /// agent's `config.json` lives under the fingerprinted `agents/`
    /// dir, so an external `clank auto` flip repaints. Plan:
    /// tui-agent-auto-toggle.
    pub(crate) agents: Vec<AgentAutoRow>,
    /// What is waiting in each agent's inbox, read ONCE per snapshot.
    ///
    /// One projection so the rows the cursor can reach and the rows
    /// drawn agree: scanning again during paint or navigation lets an
    /// agent handle a message between the two and leaves the cursor on
    /// a row that is no longer there (codex ed1baf5).
    ///
    /// Keyed by LABEL, not index: the roster reorders.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) inboxes: Vec<InboxView>,
    /// Shelved plans (stem, what they're waiting for if `--for`
    /// was used, and whether that wait is over). Plan:
    /// plan-lifecycle-verbs.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) stash: Vec<StashItemView>,
    /// This repo's forks (name, kind). Durable repo state of exactly
    /// the kind status already reports — their absence here is why a
    /// fork could sit stranded unnoticed (fork-cli-is-a-noun).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) forks: Vec<(String, &'static str)>,
    /// Recent activity as STRUCTURED oneline rows (umbrella
    /// headers + commits + reviews), NEWEST FIRST (the git-log
    /// convention every log display follows; lloyd) — the TUI's
    /// log pane takes the head and styles per row kind. Built from the fold's LogEvents (subjects carried; no
    /// per-event git shelling). NOT emitted by `to_json` — the
    /// status --json log shape is unchanged; `clank log --json`
    /// is the machine log surface (log-plan-umbrellas, ruthless
    /// 3ea8580 concern 1). Plans: status-tui-live-log,
    /// log-plan-umbrellas.
    #[serde(skip)]
    pub(crate) log_rows: Vec<crate::cli::log::OnelineRow>,
    /// Durable fold boundary used by on-demand log paging. Keeping it in
    /// the snapshot means scrolling never rebuilds the whole repo merely to
    /// rediscover where plain Git history begins.
    #[serde(skip)]
    pub(crate) log_adopted_at: Option<crate::lifecycle::CommitSha>,
    /// Finalize SHAs that stand for their whole plan — carried so a
    /// LATER page collapses the same rows the first one did, rather
    /// than answering from whatever that page happens to contain
    /// (codex on 5b1e87c).
    #[serde(skip)]
    pub(crate) log_stands_for: crate::cli::log::StandsFor,
    /// Cursor for the next older TUI page. `None` means the currently loaded
    /// rows reach the repository root.
    #[serde(skip)]
    pub(crate) log_next: Option<crate::cli::log::HistoryCursor>,
    /// sha → branch names whose tips sit on that commit
    /// (tui-log-branch-decorations): HEAD's branch, its upstream, the
    /// main worktree's branch and its `origin/…` counterpart. TUI-only
    /// decoration; valid across paged log growth (tips
    /// don't move with scrolling) and rebuilt with the snapshot. NOT
    /// in `to_json`.
    #[serde(skip)]
    pub(crate) log_decorations: std::collections::BTreeMap<String, Vec<String>>,
    /// Active GitHub PR reviews (clank-pr-review-mode), for the
    /// `pr` gauge. Empty in the common (no-PR-review) case.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) pr_reviews: Vec<clank_core::wait::PrReviewWorkState>,
    /// The timeline's merged github events, index-aligned with the
    /// `OnelineRow::Github` rows in `log_rows` (set together —
    /// tui-github-event-page).
    #[serde(skip)]
    pub(crate) github_events: Vec<crate::cli::github::timeline::MergedEvent>,
    /// The fold's pending AD-HOC commit review, if any — carried so
    /// the TUI's activity projection sees the same work `clank wait`
    /// delivers (tui-adhoc-review-activity).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) ad_hoc: Vec<clank_core::wait::AdHocWorkState>,
    /// A broken HEAD commit tag, when present
    /// (commit-tag-fixup-is-first-class-state). The SINGLE source the
    /// renderers read so the correction is visible even when NO active
    /// plan row carries it — the unknown-tag-only case, where `T = ∅`
    /// and the tag names no real plan, has no row to mark (codex
    /// 2bf46d9).
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "correction_wire"
    )]
    pub(crate) head_correction: Option<clank_core::wait::HeadCorrection>,
}

/// One team agent as the TUI's agent panel sees it: its label,
/// roster role, and EFFECTIVE auto-mode. The auto-mode is the
/// *armed* state (takes effect at that agent's next Stop-hook
/// decision), NOT a live run indicator.
#[derive(serde::Serialize)]
pub(crate) struct AgentAutoRow {
    pub(crate) label: String,
    /// The roster TIER — Master / Commit / Plan / Final / Gate — so the
    /// panel can distinguish the kinds of reviewers (not just master vs
    /// reviewer).
    pub(crate) role: crate::cli::teams_config::RosterRole,
    #[serde(rename = "auto")]
    pub(crate) auto_mode: clank_core::vocab::AutoMode,
    /// Roster `AgentDescription` facts the detail page surfaces: the
    /// tool and the invocation that runs it (launch command + args, or
    /// the bare tool).
    pub(crate) tool: String,
    pub(crate) invocation: String,
    /// The session this label is bound to (`clank as`), from the
    /// agent's local config: its id AND its own tool, which wins over a
    /// roster entry that may have changed since it bound. `None` =
    /// unbound — surfaced as a problem on the detail page, since an
    /// unbound agent can't receive work.
    pub(crate) session: Option<clank_core::Session>,
    /// The last attendance DECISION the Stop hook made, or `None` if
    /// its most recent turn-end was not an attending silence.
    ///
    /// Not the marker: that is consumed within the turn that writes it
    /// and is never visible here. This is the observation left behind,
    /// which is why it can be shown at all.
    ///
    /// Reported, never acted on. Status must not write or clear it —
    /// the hook rewrites it at every turn-end, so there is nothing
    /// here to maintain.
    pub(crate) attending: Option<crate::cli::stop_hook::Attended>,
}

/// The invocation that runs an agent: launch command (or the bare tool
/// name) + any launch args. Shared by the roster rows and the "+ add"
/// candidates so both render the same string.
pub(crate) fn agent_invocation(desc: &crate::cli::teams_config::AgentDescription) -> String {
    let cmd = desc
        .launch
        .as_ref()
        .and_then(|l| l.command.clone())
        .unwrap_or_else(|| desc.tool.as_str().to_string());
    let args = desc
        .launch
        .as_ref()
        .map(|l| l.args.join(" "))
        .unwrap_or_default();
    if args.is_empty() {
        cmd
    } else {
        format!("{cmd} {args}")
    }
}

/// A global-library agent that is NOT yet on this repo's roster — a
/// candidate the TUI's "+ add" picker can add as a reviewer. Read
/// FRESH from the `~/.clank` library when the picker opens (not cached
/// in the snapshot), so it can't go stale. `invocation` is the launch
/// command + args (or the bare tool name) — what actually runs — and
/// `description` is the agent's `initial_prompt`, the closest thing
/// clank has to "what is this agent for" (there is no semantic purpose
/// field yet). Plan: tui-agents-panel-manage.
#[derive(Default)]
pub(crate) struct AvailableAgent {
    pub(crate) label: String,
    pub(crate) tool: String,
    pub(crate) invocation: String,
    pub(crate) description: Option<String>,
}

/// One queued plan as the snapshot carries it: display name + the
/// `NNN` priority from its filename prefix (lower = promoted sooner),
/// and the file itself — the entry's IDENTITY. The parse is lenient
/// (`foo.md`, `1-foo.md` and `001-foo.md` all read as one name and
/// priority), so nothing derived from the filename can tell two
/// entries apart; only the filename can (a-queued-plan-has-a-page).
/// One agent's pending messages, as the panel needs them.
///
/// Present only when there is something waiting: an empty inbox has no
/// row, so it has no entry here either.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct InboxView {
    pub(crate) label: String,
    /// In `inbox::scan` order -- the order they wake the agent in.
    /// EVERY message, so the page can show them all; a row that opened
    /// only the first would strand the rest (codex ed1baf5).
    pub(crate) messages: Vec<InboxMessageView>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct InboxMessageView {
    /// The handle `clank inbox` takes.
    pub(crate) handle: String,
    /// The first non-blank line, without a heading's `#`s.
    pub(crate) subject: String,
    /// Who sent it, already worded.
    pub(crate) from: String,
    /// When it was sent. ABSENT for a message written by hand without
    /// metadata, or one whose stamp will not parse -- the inbox takes
    /// those, and an unknown age is a fact to show, not one to invent.
    pub(crate) sent_at: Option<i64>,
    pub(crate) body: String,
}

impl InboxView {
    pub(crate) fn read(repo: &Path, label: &str) -> Option<Self> {
        let parsed = clank_core::ids::AgentLabel::parse(label).ok()?;
        let messages: Vec<InboxMessageView> = crate::inbox::scan(repo, &parsed)
            .into_iter()
            .map(|m| InboxMessageView {
                // `file_stem`, not `name`: name is priority-STRIPPED
                // and two entries can share one. The handle shown is
                // the handle `clank inbox` resolves, and resolving a
                // shadowed name selects a DIFFERENT message
                // (codex 3bc0d05).
                handle: m.entry.file_stem(),
                subject: m.subject().to_string(),
                from: crate::inbox::sender_words(m.from.as_deref(), None),
                sent_at: m.sent.as_deref().and_then(|s| {
                    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                        .ok()
                        .map(|t| t.unix_timestamp())
                }),
                body: m.body.clone(),
            })
            .collect();
        (!messages.is_empty()).then_some(InboxView {
            label: label.to_string(),
            messages,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct QueueItemView {
    pub(crate) priority: u16,
    pub(crate) name: String,
    /// The file, as the handle that tells two entries of one name
    /// apart — not the path, which is this machine's business
    /// (the-queue-is-workable-from-the-phone).
    #[serde(rename = "handle", serialize_with = "stem_of")]
    pub(crate) path: PathBuf,
}

fn stem_of<S: serde::Serializer>(path: &Path, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&path.file_stem().unwrap_or_default().to_string_lossy())
}

/// Build the roster's auto-mode rows: master first, then reviewers in
/// tier order, deduped (an agent in both tiers — or matching master —
/// appears once). Each row's auto-mode is the EFFECTIVE mode (the
/// same `resolve_effective_auto_mode` the stop hook reads), so the
/// panel shows exactly what would govern that agent's next Stop.
fn roster_auto_rows(
    repo: &Path,
    home: Option<&Path>,
    set: &crate::cli::teams_config::RegisteredSet,
) -> Vec<AgentAutoRow> {
    use crate::cli::teams_config::RosterRole;
    // Carry the TIER (Master/Commit/Gate), not the collapsed
    // Master/Reviewer — the two reviewer sets already distinguish it, so
    // the panel can show the kinds for free. The per-agent
    // `AgentDescription` (tool/launch/initial_prompt) rides along for the
    // detail page.
    let roster = std::iter::once((&set.master, RosterRole::Master, &set.master_desc))
        .chain(set.members.iter().map(|r| (&r.label, r.role, &r.desc)));
    let mut seen = std::collections::HashSet::new();
    let mut rows = Vec::new();
    for (label, role, desc) in roster {
        if !seen.insert(label.as_str()) {
            continue;
        }
        let cfg = crate::agent_store::load_agent_config(repo, label)
            .ok()
            .flatten();
        let auto_mode = crate::cli::team::resolve_effective_auto_mode(cfg.as_ref(), home);
        let session = cfg.as_ref().and_then(|c| c.session.clone());
        rows.push(AgentAutoRow {
            label: label.as_str().to_string(),
            role,
            auto_mode,
            tool: desc.tool.as_str().to_string(),
            invocation: agent_invocation(desc),
            session,
            attending: crate::cli::stop_hook::read_attended(
                &crate::agent_store::agents_root(repo).join(label.as_str()),
            ),
        });
    }
    rows
}

/// Global-library agents (`~/.clank` `agents`) not already on `roster`
/// — the "+ add" candidates, each with its tool. Read FRESH from the
/// library by the TUI loop when the picker opens (never cached), so
/// a `clank agent add --global` made elsewhere shows up immediately.
/// Empty without a resolvable home or library (read-only degrade; the
/// picker then shows its empty-state hint).
pub(crate) fn available_agents(
    home: Option<&Path>,
    roster: &[AgentAutoRow],
) -> Vec<AvailableAgent> {
    let Some(home) = home else {
        return Vec::new();
    };
    let Ok(cfg) = crate::cli::team::read_user_config(home) else {
        return Vec::new();
    };
    let on_roster: std::collections::HashSet<&str> =
        roster.iter().map(|a| a.label.as_str()).collect();
    cfg.agents
        .iter()
        .filter(|(label, _)| !on_roster.contains(label.as_str()))
        .map(|(label, desc)| AvailableAgent {
            label: label.as_str().to_string(),
            tool: desc.tool.as_str().to_string(),
            invocation: agent_invocation(desc),
            description: desc.initial_prompt.clone(),
        })
        .collect()
}

/// One stashed plan as the renderers see it.
#[derive(serde::Serialize)]
pub(crate) struct StashItemView {
    /// The wire has called this `plan` since before it was a stash.
    #[serde(rename = "plan")]
    pub(crate) stem: String,
    pub(crate) waiting_for: Option<String>,
    /// True when `waiting_for` names a plan that has finished —
    /// the pop nudge.
    pub(crate) ready: bool,
    /// How many commits the stash holds (the pop size).
    pub(crate) commits: usize,
}

/// The shapes the wire fixed before the snapshot serialized itself.
/// Each is here because a default would change what a consumer parses,
/// and nothing else about the model is written down twice
/// (codex on 772b84f).
mod dirty_wire {
    /// One field, two keys: the flag, and the counts only when dirty.
    pub(super) fn serialize<S: serde::Serializer>(
        dirty: &Option<super::DirtyStats>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(None)?;
        m.serialize_entry("worktree_dirty", &dirty.is_some())?;
        if let Some(d) = dirty {
            m.serialize_entry("dirty_stats", d)?;
        }
        m.end()
    }
}

mod queue_wire {
    /// One field, three keys: the count and the names consumers read,
    /// and the entries that order them and tell two of one name apart.
    pub(super) fn serialize<S: serde::Serializer>(
        queue: &[super::QueueItemView],
        s: S,
    ) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(None)?;
        if !queue.is_empty() {
            m.serialize_entry("queue_count", &queue.len())?;
            m.serialize_entry(
                "queue",
                &queue.iter().map(|q| q.name.as_str()).collect::<Vec<_>>(),
            )?;
            m.serialize_entry("queue_entries", queue)?;
        }
        m.end()
    }
}

/// Nought or one, as a list: the key has always been an array.
fn as_list<T: serde::Serialize, S: serde::Serializer>(
    one: &Option<T>,
    s: S,
) -> Result<S::Ok, S::Error> {
    s.collect_seq(one.iter())
}

fn plans_wire<S: serde::Serializer>(
    plans: &[clank_core::wait::PlanWorkState],
    s: S,
) -> Result<S::Ok, S::Error> {
    s.collect_seq(plans.iter().map(Planned))
}

fn blocks_wire<S: serde::Serializer>(
    blocks: &[crate::cli::block::BlockEntry],
    s: S,
) -> Result<S::Ok, S::Error> {
    s.collect_seq(blocks.iter().map(Answerable))
}

fn correction_wire<S: serde::Serializer>(
    correction: &Option<clank_core::wait::HeadCorrection>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match correction {
        Some(c) => serde::Serialize::serialize(&Corrected(c), s),
        None => s.serialize_none(),
    }
}

/// A plan as the wire says it: two of its keys were named otherwise
/// before this was one model, and renaming them would break every
/// consumer to tidy a word. The rest is the model's own, so a field
/// added to `PlanWorkState` stops this compiling rather than going
/// missing.
struct Planned<'a>(&'a clank_core::wait::PlanWorkState);

impl serde::Serialize for Planned<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let clank_core::wait::PlanWorkState {
            plan,
            handover,
            sha,
            gate,
            waiting_on,
            touched_code,
        } = self.0;
        let mut m = s.serialize_map(Some(6))?;
        m.serialize_entry("plan", plan.as_str())?;
        // The wire's older names for these two.
        m.serialize_entry("latest_reviewable_sha", &sha.as_ref().map(|s| s.as_str()))?;
        m.serialize_entry("gate_state", gate)?;
        m.serialize_entry("waiting_on", waiting_on)?;
        m.serialize_entry("handover", handover)?;
        m.serialize_entry("touched_code", touched_code)?;
        m.end()
    }
}

/// A head correction as the wire says it: the violation's fields sit
/// beside the sha rather than under it, which is the shape consumers
/// parse. Exhaustive at both levels.
struct Corrected<'a>(&'a clank_core::wait::HeadCorrection);

impl serde::Serialize for Corrected<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let clank_core::wait::HeadCorrection { sha, violation } = self.0;
        let clank_core::wait::HeadTagViolation {
            unknown,
            untagged_touched,
            extra_named,
        } = violation;
        let mut m = s.serialize_map(Some(4))?;
        m.serialize_entry("sha", sha.as_str())?;
        m.serialize_entry("unknown", unknown)?;
        m.serialize_entry(
            "untagged_touched",
            &untagged_touched
                .iter()
                .map(PlanKey::as_str)
                .collect::<Vec<_>>(),
        )?;
        m.serialize_entry(
            "extra_named",
            &extra_named.iter().map(PlanKey::as_str).collect::<Vec<_>>(),
        )?;
        m.end()
    }
}

/// A block as the wire says it: its own fields, and the `pending` the
/// wire has always computed from the answer.
struct Answerable<'a>(&'a crate::cli::block::BlockEntry);

impl serde::Serialize for Answerable<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let crate::cli::block::BlockEntry {
            agent,
            name,
            question,
            answer,
        } = self.0;
        let mut m = s.serialize_map(Some(5))?;
        m.serialize_entry("agent", agent)?;
        m.serialize_entry("name", name)?;
        m.serialize_entry("question", question)?;
        m.serialize_entry("answer", answer)?;
        m.serialize_entry("pending", &answer.is_none())?;
        m.end()
    }
}

/// In-process convenience for tests / callers that want a status
/// snapshot from just `(repo, home)`: cache enabled, no plan
/// filter, not watch mode. Hides `CachePolicy`/basename plumbing.
/// Plan: dogfood-init-setup-in-tests (Phase B).
pub async fn snapshot(repo: &Path, home: Option<&Path>) -> anyhow::Result<StatusSnapshot> {
    let basename = repo_basename(repo)?;
    StatusSnapshot::build_async(
        repo,
        &basename,
        home,
        crate::rebuild::CachePolicy::Use,
        None,
        false,
    )
    .await
}

impl StatusSnapshot {
    /// Build the status snapshot. `home` is explicit (not read
    /// from `$HOME`) so in-process callers — tests + the `clank
    /// status` shell — control which user-scope config layers in.
    /// Plan: dogfood-init-setup-in-tests (Phase B).
    pub async fn build_async(
        repo: &Path,
        basename: &str,
        home: Option<&Path>,
        policy: crate::rebuild::CachePolicy,
        plan_arg: Option<&str>,
        watch_mode: bool,
    ) -> anyhow::Result<Self> {
        let state = crate::rebuild::rebuild_repo_with_policy(repo, policy)
            .await
            .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))?;
        let log = recent_log_rows(repo, &state).await;
        Self::from_state(
            repo,
            basename,
            home,
            &state,
            plan_arg,
            watch_mode,
            log.rows,
            log.github_events,
            log.next,
        )
    }

    fn from_state(
        repo: &Path,
        basename: &str,
        home: Option<&Path>,
        state: &RepoState,
        plan_arg: Option<&str>,
        watch_mode: bool,
        log_rows: Vec<crate::cli::log::OnelineRow>,
        github_events: Vec<crate::cli::github::timeline::MergedEvent>,
        log_next: Option<crate::cli::log::HistoryCursor>,
    ) -> anyhow::Result<Self> {
        // One ODB handle for the whole snapshot: HEAD facts, the dirty
        // walk, the commit-tag HEAD read, and every per-plan worktree
        // diff share it (no per-read/per-plan re-open). Opaque
        // `git_io::Repo` so this stays off the gix boundary
        // (gix-not-git-gate).
        let git = crate::git_io::open(repo)?;
        let (branch, head_sha, head_subject) = head_info(&git);
        let dirty = git.working_tree_dirty()?;

        let config = crate::cli::config::load_with_home(repo, home);
        // Plan: teams-based-agent-registration. `status` is a
        // read-only renderer (also reused by `clank html`), so it
        // DEGRADES on a team-less / misconfigured repo: no team →
        // empty reviewer tiers → gate computes as zero-reviewer
        // (Continued). It never hard-errors the way the
        // workflow-driving commands (wait / finish / promote) do.
        let registered = crate::agent_store::try_resolve_via_team_with(repo, home)
            .ok()
            .flatten();
        let master = registered
            .as_ref()
            .map(|set| set.master.as_str().to_string());
        // The roster as the agent panel sees it: master first, then
        // reviewers (deduped across the two tiers and against master),
        // each carrying its EFFECTIVE auto-mode. Built here so the
        // snapshot — not an ad-hoc render-time read — is the single
        // input the nothing-changed gate covers.
        let agents = registered
            .as_ref()
            .map(|set| roster_auto_rows(repo, home, set))
            .unwrap_or_default();
        let inboxes = agents
            .iter()
            .filter_map(|a| InboxView::read(repo, &a.label))
            .collect();
        let tiers = registered
            .as_ref()
            .map(crate::agent_store::ReviewerTiers::from_registered)
            .unwrap_or_else(crate::agent_store::ReviewerTiers::empty);
        let work_policy = clank_core::wait::WorkPolicy {
            plan_feedback: config.review.plan_feedback,
            adhoc_feedback: config.review.adhoc_feedback,
            commit_reviewers: tiers.commit,
            plan_reviewers: tiers.plan,
            final_reviewers: tiers.final_,
        };
        let reviews = crate::fs_plan_state_lookup::FsPlanStateLookup::with_handle(
            repo,
            state.head.as_ref(),
            &git,
        );
        // HEAD facts feed the commit-tag invariant
        // (commit-tag-fixup-is-first-class-state): a violation renders
        // as the dominating `MasterToFixCommitTag` correction.
        let head = git.head_commit(state);
        let work_status = state
            .fold
            .derive_status(&reviews, &work_policy, head.as_ref());

        let (plans, last_finished) = select_plans_and_finished(
            &work_status.plans,
            &state.fold,
            basename,
            plan_arg,
            watch_mode,
        )?;

        let blocks = crate::cli::block::scan_blocks(repo);
        // scan_queue returns entries sorted by (priority, name);
        // capture the names so the snapshot stays the single source
        // of truth for the queue (count derives from it).
        let queue: Vec<QueueItemView> = crate::cli::queue::scan_queue(repo)
            .into_iter()
            .map(|e| QueueItemView {
                priority: e.priority,
                name: e.name,
                path: e.path,
            })
            .collect();

        let forks: Vec<(String, &'static str)> = crate::cli::fork::list_forks(repo)
            .unwrap_or_default()
            .into_iter()
            .map(|(kind, name, _)| {
                (
                    name,
                    match kind {
                        crate::cli::fork::ForkKind::Clone => "clone",
                        crate::cli::fork::ForkKind::Worktree => "worktree",
                    },
                )
            })
            .collect();

        let stash: Vec<StashItemView> = crate::cli::stash::scan_stash(repo)
            .into_iter()
            .map(|(stem, st)| {
                let ready = st.waiting_for.as_deref().is_some_and(|w| {
                    state
                        .fold
                        .finished_plans
                        .iter()
                        .any(|fp| fp.plan.as_str() == w)
                });
                StashItemView {
                    stem,
                    waiting_for: st.waiting_for,
                    ready,
                    commits: st.shas.len(),
                }
            })
            .collect();

        let log_decorations = log_decorations(&git, branch.as_deref());

        Ok(Self {
            inboxes,
            forks,
            repo_path: repo.to_path_buf(),
            basename: basename.to_string(),
            branch,
            head_sha,
            head_subject,
            dirty,
            plans,
            last_finished,
            blocks,
            queue,
            master,
            agents,
            stash,
            log_rows,
            log_adopted_at: state.fold.adopted_at.clone(),
            log_stands_for: stands_for_plans(state),
            log_next,
            github_events,
            log_decorations,
            pr_reviews: work_status.pr_reviews,
            ad_hoc: work_status.ad_hoc,
            head_correction: work_status.head_correction,
        })
    }

    /// Test accessor for the TUI log lines (integration tests live
    /// in a separate crate; the field stays crate-private).
    pub fn log_lines_for_test(&self) -> Vec<String> {
        crate::cli::log::oneline_plain_lines(&self.log_rows)
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("the snapshot serializes")
    }

    pub fn to_human(&self) -> String {
        let mut out = String::new();
        use std::fmt::Write as _;

        let _ = writeln!(out, "repo:   {}", self.repo_path.display());
        if let Some(b) = &self.branch {
            let _ = writeln!(out, "branch: {b}");
        }
        if let Some(s) = &self.head_sha {
            let _ = writeln!(out, "head:   {s}");
        }
        // A broken HEAD commit tag dominates: shown even when no active
        // plan row carries it (unknown-tag-only — codex 2bf46d9).
        if let Some(c) = &self.head_correction {
            let _ = writeln!(
                out,
                "fix-tag: HEAD {} — amend the tag ({})",
                c.sha.as_str(),
                describe_head_violation(&c.violation)
            );
        }
        let _ = writeln!(
            out,
            "dirty:  {}",
            match &self.dirty {
                Some(d) => format!("yes ({})", dirty_summary(d)),
                None => "no".to_string(),
            }
        );
        if !self.queue.is_empty() {
            let _ = writeln!(
                out,
                "queue:  {} item{}",
                self.queue.len(),
                if self.queue.len() == 1 { "" } else { "s" }
            );
        }
        for (name, kind) in &self.forks {
            let _ = writeln!(out, "fork:   {name} · {kind}");
        }
        for sv in &self.stash {
            let note = match (&sv.waiting_for, sv.ready) {
                (Some(w), true) => format!(" (was waiting on {w} — FINISHED; pop?)"),
                (Some(w), false) => format!(" (waiting on {w})"),
                (None, _) => String::new(),
            };
            let _ = writeln!(out, "stashed: {} · {} commit(s){note}", sv.stem, sv.commits);
        }
        for pr in &self.pr_reviews {
            let url = pr_url(&pr.repo, pr.pr);
            if pr.round == 0 {
                // Not opened yet: master is drafting, no reviewers.
                let _ = writeln!(out, "pr #{}: round 0 — master drafting", pr.pr);
            } else {
                let waiting = if pr.handoff_stopped() {
                    // Master was deliberately taken off this: saying
                    // it is waiting on them shows actionable work
                    // that nobody is coming to do.
                    "nobody — handoff blocked, run `clank pr-review finish`".to_string()
                } else if pr.missing_reviewers.is_empty() {
                    "master".to_string()
                } else {
                    pr.missing_reviewers
                        .iter()
                        .map(|l| l.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let _ = writeln!(
                    out,
                    "pr #{}: round {} ({}) — waiting on {waiting}",
                    pr.pr,
                    pr.round,
                    pr.gate.as_str(),
                );
            }
            let _ = writeln!(out, "  {url}");
        }
        for row in &self.agents {
            if let Some(a) = &row.attending {
                let _ = writeln!(
                    out,
                    "attending: {} → {}",
                    row.label,
                    a.summary(time::OffsetDateTime::now_utc())
                );
            }
        }

        if self.plans.is_empty() && self.last_finished.is_none() && self.pr_reviews.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(out, "no active plan, nothing pending");
        }

        for v in &self.plans {
            let _ = writeln!(out);
            let _ = writeln!(out, "plan: {}", v.plan.as_str());
            // Omit `latest reviewable:` line when sha is None (blocked
            // plan with no reviewable commit yet — e.g. intro-only).
            if let Some(sha) = &v.sha {
                let _ = writeln!(out, "  latest reviewable: {}", short_sha(sha.as_str()));
            }
            // `BLOCKED` uppercase on the gate line is a renderer-only
            // emphasis for the blocked state per lloyd's "screaming
            // loudly" directive. Other gates stay lowercase via
            // CommitGateState's Display impl. Wire form
            // (CommitGateState::as_str) remains lowercase "blocked".
            let gate_display = if matches!(v.gate, CommitGateState::Blocked) {
                "BLOCKED".to_string()
            } else {
                v.gate.to_string()
            };
            let _ = writeln!(out, "  gate:              {gate_display}");
            let _ = writeln!(out, "  waiting on:        {}", waiting_actor(&v.waiting_on));
            let _ = writeln!(
                out,
                "  reason:            {}",
                waiting_reason(&v.waiting_on)
            );
        }

        if self.plans.is_empty() {
            if let Some(fp) = &self.last_finished {
                let _ = writeln!(out);
                let _ = writeln!(
                    out,
                    "last finished: {} (finalized {})",
                    fp.plan.as_str(),
                    short_sha(fp.finalized_at.as_str())
                );
            }
        }

        if !self.blocks.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(out, "blocks:");
            for b in &self.blocks {
                if let Some(ref answer) = b.answer {
                    let _ = writeln!(out, "  UNBLOCKED ({}): {}", b.agent, b.question);
                    let _ = writeln!(out, "    answer: {answer}");
                } else {
                    let _ = writeln!(out, "  BLOCKED ({}): {}", b.agent, b.question);
                }
            }
        }

        out.trim_end_matches('\n').to_string()
    }

    fn to_json_compact(&self) -> String {
        serde_json::to_string(&self.to_json()).unwrap_or_default()
    }
}

pub async fn run(args: StatusArgs) -> anyhow::Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let basename = repo_basename(&repo)?;
    let policy = if args.no_cache {
        crate::rebuild::CachePolicy::Bypass
    } else {
        crate::rebuild::CachePolicy::Use
    };

    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);

    if args.tui {
        // The flag outgrew the command; the name says so once, and the
        // panes of sessions opened before the rename keep working.
        eprintln!("`clank status --tui` is `clank tui` now");
        return crate::cli::status_tui::run_tui(repo, basename, home, policy).await;
    }

    if args.watch {
        return run_watch(
            repo,
            basename,
            home,
            policy,
            args.json,
            args.plan.as_deref(),
        )
        .await;
    }

    let snapshot = StatusSnapshot::build_async(
        &repo,
        &basename,
        home.as_deref(),
        policy,
        args.plan.as_deref(),
        false,
    )
    .await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&snapshot.to_json())?);
    } else {
        print!("{}", snapshot.to_human());
        println!();
    }
    Ok(())
}

/// The watch loop's failure policy (tui-transient-refresh-resilience,
/// codex 697cdb1): BEFORE the first successful snapshot there is no
/// prior output to retain — a probe/build failure is FATAL, exactly
/// like the one-shot path. AFTER one success, failures degrade to a
/// stderr warning with the last output kept; the caller leaves the
/// signature untouched, so the SAME state retries on the next event
/// or the 60s backstop (recovery is the ordinary success path).
fn watch_failure(bootstrapped: bool, stage: &str, e: impl std::fmt::Display) -> anyhow::Result<()> {
    if bootstrapped {
        eprintln!("status watch: {stage} failed ({e}); kept last output, retrying");
        Ok(())
    } else {
        Err(anyhow::anyhow!("status watch: {stage} failed: {e}"))
    }
}

async fn run_watch(
    repo: std::path::PathBuf,
    basename: String,
    home: Option<std::path::PathBuf>,
    policy: crate::rebuild::CachePolicy,
    json: bool,
    plan_arg: Option<&str>,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<()>();
    let _watcher = watch_status_paths(tx, &repo)?;

    let mut last_emitted: Option<String> = None;
    let mut last_sig: Option<InputSignature> = None;

    loop {
        // Nothing-changed gate (lloyd's invariant): only rebuild when an
        // input the snapshot depends on actually changed. The signature
        // probe is far cheaper than the fold + render it guards.
        // Transient probe/rebuild failures must not kill a
        // BOOTSTRAPPED watch (see [`watch_failure`] — the first
        // snapshot stays fatal): keep the last output, do NOT
        // advance the signature, and let the next event or the 60s
        // backstop retry the SAME state.
        match input_signature(&repo) {
            Ok(sig) if last_sig.as_ref() != Some(&sig) => {
                match StatusSnapshot::build_async(
                    &repo,
                    &basename,
                    home.as_deref(),
                    policy,
                    plan_arg,
                    true,
                )
                .await
                {
                    Ok(snapshot) => {
                        let output = if json {
                            snapshot.to_json_compact()
                        } else {
                            snapshot.to_human()
                        };

                        if last_emitted.as_deref() != Some(&output) {
                            let stdout = std::io::stdout();
                            let mut out = stdout.lock();
                            if last_emitted.is_some() {
                                writeln!(out)?;
                            }
                            writeln!(out, "{output}")?;
                            out.flush()?;
                            last_emitted = Some(output);
                        }
                        last_sig = Some(sig);
                    }
                    Err(e) => watch_failure(last_sig.is_some(), "refresh", format!("{e:#}"))?,
                }
            }
            Ok(_) => {}
            Err(e) => watch_failure(last_sig.is_some(), "input probe", format!("{e:#}"))?,
        }

        // Event-driven: worktree edits, .clank writes, and git-dir
        // changes all arrive as watcher events now, so no fast
        // poll. The long timeout is a backstop against watcher
        // pathologies the error channel doesn't surface.
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(()) => while rx.recv_timeout(Duration::from_millis(200)).is_ok() {},
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("filesystem watcher disconnected")
            }
        }
    }
}

/// Recent activity for the TUI's log pane: a bounded range-fold
/// (last `LOG_WINDOW` commits; gix-backed reads, no per-event
/// subprocess — ruthless 54c37f6 concern 1) rendered through the
/// shared `clank log --oneline` row producer to PLAIN lines (the
/// TUI styles them). Scope: repo-wide recent activity, ALWAYS — the
/// pane shows the same window regardless of how many plans are active
/// (status-tui-unified-log; the old single-active-plan narrowing made
/// a just-finished plan's commits vanish the moment one plan was
/// active). Best-effort: any failure yields an empty pane, never a
/// status error.
async fn recent_log_rows(repo: &Path, state: &RepoState) -> TuiLogRead {
    const LOG_WINDOW: usize = 30;
    match state.head.clone() {
        Some(head) => {
            log_rows_windowed(
                repo,
                &head,
                LOG_WINDOW,
                state.fold.adopted_at.as_ref(),
                true,
                &stands_for_plans(state),
            )
            .await
        }
        None => TuiLogRead::default(),
    }
}

/// sha → branch names whose tips sit on that commit, for the TUI log
/// (tui-log-branch-decorations). The relevant set — HEAD's branch, its
/// configured upstream, the main worktree's branch (the base a linked
/// worktree split from) and that base's `origin/…` counterpart — is a
/// handful of local gix reads on the snapshot's existing handle: no
/// subprocess, no network, nothing per-commit. Names dedupe in display
/// order (on the main repo the base IS HEAD's branch).
fn log_decorations(
    git: &crate::git_io::Repo,
    head_branch: Option<&str>,
) -> std::collections::BTreeMap<String, Vec<String>> {
    fn push(names: &mut Vec<String>, n: String) {
        if !n.is_empty() && !names.contains(&n) {
            names.push(n);
        }
    }
    let mut names: Vec<String> = Vec::new();
    if let Some(b) = head_branch {
        push(&mut names, b.to_string());
        if let Some(up) = git.upstream_short_name(b) {
            push(&mut names, up);
        }
    }
    if let Some(base) = git.main_worktree_branch() {
        push(&mut names, base.clone());
        push(&mut names, format!("origin/{base}"));
    }
    let mut map = std::collections::BTreeMap::new();
    for (name, sha) in git.ref_tips(&names) {
        map.entry(sha.as_str().to_string())
            .or_insert_with(Vec::new)
            .push(name);
    }
    map
}

/// One bounded log read plus the cursor for the next older fixed-size page.
#[derive(Default)]
pub(crate) struct TuiLogRead {
    pub(crate) rows: Vec<crate::cli::log::OnelineRow>,
    pub(crate) github_events: Vec<crate::cli::github::timeline::MergedEvent>,
    pub(crate) next: Option<crate::cli::log::HistoryCursor>,
}

/// One additional fixed-size page, rooted at `cursor`. Github events are
/// carried by the initial/current-window read and are deliberately absent
/// here, so appending a Git page cannot duplicate event rows.
pub(crate) async fn tui_log_page(
    repo: &Path,
    cursor: &crate::cli::log::HistoryCursor,
    page_size: usize,
    adopted_at: Option<&crate::lifecycle::CommitSha>,
    stands_for: &crate::cli::log::StandsFor,
) -> TuiLogRead {
    log_rows_page(repo, cursor, page_size, adopted_at, false, stands_for).await
}

/// Which finalize commits stand for their whole plan: a finished plan
/// whose intro IS its finalize has one commit, and one commit needs no
/// umbrella over it.
///
/// Read from the FOLD, so the answer does not depend on what any
/// window or chunk happens to show — a plan whose earlier commits fell
/// outside the page must not look like a plan that never had them
/// (codex on d420716).
pub(crate) fn stands_for_plans(state: &RepoState) -> crate::cli::log::StandsFor {
    state
        .fold
        .finished_plans
        .iter()
        .filter(|f| f.intro == f.finalized_at)
        .map(|f| {
            (
                f.finalized_at.as_str().to_string(),
                f.plan.as_str().to_string(),
            )
        })
        .collect()
}

/// Rows AND the index-aligned merged events — the TUI sets both on
/// the snapshot together (tui-github-event-page).
pub(crate) async fn tui_log_with_events(
    repo: &Path,
    window: usize,
    adopted_at: Option<&crate::lifecycle::CommitSha>,
    stands_for: &crate::cli::log::StandsFor,
) -> TuiLogRead {
    match crate::cli::log::git_rev_parse(repo, "HEAD") {
        Some(head) => log_rows_windowed(repo, &head, window, adopted_at, true, stands_for).await,
        None => TuiLogRead::default(),
    }
}

/// Build oneline log rows for `HEAD~window..head`, newest first.
async fn log_rows_windowed(
    repo: &Path,
    head: &crate::lifecycle::CommitSha,
    window: usize,
    adopted_at: Option<&crate::lifecycle::CommitSha>,
    include_github: bool,
    stands_for: &crate::cli::log::StandsFor,
) -> TuiLogRead {
    let cursor = crate::cli::log::HistoryCursor {
        tip: head.clone(),
        post_adoption: adopted_at.is_some(),
    };
    log_rows_page(
        repo,
        &cursor,
        window,
        adopted_at,
        include_github,
        stands_for,
    )
    .await
}

async fn log_rows_page(
    repo: &Path,
    cursor: &crate::cli::log::HistoryCursor,
    window: usize,
    adopted_at: Option<&crate::lifecycle::CommitSha>,
    include_github: bool,
    stands_for: &crate::cli::log::StandsFor,
) -> TuiLogRead {
    use clank_core::repo_state::LogEvent;

    let Ok(history) = crate::cli::log::history_page(repo, cursor, window, adopted_at).await else {
        return TuiLogRead::default();
    };
    let events = history.folded;

    let reviewable: Vec<crate::lifecycle::CommitSha> = events
        .iter()
        .filter_map(|e| match e {
            LogEvent::PlanCommit { sha, .. }
            | LogEvent::PlanIntro { sha, .. }
            | LogEvent::AdHoc { sha, .. } => Some(sha.clone()),
            _ => None,
        })
        .collect();
    let reviews = crate::cli::log::collect_reviews(repo, &reviewable);
    // Newest first, like `clank log` — the pane reads top-down.
    let newest_first: Vec<&LogEvent> = events.iter().rev().collect();
    // Github events interleave by time, same layer as `clank log`
    // (log-timeline-github-events); read notices lead the pane as dim
    // rows — the TUI's rendering of the snapshot's diagnostics.
    let snap = if include_github {
        crate::cli::github::timeline::timeline_snapshot(repo)
    } else {
        crate::cli::github::timeline::TimelineSnapshot::default()
    };
    let items = crate::cli::log::interleave_with_plain(&newest_first, &history.plain, &snap.events);
    let mut rows: Vec<crate::cli::log::OnelineRow> = snap
        .notices
        .iter()
        .map(|n| crate::cli::log::OnelineRow::Notice(n.clone()))
        .collect();
    rows.extend(crate::cli::log::oneline_items_rows(
        &items, &reviews, stands_for,
    ));
    TuiLogRead {
        rows,
        github_events: snap.events,
        next: history.next,
    }
}

/// Decides which filesystem events wake the status loops. This is an
/// ALLOWLIST, not "anything under `.clank`" (status-tui-watch-cpu):
/// wakes on
/// - the workflow-state signal dirs under `.clank` ([`CLANK_WAKE_DIRS`]:
///   plans/queue/blocks/agents/finished/config) — feedback/queue/agent
///   state are load-bearing even though gitignored;
/// - anything under the git dir (HEAD moves, ref updates);
/// - any `.gitignore` change (which also refreshes the matcher);
/// - any worktree path the gitignore rules do NOT match (keeps
///   `dirty:` fresh on tracked edits).
///
/// It deliberately does NOT wake on the rest of `.clank`: the derived
/// fold `cache`, generated `html`, the `zellij` layout, and — the big
/// one — nested worktrees under `.clank/worktrees/<name>/`, which are
/// whole separate repos whose builds + caches would otherwise wake
/// this pane. Gitignore-matched worktree paths (`target/`, `*.log`)
/// are dropped too — a `cargo build`'s thousands of `target/` files
/// say nothing about clank state or worktree dirt.
///
/// The ignore test is git-accurate ([`crate::git_io::PathIgnore`], gix's
/// exclude stack): it honors NESTED `.gitignore` files (plus
/// `.git/info/exclude` and `core.excludesFile`), so churn under a
/// subproject's ignored dir (e.g. a Flutter `frostsnapp/build/`) is
/// recognized as ignored and dropped — not stormed on. (A root-only
/// matcher missed nested ignores and woke ~250×/s on such a tree.)
/// TRACKED-but-gitignored files (`git add -f`) are dropped like
/// any ignored path: their edits refresh `dirty:` on the 60s
/// backstop, not instantly.
pub(crate) struct WakeFilter {
    repo_root: PathBuf,
    git_dir: PathBuf,
    clank_root: PathBuf,
    ignore: crate::git_io::PathIgnore,
}

// The `.clank/` workflow-state signal dirs the snapshot depends on —
// the SHARED allowlist the core watcher wakes on. One source of truth
// in `repo_watch`, so the reuse fingerprint and the wake rule agree.
use crate::repo_watch::CLANK_WAKE_DIRS;

/// A cheap fingerprint of everything a [`StatusSnapshot`] is derived
/// from — the SAME inputs the [`WakeFilter`] wakes on: committed
/// history (HEAD), the working tree (`dirty`), and the gitignored
/// `.clank` workflow dirs ([`CLANK_WAKE_DIRS`]). The watch loops reuse
/// the previous snapshot while this is unchanged, so a wake that
/// touched nothing the snapshot depends on costs only this probe, not
/// a rebuild + render.
///
/// Keyed on the FULL set, NOT just HEAD+dirty (ruthless): a review
/// verdict lands as a gitignored `.clank/agents/<label>/feedback/<sha>`
/// that changes neither HEAD nor the working tree, yet must refresh the
/// pane — exactly what the master watches the TUI for. A reuse key
/// narrower than the wake set would hide such changes; this key IS the
/// wake set ([`CLANK_WAKE_DIRS`] is the shared source of truth).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InputSignature {
    head: Option<String>,
    dirty: Option<crate::git_io::DirtyStats>,
    clank: u64,
    /// Liveness of recorded attended tasks, which can change without
    /// a file write when the task process exits.
    attendances: u64,
}

/// Compute the current [`InputSignature`]. One ODB open (HEAD + the
/// dirty walk); the `.clank` fingerprint is stat-only (no file reads).
pub(crate) fn input_signature(repo: &Path) -> anyhow::Result<InputSignature> {
    let git = crate::git_io::open(repo)?;
    let head = git.head_sha()?.map(|s| s.as_str().to_string());
    let dirty = git.working_tree_dirty()?;
    Ok(InputSignature {
        head,
        dirty,
        clank: clank_input_fingerprint(repo),
        attendances: attendance_fingerprint(repo),
    })
}

/// Hash of every agent's reachability verdict.
///
/// Reads only: a small claim file, a small marker, the park holder,
/// and `kill(pid, 0)` per recorded pid. It never takes the park's
/// lock — a reader that takes a writer's lock to ask who holds it can
/// make a parking hook defer to a waiter that does not exist.
pub(crate) fn attendance_fingerprint(repo: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let Ok(entries) = std::fs::read_dir(crate::agent_store::agents_root(repo)) else {
        return 0;
    };
    let mut labels: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    labels.sort();
    for name in labels {
        let Some(attended) = crate::cli::stop_hook::read_attended(
            &crate::agent_store::agents_root(repo).join(&name),
        ) else {
            continue;
        };
        name.hash(&mut h);
        format!("{attended:?}").hash(&mut h);
        attended
            .pid
            .zip(attended.token.as_ref())
            .is_some_and(|(pid, token)| token.still_holds(pid))
            .hash(&mut h);
    }
    h.finish()
}

/// Stat-only hash of (relative path, mtime) over the workflow-state
/// paths the [`WakeFilter`] wakes on ([`CLANK_WAKE_DIRS`]). A new,
/// edited, or removed feedback / block / queue entry / plan / finished
/// marker / config flips it. Recursive but cheap — paths + mtimes, no
/// file contents.
fn clank_input_fingerprint(repo: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let clank = repo.join(".clank");
    let mut entries: Vec<(PathBuf, Option<std::time::SystemTime>)> = Vec::new();
    for dir in CLANK_WAKE_DIRS {
        collect_fingerprint(&clank.join(dir), &mut entries);
    }
    // readdir order isn't stable; sort for a deterministic hash.
    entries.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (path, mtime) in &entries {
        path.hash(&mut hasher);
        match mtime.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()) {
            Some(d) => d.as_nanos().hash(&mut hasher),
            None => 0u8.hash(&mut hasher),
        }
    }
    hasher.finish()
}

fn collect_fingerprint(path: &Path, out: &mut Vec<(PathBuf, Option<std::time::SystemTime>)>) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return; // absent path contributes nothing (its removal flips the set)
    };
    if meta.is_dir() {
        if let Ok(rd) = std::fs::read_dir(path) {
            for entry in rd.flatten() {
                collect_fingerprint(&entry.path(), out);
            }
        }
    } else {
        out.push((path.to_path_buf(), meta.modified().ok()));
    }
}

impl WakeFilter {
    pub(crate) fn new(repo_root: &Path, git_dir: &Path) -> anyhow::Result<Self> {
        // FSEvents delivers canonical paths (`/private/var/…`);
        // compare against canonical roots or every prefix check
        // misses on symlinked locations (e.g. macOS tempdirs).
        let repo_root = dunce::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
        let git_dir = dunce::canonicalize(git_dir).unwrap_or_else(|_| git_dir.to_path_buf());
        let ignore = crate::git_io::open_ignore(&repo_root)?;
        Ok(Self {
            clank_root: repo_root.join(".clank"),
            ignore,
            repo_root,
            git_dir,
        })
    }

    /// True → the working-tree event at `path` should wake the DIFF
    /// loop (refresh `dirty:`/diff).
    pub(crate) fn wakes(&mut self, path: &Path) -> bool {
        // Native FSEvents paths are canonical, but notify's polling
        // backend can report `/var/…` for a root canonicalized to
        // `/private/var/…`. Keep the filter's comparison in one path
        // namespace before treating a path as outside the repo.
        let canonical;
        let path = if path.starts_with(&self.repo_root) {
            path
        } else {
            canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            &canonical
        };
        // Gate-state changes — the gitdir (commits/refs) AND anything
        // under `.clank/` — are the CORE watcher's job
        // (`repo_watch::is_core_wake`). The diff watcher refreshes
        // dirty/diff from the WORKING TREE only, so it drops both: status
        // runs both watchers on one channel, and dropping here keeps the
        // diff side from double-waking on a gate change the core already
        // delivered.
        if path.starts_with(&self.git_dir) || path.starts_with(&self.clank_root) {
            return false;
        }
        // A `.gitignore` write changes what's ignored: rebuild the
        // (nested-aware) exclude stack and wake. On a rebuild error keep
        // the old stack — erring toward waking is safe.
        if path.file_name().is_some_and(|n| n == ".gitignore") {
            if let Ok(ig) = crate::git_io::open_ignore(&self.repo_root) {
                self.ignore = ig;
            }
            return true;
        }
        // Paths outside the root shouldn't arrive; if one does, wake
        // conservatively.
        let Ok(rela) = path.strip_prefix(&self.repo_root) else {
            return true;
        };
        // Working-tree path: wake unless git-ignored — keeps `dirty:`
        // fresh on tracked edits while dropping build artifacts, honoring
        // NESTED `.gitignore`s (the storm fix). `is_dir` races with
        // deletion; a vanished path reads as non-dir, only loosening
        // toward a wake.
        !self.ignore.is_ignored(rela, path.is_dir())
    }
}

/// Watcher for the status loops: the repo root recursively (working
/// tree, `.clank`, and `.git` when embedded) plus the git dir when
/// it lives elsewhere (linked worktrees) — events filtered through
/// [`WakeFilter`]. Watcher ERRORS also wake: notify signals queue
/// overflow as an error event, and the right response is one cheap
/// rebuild, not silent staleness.
pub(crate) fn watch_status_paths(
    tx: mpsc::Sender<()>,
    repo: &Path,
) -> anyhow::Result<StatusWatchers> {
    // Gate state comes from the SHARED core watcher (the same producer
    // `clank wait` uses), so the panel and the stop-hook wake on the
    // same changes. `status` is always native (no poll mode).
    let core = crate::repo_watch::RepoStateWatcher::attach(repo, false, tx.clone())?;
    let diff = watch_diff_paths(tx, repo)?;
    Ok(StatusWatchers {
        _core: core,
        _diff: diff,
    })
}

/// The two watchers `status` keeps alive together. Dropping this stops
/// both. The SHARED core watcher feeds gate state; the status-only diff
/// watcher feeds dirty/diff. They send on one channel; the loop's
/// nothing-changed gate ([`InputSignature`]) coalesces.
pub(crate) struct StatusWatchers {
    _core: crate::repo_watch::RepoStateWatcher,
    _diff: RecommendedWatcher,
}

/// The status-only DIFF watcher: the WHOLE working tree, recursively,
/// waking on tracked-file edits to refresh `dirty:`/diff while dropping
/// gitignored churn ([`WakeFilter`] holds the nested-aware
/// `git_io::PathIgnore` — status-watch-nested-ignore). It is NEVER a
/// source of gate state: it drops `.clank/` and the gitdir (the core
/// watcher owns those). Watcher errors wake (one cheap rebuild beats
/// silent staleness).
fn watch_diff_paths(tx: mpsc::Sender<()>, repo: &Path) -> anyhow::Result<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(diff_wake_handler(tx, repo)?)?;
    watcher
        .watch(repo, RecursiveMode::Recursive)
        .map_err(|e| anyhow::anyhow!("watch `{}` failed: {e}", repo.display()))?;
    Ok(watcher)
}

/// One filter for both the native production watcher and the polling
/// test backend. The latter is necessary where the process can attach
/// an FSEvents watcher but the sandbox never delivers its events.
fn diff_wake_handler(
    tx: mpsc::Sender<()>,
    repo: &Path,
) -> anyhow::Result<impl FnMut(notify::Result<notify::Event>) + Send + 'static> {
    let git_dir = crate::repo_watch::git_state_dir(repo)?;
    let mut filter = WakeFilter::new(repo, &git_dir)?;
    Ok(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(event) => {
                // Path-less events (rescan notices) wake conservatively.
                if event.paths.is_empty() || event.paths.iter().any(|p| filter.wakes(p)) {
                    let _ = tx.send(());
                }
            }
            Err(_) => {
                let _ = tx.send(());
            }
        }
    })
}

/// Forward SIGWINCH into the wake channel so a terminal resize is
/// just another event. A forwarding thread, not a signal handler:
/// `mpsc::Sender::send` is not async-signal-safe. The thread
/// blocks in `forever()` and only notices a dropped receiver on
/// the next signal — it leaks until process exit, which is fine
/// for the once-per-process TUI.
pub(crate) fn spawn_sigwinch_forwarder(tx: mpsc::Sender<()>) -> anyhow::Result<()> {
    let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])?;
    std::thread::spawn(move || {
        for _ in signals.forever() {
            if tx.send(()).is_err() {
                break;
            }
        }
    });
    Ok(())
}

fn select_plans_and_finished(
    work_plans: &[PlanWorkState],
    fold: &clank_core::repo_state::RepoState,
    basename: &str,
    plan_arg: Option<&str>,
    watch_mode: bool,
) -> anyhow::Result<(Vec<PlanWorkState>, Option<FinishedPlan>)> {
    if let Some(raw) = plan_arg {
        let stem = parse_arg(raw, basename)?;
        let key = PlanKey::parse(&stem)
            .map_err(|e| anyhow::anyhow!("invalid plan stem `{stem}`: {e}"))?;

        let matching: Vec<PlanWorkState> = work_plans
            .iter()
            .filter(|ps| ps.plan == key)
            .cloned()
            .collect();

        if !matching.is_empty() {
            return Ok((matching, None));
        }

        if watch_mode {
            let last_finished = fold
                .finished_plans
                .iter()
                .rev()
                .find(|fp| fp.plan == key)
                .cloned();
            if last_finished.is_some() {
                return Ok((vec![], last_finished));
            }
        }

        anyhow::bail!(
            "plan `{basename}/{stem}.md` not active. active: {}",
            active_summary(fold, basename)
        );
    }

    let plans = work_plans.to_vec();
    let last_finished = if plans.is_empty() {
        fold.finished_plans.last().cloned()
    } else {
        None
    };
    Ok((plans, last_finished))
}

pub(crate) fn waiting_actor(w: &WaitingOn) -> String {
    match w {
        WaitingOn::Blocked { block } => block.creator.as_str().to_string(),
        WaitingOn::ReviewerApprovalsMissing { missing }
        | WaitingOn::GateReviewersMissing { missing } => missing
            .iter()
            .map(|a| a.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        WaitingOn::MasterToRevise { .. }
        | WaitingOn::MasterToContinue
        | WaitingOn::MasterToFinalize
        | WaitingOn::MasterToCommit
        | WaitingOn::MasterToFixCommitTag => "master".into(),
    }
}

/// Human one-liner for a broken HEAD commit tag
/// (commit-tag-fixup-is-first-class-state) — shared by `clank status`
/// and the TUI so the wording can't drift.
pub(crate) fn describe_head_violation(v: &clank_core::wait::HeadTagViolation) -> String {
    let plans = |ks: &[PlanKey]| ks.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", ");
    let mut parts = Vec::new();
    if !v.untagged_touched.is_empty() {
        parts.push(format!(
            "touches plan file(s) [{}] the tag omits",
            plans(&v.untagged_touched)
        ));
    }
    if !v.extra_named.is_empty() {
        parts.push(format!(
            "tag names [{}] whose plan file it didn't touch",
            plans(&v.extra_named)
        ));
    }
    if !v.unknown.is_empty() {
        parts.push(format!(
            "tag names unknown plan(s) [{}]",
            v.unknown.join(", ")
        ));
    }
    parts.join("; ")
}

/// Max chars of block-message body shown on the per-plan `reason:`
/// line. Full message stays in the bottom `blocks:` footer.
const BLOCK_REASON_MAX_LEN: usize = 80;

/// First line of a block message, trimmed + truncated to
/// `BLOCK_REASON_MAX_LEN` chars + `…` ellipsis if longer.
fn first_line(s: &str) -> String {
    let line = s.trim_start();
    let line = line.split('\n').next().unwrap_or("");
    if line.chars().count() <= BLOCK_REASON_MAX_LEN {
        line.to_string()
    } else {
        let truncated: String = line.chars().take(BLOCK_REASON_MAX_LEN).collect();
        format!("{truncated}…")
    }
}

pub(crate) fn waiting_reason(w: &WaitingOn) -> String {
    match w {
        WaitingOn::Blocked { block } => format!("blocked: {}", first_line(&block.message)),
        WaitingOn::ReviewerApprovalsMissing { missing } => {
            let names = missing
                .iter()
                .map(|a| a.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("missing approval from {names}")
        }
        WaitingOn::GateReviewersMissing { missing } => {
            let names = missing
                .iter()
                .map(|a| a.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("commit-tier reviewers continued; waiting on gate-tier {names}")
        }
        WaitingOn::MasterToRevise {
            requesters,
            ambiguous,
        } => {
            let mut parts = Vec::new();
            if !requesters.is_empty() {
                let n = requesters
                    .iter()
                    .map(|a| a.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                parts.push(format!("changes requested by {n}"));
            }
            if !ambiguous.is_empty() {
                let n = ambiguous
                    .iter()
                    .map(|a| a.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                parts.push(format!("ambiguous verdict from {n}"));
            }
            parts.join("; ")
        }
        WaitingOn::MasterToContinue => {
            "gate continued (not FINISHED) — continue work or ask a reviewer to mark FINISHED"
                .into()
        }
        WaitingOn::MasterToFinalize => "gate FINISHED — run `clank finish`".into(),
        WaitingOn::MasterToCommit => "gate continued but plan file dirty".into(),
        WaitingOn::MasterToFixCommitTag => {
            "HEAD tags don't match the plans it touches — amend (re-tag, or drop the tag if ad-hoc)"
                .into()
        }
    }
}

fn active_summary(fold: &clank_core::repo_state::RepoState, basename: &str) -> String {
    let names: Vec<String> = fold
        .plans
        .keys()
        .map(|k| format!("{basename}/{}.md", k.as_str()))
        .collect();
    if names.is_empty() {
        "(none)".into()
    } else {
        names.join(", ")
    }
}

pub(crate) fn short_sha(s: &str) -> &str {
    &s[..s.len().min(7)]
}

fn head_info(git: &crate::git_io::Repo) -> (Option<String>, Option<String>, Option<String>) {
    let branch = git.current_branch().ok().flatten();
    let head = git.head_sha().ok().flatten();
    let sha = head.as_ref().map(|s| s.as_str().to_string());
    let subject = head.as_ref().and_then(|s| git.commit_subject(s).ok());
    (branch, sha, subject)
}

/// The PR's GitHub URL from its `owner/name` slug + number. The one
/// place clank formats a github URL, shared by the text and TUI
/// status surfaces (the TUI makes it a clickable OSC 8 hyperlink).
pub(crate) fn pr_url(repo: &str, pr: u32) -> String {
    format!("https://github.com/{repo}/pull/{pr}")
}

/// `+12 −3 · 1 binary · 2 untracked`, omitting zero parts. A dirty
/// tree whose numbers are all zero (e.g. a mode-only change) reads
/// `changes`.
pub(crate) fn dirty_summary(d: &DirtyStats) -> String {
    let mut parts = Vec::new();
    if d.insertions > 0 || d.deletions > 0 {
        parts.push(format!("+{} −{}", d.insertions, d.deletions));
    }
    // A binary file has no lines, so it is counted rather than
    // measured — and named, because otherwise a tree holding nothing
    // but a new PNG reads as clean-ish.
    if d.binary > 0 {
        parts.push(format!("{} binary", d.binary));
    }
    if d.untracked > 0 {
        parts.push(format!("{} untracked", d.untracked));
    }
    if parts.is_empty() {
        return "changes".to_string();
    }
    parts.join(" · ")
}

#[cfg(test)]
mod watch_failure_tests {
    use super::watch_failure;

    #[test]
    fn initial_failure_is_fatal_post_success_degrades() {
        // codex 697cdb1: before the first successful snapshot there
        // is nothing to retain — fail loud, like the one-shot path.
        let err = watch_failure(false, "input probe", "boom").unwrap_err();
        assert!(err.to_string().contains("input probe"), "{err}");
        let err = watch_failure(false, "refresh", "boom").unwrap_err();
        assert!(err.to_string().contains("refresh"), "{err}");
        // After one success the watch degrades and keeps running;
        // the caller leaves last_sig untouched, so the next
        // event/backstop retries the same state and an ordinary
        // success is the recovery.
        assert!(watch_failure(true, "input probe", "boom").is_ok());
        assert!(watch_failure(true, "refresh", "boom").is_ok());
    }
}

#[cfg(test)]
mod dirty_and_wake_tests {

    /// The refresh read collapses what the first read collapsed.
    ///
    /// `tui_log_with_events` is the refresh path, and it used to be
    /// handed an empty map — so a single-commit plan showed compact on
    /// open and grew its header back on the next tick, and a plan
    /// finished while the TUI was up never showed compact at all
    /// (codex on f99d42a).
    #[tokio::test]
    async fn the_refresh_read_collapses_what_the_first_read_did() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("README"), "base\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);
        let adopted = crate::lifecycle::CommitSha::parse(&git(&["rev-parse", "HEAD"])).unwrap();
        // One commit that both introduces and finishes the plan —
        // what a squashed plan looks like on disk.
        std::fs::create_dir_all(repo.join(".clank/finished")).unwrap();
        std::fs::write(repo.join(".clank/finished/solo.md"), "# solo\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "[solo] did the whole thing"]);
        let sha = git(&["rev-parse", "HEAD"]);

        let stands: crate::cli::log::StandsFor =
            [(sha.clone(), "solo".to_string())].into_iter().collect();
        let read = tui_log_with_events(repo, 30, Some(&adopted), &stands).await;
        assert!(
            read.rows.iter().any(|r| matches!(
                r,
                crate::cli::log::OnelineRow::Commit { stands_for: Some(p), .. } if p == "solo"
            )),
            "the refresh read collapses it: {:?}",
            read.rows
        );
        assert!(
            !read
                .rows
                .iter()
                .any(|r| matches!(r, crate::cli::log::OnelineRow::Header { plan: Some(p) } if p == "solo")),
            "and drops the umbrella: {:?}",
            read.rows
        );
    }

    /// Which finished plans stand for themselves: the ones whose intro
    /// IS their finalize. Read from the fold, so a plan split across
    /// chunks or falling off the end of a page cannot look like one
    /// (codex on d420716).
    #[test]
    fn only_a_finished_plan_of_one_commit_stands_for_itself() {
        use clank_core::repo_state::FinishedPlan;
        let sha = |s: &str| crate::lifecycle::CommitSha::parse(&format!("{s:0<40}")).unwrap();
        let plan = |s: &str| crate::lifecycle::PlanKey::parse(s).unwrap();
        let mut state = RepoState::empty(std::path::PathBuf::from("/tmp"));
        state.fold.finished_plans = vec![
            // One commit: intro and finalize are the same commit.
            FinishedPlan {
                plan: plan("solo"),
                intro: sha("aa"),
                finalized_at: sha("aa"),
            },
            // Two: they are not.
            FinishedPlan {
                plan: plan("duo"),
                intro: sha("bb"),
                finalized_at: sha("cc"),
            },
        ];
        let stands = stands_for_plans(&state);
        assert_eq!(
            stands.get(sha("aa").as_str()).map(String::as_str),
            Some("solo"),
            "the one-commit plan stands for itself"
        );
        assert!(
            !stands.values().any(|p| p == "duo"),
            "the two-commit plan does not: {stands:?}"
        );
        assert_eq!(stands.len(), 1, "and nothing else crept in: {stands:?}");
    }
    use super::*;
    use std::process::Command;

    #[test]
    fn dirty_stats_unborn_head_degrades_lines_to_zero() {
        // No commit yet: a staged add has no HEAD tree to diff
        // against, so +/− degrade to 0/0 while the tree still reads
        // dirty (matching the old `git diff HEAD` error path).
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        git(r, &["init", "--quiet", "-b", "main"]);
        git(r, &["config", "user.email", "t@t"]);
        git(r, &["config", "user.name", "t"]);
        std::fs::write(r.join("staged.txt"), "a\nb\n").unwrap();
        git(r, &["add", "-A"]);
        let d = crate::git_io::working_tree_dirty_at(r)
            .unwrap()
            .expect("unborn HEAD with staged add is dirty");
        assert_eq!((d.insertions, d.deletions), (0, 0));
    }

    #[test]
    fn dirty_stats_counts_staged_and_unstaged_together() {
        // `git diff HEAD` is HEAD-vs-worktree: a staged edit on one
        // file plus an unstaged edit on another must both count.
        let dir = fixture_repo();
        let r = dir.path();
        std::fs::write(r.join("b.txt"), "x\n").unwrap();
        git(r, &["add", "-A"]);
        git(r, &["commit", "--quiet", "-m", "add b"]);

        // a.txt: unstaged (+1). b.txt: staged (+1 −1).
        std::fs::write(r.join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        std::fs::write(r.join("b.txt"), "y\n").unwrap();
        git(r, &["add", "b.txt"]);
        let d = crate::git_io::working_tree_dirty_at(r)
            .unwrap()
            .expect("dirty");
        assert_eq!((d.insertions, d.deletions), (2, 1));
        assert_eq!(d.untracked, 0);
    }

    /// An uncommitted binary has no lines to count, and counting its
    /// bytes-between-newlines is how one PNG read as thousands of
    /// changed lines (lloyd on 2e27b70). git excludes binaries from
    /// `--shortstat`; clank counts them, so the tree still says it is
    /// carrying something.
    #[test]
    fn a_binary_change_is_counted_not_measured_in_lines() {
        let dir = fixture_repo();
        let r = dir.path();
        // A committed "image": NUL early, then plenty of newlines for
        // a line diff to feast on.
        let mut png = vec![0x89, b'P', b'N', b'G', 0x00, 0x1a];
        png.extend(std::iter::repeat_n(b"row\n", 2000).flatten().copied());
        std::fs::write(r.join("shot.png"), &png).unwrap();
        git(r, &["add", "-A"]);
        git(r, &["commit", "--quiet", "-m", "add shot"]);

        // Rewrite it with different bytes, and edit one text line.
        png.extend(std::iter::repeat_n(b"more\n", 3000).flatten().copied());
        std::fs::write(r.join("shot.png"), &png).unwrap();
        std::fs::write(r.join("a.txt"), "one\ntwo\n").unwrap();

        let d = crate::git_io::working_tree_dirty_at(r)
            .unwrap()
            .expect("dirty");
        assert_eq!(
            (d.insertions, d.deletions),
            (0, 1),
            "only the text edit has lines — `three` dropped from a.txt: {d:?}"
        );
        assert_eq!(d.binary, 1, "and the image is counted once");
    }

    #[test]
    fn dirty_summary_omits_zero_parts() {
        let s = |i, d, b, u| {
            dirty_summary(&DirtyStats {
                insertions: i,
                deletions: d,
                binary: b,
                untracked: u,
            })
        };
        assert_eq!(s(12, 3, 0, 0), "+12 −3");
        assert_eq!(s(12, 3, 0, 2), "+12 −3 · 2 untracked");
        assert_eq!(s(0, 0, 0, 2), "2 untracked");
        assert_eq!(s(0, 0, 0, 0), "changes");
        assert_eq!(s(0, 5, 0, 0), "+0 −5");
        // The reported case: a tree carrying one image and nothing
        // else must not read as `changes`, nor as a line count.
        assert_eq!(s(0, 0, 1, 0), "1 binary");
        assert_eq!(s(12, 3, 2, 1), "+12 −3 · 2 binary · 1 untracked");
    }

    #[test]
    fn wake_filter_matrix() {
        // Real on-disk repo: the filter's ignore test is now gix's
        // exclude stack, which reads `.gitignore` from disk and honors
        // NESTED files — so the fixture writes a root AND a nested
        // `.gitignore` and the queried paths exist where it matters.
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        git(&root, &["init", "--quiet", "-b", "main"]);
        std::fs::write(root.join(".gitignore"), "/target/\n*.log\n").unwrap();
        // A subproject that ignores its OWN build dir via a NESTED
        // `.gitignore` — the storm case (Flutter `frostsnapp/build/`).
        std::fs::create_dir_all(root.join("sub/build/macos")).unwrap();
        std::fs::create_dir_all(root.join("sub/.dart_tool")).unwrap();
        std::fs::write(root.join("sub/.gitignore"), "/build/\n.dart_tool/\n").unwrap();
        std::fs::write(root.join("sub/build/macos/App"), "x").unwrap();
        std::fs::write(root.join("sub/.dart_tool/x"), "x").unwrap();
        let git_dir = root.join(".git");
        let mut f = WakeFilter::new(&root, &git_dir).unwrap();
        let p = |rel: &str| root.join(rel);

        // The DIFF watcher refreshes dirty/diff from the WORKING TREE
        // only. Gate-state changes — gitdir AND everything under
        // `.clank/` — are the CORE watcher's job
        // (`repo_watch::is_core_wake`), so the diff filter ignores both,
        // no matter the subpath. (This avoids double-waking: status runs
        // both watchers on one channel.)
        assert!(!f.wakes(&p(".clank/agents/codex/feedback/abc.md")));
        assert!(!f.wakes(&p(".clank/plans/foo.md")));
        assert!(!f.wakes(&p(".clank/config.json")));
        assert!(!f.wakes(&p(".clank/cache/repo-state/abc123.7.v10.bin")));
        assert!(!f.wakes(&p(".clank/worktrees/wt1/src/lib.rs")));
        assert!(!f.wakes(&p(".git/HEAD")));

        // Top-level working tree: root-gitignored → drop, tracked → wake.
        assert!(!f.wakes(&p("target/debug/build/junk.o")));
        assert!(!f.wakes(&p("build.log")));
        assert!(f.wakes(&p("src/lib.rs")));
        assert!(f.wakes(&p("Cargo.toml")));

        // THE FIX: churn ignored only by the NESTED `sub/.gitignore`
        // must NOT wake (a root-only matcher missed these and stormed).
        assert!(!f.wakes(&p("sub/build/macos/App")));
        assert!(!f.wakes(&p("sub/.dart_tool/x")));
        // ...but a tracked file in the same subproject still wakes.
        assert!(f.wakes(&p("sub/main.rs")));

        // .gitignore changes always wake (and rebuild the exclude stack).
        assert!(f.wakes(&p(".gitignore")));
        assert!(f.wakes(&p("sub/.gitignore")));
    }

    fn git(repo: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn fixture_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        git(r, &["init", "--quiet", "-b", "main"]);
        git(r, &["config", "user.email", "t@t"]);
        git(r, &["config", "user.name", "t"]);
        std::fs::write(r.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        git(r, &["add", "-A"]);
        git(r, &["commit", "--quiet", "-m", "base"]);
        dir
    }

    fn empty_commit(repo: &Path, subject: &str) {
        git(repo, &["commit", "--quiet", "--allow-empty", "-m", subject]);
    }

    #[tokio::test]
    async fn unadopted_repo_history_is_plain_and_head_is_decorated() {
        let dir = fixture_repo();
        let repo = dir.path();
        empty_commit(repo, "second");
        empty_commit(repo, "third");
        let git = crate::git_io::open(repo).unwrap();
        let head = git.head_sha().unwrap().unwrap();

        crate::rebuild::reset_fold_commit_count();
        let history = crate::cli::log::history_window(repo, &head, 30, None)
            .await
            .unwrap();
        assert!(history.folded.is_empty());
        assert_eq!(history.plain.len(), 3);
        assert_eq!(
            crate::rebuild::fold_commit_count(),
            0,
            "plain history must never enter the fold"
        );
        let head_row = &history.plain[0];
        assert_eq!(head_row.meta.sha, head);
        assert!(head_row.refs.iter().any(|name| name == "main"));

        let items = crate::cli::log::interleave_with_plain(&[], &history.plain, &[]);
        let rows =
            crate::cli::log::oneline_items_rows(&items, &Default::default(), &Default::default());
        assert_eq!(rows.len(), 3);
        assert!(
            rows.iter()
                .all(|row| matches!(row, crate::cli::log::OnelineRow::PlainCommit { .. }))
        );
    }

    #[tokio::test]
    async fn history_splices_continuously_at_adoption_without_folding_before_it() {
        let dir = fixture_repo();
        let repo = dir.path();
        empty_commit(repo, "plain two");
        empty_commit(repo, "plain three");

        std::fs::create_dir_all(repo.join(".clank/plans")).unwrap();
        std::fs::write(repo.join(".clank/plans/foo.md"), "# foo\n").unwrap();
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);
        empty_commit(repo, "post one");
        empty_commit(repo, "post two");

        let state = crate::rebuild::rebuild_repo(repo).await.unwrap();
        let adopted_at = state.fold.adopted_at.clone().expect("adoption sha");
        let git = crate::git_io::open(repo).unwrap();
        let head = git.head_sha().unwrap().unwrap();
        let expected: Vec<_> = git
            .first_parent_page_to(&head, 30)
            .unwrap()
            .into_iter()
            .map(|commit| commit.sha)
            .collect();

        crate::rebuild::reset_fold_commit_count();
        let history = crate::cli::log::history_window(repo, &head, 30, Some(&adopted_at))
            .await
            .unwrap();
        assert_eq!(history.plain.len(), 3);
        assert_eq!(history.folded.len(), 3);
        assert_eq!(
            crate::rebuild::fold_commit_count(),
            3,
            "only adoption and newer commits are folded"
        );

        let folded: Vec<&clank_core::repo_state::LogEvent> = history.folded.iter().rev().collect();
        let items = crate::cli::log::interleave_with_plain(&folded, &history.plain, &[]);
        let rows =
            crate::cli::log::oneline_items_rows(&items, &Default::default(), &Default::default());
        let actual: Vec<_> = rows
            .iter()
            .filter_map(|row| match row {
                crate::cli::log::OnelineRow::Commit { sha, .. }
                | crate::cli::log::OnelineRow::PlainCommit { sha, .. } => Some(sha.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(actual, expected, "no gap or duplicate at the seam");

        crate::rebuild::reset_fold_commit_count();
        let seam_page = crate::cli::log::history_window(repo, &head, 4, Some(&adopted_at))
            .await
            .unwrap();
        let older_cursor = seam_page.next.as_ref().expect("plain page after seam");
        assert!(!older_cursor.post_adoption);
        let older = crate::cli::log::history_page(repo, older_cursor, 4, Some(&adopted_at))
            .await
            .unwrap();
        assert!(older.folded.is_empty());
        assert_eq!(older.plain.len(), 2);
        assert_eq!(
            crate::rebuild::fold_commit_count(),
            3,
            "paging beyond the seam must not perform another fold"
        );
    }

    #[tokio::test]
    async fn plain_page_cost_does_not_grow_with_repository_age() {
        async fn read(depth: usize) -> (usize, usize) {
            let dir = fixture_repo();
            for i in 1..depth {
                empty_commit(dir.path(), &format!("plain {i}"));
            }
            let git = crate::git_io::open(dir.path()).unwrap();
            let head = git.head_sha().unwrap().unwrap();
            crate::rebuild::reset_fold_commit_count();
            let history = crate::cli::log::history_window(dir.path(), &head, 5, None)
                .await
                .unwrap();
            (history.plain.len(), crate::rebuild::fold_commit_count())
        }

        assert_eq!(read(8).await, (5, 0));
        assert_eq!(read(80).await, (5, 0));
    }

    #[tokio::test]
    async fn plain_paging_reads_disjoint_fixed_size_batches() {
        let dir = fixture_repo();
        for i in 1..12 {
            empty_commit(dir.path(), &format!("plain {i}"));
        }
        let git = crate::git_io::open(dir.path()).unwrap();
        let head = git.head_sha().unwrap().unwrap();

        crate::rebuild::reset_fold_commit_count();
        let first = crate::cli::log::history_window(dir.path(), &head, 5, None)
            .await
            .unwrap();
        let second = crate::cli::log::history_page(
            dir.path(),
            first.next.as_ref().expect("older page"),
            5,
            None,
        )
        .await
        .unwrap();
        assert_eq!((first.plain.len(), second.plain.len()), (5, 5));
        let first_shas: std::collections::BTreeSet<_> = first
            .plain
            .iter()
            .map(|commit| commit.meta.sha.clone())
            .collect();
        assert!(
            second
                .plain
                .iter()
                .all(|commit| !first_shas.contains(&commit.meta.sha)),
            "the second page must not reread the first"
        );
        assert_eq!(crate::rebuild::fold_commit_count(), 0);
    }

    #[tokio::test]
    async fn unborn_repo_has_no_log_rows() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--quiet", "-b", "main"]);
        let read = tui_log_with_events(dir.path(), 30, None, &Default::default()).await;
        assert!(read.rows.is_empty());
        assert!(read.github_events.is_empty());
        assert!(read.next.is_none());
    }

    #[tokio::test]
    async fn status_shows_adhoc_commits_with_one_active_plan() {
        // show-adhoc-commits-in-status (reproduce-first): with exactly
        // ONE active plan, recent_log_rows auto-applied a single-plan
        // filter that DROPPED ad-hoc events. An ad-hoc commit (no
        // `[plan]` tag) must still appear in the status log.
        let dir = fixture_repo();
        let r = dir.path();
        // Plan intro → adopts the repo AND makes `foo` the lone active
        // plan (so the single-plan filter engages).
        std::fs::create_dir_all(r.join(".clank/plans")).unwrap();
        std::fs::write(r.join(".clank/plans/foo.md"), "# foo\n").unwrap();
        git(r, &["add", "-A"]);
        git(r, &["commit", "--quiet", "-m", "[foo] intro"]);
        // Post-adoption ad-hoc commit. Tagged `[bar]` — a NON-plan tag
        // (no `.clank/plans/bar.md`), which is what makes it ad-hoc
        // rather than inherited into `foo` via the active-plan hint
        // (this is exactly the real `[animations]` case). `foo` stays
        // the lone active plan, so the single-plan filter engages.
        std::fs::write(r.join("scratch.txt"), "scratch\n").unwrap();
        git(r, &["add", "-A"]);
        git(r, &["commit", "--quiet", "-m", "[bar] sketch the thing"]);

        let snap = StatusSnapshot::build_async(
            r,
            "repo",
            None,
            crate::rebuild::CachePolicy::Bypass,
            None,
            false,
        )
        .await
        .unwrap();

        let subjects: Vec<&str> = snap
            .log_rows
            .iter()
            .filter_map(|row| match row {
                crate::cli::log::OnelineRow::Commit { subject, .. } => Some(subject.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            subjects.iter().any(|s| s.contains("sketch the thing")),
            "ad-hoc commit must appear in status log even with one active plan; got subjects: {subjects:?}"
        );
    }

    /// Fitness function for the status path (mirrors the fold's
    /// `fold_opens_the_odb_once`): a whole status build opens the ODB a
    /// FIXED, small number of times — once PER PHASE, not once per read.
    /// The three phases each open their own handle and reuse it
    /// internally: (1) the main fold (`rebuild_with_diagnostics`), (2)
    /// the log-window fold (`rebuild_from` in `log_rows_windowed`), and
    /// (3) the snapshot's live reads (dirty walk + HEAD facts + per-plan
    /// worktree status all share that handle). Collapsing the three into
    /// one would mean threading a handle through `rebuild`'s many callers
    /// — deferred (clean-hangers scope keeps cold/one-shot simple). The
    /// EXACT assert catches a regression that re-adds a per-read open.
    #[tokio::test]
    async fn status_build_opens_the_odb_once_per_phase() {
        let dir = fixture_repo();
        let r = dir.path();
        std::fs::create_dir_all(r.join(".clank/plans")).unwrap();
        std::fs::write(r.join(".clank/plans/foo.md"), "# foo\n").unwrap();
        git(r, &["add", "-A"]);
        git(r, &["commit", "--quiet", "-m", "[foo] intro"]);

        crate::git_io::OPEN_COUNT.with(|c| c.set(0));
        let _ = StatusSnapshot::build_async(
            r,
            "repo",
            None,
            crate::rebuild::CachePolicy::Bypass,
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            crate::git_io::OPEN_COUNT.with(|c| c.get()),
            3,
            "status build opens once per phase: main fold + log-window fold + snapshot"
        );
    }

    /// The reuse probe (`input_signature`) must be far cheaper than the
    /// build it guards: ONE ODB open (HEAD + the dirty walk share a
    /// handle; the `.clank` fingerprint is stat-only). A wake that
    /// changed nothing pays only this, not the 3-phase build above.
    #[test]
    fn input_signature_opens_the_odb_once() {
        let dir = fixture_repo();
        let r = dir.path();
        std::fs::create_dir_all(r.join(".clank/plans")).unwrap();
        std::fs::write(r.join(".clank/plans/foo.md"), "# foo\n").unwrap();
        git(r, &["add", "-A"]);
        git(r, &["commit", "--quiet", "-m", "[foo] intro"]);

        crate::git_io::OPEN_COUNT.with(|c| c.set(0));
        let _ = input_signature(r).unwrap();
        assert_eq!(
            crate::git_io::OPEN_COUNT.with(|c| c.get()),
            1,
            "the reuse probe opens the ODB once"
        );
    }

    #[test]
    fn input_signature_is_stable_when_nothing_changes() {
        let dir = fixture_repo();
        let r = dir.path();
        std::fs::create_dir_all(r.join(".clank/plans")).unwrap();
        std::fs::write(r.join(".clank/plans/foo.md"), "# foo\n").unwrap();
        git(r, &["add", "-A"]);
        git(r, &["commit", "--quiet", "-m", "[foo] intro"]);
        assert_eq!(
            input_signature(r).unwrap(),
            input_signature(r).unwrap(),
            "stable across calls when nothing changed — the gate reuses"
        );
    }

    /// The reuse key must cover the FULL input set, not just HEAD+dirty
    /// (ruthless). A review verdict is a gitignored
    /// `.clank/agents/<label>/feedback/<sha>.md`: it changes neither
    /// HEAD nor the working tree, so a HEAD+dirty-only key would reuse a
    /// stale snapshot and the verdict would be INVISIBLE in the TUI —
    /// exactly what the master is watching for. The `.clank` fingerprint
    /// must flip.
    #[test]
    fn clank_fingerprint_flips_on_a_new_review_verdict() {
        let dir = fixture_repo();
        let r = dir.path();
        let before = clank_input_fingerprint(r);
        std::fs::create_dir_all(r.join(".clank/agents/codex/feedback")).unwrap();
        std::fs::write(
            r.join(".clank/agents/codex/feedback/abc123.md"),
            "CONTINUE\n",
        )
        .unwrap();
        let after = clank_input_fingerprint(r);
        assert_ne!(
            before, after,
            "a new gitignored feedback file must flip the fingerprint"
        );
    }

    #[test]
    fn clank_fingerprint_flips_on_a_pr_review_write() {
        // PR-review state (PrReviewer/PrMaster gate) is part of the
        // snapshot, so a `.clank/pr-reviews/<pr>/` write must flip the
        // fingerprint (and wake the core watcher) — otherwise the TUI
        // keeps stale PR rows after propose/note/submit/abort
        // (codex fbd3e73). pr-reviews is in CLANK_WAKE_DIRS.
        let dir = fixture_repo();
        let r = dir.path();
        let before = clank_input_fingerprint(r);
        std::fs::create_dir_all(r.join(".clank/pr-reviews/42")).unwrap();
        std::fs::write(r.join(".clank/pr-reviews/42/pr.json"), r#"{"round":1}"#).unwrap();
        let after = clank_input_fingerprint(r);
        assert_ne!(
            before, after,
            "a pr-reviews write must flip the fingerprint (TUI PR-row refresh)"
        );
    }

    /// The agent-panel auto toggle (`clank tui` SPC, or an external
    /// `clank auto`) writes `agents/<label>/config.json`. The repaint
    /// rests on the fingerprint covering that file — which it does only
    /// because the fingerprint recurses over ALL of `agents/`, not
    /// because config.json is named. This pins that coverage so a future
    /// scoping optimization (e.g. hashing only `feedback/`) can't
    /// silently kill the toggle repaint. Plan: tui-agent-auto-toggle.
    #[test]
    fn clank_fingerprint_flips_on_an_agent_config_write() {
        let dir = fixture_repo();
        let r = dir.path();
        let before = clank_input_fingerprint(r);
        std::fs::create_dir_all(r.join(".clank/agents/codex")).unwrap();
        std::fs::write(
            r.join(".clank/agents/codex/config.json"),
            r#"{"auto_mode":"on"}"#,
        )
        .unwrap();
        let after = clank_input_fingerprint(r);
        assert_ne!(
            before, after,
            "an agent config.json write must flip the fingerprint (toggle repaint)"
        );
    }

    #[test]
    fn available_agents_empty_without_home() {
        assert!(available_agents(None, &[]).is_empty());
    }

    #[test]
    fn dirty_stats_reports_lines_and_untracked() {
        let dir = fixture_repo();
        let r = dir.path();
        assert_eq!(
            crate::git_io::working_tree_dirty_at(r).unwrap(),
            None,
            "clean tree"
        );

        // 1 line modified (one in, one out), 2 added; one untracked.
        std::fs::write(r.join("a.txt"), "one\nTWO\nthree\nfour\nfive\n").unwrap();
        std::fs::write(r.join("new.txt"), "x\n").unwrap();
        let d = crate::git_io::working_tree_dirty_at(r)
            .unwrap()
            .expect("dirty");
        assert_eq!((d.insertions, d.deletions), (3, 1));
        assert_eq!(d.untracked, 1);
    }

    #[test]
    fn dirty_probe_never_writes_the_index() {
        // The self-wake hazard: a probe that refreshes .git/index
        // would fire the watcher that triggered the probe. With
        // --no-optional-locks the index bytes must stay untouched
        // even when stat info is stale.
        let dir = fixture_repo();
        let r = dir.path();
        std::fs::write(r.join("a.txt"), "one\ntwo\nthree\nmore\n").unwrap();
        // Make stat info stale so a plain `git status` would want
        // to refresh the index.
        let index = r.join(".git/index");
        let before = std::fs::read(&index).unwrap();
        let mtime_before = std::fs::metadata(&index).unwrap().modified().unwrap();
        let _ = crate::git_io::working_tree_dirty_at(r).unwrap();
        assert_eq!(
            std::fs::read(&index).unwrap(),
            before,
            "index bytes changed"
        );
        assert_eq!(
            std::fs::metadata(&index).unwrap().modified().unwrap(),
            mtime_before,
            "index mtime changed"
        );
    }

    /// Drain until the channel has been quiet for a poll cycle and a
    /// bit, so an assertion about what the NEXT write does is not
    /// answering for something earlier.
    ///
    /// Quiet is EVIDENCE, not a barrier. `RepoStateWatcher` restats
    /// every `POLL_INTERVAL`, so a change can always be reported
    /// later; observing silence for longer than a cycle is the most a
    /// test can establish.
    fn settle(rx: &mpsc::Receiver<()>) {
        const QUIET: Duration =
            crate::repo_watch::POLL_INTERVAL.saturating_add(Duration::from_millis(200));
        // A channel that never goes quiet must FAIL, not hang: a
        // watcher waking forever is the bug this test would be there
        // to catch.
        const BUDGET: Duration = Duration::from_secs(20);
        let deadline = std::time::Instant::now() + BUDGET;
        loop {
            match rx.recv_timeout(QUIET) {
                Err(mpsc::RecvTimeoutError::Timeout) => return,
                // A dead channel is not a quiet one, and only here
                // can the two be told apart: every assertion below
                // uses `recv_timeout(..).is_err()`, which a
                // disconnect satisfies just as silence does, so both
                // no-wake assertions would pass vacuously.
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("the watcher channel is closed; no assertion after this means anything")
                }
                Ok(()) => assert!(
                    std::time::Instant::now() < deadline,
                    "the watcher never went quiet in {BUDGET:?} — no wake is expected \
                     here, so something is waking on its own"
                ),
            }
        }
    }

    #[test]
    fn watcher_wakes_on_tracked_edit_not_on_ignored() {
        let dir = fixture_repo();
        let r = dir.path();
        std::fs::write(r.join(".gitignore"), "/target/\n").unwrap();
        // A subproject ignoring its OWN build dir via a NESTED
        // `.gitignore` — the storm case the root-only matcher missed.
        std::fs::create_dir_all(r.join("sub")).unwrap();
        std::fs::write(r.join("sub/.gitignore"), "/build/\n").unwrap();
        git(r, &["add", "-A"]);
        git(r, &["commit", "--quiet", "-m", "gitignore"]);
        std::fs::create_dir_all(r.join("target/debug")).unwrap();
        std::fs::create_dir_all(r.join("sub/build")).unwrap();

        let (tx, rx) = mpsc::channel::<()>();
        // The native backend can attach successfully inside the tool
        // sandbox yet deliver no FSEvents at all. The baseline before
        // this plan fails at the same tracked-edit assertion. Exercise
        // the production callback with notify's polling backend so
        // this test measures our filter and wake behavior.
        let mut poll_watcher = notify::PollWatcher::new(
            diff_wake_handler(tx, r).unwrap(),
            notify::Config::default().with_poll_interval(crate::repo_watch::POLL_INTERVAL),
        )
        .unwrap();
        poll_watcher.watch(r, RecursiveMode::Recursive).unwrap();

        // Root-ignored path: no wake. Give the poller a full cycle
        // plus slack after each write, so a broken filter cannot pass
        // merely because its event arrived after the assertion.
        const NO_WAKE: Duration =
            crate::repo_watch::POLL_INTERVAL.saturating_add(Duration::from_millis(200));
        settle(&rx);
        std::fs::write(r.join("target/debug/out.o"), "junk").unwrap();
        assert!(
            rx.recv_timeout(NO_WAKE).is_err(),
            "ignored build artifact woke the loop"
        );

        // NESTED-ignored path: no wake (THE storm fix — this is what
        // pegged a core, ~250 wakes/s of `frostsnapp/build/` churn).
        settle(&rx);
        std::fs::write(r.join("sub/build/out.o"), "junk").unwrap();
        assert!(
            rx.recv_timeout(NO_WAKE).is_err(),
            "nested-gitignored build artifact woke the loop"
        );

        // Tracked-file edit: wakes (the production native watcher
        // uses the same filter to keep `dirty:` fresh). Settled first,
        // so an event left over from setup cannot pass for the edit.
        settle(&rx);
        std::fs::write(r.join("a.txt"), "edited\n").unwrap();
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "tracked edit did not wake the loop"
        );
    }

    fn sha(s: &str) -> clank_core::ids::CommitSha {
        clank_core::ids::CommitSha::parse(s).unwrap()
    }

    fn plan_key(s: &str) -> PlanKey {
        PlanKey::parse(s).unwrap()
    }

    fn agent_row(label: &str, attending: Option<crate::cli::stop_hook::Attended>) -> AgentAutoRow {
        AgentAutoRow {
            label: label.to_string(),
            role: crate::cli::teams_config::RosterRole::Master,
            auto_mode: clank_core::vocab::AutoMode::On,
            tool: "claude".to_string(),
            invocation: "claude".to_string(),
            session: None,
            attending,
        }
    }

    fn marker(task: &str, pid: Option<i32>, at: Option<&str>) -> crate::cli::stop_hook::Attended {
        crate::cli::stop_hook::Attended {
            expect: None,
            desc: None,
            token: None,
            task: task.to_string(),
            pid,
            at: at.unwrap_or("2026-08-20T14:51:09Z").to_string(),
        }
    }

    #[test]
    fn a_live_pid_renders_as_attending_with_its_age() {
        let mut snap = minimal_snapshot();
        snap.agents = vec![agent_row(
            "claude",
            Some(marker(
                "b72qah60w",
                Some(std::process::id() as i32),
                Some("2026-08-20T14:51:09Z"),
            )),
        )];
        let out = snap.to_human();
        assert!(
            out.contains(&format!(
                "attending: claude → {} (b72qah60w) · ",
                std::process::id()
            )),
            "{out}"
        );
        assert!(!out.contains("stale"), "a live pid is not stale: {out}");
    }

    /// The defect this plan exists for: a wait that has ENDED must not
    /// keep reading as attendance just because no hook has reaped it.
    #[test]
    fn a_dead_pid_renders_as_stale() {
        let mut snap = minimal_snapshot();
        snap.agents = vec![agent_row(
            "claude",
            Some(marker("b72qah60w", Some(i32::MAX), None)),
        )];
        assert!(
            snap.to_human()
                .contains("attending: claude → 2147483647 (b72qah60w) · stale, ended"),
            "{}",
            snap.to_human()
        );
    }

    /// A marker written before `--pid` existed, or by a caller that
    /// could not supply one. Still valid, still shown — but it must
    /// claim nothing about liveness it cannot check.
    #[test]
    fn a_marker_without_a_pid_makes_no_liveness_claim() {
        let mut snap = minimal_snapshot();
        snap.agents = vec![agent_row("claude", Some(marker("b72qah60w", None, None)))];
        let out = snap.to_human();
        assert!(out.contains("attending: claude → b72qah60w"), "{out}");
        assert!(!out.contains("stale"), "{out}");
    }

    #[test]
    fn a_marker_names_the_task_silencing_the_agent() {
        let mut snap = minimal_snapshot();
        snap.agents = vec![agent_row("claude", Some(marker("bj0onbq1u", None, None)))];
        assert!(
            snap.to_human().contains("attending: claude → bj0onbq1u"),
            "{}",
            snap.to_human()
        );
    }

    #[test]
    fn an_agent_attending_nothing_adds_no_line() {
        let mut snap = minimal_snapshot();
        snap.agents = vec![agent_row("claude", None)];
        assert!(
            !snap.to_human().contains("attending"),
            "{}",
            snap.to_human()
        );
    }

    /// `clank status` names the blocked handoff only where master has
    /// actually stopped.
    ///
    /// The block survives `propose`, so a reopened round can carry it
    /// while reviewers genuinely owe verdicts. Saying "waiting on
    /// nobody" then hides real work -- and core keeps emitting those
    /// items, so the line would contradict the wake model
    /// (codex e70f8a9).
    #[test]
    fn a_blocked_handoff_only_changes_the_converged_line() {
        use clank_core::vocab::CommitGateState;
        let line = |gate, blocked, missing: &[&str]| {
            let mut snap = minimal_snapshot();
            // The shared fixture, not a literal: core's struct grows,
            // and a second hand-built copy would need editing every
            // time (ruthless 329d579).
            let mut pr = crate::cli::status_tui::fixtures::pr_work(2, missing);
            pr.pr = 7;
            pr.repo = "o/r".to_string();
            pr.gate = gate;
            pr.handoff_blocked = blocked;
            snap.pr_reviews = vec![pr];
            snap.to_human()
                .lines()
                .find(|l| l.starts_with("pr #7"))
                .expect("the PR line is rendered")
                .to_string()
        };

        let stopped = line(CommitGateState::Finished, true, &[]);
        assert!(
            stopped.contains("handoff blocked"),
            "converged and blocked: master is not coming: {stopped}",
        );
        assert!(
            stopped.contains("clank pr-review finish"),
            "with the one thing that runs it again: {stopped}",
        );
        assert!(
            line(CommitGateState::Finished, false, &[]).contains("waiting on master"),
            "converged and not blocked is master's to hand over",
        );

        // Reopened after a blocked handoff: reviewers owe verdicts,
        // and that is what the line must say.
        let reopened = line(CommitGateState::Unreviewed, true, &["codex"]);
        assert!(
            reopened.contains("waiting on codex"),
            "a reopened round is waiting on its reviewers: {reopened}",
        );
        assert!(
            !reopened.contains("handoff blocked"),
            "and not on a handoff that is not due yet: {reopened}",
        );
        // Changes requested with the block still set: master's turn,
        // and a real one.
        let integrating = line(CommitGateState::ChangesRequested, true, &[]);
        assert!(
            integrating.contains("waiting on master") && !integrating.contains("handoff blocked"),
            "master owes an integration here: {integrating}",
        );
    }

    fn minimal_snapshot() -> StatusSnapshot {
        StatusSnapshot {
            inboxes: Vec::new(),
            forks: Vec::new(),
            repo_path: PathBuf::from("/repo"),
            basename: "repo".to_string(),
            branch: Some("main".to_string()),
            head_sha: Some("abcd123".to_string()),
            head_subject: Some("do the thing".to_string()),
            dirty: None,
            plans: vec![PlanWorkState {
                handover: Default::default(),
                plan: plan_key("foo"),
                sha: Some(sha("0123456789abcdef0123456789abcdef01234567")),
                gate: CommitGateState::Unreviewed,
                waiting_on: WaitingOn::MasterToContinue,
                touched_code: true,
            }],
            last_finished: None,
            blocks: vec![crate::cli::block::BlockEntry {
                agent: "codex".to_string(),
                name: "q".to_string(),
                question: "why?".to_string(),
                answer: None,
            }],
            queue: Vec::new(),
            master: None,
            agents: Vec::new(),
            stash: Vec::new(),
            log_rows: Vec::new(),
            log_adopted_at: None,
            log_stands_for: Default::default(),
            log_next: None,
            github_events: Vec::new(),
            log_decorations: Default::default(),
            pr_reviews: Vec::new(),
            ad_hoc: Vec::new(),
            head_correction: None,
        }
    }

    #[test]
    fn to_json_minimal_omits_conditional_keys() {
        // typed-json-not-json-macro: the typed `StatusJson` must
        // serialize to the SAME shape the old `json!` builder
        // produced. Minimal case: every conditional key is ABSENT.
        // (`json!` expresses the expected value; the ban is on
        // production output. `to_value` equality is order-independent.)
        let snap = minimal_snapshot();
        let got = snap.to_json();

        let want = serde_json::json!({
            // Where it was read: `--repo` means the reader need not be
            // standing in it (codex on 772b84f).
            "repo_path": "/repo",
            "repo_basename": "repo",
            "branch": "main",
            "head_sha": "abcd123",
            "head_subject": "do the thing",
            "worktree_dirty": false,
            "plans": [{
                // The two keys the wire fixed before this was one
                // model, and the rest of what a plan's state holds.
                "plan": "foo",
                "latest_reviewable_sha": "0123456789abcdef0123456789abcdef01234567",
                "gate_state": "unreviewed",
                "waiting_on": WaitingOn::MasterToContinue,
                "handover": {"opened": null, "commit_tier_closed": null, "second_tier_closed": null},
                "touched_code": true,
            }],
            "finished_plans": [],
            "blocks": [{
                "agent": "codex",
                "name": "q",
                "question": "why?",
                "answer": null,
                "pending": true,
            }],
            // The roster, always said: a teamless repo says so rather
            // than saying nothing (status-is-one-model-rendered-two-ways).
            "master": null,
            "agents": [],
        });
        assert_eq!(got, want);

        let obj = got.as_object().unwrap();
        for absent in [
            "dirty_stats",
            "queue_count",
            "queue",
            "shelved",
            "head_correction",
        ] {
            assert!(
                !obj.contains_key(absent),
                "key `{absent}` must be absent when empty"
            );
        }
    }

    /// Every fact the model carries either reaches the JSON or is
    /// named below as the log's machinery. Destructured WITHOUT `..`,
    /// so a field added to the snapshot does not COMPILE until someone
    /// decides which it is: the roster was missing for as long as it
    /// was because nothing made its absence fail
    /// (status-is-one-model-rendered-two-ways).
    #[test]
    fn every_fact_the_model_carries_reaches_the_json() {
        use crate::cli::teams_config::RosterRole;
        use clank_core::vocab::AutoMode;
        let mut snap = minimal_snapshot();
        snap.master = Some("claude".to_string());
        let roster_row = |label: &str, role: RosterRole, auto: AutoMode| AgentAutoRow {
            role,
            auto_mode: auto,
            ..agent_row(label, None)
        };
        snap.agents = vec![
            AgentAutoRow {
                // A bound agent and an unbound one, since "has a
                // session" is a fact the pane shows and a bool that is
                // always false says nothing.
                session: Some(crate::cli::status_tui::fixtures::bound(
                    clank_core::vocab::Tool::Claude,
                    "0199a0f2-1111-4444-8888-aaaaaaaaaaaa",
                )),
                ..roster_row("claude", RosterRole::Master, AutoMode::On)
            },
            roster_row("ruthless", RosterRole::Final, AutoMode::Off),
        ];
        snap.queue = vec![QueueItemView {
            priority: 120,
            name: "later".to_string(),
            path: PathBuf::from("/r/.clank/queue/120-later.md"),
        }];
        snap.forks = vec![("spike".to_string(), "worktree")];
        snap.pr_reviews = vec![clank_core::wait::PrReviewWorkState {
            handoff_blocked: false,
            pr: 7,
            handover: Default::default(),
            repo: "llfourn/clank".to_string(),
            round: 2,
            gate: CommitGateState::Unreviewed,
            missing_reviewers: vec![clank_core::ids::AgentLabel::parse("codex").unwrap()],
        }];
        snap.ad_hoc = vec![clank_core::wait::AdHocWorkState {
            sha: sha("deadbeef"),
            gate: CommitGateState::Unreviewed,
            handover: Default::default(),
        }];
        let v = snap.to_json();

        let StatusSnapshot {
            // The facts, each asserted below.
            inboxes: _,
            basename,
            branch,
            head_sha,
            head_subject,
            dirty,
            plans,
            last_finished: _,
            blocks,
            queue,
            master,
            agents,
            stash: _,
            forks,
            pr_reviews,
            ad_hoc,
            head_correction: _,
            // Emitted, and pinned by the wire fixture below rather
            // than here: `--repo` means the reader need not be
            // standing in it (codex on 772b84f).
            repo_path: _,
            // The LOG and the paging that walks it: rows, the fold
            // boundary, the cursor, what a finalize stands for, the
            // branch decorations, and the github events index-aligned
            // with those rows. `clank log --json` is the log's surface
            // and says so; this is the state's.
            log_rows: _,
            log_adopted_at: _,
            log_stands_for: _,
            log_next: _,
            log_decorations: _,
            github_events: _,
        } = &snap;

        assert_eq!(v["repo_basename"], **basename);
        assert_eq!(v["branch"], *branch.as_deref().unwrap());
        assert_eq!(v["head_sha"], *head_sha.as_deref().unwrap());
        assert_eq!(v["head_subject"], *head_subject.as_deref().unwrap());
        assert_eq!(v["worktree_dirty"], dirty.is_some());
        assert_eq!(v["plans"].as_array().unwrap().len(), plans.len());
        assert_eq!(v["blocks"].as_array().unwrap().len(), blocks.len());
        assert_eq!(v["master"], *master.as_deref().unwrap());
        // The TIER, not "master or reviewer": the pane draws the
        // difference between a commit reviewer and a final one.
        // The TIER, and the facts the pane draws beside it: what tool
        // the agent is, how it is invoked, the session it is bound to,
        // what it is attending. A bool saying "a session exists" is
        // not those facts (codex on d7af56f).
        assert_eq!(
            v["agents"],
            serde_json::json!([
                {
                    "label": "claude",
                    "role": "master",
                    "auto": "on",
                    "tool": "claude",
                    "invocation": "claude",
                    "session": {
                        "id": "0199a0f2-1111-4444-8888-aaaaaaaaaaaa",
                        "tool": "claude",
                        "updated_at": "2026-09-22T00:00:00Z",
                    },
                    "attending": null,
                },
                {
                    "label": "ruthless",
                    "role": "final",
                    "auto": "off",
                    "tool": "claude",
                    "invocation": "claude",
                    "session": null,
                    "attending": null,
                },
            ]),
            "the roster's tiers and the rest of what it holds"
        );
        assert_eq!(agents.len(), 2);
        // The names array consumers read, AND the entries that order
        // and identify them.
        assert_eq!(v["queue"], serde_json::json!(["later"]));
        assert_eq!(v["queue_count"], queue.len());
        assert_eq!(
            v["queue_entries"],
            serde_json::json!([{"name": "later", "priority": 120, "handle": "120-later"}])
        );
        assert_eq!(
            v["forks"],
            // Against the MODEL's own value, which is what this test
            // is for: a literal here would hold the JSON against
            // something written down twice (ruthless on 840b0ee).
            // Nothing fixed a shape for this key, so it is the pair
            // the model holds.
            serde_json::to_value(forks).unwrap()
        );
        assert_eq!(v["pr_reviews"].as_array().unwrap().len(), pr_reviews.len());
        assert_eq!(v["pr_reviews"][0]["pr"], 7);
        assert_eq!(
            v["pr_reviews"][0]["missing_reviewers"],
            serde_json::json!(["codex"])
        );
        assert_eq!(v["ad_hoc"].as_array().unwrap().len(), ad_hoc.len());
        assert_eq!(v["ad_hoc"][0]["sha"], "deadbeef");
    }

    #[test]
    fn to_json_populated_includes_conditional_keys() {
        // typed-json-not-json-macro: dirty + queue + shelved +
        // head_correction all present — every conditional key fires.
        let mut snap = minimal_snapshot();
        snap.dirty = Some(DirtyStats {
            insertions: 3,
            deletions: 1,
            binary: 0,
            untracked: 2,
        });
        snap.queue = vec![
            QueueItemView {
                priority: 500,
                name: "bar".to_string(),
                path: PathBuf::new(),
            },
            QueueItemView {
                priority: 500,
                name: "baz".to_string(),
                path: PathBuf::new(),
            },
        ];
        snap.stash = vec![StashItemView {
            stem: "old".to_string(),
            waiting_for: Some("bar".to_string()),
            ready: true,
            commits: 2,
        }];
        snap.head_correction = Some(clank_core::wait::HeadCorrection {
            sha: sha("deadbeef"),
            violation: clank_core::wait::HeadTagViolation {
                unknown: vec!["ghost".to_string()],
                untagged_touched: vec![plan_key("foo")],
                extra_named: vec![plan_key("bar")],
            },
        });
        let got = snap.to_json();

        assert_eq!(got["worktree_dirty"], true);
        assert_eq!(
            got["dirty_stats"],
            serde_json::json!({"insertions": 3, "deletions": 1, "binary": 0, "untracked": 2})
        );
        assert_eq!(got["queue_count"], 2);
        assert_eq!(got["queue"], serde_json::json!(["bar", "baz"]));
        // RENAMED key (rename-shelve-to-stash): `shelved` → `stash`, a
        // deliberate breaking wire change for dogfood consumers.
        assert_eq!(
            got["stash"],
            // `commits` is how big a pop is, which the pane shows and
            // the wire used to drop.
            serde_json::json!([{"plan": "old", "waiting_for": "bar", "ready": true, "commits": 2}])
        );
        assert_eq!(got["shelved"], serde_json::Value::Null, "old key gone");
        assert_eq!(
            got["head_correction"],
            serde_json::json!({
                "sha": "deadbeef",
                "unknown": ["ghost"],
                "untagged_touched": ["foo"],
                "extra_named": ["bar"],
            })
        );
    }

    #[test]
    fn to_json_finished_plan_only_when_no_active_plans() {
        // The finished-plan row appears only when there are no active
        // plans (mirrors the old `if self.plans.is_empty()` guard).
        let mut snap = minimal_snapshot();
        snap.plans = Vec::new();
        snap.blocks = Vec::new();
        snap.last_finished = Some(clank_core::repo_state::FinishedPlan {
            plan: plan_key("foo"),
            intro: sha("aaaaaaa"),
            finalized_at: sha("bbbbbbb"),
        });
        let got = snap.to_json();
        assert_eq!(
            got["finished_plans"],
            serde_json::json!([{
                "plan": "foo",
                "intro": "aaaaaaa",
                "finalized_at": "bbbbbbb",
            }])
        );
    }
}
