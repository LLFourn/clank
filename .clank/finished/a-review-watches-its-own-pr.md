# a-review-watches-its-own-pr

Opening a PR review turns on a watch for that PR, and closing it turns
the watch off.

> "comments and commit updates to the PR are important. But I thought
> that already worked." — lloyd

He was right that it works. `wait_events` takes a github source, and
since `a-watch-sees-only-its-prs` its `prs` list scopes `pr_comment`,
`pr_updated` and `pr_merged`. Point one at PR 42 and master is woken
for new comments and pushes on it today, through the WAL, the dedup
and the offline catch-up that already exist.

What does not work is that you have to write it yourself, for each PR,
and remember to take it out again.

## Why

`clank pr-review start 42` already says which PR master is reviewing.
Everything the watch needs is in that sentence, so asking the operator
to say it a second time in `config.json` is asking them to keep two
statements of one fact in step — and the one that rots is the config,
because nothing deletes it when the review ends.

## The build

**`start` writes the watch; `finish` and `abort` take it away.** An
ordinary `wait_events` entry in master's agent config — `pr_comment`,
`pr_updated`, `pr_merged`, scoped `prs: [n]` — written for you instead
of by you. Everything downstream treats it as what it is, one you
could have typed yourself, so its WAL, its baseline, its prompt, its
acking and its cleanup are the behaviour that already exists rather
than a second set of rules.

**It carries its own instruction.** `GithubSource.prompt` is stamped
onto every item a watch presents, and a config watch's prompt lives in
the config, so `clank events show` recovers it with no extra
machinery. That is the whole of "just be aware".

**`include_own_actions` is on.** Every agent shares the human's `gh`
identity, so the default hides exactly the comments this exists to
surface. Master seeing its own published comments is the price, and
the prompt says so.

**It never takes what it did not give.** An equal watch already in the
config was written by hand; `start` leaves it alone and records that
it added nothing, so ending the review does not delete somebody's own
watch.

## Why not a watch derived from the review

I built that first, and it was a boundary clank cannot hold. A source
spawned from the review still has an ordinary source's lifetime and
persistence, so owning it means constraining what it may write and
deliver from outside — a timer that races `finish`, a Drop guard so
cleanup survives an abort, a sweep of every agent's event directory in
case master changed (codex 4af209b found each of these). A config
entry has none of THOSE problems, because the config is the lifetime.

## The window it does not close

A comment or push between `pr-review start` and master's first PARK is
never woken for, and this plan does not fix that.

The watch is armed when the wait parks, its key is new, and a feed
watch's first successful fetch is a silent baseline —
`poll_loop_baselines_then_emits_new_events` pins exactly that: tick one
emits nothing even when the feed carries a new event. So whatever
happened while master was drafting is kept as history that was born
handled.

That is the FEED's behaviour for any watch however it was written, not
something deriving or configuring changes, and it is what you would
already get from a `prs:[n]` watch you added by hand. It is called out
rather than fixed because fixing it means a real fetch at `start` —
GitHub I/O in a command that does none, a new way for `start` to fail,
and a watch that baselines differently from every other watch in the
repo.

The draft half DOES anchor at start (`DraftHorizon::started`), because
a review begins from no draft and that is knowable without asking
GitHub anything. The feed's baseline is not.

The watch's own prompt says this, so master is not left to infer it.

## Out of scope

- **Edits and deletions.** Not worth catching, per the ask. This
  matters more than it sounds: the events feed reports only CREATED
  comments — an edit produces no row at all and a delete retracts the
  create, measured against a real PR — so catching edits means
  abandoning the feed for snapshot reads of five collections. That is
  what `the-published-half-of-a-reviewed-pr` became, and it is stashed
  at `refs/clank/stash/the-published-half-of-a-reviewed-pr` if the
  requirement ever comes back.
- **CI.** `a-red-build-is-a-wake` already delivers it and an operator
  watch can scope it to a PR. Adding it here would make the derived
  watch a second place CI policy lives.
- The pending DRAFT, which `the-review-session-watches-its-pr` already
  watches. This is the published side only.

## Tests

- Starting a review arms the watch in master's config; `finish` and
  `abort` disarm it.
- A new comment and a push on the reviewed PR both present; activity
  on another PR does not.
- A watch you wrote yourself with identical filters is not
  duplicated, and survives the review that found it there.
- The human's own comment presents, on the shared `gh` identity.
- A reviewer is not given the watch: it is master's.
