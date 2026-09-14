# remote-is-a-switch-on-the-status-bar

> Open the new website thing for me. Or better yet allow me to open
> it through `clank status --tui` somehow. A remote on/off option in
> the top status menu bar would be good. — lloyd

## The page is a second command

`clank web` is a process of its own: the operator leaves the TUI,
starts it in some shell, remembers the port, opens a browser, and
later remembers to stop it — or does not, and a `zellij subscribe`
child outlives the intent. The TUI is where the operator already
lives and already reads whether zellij is reachable, in one glyph at
the right end of the bar. Remote is the same kind of fact: on or
off, and if on, where.

## The design

**A switch, not a menu.** The bar's right end already reserves the
cells for the zellij glyph (`⚡`), measured and never displaced. A
second glyph joins it — remote — dim when off, in the band's colour
when on; its width measured the same way, and the two share one
reservation so the lamp text never runs into either. `r` in the log
view toggles it (free there; the event page's `r` is Retry and stays
that). The TUI remains READ-ONLY about the repo: this switch starts
and stops a process of clank's own, nothing more.

**On.** The TUI spawns its own binary — `clank web --repo <repo>
--port 8088 --attached-to <tui pid>` — with stdout and stderr piped.
`clank web` already prints one line when it is listening (`clank
web: http://127.0.0.1:8088  (session …)`); a reader thread hands
that line to the loop, which shows an overlay in the error overlay's
shape — `remote on` / the URL / `opened in your browser` — and opens
the URL with the opener `html.rs` already has. If the child exits
before that line (the port is taken, no zellij session), the overlay
carries its stderr instead: a switch that does nothing says why
(nothing-refuses-in-silence). While the first line is awaited the
glyph is the "not yet known" mark the zellij glyph uses.

**Off.** `r` again, or leaving the TUI, sends the child SIGTERM —
not `Child::kill`, which is SIGKILL and would leave `clank web`'s
`zellij subscribe` child orphaned, the very thing its own shutdown
path exists to drop — then waits briefly and only then kills. A TUI
that dies without unwinding cannot do that, so the server watches
the pid it was attached to: `--attached-to <pid>` makes `clank web`
exit when that process is gone (polled every two seconds; `kill -0`
semantics, no ppid assumptions, since the spawn may not be a direct
child forever). One remote per TUI, and the status lease already
makes one TUI per repo.

**Where.** `http://127.0.0.1:8088`, the server's default, shown in
the overlay each time it is switched on. Not configurable here — a
port knob is a second way to say one number nobody has asked for.

Rejected: a menu in the bar — the bar is a lamp, its grammar is one
fact per glyph, and a menu would need a mode; starting `clank web`
detached with output to a file — an error nobody reads; a browser
tab as the only sign it is on — the TUI would not know.

## The build

- `status_tui/remote.rs`: `Remote` — `Off | Starting | On { url } |
  Failed { why }` — with the spawn, the line reader, TERM-then-KILL,
  and Drop, the process operations injected so the lifecycle is
  tested without a binary (as `TabIndicator` injects its rename).
- `render.rs`: `bar` takes the remote state beside `reach`; one
  reservation for both glyphs, each width-measured.
- `mod.rs`: `r` in `LogScroll` toggles; the reader's line and the
  child's exit arrive on the existing event channel; the overlay.
- `html.rs`: the opener takes a URL as well as a path.
- `web/mod.rs`, `WebArgs`: `--attached-to <pid>`; a watcher task
  that ends the server when the pid is gone.
- README: the switch, in the `clank status --tui` paragraph.

## Tests

- `Remote`: Off + toggle → Starting and one spawn; the listening
  line → On with that URL and the opener called once; a child exit
  before the line → Failed carrying its stderr, no opener; On +
  toggle → TERM sent, Off; Drop while On → TERM; nothing spawned
  twice while Starting.
- `bar`: with remote on/off the zellij glyph sits where it did and
  neither glyph is ever truncated before the lamp text is.
- `--attached-to`: the watcher returns at once for a pid that does
  not exist and not for the test's own pid.
- Mutations: kill instead of TERM; drop without TERM; the failed
  child's stderr not carried — each caught.

## Out of scope

- Binding beyond localhost, tunnels, authentication.
- A port setting.
- Starting `clank web` from anywhere but the TUI.

## Acceptance

- [ ] `r` in the log view starts `clank web` and opens the browser
      on the URL; the bar shows remote on; the overlay says where
- [ ] a failed start shows its reason in the overlay
- [ ] `r` again, or leaving the TUI, ends the server by SIGTERM and
      leaves no `zellij subscribe` behind
- [ ] a TUI that dies takes the server with it within seconds
- [ ] tests as above, mutation-checked
