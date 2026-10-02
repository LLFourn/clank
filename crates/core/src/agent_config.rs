//! Typed config schema for per-agent Clank state.
//!
//! Pure data + serde round-trip. No clock, no env, no filesystem
//! — the CLI is the only place that touches the world. This type
//! describes what's written to
//! `.clank/agents/<label>/config.json` (per-agent, per-machine;
//! gitignored).
//!
//! There is intentionally no repo-shared config type. Master is
//! a per-user preference: whoever wrote the plan wants their
//! `wait` to default to master's view; everyone else wants
//! reviewers. There's no useful repo-wide assertion of "alice is
//! THE master" — gate state is computed from the cumulative
//! participant set, not from role claims.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ids::SessionId;
use crate::vocab::{AutoMode, Tool};

/// Per-agent, per-machine state for one (label, repo). Lives at
/// `.clank/agents/<label>/config.json`. Gitignored.
///
/// State only: which session this label is bound to, whether
/// auto-mode is on, and the wait-for-work timeout. Identity
/// (role / tool / launch / initial_prompt) lives in the
/// team-based registration model — user-scope `agents` +
/// `teams` and repo-scope `team` — not here. Unknown keys in
/// existing on-disk skeletons (e.g. a leftover `role`/`tool`
/// from before the cutover) are ignored by serde.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    /// `Some` only when this session EXPLICITLY chose via `clank auto
    /// on|off`; `None` means "unset, inherit the user-global default"
    /// (`auto-mode-default-on`). The distinction matters: a fresh
    /// skeleton must be `None` so it can inherit, while an explicit
    /// `clank auto off` must STICK even under a global default — so
    /// the field is omitted from a fresh skeleton, never written as a
    /// silent `off`. Resolve via [`effective_auto_mode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_mode: Option<AutoMode>,
    /// Session this agent label is currently bound to. Written by
    /// `clank as <label>` (or `clank init` phase 2) using the
    /// `CLAUDE_CODE_SESSION_ID` / `CODEX_THREAD_ID` env var.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Session>,
    /// Extra wake sources for this agent's `clank wait`
    /// (extra-wait-events): things beyond this repo's state — GitHub
    /// activity on OTHER repos, arbitrary command completions. Fixed
    /// at arm time; a config edit takes effect on the next re-arm.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wait_events: Vec<WaitEventSource>,
}

/// One extra wake source (extra-wait-events). Tagged sum type so the
/// set is additive: new kinds parse-fail LOUD on old binaries (an
/// unknown `kind` is an error, not a silent drop) and new filters can
/// join a kind without breaking existing configs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitEventSource {
    Github(GithubSource),
    Command(CommandSource),
}

/// Watch one GitHub repo's event feed. An agent lists as many of
/// these as it likes — nothing is fixed to a single repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubSource {
    /// `owner/name`.
    pub repo: String,
    /// Which event sub-kinds wake the agent — the FLAT shape, whose
    /// scopes are [`prs`](Self::prs) and [`branches`](Self::branches).
    /// Empty only when [`watch`](Self::watch) says it instead.
    #[serde(default)]
    pub events: Vec<GithubEventKind>,
    /// Poll cadence as a duration string (`"60s"`, `"5m"`), same form
    /// as a duration string; validated at use site. The EFFECTIVE
    /// interval is max(this, the server's `X-Poll-Interval`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval: Option<String>,
    /// Whether events actored by the authenticated `gh` login wake
    /// this agent. Default FALSE: a controller agent's own PR comment
    /// must not wake itself in a loop (and its own pushes don't
    /// either — load-bearing for `branch_push`).
    #[serde(default)]
    pub include_own_actions: bool,
    /// `branch_push` scope: only pushes to these branches wake the
    /// agent (short names, e.g. `"main"`). Empty = every branch.
    /// Applies to `branch_push` only — the PR kinds are
    /// number-scoped already.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<String>,
    /// PR scope: only these PR numbers wake the agent. Empty = every
    /// PR the watch sees.
    ///
    /// Applies to every kind that NAMES a PR — `pr_updated`,
    /// `pr_merged`, `pr_comment` and the two CI kinds — the way
    /// [`branches`](Self::branches) applies to `branch_push`.
    /// `pr_opened` is deliberately outside it (a PR that does not
    /// exist yet cannot be in a list written before it), as are the
    /// issue kinds and `branch_push`.
    ///
    /// For the feed kinds it filters events that arrive anyway; for
    /// CI it also bounds the WORK, since each PR in scope is a pair
    /// of requests per poll.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prs: Vec<u64>,
    /// Operator instructions stamped onto every wake item this
    /// watch presents (github-watch-prompts): free text telling the
    /// agent exactly how to react. Presentation config — never
    /// stored in the event WAL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Objects and their events, as an alternative to the flat
    /// [`events`](Self::events) + [`prs`](Self::prs) +
    /// [`branches`](Self::branches). Empty means the flat shape; a
    /// config uses one or the other, never both.
    ///
    /// See [`WatchClause`] for why nothing translates between them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watch: Vec<WatchClause>,
    /// How events arrive (github-http-client-and-realtime M2):
    /// `poll` (default) or `realtime` — the webhook-forward relay
    /// front-running the poll for latency, with the poll remaining
    /// the completeness backstop. Unknown values fail loud.
    #[serde(default)]
    pub delivery: Delivery,
}

/// One clause of a nested watch (github-selectors): which objects,
/// which of their events, and the filters describing those events.
///
/// A watch admits an event if ANY clause does, so clauses SELECT and
/// do not fan out — one event matched twice is still one event,
/// because its feed id or action key is what the log dedups on. To
/// give two events different filters, write two clauses.
///
/// This is not a second encoding of the flat fields, and nothing
/// translates between them, because where they overlap they mean
/// DIFFERENT things: [`GithubSource::prs`] scopes only the kinds that
/// name a PR (`pr_opened` is deliberately outside it), while
/// `numbers` under `pr` scopes every event beneath it. Rewriting a
/// flat watch into this shape therefore changes what it wakes on,
/// which is the author's call to make deliberately — and it re-keys
/// the watch's log, so it is not a migration anything should perform
/// unasked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchClause {
    Pr(PrWatch),
    Issue(IssueWatch),
    Branch(BranchWatch),
}

/// Pull requests and the events under them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrWatch {
    /// Only these PRs. Empty = every PR the watch sees. Unlike
    /// [`GithubSource::prs`] this scopes EVERY event in the clause,
    /// `opened` included — so a watch that wants new PRs as well
    /// gives them their own clause.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub numbers: Vec<u64>,
    pub events: Vec<PrEvent>,
    /// Overrides [`GithubSource::include_own_actions`] for this
    /// clause alone. `review_submitted` ignores both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_own_actions: Option<bool>,
}

/// Non-PR issues and the events under them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueWatch {
    pub events: Vec<IssueEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_own_actions: Option<bool>,
}

/// Branches and the events under them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchWatch {
    /// Short names, e.g. `"main"`. Empty = every branch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    pub events: Vec<BranchEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_own_actions: Option<bool>,
}

/// What can happen to a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrEvent {
    Opened,
    Updated,
    Merged,
    /// Every comment class below, the review bodies of a watch that
    /// does not also name `review_submitted` included.
    Comment,
    /// An issue comment posted on the PR.
    IssueComment,
    /// An inline comment on the diff — what a pending review's drafts
    /// arrive as.
    ReviewComment,
    /// A review that was SUBMITTED. Never a pending one, never its
    /// drafts, and never filtered by `include_own_actions`.
    ReviewSubmitted,
    CiPassed,
    CiFailed,
}

/// What can happen to an issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueEvent {
    Opened,
    Closed,
    Comment,
}

/// What can happen to a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchEvent {
    Push,
}

/// Which comment class a `pr_comment` is: the classifier stamps it as
/// the event's `detail`, and [`PrEvent`] narrows to one.
///
/// One spelling for both, because the narrowing is a string match
/// against what the classifier wrote — a second copy of these words
/// would fail by selecting nothing rather than by failing to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentClass {
    IssueCommentOnPr,
    ReviewComment,
    /// A submitted review seen as a comment, by a watch that predates
    /// `review_submitted`.
    Review,
}

impl CommentClass {
    pub fn wire(self) -> &'static str {
        match self {
            Self::IssueCommentOnPr => "issue_comment_on_pr",
            Self::ReviewComment => "review_comment",
            Self::Review => "review",
        }
    }
}

impl PrEvent {
    /// The kind this names, and the `detail` it narrows to
    /// (`None` = every detail of that kind).
    pub fn selects(self) -> (GithubEventKind, Option<&'static str>) {
        match self {
            Self::Opened => (GithubEventKind::PrOpened, None),
            Self::Updated => (GithubEventKind::PrUpdated, None),
            Self::Merged => (GithubEventKind::PrMerged, None),
            Self::Comment => (GithubEventKind::PrComment, None),
            Self::IssueComment => (
                GithubEventKind::PrComment,
                Some(CommentClass::IssueCommentOnPr.wire()),
            ),
            Self::ReviewComment => (
                GithubEventKind::PrComment,
                Some(CommentClass::ReviewComment.wire()),
            ),
            Self::ReviewSubmitted => (GithubEventKind::ReviewSubmitted, None),
            Self::CiPassed => (GithubEventKind::CiPassed, None),
            Self::CiFailed => (GithubEventKind::CiFailed, None),
        }
    }
}

impl IssueEvent {
    pub fn selects(self) -> (GithubEventKind, Option<&'static str>) {
        match self {
            Self::Opened => (GithubEventKind::IssueOpened, None),
            Self::Closed => (GithubEventKind::IssueClosed, None),
            Self::Comment => (GithubEventKind::IssueComment, None),
        }
    }
}

impl BranchEvent {
    pub fn selects(self) -> (GithubEventKind, Option<&'static str>) {
        match self {
            Self::Push => (GithubEventKind::BranchPush, None),
        }
    }
}

impl WatchClause {
    /// Every `(kind, detail)` this clause names, in written order.
    pub fn selects(&self) -> Vec<(GithubEventKind, Option<&'static str>)> {
        match self {
            Self::Pr(w) => w.events.iter().map(|e| e.selects()).collect(),
            Self::Issue(w) => w.events.iter().map(|e| e.selects()).collect(),
            Self::Branch(w) => w.events.iter().map(|e| e.selects()).collect(),
        }
    }

    /// This clause's own `include_own_actions`, if it set one.
    pub fn include_own_actions(&self) -> Option<bool> {
        match self {
            Self::Pr(w) => w.include_own_actions,
            Self::Issue(w) => w.include_own_actions,
            Self::Branch(w) => w.include_own_actions,
        }
    }

    /// The object name an operator wrote, for diagnostics.
    pub fn object(&self) -> &'static str {
        match self {
            Self::Pr(_) => "pr",
            Self::Issue(_) => "issue",
            Self::Branch(_) => "branch",
        }
    }
}

impl GithubSource {
    /// What makes this watch unusable, said so the operator knows
    /// what to write instead — or `None` if it is fine.
    ///
    /// The shape errors serde already gives (an unknown object, event
    /// or filter, each listing the valid ones) are not repeated here.
    /// What is left is what one field means in the presence of
    /// another, which serde cannot see.
    pub fn refusal(&self) -> Option<String> {
        let at = || format!("github watch on {}", self.repo);
        if self.watch.is_empty() {
            return self.events.is_empty().then(|| {
                format!(
                    "{} names no events: give it `events` (a flat list) or `watch` \
                     (objects and the events under them)",
                    at()
                )
            });
        }
        if !self.events.is_empty() {
            return Some(format!(
                "{} names events both at the top level and under `watch`. Use one shape: \
                 they scope differently, so there is no reading that means both.",
                at()
            ));
        }
        for (field, used) in [
            ("prs", !self.prs.is_empty()),
            ("branches", !self.branches.is_empty()),
        ] {
            if used {
                return Some(format!(
                    "{} sets `{field}` beside `watch`. `{field}` scopes the flat shape, and it \
                     scopes FEWER events than a clause does — write the scope on the object \
                     instead, as {{\"pr\": {{\"numbers\": [..], \"events\": [..]}}}} or \
                     {{\"branch\": {{\"names\": [..], \"events\": [\"push\"]}}}}.",
                    at()
                ));
            }
        }
        self.watch
            .iter()
            .find(|c| c.selects().is_empty())
            .map(|c| format!("{}: its `{}` clause names no events", at(), c.object()))
    }
}

/// Delivery mode for a github source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    #[default]
    Poll,
    Realtime,
}

/// The GitHub event sub-kinds `clank wait` can wake on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GithubEventKind {
    PrOpened,
    /// New commits landed on a PR (the events feed's `synchronize`
    /// action). Fires on the BASE repo, so fork PRs are covered.
    PrUpdated,
    PrMerged,
    /// All three comment classes: an issue comment on a PR, a
    /// submitted review (including empty-body approvals), and an
    /// inline diff comment.
    ///
    /// Lumping them is why `ReviewSubmitted` exists: a team that
    /// drafts review comments under one login has to exclude that
    /// login to stop the drafts, and excluding it also loses that
    /// person's submitted reviews.
    PrComment,
    /// A review that was SUBMITTED — approved, changes requested, or
    /// commented — never a pending one and never its draft comments.
    ///
    /// Its own kind so a watch can ask for every submitted review
    /// while still filtering comments, and it ignores
    /// `include_own_actions`: submitting is a deliberate act, and a
    /// watcher who submits a review has done something its team
    /// should hear about even though the actor is itself.
    ReviewSubmitted,
    IssueOpened,
    IssueClosed,
    /// An issue comment on a NON-PR issue.
    IssueComment,
    /// A push to a BRANCH (`refs/heads/*` only — tag pushes are not
    /// branch pushes and never match). Scope with
    /// [`GithubSource::branches`].
    BranchPush,
    /// Every check reporting on a PR's head commit has finished and
    /// none failed. Scope with [`GithubSource::prs`].
    CiPassed,
    /// A check on a PR's head commit reached a terminal state that is
    /// not a success. Scope with [`GithubSource::prs`].
    CiFailed,
}

impl GithubEventKind {
    /// Whether this kind is read from a PR's checks rather than from
    /// the repo's event feed. The two CI kinds are a reduction over
    /// every check reporting on a PR head (`github::ci`), because no
    /// single check suite is evidence about the PR.
    pub fn is_ci(self) -> bool {
        matches!(self, Self::CiPassed | Self::CiFailed)
    }

    /// Whether `include_own_actions` can suppress this kind.
    ///
    /// A submitted review is the exception. The filter exists so a
    /// watcher does not hear its own chatter back, and submitting is
    /// not chatter — it is the act the rest of the team is waiting
    /// on, and the login that drafts a review is usually the login
    /// that submits it.
    ///
    /// On the type because two places ask: the filter itself, and the
    /// question of whether a watch needs to resolve its own login at
    /// all.
    pub fn is_filterable_by_actor(self) -> bool {
        self != Self::ReviewSubmitted
    }

    /// Whether [`GithubSource::prs`] scopes this kind — i.e. whether
    /// it names a PR that could be in such a list.
    ///
    /// `PrOpened` is NOT scoped: a PR that does not exist yet cannot
    /// appear in a list written before it, so a watch that wants new
    /// PRs and a named PR's activity lists both and gets both.
    pub fn is_pr_scoped(self) -> bool {
        matches!(
            self,
            Self::PrUpdated
                | Self::PrMerged
                | Self::PrComment
                | Self::ReviewSubmitted
                | Self::CiPassed
                | Self::CiFailed
        )
    }

    /// The wire name, and its inverse. The pair lives together so a
    /// kind added to one is missing from the other loudly.
    pub fn wire(self) -> &'static str {
        match self {
            Self::PrOpened => "pr_opened",
            Self::PrUpdated => "pr_updated",
            Self::PrMerged => "pr_merged",
            Self::PrComment => "pr_comment",
            Self::ReviewSubmitted => "review_submitted",
            Self::IssueOpened => "issue_opened",
            Self::IssueClosed => "issue_closed",
            Self::IssueComment => "issue_comment",
            Self::BranchPush => "branch_push",
            Self::CiPassed => "ci_passed",
            Self::CiFailed => "ci_failed",
        }
    }

    /// Every kind. The one list: `from_wire` reads it, and a caller
    /// selecting a subset (`is_pr_scoped`, `is_ci`) filters it, so a
    /// kind added to the enum cannot be missing from one of them.
    pub const ALL: [Self; 11] = [
        Self::PrOpened,
        Self::PrUpdated,
        Self::PrMerged,
        Self::PrComment,
        Self::ReviewSubmitted,
        Self::IssueOpened,
        Self::IssueClosed,
        Self::IssueComment,
        Self::BranchPush,
        Self::CiPassed,
        Self::CiFailed,
    ];

    pub fn from_wire(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.wire() == s)
    }
}

/// A long-running producer streaming JSONL events
/// (a-command-is-an-event-source-like-any-other). One object per line
/// on stdout is one event; stderr is diagnostics. Supervised: it is
/// expected to stay up, restarted with backoff when it does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSource {
    /// Labels the wake item, and IDENTIFIES the source: its durable
    /// state — resume token and decision window — is keyed on this
    /// alone. See [`CommandSource::source_key`].
    pub name: String,
    /// Argv — program + args, NO implicit shell (write
    /// `["sh", "-c", "…"]` to get one). Runs in its own process
    /// group; the group is killed when the wait exits without it.
    pub command: Vec<String>,
    /// Optional backend that accepts answers to this source's events. Receives
    /// one JSON object on stdin; subscription argv and source identity stay put.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_command: Option<Vec<String>>,
    /// The single character the log and the TUI draw this source's
    /// events with. Config, not per-line, so an untrusted producer
    /// cannot reach the renderer's layout; validated at load for a
    /// display width of exactly one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glyph: Option<String>,
}

impl CommandSource {
    /// The identity its durable state is filed under.
    ///
    /// The NAME alone. Not the argv (fixing a path in a command does
    /// not make it a different source), not the filter (see `kinds`),
    /// not the glyph (cosmetic) — the same reasoning that keeps a
    /// github watch's prompt out of its key.
    ///
    /// Slug PLUS a hash of the whole name, as the github key does.
    /// The slug alone is lossy — `build/test` and `build-test` both
    /// flatten to `build-test` — and this key owns the ingest lease
    /// and the resume/decision state, so a collision would make two
    /// unrelated sources contend for one lease and advance each
    /// other's checkpoint past events they never saw (codex c6dc55f).
    pub fn source_key(&self) -> String {
        let slug: String = self
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let mut h: u32 = 0x811c_9dc5;
        for b in self.name.as_bytes() {
            h ^= u32::from(*b);
            h = h.wrapping_mul(0x0100_0193);
        }
        format!("command-{slug}-{h:08x}")
    }
}

/// Resolve a session's effective auto-mode from the per-agent
/// setting and the user-global default (`auto-mode-default-on`).
/// An explicit per-agent choice wins (so `clank auto off` sticks);
/// otherwise the user-global default applies; otherwise `Off`. This
/// is THE single resolver — every consumer (stop hook, agent start,
/// `clank auto status`) routes through it so the surfaces can't
/// disagree.
pub fn effective_auto_mode(
    per_agent: Option<AutoMode>,
    user_default: Option<AutoMode>,
) -> AutoMode {
    effective_auto_mode_with_source(per_agent, user_default).0
}

/// Where an effective auto-mode came from — so surfaces that show the
/// value (doctor) can say WHY without recreating the precedence
/// (grok-first-turn-orchestration: doctor printed the raw per-agent
/// field, an agent read `unset` as off, and misdiagnosed a wake gap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoModeSource {
    PerAgent,
    UserDefault,
    /// Neither set: the built-in `Off`.
    Builtin,
}

/// The single mode+source resolver every consumer projects from.
pub fn effective_auto_mode_with_source(
    per_agent: Option<AutoMode>,
    user_default: Option<AutoMode>,
) -> (AutoMode, AutoModeSource) {
    match (per_agent, user_default) {
        (Some(m), _) => (m, AutoModeSource::PerAgent),
        (None, Some(m)) => (m, AutoModeSource::UserDefault),
        (None, None) => (AutoMode::Off, AutoModeSource::Builtin),
    }
}

/// General-purpose launch profile: executable + args + env.
///
/// Two consumers in the codebase today (per OQ2 of
/// `clank-diff-editor`: reuse this struct rather than fork):
///
/// 1. **`clank agent start <name>`** (per-agent launch). Args are
///    spliced BEFORE the session-restore suffix (claude's
///    `--resume <id>` or codex's `resume <id> --cd <repo>`) so
///    they attach to the top-level tool. For codex specifically:
///    flags AFTER the `resume` subcommand attach to `resume`, not
///    to the `codex` binary; putting `launch.args` first preserves
///    the typical use case (`codex --profile deep resume <id>`).
///    For claude (flat flags) position is cosmetic, but the same
///    rule keeps the mental model consistent.
///
/// 2. **`clank diff` editor launch** (`Config.diff.editor`).
///    `args` may include template variables like `{range}`,
///    `{commits}`, `{patch_file}` that are substituted at compose
///    time. Args do not have a "session-restore suffix" concept;
///    args are passed verbatim (post-substitution) to the editor.
///
/// If future consumers need fields beyond `command/args/env`
/// (e.g. multi-pane layout descriptors), split into a sibling
/// struct rather than overloading this one.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LaunchConfig {
    /// Override the executable. `None` falls back to the
    /// session-tool's bare name (`claude` / `codex`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Args passed to the executable BEFORE the session-restore
    /// suffix. Example: `["--profile", "deep"]` for a codex agent
    /// produces `codex --profile deep resume <id> --cd <repo>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Env vars merged onto the calling process env. On key
    /// collision, this map wins ("config wins over inherited
    /// environment").
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// One session's binding to an agent label. Stored under
/// [`AgentConfig::session`]; the resolver matches by `(id, tool)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub tool: Tool,
    /// RFC3339 / ISO-8601 timestamp of the last bind, in UTC
    /// (e.g. `"2026-05-23T06:24:47Z"`). Stringly typed so core
    /// stays free of `chrono`/`time` deps; the CLI formats with
    /// whatever clock source it has. UTC over local-offset
    /// because the `time` crate's `local-offset` codepath has
    /// known soundness issues in multi-threaded programs.
    pub updated_at: String,
    /// Where this session's own transcript lives, when the harness
    /// said (Claude's SessionStart hook does). Absent for harnesses
    /// that keep no readable transcript, or whose file is found
    /// another way (codex, by session id). `default` so every
    /// binding written before this field existed still reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
}

#[cfg(test)]
mod watch_grammar_tests {
    use super::*;

    fn src(json: &str) -> Result<GithubSource, serde_json::Error> {
        serde_json::from_str(json)
    }

    fn ok(json: &str) -> GithubSource {
        let s = src(json).expect("parses");
        assert_eq!(s.refusal(), None, "{json}");
        s
    }

    /// The plan's own example, verbatim enough to matter: objects as
    /// the major type, their events beneath, and a scope written once
    /// on the object.
    #[test]
    fn the_nested_shape_parses_as_written() {
        let s = ok(r#"{
            "repo": "frostsnap/frostsnap",
            "watch": [
              {"pr": {"numbers": [603, 522],
                      "events": ["review_submitted", "comment", "review_comment",
                                 "updated", "merged"]}},
              {"pr": {"events": ["opened", "merged", "review_submitted"]}},
              {"issue": {"events": ["opened"]}}
            ]
        }"#);
        assert_eq!(s.watch.len(), 3);
        assert!(s.events.is_empty(), "the flat list stays empty");
        let WatchClause::Pr(first) = &s.watch[0] else {
            panic!("the first clause is about PRs")
        };
        assert_eq!(first.numbers, [603, 522]);
        assert_eq!(
            first.events[2].selects(),
            (
                GithubEventKind::PrComment,
                Some(CommentClass::ReviewComment.wire())
            ),
            "an inline comment narrows pr_comment rather than inventing a kind",
        );
    }

    /// Serde refuses an unknown name and names the alternatives, which
    /// is the whole reason the vocabularies are enums rather than
    /// strings. An operator who guesses learns the list from the
    /// error.
    #[test]
    fn an_unknown_selector_is_refused_with_the_valid_ones() {
        let object = src(r#"{"repo":"o/r","watch":[{"pull": {"events": ["opened"]}}]}"#)
            .expect_err("an unknown object is refused")
            .to_string();
        for want in ["pull", "pr", "issue", "branch"] {
            assert!(object.contains(want), "{object}");
        }

        let event = src(r#"{"repo":"o/r","watch":[{"pr": {"events": ["approved"]}}]}"#)
            .expect_err("an unknown event is refused")
            .to_string();
        for want in [
            "approved",
            "review_submitted",
            "review_comment",
            "ci_failed",
        ] {
            assert!(event.contains(want), "{event}");
        }

        let filter = src(
            r#"{"repo":"o/r","watch":[{"pr": {"events": ["opened"], "include_own_action": false}}]}"#,
        )
        .expect_err("an unknown filter is refused")
        .to_string();
        for want in ["include_own_action", "numbers", "events"] {
            assert!(filter.contains(want), "{filter}");
        }

        // An issue has no numbers to scope by, and saying so beats
        // accepting a field that would do nothing.
        assert!(
            src(r#"{"repo":"o/r","watch":[{"issue": {"numbers": [1], "events": ["opened"]}}]}"#)
                .is_err()
        );
    }

    /// The two shapes scope differently, so a config that mixes them
    /// has no reading that means both. Each refusal says what to
    /// write instead.
    #[test]
    fn the_two_shapes_are_not_mixed() {
        let both = src(r#"{"repo":"o/r","events":["pr_opened"],
                           "watch":[{"pr":{"events":["opened"]}}]}"#)
        .unwrap()
        .refusal()
        .expect("refused");
        assert!(
            both.contains("both at the top level and under `watch`"),
            "{both}"
        );

        let prs = src(r#"{"repo":"o/r","prs":[1],"watch":[{"pr":{"events":["opened"]}}]}"#)
            .unwrap()
            .refusal()
            .expect("refused");
        assert!(
            prs.contains("\"numbers\""),
            "{prs} must say what to write instead"
        );

        let branches =
            src(r#"{"repo":"o/r","branches":["main"],"watch":[{"branch":{"events":["push"]}}]}"#)
                .unwrap()
                .refusal()
                .expect("refused");
        assert!(branches.contains("\"names\""), "{branches}");

        let empty = src(r#"{"repo":"o/r","watch":[{"pr":{"events":[]}}]}"#)
            .unwrap()
            .refusal()
            .expect("refused");
        assert!(empty.contains("`pr` clause names no events"), "{empty}");

        let neither = src(r#"{"repo":"o/r"}"#)
            .unwrap()
            .refusal()
            .expect("refused");
        assert!(neither.contains("names no events"), "{neither}");
    }

    /// A clause inherits the watch's own-actions setting and may
    /// override it, so one rule everywhere is still written once.
    #[test]
    fn a_clause_overrides_the_watchs_own_actions_setting() {
        let s = ok(r#"{"repo":"o/r","include_own_actions":true,"watch":[
            {"pr":{"events":["comment"],"include_own_actions":false}},
            {"pr":{"events":["merged"]}}
        ]}"#);
        assert_eq!(s.watch[0].include_own_actions(), Some(false));
        assert_eq!(s.watch[1].include_own_actions(), None, "inherits");
    }

    /// Every flat config still parses, and gains an empty `watch`
    /// rather than a translation of itself.
    #[test]
    fn a_flat_config_is_untouched() {
        let s = ok(r#"{"repo":"o/r","events":["pr_comment"],"prs":[7],
                       "branches":["main"],"include_own_actions":true}"#);
        assert!(s.watch.is_empty());
        assert_eq!(s.prs, [7]);
        // And it serializes without the new key, so a round-trip
        // through this binary does not rewrite anyone's config.
        let back = serde_json::to_string(&s).unwrap();
        assert!(!back.contains("watch"), "{back}");
    }
}

#[cfg(test)]
mod tests {
    fn cmd(name: &str) -> CommandSource {
        CommandSource {
            name: name.into(),
            command: vec!["true".into()],
            reply_command: None,
            glyph: None,
        }
    }

    #[test]
    fn reply_handlers_round_trip_without_rekeying_existing_command_sources() {
        let legacy: WaitEventSource = serde_json::from_value(serde_json::json!({
            "kind":"command", "name":"questions", "command":["backend","subscribe"]
        }))
        .unwrap();
        let with_reply: WaitEventSource = serde_json::from_value(serde_json::json!({
            "kind":"command", "name":"questions", "command":["backend","subscribe"],
            "reply_command":["backend","answer","--verbatim"]
        }))
        .unwrap();
        let (WaitEventSource::Command(old), WaitEventSource::Command(new)) = (&legacy, &with_reply)
        else {
            panic!("command sources");
        };
        assert!(old.reply_command.is_none());
        assert!(
            !serde_json::to_value(&legacy)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("reply_command")
        );
        assert_eq!(old.source_key(), new.source_key());
        assert_eq!(
            new.reply_command.as_ref().unwrap(),
            &["backend", "answer", "--verbatim"]
        );
        assert_eq!(
            serde_json::from_value::<WaitEventSource>(serde_json::to_value(&with_reply).unwrap())
                .unwrap(),
            with_reply
        );
    }

    /// Names that SLUG the same must not key the same.
    ///
    /// The key owns the ingest lease and the resume/decision state,
    /// so a collision makes two unrelated sources contend for one
    /// lease and advance each other's checkpoint past events they
    /// never saw (codex c6dc55f).
    #[test]
    fn distinct_names_never_share_a_key() {
        let colliding = [
            ("build/test", "build-test"),
            ("a b", "a-b"),
            ("ci:deploy", "ci-deploy"),
            ("café", "caf-"),
        ];
        for (a, b) in colliding {
            assert_ne!(
                cmd(a).source_key(),
                cmd(b).source_key(),
                "`{a}` and `{b}` slug alike and must still key apart",
            );
        }
        // And the same name keys the same, every time.
        assert_eq!(
            cmd("build/test").source_key(),
            cmd("build/test").source_key()
        );
    }

    /// The key is the NAME's alone: everything else may change
    /// without orphaning the source's durable state.
    ///
    /// Re-keying is not free — the old state file is stranded, the
    /// window it held is lost, and a widened filter would re-offer
    /// every event still in the producer's reach.
    #[test]
    fn the_key_survives_every_change_but_a_rename() {
        let base = cmd("deploys");
        let key = base.source_key();

        let mut argv = base.clone();
        argv.command = vec!["sh".into(), "-c".into(), "other".into()];
        assert_eq!(
            argv.source_key(),
            key,
            "fixing a command is not a new source"
        );

        let mut glyph = base.clone();
        glyph.glyph = Some("*".into());
        assert_eq!(glyph.source_key(), key, "the glyph is cosmetic");

        let mut renamed = base.clone();
        renamed.name = "deploys-2".into();
        assert_ne!(renamed.source_key(), key, "a rename IS a new source");
    }

    use super::*;

    fn session_id(s: &str) -> SessionId {
        SessionId::parse(s).unwrap()
    }

    #[test]
    fn agent_config_round_trips_defaults() {
        let cfg = AgentConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
        assert_eq!(back.auto_mode, None, "fresh skeleton is unset, not Off");
        assert!(back.session.is_none());
    }

    #[test]
    fn agent_config_round_trips_populated() {
        let cfg = AgentConfig {
            auto_mode: Some(AutoMode::On),
            session: Some(Session {
                id: session_id("742f6a04-f174-409a-ab01-419a16c5f372"),
                tool: Tool::Claude,
                updated_at: "2026-05-23T16:24:47+10:00".into(),
                transcript: None,
            }),
            wait_events: Vec::new(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn agent_config_ignores_unknown_legacy_fields() {
        // On-disk skeletons from before the team-based cutover
        // still carry `role`/`tool`/`launch`/`initial_prompt`.
        // serde must ignore them (no `deny_unknown_fields`).
        let json = r#"{
            "auto_mode": "off",
            "role": "master",
            "tool": "claude",
            "launch": { "command": "claude", "args": ["--skill", "ruthless"] },
            "initial_prompt": "hi"
        }"#;
        let cfg: AgentConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.auto_mode, Some(AutoMode::Off), "explicit off survives");
        assert!(cfg.session.is_none());
        // Reserialization drops the unknown keys entirely.
        let back = serde_json::to_string(&cfg).unwrap();
        assert!(!back.contains("role"));
        assert!(!back.contains("tool"));
        assert!(!back.contains("launch"));
    }

    #[test]
    fn launch_config_default_is_empty() {
        let lc = LaunchConfig::default();
        assert!(lc.command.is_none());
        assert!(lc.args.is_empty());
        assert!(lc.env.is_empty());
        // Round-trips as `{}`.
        let json = serde_json::to_string(&lc).unwrap();
        assert_eq!(json, "{}", "default LaunchConfig should emit `{{}}`");
    }

    #[test]
    fn agent_config_accepts_missing_optional_fields() {
        let json = r#"{ "auto_mode": "wait" }"#;
        let cfg: AgentConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.auto_mode, Some(AutoMode::On));
        assert!(cfg.session.is_none());
    }

    #[test]
    fn agent_config_omits_none_session() {
        let cfg = AgentConfig {
            auto_mode: Some(AutoMode::On),
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("session"), "expected no session key: {json}");
    }

    #[test]
    fn github_source_round_trips_new_kinds_and_branches() {
        // github-wake-pr-updates-and-pushes: the additive sub-kinds +
        // the branches filter round-trip…
        let src = WaitEventSource::Github(GithubSource {
            repo: "o/r".into(),
            events: vec![GithubEventKind::PrUpdated, GithubEventKind::BranchPush],
            watch: Vec::new(),
            poll_interval: None,
            include_own_actions: false,
            branches: vec!["main".into()],
            prs: vec![7, 9],
            delivery: Delivery::Realtime,
            prompt: None,
        });
        let json = serde_json::to_string(&src).unwrap();
        assert!(json.contains("pr_updated") && json.contains("branch_push"));
        let back: WaitEventSource = serde_json::from_str(&json).unwrap();
        assert_eq!(back, src);
        // …and an OLD config (absent branches, original kinds) parses
        // unchanged with the empty-filter default.
        // delivery absent = poll; unknown value fails loud.
        assert!(
            serde_json::from_str::<WaitEventSource>(
                r#"{"kind":"github","repo":"o/r","events":[],"delivery":"carrier-pigeon"}"#
            )
            .is_err()
        );
        let old = r#"{"kind":"github","repo":"o/r","events":["pr_opened","issue_comment"]}"#;
        let parsed: WaitEventSource = serde_json::from_str(old).unwrap();
        let WaitEventSource::Github(g) = parsed else {
            panic!("github kind");
        };
        assert!(g.branches.is_empty(), "absent branches = every branch");
        assert_eq!(g.delivery, Delivery::Poll, "absent delivery = poll");
        assert_eq!(
            g.events,
            vec![GithubEventKind::PrOpened, GithubEventKind::IssueComment]
        );
    }

    #[test]
    fn effective_auto_mode_layers_explicit_over_default() {
        use AutoMode::{Off, On};
        // Explicit per-agent wins (so `clank auto off` sticks even
        // under a global default-on).
        assert_eq!(effective_auto_mode(Some(Off), Some(On)), Off);
        assert_eq!(effective_auto_mode(Some(On), Some(Off)), On);
        // Unset per-agent inherits the user-global default.
        assert_eq!(effective_auto_mode(None, Some(On)), On);
        assert_eq!(effective_auto_mode(None, Some(Off)), Off);
        // Nothing set anywhere → Off (today's behavior).
        assert_eq!(effective_auto_mode(None, None), Off);
    }
}
