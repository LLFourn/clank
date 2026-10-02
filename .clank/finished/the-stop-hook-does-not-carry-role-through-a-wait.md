# the-stop-hook-does-not-carry-role-through-a-wait
# Resolve the wake's role after the wait

## Why

The stop hook resolves an agent's role before parking and passes that value
through `compute_wait_outcome` to the wake renderer. A promotion or demotion
during the wait can therefore produce instructions headed with the old role.
Work selection already derives the role from the roster; this is a small fix to
the role used to present the reply, not a redesign of the hook.

## Change

- Keep the existing entry checks for auto mode, roster validity, session binding
  and attendance. Do not retain the resolved role in the driving plan or pass it
  into the asynchronous wait operation.
- For a successful, nonempty wait reply, after checking the session still owns
  the label, resolve its role once from the current roster immediately before
  rendering the wake. SessionStart already follows this general ordering.
- Preserve the reply decoder's pure/testable boundary with a narrow, lazy role
  resolver if needed. Capture repo/label, not a precomputed role. Superseded,
  failed or malformed replies must retain their existing outcomes without an
  unnecessary role lookup.
- If the role cannot be resolved at presentation, report a clear diagnostic;
  never fall back to the role from before the wait. Preserve stale-binding
  suppression and the existing deadline/cancellation behavior.

## Acceptance

- A controlled wait starts with a reviewer and finishes after promotion: its
  wake says master. The reverse transition says reviewer. Use a controlled
  completion/test seam, not scheduling sleeps or a real monitor process.
- Tests cover an unavailable role at presentation and preserve superseded,
  unsuccessful and empty-reply handling. The fresh lookup occurs only when
  needed to render actionable work.
- The relevant stop-hook tests and formatting checks pass. No role value from
  hook entry crosses the wait to determine the rendered role.

## Scope and cost

One roster lookup per actionable wake. No polling, timer, watcher, protocol
change or background service. Event reassignment, role-change notifications,
voice acknowledgements and timeline markers are separate work.
