# external-wake-hints-carry-payload
# external wake hints carry their payload on every channel

`clank wait --json` gives a github wake the full item (repo, event,
detail, number, title, actor, url) — but the two hint channels agents
actually read lag it (lloyd asked 2026-07-13):

- The HUMAN one-liner (what a claude/grok agent's armed wait prints)
  drops the `url` — the single most actionable field — and the
  `detail` (which pr_comment class fired).
- The CODEX in-hook renderer is effectively broken for the new kinds:
  its typed WaitItem mirror (stop_hook.rs) deserializes none of the
  github/command fields and has no `github_event`/`command_event`
  arms, so the loose fallback renders `- github_event: ? @ ?`.

## Fix

- Human one-liner: append the `url` (plain text — an agent parses it,
  no OSC-8) and the `detail` when present:
  `github   pr_comment (review)  o/r  #12  Add the widget  by hubot  https://…`.
  `command_event`'s line already carries name/exit/tail-snippet —
  unchanged.
- Codex hook mirror: add the optional fields (repo, event, detail,
  number, title, actor, url; exit_code, output_tail) and real arms:
  `- github: pr_opened o/r #12 "Add the widget" by octocat <url>` and
  `- command: <name> exit <code>: <last tail line>`. Keep the mirror
  presence-tolerant (every field optional, unknown kinds still fall
  through).

## Acceptance

- Human-render test: a full github item renders kind/detail/repo/
  number/title/actor/url on one line; absent optionals degrade
  cleanly (no dangling separators).
- Hook-render test: github_event and command_event items (built from
  wait's actual `--json` field names) render the real hints, not the
  `? @ ?` fallback; an unknown future kind still falls through loose.
- The wire shape is unchanged (JSON emitters untouched); existing
  hint tests for plan kinds byte-identical.
- fmt/clippy at the 18/6 baseline; suites green.
