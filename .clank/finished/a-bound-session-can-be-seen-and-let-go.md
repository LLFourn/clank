# a-bound-session-can-be-seen-and-let-go
# A bound session can be seen, and let go

## Why

> "so right now in ~/docs/trading grok can't find its session to
> restore. On the agent menu can in clank status --tui can we show the
> session id and have an option to clear it."

`~/docs/trading/.clank/agents/grok/config.json` binds grok to
`019f5eed-8d98-7bc3-8c8b-2f84f27e35a3`, written 2026-08-01 on the
machine this one was restored from. `~/.grok/sessions/` has no such
session now. Every pane runs `clank agent start`, and its resume path
(`agent.rs:258`, `Holder::Nobody` → `compose_launch`) hands the bound id
to `grok --resume` without asking whether grok still has it, so grok
cannot find the session and the agent never comes back.

The agent page does not help. Its one content line says
`session   bound` (`render.rs`, `render_agent_detail`) for ANY binding
— a binding to a session that no longer exists reads exactly like a
healthy one. The id itself is only on the `i` document. And nothing
in the TUI, or the CLI, lets a human drop a binding: the only release
is `agent_store::release_binding`, used when a paneless session gives
up its label.

## The model

**A binding is a claim that a label's conversation lives at session S
in its tool's store.** Two facts about it matter to the person looking
at the page: which S, and whether the tool still has S. The second
already has one answer, `session_probe::session_jsonl_exists` — the
probe `clank fork` uses for exactly this — which returns
`Some(true)`, `Some(false)` or `None` (the store cannot be probed:
opencode). `None` is never shown as gone.

**Letting go of a binding is `release_binding`**, not a new write.
It compares and clears under the label's binding lock, and mints a
wait generation so a wait parked under the old identity dies with it.
The comparison is against the binding captured in the keystroke
that opens the confirm (`ClearTarget`), held by the loop beside the
confirm because a `Copy` `Mode` cannot carry it, so a binding that
changed after the confirm opened (the agent rebinding itself) is not
the one cleared. Once released, the label is unbound,
and the next `clank agent start` takes the bootstrap path: a fresh
session that binds itself.

**Releasing forgets; it does not disarm.** It revokes the wait parked
NOW, but a launched agent carries `CLANK_AGENT`, and identity
resolution returns that label before it consults any binding
(`agent_env.rs`, `resolve_identity_in` / `resolve_identity_for_hook_in`).
A process still running under the label resolves it again at its
next Stop hook or `clank wait` and is handed work; its SessionStart
can bind it again (codex on a8d45da). Clearing is for a binding whose
session is gone or whose pane is closed, and the page says so rather
than promising more.

**The probe is asked when the page opens, not on every refresh.**
It walks the tool's store — for codex, every rollout file when the id
is missing — and the snapshot is rebuilt on every repo change. The
answer is carried with the open page, and asked again after a clear.

## Deliverables

1. **The page names the session.** The content line becomes
   `session   <full id>` followed by its state: `✓` when the tool has
   it, `✗ gone — <tool> has no such session` when it does not, and no
   mark when the store cannot be probed. An unbound agent still reads
   `✗ unbound`. The id is shown whole: it is the thing you compare
   against the tool's store.
2. **A `clear session` action** on the agent page for every role,
   offered only while a session is bound, on a free key (`u`), styled
   as danger. It opens a confirm that names the id and says what
   clearing does: the saved session is forgotten and its parked wait
   ends, so the label's next start is a fresh session. Unless the
   agent's pane is known to be gone (`Presence::Missing`), it also says
   what clearing does NOT do: it does not stop a session that is still
   running — close the agent's pane before starting it fresh.
3. **Confirming releases the binding the page showed.**
   `release_binding(repo, label, <that id>)`:
   - `true` → the row becomes unbound, the page stays open, and
     `reopen pane` appears if the pane is gone;
   - `false` → the binding changed since the page showed it; nothing
     is cleared, and the page says so;
   - `Err` → the failure overlay, as promote's failure does.
4. **The probe runs once per page open**, and again after a clear —
   never in the snapshot builder. Its answer is kept with the tool and
   session id it was asked about — the BOUND session's tool, which
   wins over a roster entry that may have changed — and is shown only
   while the row is bound to that same session. A refreshed binding
   never inherits another session's `✓` or `gone`.

## Tests

- **Render:** a bound session the tool has → the line carries the full
  id and `✓`; one it lacks → the id and `gone`; an unprobeable store →
  the id and no mark; unbound → `✗ unbound`.
- **Actions:** `clear session` is on the page iff a session is bound,
  for a master and for a reviewer.
- **Confirm wording:** with the pane `Missing` the confirm names the id
  and the fresh start; with it `Live` or `Unknown` it also says a
  running session is not stopped and to close its pane first. It never
  says the running session stops receiving work.
- **The observation belongs to its session:** probe `gone` for session
  A, then rebind the row to session B; the page shows B with no mark
  (or B's own answer), never A's `gone`.
- **Clear:** on a repo fixture with a bound agent, confirming removes
  the session from `.clank/agents/<label>/config.json`, bumps
  `wait.gen`, and leaves the page showing the agent unbound.
- **Compare-and-clear:** the binding is rewritten to another id after
  the page opens; confirming clears nothing, the config keeps the new
  id, and the page says the binding changed. Mutation check: clear
  unconditionally and this fails.
- **Probe placement:** the probe is injected where the page opens, and
  a snapshot rebuild with the page open does not call it.

Run `cargo test -p clank --lib cli::status_tui`, then the whole
`-p clank --lib` suite.

## Out of scope

- `clank agent start` noticing a gone binding by itself and launching
  fresh, the way `clank fork` already does. You asked for a manual
  control; this makes a dead binding visible and lets it be dropped.
- A CLI command to clear a binding.
- Recovering grok's sessions from the old machine.
