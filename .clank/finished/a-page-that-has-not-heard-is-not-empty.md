# a-page-that-has-not-heard-is-not-empty

A browser is told the facts when it connects, and says so until it is.

> "I'm trying to join the remote session in ~/src/safepay … but it
> says no agents in this session." — lloyd, on a repo whose pane was
> showing `claude master` and `codex commit` at the time

## Why

`No agents in this session yet.` is the markup's DEFAULT state
(`page.html`: the `.empty` div renders `display: flex`), and the only
thing that ever hides it is `onStatus`. So before the first status
frame the page states, as fact, the one thing it does not know yet —
and it is the first thing anyone sees after signing in.

That is how it misled its own author. The remote above showed it for
two sessions while the pane beside it listed `claude master` and
`codex commit`; the cause was a SyntaxError that killed the page's
script (`a-page-that-does-not-parse-is-not-a-page`), and this message
is what a dead script looks like. I spent a while hunting a delivery
fault that was not there, because the page was answering a question it
had not been asked. A state it cannot distinguish is a state it must
not name.

There is also a real, narrow hole behind it. `Instance::observe`
publishes the facts when the TUI rebuilds its snapshot, and once when
the remote starts — but the listener accepts connections before that
first publish, so a browser arriving in between is given the panes
(polled on their own timer) and no facts. On a busy repo the next
rebuild is seconds away; on an idle one it need not come at all.

## The build

**Connecting is a reason to publish.** The feed answers a new
subscriber with the facts as they are, rather than with whatever it
last happened to send: the remote asks the TUI for its current
snapshot when a browser connects, or the loop hands the remote a
snapshot to retain when the instance starts, so "what a page is given
on connect" and "what the TUI last built" are the same thing rather
than two that drift apart when the repo is quiet.

**Not heard from is its own state.** The page shows the roster's
emptiness only once it has been told the roster: before the first
status frame it says it is waiting — the wording the header already
uses for a channel that is not live — and the empty note is what
remains when the facts arrive and name nobody.

## Tests

- A browser connecting to a remote whose TUI has not rebuilt since it
  started is given the facts, so its first frames name the roster.
- The feed's replay to a late subscriber still carries the latest
  facts, not the first ones.
- The page's empty note is not its default: with no status frame it
  says it is waiting, and it says a session has no agents only when a
  frame said so.

## Out of scope

- The pane poll and its cadence; panes already arrive on their own.
- What the page does when the channel drops, which reconnects already.
