# Changelog

All notable changes to clank are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and clank uses
[semantic versioning](https://semver.org/). Releases are tagged
`v<version>`.

## [Unreleased]

## [0.1.0]

The first public release.

clank runs a team of coding agents in one repository: one master and
any number of reviewers, each in its own terminal pane. They work from
plans committed to the repo and review each other's commits.

- **Harnesses:** Claude Code, Codex, Grok and OpenCode, each in a zellij
  pane opened by `clank open`.
- **Plans:**
  - a plan is a file in `.clank/plans/`, queued and promoted with
    `clank queue`;
  - plans are stashed, purged and picked across branches;
  - `clank finish` squashes a plan's commits and records it in
    `.clank/finished/`.
- **Review:** every master commit is reviewed. Verdicts are written to
  `.clank/agents/<reviewer>/feedback/` and gate the plan:
  CONTINUE, REQUEST_CHANGES or FINISHED.
- **Wakes:** a per-repo monitor wakes each agent when it has work: a
  review, a verdict, a message, a block, a GitHub event, or a timer.
  Agents hand over the turn through their harness's Stop hook.
- **TUI:** `clank tui` shows every agent, the plan, the log and event
  sources. Remote control through a Cloudflare tunnel is optional.
- **Licence:** 0BSD. `clank licenses` prints the notices of everything
  it bundles.

[Unreleased]: https://github.com/LLFourn/clank/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/LLFourn/clank/releases/tag/v0.1.0
