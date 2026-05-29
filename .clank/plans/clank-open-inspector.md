# clank-open-inspector (exploratory)

A code editor wants to ask clank: "I want to open `<path>`. What's
there, and what should I do to get a working clank session?" — and
get back enough structured data to drive its own UI. Today the
editor has to do all that probing itself; clank already knows the
shape of an initialized repo, so it can answer this directly.

This plan is **exploratory**: scope the command, the response
shape, and the open questions. Implementation can be split out
into one or more follow-ups once the model is settled.

## Goal

A single command, e.g. `clank open <path> [--json]`, that
classifies `<path>` and returns a recommendation. No mutation —
the editor decides whether to act on the recommendation.

## Path handling (the not-just-`.git/` problem)

Editors commonly hand us a *subdirectory* of a repo, or a path
inside a linked worktree (whose `.git` is a file pointing at a
separate `gitdir`). Looking for a literal `.git/` directly under
`<path>` would misclassify both cases as "not a git repo."

Inspector contract:

- Canonicalize `<path>` with `dunce::canonicalize` (matches
  `crates/cli/src/cli/mod.rs:resolve_repo`).
- Probe git with `git -C <canonical> rev-parse --show-toplevel`
  AND `--git-dir`. Success on both → `repo_root` =
  `--show-toplevel`, `git_dir` = `--git-dir` (these differ for
  linked worktrees).
- Classification keys off `repo_root`, not off `<path>` itself.
  In particular, `.clank/config.json` is looked up at
  `repo_root/.clank/config.json`.
- Both `opened_path` and `repo_root` are returned to the editor
  so it knows the original input and the canonical project root.
- If `<path>` exists but git rev-parse fails (no repo found
  walking up), we fall through to `DirectoryNotGit`.

## States to distinguish

In rough order of "less set up" → "more set up":

1. **`PathMissing`** — `<path>` does not exist.
   Recommended actions: `mkdir`, `git init`, `clank init`,
   optionally a `clank as` for at least one agent.

2. **`PathNotDirectory`** — exists but is a file, symlink to one,
   etc. Error state; the editor probably shows a message and
   lets the user pick a different path.

3. **`EmptyDirectory`** — exists, no entries (or only ignored
   noise like `.DS_Store`), and git rev-parse finds no repo
   walking up.
   Recommended actions: `git init` + `clank init`.

4. **`DirectoryNotGit`** — non-empty, git rev-parse finds no
   repo walking up.
   Recommended actions: prompt user; either `git init` here or
   bail.

5. **`GitWithoutClank`** — git rev-parse succeeded
   (`repo_root` known, linked worktrees included), but
   `repo_root/.clank/config.json` is missing.
   Recommended actions: `clank init` at `repo_root`.

6. **`ClankInitialized`** — `repo_root/.clank/config.json`
   exists. Returns the existing master designation, list of
   agents (`repo_root/.clank/agents/<label>/`), any per-agent
   local config (`config.json` with last-bound session ID), and
   the current `clank status` summary so the editor can show
   "1 active plan, waiting on reviewer" without a second call.

(Sub-cases worth flagging on top of 5/6 rather than as separate
states: detached HEAD, dirty worktree, linked-worktree vs main
worktree, partially-initialized `.clank/` without `config.json`.
These ride as boolean / enum fields on the response, not as new
top-level `state` values.)

## Proposed response shape (JSON)

```json
{
  "opened_path": "/abs/path/maybe/sub/dir",
  "repo_root": "/abs/path",
  "state": "ClankInitialized",
  "git": {
    "is_repo": true,
    "git_dir": "/abs/path/.git",
    "is_linked_worktree": false,
    "head_branch": "main",
    "detached_head": false,
    "dirty": false
  },
  "clank": {
    "master": "claude",
    "agents": [
      {
        "label": "claude",
        "last_session_id": "742f6a04-...",
        "tool": "claude-code"
      },
      {
        "label": "codex",
        "last_session_id": "thr_abc...",
        "tool": "codex"
      }
    ],
    "active_plans": 1,
    "waiting_on": "reviewer"
  },
  "recommendations": [
    {
      "kind": "ResumeAgent",
      "label": "claude",
      "tool": "claude-code",
      "session_id": "742f6a04-...",
      "command_hint": "claude --resume 742f6a04-..."
    },
    {
      "kind": "ResumeAgent",
      "label": "codex",
      "tool": "codex",
      "session_id": "thr_abc...",
      "command_hint": "codex resume thr_abc..."
    }
  ]
}
```

For PathMissing / Empty / etc., `recommendations` contains
`InitDirectory`, `GitInit`, `ClankInit`, `BindAgent` entries in
order. The editor walks them. The `ClankInit` recommendation
carries the `repo_root` it should run against, so it's correct
even when the editor opened a subdirectory.

## Test coverage to bake in

When this lands, the test matrix has to include — not just the
six top-level states, but the path-handling cases:

- Opened path is the repo root → `opened_path == repo_root`.
- Opened path is a nested subdirectory of an initialized repo
  → `repo_root` correctly points up; classification is still
  `ClankInitialized`; `ClankInit` is NOT recommended.
- Opened path is inside a linked worktree (`.git` file, external
  `git_dir`) → `is_linked_worktree: true`, `repo_root` points at
  the linked worktree's toplevel.
- Opened path is inside a bare repo subdirectory (rare for
  editors but worth a defined behavior).
- Symlinked path resolves to the same identity as the canonical
  one (regression on `dunce::canonicalize`).

## Open questions (must research before designing the recs)

1. **Can Claude Code sessions be resumed by ID?**
   - `claude --resume <id>` / `claude -r <id>` — verify it works,
     and that the resumed session sees the same `cwd` /
     `CLAUDE_CODE_SESSION_ID`. If the ID changes on resume, the
     binding we stored is useless and we'd need a different
     handle (project name, transcript path, etc.).
2. **Can Codex sessions be resumed by thread ID?**
   - Codex uses `CODEX_THREAD_ID`. Is `codex resume <thread>` (or
     similar) a thing? If only the most-recent thread is
     resumable, we may only be able to recommend "open a new
     codex session bound to this label."
3. **What does the editor actually need to run?**
   - For each agent it's not enough to know "claude" — it needs
     the binary path / launch command (terminal? embedded? IDE
     extension protocol?). Treat that as the editor's problem;
     clank only returns `tool` + `session_id` + a `command_hint`
     string the editor may or may not use.
4. **Where do we read the "last session ID" from?**
   - `.clank/agents/<label>/config.json` (gitignored, per-agent
     local) — confirm it persists the session ID, and that
     "session-ness" matches whatever resume-handle Claude/Codex
     actually want.
5. **Per-machine sessions.** A repo cloned to a second machine
   will have no local agent state. The recommendation in that
   case is "bind a new session" — not "resume a stale ID from
   another machine."

Research outputs from these questions feed the eventual schema
for `recommendations[*]`. Stop here and don't implement until
that's nailed down.

## Non-goals (for now)

- The command does **not** mutate anything. No mkdir, no git
  init, no clank init. The editor chooses.
- No UI; this is API-shaped output only.
- No multi-repo / workspace concept; one `<path>` per call.
- No live-status streaming; one shot, returns and exits.

## Suggested follow-up plans (after research)

- `clank-open-inspector-impl` — implement the read-only inspector
  with the agreed response shape, covering states 1-6 above.
- `clank-open-bind-flow` — once Claude/Codex resume behavior is
  known, define how the editor goes from "fresh repo" to "bound
  agents ready to take work."
