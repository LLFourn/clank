# the-attending-row-fits-and-shows-only-live-waits

## Why

The agent row overflows its pane and takes the layout down with it:

    ► claude  master  ⌛ 79575 (b3huk9tn1) · stale, ended   ← band runs past the edge
                                                            ← a row that should not exist
    ► kimi  commit

Three reported symptoms — the selection band running past the pane
edge, a blank gap under it, and the main status bar vanishing — are
ONE cause.

`char_width` (`status_tui/text.rs:279`) counts a char as 2 columns only
inside the emoji plane:

    if ('\u{1F000}'..='\u{1FAFF}').contains(&c) { 2 } else { 1 }

`⌛` is **U+231B**, Miscellaneous Technical, OUTSIDE that range. It is
East-Asian-Wide, so the terminal draws it in 2 columns while the
renderer believes 1. Every attending row is therefore one column wider
than computed: `truncate_to` keeps one char too many, `emit_selected`
pads to a width already exceeded, the physical line is `cols + 1`, the
terminal wraps it, and the extra row breaks the height clamp that keeps
the status bar on screen.

The function's own doc claims the approximation is *"exact for
everything we draw"*. It was — until a glyph outside the set was drawn,
with nothing to catch it. A swept scan of the TUI sources finds `⌛` is
the only drawn character in that state today (the CJK strings are test
fixtures).

## Fix the class, not the character

Widening the range for one glyph leaves the next one to find the same
way, in production, as a layout collapse.

Make the claim enforceable: a test that pins the width of EVERY
non-ASCII character the TUI draws against its true East-Asian width.
Adding a glyph the width function mis-measures then fails the build
instead of the pane.

The width function may stay a lookup rather than gaining a dependency —
that is a fine trade — but the set it covers must be checked, not
asserted in prose.

## The row says too much, and some of it is untrue

Two further reports, and they resolve together:

**A wait that has ENDED should not be on screen.** The row exists to
answer "who is waiting, and on what". A dead process answers "nobody",
so rendering `· stale, ended` spends the pane's scarcest resource
saying nothing. Render only attendance whose process is still alive.

Status still must NOT delete the record — reaping is the hook's, and
that boundary already holds. Hiding is a render decision; deletion is
an ownership one. A record with no pid stays visible, because "cannot
check" is not "ended".

**Drop the harness task id from the row.** `b3huk9tn1` is an opaque
handle from the agent's own tool. It cannot be looked up, correlated
with anything on screen, or found in `ps` — the reported reaction to
first seeing it was "what is that id? it doesn't look like a PID". The
pid is the half a human can act on. Keep `⌛ 79575`; the id belongs in
the plain `clank status` line, where width is not scarce.

Together these delete the `stale, ended` string entirely, which was the
longest part of the row and the least useful.

`Attended::summary` has exactly two callers: `status.rs:716`, the
plain line this plan does not touch, and `render.rs:485`, the row it
does. Narrowing `summary` itself would strip the task id from both and
silently take the out-of-scope line with it. The TUI needs its own
rendering of the record; `summary` keeps its contract.

## Required tests

- Every non-ASCII character the TUI draws is measured correctly by
  `char_width` — the enforceable form of the doc's claim, and the test
  that would have caught `⌛`.
- An agent row carrying attendance renders at most `cols` display
  columns, asserted with the real glyph, so a mis-measured char fails
  here rather than in a pane.
- The selection band on that row is exactly `cols` wide.
- A record whose pid is dead renders NO attendance marker.
- A record with no pid still renders one — unknown is not ended.
- The record is NOT deleted by any render path.
- No test spawns zellij or an agent binary.

## Out of scope

- The plain-text `clank status` line. It has no width pressure and the
  task id is worth keeping there.
- Reaping policy. The hook owns it and this plan does not touch it.
