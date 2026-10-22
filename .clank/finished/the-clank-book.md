# the-clank-book
# The clank book

## Why

clank's only long-form documentation is a 1,200-line README written as
a reference, in the order features landed. It never pitches the idea:
why the review history should be the commit history, and why agents
are woken instead of polling or being chased. It also teaches none of
the three ways people actually use clank as a workflow. A new user,
or a new agent handed the repo, has to rebuild the mental model from
command flags.

We want a book: a website committed to this repo that explains clank
to both humans and agents. It should pitch the design philosophy,
then walk through the common patterns, with real terminal and zellij
recordings showing each one done. It should feel like a branding
expert and a designer sat down to make learning the tool enjoyable,
because they did.

## What ships

### `book/`: a static site with no build step

- Hand-authored HTML, one shared stylesheet, and a small amount of
  vanilla JS. No framework, no bundler, no `npm install` to read it.
- Opening `book/index.html` from disk (`file://`) works fully,
  recordings included. It also works served from any static host
  (GitHub Pages from `/book` later; configuring Pages is out of
  scope).
- Fonts are self-hosted woff2 files under an OFL licence, committed
  with their licence files, so the book renders offline and the same
  everywhere. No Google Fonts or CDN fetch at read time.
- The recording player is asciinema-player (Apache-2.0), vendored at
  a pinned version with its licence. Casts are wrapped as small `.js`
  files that register the cast data, because `fetch` of a `.cast`
  fails on `file://`.
- The book works at phone width, in light and dark mode, with visible
  keyboard focus, and honours `prefers-reduced-motion`: recordings
  never autoplay under reduced motion.
- Use the frontend-design skill for the visual identity. The book
  needs its own point of view that comes from clank's world:
  terminals, peer review, a master and reviewers, plans sealed into a
  record. It must avoid the generic generated-site defaults that
  skill lists. Check every page with headless Chrome screenshots at
  desktop and phone widths, in both colour schemes, before
  committing.

### Content

The book is one argument, in order. Each page ends by pointing to the
next.

1. **Why clank.** The pitch and the philosophy, each claim grounded
   in a mechanism the code really has:
   - one plan at a time, with peer review between agents from
     different vendors;
   - the review history is the git history (`.clank/` in the repo,
     feedback files, sealed `finished/` plans);
   - agents are woken when it's their turn, and the human is never
     the scheduler;
   - roles come from the roster, not from flags;
   - clank prefers manual control to automation where it matters.

   Where the README's tagline is now false, the book says what is
   true. For example, there IS a per-repo monitor process now; say
   what it is and what it is not.
2. **Getting started.** Install, `clank setup`, `clank init`, adding
   agents, `clank open`, and `clank doctor`.
3. **Pattern 1, group coding.** Plans, the master and reviewers:
   - drafts, the queue, promotion and the intro commit;
   - reviewer tiers (commit/plan/final/gate);
   - CONTINUE, REQUEST_CHANGES and FINISHED, and what each obliges;
   - one-commit implementation versus justified milestones;
   - `clank finish` and autosquash;
   - stash/unfinish/rereview;
   - blocks: questions only the human can answer;
   - watching it happen with `clank tui` and `clank log`.
4. **Pattern 2, teams.** The team library (`team save`, `init --team`,
   `team agent`), `fork create` into a linked worktree (and
   `--clone`, `--pr`), how forked sessions inherit context, working
   several forks at once, and `fork list` / `fork remove`.
5. **Pattern 3, a controller repo.** A repo whose agents react to
   events and run other teams:
   - wake sources: GitHub (watches, object clauses, prompts,
     realtime vs polling, what credentials each needs), timers and
     recurrence, command sources;
   - the event inbox and the react-then-ack loop;
   - observing foreign repos (`clank wait --repo … --for …`);
   - talking across repos (`clank send --repo`);
   - spawning and supervising teams in other repos.
6. **For agents.** A page addressed to an agent handed this repo or
   any clank repo:
   - how it is woken: the waiting contract is no self-scheduled
     polling and no duplicate automatic wait. Claude, Codex and
     OpenCode park in their hook or plugin. Grok's native Stop hook
     asks for one ordinary background `clank wait`, which the agent
     starts once, and that wait's completion wakes it
     (GROK-INTEGRATION.md). Separately, a deliberate observer wait
     (`clank wait --repo … --for …`) watches another repo for
     controller work without replacing this repo's automatic
     subscription. The controller pattern page teaches the same
     distinction. This documents current behaviour; it does not
     redesign the wait lifecycle;
   - `clank run` and `clank attend`;
   - inbox handling;
   - how to read feedback (`clank feedback read`);
   - commit subject tagging;
   - what each verdict obliges.

   It must agree with the shipped skills (`clank-master`,
   `clank-reviewer`, `clank-github`) and link the reader to them, not
   restate them. Where the book and a skill disagree, the code
   decides and the other one is fixed in this plan.
7. **The remote.** Driving a team from a browser or phone through
   the `clank tui` remote row, `clank remote on|off|status`, the
   default host behind its token door, one-time links and sessions,
   tunnel providers, and attaching to a separately operated host.
   Label it plainly as **experimental but very powerful**. State the
   security limitation plainly: with a Cloudflare tunnel, Cloudflare's
   edge terminates TLS. Cloudflare can therefore read everything that
   crosses it: the terminal stream, keystrokes, the one-time links and
   the session cookie. That is exactly the access a session has, so
   the remote is only as private as your trust in the tunnel provider.
   Say what avoids it: a tunnel or host you operate yourself. Prose
   and illustrations only. No scene records the remote, per the
   recording rules below, and no page shows a real token, link or
   tunnel hostname.
8. **How it works.** A short tour of the machinery a curious user
   needs to trust clank:
   - the monitor and the stop hook as a messenger;
   - the turn-end decision;
   - the `.clank/` layout;
   - what is local-only versus committed.
9. **Reference.** Links to `clank <cmd> --help` and the README
   command reference. The book does not duplicate the flag reference.

Every factual claim is checked against the current code and
`--help`, not the README. The README is known to lag.

### Recordings

Real recordings of real runs. No mocked output and no hand-typed
transcripts.

- `scripts/book/record.py` is a dependency-free recorder: Python
  `pty` writing asciicast v2 at a fixed size. It runs one scripted
  scenario and drives keystrokes with human-paced delays. Idle
  stretches are compressed to a cap so a 20-minute agent run plays in
  about a minute. The scene is named in the file, and each recording
  is reproducible by rerunning its scenario.
- Scenarios run in throwaway demo repos under a temp directory, with a
  zellij session name unique to the recording. They never attach to,
  restart or signal any existing zellij session, monitor, agent, web
  bridge or tunnel. The only process a scenario may stop is one it
  started, and it stops that process by the pid or session name it
  recorded.
- Each scenario clears inherited session, agent and zellij identity
  (`CLANK_AGENT`, `CLAUDE_CODE_SESSION_ID`, `CODEX_THREAD_ID`,
  `ZELLIJ*`) and points clank's user-level config at a demo-owned
  directory, so a recording cannot bind to or reconfigure a live
  session.
- Zellij recordings record the outer pty running `clank open` in the
  demo repo, so the viewer sees the real multi-pane team.
- Agent scenes use real claude/codex agents on a deliberately tiny
  task, so the plan → review → implement → FINISHED → finish loop is
  shown truthfully.
- **Scrubbing is a gate, not a habit.** `record.py` checks the
  reconstructed terminal output, joined across cast event
  boundaries because a token can span two PTY reads, and refuses
  to write a cast that contains any of: the user's home path (it is rewritten
  to `~` first), an email address, `ghp_`/`gho_`/`github_pat_`/
  `sk-`-shaped tokens, a `trycloudflare.com` or other tunnel URL, or
  a login/QR URL. No scenario runs `clank remote`, `clank web` or any
  credential-printing command.
- Minimum set of scenes:
  - install and `clank init`;
  - `clank open` bringing up a team in zellij;
  - a full plan cycle (promote → reviewer verdict → implementation
    commit → FINISHED → `clank finish`);
  - `clank tui` while a team works;
  - `fork create` and the forked tab;
  - a controller repo woken by a timer event, and by a GitHub-shaped
    command-source event, then sending work to another repo's team.

  Avoid a real GitHub account in recordings: a command source that
  emits GitHub-shaped JSONL is how the scene shows the reaction
  without credentials. The prose explains the real GitHub source.

### Keeping it true

A module of the `it` harness, `book_is_true`, scans `book/` in
process, without spawning the clank binary:

- Every `clank …` command line in a `<pre>`/`<code>` block parses
  through the real `Cli` (`try_parse_from`), so a renamed or removed
  command or flag fails the build. Where a line intentionally shows a
  placeholder, the placeholder syntax is defined once and substituted
  before parsing.
- Every internal link and anchor resolves, and every cast a page
  references exists.
- No page references a network resource. Everything is relative and
  in the repo.
- Every cast passes the same scrub rules `record.py` applies, with the
  patterns shared from one definition so the two cannot drift.

Mutation-check the gate: a renamed subcommand in one page, a broken
anchor, a missing cast and a planted token must each fail it.

The README gains a short pointer to the book near the top. The
README's reference content stays.

## Out of scope

- Publishing: GitHub Pages config, a domain, analytics.
- Rewriting the README beyond the pointer.
- Generating the flag reference from clap.

## Acceptance

- `book/index.html` opens from disk and every page, recording and link
  works offline.
- The three patterns each have a page with at least one real
  recording of the pattern being done end to end.
- `book_is_true` passes, and each of its four mutations fails it.
- Screenshots of every page at 1440px and 390px, in light and dark,
  reviewed and free of overflow, illegible contrast or broken layout.
- No cast contains anything the scrub rules ban.
- Nothing in the recording run touched a pre-existing session,
  monitor or agent.
