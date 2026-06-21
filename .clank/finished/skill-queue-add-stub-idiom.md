# skill-queue-add-stub-idiom

The master skill (`skill_master.md`) tells agents to `clank queue promote
<name>` but never documents how to CREATE a queued plan. With no guidance,
agents stage the plan body in `/tmp` and pass `--from` — which bypasses
the gitignored `.clank/stubs/` staging area and leaves a stray temp file
(the `--from` source is deliberately not consumed).

## The model

`clank queue add <name>` already has a designed authoring path: write the
body to `.clank/stubs/<name>.md`, then `clank queue add <name>` with no
`--from`/`-m`. The stubs dir is the gitignored staging area (`/stubs/` in
`.clank/.gitignore`) and `queue add` CONSUMES the stub on use (it
self-empties — see queue.rs "the stubs-dir source is consumed"). The skill
just doesn't say so, so agents invent a worse route.

## Change

- In `crates/cli/src/cli/setup_assets/skill_master.md`, in the
  "Commands you own" list, add a `clank queue add <name>` entry that
  states the idiom: write the body to `.clank/stubs/<name>.md` first, then
  `clank queue add <name>` (the stub is consumed; lower `--priority`
  number promotes first, default 500). Explicitly: don't stage plan
  bodies in `/tmp`.
- Keep it terse and idiomatic — one bullet, matching the surrounding
  command list's style. No WHAT-comments; just the actionable how.
- Confirm `skill_shared_core.md` already names `queue/` (it does, line 6);
  no change needed there unless the stub idiom reads better in shared
  core — keep it master-only since only the master authors plans.

## Out of scope

- No code/behavior change to `queue add` itself — it already works. This
  is skill documentation only.
- Re-running `clank setup` to install the refreshed skill is a finalize-
  time step, not part of the plan body.

## Acceptance

- `skill_master.md` documents `clank queue add <name>` via the
  `.clank/stubs/<name>.md` idiom, steering agents off `/tmp` + `--from`.
- No other skill/command behavior changes; the file stays terse and
  consistent with its existing command list.
