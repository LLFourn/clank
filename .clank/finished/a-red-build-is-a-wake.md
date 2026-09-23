# a-red-build-is-a-wake

CI passing or failing wakes an agent the way every other GitHub fact
does.

> "we need a github event for ci success/failure optionally scoped to a
> specific list of PRs. Right now you have to make your own command
> tool." — lloyd

## Why

`WaitEventSource::Github` already carries the vocabulary an agent
reacts to — `pr_opened`, `pr_updated`, `pr_merged`, `pr_comment`,
`issue_*`, `branch_push` — with a repo, a poll cadence, an
own-actions rule, a `branches` scope and an operator `prompt`. The one
fact a controller actually blocks on is missing: did the build go
green.

So every repo that needs it writes a `CommandSource` that shells out to
`gh` in a loop. That command is outside everything the github path
gives: no WAL, no at-least-once replay across a crash, no
`clank events list` / `ack` lifecycle, no dedupe, and no `prompt` to
say how to react when it fires. It is
the one wake source each repo re-implements and each one gets the
retry semantics wrong.

## The build

**Two kinds, because there are two reactions.** `ci_passed` and
`ci_failed` join `GithubEventKind` — an agent that only wants to be
woken by red says so, and the tagged-sum rule holds (an old binary
fails loud on the new kind rather than dropping it).

**Scoped like `branches` is.** A `prs: Vec<u64>` on `GithubSource`,
empty meaning every PR the watch sees, documented as applying to the CI
kinds the way `branches` applies to `branch_push`.

**Settle where the fact comes from before building on it.** The events
feed the poller reads does not carry every check-related event — check
runs and suites are webhook-only, and `StatusEvent`'s presence in that
feed has to be confirmed against a real repo, not assumed. Whatever the
answer, the poll path can always read the conclusion directly (the
check rollup for a PR's head sha), and the `realtime` relay already
receives what the webhook sends. The plan's first milestone is to
establish which source each delivery mode uses and write it down; the
wake surface is the same either way.

### What M1 settled

**A PR's result is a reduction, never one delivery.** A check suite
summarizes the runs of ONE GitHub App and a repo's checks routinely
come from several, plus the older commit-status API — so no suite, run
or webhook body is evidence about the PR. Both delivery modes
normalize what they fetched into the same `CheckReport`s and reduce
them with the same function (`cli/github_ci.rs`); a webhook is a
TRIGGER to re-read the PR's head, not a conclusion, so a realtime
watch and a polling watch cannot disagree. That is why the relay
subscribes to `check_suite` and `status` and not to the per-job
`check_run`: the suite completing is the cue, the rollup read is the
truth.

**Pending, and no checks at all, are different and neither wakes.** A
repo with no CI is not a build that has not finished. One outstanding
check holds the whole result pending — except that failure dominates:
a red check alongside a running one is already a build to act on, and
waiting for the stragglers only delays the wake.

**Every terminal state that is not a success is `ci_failed`.** That
includes `cancelled`, `timed_out`, `action_required` and `stale` —
GitHub's own merge box shows those red, and a wake that fired only on
`failure` would call such a PR green. `neutral` and `skipped` did not
fail. Anything unrecognized is pending: never wake on a state this
binary does not understand.

**A conclusion is not a question about which head it belongs to.** The
reduction classifies conclusions and nothing else; which runs report
on the head is settled by asking GitHub for THAT commit's checks, so
everything the reader hands it belongs there. Mixing the two is how a
result that does belong gets dropped for looking superseded — `stale`
means GitHub marked a check that took too long, not a run about an
older commit, and reading it the other way loses a failed build's wake
and can announce green on a check that never succeeded.

**The PR scope is part of the source identity, and only when it is
set.** `prs` joins `source_key` like `events` and `branches` — a
narrowed scope is a different presentable set, so it gets its own WAL
rather than a cursor baselined for another one. But the segment is
appended only when the list is non-empty, so every watch that predates
this plan keeps its key byte for byte: re-keying leaves the old log's
rows listed as unhandled with nothing left to re-present them, orphans
the prompt sidecar, and — while an old and a new config are both armed
— points the ingest lease at a different file, so two loops ingest the
same repo into two logs. A pinned test holds the pre-`prs` key.

**A kind that cannot wake you does not arm.** The vocabulary lands in
M1 and the reader in M2, so between them a config naming `ci_passed`
would arm a watch that silently never fires — worse than the loud
failure an unknown kind already gets. Arming refuses such a source by
name until the reader exists; the refusal and its test are deleted in
M2.

**Two consequences to accept, written down rather than discovered.**
Identity `(pr, head sha, conclusion)` suppresses a repeated identical
conclusion on an unchanged head even when an opposite result came
between them — a re-run that goes red, green, red wakes twice, not
three times. And cold start differs from the feed's: the feed silently
baselines the events already in it, whereas reading a PR's checks
observes present state, so the first poll of a watch records each
in-scope PR's current conclusion as seen without presenting it. Both
are M2's to implement against these words.

**A conclusion fires once.** Identity is `(pr, head sha, conclusion)`,
appended to the per-agent WAL before it is presented, exactly like a
feed event — a re-poll of an unchanged rollup is an `obs`, not a
second wake. A new push to the PR is a new head sha and so a new
conclusion to wake on.

**The item says what broke.** The wake carries the PR number, the
conclusion, and the jobs that failed — enough to start work without a
round trip, the way a `pr_comment` item carries its body.

**The docs land with the reader, and they correct what this falsifies.**
Until the reader exists the kinds are refused at arm time, so nothing
agent-facing advertises them — a shipped skill that names a kind which
errors is worse than one that omits it. When the reader lands, three
surfaces change together, and two of them hold sentences this source
makes untrue:

- `README.md`'s wake-source list (the kinds, `prs`, and that the
  own-actions filter does not apply to a conclusion, which has no
  actor).
- `setup_assets/skill_master.md`'s `wait_events` entry — the same
  kinds and scope, in the agent's words.
- `setup_assets/github_events_skill.md`, the `clank-github` skill:
  its catch-up section says the first poll "reconciles GitHub's event
  feed against the inbox and presents what you missed", and the README
  repeats it. **That is not true of a state read.** A rollup read sees
  only the CURRENT conclusion: a build that went red and was replaced
  by a green one while no wait was armed is not recoverable, and the
  skill has to say so rather than promise a catch-up it cannot do.
  Its "editing a source's config starts a fresh log" line needs `prs`
  added — with the exception that an empty list deliberately does not
  re-key.

### What M2 inherits

Read out of the existing pipeline while settling the above, so M2 does
not rediscover it:

- **The action key is mandatory here, not optional.** A rollup read has
  no feed id, and a WAL row with neither identity is grouped by a hash
  of its content — two identical conclusions would collapse into one
  indistinguishable row. `(pr, head, conclusion)` must be minted by
  BOTH the poll read and the webhook-triggered re-read, which is what
  makes the existing coordinator dedupe them for free.
- **The reconcile trigger has an exact site.** `check_suite` and
  `status` deliveries already arrive with `delivery: realtime`, and the
  relay drain drops them because the webhook classifier returns `None`
  for names it does not know. That `else` is where "a suite finished →
  re-read this PR's checks" goes.
- **Reuse `Transport::Poll`.** A third transport variant makes older
  binaries read those lines as foreign records — preserved, counted,
  never presented.
- **Do not write an observation per PR per tick.** The log compacts on
  volume; only a CHANGED rollup is an observation worth recording.
- **The own-actions gate is about actors, and a conclusion has none.**
  Settled in M2b: the CI read runs BEFORE that gate, so a login outage
  cannot hold a red build silent for a filter that has nothing to
  filter.

## Tests

- A rollup that goes red presents one item, and polling it again
  presents nothing; a new head sha on the same PR presents again.
- `prs: [7]` wakes on 7 and not on 8; empty wakes on both.
- An agent listing only `ci_failed` is not woken by green.
- A config naming an unknown kind still fails loud (the existing
  contract, unbroken).
- The three doc surfaces name the kinds and the scope, and the two
  catch-up claims say what a state read can actually do. A reviewer
  should be able to configure a CI watch from the README alone.

## Out of scope

- Reading logs or diagnosing a failure — the agent does that with `gh`
  once it is awake.
- Re-running a failed job.
- Non-GitHub CI.
