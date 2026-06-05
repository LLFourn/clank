# html-per-plan-landing-pages
# Add `clank html open <plan>` to launch the browser at the existing `.clank/html/plan/<stem>.html` page instead of `index.html`.

## Problem

`clank html open` builds the site and launches the browser on `index.html`. There's no shortcut for "open the landing page for plan `foo`."

lloyd 2026-06-05: *"I would like `clank html open <plan-name>` to take me directly to the plan landing page where I could see a restricted timeline of just the plan events and the current version of the plan."*

## Verified before promotion (2026-06-05 — REVISED after codex 46eefa1 catch)

The original draft of this plan claimed per-plan landing pages didn't exist. **They do.** Codex caught the staleness; verified by re-grepping `crates/cli/src/cli/html.rs`:

- `build_site` already mkdir-p's `out_dir.join("plan")` at `html.rs:52`.
- `render_plan_page` exists at `html.rs:888` and produces a per-plan page with plan body + a restricted timeline filtered to that plan's commits.
- `build_site` already iterates active + finished plans and writes `out_dir.join(format!("plan/{stem}.html"))` at `:217-226`.
- Cross-links exist: index → plan pages at `:603-608`, commit pages → plan pages at `:725-730`.
- Existing test coverage at `crates/cli/tests/html_integration.rs:1257-1289`.
- Plan body source is already the COMMITTED markdown (lloyd's pin matches today's behavior).
- Finished plans already get their own pages (lloyd's pin matches today's behavior).

So Phases 1, 2, and 3 of the prior draft are already shipped. The only gap:

- `clank html open` takes no positional. Verified via `clank html --help`: `Usage: clank html [OPTIONS] [COMMAND]` and `Commands: open / help`. The `open` subcommand has no args today.
- `HtmlCmd::Open` at `crates/cli/src/cli/mod.rs` is a unit variant.
- `cli::html::run` (`html.rs:23-42`) handles `HtmlCmd::Open` by calling `launch_opener(&out_dir.join("index.html"))`.

## Approach

### Phase 1 — CLI shape: positional `<plan>` on `clank html open`

`HtmlCmd::Open` becomes:

```rust
Open {
    /// Plan name. When supplied, the browser opens `plan/<stem>.html`
    /// instead of `index.html`. Resolved against active plans first,
    /// then finished — same shape as `clank diff <plan>`.
    plan: Option<String>,
    /// Print the resolved target path on stdout and exit without
    /// launching a browser. Mirrors `--print` on `clank agent start`,
    /// `clank diff`, `clank open zellij`.
    #[arg(long)]
    print_path: bool,
},
```

### Phase 2 — Resolve + open

In `cli::html::run`'s `Open` arm:

1. Run `build_site` as today (unchanged — the page already gets rendered for every active + finished plan).
2. If `plan.is_some()`:
   - Resolve via `plan_resolve` (same call shape as `clank diff <plan>`): try active first, then finished.
   - On no match: error with the established "not a known plan" diagnostic (parity with `clank diff <plan>`).
   - Compute target = `out_dir.join(format!("plan/{stem}.html"))` where `stem` = the resolved `PlanKey::as_str()`.
3. Else: target = `out_dir.join("index.html")` (current behavior preserved).
4. If `print_path` is set: `println!("{}", target.display())`, exit 0. Otherwise: `launch_opener(&target)?`.

Plan-name resolution helper lives at `crates/cli/src/cli/plan_resolve.rs`. Reuse it directly; do NOT fork.

### Phase 3 — Tests

Integration tests in a new file `crates/cli/tests/html_open_with_plan_arg_integration.rs`:

1. `html_open_with_active_plan_arg_prints_per_plan_path`: `clank html open foo --print-path` in a repo with active plan `foo`; assert stdout ends with `plan/foo.html` AND that file exists on disk.
2. `html_open_with_finished_plan_arg_prints_per_plan_path`: same as 1 but `foo` is finalized.
3. `html_open_with_unknown_plan_errors_like_diff`: `clank html open nonexistent --print-path` — exit nonzero, stderr mentions "not a known plan" (matching the `clank diff <plan>` diagnostic). Phrasing-agnostic via `contains("not a known plan")` so tightening the wording later doesn't break.
4. `html_open_with_no_plan_arg_falls_through_to_index`: `clank html open --print-path` (no plan); assert stdout ends with `index.html`. Backward-compat regression guard.

## Out of scope

- Changes to `render_plan_page`'s rendering shape. The page already exists; this plan only adds a CLI shortcut to it.
- Changes to plan-body source (already committed-only, matching lloyd's pin).
- Changes to the `plan/` output directory name. Codex flagged "plans/" vs "plan/" in the original draft; the existing `plan/` is what stays.
- Markdown rendering of the plan body. Pre-existing (whatever `render_plan_page` does today is what `clank html open <plan>` shows).
- `--rebuild` semantics. Existing behavior preserved: `clank html open <plan>` runs the same `build_site` as `clank html open` (no plan).

## Acceptance

- `clank html open <plan>` resolves `<plan>` against active + finished plans (same shape as `clank diff <plan>`); errors with the established "not a known plan" diagnostic on miss.
- After build, `clank html open <plan>` opens the browser on `<repo>/.clank/html/plan/<stem>.html`, not `index.html`.
- `clank html open` (no positional) continues to open `index.html` — backward-compat preserved exactly.
- `--print-path` prints the resolved target on stdout, exits 0, doesn't launch a browser. Mirrors the `--print` convention on adjacent commands.
- Unknown plan errors with stderr matching the `clank diff <plan>` diagnostic shape.
- All existing `clank html` tests pass unmodified (`html_integration.rs`).
- `cargo test --workspace` passes.

## Related

- `clank-diff-editor` (FINISHED): established the plan-name resolution shape this plan reuses (`plan_resolve` active-then-finished + "not a known plan" diagnostic).
- `clank-open-zellij-layout-file` (FINISHED): the `--print`-style convention this plan extends to `clank html open --print-path`.
- Existing `clank html` plan-page rendering (codex caught the original draft missed this): `render_plan_page` at `html.rs:888`, output dir `plan/`, cross-links from index + commit pages, test coverage at `html_integration.rs:1257-1289`.
