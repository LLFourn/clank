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

### One shared resolver — the load-bearing piece (codex b29d8d0)

The default must NOT be applied only at skeleton-creation / agent
start: the **Stop hook is a separate read path**. Today
`stop_hook.rs` returns Silent when `load_agent_config` is `None` and
otherwise matches `cfg.auto_mode` directly — so a fresh session
(no skeleton yet, or auto unset) would stay silent even with a
user-global `auto: on`. That's the exact case the user hits.

So introduce ONE resolver — `effective_auto_mode(per_agent:
Option<&AgentConfig>, user_default: Option<AutoMode>) -> AutoMode` —
that resolves: explicit per-agent setting wins; else the user-global
default; else Off. Route EVERY consumer through it:
  - `stop_hook.rs` (the auto gate — must use it for the `None` /
    auto-unset case, or fresh sessions never auto-continue),
  - `agent start` initial-prompt resolution (`agent.rs:218`,
    feeding `resolve_initial_prompt`),
  - `clank auto status` (report the EFFECTIVE mode, not just the
    stored one),
  - skeleton writes / `clank as` (whatever it persists stays
    consistent with what the resolver returns).
No consumer reads `cfg.auto_mode` (or `unwrap_or_default()`) raw
anymore — the resolver is the single source of truth.
- This covers forks for free: a forked worktree's fresh agent config
  (or absent config) resolves through the same `~/.clank` default —
  no separate fork-seeding needed. (Note this explicitly so we don't
  also build fork-inheritance.)
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

Pure `effective_auto_mode` resolver tests (the seam), no clank-binary
spawning:
- fresh / absent agent config + user-global `auto: on` → resolves On.
- no user-global default → resolves Off (today's behavior, other
  users unaffected).
- explicit per-agent `clank auto off` → stays Off even with
  user-global `auto: on` (explicit overrides default + sticky-off).
- **Stop-hook path under a user-global default with NO per-agent
  config → resolves On (not Silent)** — the regression codex flagged;
  pin it via the stop-hook's auto-resolution seam (the in-process
  core, not a spawned binary).
- `UserConfig` round-trips the new field and tolerates its absence.

## Non-goals

- Changing what auto-mode DOES once on (the stop-hook continuation
  behavior) — we only change how the effective auto_mode is RESOLVED
  (routing the stop hook + others through the shared resolver), not
  what happens when it's on.
- A separate fork-inheritance mechanism — the `~/.clank` default
  covers forked worktrees because their fresh agent configs inherit
  it.
- Per-repo auto default — user-global only, per the decision.
