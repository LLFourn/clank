# rename-shelve-to-stash
# Rename shelve → stash, with git-stash semantics + TUI/HTML surfaces

## Rationale (lloyd)

`shelve`/`unshelve` describes the same mental model git users already have
for `git stash`: set work aside, keep it restorable, bring it back later.
Adopt the git verbs so the muscle memory transfers, and make stashed plans
VISIBLE: scrollable in `status --tui` (above the queue) and on the HTML
homepage (like queue items, but stashed plans have COMMITS worth surfacing).

## Command mapping (stay close to `git stash`)

| git             | clank (new)                | today                  |
|-----------------|----------------------------|------------------------|
| `stash push`    | `clank stash push <plan>`  | `clank shelve <plan>`  |
| `stash pop`     | `clank stash pop <plan>`   | `clank unshelve <plan>`|
| `stash list`    | `clank stash` (no args)    | only via `status`      |
| `stash show`    | `clank stash show <plan>`  | — (new)                |
| `stash drop`    | `clank stash drop <plan>`  | `clank shelve clean`   |

- `push` keeps every current flag: `--for`, `--to-queue`, `--priority`,
  `--force`, `--dry`, `--yes`, `--allow-rewrite-protected`.
- `pop` = restore + consume (cherry-pick the recorded shas onto HEAD, then
  delete the record + protective ref) — matches git pop, and is what
  `unshelve` already does. Reviews still reset by design (new shas).
- `show <plan>` prints the stashed plan body + its commit list (sha +
  subject). The body must come FROM THE PROTECTIVE REF's tree via git_io —
  a stashed plan's file no longer exists on the branch. THE REF-PATH
  CONTRACT (codex d633f87): every body/commit read resolves the item
  through the merged new+legacy scan and uses THE RECORD'S OWN
  `git_ref` field (`ShelveState.git_ref` already stores it) — never a
  path constructed from the stem. New records carry
  `refs/clank/stash/<stem>`, legacy ones `refs/clank/shelved/<stem>`;
  readers (show, the TUI overlay, the HTML page) are agnostic. A test
  pins that a LEGACY record's body renders on every surface.
- `list` output: one line per item — `name · N commit(s) · waiting for X /
  ready` (ready = the `--for` dependency finished, same nudge logic
  `status` uses today).

Deliberate deviations from git (name them in the help):
- Name-keyed, not a stack: no `stash@{0}` indices — plans have stems, and
  clank's stash was never LIFO. `pop <plan>` requires the name (no
  bare-`pop` "most recent" default; ambiguity isn't worth it).
- No `apply` (restore-but-keep): duplicate live+stashed copies of the same
  commits invite confusion; `pop` re-push is cheap if needed. Can add later
  if wanted.

## Rename mechanics

- New `clank stash` subcommand family; keep `clank shelve` + `clank
  unshelve` as HIDDEN aliases for one release (memory: keep the alias + its
  test; drop later).
- Full-repo sweep per the rename memory: `crates/*/src` AND
  README/docs/skills (`setup_assets/*.md` mention shelve), error strings,
  tests, `status` wording ("shelved", the unshelve nudge), HTML labels.
  Intentional keepers: plan-history narrative in `finished/`, this plan's
  own text.
- Storage: keep `.clank/shelved/<stem>.json` + `refs/clank/shelved/<stem>`
  paths readable, write NEW records to `.clank/stash/` +
  `refs/clank/stash/` and read BOTH for one release (the drafts/stubs
  fallback pattern) — no migration step for dogfood repos with live shelved
  state. `scan` merges both locations (new wins on collision).
- `status --json`: rename the `shelved` field to `stash`. Breaking wire
  change — call it out in the commit; dogfood consumers only.
- Internal identifiers (`ShelveState`, `scan_shelved`, `ShelvedView`,
  `run_shelve`…) rename to stash-terms in the same sweep so the code
  doesn't speak two languages.

## `status --tui`: STASH section above the queue

Order becomes: gauges → AGENTS → STASH → QUEUE → LOG.

- Reuse the QUEUE-section pattern wholesale (it solved the same problems):
  - Panel selection space extends: agents → "+ add" → STASH rows → QUEUE
    rows → cross into LOG from the last row.
  - Section hidden entirely when empty (no empty header).
  - Row: `name · N commits · waiting for X | ready` (ready in the accent
    color — it's actionable).
  - Refresh rebind: re-locate the selected stash row by NAME
    (`rebind_panel_sel` grows a stash segment — same identity rule that
    keeps the queue cursor stable).
- **Enter on a stash row → plan overlay** showing the plan body read from
  the protective ref (new `OverlayData` variant or reuse `QueuedPlan`'s
  shape with a ref-reading fetch; re-resolve by name on refresh).
- `o` opens the item's HTML page (below) via the same detached spawn.
- No mutating TUI actions for v1 (pop/drop stay CLI verbs; a TUI pop can
  come later behind a confirm).

## HTML: stash on the homepage

- Homepage block like the QUEUE block (and placed above it): each stashed
  plan links to `stash/<name>.html`, showing name, commit count, and
  waiting-for/ready. Omitted when empty.
- Per-item page `stash/<name>.html`: the plan body (from the protective
  ref) + the stashed COMMITS as a list (short sha + subject, oldest first —
  the cherry-pick order). No links to commit pages: stashed commits are off
  the first-parent history, so those pages don't exist; render them inline.
- Fully re-rendered each build like queue pages (stash changes without
  branch commits); wipe `stash/` dir per build.
- `clank html open --stash <name>` target, mirroring `--queue`.

## Tests

- CLI: push/pop/list/show/drop round-trip in-process (git fixtures fine);
  aliases still parse; pop consumes; drop discards ref + record; show reads
  the body from the ref.
- Read-both storage: a legacy `.clank/shelved/` record is listed, poppable,
  and droppable; new pushes land in `.clank/stash/`.
- TUI (pure layer): panel routing over agents+stash+queue rows (Enter/o,
  boundary crossings); rebind keeps a stash selection by name; render
  order AGENTS → STASH → QUEUE → LOG; empty-section hiding.
- HTML: homepage block ordering + omission; stash page shows body +
  commits; `html open --stash` resolves + errors clearly on unknown names.

## Open questions

- OQ1: `clank stash` bare = list (git prints the list too) — or keep an
  explicit `list` subcommand as well? Lean: bare = list, no subcommand.
- OQ2: is one release of read-both storage enough before dropping
  `.clank/shelved/` support? Lean: yes (dogfood-only).
- OQ3: split this into two plans (rename first, surfaces second)? It's one
  coherent story but a wide diff; reviewer's call at promote time.
