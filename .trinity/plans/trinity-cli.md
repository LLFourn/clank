# trinity-cli

Add the operator-facing CLI binary: `trinity init`, `trinity
finish`, `trinity purge`. The daemon never writes Trinity
artifacts — the CLI does, via local `git` subprocess calls. The
CLI reads projected state (gate, attribution, finished-ness)
from the daemon over HTTP and writes commits locally.

## Why

Today an operator finishing a plan does it by hand: `mkdir
.trinity/finished/<stem>/`, copy the approving feedback file in,
`git add`, `git commit -m "Finalize <stem>"`. I just made the
mistakes that motivate this plan two messages ago in this
session: moved the plan file to `.trinity/plans/done/` (which
isn't even a real path), and only after the user prompted did I
realise the right ceremony is to copy the latest approving
feedback into `.trinity/finished/<stem>/<author>.md`.

A CLI that knows the ceremony eliminates that class of mistake.
It also unlocks the other two commands:

- `trinity init` — bootstrap `.trinity/` and the gitignore line
  for a new repo without leaving the operator to read source for
  the right shape.
- `trinity purge` — strip a plan's `.trinity/` artifacts from
  history. Needed when a plan was abandoned, when a repo wants
  to upstream code without the Trinity trace, or when an
  operator squashed a plan into one commit and wants no
  externally visible Trinity trail. Today this is multi-step
  `git filter-branch` / `git rebase -i` work; the daemon's
  attribution walk already knows which commits belong to a
  plan, so a single command can drive the rewrite.

## Non-goals

- **Daemon-side mutations.** The daemon adds read-only typed
  projection endpoints (`finish_preview`, `rewrite_preview`)
  but never writes Trinity artifacts, never runs mutating git
  operations, and never edits the working tree. All
  mutations — file copies, `git add`, `git commit`, tree
  rewrites, ref updates — happen in the CLI from data the
  daemon previewed.
- A `trinity wfw` CLI — separate stub `trinity-wfw-cli.md`.
- Integrating the CLI into post-commit hooks or auto-WFW —
  separate stub `agent-automation-hooks.md`.
- Persistent CLI configuration. Every invocation reads the live
  daemon state and worktree.
- A historical-attribution endpoint for plans no longer
  visible in the fold (see `--recover-from-history` note
  under `trinity purge`).

## Dependencies

Existing daemon surface, all already shipped:

- `disk_format::parse_finalize_path`, `finalize_first_line_starts_with_approve`,
  and `disk_snapshot::finalize_rule_satisfied` — the finished
  rule's parser and validator.
- `git_io::read_finalize_snapshot` — reads `.trinity/finished/<stem>/`
  at any revision.
- `attribution.rs` — per-commit `Attributed { session, plan_touch,
  has_code_changes }` classification.
- `projection::all_plan_revisions` and `all_implementation_commits`
  — commit ranges per plan.
- HTTP `/api/plan/{repo}/{stem}.md` — gate state, worktree status.

Two new daemon endpoints — both **pure projections over the
existing fold**, no new fold state — provide everything the
CLI needs:

- `GET /api/plan/{repo}/{stem}.md/finish_preview` — gate state,
  latest reviewable sha, dirty-worktree status, plan
  finished-ness, AND the **exact sealed-approval set** to
  copy: `[{author, source_path, verdict, body_hash}]`. The CLI
  copies only these files, re-reading each and checking the
  hash before sealing. A feedback file changing between
  preview and commit fails the hash check and aborts.

- `GET /api/plan/{repo}/{stem}.md/rewrite_preview?include_finalize=…`
  — the full operation manifest for `purge` / `squash` /
  `--dry` modes:
  - commit range (intro sha → end sha) walked
  - per-commit disposition: `drop | keep_verbatim | rewrite`
  - foreign commits in the range (so `--squash` can refuse
    cleanly)
  - per-`rewrite` commit: the strip-path set (what gets
    removed from the new tree)
  - resulting parent chain shape
  - target-branch operation: `update_current | new_branch`

  **Data sources.** Attribution comes from the fold
  (`attribution.rs`). The strip-path set for `rewrite`
  commits requires per-commit tree inspection — which entries
  under `.trinity/` for this plan exist in that commit's
  tree — and the fold doesn't store full tree contents, so
  the endpoint performs **read-only git inspection at request
  time** (`git ls-tree`, `git diff-tree`) for the in-range
  shas. The classification rules themselves (drop/keep/
  rewrite, foreign-commit definition, what counts as a
  plan-`.trinity/` path) live in typed code that's
  unit-testable against fixture commit data — no daemon-side
  branch state, no working-tree dependence.

  The CLI is a thin executor of this manifest. The daemon
  owns the classification model; the CLI owns the
  side-effects.

  `--include_finalize` toggles whether the latest finalize
  commit is in the strip set (controls `--purge` vs `--squash
  --purge` vs `--drop-finalize`).

## CLI binary wiring

`trinity` already has subcommands `serve` and `mcp` in
`src/main.rs`. Add `init`, `finish`, `purge` alongside. Each
dispatches into a module under `src/cli/` so the binary stays a
thin entry point.

Daemon transport: the CLI talks to `http://127.0.0.1:<port>` by
default; `--daemon-url` overrides; `TRINITY_DAEMON_URL` env var
overrides the default. If the daemon isn't reachable and the
command needs projected state, the CLI errors with a clear
message. `trinity init` is the one exception — it works
without a daemon because it only touches the filesystem.

## `trinity init`

```text
trinity init [--repo <path>]
```

Creates, in this order:

1. `.trinity/plans/` (empty directory marker not needed; git
   tracks files, not directories).
2. `.trinity/.gitignore` containing:
   ```
   feedback/
   cache/
   ```

   Mirrors the existing convention (see the repo-root
   `.gitignore` in this tree). Drift between init output and
   established convention would be confusing.

That's it. No `--ignore` mode appending to the repo-root
gitignore — pick one shape and ship it. Trinity's own
`.gitignore` keeps feedback files out of history; this is the
documented convention.

Pre-flight:

- Refuse if `.trinity/.gitignore` exists with different content.
- Detect coverage by a parent `.gitignore` or
  `core.excludesFile` and warn if `.trinity/` (the whole tree)
  is excluded — that would hide tracked plan files too.
- Optionally call a running daemon to register the repo so
  `start_plan` works immediately; absence of daemon is not an
  error.

## `trinity finish`

The finalize ceremony. The daemon's finished rule is "plan file
exists AND `.trinity/finished/<stem>/` contains ≥1 file AND
every file starts with APPROVE." The CLI's job is to make sure
the APPROVE that lands there is one the operator would
actually stand behind, and to handle the common
amend/squash/purge variants without dropping into `git rebase
-i`.

### Bare

```text
trinity finish [<plan>]
```

`<plan>` accepts either the plan id (`<repo>/<stem>.md`) or
just the stem. Optional if the cwd-repo has exactly one
in-flight active plan.

Pre-flight (all checked against the daemon's `finish_preview`):

- Plan exists, plan file present in HEAD.
- At least one reviewable commit attributed to this plan
  (`PlanOnly | CodeOnly | Mixed`).
- Live gate for the latest reviewable commit is fully approved:
  ≥1 APPROVE, 0 REQUEST_CHANGES, 0 unmarked, every voting
  participant approved.
- All reviewers in the gate parse as valid Trinity participants.
- Working tree clean.
- Daemon reachable.

On success:

- Wipe any existing `.trinity/finished/<stem>/` contents.
- For each entry in `finish_preview.sealed_approvals`
  `{author, source_path, body_hash}`: re-read the source file,
  recompute the hash, abort with a clear error if the hash
  drifted (someone edited feedback between preview and commit
  — the operator should re-poll), otherwise copy the body to
  `.trinity/finished/<stem>/<author>.md`.
- `git add .trinity/finished/<stem>/`.
- `git commit -m "Finalize <stem>"` (override with `-m`).

The hash check is what makes "the approval the operator would
stand behind" mean anything stronger than "whatever's on
disk." The daemon already projected the gate as approved with
specific file contents; the CLI seals exactly that set.

Idempotent: invoking on an already-finished plan with no new
reviewable commits prints "already finished" and exits 0.

### Flags

`--amend`: HEAD must be a finalize commit for this plan; same
pre-flight, then `git commit --amend` with the recomputed
`.trinity/finished/<stem>/` tree. Used when you ran `trinity
finish`, then realised you wanted `--squash` or `--purge`.

`--squash "<message>"`: collapse every plan-attributed commit
from the plan's intro to HEAD into a single commit with the
supplied message, then add the finalize tree on top (or fold
the finalize tree into the squash if the operator passes
`--squash --purge` — see below). Refuses if any foreign commit
sits between plan-attributed commits in the range.

A **foreign commit** here is any commit not exclusively
attributed to this plan: another plan's commits, unattributed
commits, or cross-plan mixed commits. `--purge` can handle
cross-plan mixed (it strips selectively); `--squash` cannot
(squash is all-or-nothing on tree content), so it refuses and
suggests plain `--purge` with `--squash`.

`--squash --purge "<message>"`: same range, but the recommitted
tree omits everything under `.trinity/` for this plan. One
commit, only the code diff, no Trinity trace.

`--purge`: preserve per-commit history, but rewrite each
plan-attributed commit to strip its `.trinity/` content. Mixed
commits keep code, drop `.trinity/` paths; pure plan commits
drop entirely; pure code commits keep verbatim. The shared
history-rewriting engine — see below.

`--dry`: see "Dry-run" section.

## `trinity purge`

The `--purge` machinery without a finalize commit.

```text
trinity purge [<plan>] [--squash <msg>] [--amend]
              [--drop-finalize]
              [--into-branch <name>]
              [--dry]
              [--yes] [--allow-rewrite-protected]
```

Use cases:

- plan abandoned mid-flight, leave no trace
- post-finalize cleanup the original `trinity finish` didn't
  ask for
- exporting code without `.trinity/` for an external consumer

Plan argument resolution matches `trinity finish`: id or stem,
optional if cwd-repo has exactly one in-flight plan.

**Scope of purge by plan id**: only plans the daemon can still
project — i.e. the plan file is present at HEAD or in the
fold's recent commit-attribution walk. The daemon's
`rewrite_preview` is the source of truth here. If the plan
was fully deleted from HEAD and pre-dates the fold's
attribution window, `purge` refuses with a clear "this plan
is not projectable; use `trinity purge --recover-from-history
<intro-sha>`" message. **`--recover-from-history` is out of
scope for this plan** — adding it means a daemon historical-
attribution endpoint that walks git outside the fold. The
common case (recently-abandoned plans still in HEAD's tree
or in the recent commit-graph) is supported; the recovery
case is a follow-up.

Flags:

- `--squash "<message>"`: collapse plan-attributed commits into
  one (same interleaving rules as `trinity finish --squash`).
- `--amend`: amend HEAD if HEAD touches this plan's artifacts.
- `--drop-finalize`: also strip `.trinity/finished/<stem>/` if
  HEAD contains it. The engine adds the finished path to the
  per-commit strip set. Legal with `--amend`; redundant with
  `--squash` (squash already strips everything `.trinity/`).
- `--into-branch <name>`: don't rewrite the current branch.
  Build the rewritten history on a new branch named `<name>`
  starting from the same root, then leave the current branch
  alone. Safest mode — the operator can diff/inspect/cherry-
  pick before deciding to overwrite the current branch. Creates
  the branch atomically with `git update-ref refs/heads/<name>
  <new-tip>`; refuses if the branch already exists.
- `--yes`: skip interactive confirmation.
- `--allow-rewrite-protected`: opt-in for branches the CLI's
  protected-branch detection refuses by default (`main`,
  `master`, branches matched by `branch.<name>.protect` or
  similar — exact heuristic TBD during impl).

Safety pre-flights:

- Refuse on dirty working tree.
- Refuse on protected branch without
  `--allow-rewrite-protected` (unless `--into-branch` is set —
  that mode doesn't touch the protected branch).
- Refuse if `.trinity/finished/<stem>/` exists in HEAD and
  neither `--squash` nor `--drop-finalize` is supplied — bare
  purge in that state strips the history the snapshot
  documents and leaves the snapshot orphaned. Operator must
  opt in explicitly.
- Prompt for confirmation by default; `--yes` skips.

## Dry-run mode (`--dry`)

`--dry` works on every command that would mutate (`finish`,
`finish --amend`, `finish --squash`, `finish --purge`, `finish
--squash --purge`, `purge`, `purge --squash`, etc.).

Output shape:

```text
trinity finish foo --squash "Implement foo" --dry

would squash 4 commits into 1:
  abc1234  PlanOnly       Plan: foo
  def5678  Mixed          Plan revision: address codex on abc1234
  9012abc  CodeOnly       Implement foo
  3456def  PlanOnly       (finalize tree would land here)

resulting commit:
  message: Implement foo
  tree:    same as HEAD, plus .trinity/finished/foo/<author>.md

would NOT touch:
  - any commit before abc1234 (parent: parent-of-abc1234)
  - any branch other than master
```

For `--purge` mode, the listing shows per-commit disposition
(`drop` / `keep verbatim` / `rewrite`) and the resulting
parent chain. The engine builds its full rewrite plan; `--dry`
just stops before any `git commit-tree` / `git update-ref`
call. Best-effort — if a step would fail (e.g. dirty worktree,
foreign commit in range), the dry-run reports the same failure
the live run would, just earlier.

## History-rewriting engine (shared)

`trinity finish --purge`, `trinity finish --squash --purge`,
and `trinity purge` share one engine.

Walk the plan's commit range (intro → HEAD or intro →
`<branch tip>`). For each commit:

- **Drop**: tree only changed `.trinity/` paths for this plan.
  Skip entirely; parent chain hops over it.
- **Keep verbatim**: didn't touch `.trinity/` for this plan.
  Reuse SHA as-is in the parent chain.
- **Rewrite**: touched both `.trinity/` (for this plan) and
  non-`.trinity/` (or other plans' `.trinity/`). Build a new
  tree object omitting this plan's `.trinity/` entries; reuse
  author, message, timestamp; parent is the previous rewritten
  commit.

Implementation: `tokio::process::Command` wrapping git
plumbing — same shape as the rest of the daemon's git I/O. No
`libgit2`/`gix` dependency. Build new trees with `git ls-tree`
+ `git mktree` (or `git read-tree -i` + `git write-tree`),
build commits with `git commit-tree`, update the branch with
`git update-ref` at the end. Explicitly avoid `git
filter-branch` (deprecated) and `git filter-repo` (external
dep).

Engine pre-flight refusals:

- Range contains a merge commit (merge tree rewriting is out
  of scope).
- Working tree dirty.
- Plan's intro commit (from the daemon's `rewrite_preview`)
  is not reachable from the current branch's tip
  (`git merge-base --is-ancestor <intro> HEAD`). This is the
  git-checkable replacement for "plan start branch" — the
  daemon's fold doesn't track which branch a plan was started
  on, so we phrase the refusal in terms the operator's git
  state can answer.

In `--into-branch` mode the engine writes the rewritten chain
to a fresh branch ref instead of `update-ref`-ing the current
branch; everything else is identical.

## Testing

The history-rewriting engine is the load-bearing piece. Each
case constructs a small repo with the named shape and asserts
the post-rewrite history.

Per-commit shape cases:

1. Pure plan-only commit → dropped.
2. Pure code commit, plan-attributed via walk-back inheritance
   from a plan-touching parent → kept verbatim.
3. Mixed (code + plan revision) → rewritten: code preserved,
   plan-file removed, message/author/timestamp preserved.
4. Sequence of two mixed commits then one pure-code commit →
   c1' → c2' → c3 (c3 verbatim).
5. Interleaved foreign commit (c1 plan, c2 foreign, c3 mixed)
   → c1 dropped, c2 kept (parent = c1's parent), c3 rewritten.

Range/flag cases:

6. Finalize commit at HEAD under `--purge` → kept; under
   `--squash --purge` → dropped; under `trinity purge
   --drop-finalize` → dropped.
7. Two active plans (foo, bar). `trinity finish foo --purge`
   leaves bar's commits verbatim; mixed foo+bar commits
   rewritten to strip only foo.
8. Re-running `--purge` after `--purge` → idempotent.
9. `--amend --purge` on a finalize commit → amends HEAD to
   also strip HEAD's parent if it's a plan-touching commit;
   otherwise refuses (amend only rewrites HEAD).
10. `--into-branch newbr` creates `newbr` with the rewritten
    chain; current branch is unchanged; `newbr` refuses to
    overwrite if it already exists.

Refusal cases:

11. Working tree dirty → refuse.
12. Merge commit in range → refuse with clear message.
13. Orphan-finalize-snapshot under bare `trinity purge` →
    refuse, suggest `--squash` or `--drop-finalize`.

Dry-run cases:

14. `--dry` on a squash prints the commit listing and target
    commit shape; makes zero ref updates and zero commits.
15. `--dry` on a purge that would fail (e.g. merge in range)
    prints the same failure the live run would.

Plus the obvious init/finish happy-paths and the daemon-
unreachable error case.

`cargo test --workspace --exclude trinity-frontend`,
`cargo clippy --all-targets`, `cargo fmt -- --check`.

## Dev workflow: dogfood via `cargo install`

The dogfood loop starts **before** Phase 1 implementation
begins, not after it ships. First action: kill the running
`trinity serve` daemon, `cargo install --path . --locked`,
restart the daemon, and confirm `trinity --version` reports
something. From that point every phase commit is followed by
the same cycle: bump `Cargo.toml`, `cargo install`, restart
the daemon if `serve` changed.

The CLI is the tool we want to use; running it through `cargo
run` keeps us from noticing UX cliff-edges. Concretely:

- **Bump `Cargo.toml`'s `version` on every phase ship**, minor
  (`0.0.1` → `0.1.0` → `0.2.0` …). Patch bumps for follow-up
  fixes on the same phase. This is the user-visible signal
  that a `cargo install` is worth running.
- **`trinity --version` is a real flag.** Phase 1 wires it via
  `clap`'s built-in version support (reads `CARGO_PKG_VERSION`).
  Cheap, no maintenance.
- **Operator playbook (in the README) after pulling:**

  ```sh
  trinity --version              # what's installed?
  grep '^version' Cargo.toml     # what's in the tree?
  # mismatch → cargo install --path . --locked
  just restart                   # restart daemon to pick up server changes
  ```

  The `just restart` step is for the daemon (`trinity serve`);
  CLI binary updates need the reinstall step but not a daemon
  restart. The README should call this out.

Not part of the plan: a CI publish to crates.io, a homebrew
formula, anything `cargo install` from a non-`--path` source.
Dogfooding the local binary is the goal; public packaging is
later work.

## Acceptance criteria

- `trinity --version` reports the installed `Cargo.toml`
  version. Each phase ships with a minor-version bump.
- `trinity init` scaffolds `.trinity/plans/` and
  `.trinity/.gitignore` containing both `feedback/` and
  `cache/`; refuses to overwrite a different existing
  `.trinity/.gitignore`; warns on `.trinity/` already being
  globally excluded.
- `trinity finish` performs the full ceremony from an approved
  gate to a committed finalize tree, with `--amend`,
  `--squash`, `--squash --purge`, and `--purge` variants.
- `trinity purge` runs the history-rewriting engine without a
  finalize commit, accepts plan id or stem, supports
  `--into-branch`, `--drop-finalize`, `--amend`, `--squash`,
  and `--dry`.
- `--dry` on any mutating command prints the planned action
  shape and exits 0 with no commits and no ref updates.
- The CLI never re-derives projection state — every "is this
  plan finished?" / "what's the latest reviewable sha?" /
  "give me the plan's commit range" question is answered by
  the daemon.

## Phases

**Phase 0 — Bootstrap the dogfood loop.** Before any code:
stop the running `trinity serve`, `cargo install --path .
--locked`, restart, confirm `trinity --version` (will report
whatever's in `Cargo.toml` today — the value doesn't matter
yet). Update the README with the cargo-install dev loop. This
is a single small commit that proves the workflow before we
start relying on it.

**Phase 1 — Wiring, init, finish_preview endpoint.** Add
`init`/`finish`/`purge` subcommands to `src/main.rs`,
scaffold `src/cli/`, implement `trinity init` end-to-end with
the gitignore detection pre-flight. Wire `--version` via
clap's built-in. Implement the `finish_preview` HTTP
endpoint, including the sealed-approval list with body
hashes. Bump `Cargo.toml` to `0.1.0`.

**Phase 2 — Finish (bare + amend).** Implement `trinity
finish` and `trinity finish --amend` against
`finish_preview`. The CLI re-reads each sealed-approval
source, verifies the body hash matches, and aborts on drift.
No history rewriting yet; no squash. Covers the common case
(the ceremony I bungled in this session).

**Phase 3 — rewrite_preview endpoint.** Implement the
`rewrite_preview` HTTP endpoint: attribution + commit-range
from the fold, per-commit strip-path set via read-only `git
ls-tree`/`git diff-tree` at request time. Classification
rules (drop/keep/rewrite, foreign-commit definition, plan-
`.trinity/` path predicate) are typed code with unit tests
that work against fixture commit data — no working-tree
dependence in the rule layer. Integration tests exercise the
endpoint against small fixture repos so the git-I/O glue is
covered too. No CLI changes — the endpoint is consumed in
Phase 4.

**Phase 4 — Rewriting engine + purge/squash + --dry.**
History-rewriting engine, `trinity finish
--squash`/`--purge`/`--squash --purge`, `trinity purge` and
all its flags including `--into-branch` and `--dry`. The
engine is a thin executor of `rewrite_preview`'s manifest.
The hard phase — the engine is what every purge/squash
variant funnels through.
