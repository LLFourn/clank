# first-run-walk-per-harness
# A clean first run works on each harness, and the README claims the versions that passed

## Why

RELEASE-CHECKLIST.md §1 `first-run-walk-per-harness`. Nobody has walked
the full first run on a clean account:
1. install;
2. `clank setup`;
3. `clank agent add`;
4. `clank open`;
5. one plan through review;
6. `clank finish`.

This hasn't been done for Claude Code, Codex, Grok or OpenCode, so the
versions the release supports are unverified. The `#[ignore]` harness
acceptance tests also run only by hand.

## What changes

This plan's deliverable is **the procedure, the evidence record, and a
gate that keeps the README's claims honest**. Doing the live walks is
follow-up work for Lloyd, or for the master with Lloyd present. The plan
finishes with truthful `not-run` records; it does not wait for walks to
happen.

### 1. Isolation: what "clean" means

`setup.rs::resolve_provider_root` (445-465) gives `CLAUDE_CONFIG_DIR`,
`CODEX_HOME`, `GROK_HOME`, `OPENCODE_CONFIG_DIR` and `XDG_CONFIG_HOME`
precedence over `HOME`. Replacing `HOME` alone would therefore reuse the
production profiles. `scripts/first-run/env.sh` makes the clean
environment:
- **Starts from `env -i`.** It keeps only `PATH`, `TERM`, `LANG` and
  `USER`, and sets `HOME` to a new temp directory.
- **Remaps every provider root** inside that temp home. Each of the five
  variables above is either set to a path there, or deliberately unset
  so that it resolves under the new `HOME`. The script prints which.
- **Has no inherited session or clank identity:** it unsets every entry
  of `SESSION_IDENTITY_VARS`, and `CLANK_AGENT`.
- **Uses a disposable repo** (`git init` in the temp directory) and a
  zellij session named `clank-first-run-<harness>-<stamp>`, separate from
  any real session.
- **Authentication is never copied from a production profile.** Each
  harness logs in freshly inside the clean root with its own login flow,
  run by Lloyd. The procedure says which command each one uses, and that
  credentials are never written into the repo or the record.

### 2. The procedure: `docs/first-run-walk.md`

For each harness:
1. enter the clean environment;
2. install clank, from the release installer or `cargo install`;
3. `clank setup`;
4. `clank agent add` (or `init --team`);
5. `clank open`;
6. one plan through review;
7. `clank finish`.

Each harness also lists its **required native acceptance tests**. These
are the existing `#[ignore]`d tests that run against an installed
harness, given by exact test identity and with the environment each one
needs:

- **Claude Code:**
  - `it::claude_question_protocol::interactive_permission_question_hooks_arbitrate_with_the_local_terminal`
    (an installed Claude, a local PTY and a socket);
  - `cli::web::claude_questions::tests::installed_claude_uses_the_real_clank_hook_and_broker_end_to_end`
    (an installed Claude and a freshly built clank).
- **Codex:**
  - `it::codex_question_protocol::installed_codex_replays_questions_and_arbitrates_native_answers`
    (an installed Codex and the isolated localhost model fixture).
- **Grok:** both run with `CLANK_GROK_TEST_BIN`, in a credential-free
  process under `env -i`:
  - `cli::grok_setup::tests::installed_initialize_advertises_the_native_baseline_without_starting_a_session`;
  - `cli::grok_launch::tests::native_offline_fork_uses_production_preparation_owner`.
- **OpenCode:** no native acceptance test exists.
  `it/opencode_plugin_lifecycle.rs` and `tests/opencode_plugin_lifecycle.mjs`
  use a stub OpenCode loader and a fake clank, so they are fixture tests
  and never count as installed-version evidence. OpenCode's required
  list is recorded as `unavailable`, which blocks any "tested" claim for
  it until a native test exists.

Each test has an exact `cargo test … -- --ignored --exact <name>`
command. No selector is invented.

### 3. The evidence record: `docs/harness-support.toml`

The record is machine-readable. For each harness, with each field kept
separate:
- `source_floor`: the minimum the source enforces, from `setup.rs` and
  `grok_setup.rs`, with the file and line;
- `walks`: a list, each with the exact harness `version`, `date`,
  `outcome` (`passed`, `failed` or `not-run`), and the queued or finished
  plan names for each rough edge found;
- `required_acceptance`: the list of required test identities above, or
  the marker `unavailable`;
- `acceptance`: one entry per required test and version, each with its
  test identity, `version`, `date` and `outcome`. There is no aggregate
  row.

It lands with every harness at `not-run`.

### 4. The README is held to the record

The README's harness table gains a "tested" column. A version counts as
tested only when it has a `passed` walk **and** a `passed` acceptance
entry for **every** test in its `required_acceptance` list. Otherwise
the column says "not yet tested". An `unavailable` list never qualifies,
and an empty list is not a pass. The floor column comes from `source_floor`. A version range is
never inferred from one version.

The `it` gate `harness_support` parses both the record and the README
table. It rejects:
- a README version that doesn't match any recorded version;
- a "tested" claim whose walk or acceptance outcome is `failed` or
  `not-run`;
- a floor that differs from the source floor;
- a harness row missing from either.

It also rejects a "tested" claim:
- for a harness whose required acceptance is `unavailable`;
- for a version missing a `passed` entry for any one of its required
  tests.

Its self-tests run on fixture strings and must reject each of these: a
mismatched version, a tested claim backed by a `not-run` walk, one
backed by a `failed` walk, a tested claim for an `unavailable` harness,
and one missing one of two required acceptance tests.

**Rough-edge links name plan stems,** never file paths. A link is valid
while the plan is in the queue or active, or under `.clank/finished/`, so
it survives promotion and finishing.

## Acceptance

- `scripts/first-run/env.sh` exists. A test runs it with `--print`,
  without launching anything, and checks it remaps or unsets every
  provider-root variable and clears every identity variable.
- `docs/first-run-walk.md` contains the procedure and, for each harness,
  its exact acceptance commands.
- `docs/harness-support.toml` is committed with every harness `not-run`,
  and the README says "not yet tested" everywhere.
- The `harness_support` gate passes, and its self-tests reject the three
  forbidden claims.
- Follow-up, not a gate: Lloyd runs the walks. Each result is a record
  edit plus the README change, both checked by the gate.

## As landed

- **Implementation** (9ea09b54 after rebase):
  - **`scripts/first-run/env.sh`** builds an `env -i` environment: a
    fresh `HOME`, all five provider roots deliberately unset so they
    resolve under it, and no identity variables. It creates a disposable
    `fr-<harness>-<hex>` repo with one empty commit, and its
    `clank-fr-<harness>-<hex>` session.
    - The plan's session name, `clank-first-run-<harness>-<stamp>`, would
      exceed `SESSION_NAME_MAX` (24), so the repo basename carries the
      uniqueness.
    - `--print` launches nothing.
  - **`docs/first-run-walk.md`** gives the steps and the exact commands for
    the five ignored acceptance tests. The Grok tests run the built
    lib-test binary under `env -i`. The doc recommends a separate OS user,
    because the macOS keychain is not isolated by `HOME`.
  - **`docs/harness-support.toml`** records the source floors by file and
    line, and every walk and acceptance result as not-run. OpenCode's
    acceptance is `unavailable`.
  - **The README table** gains a "tested" column.
  - **`it/harness_support`** enforces the record against the README, and
    the floors against their source lines. It has fixture rejections for
    every forbidden claim. 12 mutations were all caught.
  - **New dev-dependency:** `toml` 1.
- **Review fixes:**
  - 39e6c292: `eval` is gone, verified with ShellCheck installed. The
    procedure now defines the master and a gate reviewer inline with
    `--tool`, and gives the `first-run-hello` plan and its lifecycle
    commands.
  - 385b2503: the finish command carries the WHY body that `clank finish`
    requires.
- **Not done here:** the live walks. These are Lloyd's follow-up. Each
  walk is a record edit plus a README change, both checked by the gate.
  - The login commands for Claude, Codex and OpenCode are unverified.
    Grok's is confirmed by `grok --help`.
- **Related, not in scope:** `cargo deny check advisories` reports 16
  existing advisories, among them bincode and rustls. CI runs only the
  licence check.
