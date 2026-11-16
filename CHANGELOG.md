# Changelog

All notable changes to clank are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and clank uses
[semantic versioning](https://semver.org/). Releases are tagged
`v<version>`.

## [Unreleased]

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
- The monitor protocol is 25. v0.2.0 and the development builds before it
  speak 23 or 24, so they and this release do not serve each other: a newer
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

[Unreleased]: https://github.com/LLFourn/clank/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/LLFourn/clank/releases/tag/v0.2.0
