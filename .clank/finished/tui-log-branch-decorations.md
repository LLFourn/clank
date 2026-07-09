# tui-log-branch-decorations
# tui log branch decorations

In `clank status --tui`, decorate log commit rows with the branch names
whose tips point at them — `git log --decorate` restricted to a small
relevant set, resolved from LOCAL git data via gix. Nothing on this path
may touch the network.

## Why

In a worktree the log is one undifferentiated stream; the orientation you
want is "where is my base branch / its remote in this history?" Branch
names on their tip commits answer that at a glance, and unlike a single
divider they stay honest when the base has advanced past the fork point
(its tip simply isn't in the window).

## Relevant refs

Each is one cheap local ref read (gix, inside `git_io`); resolve to a sha
and skip silently if absent:

- HEAD's own branch (worktree branch name)
- its configured upstream, if any
- the main repo's checked-out branch (the base a clank worktree split
  from) — `fork::main_repo_root` + `git_io::current_branch_at`
- the remote-tracking ref of that base (e.g. `origin/master`)

The set is deliberately a starting point — keep it a data-driven list so
adding/removing a ref is a one-line change.

## Sketch

- `git_io` gains a read that resolves a list of ref names to shas
  (gix ref lookups; no subprocess, no network — the layer boundary
  already enforces this).
- Build a `sha → Vec<ref name>` map once per snapshot refresh (and per
  `tui_log_rows` re-fetch), NOT per frame and NOT per commit — a handful
  of ref reads total, so log build time is unchanged.
- Carry the map as a sidecar in the TUI snapshot (do not change
  `OnelineRow` — the CLI `clank log` renderer stays untouched).
- Render: on a matching commit row, append the names after the subject,
  e.g. `⚒ 4518a0d subject (master, origin/master)`, colored so they read
  as decoration not subject text; exact palette at implementer's
  discretion, consistent with the TUI's existing accent styling.
  Truncation follows the row's existing width handling.

## Non-goals

- No merge-base/fork-point divider (superseded earlier revision of this
  idea; may return separately).
- No fetching, no remote API calls, no `git` subprocesses on this path.
- No decoration of arbitrary refs/tags — only the relevant set above.

## Acceptance

- In a worktree whose base tip (or its origin ref) is inside the fetched
  log window, those rows show the branch name(s), colored.
- On the main repo, HEAD's branch decorates the top commit; absent refs
  (no upstream configured, no remote) degrade to no marker, silently.
- Rows never gain decorations they don't own; multiple names on one sha
  render comma-separated in one parenthetical.
- Implementation uses only gix reads inside `git_io` (boundary test stays
  green); no measurable log build regression.
- fmt/clippy at baseline; unit tests for the ref→sha map (absent refs,
  multiple refs on one sha) and a render test pinning the decorated row.
