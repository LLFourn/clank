# rename-wfw-to-wait

Rename the `clank wfw` command → `clank wait` (lloyd: "wfw" is opaque; "wait"
says what it does). Bigger sweep (~176 `wfw` hits, but most are identifiers +
`finished/` history). Keep `wfw` working as an alias through the transition.

## Not a hook hazard (verified)

The installed Stop hook invokes `clank stop-hook` (HOOK_ID `clank-stop-hook`,
`Command::StopHook`), NOT `clank wfw` — so renaming the command does NOT break
installed hooks. The user-facing exposure is the SKILL ("run `clank wfw`") and
muscle memory / any external scripts. So: rename + keep a `wfw` alias + re-sync
the skill, and nothing breaks.

## Sweep (one up-front full-repo grep for `wfw`/`Wfw`/`WFW`)

User-facing command:
- The clap subcommand `wfw` → `wait`, with `#[command(visible_alias = "wfw")]`
  so `clank wfw` keeps working. Add a parse test pinning the alias.

Internal identifiers (rename for consistency — the whole point is clarity):
- `Command::Wfw` → `Wait`; `WfwArgs`/`WfwRole`/`WfwTimeout` → `Wait*`;
  module `cli::wfw` (`wfw.rs` → `wait.rs`); `cli::wfw::run`; the
  `WfwTimeout` downcast in `main.rs`'s exit-code mapping; fn/local names.
  (`mod.rs` WfwArgs/WfwRole + tests, `main.rs` dispatch, `wfw.rs` internals.)

Docs / skills:
- `setup_assets/skill_master.md` + `skill_shared_core.md`: "clank wfw" →
  "clank wait" (note `wfw` still works as an alias).
- `README.md`, `RELEASE-CHECKLIST.md`.

KEEP (intentional): `.clank/finished/*` history (incl. `clank-wfw`,
`wfw-*` plan narratives), the `visible_alias = "wfw"` + its test.

## Re-sync after install

`setup_assets/skill_*.md` are installed into `~/.{claude,codex}/skills/…`.
After `cargo install`, run `clank setup --force` so live master/reviewer
agents get "clank wait" (mirrors the commit-tag-guidance ship). The `wfw`
alias means agents reading a stale skill still work in the meantime.

## Testing (no-binary-spawning — in-process clap parse)

- `clank wait` parses to the same args struct as before.
- `clank wfw` (alias) STILL parses to it (a regression guard for the alias).
- Skill renders "clank wait".

## Acceptance

- `clank wait` is the command; `clank wfw` works as a visible alias (pinned by
  a test).
- No stray `wfw` identifiers in src outside the sanctioned alias; `finished/`
  history left intact.
- Skill + docs say "clank wait"; skills re-synced after install.
