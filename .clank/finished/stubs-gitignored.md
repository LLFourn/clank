# stubs-gitignored

Make `.clank/stubs/` a first-class, gitignored staging area for
`clank queue add`, so an agent writes a plan straight to
`.clank/stubs/<name>.md` and runs `clank queue add <name>` instead of
staging through `/tmp` + `--from`.

## Problem

`clank queue add <name>` already falls back to `.clank/stubs/<name>.md`
as its body source (after `--from` and `-m`), so the intended workflow
is: write the stub, then `clank queue add <name>` (no flags). But
`/stubs/` is MISSING from the canonical `.clank/.gitignore` managed
set, `CLANK_GITIGNORE_ENTRIES` in `crates/cli/src/init_facts.rs`, which
lists `/agents/ /cache/ /feedback/ /queue/ /html/ /pr-reviews/
/shelved/ /worktrees/ /zellij/` but not `/stubs/`.

Consequence: in any repo that relies on `.clank/.gitignore` (a normal
clank-adopting repo, plus forks/worktrees), `.clank/stubs/` is NOT
ignored and would be tracked. It only happens to be ignored in clank's
OWN repo because the repo-root `.gitignore` has a broad `.clank/*`
rule. Agents correctly notice stubs isn't in `.clank/.gitignore` and
avoid writing there — hence the `/tmp` habit.

Because `.clank/.gitignore` is validated/repaired by SET MEMBERSHIP
("contains every managed entry, nothing foreign" — ruthless 02da305),
hand-adding `/stubs/` to the file alone would be read as foreign and
rewritten. The fix must add `/stubs/` to the managed set itself.

## Changes

1. Add `"/stubs/"` to `CLANK_GITIGNORE_ENTRIES`
   (`crates/cli/src/init_facts.rs`). This makes `init` write it into a
   fresh `.clank/.gitignore`, and makes the set-membership validator
   accept AND require it.
2. Lazy migration for existing repos, mirroring the established
   pattern (init writes/repairs; fork ensures `/worktrees/`; open
   zellij ensures `/zellij/`): have `clank queue add` call
   `ensure_clank_gitignore_entry(repo, "/stubs/")` so a repo that
   predates this entry self-heals the first time stubs is used — no
   full `clank init` re-run required. (Doctor's gitignore check will
   also flag stale bodies as before.)
3. Update the `init_facts` tests that assert the canonical body /
   classification (e.g. `classify_clank_gitignore_canonical`) for the
   new entry, and add a case proving a `/stubs/`-less body classifies
   as `Legacy` and repairs to include `/stubs/` (set membership
   preserved, no foreign entries introduced).

## Testing (in-process; no binary spawning — [[no-binary-spawning-tests]])

- `clank_gitignore_body()` includes `/stubs/`; canonical body still
  classifies `Canonical`.
- A legacy body missing `/stubs/` → `Legacy`; `ensure_clank_gitignore_
  entry(repo, "/stubs/")` adds it and leaves the file canonical-valid.
- `queue add` on a repo whose `.clank/.gitignore` lacks `/stubs/`
  results in `/stubs/` being present afterward (the lazy ensure).

## Non-goals / decisions

- NOT changing `queue add`'s source precedence — the stubs fallback
  already exists and is correct.
- Stub CLEANUP (deleting `.clank/stubs/<name>.md` once `queue add`
  consumes it) is OUT of scope; stubs are gitignored scratch. ~24
  stale stubs exist today — revisit as a follow-up only if the
  accumulation bothers.
- Agent GUIDANCE (steering agents to write `.clank/stubs/<name>.md`
  rather than `/tmp`) lives in agent prompts / the `clank` skill, not
  repo code. The repo lever here is making stubs safely ignored; the
  `queue add --help` text already documents the fallback.
