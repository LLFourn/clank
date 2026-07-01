# clank-gitignore-single-allowlist

Collapse clank's ignore setup to ONE tracked source of truth: a
`.clank/.gitignore` allow-list. Kills the current two-file redundancy (root
`.clank/*`+exceptions AND `.clank/.gitignore`'s per-dir list, which overlap
and can drift) and removes the per-subdir enumeration churn.

## Problem

Two `.gitignore` files both govern `.clank/`:
- **root `.gitignore`**: `.clank/*`, `!.clank/plans/`, `!.clank/finished/`
  (+ redundant explicit `.clank/feedback/`, `.clank/cache/`).
- **`.clank/.gitignore`** (currently UNTRACKED): a per-dir deny list
  (`/agents/ /cache/ /feedback/ /queue/ /drafts/ /html/ /pr-reviews/
  /shelved/ /worktrees/ /zellij/`).

They're redundant (both hide the same dirs), can drift (a stale claim this
session asserted the exact opposite of reality), and the per-dir list must be
extended every time clank adds a subdir (the `/worktrees/` upgrade was one
such churn). The ENTIRE tracked surface under `.clank/` is plan docs:
`plans/` (1 file) + `finished/` (183) — verified via `git ls-files`.

## Design (Option B — one allow-list, keep the layout)

`.clank/.gitignore` becomes the SINGLE source of truth, TRACKED, an
allow-list:

```
# .clank/.gitignore
/*
!/plans/
!/finished/
!/.gitignore
```

"Ignore everything under `.clank/` except plans, finished, and this file."
The root `.gitignore` carries NO `.clank/` rules — it goes back to being
about the repo's own code. Wins:

- **One source of truth**, tracked, so it travels with the repo (`clank init`
  writes/refreshes it) — no drift between two files.
- **Future-proof**: a NEW `.clank/<subdir>` is ignored automatically by `/*`
  — no more editing an enumerated deny list per added dir.
- Tracked surface stays exactly `plans/` + `finished/` (+ the ignore file);
  `git check-ignore` confirms everything else hidden. (Standard `.github/`-
  style idiom; the `!` exceptions now live in ONE obvious tracked file.)

(Option A — move `plans/`/`finished/` to the repo root and make `.clank/`
100% ignored, zero exceptions — is the purer form but a breaking cross-repo
restructure. Deliberately deferred; this plan is B.)

## Implementation

- **`crates/cli/src/init_facts.rs`**: `CLANK_GITIGNORE_ENTRIES` becomes the
  ORDERED allow-list `["/*", "!/plans/", "!/finished/", "!/.gitignore"]`;
  `clank_gitignore_body()` joins them in order (order matters for gitignore —
  `/*` MUST precede the `!` re-includes).
- **`classify_gitignore_body`** (returns the real `GitignoreState` =
  `Missing / Canonical / Legacy / Drifted`): it's currently SET-based
  (order-insensitive) — wrong for an allow-list. Rework to order-aware:
  `Canonical` iff the body's non-comment lines equal the canonical allow-list
  IN ORDER; recognize the LEGACY per-dir deny list (keep it as a
  `LEGACY_ENTRIES` const) → `Legacy` so existing repos get rewritten on
  `clank init`; a managed-but-modified body → `Drifted`; absent → `Missing`.
- **DELETE the per-dir appender `ensure_clank_gitignore_entry`
  (init_facts.rs:447) and ALL its production callers** (ruthless 9804336 —
  THE load-bearing fix). Five commands currently APPEND a per-dir entry to
  `.clank/.gitignore` when they run: `fork` → `/worktrees/` (fork.rs:264),
  `open zellij` → `/zellij/` (open_zellij.rs:225 + :397), `queue add` →
  `/drafts/` (queue.rs:252), `pr-review` → `/pr-reviews/` (pr_review.rs:234).
  Under the order-sensitive allow-list an append makes the body non-canonical
  → `classify` → `Drifted`/`Foreign` → the single source of truth is defeated
  on the FIRST fork/queue/open/pr-review. But those dirs are ALL already
  ignored by `/*` (the flip side of the future-proofing win), so the appender
  is obsolete: delete the function, remove all five call sites (one up-front
  FULL-repo grep — a stranded caller silently re-wedges the allow-list), and
  drop its tests.
- **`clank init`** flow is otherwise unchanged (write / upgrade /
  warn-on-drift); it now emits the allow-list. Ensure `.clank/.gitignore` is
  **tracked** (the `!/.gitignore` self-include makes it trackable; `init`
  should `git add` it or at least it's no longer ignored).
- **Root `.gitignore`**: `clank init` still does NOT edit the user's root
  file. Decision to flag (below): warn-only vs. auto-strip clank rules.
- **`crates/cli/src/cli/doctor.rs`**: keep the `.clank/.gitignore` presence
  check and the plans/finished tracked probes (both still hold). ADD a check
  that the ROOT `.gitignore` has NO `.clank/` rules (they're now redundant /
  a drift hazard) — Warn with a "remove these; `.clank/.gitignore` owns it"
  hint. Optionally check `.clank/.gitignore` is tracked.
- **`check_ancestor_gitignore` / `matched_by_clank_gitignore`** (init.rs):
  the allow-list still matches `.clank/**` paths, so the warning-suppression
  keeps working — verify with the new body.

## Decision to flag for review

**Root `.gitignore` migration:** clank has only ever WARNED about the root,
never edited it. For a clean single-source end-state the root's `.clank/*`
block must go. Options: (a) keep warn-only — `doctor` flags it, the human (or
the rollout) strips it; (b) `clank init` auto-strips clank-recognizable
`.clank/` lines from the root. Lean **(a) warn-only** — editing a user's root
`.gitignore` is invasive and out of character; the rollout strips this repo's
by hand. Reviewer's call.

## Migration / rollout (finalize step, not code)

Existing repos (this one + other dogfood repos) have the two-file setup. On
the new binary: run `clank init` in each (rewrites `.clank/.gitignore` to the
allow-list via the `Legacy` state), strip the root `.clank/*` block by hand, and
`git add .clank/.gitignore` so it's tracked. Do this BEFORE `cargo install`
so no repo is left half-migrated (per the migrate-before-install discipline).

## Tests

- `clank_gitignore_body()` == the ordered allow-list.
- `classify_gitignore_body`: allow-list → `Canonical`; the legacy per-dir body
  → `Legacy`; a managed-but-modified body → `Drifted`; absent → `Missing`.
- A repo scaffolded with the allow-list: `git check-ignore` shows `plans/` +
  `finished/` + `.clank/.gitignore` tracked, and `agents/`/`cache/`/a NEW
  hypothetical `.clank/stubs/` all ignored (proves future-proofing). (Reuse
  the init test harness; assert via check-ignore or the classifier.)
- **INVERT** the existing `classify_gitignore_body_is_order_insensitive_and_
  set_based` (init_facts.rs:337) — it asserts the very order-insensitivity
  being removed; now the allow-list is order-SENSITIVE (reordering → not
  `Canonical`). Update/replace the init tests that assert the body contains
  `/agents/` etc., and remove `ensure_clank_gitignore_entry`'s tests.
- **Regression guard for the whole point of the plan** (ruthless): after
  writing the canonical allow-list, running each of `fork` / `queue add` /
  `open` / `pr-review` (in-process, per the no-binary-spawn rule — drive the
  handlers/their gitignore path against a temp repo) leaves `.clank/.gitignore`
  **byte-identical** to the canonical allow-list — no appended entry.

## Acceptance criteria

- Fresh `clank init` writes the tracked allow-list `.clank/.gitignore`; no
  `.clank/` rules land in the root.
- `plans/` + `finished/` tracked; everything else under `.clank/` (incl. a
  new subdir) ignored — one source of truth, no root duplication.
- `ensure_clank_gitignore_entry` and its five callers are GONE; running
  fork/queue/open/pr-review leaves the allow-list byte-identical (no drift).
- `classify` upgrades a `Legacy` per-dir body; doctor flags redundant root
  `.clank/` rules. clippy at baseline; tests pass.

## Deploy

`cargo install --path crates/cli --force`, then migrate each dogfood repo
(above) before relying on it.
