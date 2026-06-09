# dogfood-init-setup-in-tests

Factor the CLI command handlers into env-free, args-free
**library cores** that both the `run()` handlers AND the tests
call. This fixes two problems with one change:

1. **Drift / duplicated source of truth** — integration tests
   hand-roll repo state by poking `.clank/config.json` directly
   (each test file grew its own `register_reviewer` /
   `write_repo_agents` / `write_team_config` helper writing
   JSON). That's a second model of "what a registered repo looks
   like on disk" living in the tests, which silently diverges
   from what `clank init` / `clank team` / `clank agent`
   actually write. The `teams-based-agent-registration` schema
   change broke every one of those bespoke pokers.

2. **Test speed (the bigger payoff)** — ~30 integration test
   files spawn the real `clank` binary
   (`Command::new(env!("CARGO_BIN_EXE_clank"))`) against a temp
   git repo, for setup AND for the assertion. Each test is a
   fork+exec of the binary + several `git` subprocesses +
   tempdir churn; multiply by files × tests × spawns-each =
   hundreds of process launches → the suite "takes forever."
   Most of those tests don't need a process at all — they spawn
   only because the handler bundles `read $HOME/cwd from env +
   parse clap Args + do the work + println!`, leaving no
   function to call in-process.

## The principle (lloyd 2026-06-09)

The disk changes / outputs that `init` / `setup` / `team` /
`agent` / `status` / `html` / `doctor` / `feedback` / `as` /
`finish` / `demote` / `purge` make should be **plain library
functions** — `(home, repo, params) -> Result<T>`, no `$HOME`
or cwd env reads, no clap `Args` structs, returning a value (or
a rendered view) rather than only `println!`-ing. The CLI
`run()` handler becomes a thin shell: resolve `$HOME`/cwd, parse
args, call the core, format the result.

Then:
- **Setup** in tests is a few calls to the real cores (create
  team, set master, add reviewer, init scaffold) — the genuine
  path, can't drift.
- **Assertions** call the render/query core and inspect the
  returned struct/string — in-process, milliseconds, no spawn.

## Which tests genuinely need the binary (keep spawning)

A minority — keep these as process-level tests:
- `wfw` — the watch loop, the `--timeout` → exit-2 contract,
  signal handling.
- `stop_hook` — the per-tool exit-code 0/2 wire contract.
- `agent start` — it `exec`s.
- clap-layer rejection tests (e.g. `open dry --repo` must fail
  at the parser).

Everything else should move in-process.

## Current state (half-done)

- `team.rs`'s `create` / `add` / `set_master` already take
  `home: &Path` and do the real writes — but they're private
  and still take `Team*Args` clap structs.
- `init.rs`'s `write_repo_team_field` already takes
  `(home, repo, team)`.
- `status::StatusSnapshot` already builds a value (good shape) —
  but the CLI prints it; tests spawn `clank status` and parse
  stdout/json instead of building the snapshot directly.

## Visibility: cores are `pub` in the internal lib crate (codex 14bbfd3)

Integration tests under `crates/cli/tests/` are SEPARATE crates;
they can only reach `pub` items exported by the `clank` library,
not `pub(crate)`. So the cores must be **`pub`** (not
`pub(crate)`).

That is consistent with the existing convention, not a new
surface concern: `clank`'s lib crate is internal — it backs the
binary and its own integration tests; it is NOT a published
library with a semver contract. Today's integration tests
already call `pub` lib items (`clank::cli::config::*`,
`clank::agent_store::{load_reviewer_tiers, try_resolve_via_team}`,
`clank::cli::teams_config::*`). The cores join that same
internal-pub surface. To keep it deliberate rather than
accidental, gather the test-facing entry points under a clearly
named module path (e.g. each command module's `pub fn <verb>`),
and resist exposing internal helpers — only the verbs tests need
go `pub`.

## Contract: cores are env-free + args-free, NOT pure (ruthless 14bbfd3 #2)

The cores still do file IO (read/write `.clank/`) and shell out
to `git`. "Core" here means: **no `$HOME`/cwd env reads, no clap
`Args` structs** — they take resolved `(home, repo, params)`.
The testability win is that a test passes explicit tempdir paths
and calls the core IN-PROCESS (no subprocess), not that the core
is side-effect-free. Do NOT over-engineer toward purity (no
filesystem-abstraction injection); passing resolved paths is the
whole mechanism.

## The work — phased (ruthless 14bbfd3 #1)

This touches nearly every CLI command (~12 mutation cores + ~5
query cores + thin-shelling every `run()` + ~30 test files). As
one commit it's an unreviewable mega-diff, so land it as a
SERIES of independently-reviewable phases:

- **Phase A — mutation cores + their test migration.** Extract
  `pub` cores for `team::create_team` / `set_master` /
  `add_member`, `agent::declare_global` / `add_local` /
  `remove_*` / `promote_repo`, `init::scaffold` +
  `register_repo_team`, `as_cmd::bind`, `auto::set_mode`,
  `feedback::write_entry`, `finish` / `demote` / `purge`.
  Thin-shell their `run()`. Replace the test SETUP pokers
  (`common::write_team_config`, per-file `register_reviewer` /
  `write_repo_agents`) with calls to these cores.
- **Phase B — query/render cores + their test migration.**
  `status::snapshot(repo) -> StatusView`,
  `html::render(...) -> String`, `doctor::run(repo) ->
  Vec<CheckResult>`, `agent::list_rows(repo) -> Vec<AgentRow>`,
  `open::inspect(...) -> OpenResponse`. Migrate the assertion
  side to call these in-process.
- **Phase C — cleanup.** Delete `crates/cli/tests/common/mod.rs`'s
  raw-JSON helper and any per-file pokers once nothing uses
  them; confirm only the genuinely-process-level tests still
  spawn the binary.

(Each phase may itself be split if a single command group's
diff is large. It may even be cleaner as a small series of
plans; decide at phase boundaries.)

### Prerequisite: tests must use a separate HOME from the repo (lloyd 2026-06-09)

Most integration tests' `run()` helpers set `HOME=repo`
(`.env("HOME", repo)`). That collapses user-scope
(`$HOME/.clank/config.json`) and repo-scope
(`<repo>/.clank/config.json`) onto the SAME FILE — unrealistic
(no real user has `$HOME == repo`) and the reason the repo-only
`write_team_config` shortcut was needed. It also blocks the
real-cores `register_team` (which writes both scopes — they'd
collide).

**Principle: a test's HOME must be a distinct directory from its
repo.** Fix this first (its own slice): each `run()` helper
points `HOME` at a separate tempdir, not the repo. For tests
that don't touch user-scope, an empty per-invocation tempdir is
enough; tests that adopt `register_team` own a persistent home
tempdir and thread it through `run()`. This unblocks the rest of
Phase A's setup migration.

## Coverage the migration must NOT silently drop

- **Render formats are contracts (ruthless 14bbfd3 #3).** Tests
  that assert on `clank status --json` / `clank html` stdout
  cover the SERIALIZATION shape (a consumed wire format for
  `--json`). When migrating, keep a test of the actual rendered
  string/JSON — call the render core and assert on its returned
  `String`/serialized output, NOT just the pre-render struct.
  "Assert the value" must not silently replace "assert the
  format."
- **Shell-glue (ruthless 14bbfd3 #4).** Once tests call cores
  directly, each thin `run()` (resolve `$HOME`/cwd, parse clap,
  thread args, format) is no longer exercised — and that wiring
  is exactly where a path-drop / flag-drop bug hides. Either
  keep ONE through-the-binary smoke test per command group, or
  make each shell mechanically trivial enough that inspection
  suffices. Do not migrate 100% in-process and leave every shell
  untested.

(The "which tests genuinely need the binary" list above —
`wfw`, `stop_hook`, `agent start`, clap-rejection — are the
only ones that keep spawning.)

## Why this is a follow-up, not part of the hard cut

The hard cut (`teams-based-agent-registration`) got tests green
the pragmatic way (raw-JSON `common::write_team_config` +
spawning). That works but is exactly the duplicated-model +
slow-suite smell this plan removes. Keeping it separate kept the
cut reviewable.

## Status

Stub — queued 2026-06-09 (widened to cover test speed /
handler-core factoring). Pick up after
`teams-based-agent-registration` lands.
