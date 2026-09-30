# a-nudge-types-at-an-agents-terminal
# clank — `clank nudge` types a message straight into an agent's pane

## Why

`clank send` is the right mechanism and it has a failure mode it cannot report: it
writes a durable inbox row and trusts the agent's wake path to deliver it. When that
wake path is what is broken, the message lands on disk and nothing ever reads it. The
send succeeds. The agent goes quiet. Nobody finds out until someone looks.

This is not hypothetical — an orphaned Claude Code mod left a frostsnap agent erroring
at every turn end for hours, unable to receive anything, while its commit sat unreviewed.
A message would not have reached it. Typing at its terminal would have.

So: an escape hatch that shares nothing with the machinery it exists to bypass. When the
delivery path is suspect, the human needs a way to put words in front of an agent that
does not consult a hook, an inbox, a session binding or a wait.

## What it is

`clank nudge` addressed exactly like `clank send`, so the two are interchangeable at the
call site and one can be tried after the other:

```
clank nudge [--repo P] (--to <LABEL> | --master) (-m <TEXT> | -F <FILE>)
```

It finds that repo+label's live pane, types the text and presses Enter. Nothing is
written to disk.

## Where the pane actually is

The first draft said "resolve the repo's session with `repo_session_name`". That is
wrong, and the function's own doc says so (codex on 898a63c):

> The session `clank open` names for `repo` — **the convention, not necessarily where the
> repo's panes ARE**: `open_one` adds a tab to whatever session the caller is in. A caller
> that needs the truth verifies the listing.

So a repo opened from inside an existing zellij session has its agent panes in THAT
session, under whatever name it has. A nudge that trusts the convention reports "no
session for this repo" while the agent sits there live — the escape hatch failing in
exactly the confusing way it exists to prevent.

**Discovery is by listing, not by naming.** `panes_in_all_sessions(deadline)` returns
`SessionPane { session, pane }` across every live session, and `session_holder.rs:78`
already uses it for the sibling question of who holds a session. Nudge matches on the
exact `agent_start_command(label, repo)` and on `runs()`, which excludes a corpse.

It costs one `list-panes` per live session, run concurrently, with a deadline —
`session_holder` uses 1 s, on the measurement that healthy servers answer in well under
0.2 s and only stale ones take seconds.

**The scan currently cannot report what it missed, and must learn to.** It returns a bare
`Vec<SessionPane>` and drops a session silently in four places: a non-success exit, a
join/parse failure, a `try_wait` error, and the deadline kill at the end. Its docstring
accepts that — *"Dropping a session means its panes are not seen, which fails OPEN (no
holder found)"* — which is right for a launch and wrong here (codex on 7dc04df). So the
discovery returns the unanswered session names alongside the panes, and nudge fails CLOSED
on them: "no pane found, and N session(s) did not answer" is a different sentence from "no
pane found", because only one of them means the agent is not there. `session_holder` keeps
failing open, but by ignoring the field on purpose rather than by not being told.

**Multiplicity is refused, not resolved.** One repo+label can have a live pane in more
than one session (the repo opened twice). Typing into an arbitrary one is the silent
wrong-delivery this command exists to avoid, so: exactly one live match nudges; zero
reports which condition failed; more than one refuses and names the sessions, leaving the
human to pick.

## Nearly all of the rest already exists

In `crates/cli/src/cli/open_zellij.rs`:

- `panes_in_all_sessions(deadline)` / `SessionPane` — discovery across sessions.
- `snapshot_panes_in(session)` — `list-panes --json`, the cheap action the cost gate
  permits.
- `agent_pane_label(pane, repo_path)` / `agent_pane_table(panes, repo)` — pane → label,
  anchored on the exact `clank agent start <label> --repo <path>` command, and already
  proven to disambiguate the same label across repos.
- `say_argvs(pane, text)` → `write-chars -p <pane> -- <text>` then `write -p <pane> 13`.
  Text follows `--`, so a message starting with `-` is a message. Enter is byte 13, not
  a newline, because a chat prompt treats a pasted newline differently from Return.
- `say_to_pane` / `say_to_codex_pane` — the latter waits past the paste-burst window and
  sends kitty-protocol Enter (CSI 13;1:1u), which Codex needs and others do not.

`Panes::say` at `crates/cli/src/cli/web/mod.rs:114-139` composes all of it already,
including the tool lookup. The work is to make that reachable rather than to write it.
If `the-zellij-seam-is-a-crate-not-a-grep` has landed, this plan calls the extracted
function. If it has not, this plan must NOT copy the body into a second place — it lifts
it to a shared home first, even a small one.

## The hazard that makes this more than a wrapper

A nudge to a dead pane succeeds. `find_pane_by_command` matches on the command string
and deliberately ignores exit state, because a crashed agent's pane is still that
agent's pane and the reconciler must not replace it. `runs()` is the predicate that
excludes a corpse, and it is used elsewhere. So the naive composition — find the pane,
write to it — types into a pane whose process ended, zellij accepts it, and the exit
code is 0. The one command whose whole purpose is "the normal path may be lying to you"
would then lie in exactly the same way.

Every way this can be unavailable needs its own diagnosis and a non-zero exit:
no zellij binary; no session for that repo; a session with no pane for that label; a
pane whose process has exited. "Nudged" must mean the keys went somewhere alive.

## Questions

1. **Does a nudge use the `codex_chat` app-server fast path? NO — pane only.** Two
   answers were tried before this one. The fast path is not reached by a thread id the
   pane reports; `web/mod.rs:126` reads the PERSISTED binding —
   `load_agent_config(repo, label).session.id` — and sends to that thread. A stale binding
   that still names a loaded thread therefore delivers to the WRONG session and returns
   success (codex on 7dc04df).

   A stale binding is one of the things `clank send` fails on, and this plan opens by
   promising a path "that does not consult a hook, an inbox, a session binding or a wait".
   Using the fast path contradicts that in the same document.

   **This does not mean a second copy of the tool-aware `say`.** It means the shared thing
   is one layer lower than the first draft assumed — a pane-only, tool-aware submit that
   owns the Codex kitty-Enter distinction and nothing else:

       say_to_pane_for_tool(session, pane, tool, text)   ← nudge calls this
       Panes::say = try codex_chat with the binding, else say_to_pane_for_tool

   The knowledge that Codex needs `CSI 13;1:1u` after the paste-burst window lives in one
   place; what differs is only whether a caller is willing to trust a stored binding. The
   web bridge is (it is driving a pane it just listed for a live UI); the escape hatch is
   not.
2. **Does a nudge leave any trace?** It is deliberately not durable, but "I nudged it at
   10:04" is the kind of thing the timeline exists for, and a nudge that vanishes is
   indistinguishable from one that was never sent. A log row is not an inbox row.
3. **Should `send` fall back to `nudge`, or suggest it?** Probably not automatic — a
   silent escalation to typing at a terminal is surprising — but `send` could name it
   when the recipient has a live pane and a stale-looking binding.
4. **Is it in the TUI too?** The agent pages already drive panes; a nudge key there is
   cheap and is where a human actually notices an agent has gone quiet.
5. **What does the ownership gate say?** `say_argvs`' own doc claims typing is not a
   pane MUTATION in that gate's sense, because it leaves the pane's place in the layout
   to the reconciler. If that holds, a CLI caller may reach the helper directly; if it
   does not, the nudge goes through `PaneIo` like every other pane operation. Check
   rather than assume — the gate scans source, and a new caller is exactly what it
   watches for.
6. **Multi-line and shell safety.** `-F -` reads stdin like `send`; a multi-line body is
   a sequence of `write-chars` with real newlines, and the submit is still one Enter.
   What a newline does to a given agent's prompt is tool-specific and needs deciding.

## Answered, 2–6

2. **Does a nudge leave any trace? No durable one, deliberately.** It prints where it
   typed — `nudged `rev` in /w/proj (session clank-proj, pane terminal_3)` — which is what
   a person needs to know it reached the agent they meant. A timeline row was considered
   and not built: the value of this command is that it touches none of the machinery that
   may be broken, and a log write is one more thing that can fail while the keystrokes
   already landed. The counter-argument stands and is a follow-up, not a defect.

3. **Should `send` suggest `nudge`?** Not built. It would mean `send` learning to inspect
   panes, which couples the durable path to the one that exists because the durable path
   can fail. A follow-up if the pairing proves awkward in use.

4. **Is it in the TUI too?** Not built. The agent pages already drive panes, so the key is
   cheap — but `locate`'s refusals are the whole design here, and a TUI affordance that
   silently picked for the user would undo them. Worth doing only with the same refusals
   rendered.

5. **What does the ownership gate say? It permits this**, as `say_argvs`' own doc claimed:
   the gate's `PANE_MUTATIONS` are five pane PLACEMENT operations (`focus_pane`,
   `add_reviewer_pane`, `stack_reviewer_panes`, `remove_reviewer_panes`,
   `relocate_for_promote`), and typing leaves a pane's place in the layout to the
   reconciler. Verified rather than assumed: `zellij_ownership_boundary` and
   `zellij_cost_boundary` both pass with the new caller, which is what the check was for.

6. **Multi-line and shell safety.** `-F -` reads stdin as `send` does, and `-m -` does too
   since `send-m-dash` — which is how a heredoc body reaches this command at all. A
   multi-line body goes through `write-chars` with its real newlines and one Enter to
   submit; the text follows `--`, so a message starting with `-` is a message. What a
   newline does inside a given agent's prompt is that agent's business, which is why the
   submit is a separate keypress rather than a trailing newline.

## Acceptance

- `clank nudge --to <label> -m "..."` puts the text in that agent's pane and submits it.
- The address grammar matches `clank send` — `--repo`, `--to`, `--master`, `-m`, `-F` —
  so the escape hatch is a one-word edit away from the command that failed.
- **A nudge to an exited pane fails**, with a message naming the pane and the exit, and a
  non-zero status. Tested against a pane fixture that reports `exited: true`.
- **A repo opened inside another session is found**, with a fixture whose agent pane lives
  in a session NOT named by the convention. This is the case the first draft would have
  failed.
- **Two live panes for one repo+label refuse**, naming both sessions rather than picking.
- Each unavailability is distinguishable: no zellij, no live pane anywhere, dead pane, a
  session that did not answer within the deadline (which must not read as "not found").
- **A stale session binding cannot misdirect a nudge**, tested: a config naming a Codex
  thread that is not this pane's must not change where the keys land. The nudge path never
  reads `load_agent_config`.
- The Codex Enter distinction lives in exactly one place — the pane-only submit that both
  the web bridge and nudge call — and this plan does not create a second copy of it.
- No test starts a zellij server; pane resolution and argv construction are pure over a
  `Vec<ZellijPane>` fixture, and the subprocess stays behind a seam.

## Out of scope

Extracting the zellij crate (`the-zellij-seam-is-a-crate-not-a-grep`). Changing what
`clank send` does. Reading a pane back to confirm the agent reacted — a nudge presses
keys; whether the agent then works is the agent's business.
