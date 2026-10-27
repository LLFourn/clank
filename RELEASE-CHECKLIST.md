# Release checklist

What has to happen before clank's first public release. Verified against
the source and `clank --help` at `5681c4bc`, 2026-10-27. Every unchecked
item is queueable as written: plan name, problem, shape of the fix.
"Unverified" means nobody ran the check; it is not evidence either way.

## 0. Decisions (make these before queueing the per-channel items)

- **Channels.** crates.io, prebuilt GitHub binaries (plus a `curl | sh`
  installer), Homebrew (a formula over the prebuilt binaries; no §2
  item until chosen), or a subset. Today the only path is clone +
  `cargo install --path crates/cli --locked` (README.md:67-71).
  §2 lists what each channel costs; pick before queueing any of it.
- **Platforms.** macOS arm64 is the only target built here. Linux has
  code (`crates/cli/src/proc_identity/linux.rs`) but no build or test
  run is on record: unverified. Windows: the code is unix-only, with
  `cfg(not(unix))` stubs (`cli/run.rs:20`, `cli/agent.rs:1440`,
  `cli/init.rs:331`). Declare it a non-goal or say otherwise.
- **Licence.** Which one (§1 `license-and-attribution`).
- **Supported harness versions.** Which versions the release claims
  (see the table in §1 `first-run-walk-per-harness`).
- **Hidden aliases.** README.md:480 says older spellings "still work,
  hidden" (`block create`, `unblock`, `open zellij`, `agent promote`,
  …). A first public release has no users of them: drop them
  before release or keep them a release longer.
- **The book.** Ship `book/` only in the repo (README.md:7-9 has users
  open it locally), or host it as well.

## 1. Release blockers: every channel

- [ ] **`license-and-attribution`** — there is no LICENSE file, and no
  crate declares `license` (all five `crates/*/Cargo.toml`). Shape: add
  LICENSE, set `license` (and `description`, `repository`) in every
  manifest, and ship a third-party notice. Vendored assets already carry
  their licences (`crates/web/assets/vendor/*.LICENSE`, `book/fonts/OFL-*.txt`,
  `book/vendor/asciinema-player/LICENSE`); the binary embeds the web
  assets, so the notice has to travel with the binary. Dependencies
  (`cargo metadata`, 2026-10-27): all declare a licence; the ones worth a
  look are `uluru` (MPL-2.0), `self_cell` (Apache-2.0 OR GPL-2.0-only),
  the CDLA-Permissive-2.0 and Unicode-3.0 crates. Add a `cargo deny`
  licence check to CI so a new dependency is checked automatically.

- [ ] **`ci-gates-every-push`** — there is no `.github/workflows/`.
  Reviewers run the gates by hand. Shape: one workflow on push/PR that
  provisions **git, Node, and a C compiler** (`rquickjs`, a dev-dependency
  in crates/cli/Cargo.toml) and runs `cargo fmt --all --check` (it passes
  on 2026-10-27), `cargo clippy --workspace --all-targets` (whether it
  passes today is unverified), `cargo test -p clank --lib`,
  `cargo test -p clank --test it` (which holds every source-scanning gate,
  `book_is_true` and the README/cli-audit tests in `cli::command`), and the
  other crates' tests. Rules:
  - No silent green. `opencode_plugin_lifecycle.rs:14-24` fails when Node
    is missing unless `CLANK_SKIP_NODE_HARNESS=1` is set. CI provisions
    Node and must not set that variable.
  - Run the `it` harness in an environment with no agent-session
    variables (`CLANK_AGENT` and the harness session ids). The harness
    reads them and fails ownership checks when they are set.
  - `#[ignore]` tests are deliberate opt-outs and do not run in this job:
    the live tunnel tests (`web-cloudflare/src/tunnel.rs:1068,1232,1262`)
    and the installed-harness acceptance tests
    (`it/claude_question_protocol.rs:7`, `it/codex_question_protocol.rs:337`,
    `cli/grok_setup_tests.rs:338`, `cli/grok_launch_tests.rs:342`,
    `cli/web/claude_questions.rs:805`). The CI summary says they did not
    run; it never counts them as passed.
  - Decide whether `scripts/book/test_record.py` and
    `scripts/check-timeline-ui.cjs` (needs Playwright) belong in CI or in
    a manual release step, and write the choice down.
  - Pin the toolchain (`rust-toolchain.toml`) and declare `rust-version`.
    There is none today. The code uses let-chains
    (`build_stamp_inputs.rs:161`), so the MSRV is at least 1.88; the exact
    floor is unverified.

- [ ] **`version-tags-and-changelog`** — `clank --version` prints
  `0.0.1+8d05d296` (`build_stamp_inputs.rs:133-190`). It prints the bare
  version only for a clean checkout whose HEAD is tagged `0.0.1` or
  `v0.0.1`, or for a build with no git checkout. There are no tags and no
  CHANGELOG, and all five crates sit at `0.0.1`. Shape: choose the tag
  form (`v<semver>`), bump the version, tag, and add a CHANGELOG. The
  release job parses the built binary's `clank <version>` and checks
  that `<version>` equals the tag with its leading `v` removed
  (`human_version` prints the bare package version for a `v`-prefixed
  tag), rejecting any `+sha` or `.dirty`.

- [ ] **`durable-formats-survive-upgrade`** — the formats that live across
  releases: `.clank/config.json` and `~/.clank/config.json`, the
  `.clank/` tree (plans, finished, agents/*/feedback, inbox, events,
  blocks), installed hooks and skills, and running monitors. Some of this
  is handled already:
  - Monitors retire when a different build is installed
    (`cli/monitor_keeper.rs:3`, a5004481). `clank monitor upgrade`
    (hidden) moves monitors that predate retirement.
  - The monitor protocol is versioned (`cli/monitor.rs:47`, `PROTO = 19`),
    and so is the state cache (`state_cache.rs:163`).
  - Agent launch repairs missing or older setup (d30bfc34).

  Still missing:
  - The config files have no schema version.
  - A config layer that fails to parse is skipped with a warning on
    stderr (`cli/config.rs:413-418`; shown by default through the
    `clank=info,warn` filter in `main.rs:23-31`), and the command carries
    on. After an upgrade an invalid layer leaves inherited or default
    settings in effect while the command succeeds. Shape: a layer that
    fails to parse fails the command, and `clank doctor` reports it.
  - Migrations are one-offs: `inbox::migrate` (`inbox.rs:360`), the hidden
    `block migrate`, `migrate_claude_perms` (`cli/init.rs:417`), and
    setup's hook migration.

  Shape: put a version in each durable format, migrate on read, keep a
  fixture of the previous release's bytes for each format, and make a
  config that fails to parse a loud error (`doctor` reports it as a
  failure). Say in the README which sessions survive an upgrade.

- [ ] **`post-rewrite-hook-chaining`** — when `.git/hooks/post-rewrite`
  already exists with someone else's content, `clank init` warns and skips
  it (`cli/init.rs:293-301`), and `--force-hooks` overwrites it. The hook is
  always written to `<common-dir>/hooks` (`cli/init.rs:314-320`) and
  `core.hooksPath` is never consulted (no match in `crates/cli/src`), so a
  repo using a hook manager never runs rewire. Feedback then silently fails
  to follow a rebase. Shape: honour `core.hooksPath`, and chain into a
  foreign hook idempotently instead of skipping it; `doctor` reports a hook
  that is not wired.

- [ ] **`first-run-walk-per-harness`** — walk a clean user account
  (empty `~/.clank`, no skills installed): install → `clank setup` →
  `clank agent add` (the first roster write initializes the repo,
  ba44b1cf) or `clank init --team <name>` → `clank open` → one plan
  through review → `clank finish`. Do it once per harness. Every rough
  edge becomes its own plan. None of these harnesses was launched for this
  checklist, so compatibility is unverified. What the source requires,
  next to the versions installed here on 2026-10-27 (`--version`):

  | harness | floor in source | installed 2026-10-27 |
  |---|---|---|
  | Claude Code | ≥ 2.1.223 for async-rewake hooks, else legacy loop (`cli/setup.rs:1574`) | 2.1.308 |
  | Codex CLI | no version gate | 0.164.0 |
  | OpenCode | major ≥ 2 (`cli/setup.rs:362`); version read only from Homebrew/npm installs, else "unknown" (`cli/setup.rs:395-415`) | 2.1.4 |
  | Grok | 1.x ≥ 1.0.46 with an advertised Stop gate (`cli/grok_setup.rs:100-110`) | 1.0.70 |
  | zellij | needs `override-layout`; 0.45 lacks it (`cli/doctor.rs:388-396`) | 0.46.0 |

  The walk also runs the `#[ignore]` acceptance tests above against each
  installed harness and records the versions that passed. That record is
  what the README states as supported.

- [ ] **`docs-match-the-release`** — the README and the book are gated
  (`it/book_is_true.rs`, and the README reference test in
  `cli/command.rs:531`), but these are not:
  - `clank setup`'s summary says "~/.claude, ~/.codex, ~/.grok"
    (`cli/command.rs:195`), but setup also installs OpenCode
    (`cli/setup.rs:297`).
  - `open-worktree.sh` duplicates `clank fork create` plus
    `clank open --fork`. Its header names `clank open --json`, which no
    longer exists; it invokes the hidden `open dry --json`
    (README.md:477-478). Delete the script, and `open dry` with it if
    nothing else uses it.
  - `GROK-INTEGRATION.md` is a top-level design note that README.md:762
    links as "the integration contract". Fold it into the README or the
    book, or move it under `docs/`.
  - `docs/cli-audit.md` is kept true by tests (`cli/command.rs:880-960`),
    but its follow-up plans have all finished (`.clank/finished/cli-*.md`).
    Keep it as a record or delete it, along with its tests.
  - `scripts/migrate-feedback-to-agents.sh` is a one-off from 2026-05-23
    (6730f008). Delete it unless some pre-release repo still needs it.
  - The README Install section (README.md:65-130) is rewritten for the
    chosen channels.

- [ ] **`security-review-for-release`** — what is known from the source:
  - Listeners bind to 127.0.0.1 only (`web-cloudflare/src/host.rs:89`).
  - The remote is off until someone turns it on in the TUI, and the book
    states that Cloudflare terminates its TLS (`book/remote.html:75-91`).
  - Remote sessions and links are stored hashed
    (`web-cloudflare/src/door.rs:25-26,76-81`).
  - Diagnostics redact bearer tokens (`cli/source_diagnostic.rs:303`).
  - GitHub credentials come from `GH_TOKEN`/`GITHUB_TOKEN` or
    `gh auth token` (`cli/github/session.rs:11`).
  - The `hooks.blocked` command runs through `sh -c` (`cli/block.rs:192`),
    but only from untracked config (`.clank/.gitignore` tracks only
    `plans/`, `finished/` and `.gitignore`).

  Open questions:
  - The remote token is stored in plaintext in `~/.clank/config.json`
    (`door.rs:297`), which any agent running as the user can read.
  - GitHub event bodies from third parties reach agent context, a
    prompt-injection path: automatically, because a wake carries the
    whole event body (`cli/wait.rs:3029-3069`), and through
    `clank events read`.
  - Is anything sensitive written into `clank html` output?

  Shape: a short SECURITY.md that names the threat model and these
  exposures, plus a fix or a documented acceptance for each one.

## 2. Release blockers: per channel (only for the channels chosen in §0)

### crates.io

- [ ] **`crates-io-packaging`** — three problems:
  - `clank` depends on four path crates with no `version`, two of them
    `publish = false` (`crates/core`, `crates/user-config`). By Cargo's
    rules it cannot be published as it stands (`cargo publish --dry-run`
    has not been run).
  - `build.rs:4-9` assumes the crate sits two levels below a workspace. In
    a registry tarball that path is `~/.cargo/registry/src`, so
    `stamp_files` reads nothing. Every crates.io build made with the same
    toolchain and features would then have the same stamp, and monitors
    would not retire across upgrades (`build_stamp.rs:1-7`). This is read
    from the source; no tarball build was run.
  - The crate name `clank` may already be taken on crates.io (unverified).

  Shape: publish the internal crates with versions or fold them into the
  main crate, make the build stamp work without a workspace (hash the
  package's own files plus `CARGO_PKG_VERSION`), and claim the name.

### Prebuilt binaries

- [ ] **`release-binaries-and-installer`** — a workflow, triggered by a
  tag, that builds `clank` and `clank-web-cloudflare` for each target
  chosen in §0. The TUI runs the second as a sibling process, so the two
  ship together (crates/cli/Cargo.toml `[[bin]]`). The workflow runs §1's
  gates on each target, checks `--version` against the tag, and attaches
  the archives and checksums. A `curl | sh` installer picks the right
  archive and puts both binaries on `$PATH`. macOS binaries also need a
  signing and notarization decision.

## 3. After the first release

- [ ] **`zellij-focus-follower`** — it is hard to see which agent is
  acting. Nothing follows focus today: `focus-pane-id` in
  `cli/open_zellij.rs` only places panes and restores focus. Shape: a watcher that maps the monitor's "whose
  turn" to `zellij action` focus or tab changes, built on what `clank tui`
  already reads.
- [ ] **`harness-acceptance-job`** — the `#[ignore]` acceptance tests run
  only by hand. Shape: a scheduled job, with pinned harness versions and
  credentials, that runs them and publishes which versions passed.
- [ ] **`retire-hidden-aliases`** — only if §0 keeps them: remove the
  hidden spellings after one release.

## Done (evidence)

- `help-output-beauty`: bd470f4a (one-line summaries in sections),
  74d7a8e5 (long help says what a command is for).
- `readme-rewrite`: the pitch, mental model and demo moved into the book
  (3d379512). README.md:7-9 sends newcomers there, and the README is the
  reference; Quickstart is at README.md:132.
- `tutorial-screencast-content`: six casts recorded from real runs
  (`book/casts/*.js`, `scripts/book/scenarios/`), 3d379512.
- core/cli boundary holds: `crates/core/Cargo.toml` depends only on
  serde, serde_json and an optional wincode, and nothing in
  `crates/core/src` uses `std::fs`, `std::process` or `std::net`. The
  claim that it "compiles to wasm32" is unverified: there is no wasm32
  target or CI.
- The fold stays pure: it lives in `crates/core/src/repo_state.rs`. The
  cli wrapper (`crates/cli/src/repo_state.rs:1`) adds only identity, and
  the I/O is in `crates/cli/src/rebuild.rs`.
- Reviewers are no longer woken on finish:
  `.clank/finished/finish-does-not-wake-reviewers.md` and
  `gate-reviewers-only-plan-change-and-finish.md`.
- Bare `clank init` adopts the `default` team (`clank init --help`;
  `.clank/finished/bare-init-uses-the-default-team.md`).
- `clank tui` and zellij layout chrome (`clank tui` in `--help`;
  `clank_agents` in `cli/open_zellij.rs`;
  `.clank/finished/zellij-layout-config-around-agent-panes.md`).
- Control-work wake items are one-line hints
  (`.clank/finished/wfw-output-is-a-minimal-hint.md`,
  `claude-stop-hook-minimal-hint.md`). Event wakes deliberately carry the
  whole event content, a multiline body on indented follow-on lines
  (`cli/wait.rs:3029-3069`, `.clank/finished/tui-github-event-content.md`).
- Version string: 8d05d296. `--locked` install: 32c162f3. Monitors follow
  the installed build: a5004481.
