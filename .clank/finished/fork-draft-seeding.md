# fork-draft-seeding

`clank fork --draft <draft-name>` — fork the repo and MOVE the named
draft from the source repo's `.clank/drafts/` into the new forked
repo's queue. Repeatable: `--draft a --draft b --draft c` queues all
of them, in list order — the FIRST named draft gets the lowest
priority prefix (`000-a.md`, then `001-b.md`, `002-c.md`), so the
forked master is handed them in the order given.

The point: spin up a worktree whose team immediately starts working a
prepared backlog, without hand-copying draft files into the new tree.

## Semantics

- `--draft <name>` names a file in the SOURCE repo's
  `.clank/drafts/<name>.md` (with or without the `.md`). Validate ALL
  named drafts exist BEFORE creating the worktree — a typo must fail
  fast, not leave a half-seeded fork.
- After the worktree lands, write each draft's body into the fork's
  `.clank/queue/` with canonical `{:03}-<name>.md` names, priorities
  000, 001, … by list position. Reuse the queue-add core (or its
  naming helper) — do not hand-format queue filenames in fork code.
- MOVE, not copy: delete each source draft only after its queue entry
  is written in the fork. A failure mid-way leaves remaining drafts
  in place (idempotent retry: already-moved ones are just gone).
- Stem collision with an entry already in the fork's queue → error
  before any move, listing the colliding names. (The queue is
  gitignored per-worktree state — a FRESH fork's queue starts empty,
  so collisions only arise on re-fork against a previously seeded
  stem whose draft was since re-created.)
- Composes with the existing flags (`--prompt`, `--no-open`,
  `--path`, `--branch`). `--pr`/`--review` + `--draft` is allowed but
  pointless in practice; no special-casing.
- The forked sessions' orientation prompt should mention the seeded
  queue (e.g. "queued plans, in order: a, b, c") so the master knows
  work is waiting without running `clank queue`.

## Open question for implementation

Where the queue write happens: the fork's worktree is a checkout of
the new branch, so seeding is a plain file write into
`<worktree>/.clank/queue/`. Whether queue entries need a commit or
are picked up from the working tree by `scan_queue` — follow whatever
`clank queue add` does today (it does not commit).

## Tests (in-process, no binary spawning; extend fork_integration.rs)

- Fork with two drafts: queue of the new worktree holds `000-first.md`
  and `001-second.md` with the drafts' bodies; source drafts gone.
- Missing draft name → error, NO worktree created, no drafts consumed.
- Collision with an existing queue stem → error before any move.
- Single draft with and without `.md` suffix both resolve.
- Order: `clank wait`'s promote item in the fork surfaces the
  000-prefixed plan first (or assert scan_queue order directly).
