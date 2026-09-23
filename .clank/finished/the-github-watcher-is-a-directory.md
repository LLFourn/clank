# the-github-watcher-is-a-directory

`github_events.rs` becomes a module tree along the seams it already
has.

## Why

One file holds the HTTP session and transport, the events-feed
classifier, the webhook classifier, the poll state machine, the relay
(listener, forwarder, inbox), the ingest lease and supervisor, the
emission coordinator, both source horizons, and the CI reader seam —
2400 production lines and 4500 of tests. `github_ci.rs` holds four
more concerns: a domain model, a paginated reader, a wake mapping and
a durable memory.

Nothing is wrong with the seams. They are already there and the
reviewers agreed they are in the right places — a `FeedHorizon` and a
`CiHorizon`, a `FeedSide` and a `ChecksSide`, a coordinator that is
the one emission gate. What is missing is any boundary between them
in the file system, which is what makes every change to this
subsystem a change to one enormous file, and what makes "where does
this belong" a question with no answer.

The cost is not hypothetical: `a-red-build-is-a-wake` took nine
review rounds, and five of its findings were two pieces of loop state
being taken for each other. Types fixed that where it counted. The
files did not follow.

## The build

**Move, do not rewrite.** A `github/` directory: `session.rs` (the
token, the HTTP seam, the shared session), `feed.rs` (the two
classifiers, the poll state machine, the feed horizon), `ci.rs` (what
`github_ci.rs` is now, itself split if it reads better),
`coordinator.rs`, `relay.rs`, `supervisor.rs` (the lease, the tail,
the takeover) and `log.rs`. One commit per seam, each a pure move with
its tests travelling with it, so every commit is readable as "these
lines went there" and a reviewer can diff for equality rather than
read for meaning.

**Nothing changes behaviour.** No signature is improved on the way,
no test is rewritten, no comment is reflowed. If something wants
fixing, it is noted and left for its own plan — a move commit that
also fixes things is a commit nobody can review.

**The two questions the move settles.** Where the source's driver
lives, and whether `poll_loop` stays a free function or becomes the
method of a type that owns the two horizons and two sides it already
takes. The second is the shape the last plan kept circling; it is not
required by the move, and it should be decided in the intro rather
than drifted into.

## Out of scope

- The debt this makes visible but does not fix: `is_ci_key` sniffing a
  key prefix to ask "is this WAL row the feed's or the checks'?" That
  belongs in the record, and it is a format change with its own
  migration question.
- Any behaviour, any new test, any renamed function.

## Tests

- The suites pass unchanged at every commit, and the test COUNT per
  module is accounted for: a move that loses a test is the failure
  this plan can actually have.
- `git diff --stat` for each commit shows equal insertions and
  deletions for the moved lines, allowing only the module declarations
  and `use` lines to differ.
