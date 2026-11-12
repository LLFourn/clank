# The first-run walk

A harness version counts as tested only when a person has walked clank's
whole first run on it from a clean account, and its native acceptance tests
have passed against that same installed version. This is the procedure. Its
results go in [`harness-support.toml`](harness-support.toml), and the
README's harness table may claim only what that record backs: the
`harness_support` test (`crates/cli/tests/it/harness_support.rs`) rejects
anything else.

The walk is done once per harness: Claude Code, Codex, Grok and OpenCode.

## What "clean" means

`scripts/first-run/env.sh <harness>` opens a shell that:

- starts from `env -i`, keeping only `PATH`, `TERM`, `LANG` and `USER`;
- sets `HOME` to a new directory under `$TMPDIR`, with its own
  `.local/bin` first on `PATH`;
- leaves `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GROK_HOME`,
  `OPENCODE_CONFIG_DIR` and `XDG_CONFIG_HOME` unset, so each provider root
  resolves under the new `HOME`. Setting `HOME` alone would not be enough:
  each of these variables takes precedence over it;
- has no session or clank identity: every variable in
  `SESSION_IDENTITY_VARS` (`crates/cli/src/agent_env.rs`) is unset,
  `CLANK_AGENT` among them;
- starts in a disposable git repo, `fr-<harness>-<stamp>`, so `clank open`
  names its zellij session `clank-fr-<harness>-<stamp>`, apart from any
  real session. Zellij session names are limited to 24 characters, so the
  stamp is six random hex digits.

`scripts/first-run/env.sh --print <harness>` prints the paths and the
environment it would use, and creates and launches nothing.

Nothing is copied from the real `HOME` or from a production profile. Each
harness logs in afresh inside the clean root:

| harness | login |
|---|---|
| Claude Code | run `claude` and complete its first-run sign-in (`/login` inside the session runs it again) |
| Codex | `codex login` |
| Grok | `grok login` |
| OpenCode | `opencode auth login` |

Credentials never go in this repository, in the walk's repo, or in
`harness-support.toml`. A harness that keeps its credential outside `HOME`
(the macOS login Keychain, for one) is not isolated by a new `HOME`: a fresh
login there can replace the real account's credential. Walk on a separate
OS user account or machine when that matters.

## The procedure

For each harness:

1. **Enter the clean environment:** `scripts/first-run/env.sh <harness>`,
   then log in with that harness's command above.
2. **Install clank**, from the release installer:

   ```sh
   curl -fsSL https://llfourn.github.io/clank/install.sh | sh
   ```

   or from a checkout, into the clean `HOME` rather than the real
   `~/.cargo/bin`:

   ```sh
   cargo install --locked --path <checkout>/crates/cli --root "$HOME/.local"
   ```

   The clean environment has no `RUSTUP_HOME` or `CARGO_HOME`, so the cargo
   route needs `RUSTUP_HOME` pointed at an installed toolchain. Check that
   `command -v clank` is under `$HOME/.local/bin`, not a real install
   further down `PATH`.
3. **`clank setup`**, then `clank doctor`.
4. **Define the roster** in the disposable repo, which `env.sh` created
   with one empty commit. The first `agent add` initializes clank there.
   The clean `HOME` has no agent library or team, so both agents are
   defined inline with `--tool`. Substitute the harness under test
   (`claude`, `codex`, `grok` or `opencode`) for `<harness>`:

   ```sh
   clank agent add master --tool <harness>
   clank agent add reviewer --tool <harness> --review gate
   clank agent list
   ```

   `master` becomes the master because the roster has none. `reviewer` is
   a gate reviewer, so it reviews both the plan and the finish.
5. **`clank open`.** It opens both agents' panes in the
   `clank-fr-<harness>-<hex>` zellij session that `env.sh` printed. Log
   each pane's harness in if it asks, using step 1's login.
6. **One plan through review.** From a shell in the repo:

   ```sh
   mkdir -p .clank/drafts
   cat > .clank/drafts/first-run-hello.md <<'PLAN'
   # first-run-hello

   ## What changes

   Add `HELLO.md` containing the single line `hello`.

   ## Acceptance

   - `HELLO.md` exists and contains `hello`.
   PLAN
   clank queue add first-run-hello
   clank queue promote first-run-hello
   ```

   The master is woken with the promoted plan. Once the reviewer's intro
   verdict allows it, the master commits
   `[first-run-hello] HELLO.md says hello`, and the reviewer reviews that
   commit. `clank status` shows the gate the whole way. The walk passes
   this step when the reviewer writes `FINISHED` and the master is woken
   to finalize. Every wake should arrive unprompted. If you have to type
   anything into a pane to make progress, record that as a rough edge.
7. **`clank finish`.** The master runs
   `clank finish first-run-hello -m "[first-run-hello] HELLO.md says hello" -m "A first-run plan that exercises review and finalization on a clean account."`.
   The second `-m` is the required WHY body: `clank finish` refuses a
   message that is only a subject.
   Afterwards `.clank/finished/first-run-hello.md` exists, and
   `clank status` shows no active plan.

Each rough edge found becomes its own plan. Record the walk in
`harness-support.toml` as a `[[harness.walks]]` entry: the exact
`<harness> --version`, the date, the outcome (`passed`, `failed` or
`not-run`), and in `rough_edges` the stem of each plan filed (never a file
path). A stem stays valid while that plan is queued, active or under
`.clank/finished/`.

## The native acceptance tests

Each harness's required tests are the existing `#[ignore]`d tests that run
against an installed harness. Run them from a clank checkout at the commit
being released, against the same installed version the walk used, in a
shell with no session identity:

```sh
unset CLAUDE_CODE_SESSION_ID CODEX_THREAD_ID OPENCODE_SESSION_ID GROK_AGENT GROK_SESSION_ID CLANK_AGENT CLANK_BOOTSTRAP_SESSION_ID
```

Record each as a `[[harness.acceptance]]` entry: the test identity exactly as
`required_acceptance` lists it, the version, the date and the outcome. A
version needs a passed entry for every required test; there is no aggregate
row.

### Claude Code

`it::claude_question_protocol::interactive_permission_question_hooks_arbitrate_with_the_local_terminal`
needs the installed Claude, a local PTY and a socket:

```sh
CLANK_CLAUDE_PROTOCOL_BINARY="$(command -v claude)" \
  cargo test --locked -p clank --test it -- --ignored --exact claude_question_protocol::interactive_permission_question_hooks_arbitrate_with_the_local_terminal
```

`cli::web::claude_questions::tests::installed_claude_uses_the_real_clank_hook_and_broker_end_to_end`
needs the installed Claude, a freshly built clank, and the shared Rust process
fixture (no Python). The fixture is test-only and is not installed with clank:

```sh
cargo build --locked -p clank --features test-runtime --bin clank --bin clank-test-fixture
CLANK_CLAUDE_PROTOCOL_BINARY="$(command -v claude)" \
CLANK_QUESTION_BINARY="$PWD/target/debug/clank" \
CLANK_PROCESS_FIXTURE="$PWD/target/debug/clank-test-fixture" \
  scripts/test.sh --locked -p clank --lib -- --ignored --exact cli::web::claude_questions::tests::installed_claude_uses_the_real_clank_hook_and_broker_end_to_end
```

### Codex

`it::codex_question_protocol::installed_codex_replays_questions_and_arbitrates_native_answers`
needs the installed Codex; it talks to an isolated localhost model fixture:

```sh
CLANK_CODEX_PROTOCOL_BINARY="$(command -v codex)" \
  cargo test --locked -p clank --test it -- --ignored --exact codex_question_protocol::installed_codex_replays_questions_and_arbitrates_native_answers
```

### Grok

Both Grok tests refuse to run unless their process environment holds only
`PATH` and `CLANK_GROK_TEST_BIN`, so no credential can reach them. `cargo`
and `rustup` add their own variables to the processes they run, so the test
binary is built first and then run directly under `env -i`:

```sh
cargo test --locked -p clank --lib --no-run
# Note the "Executable unittests src/lib.rs (target/debug/deps/clank-<hash>)" line.
LIB_TESTS=target/debug/deps/clank-<hash>
GROK_BIN="$(command -v grok)"
```

`cli::grok_setup::tests::installed_initialize_advertises_the_native_baseline_without_starting_a_session`:

```sh
env -i PATH="$PATH" CLANK_GROK_TEST_BIN="$GROK_BIN" \
  "$LIB_TESTS" --ignored --exact cli::grok_setup::tests::installed_initialize_advertises_the_native_baseline_without_starting_a_session
```

`cli::grok_launch::tests::native_offline_fork_uses_production_preparation_owner`:

```sh
env -i PATH="$PATH" CLANK_GROK_TEST_BIN="$GROK_BIN" \
  "$LIB_TESTS" --ignored --exact cli::grok_launch::tests::native_offline_fork_uses_production_preparation_owner
```

### OpenCode

No native acceptance test exists. `crates/cli/tests/it/opencode_plugin_lifecycle.rs`
and `crates/cli/tests/opencode_plugin_lifecycle.mjs` run a stub OpenCode loader against a
fake clank, so they are fixture tests and never count as evidence for an
installed version. OpenCode's `required_acceptance` is recorded as
`unavailable`, and no OpenCode version can be claimed as tested until a
native test exists.

## Updating the README

When a version qualifies, list it in the README harness table's `tested`
column (versions separated by `, `), and keep the floor column equal to the
record's `source_floor.readme`. `cargo test -p clank --test it
harness_support::` checks both. A version range is never inferred from one
version.
