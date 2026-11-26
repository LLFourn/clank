# clank-finds-a-claude-transcript-in-any-directory
# Clank finds a Claude transcript in any directory

## Problem

Clank cannot read the transcript of a Claude agent whose working
directory contains anything other than letters, digits, `-` and `/`. That
includes every agent in a `.clank/worktrees/<name>` checkout. Two cases
seen so far:

- `ruthless` in `ios-frostsnap/.clank/worktrees/s3-provisioning`
  (2026-11-21).
- `claude`, the master, in `ios-frostsnap/.clank/worktrees/device-experiment`
  (2026-11-26).

Both show up as `unknown · not nudged: its record cannot be read and
recovery.blind_nudge is off: its record could not be read`. Recovery
cannot see their turns, so a stalled or failed turn in these agents is
never noticed or recovered. When an agent's waiter is connected, its row
reads "waiting for work" instead, which hides the same blindness. In
device-experiment, `ruthless` is in that state.

The cause is `claude_derived` (`crates/cli/src/cli/web/transcript.rs`).
It builds Claude's project directory name by replacing only `/` and `\`
with `-`. Claude Code replaces every character that is not ASCII
alphanumeric:

- clank derives `~/.claude/projects/-Users-llfourn-src-ios-frostsnap-.clank-worktrees-device-experiment/`, which does not exist;
- Claude writes `~/.claude/projects/-Users-llfourn-src-ios-frostsnap--clank-worktrees-device-experiment/769074ef-….jsonl`.

The `_` in `frostsnap_nostr-taipei` gets the same treatment: Claude's
directory is `…-frostsnap-nostr-taipei`. No session's binding records a
transcript path for these agents, so the derived path is the only one
clank tries.

## Scope

1. `claude_derived` maps every non-ASCII-alphanumeric character to `-`,
   as Claude Code does. Confirm the rule against the Claude Code bundle
   (or its docs) before relying on it, and cite where it was confirmed in
   the function's doc. Also check whether Claude truncates or hashes long
   paths. If it does, implement that too, or document the limit.
2. Find every other place in clank that derives Claude's project
   directory from a path. Each one uses this single function; none
   carries a second copy of the rule.
3. Change the row so that an agent with no binding says `unbound` rather
   than `unknown`. Today the list row has no binding input
   (`agent_state.rs` `brief`/`said`), and only the agent page shows
   `✗ unbound`.

## Acceptance

- A test derives the path for `/x/repo/.clank/worktrees/w` and for a path
  containing `_`, and asserts Claude's actual directory names. The
  expected names come from directories Claude really wrote, not from the
  new rule restated. Reverting `claude_derived` to the `/`-only mapping
  makes the test fail.
- A recovery-level test: a bound Claude agent in a `.clank/worktrees/`
  cwd, with its transcript at Claude's real path, reads as working or
  idle, not `Unknown`.
- A roster row for an unbound agent reads `unbound`. A test pins it, and
  mutating the row back to the activity text makes that test fail.

## Out of scope

- `recovery.blind_nudge` defaults.
- Restarting the affected agents. Once this is installed, the monitor
  rereads on its next rescan or restart.
