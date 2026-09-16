# a-reason-you-can-paste
# A reason you can paste

## Why

The remote failure overlay (`render.rs:2756`, `── ✗ REMOTE · esc back ──`) shows
the only copy of a diagnosis that is often long, exact, and needed somewhere
else — a bug report, a search, this conversation. The user asked for it directly
after losing one:

> "it errored but I didn't get to copy the error in time"

A terminal selection is not an answer: the text is wrapped to the region, the
pane is inside zellij, and the overlay disappears on the next transition.

## The model

An overlay that shows a reason owns that reason as *text*, not only as painted
cells. If it can render it, it can hand it over. The copy key belongs to the
overlay generally, not to the remote one specially — every `✗` region has the
same problem.

## Deliverables

1. **`y` copies the shown reason** from the failure overlay, and the region's
   hint says so: `esc back` becomes `y copy · esc back`.
2. **OSC 52** as the mechanism, written to the TUI's own output stream. It is
   the only path that reaches the clipboard of the terminal the human is
   actually looking at — through zellij, and through ssh — which is where clank
   runs. Base64 the payload; chunk if the sequence needs it.
3. **Confirmation in the hint line**: `y copy` → `copied` for a couple of
   seconds. A copy with no feedback is indistinguishable from a dead key, and
   OSC 52 gives no reply to wait for.
4. The failure text copied is the **full** reason, not the wrapped and truncated
   lines as painted.

## Tests

- The overlay's copy action yields the untruncated reason, including a reason
  longer than the region is wide.
- The emitted sequence is a well-formed OSC 52 with the reason base64'd in it.
- The hint reads `copied` after the key and reverts.
- Mutation-check each claim.

## Out of scope

- Copying from regions other than the failure overlay (the ledger, the token).
  Same key, later, once this one proves the mechanism.
- A `pbcopy`/`wl-copy` subprocess fallback. OSC 52 is what works under zellij;
  add a fallback only if a terminal in use turns out to refuse it.
