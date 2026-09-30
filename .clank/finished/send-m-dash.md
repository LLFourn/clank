# send-m-dash
# clank — `clank send -m -` reads stdin, as `-F -` does

## Rules

- Commit in this repo. Do not push or post anywhere.
- No `claude.ai/code` link and no `Claude-Session:` trailer in any commit or message.
- Comments explain WHY, never WHAT.
- Report to `clank send --repo ~/src/clank-wr --to claude -F <file>`. Begin it with a line telling
  the reader to read the whole message.

## Why

clankstack's CLI reads stdin for `-m -`. Agents that use both tools now type `clank send -m -` with a
heredoc, and clank sends a literal "-". This has happened twice today: two messages from the
fs-war-room controller arrived as just "-", and their content was lost until re-sent.

## Change

- `clank send -m -` reads the message from stdin, verbatim, exactly as `-F -` does. A lone `-` is
  never a useful message, so nothing is lost by the change.
- The same for any other clank command whose `-m` takes free text, if any, so that `-m -` means stdin
  across the CLI.

## The survey the change asks for

"If any" is **all seven**. Every `-m` in the CLI takes free text (`crates/cli/src/cli/mod.rs`):

| command | line | type | has `-F`? |
|---|---|---|---|
| `send -m` | 697 | `Vec<String>` | yes |
| `feedback write -m` | 813 | `String` | no |
| `pr-review -m` | 903 | `String` | no |
| `queue add -m` | 1414 | `Option<String>` | no |
| `finish -m` | 1448 | `Vec<String>` | no |
| `block -m` | 1674 | `String` | no |
| `unblock -m` | 1772 | `String` | no |

**Only `send` has an `-F` today**, so for the other six `-m -` would be the only way to pass
a body that is not shell-safe. That matters most for `feedback write -m`, which carries a
whole review — backticks, `$(…)` and code quotes are ordinary content there — and for
`finish -m`, whose body becomes a squash message. The reported loss is `send`; the
exposure is wider.

## Decisions this needs, not defaults

- **Repeated `-m` with one `-`** (`send`, `finish`): DEFINED, not refused. The `-` is read
  once and the text takes that paragraph's position, so
  `-m "<subject>" -m - -m "<tail>"` composes in order. A second `-m -` in one invocation
  is an error that says stdin can be read only once. Mixing is the useful case — a
  hand-written subject with a heredoc body — and refusing it would have left `finish` with
  no way to use this at all.
- **Empty stdin** is an empty message and takes whatever rejection an empty `-m` already
  takes, rather than sending a blank.
- **`queue add`** keeps its drafts-dir fallback: `-m` omitted still reads
  `.clank/drafts/<name>.md`, and `-m -` is stdin. No conflict, but the help must not imply
  one.
- **`send -m - -F -`** is already impossible — clap's `body` ArgGroup makes `message` and
  `file` mutually exclusive — and a test should pin that rather than leave it incidental.
- A message that is genuinely the one character `-` becomes unsendable. The change says
  nothing is lost by that; it should be written down as a deliberate trade, since it is
  the only behaviour removed.

## Decisions taken

- **One pass before dispatch, not a call in seven `run`s.** The seven disagree about
  emptiness — `block create` writes the text with no check, `feedback write` accepts an
  empty body as a verdict-only note, `send` / `queue add` / `finish` each reject empty
  their own way — so the pass SUBSTITUTES and nothing else, and every command keeps the
  behaviour it already had for the text it is handed. That answers the empty-stdin
  question per command without writing a new policy anywhere.
- **The wiring is tested per command**, not only at the helper: each of the seven is
  parsed from argv and resolved against a canned stdin.
- **The regression is tested end to end, in process.** `dispatch` takes its stdin reader
  as an argument (`dispatch_with`), so a test drives the real path —
  `dispatch_with` → `send::run` → `inbox::deliver` — with a heredoc-shaped body and reads
  the DELIVERED row back byte for byte. That row is the only copy: *"`clank send` no
  longer leaves a file anywhere, so this record is the only copy: a body that is not here
  was not sent."* Deleting the resolve from `dispatch_with` fails this test, so it also
  replaces the earlier source-grep guard, which is gone.

  A SUBPROCESS test was requested and is not used: spawning the clank binary in tests is
  banned in this repo after a leaked zellij server congested the machine. The cited
  precedent (`tests/it/web_host_process.rs`) spawns `clank-web-cloudflare`, a web host
  that drives no panes — it is not a precedent for spawning `clank`. Injecting the reader
  gets the same coverage without the ban.
- **An eighth `-m` is caught.** `field_of` needs a wildcard arm, so a new `-m` would
  silently keep sending a literal `-`. A test counts the `short = 'm'` declarations and
  fails when the count moves, naming what to wire.
- **Skills are NOT updated** (`skill_shared_core.md`, `skill_reviewer.md`,
  `pr_review_skill.md` all show `-m "…"`). Changing embedded skill assets puts every
  machine's installed copy out of date until `clank setup --force` runs, and the change
  does not ask for it. Worth doing as a follow-up, since agents are the users — flagged
  rather than done.
- Help text says so.

## Tests

- `-m -` fed by a quoted heredoc round-trips text containing backticks, `$(…)` and quotes byte for
  byte.
- Repeated `-m` paragraphs still work, and `-m -` combined with other `-m`s is refused with a clear
  message, or defined. Say which.
- The usual lint and tests pass.

## Done when

- Report as above.
