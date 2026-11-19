# Changelog

All notable changes to clank are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and clank uses
[semantic versioning](https://semver.org/). Releases are tagged
`v<version>`.

## [Unreleased]

## [0.3.0]

Released 2026-11-19.

This release changes what clank stores about each agent and how its monitor
talks to clients, with no migration from v0.2.0: read Breaking and Upgrading
before installing it over a running team.

### Breaking

- An agent's runtime state is one file, `.clank/agents/<label>/state.json`:
  the session bound to it, the monitor's turn record, its questions, its
  recovery episode and attempt numbers, what reached it and what status
  shows of it. `config.json` holds only what you set. Nothing is migrated:
  after upgrading every agent is unbound and its hooks go quiet, so restart
  every agent (a pane `clank open` launched binds again as it starts).
  `turn.json`, `question.json`, `recovery.json`, `recovery-issued.json`,
  `delivered.json`, `ledger.json`, `binding.lock` and `question.lock` are no
  longer read or written and can be deleted, with the long-dead
  `repeat.json`, `repeat.lock` and `attending.holder`, as can a `session`,
  `resume_pending` or `outage_notified` left in an agent's `config.json`.
- The monitor protocol is 25. v0.2.0 speaks 23 and the development builds
  since it speak 24, so they and this release do not serve each other: a newer
  client replaces an older monitor, and an older client is declined. Restart
  agents still running an older binary after upgrading. Protocol 25 names the
  producer of a source restart, and of each source in the published sources
  account, as a tagged value rather than its display name: an agent labelled
  `watch:1` and repository watch 1 are no longer confused. A sources account
  written by an older monitor reads as unreadable until that monitor is
  replaced.
- Blocks left in the per-agent layout (`.clank/agents/<label>/blocks`,
  `unblocks` and `blocks.lock`) are no longer migrated or read. Ask any that
  are still pending again; the old directories and `.clank/block-migration.json`
  can be deleted. `clank block migrate` is removed.
- Messages left in an old per-agent file inbox (`.clank/agents/<label>/inbox/`)
  are no longer moved into the event log. Read or delete them before
  upgrading; the directory can be deleted.
- A block's answer is delivered best effort and then deleted, not kept.
  Once the wake carrying it is emitted, the question, answer, creator and
  attempt files are removed, and no later session or master is shown it
  again; nothing records which session received it. A submitted
  `block answer --nudge`, or `block answer --delivered`, deletes it the same
  way. `clank block list` no longer shows delivered decisions, and
  `clank block clean` is removed. Asking a name whose question still exists,
  pending or answered and undelivered, is refused. Answers already recorded
  are delivered once more after upgrading, and `.received` files are no
  longer read and can be deleted.
- Only the master raises a block. `clank block ask` from a reviewer or any
  other agent, `--permission` included, is refused before anything is
  written: a reviewer is told to write REQUEST_CHANGES so the master can
  raise it, anyone else to `clank send --master`. A person at a shell with
  no agent identity can still ask. An identity that fails to resolve (a bad
  session variable, an unbound session) is refused rather than taken for a
  person. Reviewer and standby wakes no longer suggest `clank block ask`.

### Added

- `clank remote link` prints a fresh one-time login link for the repo's
  remote, the one the TUI's remote page shows, alone on stdout. Whoever
  reads that output can log in, an agent included. It does not switch the
  remote on; when the remote is off it says to run `clank remote on` first.
- Codex and OpenCode agents are told when a `clank run` they started ends:
  a parked agent is woken at once, and one that is busy hears at its next
  Stop. Clank does not know the run's exit status, so the notice asks the
  agent to collect the result where it started the run. Claude Code and
  Grok report the end themselves and get nothing extra. A bare
  `clank attend` still stands until cleared and checks in at `--expect`.
- `clank doctor` has a "harness fixtures" section. It warns when an
  installed harness's version differs from the one clank's recorded hook
  payloads came from. No recording yet names its version, so an installed
  Claude Code or Codex currently reports this warning; it is advisory.
- The TUI can be told to try again where a failure is waiting on you: `m`
  in the log attempts a monitor when none could be attached, `s` retries a
  failed status rebuild and `w` watches the installation again. A stalled
  pane in the remote browser view has a retry button.

### Changed

- Agent pane titles name their repo: `<glyph> <label> · <repo>`, for example
  `💤 codex · clankstack`, so zellij's pane list tells apart same-named
  agents of different repos in one session. `<repo>` is the base name of the
  repo's tab. Panes opened by older builds are still found and are retitled
  once a TUI of this build is running. The TUI also retitles panes it
  previously missed.
- No more polling except GitHub's. Retries that ran on a timer with no end
  now wait for an event that can end the failure, and give up and report it
  by a fixed deadline when none comes:
  - a `clank wait`, hook or TUI with no monitor waits for the repository's
    lease to be released, a monitor to start, an installation or a config
    file change, instead of retrying every few seconds. A wait whose
    session was already told of the outage stays parked quietly until one
    of those happens;
  - recovery no longer re-looks at a stalled agent every minute; it acts
    when something it waits on moves, and a recovery that cannot proceed is
    shown in `clank status` after its deadline;
  - timers that cannot be written, failed status rebuilds, failed
    self-replacement after an install, failing command sources, the remote
    bridge's reconnect and the remote host's session checks wait on their
    cause, not a backoff.
- The monitor never waits on a lock another clank process holds (an agent's
  state, blocks, event logs, the ingest lease). A held lock delays only the
  work that needs it, and serving every other agent goes on.
- Claude Code shows an `AskUserQuestion` prompt at once: the question hook
  no longer waits for a monitor to start before answering, and it no longer
  prints a routing error for permission requests.
- The agent detail menu always offers `reopen pane` when the agent's zellij
  session can be found, whatever the pane's last known state. Choosing it on
  a live pane says it is already open. When no session can be found the
  page says why.
- Watches: adding or retargeting a watch that would give an agent two
  producers of the same source is refused, naming the other one, and
  `clank events watch retarget` or `transfer` to an agent not on the roster
  is refused. A master change is never refused because of watches; where
  it creates a duplicate, one producer hosts, the other ingests nothing new
  while its existing events stay owed, and `clank status` reports which
  won.
- Documentation, skills and `clank run --help` say per harness who reports
  a background run's end, rather than promising that every harness does.

### Fixed

- After a machine sleeps, `clank status` no longer reports a GitHub source
  as failing ("no completed poll for 20h") while it polls normally. Poll
  times are wall-clock times.
- A Grok Stop that the monitor answers late arms Grok's background wait
  instead of leaving the agent stranded until someone steps in.
- `clank open`, `clank as`, the TUI and the remote view work outside zellij
  when the shell's ancestry cannot be read, as in a new Ghostty window on
  macOS. Inside zellij an unreadable ancestry no longer stops `clank open`
  either. The TUI no longer claims "no Zellij session found" before its
  first listing.
- An agent labelled like a watch (`watch:1`) can read and acknowledge its
  own events and is not confused with repository watch 1.
- A rebound master is no longer shown every uncleaned block answer again,
  one per turn.
- An agent with no `config.json`, on the defaults, is bound, recoverable and
  healthy in `clank doctor`.
- A held block, event-log, inbox or agent lock can no longer stall the
  monitor for every agent in the repository.

### Upgrading from v0.2.0

- After upgrading every agent is unbound: its session was stored in a file
  this release no longer reads. Restart each agent (a pane `clank open`
  launched binds again as it starts), or run `clank as <label>` inside the
  agent's session.
- Restart TUIs, monitors and anything else still on the older build. An
  older monitor is replaced by the first client of this build; an older
  client is declined. `clank remote` commands tell you to restart a TUI of
  the older build, whose remote-control protocol they no longer speak.
  Agent panes take their new `<label> · <repo>` titles once the TUI is
  restarted.
- Run `clank setup` to refresh the installed skills and hooks; `clank doctor`
  reports the ones an older setup wrote.
- Before upgrading, read or delete messages left in an old per-agent inbox.
  After upgrading, ask again any block that was still pending in the
  per-agent layout.

### Platform and verification notes

- Platforms, packaging and licence are as in 0.2.0: macOS and Linux, arm64
  and x86_64; Linux binaries are static musl and need Linux 5.3 or newer;
  macOS binaries are unsigned and not notarized.
- Clean-account, installed-harness acceptance walks are still not recorded
  as passed for any harness.

### Missing from the 0.2.0 notes

Two changes landed in v0.2.0 after its notes were written.

- A hook whose monitor cannot be reached, or does not answer before the hook
  must, no longer wakes the agent with an outage notice. It emits a diagnostic
  and leaves pending work undelivered (Grok's Stop asks for its background
  wait instead), so automatic progress can stop until a monitor is available
  and a new hook or wait runs. A background `clank wait`
  fails once per outage with `clank cannot start its monitor: …`; a later wait
  of the same session retries quietly until a monitor serves it, which ends
  the outage.
- Grok's native hooks (`hooks/clank.json`) run with a 30-second timeout rather
  than 5, to fit the monitor exchange and the hook's cleanup. A file written by
  an earlier build reports as drifted: `clank doctor` flags it, and an
  automatic Grok agent will not launch, until `clank setup` (with
  `--grok-agent <label> --repo <repo>` for an agent's own Grok home) rewrites it.

## [0.2.0]

The first published release of clank. The previous `0.1.0` version identified
development builds; there was no published v0.1.0 release.

clank runs a team of coding agents in one repository: one master and any number
of reviewers, each in its own terminal pane. They work from committed plans,
review each other's commits, and hand work over through their harness's Stop hook.

### Added

- Integration with Claude Code, Codex, Grok and OpenCode, with agent panes opened
  by `clank open` in zellij.
- A plan and review workflow: queue, promote, stash, restore and finish plans;
  review verdicts gate progress, and finishing records and squashes the plan.
- A terminal dashboard for agents, plans, history and event sources, with optional
  remote browser access through a Cloudflare tunnel.
- GitHub, command and timer event sources. Repository watches target the master
  by default and follow role changes; explicitly named agents remain supported.
- Background-work attendance and check-ins, with delivery receipts and recovery
  for work that has not reached an agent.
- Repository-owned human questions. `clank block list` keeps answers inspectable;
  answer delivery follows the current master, including across handover.
- macOS arm64/x86_64 and Linux arm64/x86_64 release archives containing both
  `clank` and `clank-web-cloudflare`, licences and dependency notices. The website
  installer verifies SHA256 checksums and installs both executables.

### Changed

- Long-lived local processes react to filesystem, process and connection events
  instead of periodic polling. GitHub sources retain their configured polling.
- Wake messages are shorter and state the action needed to discharge each item.
- `clank status --watch -j` can watch multiple repositories in one process.
- Repository and user configuration have explicit format handling, migration and
  errors for unreadable settings; running processes retire across build upgrades.
- `clank init` chains existing plain post-rewrite hooks, respects `core.hooksPath`,
  and leaves symlinked or hook-manager-owned hooks alone. `clank doctor` reports
  how the hook is wired.

### Fixed

- Timing-sensitive tests and process shutdown fixtures now synchronize on the
  events they need. Shared Rust/Git fixtures reduce subprocesses. The Rust test
  suite and release tooling no longer require Python; the optional screencast
  recorder still uses it.
- Pending blocks pause the repository's review cycle without withholding unrelated
  messages or events. Legacy questions, answers and receipts migrate together;
  damaged delivery evidence stays unresolved instead of replaying old decisions.

### Upgrading from development builds

- Use `clank block answer <name> -m "answer"`, without an agent argument.
  `block ask --author` is removed. Block names are repository-wide; legacy
  name collisions receive stable suffixes reported during migration.
- Old hidden CLI aliases and `clank init --force-hooks` are removed. Use
  `clank --help` and the generated command reference for current spellings.

### Platform and verification notes

- Linux binaries use static musl and require Linux 5.3 or newer. `clank open`
  requires zellij; agent harnesses are installed separately.
- macOS binaries are unsigned and not notarized. Use the installer or build with
  Cargo; browser downloads may be blocked by Gatekeeper.
- Clean-account, installed-harness acceptance walks are not yet recorded as
  passed. The README's harness table distinguishes source version requirements
  from tested versions; automated fixture tests do not establish those claims.
- Clank is licensed under 0BSD. `clank licenses` prints bundled licence notices.

[Unreleased]: https://github.com/LLFourn/clank/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/LLFourn/clank/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/LLFourn/clank/releases/tag/v0.2.0
