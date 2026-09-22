# a-message-wakes-its-agent-until-it-is-handled
# A message wakes its agent until it is handled

## Why

> "we need each agent to have an inbox directory. Messages in the inbox
> are markdown files and are usually prefixed with a priority (just
> like the plan queue -- use the same code). The difference between
> this and the queue is that these are just arbitrary messages and they
> can be sent to any agent. Both master and reviewer skills should
> include this. getting an inbox message should wake up the agent until
> the message is removed from the inbox and acted upon.
>
> There should be a new cli subcommand which allows sending messages to
> other agents. Messages are addressed with the path to the repo and the
> agents name (but with some way to just address master regardless of
> their name)."

Agents can only reach each other through the review cycle today:
there is no way for one agent — in this repo or another — to hand
another a piece of work that is not a commit. The one inbox that exists
is GitHub's (`.clank/agents/<label>/events/`), and it is the wrong
model to copy: its items come from event sources that only run inside
a PARKED wait, so `--peek`, SessionStart catch-up and a wait whose
first pass finds repo work never see them (`cli/wait.rs:212-273`).

## The model

**An inbox is a directory of priority-named markdown files, the same
kind of directory as the plan queue.** `.clank/agents/<label>/inbox/`
holds `NNN-<name>.md`; an unprefixed file parses at 999 and sorts last,
exactly as in the queue, while `clank send --priority` defaults to 500
as `queue add` does — the extraction keeps those two defaults apart. The queue's directory code (`queue.rs`:
`parse_queue_stem`, `scan_queue`, `resolve_entry`, `find_unique`,
`set_priority`, `remove`) is hard-wired to a private `queue_dir()`, so
it moves into one module over a directory, and the queue and the inbox
are both its callers. What stays the queue's own: plan-name validation,
duplicate-name refusal, drafts consumption, promote and unqueue.

**A message is its file, and its name is its place in line and its
identity.** The name is a sequence number, zero-padded to eight digits,
then a slug of the first line: `500-00000017-rebase-please.md`. The number comes BEFORE the
slug, so within a priority the `(priority, name)` sort is send order —
a timestamp-then-subject name would sort two same-second sends by
subject, and an unpadded suffix puts `-10` before `-2` (codex on
15bea1a).

**Sending never replaces a message.** Atomic visibility is not
exclusive creation: two senders renaming complete files onto the same
name both succeed, and the second silently deletes the first before
its recipient saw it. So a send holds the inbox's lock
(`inbox/.lock`, flock) while it allocates the next number and
publishes. It writes the body to a
dot-named temporary file the scanner never reads, then publishes it by
hard link, which refuses an existing name where a rename would replace
it; on that refusal it takes the next number and tries again, so even
a writer that bypassed the lock cannot lose a message.

**A number is never reused.** The name is what a wake line shows and
what `show`, `done` and `reprioritise` take, so it outlives the file:
a counter derived from the highest PENDING message restarts when the
inbox drains, gives the next message with the same first line the same
name, and a late `clank inbox done` for the old one removes the new
one unread (codex on aeceb8e). The counter lives in `inbox/.seq` — the
last number allocated — and advances under the lock: the next number
is one past the larger of it and the highest pending, so a message
written by hand cannot make it go back. Eight digits is a hundred
million sends to one inbox; a send past that is refused rather than
wrapped. The send time lives in the header, not the name. The body is markdown under a small header:

```
---
from: claude
repo: /Users/llfourn/src/clank
sent: 2026-09-22T10:15:00Z
---
```

`from` and `repo` are the sender's label and repo when `clank send`
runs inside an agent session; they are absent when a person sends it.
They are what the recipient replies to.

**Pending messages are repo work, not an event source.** Reading an
inbox is one directory listing, so the wait reads it in the SAME pass
as `work_for` — the first pass, every refold and `--peek` — and a
pending message is a `WaitItem` alongside the agent's other work. That
is what makes "wakes until removed" true on every path: a parked wait,
a wait that returns repo work, SessionStart catch-up and codex's peek
all carry it. `.clank/agents` is already a recursive wake dir
(`repo_watch.rs:29-37`), so a message landing in the inbox wakes a
parked wait with no new watching. Messages go to any role; a pending
block still parks an agent entirely, messages included.

**A message is handled when it leaves the inbox.** `clank inbox done
<name>` removes it; until then every arm returns it. Sending never
waits for the recipient, and nothing tracks a reply: a reply is a
message sent the other way.

## Deliverables

1. **The priority directory, once.** A module over a directory:
   parse and format `NNN-<name>`, scan in `(priority, name)` order,
   resolve a handle (exact stem, then a unique name), reprioritise by
   rename, remove. The queue calls it for `.clank/queue/`; its
   behaviour and error text are unchanged, and its tests pass
   untouched.
2. **`clank send`.**
   `clank send [--repo <path>] (--to <label> | --master) [--priority N]
   (-m <text>… | -F <file>)`.
   - `--repo` names the RECIPIENT's repo and defaults to the current
     one. `--master` resolves whoever that repo's roster names master
     at send time. Labels may contain almost any character, so the
     master is addressed by a flag rather than a reserved spelling.
   - The recipient must be on that repo's roster; anything else is
     refused, naming the roster.
   - `--priority` defaults to 500, as `queue add` does.
   - Allocation and publication are as the model says: under the inbox
     lock, a dot-named temporary file, published by hard link, retried
     on the next number when the name is taken. A wait never reads half
     a message, and no send ever replaces one.
   - It prints the path it wrote.
3. **`clank inbox`**, for the calling agent (or `--author`), with
   `--repo` like `clank events`:
   - `list` (the default): each message's name, priority, sender and
     first line, in the order they wake;
   - `show <name>`: the whole message;
   - `done <name>`: remove it;
   - `reprioritise <name> <N>`.
4. **The wake.** A `WaitItem` for a pending message carries its name,
   priority, sender and repo, and first line. It is produced wherever
   `work_for` runs — initial pass, refold, `--peek` — for the calling
   agent's own inbox only. `firings_from_items` maps it to no hook.
   The Stop hook renders it as
   `  - message: <name> from <from> (<repo>) (priority NNN)`, with a
   discharge line: `· fires until the message is handled —
   clank inbox done <name>`. It reaches the hook's JSON through its own
   fields, not the catch-all.
5. **The skills.** The shared core both role skills are composed from
   (`setup_assets/skill_shared_core.md`) gains an inbox section:
   - a `message` item means read it (`clank inbox show`), act on it,
     then `clank inbox done`;
   - reply with `clank send --repo <repo> --to <from>`;
   - send with `clank send`, addressing the master with `--master`.

   The master's command list names `clank send`. `clank setup` installs
   the new text, and doctor's drift check covers it as it covers the
   rest.

## Tests

- **The queue is unchanged**: its existing tests pass without edits
  once it calls the shared directory module.
- **Order**, with a fixed clock: sends named `z-first` then `a-second`
  read in that order; twelve sends with the same first line read in
  send order (the tenth after the ninth, not after the first); a higher
  priority sent later reads first; an unprefixed file sorts last.
- **Every path carries a pending message**: the first pass of a wait,
  a refold after one lands, and `--peek` each return it. A wait whose
  first pass finds repo work returns the message WITH that work. This
  is the case GitHub events miss. Mutation check: read the inbox only
  in the park loop and the peek and first-pass cases fail.
- **It keeps waking until done**: two arms both return it; after
  `clank inbox done` neither does.
- **A landing message wakes a parked wait**: write one into the inbox
  of an agent whose wait is parked, and it returns.
- **Addressing**: `--master` resolves the current master by roster, and
  follows a `clank agent promote`; `--to` a label not on the roster is
  refused; `--repo` writes into the other repo's inbox.
- **Sender**: sent from an agent session, the header names its label
  and repo; sent without one, both are absent.
- **Atomicity**: a scan never sees a file under its final name before
  its body is complete, and never lists a temporary file.
- **No replacement**: with the file the next send would choose already
  present (a writer that bypassed the lock), the send publishes under
  the next number and the existing file is byte-identical. Mutation
  check: publish by rename and this fails.
- **Concurrent senders**: threads released together each send to one
  inbox; every message survives with its full, distinct body.
- **A handled name stays dead**: send A, `done` it, send B with the same
  first line; B has a different name, and retrying A's `done` fails
  with "no such message" and leaves B byte-identical. The same holds
  for a wake line naming A that is acted on after B arrives. Mutation
  check: allocate from the highest pending and this fails.
- **The hook line and discharge** for a message item.

Run `cargo test -p clank --lib cli::queue`, the modules touched, the
whole `-p clank --lib` suite, and `cargo test -p clank --test it
wait_` for the wait integration modules.

## Out of scope

- Standby agents that only messages wake — the next plan, which needs
  this one.
- Showing inboxes in the TUI or the web page.
- Delivery across machines: an address is a path this machine can
  write.
