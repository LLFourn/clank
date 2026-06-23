# rename-stubs-to-drafts

Rename the queue-add staging area `.clank/stubs/` → `.clank/drafts/` (lloyd:
agents prefer "drafts"; "stub" reads as a code stub, not a plan-in-progress).
Small, contained sweep — but it's a gitignored dir that exists in live repos,
so it needs migration. ALSO (rolled in, lloyd): remove the `clank queue add
--from` flag — it's the abuse vector (agents write a body to `/tmp` then
`--from /tmp/x`); we want them to write directly to `.clank/drafts/<name>.md`
then `queue add`, or queue-then-promote.

## Remove `clank queue add --from`

`resolve_body_source` (queue.rs ~75) checks `args.from` FIRST, covering BOTH
`--from <file>` (the /tmp abuse) and `--from -` (stdin). Remove the flag and
both those sources:
- Drop the `--from` arg field (the `QueueAdd` args in `mod.rs`).
- Remove `BodySourceKind::FromPath` AND `BodySourceKind::Stdin` (stdin existed
  only via `--from -`) and their `describe()` arms; simplify
  `resolve_body_source` to: `-m` inline, else the drafts dir.
- Update help/error text and any tests that pass `--from`.
- KEEP `-m` (inline one-liners — not the /tmp vector). The canonical flow
  becomes: write `.clank/drafts/<name>.md` → `clank queue add <name>` (or
  `queue add <name> -m "…"` for trivial bodies). (If `-m` later proves
  abuse-prone too, removing it is a separate call — note it, don't do it now.)
- Skill (`skill_master.md` queue-add bullet): drop the `/tmp`/`--from`
  mention; state the drafts-dir flow as the way. The
  `queue-plan-via-stubs-dir` memory already says "never /tmp + --from" — now
  the flag is gone; reword to the drafts flow.

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

## Two uses of "stub" — decision (ruthless)

The grep surfaces two senses; handle each consciously:
- **(a) The staging dir AND its terminology → `draft`.** The `.clank/stubs/`
  dir, the `StubsDir` variant, the `/stubs/` gitignore entry, AND the loose
  "queued plan stub" / "consume the stub" wording (queue.rs, mod.rs ~1086,
  skill_shared_core.md "queued plan stubs awaiting promotion", status.rs ~747
  WakeFilter comment). These are all the same artifact — a not-yet-active plan
  body — so "draft" is the clean, consistent term at every stage (draft →
  `queue add` → queued draft → promote → active plan). The plan's own
  motivation ("stub reads as a code stub") applies to the terminology too, so
  it's IN scope.
- **(b) Unrelated "stub" → KEEP.** `RELEASE-CHECKLIST.md`'s "stub item 7" /
  "the stub's radar" is that doc's own term for the release checklist itself —
  nothing to do with `.clank/stubs/`. Leave it.

Drive completeness from the grep, not the enumerated list (it also hits
`mod.rs`, `skill_shared_core.md`, `status.rs` beyond the first draft's list).

## Testing (no-binary-spawning — in-process)

- `queue add` consumes a body from `.clank/drafts/<name>.md`.
- The canonical gitignore body contains `/drafts/` (and the repair adds it).
- If the fallback is kept: a body in `.clank/stubs/` is still consumed.
- (No test for `--from` being gone — don't test removed things.)

## Memory sweep

Update BOTH project-memory files that reference stubs —
`queue-plan-via-stubs-dir.md` AND `feedback-stubs-dir.md` — plus the
`MEMORY.md` index line (as part of finishing, separate from the repo commit).

## Acceptance

- `.clank/drafts/` is the queue-add staging area; `/drafts/` is the canonical
  gitignore entry.
- No `.clank/stubs/` STAGING-DIR references, and no "stub" plan-body
  terminology, in src/skills/docs outside `finished/` history (and the
  sanctioned `stubs/` fallback read, if kept).
- The unrelated `RELEASE-CHECKLIST.md` "stub" usage is deliberately kept.
- `clank queue add --from` is removed; body sources are the drafts dir + `-m`.
- Existing repos migrated before install; skill says "drafts".
