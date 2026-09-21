# a-hook-is-known-by-what-it-runs
# A hook is known by what it runs

## Why

> "To fix this bug you've found. Queue a plan!"

On this machine, freshly restored, `~/.claude/settings.json` holds
THREE identical clank SessionStart entries, and not one clank entry in
it carries the `"id"` marker setup writes. The Stop entry kept
`statusMessage` and `asyncRewake` — fields Claude Code knows — and lost
only `id`, the one field it doesn't. `~/.codex/hooks.json`, written by
the same setup, still has both ids. Something that rewrites claude's
settings against its own schema dropped the marker. `HOOK_ID`'s doc
(`setup.rs:196`) rests on "neither claude nor codex's hook schema
rejects unknown fields" — true, and beside the point: not rejecting a
field is not preserving it.

What that does, observed here:

- `clank doctor` says "no clank SessionStart hook in claude config …
  run `clank setup`" — with three installed.
- `clank setup --dry-run` says `add … (claude SessionStart hook)`. The
  advice doctor gives appends a FOURTH. The loop feeds itself: every
  setup the warning prompts adds one more entry, and every entry runs
  `clank stop-hook --session-start` on every claude session start.
- doctor's `claude loop mode` line is gone entirely.
  `check_claude_loop_mode` (`doctor.rs:446`) finds the Stop entry by
  id, finds nothing, and returns `None` — the drift check it exists
  for silently never runs.
- The Stop entry is still recognised only because of the "legacy"
  command-prefix fallback, documented as "drop once nobody has
  un-upgraded configs". Dropping it as documented would make the Stop
  hook pile up exactly the way SessionStart has.

## The model

**A clank hook entry is known by what it runs.** The `command` is the
one field no host tool can drop — it IS the hook. Every entry clank
has ever written runs `clank stop-hook …`: all four kinds (claude
Stop in both loop modes, codex Stop, both SessionStart companions), in
every version of `setup.rs`. So "this entry's command invokes
`clank stop-hook`" is exact, needs no marker, and survives any
schema-aware rewrite. Which KIND of entry it is is already given by
where it sits: the event array (`Stop` / `SessionStart`) that setup
merges into.

The `id` came in with D8 (6dee0cd) so that renames, absolute-path
wrappers and env shims wouldn't strand entries. The command test must
therefore accept an absolute path to the binary
(`/…/bin/clank stop-hook …`): the program's basename is `clank` and
its first argument is `stop-hook`. If the subcommand is ever renamed,
the predicate recognises both names — that is the cost of keying on
the command, and it is paid in one place.

Today "is this entry clank's?" is answered four times, four ways:

| site | test |
|---|---|
| `setup::wrapper_is_clank_id` (`setup.rs:1037`) | id; command prefix for Stop only |
| `doctor::check_hook_entry` (`doctor.rs:787`) | id or prefix, an inline copy |
| `doctor::check_session_start` (`doctor.rs:840`) | id only |
| `doctor::check_claude_loop_mode` (`doctor.rs:446`) | id only, silently `None` |

That is how this bug got in: SessionStart was added (7cdf1f0) through
two of the copies and got the id-only answer in both. There will be
ONE predicate, owned by setup and used by every site. Setup stops
writing `id`: once nothing reads it, it is a second source of truth
that one host preserves and the other drops, and it invites the next
reader to key on it again.

**What clank owns is the inner entry, not the wrapper around it.** An
event array holds wrappers (`{"matcher": …, "hooks": [ … ]}`), and
one wrapper can hold a clank entry beside a user's. Today's merge
removes a whole wrapper when ANY inner entry in it is owned
(`stop.retain(|wrapper| !wrapper_is_clank_id(…))`). That was already
wrong for Stop through the legacy prefix fallback, and recognising
id-less SessionStart entries would extend it:
`{"hooks": [{"command": "clank stop-hook --tool claude --session-start"},
{"command": "echo user-startup"}]}` would lose `echo user-startup`
(codex on bb8ee37). Removal is per inner entry: the wrapper keeps
its other entries, its other fields and its place in the array, and
is dropped only when taking clank's entries out leaves it with none.

Duplicates become a state the model can name: more than one owned
entry under one event means that hook runs that many times per event.
Once removal takes out every owned inner entry before the fresh one
is added, the three collapse to one with no special case. doctor
counts them and says so.

## Deliverables

1. **One ownership predicate** in `setup.rs`, `pub(crate)`, over a
   single inner hook entry: its `command`, split into words, has a
   program whose basename is `clank` and a first argument
   `stop-hook`. Words, not `starts_with`: an absolute path matches,
   and `clank stop-hooks`, `notclank stop-hook` and
   `echo clank stop-hook` do not.
2. **Setup owns entries by that predicate for every kind.**
   `HOOK_ID`, `SESSION_START_HOOK_ID`, `LEGACY_COMMAND_PREFIX`,
   `wrapper_is_clank_id` and `HookKind::id` go, and written entries
   no longer carry `id`. Entries that still carry one (codex's today)
   are owned because of their command and are replaced like any
   other. Removal works on inner entries: every owned entry is taken
   out of its wrapper; a wrapper keeps its remaining entries, its
   other fields (`matcher`, …) and its position; a wrapper is removed
   only when that removal empties it. Clank's fresh entry is appended
   in a wrapper of its own, as today. Stop and SessionStart go through
   this one function.
3. **doctor asks the same predicate, through one lookup.** A shared
   helper returns the owned entries under one event of a parsed
   settings value. `check_hook_entry` and `check_session_start` read
   it: none → the existing "run `clank setup`" warning; more than one
   → WARN naming the count and the event ("3 clank SessionStart
   entries: each runs on every session start — `clank setup`
   collapses them"); exactly one → OK. No check walks the hook JSON
   on its own.
4. **The loop-mode check always runs when there is a Stop entry.**
   Reading the installed mode becomes a pure function of the settings
   JSON (owned Stop entry → does its command carry
   `--loop asyncrewake`), so it is testable without a `claude`
   binary; the `claude --version` probe stays at the call site. It
   returns `None` only when there is no owned Stop entry, which the
   Stop check already reports.
5. **This machine heals.** After install, `clank setup --dry-run`
   reports `merge` (not `add`) for the claude SessionStart hook;
   `clank setup` leaves exactly one clank entry per event in
   `~/.claude/settings.json` and upgrades the stale `clank-github`
   skills; `clank doctor` shows the claude SessionStart hook OK and
   prints a `claude loop mode` line.

## Tests

- **The observed shape heals.** A claude `settings.json` exactly as
  found here — an id-less Stop entry, three id-less SessionStart
  entries — plus an unrelated user hook in a wrapper of its own under
  each event. After both merges: one clank entry per event, both
  unrelated hooks untouched and in place, and the summary says
  `merge` for both events.
- **A mixed wrapper keeps what isn't clank's**, under BOTH events: a
  wrapper carrying a `matcher`, an id-less clank entry and a user's
  `echo user-startup` (for SessionStart; a user command likewise for
  Stop), placed ahead of other wrappers. After the merge the wrapper
  is still there, in the same position, with its `matcher` and only
  the user's entry; clank's single fresh entry sits in its own
  wrapper. Mutation check: go back to removing whole wrappers and the
  user's entry vanishes.
- **An old id-carrying entry is still owned** (codex's current
  file): merging replaces it; it is not kept beside the fresh one.
- **Predicate table:** `clank stop-hook --tool claude` and
  `/Users/x/.cargo/bin/clank stop-hook --tool codex --session-start`
  are owned; `clank status`, `clank stop-hooks`,
  `notclank stop-hook`, `echo clank stop-hook` and an entry with no
  `command` are not.
- **doctor, id-less SessionStart → OK.** Mutation check: put the
  id-only test back in `check_session_start` and this fails.
- **doctor, two owned SessionStart entries → WARN** whose message
  names the count.
- **Loop mode from an id-less entry:** the pure reader returns
  async for an id-less `--loop asyncrewake` Stop entry and legacy
  for `clank stop-hook --tool claude`. Mutation check: key it on
  `id` again and both fail.
- Existing tests that assert the id model
  (`merge_hook_creates_settings_with_claude_entry`'s id assertion,
  `merge_hook_replaces_entry_keyed_by_id_even_if_command_changed`,
  the codex SessionStart `ss["id"]` assertion, the
  `wrapper_is_clank_*` tests, doctor's id-keyed fixtures) change
  with the model. No test asserts that `id` is absent.

Run: `cargo test -p clank --lib cli::setup`, `cargo test -p clank
--lib cli::doctor`, then the whole `-p clank --lib` suite.

## Out of scope

- WHICH Claude Code operation strips the field. The model stops
  depending on the answer.
- Env-shim commands (`env X=1 clank stop-hook …`). Setup has never
  written one; a user who hand-wraps the hook owns that entry.
- This machine's re-resolved `Cargo.lock` and the unlocked install:
  a separate operational fix (`git checkout Cargo.lock`,
  `cargo install --path crates/cli --locked`).
