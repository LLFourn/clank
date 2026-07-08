# drop-stop-hook-trace
# drop the stop-hook decision trace

Delete the per-run decision trace the stop hook writes to
`.clank/agents/<label>/stop-hook.json` (or `.clank/stop-hook.json` when the
session doesn't resolve to an agent).

## Why

The trace (stop-hook-decision-trace, 348158f) existed to debug "why didn't
the hook wait/block" during the claude blocking experiments. That need has
passed: the claude flow never waits or renders items in-hook anymore
(claude-stop-hook-minimal-hint) — its only decision is a terse arm-`clank
wait` nudge or silence. Nothing in production reads the trace back; the only
readers are `stop_hook.rs`'s own tests.

Considered tradeoff: the trace also covers codex, which still long-polls and
blocks in-hook, so codex non-wait diagnostics go too. Accepted — nothing has
needed them since the minimal-hint flow settled, and the planned per-agent
activity record (`agents/<label>/runtime/activity.json`) is the natural home
for the useful subset (last edge + outcome) if the need returns.

Deleting it also stops a pointless wake: every trace write lands inside
`agents/`, which is a `CLANK_WAKE_DIRS` entry, so every stop-hook run
currently pings every `clank wait` long-poll for no gate reason.

## Change

- Remove the `Trace` struct and all trace plumbing from
  `crates/cli/src/cli/stop_hook.rs`: `record_input`, the `set_*` helpers,
  `mark_in_flight`, `finalize`, the best-effort file write, and the
  `trace: &mut Trace` parameters threaded through `compute_outcome` and
  friends.
- Update the tests that assert trace contents (`read_trace` helper and the
  tests pinning `continue_kind`, disposition, label/repo resolution):
  keep their behavioral assertions about hook *outcomes*, drop the
  trace-file assertions. Delete tests whose only subject is the trace.
- Stale `stop-hook.json` files on disk are abandoned in place — gitignored
  runtime state, no cleanup pass.

## Out of scope

- `stop-hook-state/` — operational state the hook reads back; keep.
- Any change to hook outcomes for either tool.

## Acceptance

- No production or test code references `stop-hook.json` or the `Trace`
  struct.
- Hook outcome tests still pin the claude nudge/silent and codex
  wait/block behaviors.
- `cargo test` green; fmt/clippy at baseline.
