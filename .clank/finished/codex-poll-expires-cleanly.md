# codex-poll-expires-cleanly
# codex in-hook poll expires cleanly, never hits the hook ceiling

A codex agent idle for 24 hours shows `Stop hook (failed): hook timed
out after 86400s`. The number is clank's own: setup writes
`timeout: 86400` into the codex hooks.json entry ("effectively
infinite" so codex's 600s default doesn't kill the long-poll), and
when an agent has no work for a full day, the in-hook `clank wait`
(default `--timeout 0` = indefinite) outlives the ceiling and codex
KILLS the hook — a scary failed banner for a normal idle.

## Change

The codex stop hook's self-spawned wait always expires BEFORE the
hook ceiling: when the agent's `wait_timeout` is unset, pass an
explicit default just under the hooks.json timeout (e.g. ceiling minus
five minutes) instead of `0`. The poll then exits 2 (timeout), the
hook maps it to Silent as it always has, and codex goes idle with no
failure. An explicit user-configured `wait_timeout` still wins; one
shared constant ties the default to `HOOK_TIMEOUT_SECS` so the two
can't drift apart.

## Not changed

- The hooks.json `timeout: 86400` stays — it is the backstop, now
  never reached in normal operation.
- The stranding itself: after a clean expiry codex still sits with no
  poll until prompted (unchanged behavior, accepted; the self-healing
  re-fire continuation remains a separate future idea).

## Acceptance

- With `wait_timeout` unset, the composed wait argv carries the
  sub-ceiling default, not `0` (unit test on the arg computation).
- An explicit `wait_timeout` is passed through unchanged.
- The default is derived from `HOOK_TIMEOUT_SECS` (single source),
  pinned by a test asserting default < ceiling.
- fmt/clippy at the 18/6 baseline; tests green.
