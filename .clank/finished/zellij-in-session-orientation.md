# zellij-in-session-orientation
# In-session `clank open zellij` detects orientation from the real window

## Bug (lloyd, live)

`clank open zellij` run INSIDE a zellij session composes a landscape
layout even when the terminal window is portrait.

Root cause, verified in code: both compose sites (open_zellij.rs:204
multitab, :346 single-tab) call `term::term_size()` — a `TIOCGWINSZ`
ioctl on OUR stdout. Inside zellij, stdout is the invoking PANE's PTY,
so orientation is decided by the pane's aspect, not the window's. Any
wide pane (a shell split under the agents: 120×15 → cols ≥ 2·rows)
reads landscape regardless of the window. Outside zellij the ioctl hits
the real terminal and detection is correct.

## Mechanism (verified live on this machine)

Zellij's CLI cannot report window size (`list-panes`/`list-tabs` carry
no geometry). But the attached zellij CLIENT process has the REAL
terminal as its controlling tty, discoverable and measurable without
zellij's help:

- `ps -axo tty=,command=` lists clients WITH their tty, and the argv
  names the session: `zellij attach clank-dark_skippy` on `ttys004`,
  `zellij --session clank-fsctl --new-session-with-layout …` on
  `ttys013` (both shapes observed live).
- `open("/dev/ttys004", O_RDONLY|O_NOCTTY)` + `TIOCGWINSZ` returned the
  true window (238×75) — same-uid tty reads work on macOS; Linux is
  `pts/N` → `/dev/pts/N`.

## Fix

In-session window-size resolution, used by BOTH compose sites:

1. If `$ZELLIJ` is unset → today's ioctl (unchanged).
2. Else find the client for `$ZELLIJ_SESSION_NAME` (zellij sets it in
   panes): a `zellij` process whose argv contains the session name via
   `attach <name>`, `--session <name>`, or `-s <name>`.
3. Fallbacks, in order: exactly ONE zellij client on any tty → use it;
   otherwise → today's pane ioctl (never worse than current behavior).
4. `TIOCGWINSZ` on the client's `/dev/<tty>`; ioctl failure → pane
   fallback.

Shape: a pure decision function over parsed `(tty, argv)` lines picks
the client (unit-tested: named match beats sole-client; multiple
unnamed clients → None; `attach` and `--session`/`-s` argv shapes;
Linux `pts/N` mapping), with the `ps` read and the ioctl at the edge in
`term.rs` next to `term_size`. This is a process/tty read, not git —
the git-layer boundary doesn't apply, but keep the subprocess isolated
in one function with a "why" note.

Multi-client caveat (documented in code): two attached clients on
different terminals can disagree; the named match takes the first —
arbitrary but deterministic, and strictly better than measuring a pane.

## Non-goals

- No change to Orientation::detect's 2:1 threshold or the swap-variant
  shipping (alt+[ / alt+] already flips a wrong guess manually).
- No zellij plugin, no escape-sequence probing (zellij answers CSI
  size queries with PANE dimensions — same trap).

## Tests

Pure tests for the client-picker + tty-path mapping; a term.rs unit
test that the in-session resolver is bypassed when `$ZELLIJ` is unset
is NOT needed (branching is trivial and env-dependent) — instead the
decision fn is the tested surface. No binary spawning; no zellij
servers in tests.

## Acceptance

From a wide split pane inside a portrait-window zellij session,
`clank open zellij --print` composes the portrait agent group; outside
zellij behavior is byte-identical; clippy/fmt/suites green.
