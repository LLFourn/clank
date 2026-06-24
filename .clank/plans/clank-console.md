# clank-console
# clank console — a self-managed agent multiplexer (MVP)

## Goal

A single command, `clank console`, that takes over the terminal for one
project and runs the whole team inside it: one screen per agent, plus a
screen that is a full `clank status --tui`. You switch between screens
with a hotkey. No zellij, no KDL, no external multiplexer — clank owns
the windows so it can decorate and control them better than zellij can.

This is the easy entrypoint for new users: `clank open` with no live
zellij session can launch the console instead of generating a layout.

## The core model (read this first)

**clank console is a thin VT multiplexer layered over the *unchanged*
agent-start machinery.** That sentence is the whole architecture, and
every decision below follows from keeping it true.

Today zellij runs `clank agent start <label> --repo <repo>` in each
pane; that process `exec()`s claude/codex and inherits the pane's PTY
(see `open_zellij.rs` layout composition + `agent.rs::start`). The
console changes *who allocates the PTY and draws the pane* — nothing
else. It does **not** know about tools, launch configs, bootstrap vs
fork vs resume, or session binding. It runs the same command zellij
runs, in a PTY it owns, and paints the result.

A **Screen** is therefore just: a child process in a PTY + a terminal
emulator grid that mirrors what that child has drawn. The console holds
an ordered list of Screens and an index of the active one. The screens are:

1. master → `clank agent start <master-label> --repo <repo>`
2. each commit reviewer → `clank agent start <label> --repo <repo>`
3. each gate reviewer → `clank agent start <label> --repo <repo>`
4. status → `clank status --tui --repo <repo>`

The screen list is derived from the same `RegisteredSet`
(`teams_config.rs`: master + commit_reviewers + gate_reviewers) that
`open_zellij` uses to compose its layout. There is **one source of
truth for "who is on the team," and it is the roster** — the console
reads it, it does not maintain its own copy.

Why this framing matters: it means the console has *no* agent-lifecycle
logic to get wrong. Bootstrap, fork, resume, identity binding, auto-mode,
the Stop hook — all of it already works and runs untouched inside each
PTY. The console is a generic multiplexer that happens to spawn clank
commands. If the console crashes, every agent's session is still
recoverable, because `clank agent start` resumes from on-disk session
state next launch (this is the existing resume path — see
`open.rs::agent_session_info` / `session_jsonl_exists`). That property
is what makes "no detach/reattach in the MVP" an acceptable limitation
rather than a data-loss bug.

### Invariants

1. **Every PTY is always drained**, foreground and background alike. A
   child whose PTY read buffer fills (~64 KB) blocks on write and hangs.
   A reader per child guarantees this and, as a side effect, keeps every
   grid current even while off-screen.
2. **The active grid is the single source of truth for the visible
   content region.** Switching screens is "change the active index and
   repaint from that grid" — never a replay of raw bytes, never a
   force-repaint hack. This is the entire payoff of using a real
   emulator (below) and it is why switching is instant and correct.
3. **Window size is propagated to every child on resize.** The content
   rectangle = terminal minus the console's chrome row; every child's
   PTY winsize *and* its emulator grid are set to that rectangle. A
   child never draws under the chrome and never holds stale geometry.
   (Contrast: the recent `relocate-orientation-from-geometry` work
   existed only because zellij owned the winsize and clank had to infer
   it. Here clank owns it outright — the whole class of bug evaporates.)
4. **Input is forwarded verbatim to the active child except the prefix
   key.** Raw stdin bytes (arrows, paste, mouse, UTF-8) pass through 1:1
   so claude/codex behave exactly as they do under zellij. The only
   bytes the console intercepts are the prefix and its immediate
   successor.

## The central technical question: dependencies

The user's stated top interest: *can this be done without dependencies,
and is ratatui called for?* Here is the honest answer.

### What a multiplexer actually requires

Building this is building a (small) tmux/zellij. The work splits into
four parts; three are easy, one is not:

| Part | Difficulty | Dep-free? |
|------|-----------|-----------|
| Allocate a PTY per child | easy | **yes** — `libc` (already a dep) |
| Multiplex stdin / route the prefix key | easy | **yes** — hand-rolled |
| Draw chrome (tab/status bar) | easy | **yes** — reuse `status_tui` primitives |
| **Reconstruct a background screen on switch** | **hard** | **no, not correctly** |

The hard part is terminal emulation: to repaint an agent after you
switch back to it, you must know what is *currently on its screen* —
which means parsing the child's ANSI/VT output stream and maintaining a
grid of cells (with attributes, cursor position, scroll region,
alt-screen state, modes). This is exactly what tmux and zellij do
internally.

Without an emulator you have only two dep-free options, both inadequate
for our children specifically (claude and codex are full-screen
alt-screen TUIs):

- **Raw byte-replay**: buffer each background child's bytes, replay the
  buffer on switch. ANSI streams are not idempotent on replay —
  absolute cursor moves, alt-screen enter/leave, partial sequences split
  across reads, and scroll-region resets all corrupt the redraw. For
  alt-screen apps it visibly breaks.
- **Force-repaint via SIGWINCH**: poke the child to redraw on switch.
  Lossy (drops anything printed while backgrounded), racy, and many apps
  don't fully repaint. tmux/zellij deliberately do *not* rely on this.

So "dependency-free" is *technically* possible and *practically* wrong.
The dep-free path's only way to be correct is to **write your own VT100
emulator** — a 2,000–5,000-line state machine that is a notorious
correctness minefield. That is strictly worse than depending on the
mature, tiny, pure-Rust parser the whole ecosystem already uses.

### Recommendation

- **Add one dependency: `vt100`** (which pulls `vte`, Alacritty's VT
  parser). `vte` is the de-facto Rust VT state machine; `vt100` layers a
  screen grid on top and gives you `screen.contents_formatted()` →
  the ANSI to paint the current screen, plus cursor position/visibility.
  Both are pure Rust, minimal transitive deps, widely used. This single
  dep turns invariant #2 from "impossible" into "three lines."
- **PTY via `libc` — no new dependency.** `openpty()` + `std::process::
  Command` with a `pre_exec` hook (`setsid`, make the slave the
  controlling tty, `dup2` slave→0/1/2, close the master). ~100 lines,
  well-trodden, and consistent with how `status_tui.rs` already uses
  `libc` directly for termios/ioctl/signals rather than `nix`.
  - *Documented fallback:* if the `openpty` plumbing proves fiddly under
    the tokio runtime, `portable-pty` (wezterm) or `pty-process` (same
    author as `vt100`) are drop-in replacements. Recommend starting with
    `libc` to stay lean; switch only if it fights us.
- **Do NOT use ratatui.** Ratatui draws TUI *chrome* (layout/widgets) —
  it does not do PTY management or VT emulation, i.e. it solves none of
  the hard part. Adopting it would add a *second* rendering paradigm
  competing with the hand-rolled span/`emit`/`display_width` layer in
  `status_tui.rs`, and pull in crossterm (a terminal backend the
  codebase has deliberately avoided). Our chrome is one bar plus a
  single full-screen child viewport; the existing primitives draw it in
  a few lines. Reuse them.

**Net: one new dependency (`vt100`+`vte`), zero for everything else, no
ratatui.** That is the right shape: pay a dependency exactly where
hand-rolling is a multi-thousand-line liability, hand-roll everything
the codebase already hand-rolls well.

## Component design

All new code lives in `crates/cli/src/cli/console/` (a module dir, since
this is bigger than one file): `mod.rs` (command entry + event loop),
`pty.rs` (PTY allocation + child spawn), `screen.rs` (Screen = child +
emulator), `mux.rs` (the pure state machine: active index, prefix
routing, layout math), `render.rs` (chrome + content compositing).

### PTY allocation (`pty.rs`)

```
fn open_pty(rows, cols) -> (master_fd, slave_fd)        // libc::openpty + TIOCSWINSZ
fn spawn_in_pty(cmd, args, cwd, env, slave_fd) -> Child  // Command + pre_exec
fn set_winsize(master_fd, rows, cols)                    // TIOCSWINSZ on resize
```

`pre_exec`: `setsid()`, `ioctl(TIOCSCTTY)`, `dup2` slave→{0,1,2}, close
master and the now-duplicated slave. Parent keeps the master fd, sets it
non-blocking-or-threaded for reads.

### Screen (`screen.rs`)

```
struct Screen {
    label: String,           // "claude (master)", "codex (reviewer)", "status"
    master_fd: RawFd,        // PTY master we read/write
    child: Child,            // the clank subprocess
    parser: vt100::Parser,   // grid sized to the content rectangle
}
```

One reader thread per Screen: `read(master_fd)` → `parser.process(&buf)`
→ send a coalesced `Ev::Output(idx)` to the main channel. (Coalesce so a
chatty background child can't flood the loop; the loop only repaints if
the *active* screen produced output.) Writes go the other way: console
forwards stdin bytes via `write(master_fd, ...)`.

### Event loop (`mod.rs`)

Mirror `status_tui.rs` exactly: `async fn run`, a sync core, dedicated
threads feeding one `mpsc` channel, an `AltScreen` RAII guard for
raw-mode/alt-screen/cursor + panic + SIGINT/SIGTERM restoration (lift it
near-verbatim — factor the shared bits out of `status_tui.rs` rather
than fork them).

```
enum Ev {
    Output(usize),   // a screen's grid changed (idx)
    Stdin(Vec<u8>),  // raw bytes from the real terminal
    Resize,          // SIGWINCH
    ChildExit(usize),
}
```

Loop: block on `recv` → handle event → if the active screen's grid (or
chrome) changed, repaint. No animation timeout needed unless we add our
own spinner; child spinners animate via their own `Ev::Output`.

### The mux state machine (`mux.rs`) — pure, fully testable

This is the architectural heart and follows the `status_tui` Mode
discipline: **one enum owns input routing; each byte is resolved in
exactly one place per mode; pure functions return actions the loop
performs.**

```
enum Mode { Passthrough, Prefix }   // Copy

enum Action {
    Forward(Vec<u8>),     // write bytes to active child's PTY
    SwitchTo(usize),      // change active screen
    Next, Prev,           // cycle
    Quit,                 // confirm-then-teardown
    Redraw,               // chrome-only change
    None,
}

fn route(mode, byte, screen_count) -> (Mode, Action)
```

- `Passthrough` + prefix byte → `(Prefix, None)`.
- `Passthrough` + anything else → `(Passthrough, Forward(byte))`.
- `Prefix` + command key → `(Passthrough, <action>)`:
  - digit `1..9` → `SwitchTo(n-1)`
  - `n` / `Tab` → `Next`; `p` → `Prev`
  - `q` → `Quit`
  - prefix again → `(Passthrough, Forward(prefix))` (send a literal
    prefix to the child — tmux convention)
  - anything else → `(Passthrough, None)` (swallow unknown command)

Layout math is pure too:

```
fn content_rect(rows, cols) -> (rows-1, cols)   // chrome takes 1 row
fn chrome_line(screens, active, cols) -> String // the tab/status bar
```

Both get unit tests with zero IO, exactly like `agent_panel_action` /
`scroll_to_show` today.

### Keybinding: the prefix

Multiplexing a terminal whose children grab almost every key requires a
reserved **prefix key** (tmux `Ctrl-b`, zellij modal). A single rare
chord is the legible MVP choice.

- **Default prefix: `Ctrl-a`** (one byte, `0x01`), configurable via
  `~/.clank/config.json#/console/prefix`. Then a one-key command:
  `Ctrl-a` then `n`/`p`/digit/`q`. `Ctrl-a Ctrl-a` sends a literal
  `Ctrl-a` to the child.
- Rationale + tradeoff to settle in review: `Ctrl-a` is readline
  "start of line"; `Ctrl-b` collides with nested tmux. We pick `Ctrl-a`
  as default *and* make it config so anyone affected rebinds in one
  line. (Open the question to reviewers; don't over-anchor.)

### Rendering (`render.rs`)

- Content region: `paint` the active screen's
  `parser.screen().contents_formatted()` into rows `0..rows-1`, then
  place the real cursor at the active grid's cursor (respecting its
  hide/show) so the agent's input caret is correct.
- Chrome (last row): a tab strip built from existing `status_tui`
  primitives — `emit` / `emit_selected` (active screen reverse-video
  band), `display_width` / `truncate_to`, `region_rule` styling. Show
  each screen's label and a liveness/role mark; reuse the role emoji
  vocabulary already parsed from pane titles.

## `clank console` command + `clank open` integration

- New top-level `Console(ConsoleArgs { repo: Option<PathBuf> })` variant
  in `cli/mod.rs`, dispatched from `main.rs`, resolving the repo the same
  way `open_zellij` does.
- `clank open` integration (kept conservative for MVP): add a
  `console` toggle to config; when set, `open.rs` routes bare
  `clank open` (outside any live zellij session) to the console instead
  of `open_zellij`. Default **off** until the console is proven, so we
  never silently replace the working zellij path. The draft's "open the
  console when there's no zellij session" becomes a flip of this flag.

## Explicitly OUT of the MVP (and why it's safe to defer)

- **Detach / reattach / persistence.** Console is foreground; quitting
  stops the children. Safe because agent sessions are resumable from
  disk — relaunching re-attaches via the existing resume path. (This is
  the big zellij feature we consciously drop for v1.)
- **Status overlay in the top-right corner.** The draft's stretch goal.
  Deferred to "status is its own full screen" (explicitly the draft's
  MVP ask). Note: `vt100` grids make the overlay a natural follow-up —
  compositing is "copy the status grid's cells into a sub-rectangle of
  the composed frame," not a new mechanism.
- **Multiple tabs / worktrees** (zellij `--all`). One project per
  console for v1.
- **Landscape/portrait swap, split views.** The console shows one screen
  at a time; splits are post-MVP layout work.
- **Mouse-driven pane selection, scrollback search, copy mode.** Raw
  mouse bytes still forward to the active child; console-level mouse is
  later.

## Testing strategy (respects "no binary-spawning tests")

The clank-binary-spawn ban exists because leaked zellij servers
congested the machine. The console spawns *no zellij* and we must not
spawn the *clank binary* in tests. So:

- **Pure-function tests (the bulk):** `route()` (every mode×byte
  transition, prefix-escape, unknown-swallow), `content_rect`,
  `chrome_line` (active band, truncation, width), screen-list derivation
  from a fixture `RegisteredSet`. All zero-IO, like the existing
  `status_tui` router tests.
- **PTY/emulator integration tests:** spawn a *trivial, non-clank*
  child in a PTY — `cat`, `printf`, or `sh -c 'printf ...'` — feed/read
  bytes, assert the `vt100` grid reflects what was written and that
  resize re-propagates. This exercises `pty.rs` + `screen.rs` wiring
  without spawning `clank` or zellij. (Spawning `cat` in a test is fine;
  the ban is specifically the clank binary.)
- **No test launches `clank console` or `clank agent start`.** The
  agent-start path is already covered by its own in-process tests; the
  console's contract with it is "run this argv in a PTY," which the
  trivial-child tests cover structurally.

## Milestones

1. **M1 — multiplexer skeleton.** `pty.rs` + `screen.rs` + `mux.rs`
   pure core + event loop. Hardcode two screens running `cat`/a shell;
   prove switching, input forwarding, resize, clean teardown, and the
   `vt100` repaint-on-switch invariant. All pure tests + PTY harness
   tests green.
2. **M2 — roster wiring.** Derive the real screen list from
   `RegisteredSet`; each screen runs `clank agent start <label>`. Add the
   status screen (`clank status --tui`). `clank console` command lands.
3. **M3 — chrome + polish.** Tab strip with role marks/liveness, active
   band, cursor sync, prefix config, quit confirmation, child-exit
   handling (respawn vs mark-dead).
4. **M4 — `clank open` integration** behind the default-off `console`
   config toggle.

## Risks / things to verify during M1

- `pre_exec` controlling-tty setup correctness across macOS/Linux (the
  one genuinely fiddly bit; the trivial-child PTY test is the guard).
- `vt100` fidelity for claude/codex specifically (mouse modes, bracketed
  paste, true-color) — validate by hand against a real agent early in M2;
  `vt100` is mature but confirm before committing the UX.
- Reader-thread coalescing under a chatty background agent (don't repaint
  on background output; don't starve the loop).
- Lifting `AltScreen`/termios/signal handling out of `status_tui.rs`
  without regressing the status TUI — factor, don't fork.
