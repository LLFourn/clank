# the-queue-is-workable-from-the-phone

The remote page shows the queue and can order it.

> "can you queue a plan to be able to do queue management in the
> remote web ui." — lloyd

## Why

The page knows the queue as a NUMBER: `Ledger::queue` is a count, so
`4 queued` is everything it can say. What is in it, what runs next,
and what order it runs in are the pane's to show and the pane's to
change — and the pane is on the machine, which is the thing the remote
exists to get away from.

The work itself is already factored: `queue::promote`, `queue::unqueue`
and `queue::set_priority` are the primitives the TUI's queued-plan page
calls, and the remote runs inside that same TUI process, so the page
can ask for exactly what the pane asks for. Nothing about promoting
needs to be written twice — and must not be: a second opinion about
what promoting means is how the two surfaces come to disagree.

## The build

**The queue arrives as entries.** The facts carry what the pane's
QUEUE section draws — each entry's name, its priority, and the FILE
that is its identity — in the order the queue will run them. The count
stays, derived from the entries rather than beside them.

**An entry is addressed by its file, never by its name.** `queue add`
allows two files that parse to one name, which is why the pane's page
resolves an entry by path and closes rather than guess when two
survive (`a-queued-plan-has-a-page`). A page that promoted "the one
called foo" would promote whichever the server found first.

**The actions are the pane's actions.** Promote, unqueue and a new
priority, each posted to this origin and each landing in the core the
pane calls — behind the door, same-origin checked, as `/say` is. The
page does not edit its own copy of the queue: it asks, and the next
status frame is what redraws it, so what the page shows is what the
repo has.

**What the pane asks about, the page asks about.** Promote commits the
intro and starts the review cycle; unqueue takes the plan out of the
queue. Both are confirmed before they are sent, and a refusal is shown
with its reason rather than swallowed — the phone is where you are
least able to go and look at what happened.

## Tests

- The facts carry each queued entry with its priority and its handle,
  in the order the queue runs them, and the count agrees with the
  entries.
- Each action lands in the core the pane uses: a promote makes it the
  active plan, an unqueue leaves the body in the drafts, a priority
  write reorders the queue.
- Two entries parsing to ONE name: the action acts on the one the page
  named, by file, and a handle that no longer exists is refused rather
  than resolved to its sibling.
- The routes are behind the door and same-origin, as `/say` is: no
  session is a 401, another origin is a 403.
- A refused action answers with its reason, and the page says it.
- The page names the routes it calls, so the two ship together.

## Out of scope

- Putting a plan INTO the queue from the page. That needs a body, which
  is writing rather than managing; the drafts directory and `queue add`
  stay where they are.
- The stash, which the page also counts. Same shape, another plan.
