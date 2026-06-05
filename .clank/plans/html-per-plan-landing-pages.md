# html-per-plan-landing-pages
# Add per-plan landing pages to `clank html` — `clank html open <plan>` takes you to a page showing the plan body + a timeline restricted to that plan's events.

## Problem

`clank html` today renders three page types (`crates/cli/src/cli/html.rs:438-870`):

- `index.html` — global timeline + status + all-plans umbrella.
- `commit/<sha>.html` — per-commit detail.
- An umbrella table inside `index.html` linking to commit pages.

There's no per-plan landing page. To inspect "what's the current state of plan `foo`, including its body + a focused timeline," the user has to:
1. Open `index.html`.
2. Scroll/Ctrl-F to plan rows in the global timeline.
3. Cross-reference plan name to commits manually.
4. Open `.clank/plans/foo.md` (or `.clank/finished/foo.md`) in a separate editor to see the body.

lloyd 2026-06-05: *"I would like `clank html open <plan-name>` to take me directly to the plan landing page where I could see a restricted timeline of just the plan events and the current version of the plan."*

## Verified before promotion (2026-06-05)

- `clank html` subcommand currently has only `clank html open` (no positional, no `<plan-name>`). Verified via `clank html --help`. Adding a positional is a Phase 1 CLI shape change.
- `crates/cli/src/cli/html.rs:43-160` (`build_site`) writes all output files to `out_dir` (= `.clank/html`). Today: `index.html` + `commit/<sha>.html` per commit. Adding per-plan pages = an additional output subdir `plans/<stem>.html` per plan.
- `crates/cli/src/cli/html.rs:551-625` (`render_timeline`, `render_row`) is the existing timeline renderer. The current implementation walks `events: &[(CommitSha, ...)]` and emits a row per event. **Per-plan timeline = same renderer with a pre-filtered event slice.** No new rendering primitive needed.
- Plan-body source: active plans live at `.clank/plans/<stem>.md`; finished plans at `.clank/finished/<stem>.md`. Per lloyd's clarification, body comes from the LAST COMMITTED version (i.e., what `git show HEAD:.clank/plans/<stem>.md` returns), NOT the worktree-dirty version. Implementation: `git show HEAD:.clank/{plans|finished}/<stem>.md`.
- Plan attribution to commits is already computed by the fold — `RepoState.plans[key].commits: Vec<PlanTimelineEvent>` (`crates/core/src/repo_state.rs:117-126`) lists every commit attributed to the plan. For finished plans, `RepoState.finished_plans[idx]` has `intro` + `finalized_at` boundary SHAs. A "plan's events" projection is: walk the global commit list and filter to SHAs that appear in `state.fold.plans[key].commits` (active) OR are in `[intro..finalized_at]` for the finished plan.
- `clank diff <plan>`'s plan-name resolution at `crates/cli/src/cli/plan_resolve.rs` is the established shape: try active first, then finished, error with "not a known plan AND not a valid git range" if neither matches. **Pinned to reuse this** — consistent CLI grammar across plan-targeting commands.
- The existing `clank html open` opens the browser via the host opener (`launch_opener` at `html.rs:38`). Extending to point at a different file is a one-line change.

## Approach

### Phase 1 — CLI shape

- `HtmlCmd::Open` gains an optional positional `<plan>`:
  ```rust
  Open { plan: Option<String> }
  ```
- When omitted: current behavior preserved (opens `index.html`).
- When supplied: resolve the plan via `plan_resolve` (active first, then finished), error with the established "not a known plan" diagnostic if no match. On match, open `plans/<stem>.html` instead of `index.html`.

### Phase 2 — Render `plans/<stem>.html` for every plan

Extend `build_site` (`html.rs:43-160`) to emit one page per plan (active + finished):

1. After computing `events` + `state` + `subjects`, iterate `state.fold.plans` for active and `state.fold.finished_plans` for finished. Both produce a `(PlanKey, Vec<CommitSha>)` projection — the SHAs attributed to that plan.
2. For each plan, render a page with:
   - Header: plan name, lifecycle status (Active / Finished + finalize-at SHA), link back to `../index.html` (the breadcrumb pattern already used by `commit/<sha>.html`).
   - **Plan body section**: `git show HEAD:.clank/plans/<stem>.md` (active) OR `git show HEAD:.clank/finished/<stem>.md` (finished). Render as markdown→HTML using the existing markdown pipeline if there's one, OR as a `<pre>` block if no md→html exists. Pin at promote-time: check whether `html.rs` already uses a markdown renderer for plan bodies on the umbrella.
   - **Restricted timeline section**: same `render_timeline` call as the index, but with `events` pre-filtered to the plan's commit SHAs. The `render_row` per-commit renderer is unchanged.
3. Output path: `<out_dir>/plans/<stem>.html`. Mkdir-p the parent.

### Phase 3 — Cross-linking

- `index.html`'s umbrella row for each plan gets a hyperlink to `plans/<stem>.html` (currently a plain text plan name).
- `commit/<sha>.html`'s plan attribution line (if present) gets the same link.
- The per-plan page's breadcrumb links back to `index.html`.

### Phase 4 — `clank html open <plan>` wiring

- `cli::html::run`'s `Open { plan: Some(name) }` branch:
  1. Resolve `<name>` via plan_resolve.
  2. Run `build_site` as today.
  3. Compute target path: `<out_dir>/plans/<resolved-stem>.html`.
  4. `launch_opener(&target)?` (same opener used today for `index.html`).
- `Open { plan: None }` branch unchanged.

### Phase 5 — Tests

Unit tests in `cli::html::tests`:
1. `per_plan_page_filters_timeline_to_plan_commits_only`: feed a mock `RepoState` with two plans foo + bar, each with 3 commits; render foo's page; assert the timeline section contains foo's 3 SHAs and NOT bar's.
2. `per_plan_page_includes_committed_body_not_worktree`: write a different on-disk body than what was committed; assert the rendered page shows the committed text (verifies the `git show HEAD:...` projection).
3. `per_plan_page_for_finished_plan_uses_finished_dir`: finished plan body sourced from `.clank/finished/<stem>.md` not `.clank/plans/<stem>.md`.

Integration test in `crates/cli/tests/html_open_per_plan_integration.rs`:
4. `html_open_with_plan_arg_opens_per_plan_page`: spawn `clank html open foo --print-path` (new `--print-path` flag for testability, OR a different mechanism — pin at promote-time) and assert the resolved path ends with `plans/foo.html` AND the file exists.
5. `html_open_unknown_plan_errors_with_diff_style_diagnostic`: parity with `clank diff <unknown-plan>` — error message names available plans.

## Open questions (tentative picks)

1. **Plan-name resolution**: pinned — reuse `plan_resolve`'s active-then-finished search, same as `clank diff <plan>`. Consistent CLI grammar.
2. **Finished plans get landing pages**: pinned per lloyd 2026-06-05. They appear in the umbrella's "last finished" / "finished plans" footer with hyperlinks.
3. **Plan body source**: pinned per lloyd 2026-06-05 — committed body (`git show HEAD:...`), not worktree-dirty.
4. **`--rebuild` interaction**: tentative — `clank html open <plan>` always runs the same `build_site` pass it runs today; `--rebuild` still forces a full re-render. No "build just this plan's page" fast-path in v1 (premature optimization).
5. **Markdown rendering for the plan body**: pin at promote-time after verifying whether `html.rs` already has md→html for inline plan bodies elsewhere. If yes, reuse; if no, ship `<pre>` first (the bodies are markdown-shaped but a `<pre>` block is still readable) and consider a follow-up plan for proper rendering.
6. **Testability mechanism for the open-target**: tentative — add a `--print-path` flag on `clank html open` (skips browser launch, prints the resolved target). Mirrors the `--print` convention used by `clank agent start`, `clank diff`. Alternative: env var `CLANK_HTML_OPENER=echo`. Pin at promote-time.

## Out of scope

- Live-rebuild on file changes (watch mode). Existing model is "run on demand."
- Per-plan RSS / atom feeds.
- Search across plans.
- Restyle of the existing umbrella / commit pages. Header link addition is in-scope; redesigning the umbrella is not.
- Per-commit pages getting per-plan side-nav. Header breadcrumb to `index.html` stays; a "next/prev commit within this plan" nav is follow-up.

## Acceptance

- `clank html open <plan>` resolves `<plan>` against active + finished plans (same as `clank diff <plan>`), errors with a "not a known plan" diagnostic if unmatched.
- After build, `<repo>/.clank/html/plans/<stem>.html` exists for every active AND every finished plan in the fold.
- Each per-plan page contains: plan name + lifecycle status + back-link to `index.html`; the plan body as committed at HEAD; a timeline section showing ONLY commits attributed to this plan.
- `clank html open <plan>` launches the browser on `plans/<stem>.html`, not `index.html`.
- `clank html open` (no arg) preserves current behavior — opens `index.html`.
- `index.html`'s umbrella row per plan links to `plans/<stem>.html`.
- Unknown plan errors with a diagnostic shape matching `clank diff <unknown-plan>` (test parity).
- `cargo test --workspace` passes.

## Related

- `clank-diff-editor` (FINISHED): established the plan-name resolution shape this plan reuses (`plan_resolve` active-then-finished + "not a known plan" diagnostic).
- `clank-open-zellij-layout-file` (FINISHED): the `--print`-style convention for testability without side-effects.
- `status-blocks-dominate-gate` (FINISHED): unrelated, no overlap.
