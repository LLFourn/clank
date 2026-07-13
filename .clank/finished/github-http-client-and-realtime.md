# github-http-client-and-realtime
# github sources: real HTTP client + realtime webhook relay, polling as floor

Every GitHub interaction shells `gh` and hand-parses its `--include`
text blob — this week's 304-exit-code bug, the CRLF header bug, and
the substring status check all came from re-implementing the bottom
half of an HTTP client around a tool that hides it (lloyd,
2026-07-13). And polling is the only delivery mode, though GitHub has
one official push channel.

## M1 — transport: HTTP client, gh's credential

- Token acquisition, once per host, in order: `GH_TOKEN` /
  `GITHUB_TOKEN` env, else `gh auth token` (one subprocess at source
  startup — the LAST place gh is shelled for polling). Re-acquire on
  a 401 (token rotated) before degrading the source.
- The poller's transport becomes a real HTTP client — recommend
  `reqwest` (rustls, default-features off; tokio is already the
  runtime) — hitting `api.github.com/repos/<r>/events` with typed
  status/headers: `If-None-Match` out, `ETag` + `X-Poll-Interval` in,
  `status == 304` as a VALUE. `interpret_gh_events_output`,
  `parse_gh_include`, and `http_status_of` are DELETED, not hardened.
  The `EventFetcher` seam and the whole poll state machine
  (seen-cursor, floors, baselines) stay exactly as tested.
- `gh_login` (own-action filter) moves to `GET /user` over the same
  client.
- Dependency call is an intro-review question: reqwest vs ureq (in
  spawn_blocking). Recommendation: reqwest.

## M2 — realtime where possible, explicit and fallback-safe

- Per-source opt-in: `"delivery": "realtime"` (default `"poll"`).
  Realtime uses GitHub's OFFICIAL webhook-forwarding relay — the
  `gh webhook forward` machinery: clank spawns
  `gh webhook forward --repo <r> --events <mapped set> --url
  http://127.0.0.1:<port>` as a supervised child (process-group,
  killed with the wait, like command sources) and receives webhook
  deliveries on a loopback listener; each delivery maps through a
  webhook-payload variant of the classifier (actions differ slightly
  from the events feed — e.g. a real `synchronize`).
- **One coordinator per source owns emission** (codex 1ddc0f6).
  Polling NEVER stops and never goes silent when realtime is on —
  realtime only front-runs it for latency. Both payload forms
  normalize to a content-derived ACTION KEY (the classified item's
  semantic identity: `pr_opened#12`, `pr_updated#12@<head sha>`,
  `branch_push@<ref>+<head>`, comment/review ids — both forms carry
  the same underlying objects; feed event ids and webhook delivery
  GUIDs are different id spaces and are NOT the key). A single
  bounded seen-ACTION set (same eviction story as the feed cursor's
  SeenSet) gates emission: FIRST arrival on either path emits, the
  other path's copy is dropped at the gate. The transport-level
  feed-id cursor stays, underneath, unchanged.
- **The invariant**: at-most-once emission per action key within the
  horizon, and no missed wake for any action that reaches EITHER
  path — the poll remains the completeness backstop (a webhook the
  forwarder drops is emitted by the next poll), realtime is purely
  the latency path. Forwarder death between polls loses nothing,
  because polling never stopped; the fallback stderr line is
  informational only.
- **Baseline owns the horizon** (codex afb7a35). The poll's
  arm-time baseline defines "now", for BOTH paths, structurally:
  1. Source start spawns the relay AND runs the baseline fetch
     concurrently; relay deliveries BUFFER (bounded, drop-oldest
     with a stderr note — the window is startup-sized) and emit
     NOTHING until a baseline has succeeded.
  2. The successful baseline seeds the feed-id cursor AND the
     action-key set (every action visible at arm time is
     pre-seeded), and emits nothing — exactly today's delta-from-now.
  3. Only then does the buffered relay backlog drain through the
     gate: pre-baseline actions hit their seeded keys and drop; a
     genuinely-new action emits once.
  While the baseline keeps failing, webhook emission stays gated —
  realtime without a poll-established horizon could replay arbitrary
  history. The failed-arm-time rule is unchanged from today: the
  first SUCCESSFUL fetch is the baseline.
- Honest constraints, stated in config docs: requires ADMIN on the
  watched repo (it creates a webhook), the `cli/gh-webhook`
  extension, and GitHub bills it as dev tooling.
- We deliberately do NOT reimplement the relay's websocket protocol:
  the gh extension owns that (it can shift); clank owns the listener
  and the mapping. Listener is minimal (loopback-only, hyper).

## Acceptance

- M1: injected-transport tests unchanged in spirit (the fetcher trait
  survives); token order pinned (env beats gh, 401 re-acquires); the
  three hand-parsers are gone; NO subprocess in the poll path.
- M2: webhook-payload fixtures for the mapped kinds (incl.
  synchronize and a tag-push ignored); the coordinator invariant
  pinned three ways (codex 1ddc0f6): webhook-then-poll of the SAME
  action → exactly one emission; a poll-only action while realtime
  is healthy (forwarder missed it) → the poll emits it; forwarder
  death between polls → the next poll emits everything, no gap
  (injected supervisor + scripted transport, no network). Baseline
  lifecycle pinned (codex afb7a35): an action existing at arm time,
  delivered LATER by webhook → no wake (its key was baseline-seeded);
  a webhook delivery arriving BEFORE the baseline completes buffers
  and then gates correctly; a post-baseline action webhook-then-poll
  → exactly one. Listener rejects non-loopback binds by
  construction.
- Config round-trip: `delivery` absent = poll; unknown value fails
  loud. Skill docs updated (controller section: when realtime is
  available and what it needs).
- fmt/clippy baseline; suites green; no network in tests.
