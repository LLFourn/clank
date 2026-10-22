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
    /// The serving monitor's build, and whether it is still the installed
    /// one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) monitor: Option<String>,
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
    #[serde(serialize_with = "active_plan_wire")]
    pub(crate) active_plan: Option<PlanWorkState>,
    /// The latest finished run, shown only when no active plan is selected.
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
    /// Plan ideas in `.clank/drafts/`, newest first. Only the panel and
    /// the remote page's count read them, so the wire omits them.
    #[serde(skip)]
    pub(crate) drafts: Vec<crate::cli::queue::Draft>,
    /// The agents' configured event sources, by agent and name: the
    /// panel's sources region. The wire omits them.
    #[serde(skip)]
    pub(crate) sources: Vec<crate::cli::source_status::ConfiguredSource>,
    /// What the running monitor says of those sources, if one runs.
    #[serde(skip)]
    pub(crate) sources_live: crate::cli::source_status::Live,
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) warnings: Vec<clank_core::repo_state::RepoWarning>,
}

/// One team agent as the TUI's agent panel sees it: its label,
/// roster role, effective auto-mode, and optional per-agent override.
/// The auto-mode is the *armed* state (takes effect at that agent's next Stop-hook
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
    /// `None` means the effective value comes from the user default
    /// or the built-in off default, as `clank auto status --json` says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) auto_mode_explicit: Option<clank_core::vocab::AutoMode>,
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
    /// The background work the agent attends, as its monitor last
    /// published it. Reported, never acted on.
    pub(crate) attending: Option<crate::cli::agent_state::Attended>,
    /// Why this agent's pending work is waiting for a person: its turn
    /// failed and the monitor will not, or can no longer, retry it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) waiting_for_you: Option<String>,
    /// What clank knows the agent is doing, as its monitor last published
    /// it, or why that is stale.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) state: Option<crate::cli::agent_state::View>,
    /// The question the agent is waiting on a person to answer, by its
    /// header, while one is pending in its bound session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) asking: Option<String>,
    /// How many times in a row the same items have been delivered, once
    /// that reaches the escalation threshold. A count, not a verdict.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) repeated: Option<u32>,
}

impl AgentAutoRow {
    /// The bound session the roster's tool has moved away from.
    pub(crate) fn superseded(&self) -> Option<&clank_core::Session> {
        let configured = clank_core::Tool::from_str(&self.tool)?;
        crate::cli::agent::superseded_session(configured, self.session.as_ref())
    }
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
        let state =
            crate::cli::agent_state::view(repo, label, session.as_ref().map(|s| s.id.as_str()));
        // What the monitor holds of the agent's turns, only while what it
        // published still describes this binding.
        let current = state
            .as_ref()
            .filter(|v| v.stale.is_none())
            .map(|v| &v.state);
        let asking = current.and_then(|s| s.asking.clone());
        let repeated = current.and_then(|s| s.repeated);
        let attending = current.and_then(|s| s.attending.clone());
        rows.push(AgentAutoRow {
            label: label.as_str().to_string(),
            role,
            auto_mode,
            auto_mode_explicit: cfg.as_ref().and_then(|c| c.auto_mode),
            tool: desc.tool.as_str().to_string(),
            invocation: agent_invocation(desc),
            session,
            attending,
            waiting_for_you: crate::cli::recovery::waiting_for_you(
                &crate::agent_store::agents_root(repo).join(label.as_str()),
            ),
            state,
            asking,
            repeated,
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

fn active_plan_wire<S: serde::Serializer>(
    plan: &Option<PlanWorkState>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match plan {
        Some(plan) => s.serialize_some(&Planned(plan)),
        None => s.serialize_none(),
    }
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
        m.serialize_entry("extra_named", &extra_named.as_ref().map(PlanKey::as_str))?;
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
            occurrence,
            question,
            answer,
        } = self.0;
        let mut m = s.serialize_map(Some(6))?;
        m.serialize_entry("agent", agent)?;
        m.serialize_entry("name", name)?;
        m.serialize_entry("occurrence", occurrence)?;
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

        let (active_plan, last_finished) = select_plan_and_finished(
            &work_status.active_plan,
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
            forks,
            repo_path: repo.to_path_buf(),
            monitor: crate::cli::monitor::build_line(repo),
            basename: basename.to_string(),
            branch,
            head_sha,
            head_subject,
            dirty,
            active_plan,
            last_finished,
            blocks,
            queue,
            drafts: crate::cli::queue::scan_drafts(repo),
            sources: crate::cli::source_status::configured(repo),
            sources_live: crate::cli::source_status::live(repo),
            master,
            agents,
            stash,
            log_rows,
            log_adopted_at: state.fold.adopted_at.clone(),
            log_stands_for: stands_for_plans(state),
            log_next,
            github_events,
            log_decorations,
            ad_hoc: work_status.ad_hoc,
            head_correction: work_status.head_correction,
            warnings: state
                .fold
                .warnings
                .iter()
                .filter(|warning| warning.is_amendable_at(state.head.as_ref()))
                .cloned()
                .collect(),
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
        if let Some(monitor) = &self.monitor {
            let _ = writeln!(out, "monitor: {monitor}");
        }
        for warning in &self.warnings {
            let _ = writeln!(
                out,
                "warning: {} ({})",
                warning.warning,
                short_sha(warning.sha.as_str())
            );
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
        for row in &self.agents {
            if let Some(old) = row.superseded() {
                let _ = writeln!(
                    out,
                    "tool changed: {} → bound to a {} session, configured {}; \
                     its next start runs {} fresh",
                    row.label,
                    old.tool.as_str(),
                    row.tool,
                    row.tool
                );
            }
            if let Some(state) = &row.state {
                let _ = writeln!(
                    out,
                    "state: {} → {}",
                    row.label,
                    state.line(crate::cli::recovery::unix_now())
                );
            }
            // A current state already says why it waits for a person.
            if let Some(why) = &row.waiting_for_you
                && row.state.as_ref().is_none_or(|s| s.stale.is_some())
            {
                let _ = writeln!(out, "waiting for you: {} → {why}", row.label);
            }
            if let Some(header) = &row.asking {
                let _ = writeln!(out, "asking you: {} → \"{header}\"", row.label);
            }
            if let Some(n) = row.repeated {
                let _ = writeln!(
                    out,
                    "repeating: {} → same items delivered {n} times",
                    row.label
                );
            }
            if let Some(a) = &row.attending {
                let _ = writeln!(
                    out,
                    "attending: {} → {}",
                    row.label,
                    a.summary(time::OffsetDateTime::now_utc())
                );
            }
        }

        if self.active_plan.is_none() && self.last_finished.is_none() {
            let _ = writeln!(out);
            let _ = writeln!(out, "no active plan, nothing pending");
        }

        if let Some(v) = &self.active_plan {
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

        if self.active_plan.is_none() {
            if let Some(fp) = &self.last_finished {
                let _ = writeln!(out);
                let _ = writeln!(
                    out,
                    "last finished: {} (finalized {}, duration {})",
                    fp.plan.as_str(),
                    short_sha(fp.finalized_at.as_str()),
                    crate::age::duration(fp.duration_secs)
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
/// pane shows the same window whether or not a plan is active
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
    let items = crate::cli::log::interleave_with_plain(
        &newest_first,
        &history.plain,
        &snap.events,
        &snap.nudges,
    );
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
use crate::repo_watch::{CLANK_STATUS_ONLY_DIRS, CLANK_WAKE_DIRS};

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
    /// The monitor's account of its sources, which changes outside the
    /// repo: a producer starting or ending, or the monitor going.
    sources: u64,
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
        sources: sources_fingerprint(repo),
    })
}

/// Hash of the sources account the monitor publishes, as the panel would
/// read it, including whether its monitor still runs, and of each
/// active timer's state, which changes when its evidence does.
fn sources_fingerprint(repo: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{:?}", crate::cli::source_status::live(repo)).hash(&mut h);
    for source in crate::cli::source_status::configured(repo) {
        if let Some(state) = &source.timer {
            (source.agent.as_str(), &source.key, format!("{state:?}")).hash(&mut h);
        }
    }
    h.finish()
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
        let Some(attended) =
            crate::cli::agent_state::load(&crate::agent_store::agents_root(repo).join(&name))
                .and_then(|s| s.attending)
        else {
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
    for dir in CLANK_WAKE_DIRS.iter().chain(CLANK_STATUS_ONLY_DIRS) {
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
        // delivered. The status-only dirs under `.clank/` are the
        // exception: no gate reads them, so only this watcher delivers
        // them.
        if let Ok(rel) = path.strip_prefix(&self.clank_root) {
            return rel
                .components()
                .next()
                .and_then(|c| c.as_os_str().to_str())
                .is_some_and(|head| CLANK_STATUS_ONLY_DIRS.contains(&head));
        }
        if path.starts_with(&self.git_dir) {
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
    // Gate state comes from the repo's MONITOR when one is running —
    // one watcher and one fold for the whole repo, which is the point
    // of `a-repo-is-watched-once-not-once-per-waiter`: this panel used
    // to attach a watcher of its own alongside one per parked wait.
    // With no monitor it attaches that watcher itself, so the panel
    // behaves the same either way. `status` is always native (no poll
    // mode).
    let core = crate::cli::monitor::core_wakes(repo, tx.clone())?;
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
    /// The shared fold, or a watcher of our own — see
    /// [`crate::cli::monitor::core_wakes`].
    _core: crate::cli::monitor::CoreWakes,
    /// Kept local on purpose: the monitor has no reason to compute a
    /// diff, and nothing but this panel wants one.
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

fn select_plan_and_finished(
    work_plan: &Option<PlanWorkState>,
    fold: &clank_core::repo_state::RepoState,
    basename: &str,
    plan_arg: Option<&str>,
    watch_mode: bool,
) -> anyhow::Result<(Option<PlanWorkState>, Option<FinishedPlan>)> {
    if let Some(raw) = plan_arg {
        let stem = parse_arg(raw, basename)?;
        let key = PlanKey::parse(&stem)
            .map_err(|e| anyhow::anyhow!("invalid plan stem `{stem}`: {e}"))?;

        if let Some(matching) = work_plan.as_ref().filter(|ps| ps.plan == key) {
            return Ok((Some(matching.clone()), None));
        }

        if watch_mode {
            let last_finished = fold
                .finished_plans
                .iter()
                .rev()
                .find(|fp| fp.plan == key)
                .cloned();
            if last_finished.is_some() {
                return Ok((None, last_finished));
            }
        }

        anyhow::bail!(
            "plan `{basename}/{stem}.md` not active. active: {}",
            active_summary(fold, basename)
        );
    }

    let active_plan = work_plan.clone();
    let last_finished = if active_plan.is_none() {
        fold.finished_plans.last().cloned()
    } else {
        None
    };
    Ok((active_plan, last_finished))
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
    if let Some(extra) = &v.extra_named {
        parts.push(format!(
            "tag names [{}] whose plan file it didn't touch",
            extra.as_str()
        ));
    }
    if let Some(unknown) = &v.unknown {
        parts.push(format!("tag names unknown plan [{}]", unknown));
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
    fold.active_plan.as_ref().map_or_else(
        || "(none)".into(),
        |p| format!("{basename}/{}.md", p.key.as_str()),
    )
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
                .any(|r| matches!(r, crate::cli::log::OnelineRow::Header { plan: Some(p), .. } if p == "solo")),
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
                duration_secs: 0,
                plan: plan("solo"),
                intro: sha("aa"),
                finalized_at: sha("aa"),
            },
            // Two: they are not.
            FinishedPlan {
                duration_secs: 0,
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
        // Drafts are the panel's alone: no core watcher delivers them.
        assert!(f.wakes(&p(".clank/drafts/idea.md")));
        assert!(f.wakes(&p(".clank/drafts")));

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

        let items = crate::cli::log::interleave_with_plain(&[], &history.plain, &[], &[]);
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
        let items = crate::cli::log::interleave_with_plain(&folded, &history.plain, &[], &[]);
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

    /// Unqueueing a plan makes it a draft in the very next snapshot the
    /// panel would build, and the refresh that builds it is not skipped.
    #[tokio::test]
    async fn an_unqueued_plan_is_a_draft_in_the_next_snapshot() {
        let dir = fixture_repo();
        let r = dir.path();
        std::fs::create_dir_all(r.join(".clank/queue")).unwrap();
        std::fs::write(r.join(".clank/queue/500-idea.md"), "# idea\n\nbody\n").unwrap();
        let before = snapshot(r, None).await.unwrap();
        assert_eq!(before.queue.len(), 1);
        assert!(before.drafts.is_empty());
        let signature = input_signature(r).unwrap();
        crate::cli::queue::unqueue(r, "idea").unwrap();
        assert_ne!(signature, input_signature(r).unwrap());
        let after = snapshot(r, None).await.unwrap();
        assert!(after.queue.is_empty());
        assert_eq!(
            after
                .drafts
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>(),
            ["idea"]
        );
    }

    /// A draft written, edited (a new mtime) or removed changes what the
    /// panel shows, so a refresh must not reuse the snapshot.
    #[test]
    fn clank_fingerprint_flips_on_every_draft_change() {
        let dir = fixture_repo();
        let r = dir.path();
        let draft = r.join(".clank/drafts/idea.md");
        let before = clank_input_fingerprint(r);
        std::fs::create_dir_all(draft.parent().unwrap()).unwrap();
        std::fs::write(&draft, "# idea\n\nfirst\n").unwrap();
        let written = clank_input_fingerprint(r);
        assert_ne!(before, written, "a new draft");
        let file = std::fs::File::options().write(true).open(&draft).unwrap();
        file.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(7))
            .unwrap();
        let edited = clank_input_fingerprint(r);
        assert_ne!(written, edited, "an edited draft");
        std::fs::remove_file(&draft).unwrap();
        assert_ne!(edited, clank_input_fingerprint(r), "a removed draft");
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

    fn agent_row(
        label: &str,
        attending: Option<crate::cli::agent_state::Attended>,
    ) -> AgentAutoRow {
        AgentAutoRow {
            label: label.to_string(),
            role: crate::cli::teams_config::RosterRole::Master,
            auto_mode: clank_core::vocab::AutoMode::On,
            auto_mode_explicit: None,
            tool: "claude".to_string(),
            invocation: "claude".to_string(),
            session: None,
            waiting_for_you: None,
            state: None,
            asking: None,
            repeated: None,
            attending,
        }
    }

    fn marker(task: &str, pid: Option<i32>, at: Option<&str>) -> crate::cli::agent_state::Attended {
        crate::cli::agent_state::Attended {
            expect: None,
            desc: None,
            token: None,
            task: task.to_string(),
            pid,
            at: at.unwrap_or("2026-08-20T14:51:09Z").to_string(),
            subagent: false,
            pending: false,
            check_in: None,
        }
    }

    #[test]
    fn a_changed_tool_is_said_until_the_new_session_binds() {
        let bound = |tool: &str| -> clank_core::Session {
            serde_json::from_str(&format!(
                r#"{{ "id": "01a156a7-f516-7dc0-8b58-520faf122796", "tool": "{tool}",
                     "updated_at": "x" }}"#
            ))
            .unwrap()
        };
        let mut snap = minimal_snapshot();
        let mut row = agent_row("browser2", None);
        row.tool = "claude".into();
        row.session = Some(bound("codex"));
        snap.agents = vec![row];
        let out = snap.to_human();
        assert!(
            out.contains(
                "tool changed: browser2 → bound to a codex session, configured claude; \
                 its next start runs claude fresh"
            ),
            "{out}"
        );
        snap.agents[0].session = Some(bound("claude"));
        assert!(!snap.to_human().contains("tool changed"));
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

    /// The status reports the count the monitor published, never a
    /// verdict on the agent; a count published for a session that is no
    /// longer bound is not shown.
    #[test]
    fn a_repeating_agent_is_reported_by_its_count() {
        let mut snap = minimal_snapshot();
        let mut row = agent_row("claude", None);
        row.repeated = Some(6);
        row.asking = Some("Life monitor".into());
        snap.agents = vec![row];
        let text = snap.to_human();
        assert!(
            text.contains("repeating: claude → same items delivered 6 times"),
            "{text}"
        );
        assert!(
            text.contains("asking you: claude → \"Life monitor\""),
            "{text}"
        );
        assert!(
            !text.contains("ignor") && !text.contains("inactive"),
            "{text}"
        );

        let repo = crate::cli::monitor::TempRepo::new();
        let label = clank_core::ids::AgentLabel::parse("claude").unwrap();
        crate::agent_store::bind_session_to_agent(
            repo.path(),
            &label,
            clank_core::Tool::Claude,
            &clank_core::SessionId::parse("session-one").unwrap(),
        )
        .unwrap();
        let dir = crate::agent_store::agents_root(repo.path()).join("claude");
        let me = std::process::id() as i32;
        let role = crate::agent_store::resolve_role(repo.path(), &label)
            .map(|r| r.to_string())
            .unwrap_or_default();
        crate::cli::monitor::publish(
            repo.path(),
            &crate::cli::monitor::describe(repo.path(), me, std::path::Path::new("socket")),
        )
        .unwrap();
        let shown = |count: u32, session: &str| {
            std::fs::create_dir_all(&dir).unwrap();
            crate::cli::agent_state::store(
                &dir,
                &crate::cli::agent_state::AgentState {
                    role: role.clone(),
                    session: session.into(),
                    tool: clank_core::Tool::Claude,
                    monitor: crate::cli::agent_state::Observer::me(),
                    activity: crate::cli::agent_state::Activity::Idle,
                    since: 0,
                    suppressors: Vec::new(),
                    nudge: crate::cli::agent_state::Nudge::Nothing,
                    resumes: None,
                    cannot_resume: None,
                    attending: None,
                    asking: Some("Life monitor".into()),
                    repeated: Some(count),
                },
            );

            let set = crate::cli::teams_config::RegisteredSet {
                master: label.clone(),
                master_desc: crate::cli::teams_config::AgentDescription {
                    tool: clank_core::Tool::Claude,
                    launch: None,
                    initial_prompt: None,
                },
                members: Vec::new(),
            };
            let row = roster_auto_rows(repo.path(), None, &set).remove(0);
            (row.repeated, row.asking)
        };
        assert_eq!(
            shown(5, "session-one"),
            (Some(5), Some("Life monitor".into()))
        );
        assert_eq!(
            shown(5, "session-old"),
            (None, None),
            "another session's count and question"
        );
    }

    #[test]
    fn an_agent_left_to_a_person_says_so() {
        let mut snap = minimal_snapshot();
        let mut row = agent_row("codex", None);
        row.waiting_for_you = Some("codex's turn failed (server_overloaded)".into());
        snap.agents = vec![row];
        assert!(
            snap.to_human()
                .contains("waiting for you: codex → codex's turn failed (server_overloaded)"),
            "{}",
            snap.to_human()
        );
    }

    /// Plain status gives each agent's current state with its reasons,
    /// once: a current state already says why it waits for a person, and
    /// a stale one says it is stale and leaves the reason to the episode.
    #[test]
    fn plain_status_gives_each_agents_state() {
        use crate::cli::agent_state::{Activity, AgentState, Nudge, Observer, View};
        let state = |stale| View {
            state: AgentState {
                role: "commit".into(),
                session: "s1".into(),
                tool: clank_core::Tool::Codex,
                monitor: Observer {
                    pid: 1,
                    birth: None,
                },
                activity: Activity::Failed("billing_error".into()),
                since: 0,
                suppressors: Vec::new(),
                nudge: Nudge::WaitingForYou("this kind of failure needs a person".into()),
                resumes: None,
                cannot_resume: Some("no evidence".into()),
                attending: None,
                asking: None,
                repeated: None,
            },
            stale,
        };
        let shown = |stale| {
            let mut snap = minimal_snapshot();
            let mut row = agent_row("codex", None);
            row.waiting_for_you = Some("this kind of failure needs a person".into());
            row.state = Some(state(stale));
            snap.agents = vec![row];
            snap.to_human()
        };
        let current = shown(None);
        assert!(
            current.contains("state: codex → failed (billing_error)"),
            "{current}"
        );
        assert!(current.contains("no evidence"), "{current}");
        assert_eq!(current.matches("needs a person").count(), 1, "{current}");
        let stale = shown(Some("its monitor is gone"));
        assert!(
            stale.contains("state: codex → stale: its monitor is gone"),
            "{stale}"
        );
        assert!(stale.contains("waiting for you: codex"), "{stale}");
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

    fn minimal_snapshot() -> StatusSnapshot {
        StatusSnapshot {
            forks: Vec::new(),
            repo_path: PathBuf::from("/repo"),
            monitor: None,
            basename: "repo".to_string(),
            branch: Some("main".to_string()),
            head_sha: Some("abcd123".to_string()),
            head_subject: Some("do the thing".to_string()),
            dirty: None,
            active_plan: Some(PlanWorkState {
                handover: Default::default(),
                plan: plan_key("foo"),
                sha: Some(sha("0123456789abcdef0123456789abcdef01234567")),
                gate: CommitGateState::Unreviewed,
                waiting_on: WaitingOn::MasterToContinue,
                touched_code: true,
            }),
            last_finished: None,
            blocks: vec![crate::cli::block::BlockEntry {
                agent: "codex".to_string(),
                name: "q".to_string(),
                question: "why?".to_string(),
                answer: None,
                occurrence: None,
            }],
            queue: Vec::new(),
            drafts: Vec::new(),
            sources: Vec::new(),
            sources_live: crate::cli::source_status::Live::Unavailable,
            master: None,
            agents: Vec::new(),
            stash: Vec::new(),
            log_rows: Vec::new(),
            log_adopted_at: None,
            log_stands_for: Default::default(),
            log_next: None,
            github_events: Vec::new(),
            log_decorations: Default::default(),
            ad_hoc: Vec::new(),
            head_correction: None,
            warnings: Vec::new(),
        }
    }

    /// `clank status` is where a finalize is legible, now that it is
    /// not delivered as a wake.
    ///
    /// `a-repo-transition-is-derived-not-delivered` retired the
    /// `repo-lifecycle` source, whose `Finished` notice asked for
    /// nothing but an ack. The state it announced is derived, and this
    /// line is where it shows — untested until the notice went away.
    #[test]
    fn an_idle_repo_says_which_plan_finished_last() {
        let mut snap = minimal_snapshot();
        snap.active_plan = None;
        snap.blocks = Vec::new();
        snap.last_finished = Some(clank_core::repo_state::FinishedPlan {
            duration_secs: 0,
            plan: clank_core::ids::PlanKey::parse("done").unwrap(),
            intro: clank_core::ids::CommitSha::parse(&"1".repeat(40)).unwrap(),
            finalized_at: clank_core::ids::CommitSha::parse(&"2".repeat(40)).unwrap(),
        });

        let out = snap.to_human();

        assert!(
            out.contains("last finished: done (finalized 2222222, duration 0s)"),
            "the finalize must still be readable somewhere: {out}"
        );
        assert!(
            !out.contains("no active plan, nothing pending"),
            "a finalize is not nothing: {out}"
        );
    }

    #[test]
    fn to_json_minimal_omits_conditional_keys() {
        // The typed snapshot is the wire, including its optional active
        // and latest-finished values. Conditional keys are absent here.
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
            "active_plan": {
                // The two keys the wire fixed before this was one
                // model, and the rest of what a plan's state holds.
                "plan": "foo",
                "latest_reviewable_sha": "0123456789abcdef0123456789abcdef01234567",
                "gate_state": "unreviewed",
                "waiting_on": WaitingOn::MasterToContinue,
                "handover": {"opened": null, "commit_tier_closed": null, "second_tier_closed": null},
                "touched_code": true,
            },
            "last_finished": null,
            "blocks": [{
                "agent": "codex",
                "name": "q",
                "occurrence": null,
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
        snap.ad_hoc = vec![clank_core::wait::AdHocWorkState {
            sha: sha("deadbeef"),
            gate: CommitGateState::Unreviewed,
            handover: Default::default(),
        }];
        let v = snap.to_json();

        let StatusSnapshot {
            // The facts, each asserted below.
            basename,
            branch,
            head_sha,
            head_subject,
            dirty,
            active_plan,
            last_finished: _,
            blocks,
            queue,
            master,
            agents,
            stash: _,
            // The panel's and the remote page's; the wire skips them.
            drafts: _,
            sources: _,
            sources_live: _,
            forks,
            ad_hoc,
            head_correction: _,
            warnings,
            // Emitted, and pinned by the wire fixture below rather
            // than here: `--repo` means the reader need not be
            // standing in it (codex on 772b84f).
            repo_path: _,
            // Read from the runtime namespace, not the repo: shown when a
            // monitor is published, and absent from a fixture without one.
            monitor: _,
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

        assert!(warnings.is_empty() && v.get("warnings").is_none());
        assert_eq!(v["repo_basename"], **basename);
        assert_eq!(v["branch"], *branch.as_deref().unwrap());
        assert_eq!(v["head_sha"], *head_sha.as_deref().unwrap());
        assert_eq!(v["head_subject"], *head_subject.as_deref().unwrap());
        assert_eq!(v["worktree_dirty"], dirty.is_some());
        assert_eq!(v["active_plan"].is_object(), active_plan.is_some());
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
                unknown: Some("ghost".to_string()),
                untagged_touched: vec![plan_key("foo")],
                extra_named: Some(plan_key("bar")),
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
                "unknown": "ghost",
                "untagged_touched": ["foo"],
                "extra_named": "bar",
            })
        );
    }

    #[test]
    fn to_json_finished_plan_when_active_slot_is_empty() {
        // The idle projection carries the latest finished run as an object.
        let mut snap = minimal_snapshot();
        snap.active_plan = None;
        snap.blocks = Vec::new();
        snap.last_finished = Some(clank_core::repo_state::FinishedPlan {
            duration_secs: 0,
            plan: plan_key("foo"),
            intro: sha("aaaaaaa"),
            finalized_at: sha("bbbbbbb"),
        });
        let got = snap.to_json();
        assert_eq!(
            got["last_finished"],
            serde_json::json!({
                "plan": "foo",
                "intro": "aaaaaaa",
                "finalized_at": "bbbbbbb",
                "duration_secs": 0,
            })
        );
    }
}
