# the-hook-ceiling-is-in-the-wrong-unit

`HOOK_TIMEOUT_SECS = i32::MAX` cancels every parked Stop hook within a
second of it starting — the exact opposite of the "effectively never"
it was chosen to mean.

## The trap

The field is in SECONDS. The runner converts it to MILLISECONDS for a
JS timer, whose delay is a signed 32-bit int capped at 2147483647 ms
(~24.8 days); past that it wraps and the timer fires on the next tick.

    i32::MAX seconds = 2147483647 s = 2147483647000 ms
                                      ~1000x over the cap

So the value LOOKS safely inside a 32-bit range and is a thousandfold
past it, because the range is quoted in a different unit from the
field. Asking for a ceiling that never fires produced one that fires
immediately.

The docs confirm the timeout applies here: `asyncRewake` hooks are
subject to it, and only `async: true` is exempt. A hook that reaches
its timeout is cancelled.

## Measured

Before, with `i32::MAX`:

    19:55:21.064  hook=[94854]  wait=[]  holder=19:55:20
    19:55:21.657  hook=[]       wait=[]  holder=19:55:20   ← gone in <0.6s

and trapping signals showed SIGTERM in the same second the hook parked.

After, at `2_147_483`:

    hook=[56902/03:48] wait=[56903]   ← still alive at 3m52s

The `wait.holder` file is written either way, so from disk a cancelled
hook looks exactly like a park that happened — which is why this read
for two days as "the park dies" rather than "the timer fired".

## Change

    pub(crate) const HOOK_TIMEOUT_SECS: u64 = 2_147_483;

The cap divided by a thousand: the true maximum in this field's own
units, 24.9 days. Not dropped — absent the field each runner applies
its own default, and claude documents 600s for a `command` hook, which
would cut a legitimate park far shorter. Not lowered to the old 86400
either: that ceiling really was reached, silencing this repo's codex
reviewer for 22 hours with a review waiting.

## The test asserts arithmetic, not ambition

The old test asserted the ceiling was unreachable by a running machine
(`> 50 years`). That assertion WAS the bug, written down as a
requirement, which is why it stayed green while every park died.

Assert instead, in MILLISECONDS — where the limit lives, and where
seconds-based reasoning went wrong:

- `HOOK_TIMEOUT_SECS * 1000 <= 2_147_483_647` — does not wrap;
- `(HOOK_TIMEOUT_SECS + 1) * 1000 > 2_147_483_647` — and is the
  largest such value, so it cannot silently drift down;
- `poll_deadline` remains sound at it.

## What this supersedes

Two plans were built on the belief that the runner had changed and
that clank's wake channel needed defending. Both are reverted, and the
reasoning is recorded here so it is not re-derived:

- **attendance-owns-its-wake** — park epochs, a takeover lock, `Owner`
  identity plumbing, a holder format change and identity-aware
  `park_decision`, over thirteen review rounds. It defended against
  races nobody has observed, reachable only under asyncrewake, built
  because attendance appeared to be losing wakes. It was not.
- **the-park-cannot-outlive-the-hook** — blamed the Claude Code
  2.1.246 → 2.1.248 update, which was coincidental: that update landed
  the same morning this constant was installed. It also added a
  `CLANK_CLAUDE_LOOP` escape hatch to route around the failure, which
  is unnecessary once the cause is fixed and left two ways to do one
  thing.

The 22-hour codex silence that motivated raising the ceiling in the
first place was real. The change was right; the value was wrong.

## Acceptance

- A parked hook survives well past a second, observed in production.
- The ceiling is the largest non-wrapping value, asserted in ms.
- No environment variable selects the loop; there is one way.
- fmt/clippy/suite green at baseline.
