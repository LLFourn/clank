# github-poll-304-is-success
# github poll: gh's non-zero exit on HTTP 304 is SUCCESS, not failure

A controller repo's `clank wait` prints, per watched repo, per poll
cycle (reproduced by lloyd's agent, 2026-07-13):

    wait: github frostsnap/frostsnap poll failed: gh /events failed: gh: HTTP 304

304 Not Modified is the HEALTHY answer to the conditional
`If-None-Match` poll — it's what the ETag machinery exists for. The
bug is in `GhFetcher::fetch` (github_events.rs): it bails on
`!out.status.success()` BEFORE parsing, and `gh api` exits NON-ZERO
for every non-2xx status, 304 included. `parse_gh_include` already
classifies a 304 correctly — it just never gets the bytes. Beyond the
noise, a REAL failure (auth, network) prints the same "poll failed"
shape and can hide in the 304 stream.

## Fix

Extract the exit-status interpretation as a pure function the fetcher
calls — `interpret_gh_events_output(success, stdout, stderr) ->
anyhow::Result<GhResponse>`:

- success → `parse_gh_include(stdout)` as today.
- failure → reclassify ONLY on an EXACT 304 (codex a950e7b —
  `parse_gh_include` treats any non-304 status as an Ok body, even an
  empty one, so routing every failed include block through it would
  swallow a 401/403/500 as a healthy poll):
  - stdout whose `--include` STATUS LINE parses to exactly 304 →
    `NotModified` (with the `X-Poll-Interval` floor when the header
    is present — the server can raise it on a 304).
  - else stderr matching gh's 304 shape STRICTLY (the
    `gh: HTTP 304` line, not a substring grab that a proxy error
    mentioning 304 could satisfy) → `NotModified { poll_interval:
    None }`.
  - EVERYTHING else — any other status in the include block (401,
    403, 500, with empty or list-shaped bodies), or no recognizable
    304 anywhere — stays `Err` with stderr preserved verbatim. Real
    failures keep logging loudly.

`poll_loop` is untouched: `NotModified` is already silent there.

## Acceptance

- Unit tests on the pure interpreter: 200 body parses; non-zero exit
  with a 304 `--include` block → NotModified carrying the floor;
  non-zero exit with bare `gh: HTTP 304` stderr → NotModified;
  non-zero exit with a genuine error stderr → Err containing it;
  non-zero exit with a NON-304 include block (401/403/500 — empty
  body AND list-shaped body) → Err, never a healthy poll (codex
  a950e7b); a stderr merely MENTIONING 304 inside another message →
  Err.
- A poll_loop-level test (scripted fetcher) already pins that
  NotModified emits nothing and logs nothing — reference it; the new
  behavior needs no loop change.
- fmt/clippy at the 18/6 baseline; suites green; no network in tests.
