# agent-keyed-feedback-layout

## Summary

Re-shape `.clank/feedback/` so the agent owns the subtree.
Today feedback is keyed by `(target, commit)` first:

```text
.clank/feedback/<plan-or-_>/<full-sha>/<author>.md
```

(`<plan>` for plan-attributed feedback, the reserved `_`
segment for ad-hoc commit feedback — `FeedbackTarget::{Plan,
AdHoc}` in `crates/cli/src/disk_format.rs`.)

After this plan it's keyed by agent first:

```text
.clank/agents/<author>/feedback/<plan-or-_>/<short-sha>.md
```

Three simultaneous shape changes:

1. **Agent-owned subtree.** Each agent gets `.clank/agents/<label>/`
   as its own root. Both target kinds (`Plan` and `AdHoc`)
   live under `feedback/<target>/`. The reserved `_` segment
   for ad-hoc continues to live in the exact same position
   relative to the agent's root.
2. **Commit-as-file, not commit-as-directory.** The
   per-commit directory collapses to a flat file per commit
   per agent. Two reviewers' feedback on the same commit
   no longer share a directory.
3. **Short SHA in the filename.** Use the 7-char prefix
   (`<full>[..7]`) by default. Writer escalates to the full
   40-char SHA only when two commits in the same scope
   (`<target>`) would collide on their short form. Reader
   accepts both.

## Hard Direction

- **Agent owns its own directory.** Two parallel agents writing
  in the same repo are no longer at risk of stepping on each
  other's files through a shared directory tree — each writes
  under its own subtree. The label is the validated
  `AgentLabel`, so the dirname has the same character class as
  the current `<author>.md` stem.
- **Short SHAs by default, long on collision.** Operators reading
  `git log --oneline` see 7-char hashes; the on-disk paths
  match. Long SHAs appear only when needed — when a plan
  contains two reviewable commits that share the same 7-char
  prefix. The decision is per-plan, not global: a plan with two
  colliding commits puts ALL of its filenames in long form so
  the lookup is unambiguous.
- **Reader accepts both.** A reader looking for feedback on
  commit `<full>` looks at `<full>.md` first, falls back to
  `<full>[..7].md`. Either form is valid input. Writers should
  only ever produce one form per commit, but stale files from
  prior writes (e.g. format-version skew) don't break the
  reader.
- **No symlinks, no manifests.** The layout is files on disk.
  No `*.json` index files. No symlinks. The mapping
  short↔full is computed from the plan's commit timeline at
  read time.

## Layout

Before:

```text
.clank/
  plans/<stem>.md
  feedback/
    <plan>/                     <- FeedbackTarget::Plan
      <full-sha>/
        <author>.md
    _/                          <- FeedbackTarget::AdHoc
      <full-sha>/
        <author>.md
  finished/
    <stem>/
      <author>.md
```

After:

```text
.clank/
  plans/<stem>.md
  agents/
    <author>/
      feedback/
        <plan>/                 <- FeedbackTarget::Plan
          <short-or-full>.md
        _/                      <- FeedbackTarget::AdHoc
          <short-or-full>.md
  finished/
    <stem>/
      <author>.md
```

The reserved `_` segment continues to mean "ad-hoc, no plan."
It sits at the same position relative to its agent root as the
`<plan>` segment, so the typed `FeedbackTarget::{Plan, AdHoc}`
parser sees the same closed set of shapes — just shifted under
`.clank/agents/<author>/feedback/` instead of
`.clank/feedback/`.

`.clank/finished/` is unchanged in this plan — the sealed
approval snapshot stays a repo-wide artifact, not a per-agent
one. See Open Questions for the symmetry argument.

## SHA-naming rule

Given a plan with reviewable-commit set `C = {c₁, c₂, …}`:

1. Compute every commit's 7-char prefix `short(cᵢ) = cᵢ[..7]`.
2. If any two commits in `C` share the same short, the plan is
   in **long mode**: every feedback filename uses the full
   40-char SHA.
3. Otherwise the plan is in **short mode**: every feedback
   filename uses the 7-char prefix.

The rule is per-plan, deterministic from the plan's commit
list, and the same at write time and read time. Writer
computes the mode from `state.fold.plans[plan].commits` (the
reviewable subset). Reader does the same.

Edge cases:

- A new reviewable commit lands whose short collides with an
  earlier one. The plan flips from short mode to long mode.
  Existing short-form files for this plan are NOT renamed — the
  reader's fallback (`<full>.md` then `<full>[..7].md`) keeps
  them findable. Future writes use long mode. Out-of-scope to
  proactively re-encode old files; the lookup still works.
- A commit that's no longer reviewable (deleted plan, rewritten
  history) leaves stray feedback files. Same disposition as
  today: `clank purge` strips them on finalize.

## Reader behavior

`crates/cli/src/feedback_scan.rs::scan_feedback` is the
plan-scoped reader. Today it walks
`.clank/feedback/<plan>/<sha>/*.md`. The new walk:

1. For each reviewable commit `c` in `reviewable_shas`:
   a. For each entry under `.clank/agents/*/`:
      - Read `agents/<author>/feedback/<plan>/<c>.md` if it
        exists.
      - Else read `agents/<author>/feedback/<plan>/<c[..7]>.md`
        if it exists.
   b. Record the parsed `FeedbackEntry` keyed by author.

The output type `FeedbackView` is unchanged.

The repo-wide feedback reader is
`crates/cli/src/git_io.rs::collect_feedback_files`. It walks
`.clank/feedback/` exhaustively today, hands every file to
`parse_feedback_path`, and returns a `Vec<FeedbackBlob>` keyed
by the typed `FeedbackPath { target, target_sha, author }`.
This is the input the rebuild path uses to build the in-memory
overlay (plus the stale-review machinery in `runtime.rs` and
the live overlay that powers `wfw`'s
`StaleReview`/`CurrentReview` projections — see
`runtime.rs::push_feedback_change`). The new walk:

1. Recurse `.clank/agents/`.
2. For each `<author>/feedback/<plan-or-_>/<file>.md`:
   - The file stem is a SHA prefix (7+ hex) or full (40 hex).
   - `parse_feedback_path` returns the same
     `FeedbackPath { target, target_sha, author }` typed
     value. The reader doesn't care which length the stem is.
3. Filter out anything outside the canonical shape (stray
   `.DS_Store`, the legacy `<plan>/<sha>/<author>.md` form
   after migration, etc.).

The reader does NOT need the plan's "mode" (short vs long) —
it just walks every file under `agents/<author>/feedback/` and
typed-parses each one. The plan-mode rule is a write-time
invariant; the read-time check is "is the stem a valid SHA
prefix between 7 and 40 hex chars".

## Writer behavior

The writers today:

- **Reviewers** (humans + agents) writing approval files.
  These are not done through `clank` itself — `clank wfw`
  advertises a path in `ReviewerAction::feedback_path` and the
  reviewer drops the file there.
- **`clank finish`** reads sealed approvals via
  `git_io::collect_feedback_files` (or the equivalent narrow
  scan) and copies into `.clank/finished/<stem>/<author>.md`.
  Source-path migration is mechanical.

`disk_format.rs` is the central path producer. Three exports
move:

- `canonical_feedback_path(plan_key, target_sha, author)`
  becomes `canonical_feedback_path(target, target_sha,
  author, scope_shas)` where `target: FeedbackTarget` is
  `Plan(key)` or `AdHoc`, and `scope_shas` is the set of
  reviewable commits in this target's scope (so the writer
  can compute short-vs-long mode). The return value is the
  PathBuf relative to the agent's `feedback/` root —
  `<target>/<short-or-full>.md`.
- `feedback_path_wire(...)` (the repo-relative wire form used
  by `WorkPayload`/`StaleReview`) gets the same signature and
  prepends `.clank/agents/<author>/feedback/`.
- `parse_feedback_path` is rewritten to accept the new
  per-agent layout. It receives the path relative to
  `<repo>/.clank/agents/<author>/feedback/`. The match arm is
  still `[session, file]` (two segments), and `session_str`
  still gates `_` → `AdHoc` vs `<plan-key>` → `Plan(key)`.
  The file stem replaces the previous `<sha>/<author>.md`
  pair — the stem IS the SHA prefix (7–40 hex), and the
  author comes from the directory walk above it. Returns
  `FeedbackPath { target, target_sha, author, raw }` —
  unchanged.

`derive_work`'s `ReviewerAction::feedback_path` builds the
canonical write path. The mode (short vs long) is computed at
projection time from the target's scope-of-reviewable commits,
so the field carries the right form. For `Plan(key)` scope is
`state.fold.plans[key].commits` filtered to reviewable. For
`AdHoc` scope is the ad-hoc reviewable timeline (currently
under `state.fold.ad_hoc`).

`fs_watcher.rs::path_to_signal` keys on the
`.clank/feedback/` prefix today. The new prefix is
`.clank/agents/<author>/feedback/`; the segment-shape match
stays the same after the prefix strip.

`git_io.rs::collect_feedback_files` walks the new root
(`.clank/agents/*/feedback/`) and yields the same
`FeedbackBlob { path: FeedbackPath, body }` shape.

`runtime.rs::push_feedback_change` consumes
`FeedbackTarget::{Plan, AdHoc}` via the
`FilesystemSignal::FeedbackWritten { parsed }` shape. No
change to the consumer; `parsed` now comes from the new
parser.

## CLI surfaces

- `wfw`: `ReviewerAction` JSON's `feedback_path` field carries
  the new path shape. `kind`, `plan`, `plan_path`, `sha` are
  unchanged.
- `status`: no path-shape change — status doesn't expose the
  feedback path directly (it reports waiting_on / reason).
- `finish`: `SealedApproval.source_path` is the on-disk feedback
  path the CLI re-reads before sealing. Same field, new shape.
- `purge`: the strip-paths logic that nukes `.clank/feedback/`
  on finalize moves to `.clank/agents/*/feedback/<plan>/`.

## Migration

This repo and any other already-active Clank repo has feedback
files at the old paths. Choices:

1. **Hard cutover.** New code stops reading the old paths. Any
   existing feedback files are migrated by an operator (manual
   `mv`) before they can resume work on an active plan.
2. **Compat read window.** New code reads BOTH old and new
   layouts for one release, writes only the new layout. Drop
   the old read after a cleanup commit.

This plan picks **hard cutover.** Clank is heavy-dev — the only
real consumer is this repo, and we can migrate by hand. The
implementation commit includes a tiny shell script
(`scripts/migrate-feedback-to-agents.sh`) that does the move
deterministically for any existing repo, so the cutover isn't
hostile to operators.

The migration script handles BOTH `FeedbackTarget` kinds:

```text
for old in .clank/feedback/<target>/<sha>/<author>.md:
  # <target> is either a plan stem or the reserved `_`.
  new = .clank/agents/<author>/feedback/<target>/<sha>.md
  mkdir -p $(dirname new)
  git mv old new
```

It keeps the full SHA in the new filename to preserve history;
the writer will start producing short form on the next review
cycle. Plan-scoped and ad-hoc paths use the same rewrite (the
`<target>` segment is opaque to the script — `_` or any plan
stem is moved identically).

## Tests

### Core / disk_format

- `parse_feedback_path` table-driven over BOTH targets:
  - `<plan>/<short>.md` → `Plan(plan)` + short sha.
  - `<plan>/<full>.md` → `Plan(plan)` + full sha.
  - `_/<short>.md` → `AdHoc` + short sha.
  - `_/<full>.md` → `AdHoc` + full sha.
  - Rejects: stem under 7 hex / over 40 hex / non-hex / extra
    directory levels / wrong file extension.
- `canonical_feedback_path` round-trip with `parse_feedback_path`
  for both targets and both modes.
- `feedback_path_wire` returns the new
  `.clank/agents/<author>/feedback/<target>/<sha>.md` shape;
  asserts the `<target>` slot is `_` for ad-hoc.
- New pure helper for the mode rule:
  `fn feedback_filename_mode(reviewable_shas: &[CommitSha]) ->
  Mode { Short, Long }`. Unit-tested for empty / one /
  no-collision / collision sets.

### feedback_scan + plan_view

- Reader finds plan-scoped feedback at the short path.
- Reader finds plan-scoped feedback at the full path.
- Reader prefers full when both exist (defensive — shouldn't
  happen, but defines the rule).
- Plan with multiple agents under `.clank/agents/*/` produces
  a `CommitFeedback.entries` map keyed correctly per author.

`plan_view` projection tests survive the path change because
the type shape (`FeedbackView`) is unchanged.

### collect_feedback_files

- Repo with mixed plan and ad-hoc feedback under
  `.clank/agents/*/feedback/`. Result `Vec<FeedbackBlob>`
  carries `FeedbackPath::target` of the right variant per
  entry; both targets present in the result.
- Pre-existing test fixtures using the old layout are
  rewritten to the new layout (these are part of
  Commit 2's churn).

### Integration

- `wfw`: `ReviewerAction.feedback_path` JSON contains
  `.clank/agents/<author>/feedback/<plan>/<short>.md` for a
  plan with no collisions; full form for a plan with two
  colliding-short commits. An equivalent ad-hoc case asserts
  the `_` segment.
- `clank finish`: writes a sealed approval whose
  `source_path` points at the new layout. The existing
  finalize-wake test continues to pass.
- Migration script smoke tests:
  - Old layout with only plan-scoped feedback → new layout
    with plan-scoped feedback. `clank status` projects.
  - Old layout with only ad-hoc (`_`) feedback → new layout
    with ad-hoc feedback. `collect_feedback_files` returns
    the expected `AdHoc` blob.
  - Old layout with BOTH targets → both move.

## Sequencing

Two commits:

1. **Core + reader.** Adds the plan-mode helper to
   `clank-core`. Rewires `scan_feedback` for the new layout.
   Updates `derive_work` to emit the new
   `ReviewerAction::feedback_path` shape. Tests for both.
2. **CLI writers + purge + migration script.** Updates
   `clank finish` (`SealedApproval::source_path` builders) and
   `clank purge`'s strip-paths rule. Adds the migration shell
   script. Updates all integration tests' fixture-writes to
   the new path shape. Migrates THIS repo's own feedback at
   the same commit.

## Acceptance

- Every reader and writer of feedback uses the new layout.
  `grep -rn 'feedback/' crates/ | grep -v 'agents/'` shows no
  hits on the old `.clank/feedback/<plan>/<sha>/<author>.md`
  shape.
- `FeedbackTarget::{Plan, AdHoc}` round-trips through
  `parse_feedback_path` and `canonical_feedback_path` on the
  new layout. The reserved `_` segment continues to mean
  ad-hoc.
- A plan (or ad-hoc target) with reviewable commits whose
  7-char prefixes are all unique produces feedback files at
  `.clank/agents/<author>/feedback/<target>/<short>.md`.
- A scope with two reviewable commits sharing a 7-char prefix
  produces feedback files at the full-SHA path for the entire
  scope.
- The reader finds feedback at either form (short or full),
  preferring full when both exist.
- `clank wfw` advertises the new `feedback_path` in its
  `ReviewerAction` shape for both plan-scoped and ad-hoc
  reviewer work.
- `clank finish` reads, validates, and seals approvals from
  the new layout, then writes them into
  `.clank/finished/<stem>/<author>.md` unchanged.
- `git_io::collect_feedback_files` walks the new root and
  produces `FeedbackBlob`s with the correct
  `FeedbackTarget` per entry.
- `fs_watcher::path_to_signal` routes events under
  `.clank/agents/<author>/feedback/<target>/` correctly,
  including ad-hoc.
- All 13 wfw integration tests pass.
- The migration script transforms a synthetic old-layout repo
  (with both plan and ad-hoc feedback) into the new layout
  and `clank status` projects correctly.
- `cargo test --workspace` green; `cargo fmt --check` clean.

## Out of Scope

- **Moving `.clank/finished/` under agents/.** See the Open
  Question — the sealed-snapshot is a repo-wide artifact, not
  per-agent. If we ever rethink that, separate plan.
- **Compat read window for the old layout.** Hard cutover.
- **Configurable short-SHA length.** Fixed at 7 chars. Git's
  default unique-prefix length is also typically 7 in small
  repos; we just always use 7 and escalate to 40 on collision.
- **Renaming existing short-form files on retroactive
  collision.** Reader's two-path fallback handles it; we don't
  proactively re-encode.

## Open Questions

- **Should `.clank/finished/<stem>/<author>.md` also move to
  `.clank/agents/<author>/finished/<stem>.md`?** Symmetry says
  yes. Current plan says no — the finished snapshot is the
  repo's record of "this plan ended approved by these
  reviewers," and the per-plan grouping is more natural for
  audit than the per-agent one. If symmetry wins, fold it into
  Commit 2 of this plan. If we keep them split, the rationale
  ("feedback is reviewer's draft; finished is the repo's
  sealed record") goes in a follow-up doc note.
- **Should `clank wfw` block when an agent's
  `.clank/agents/<author>/` directory doesn't exist yet?** It
  shouldn't — the reader treats missing directories as "no
  feedback" today and that should hold. Flagged to confirm
  during implementation.
