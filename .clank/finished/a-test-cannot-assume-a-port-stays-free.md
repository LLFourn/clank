# a-test-cannot-assume-a-port-stays-free
# A test cannot assume a port stays free

## Why

`cli::web::tests::a_repo_remembers_its_port` fails about once in ten
runs, and has cost a diagnosis three times in one session:

```
thread 'cli::web::tests::a_repo_remembers_its_port' panicked at
crates/cli/src/cli/web/mod.rs:2474:
assertion `left == right` failed: remembered, and free
  left: 52102
 right: 52068
```

The test asks the OS for an ephemeral port, remembers it, drops the
listener, and asserts the next `listen()` returns the same one because
it is "remembered, and free". Between the drop and the next bind the
port is free for EVERYONE — and this binary is meanwhile running other
tests that bind ephemeral ports of their own. When one of them takes
it, `listen()` correctly falls back to a fresh port and the assertion
fails.

Nothing is wrong with the port policy. The test asserts a precondition
it does not control.

## The model

**A test may assert what it establishes; freeness of a released port is
not something it can establish.**

This is the same shape as the identity variables: an input the fixture
does not own, read as though it did. The difference is that an
environment can be injected and a port cannot — the OS's ephemeral
range is shared with every other test in the binary and with the
machine.

So the fix is not a seam. Either:

1. **Retry the window.** The claim under test is the POLICY — remembered
   and free gives back the same port — and a lost race is not evidence
   against it. Re-run the setup a bounded number of times and fail only
   if the policy never holds. A genuine regression (never returning the
   remembered port) fails every attempt, so the assertion keeps its
   teeth; a stolen port costs a retry.
2. **Or stop releasing it.** If there is a way to hold the port through
   the check — a listener the second `listen()` can inherit, or a policy
   call that does not need to bind — the race disappears instead of
   being tolerated. Worth ten minutes before settling for (1).

Whichever it is, the reason belongs in the test, because the next
person to see it fail will otherwise go looking for a bug in the port
policy, as I did.

## Deliverables

1. Fix `a_repo_remembers_its_port` by (2) if it is possible and (1) if
   it is not, with the reason written down.
2. Check the other three parts of that test for the same assumption —
   "remembered but taken → a fresh one" binds a stranger to the port
   first, which IS established, but the fresh port it then gets is
   asserted against nothing.
3. Look for the same shape elsewhere in `cli::web`: every test that
   binds an ephemeral port, drops it, and expects it back.

## Tests

- The repaired test passes under contention aggressive enough to break
  the versions it replaces — which means OVERLAPPING PROCESSES, not
  just a busy module: 16 copies of the test binary running the one test
  at once (codex on 657d596). Three builds, one command, same machine:

  | | 16 concurrent copies |
  |---|---|
  | the original | 12 failed |
  | policy mutated to ignore the remembered port | 16 failed |
  | repaired | 0 failed |

  The middle row is the teeth: the retry cannot pass a policy that
  never returns the remembered port, because every candidate loses.
- Serially, whole module: the original failed 2 of 20 runs; the
  repaired one passed 50 of 50.
- Mutation-check: a policy that ignores the remembered port must still
  fail the repaired test on every attempt.

## Out of scope

- The port policy itself, which is correct.
- Any other flaky test not of this shape.
