# an-opencode-agent-binds-a-session-opencode-has
# An OpenCode agent binds a session OpenCode has

## Problem

In secp256kfun, the reviewer `kimi` (OpenCode 2.1.4, `--standalone`) has
owed the review of 60b4eee (plan `frostsnap-impact`) for 91 days. Status
says `UNDELIVERED 91d · not nudged: its record cannot be read`. It cannot
recover by restarting, because clank binds it to a session OpenCode never
creates.

What happened on 2026-11-25 at 04:21 UTC, when `clank agent start kimi`
ran:

- `reserve_opencode_session` (`agent_store.rs`) minted a new id,
  `ses_def0ffe86ffeKnEmiwiP1mYVmL`, and recorded it as kimi's binding.
- `compose_unbound_launch` (`agent.rs`) launched
  `opencode --standalone --session ses_def0…`. Its doc says "`--session`
  creates a session that does not exist yet".
- OpenCode's log shows `GET /api/session/ses_def0… → 404`.
- No session row was created. `session_v2` in `opencode.db` holds nothing
  newer than 2026-11-05.
- The clank plugin loaded (`loading plugin …/plugin/clank.js`), but it
  logged nothing for any session.
- `.clank/agents/kimi/turns.jsonl` was never written. So recovery cannot
  read the record, and the `unknown · not nudged` follows from that.

The binding names a session that does not exist. Every later
`agent start` resumes the same dead id, since an OpenCode binding is
returned as-is (`reserve_opencode_locked`, `Some(s) if s.tool ==
OpenCode`). Also, there is no command that drops it.

`clank doctor` reports the dead session, but gives the wrong cause:
"OpenCode most likely did not load clank's plugin". In this case the
plugin did load.

## Model

A binding is a claim that the harness holds that session. For OpenCode,
clank makes that claim before launch, so the claim is only true once
OpenCode has the session. The start must make it true, and must also
recover when it turns out to be false. Recording an id and hoping OpenCode
adopts it is not enough.

## Scope

1. **Find what OpenCode 2.1.4 actually does with `--session <unknown id>`,
   and record it.** Inspect the shipped behaviour (the binary or the
   source at that tag) and record a fixture. The 404 above is evidence,
   not a full answer: it may open a transient session, show an error, or
   do something else. Record the finding in the doc of whichever launch
   path replaces the false premise.
2. **Bind a session OpenCode has.** Choose a mechanism that OpenCode 2.1.4
   supports and that keeps the reason for binding before launch (the model
   never spends a turn binding itself). Options to evaluate, in the plan
   or in its first commit:
   - creating the session through OpenCode's own API, then launching
     `--session` on it;
   - launching fresh and letting the plugin report the session it is
     actually in, which clank then binds;
   - another route the investigation shows is better.

   Whichever is chosen, the binding is written only for a session OpenCode
   acknowledges. A launch that ends without one leaves no binding that
   names a missing session.
3. **Recover from a dead OpenCode binding when starting.** If the bound
   session is not in OpenCode's store, `agent start` stops resuming it. It
   binds a session OpenCode has, by the mechanism from 2, and says so on
   stderr, as the Claude/Codex fork path already does with "forked session
   is gone". The existence check reads the same store `clank doctor`
   already reads. Share one function between doctor and start; don't
   write a second lookup.
4. **Fix doctor's diagnosis.** When the store has no such session, the
   check says the binding names a session OpenCode does not have, and that
   `clank agent start <label>` replaces it. It does not blame plugin
   loading. The plugin hint stays only where it fits: when the session
   exists but no turn of it was reported.

## Acceptance

- One test drives a start whose bound OpenCode session is missing from
  the store. It asserts that the new binding names a session the
  (fixture) store has, and that the old id is gone. The mutant is the
  current body: pasting `Some(s) if s.tool == OpenCode => Ok(s.clone())`
  back must fail the test.
- One test asserts that a start which binds a fresh session, by the chosen
  mechanism, records only an id OpenCode reports as existing. The fixture
  is built from the recorded 2.1.4 behaviour from 1, not from an assumed
  one.
- Doctor's message for a missing session no longer mentions plugin
  loading, and a test pins that.
- The `compose_unbound_launch` doc no longer claims `--session` creates a
  session, unless 1 shows that it does under conditions the launch
  meets. If so, the plan names those conditions and why today's launch
  missed them.
- README and the clank skills: sweep for the "bound before launch" and
  `--session` claims and correct whatever goes stale.

## Out of scope

- The Claude transcript-path bug in worktrees (`claude_derived` only
  replaces `/`). That is a separate plan.
- Rebinding kimi in secp256kfun itself. That is an operational step for
  after this lands: run `clank agent start kimi`, or `clank as kimi` from
  inside its session.
