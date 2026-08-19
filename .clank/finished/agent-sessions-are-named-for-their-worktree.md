# agent-sessions-are-named-for-their-worktree

## Problem

Every clank-started agent session ends up named after the first thing
clank told it to do. The launch prompts are, verbatim:

    Run `clank as kimi` to bind this session.

    You are `kimi` in clone `sec-review-0bbc18be` of
    /Users/llfourn/src/frostsnap (branch `sec-review-0bbc18be` off
    0bbc18be…), session forked for: parallel work. Run `clank as kimi`
    to bind this forked session. Queued plans seeded for this fork, in
    order: security-review.

Tools derive a session title from the opening exchange, so a session
list reads as a column of near-identical "bind this session" entries.
With dozens of live sessions across worktrees, nothing distinguishes
them — the one piece of information that would (WHICH worktree) is the
one not in the title.

## What each tool supports — researched, not assumed

Verified against the versions installed on this machine:

| tool | mechanism | usable by clank's launch? |
|---|---|---|
| `claude` | `-n, --name <name>` — "Set a display name for this session (shown in the prompt box, /resume …)" | **YES** — a top-level flag, so it applies to the interactive launch |
| `opencode` | `--title <title>` — "title for the session (uses truncated prompt if no value provided)" | **NO** — exists only on `opencode run` (non-interactive). The interactive top level takes `--prompt`, `--agent`, … and no title |
| `codex` | sessions HAVE names (`resume` takes "session id or session name"; `archive`/`delete` likewise) | **NO** — no launch-time flag found among its global options |

opencode's own flag documentation states the failure mode outright:
the title "uses truncated prompt if no value provided". That is
exactly what is being observed.

## Approach

1. **Where a flag exists, clank sets it.** `claude` gets `-n <name>`
   in its composed launch. Deterministic, no agent cooperation, and
   nothing to ignore.

2. **Where none exists, clank still drives it — through the prompt it
   already composes.** For `codex` and `opencode` the title is derived
   from the opening message, and clank writes that message
   (`bootstrap_bind_prompt`, `compose_fork_launch`). Leading with the
   name is therefore a CLANK-side change, not an instruction the agent
   must choose to follow:

       recovery-scan · kimi — run `clank as kimi` to bind this session.

   This corrects the earlier draft of this plan, which proposed
   "instructing the agent to name itself" as the fallback. That is the
   weakest available option and close to the cause: today's useless
   titles come from the agent-facing text clank already sends, so the
   fix is to change that text, not to add a request on top of it.

3. **Forks name themselves after the fork.** `compose_fork_launch`
   already knows the fork name and puts it in the prompt; that is the
   name the session should carry.

4. **Do not paper over the derived case.** Where the title is derived
   rather than set, say so — the tool decides the final string, and a
   plan that claims otherwise will be wrong the next time a summariser
   changes.

## Settled decisions

- **The name is `<directory> · <label>`.** The directory is the
  worktree, clone, or plain checkout the agent runs in — the same
  string the zellij tab carries, and just `path.file_name()`, so a
  main checkout needs no special case.

  The label is a deliberate departure from the literal ask ("the name
  of the worktree"). Several agents share one worktree, so the
  directory alone repeats across the master and every reviewer there,
  and a session list of identical entries is the complaint being
  fixed. Leading with the place keeps the ask; the label makes it
  answer the question.

- **The name is set on EVERY launch, including resume.** Nothing can
  read a session's current name back, so "set it once" is not
  implementable as stated — and its effect would be to leave every
  session created before this change permanently mis-titled, which is
  the reported problem rather than a hypothetical. The cost is
  accepted: a name set by hand inside the tool is overwritten on the
  next clank-driven launch.

## Open question, for the fork case

Recorded rather than guessed: does a `codex fork <src>` session derive
its title from the NEW fork prompt, or inherit the source transcript's?
If it inherits, leading the prompt does nothing for codex forks and
this fix covers fresh launches only. A throwaway fork answers it in one
step. It does not gate the rest — the plan already declines to
overclaim the derived case — but the answer belongs in the tree before
anyone relies on fork titles.

## Required tests

- `claude`'s composed argv carries `-n <name>` — asserted on the argv
  clank builds, which `clank agent start --print` already exposes.
- For the derived tools, the composed PROMPT leads with the name, so
  the tool's own summary starts there.
- A fork's prompt leads with the fork name.
- The main-checkout name is whatever the plan decided, asserted
  explicitly rather than falling out of a path split.
- No test spawns a real agent binary.

## Out of scope

- Renaming sessions that already exist.
- zellij tab or pane titles, which are already named for the worktree.
