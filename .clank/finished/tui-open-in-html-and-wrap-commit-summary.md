# tui-open-in-html-and-wrap-commit-summary

Two small `clank status --tui` improvements to the plan/commit detail
overlays.

## Part 1 — "open in HTML" from a plan or commit page

In `status --tui`, pressing Enter on a plan-header or commit row opens a
detail overlay (`OverlayData::Plan { stem, .. }` / `OverlayData::Commit(..)`
in `status_tui/mod.rs`). Add a keybinding on those overlays to open the same
plan/commit as its rendered HTML page in the browser.

The HTML site already builds per-plan and per-commit pages
(`.clank/html/plan/<stem>.html`, `.clank/html/commit/<full_sha>.html`), and
`clank html open <plan>` already builds + opens a plan page — but there is
**no commit open target** yet.

### 1a. Add a `--commit` open target to `clank html open`

`crates/cli/src/cli/mod.rs` (`HtmlOpenArgs`) + `crates/cli/src/cli/html.rs`
(`resolve_open_target`):

- Add `#[arg(long)] commit: Option<String>` to `HtmlOpenArgs`, mutually
  exclusive with the positional `plan` (clap `conflicts_with`).
- `resolve_open_target`: when `commit` is set, resolve the sha against the
  folded repo (accept a short sha, like other commit-taking commands) and
  return `out_dir.join(format!("commit/{full_sha}.html"))`. Error clearly if
  the commit has no built page (e.g. it fell outside the incremental top-N —
  surface a "rebuild with `clank html`" hint rather than opening a 404).

### 1b. Wire the key into the overlays

`crates/cli/src/cli/status_tui/{input.rs,mod.rs}`:

- Add a `Key` variant for it (bind `o` — "open in browser"; free on the
  overlay) in `parse_keys`, and a `DocNav::OpenHtml` variant routed from
  `doc_nav` (input.rs).
- Handle `DocNav::OpenHtml` in the overlay event loop (mod.rs ≈574): dispatch
  by overlay kind —
  - `Plan { stem, .. }` → open `plan/<stem>.html`
  - `Commit(d)` → open `commit/<full_sha>.html` (the overlay carries the
    full `CommitSha`).
- **Mechanism (flag for review):** the TUI is in raw-mode/alt-screen, so it
  must not print build output into the pane. Spawn the built `clank`
  (`current_exe`) **detached** with stdout/stderr to null (precedent: the
  stop hook self-spawns `clank wait`; production subprocesses are allowed —
  the no-binary-spawn rule is test-only). Detached = the browser opens when
  the incremental build finishes without freezing the loop.
- **CRITICAL — flag ordering (codex 70fbe03):** `--repo` and `--quiet` are
  parent `html` flags, NOT `open` flags, so they must precede the
  subcommand. The exact argv is:
  - plan → `clank html --repo <repo> --quiet open <stem>`
  - commit → `clank html --repo <repo> --quiet open --commit <full_sha>`

  `clank html open <stem> --quiet` FAILS to parse — and because we detach and
  null stderr, that failure is **silent** (no browser, no error). So the
  wiring must build argv in this order, and a clap-parse test (below) pins
  it. *Alternative considered:* mark `--quiet`/`--repo` `global = true` so
  ordering can't break it — reviewer's call; the plan takes the
  fixed-order + parse-test route to avoid changing the flag surface.

  *In-process alternative:* call `html::build_site` + `launch_opener`
  directly — cleaner and no argv/ordering risk, but the build is async and
  the TUI loop is sync (needs a runtime handle). Reviewer's call; the plan
  leans spawn-detached for loop-responsiveness.
- Update the overlay's key hint (footer in `render_commit_detail` /
  `render_plan_doc`) to advertise the new key (e.g. `o open in browser`).

## Part 2 — wrap the commit summary on the commit page (don't truncate)

On the commit-detail overlay the subject is rendered with
`one_line(subject, cols)` → `truncate_to` (render.rs ≈876, text.rs ≈187), so
a long subject runs off-screen with an ellipsis and can't be read in full.
The commit BODY right below it already wraps (`wrap(body, cols)`,
render.rs ≈885).

Fix: render the subject with `wrap` too, so the whole summary is readable:

- Keep the `<short_sha>  ` prefix on the first line; wrap the subject across
  the remaining width, aligning continuation lines under the subject start
  (a fixed gutter = sha width + 2), OR — simpler — emit the sha line then the
  wrapped subject full-width. Pick whichever reads cleanest in a narrow pane;
  match the body's plain style.
- Only the commit-detail overlay changes. Log/oneline commit rows keep
  `one_line` (they're deliberately one line each) — do NOT touch those.

## Tests

- `status_tui` render test: a commit subject longer than `cols` produces
  MULTIPLE wrapped lines in `render_commit_detail` (all words present, none
  truncated with `…`); the sha still leads the first line.
- `clank html open --commit <sha>` resolves to `commit/<full_sha>.html`
  (short sha accepted); `--commit` + positional `plan` conflict; a
  no-built-page commit errors with the rebuild hint. (In-process, per the
  no-binary-spawn rule — call the resolver / clap parse, not the binary.)
- **Pin the spawned argv form** (guards the silent-failure risk): the EXACT
  argv the TUI builds — `["clank","html","--repo",R,"--quiet","open",S]` and
  `["clank","html","--repo",R,"--quiet","open","--commit",SHA]` — parses via
  `Cli::try_parse_from` to the expected `Html` command. If the wiring ever
  regresses to flags-after-subcommand, this test fails at build time instead
  of silently at runtime.
- `doc_nav` routes the new key to `OpenHtml` and leaves scrolling/back
  intact (pure unit test).

## Acceptance criteria

- On a plan overlay, `o` opens `plan/<stem>.html`; on a commit overlay, `o`
  opens `commit/<full_sha>.html` — without corrupting the TUI (detached,
  quiet) and without freezing the loop.
- `clank html open --commit <sha>` works from the CLI too (short sha ok);
  conflicts with a plan arg.
- A long commit subject on the commit page wraps and is fully readable; the
  sha still leads; log rows unchanged.
- Overlay hint advertises the new key; clippy at baseline; tests pass.

## Deploy

After FINISHED: `cargo install --path crates/cli --force` (no `clank setup`
— skill docs unchanged).
