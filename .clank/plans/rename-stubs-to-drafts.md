# rename-stubs-to-drafts

Rename the queue-add staging area `.clank/stubs/` → `.clank/drafts/` (lloyd:
agents prefer "drafts"; "stub" reads as a code stub, not a plan-in-progress).
Small, contained sweep — but it's a gitignored dir that exists in live repos,
so it needs migration.

## Sweep (one up-front full-repo grep for `stub`/`stubs`, not just src)

Code:
- `crates/cli/src/cli/queue.rs`: the `.clank/stubs/{name}.md` path, the
  `BodySourceKind::StubsDir` variant → `DraftsDir`, the "write
  `.clank/stubs/{name}.md` and re-run" error string, and the comments.
- `crates/cli/src/init_facts.rs`: the `/stubs/` gitignore entry (the canonical
  `clank_gitignore_body`), the gitignore-repair logic + its tests (~281–301),
  and the doc comments (~11, 433).
- `.clank/.gitignore`: `/stubs/` → `/drafts/`.

Docs / skills:
- `setup_assets/skill_master.md` (the `queue add` bullet: "the stubs dir is
  the gitignored staging area" → drafts).
- `README.md`, `RELEASE-CHECKLIST.md` if they mention stubs.

KEEP (intentional, do not rewrite): `.clank/finished/*` history (narrative,
incl. `skill-queue-add-stub-idiom`) and any negative/parse tests.

## Migration (install-breaking-config playbook)

The gitignore entry is a schema-ish change and the dir holds LIVE drafts.
With the freshly-BUILT (not-yet-installed) binary, BEFORE `cargo install`,
for every repo (global + this repo + per-worktree `.clank/`):
- rename `.clank/stubs/` → `.clank/drafts/` (this repo currently has ~6 draft
  files in `.clank/stubs/` — move them, don't lose them);
- update each `.clank/.gitignore` `/stubs/` → `/drafts/` (or re-run the
  gitignore-repair path with the new binary).
Enumerate repos with `find` (skip nested `.clank/`), migrate, validate each.

Back-compat option (decide in impl): have `queue add` read `.clank/drafts/`
and FALL BACK to `.clank/stubs/` for one release so an un-migrated repo's
in-flight drafts aren't orphaned — or rely solely on the migration. Lean
toward the fallback (cheap, one `if`), with a comment to remove it later.

Also update the `queue-plan-via-stubs-dir` auto-memory + MEMORY.md to say
drafts (separate from the repo; do as part of finishing).

## Testing (no-binary-spawning — in-process)

- `queue add` consumes a body from `.clank/drafts/<name>.md`.
- The canonical gitignore body contains `/drafts/` (and the repair adds it).
- If the fallback is kept: a body in `.clank/stubs/` is still consumed.

## Acceptance

- `.clank/drafts/` is the queue-add staging area; `/drafts/` is the canonical
  gitignore entry.
- No `stubs` references outside `finished/` history (and the sanctioned
  fallback, if kept).
- Existing repos migrated before install; skill says "drafts".
