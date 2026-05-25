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
  match. Long SHAs appear only when needed — when a target
  contains two reviewable commits that share the same 7-char
  prefix. The decision is per-target, not global: a target
  with two colliding commits puts ALL of its filenames in
  long form so the lookup is unambiguous.
- **Disk-side SHA is a `CommitRef`, NOT a `CommitSha`.** The
  filename stem is a 7-to-40-char hex prefix. It does not
  carry commit identity by itself; downstream code that
  compares against fold-state SHAs uses fully-resolved
  values. A new typed `CommitRef` (in `clank-core::ids`)
  represents the on-disk identifier. The path parser produces
  `CommitRef`; resolution against a scope produces a
  `CommitSha` (or `None` for an orphan / unmatched ref). NO
  consumer ever treats `CommitRef` as if it were
  `CommitSha`.
- **One path parser, one typed output.** `parse_feedback_path`
  takes a single path argument (repo-relative under
  `.clank/`) and returns ONE typed value including author,
  target, and ref. `fs_watcher::path_to_signal` and
  `git_io::collect_feedback_files` both call that same
  parser. No caller does "parse the segment list, then glue
  author on from somewhere else."
- **No symlinks, no manifests.** The layout is files on disk.
  No `*.json` index files. No symlinks. The mapping
  short↔full is computed from the relevant commit scope
  (per target) at read time.

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

## Types

### `CommitRef` (new, `clank-core::ids`)

```rust
/// On-disk identifier for a commit, as it appears in a
/// feedback filename stem. 7-to-40-char lowercase hex —
/// could be a short prefix OR a full SHA. NOT a commit
/// identity; must be resolved against a known scope before
/// equality comparison with `CommitSha`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRef(String);

impl CommitRef {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        // 7..=40 lowercase hex chars; reuses the
        // hex-validation rule from CommitSha::parse.
    }

    pub fn as_str(&self) -> &str { &self.0 }

    /// Try to resolve this ref against a known scope. Returns
    /// `Some(full)` iff exactly one commit in `scope` has a
    /// SHA that starts with `self.as_str()` (or equals it
    /// when `self` is 40 chars). Otherwise `None` — either
    /// the ref doesn't match any reviewable commit (orphan)
    /// or somehow matches multiple (which shouldn't happen
    /// under the plan-mode rule but the type doesn't
    /// pretend otherwise).
    pub fn resolve_against(&self, scope: &[CommitSha]) -> Option<CommitSha>;
}
```

The orphan case maps to "feedback file with no matching
reviewable commit — drop on read." This makes
history-rewrite cleanup self-healing: stale files are
ignored at read time even before `clank purge` strips them.

### `FeedbackPath` (revised)

```rust
pub struct FeedbackPath {
    pub author: AgentLabel,
    pub target: FeedbackTarget,    // unchanged: Plan(key) | AdHoc
    pub target_ref: CommitRef,     // was `target_sha: CommitSha`
    pub raw: PathBuf,
}
```

The field rename + retyping is the load-bearing change.
Every existing consumer of `target_sha` (in `runtime.rs`,
`preview.rs`, etc.) is forced to think about resolution
explicitly — the compiler rejects naive comparison against
`CommitSha`.

## Filename-mode rule

Given a target with reviewable-commit set `C = {c₁, c₂, …}`:

1. Compute every commit's 7-char prefix `short(cᵢ) = cᵢ[..7]`.
2. If any two commits in `C` share the same short, the target
   is in **long mode**: every feedback filename uses the full
   40-char SHA.
3. Otherwise the target is in **short mode**: every feedback
   filename uses the 7-char prefix.

The rule is per-target (where target is a plan stem or the
ad-hoc `_` scope), deterministic from the target's commit
list, and the same at write time and read time. Writer
computes the mode from `state.fold.plans[plan].commits` (or
`state.fold.ad_hoc`) filtered to reviewable. Reader doesn't
need to know the mode — it parses every file's stem as a
`CommitRef` and resolves through the same scope.

Edge cases:

- A new reviewable commit lands whose short collides with an
  earlier one. The target flips from short mode to long mode.
  Existing short-form files for this target are NOT renamed —
  the resolver still maps them to the right full SHA as long
  as no other reviewable commit's full SHA shares the prefix.
  Out-of-scope to proactively re-encode old files.
- A commit that's no longer reviewable (deleted plan,
  rewritten history) leaves stray feedback files. The
  resolver returns `None` for those refs; the reader drops
  them. `clank purge` then strips them on finalize as today.

## Reader behavior

### `scan_feedback` (plan-scoped, drives FeedbackView)

`crates/cli/src/feedback_scan.rs::scan_feedback(repo, plan,
reviewable_shas)` returns a `FeedbackView`. The new walk:

1. Walk `<repo>/.clank/agents/*/feedback/<plan>/*.md`.
2. For each entry, call `parse_feedback_path(rel)` to get the
   typed `FeedbackPath { author, target, target_ref, raw }`.
3. Skip entries whose `target` isn't `Plan(plan)` (defensive).
4. Resolve `target_ref.resolve_against(reviewable_shas)`. If
   `None` (orphan), drop the entry.
5. Record the parsed `FeedbackEntry` keyed by the resolved
   full `CommitSha` AND the author. The view's
   `CommitFeedback.sha: CommitSha` is fully resolved by the
   time it reaches `plan_view::project`.

The output type `FeedbackView` is unchanged in shape — only
the path layer it's built from is new.

### `collect_feedback_files` (repo-wide, drives the overlay)

`crates/cli/src/git_io.rs::collect_feedback_files(repo_root)`
returns a `Vec<FeedbackBlob { path: FeedbackPath, body }>`.
The walk:

1. Recurse `<repo>/.clank/agents/*/feedback/`.
2. For each `.md` file, call `parse_feedback_path(rel)` once
   and push the parsed value.
3. Filter out anything outside the canonical shape (`.DS_Store`,
   legacy `.clank/feedback/<plan>/<sha>/<author>.md` after
   migration, etc.).

`collect_feedback_files` does NOT resolve `CommitRef`s — it
has no scope to resolve against. The consumer
(`runtime.rs::push_feedback_change`, the overlay builder)
resolves per blob using the right scope: `Plan(key)` blobs
resolve against `state.fold.plans[key].commits`, `AdHoc`
blobs against the ad-hoc reviewable timeline.

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

`disk_format.rs` is the central path producer. The
parse/build pair:

```rust
/// Parse a path RELATIVE TO `<repo>/.clank/`. The single
/// parser used by `git_io::collect_feedback_files`,
/// `feedback_scan`, and `fs_watcher::path_to_signal`. No
/// caller does the segment-splitting itself.
///
/// Accepted shape:
/// `agents/<author>/feedback/<plan-or-_>/<commit-ref>.md`
///   - <author>     parses through `AgentLabel::parse`
///   - <plan-or-_>  parses through `PlanKey::parse`, or
///                  matches the reserved `_` literal
///   - <commit-ref> parses through `CommitRef::parse`
///                  (7–40 hex)
///
/// Returns `None` for any other shape.
pub fn parse_feedback_path(rel: &Path) -> Option<FeedbackPath>;

/// Build the canonical RELATIVE path
/// `agents/<author>/feedback/<target>/<ref>.md`. The caller
/// decides the ref form by passing the resolved scope.
pub fn canonical_feedback_path(
    author: &AgentLabel,
    target: &FeedbackTarget,
    target_sha: &CommitSha,
    scope_shas: &[CommitSha],
) -> PathBuf;

/// Repo-relative wire form (`.clank/agents/<author>/
/// feedback/<target>/<ref>.md`). Same signature as
/// `canonical_feedback_path`; prepends `.clank/`.
pub fn feedback_path_wire(
    author: &AgentLabel,
    target: &FeedbackTarget,
    target_sha: &CommitSha,
    scope_shas: &[CommitSha],
) -> String;
```

Author extraction lives in `parse_feedback_path` exclusively.
`fs_watcher::path_to_signal` calls
`parse_feedback_path(rel_to_clank)` after stripping
`.clank/` — no second author parse, no segment-list
hand-rolling at the call site.

`derive_work`'s `ReviewerAction::feedback_path` calls
`feedback_path_wire(author, target, sha, scope_shas)`. The
scope (short-vs-long mode) is computed at projection time
from `state.fold.plans[key].commits` filtered to
reviewable. `target` is always `Plan(key)` in the wait
surface — see "Ad-hoc scope" below.

`runtime.rs::push_feedback_change` consumes
`FeedbackTarget::{Plan, AdHoc}` via the
`FilesystemSignal::FeedbackWritten { parsed }` shape. After
this change `parsed.target_ref` is a `CommitRef`; the
consumer resolves against the relevant scope before doing
anything that needs commit identity.

## Ad-hoc scope

`FeedbackTarget::AdHoc` continues to round-trip through the
disk layout and the typed `FeedbackPath` API. That preserves
the model — ad-hoc feedback is parseable, persistable, and
the path parser handles it uniformly.

But `clank wfw`'s current wait surface
(`WaitItem::Reviewer { plan: PlanKey, ... }`) is plan-only.
`derive_work` only iterates `[PlanView]`; there is no
ad-hoc projection in core today, and adding one — plus a
typed `ReviewTarget::{Plan, AdHoc}` on `WaitItem::Reviewer`,
plus an ad-hoc-aware `feedback_path_wire` consumer — is
substantial scope creep beyond a directory rename.

So this plan does NOT promise ad-hoc reviewer work surfaces
through `clank wfw`. Ad-hoc on-disk feedback is preserved;
the rest is deferred to a follow-on plan (e.g.
`wfw-ad-hoc-reviewer-work`). The migration script still
moves ad-hoc paths so the storage stays correct for any
future surface.

`git_io.rs::collect_feedback_files` walks the new root
(`.clank/agents/*/feedback/`) and yields the same
`FeedbackBlob { path: FeedbackPath, body }` shape.

`runtime.rs::push_feedback_change` consumes
`FeedbackTarget::{Plan, AdHoc}` via the
`FilesystemSignal::FeedbackWritten { parsed }` shape. No
change to the consumer; `parsed` now comes from the new
parser.

### Watcher interaction (no changes to wfw's watch roots)

The wfw watcher landed in `wfw-daemon-style-watches` already
watches `<repo>/.clank` recursively. That single root covers
EVERY path under `.clank/`, including the new
`.clank/agents/<author>/feedback/<target>/<sha>.md` tree.
No additional `watcher.watch(...)` call is needed. The
recursive descent picks up newly-created agent subdirs
(`agents/alice/`, `agents/codex/`, …) automatically when
those directories first appear.

This works identically in both native and polling mode:

- **Native mode (`--no-poll`).** `.clank/` events fire
  through native notify, exactly as today. The watcher is
  unaffected by the per-agent layout.
- **Polling mode (`--poll`, default under
  `CODEX_SANDBOX=seatbelt`).** `.clank/` events still come
  through native notify in this mode too — only the gitdir
  watch is skipped. Feedback writes still trigger immediate
  wakes; only commit-boundary git events fall back to the
  500ms refold tick.

So this plan is entirely orthogonal to the watcher design.
The only watcher-adjacent change is the
`fs_watcher::path_to_signal` parser update above (a typed-
path concern, not a watch-root concern).

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
   existing feedback files are migrated by an operator (running
   the bundled shell script) before they can resume work on an
   active plan.
2. **Compat read window.** New code reads BOTH old and new
   layouts for one release, writes only the new layout. Drop
   the old read after a cleanup commit.

This plan picks **hard cutover.** Clank is heavy-dev — the
only real consumer is this repo, and we can migrate by hand.
The implementation commit includes a tiny shell script
(`scripts/migrate-feedback-to-agents.sh`) that does the move
deterministically for any existing repo.

### Feedback is ignored — use plain `mv`, NOT `git mv`

`.clank/feedback/` is gitignored (the root `.gitignore` has
`.clank/*` with carve-outs only for `plans/` and `finished/`,
plus an explicit `.clank/feedback/` rule). Feedback files are
local filesystem truth — not tracked git artifacts.
`.clank/agents/` will be ignored too, by the same `.clank/*`
catch-all (no new `.gitignore` entry needed; no carve-out
desired — feedback stays untracked).

`git mv` on an ignored file fails (`fatal: not under
version control`). The migration script uses plain `mv`
(filesystem rename):

```text
for old in .clank/feedback/<target>/<sha>/<author>.md:
  # <target> is either a plan stem or the reserved `_`.
  new = .clank/agents/<author>/feedback/<target>/<sha>.md
  mkdir -p $(dirname new)
  mv old new
```

`std::fs::rename` is the equivalent in any Rust-based
variant; the shell script uses `mv`.

The migration keeps the full SHA in the new filename to
preserve identity; the writer will start producing short
form on the next review cycle. Plan-scoped and ad-hoc paths
use the same rewrite (the `<target>` segment is opaque to
the script — `_` or any plan stem is moved identically).

## Tests

### Core / ids

- `CommitRef::parse` accepts every length from 7 to 40
  lowercase hex; rejects 6 chars, 41 chars, uppercase,
  non-hex, empty.
- `CommitRef::resolve_against`:
  - Full 40-char SHA in scope → `Some(that_sha)`.
  - 7-char prefix matching exactly one scope SHA →
    `Some(full_match)`.
  - Prefix matching two scope SHAs → `None` (ambiguous —
    can't happen under correct write-time mode but the type
    doesn't pretend).
  - Prefix matching zero scope SHAs → `None` (orphan).
  - Empty scope → `None` for any non-empty ref.

### Core / disk_format

- `parse_feedback_path` table-driven over BOTH targets,
  taking a SINGLE path relative to `<repo>/.clank/`:
  - `agents/alice/feedback/foo/<short>.md` → `Plan(foo)`,
    author=`alice`, ref=short.
  - `agents/alice/feedback/foo/<full>.md` → `Plan(foo)`,
    author=`alice`, ref=full.
  - `agents/codex/feedback/_/<short>.md` → `AdHoc`,
    author=`codex`, ref=short.
  - `agents/codex/feedback/_/<full>.md` → `AdHoc`,
    author=`codex`, ref=full.
  - Rejects: stem under 7 hex / over 40 hex / non-hex / a
    missing `agents/` prefix / a missing `feedback/`
    segment / invalid `AgentLabel` / extra directory
    levels / wrong file extension.
- `canonical_feedback_path` and `feedback_path_wire` round-
  trip with `parse_feedback_path` for both targets and both
  modes.
- New pure helper for the mode rule:
  `fn feedback_filename_mode(reviewable_shas: &[CommitSha])
  -> Mode { Short, Long }`. Unit-tested for empty / one /
  no-collision / collision sets.

### feedback_scan + plan_view

- Reader finds plan-scoped feedback at the short path.
- Reader finds plan-scoped feedback at the full path.
- Reader drops a feedback file whose stem doesn't resolve
  against `reviewable_shas` (orphan from rewritten history).
- Plan with multiple agents under `.clank/agents/*/` produces
  a `CommitFeedback.entries` map keyed correctly per author.

`plan_view` projection tests survive the path change because
the type shape (`FeedbackView`) is unchanged.

### collect_feedback_files

- Repo with mixed plan and ad-hoc feedback under
  `.clank/agents/*/feedback/`. Result `Vec<FeedbackBlob>`
  carries `FeedbackPath::target` of the right variant per
  entry; both targets present in the result.
- The `target_ref` field is `CommitRef` (not `CommitSha`) —
  the consumer is responsible for resolution.
- Pre-existing test fixtures using the old layout are
  rewritten to the new layout (part of Commit 2's churn).

### Integration

- `wfw`: `ReviewerAction.feedback_path` JSON contains
  `.clank/agents/<author>/feedback/<plan>/<short>.md` for a
  plan with no collisions; full form for a plan with two
  colliding-short commits. (Ad-hoc is NOT exercised here —
  see Ad-hoc scope; the wfw wait surface stays plan-only.)
- `clank finish`: writes a sealed approval whose
  `source_path` points at the new layout. The existing
  finalize-wake test continues to pass.
- Migration script smoke tests. **Crucially these set up
  the OLD layout as untracked files (`.gitignore` has
  `.clank/feedback/`), not staged or committed, because
  that's what real Clank repos have.** A `git mv`-based
  script would fail this test; the `mv`-based script
  passes.
  - Old layout with only plan-scoped feedback (untracked) →
    new layout with plan-scoped feedback. `clank status`
    projects.
  - Old layout with only ad-hoc (`_`) feedback
    (untracked) → new layout with ad-hoc feedback.
    `collect_feedback_files` returns the expected `AdHoc`
    blob with the correct `CommitRef`.
  - Old layout with BOTH targets (untracked) → both move.

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
- `parse_feedback_path` is a single function taking ONE
  argument (path relative to `<repo>/.clank/`). Author is
  extracted by that one parser; no caller hand-rolls
  segment splitting. `grep -rn 'agents/' crates/cli/src` in
  the watcher / path-routing code shows only the one call
  site that invokes `parse_feedback_path` (no per-caller
  re-parses).
- `FeedbackPath.target_ref` is `CommitRef`, NOT `CommitSha`.
  `grep -rn 'CommitSha' crates/cli/src/disk_format.rs`
  shows no field-typed-as-CommitSha for the feedback-stem
  position.
- Resolution of `CommitRef → Option<CommitSha>` happens in
  `feedback_scan` and the `collect_feedback_files` consumer
  (the overlay builder in `runtime.rs`). Downstream code
  that does equality against fold-state SHAs sees only
  fully resolved `CommitSha` values.
- A target (plan or ad-hoc) with reviewable commits whose
  7-char prefixes are all unique produces feedback files at
  `.clank/agents/<author>/feedback/<target>/<short>.md`.
- A scope with two reviewable commits sharing a 7-char prefix
  produces feedback files at the full-SHA path for the entire
  scope.
- Orphan feedback files (a stem that no longer resolves
  against any reviewable SHA) are dropped at read time
  without error.
- `clank wfw` advertises the new `feedback_path` in its
  `ReviewerAction` shape for plan-scoped reviewer work.
  Ad-hoc reviewer work is NOT in the wait surface in this
  plan (see Ad-hoc scope); deferred to a follow-on.
- `clank finish` reads, validates, and seals approvals from
  the new layout, then writes them into
  `.clank/finished/<stem>/<author>.md` unchanged.
- `git_io::collect_feedback_files` walks the new root and
  produces `FeedbackBlob`s with the correct
  `FeedbackTarget` per entry. The `target_ref` field is
  `CommitRef`, not resolved.
- `fs_watcher::path_to_signal` routes events under
  `.clank/agents/<author>/feedback/<target>/` correctly,
  including ad-hoc, via the single shared parser.
- All 14 wfw integration tests pass.
- The migration script transforms a synthetic old-layout repo
  (with both plan and ad-hoc feedback, set up as UNTRACKED
  files matching the production `.gitignore` rule for
  `.clank/feedback/`) into the new layout and `clank status`
  projects correctly. `grep -n 'git mv' scripts/migrate-*` is
  empty — the script uses plain `mv`.
- `cargo test --workspace` green; `cargo fmt --check` clean.

## Out of Scope

- **Ad-hoc reviewer work in the wfw wait surface.** Ad-hoc
  feedback is preserved on-disk and round-trips through the
  typed `FeedbackPath` API, but `clank wfw` does NOT
  advertise ad-hoc reviewer work in `WaitItem::Reviewer`.
  Adding it requires a typed `ReviewTarget::{Plan, AdHoc}`
  on `WaitItem::Reviewer`, an ad-hoc projection in core
  parallel to `PlanView`, and `derive_work` iterating both
  view kinds. Substantial scope creep beyond a directory
  rename — deferred to a follow-on
  (`wfw-ad-hoc-reviewer-work` or similar).
- **Moving `.clank/finished/` under agents/.** See the Open
  Question — the sealed-snapshot is a repo-wide artifact,
  not per-agent. Separate plan if symmetry wins.
- **Compat read window for the old layout.** Hard cutover.
- **Configurable short-SHA length.** Fixed at 7 chars.
  Per-target escalation to 40 on collision.
- **Renaming existing short-form files on retroactive
  collision.** Resolver handles it; we don't proactively
  re-encode.
- **`CommitRef` ambiguity reporting.** The resolver returns
  `None` for both "no match" and "multiple matches." That's
  sufficient for the reader to drop the entry; surfacing
  the distinction in a structured error would need its own
  plan if it's ever useful.

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
