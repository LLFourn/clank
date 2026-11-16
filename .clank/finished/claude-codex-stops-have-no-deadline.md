# claude-codex-stops-have-no-deadline

Invariants: I1.

## Why

The Claude and Codex Stop hooks compute a deadline from `HOOK_TIMEOUT_SECS` minus a margin, about 25 days (`stop_hook.rs` `poll_deadline`, `VERDICT_GRACE`), and translate a `Park` sentinel at it. In practice it never fires. Its expiry also runs through report deadlines, the turn child's last word (`turn_child.rs`) and upgrade handoffs (`hook_handoff.rs`): code that is never exercised but has to be kept right.

## Build

A Claude or Codex Stop report says it has no deadline, and every layer reads that as never expiring. The wire shape does not change: the report's `deadline` stays a `Ms`, and these tools send `Ms::MAX` (`u64::MAX`). So this is no protocol change (I8), and an older monitor reading it never expires the report either. Monitor tests already use `u64::MAX` for this.

1. **The hook** (`stop_hook.rs`): Claude and Codex reports are built with `deadline: u64::MAX`. Delete `poll_deadline`, `POLL_MARGIN_SECS`, `VERDICT_GRACE`, `verdict_deadline`, `under_deadline` and `at_deadline` for these tools, and the `Park` sentinel translated at the deadline. If `emit`'s `FnMut` relaxation has no other user, revert it to `FnOnce`.
2. **The child** (`turn_child.rs`): `last_word`/`answer_by` already derive from the report's deadline. With `u64::MAX` they never pass, which the child must treat as no deadline: no overflow in `checked_add`, and no sleep scheduled at it. Make "no deadline" explicit there (for example `None` from `last_word`), not an accident of a large number.
3. **The monitor:** `expire_reports` and `report_deadline` skip a report whose deadline is `u64::MAX`, so no timer is armed for it. OpenCode's renewal (`RenewWait`) and Grok's bounded handoff keep their real deadlines and are untouched.
4. **Upgrade handoff** (`hook_handoff.rs`): the handed-over report carries the same `u64::MAX`. A handoff record from an older build that names a finite deadline is honoured as written.
5. If the harness's own timeout is ever hit, the hook is killed and the existing killed-hook path applies.

## Acceptance

- No code computes a Stop deadline for Claude or Codex.
- Upgrade handoff of a parked Claude hook still works.
- OpenCode renewal tests pass unchanged.
