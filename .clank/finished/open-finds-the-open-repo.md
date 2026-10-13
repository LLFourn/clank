# open-finds-the-open-repo
# `clank open` on a repo already open anywhere says where, instead of opening it again

## Why

`clank open` only looks for the repo where it is about to open it. Inside zellij it checks
the current session's tab names by the repo's basename; outside, it attaches to or creates
`clank-<basename>`. A repo already open in another session (another `clank-*` session, a
tab in a session you started by hand, a second terminal) is opened again: a second copy of
its agent panes on the same bindings. The basename match also confuses two repos with the
same name. The user wants `clank open` idempotent across the whole machine.

## What to build

One rule governs the whole decision: whether a repo is open is decided by its canonical
path, never by a tab or session name. Names are only for display and placement.

1. **One scan, before anything is composed.** `clank open` scans every zellij session on
   the machine once per invocation, for its whole target set (one repo, or every
   `--all` target), with the existing bounded scan
   (`open_zellij::panes_in_all_sessions_async`). A live pane belongs to a target when:
   - it is any clank agent launch for that target's canonical path, whatever its label
     and whether or not the label is on the current roster, read with the existing
     exact parser (`agent_pane_label`), so a renamed or removed agent cannot hide an
     open repo;
   - or it is the target's status pane, in its current or legacy spelling.

   A pane that has exited does not count.
2. **If found, say where and stop.** Locations are grouped by session and stable tab
   id. The description shows, for each:
   - the session;
   - the tab's name and its position (the `tab_position` zellij shows, not its id);
   - the agents with live panes there.

   How to get there depends on where the caller is:
   - **outside zellij:** `zellij attach <session>`, then the tab;
   - **inside that same session:** switch to the tab at its position;
   - **inside a different session:** reach that session first (zellij's session switcher,
     or detach and `zellij attach <session>`), then its tab. "switch to tab N" alone
     would act on the wrong session.

   It exits successfully and creates nothing. Several locations are all listed.
3. **If not found, open it, with names never standing for identity.** Placement keeps
   today's conventions where they do not collide:
   - **inside zellij,** a new tab named for the repo's basename in the current
     session;
   - **outside,** the `clank-<basename>` session.

   A tab or session of that name that belongs to a different repo neither suppresses
   the open nor is attached to. Inside, the target opens in a new tab anyway, and the
   tab name is made unique with its parent directory (e.g. `repo (b)`). Outside, a
   `clank-<basename>` session already holding another repo is left alone, and the target
   opens in a session named uniquely the same way. `open_one`'s tab check, `open_all`'s
   and `missing_tabs`' membership, and the named-session attach decision all take the
   scan's path-based answer, and `--all` carries each target's path, not its basename.
4. **A scan that cannot answer is not "not open".** If the listing fails or a session does
   not answer within the deadline, `clank open` refuses, naming the sessions that did not
   answer and saying to retry, and opens nothing. `--print` stays offline: it does not
   scan, as today.
5. **`--all`, `--fork` and `--pr`** use the same scan result: a target open anywhere is
   reported and skipped, the rest open.

## Acceptance

A test drives the production decision with a fixture scan through its seam (the existing
`OpenIo`, or the scan type), never spawning zellij or clank:

- **Open in another session:** reports it and opens nothing.
- **Open in the caller's session:** reports it as here, with its tab position.
- **The caller inside zellij, the repo in a different session:** the hint reaches that
  session first.
- **Open in two places:** lists both accurately.
- **A moved tab** (a tab id that does not match its position): the location shows the
  position.
- **An exited pane only:** opens.
- **A renamed or off-roster agent's live pane:** found.
- **`/a/repo` open and `/b/repo` requested:**
  - with `/a/repo`'s tab in the current session, `/b/repo` is opened in a new, uniquely
    named tab;
  - with `/a/repo` in `clank-repo`, `/b/repo` opens in a uniquely named session and is
    never attached to `clank-repo`.
- **`--all` targets sharing a basename:** each is decided by its path, and the missing
  one is opened.
- **`--all` with one target open elsewhere:** opens only the others.
- **A failed or partial scan:** refuses, naming the unanswered sessions, and opens
  nothing.
- **Existing open tests** still pass.

One implementation commit.

## Out of scope

Focusing or switching to the found tab automatically. Repos open without clank's agent or
status panes.

## As built

- **One scan, through the seam.** `run` is now `run_with` over `OpenIo`, which gains
  `scan` (`panes_in_all_sessions_async`, 3 s) and `report`. Unless `--print`, the scan is
  required to be complete (`require_complete`: a failed listing, or any unanswered
  session, named, refuses with nothing opened).
- **Matching.** `sort_targets` splits the invocation's targets into those open
  somewhere (reported) and the rest. `locations` matches a live pane to a target by
  canonical path:
  - `pane_for` is matched against the target, not parsed on its own. For `clank agent
    start <label> --repo <path>` it tries every place ` --repo ` could end the label,
    taking one only where the label is a valid agent label and the path after it is the
    target, so a path or label holding spaces or ` --repo ` itself is read right. It
    also matches `clank tui --repo <path>` and the legacy `clank status --repo <path>
    --tui`;
  - `is_repo` compares canonical paths, so an alias of the directory matches;
  - exited panes do not count;
  - locations are grouped by session and `tab_id`, and carry `tab_position`, newly read
    from `list-panes --json --tab`.
- **The description** (`describe`) gives each place's session, its tab as
  `tab <position+1> `name``, and its live agents. How to get there depends on the
  caller:
  - outside zellij: `zellij attach <session>`, shell-quoted;
  - in the same session: switch to the tab;
  - in another session: reach that session first (the session manager, or detach and
    attach), then the tab.
- **Names place, never identify.**
  - `open_one`/`open_tab` no longer skip a same-named tab, nor attach to a live
    same-named session.
  - Inside zellij, the tab gets `unused_tab_name`: the basename, else `basename
    (parent)`, else numbered, checked glyph-stripped against every tab there, including
    those the same `--all` just added.
  - Outside, a live conventional session is another repo's (the scan found the target
    nowhere), so `unused_session_name` picks `clank-<basename>-<parent>[-n]`, unused by
    any listed session, live or dead.
  - `--all` outside (`plan_all`) adds tabs to the conventional session only when this
    repo is open there (`hosts`), else creates a session of its own.
  - `tab_is_open`, `missing_tabs` and `attach_argv` are gone.
- **Session listing.** `listed_sessions` reads each line as zellij frames it:
  - the frame is the last ` [Created …]`;
  - the name is everything before it, kept whole whatever it contains (spaces,
    `EXITED`, even ` [Created (EXITED`);
  - the state is read only after the frame's closing `]`.

  A line whose frame cannot be found or does not close is a session not seen: both
  scans report it unanswered, so the scan is incomplete and `clank open` refuses.
- **README** says what `clank open` does when the repo is already open, and what an
  unanswered session means.
- **Mutation-checked:** 8 mutants, all caught:
  - a repo known by its name;
  - an exited pane holding the repo;
  - the tab id shown as its place;
  - a foreign-session caller told to switch tab;
  - an unanswered session ignored;
  - another repo's session getting the tabs;
  - another repo's session reused;
  - a session name read as its first word.
- **`--all` through `run_with`:** a test with a real git worktree has one target open
  in another session and one missing. It reports the first, and composes and creates a
  session holding only the second.
- **Revision mutation-checked:** 3 more mutants, all caught: state read from the whole
  line, only the last ` --repo ` tried, and an unreadable line dropped.
- **Frame revision mutation-checked:** taking the first frame marker, and reading an
  unclosed frame, are both caught.
