# clank-open-inspector-impl

Implement the read-only `clank open <path> [--json]` command
scoped in `.clank/finished/clank-open-inspector.md`. No mutation;
classify the path and return enough structured data for an editor
to drive its own bootstrap UI.

## Command surface

```
clank open <path> [--json]
```

- `<path>`: required. Editor's literal input — accepted as
  given, stored verbatim in `requested_path`.
- `--json`: emit the response as JSON. Without it, emit a
  human-readable rendering of the same data (state line +
  recommendations as a bulleted list).
- No `--repo` flag — the path under inspection IS the input.
- Exit codes: `0` always when the inspector ran successfully
  (even for `PathMissing`/error states). Bad CLI usage → 2.

## File layout

- New file `crates/cli/src/cli/open.rs` with:
  - `pub async fn run(args: OpenArgs) -> anyhow::Result<()>`
  - `OpenResponse` + nested types (`GitInfo`, `ClankInfo`,
    `AgentInfo`, `Recommendation` enum).
  - Serde derives on all of the above. snake_case field names,
    matching the rest of clank's JSON output style.
- Wire `Open(OpenArgs)` into `Commands` in `crates/cli/src/cli/mod.rs`.
- Dispatch in `crates/cli/src/main.rs`.

## Path handling (from scoping doc, restated)

1. Echo `requested_path` verbatim.
2. `std::fs::try_exists(requested_path)`:
   - `Ok(false)` → state = `PathMissing`. Lex-clean to absolute
     (`std::path::absolute` if available, else `std::env::current_dir().join(p)`
     plus manual `..`/`.` collapsing). NO canonicalize. Skip
     git/clank probing. Return.
   - `Err(_)` → bubble as a CLI error (permission denied, etc.).
   - `Ok(true)` → check metadata:
     - Not a directory → `PathNotDirectory`. Same lex-clean,
       same skip. Return.
     - Directory → `dunce::canonicalize` → `opened_path`. Continue.
3. Git probe: `git -C <opened_path> rev-parse --show-toplevel`
   and `--git-dir` in parallel. Either failing → no git.
   - Both succeed: `repo_root` = `--show-toplevel` (already
     absolute). `git_dir` = normalize via the same algorithm as
     `crates/cli/src/cli/status.rs:git_resolve_dir` — join
     relative output to `opened_path` if not absolute, then
     `dunce::canonicalize` with raw absolute as fallback.
   - `is_linked_worktree` = `git_dir.starts_with(repo_root)` is
     `false`. (Linked worktrees' gitdir lives under the main
     repo's `.git/worktrees/<name>/`, OUTSIDE the linked
     worktree's `repo_root`.)
4. Clank probe: `repo_root/.clank/config.json` exists →
   `ClankInitialized`, else `GitWithoutClank`.
5. Empty-vs-not-git tie-break for the no-git case: read
   `std::fs::read_dir` ignoring `.DS_Store`. Empty →
   `EmptyDirectory`, else `DirectoryNotGit`.

## Response shape

(Same as the scoping doc — restated here as the authoritative
schema for this implementation.)

```rust
#[derive(Serialize)]
struct OpenResponse {
    requested_path: String,
    opened_path: String,
    state: OpenState,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git: Option<GitInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    clank: Option<ClankInfo>,
    recommendations: Vec<Recommendation>,
    /// Non-fatal degradations (e.g. fold failed, JSONL probe
    /// errored). Empty when everything resolved cleanly.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    warnings: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum OpenState {
    PathMissing,
    PathNotDirectory,
    EmptyDirectory,
    DirectoryNotGit,
    GitWithoutClank,
    ClankInitialized,
}

#[derive(Serialize)]
struct GitInfo {
    is_repo: bool, // always true when GitInfo is present
    git_dir: String,
    is_linked_worktree: bool,
    head_branch: Option<String>, // None if detached
    detached_head: bool,
    dirty: bool,
}

#[derive(Serialize)]
struct ClankInfo {
    /// Labels of agents whose per-agent
    /// `.clank/agents/<label>/config.json` declares `role:
    /// "master"`. Per `crates/core/src/agent_config.rs`, master
    /// is a per-user role claim, not a repo-wide assertion —
    /// the list may be empty, single, or multi-element, and
    /// none of those are "wrong" states.
    master_agents: Vec<String>,
    agents: Vec<AgentInfo>,
    active_plans: usize,
    waiting_on: Option<String>, // short string like "reviewer"
}

#[derive(Serialize)]
struct AgentInfo {
    label: String,
    tool: Option<String>,        // "claude" | "codex" | None
    last_session_id: Option<String>,
    session_resumable: bool,     // true iff JSONL exists on disk
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Recommendation {
    InitDirectory { path: String },
    GitInit { cwd: String },
    ClankInit { cwd: String },
    BindAgent { label: Option<String>, tool: Option<String> },
    ResumeAgent {
        label: String,
        tool: String,
        session_id: String,
        command_hint: String,
    },
}
```

## Recommendation construction per state

- `PathMissing` → `[InitDirectory, GitInit, ClankInit, BindAgent]`.
- `PathNotDirectory` → `[]` (editor decides).
- `EmptyDirectory` → `[GitInit, ClankInit, BindAgent]`.
- `DirectoryNotGit` → `[GitInit, ClankInit, BindAgent]` (editor
  may want to prompt before `GitInit`).
- `GitWithoutClank` → `[ClankInit, BindAgent]`.
- `ClankInitialized`:
  - For each agent with `last_session_id` AND a matching JSONL
    on disk → emit `ResumeAgent` with `command_hint`:
    - `claude`: `claude --resume <UUID>`
    - `codex`: `codex resume <UUID>`
  - For each agent without a resumable session → emit
    `BindAgent { label: Some(label), tool: agent.tool }`.

## Session-file presence checks (from research)

The `session_resumable` boolean and the gating for `ResumeAgent`
both depend on the JSONL file existing locally.

- `claude` tool: `~/.claude/projects/<cwd-encoded>/<UUID>.jsonl`
  where `<cwd-encoded>` is `repo_root` with `/` → `-` (drop the
  leading separator's hyphen if it ends up doubled — confirm
  with a real existing session in the test).
- `codex` tool: glob
  `~/.codex/sessions/*/*/*/rollout-*-<UUID>.jsonl`. Use a single
  recursive walk capped at `~/.codex/sessions/` and break on the
  first match. Absence → `session_resumable: false`.

Both checks are best-effort — failures (HOME unset, dir
unreadable) downgrade to `session_resumable: false` rather than
erroring the whole inspector.

## ClankInfo population

Reuse existing machinery — don't reimplement:

- `agents` from `.clank/agents/<label>/config.json` reads
  (`AgentConfig` in `crates/core/src/agent_config.rs`).
  Extract `session.id` and `session.tool`.
- `master_agents` = labels whose `AgentConfig::role` is
  `Role::Master`. Same scan as `agents`; just filter.
- `active_plans` and `waiting_on` from a `rebuild_repo` +
  `derive_status` call on `repo_root`. Reuse the same path
  `clank status` uses (`StatusSnapshot::build_async`) and pull
  the summary fields. If `rebuild_repo` fails (broken repo,
  partial init, etc.), set `active_plans: 0` /
  `waiting_on: None`, do NOT downgrade the state (it's still
  `ClankInitialized` if `config.json` exists), and push a
  message like `"fold failed: <err>"` onto
  `OpenResponse::warnings`.

## Human-readable output (no `--json`)

Single block, no banners:

```
state:        ClankInitialized
opened_path:  /Users/me/code/myproj/sub
repo_root:    /Users/me/code/myproj
git:          branch=main, clean
clank:        master=[claude], 2 agents, 1 active plan, waiting on reviewer

recommendations:
  - resume agent `claude` (claude --resume 742f6a04-...)
  - resume agent `codex`  (codex resume 019e54b7-...)
```

For non-`ClankInitialized` states, the same shape with
`repo_root: -` / `clank: -` lines. If `warnings` is non-empty,
append a `warnings:` block after `recommendations:` with one
bullet per entry.

## Tests

Integration tests live in `crates/cli/tests/open_integration.rs`.

State-coverage tests (spawn the binary, parse `--json` output):

- `path_missing_returns_clean_response` — point at
  `/tmp/<random>` that doesn't exist; assert state, no
  `repo_root`/`git`/`clank`, recommendations start with
  `init_directory`.
- `path_not_directory` — point at a real file; assert
  `PathNotDirectory`, empty recommendations.
- `empty_directory` — fresh empty dir; assert state +
  `git_init` first recommendation.
- `directory_not_git` — non-empty dir, no `.git`; same as
  above.
- `git_without_clank` — `git init` only; assert state, `git`
  populated with branch/clean, recommendations =
  `[clank_init, bind_agent]`.
- `clank_initialized_with_agents` — full repo with two agents
  in `.clank/agents/`, one with `role: "master"`, one with
  `role: "reviewers"`. Assert `master_agents == ["<that-label>"]`,
  both agents listed, and resume recommendations emitted.
- `clank_initialized_zero_or_many_master_agents` — first
  variant: both agents `role: "reviewers"` → `master_agents`
  is empty (not an error). Second variant: both agents
  `role: "master"` → both labels appear in `master_agents`.
- `clank_initialized_subdir` — open a subdirectory of an
  initialized repo; assert `repo_root` is the toplevel,
  `opened_path` is the subdir, state is still
  `ClankInitialized`, no `ClankInit` recommendation.
- `clank_initialized_linked_worktree` — `git worktree add`,
  initialize clank in the linked worktree, open inside it;
  assert `is_linked_worktree: true`, `git_dir` points outside
  `repo_root`.

Path-handling regression tests:

- `git_dir_normalized_from_repo_root` — open the repo root
  itself; assert response `git.git_dir` is an absolute canonical
  path ending in `/.git`, NOT the literal string `.git`.
- `symlink_canonicalization` — symlink to a clank-initialized
  repo; assert `opened_path` resolves to the canonical target.

Session-resumability tests:

- `agent_with_existing_claude_session_recommends_resume` —
  fake `~/.claude/projects/<encoded>/<UUID>.jsonl` (using a
  temp `HOME`), agent config points at that UUID, assert
  `ResumeAgent` is in recommendations.
- `agent_with_stale_session_id_falls_back_to_bind` — agent
  config has a UUID with no JSONL on disk; assert
  `session_resumable: false` and recommendation is
  `BindAgent`, not `ResumeAgent`.

Warning-surface tests:

- `fold_failure_pushes_warning_but_keeps_clank_initialized` —
  initialize clank, then corrupt `.git/HEAD` (or commit garbage
  that breaks rebuild). Assert state is still
  `ClankInitialized`, `clank.active_plans == 0`,
  `clank.waiting_on == None`, and `warnings` contains one entry
  mentioning the fold failure. JSON output omits `warnings` when
  empty (serde `skip_serializing_if`) — pin that too.

## Out of scope

- Pre-allocating session UUIDs via `claude --session-id`. Editor
  can choose to do that; clank doesn't need to.
- Any mutation: `clank open` never creates dirs, runs `git init`,
  runs `clank init`, or binds agents. Recommendations are
  advice, not actions.
- Cross-machine session resume. Sessions live on the local
  machine; the inspector is a local view.
- A `clank bootstrap` command that ACTS on the recommendations.
  That's a separate plan (the not-being-written
  `clank-open-bind-flow`).
- Multi-path / workspace inputs. One `<path>` per call.
