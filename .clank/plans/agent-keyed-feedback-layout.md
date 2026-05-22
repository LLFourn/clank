# agent-keyed-feedback-layout

## Summary

Re-shape `.clank/feedback/` so the agent owns the subtree. Today
feedback is keyed by commit first:

```text
.clank/feedback/<plan>/<full-sha>/<author>.md
```

After this plan it's keyed by agent first:

```text
.clank/agents/<author>/feedback/<plan>/<short-sha>.md
```

Two simultaneous shape changes:

1. **Agent-owned subtree.** Each agent gets `.clank/agents/<label>/`
   as its own root. Plan-scoped feedback lives under
   `feedback/<plan>/`. Commit-scoped becomes a flat file per
   commit, not a directory.
2. **Short SHA in the filename.** Use the 7-char prefix
   (`<full>[..7]`) by default. Writer escalates to the full
   40-char SHA only when two commits in the same plan would
   collide on their short form. Reader accepts both.

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
    <plan>/
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
        <plan>/
          <short-or-full>.md
  finished/
    <stem>/
      <author>.md
```

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

`crates/cli/src/feedback_scan.rs::scan_feedback` is the reader.
Today it walks `.clank/feedback/<plan>/<sha>/*.md` and reads
each file's body. The new walk:

1. For each reviewable commit `c` in `reviewable_shas`:
   a. For each entry under `.clank/agents/*/`:
      - Read `agents/<author>/feedback/<plan>/<c>.md` if it
        exists.
      - Else read `agents/<author>/feedback/<plan>/<c[..7]>.md`
        if it exists.
   b. Record the parsed `FeedbackEntry` keyed by author.

The output type `FeedbackView { per_commit: Vec<CommitFeedback>
{ sha, entries: BTreeMap<AgentLabel, FeedbackEntry> } }` is
unchanged.

The reader does NOT need to know the plan's "mode" (short vs
long) — it just tries both forms. The plan-mode rule is a
write-time invariant; the read-time check is a two-path
lookup.

## Writer behavior

The only writers today are:

- **Reviewers** (humans + agents) writing approval files. These
  are not done through `clank` itself — agents/humans use
  whatever editor they like. Convention is that the path is
  predictable enough that `clank wfw` can advertise it in the
  `ReviewerAction::feedback_path` field.
- **`clank finish`** reads sealed approvals from
  `.clank/feedback/<plan>/<sha>/<author>.md` and writes them to
  `.clank/finished/<stem>/<author>.md`. Source-path migration
  is mechanical.

`derive_work`'s `ReviewerAction` builds the canonical
feedback path. That string moves from
`.clank/feedback/<plan>/<sha>/<author>.md` to
`.clank/agents/<author>/feedback/<plan>/<sha-or-short>.md`.
The mode (short vs long) is computed at projection time from
the plan's reviewable-commit timeline, so the
`ReviewerAction::feedback_path` field carries the right form
for the caller to write.

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

The migration script:

```text
for old in .clank/feedback/<plan>/<sha>/<author>.md:
  new = .clank/agents/<author>/feedback/<plan>/<sha>.md
  mkdir -p $(dirname new)
  git mv old new
```

It keeps the full SHA in the new filename to preserve history;
the writer will start producing short form on the next review
cycle.

## Tests

### Core

- `feedback_scan::scan_feedback` table-driven:
  - Reader finds feedback at the short path.
  - Reader finds feedback at the full path.
  - Reader prefers full when both exist (defensive — shouldn't
    happen, but defines the rule).
  - Plan with multiple agents under `.clank/agents/*/` produces
    a `CommitFeedback.entries` map keyed correctly per author.

- `plan_view` indirectly: existing projection tests survive
  the path change because the type shape (`FeedbackView`)
  doesn't change.

- A new pure helper in core for the plan-mode rule:
  `fn feedback_filename_mode(reviewable_shas: &[CommitSha]) ->
  Mode { Short, Long }`. Unit-tested for empty / one /
  no-collision / collision sets.

### Integration

- `wfw`: `ReviewerAction.feedback_path` JSON contains
  `.clank/agents/<author>/feedback/<plan>/<short>.md` for a
  plan with no collisions; full form for a plan with two
  colliding-short commits.
- `clank finish`: writes a sealed approval whose
  `source_path` is the new shape. The existing finalize-wake
  test continues to pass.
- Migration script smoke test: create a temp repo with the old
  layout, run the script, run `clank status` — feedback is
  visible at the new paths.

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
  `grep -rn 'feedback/<plan>/<sha>' crates/` returns nothing.
- A plan with reviewable commits whose 7-char prefixes are all
  unique produces feedback files at
  `.clank/agents/<author>/feedback/<plan>/<short>.md`.
- A plan with two reviewable commits sharing a 7-char prefix
  produces feedback files at the full-SHA path for the entire
  plan.
- The reader finds feedback at either form (short or full),
  preferring full when both exist.
- `clank wfw` advertises the new `feedback_path` in its
  `ReviewerAction` shape.
- `clank finish` reads, validates, and seals approvals from
  the new layout, then writes them into
  `.clank/finished/<stem>/<author>.md` unchanged.
- All 13 wfw integration tests pass.
- The migration script transforms a synthetic old-layout repo
  into the new layout and `clank status` projects correctly.
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
