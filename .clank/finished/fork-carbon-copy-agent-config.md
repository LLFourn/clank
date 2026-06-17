# fork-carbon-copy-agent-config

`clank fork` should carbon-copy the source's per-agent config into
the fork's worktree, so a fork starts with the SAME settings the
source was using — not a blank slate. Today `run_fork` copies only
`.clank/config.json` (team selection) and seeds fork specs
(`from_session` + prompt); it does NOT copy
`.clank/agents/<label>/config.json` (auto_mode, wfw_timeout), so a
fork loses the source's settings.

## Behavior

For each team member, copy the SOURCE's per-agent settings into the
dest worktree's `agents/<label>/config.json`:
- `auto_mode` — carried as-is. Because it's `Option` (after
  [[auto-mode-default-on]]): copying `None` keeps the fork inheriting
  the `~/.clank` global default; copying `Some(on/off)` carries the
  source's explicit override. So "fork behaves like its source"
  falls out correctly.
- `wfw_timeout` — carried as-is.
- `session` — NOT copied. The fork mints its own via the fork spec /
  `clank as`. This is the one field that must be excluded.

## Depends on auto-mode-default-on

The `Option<AutoMode>` model is a prerequisite: with the old
non-Option `auto_mode`, copying would carry a baked-in `off` and
defeat the global default. Sequence this AFTER auto-mode-default-on.

## The interaction to get right

The forked agent later runs `clank as` to bind its new session. That
must MERGE into the carbon-copied config (load existing → set
session → save), NOT overwrite it — else the copied auto_mode/
wfw_timeout are clobbered. Verify `clank as`'s write path merges;
fix if it overwrites. (Same for the env-var bind hook if it writes
the skeleton.)

## Testing

In-process (no clank-binary spawning; fork uses the core directly):
- `run_fork` with a source whose config has `auto_mode = Some(off)`
  and `wfw_timeout = Some("5m")` → dest config has both, and NO
  session.
- source with `auto_mode = None` → dest omits auto_mode (still
  inherits global).
- after a simulated `clank as` bind in the dest, the copied settings
  survive (merge, not overwrite).

## Non-goals

- Copying the session (the fork is a NEW session).
- Team selection copy (already done) or fork-spec seeding (already
  done) — this adds only the per-agent settings copy.
