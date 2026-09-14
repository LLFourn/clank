# remote-access-is-the-tuis-to-grant

> What would be the best way to grant remote access? A cloudflare
> tunnel for a fixed URL — alternatives? Some kind of API key, set at
> user level — what's the correct auth scheme? It would be great to
> auth with my YubiKey on the phone, storing its public key in my
> user-level config. How do we make sure the remote is open as long
> as the clank session is — should `clank status --tui` just run the
> web server in process? zellij is a dependency of the web server
> and the TUI must be open for it to work, so `clank web` outside the
> TUI doesn't make much sense. Promote `clank status --tui` to
> `clank tui`; it's a very big thing now. Queue a plan; come up with
> a design. Don't implement code just yet. — lloyd

This plan is the design. It answers the four questions and names
the plans that build it; it is finished when the document is agreed.

## What is true today

- The TUI is `clank status --tui`, an async loop on a tokio runtime
  already; one per repo, held by a lease.
- `clank web` is a process of its own, started by the TUI's `remote`
  row through a launcher, adopted if found running, watched for its
  exit, attached to the TUI's pid so it dies with it, identified at
  `/identity` so a taken port can be told from a stranger's — a
  page of machinery whose whole purpose is that the server is NOT in
  the TUI. It binds `127.0.0.1` on a port the repo remembers.
- Nothing authenticates. The page can type into agents (`/say`).
- `cloudflared` 2026.8.2 is on this machine; tailscale and ngrok are
  not.

## The design

### 1. `clank tui`

The TUI becomes a command: `clank tui [--repo]`. `clank status
--tui` stays one release as an alias that says so once on stderr,
so muscle memory and the panes of sessions already open keep
working; the layout composer runs `clank tui` from now on. A whole-
repo sweep (README, RELEASE-CHECKLIST, skills, error strings, tests,
the ~45 mentions across 12 files) — not the src-only grep that finds
strays one review at a time.

### 2. The remote lives in the TUI — as one owned thing

The web server runs on the TUI's own runtime, but "a task on the
runtime" is not a lifetime: today's server spawns a status thread,
a transcript tail per agent, a pane-poll task that restarts the
`zellij subscribe` child, and a task per HTTP connection, none of
which a dropped server task would end (codex on 8b19fc6). So the
remote is ONE value, `Remote`, that owns everything it started —
the listener, every connection task (a `JoinSet`), the status and
tail producers, the subscription child, and the tunnel child of §3
— under one cancellation token. Switching off, leaving the TUI, or a
start that fails part-way cancels the token, ends the children, and
JOINS every task and thread before the switch reads `off` — so a
restart never races a producer of the last instance, and an SSE
stream never outlives the remote that served it. Nothing of the
remote's is detached; a detached handle is exactly the leak.

The `remote` row starts and stops that value. The remembered port
stays — it is what a bookmark is. Gone, because in-process makes
each of them a question with no answer: the launcher and its
`already on` line, adoption, the watcher, `--attached-to`,
`/identity` and the probe, `clank web` as a command. The `web`
module stays as the server; `clank web` is not a thing a person
runs. What the row says is unchanged: `off`, `starting…`, the URL,
`failed — <reason>`.

### 3. Reaching it: a tunnel the TUI switches on

The server keeps binding loopback. Reaching it from a phone is a
tunnel, and the tunnel is the TUI's to start and stop with the
remote, as a child owned by the `Remote` of §2, from user-level
config:

    ~/.clank/config.json
    "remote": {
      "tunnel": {
        "name": "clank",
        "run": ["cloudflared", "tunnel", "run", "--url", "http://127.0.0.1:{port}", "clank"],
        "url": "https://clank.example.com"
      }
    }

`{port}` is the repo's remembered port. Its own secrets — the
tunnel's credentials — stay in cloudflared's files, never in
clank's.

**Up means reachable, proven end to end.** A live child is not a
public URL (codex on 8b19fc6): the server answers `GET /instance`
with a nonce minted at this remote's start, and the tunnel is
`starting…` until the TUI fetches `<url>/instance` THROUGH the
public URL and reads its own nonce back — then the row shows the
public URL. Another nonce means another instance is behind the
hostname; no answer within the start's grace means the tunnel is
not up, and the child's stderr is the reason. This is
provider-agnostic: any tunnel that forwards the hostname to the port
passes the same check.

**One tunnel, one holder.** Cloudflare runs any number of connectors
for one named tunnel — a second `cloudflared tunnel run` does not
fail, it becomes a replica, and two repos would silently share the
hostname (codex on 8b19fc6). So the tunnel is claimed before it is
run: a user-level lease keyed by its `name`
(`~/.clank/remote/<name>.lock`, flocked, holder recorded, released
when the child is gone — the status lease's own mechanism), and a
TUI that finds it held says whose it is on the row and starts
nothing. The nonce check above is the belt to that lease's braces.

Why a NAMED Cloudflare tunnel: a fixed hostname and TLS, both of
which the auth below needs (a passkey is bound to an origin; a
random `trycloudflare.com` hostname every start would need a new
registration every start). Alternatives, so the choice is a choice:
Tailscale `serve` — stable name, TLS, and no public exposure at all,
the best answer when phone and desktop share a tailnet; not
installed here, and the config above runs it as well as it runs
cloudflared. An SSH reverse tunnel to a VPS — the same shape, with
TLS to arrange oneself. A quick tunnel — no account, but a new
origin every time; fine for a demo, wrong for a passkey.

Several repos behind one hostname is a hub, and a later plan.

### 4. Who gets in: a passkey, and links the TUI mints

**Not an API key.** A static secret at user level pasted into a
phone URL is a password with worse hygiene: it lives in browser
history, tunnel logs and screenshots, and it never rotates. It also
does not use the YubiKey.

**WebAuthn, with the YubiKey as the passkey.** The server is a
relying party (`webauthn-rs`); the RP ID is the tunnel's hostname,
which is why the hostname must be fixed. A YubiKey over NFC or
USB-C is a passkey to the phone's browser. What is stored at user
level is the credential — id, COSE public key, sign count, a name,
when it was added — under `~/.clank/config.json#/remote/passkeys`.
Not the YubiKey's SSH public key: a FIDO credential is minted per
origin at registration, so there is nothing to copy in beforehand;
registration is one ceremony, once.

**The TUI mints the way in.** Whoever holds the terminal holds the
root of trust, so the TUI is where doors open:

- *Register a passkey*: a key on the `remote` row mints a one-time
  registration link — a 5-minute token in the URL, shown as text and
  as a QR code (half-block cells) — the phone opens it, the browser
  runs the WebAuthn registration, the credential is stored. A link
  is consumed on use or on expiry.
- *Open it here*: `o` on the row opens the URL with a one-time login
  token, so the desktop browser needs no ceremony and loopback needs
  no trust rule — it cannot have one, since the tunnel arrives from
  loopback too.
- *Log in on the phone*: `/login` runs a WebAuthn assertion against
  the stored passkeys; success sets the session.

**Sessions.** A random 256-bit id in an `HttpOnly; Secure;
SameSite=Strict` cookie, the session record at user level
(`~/.clank/remote-sessions.json`: id hash, passkey name, created,
last seen) with a 30-day life, so a TUI restart does not ask again;
a row action lists and revokes them. Every route but `/login` and
the registration page requires a session — the page, `/events`,
`/say`, `/html/…` — and `/say` additionally checks the `Origin`
header, since it types into agents. A session's end ends its
streams too: every open `/events` connection holds its session id,
and a revocation or expiry closes those connections rather than
waiting for their next request (codex on 8b19fc6). Login attempts
are rate-limited per source; comparisons are constant-time; nothing
secret is logged.

### What this does not do

- A hub: several repos behind one hostname.
- Reach without a tunnel; TLS of clank's own.
- Anything on the page beyond what exists.

## The plans that build it

1. `clank-tui-runs-the-remote-in-process` — §1 and §2: the rename
   and sweep, the server as a task, the deletions.
2. `the-tui-mints-the-way-in` — §4: sessions, one-time links, the
   passkey registration and login, the revocation row.
3. `a-tunnel-is-a-switch-too` — §3: the configured tunnel started
   and stopped with the remote, the public URL on the row.

## Acceptance

- [ ] the four questions have one answer each, with the alternatives
      named and the reason for the choice
- [ ] the three implementation plans are scoped so each is
      reviewable on its own
