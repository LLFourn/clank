# a-repo-remembers-its-port

> Let's just sample a new port from the OS and set it in the .clank
> what port we use. If something is already running we check if it's
> our clank server already — it should have some JSON API or
> something, right? — lloyd

## One port for everyone is one port too few

`clank web` listens on 8088 unless told otherwise, and so does every
other repo's. Two repos cannot both be on, a second press against a
port that is taken reads `Address already in use`, and a server that
is already up for this repo — started by hand, or by a TUI that
died without unwinding — looks the same as a stranger on the port.
A hashed port would be stable but is a guess; a fresh port each time
is never a guess but never the same; and neither knows what is
already there.

## The design

**The repo remembers.** The first time a repo's server starts with
no `--port`, it asks the OS for a free port (bind port 0, read the
port back) and records it in `.clank/config.json` under `web.port`,
through the typed `RepoConfigFile` read/write, never a JSON literal.
Every later start binds that port. `--port` still says exactly
where, and records nothing.

**The server says who it is.** `GET /identity` answers JSON —
`{ "clank": "web", "repo": <canonical path>, "session": <zellij
session>, "pid": <pid>, "port": <port> }`. That is what makes the
next rule possible, and what a hub could enumerate later.

**A taken port is asked.** When the remembered port will not bind,
the server asks `http://127.0.0.1:<port>/identity` (a one-second
request). A clank server for THIS repo answers: this one prints
`clank web: already on <url> (pid <n>)` and exits 0 — the operator
has what they wanted. Anything else answers, or nothing does: the
port is somebody else's now; a fresh one is sampled, recorded, and
bound, and the old number is forgotten. `--port` never probes: it
was told.

**The switch adopts, and watches.** `/identity` also reports
`attached_to` — the pid the server ends with, or none. The launcher
that finds this repo's server up prints `clank web: already on <url>
(pid <n>, attached to <m>|nobody)` and exits 0, and the switch reads
that line as one of two things:

- *Somebody else's*: attached to a pid that is alive and is not this
  TUI. The row shows the URL, on, marked `pid <m>`; `o` opens it;
  Enter says whose it is and does nothing to it. Nothing is owned.
  (The lease keeps two TUIs off one repo, so this is a hand-run
  server told to follow some other process — rare, and not ours.)
- *Adopted*: attached to nobody, or to this TUI, or to a pid that is
  dead — the last cannot last, since the server's own watcher ends
  it within two seconds, and the switch will see that go. The row
  shows the URL, on; Enter sends SIGTERM to the pid the line names,
  as does leaving the TUI — once switched on, off means off, whoever
  started it; and the switch WATCHES the pid: a thread polls it
  every two seconds (the `kill(pid, 0)` the server uses for its own
  owner) and reports `Exited { pid, why: "the server is gone" }` when
  it disappears, which turns the row to failed with that reason.

Every exit outcome carries the pid it is about, and one for a pid
the switch is not holding is nothing — the launcher's own exit,
after the `already on` line, is exactly that.

**Bind before children.** The port is bound before the `zellij
subscribe` child exists. It came after, and a bind that failed
returned with the poll task's clone of the child still alive — a
subscribe streaming to nobody, found live after a press against a
taken port. With the port first, a failed bind has nothing to clean
up.

Rejected: hashing the path — stable, but a guess with a birthday
problem and no answer to "who is there"; a fresh port every time —
never bookmarkable; a hub on one port — the right shape for a
tunnel, later, and `/identity` is its first brick.

## The build

- `web/mod.rs`: `port_for(repo, explicit)` — remembered, else
  sampled and recorded; the probe; `/identity`; the `already on`
  line; the bind moved before the subscription.
- `cli/config.rs` (or beside `RepoConfigFile`): `web.port` read and
  write, typed.
- `status_tui/remote.rs`: the `already on` line → on with that pid,
  owned or somebody else's; `Outcome::Exited { pid, .. }` matched
  against the held pid; `Io::watch(pid, report)` — a poll every two
  seconds until the pid is gone — started on adoption.
- README: the port is the repo's own; `--port` overrides.

## Tests

- `port_for`: no record → a port the OS gave, now recorded; a record
  → that port; `--port` → that port, nothing recorded.
- The probe against a listener answering `/identity` for this repo
  → "already on"; against one for another repo, or a listener that
  is not clank, or a closed port → a fresh port, recorded.
- `/identity` through the route: the fields, as JSON.
- `Remote`: the `already on` line → on, URL shown, pid held; the
  launcher's own exit afterwards → still on; Enter → TERM to that
  pid; the watcher reporting the pid gone → failed, `the server is
  gone`; a line naming a live other owner → shown, not owned, Enter
  refuses with the owner's pid; adoption of a server attached to a
  dead prior TUI → the server ends on its own (the `--attached-to`
  test) and the watcher turns the row off.
- Mutations: the record not written — caught; the probe skipped —
  caught; an exit for the wrong pid ending the switch — caught; the
  adopted pid not watched — caught; a live other owner's server
  owned — caught; the
  subscription started before the bind — caught by a test that a
  failed bind leaves no child (the subscription takes the bound
  listener as its witness, so this one is the type's).

## Out of scope

- A hub serving several repos on one port.
- Reach beyond localhost.

## Acceptance

- [ ] a repo's server comes up on the port it remembers, or a fresh
      one the first time, recorded in `.clank/config.json`
- [ ] a press while this repo's server is already up shows its URL
      and owns it — and the row turns off when that server goes; a
      server following another live process is shown, not owned; a
      stranger on the port is left alone
- [ ] `--port` still means exactly that port
- [ ] a failed bind leaves no `zellij subscribe` behind
- [ ] tests as above, mutation-checked
