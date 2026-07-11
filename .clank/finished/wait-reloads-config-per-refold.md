# wait-reloads-config-per-refold
# wait re-resolves role and policy on every refold

`clank wait` resolves the caller's role (roster-derived), the repo
config, the reviewer tiers, and the `WorkPolicy` ONCE, before entering
the watch loop (wait.rs ~163-190). The loop refolds repo state on
every wake — and `config.json` is a wake dir, so roster changes DO
wake it — but the projection still runs with the values captured at
arm time. A parked wait therefore computes work against a stale team:
after a promote or member change, every already-armed wait (agents'
armed waits, codex's in-hook poll) projects the OLD roster until it
happens to exit and be re-armed. Observed live: waits that "aren't
waiting" for the right things after roster edits.

## Change

Move the per-wake inputs INSIDE the loop: on each refold pass,
re-resolve role (when `--role` wasn't explicit — an explicit flag
stays authoritative), reload the repo config, tiers, and rebuild the
`WorkPolicy` before projecting. These are small local JSON reads —
negligible next to the refold itself. The initial pass and the loop
pass should derive them through ONE shared helper so they cannot
drift (same single-source discipline as `queue_promote_outcome`).

Resolution errors mid-loop are fail-soft where the arm-time behavior
was fail-soft (role defaulting) and keep the previous pass's values
where arm-time errored hard (a transient config read failure must not
kill a parked wait; retry next wake).

## Not changed

- `--author` / identity: fixed at arm time by design (who you are
  doesn't change mid-wait).
- The observer path (`--for`): resolves nothing today, stays that way.
- Hook config for lifecycle firings: reload with the rest (same
  helper), so a hooks edit also takes effect without re-arming.

## Acceptance

- An in-process test: arm a wait (short timeout) as a reviewer, flip
  the roster (e.g. change tiers so the caller stops being a commit
  reviewer / gains work), and assert the NEXT wake projects with the
  new config — without the wait restarting.
- Explicit `--role` is never overridden by re-resolution.
- A transient config read failure mid-loop does not kill the wait
  (fail-soft, retries next wake).
- One shared derivation helper feeds the initial pass and the loop.
- Omitted-role re-resolution is proven behaviorally (park as an
  omitted-role reviewer, promote the author mid-wait, wake as
  MASTER), explicit-role precedence is proven by the same promotion
  NOT overriding an explicit `--role reviewer`, and the fail-soft
  branch is proven by corrupting the roster mid-wait (wait survives on
  last-known-good) then repairing it (wait wakes on the repaired
  config).
- fmt/clippy at the 18/6 baseline; tests green; no CLANK-binary
  spawning in tests (git fixtures are the standing exemption, as in
  every integration test).
