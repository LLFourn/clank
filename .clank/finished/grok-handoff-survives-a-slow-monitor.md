# grok-handoff-survives-a-slow-monitor
# Grok handoff survives a slow monitor

## Why

The first v0.2.0 candidate dc32e3a9 failed Linux CI run 43170286904:
`grok_launch_acceptance::fresh_launch_delivers_pending_work_and_ten_background_completion_turns`
and `fork_launch_delivers_only_in_child_repo_and_never_rebinds_source` both
ended before binding with `Grok lifecycle got a verdict it cannot frame: Park`.
The common stop exchange can return Park at a non-parking report's deadline;
the Grok wire adapter rejects it. Trace the actual deadline and monitor response
paths before choosing a fix. The release remains untagged.

## Complete change

- Reproduce or deterministically exercise the failed Grok handoff boundary,
  including slow/unavailable monitor and ordinary startup behavior.
- Correct the report/terminal-frame/deadline contract at its owning layer so
  Grok gets an intentional, supported outcome, preserves pending work and does
  not silently strand the agent. Do not merely hide stderr, weaken the fixture,
  retry a failed assertion or inflate sleeps to make CI green.
- Preserve session binding, fork isolation, background completion and other
  harness handoff semantics. Use direct Rust tests for deterministic policy;
  retain native integration coverage for the launch/exec boundary.
- Run focused handoff tests, both failed Grok integration cases, formatting and
  strict Clippy. Commit one complete fix for review.

After reviewer clearance and finalization, resume authorized v0.2.0 publication:
push the corrected commit, require successful full CI at that exact SHA, then
create the immutable annotated tag and verify release builds/assets/installer.
Never tag the failed candidate or move a published tag.

## Implementation and validation

The monitor answers Handoff directly using its registration evidence. The child
already bounds both failed attachment and an attached-but-unanswered exchange,
announcing an outage before the parent deadline when it can. The remaining gap
was the parent deadline: its non-parking fallback was an internal Park sentinel,
which the Grok adapter diagnosed instead of supplying a supported decision.

The parent now uses the child's gated outage-announcement path for an expired
Handoff. Both paths share the binding lock and once-per-outage marker; a notice
is recorded only after a successful write. Pending work is neither presented
nor acknowledged. Other report deadlines retain their existing behavior.
The shared emitter also avoids writing twice when persistence fails after an
already attempted delivery.

Passed locally: 101 hook/carrier tests (including connected silent monitor,
unavailable monitor and deterministic parent deadline cases), all four native
Grok launch integration tests, strict workspace/all-target Clippy, formatting
and diff checks. New parent coverage checks pre-arm and in-flight expiry,
deduplication, remaining pending events, unwritten/failed output, manual mode
and replacement bindings. The original Linux failures were not reproduced as
load-dependent failures locally; their exact unsupported-frame boundary is now
exercised deterministically. Full remote CI remains required before tagging.

## Review correction: runner ceiling and continuation intent

The first review found that the installed five-second Grok runner could kill
the hook before its ten-second report budget, and that an outage block's
continuation was incorrectly treated as a wait-arm continuation.

Installation and validation now share a thirty-second ceiling derived from
the ten-second routing/exchange budget, ten-second maximum child shutdown and
ten-second startup/emission margin. Routing consumes the exchange budget rather
than resetting it. The native launch fixture reads the installed timeout and
enforces it instead of using its own twenty-second limit. A paused-clock test
uses the installed value and exercises startup, a silent carrier, full cleanup
and emission; the earlier five-second setting cannot pass it.

Before its carrier can clear an outage marker on recovery, the Grok adapter
captures whether this continuation followed an outage notice. That continuation
can request its first wait; a subsequent genuine wait-arm continuation remains
quiet, as does an already registered wait. Unit coverage emits a real timeout
notice then exercises recovery; an independent native CLI test seeds the durable
outage state, verifies the recovered monitor requests one wait, then verifies
the next continuation requests none and pending work remains unhandled.

Correction validation passed: 182 hook/carrier/setup tests, five native Grok
integration tests, strict workspace/all-target Clippy, formatting and diff
checks. One installed-provider opt-in test remains ignored, not passed. Evidence:
`/tmp/clank-grok-correction.log`. Remote CI and publication remain outstanding.
