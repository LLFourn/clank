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

## States to distinguish

In rough order of "less set up" → "more set up":

1. **`PathMissing`** — `<path>` does not exist.
   Recommended actions: `mkdir`, `git init`, `clank init`,
   optionally a `clank as` for at least one agent.

2. **`PathNotDirectory`** — exists but is a file, symlink to one,
   etc. Error state; the editor probably shows a message and
   lets the user pick a different path.

3. **`EmptyDirectory`** — exists, no entries (or only ignored
   noise like `.DS_Store`).
   Recommended actions: `git init` + `clank init`.

4. **`DirectoryNotGit`** — non-empty, no `.git/`.
   Recommended actions: prompt user; either `git init` here or
   bail.

5. **`GitWithoutClank`** — `.git/` exists, no `.clank/`.
   Recommended actions: `clank init`.

6. **`ClankInitialized`** — `.clank/config.json` exists.
   Returns the existing master designation, list of agents
   (`.clank/agents/<label>/`), any per-agent local config
   (`config.json` with last-bound session ID), and the current
   `clank status` summary so the editor can show "1 active plan,
   waiting on reviewer" without a second call.

(There are sub-cases worth flagging — e.g. "clank init started
but `.clank/config.json` missing", "git repo with detached HEAD"
— but the editor probably treats those as "5/6 with warnings"
rather than separate top-level states.)

## Proposed response shape (JSON)

```json
{
  "path": "/abs/path",
  "state": "ClankInitialized",
  "git": { "is_repo": true, "head_branch": "main", "dirty": false },
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
order. The editor walks them.

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
