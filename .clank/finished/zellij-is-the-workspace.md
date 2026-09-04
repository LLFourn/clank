# zellij-is-the-workspace

Cut the self-managed console. zellij is the sole workspace, every
call into it goes through one boundary, and the status TUI shows
whether zellij is reachable.

## Why cut rather than fix

The console was the alternative to zellij, and zellij works well
enough that the alternative is not worth carrying. Two ways forward
existed — make the console good, or remove it — and the second is
cheaper by a large margin: 3,200 lines whose only job was to not be
zellij. Removing it deletes a whole class of routing decisions ("am I
inside zellij, or should I be the console?") that today live at
`open.rs:25-35`.

The console's Rust coupling is small — two references outside its
module, no integration tests, no other importers — but its FOOTPRINT
is not just Rust, and an earlier draft of this plan counted only the
Rust (codex on cf93aae). The console exclusively owns:

- the `vt100` dependency in `crates/cli/Cargo.toml`;
- a VENDORED, patched copy of it under `vendor/vt100/` with a
  one-line scrollback fix, wired in by a `[patch.crates-io]` entry in
  the workspace `Cargo.toml`;
- its own PTY / signal handling paths;
- product claims in the README ("opens clank's own console") and in
  `clank open`'s clap help.

Leaving any of that behind means the console is gone but its build
and maintenance cost is not. All of it is in scope, and "the vendor
tree is deleted" is an acceptance criterion, not housekeeping.

## The boundary already exists, and is bypassed

This is the finding the plan turns on. A `PaneIo` trait ALREADY lives
at `status_tui/zellij.rs:472` — a real `ZellijPaneIo` impl, a test
fake, methods for snapshot / add / stack / relocate / focus. That is
the abstraction the user is asking for. But it covers only the
reconcile pass. Everything else bypasses it:

- `open_zellij.rs` spawns `zellij` directly at SIX sites: tab-name
  queries (×2), `list-sessions`, the two capability probes, and the
  general `zellij_action`.
- `status_tui/zellij.rs` itself spawns directly at FOUR more, outside
  the trait.
- `$ZELLIJ` is read raw in FOUR files: `open.rs`, `open_zellij.rs`,
  `status_tui/zellij.rs`, `fork.rs`.

So "put zellij behind a boundary" is not inventing one — it is taking
the partial one that exists, widening it to cover every operation, and
ENFORCING it. This is exactly the git layer's shape: `git_io.rs` +
`git_plumbing.rs` own all git access, and `tests/git_boundary.rs`
fails the build if production code outside them names `git` or `gix`.
That precedent is the template, including the test.

## Change

**1. Remove the console — all of it.** Delete `cli/console/`, its
`mod` line, the `console_default` routing, the `vt100` dependency,
the `vendor/vt100/` tree and its `[patch.crates-io]` entry, and every
README / clap-help sentence that describes a console. Bare `clank open` is the zellij opener:
inside a session it adds a tab, outside one it spawns a session with
the layout. `clank open zellij` stays as an alias so nothing invoking
it breaks, but it means the same thing as `clank open`.

**2. One zellij module owns every call.** A single `cli/zellij.rs`
(or a small directory) is the ONLY production code that spawns
`zellij` or reads `$ZELLIJ`. It exposes a typed API — the operations
the six-plus-four sites actually perform — and `PaneIo` moves into it
as the trait callers program against, with `ZellijPaneIo` as the sole
production impl.

The trait is the right shape here, not message passing: every call is
synchronous request/response against a CLI, there is no long-lived
connection to multiplex, and the existing test fake shows the
trait-with-fake pattern already pays for itself. Message passing would
add a channel and a task to model something that is a function call.
Say so in the module doc, so the next person does not reopen it.

The thing callers must NOT be able to do is spawn `zellij` themselves.
Every current bypass becomes a method.

**3. Enforce it — by extending the gates that already exist, not by
adding a weaker one.** An earlier draft proposed a new
`zellij_boundary.rs` copied from `git_boundary.rs`, with test code
exempt. That would have been a REGRESSION: two zellij gates already
exist and both are stricter.

- `tests/zellij_ownership_boundary.rs` names the owner files and
  forbids pane MUTATIONS elsewhere — and it deliberately scans
  `crates/*/tests` too, because a zellij server leaked from a test
  once congested the machine. Test code is not exempt for zellij and
  must not become so.
- `tests/zellij_cost_boundary.rs` forbids production code from
  invoking the zellij actions that shell out to `ps` per pane.

The change is to point the ownership gate's `OWNERS` at the new sole
module and widen what it forbids outside it: not only the pane
mutation helpers but ANY `Command::new("zellij")` and ANY read of
`ZELLIJ` from the environment. One gate, one owner, tests included.
The cost gate is unchanged in intent and re-pointed at the module.

This matters more than the abstraction itself. A trait that can be
walked around is documentation; the gate is what makes it a
property.

**4. A connection indicator in the status TUI.** One glyph in the
header showing whether zellij is reachable — not whether `$ZELLIJ` is
set, but whether the client actually answers. Three states, because
they mean different things to the operator:

- connected — inside a session and the client responds;
- not in a session — `$ZELLIJ` unset, so tab/pane operations are
  unavailable and `clank open` would start one;
- unreachable — `$ZELLIJ` is set but the client does not answer,
  which is the case that otherwise looks like "clank did nothing".

It reads through the boundary like everything else, and it must be
cheap: the TUI already pays for one listing per pass, so the
indicator derives from that pass rather than adding a probe per
frame. Use `Hue`, the type that makes a colour an attribute of a state
rather than a literal in the renderer.

## What "sole option" cannot mean

`clank status --tui` and every non-workspace command run fine without
zellij and must keep doing so. "Sole workspace option" means there is
one way to open the TEAM, not that clank refuses to run outside a
multiplexer. `open` without zellij installed fails with a message that
says to install it, once, and nothing else changes behaviour.

## Tests

- The extended ownership gate fails on a planted `Command::new(
  "zellij")` outside the module AND on a planted `$ZELLIJ` read —
  proven by adding each in a scratch file (a test file too, since
  tests are in scope), watching it fail, removing it. The gate is the
  deliverable; the migration is what makes it pass.
- `cargo tree` shows no `vt100`, and `vendor/` and the `[patch]` entry
  are gone. The build must not carry a dependency nothing uses.
- No README or `--help` text still describes a console.
- Every operation the six-plus-four sites performed has a method with
  a test fake, and the fake is used by at least one existing test
  that previously could not run without zellij.
- Bare `clank open` outside a session produces the zellij spawn argv
  (via `--print`), never a console.
- The indicator renders all three states from a fake, and its colour
  comes from `Hue`.
- `clank open` with no zellij binary fails with the install message
  and a non-zero exit, rather than a panic or a silent nothing.

## Out of scope

- The layout geometry and the stack-membership proof
  (`a-reviewer-pane-lands-where-it-belongs`). They move into the
  module unchanged.
- Any change to what the reconcile pass DOES. It changes where it
  calls through, not what it decides.
- opencode's plugin and the agent tools' own hooks, which are not
  zellij.
