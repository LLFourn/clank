# open-and-fork-idempotent

Make opening an existing fork/worktree's workspace a first-class,
idempotent operation, and stop `clank fork` from erroring when the
fork already exists.

## Problem

Today there's no way to (re)open an EXISTING fork's zellij tab, and no
way to open them in bulk:

- `clank fork <name>` only CREATES — it errors if the worktree/branch
  already exists, so it can't double as "reopen."
- `clank open zellij --repo <path>` spawns a worktree's tab layout, but
  it's per-path, has no idempotency (re-running double-opens the tab),
  and isn't surfaced as "open fork X".
- Listing forks is `git worktree list` (no clank-native surface), and
  there's no batch open.

The operator wants: see the forks, open one by name/PR, or open them
all — without spawning duplicate tabs for forks already on screen.

## Core model — reconcile desired tabs against open tabs

The unifying idea: opening is a RECONCILE between the DESIRED set of
fork tabs and the CURRENTLY-OPEN tabs in this zellij session. Open only
the difference; never double-open. One primitive underlies everything:

> `ensure_tab_open(worktree)`: if a tab for this fork is already open,
> do nothing; otherwise spawn its layout (the existing
> `clank open zellij --repo <path>` path).

"Already open" = a tab whose name matches the fork name. Tab names come
from `zellij action query-tab-names`, BUT the status-glyph feature
(tui-tab-mirror-bar-emoji) prefixes them (e.g. `🔨 device-prompt-…`),
so matching must strip the leading glyph first — reuse
`strip_leading_emoji` (the same helper `parse_agent_panes` uses for
pane titles). Fork tab name == fork/worktree name by construction
(`clank fork`: "NAME … also the zellij tab name").

`open --fork`, `open --pr`, `open --all`, and an idempotent `fork` all
become thin callers of `ensure_tab_open`.

## Part 1 — `ensure_tab_open` primitive + `clank open --fork <name>`

- Add the reconcile primitive: query open tab names (strip glyphs),
  and open the fork's tab only if absent.
- `clank open --fork <name>`: resolve `<name>` to a worktree under
  `.clank/worktrees/<name>` (error with a clear message — and a hint to
  `clank fork <name>` — if no such worktree), then `ensure_tab_open`.

## Part 2 — `clank open --pr <N>`

- `--pr <N>` resolves to the fork named `pr-<N>` (the convention
  `clank fork --pr <N>` already uses), then delegates to the same path
  as `--fork pr-<N>`.

## Part 3 — `clank open --all`

- Enumerate worktrees (the `.clank/worktrees/*` entries from
  `git worktree list`), and `ensure_tab_open` each — opening only the
  ones not already on screen.
- Loud about what it skipped vs opened (so "nothing happened" is
  legible when everything was already open).
- COST NOTE: each fork tab spawns its full team (master + reviewers) +
  a `status --tui` watcher. `--all` across many forks is heavy (this is
  the fleet that loaded the zellij servers in status-tui-watch-cpu).
  Keep it explicit/opt-in; do not auto-open-all anywhere.

## Part 4 — make `clank fork` idempotent

- When the target worktree already exists AND it's genuinely this
  fork (same path under `.clank/worktrees/<name>` on the expected
  branch), `clank fork <name>` must NOT error. Instead: skip the
  `git worktree add` + session seeding, and just `ensure_tab_open`.
  Net effect: `fork` = "ensure the worktree exists" + "ensure its tab
  is open", both idempotent.
- Keep erroring ONLY on a genuine conflict: the branch/path exists but
  is NOT a clank fork of this source (don't silently adopt foreign
  state).

## Design decisions

- **`open` opens; `fork` creates.** `open --fork <name>` / `--pr <N>`
  operate on an EXISTING worktree; if none exists, error with a hint to
  `clank fork <name>` / `clank fork --pr <N>`. Create-or-open is the
  idempotent `fork`'s job (Part 4) — keeping the two verbs distinct is
  clearer than overloading `open`. (Redirectable if you'd rather `open`
  fall through to `fork`.)
- **Outside zellij**: mirror today's `clank open` — attach-or-create
  `clank-<repo>`, THEN open the requested tab(s); after attach the
  same `query-tab-names` reconcile applies (no double-open).
- **Which session's tabs count**: the CURRENT session only (a fork open
  in another session still opens a tab here).

## Testing (in-process; no binary spawning — [[no-binary-spawning-tests]])

- Pure reconcile: given a worktree set + a `query-tab-names` output
  (WITH glyph prefixes), assert `--all` opens exactly the not-already-
  open forks and skips the rest; glyph-prefixed names match.
- `open --pr <N>` resolves to `pr-<N>` and reconciles like `--fork`.
- `fork` idempotency: re-running on an existing fork is a no-op on the
  worktree (no re-`add`, no error) and triggers `ensure_tab_open` —
  exercised against the in-process core with the zellij calls injected.
- `--fork`/`--pr` on a missing worktree errors with the fork hint.

## Non-goals

- Closing / lifecycle-managing tabs beyond opening.
- Cross-session tab reconciliation (only the current session).
- A `clank worktrees` list command — `git worktree list` suffices;
  revisit only if a clank-native list earns its keep.
