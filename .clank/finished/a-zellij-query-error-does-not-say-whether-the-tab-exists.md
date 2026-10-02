# a-zellij-query-error-does-not-say-whether-the-tab-exists
# Investigate a visible fork tab followed by a Zellij query error

## Report

An agent created the `changeless-hub-research` fork, then ran
`clank open --fork changeless-hub-research`. The user could see its tab, but the
agent received exactly:

```
the zellij session this shell belongs to did not answer (`query-tab-names` exited exit status: 1). `$ZELLIJ` is set but the server behind it may be dead; open a fresh terminal and re-run `clank open`.
```

The same shell's `zellij list-sessions` returned
`No active zellij sessions found.` The agent initially inferred that the tab
had not been created. Neither failed query nor that shell's listing establishes
whether the user's visible tab exists in another reachable session/context.

## What the current code establishes

`open_zellij::open_one` queries existing tabs before writing/spawning the new
layout. `fork create` can automatically open its tab before a subsequent
explicit `open --fork`. The report therefore does not yet prove that a single
open invocation created a tab and then errored. `in_session()` checks only
whether `$ZELLIJ` is set, despite describing a live session; the failed query
diagnostic currently omits the subprocess stderr. Treat these as investigation
starting points, not a diagnosis of a dead server.

## Investigation and resulting fix

- Reconstruct the exact command order and separate fork creation, automatic
  opening, explicit opening, dedup lookup and session/tab verification. Establish
  which command created the visible tab and which invocation failed. Record
  known side effects separately from later lookup failures.
- Trace the Zellij target used for listing, query and creation. Inspect relevant
  session/socket/runtime environment and subprocess execution context, including
  whether an agent's sandbox can reach the user's server. Collect command argv,
  exit status and bounded stderr without dumping unrelated environment or
  claiming that a binary which ran successfully proved the server was dead.
- Define one target-session decision for each operation and use it consistently
  for lookup and creation. Distinguish environment indicating Zellij, a server
  actually answering, an answered listing with no matching tab, and an
  unanswered/failed query. Do not treat unknown as an empty tab list, silently
  switch to another session, or create duplicate tabs to bypass the failure.
- Improve diagnostics based on evidence: identify the failed operation and
  target, retain useful underlying error text, and state which earlier effects
  are known to have succeeded. Suggest a fresh terminal only when the evidence
  supports stale context; otherwise explain the observed access/query failure.
- Apply the smallest routing, sequencing or reporting fix that the reproduction
  justifies. If this is a shell-context problem rather than a failed creation,
  document that distinction and fix misleading diagnostics instead of inventing
  a new session-discovery mechanism.

## Acceptance

- Add deterministic subprocess/decision fixtures for automatic fork opening
  followed by an explicit query failure, an already-open tab, a genuinely absent
  tab, stale Zellij environment, and query/access failure. Verify command order,
  selected session, diagnostics and whether a creation command was issued.
- A failed query never becomes proof that a tab is absent. Re-opening an
  existing fork/tab remains idempotent. Failure of tab opening does not imply
  that an already-created fork was rolled back.
- Exercise the normal create/open and `--all` paths affected by the fix; retain
  their existing target/session behavior where the server answers correctly.
- Use the existing test seams and run relevant open/fork/Zellij boundary tests
  and formatting. Avoid tests that require spawning real tabs in the user's
  running session. Record whether the exact report was reproduced or only the
  failure contract was exercised, and what evidence supports the diagnosis.

## Scope and cost

This is an investigation with a targeted fix, not a Zellij lifecycle redesign.
No periodic session discovery, background polling, extra watcher or ongoing
process scan. No automatic tab/session deletion, restarts or configuration
migration. Any additional diagnostic probe must be bounded and run only for the
explicit operation that needs it.

## Promotion assessment

Checked against the current implementation after the reply-required event plan
was finalized. Ready for a bounded investigation and targeted fix:

- `fork::run_create` completes fork creation and prints its path before
  attempting automatic opening. Its open error propagates without explaining
  that the fork already exists; make that side-effect boundary explicit.
- `open_zellij::open_one` performs its current-session tab query before writing
  the layout or issuing creation. This supports distinguishing two invocations,
  but does not reconstruct the user's historical command sequence by itself.
- `zellij_tab_names` discards stderr on failure and suggests a dead/stale server
  solely from a nonzero query result. Preserve the actual failure evidence.
- In the outside-session `--all` path, an answered listing selecting a live
  session is followed by `session_tab_names`, which currently turns a failed
  query into an empty list. Include this existing fallback in the fix: unknown
  tab state must not authorize adding every tab again.

Use deterministic fixtures for these operation boundaries and the existing
bounded subprocess facilities where applicable. Exact historical reproduction
is an evidence question, not a prerequisite for correcting a demonstrated
failure contract; clearly report which case was reproduced. Do not add live
workspace mutations or broaden this into session discovery or recovery.

## Investigation findings and implementation

The original agent shell and its historical Zellij socket context were not
available. The exact visible-tab report has therefore not been reproduced.
The installed local Zellij reports 0.46.0; its help was inspected without
opening/querying live sessions. Neither that version nor the current shell's
environment establishes why the original query failed.

The two demonstrated failures are corrected at the existing open boundary:
nonzero queries retain argv, status, selected-session context and bounded
stderr instead of suggesting a dead server, and an unanswered listing/tab
query no longer becomes an empty answer authorizing creation. The current
implicit-session route or outside named-session route is captured once for
lookup and creation; there is no fallback server discovery. Only the recognized
no-sessions answer authorizes the existing fresh-session path on nonzero exit.

Read-only queries use concurrent pipe drains and a three-second deadline, with
one MiB maximum stdout and four KiB stderr tail. Oversized/non-UTF-8 stdout is
unknown, never a partial listing. Timeout/error cleanup ends the process group
and bounds reap waiting; cancelled futures own their child and pipe readers,
with no detached reader tasks. Inside `--all` reuses one answered tab query.
`--print` no longer probes a server and previews the fresh outside layout.

Fork creation still prints its completed path before opening; the shared
post-create phase preserves that path and the original open error if opening
fails. Fixtures exercise automatic opening followed by a separate explicit
query failure, answered existing/absent tabs, failed/stale/access contexts,
outside single and all routes, and current-session all deduplication. A real
temporary fork fixture verifies its registration/files survive automatic-open
failure. Harmless shell fixtures exercise output limits and a descendant
retaining pipe writers; no fixture starts a Zellij server or opens real tabs.

Validation: 103 open-Zellij unit tests, 25 fork unit tests, 27 open integration
tests, 37 fork integration tests, seven Zellij ownership/cost checks and two
signal-boundary checks passed. Formatting and whitespace checks passed. No
live workspace tabs were opened or queried, and no installation was performed.
