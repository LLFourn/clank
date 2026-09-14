# clank-tui-runs-the-remote-in-process

Part 1 of the design finalized as `remote-access-is-the-tuis-to-grant`
(§1 and §2): the TUI becomes `clank tui`, and the web server becomes
one value the TUI owns.

## Why

`clank web` is a process of its own, and everything the TUI does to
run it — the launcher, the `already on` line, adoption, the watcher,
`--attached-to`, `/identity` and the probe — exists only because it
is. zellij is the server's dependency and the TUI must be open for
it to mean anything, so the server belongs in the TUI. And `clank
status --tui` has outgrown its flag.

## The build

**`clank tui`.** A top-level command, `clank tui [--repo]`, running
what `status --tui` runs. `clank status --tui` remains one release
as an alias that prints one line to stderr naming the new command,
so open panes and habits keep working. The layout composer emits
`clank tui --repo …` for the status pane. A whole-repo sweep of
`status --tui` / `--tui` (README, RELEASE-CHECKLIST, skills, hook
text, error strings, doctor, tests; the ~45 mentions in 12 files),
keeping the alias's own mentions and its test.

**`Remote` is one value.** `status_tui::remote::Remote` owns the
server instead of a child: a cancellation token; the listener; a
`JoinSet` of connection tasks; the status loop and each transcript
tail as tasks or joined threads; the `zellij subscribe` child and
the pane poll that restarts it. `toggle` off, `Drop`, and a start
that fails part-way cancel the token, end the children, and join
everything before the state reads `off`. Nothing of the remote's is
detached. The port policy stays (remembered, else sampled and
recorded; `--port` gone with the command); the `remote` row's words
stay (`off`, `starting…`, the URL, `failed — <reason>`); `o` opens
the URL; `r` in the log view still toggles.

**Deleted.** The `Processes` adapter, `Io::start/terminate/watch/
alive`, the launcher argv, `parse_already_on`, `Outcome::AlreadyOn`,
`Outcome::Exited`'s pid, the `attached_to` watcher, `/identity`,
`identity_at`, `Listen::AlreadyOn`, `WebArgs`, `Command::Web`, and
`clank web` from the README. The `web` module stays as the server
the TUI runs, with `run`'s body becoming `Remote`'s start.

## Tests

- `Remote`: on → the listener answers on the remembered port; off →
  the listener is closed, every task joined, the subscribe child
  gone (its pid does not exist), and a second on binds the same
  port at once; a start whose bind fails leaves no task and no child
  and reads `failed — <reason>`; drop while on does the same as off.
  Driven with the `Io`-style injection where a zellij is needed, a
  real listener where not.
- The alias: `clank status --tui` parses to the same run as `clank
  tui` and says so once.
- The sweep: no `status --tui` remains outside the alias and its
  test (a gate test in `it`, like the boundary gates).
- Mutations: a producer left detached — caught by the "second on
  binds at once" test; the subscribe child not ended — caught.

## Out of scope

Auth (part 2), the tunnel (part 3).

## Acceptance

- [ ] `clank tui` runs the TUI; `clank status --tui` still works and
      says so; the layout runs `clank tui`
- [ ] the `remote` row starts and stops a server that lives and dies
      with the TUI, with nothing detached
- [ ] `clank web`, the launcher, adoption, `--attached-to` and
      `/identity` are gone
- [ ] tests as above, mutation-checked
