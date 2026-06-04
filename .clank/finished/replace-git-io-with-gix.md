# replace-git-io-with-gix
# Backend swap: replace shell-out + text-parse with gix programmatic access

## Problem

`crates/cli/src/git_io.rs` is a clean typed API (16 public functions, all returning `CommitSha` / `CommitChanges` / `CommitMeta` / etc. behind `GitIoError`). 20 external call sites consume it. The signatures don't leak shell-out details.

What's hidden behind the API today:

- **Text parsers:** `diff_tree_changes` parses `git diff-tree -r --name-status -M --no-commit-id` line-by-line (~100 LoC + 10+ tests). `commit_message` uses `git show -s --format='%s%x00%b'` with literal NUL separators. The deleted `parse_diff_git` (commit `d4f3226`) was an extreme example of where this approach leads.
- **Shell-outs that don't parse much** but still pay subprocess fork/exec per call: `rev_parse_head`, `is_ancestor`, the `first_parent_commits_*` family.
- **Long-tail fragility:** `core.quotePath`, `diff.mnemonicPrefix`, `diff.noprefix`, whitespace-in-filenames, format drift across git versions.

Because `git_io.rs` is a clean API, this is a **backend swap, not a redesign**. Signatures stay identical. Callers don't change. Tests at the API surface stay identical.

## Approach

### Library choice: `gix` 0.84

Confirmed via crates.io 2026-05-26 release. MSRV: Rust 1.85 (clank already builds on edition-2024 toolchain). Pure-Rust; modular; used in production by cargo for dependency fetching.

`git2` (libgit2 bindings) was the alternative; rejected because (a) it would be clank's first C dependency, and (b) the modularity story on gix lets us trim what we don't need. If gix turns out to have a gap mid-migration, falling back to `git2` for that one function is an option — not the plan's default.

**Cargo.toml addition:**

```toml
[dependencies]
gix = { version = "0.84", default-features = false, features = [
    "sha1",                  # required: at least one hash backend must be enabled
    "revision",              # rev_parse, rev_walk, merge_base
    "blob-diff",             # diff::tree_with_rewrites with rename detection
    "max-performance-safe",  # zlib-rs + parallel without C deps
    "parallel",              # Repository: Send under parallel feature
] }
```

The `sha1` feature is required: `gix-hash` refuses to build without at least one hash backend, and `default-features = false` strips the default `sha1`. clank only ever operates on sha1 repos (git defaults to sha1; `--object-format=sha256` at init is exceedingly rare), so this is mechanical, not a decision point.

Explicitly **not** enabling: `blocking-network-client`, `async-network-client`, any HTTP transport features. We don't clone/fetch/push from git_io.

**Feature-trim ladder (if the size escape hatch fires):** the four enabled features rank by load-bearing-ness as `revision` > `blob-diff` > `max-performance-safe` > `parallel`. The first two are migration targets (rev_walk + tree-diff are the whole point); never trim them. If 45MB is exceeded, drop in this order:

1. **Drop `parallel` first.** clank never caches a `Repository` across threads today; `Send`-ness is unused. Smallest impact.
2. **Drop `max-performance-safe` second.** Accept the pure-Rust zlib decompression perf hit. clank's git workloads are small (one repo, dozens of commits, kilobytes of blobs), so the hit is bounded.
3. **Stop.** If we're still over budget after those two, the plan's premise is wrong and we should reassess (not silently drop a load-bearing feature).

### Repository handle pattern

Today: each `git_io::*` function shells out from scratch. With gix, opening a `Repository` parses config and probes the filesystem — non-trivial per-call cost.

Migration default: keep `&Path` signatures, open per-call. Adding a `Repo` cache layer is a follow-up optimization once the migration is green.

### Dead-code purge (committed alongside the spike)

Audit during the spike found three functions in `git_io.rs` with zero callers anywhere in the workspace:

- `commit_message` — the original spike target. Dead, deleted.
- `first_added_commit` — used `git log --diff-filter=A --follow` which would have needed gix's rename-detection (`Rewrites` options) to preserve semantics. Dead, deleted.
- `ls_tree_plans` + the `PlanEntry` struct it returned. Dead, deleted.

Migration scope shrank from 16 functions to **13**. The dead deletions also retired ~80 LoC and removed one feature dependency (`--follow` rename-following would have driven extra gix surface).

### Step 2 became `rev_parse_head`, not `commit_message`

The plan originally named `commit_message` as the step-2 spike. The audit above retired it. The actual spike landed against `rev_parse_head` (used in `rebuild.rs:131`, `preview.rs:232`, and `snapshot`'s internal call site) — same checkpoint purpose, real call sites.

The spike surfaced one real semantic divergence: `repo.head_id()` peels HEAD through to a commit object (errors on dangling HEAD); `repo.head()?.id()` reads the ref target WITHOUT peeling (matches `git rev-parse HEAD`'s legacy behavior). The dangling-HEAD regression test at `open_integration.rs:737` caught the difference. The implementation uses the non-peeling variant.

### Per-function API mapping (concrete gix 0.84 references)

| `git_io.rs` function | gix call |
|---|---|
| `rev_parse_head(repo) -> Option<CommitSha>` | `gix::open(repo).ok().and_then(\|r\| r.head().ok()).and_then(\|h\| h.id()).map(\|id\| id.detach())` — note `head().id()`, NOT `head_id()`; the latter peels and breaks dangling-HEAD semantics |
| ~~`ls_tree_plans(repo, head)`~~ | **DELETED** — dead code (zero callers) |
| `show_blob(repo, sha, path)` | `repo.find_commit(sha)?.tree()?.lookup_entry_by_path(path)? + repo.find_blob(entry.oid())?.data.clone()` |
| `is_ancestor(repo, a, b)` | `matches!(repo.merge_base(a, b), Ok(id) if id.detach() == a)` (no built-in; idiom from gix docs) |
| `first_parent_commits_between(repo, from, to)` | `repo.rev_walk([to]).with_hidden([from]).first_parent_only().sorting(ByCommitTime(NewestFirst)).all()?` then **reverse the Vec** before returning. The legacy API contract is **oldest-first** (matches `git log --reverse`); gix's walks all yield newest-first. Reversal happens in the wrapper, not at gix call time. |
| `parent_of(repo, sha)` | `repo.find_commit(sha)?.parent_ids().next().map(\|id\| id.detach())` |
| ~~`first_added_commit(repo, path)`~~ | **DELETED** — dead code (zero callers); would have needed gix rename-detection (`Rewrites`) to preserve `--follow` semantics |
| `tree_plan_paths(repo, sha)` | Recorder walk, filter by `filepath.starts_with(b".clank/plans/")` and `mode.is_blob()` |
| `tree_clank_paths(repo, sha)` | same Recorder walk, filter by `filepath.starts_with(b".clank/")` and `mode.is_blob()` |
| `commit_parent_count(repo, sha)` | `repo.find_commit(sha)?.parent_ids().count()` |
| `first_parent_commits_to(repo, to)` | `repo.rev_walk([to]).first_parent_only().all()?` then **reverse the Vec** (oldest-first contract; same reason as above) |
| `first_parent_commits(repo)` | `repo.rev_walk([repo.head().ok()?.id()?.detach()]).first_parent_only().all()?` then **reverse the Vec**. Use `head().id()` not `head_id()` (per the rev_parse_head divergence noted above). |
| ~~`commit_message(repo, sha)`~~ | **DELETED** — dead code (zero callers) |
| `diff_tree_changes(repo, sha) -> CommitChanges` | `repo.diff_tree_to_tree(Some(&parent_tree), Some(&this_tree), Some(Options{ rewrites: Some(Rewrites{ percentage: Some(0.5), .. }), location: Some(Location::Path), .. }))?` → match `Change::{Addition, Deletion, Modification, Rewrite}`. **First-parent for merges:** `commit.parent_ids().next()` IS the first parent (the legacy shell-out's `--first-parent` semantics fall out naturally — no extra flag needed). **Root commit:** when `parent_ids().next()` is None, pass `Some(&repo.empty_tree())` as the parent (matches the legacy `--root` flag's behavior). |
| `snapshot(repo_root)` | composes `head_id` + `commit_message` + `tree_clank_paths` + `diff_tree_changes` on one opened `Repository` |
| `collect_feedback_files(repo_root)` | unchanged — filesystem walk, not git |

### Sharp edges to know about up front

1. **`Commit::author()` / `committer()` return `Result`** since gix 0.76 (truly malformed headers fail). Plan error handling through `GitIoError`.
2. **`Tree::iter()` is NOT recursive** — direct children only. For "all files under `.clank/plans/`" use `gix_traverse::tree::Recorder` (allocates the full entry list) OR a custom `Visit` impl that returns `Skip` outside `.clank/`. Recorder is fine for `.clank/`-sized subtrees.
3. **No built-in `A..B` range on rev_walk** — emulate with `with_hidden(A)` from tip `B`. This excludes A and its ancestors, matching `git log A..B`.
4. **No built-in `is_ancestor`** — `merge_base(a, b)? == a` is the idiom.
5. **No built-in "first commit introducing a path"** — `first_added_commit` is the only function that needs hand-rolled logic (~20 LoC).
6. **Object cache:** call `repo.object_cache_size_if_unset(64 * 1024 * 1024)` once after opening before heavy traversal. gix docs flag this explicitly.
7. **Threading:** `Repository` is `!Send` without the `parallel` feature, never `Sync`. Use `ThreadSafeRepository` if we ever cache a handle across threads (we currently don't).
8. **gix walks are newest-first; the legacy API is oldest-first.** Every `first_parent_commits_*` function returns `Vec<CommitMeta>` ordered oldest-first (matches `git log --reverse`'s output). gix's `rev_walk` yields newest-first regardless of `Sorting` choice — `ByCommitTime(NewestFirst)` makes ordering deterministic across DAG layouts but does NOT reverse. The wrapper must `.collect()` the walk, then `.reverse()` the Vec, before returning. Forgetting this swaps the API contract silently and breaks every caller that consumes the timeline (rebuild fold, status projection, the attribution walk). Codex caught this in plan review of `42e0f8d`.

9. **Semver pre-1.0 cadence (recurring maintenance tax)**: gix releases minor versions roughly every 1-2 months and breaking changes are common in those bumps. Adding gix puts clank on a treadmill where every couple of months someone reviews a breaking-change PR. This is the recurring cost the migration accepts. The honest framing: we're trading subprocess fragility (silent runtime drift via `core.quotePath`, format changes) for library fragility (loud compile-time breakage on `cargo update`). Compile-time breakage is preferable — you find out immediately, not when a user reports a mysterious diff misparse. Pin to `0.84` (caret-major-zero matches patch only). A follow-up plan vendoring a thin wrapper crate to amortize this churn is a possibility but not in scope here.

### Scope of this plan

In:

- The 16 public functions in `git_io.rs` listed above.
- Cargo.toml change.
- Tests at the API surface stay green; existing test cases ARE the spec.
- The `run_ok` / `run_ok_raw` helpers can stay (other CLI code still shells out for mutating ops like `git commit`, `git worktree add`).

Out:

- `FileDiff` / `FileDiffMode` / `DiffHunk` / `DiffLine` in `crates/core/src/api.rs` (no current producer; future plan if a UI needs them).
- `clank purge` / worktree-mutating shell-outs.
- Performance benchmarking (interesting but not load-bearing).
- A `Repo` cache layer / handle reuse (follow-up optimization).
- Removing `run_ok` / `run_ok_raw` themselves.

## Migration order

Each step is its own commit. Reviewers approve incrementally. Goal: existing tests stay green after every commit.

1. **Add gix dependency** (Cargo.toml + Cargo.lock). No code change yet; just confirms the dep tree compiles and binary size delta is acceptable. Verify `cargo build --workspace` clean. **DONE: commit `3518604`** (5.6 MB → 5.6 MB; DCE strips unused gix).
2. **Spike-and-evaluate checkpoint: migrate `rev_parse_head` + delete dead code.** (Originally named `commit_message` but that function turned out to be dead.) After this commit, the implementer evaluates: did gix integrate cleanly into `GitIoError`? Did binary size grow within budget? If either is "no, this is awkward", the plan reverts. **DONE: commit `d2e846b`** (spike) + this commit (dead-code purge). Outcome: PROCEED. Binary 6.54 MB; ~5.5 MB headroom.
3. **Migrate `parent_of`, `commit_parent_count`** — trivial, mechanical.
4. **Migrate `is_ancestor`** — uses `merge_base` idiom.
5. **Migrate the `first_parent_commits_*` family** (3 functions). They share the rev_walk builder pattern; pull a small helper out.
6. **Migrate `show_blob`, `tree_plan_paths`, `tree_clank_paths`** — tree-iteration family. Recorder-based.
7. **Migrate `diff_tree_changes`** — the biggest win. The 10 `parse_diff_tree_*` tests in `git_io.rs:938-1048` are *scenario tests*, not parser-implementation noise. Each verifies a real behavioral case (rename out of plans, plans-with-code, finish detection via plan-deleted+finished-added, etc.). They must be **converted, not deleted**.

   **Test conversion plan — one-to-one mapping:**

   For each of the 10 `parse_diff_tree_*` tests, write an integration test in a new `crates/cli/tests/diff_tree_changes_scenarios.rs` file that:
   - Builds a real git tree using `tempfile` + `git init` + commits matching the scenario shape.
   - Calls `diff_tree_changes(repo, sha)` directly.
   - Asserts the same `CommitChanges` shape the parser test was asserting.

   Scenario coverage to preserve, by name:
   - `single_plan_intro` — first commit introducing `.clank/plans/<plan>.md`.
   - `rename_out_of_plans_is_delete` — `git mv .clank/plans/x.md other.md` registers as plan deletion.
   - `rename_into_plans_is_intro` — `git mv other.md .clank/plans/x.md` registers as plan intro.
   - `plan_revision_with_code` — same commit touches a plan and unrelated source.
   - `pure_code` — no `.clank/` touch.
   - `multi_plan_touch` — one commit touches multiple `.clank/plans/*.md`.
   - `ignores_other_clank_paths` — `.clank/agents/` etc. is not classified as plan touch.
   - `finish_detected_when_plan_deleted_and_finished_added` — paired delete+add detects finish.
   - `finished_added_alone_is_finish` — `.clank/finished/<plan>.md` added without paired delete still detects finish.
   - `finished_without_md_extension_ignored` — `.clank/finished/<plan>` with no `.md` doesn't count.

   Only after the 10 integration tests are written and green is `parse_diff_tree` + its parser tests removed in the same commit. The new tests are the spec; the parser deletion is the cleanup.

   Acceptance check for this step: `git_io.rs` line count drops by ~150-200; `diff_tree_changes_scenarios.rs` exists with 10 tests passing.
8. **Migrate `snapshot`** — falls out naturally once its primitives are migrated.

## Verification

- `cargo build --workspace` + `cargo test --workspace` green after every commit.
- After the final migration commit: `grep -n "run_ok\b" crates/cli/src/git_io.rs` must return nothing. (Mutating ops live elsewhere; `git_io.rs` is reads-only after this plan.)
- A spike test: synth a repo with `diff.mnemonicPrefix=true` set and confirm the gix-backed `diff_tree_changes` is immune (the shell-out path would silently break).
- Binary size diff before/after via `ls -la target/release/clank`. **Baseline verified 2026-06-04: 5.6 MB stripped** (the original plan-text estimate of "~30MB" was wrong — corrected here so the checkpoint arithmetic matches reality). Plan acceptance ceiling: **12 MB stripped post-migration** (~6.4 MB headroom for gix's full linked surface). Rationale: a typical gix linkage with `revision` + `blob-diff` + `parallel` + `max-performance-safe` lands in the 3-6 MB range from comparable downstream tools; 12 MB ceiling gives headroom for that without rubber-stamping unbounded growth. If exceeded at step 2 (the spike), the trim ladder fires before further commits land.
- Spot-check: at least one consumer (`rebuild.rs`) end-to-end with a real local clone, confirm no behavior regression.

## Acceptance

- `crates/cli/src/git_io.rs` contains no `parse_diff_tree`-style line-by-line text parsers.
- Every git-read operation in `git_io.rs` is backed by a `gix` call.
- `cargo test --workspace` passes (existing tests are the spec).
- The `run_ok` / `run_ok_raw` helpers may still exist for mutating operations, but no longer used by any read path in `git_io.rs`.
- Binary size growth within the documented bound; if exceeded, the migration's feature flags are trimmed before merge.

## Related history

- Plan `fix-diff-git-header-path-spaces` (FINISHED `d4f3226`): deleted the dead `parse_diff` surface rather than hardening its parser. The "text-parsing is fragile" lesson surfaced there is what makes this plan worth doing for the *live* text-parser functions (`diff_tree_changes`, `commit_message`).
