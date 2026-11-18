# clank-remote-link-prints-a-login-link
# `clank remote link` prints a login link

## Why

An agent asked over Signal for "the access link with the token embedded"
spent a whole turn (2026-11-18, `~/src/life`'s assistent) reading clank's
source and then driving the TUI pane with zellij `send-keys` and
`dump-screen` to scrape the remote page, while the user's later Signal
messages queued behind it. The link exists only on the TUI's remote page:
`clank remote` can switch the remote and report its address, but cannot
produce a link anyone can log in with.

The user decided (2026-11-18) that the command prints the link to stdout
wherever it runs, agents included. That deliberately reverses the earlier
rule that login links never reach agent context; what stays true is that
they never reach a file anyone else can read, a published status, or a
log.

## What

`clank remote link [--repo P] [--timeout S]` asks the repo's TUI owner for
a fresh login link and prints exactly that link on stdout, one line,
nothing else on stdout. It is the same link the remote page shows:
`Remote::link` (status_tui/remote.rs), so a host this TUI does not
own still yields its base URL, as on the page.

- Remote off, starting, failed or unconfigured: fail with what
  `clank remote status` would say and, when off, `run clank remote on
  first`. It does not switch the remote on.
- No TUI owner, or control not ready: the same answers `clank remote
  on` gives today (`Readiness`).
- An owner that does not speak the new protocol: say it must be
  restarted, rather than timing out.

## How

The file protocol in `remote_control.rs` carries it; no socket, port or
endpoint is added.

1. **Request.** A request says what it wants: switch (`Want::On` /
   `Want::Off`) or a link. Model it so a link request cannot be decided
   by the on/off transition table: `decide` stays about switching, and
   the link path is its own branch in `RemoteOwner::request` that
   answers at once from the current state (no `in_flight`). Bump
   `PROTOCOL`: `readiness` already answers `Unsupported` for an owner
   advertising another protocol, so an old TUI is told to restart, not
   waited on.
2. **Answer.** A new `Outcome::Link { link }` written to the request's
   own result file. That file is the only place the link rests:
   - mode 0600 from creation (`write_atomic`'s `NamedTempFile` already
     is; a test pins it), in the runtime directory that is already
     per-uid;
   - the CLI reads it and unlinks it before printing; the owner's
     `sweep` removes any a crashed CLI left behind (existing `KEEP`);
   - `Status`, `control_reason`, `error` and every `safe_reason` path
     stay link-free; the published `Status` doc comment keeps saying
     so.
3. **Minting** happens on the owner via `Remote::link`, which is
   async; the owner's request handling must call it without blocking
   the TUI loop (follow how the remote page already obtains it).
4. **CLI.** `RemoteCmd::Link` in `cli/mod.rs`; `run` prints
   `Outcome::Link`'s link and exits 0; every other outcome bails as
   today. No logging of the request result anywhere.
5. **Docs.** README's remote section (≈lines 520–560) and the command
   table row for `remote` (`status` / `on` / `off` / `link`): say the
   link is one-time, printed to stdout, and that whoever reads that
   output can log in.

## Tests (in-process, no binary spawning)

- `RemoteOwner::request` with a fake `Switch`/link source: a link
  request while on answers `Outcome::Link` with the fake's link; while
  off answers a failure naming `clank remote on`; it never touches
  `in_flight` and never starts or stops the remote.
- The result file holding a link is mode 0600 at creation, and the CLI
  read path removes it (assert it is gone after the answer is taken).
- `Status` published after a link request contains no part of the
  link (serialize and search for the token).
- An owner advertising the old protocol gets a "restart the TUI"
  answer from the CLI path, not a timeout.
- `cli::command` README/CLI tests cover the new subcommand.

## Out of scope

- Delivering the link anywhere but stdout (Signal replies, clipboard).
- Switching the remote on implicitly.
