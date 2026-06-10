# Release checklist

Everything that should happen before clank is publicly released.
Each unchecked item is written to be directly queueable: name,
problem, and the shape of the fix. Queue plans from here
(`.clank/queue/NNN-<name>.md`) as capacity allows; check items off
as their plans finalize. Items marked **answered** were
investigated while authoring this checklist and need no plan.

## 1. Install & distribution

- [ ] **`install-script-and-release-builds`** — today the only
  install is clone + `cargo install --path crates/cli`. A public
  release needs: a GitHub Actions release workflow building
  binaries for the platforms we care about (macOS arm64/x86_64,
  Linux x86_64/arm64 to start), plus a `curl … | sh` installer
  that picks the right artifact and drops `clank` on `$PATH`.
  There is currently NO `.github/workflows/` at all, so this also
  bootstraps CI (at minimum: `cargo test --workspace` + `cargo
  fmt --check` on PRs — the same gates reviewers run by hand
  today).
- [ ] **`first-run-experience-test`** — walk the true
  first-contact path on a clean machine/user: install → `clank
  setup` → `clank team create` + agent declarations → `clank
  init` → `clank open zellij` → first plan through the full
  review loop. Every rough edge found becomes a checklist
  addendum. (Several first-run fixes already landed this cycle:
  bare `clank init` adopts the user's `default` team; init warns
  with exact commands when no team exists. The walk verifies the
  whole chain, not just init.)

## 2. Notifications & presence

- [ ] **`per-event-notifications-not-per-agent`** — hooks fire
  from each agent's own wfw process (wfw.rs `run_hook` call
  sites), so one event (a new reviewable commit) wakes N
  reviewers' wfw processes and fires N hooks: a `say` hook
  announces the same commit several times. The model fix: a hook
  channel keyed by EVENT, not by woken agent — e.g. fire
  user-facing notification hooks once per (event, sha) via a
  marker/lockfile under `.clank/`, or restrict announce-style
  hooks to one designated process. Needs a small design pass on
  hook semantics (per-agent action hooks vs per-event
  notification hooks are genuinely different things). Partially
  mitigated already: `finish-does-not-wake-reviewers` removed the
  spurious reviewer wakes on finalize, so the volume is lower —
  but the N-reviewers-one-commit case remains.
- [ ] **`zellij-focus-follower`** — in a multi-pane zellij
  session it's not obvious which agent is currently acting. Idea
  from the stub: a side process driving `zellij action` (e.g.
  `go-to-tab` / pane focus / renames) off the same wait-surface
  data the status TUI consumes, so the active agent's pane comes
  forward (or inactive panes collapse) automatically. Building
  blocks shipped this cycle: `clank status --tui` (the
  instrument-panel pane, snapshot-driven) and user-authored
  layout templates (`zellij.layout` + `clank_agents` marker).
  This item is the ACTIVE half: a `clank zellij follow`-style
  watcher that maps "whose turn is it" to zellij actions.

## 3. Docs & pitch

- [ ] **`readme-rewrite`** — README.md exists (234 lines,
  accurate, structured) but reads as an operator manual, not a
  pitch. A public README needs: the motivating story (why
  multi-agent peer review; what goes wrong without the gate), a
  60-second demo path, the mental model (plans, the two-tier
  gate, milestones), THEN the reference material. Rewrite once
  the install story (item 1) exists so the quickstart is real.
- [ ] **`tutorial-screencast-content`** — script/content for an
  intro screencast: a repo going from `clank init` through a
  full plan lifecycle (intro → sizing reviews → implement →
  commit reviews → FINISHED → finalize), showing the zellij
  session with the status pane, a REQUEST_CHANGES round, and a
  block/unblock. The script doubles as the tutorial doc. Depends
  on items 1 (install) and the README's mental-model section for
  vocabulary.

## 4. Polish

- [ ] **`help-output-beauty`** — `clank --help` wraps badly
  because several subcommands carry paragraph-length `about`
  strings (team, shelve, open, rewire, stop-hook…). Fix shape:
  one-line `about` for the command list + move the detail to
  `long_about` (shown on `clank <cmd> --help`). Audit all
  subcommands; eyeball at 80 cols. Small, mechanical.
- [ ] **`post-rewrite-hook-chaining`** — `clank init` warns and
  skips when `.git/hooks/post-rewrite` already exists with
  non-clank content (init.rs `write_post_rewrite_hook`);
  `--force-hooks` clobbers. Neither is clean for a repo with
  existing hooks. Fix candidates: (a) append/chain — detect a
  foreign hook and add the `clank rewire --from-stdin` line to
  it idempotently; (b) respect/offer `core.hooksPath`-style
  d-directories. (a) is probably enough; the warning text should
  then disappear for the common case.

## 5. Code health — investigated, answered inline

- [x] **core/cli boundary (stub item 7) — answered: keep it.**
  `clank-core` is pure data + state machine (serde-only deps,
  "compiles to wasm32 unchanged"); ALL IO — git, filesystem,
  spawning — lives in the cli crate. The boundary earns its keep:
  the gate state machine (`compute_gate`/`derive_status`/
  `work_for`) is unit-tested headless, and the binary-spawning
  test purge was only possible because behavior was testable
  through cores. No action.
- [x] **fold purity drift (stub item 8) — answered: holds.** The
  repo fold is sans-io by construction: `rebuild.rs` (cli) does
  the git IO and feeds inputs; `repo_state`'s fold consumes them
  purely. The per-plan timelines, finished-plan records, and the
  wait-surface all derive from the fold without reaching back to
  disk. Nothing this cycle required reconciliation passes or
  post-hoc fixups in core — the sans-io shape survived heavy
  feature work (milestone gating, finished-notice routing),
  which is the practical test. No action; re-audit only if a
  future plan has to thread IO into core.

## Already done this cycle (was on the stub's radar)

- Reviewer wake noise: `finish-does-not-wake-reviewers` (finish
  is master-only notification) and `gate-reviewers-only-…`
  (gate tier fires at milestones only) — the hook-spam problem
  in item 2 is now scoped to the genuine N-reviewers case.
- First-run init: bare `clank init` adopts the user's `default`
  team; no-team warns with exact commands.
- Status visibility: `clank status --tui` instrument panel;
  user-authored zellij layout chrome (`zellij.layout` +
  `clank_agents` marker) with the `--tui` pane as the documented
  example.
- Wake payloads: stop-hook/wfw emit one-line hints; the HOW
  lives in the skills.

## Suggested order

1. `install-script-and-release-builds` (everything else assumes
   it)
2. `help-output-beauty` + `post-rewrite-hook-chaining` (small,
   sharpen first impressions)
3. `per-event-notifications-not-per-agent`
4. `readme-rewrite`
5. `first-run-experience-test` (validates 1-4)
6. `tutorial-screencast-content`
7. `zellij-focus-follower` (delight, not blocking)
