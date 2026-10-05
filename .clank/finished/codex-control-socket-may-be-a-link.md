# codex-control-socket-may-be-a-link
# Codex's control socket may be a link

## Why

Codex 0.160 publishes `~/.codex/app-server-control/app-server-control.sock` as a
symlink into `/private/tmp/codex-daemon-501/<hash>`.
`codex_interrupt::target()` checks the path with `symlink_metadata`, which sees
the link, not the socket, and returns `None`. So every native Codex wake falls
back to the terminal, including today's event interruptions. Found in the
research plan `research-agent-stalls-and-provider-capacity-recovery` (D1,
appendix A5).

## What to do

Resolve the link and apply the existing checks to the final target: it must be a
socket and owned by this user. A dangling link, or one resolving to anything else,
keeps refusing. Keep the "never borrow the sender's CODEX_HOME" rule unchanged.

## Acceptance

- A test with a real Unix socket behind a symlink is accepted.
- These are refused:
  - a symlink to a regular file;
  - a dangling link;
  - a socket owned by another user. Cover this through the ownership check's
    seam; no foreign user is needed.
