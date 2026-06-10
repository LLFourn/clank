# wfw-output-is-a-minimal-hint

`clank wfw` / the stop-hook continuation output has drifted
into a mountain of noisy, context-churning info. It's meant to
be the SMALLEST possible hint about what the agent should do
next. If the agent needs more, it runs `clank status` (or
`clank feedback`, `clank diff`, etc.). The wfw payload is a
nudge, not a briefing.

## The smell (current output)

A single reviewer wake currently emits something like:

```
Clank wfw returned work for `codex` (reviewer). Items:
  - reviewer: plan `teams-based-agent-registration` at f6feba2 — write feedback via
    `clank feedback write --commit f6feba231685eb198eb412e5d014de836c4ddf81 \
        --author codex --verdict approve|finished|request-changes \
        -m "<summary>"`
    Use FINISHED when you think the plan is done and `clank finish` should
    run. Use APPROVE for mid-flight commits; include a one-sentence reason
    you're not marking FINISHED (e.g. "tests still missing").
  - blocked: agent `claude` block `needs-design-thought` (scope: ...)
    Question: <full block message>
    This block suppresses work until a human runs:
    `clank unblock claude needs-design-thought --plan ... -m "<answer>"`
```

That's a tutorial, repeated every wake. The full
`feedback write` invocation (with the 40-char SHA, the
verdict menu, the multi-line FINISHED-vs-APPROVE guidance) and
the full block explanation are reference material the agent
already knows or can look up — not per-wake signal.

## The principle

Each wait item should be ~one line: WHO + WHAT-plan + the
single next verb. Examples of the target density:

```
reviewer: review teams-based-agent-registration @ f6feba2
master:   revise teams-based-agent-registration (changes requested)
blocked:  claude/needs-design-thought on open-zellij-... (awaiting human)
```

The agent's own skill/system prompt already documents HOW to
write feedback; wfw shouldn't re-teach it every wake. Short
SHA, not full. No verdict menu. No FINISHED-vs-APPROVE essay.
No spelled-out `clank unblock ...` command — "awaiting human"
is enough (the human reads the block elsewhere).

## json and human carry the SAME data

The json output and the human-readable output should be the
same data, just formatted differently — NOT "json is the
verbose structured one, human is the trimmed one." The noise
problem is that too much DATA is emitted at all; the fix
reduces the payload in BOTH views and keeps them in parity.
So json is not an escape hatch for the tutorial fields — if a
field is tutorial noise (the verdict menu, the spelled-out
`feedback write`/`unblock` invocation, the FINISHED-vs-APPROVE
essay), it leaves the json too. What remains — the minimal
next-action signal — appears in both, one as fields and one as
a one-line string built from those same fields.

## Scope / surfaces

- `crates/cli/src/cli/wfw.rs` — the WaitItem rendering for
  BOTH `--json` and the default human text.
- `crates/cli/src/cli/stop_hook.rs` — the per-tool
  continuation message it composes from wfw items.
- Audit every per-item field/string for "is this signal or is
  this a tutorial the agent can get from `clank status`?" —
  and drop it from both representations together.

## Promote-time notes (verified against current code)

- **Short SHAs already work end-to-end**: `feedback write
  --commit` resolves via `CommitRef::parse` +
  `resolve_against(all_shas)` (feedback.rs:23/54), with clear
  not-found / ambiguous errors. The minimal hint can carry a
  short sha with NO enabling change. (Json keeps the full-sha
  FIELD; the human line shows the short form — same data,
  different format, per the parity principle.)
- **The skill assets must change in lockstep**:
  claude_skill.md:81 + codex_skill.md:52 promise "Reviewer
  prompts include the exact `clank feedback write …` invocation —
  run it verbatim." After this plan the wake carries no command;
  the agent composes it from the skill's documented form + the
  hint's sha. Reword BOTH skills' stop-hook paragraph (and keep
  them consistent — cf. the FINISHED-definition lockstep test
  pattern from finished-means-impl-done-not-plan-text).
- **The FINISHED-vs-APPROVE essay is now safe to delete**:
  finished-means-impl-done-not-plan-text landed; the canonical
  (implementation-complete) definition lives in both skills. The
  per-wake copy in stop_hook.rs is pure repetition now.
- **The bloat is mostly stop_hook.rs**: wfw's own `render_human`
  is already near the target density (one line per item); audit
  its json twin for tutorial fields, but the main surface is
  stop_hook.rs's per-tool continuation composition (the reviewer
  tutorial at ~:220 and the blocked/unblock spell-out).
- **Tests**: the old binary-spawning wfw/stop-hook output tests
  were purged (ffb94b9). Coverage comes back as in-process unit
  tests of the composing subroutines (render_human/render_json in
  wfw.rs; the stop_hook message composer) — pin the one-line
  shape and the json/human field parity. NO binary spawning.

## Out of scope

- Changing what work wfw detects (gate logic untouched).

## Status

Stub — queued LOW priority (lloyd 2026-06-09: "cheeky" /
nice-to-have). Pure UX-noise cleanup; no behavior change. Sized
at promote time 2026-06-10 with the notes above.
