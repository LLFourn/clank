# auto-mode-default-on

`clank auto on` is never on by default in the user's clank sessions —
they must run it manually each time. Figure out why, and decide how a
clank session should come up with auto-mode already on.

## What's known

- `auto_mode` is per-agent, per-machine state in
  `AgentConfig.auto_mode`, and its default is `AutoMode::Off`
  (agent_config.rs; `agent.rs:218` reads it via
  `unwrap_or_default()` → Off when no config / unset).
- The ONLY thing that sets it On is `clank auto on` (auto.rs:34).
  Nothing in `clank as` (bind), `clank agent start`, fork seeding, or
  setup turns it on. So a fresh agent config = auto Off, always.
- Forked worktrees get a FRESH per-agent config: `run_fork` copies
  `.clank/config.json` (team selection) but NOT
  `.clank/agents/<label>/config.json` (session + auto_mode are
  per-worktree, gitignored). So even if the SOURCE session had auto
  on, the fork starts Off — the likely root of "my sessions aren't on
  by default" given how much forking happens here.

## Design (decided): a user-global default in `~/.clank`

Add an auto default to the existing user-global config
(`~/.clank/config.json`, the `UserConfig` read by
`crate::cli::team::read_user_config` — already used for
`cfg.zellij`). A FRESH agent config inherits it; an explicit
`clank auto on/off` still overrides per-session.

- Add a field, e.g. `auto: Option<AutoMode>` (or
  `default_auto_mode`), to `UserConfig`. Absent = today's behavior
  (Off), so other users are unaffected — opt-in by writing it.
- Resolution: when an agent's `auto_mode` is being determined for a
  FRESH skeleton (no per-agent config yet, or one without an
  explicit auto setting), fall back to the user-global default
  instead of `unwrap_or_default()` → Off. Pinpoint the single read
  site (`agent.rs:218` and wherever `clank as` first writes the
  skeleton) so the inheritance happens once, consistently.
- This covers forks for free: a forked worktree's fresh agent config
  inherits the same `~/.clank` default — no separate fork-seeding
  needed. (Note this explicitly so we don't also build
  fork-inheritance.)
- Distinguish "never set" from "explicitly set Off": once a session
  runs `clank auto off`, that explicit choice must STICK and not be
  re-defaulted to On on the next start. So the per-agent config needs
  to record presence-vs-absence (Option), not just On/Off — verify
  the current shape supports this or adjust.
- Role nuance: `clank auto on [--role master|reviewers]` exists; the
  default should apply with a sensible role scope (check auto.rs for
  what `--role` gates) — likely default applies to whatever role the
  session binds as.

## First, confirm the diagnosis

Before building: in a real session and a forked one, inspect
`.clank/agents/<label>/config.json` and confirm `auto_mode` is Off
until `clank auto on`, and that no default-on path exists today
(absent, not silently broken).

## Risks / care

- Auto-mode means the Stop hook drives the agent unattended; making
  it default-on is a behavior change. Confirm it's opt-IN via the
  chosen config (the user opts their machine in), not a hardcoded
  global default that surprises other users.

## Testing

Pure config-resolution tests (the seam), no clank-binary spawning:
- fresh agent config + user-global `auto: on` → resolves On.
- no user-global default → resolves Off (today's behavior, other
  users unaffected).
- explicit per-agent `clank auto off` → stays Off even with
  user-global `auto: on` (the "explicit overrides default" + sticky-
  off case).
- `UserConfig` round-trips the new field and tolerates its absence.

## Non-goals

- Changing what auto-mode DOES (the stop-hook loop) — only how its
  initial value is chosen.
- A separate fork-inheritance mechanism — the `~/.clank` default
  covers forked worktrees because their fresh agent configs inherit
  it.
- Per-repo auto default — user-global only, per the decision.
