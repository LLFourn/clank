# clank-web-shows-each-agent-on-a-phone

> What I want is our own webpage where we could see these sessions,
> with a `clank status --tui` functionality adapted for the web. The
> geometry of `zellij web` has to match the desktop, so it would be
> good if the web interface let you flick through different tabs,
> each with one agent, on your phone. In the clank web we track and
> subscribe to each pane separately and don't subscribe to the status
> pane.
>
> First the PoC: local webserver that displays the agents in a nice
> interface. Engage your frontend design skill. No security necessary
> yet. — lloyd

## Why not `zellij web`

It works — lloyd typed into this session from it — but a shared
session has ONE geometry, sized to its smallest client. A phone either
gets the desktop's 200-column tab or shrinks the desktop to phone
width. `zellij subscribe` sidesteps the layout entirely: each pane's
viewport arrives on its own, so a phone can show one agent at a time.

The honest limit, stated once: terminals do not reflow. A subscribed
pane arrives at its desktop size, and the phone shows it in a scroll
and pinch container. That is one agent full-screen rather than a
reflowed chat — and far better than the whole tab.

## What subscribe actually gives us (measured on 0.45.0)

    zellij --session S subscribe --format json --ansi -p terminal_5

JSON lines. Every event is a FULL viewport — 41 events in 4s from one
busy pane, ~28KB each with ANSI, every one 135 lines (the pane's
height). The type is `ServerToClientMsg::PaneRenderUpdate { pane_id,
viewport: Vec<String>, scrollback: Option<Vec<String>>, is_initial }`
plus `SubscribedPaneClosed { pane_id }`, mapped by the client to
`{"event":"pane_update", …}`. No cursor, no columns: the width comes
from `list-panes --json` (`pane_columns`, which clank already reads).

So the "use the zellij crate as a library" question answers itself:
the wire is five plain JSON fields with ANSI strings inside, and
`zellij-utils` is not on crates.io (only the binary is). A serde
struct of five fields IS error-free decoding; a git dependency on the
whole workspace would buy nothing but weight.

Input is `zellij action write-chars -p terminal_N "text"` then
`write -p terminal_N 13` (Enter), both pane-targeted and both
`--session`-prefixable from outside.

## The design

The subject is a person glancing at several agents working in
terminals, from a phone or a second screen, then reading one, then
telling it one thing. The panes are dense monospace and are the
content; the page around them is a bezel.

**The one bold thing: channel strips.** Several agents working at
once is a patch bay, and each agent gets a strip that IS its tab —
the name set big and heavy, its role under it, and its live verb with
how long (`reviewing 20m`), straight from the TUI's own derivation.
On a phone the strips are a horizontal row that scrolls; tapping one
brings its pane up. On a desktop the same strips head a row of panes
side by side. Same DOM; CSS reflows.

    ┌────────────────────────────────┐
    │ clank-clank                    │  session, quiet
    ├───────┬────────┬───────┬───────┤
    │claude │codex   │ruthle…│       │  strips: name, role,
    │master │commit  │final  │       │  verb + elapsed; the
    │●work… │        │       │       │  dot breathes only
    ├───────┴────────┴───────┴───────┤  while working
    │                                │
    │   [xterm pane at desktop size  │  scroll x/y, pinch
    │    inside a scroll container]  │
    │                                │
    ├────────────────────────────────┤
    │ 🔨 CLAUDE working  a-tabs-sha… │  the TUI's own lamp text
    ├────────────────────────────────┤
    │ Say something to claude      ↵ │  sticky send box
    └────────────────────────────────┘

- **Color.** Warm charcoal, not tinted black: `#1A1917` base,
  `#242220` bezel, `#E8E4DC` ink, `#8A857C` muted. ONE accent with
  one meaning — muted amber `#E0A458` (phosphor, not acid) for "this
  agent is working right now" and the send action. Every status hue
  — idle, active, blocked, correction, the ✓ ✓✓ ✗ marks — is the
  TUI's `Hue` table mapped to hex, so a blocked plan is the same red
  on both surfaces.
- **Type.** Two monospace families, clearly distinct: Martian Mono
  (wide, heavy, 700) for the agent names on the strips — stencilled
  equipment labels — and the terminal's own face (`ui-monospace,
  Menlo`) for everything else including the panes, so chrome text
  matches what the panes render in. Names stay lowercase: they are
  clank's labels, and the TUI shows them lowercase.
- **Layout.** Left-aligned throughout; the panes are left-aligned
  grids and the chrome follows them. Strips are flat, separated by
  the bezel, no cards, no shadows.
- **Principles.** The pane is the content and nothing in the chrome
  is brighter than the pane's own text. One accent, one meaning.
  Status vocabulary is the TUI's — same verbs, same hues, same
  `age::coarse` elapsed, same lamp line — so someone who knows the
  TUI reads this without learning anything. Motion only where
  something is happening: the working dot breathes, nothing else
  moves. Copy from the user's side: "Say something to claude",
  "Sent", "claude's pane closed".

**Reviewed against the generic defaults, and what changed:** the
first draft had `plan: … · gate ✓` as a dotted meta string under the
pane — replaced by the TUI's `bar_text` lamp line, which is specific
to this client and already exists. Agent names were set in capitals —
lowercased, because they are identifiers clank shows lowercase. Dark
plus one bright accent is a known default; kept because the brief
says dark and terminal-native, but the base is warm and the accent
carries exactly one meaning rather than decorating.

## The build

`clank web [--repo <path>] [--port 8088]`, 127.0.0.1 only, no auth
(PoC). One server per repo, like one TUI per repo.

- **Which session.** Not the convention alone. `open_one` adds the
  repo's tab to whatever session the caller is IN, so a repo's panes
  can live in a session named for a different repo — penlock's did,
  inside `clank-full-app-sim--38b0`, which is how its rogue TUI came
  to be (codex on 7800c69). The server picks in order: `--session` if
  given; else `ZELLIJ_SESSION_NAME` when it runs inside zellij; else
  `clank-<basename>`. Whichever it picks is VERIFIED: the listing must
  contain this repo's status pane or an agent pane (`repo_tab_id`,
  which exists), or the server prints which session it looked in and
  what it expected and exits — never a page of empty strips. The
  listing needs a `--session` form, since `snapshot_panes` is guarded
  by `in_session()`: `open_zellij` gains one beside it.
- **The feed is state plus a stream, never a stream alone.** The
  subscribe child emits each pane's initial viewport once, at start;
  a browser that connects later, or reconnects, would otherwise stare
  at blank panes until the agent next prints (codex on 7800c69). So
  the server RETAINS the latest of everything — status, the pane
  table (id, label, columns, rows, open/closed), and the full viewport
  per pane — behind a lock, and every `GET /events` gets a
  snapshot-then-live handoff done in the order that cannot lose an
  update: subscribe to the broadcast FIRST, then read the retained
  state, then send it, then forward the live stream. An update that
  lands between the read and the first forward is already in the
  receiver. A client the broadcast reports as lagged is resynced the
  same way — resend the retained state — rather than left stale.
- **One child per repo**, `zellij --session S subscribe --format json
  --ansi -p <every agent pane>`. Its lines → `SubscribeEvent` →
  retained state → broadcast → each browser as Server-Sent Events.
  Not websockets: one direction, hyper does it as a chunked body with
  no new dependency, and input is a plain `POST /say`.
- **The pane set is re-listed on a bounded cadence**, every few
  seconds, with the same listing the reconciler already uses. A
  status change is a HINT, not the trigger: the TUI creates panes
  asynchronously after the roster change that the watcher sees, so a
  list taken on that wake is too early, and nothing wakes it again;
  and a resize changes nothing the watcher watches (codex on
  7800c69). Each listing is diffed against the pane table: a changed
  set of pane ids restarts the child (kill, respawn with the new
  `-p`s; the initial viewports refill the gap); changed columns or
  rows update the table and emit a `pane_meta` event so the browser
  resizes that term; a `pane_closed` from the child marks the pane
  closed at once, ahead of the next listing. On server exit the child
  is killed — it is a subprocess of ours and must not outlive us.
- **Input.** `POST /say {pane, text}` → `say_to_pane(session, pane,
  text)`: `write-chars` then `write 13`. Empty text sends nothing.
- **Status.** In-process: `StatusSnapshot::build_async` +
  `watch_status_paths` (the TUI's own watcher) → on change, retained
  and broadcast as a `status` event carrying the snapshot's JSON plus
  the strip data: per agent, role, `in_progress_rows`' verb and
  `since`, and `bar_text`. Reused, never re-derived.
- **The page.** One HTML file, `include_str!`'d like `setup_assets`,
  with xterm.js from cdnjs for the PoC (vendoring is the follow-up;
  the phone has internet). Each pane is an xterm sized `columns ×
  viewport.len()`; every update is a full repaint: cursor home, the
  lines joined with CRLF, clear to end. A `pane_closed` dims the
  strip and says so. Dimensions change → resize the term.
- **Ownership.** The zellij gate stands: `subscribe_panes` and
  `say_to_pane` and the session-targeted listing live in
  `open_zellij.rs`; `cli/web/` never names `zellij`. Same for git.

### Files

`cli/web/mod.rs` (args, server, routes), `cli/web/feed.rs`
(SubscribeEvent, SSE framing, the strip projection),
`cli/web/page.html`; `open_zellij.rs` gains the three primitives;
`command.rs`/`mod.rs` gain `Web(WebArgs)`.

## Tests

No binary is spawned by any test (the repo's rule). The pure parts
carry it:

- `SubscribeEvent` decodes the REAL lines: two captured 0.45.0 events
  (one initial, one closed) trimmed to a fixture — a pane id, 135 →
  3 lines, ANSI intact — and an unknown `event` value is kept, not an
  error, so a new zellij cannot kill the feed.
- SSE framing: an event becomes `event: pane\ndata: {json}\n\n`, and a
  json containing a newline is still one frame (viewport lines never
  contain `\n`; the framing does not rely on it).
- The strip projection from a snapshot fixture: role, verb, `since`
  → the same `age::coarse` text the TUI shows; an idle agent has no
  verb and no dot; the lamp line is `bar_text`.
- `say_to_pane`'s argv, assertable like `new_pane_argv`: `write-chars`
  with the text verbatim (a leading `-` cannot become a flag — `--`
  precedes it), then `write 13`; empty text yields no argv at all.
- `subscribe_panes`' argv: `--session S`, `--format json`, `--ansi`,
  one `-p` per pane, and zero panes yields no child.
- Session choice, pure over its inputs: `--session` wins; inside
  zellij the current session; else the convention — and a listing
  with none of this repo's panes is a refusal that names the session
  it looked in, never an empty page.
- The retained state, with an injected feed (no child):
  - a client connecting AFTER the initial viewports arrived receives
    every pane's current viewport and the status before any live
    frame;
  - a client reconnecting while the panes are QUIET receives the same
    — the stream alone would have given it nothing;
  - an update arriving between a client's snapshot read and its first
    live frame is delivered, not lost (the subscribe-then-read order);
  - a client the broadcast reports as lagged gets the retained state
    again.
- Pane-set lifecycle, with injected listings:
  - a pane that appears one listing AFTER the status change is still
    picked up, and the child is respawned with it (delayed creation);
  - a listing whose only change is one pane's columns emits
    `pane_meta` and respawns nothing;
  - a `pane_closed` marks the pane closed before any listing does;
  - an unchanged listing is a no-op — no respawn, no event.
- The server, in-process like `github_events`' own tests: bound on
  127.0.0.1:0 with a fake feed, `GET /` serves the page, `GET /events`
  streams one frame per broadcast, `POST /say` reaches the sender
  with the pane and text it was given.

The browser side — xterm repaint, the pinch container, the strip
switching — is smoke-tested by hand on lloyd's phone and desktop;
that is the PoC's actual deliverable, and the findings go in the
finish message.

## Out of scope — after the PoC

- Launching a session from the web. Every `clank open` path attaches
  a terminal, and whether 0.45 can create one headlessly is unchecked.
- Any security: tokens, who is connected, TLS, tunnels. Localhost.
- Starting it from the TUI, or the TUI showing it is on.
- The status pane, scrollback, control keys beyond text + Enter,
  more than one repo per server, vendored xterm.js.
