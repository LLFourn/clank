# two-setup-tests-fail-on-master
# clank — two setup tests have been red on master, and nobody knows since when

## Why

`cargo test -p clank --lib` has failed on master for the whole of the last several
plans. Every commit message I wrote said so, which is the problem: a red suite you
report in prose is a red suite you have stopped reading. The two failures are:

```
cli::setup::tests::opencode_plugin_scrubs_every_foreign_identity_var
  setup.rs:2480  "plugin must blank-scrub GROK_SESSION_ID"

cli::setup::tests::role_invariants_lead_each_body
  setup.rs:1449  "both roles must forbid polling (Claude)"
```

Both were attributed to `80f01612`
(`[investigate-wait-generation-removal] Remove wait generation, claim, and
turn-end state`) — but that attribution was carried forward from session notes and
has NOT been verified by bisect. Verifying it is step one; it may be wrong.

These are not cosmetic. Each asserts something a generated asset must SAY:

- The opencode plugin must blank out every foreign agent-identity env var, so an
  agent launched inside another agent's session does not inherit its identity.
  `GROK_SESSION_ID` is not being scrubbed.
- Both role skills must tell the agent not to poll — the invariant that stops
  every agent running its own `clank wait` and duplicating deliveries.

So the question is not "why is the test unhappy" but "did an asset lose a rule it
is supposed to carry, and has clank been shipping it that way since". A generated
prompt that dropped "do not poll" is a live behavioural regression in every repo
set up since, not a test bug.

## Also

One `cargo test -p clank --lib` run during github-selectors reported THREE
failures with the same 1714 total, so a third test flipped once. I did not capture
which, and eight runs since did not reproduce it. Worth one bounded attempt to
identify (run the suite N times capturing failure names; do not chase it further
than that), because a test that fails one run in ten is worse than one that always
fails — this whole plan exists because a steady red became background noise.

## Shape

1. Bisect each failure to the commit that introduced it. Report the real shas;
   do not assume `80f01612`.
2. For each, decide WHICH side is wrong — the asset or the assertion — and say
   why in the commit. An assertion that outlived its rule gets deleted, with the
   reason; a rule that was dropped by accident gets restored.
3. If a rule was genuinely lost, check what else that commit dropped from the same
   asset: one missing line is rarely alone.
4. Consider whether anything should make a red `--lib` suite harder to carry for
   weeks. Do not build a CI system; just say whether something cheap would have
   caught it.

## Acceptance

- `cargo test -p clank --lib` is green, or every remaining failure is named with
  the reason it is expected and the commit that made it so.
- Each fix says which side was wrong and why, not just that the test passes.
- If an asset lost a rule, the plan says what agents set up in the meantime are
  missing and whether anything needs re-running for them.
- The intermittent third failure is either identified or explicitly recorded as
  not reproducible after a bounded attempt, with the count of runs.

## Findings (written as the work landed)

**Two failures, two opposite causes, both from `80f01612` — verified by that
commit's own diff, which is stronger evidence than a bisect.**

1. `opencode_plugin_scrubs_every_foreign_identity_var` was a REAL LEAK. The
   commit added `GROK_SESSION_ID` to `SESSION_IDENTITY_VARS`; the opencode
   plugin's hand-written copy of that set never learned it, and has never
   contained the string in its entire history. An opencode agent under a grok
   session was handing that session id to every tool shell.
2. `role_invariants_lead_each_body` was the OPPOSITE: the rule was intact. The
   commit rewrote claude's prohibition — correctly, since it made the hook own
   the wait — and changed `NEVER` to `Never` in the same line. The test was
   asserting the capitalization.

**A third copy was stale and nothing was guarding it.** `identity_env_boundary`
enforces that only `agent_env.rs` READS an identity variable, from its own
hand-written list with a comment asking whoever added one to add it here too.
Nobody did, so the gate silently stopped guarding the variable that had just
been added. It now derives from the production const; a planted
`env::var("GROK_SESSION_ID")` fails it with the derived list and passes silently
with the old copy. The hole was latent — nothing in the tree was reading it.

**Affected installs.** Anyone who ran `clank setup` after `80f01612` has the
leaky plugin. This machine did: `~/.config/opencode/plugin/clank.js` had no
`GROK_SESSION_ID`. No new tooling is needed to find them — `clank doctor` already
compares on-disk assets to embedded content and says
`~/.config/opencode/plugin/clank.js is an old setup version; run `clank setup`
(no --force needed) to upgrade`. Verified against the freshly built binary.

**The intermittent third failure did not reproduce.** 20 consecutive
`cargo test -p clank --lib` runs after the fixes: 1719 passed, 0 failed, every
time. Recorded as not reproducible rather than explained.

**Would anything cheap have caught this?** Not really, and it is worth being
exact about why. Detection never failed: the suite was run on every commit and
the failures were reported in every commit message. What failed was that
"pre-existing" was used as a dismissal — the attribution to `80f01612` was
asserted for weeks and never checked, and checking it here took one `git log -S`
and two minutes. No tool makes someone curious.

One cheap mechanism WOULD have helped with a different failure: a checked-in list
of known-failing tests, with a gate that fails when the actual set differs. That
makes a NEW failure loud (it would have caught the one-run flip this plan could
not reproduce) and makes removing a failure require deleting its line, so the debt
shrinks visibly instead of being re-typed into prose. It would not have fixed
this: the list would have been carried exactly as the prose was. Suggested as a
possible follow-up, deliberately not built here.

**`--test it` is out of this plan's scope and still red**, with two different
failures: `panel_probes_wakers::the_status_loop_probes_the_waker_fingerprint_on_its_own_turns`
and `wait_event_sources::peek_starts_no_command_source`. They are unattributed —
the same mistake this plan exists to correct — so they are named here rather than
assumed to share a cause with the two it fixed.

## Out of scope

Rewriting the setup assets, changing the skill content beyond restoring what was
lost, and anything about `investigate-wait-generation-removal` beyond the two
failures it is suspected of.
