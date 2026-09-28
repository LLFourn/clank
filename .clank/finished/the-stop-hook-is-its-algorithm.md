# the-stop-hook-is-its-algorithm

## Goal

The turn-end decision has one real algorithm. Write it down once, make the Stop path of `crates/cli/src/cli/stop_hook.rs` express exactly that algorithm, and delete everything on that path which is not it. The file's other adapter — SessionStart — is a second, smaller algorithm that is stated and preserved rather than purged.

The behaviour is close to right already. What is wrong is that `80f01612` ("Remove wait generation, claim, and turn-end state") removed the previous model's *code* but left its *vocabulary* and one of its *branches*. The file still names `hook_held`, `wait.lock` probing, claims, generations and `reachability` — none of which exist — and still routes through a two-armed choice whose arms are byte-for-byte the same call. The result is a file a reader cannot trust and an implementor cannot simplify, because the false parts are indistinguishable from the load-bearing ones.

## The algorithm: a Stop turn

Stated here so the code can be checked against it rather than described by it. Its inputs are the tool, whether this hook outlives the turn (`--loop asyncrewake`), the hook's stdin (session id, cwd, live background tasks), and the repo.

1. **Identify.** Repo from `--repo`, else stdin `cwd`. Label from the environment, else from the binding. If the binding no longer names this session and this tool, be Silent — someone else is this label now.
2. **Gather.** Reads only, no effects, all of them before anything decides: auto mode (absent means "unset, inherit the `~/.clank` default", not "off"), role, the attendance marker, and whether that marker's recorded process is provably still the process it recorded.
3. **Decide.** One pure function of that evidence, in this precedence, because the order *is* the content:
   - config unreadable → Diagnostic;
   - auto off → Silent (and no word about the roster: that is noise about work this agent will not do);
   - role unresolvable → Diagnostic (never a default role — laundering that failure arms a poll that can never return);
   - a proven attendance *and* a tool that delivers a completion wake → yield to the task, and hold the check-in timer if and only if this hook outlives the turn;
   - a proven attendance and a tool that does not → **decline** it and drive;
   - a passive adapter → Diagnostic, because nothing can drive it;
   - otherwise → drive.
4. **Take the marker** — unless the attendance was declined. Declining is not consuming: the work is still running and the record still describes it.
5. **Run the one effect the decision names**: the in-hook long poll, the check-in timer, or nothing.
6. **Re-check the binding before emitting.** A park can outlive the incarnation that armed it.
7. **Render** to the tool's wire shape and exit. **Exit 2 is Claude's continuation channel and nothing else**: a `Continue` for Claude exits `CLAUDE_CONTINUATION_EXIT` (2), which is *how* work reaches the session. Every other outcome — Silent, Diagnostic, Codex's block JSON, OpenCode's renew wire — exits `HOOK_OK_EXIT` (0). The invariant is about *failure*, not about zero: **no error path ever exits non-zero**, because a hook that fails would fail the agent, and that is a clank bug.

Its whole state on disk is three files under `.clank/agents/<label>/`, and there must be no fourth: `attending` (the agent's intent for ONE yield, written by `clank run` / `clank attending`, consumed here), `attended` (the projection `clank status` and the TUI read), and `attending.holder` (the identity of the armed check-in timer).

Every one of these steps is already in the file. The deliverable is that afterwards, nothing else of the *Stop* path is.

## The second algorithm in this file

`stop_hook.rs` hosts two hook adapters, not one. `clank stop-hook --tool claude --session-start` is a different hook event with a different contract, and it owns `SessionStartInput` (`stop_hook.rs:531`), `session_started` (`:557`), `run_session_start` (`:611`), `session_start_payload` (`:666`) and `peek_items` (`:706`). **None of these belong to the seven steps above, and none of them is dead.** Its algorithm is:

1. Read stdin. Anything unparseable exits 0 quietly.
2. **Bind or identify.** A session clank launched carries `CLANK_AGENT`, and that label is bound to the id on stdin — `startup` is the first bind, `clear` the rebind that keeps a session tracked across a new id, `resume`/`compact` an idempotent re-assertion — recording the transcript path when the tool sends one. A session clank did not launch is identified by its id, as `clank as` bound it.
3. If auto is off, stop.
4. Peek for pending work (`clank wait --peek --json`, side-effect-free: it fires no hooks).
5. If there is any, and the binding still holds, surface it on `additionalContext` so a resumed session catches up instead of waiting for its first turn-end.

**Fail-open at every step, by policy:** a session start must never be broken by clank state, so every error path exits 0 and the session starts regardless. Most return *quietly* and are meant to — unreadable or malformed stdin, repo resolution, a failed peek, an unresolvable role. The only paths that speak are the two bind-side failures in `session_started`: a roster tool that will not resolve, and a bind that did not land, where the diagnostic names what did not happen and the previous binding survives. Preserve that split exactly; a restatement must not turn the quiet paths into talking ones. Its disk writes are the binding (`config.json`) and its flock (`binding.lock`), and none of the Stop turn's three state paths.

Both algorithms are load-bearing. The purge criterion below applies to the Stop path; this path is preserved intact, and the only question the plan asks of it is whether it should live in its own module.

## Problem

Three findings, all the same shape: a removal pass took bodies and left the descriptions and one branch behind.

**1. `TurnRoute::Park` and `TurnRoute::Wait` are the same route.** `stop_hook.rs:229` and `stop_hook.rs:243` call the same function with the same eight arguments in the same order, and `asyncrewake_park` (`stop_hook.rs:679`) is a pure forwarding wrapper. Collapsing the variant, dropping `async_loop` from `route()` and deleting the wrapper passes all 87 `cli::stop_hook` tests. The single failure is `an_empty_wait_is_diagnostic_on_both_adapters`, whose last two lines are

```
assert_eq!(callers, 3, "expected the definition plus both production call sites");
assert!(production.contains("    compute_wait_outcome(\n        repo,"));
```

— a source scan that counts occurrences of a function name and pins the wrapper's body shape. It asserts the duplication is still present, and its premise ("both paths") is false because there is one path. Meanwhile `route()` has **no test at all**: the covering test, `the_route_is_a_table`, was deleted by `80f01612`, which is exactly why a branch that stopped branching went unnoticed.

The cost is not the wasted lines. It is that `--loop asyncrewake` means one thing — *this hook can outlive the turn, so it may hold the check-in timer* — and the code says it twice, once truthfully (`Attend` vs `AttendedYield`) and once vacuously (`Park` vs `Wait`). A reader cannot tell which spelling is the real one.

**2. Twenty-one doc-comment blocks describe items that no longer exist.** They compile because consecutive `///` runs separated by blank lines all attach to the next item, so `a_proven_attendance_yields_at_once_or_holds_the_timer` (`stop_hook.rs:3293`) currently carries six docstrings, five of them for deleted tests. Fourteen are in `stop_hook.rs` (lines 3238, 3250, 3259, 3272, 3280, 3356, 3363, 3413, 3429, 3443, 3454, 4126, 4244, 4417); the rest are in `agent_store.rs` (736, 752, 758, 763), `wait.rs` (2537), `doctor.rs` (1306) and `status_tui/render.rs` (3831) — every one of them a file `80f01612` gutted.

Three more are stranded onto the wrong item rather than onto nothing, which is worse because they read as current documentation:
- `stop_hook.rs:978` — `attended_path`'s doc sits on `read_attended_now`, so `attended_path` is undocumented while its twin `attending_path` is documented;
- `stop_hook.rs:1007` — the `Attending` struct's doc sits on `Correlation`, leaving `Attending` undocumented;
- `stop_hook.rs:1374` — `read_attending` carries `clear_attending`'s old doc, and that doc's central claim is now **false**: "Unconditional and early by design… the only deletion that closes them is one no branch can skip" describes a clear that `takes_marker()` made conditional.

**3. The projection is deleted and then rewritten.** `clear_attending` removes `attending` *and* `attended`; both attended branches (`stop_hook.rs:200`, `stop_hook.rs:206`) then immediately re-write `attended`. Two jobs in one function, half of it undone by half its callers. `attended` is a function of the turn's decision and should be written once, at the end, from that decision.

## Work

- Collapse the false route. `TurnRoute` becomes the two states that differ (drive the in-hook poll, or nothing can drive this adapter); `route` takes the tool alone; `asyncrewake_park` goes. `async_loop` keeps its one honest consumer, and the argv flag's meaning is stated where that consumer reads it.
- Replace the occurrence-counting assertion with a real test. `route` must be asserted directly, one row per tool, and the "empty successful wait is a Diagnostic" invariant must be asserted through the behaviour it names rather than through a substring count. A test that counts appearances of a function name is not a test of behaviour, and this one blocked the simplification it was standing next to.
- Make the attended projection a function of the decision, on the **bound** path only. A bound Stop turn updates `attended` exactly once from its decision — writing the record, or removing it when the turn decided there was nothing to attend — instead of deleting it and conditionally putting it back. The stale-binding return at `stop_hook.rs:130` precedes that write and must keep doing so: once the binding names another session, that state belongs to whoever holds the label now, and a departing hook must not touch it. `clear_attending` keeps one job.
- Purge the dead descriptions. Delete every doc block whose item is gone; reattach the three stranded ones to the items they describe and correct the one that is now false. This covers all five files, not only `stop_hook.rs`: the same removal pass produced all of them.
- Add a source gate so this cannot recur, as a module of the existing `crates/cli/tests/it/` gate family (`git_boundary`, `zellij_cost_boundary`, `no_json_literal_config_writes`): a run of `///` lines must be followed immediately — attributes allowed, blank lines not — by the item it documents. Prove it bites by inserting an orphan and watching the named test fail, and restore the tree. A hit that turns out to be a legitimate shape is grounds for a stated exception in the gate, never for weakening the rule.
- Make the module doc the algorithms. Both belong at the top of `stop_hook.rs` — the seven Stop steps and the five SessionStart ones — stated as the contracts they are, including the exit-code contract exactly as step 7 gives it. The Stop code below must read as its steps in that order, and anything on the Stop path that cannot be placed in a step is either state that needs naming or code that needs deleting. The SessionStart items are named as that second path and are not subject to that criterion.
- Audit the test-only surface. Each `#[cfg(test)]` item in the hook (`compute_outcome`, `compute_outcome_in`, `consume_attending`, `decision_from_label`, `EVIDENCED`, `TurnDecision::waker`) must be justified by a production path it lets a test drive. Delete the ones that only rename a call; keep and say why for the ones that let a test reach a real path.
- Decide where the SessionStart path lives. Preferred, if it leaves each file reading as one algorithm: move those five items into their own module, keeping `peek_items` with them and making it private — it is `pub(crate)` today with exactly one caller, `run_session_start`. A pure move; behaviour, argv and wire shape unchanged either way. If it does not pay, leave it in place with its own delimiting section comment, so the next reader is not left deciding whether it is dead.
- Consider extracting the wait wire and item rendering (`WaitEnvelope`/`WaitItem`/`parse_wait_*`/`discharge_of`/`render_wait_items*`, roughly 630 of the file's 2610 production lines) into its own module, so what remains is the turn algorithm and nothing else. Pure move, no behaviour change, and only worth doing if it leaves `stop_hook.rs` reading as the seven steps.
- **Do not "tidy" the holder.** `attend` writes `attending.holder` and deliberately never removes it (`stop_hook.rs:1614`): a compare-then-delete has a window in which it deletes a *newer* attendance's holder and silences that timer. A stale holder costs nothing because only a live timer ever compares against it. If no test pins that absence, add one, so the tidying edit fails instead of passing.

## Implementation milestones

1. Collapse `Park`/`Wait`, delete `asyncrewake_park`, and land a direct `route` test per tool plus a behavioural replacement for the counting assertion. This is the load-bearing milestone: it removes the only branch that lies about what the code does.
2. Purge and reattach the doc comments across all five files, and land the `crates/cli/tests/it/` gate that keeps them attached. Demonstrate the gate failing on an inserted orphan.
3. Make `attended` a single write derived from the turn's decision, and give `clear_attending` one job.
4. Rewrite the module doc as both algorithms, complete the test-only-surface audit, and settle the two placement questions — the SessionStart path and the wait wire — doing each extraction only if it earns itself. Pin the holder's deliberate non-deletion.

Each milestone must leave the hook behaviourally identical and the suite green, and must be reviewable on its own.

## Acceptance

- `cargo test -p clank --lib cli::stop_hook` passes, and the hook's observable behaviour is unchanged for every tool and every disposition: the same verdict, the same wire shape, the same exit code.
- `route` is asserted directly for every tool. No test in the repo asserts a count of occurrences of an identifier, or the formatting of a function body, in place of the behaviour it stands for.
- There is exactly one production call site for the in-hook wait, and the routing type has no variant that behaves identically to another.
- `stop_hook.rs` contains no reference to `hook_held`, `wait.lock`, wait generations, claims, or `reachability` — in code or in prose.
- The new gate fails on an inserted orphan doc block, and passes over `crates/**/src` with all 21 existing ones resolved. Every doc comment in the touched files describes the item it is attached to.
- A Stop turn touches only these three state paths under `.clank/agents/<label>/`, and no fourth. `attending` is consumed, never written here. `attended` is updated at most once by a **bound** turn, from that turn's decision: written when the turn yielded to an attendance, removed when it took the marker without one, and left alone when it *declined* an attendance it must not forget. `attending.holder` is written only when a timer is armed, and is never removed by the hook. So an ordinary driving turn writes none of the three and removes two, and a turn that returns on a stale binding touches none. A test pins the holder's non-removal and names why.
- The exit contract is unchanged and is asserted: a Claude `Continue` exits 2, every other outcome exits 0, and no error path exits non-zero for any adapter.
- `stop_hook.rs`'s module doc states both algorithms. Every top-level item on the Stop path belongs to one of its seven steps or to the state those steps read; the SessionStart items are named as the second path and are preserved with their behaviour, argv and wire shape intact.
- A declined attendance still keeps its marker, and an agent whose turn ends with no wake coming still gets one — the two failures this machinery exists to prevent are both still covered.

## Decisions taken during implementation

Two of the Work items above are conditional, and both were decided against.
Recorded here so this document does not read, once archived, as if work is
still owed.

- **The SessionStart path stays in `stop_hook.rs`**, with a section boundary
  naming what it is and where it ends. Extracting it means moving roughly
  3000 lines of tests with it, and a test lost in that move is invisible —
  the same class of silent staleness this plan exists to remove, and a worse
  failure than a long file. Mechanical to do later if someone watches the
  test count across the move.
- **The wait wire and item rendering stay** too, for the same reason and with
  the same boundary comment.

One thing the milestones surfaced that no gate covers: a doc comment can name
a symbol that no longer exists and nothing objects. The orphan-doc gate
catches DETACHMENT, not a stale reference — two ghosts (`hook_held`,
`asyncrewake_park`) survived into the final milestone, one of them written by
the commit whose job was removing stale references. A grep gate over removed
identifiers would catch it, at the cost of a list that itself goes stale.
Left for a plan of its own if it recurs.

## Scope

This is a purge and a restatement, not a redesign. The turn-end decision, its precedence, the three disk files, every wire shape and every exit code stay exactly as they are, as does the SessionStart path; what changes is that the code says each of them once. No new capability, no change to what any adapter receives, and no change to `clank wait`.

Out of scope: `clank wait` itself (3687 lines, its own plan if it wants one), the `setup.rs` probe that chooses the loop mode, and the attendance/check-in *policy* — the expectation default, the poll interval and the check-in wording are all correct and are not to be revisited here.
