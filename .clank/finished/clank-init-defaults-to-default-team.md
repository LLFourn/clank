# clank-init-defaults-to-default-team

`clank init` with no `--team` leaves the repo with NO team
configured. Then every workflow command fails:

```
$ clank init
$ clank open zellij
this repo has no team configured. Run `clank init --team <name>` first.
```

That's broken UX — a bare `clank init` should Just Work for the
common case.

## Root cause

This is a deliberate decision from the `teams-based-agent-registration`
hard cut that turned out wrong in practice. That plan's "AS
SHIPPED" section says: "`clank init` with no `--team` does NOT
pick a `default` team and does NOT migrate old configs." The
intent was to avoid magic. But the result is that the most
natural first command (`clank init`) produces a repo that
immediately errors on the next command.

## The fix (lloyd 2026-06-09)

`clank init` (no `--team`) sets the repo to the user's
**`default` team** when one exists:

- If `~/.clank/config.json#/teams` has a `default` entry →
  write `team: "default"` to `<repo>/.clank/config.json` (same
  as `clank init --team default`). Fixes the common case: an
  established user with a `default` team gets a working repo
  from a bare `clank init`.
- `clank init --team <name>` keeps overriding with an explicit
  team (unchanged).

## Decisions (pinned)

- **No `default` team in user-scope** → `clank init` STILL
  succeeds (scaffold + hooks + perms are the core job and must
  not fail), but sets no team and prints a clear WARNING:
  "no `default` team in ~/.clank/config.json; run
  `clank init --team <name>`, or create one with `clank team
  create default` + `clank team set-master`/`clank team add`."
  Rationale: `init` must NOT invent a team composition (no
  master/reviewers to choose) and must NOT hard-fail the
  scaffold. The brand-new user (no `~/.clank` at all) hits this
  warning on first `init` — guided, not blocked. No interactive
  walkthrough (out of scope; the warning names the commands).
- **Exactly one team, not named `default`** → still requires
  `--team`. Only the literal `default` is the implicit pick; any
  other name is explicit. No "use the only team" guessing.
- **Supersedes** the now-finalized `teams-based-agent-registration`
  plan body's "no default-team fallback" statement. That plan is
  history in `.clank/finished/`; do NOT edit it — this plan
  records the reversal.

### Sizing concerns folded (ruthless cc4c7c8)

- **Never clobber an existing team on re-init** (the regression
  risk): the `default`-adoption applies ONLY when the repo has no
  `team` field yet. A bare `clank init` on a repo already set to
  `team: dev` (e.g. re-run to refresh hooks/perms) PRESERVES
  `dev`. Implemented via a `repo_has_team(repo)` guard before the
  default path; `--team <name>` remains the explicit override.
- **Warning → stderr** (not stdout): per the convention (stdout =
  artifact, stderr = metadata/warnings), the no-default warning
  prints to stderr so it doesn't pollute output and tests can
  assert it separately.
- **Missing `~/.clank` is empty, not an error**: verified —
  `read_user_config`/`register_repo_team` treat a non-existent
  user config as an empty `UserConfigFile` (NotFound → default),
  so a first-ever `clank init` warns rather than failing at the
  read. Pinned by `bare_init_without_default_team_succeeds_and_warns`
  (fresh HOME → exit 0 + warn).

## Surfaces

- `crates/cli/src/cli/init.rs` `run()` — when `args.team` is
  `None`: read user-scope via
  `crate::cli::team::read_user_config(home)` (pub) and, if
  `teams` contains `default`, call
  `register_repo_team(home, &repo, "default")`; else emit the
  warning above. (`register_repo_team` — NOT the old
  `write_repo_team_field` name; renamed during
  `dogfood-init-setup-in-tests`. It already validates the team
  exists in user-scope and is the same core `--team` uses.)
- Tests: `default` user team present → bare `clank init` writes
  `team: "default"` to the repo config; no `default` team → init
  exits 0, writes no team field, warns. Set up the user-scope
  `default` team via `common::TestEnv` + the real cores
  (`register_team`) — the dogfood pattern.

## Status

Stub — queued 2026-06-09 (real bug: bare `clank init` →
unusable repo).
