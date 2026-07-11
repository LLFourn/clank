# zellij-one-listing-per-pass
# one zellij listing per reconcile pass

A reconcile pass is dominated by `zellij action list-panes --json`,
measured live at ~1.1s per call (plain-text `list-panes` is 13ms,
`query-tab-names` 16ms — the JSON variant is uniquely slow). A promote
pass runs it THREE times — the worker's live listing, a second one
inside `relocate_for_promote`, and the verifying re-list — plus a
0.2s `list-clients` focus capture per op. That is the "large delay
between confirming and the layout switching": ~4s of listings on a
chain where every other action is milliseconds.

## Change

- **One listing per pass, with one honest exception**: the worker
  lists panes once and threads `&[ZellijPane]` through the primitives
  — `add_reviewer_pane`, `remove_reviewer_pane`,
  `relocate_for_promote` take the listing as a parameter instead of
  re-listing internally. A pass-start listing stays valid for anchor
  lookup, removal pane-id resolution, geometry, and classification.
  The exception (codex 631636e): when a pass ADDS panes and also
  relocates, the relocation depends on the just-created panes —
  `new-pane` reports no id and compose skips on a master absent from
  the supplied listing — so the worker refreshes the listing ONCE
  after the adds, before relocating. The common promote (all panes
  pre-exist) stays at one listing.
- **One focus transaction per pass**: capture the user's focused pane
  once at pass start (only when the plan has actions), restore once
  after the last action — replacing the per-op capture/restore. Same
  invariant (reconciliation never moves the user's focus), fewer
  subprocesses, and no mid-pass flicker between ops.
- **Verify via `dump-layout`, not a second `--json` listing**
  (measured 0.12s vs 1.1s): the layout dump carries each pane's
  command + args (the exact repo-scoped agent-start invocation) and
  its name (the role title) — everything verification's
  `(label, master_titled)` pairs need, no pane ids required. A small
  KDL-scanning parser with unit tests; on parse/query failure verify
  falls back to the `--json` listing (correctness over speed).
- The standalone-callable primitives keep working for any remaining
  non-worker caller by listing themselves when no listing is supplied
  (or, if the worker is by now their only caller, drop the fallback
  and say so).

## Expected effect

Promote-shaped pass: 3× list-panes --json + 3× list-clients ≈ 3.9s of
overhead becomes 1× list-panes --json + 1× dump-layout + 1×
list-clients ≈ 1.4s — confirm-to-switch lands around 1.5s, from
~9.5s. The remaining floor is the single `--json` listing ops need
for pane ids and geometry; going below it would need a zellij fix or
a clank zellij plugin (out of scope, noted for the record).

## Out of scope

- Any semantics change: plans, convergence verification, coalescing,
  invalidation, and shutdown behavior stay exactly as reviewed.
- The retitler's cached text listing (13ms path) — already cheap.
- Upstreaming a zellij `list-panes --json` perf fix.

## Acceptance

- A promote-shaped pass (all panes pre-exist): exactly ONE
  `list-panes --json` (at start), ONE `dump-layout` verify, ONE
  `list-clients`. An add+relocate pass: exactly TWO `--json` listings
  (start + post-add refresh). Both call counts pinned by injected-I/O
  tests, including the full add+relocate+remove team replacement
  converging in ONE pass.
- A no-action pass (already converged) performs at most the one
  listing it planned from, and an empty plan skips the verify as
  today.
- Focus still restored to the user's pane after the pass (single
  restore), including when ops fail mid-pass.
- All existing zellij/reconciler tests pass unchanged in meaning;
  fmt/clippy at the 18/6 baseline; no binary-spawning tests.
