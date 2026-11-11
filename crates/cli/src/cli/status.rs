//! `clank status` — read-only summary of the current repo's Clank
//! state. Folds the repo, projects per-plan via `clank-core::plan_view`,
//! then renders. Never blocks.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

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
    /// Whether the repo's monitor runs, and its build. On the wire as
    /// the `monitor:` line.
    #[serde(serialize_with = "monitor_wire")]
    pub(crate) monitor: crate::cli::monitor::Liveness,
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
    /// Why a config document could not be read, once per distinct
    /// failure. The snapshot is still built, from the defaults, so the
    /// failure is shown rather than the status lost.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) config_errors: Vec<String>,
}

/// Keep `error` as a config failure the snapshot shows, once.
fn config_failed(errors: &mut Vec<String>, error: anyhow::Error) {
    let line = format!("{error:#}");
    if !errors.contains(&line) {
        errors.push(line);
    }
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
    /// Every piece of background work the agent attends, as its monitor
    /// last published them. Reported, never acted on.
    pub(crate) attending: Vec<crate::cli::agent_state::Attended>,
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
    /// Why the agent's own config cannot be read, when it cannot: its
    /// auto mode, binding and sources are then unknown, not off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) unreadable: Option<String>,
}

impl AgentAutoRow {
    /// The bound session the roster's tool has moved away from.
    pub(crate) fn superseded(&self) -> Option<&clank_core::Session> {
        let configured = clank_core::Tool::from_name(&self.tool)?;
        crate::cli::agent::superseded_session(configured, self.session.as_ref())
    }

    /// The attendance a wait page opens on: the first that covers its
    /// work, else the first.
    pub(crate) fn primary_wait(&self) -> Option<&crate::cli::agent_state::Attended> {
        self.attending
            .iter()
            .find(|att| att.covers())
            .or_else(|| self.attending.first())
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
    config_errors: &mut Vec<String>,
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
        let (cfg, unreadable) = match crate::agent_store::load_agent_config(repo, label) {
            Ok(cfg) => (cfg, None),
            Err(e) => {
                let why = format!("{e:#}");
                config_failed(config_errors, e);
                (None, Some(why))
            }
        };
        let auto_mode = crate::cli::team::resolve_effective_auto_mode(cfg.as_ref(), home)
            .unwrap_or_else(|e| {
                config_failed(config_errors, e);
                clank_core::vocab::AutoMode::Off
            });
        let session = cfg.as_ref().and_then(|c| c.session.clone());
        if let Err(why) = crate::cli::agent_state::read(
            &crate::agent_store::agents_root(repo).join(label.as_str()),
        ) {
            config_failed(config_errors, anyhow::anyhow!(why));
        }
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
        let attending = current.map(|s| s.attending.clone()).unwrap_or_default();
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
            unreadable,
        });
    }
    rows
}

/// Global-library agents (`~/.clank` `agents`) not already on `roster`
/// — the "+ add" candidates, each with its tool. Read FRESH from the
/// library by the TUI loop when the picker opens (never cached), so
/// a `clank team agent add` made elsewhere shows up immediately.
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

fn monitor_wire<S: serde::Serializer>(
    monitor: &crate::cli::monitor::Liveness,
    s: S,
) -> Result<S::Ok, S::Error> {
    s.serialize_str(&monitor.line())
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
            kind: _,
            question,
            answer,
            nudge,
            unreadable,
        } = self.0;
        let mut m = s.serialize_map(Some(10))?;
        m.serialize_entry("agent", agent)?;
        m.serialize_entry("name", name)?;
        m.serialize_entry("occurrence", occurrence)?;
        m.serialize_entry("kind", self.0.kind_name())?;
        m.serialize_entry("suggestion", &self.0.suggestion())?;
        m.serialize_entry("question", question)?;
        m.serialize_entry("answer", answer)?;
        m.serialize_entry("pending", &answer.is_none())?;
        m.serialize_entry("nudge", &nudge.as_ref().map(Unsettled))?;
        m.serialize_entry("unreadable", unreadable)?;
        m.end()
    }
}

/// An unsettled nudge as the wire says it: its outcome is unknown until a
/// person settles it.
/// An unreadable record says only why.
struct Unsettled<'a>(&'a crate::cli::block::Stored<crate::cli::block::Attempt>);

impl serde::Serialize for Unsettled<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(6))?;
        m.serialize_entry("outcome", "unknown")?;
        m.serialize_entry("reason", &self.0.unknown_reason())?;
        match self.0 {
            crate::cli::block::Stored::Valid(crate::cli::block::Attempt {
                occurrence: _,
                attempt,
                text,
                started,
                diagnostic,
            }) => {
                m.serialize_entry("diagnostic", diagnostic)?;
                m.serialize_entry("attempt", attempt)?;
                m.serialize_entry("text", text)?;
                m.serialize_entry("started", started)?;
            }
            crate::cli::block::Stored::Unreadable(why) => {
                m.serialize_entry("diagnostic", why)?;
                m.serialize_entry("attempt", &None::<String>)?;
                m.serialize_entry("text", &None::<String>)?;
                m.serialize_entry("started", &None::<u64>)?;
            }
        }
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
        Self::build_with(
            repo,
            basename,
            home,
            policy,
            plan_arg,
            watch_mode,
            &GitStatus::default(),
        )
        .await
    }

    /// [`Self::build_async`], reading the Git status from `git_status`.
    pub(crate) async fn build_with(
        repo: &Path,
        basename: &str,
        home: Option<&Path>,
        policy: crate::rebuild::CachePolicy,
        plan_arg: Option<&str>,
        watch_mode: bool,
        git_status: &GitStatus,
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
            git_status,
        )
    }

    #[allow(clippy::too_many_arguments)]
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
        git_status: &GitStatus,
    ) -> anyhow::Result<Self> {
        // One ODB handle for the whole snapshot: HEAD facts, the dirty
        // walk when the cache is stale, the commit-tag HEAD read, and
        // every per-plan worktree diff share it (no per-read/per-plan
        // re-open). Opaque
        // `git_io::Repo` so this stays off the gix boundary
        // (gix-not-git-gate).
        let git = crate::git_io::open(repo)?;
        let (branch, head_sha, head_subject) = head_info(&git);
        let dirty = git_status.read(&git)?;

        // `status` is a read-only renderer (also reused by `clank html`), so
        // a config it cannot read becomes a failed config line over the
        // defaults rather than a lost status: no team → empty reviewer
        // tiers → gate computes as zero-reviewer (Continued). The
        // workflow-driving commands (wait / finish / promote) refuse.
        let mut config_errors = Vec::new();
        let config = crate::cli::config::load_with_home(repo, home).unwrap_or_else(|e| {
            config_failed(&mut config_errors, e);
            crate::cli::config::Config::default()
        });
        if let Some(home) = home
            && let Err(e) = crate::cli::team::read_user_config(home)
        {
            config_failed(&mut config_errors, e);
        }
        let registered =
            crate::agent_store::try_resolve_via_team_with(repo, home).unwrap_or_else(|e| {
                config_failed(&mut config_errors, e);
                None
            });
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
            .map(|set| roster_auto_rows(repo, home, set, &mut config_errors))
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
            monitor: crate::cli::monitor::liveness(repo),
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
            sources: crate::cli::source_status::configured(repo).unwrap_or_else(|e| {
                config_failed(&mut config_errors, e);
                Vec::new()
            }),
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
            config_errors,
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
        self.human(false)
    }

    /// [`Self::to_human`], with an interrupted agent's state in colour
    /// when `paint`.
    pub(crate) fn human(&self, paint: bool) -> String {
        let mut out = String::new();
        use std::fmt::Write as _;

        let _ = writeln!(out, "repo:   {}", self.repo_path.display());
        if let Some(b) = &self.branch {
            let _ = writeln!(out, "branch: {b}");
        }
        if let Some(s) = &self.head_sha {
            let _ = writeln!(out, "head:   {s}");
        }
        let _ = writeln!(out, "monitor: {}", self.monitor.line());
        for line in crate::cli::source_status::status_lines(
            &self.sources,
            &self.sources_live,
            crate::cli::github::log::now_epoch(),
            paint,
        ) {
            let _ = writeln!(out, "{line}");
        }
        for error in &self.config_errors {
            let _ = writeln!(out, "config: FAILED — {error}");
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
                let now = crate::cli::recovery::unix_now();
                let line = state.line(now);
                let line = match state.alarm(now).filter(|_| paint) {
                    Some(alarm) => format!("\x1b[{}m{line}\x1b[0m", alarm.sgr()),
                    None => line,
                };
                let _ = writeln!(out, "state: {} → {line}", row.label);
            } else if row.session.is_some() {
                let _ = writeln!(
                    out,
                    "state: {} → {}",
                    row.label,
                    crate::cli::agent_state::UNKNOWN
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
            for a in &row.attending {
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

        if self.active_plan.is_none()
            && let Some(fp) = &self.last_finished
        {
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "last finished: {} (finalized {}, duration {})",
                fp.plan.as_str(),
                short_sha(fp.finalized_at.as_str()),
                crate::age::duration(fp.duration_secs)
            );
        }

        if !self.blocks.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(out, "blocks:");
            for b in &self.blocks {
                let who = match b.kind_name() {
                    "plain" => b.agent.clone(),
                    kind => format!("{}, {kind}", b.agent),
                };
                if let Some(ref answer) = b.answer {
                    let _ = writeln!(out, "  UNBLOCKED ({who}): {}", b.question);
                    let _ = writeln!(out, "    answer: {answer}");
                } else {
                    let _ = writeln!(out, "  BLOCKED ({who}): {}", b.question);
                }
                if let Some(suggestion) = b.suggestion() {
                    let _ = writeln!(out, "    suggested: {suggestion}");
                }
                if let Some(nudge) = &b.nudge {
                    let _ = writeln!(out, "    {}", nudge.unknown_reason());
                    if let Some(a) = nudge.valid() {
                        let _ = writeln!(out, "    typed: {}", a.text);
                    }
                }
                if let Some(why) = &b.unreadable {
                    let _ = writeln!(out, "    unreadable: {why}");
                }
            }
        }

        out.trim_end_matches('\n').to_string()
    }
}

pub async fn run(args: StatusArgs) -> anyhow::Result<()> {
    let policy = if args.no_cache {
        crate::rebuild::CachePolicy::Bypass
    } else {
        crate::rebuild::CachePolicy::Use
    };

    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);

    if args.watch {
        let panel = super::status_watch::StatusPanel {
            watches: Watches::default(),
            home,
            policy,
            plan: args.plan.clone(),
        };
        return super::status_watch::run(args, panel).await;
    }
    if args.repo.len() > 1 {
        anyhow::bail!("several repos are shown only by `status --watch -j`");
    }
    let repo = resolve_repo(args.repo.first().map(PathBuf::as_path))?;
    let basename = repo_basename(&repo)?;

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
        print!("{}", snapshot.human(paint_stdout()));
        println!();
    }
    Ok(())
}

/// Whether human output may carry colour: stdout is a terminal and
/// `NO_COLOR` is unset.
pub(crate) fn paint_stdout() -> bool {
    use std::io::IsTerminal as _;
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
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
    log_rows_windowed(
        repo,
        state.head.as_ref(),
        LOG_WINDOW,
        state.fold.adopted_at.as_ref(),
        &stands_for_plans(state),
    )
    .await
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
    log_rows_page(repo, Some(cursor), page_size, adopted_at, false, stands_for).await
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
    let head = crate::cli::log::git_rev_parse(repo, "HEAD");
    log_rows_windowed(repo, head.as_ref(), window, adopted_at, stands_for).await
}

/// Oneline log rows for `HEAD~window..head`, newest first, with the
/// timeline's events. An unborn branch (`head` is `None`) has only the
/// events.
async fn log_rows_windowed(
    repo: &Path,
    head: Option<&crate::lifecycle::CommitSha>,
    window: usize,
    adopted_at: Option<&crate::lifecycle::CommitSha>,
    stands_for: &crate::cli::log::StandsFor,
) -> TuiLogRead {
    let cursor = head.map(|head| crate::cli::log::HistoryCursor {
        tip: head.clone(),
        post_adoption: adopted_at.is_some(),
    });
    log_rows_page(repo, cursor.as_ref(), window, adopted_at, true, stands_for).await
}

async fn log_rows_page(
    repo: &Path,
    cursor: Option<&crate::cli::log::HistoryCursor>,
    window: usize,
    adopted_at: Option<&crate::lifecycle::CommitSha>,
    include_github: bool,
    stands_for: &crate::cli::log::StandsFor,
) -> TuiLogRead {
    use clank_core::repo_state::LogEvent;

    let history = match cursor {
        Some(cursor) => {
            let Ok(history) = crate::cli::log::history_page(repo, cursor, window, adopted_at).await
            else {
                return TuiLogRead::default();
            };
            history
        }
        None => crate::cli::log::HistoryWindow::default(),
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
        history.next.is_some(),
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

/// What one filesystem event says to a status loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Seen {
    /// Nothing the panel shows: ignored churn, Git's object store, and
    /// the `.clank/` state the core watcher already delivers.
    Nothing,
    /// Something the panel shows that Git's status does not depend on.
    Shown,
    /// Something Git's status depends on: the cached [`GitStatus`] is
    /// stale.
    GitInput,
}

/// Classifies the status loops' filesystem events by Git's own view of
/// the repo, never by path prefix or ignore pattern alone.
///
/// A [`Seen::GitInput`] is a path the index holds (so a tracked plan
/// under `.clank/` and a force-added file under an ignored directory
/// both count), an untracked path that is not ignored, the index, HEAD
/// and the refs, and whatever decides what Git sees: a `.gitignore`,
/// `info/exclude`, the repo's config, and the files
/// [`crate::git_io::status_inputs_elsewhere`] names. Ignored churn
/// (`target/`, Git-ignored runtime state under `.clank/`) is
/// [`Seen::Nothing`], honoring nested `.gitignore` files: a root-only
/// matcher once woke ~250×/s on a subproject's ignored `build/`.
pub(crate) struct WakeFilter {
    repo_root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    clank_root: PathBuf,
    /// The files elsewhere as last read; the watch on them reads the set
    /// again whenever one changes, and updates it here.
    elsewhere: Known,
    ignore: crate::git_io::PathIgnore,
    /// Raised by the watch on the files elsewhere, which runs on
    /// another thread than the one this filter is driven on.
    stale_ignore: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

// The `.clank/` workflow-state signal dirs the snapshot depends on —
// the SHARED allowlist the core watcher wakes on. One source of truth
// in `repo_watch`, so the reuse fingerprint and the wake rule agree.
use crate::repo_watch::{CLANK_STATUS_ONLY_DIRS, CLANK_WAKE_DIRS};

/// A cheap fingerprint of everything a [`StatusSnapshot`] is derived
/// from — the SAME inputs the status loops wake on: committed history
/// (HEAD), the working tree (`dirty`, from the [`GitStatus`] cache), and
/// the gitignored
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
    health: ProcessHealth,
    /// The monitor's account of its sources, which changes outside the
    /// repo: a producer starting or ending, or the monitor going.
    sources: u64,
}

/// The snapshot inputs that change with no file write: a process
/// exiting. The TUI's idle probe reads exactly this, and the signature
/// carries it, so a change the probe raises is one the gate keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessHealth {
    /// See [`attendance_fingerprint`].
    pub(crate) attendances: u64,
    pub(crate) monitor: crate::cli::monitor::Liveness,
}

pub(crate) fn process_health(repo: &Path) -> ProcessHealth {
    ProcessHealth {
        attendances: attendance_fingerprint(repo),
        monitor: crate::cli::monitor::liveness(repo),
    }
}

/// Compute the current [`InputSignature`]. One ODB open for HEAD; the
/// Git status comes from `git_status`, walked only if an event made it
/// stale; the `.clank` fingerprint is stat-only (no file reads).
pub(crate) fn input_signature(
    repo: &Path,
    git_status: &GitStatus,
) -> anyhow::Result<InputSignature> {
    input_signature_with(repo, git_status, process_health(repo))
}

/// [`input_signature`] with its [`ProcessHealth`] supplied.
pub(crate) fn input_signature_with(
    repo: &Path,
    git_status: &GitStatus,
    health: ProcessHealth,
) -> anyhow::Result<InputSignature> {
    let git = crate::git_io::open(repo)?;
    let head = git.head_sha()?.map(|s| s.as_str().to_string());
    let dirty = git_status.read(&git)?;
    Ok(InputSignature {
        head,
        dirty,
        clank: clank_input_fingerprint(repo),
        health,
        sources: sources_fingerprint(repo),
    })
}

/// The working tree's Git status (`DirtyStats`, diff statistics
/// included), walked once per change and shared by every reader in a
/// process: the gate probe and the snapshot build.
///
/// It is fresh until [`GitStatus::invalidate`], which the status loops'
/// Git watcher calls for exactly the events [`WakeFilter`] calls
/// [`Seen::GitInput`]. A new one is stale, so a one-shot reader walks
/// once. Clones share one cache.
#[derive(Clone, Default)]
pub(crate) struct GitStatus(std::sync::Arc<GitStatusCache>);

#[derive(Default)]
struct GitStatusCache {
    fresh: std::sync::atomic::AtomicBool,
    last: std::sync::Mutex<Option<Option<DirtyStats>>>,
}

impl GitStatus {
    /// Something the status depends on changed: the next read walks.
    pub(crate) fn invalidate(&self) {
        self.0
            .fresh
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// The status, walking the tree only if it is stale.
    pub(crate) fn read(
        &self,
        git: &crate::git_io::Repo,
    ) -> Result<Option<DirtyStats>, crate::git_io::GitIoError> {
        use std::sync::atomic::Ordering;
        let mut last = self
            .0
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Marked fresh before the walk, so an event landing during it
        // marks it stale again and the next read walks.
        if self.0.fresh.swap(true, Ordering::AcqRel)
            && let Some(dirty) = *last
        {
            return Ok(dirty);
        }
        match git.working_tree_dirty() {
            Ok(dirty) => {
                *last = Some(dirty);
                Ok(dirty)
            }
            Err(e) => {
                self.invalidate();
                Err(e)
            }
        }
    }
}

/// Exit watches on the processes a status panel shows as alive: each
/// agent's attended processes and the repo's monitor. A process ending
/// writes no file, so without these the panel would show it alive until
/// some unrelated event.
pub(crate) struct ProcessExits {
    watch: Option<ProcExitWatch>,
    armed: Vec<(i32, crate::proc_identity::ProcToken)>,
    wake: std::sync::Arc<dyn Fn() + Send + Sync>,
    fault: crate::fs_notice::Fault,
}

type ProcExitWatch =
    crate::proc_identity::exit_watch::ExitWatch<(i32, crate::proc_identity::ProcToken)>;

impl ProcessExits {
    /// `wake` is called when a watched process exits.
    pub(crate) fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        let wake: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(wake);
        let fault = crate::fs_notice::Fault::default();
        let watch = ProcExitWatch::new(None, {
            let (wake, fault) = (wake.clone(), fault.clone());
            move |w| {
                if let crate::proc_identity::exit_watch::Wake::Failed(why) = w {
                    crate::fs_notice::record(&fault, format!("the process watch failed: {why}"));
                }
                wake()
            }
        });
        if let Err(e) = &watch {
            crate::fs_notice::record(&fault, format!("cannot watch for process exits: {e}"));
        }
        ProcessExits {
            watch: watch.ok(),
            armed: Vec::new(),
            wake,
            fault,
        }
    }

    /// Why a shown process's exit can no longer be heard, once it can't:
    /// from then on the panel would show it alive after it ended.
    pub(crate) fn fault(&self) -> Option<String> {
        crate::fs_notice::read(&self.fault)
    }

    /// Watch exactly the processes `repo` shows as alive now. One found
    /// already gone wakes at once, and only the first time it is seen.
    pub(crate) fn track(&mut self, repo: &Path) {
        self.track_these(shown_processes(repo));
    }

    fn track_these(&mut self, shown: Vec<(i32, crate::proc_identity::ProcToken)>) {
        use crate::proc_identity::exit_watch::Armed;
        let Some(watch) = &self.watch else {
            return;
        };
        for gone in self.armed.iter().filter(|key| !shown.contains(key)) {
            watch.disarm(gone);
        }
        for key in shown.iter().filter(|key| !self.armed.contains(key)) {
            if watch.arm(key.clone(), key.0, &key.1) == Armed::Ended {
                (self.wake)();
            }
        }
        self.armed = shown;
    }
}

/// The processes the panel shows as alive, by pid and birth.
fn shown_processes(repo: &Path) -> Vec<(i32, crate::proc_identity::ProcToken)> {
    let agents = crate::agent_store::agents_root(repo);
    let mut shown: Vec<_> = std::fs::read_dir(&agents)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| crate::cli::agent_state::load(&entry.path()))
        .flat_map(|state| state.attending)
        .filter_map(|attended| attended.pid.zip(attended.token))
        .collect();
    if let crate::cli::monitor::Liveness::Running { pid, .. } = crate::cli::monitor::liveness(repo)
        && let Some(token) = crate::proc_identity::token_for(pid)
    {
        shown.push((pid, token));
    }
    shown
}

/// Hash of the sources account the monitor publishes, as the panel would
/// read it, including whether its monitor still runs, and of each
/// active timer's state, which changes when its evidence does.
fn sources_fingerprint(repo: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{:?}", crate::cli::source_status::live(repo)).hash(&mut h);
    for source in crate::cli::source_status::configured(repo).unwrap_or_default() {
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
        let Some(state) =
            crate::cli::agent_state::load(&crate::agent_store::agents_root(repo).join(&name))
        else {
            continue;
        };
        name.hash(&mut h);
        for attended in &state.attending {
            format!("{attended:?}").hash(&mut h);
            attended
                .pid
                .zip(attended.token.as_ref())
                .is_some_and(|(pid, token)| token.still_holds(pid))
                .hash(&mut h);
        }
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
    pub(crate) fn new(repo_root: &Path) -> anyhow::Result<Self> {
        // FSEvents delivers canonical paths (`/private/var/…`);
        // compare against canonical roots or every prefix check
        // misses on symlinked locations (e.g. macOS tempdirs).
        let repo_root = dunce::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
        Ok(Self {
            git_dir: crate::repo_watch::git_state_dir(&repo_root)?,
            common_dir: crate::repo_watch::git_common_state_dir(&repo_root)?,
            elsewhere: Known::default(),
            ignore: crate::git_io::open_ignore(&repo_root)?,
            stale_ignore: Default::default(),
            clank_root: repo_root.join(".clank"),
            repo_root,
        })
    }

    /// The git directories outside the worktree, each once: a linked
    /// worktree's own gitdir lies inside the shared one.
    fn git_dirs_outside(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        for dir in [&self.common_dir, &self.git_dir] {
            if !dir.starts_with(&self.repo_root) && !dirs.iter().any(|d| dir.starts_with(d)) {
                dirs.push(dir.clone());
            }
        }
        dirs
    }

    fn reopen_ignore(&mut self) {
        // On an error the old stack stays, which errs toward waking.
        if let Ok(ignore) = crate::git_io::open_ignore(&self.repo_root) {
            self.ignore = ignore;
        }
    }

    pub(crate) fn sees(&mut self, path: &Path) -> Seen {
        if self
            .stale_ignore
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            self.reopen_ignore();
        }
        // Native FSEvents paths are canonical, but notify's polling
        // backend can report `/var/…` for a root canonicalized to
        // `/private/var/…`. Keep the filter's comparison in one path
        // namespace before treating a path as outside the repo.
        let canonical;
        let path = if [&self.repo_root, &self.git_dir, &self.common_dir]
            .iter()
            .any(|root| path.starts_with(root))
        {
            path
        } else {
            canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            &canonical
        };
        if self.elsewhere.lock().iter().any(|file| file == path) {
            self.reopen_ignore();
            return Seen::GitInput;
        }
        // The shared dir holds every linked worktree's gitdir, so this
        // worktree's own is asked first.
        for dir in [self.git_dir.clone(), self.common_dir.clone()] {
            if let Ok(rel) = path.strip_prefix(&dir) {
                if !git_state_input(rel) {
                    return Seen::Nothing;
                }
                if rel.starts_with("info") {
                    self.reopen_ignore();
                }
                return Seen::GitInput;
            }
        }
        let Ok(rela) = path.strip_prefix(&self.repo_root) else {
            return Seen::GitInput;
        };
        if rela.as_os_str().is_empty() {
            return Seen::Nothing;
        }
        if path.file_name().is_some_and(|n| n == ".gitignore") {
            self.reopen_ignore();
            return Seen::GitInput;
        }
        if self.ignore.is_tracked(rela) {
            return Seen::GitInput;
        }
        // `is_dir` races with deletion; a vanished path reads as
        // non-dir, only loosening toward a wake.
        if !self.ignore.is_ignored(rela, path.is_dir()) {
            return Seen::GitInput;
        }
        let shown = path
            .strip_prefix(&self.clank_root)
            .ok()
            .and_then(|rel| rel.components().next())
            .and_then(|c| c.as_os_str().to_str())
            .is_some_and(|head| CLANK_STATUS_ONLY_DIRS.contains(&head));
        match shown {
            true => Seen::Shown,
            false => Seen::Nothing,
        }
    }
}

/// Whether a path inside a git directory is one a status walk reads:
/// HEAD and the refs it may point through (`git reset --soft` moves
/// only these), the index, and what decides what Git sees. Objects,
/// logs and lock files are not.
fn git_state_input(rel: &Path) -> bool {
    if rel.extension().is_some_and(|ext| ext == "lock") {
        return false;
    }
    match rel.components().next().and_then(|c| c.as_os_str().to_str()) {
        Some("HEAD" | "index" | "packed-refs" | "refs" | "config" | "config.worktree") => true,
        Some("info") => {
            rel == Path::new("info")
                || rel == Path::new("info/exclude")
                || rel == Path::new("info/sparse-checkout")
        }
        _ => false,
    }
}

/// The status loops' watchers: the core wakes (the monitor's, or a
/// watcher of our own) and the Git watcher ([`watch_git`]), on one
/// channel. The worktree is heard once `watches` is next committed.
pub(crate) fn watch_status_paths<M: Clone + Send + Sync + 'static>(
    tx: impl Into<crate::fs_notice::Wake<M>>,
    repo: &Path,
    git_status: &GitStatus,
    watches: &Watches,
) -> anyhow::Result<StatusWatchers> {
    let tx = tx.into();
    // Gate state comes from the repo's MONITOR when one is running —
    // one watcher and one fold for the whole repo, which is the point
    // of `a-repo-is-watched-once-not-once-per-waiter`: this panel used
    // to attach a watcher of its own alongside one per parked wait.
    // With no monitor it attaches that watcher itself, so the panel
    // behaves the same either way.
    let core = crate::cli::monitor::core_wakes(repo, tx.clone())?;
    let git = watch_git(tx, repo, git_status, watches)?;
    Ok(StatusWatchers { core, git })
}

/// The TUI's watchers: the keeper, which holds the repo's monitor
/// subscription and brings a monitor back when it goes, and the same
/// Git watcher as [`watch_status_paths`]. Plain `status --watch` stays
/// on [`watch_status_paths`], which never starts anything.
pub(crate) fn watch_tui_paths(
    tx: mpsc::Sender<()>,
    repo: &Path,
    git_status: &GitStatus,
    spawn: impl Fn(&Path) -> std::io::Result<Box<dyn crate::cli::monitor::Started>> + Send + 'static,
) -> anyhow::Result<TuiWatchers> {
    let keeper = crate::cli::monitor::keeper::MonitorKeeper::start(repo, tx.clone(), spawn);
    let watches = Watches::default();
    let git = watch_git(tx, repo, git_status, &watches)?;
    watches.commit()?;
    if let Some(why) = git.fault() {
        anyhow::bail!("{why}");
    }
    Ok(TuiWatchers { keeper, git })
}

/// What [`watch_tui_paths`] keeps alive. Dropping it stops the keeper
/// and the Git watcher.
pub(crate) struct TuiWatchers {
    pub(crate) keeper: crate::cli::monitor::keeper::MonitorKeeper,
    pub(crate) git: GitWatch,
}

/// The two watchers `status` keeps alive together. Dropping this stops
/// both. They send on one channel; the loop's nothing-changed gate
/// ([`InputSignature`]) coalesces.
pub(crate) struct StatusWatchers {
    /// The shared fold, or a watcher of our own — see
    /// [`crate::cli::monitor::core_wakes`].
    pub(crate) core: crate::cli::monitor::CoreWakes,
    /// Kept local on purpose: the monitor has no reason to compute a
    /// diff, and nothing but this panel wants one.
    pub(crate) git: GitWatch,
}

/// The status loops' Git watcher, and what ends them when it fails.
pub(crate) struct GitWatch {
    _tree: Route,
    _elsewhere: Follow,
    _watches: Watches,
    fault: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl GitWatch {
    /// Why the watcher stopped being trustworthy, once it has. A loop
    /// that reads this as `Some` must end, saying so: from then on it
    /// would sit on a stale status with nothing to correct it.
    pub(crate) fn fault(&self) -> Option<String> {
        self.fault
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Watch everything [`WakeFilter`] can call a [`Seen::GitInput`]: the
/// worktree and the git directories recursively, through the process's
/// one worktree stream, and the files elsewhere one by one, through its
/// one kernel watch of them, since a recursive watch of a home directory
/// would hear everything under it. Each Git input invalidates
/// `git_status` before it wakes `tx`.
fn watch_git<M: Clone + Send + Sync + 'static>(
    tx: impl Into<crate::fs_notice::Wake<M>>,
    repo: &Path,
    git_status: &GitStatus,
    watches: &Watches,
) -> anyhow::Result<GitWatch> {
    let tx = tx.into();
    let filter = WakeFilter::new(repo)?;
    let roots: Vec<PathBuf> = std::iter::once(filter.repo_root.clone())
        .chain(filter.git_dirs_outside())
        .collect();
    let known = filter.elsewhere.clone();
    let stale_ignore = filter.stale_ignore.clone();
    let fault = std::sync::Arc::new(std::sync::Mutex::new(None));
    let tree = watches.worktrees.route(
        roots,
        Box::new(git_wake_handler(
            tx.clone(),
            filter,
            git_status.clone(),
            fault.clone(),
        )),
    );
    let repo = repo.to_path_buf();
    let elsewhere = watches.elsewhere.follow(
        Box::new(move || crate::git_io::status_inputs_elsewhere(&repo).map_err(|e| e.to_string())),
        known,
        {
            let (git_status, tx) = (git_status.clone(), tx.clone());
            std::sync::Arc::new(move || {
                stale_ignore.store(true, std::sync::atomic::Ordering::Release);
                git_status.invalidate();
                tx.ring();
            })
        },
        {
            let fault = fault.clone();
            std::sync::Arc::new(move |why| {
                crate::fs_notice::record(&fault, why);
                tx.ring();
            })
        },
    )?;
    Ok(GitWatch {
        _tree: tree,
        _elsewhere: elsewhere,
        _watches: watches.clone(),
        fault,
    })
}

/// The watches every status loop in a process shares: one platform
/// stream over every worktree it watches, and one kernel watch of the
/// Git inputs outside them, which repos share (the global and XDG
/// config, an excludes file). Clones share them.
#[derive(Clone, Default)]
pub(crate) struct Watches {
    worktrees: Worktrees,
    elsewhere: ElsewhereHub,
}

impl Watches {
    /// Bring the worktree stream in line with the Git watchers alive now:
    /// one restart however many came or went since the last commit.
    /// Whether it restarted, in which case what changed in a worktree
    /// already watched while it was stopped went unheard.
    pub(crate) fn commit(&self) -> anyhow::Result<bool> {
        self.worktrees.commit()
    }

    /// How many platform streams were made.
    #[cfg(test)]
    pub(crate) fn streams(&self) -> usize {
        self.worktrees
            .0
            .streams
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// How many kernel watches of the files elsewhere were made.
    #[cfg(test)]
    pub(crate) fn elsewhere_watches(&self) -> usize {
        self.elsewhere.made()
    }
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

type TreeHandler = Box<dyn FnMut(notify::Result<notify::Event>) + Send>;

/// One platform stream over every worktree a process watches. FSEvents
/// costs about a second to start, and a stream per repo made a watch of
/// many repos wait that long for each.
#[derive(Clone, Default)]
struct Worktrees(std::sync::Arc<WorktreesInner>);

#[derive(Default)]
struct WorktreesInner {
    stream: std::sync::Mutex<Option<RecommendedWatcher>>,
    routes: std::sync::Arc<std::sync::Mutex<Routes>>,
    /// The stream reported a failure: the next commit makes it afresh.
    broken: std::sync::Arc<std::sync::atomic::AtomicBool>,
    streams: std::sync::atomic::AtomicUsize,
}

/// Which handler hears which part of the stream.
#[derive(Default)]
struct Routes {
    next: u64,
    list: Vec<RouteEntry>,
    /// The roots the stream holds.
    watched: Vec<PathBuf>,
    /// A route came or went since the stream was last committed.
    dirty: bool,
}

struct RouteEntry {
    id: u64,
    roots: Vec<PathBuf>,
    handler: TreeHandler,
}

/// A handler's share of the stream, until dropped.
struct Route {
    id: u64,
    routes: std::sync::Arc<std::sync::Mutex<Routes>>,
}

impl Drop for Route {
    fn drop(&mut self) {
        let mut routes = lock(&self.routes);
        routes.list.retain(|entry| entry.id != self.id);
        routes.dirty = true;
    }
}

impl Routes {
    /// Hand each path to every route whose roots hold it, and what
    /// concerns the whole stream to every route.
    fn dispatch(&mut self, res: notify::Result<notify::Event>) {
        let event = match res {
            Err(e) => {
                let why = e.to_string();
                for entry in &mut self.list {
                    (entry.handler)(Err(notify::Error::generic(&why)));
                }
                return;
            }
            Ok(event) if event.need_rescan() || event.paths.is_empty() => {
                for entry in &mut self.list {
                    (entry.handler)(Ok(event.clone()));
                }
                return;
            }
            Ok(event) => event,
        };
        let mut shares: Vec<Vec<PathBuf>> = vec![Vec::new(); self.list.len()];
        for path in &event.paths {
            for owner in self.owners(path) {
                shares[owner].push(path.clone());
            }
        }
        for (entry, paths) in self.list.iter_mut().zip(shares) {
            if !paths.is_empty() {
                let mut share = event.clone();
                share.paths = paths;
                (entry.handler)(Ok(share));
            }
        }
    }

    /// Every route with a root holding `path`. Routes subscribe to
    /// trees, not own them: a linked worktree watches the main
    /// checkout's git directory, which lies inside the main checkout's
    /// own root, and both depend on what changes there. Each route's
    /// filter decides whether a path is one of its repo's inputs.
    fn owners(&self, path: &Path) -> Vec<usize> {
        let holding = |path: &Path| -> Vec<usize> {
            (0..self.list.len())
                .filter(|&i| self.list[i].roots.iter().any(|root| path.starts_with(root)))
                .collect()
        };
        let owners = holding(path);
        match (owners.is_empty(), dunce::canonicalize(path)) {
            (true, Ok(canonical)) => holding(&canonical),
            _ => owners,
        }
    }
}

impl Worktrees {
    /// Route the events under `roots` to `handler` from the next commit.
    fn route(&self, roots: Vec<PathBuf>, handler: TreeHandler) -> Route {
        let mut routes = lock(&self.0.routes);
        routes.next += 1;
        let id = routes.next;
        routes.list.push(RouteEntry { id, roots, handler });
        routes.dirty = true;
        Route {
            id,
            routes: self.0.routes.clone(),
        }
    }

    fn commit(&self) -> anyhow::Result<bool> {
        use std::sync::atomic::Ordering;
        let inner = &*self.0;
        let mut stream = lock(&inner.stream);
        let rebuild = inner.broken.swap(false, Ordering::AcqRel);
        let (want, have) = {
            let mut routes = lock(&inner.routes);
            if !routes.dirty && !rebuild {
                return Ok(false);
            }
            routes.dirty = false;
            let mut want: Vec<PathBuf> = routes
                .list
                .iter()
                .flat_map(|entry| entry.roots.iter().cloned())
                .collect();
            want.sort();
            want.dedup();
            let have = match rebuild {
                true => Vec::new(),
                false => routes.watched.clone(),
            };
            (want, have)
        };
        if stream.is_none() && want.is_empty() {
            return Ok(false);
        }
        if rebuild || stream.is_none() {
            *stream = None;
            let (routes, broken) = (inner.routes.clone(), inner.broken.clone());
            // The routes' lock is never held across a commit, which stops
            // the stream and waits for the thread this runs on.
            *stream = Some(notify::recommended_watcher(
                move |res: notify::Result<notify::Event>| {
                    if res.is_err() {
                        broken.store(true, Ordering::Release);
                    }
                    lock(&routes).dispatch(res);
                },
            )?);
            inner.streams.fetch_add(1, Ordering::SeqCst);
        }
        let Some(watcher) = stream.as_mut() else {
            return Ok(false);
        };
        let mut failed = Vec::new();
        {
            let mut paths = watcher.paths_mut();
            for gone in have.iter().filter(|root| !want.contains(root)) {
                let _ = paths.remove(gone);
            }
            for root in want.iter().filter(|root| !have.contains(root)) {
                if let Err(e) = paths.add(root, RecursiveMode::Recursive) {
                    failed.push((root.clone(), e.to_string()));
                }
            }
            paths.commit()?;
        }
        let mut routes = lock(&inner.routes);
        routes.watched = want
            .into_iter()
            .filter(|root| !failed.iter().any(|(f, _)| f == root))
            .collect();
        for (root, why) in &failed {
            let why = format!("watch `{}` failed: {why}", root.display());
            for entry in routes.list.iter_mut().filter(|e| e.roots.contains(root)) {
                (entry.handler)(Err(notify::Error::generic(&why)));
            }
        }
        Ok(true)
    }
}

/// The files elsewhere that decide what Git sees, as last read.
#[derive(Clone, Default)]
pub(crate) struct Known(std::sync::Arc<std::sync::Mutex<Vec<PathBuf>>>);

impl Known {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<PathBuf>> {
        lock(&self.0)
    }
}

type ReadSet = Box<dyn Fn() -> Result<Vec<PathBuf>, String> + Send>;
type Changed = std::sync::Arc<dyn Fn() + Send + Sync>;
type Failed = std::sync::Arc<dyn Fn(String) + Send + Sync>;

/// The watch on the files elsewhere, through the kernel, one per process
/// however many repos read them. Each file is watched by the logical
/// path Git names, as an entry of its directory, so replacing it or
/// retargeting a link there is heard, and by the file it resolves to, for
/// its content and for being replaced. A change to any of them can name
/// others — a config sets `core.excludesFile` or includes another file —
/// so each repo that follows it reads its set again and the watch follows
/// that, a file not made yet included. A watch that failed is replaced
/// for the next repo to follow.
#[derive(Clone, Default)]
struct ElsewhereHub(std::sync::Arc<HubSlot>);

#[derive(Default)]
struct HubSlot {
    current: std::sync::Mutex<Option<std::sync::Arc<Followed>>>,
    made: std::sync::atomic::AtomicUsize,
}

/// One kernel watch and the thread that rereads for it.
struct Followed {
    shared: std::sync::Arc<FollowShared>,
    rereads: mpsc::Sender<Reread>,
    thread: Option<std::thread::JoinHandle<()>>,
    dead: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

struct FollowShared {
    state: std::sync::Mutex<FollowState>,
    knowns: std::sync::Arc<std::sync::Mutex<Vec<(u64, Known)>>>,
}

struct FollowState {
    registry: Registry,
    followers: Vec<Follower>,
    next: u64,
}

struct Follower {
    id: u64,
    read: ReadSet,
    known: Known,
    following: Following,
    changed: Changed,
    failed: Failed,
}

enum Reread {
    Of(Vec<u64>),
    All,
    Failed(String),
    Stop,
}

/// One repo's share of the watch on the files elsewhere, until dropped.
pub(crate) struct Follow {
    followed: std::sync::Arc<Followed>,
    id: u64,
}

impl ElsewhereHub {
    /// Follow the files `read` names for one repo: `changed` is called
    /// once the watch covers what a change named, and `failed` once the
    /// watch, or a read of the set, fails.
    fn follow(
        &self,
        read: ReadSet,
        known: Known,
        changed: Changed,
        failed: Failed,
    ) -> anyhow::Result<Follow> {
        let followed = {
            let mut current = lock(&self.0.current);
            match &*current {
                Some(followed) if !followed.dead.load(std::sync::atomic::Ordering::Acquire) => {
                    followed.clone()
                }
                _ => {
                    let followed = std::sync::Arc::new(Followed::start()?);
                    self.0
                        .made
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    *current = Some(followed.clone());
                    followed
                }
            }
        };
        followed.add(read, known, changed, failed)
    }

    #[cfg(test)]
    fn made(&self) -> usize {
        self.0.made.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Followed {
    fn start() -> anyhow::Result<Self> {
        let (rereads, asked) = mpsc::channel();
        let knowns: std::sync::Arc<std::sync::Mutex<Vec<(u64, Known)>>> = Default::default();
        let dead = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher = crate::proc_identity::tree_watch::KernelWatcher::new(
            {
                let knowns = knowns.clone();
                move |path| {
                    lock(&knowns)
                        .iter()
                        .any(|(_, known)| known.lock().iter().any(|file| file == path))
                }
            },
            {
                let (rereads, knowns, dead) = (rereads.clone(), knowns.clone(), dead.clone());
                move |res: notify::Result<notify::Event>| {
                    use crate::fs_notice::{Notice, notice};
                    let ask = match notice(&res) {
                        Notice::Read => return,
                        Notice::Rescan => Reread::All,
                        // A directory that appears may already hold one.
                        Notice::Changed(paths) => {
                            let ids: Vec<u64> = lock(&knowns)
                                .iter()
                                .filter(|(_, known)| {
                                    paths.iter().any(|path| {
                                        known.lock().iter().any(|file| file.starts_with(path))
                                    })
                                })
                                .map(|(id, _)| *id)
                                .collect();
                            if ids.is_empty() {
                                return;
                            }
                            Reread::Of(ids)
                        }
                        Notice::Failed(why) => {
                            dead.store(true, std::sync::atomic::Ordering::Release);
                            Reread::Failed(why)
                        }
                    };
                    let _ = rereads.send(ask);
                }
            },
        )?;
        let shared = std::sync::Arc::new(FollowShared {
            state: std::sync::Mutex::new(FollowState {
                registry: Registry {
                    watcher,
                    counts: Default::default(),
                },
                followers: Vec::new(),
                next: 0,
            }),
            knowns,
        });
        let thread = std::thread::Builder::new()
            .name("git-elsewhere".into())
            .spawn({
                let shared = shared.clone();
                move || reread_on_ask(&shared, &asked)
            })?;
        Ok(Followed {
            shared,
            rereads,
            thread: Some(thread),
            dead,
        })
    }

    fn add(
        self: &std::sync::Arc<Self>,
        read: ReadSet,
        known: Known,
        changed: Changed,
        failed: Failed,
    ) -> anyhow::Result<Follow> {
        let mut state = lock(&self.shared.state);
        state.next += 1;
        let id = state.next;
        lock(&self.shared.knowns).push((id, known.clone()));
        let mut follower = Follower {
            id,
            read,
            known,
            following: Following::default(),
            changed,
            failed,
        };
        let FollowState {
            registry,
            followers,
            ..
        } = &mut *state;
        match follower
            .following
            .settle(&follower.read, registry, &follower.known)
        {
            Ok((moved, relisted)) => {
                if moved {
                    (follower.changed)();
                }
                tell_relisted(followers, id, &relisted);
                followers.push(follower);
                Ok(Follow {
                    followed: self.clone(),
                    id,
                })
            }
            Err(e) => {
                follower.following.release(registry);
                lock(&self.shared.knowns).retain(|(other, _)| *other != id);
                Err(e)
            }
        }
    }
}

/// Read again the sets a change named, until asked to stop.
fn reread_on_ask(shared: &FollowShared, asked: &mpsc::Receiver<Reread>) {
    while let Ok(first) = asked.recv() {
        let mut ids = Vec::new();
        let mut all = false;
        let mut failure = None;
        for ask in std::iter::once(first).chain(asked.try_iter()) {
            match ask {
                Reread::Stop => return,
                Reread::All => all = true,
                Reread::Of(of) => ids.extend(of),
                Reread::Failed(why) => failure = Some(why),
            }
        }
        let mut state = lock(&shared.state);
        let FollowState {
            registry,
            followers,
            ..
        } = &mut *state;
        if let Some(why) = failure {
            for follower in followers.iter() {
                (follower.failed)(format!("the watch on Git's files elsewhere failed: {why}"));
            }
            continue;
        }
        let mut relisted = Vec::new();
        for follower in followers.iter_mut().filter(|f| all || ids.contains(&f.id)) {
            match follower
                .following
                .settle(&follower.read, registry, &follower.known)
            {
                Ok((_, dirs)) => {
                    (follower.changed)();
                    relisted.extend(dirs.into_iter().map(|dir| (follower.id, dir)));
                }
                Err(e) => (follower.failed)(format!("cannot follow Git's files elsewhere: {e:#}")),
            }
        }
        for (by, dir) in relisted {
            tell_relisted(followers, by, std::slice::from_ref(&dir));
        }
    }
}

/// A directory listed again for one repo could have missed a change for
/// the others that follow files in it: they read their sets again too.
fn tell_relisted(followers: &[Follower], by: u64, relisted: &[PathBuf]) {
    for follower in followers
        .iter()
        .filter(|f| f.id != by && relisted.iter().any(|dir| f.following.dirs.contains(dir)))
    {
        (follower.changed)();
    }
}

impl Drop for Follow {
    fn drop(&mut self) {
        let shared = &self.followed.shared;
        let mut state = lock(&shared.state);
        let FollowState {
            registry,
            followers,
            ..
        } = &mut *state;
        if let Some(at) = followers.iter().position(|f| f.id == self.id) {
            followers.remove(at).following.release(registry);
        }
        lock(&shared.knowns).retain(|(id, _)| *id != self.id);
    }
}

impl Drop for Followed {
    fn drop(&mut self) {
        let _ = self.rereads.send(Reread::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The directories the kernel watch holds, each once however many repos
/// follow files in it.
struct Registry {
    watcher: crate::proc_identity::tree_watch::KernelWatcher,
    counts: std::collections::BTreeMap<PathBuf, usize>,
}

impl Registry {
    fn acquire(&mut self, dir: &Path) -> anyhow::Result<()> {
        use notify::Watcher as _;
        if !self.counts.contains_key(dir) {
            self.watcher
                .watch(dir, RecursiveMode::NonRecursive)
                .map_err(|e| anyhow::anyhow!("watch `{}` failed: {e}", dir.display()))?;
        }
        *self.counts.entry(dir.to_path_buf()).or_default() += 1;
        Ok(())
    }

    fn release(&mut self, dir: &Path) {
        use notify::Watcher as _;
        if let Some(count) = self.counts.get_mut(dir) {
            *count -= 1;
            if *count == 0 {
                self.counts.remove(dir);
                let _ = self.watcher.unwatch(dir);
            }
        }
    }

    fn relist(&mut self, dir: &Path) -> anyhow::Result<()> {
        use notify::Watcher as _;
        let _ = self.watcher.unwatch(dir);
        self.watcher
            .watch(dir, RecursiveMode::NonRecursive)
            .map_err(|e| anyhow::anyhow!("watch `{}` failed: {e}", dir.display()))
    }
}

/// What one repo's set is registered for: the files as Git names them,
/// and the directories watched for them and for what they resolve to.
#[derive(Default)]
struct Following {
    files: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
}

impl Following {
    /// Register for the set `read` names, then read it again, until a
    /// read finds what is registered: a change made between a read and
    /// its registration names inputs no event would ever report. Whether
    /// a read after the first found the set moved, and the directories
    /// already watched that were listed again.
    fn settle(
        &mut self,
        read: &ReadSet,
        registry: &mut Registry,
        known: &Known,
    ) -> anyhow::Result<(bool, Vec<PathBuf>)> {
        let mut moved = false;
        let mut first = true;
        let mut relisted_all = Vec::new();
        loop {
            let mut files = read().map_err(|e| anyhow::anyhow!("{e}"))?;
            files.sort();
            files.dedup();
            let resolved: Vec<PathBuf> = files.iter().map(|f| canonical_file(f)).collect();
            let mut followed: Vec<PathBuf> = files.iter().chain(&resolved).cloned().collect();
            followed.sort();
            followed.dedup();
            let mut dirs: Vec<PathBuf> = followed
                .iter()
                .filter_map(|f| f.parent().map(Path::to_path_buf))
                .collect();
            dirs.sort();
            dirs.dedup();
            if files == self.files && dirs == self.dirs && followed == *known.lock() {
                return Ok((moved, relisted_all));
            }
            moved |= !first;
            first = false;
            let newly: Vec<PathBuf> = followed
                .iter()
                .filter(|f| !known.lock().contains(f))
                .cloned()
                .collect();
            *known.lock() = followed;
            // A directory already watched chose its files' content when it
            // was listed: it is listed again for a file newly followed in
            // it. What changed meanwhile is covered by the caller's
            // `changed` after the set settles.
            let relisted: Vec<PathBuf> = self
                .dirs
                .iter()
                .filter(|d| dirs.contains(d))
                .filter(|d| newly.iter().any(|f| f.parent() == Some(d.as_path())))
                .cloned()
                .collect();
            for gone in self.dirs.iter().filter(|d| !dirs.contains(d)) {
                registry.release(gone);
            }
            self.dirs.retain(|d| dirs.contains(d));
            for dir in &relisted {
                registry.relist(dir)?;
            }
            for dir in &dirs {
                if !self.dirs.contains(dir) {
                    registry.acquire(dir)?;
                    self.dirs.push(dir.clone());
                }
            }
            relisted_all.extend(relisted);
            self.files = files;
            self.dirs = dirs;
        }
    }

    fn release(&mut self, registry: &mut Registry) {
        for dir in self.dirs.drain(..) {
            registry.release(&dir);
        }
    }
}

/// Where `path` resolves to; through its directory when it does not
/// exist, as the directory's watch would name it.
fn canonical_file(path: &Path) -> PathBuf {
    dunce::canonicalize(path).ok().unwrap_or_else(|| {
        match (
            path.parent().and_then(|d| dunce::canonicalize(d).ok()),
            path.file_name(),
        ) {
            (Some(dir), Some(name)) => dir.join(name),
            _ => path.to_path_buf(),
        }
    })
}

/// The Git watcher's callback, apart from any backend so a test can
/// drive it with the events a backend would deliver.
fn git_wake_handler<M: Clone + Send + Sync + 'static>(
    tx: impl Into<crate::fs_notice::Wake<M>>,
    mut filter: WakeFilter,
    git_status: GitStatus,
    fault: std::sync::Arc<std::sync::Mutex<Option<String>>>,
) -> impl FnMut(notify::Result<notify::Event>) + Send + 'static {
    let tx = tx.into();
    move |res: notify::Result<notify::Event>| {
        use crate::fs_notice::{Notice, notice};
        let seen = match notice(&res) {
            Notice::Read => Seen::Nothing,
            Notice::Rescan => Seen::GitInput,
            Notice::Changed(paths) => paths
                .iter()
                .map(|p| filter.sees(p))
                .max()
                .unwrap_or(Seen::Nothing),
            Notice::Failed(why) => {
                crate::fs_notice::record(&fault, format!("the filesystem watcher failed: {why}"));
                tx.ring();
                return;
            }
        };
        if seen == Seen::GitInput {
            git_status.invalidate();
        }
        if seen != Seen::Nothing {
            tx.ring();
        }
    }
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
pub(crate) mod dirty_and_wake_tests {

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

    /// One commit restoring many finished records is ONE row, marked and
    /// counted, with no per-plan footer; the plan landed after it reads as
    /// it would without it, and reading the window a page at a time gives
    /// the rows the whole read gives.
    #[tokio::test]
    async fn a_commit_finishing_many_plans_is_one_row() {
        use crate::cli::log::{OnelineRow, RowMarker};
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
        let commit = |subject: &str| {
            git(&["add", "-A"]);
            git(&["commit", "-qm", subject]);
            git(&["rev-parse", "HEAD"])
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("README"), "base\n").unwrap();
        let adopted = crate::lifecycle::CommitSha::parse(&commit("base")).unwrap();
        std::fs::create_dir_all(repo.join(".clank/finished")).unwrap();
        let restored = ["sim-a", "sim-b", "sim-c", "sim-d", "simctl-up"];
        for name in restored {
            std::fs::write(
                repo.join(format!(".clank/finished/{name}.md")),
                format!("# {name}\n"),
            )
            .unwrap();
        }
        let bulk = commit("Restore the simulator line's clank plans");
        std::fs::write(repo.join("README"), "tweak\n").unwrap();
        let adhoc = commit("drive-by tweak");
        std::fs::create_dir_all(repo.join(".clank/plans")).unwrap();
        std::fs::write(repo.join(".clank/plans/later.md"), "# later\n").unwrap();
        let intro = commit("[later] intro");
        std::fs::write(repo.join("code.txt"), "work\n").unwrap();
        let work = commit("[later] build it");

        let state = crate::rebuild::rebuild_repo(repo).await.unwrap();
        let stands = stands_for_plans(&state);

        let whole = tui_log_with_events(repo, 30, Some(&adopted), &stands)
            .await
            .rows;
        let mut paged = tui_log_with_events(repo, 2, Some(&adopted), &stands).await;
        while let Some(next) = paged.next.take() {
            let older = tui_log_page(repo, &next, 2, Some(&adopted), &stands).await;
            crate::cli::log::extend_oneline_rows(&mut paged.rows, older.rows);
            paged.next = older.next;
        }
        let short = |sha: &str| sha[..7].to_string();
        let expected = vec![
            format!("⚒ {} build it", short(&work)),
            format!("✎ {} intro", short(&intro)),
            "later".to_string(),
            format!("~ {} drive-by tweak", short(&adhoc)),
            crate::cli::log::RUN_END_RULE.to_string(),
            format!(
                "⁂ {} Restore the simulator line's clank plans (5 plans)",
                short(&bulk)
            ),
        ];
        for rows in [&whole, &paged.rows] {
            assert_eq!(crate::cli::log::oneline_plain_lines(rows), expected);
            let bulk_rows: Vec<_> = rows
                .iter()
                .filter(|r| matches!(r, OnelineRow::Commit { sha, .. } if sha.as_str() == bulk))
                .collect();
            assert!(
                matches!(
                    bulk_rows.as_slice(),
                    [OnelineRow::Commit {
                        marker: RowMarker::Bulk,
                        stands_for: None,
                        ..
                    }]
                ),
                "{bulk_rows:?}"
            );
        }
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
    use std::time::Duration;

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

    /// A repo with every kind of path the filter tells apart: tracked
    /// files in and out of `.clank/`, a file force-added under an ignored
    /// directory, ignored runtime state under `.clank/`, and ignore
    /// rules both at the root and nested.
    fn classified_repo(root: &Path) {
        git(root, &["init", "--quiet", "-b", "main"]);
        git(root, &["config", "user.email", "t@t"]);
        git(root, &["config", "user.name", "t"]);
        for (path, body) in [
            (".gitignore", "/target/\n*.log\n/build/\n"),
            ("sub/.gitignore", "/build/\n.dart_tool/\n"),
            ("sub/build/macos/App", "x"),
            ("sub/.dart_tool/x", "x"),
            (
                ".clank/.gitignore",
                "agents/\ncache/\nworktrees/\ndrafts/\n",
            ),
            (".clank/plans/foo.md", "# foo\n"),
            (".clank/drafts/idea.md", "# idea\n"),
            ("src/lib.rs", "fn a() {}\n"),
            ("build/kept.bin", "kept\n"),
        ] {
            std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            std::fs::write(root.join(path), body).unwrap();
        }
        git(root, &["add", "-A"]);
        git(root, &["add", "-f", "build/kept.bin"]);
        git(root, &["commit", "--quiet", "-m", "base"]);
    }

    /// What each kind of path says, asked of the index and Git's own
    /// exclude stack: a tracked plan under `.clank/` and a force-added
    /// file under an ignored directory are Git inputs; ignored churn,
    /// nested ignores included, is nothing.
    #[test]
    fn wake_filter_matrix() {
        use Seen::{GitInput, Nothing, Shown};
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        classified_repo(&root);
        let mut f = WakeFilter::new(&root).unwrap();
        for (rel, want) in [
            (".clank/plans/foo.md", GitInput),
            (".clank/plans/new.md", GitInput),
            (".clank/.gitignore", GitInput),
            (".clank/agents/codex/feedback/abc.md", Nothing),
            (".clank/cache/repo-state/abc123.7.v10.bin", Nothing),
            (".clank/worktrees/wt1/src/lib.rs", Nothing),
            (".clank/drafts/idea.md", Shown),
            (".clank/drafts", Shown),
            ("build/kept.bin", GitInput),
            ("build/other.o", Nothing),
            ("target/debug/build/junk.o", Nothing),
            ("build.log", Nothing),
            ("src/lib.rs", GitInput),
            ("src/new.rs", GitInput),
            ("sub/build/macos/App", Nothing),
            ("sub/.dart_tool/x", Nothing),
            ("sub/main.rs", GitInput),
            (".gitignore", GitInput),
            ("sub/.gitignore", GitInput),
            (".git/HEAD", GitInput),
            (".git/index", GitInput),
            (".git/refs/heads/main", GitInput),
            (".git/packed-refs", GitInput),
            (".git/config", GitInput),
            (".git/info/exclude", GitInput),
            (".git/objects/ab/cdef", Nothing),
            (".git/index.lock", Nothing),
            (".git/logs/HEAD", Nothing),
            (".git/FETCH_HEAD", Nothing),
        ] {
            assert_eq!(f.sees(&root.join(rel)), want, "{rel}");
        }
    }

    /// The walks a refresh costs, as the watch loops make one: the probe,
    /// then the build when the probe moved.
    async fn refresh(r: &Path, status: &GitStatus) -> (usize, InputSignature) {
        let before = crate::git_io::DIRTY_WALKS.with(|c| c.get());
        let sig = input_signature(r, status).unwrap();
        StatusSnapshot::build_with(
            r,
            "repo",
            None,
            crate::rebuild::CachePolicy::Use,
            None,
            true,
            status,
        )
        .await
        .unwrap();
        (crate::git_io::DIRTY_WALKS.with(|c| c.get()) - before, sig)
    }

    fn event(r: &Path, paths: &[&str]) -> notify::Result<notify::Event> {
        Ok(paths
            .iter()
            .fold(notify::Event::new(notify::EventKind::Any), |event, path| {
                event.add_path(r.join(path))
            }))
    }

    /// Drives the Git watcher's own callback with what a backend reports
    /// for each change, then refreshes as the loops do. Every Git input
    /// costs exactly one walk, shared by the probe and the snapshot, and
    /// the status it shows is the tree's; nothing else costs a walk.
    #[tokio::test]
    async fn each_git_input_walks_once_and_nothing_else_walks() {
        let dir = tempfile::tempdir().unwrap();
        let r = dunce::canonicalize(dir.path()).unwrap();
        classified_repo(&r);
        let excludes = tempfile::tempdir().unwrap();
        let excludes = dunce::canonicalize(excludes.path()).unwrap().join("ignore");
        std::fs::write(&excludes, "*.tmp\n").unwrap();
        git(
            &r,
            &["config", "core.excludesFile", excludes.to_str().unwrap()],
        );
        let status = GitStatus::default();
        let (tx, rx) = mpsc::channel();
        let mut handle = git_wake_handler(
            tx,
            WakeFilter::new(&r).unwrap(),
            status.clone(),
            Default::default(),
        );
        assert_eq!(refresh(&r, &status).await.0, 1, "a new cache walks once");
        assert_eq!(refresh(&r, &status).await.0, 0, "and then not again");

        let write = |path: &str, body: &str| {
            let path = r.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        };
        type Change<'a> = (&'a str, Box<dyn Fn() + 'a>, notify::Result<notify::Event>);
        let walked: Vec<Change> = vec![
            (
                "a tracked edit",
                Box::new(|| write("src/lib.rs", "fn b() {}\n")),
                event(&r, &["src/lib.rs"]),
            ),
            (
                "a tracked plan under .clank",
                Box::new(|| write(".clank/plans/foo.md", "# foo\n\nmore\n")),
                event(&r, &[".clank/plans/foo.md"]),
            ),
            (
                "a force-added file under an ignored directory",
                Box::new(|| write("build/kept.bin", "changed\n")),
                event(&r, &["build/kept.bin"]),
            ),
            (
                "a new untracked file",
                Box::new(|| write("src/loose.rs", "\n")),
                event(&r, &["src/loose.rs"]),
            ),
            (
                "an untracked file moved into an ignored path",
                Box::new(|| {
                    std::fs::create_dir_all(r.join("target")).unwrap();
                    std::fs::rename(r.join("src/loose.rs"), r.join("target/loose.rs")).unwrap()
                }),
                event(&r, &["src/loose.rs", "target/loose.rs"]),
            ),
            (
                "an untracked file deleted",
                Box::new(|| {
                    write("src/gone.rs", "\n");
                    std::fs::remove_file(r.join("src/gone.rs")).unwrap()
                }),
                event(&r, &["src/gone.rs"]),
            ),
            (
                "staging",
                Box::new(|| {
                    write("src/staged.rs", "\n");
                    git(&r, &["add", "src/staged.rs"])
                }),
                event(&r, &[".git/index"]),
            ),
            (
                "a commit, then HEAD alone moved back over it",
                Box::new(|| {
                    git(&r, &["commit", "--quiet", "-am", "two"]);
                    git(&r, &["reset", "--quiet", "--soft", "HEAD~1"])
                }),
                event(&r, &[".git/refs/heads/main", ".git/logs/HEAD"]),
            ),
            (
                "a detached HEAD moved alone",
                Box::new(|| {
                    git(&r, &["commit", "--quiet", "-am", "three"]);
                    git(&r, &["checkout", "--quiet", "--detach"]);
                    git(&r, &["reset", "--quiet", "--soft", "HEAD~1"])
                }),
                event(&r, &[".git/HEAD"]),
            ),
            (
                "a .gitignore change",
                Box::new(|| write(".gitignore", "/target/\n*.log\n/build/\n/src/staged.rs\n")),
                event(&r, &[".gitignore"]),
            ),
            (
                "the global excludes file",
                Box::new(|| std::fs::write(&excludes, "*.tmp\n*.rs\n").unwrap()),
                Ok(notify::Event::new(notify::EventKind::Any).add_path(excludes.clone())),
            ),
            (
                "a must-rescan",
                Box::new(|| {}),
                Ok(notify::Event::new(notify::EventKind::Any)
                    .add_path(r.clone())
                    .set_flag(notify::event::Flag::Rescan)),
            ),
            (
                "a path-less event",
                Box::new(|| {}),
                Ok(notify::Event::new(notify::EventKind::Any)),
            ),
        ];
        for (what, change, reported) in walked {
            change();
            handle(reported);
            assert!(rx.try_recv().is_ok(), "{what} woke the loop");
            let (walks, sig) = refresh(&r, &status).await;
            assert_eq!(walks, 1, "{what}");
            assert_eq!(
                sig.dirty,
                crate::git_io::working_tree_dirty_at(&r).unwrap(),
                "{what}: the status shown is the tree's"
            );
        }

        let unwalked: Vec<Change> = vec![
            (
                "an ignored build output",
                Box::new(|| write("target/debug/out.o", "junk")),
                event(&r, &["target/debug/out.o"]),
            ),
            (
                "ignored runtime state under .clank",
                Box::new(|| write(".clank/agents/codex/feedback/abc.md", "CONTINUE\n")),
                event(&r, &[".clank/agents/codex/feedback/abc.md"]),
            ),
            (
                "the object store",
                Box::new(|| {}),
                event(&r, &[".git/objects/ab/cdef", ".git/index.lock"]),
            ),
        ];
        for (what, change, reported) in unwalked {
            change();
            handle(reported);
            assert!(rx.try_recv().is_err(), "{what} woke the loop");
            assert_eq!(refresh(&r, &status).await.0, 0, "{what}");
        }
        // A monitor's wake and an install's reach the loop by other
        // senders and never touch the cache: the refresh they cause
        // walks nothing.
        assert_eq!(refresh(&r, &status).await.0, 0, "a monitor or install wake");
    }

    /// A process the panel shows as alive wakes it when it exits, which
    /// no file write would; one found already gone wakes it once.
    #[test]
    fn an_exit_of_a_shown_process_wakes_the_panel() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let token = crate::proc_identity::token_for(pid).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut exits = ProcessExits::new(move || {
            let _ = tx.send(());
        });
        exits.track_these(vec![(pid, token.clone())]);
        exits.track_these(vec![(pid, token.clone())]);
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "alive"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok(), "its exit");
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "once");

        let (tx, rx) = mpsc::channel();
        let mut later = ProcessExits::new(move || {
            let _ = tx.send(());
        });
        later.track_these(vec![(pid, token.clone())]);
        later.track_these(vec![(pid, token)]);
        assert!(rx.try_recv().is_ok(), "already gone");
        assert!(rx.try_recv().is_err(), "and said once");
    }

    /// The status walk reads the index and HEAD, and on Linux each open
    /// is a notification: a read must neither wake the loop nor make the
    /// cache stale, or every walk would schedule the next.
    #[tokio::test]
    async fn reading_what_the_status_reads_is_not_a_change() {
        use crate::fs_notice::tests::opened;
        let dir = tempfile::tempdir().unwrap();
        let r = dunce::canonicalize(dir.path()).unwrap();
        classified_repo(&r);
        let status = GitStatus::default();
        let (tx, rx) = mpsc::channel();
        let mut handle = git_wake_handler(
            tx,
            WakeFilter::new(&r).unwrap(),
            status.clone(),
            Default::default(),
        );
        assert_eq!(refresh(&r, &status).await.0, 1);
        for read in [
            ".git/HEAD",
            ".git/index",
            "src/lib.rs",
            ".clank/plans/foo.md",
        ] {
            handle(opened(&r.join(read)));
        }
        assert!(rx.try_recv().is_err(), "a read woke the loop");
        assert_eq!(refresh(&r, &status).await.0, 0, "and made the cache stale");
    }

    /// A process watch whose kernel wait fails wakes the panel and says
    /// why, so the loop ends rather than show exited processes as alive.
    #[test]
    fn a_failed_process_watch_is_the_panels_fault() {
        let (tx, rx) = mpsc::channel();
        let exits = ProcessExits::new(move || {
            let _ = tx.send(());
        });
        assert_eq!(exits.fault(), None);
        exits.watch.as_ref().unwrap().fail_for_test();
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());
        assert!(exits.fault().is_some_and(|why| why.contains("failed")));
    }

    /// A watcher that fails ends the loop rather than leaving it on a
    /// status nothing will correct: the fault is kept for the loop to
    /// read, and the loop is woken to read it.
    #[test]
    fn a_failing_watcher_is_a_fault_the_loop_is_woken_for() {
        let dir = tempfile::tempdir().unwrap();
        let r = dunce::canonicalize(dir.path()).unwrap();
        classified_repo(&r);
        let fault = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (tx, rx) = mpsc::channel();
        let mut handle = git_wake_handler(
            tx,
            WakeFilter::new(&r).unwrap(),
            GitStatus::default(),
            fault.clone(),
        );
        handle(Err(notify::Error::generic("the stream died")));
        assert!(rx.try_recv().is_ok());
        let said = fault.lock().unwrap().clone().expect("a fault");
        assert!(said.contains("the stream died"), "{said}");
    }

    /// The files elsewhere, watched as `watch_git` watches them, with
    /// each change counted.
    fn elsewhere_of(repo: &Path) -> (Follow, Known, mpsc::Receiver<()>) {
        elsewhere_in(&ElsewhereHub::default(), repo)
    }

    /// [`elsewhere_of`], on the watch `hub` holds.
    fn elsewhere_in(hub: &ElsewhereHub, repo: &Path) -> (Follow, Known, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel();
        let known = Known::default();
        let repo = repo.to_path_buf();
        let watch = hub
            .follow(
                Box::new(move || {
                    crate::git_io::status_inputs_elsewhere(&repo).map_err(|e| e.to_string())
                }),
                known.clone(),
                std::sync::Arc::new(move || {
                    let _ = tx.send(());
                }),
                std::sync::Arc::new(|why| panic!("{why}")),
            )
            .unwrap();
        (watch, known, rx)
    }

    fn heard(rx: &mpsc::Receiver<()>) -> bool {
        rx.recv_timeout(Duration::from_secs(5)).is_ok()
    }

    fn append(path: &Path, text: &str) {
        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }

    /// [`elsewhere_of`] with the set read by `read`, and every change
    /// logged in `log` beside whatever `read` logs.
    fn elsewhere_reading(
        read: impl Fn() -> Result<Vec<PathBuf>, String> + Send + 'static,
        log: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
    ) -> (Follow, Known, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel();
        let known = Known::default();
        let watch = ElsewhereHub::default()
            .follow(
                Box::new(read),
                known.clone(),
                std::sync::Arc::new(move || {
                    log.lock().unwrap().push("changed");
                    let _ = tx.send(());
                }),
                std::sync::Arc::new(|why| panic!("{why}")),
            )
            .unwrap();
        (watch, known, rx)
    }

    /// The set changes between the first read and its registration: the
    /// file the config names by the time the watch is in place is the
    /// one followed, and a later write to it is heard.
    #[test]
    fn a_dependency_selected_while_the_watch_is_set_up_is_followed() {
        let repo = fixture_repo();
        let ext = tempfile::tempdir().unwrap();
        let dir = dunce::canonicalize(ext.path()).unwrap();
        let (a, b) = (dir.join("a.ignore"), dir.join("b.ignore"));
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();
        git(
            repo.path(),
            &["config", "core.excludesFile", a.to_str().unwrap()],
        );
        let first = std::sync::atomic::AtomicBool::new(true);
        let (path, b_name) = (repo.path().to_path_buf(), b.clone());
        let (_watch, _known, rx) = elsewhere_reading(
            move || {
                let set = crate::git_io::status_inputs_elsewhere(&path).map_err(|e| e.to_string());
                if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    git(
                        &path,
                        &["config", "core.excludesFile", b_name.to_str().unwrap()],
                    );
                }
                set
            },
            Default::default(),
        );
        settle(&rx);
        append(&b, "*.log\n");
        assert!(heard(&rx), "the file selected in the setup gap");
    }

    /// A file newly selected and written while the watch is still taking
    /// it up: the change is said only after the watch covers it, so an
    /// owner that reads on that word sees the write.
    #[test]
    fn a_write_during_reconfiguration_is_said_after_it() {
        let repo = fixture_repo();
        let ext = tempfile::tempdir().unwrap();
        let b = dunce::canonicalize(ext.path()).unwrap().join("b.ignore");
        std::fs::write(&b, "").unwrap();
        let log: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>> = Default::default();
        let (path, b_read, read_log) = (repo.path().to_path_buf(), b.clone(), log.clone());
        let (_watch, _known, rx) = elsewhere_reading(
            move || {
                let set =
                    crate::git_io::status_inputs_elsewhere(&path).map_err(|e| e.to_string())?;
                if set.contains(&b_read) && !read_log.lock().unwrap().contains(&"written") {
                    append(&b_read, "*.tmp\n");
                    read_log.lock().unwrap().push("written");
                }
                Ok(set)
            },
            log.clone(),
        );
        settle(&rx);
        log.lock().unwrap().clear();
        git(
            repo.path(),
            &["config", "core.excludesFile", b.to_str().unwrap()],
        );
        assert!(heard(&rx));
        settle(&rx);
        let log = log.lock().unwrap().clone();
        let written = log.iter().position(|e| *e == "written").expect("written");
        assert!(
            log[written..].contains(&"changed"),
            "the change was said before the write it covers: {log:?}"
        );
    }

    /// An included config that holds nothing yet is still one Git
    /// consults: its first write is heard, and an unrelated entry beside
    /// it is not.
    #[test]
    fn the_first_write_to_an_empty_included_config_is_heard() {
        let repo = fixture_repo();
        let ext = tempfile::tempdir().unwrap();
        let empty = dunce::canonicalize(ext.path())
            .unwrap()
            .join("initially-empty.gitconfig");
        std::fs::write(&empty, "").unwrap();
        git(
            repo.path(),
            &["config", "include.path", empty.to_str().unwrap()],
        );
        let (_watch, known, rx) = elsewhere_of(repo.path());
        assert!(known.lock().contains(&empty), "{:?}", known.lock());
        settle(&rx);
        std::fs::write(ext.path().join("unrelated"), "x").unwrap();
        assert!(
            rx.recv_timeout(Duration::from_millis(700)).is_err(),
            "an unrelated entry"
        );
        append(&empty, "[core]\n\tautocrlf = false\n");
        assert!(heard(&rx), "its first write");
    }

    /// An included config that does not exist yet, named relative to the
    /// file that includes it: its creation is heard.
    #[test]
    fn creating_a_missing_included_config_is_heard() {
        let repo = fixture_repo();
        let ext = tempfile::tempdir().unwrap();
        let dir = dunce::canonicalize(ext.path()).unwrap();
        let outer = dir.join("outer.gitconfig");
        std::fs::write(&outer, "[include]\n\tpath = later.gitconfig\n").unwrap();
        git(
            repo.path(),
            &["config", "include.path", outer.to_str().unwrap()],
        );
        let later = dir.join("later.gitconfig");
        let (_watch, known, rx) = elsewhere_of(repo.path());
        assert!(known.lock().contains(&later), "{:?}", known.lock());
        settle(&rx);
        std::fs::write(dir.join("unrelated"), "x").unwrap();
        assert!(
            rx.recv_timeout(Duration::from_millis(700)).is_err(),
            "an unrelated entry"
        );
        std::fs::write(&later, "[core]\n").unwrap();
        assert!(heard(&rx), "its creation");
    }

    /// Git's section and key names are case-insensitive: a mixed-case
    /// include, and a mixed-case conditional one, are followed to an
    /// empty file and a missing one, each heard when written.
    #[test]
    fn mixed_case_includes_are_followed() {
        let repo = fixture_repo();
        let ext = tempfile::tempdir().unwrap();
        let dir = dunce::canonicalize(ext.path()).unwrap();
        let (empty, missing) = (dir.join("empty.gitconfig"), dir.join("missing.gitconfig"));
        std::fs::write(&empty, "").unwrap();
        append(
            &repo.path().join(".git/config"),
            &format!(
                "[InClUdE]\n\tPaTh = {}\n[INCLUDEIF \"gitdir:/nowhere/\"]\n\tPATH = {}\n",
                empty.display(),
                missing.display()
            ),
        );
        let (_watch, known, rx) = elsewhere_of(repo.path());
        for file in [&empty, &missing] {
            assert!(
                known.lock().contains(file),
                "{file:?} in {:?}",
                known.lock()
            );
        }
        settle(&rx);
        append(&empty, "[core]\n");
        assert!(heard(&rx), "the empty one written");
        settle(&rx);
        std::fs::write(&missing, "[core]\n").unwrap();
        assert!(heard(&rx), "the missing one made");
    }

    /// A config reached through a link: retargeting the link is heard,
    /// and so are writes to the new target and its replacement, with
    /// no restart.
    #[test]
    fn a_linked_config_is_followed_across_a_retarget() {
        let repo = fixture_repo();
        let ext = tempfile::tempdir().unwrap();
        let dir = dunce::canonicalize(ext.path()).unwrap();
        let (a, b, link) = (
            dir.join("a.gitconfig"),
            dir.join("b.gitconfig"),
            dir.join("link"),
        );
        std::fs::write(&a, "[core]\n").unwrap();
        std::fs::write(&b, "[core]\n").unwrap();
        std::os::unix::fs::symlink(&a, &link).unwrap();
        git(
            repo.path(),
            &["config", "include.path", link.to_str().unwrap()],
        );
        let (_watch, known, rx) = elsewhere_of(repo.path());
        assert!(known.lock().contains(&link), "{:?}", known.lock());
        settle(&rx);

        let fresh = dir.join("link.new");
        std::os::unix::fs::symlink(&b, &fresh).unwrap();
        std::fs::rename(&fresh, &link).unwrap();
        assert!(heard(&rx), "the retarget");
        settle(&rx);
        append(&b, "\tautocrlf = false\n");
        assert!(heard(&rx), "a write to the new target");
        settle(&rx);
        let replacement = dir.join("b.new");
        std::fs::write(&replacement, "[core]\n\tbare = false\n").unwrap();
        std::fs::rename(&replacement, &b).unwrap();
        assert!(heard(&rx), "the new target replaced");
    }

    /// A config file elsewhere edited in place, as an editor that writes
    /// through the file does, is heard; an unrelated entry beside it is
    /// not, and neither is reading it.
    #[test]
    fn an_external_config_edited_in_place_is_heard() {
        let repo = fixture_repo();
        let ext = tempfile::tempdir().unwrap();
        let extra = dunce::canonicalize(ext.path())
            .unwrap()
            .join("extra.gitconfig");
        std::fs::write(&extra, "[core]\n").unwrap();
        git(
            repo.path(),
            &["config", "include.path", extra.to_str().unwrap()],
        );
        let (_watch, known, rx) = elsewhere_of(repo.path());
        assert!(known.lock().contains(&extra), "{:?}", known.lock());
        settle(&rx);

        std::fs::write(ext.path().join("unrelated"), "x").unwrap();
        let _ = std::fs::read(&extra).unwrap();
        assert!(
            rx.recv_timeout(Duration::from_millis(700)).is_err(),
            "an unrelated entry, or a read"
        );
        append(&extra, "\tautocrlf = false\n");
        assert!(heard(&rx), "the in-place edit");
    }

    /// Every repo's Git watch goes through one worktree stream and one
    /// kernel watch of the files elsewhere: adding several repos, or
    /// dropping one, restarts that one stream once, and never makes
    /// another.
    #[test]
    fn one_stream_and_one_elsewhere_watch_serve_every_repo() {
        let repos = [fixture_repo(), fixture_repo(), fixture_repo()];
        let watches = Watches::default();
        let (tx, _rx) = mpsc::channel::<()>();
        let mut held: Vec<StatusWatchers> = repos
            .iter()
            .map(|repo| {
                watch_status_paths(tx.clone(), repo.path(), &GitStatus::default(), &watches)
                    .unwrap()
            })
            .collect();
        assert!(watches.commit().unwrap(), "the stream started");
        assert!(!watches.commit().unwrap(), "nothing came or went");
        held.pop();
        assert!(watches.commit().unwrap(), "a repo went");
        assert_eq!(watches.streams(), 1);
        assert_eq!(watches.elsewhere_watches(), 1);
        assert!(held.iter().all(|w| w.git.fault().is_none()));
    }

    /// A main checkout and its linked worktree, routed as `watch_git`
    /// routes them: the main checkout's root holds its `.git`, and the
    /// linked worktree watches that `.git` as well. A change in the main
    /// index, a shared ref, or one worktree's files reaches every repo
    /// that depends on it, invalidates exactly those whose filters call
    /// it an input, and shows in their status.
    #[tokio::test]
    async fn a_main_checkout_and_its_linked_worktree_each_hear_their_inputs() {
        let dir = fixture_repo();
        let main = dunce::canonicalize(dir.path()).unwrap();
        let ext = tempfile::tempdir().unwrap();
        let linked = dunce::canonicalize(ext.path()).unwrap().join("linked");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                linked.to_str().unwrap(),
            ],
        );
        let worktrees = Worktrees::default();
        let mut wakes = Vec::new();
        let mut routes = Vec::new();
        let statuses = [GitStatus::default(), GitStatus::default()];
        for (repo, status) in [&main, &linked].into_iter().zip(&statuses) {
            let filter = WakeFilter::new(repo).unwrap();
            let roots: Vec<PathBuf> = std::iter::once(filter.repo_root.clone())
                .chain(filter.git_dirs_outside())
                .collect();
            let (tx, rx) = mpsc::channel();
            wakes.push(rx);
            routes.push(worktrees.route(
                roots,
                Box::new(git_wake_handler(
                    tx,
                    filter,
                    status.clone(),
                    Default::default(),
                )),
            ));
        }
        assert_eq!(
            routes.len(),
            2,
            "the linked worktree also watches {}",
            main.join(".git").display()
        );
        let mut seen = Vec::new();
        for (repo, status) in [&main, &linked].into_iter().zip(&statuses) {
            // Twice: the first build makes the repo's untracked `.clank/`,
            // which the first walk did not see.
            refresh(repo, status).await;
            status.invalidate();
            seen.push(refresh(repo, status).await.1);
        }
        // What each change walks, and whose status it moves.
        let mut heard = async |paths: Vec<PathBuf>| {
            let mut event = notify::Event::new(notify::EventKind::Any);
            event.paths = paths;
            lock(&worktrees.0.routes).dispatch(Ok(event));
            let mut out = Vec::new();
            for (i, (repo, status)) in [&main, &linked].into_iter().zip(&statuses).enumerate() {
                let woken = wakes[i].try_recv().is_ok();
                while wakes[i].try_recv().is_ok() {}
                let (walks, sig) = refresh(repo, status).await;
                out.push((
                    woken,
                    walks,
                    sig.dirty != seen[i].dirty || sig.head != seen[i].head,
                ));
                seen[i] = sig;
            }
            out
        };

        std::fs::write(main.join("staged.txt"), "new\n").unwrap();
        git(&main, &["add", "staged.txt"]);
        assert_eq!(
            heard(vec![main.join(".git/index")]).await,
            [(true, 1, true), (true, 1, false)],
            "the main index: the main checkout walks and shows the staged file; \
             the linked worktree's filter calls the shared directory's index an input"
        );

        git(&main, &["commit", "-q", "-m", "staged"]);
        assert_eq!(
            heard(vec![main.join(".git/refs/heads/main")]).await,
            [(true, 1, true), (true, 1, false)],
            "a shared ref: both depend on it, and the main checkout's HEAD moved"
        );

        std::fs::write(linked.join("a.txt"), "edited\n").unwrap();
        assert_eq!(
            heard(vec![linked.join("a.txt")]).await,
            [(false, 0, false), (true, 1, true)],
            "a file of the linked worktree alone"
        );

        std::fs::write(main.join("a.txt"), "edited\n").unwrap();
        assert_eq!(
            heard(vec![main.join("a.txt")]).await,
            [(true, 1, true), (false, 0, false)],
            "a file of the main checkout alone"
        );
    }

    /// One file elsewhere that two repos read, as one watch of many repos
    /// holds them: it is watched once, and an edit to it is heard for
    /// each.
    #[test]
    fn a_shared_external_input_is_heard_for_every_repo_that_reads_it() {
        let (one, two) = (fixture_repo(), fixture_repo());
        let ext = tempfile::tempdir().unwrap();
        let shared = dunce::canonicalize(ext.path())
            .unwrap()
            .join("shared.ignore");
        std::fs::write(&shared, "").unwrap();
        for repo in [&one, &two] {
            git(
                repo.path(),
                &["config", "core.excludesFile", shared.to_str().unwrap()],
            );
        }
        let hub = ElsewhereHub::default();
        let (_one_watch, _, one_rx) = elsewhere_in(&hub, one.path());
        let (_two_watch, _, two_rx) = elsewhere_in(&hub, two.path());
        assert_eq!(hub.made(), 1, "one kernel watch for both");
        settle(&one_rx);
        settle(&two_rx);
        append(&shared, "*.log\n");
        assert!(heard(&one_rx), "the first repo");
        assert!(heard(&two_rx), "the second repo");
    }

    /// `core.excludesFile` changed to a file elsewhere that does not exist
    /// yet: the change of config is heard, the new file is followed from
    /// then on, its creation and an in-place edit are heard, and nothing
    /// is restarted.
    #[test]
    fn a_newly_selected_external_excludes_file_is_followed() {
        let repo = fixture_repo();
        let ext = tempfile::tempdir().unwrap();
        let ignore = dunce::canonicalize(ext.path()).unwrap().join("git/ignore");
        let (_watch, known, rx) = elsewhere_of(repo.path());
        settle(&rx);

        git(
            repo.path(),
            &["config", "core.excludesFile", ignore.to_str().unwrap()],
        );
        assert!(heard(&rx), "the config naming it");
        let by = std::time::Instant::now() + Duration::from_secs(5);
        while !known.lock().contains(&ignore) {
            assert!(
                std::time::Instant::now() < by,
                "never followed: {:?}",
                known.lock()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        settle(&rx);
        // Its directory appearing is a look, since it may already hold
        // the file; an unrelated entry in it is not.
        std::fs::create_dir_all(ignore.parent().unwrap()).unwrap();
        settle(&rx);
        std::fs::write(ext.path().join("git/unrelated"), "x").unwrap();
        assert!(
            rx.recv_timeout(Duration::from_millis(700)).is_err(),
            "an unrelated entry"
        );
        std::fs::write(&ignore, "*.log\n").unwrap();
        assert!(heard(&rx), "its creation");
        settle(&rx);
        append(&ignore, "*.tmp\n");
        assert!(heard(&rx), "its in-place edit");
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

        let items = crate::cli::log::interleave_with_plain(&[], &history.plain, &[], &[], true);
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
        let items = crate::cli::log::interleave_with_plain(&folded, &history.plain, &[], &[], true);
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

    /// The pane's message rows as (state, subject), each read through
    /// the event its row opens.
    fn message_rows(
        rows: &[crate::cli::log::OnelineRow],
        events: &[crate::cli::github::timeline::MergedEvent],
    ) -> Vec<(crate::cli::github::timeline::DeliveryState, String)> {
        rows.iter()
            .filter_map(|row| match row {
                crate::cli::log::OnelineRow::Github {
                    event_idx,
                    delivery,
                    ..
                } => Some((*delivery, events[*event_idx].body.clone().unwrap())),
                _ => None,
            })
            .collect()
    }

    fn unborn_messages() -> Vec<(crate::cli::github::timeline::DeliveryState, String)> {
        crate::cli::log::unborn_tests::MESSAGES
            .iter()
            .rev()
            .map(|(subject, state)| (*state, subject.to_string()))
            .collect()
    }

    /// The snapshot's first read, its refresh and the web page's ledger
    /// all show an unborn branch's messages; the first commit then
    /// joins them through the same read.
    #[tokio::test]
    async fn an_unborn_branch_shows_its_messages_in_the_pane_and_on_the_page() {
        use crate::cli::log::unborn_tests::{git, unborn_repo_with_messages};
        let dir = unborn_repo_with_messages();
        let repo = dir.path();

        let snap = StatusSnapshot::build_async(
            repo,
            "repo",
            None,
            crate::rebuild::CachePolicy::Bypass,
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            message_rows(&snap.log_rows, &snap.github_events),
            unborn_messages()
        );
        let ledger = crate::cli::status_tui::web_facts(&snap, &Default::default())
            .ledger
            .rows;
        for (state, subject) in unborn_messages() {
            assert!(
                ledger.iter().any(|r| r.text.contains(&subject)
                    && r.glyph.as_deref() == Some(format!("{} ✉", state.glyph()).as_str())),
                "{subject}: {ledger:?}"
            );
        }

        let refreshed = tui_log_with_events(repo, 30, None, &Default::default()).await;
        assert_eq!(
            message_rows(&refreshed.rows, &refreshed.github_events),
            unborn_messages()
        );
        assert!(refreshed.next.is_none());

        git(repo, &["commit", "--quiet", "--allow-empty", "-m", "first"]);
        let first = tui_log_with_events(repo, 30, None, &Default::default()).await;
        assert_eq!(
            message_rows(&first.rows, &first.github_events),
            unborn_messages()
        );
        let commits: Vec<&str> = first
            .rows
            .iter()
            .filter_map(|row| match row {
                crate::cli::log::OnelineRow::PlainCommit { subject, .. } => Some(subject.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(commits, ["first"]);
        assert!(
            matches!(
                first.rows.first(),
                Some(crate::cli::log::OnelineRow::PlainCommit { .. })
            ),
            "the commit is newer than every message: {:?}",
            first.rows
        );
    }

    #[tokio::test]
    async fn an_unborn_repo_with_no_events_has_an_empty_log() {
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
        let _ = input_signature(r, &GitStatus::default()).unwrap();
        assert_eq!(
            crate::git_io::OPEN_COUNT.with(|c| c.get()),
            1,
            "the reuse probe opens the ODB once"
        );
    }

    /// The monitor dying changes no repo input, so the signature must
    /// carry it or the gate discards the rebuild that would show it.
    #[test]
    fn input_signature_moves_when_only_the_monitor_does() {
        use crate::cli::monitor::{Liveness, describe, liveness, publish, runtime_path};
        let dir = fixture_repo();
        let r = dir.path();
        let socket = Path::new("/tmp/x.sock");
        let me = std::process::id() as i32;
        publish(r, &describe(r, me, socket)).unwrap();
        let running = input_signature(r, &GitStatus::default()).unwrap();
        assert_eq!(process_health(r).monitor, liveness(r));
        assert!(matches!(
            process_health(r).monitor,
            Liveness::Running { .. }
        ));

        publish(r, &describe(r, i32::MAX, socket)).unwrap();
        let gone = input_signature(r, &GitStatus::default()).unwrap();
        assert_eq!(process_health(r).monitor, Liveness::Gone { pid: i32::MAX });
        assert_ne!(running, gone);
        let _ = std::fs::remove_file(runtime_path(r, "json"));
        assert_ne!(
            gone,
            input_signature(r, &GitStatus::default()).unwrap(),
            "and so does its absence"
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
            input_signature(r, &GitStatus::default()).unwrap(),
            input_signature(r, &GitStatus::default()).unwrap(),
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
        let signature = input_signature(r, &GitStatus::default()).unwrap();
        crate::cli::queue::unqueue(r, "idea").unwrap();
        assert_ne!(
            signature,
            input_signature(r, &GitStatus::default()).unwrap()
        );
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
    /// The polling backend's cadence, in the one test that drives the Git
    /// watcher's callback through it.
    const POLL: Duration = Duration::from_millis(500);

    fn settle(rx: &mpsc::Receiver<()>) {
        const QUIET: Duration = POLL.saturating_add(Duration::from_millis(200));
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
            git_wake_handler(
                tx,
                WakeFilter::new(r).unwrap(),
                GitStatus::default(),
                Default::default(),
            ),
            notify::Config::default().with_poll_interval(POLL),
        )
        .unwrap();
        poll_watcher.watch(r, RecursiveMode::Recursive).unwrap();

        // Root-ignored path: no wake. Give the poller a full cycle
        // plus slack after each write, so a broken filter cannot pass
        // merely because its event arrived after the assertion.
        const NO_WAKE: Duration = POLL.saturating_add(Duration::from_millis(200));
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
            attending: attending.into_iter().collect(),
            unreadable: None,
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
            declared: false,
            check_in: None,
            unqualified: None,
            rests_on: None,
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

    /// A published monitor whose process exited reads as gone, not as
    /// a build; and nothing published is said, not omitted.
    #[test]
    fn the_monitor_line_says_when_there_is_none_to_wake_anyone() {
        use crate::cli::monitor::Liveness;
        let mut snap = minimal_snapshot();
        snap.monitor = Liveness::Gone { pid: 11481 };
        assert!(
            snap.to_human()
                .contains("\nmonitor: gone (pid 11481 exited)\n"),
            "{}",
            snap.to_human()
        );
        snap.monitor = Liveness::None;
        assert!(
            snap.to_human().contains("\nmonitor: none\n"),
            "{}",
            snap.to_human()
        );
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
                    reachability: Default::default(),
                    owed: None,
                    nudge: crate::cli::agent_state::Nudge::Nothing,
                    cannot_resume: None,
                    attending: Vec::new(),
                    asking: Some("Life monitor".into()),
                    repeated: Some(count),
                },
                &crate::upgrade::process().permit().unwrap(),
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
            let row = roster_auto_rows(repo.path(), None, &set, &mut Vec::new()).remove(0);
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

    /// The invisible attendance of 10 Nov: a state a newer build wrote,
    /// in a shape this one cannot read. Its row has no attendance to draw,
    /// and the status says why: the file is another build's, not absent.
    #[test]
    fn a_state_from_another_build_is_said_to_be_one() {
        let repo = fixture_repo();
        let label = clank_core::ids::AgentLabel::parse("claude").unwrap();
        let dir = crate::agent_store::agents_root(repo.path()).join("claude");
        std::fs::create_dir_all(&dir).unwrap();
        let set = crate::cli::teams_config::RegisteredSet {
            master: label,
            master_desc: crate::cli::teams_config::AgentDescription {
                tool: clank_core::Tool::Claude,
                launch: None,
                initial_prompt: None,
            },
            members: Vec::new(),
        };
        let mut errors = Vec::new();
        roster_auto_rows(repo.path(), None, &set, &mut errors);
        assert_eq!(errors, Vec::<String>::new(), "no state is no error");

        std::fs::write(
            dir.join("state.json"),
            r#"{"role":"master","session":"s","attending":[{"attended":{"by":"a newer shape"}}]}"#,
        )
        .unwrap();
        let rows = roster_auto_rows(repo.path(), None, &set, &mut errors);
        assert!(rows[0].attending.is_empty());
        let [said] = errors.as_slice() else {
            panic!("{errors:?}");
        };
        assert!(
            said.contains("state.json was written by a clank build this one cannot read")
                && said.contains("not absent"),
            "{said}"
        );
        let mut snap = minimal_snapshot();
        snap.config_errors = errors;
        assert!(
            snap.to_human().contains("cannot read"),
            "{}",
            snap.to_human()
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
            owed: None,
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
                reachability: Default::default(),
                owed: None,
                nudge: Nudge::WaitingForYou("this kind of failure needs a person".into()),
                cannot_resume: Some("no evidence".into()),
                attending: Vec::new(),
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

    /// An interrupted agent's state reads in capitals, and in the TUI's
    /// orange only when painted; any other state is never painted.
    #[test]
    fn plain_status_marks_an_interrupted_agent() {
        use crate::cli::agent_state::{
            Activity, AgentState, INTERRUPTED_SGR, Nudge, Observer, View,
        };
        let shown = |activity, paint| {
            let mut snap = minimal_snapshot();
            let mut row = agent_row("claude", None);
            row.state = Some(View {
                owed: None,
                state: AgentState {
                    role: "commit".into(),
                    session: "s1".into(),
                    tool: clank_core::Tool::Claude,
                    monitor: Observer {
                        pid: 1,
                        birth: None,
                    },
                    activity,
                    since: 0,
                    reachability: Default::default(),
                    owed: None,
                    nudge: Nudge::Nothing,
                    cannot_resume: None,
                    attending: Vec::new(),
                    asking: None,
                    repeated: None,
                },
                stale: None,
            });
            snap.agents = vec![row];
            snap.human(paint)
        };
        let plain = shown(Activity::Interrupted, false);
        assert!(plain.contains("state: claude → INTERRUPTED "), "{plain}");
        assert!(!plain.contains('\x1b'), "{plain:?}");
        let painted = shown(Activity::Interrupted, true);
        assert!(
            painted.contains(&format!(
                "state: claude → \x1b[{INTERRUPTED_SGR}mINTERRUPTED "
            )),
            "{painted:?}"
        );
        let idle = shown(Activity::Idle, true);
        assert!(!idle.contains('\x1b'), "{idle:?}");
    }

    /// Work an agent's bound session never received is said in its state
    /// line, in red when painted, with the reason clank keeps its hands
    /// off; and `status -j` carries the oldest owed item's age, kind and
    /// delivery.
    #[test]
    fn status_shouts_undelivered_work_and_json_carries_the_owed_item() {
        use crate::cli::agent_state::{
            Activity, AgentState, Case, HandsOff, Nudge, Observer, OwedItem, Owing,
            UNDELIVERED_SGR, View,
        };
        use crate::cli::ledger::{Delivery, Since};
        let now = crate::cli::recovery::unix_now();
        let item = OwedItem {
            kind: "message".into(),
            what: "message from claude".into(),
            since: Since::At(now - 7200),
            delivery: Delivery::NotDelivered,
        };
        let state = AgentState {
            role: "standby".into(),
            session: "s1".into(),
            tool: clank_core::Tool::Codex,
            monitor: Observer {
                pid: 1,
                birth: None,
            },
            activity: Activity::TurnOpen,
            since: now - 2520,
            reachability: Default::default(),
            owed: Some(Owing {
                oldest: item.clone(),
                undelivered: Some(item),
                warn: 2400,
                case: Case::HandsOff(HandsOff::AutoOff),
            }),
            nudge: Nudge::Nothing,
            cannot_resume: None,
            attending: Vec::new(),
            asking: None,
            repeated: None,
        };
        let mut snap = minimal_snapshot();
        let mut row = agent_row("artist4", None);
        row.state = Some(View::at(state, None, now));
        snap.agents = vec![row];
        let plain = snap.human(false);
        assert!(
            plain.contains("state: artist4 → UNDELIVERED 2h · message from claude · auto off"),
            "{plain}"
        );
        assert!(!plain.contains("working"), "{plain}");
        let painted = snap.human(true);
        assert!(
            painted.contains(&format!(
                "state: artist4 → \x1b[{UNDELIVERED_SGR}mUNDELIVERED 2h"
            )),
            "{painted:?}"
        );
        let json = serde_json::to_value(&snap).unwrap();
        assert_eq!(
            json["agents"][0]["state"]["state"]["reachability"],
            serde_json::json!({"by": "unknown"}),
            "{json}"
        );
        let owed = &json["agents"][0]["state"]["owed"];
        assert_eq!(owed["age"], 7200, "{json}");
        assert_eq!(owed["kind"], "message");
        assert_eq!(owed["delivery"]["is"], "not_delivered");
    }

    /// Status says of an agent what its row says: unreachable, in red once
    /// it is owed work, with the reachability value in the JSON; and of a
    /// bound agent with no published state, that nothing is known.
    #[test]
    fn status_says_unreachable_and_unknown_as_the_row_does() {
        use crate::cli::agent_state::{
            Activity, AgentState, Case, HandsOff, Nudge, Observer, OwedItem, Owing, Reachability,
            UNDELIVERED_SGR, View,
        };
        use crate::cli::ledger::{Delivery, Since};
        let now = crate::cli::recovery::unix_now();
        let item = OwedItem {
            kind: "message".into(),
            what: "message from claude".into(),
            since: Since::At(now - 60),
            delivery: Delivery::NotDelivered,
        };
        let state = |owed: Option<Owing>| AgentState {
            role: "standby".into(),
            session: "s1".into(),
            tool: clank_core::Tool::Grok,
            monitor: Observer {
                pid: 1,
                birth: None,
            },
            activity: Activity::Idle,
            since: now - 2520,
            reachability: Reachability::NoWaiter {
                since: Some(now - 2520),
            },
            owed,
            nudge: Nudge::Nothing,
            cannot_resume: None,
            attending: Vec::new(),
            asking: None,
            repeated: None,
        };
        let shown = |owed: Option<Owing>, paint| {
            let mut snap = minimal_snapshot();
            let mut row = agent_row("grok", None);
            row.state = Some(View::at(state(owed), None, now));
            snap.agents = vec![row];
            (snap.human(paint), serde_json::to_value(&snap).unwrap())
        };
        let (dim, json) = shown(None, true);
        assert!(dim.contains("state: grok → unreachable 42m\n"), "{dim:?}");
        assert_eq!(
            json["agents"][0]["state"]["state"]["reachability"],
            serde_json::json!({"by": "none", "since": now - 2520})
        );
        let owing = Owing {
            oldest: item.clone(),
            undelivered: Some(item),
            warn: 2400,
            case: Case::HandsOff(HandsOff::CannotResume("no way to resume it".into())),
        };
        let (red, _) = shown(Some(owing.clone()), true);
        assert!(
            red.contains(&format!(
                "state: grok → \x1b[{UNDELIVERED_SGR}munreachable 42m · not nudged: no way to \
                 resume it\x1b[0m"
            )),
            "{red:?}"
        );
        let (plain, _) = shown(Some(owing), false);
        assert!(
            plain.contains("state: grok → unreachable 42m · not nudged: no way to resume it"),
            "{plain}"
        );

        let mut snap = minimal_snapshot();
        let mut bound = agent_row("claude", None);
        bound.session = Some(
            serde_json::from_str(
                r#"{"id":"01a156a7-f516-7dc0-8b58-520faf122796","tool":"claude","updated_at":"x"}"#,
            )
            .unwrap(),
        );
        snap.agents = vec![bound, agent_row("codex", None)];
        let said = snap.human(false);
        assert!(said.contains("state: claude → unknown\n"), "{said}");
        assert!(
            !said.contains("state: codex"),
            "an agent with no session has no state to be unknown: {said}"
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

    pub(crate) fn minimal_snapshot() -> StatusSnapshot {
        StatusSnapshot {
            forks: Vec::new(),
            repo_path: PathBuf::from("/repo"),
            monitor: crate::cli::monitor::Liveness::None,
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
                kind: crate::cli::block::Stored::Valid(Default::default()),
                nudge: None,
                unreadable: None,
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
            config_errors: Vec::new(),
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

    /// Tests 1 and 7, the wire half: `status -j` carries a permission
    /// block's kind and suggestion, and an unsettled nudge as unresolved
    /// with its diagnostic; the text status says the same.
    #[test]
    fn status_reports_a_permission_block_and_its_unknown_nudge() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let codex = clank_core::AgentLabel::parse("codex").unwrap();
        crate::cli::block::ask_with(
            repo,
            &codex,
            "push",
            "git push fork was refused",
            &crate::cli::block::Kind::Permission {
                suggestion: Some("yes: push it".into()),
            },
        )
        .unwrap();
        let mut snap = minimal_snapshot();
        snap.blocks = crate::cli::block::scan_blocks(repo);
        let occurrence = snap.blocks[0].occurrence.clone();
        assert_eq!(
            snap.to_json()["blocks"],
            serde_json::json!([{
                "agent": "codex",
                "name": "push",
                "occurrence": occurrence,
                "kind": "permission",
                "suggestion": "yes: push it",
                "question": "git push fork was refused",
                "answer": null,
                "pending": true,
                "nudge": null,
                "unreadable": null,
            }])
        );

        crate::cli::block::resolve(
            repo,
            "codex",
            "push",
            None,
            crate::cli::block::By::Person,
            crate::cli::block::How::Nudge {
                text: "yes: push it",
                resend: false,
                via: &mut |_: &str| crate::cli::nudge_log::Outcome::Indeterminate {
                    reason: "the prompt was not confirmed".into(),
                },
            },
        )
        .unwrap();
        snap.blocks = crate::cli::block::scan_blocks(repo);
        let block = &snap.to_json()["blocks"][0];
        assert_eq!(block["pending"], true, "unresolved, not answered");
        let nudge = &block["nudge"];
        assert_eq!(nudge["outcome"], "unknown");
        assert_eq!(nudge["diagnostic"], "the prompt was not confirmed");
        assert_eq!(
            nudge["reason"],
            "nudge outcome unknown: the prompt was not confirmed"
        );
        assert_eq!(nudge["text"], "yes: push it");
        assert!(nudge["started"].as_u64().is_some() && nudge["attempt"].is_string());
        let text = snap.to_human();
        assert!(
            text.contains("BLOCKED (codex, permission): git push fork was refused")
                && text.contains("suggested: yes: push it")
                && text.contains("nudge outcome unknown: the prompt was not confirmed"),
            "{text}"
        );
    }

    /// Unreadable state is reported, never hidden: the block stays
    /// pending, its unreadable attempt shows as an unknown outcome, and
    /// `unreadable` says why.
    #[test]
    fn status_reports_unreadable_block_state() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let codex = clank_core::AgentLabel::parse("codex").unwrap();
        crate::cli::block::ask_with(
            repo,
            &codex,
            "push",
            "git push fork was refused",
            &crate::cli::block::Kind::Permission { suggestion: None },
        )
        .unwrap();
        std::fs::write(repo.join(".clank/agents/codex/blocks/push.attempt"), "torn").unwrap();
        let mut snap = minimal_snapshot();
        snap.blocks = crate::cli::block::scan_blocks(repo);
        let block = &snap.to_json()["blocks"][0];
        assert_eq!(block["pending"], true);
        assert_eq!(block["kind"], "permission");
        assert_eq!(block["nudge"]["outcome"], "unknown");
        assert!(block["nudge"]["text"].is_null());
        let why = block["unreadable"].as_str().expect("says why");
        assert!(why.contains("malformed"), "{why}");
        assert_eq!(block["nudge"]["diagnostic"], why);
        let text = snap.to_human();
        assert!(text.contains(&format!("unreadable: {why}")), "{text}");
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
            // Always said: no monitor is a fact, not an omission.
            "monitor": "none",
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
                "kind": "plain",
                "suggestion": null,
                "question": "why?",
                "answer": null,
                "pending": true,
                "nudge": null,
                "unreadable": null,
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
            config_errors,
            // Emitted, and pinned by the wire fixture below rather
            // than here: `--repo` means the reader need not be
            // standing in it (codex on 772b84f).
            repo_path: _,
            // Read from the runtime namespace, not the repo.
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
        assert!(config_errors.is_empty() && v.get("config_errors").is_none());
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
                    "attending": [],
                },
                {
                    "label": "ruthless",
                    "role": "final",
                    "auto": "off",
                    "tool": "claude",
                    "invocation": "claude",
                    "session": null,
                    "attending": [],
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
