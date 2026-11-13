# codex-background-work-is-observable-in-the-tui
# Codex background work is observable in the TUI

## Why

The previous instrumentation research established a bounded completion-to-model
gap in isolated app-server experiments, but left the actual Clank integration
unresolved. The user needs to know how to discover background work that was NOT
launched with `clank run`, whether Stop hooks expose it, and how to make completion
resume the existing Codex TUI. Those are required answers, not optional future
research. A shared daemon previously disrupted launch environment inheritance and
`clank fork`; restoring that arrangement is not an acceptable assumed solution.

The captured 0.167.2 Stop payload contained session/turn IDs, cwd, transcript path,
model, permission mode, final assistant message and stop_hook_active, but no task
inventory. Establish the full supported contract rather than extrapolating from
that single payload. Reuse `docs/codex-integration.md` and its diagnostic.

## Deliverable

Produce a source-linked extension of the report, a reproducible isolated actual-TUI
experiment and one concrete recommended integration design. Close each question
below with observed evidence, a documented contract or a specific demonstrated
limitation. A generic recommendation to use app-server, another inventory of API
names, or testing only `clank run` does not complete this plan.

This is a bounded investigation with an integration prototype in test tooling,
not authorization to migrate live agents or ship production wake policy. Deliver
the complete artifacts in one implementation commit after intro review.

## Required questions and experiments

1. **What does the hook actually provide?** Inspect versioned hook schemas and
   producers, capture Stop payloads from the actual TUI, and enumerate whether
   commands, background sessions, PIDs, tool-call IDs, Code Mode cells or subagents
   are supplied. Inspect supported tool lifecycle hooks and invocation behavior
   as well. Distinguish a transcript path from an inventory, and a tool returning
   a running handle from the process eventually exiting. Test with live background
   work at Stop, not just an empty turn. Record exact version and feature flags.
2. **How can unregistered work be discovered?** Launch a native exec_command
   that yields without `clank run`; repeat through the Code Mode wrapper used by
   this team, a genuinely yielded Code Mode cell and, where supported, a subagent.
   Identify which are distinct lifecycle mechanisms. Evaluate hook facts, native
   running inventory/events and existing transcript observations. For each path
   specify how concrete session, item, handle and process identity are obtained;
   how existing work is discovered on monitor attachment; and how completion is
   observed after disconnect/restart. A disappearing handle or PID does not prove
   success. If no supported surface covers a path, demonstrate the limitation and
   explain the smallest viable fallback and its compatibility costs. Do not hide
   an uncovered path behind a new requirement to use `clank run`.
3. **How does this reach the actual TUI?** Determine whether a `--no-daemon` TUI
   offers a supported control/event endpoint, whether a private per-session server
   can serve that same TUI, and which client actually owns scheduling continuation.
   Inspect versioned CLI/source/docs before assuming an attach mode exists. Verify
   ownership experimentally: do not resume a copied transcript in another server
   and call that control of the original session. Explicitly distinguish the
   app-server protocol from the shared daemon deployment and in-process TUI runtime.
4. **Can completion resume a parked hook?** In an isolated actual-TUI session,
   reproduce an unwrapped background command completing while the Clank-style
   synchronous Stop hook is parked. Trace process completion, observable event,
   monitor receipt, hook release and next model request. Prototype the narrowest
   demonstrated observer-to-hook bridge. Compare native automatic continuation
   with explicit hook continuation and supported native input operations. Explain
   exactly what an app-server client can do that Clank currently cannot, including
   active-turn steering versus interrupt/cancel versus releasing our own hook.
   No timer, user prompt or terminal keystroke counts as the successful automatic
   handoff. Do not report a UI completion notification as model continuation.
5. **Preserve launch and fork semantics.** Test the proposed arrangement with two
   independent sessions using different harmless environment sentinels, cwd and
   temporary homes. Exercise Clank's fork launch path in a disposable repository.
   Prove correct environment inheritance and destination cwd, no cross-session
   input or completion, and no shared-root identity confusion. Establish what
   caused the previous environment/fork failures where evidence is available;
   otherwise record the exact tested invariants rather than asserting a cause.
6. **Resolve the native-behavior question.** Determine whether official contracts
   promise completion-driven continuation for each tested client/tool path. Where
   behavior differs, trace the responsible client/engine boundary. A recent
   regression requires a known-good comparison or a relevant behavioral change;
   do not infer it from two failing versions. If upstream clarification is needed,
   prepare a minimal reproduction and precise question without claiming a removed
   guarantee. Publishing an issue is separate from this plan.

## Acceptance and boundaries

- Provide an answer table: Stop payload contents; discovery of unregistered native
  commands, wrapper commands, yielded cells and subagents; completion observation;
  session ownership; supported wake action; reconnect behavior; version limits.
- Demonstrate success/failure exits before park, during park and at hook entry,
  with repetitions and a no-hook control. Demonstrate two simultaneous tasks and
  two isolated agents without duplicate or cross-session continuation. Reuse
  existing Rust fixture/barriers; avoid Python and ad-hoc compiled helper binaries.
- Prefer scripted local inference and isolated PTY driving for reproducibility.
  Do not modify live team sessions, global config, trust, installed versions or
  the website worktree. All subprocesses have bounded lifetimes and cleanup.
  Arrange an independent Clank check-in for background validation.
- No recurring process/inventory polling, monitor configuration in hooks, automatic
  trust bypass or production shared-daemon switch. File observation is a fallback
  to evaluate explicitly, not a stability guarantee.
- If a safe supported actual-TUI experiment is technically unavailable, supply the
  concrete attempted setup, source evidence and failure boundary. State precisely
  which answers remain unavailable and the operational alternative; do not mark
  the integration demonstrated based on the earlier app-server-only matrix.
- Revise the existing three follow-up drafts to incorporate these findings and
  order implementation work around the proven path. Do not queue overlapping
  production migrations or treat restoring registered-run wakes as resolving
  unregistered work. End with a clear recommended next implementation, its covered
  paths and any unavoidable limitations.

## Implementation evidence

- `docs/codex-tui-background.md` answers each research question, links versioned
  source, describes the measured limits and recommends a private per-agent
  server/TUI/observer arrangement. The three follow-up drafts now distinguish
  native command custody, subagent turns and uncovered cell-only lifetimes.
- The opt-in actual-TUI diagnostic covers 47 background cases, subagent hooks
  and native lifecycle, reconnect across a missed exit, steering/cancellation,
  simultaneous isolated agents and Clank's real fork spec/worktree path.
  A direct native exit-to-hook bridge continues the same TUI without a timer,
  user prompt or wake keystroke. Cell output is collected separately with `wait`.
- Stop supplies no task inventory; PostToolUse skips yielded shell results and
  their later automatic exits. Private native observation covers unregistered
  commands, but no supported public cell inventory/end API was found in 0.167.2.
  Same-server reconnect is tested; crash recovery without an authoritative end
  fact must stay unknown. No production launch or hook policy is changed here.

### Review correction

The active-model experiment now verifies the matching native command exit before
releasing the second model response. The completion is recorded and consumed
once, so it cannot silently become a final-answer race. A focused replay covers
native and wrapped commands, exits 0 and 7, twice each, with retained ordering
traces. This supersedes the original active-case evidence only.
