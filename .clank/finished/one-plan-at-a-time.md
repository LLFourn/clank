# one-plan-at-a-time
# One plan at a time, as an invariant the types carry

## Why

A repo has one plan in flight. That is how clank is used, it is what every workflow command
assumes, and it is what the review gate is built around — but nothing in the model says so.
The core rollup holds `plans: BTreeMap<PlanKey, PlanState>`, a commit's tag parses as
`TitlePrefix::Plans(Vec<String>)`, the classifier answers with a
`BTreeSet<PlanKey>`, and `MultiPlanOpen { in_progress, new_plans: Vec<PlanKey> }` exists to
cope with the case the invariant forbids. Every reader downstream — status, the TUI, the
remote UI — then carries a collection and a "which one?" question that has exactly one
answer in practice.

The cost shows up as noise rather than as bugs, which is why it has survived. In the TUI's
LOG the plan's name is re-emitted as a banner beside events that are already inside that
plan's run (see the `voice-spike` separator repeated between consecutive entries). The
renderer repeats it because the data model cannot tell it that everything between the intro
and the finalize belongs to one plan — it has a list, so it labels each item.

A plan's DURATION has the same shape of problem: it runs from that plan's intro to its
finalize, which is a single interval, not something to derive per event.

## The invariant

**At most one plan is active in a repo at a time.**

- A plan introduced while another is active is **flagged** — a warning the human sees — and
  **ignored by the core state rollup**. It does not become active, it does not displace the
  one in flight, and nothing downstream has to decide between them.
- The active plan ends at its finalize, which is also when its duration is known.

The second clause is what makes this a simplification rather than a new check: the rollup
has ONE active plan or none, so every structure below can say that.

## Simplify exhaustively — this is the deliverable

The invariant is the easy part. The work is finding everywhere that currently models more
than one, and the plan is not done while a collection of plans survives anywhere a single
one would do. Sweep the CLASS, not the names listed here — these are a starting inventory,
found in one pass, and the acceptance below requires the sweep rather than this list.

**Core rollup** (`crates/core/src/repo_state.rs`)
- `RepoState.plans: BTreeMap<PlanKey, PlanState>` → one optional active plan.
- `TitlePrefix::Plans(Vec<String>)` → one plan name. **A commit can no longer tag multiple
  plans; remove that.** `[a,b]` stops being two tags.
- `ClassifierOutput.plan_attribution: BTreeSet<PlanKey>` → `Option<PlanKey>`.
- `PlanTimelineEvent`, `FinishedPlan`, `PlanTouchInput`, `apply_commit`'s union of tag and
  touches.
- `ad_hoc: Vec<AdHocEvent>` — "cleared on the next plan intro" is a lifecycle statement that
  may become simpler once intros are serialised.

**Wait / work state** (`crates/core/src/wait.rs`)
- `MultiPlanOpen { in_progress, new_plans: Vec<PlanKey> }` and
  `WaitItem::MultiplePlansOpen` — this is the soft-disallow the invariant replaces. What
  remains is a warning that ONE plan was introduced while another was active, which is a
  different and smaller thing.
- `PlanWorkState` and everywhere it is held in a `Vec`.
- `new_plans: Vec<PlanKey>` on the wake inputs.

**CLI** — `cli/status.rs` (`plans: Vec<PlanWorkState>`), `cli/mod.rs` (`plans: Vec<String>`),
`cli/wait.rs` (`new_plans: Vec<&str>`), and the commands that take a plan argument because
there might be several.

**TUI** (`cli/status_tui/`) — `render.rs` and `fixtures.rs`. The repeated banner is the
visible bug: show the plan once for the run of events that belong to it, not beside each
one. Showing WHICH plan an event happened during stays; repeating the name does not.

**Remote / API** (`crates/core/src/api.rs`, `plan_view.rs`, the web surfaces) — `PlanEvent`
carries `plan_id` per event, which is the same repetition one level out. Decide whether the
stream still needs it once a session has one plan.

## Acceptance

- The core rollup cannot represent two active plans. Not "rejects" — cannot represent:
  a second plan arriving while one is active is a WARNING plus a no-op, driven by a test
  that applies an intro on top of an active plan and asserts the active one is untouched.
- A commit tags at most one plan, and the multi-tag path is gone rather than deprecated.
  A `[a,b]` subject is tested for whatever it now means, and no test asserts the removed
  feature is absent.
- A plan's duration is one interval, intro to finalize, computed in one place.
- The TUI's log does not repeat a plan's name between consecutive events of that plan.
  Driven by a render test on the sequence from the screenshot, not by eye.
- **The sweep is reported, not assumed.** Name the greps run (the CLASS: `plans`,
  `Vec<Plan`, `BTreeSet<PlanKey>`, `plan_id`, `MultiPlan`, `new_plans`), and for each
  surviving collection say why one plan is not enough there. A zero count is not a report.
- Every `Vec`/map of plans that remains is justified in the plan text. `finished_plans` is
  the obvious legitimate one — history is many — and the burden is on the rest.

## Out of scope

- The queue. Many plans may be QUEUED; the invariant is about active ones.
- Changing what a plan is, how it is reviewed, or the gate.
- Multi-repo. One plan per repo, not one plan per machine.

## Watch out

- `finished_plans` is a `Vec` for a good reason and a `PlanKey` may repeat in it (a plan can
  be re-introduced and re-finalized). Do not collapse history.
- The hard-forget path (`TouchKind::Delete`) removes a plan from state entirely; check what
  "active" means across it.
- This is a breaking change to the on-disk state cache and to anything persisted from these
  shapes. Migrate the live repos with the freshly BUILT binary before `cargo install`, per
  the usual rule — and fixture the OLD bytes rather than asserting no old state exists.

## Promotion assessment

Ready against the current code. The Grok integration has finalized and there
are no other active plan files. `RepoState` still holds active plans in a
`BTreeMap`; classification and wait projections still carry plan sets/vectors,
and the TUI still branches on multiple active plans. This is a coherent model
simplification whose completion requires the full downstream sweep, rather than
an isolated guard in queue promotion.

Include the mutation entry points in that sweep. `queue::promote_entry` currently
protects only a destination name, while stash restoration protects only a
same-name collision. Promotion/restoration while another plan is active must
fail before consuming the queued entry, stash ref or record. Cover competing
introductions and the shared CLI/TUI paths. The core must independently warn and
ignore an overlapping introduction from Git history; mutation preflight cannot
substitute for the single optional active-plan type.

Define ignored-introduction follow-up semantics explicitly: later edits, tags,
finalize or delete touches for that ignored run must not displace the accepted
active plan, accidentally activate the ignored one after the accepted finalize,
or manufacture reviewer work for it. Test ordinary finalization, hard-forget,
and a subsequent genuine introduction, including reusing a previously finished
name. Choose and fixture the single-tag parser's treatment of comma/multiple
prefixes as part of the core contract; preserve historical commit text instead
of rewriting it to fit the new parser.

Implement in reviewable milestones: first the scalar core lifecycle and its
classification/warning/cache contracts; then all wait, command, status/TUI and
remote projections, including the log grouping and shared duration calculation;
then the exhaustive surviving-collection report and persisted-state migration
checks. History, queued drafts, cross-repo results and lookup indexes may remain
collections when their multiplicity is real; none may act as an alternative
container for simultaneous active plans. Preserve event/run identity where a
history or transport crosses plan boundaries.

Inventory persisted consumers before changing schemas. Fixture old bytes and
exercise migration/cache invalidation with the freshly built binary; preserve
Git history, queued/stashed content, bindings and unfinished work. Follow the
plan's live migration-before-install requirement only after the concrete
migration and recovery path is validated. This refactor must retain the existing
shared-monitor/event-driven resource contract: no polling loop, repeated full
history rebuild at idle, or new resident worker to enforce the invariant.

## Core milestone: contract and implementation

The canonical fold now has `active_plan: Option<ActivePlan>`, with that run's
key and `PlanState` together. `RepoState::plan(key)` is a lookup of this slot,
not another container. `finished_plans` remains an ordered history of accepted
finalizations, including repeated names. The daemon digest uses the new scalar
shape. Existing readers have been adapted to this shape so the workspace builds;
plural work/status/transport projections are the next milestone, not the final
model being claimed here.

The transition contract is:

- One intro into an empty slot accepts a new run, records its first timeline
  event, clears the ad-hoc bucket and establishes adoption when necessary.
  Only this accepted introduction clears ad-hoc debt; a revision, implementation
  or finalization does not start a new lifecycle.
- An intro while a slot is occupied rejects the entire commit before changing
  adoption, the active timeline, finished history, ad-hoc state or log output.
  Its only result is a scoped `RejectedPlanIntro` warning identifying the
  rejected name, source SHA and accepting plan. This atomic rule also covers a
  commit that combines an accepted-plan edit with the overlapping intro.
- Several intros in one commit with an empty slot are ambiguous. Reject the
  whole commit and warn for each name; filesystem/diff order never chooses the
  winner. A later single genuine introduction can accept one of them.
- A duplicate intro of the occupied name is also rejected atomically. A name
  identifies one plan file: this does not transfer that identity away from the
  accepted run. Its later revision/finalize/hard-forget still operate on that
  accepted identity.
- A different rejected name stays ignored after the accepting plan finishes or
  is hard-forgotten. Later edits, tagged code, finish or delete of that rejected
  run produce no timeline, finished record, ad-hoc review, adoption or log event.
  In particular, a rejected reuse of an old finished name cannot erase that old
  accepted history on its later deletion.
- Rejection provenance is carried by the warning itself; there is no shadow map
  of active/parked plans. These are authoritative fold facts, not an ackable UI
  inbox. A genuine intro of the rejected name into an empty slot retires that
  name's prior rejection warning and accepts a new run. Finishing/deleting the
  accepting plan does not clear a different rejected run's warning just because
  the diagnostic names it as context.
- Finalizing the accepted run empties the slot and archives its intro/finalize
  boundaries. A squashed finished plan encountered without an active run keeps
  the existing same-commit intro/finalize convention. Hard-forget removes the
  named accepted run/history and its own warnings. Deleting older finished
  history while a different run is active leaves the active timeline intact.

`TitlePrefix::Plan(String)` parses the first bracketed candidate without comma
splitting. `[a,b]` is the literal invalid candidate `a,b`, so it cannot attribute
code; HEAD reports that one invalid name. `[a] [b] work` names `a` and preserves
`[b] work` as the body. Empty/unclosed brackets preserve the original subject.
The classifier and `CommitNode.plan_attribution` carry `Option<PlanKey>` and can
attribute code only to the accepted slot. No historical subject is rewritten.
Git diff inputs may still describe several changed plan files: that is actual
input multiplicity, not several active runs.

The HEAD gate uses the fold's shared `ignores_commit` disposition. A rejected
intro commit or ignored-run follow-up cannot become a tag correction that
pauses the accepted plan's reviewers. Mixed non-intro changes retain only
nonignored touches for normal tag validation. Status JSON, human status and the
TUI project the same scoped warnings; warnings do not create reviewer assignments
or recurring stop-hook debt. The old multi-open projection is currently always
absent; its obsolete types/wire vocabulary and plural work projections will be
removed together in the next milestone.

### Cache and resource contract

Cache format is now v14. The fixture
`crates/cli/tests/it/fixtures/repo-state-v13.bin` was captured using the actual
v13 writer at the introduction commit, before editing the fold. Its payload
contains two active map entries, which a frozen v13 test schema decodes and
checks. This is not a new payload with a falsified old version number.

Old-version checkpoint filenames are ignored. Even if an old file is renamed
to a current filename, header validation rejects it before the new scalar
payload decoder runs. Existing corrupt-cache recovery removes that alias while
the original legacy file remains. The recovery path is to fold authoritative Git
history and write v14 checkpoints; it does not translate old simultaneous-active
maps, rewrite commits, consume queues/stashes or change bindings. New checkpoints
round-trip the accepted slot and rejection provenance, including ignored
follow-ups after the accepted run ends.

A freshly built CLI was exercised in an isolated temporary HOME/repo with the
old fixture: cold rebuild, stable warm reuse with no cache-byte/mtime churn,
ignored tagged implementation after accepted finalization, ignored deletion,
and a genuine later introduction all passed. Read operations preserved Git
HEAD and queue/local-metadata sentinels. The in-process integration also checks
cold/warm status agreement, warning visibility and v14 checkpoint creation from
old bytes. Live migration and installation remain for the completed-schema
milestone; neither has been done during this partial refactor.

The invariant is enforced in the existing commit fold, not by another worker.
The shared HEAD disposition is computed during ordinary projection. No polling,
idle timer, monitor subscription, resident process or repeated idle history
rebuild was added. Existing cache idempotence/checkpoint-resume/cost checks pass.

### Validation and remaining work

Passed: 257 core unit tests; focused cache, rebuild, active/history resolution,
disk-fold adapter, latest review-target, preview, status and TUI render tests;
the legacy-cache integration; Git/identity/signal/Zellij/path cost and ownership
boundaries, typed JSON-config writes and documentation-comment checks. The
freshly built CLI acceptance described above passed separately. No live agent,
provider configuration, Zellij tab or other repository was changed.

Next: make `WorkStatus`/status/command/transport projections scalar and remove
`MultiPlanOpen`, `MultiplePlansOpen`, and `new_plans`; reject queue promotion and
stash restoration before consuming their inputs while a slot is occupied,
including competing mutations through the shared CLI/TUI paths; simplify plan
resolution and commit-page attribution instead of retaining zero-or-one lists;
compute the intro-to-finalize interval once and finish the log render fixture.
Then report the exhaustive class sweep with every surviving collection
justified, validate the final persisted-consumer migration/recovery path, and
migrate live caches with the freshly built final binary before installation.

## Mutation admission milestone

Admission precedes the remaining projection cleanup: an empty-slot check by
itself races, even when the fold can represent only one accepted plan. All
participating commands now share one mutation lock for a canonical repo root.
The guard owns both the file descriptor and that root, so its occupancy check
cannot accidentally inspect another repo. It reads the cached canonical fold
after acquiring the lock and holds the lock through the complete Git operation.

### Command contracts

- Both name-addressed queue promotion and the entry-addressed primitive used
  by the TUI refuse an occupied slot before moving the queued body, creating
  the destination directory, staging files or committing an introduction.
  Blocking/review gate state does not exempt a plan from occupying the slot.
- Stash restore, including the unshelve alias and its checked entry primitive,
  refuses occupancy before replaying commits or consuming the protective ref
  and record. Existing stale-entry and same-file collision diagnostics remain.
- Plan-preserving pick requires an empty target slot for the entire replay,
  even when a source run ends with a finalize. A batch may contain several
  accepted runs in serial source-history order; that is historical input
  multiplicity, not several active plans. A rejected source run is refused
  before any member of the batch is replayed.
- Code-only pick (`--purge`) does not introduce a run and may preserve an
  occupied slot. It shares the mutation lock, retains the existing same-name
  and finished-history collision checks, and does not consume a source stash.
- Unfinish requires an empty slot before its existing clean-worktree and
  trivial-finish checks and reset. It cannot replace an accepted active run.
- Stash push, finalize (including its squash/reword paths), and purge hold the
  same lock while changing, emptying or rewriting the slot. Readers do not take
  it. Scaffolding, queue editing and stash drop do not claim the active slot.
  Stash drop still uses the existing pair lock to protect its ref and record.

The slot lock is acquired before the existing stash pair lock on push/pop.
No path takes those locks in the reverse order. The shared rewrite engine does
not acquire a second slot lock: its mutation-command caller owns the guard.
Git remains behind the typed Git layers; fixtures alone spawn raw Git.

### Ownership and cost

The lock inode lives in the existing fixed per-uid runtime namespace, keyed by
the canonical repo's raw path bytes, with a distinct `plan-mutation` suffix.
Symlink aliases converge on the same lock. It is outside the working tree and
does not create a stash directory just to promote a plan. The namespace uses
the existing owner/permission validation; opening the lock refuses symlinks.

Contenders park in a kernel `flock`; the only syscall retry is interruption by
a signal. There is no retry timer, process scan, monitor source, resident
worker, polling or idle history rebuild. Closing the owned file on return or
process exit releases the lock. Production never unlinks it: replacing a
locked inode would let two callers lock different files at the same name.
Temporary test repositories remove only their own inode after their callers
have returned; this does not add a production cleanup process.

This is an admission transaction among participating Clank commands, not a
transaction protocol for arbitrary Git writers. Old binaries, hand-authored
Git history and external Git commands remain covered by the canonical fold's
warning/ignore rule. Existing Git-conflict handling and rewrite safety checks
remain in force. The new preservation guarantee concerns occupied-slot and
competing-admission refusals; it does not claim new rollback semantics for an
unrelated filesystem/Git failure after an admitted mutation starts.

### Evidence and next step

Six admission integration tests pass: blocked-slot refusal through both
promotion surfaces preserves the body, HEAD and staged index; concurrent
promotions through canonical/symlink paths admit one and retain the loser;
restore and pick preserve a stash when another plan is active; promotion
versus restore admits exactly one and preserves the losing input; code-only
pick retains the active run and source stash; unfinish reopens an empty slot
and refuses an occupied one.

Also passed: 20 stash integration tests, 13 pick integration tests, 29 finish
integration tests; stash/queue/purge/finish unit tests (4/25/15/12); 13 boundary,
6 typed-JSON-config and 2 documentation-comment checks. Older pick fixtures now
use serial accepted history or verify refusal before partial replay instead of
expecting overlapping plans to be imported.

A freshly built binary was exercised by two independent CLI processes, one
through a symlink repo alias, with overlapping fixture-only Git commits. One
promotion succeeded, the other refused occupancy, the loser's queued body was
unchanged, and history contained exactly one introduction. Its isolated HOME,
repo and runtime inode were cleaned after both processes exited. No live
repository, agent binding, provider configuration or Zellij tab was changed;
the binary has not been installed.

Next is the scalar work/status/transport/UI sweep and removal of the obsolete
multi-open vocabulary, then the shared duration/log fixture, full collection
inventory and final persisted-consumer/live-cache migration. This milestone
completes mutation admission; it does not mark those remaining tasks complete.

## Scalar projections and transport milestone

`WorkStatus.active_plan` is now `Option<PlanWorkState>`. Derivation maps the
canonical accepted slot directly, with a blocked-plan early return before
reviewable-commit handling. There is no intermediate active-plan vector. The
HEAD correction implicates the one accepted plan through a boolean, rather
than constructing a set of implicated active plans. Work routing branches on
that optional plan once; standby behavior, block precedence, reviewer tiers,
ad-hoc work and independent PR reviews keep their existing contracts.

`StatusSnapshot.active_plan` is the same optional work projection. Explicit
plan selection filters that one value; watch mode may instead select the
matching latest finished run. Ordinary selection returns the latest finished
record only when it selects no active plan. `last_finished` was already an
option internally and is now also scalar on the wire. Human status, HTML,
TUI attention/bar/gate rendering, agent handover clocks and the remote ledger
read the same option. The multi-plan count bar and per-active-plan row loop
are gone. Queues, stashes, history and independently active PR reviews remain
collections for their respective reasons.

### Output and compatibility contracts

- `clank status -j` now emits `active_plan: object | null` and
  `last_finished: object | null`, replacing `plans` and `finished_plans` arrays.
  The active object's existing plan/gate/SHA/handover keys retain their names;
  only cardinality and the outer key change. These are output projections,
  not another persisted copy of the canonical fold.
- `clank open` inspection reports `active_plan: name | null`, replacing an
  active-plan count that could represent impossible values. Human inspection
  names that plan. No-argument plan resolution checks the one accepted run's
  visible file directly; it no longer builds a candidate vector or carries an
  unreachable ambiguity branch. Diagnostic candidates may still contain
  several finished names because they search history.
- Repo-unwatched runtime events and removal outcomes carry the optional active
  plan identity rather than a count. The public event round-trip covers both
  `None` and a real key; its representative wire shape is updated. Historical
  per-plan events still retain identities: they are not current-slot lists.
- A commit page holds one optional readable tagged plan. Subject parsing,
  active/finished page lookup and refresh return that option directly. Its
  one action keeps the existing `1` shortcut; numbered multi-plan rows and the
  tenth-row special case are removed. Page/back/scroll coverage now uses
  active and finished subjects separately, including removal of the sole
  linked plan while a page is open.

`MultiPlanOpen`, `WorkStatus.multi_plan_open`, `WaitItem::MultiplePlansOpen`,
its `WaitKind`, the wait JSON/human branch, stop-hook fields/hint/condition,
scroll branch and parameterized stash-swap remedies are removed. The accepted
plan continues through its ordinary gate; ignored Git introductions surface as
canonical warnings and never become a second corrective wake debt. There is
no compatibility alias or zero-or-one list left in these projections.

The monitor protocol is now **6**. An old monitor must not continue routing
the prior lifecycle rules simply because a surviving frame happens to decode.
Version negotiation still happens before body decoding, and the existing
validated cooperative retirement/lease handoff handles replacement. A version
number alone, wrong repo or invalid identity cannot retire a holder. Protocol
tests cover rejection in both directions and replacement of a proto-5 fake
holder by a proto-6 client. This adds no polling, new retry loop or monitor
source; the resource/cost boundaries remain unchanged. Cache format remains
v14 because this milestone changes derived projections, not its fold payload.

### Evidence and remaining work

Passed: 257 core tests, 17 wire round-trips and the core doctest; all 452 TUI
tests across the initial run and targeted retries; status 35, stop-hook 92,
wait 61, HTML 42, resolver 9 and open 4 unit tests; status/log integration 2,
stash integration 20, open integration 27 and admission integration 6; eight
monitor version/replacement checks; boundary 13, typed-JSON-config 6 and
documentation-comment 2 checks. The initial TUI/wait runs exposed sandbox
socket/process-identity restrictions; affected cases passed with fixture-only
permissions. The one actual fixture failure was repaired by ending its active
run before adding the next historical example. The elapsed-time fixture uses
a plan plus an independent PR review instead of two active plans.

A freshly built CLI passed an isolated HOME/repo acceptance: scalar active and
finished JSON, visible ignored-introduction warning, genuine old v13 fixture
recovery to v14, byte/mtime-stable warm checkpoints, ignored tagged follow-up
after finalization, unchanged Git HEAD during reads and preservation of queued
sentinel input. The first manual check looked in the wrong cache directory;
it was corrected to `.clank/cache/repo-state` and passed. No binary installation
or live repository/provider/binding migration was performed.

The implementation sweep found no surviving `Vec<PlanWorkState>`, multi-open
type/variant, or `new_plans` field in Rust sources. The next milestone still
owes the complete class inventory with every surviving collection justified,
the shared intro-to-finalize interval and log render fixture, then final
persisted-consumer validation and live cache migration before installation.

## Duration, log runs, and collection inventory milestone

### One recorded interval

`FinishedPlan.duration_secs` is the accepted introduction's Git author timestamp
subtracted from the accepted finalization's Git author timestamp. The canonical
fold computes it once when it empties the active slot and passes that exact value
to both the finished record and `LogEvent::PlanFinalized`. Implementations,
revisions, external events, reviews, and rejected introductions cannot restart or
extend that interval. A clock reversal yields zero. The arithmetic covers the
full i64 timestamp domain without overflow. A squashed finished plan retains the
existing same-commit introduction/finalization convention and therefore has a
zero interval: the rewritten history contains no earlier introduction boundary.
Hard-forget creates no finished record or duration. Reintroduced names retain
separate finished records and independently computed intervals.

Consumers only format this value: status's `last_finished` JSON object,
`clank log --json` finalization rows, human status, the TUI's idle done row and
log banner, the remote ledger, and HTML's status, run header and finished page.
They do not subtract timestamps or start clocks. Active plans have no finalized
duration; their banner continues to show their name. The common duration text
formatter accepts the recorded u64 interval independently of wall-clock age.

### One banner per run, including external rows and loaded pages

The duplicate banners came from flushing the Git row producer whenever a
command/GitHub/message event interrupted the timeline. The shared producer now
groups the bounded Git window once, then interleaves external rows in their
existing positions. Reviews remain above their own commit. External event
indexes, badges, unhandled state, baseline state and backend identity are
preserved. CLI oneline printing uses that same typed row producer as the TUI and
remote ledger, so there is one grouping rule.

Canonical introduction and close boundaries distinguish runs even when the
name repeats. The shared core umbrella rule preserves caller order, uses stream
position rather than author clocks, and leaves post-finalize/post-delete ad-hoc
work outside a closed run. Partial/historical projections with different plan
keys stay separate; no renderer reorders commits to merge names.

A bounded page that cannot yet reach the introduction emits a provisional
footer carrying its known duration. Appending an older page inspects only that
new page and the existing tail. When the new page continues the run, the footer
moves down and its duration follows it. An ad-hoc-only intervening page also
moves the footer rather than creating another one. Reaching the introduction
makes the footer permanent. A complete run followed by an older same-name run
keeps two banners; repeated names are not run identity. Pre-adoption plain Git
rows and collapsed single-commit historical runs retain their own boundaries.
The current event-window/paging policy is unchanged: initial reads carry
external events, older Git-only page reads do not duplicate them.

The positive `voice-spike` render fixture folds introduction, implementation,
implementation and finalization at known timestamps, interleaves a voice command
and a review, and asserts the complete visible sequence at two terminal widths.
There is one `voice-spike · 3m` banner with the event and review still in order.
The pure page-join fixture covers a partial finish, implementation, ad-hoc-only
page, introduction, duration preservation, and a later separate same-name run.
The core fixture covers repeated names, equal/reversed clocks, ignored overlaps,
squashed boundaries, and matching recorded/log durations. HTML has a real Git
fixture showing that same recorded three-minute interval in its page and
repo-wide timeline.

### Final scalar leftovers and wire contracts

The class sweep found two more zero-or-one containers. `HeadTagViolation` now
holds `unknown: Option<String>` and `extra_named: Option<PlanKey>`. The parsed
candidate is optional throughout the validator; there is no intermediate tag
set. Status and wait JSON expose those two fields as a string or null. The
`untagged_touched` array remains because an actual Git diff can touch several
plan files. Existing unknown-tag, tag/file mismatch, multi-file input and HEAD
correction tests exercise those cases with the single-candidate parser.

Roster gate preservation now accepts `Option<&mut GateTarget>`, and its target
has one `touched_plan: bool`. The former map of commit contexts, vector of targets,
vector of milestone flags and nested baseline vectors could only describe one
accepted plan. They are gone. The planner preserves the same review gate while
adding each entrant, retains real verdicts, and deliberately reopens the gate
for a swap. Its stand-ins remain a vector because several reviewers can enter.
The unused daemon `active_selections` map had no reader/writer and is removed.
Scalar-slot readers for feedback, rewire, pick and digest use explicit optional
access rather than loop-shaped selection.

Name-addressed finished operations use the existing canonical
`finished_record` selection, including pick's native span and squash message and
the TUI's finished-range check. This preserves the existing first-record rewrite
convention; the full history remains a collection and timeline grouping uses
individual intro/close boundaries rather than choosing a single name bucket.

Monitor protocol is now **7** because a v6 peer would interpret tag diagnostic
arrays using the previous wire shape. Header negotiation refuses incompatible
frames before decoding their bodies, and the existing authenticated cooperative
replacement path remains unchanged. The HTML builder version is **6**: an old
page must be regenerated once to acquire run boundaries and recorded duration;
incremental builds then retain their normal skip behavior.

### Report of the class sweep

Executed these searches across all Rust crates, including integration fixtures:

```sh
rg -n '\bplans\b' crates --glob '*.rs'
rg -n 'Vec<Plan|BTreeSet<PlanKey>|plan_id|MultiPlan|MultiplePlansOpen|new_plans' crates --glob '*.rs'
rg -n 'Vec<[^>]*Plan|BTree(Map|Set)<[^>]*Plan|Hash(Map|Set)<[^>]*Plan|plans:' crates --glob '*.rs'
rg -n 'Vec<|BTreeMap<|BTreeSet<|HashMap<|HashSet<' crates/{core,cli,web,web-cloudflare}/src --glob '*.rs'
rg -n 'MultiPlan|MultiplePlansOpen|new_plans|"plans"|plan_count' crates --glob '*.{json,ts,tsx,js}'
```

The broad collection search also checked indirect containers whose element
names were not `Plan` (notably roster `GateTarget` and milestone contexts).
These are the surviving plan-related collections and why one value is insufficient:

- `RepoState.finished_plans` and the frozen v13/v14 fixture's finished records:
  accepted history is many, including repeated names. `PlanState.commits` is
  many events of one run, not many active plans. The log's `StandsFor` map is
  keyed by finalization SHA, so several independent historical single-commit
  runs retain their own identity and collapse eligibility.
- Core `CommitEvent.plan_touches`, `CommitNode.touches`; CLI
  `CommitChanges.plan_touches`, `disk_snapshot`'s converted touch inputs;
  `git_io::apply_diff_records`'s `plans_finished_added`: physical Git input can
  touch/add/delete/rename several files in one commit, including historical or
  rejected files. These describe input and support atomic ambiguity rejection;
  they do not store several accepted timelines.
- `HeadCommit.touched`, the validator's `known_plans`/derived `known`, and
  `HeadTagViolation.untagged_touched`: actual diff facts and missing-name
  diagnostics can be many. The known set is the scalar slot plus physical
  touched identities, allowing an intro/finalize/delete to name its own file.
  Parsed names, unknown candidates and extra attribution are now optional.
- `PickArgs.plans`, `pick::keys`, `natives_by_plan`, `spans`, `landings`,
  `plans_sorted`, its picked-name set and union of native SHAs: one command can
  import several accepted historical runs in serial source order. Source spans,
  dependency checks and per-run replay/squash messages need every requested run.
  Admission still requires an empty slot for plan-preserving replay; these are
  batch inputs, not a target's active-plan list. Pick integration and admission
  fixtures exercise serial batches and preserve the losing/blocked inputs.
- `PurgeAllPreviewResponse.plans_touched` and `preview::plans_seen`: a purge-all
  history walk must identify every stem whose artifacts appear in that range,
  including finished/ignored historical files. `native_shas` are the many
  commits of one selected run. Rewrite `Vec<PlanStep>` is a manifest of commit
  operations, not a collection of plan lifecycles.
- HTML `collect_plan_buckets` and `plan_writes`: one generated site contains the
  accepted active page and many finished pages. These are page-name/history
  indexes, not work-state containers. The actual timeline keeps separate run
  boundaries even when names repeat. Four older integration fixtures were
  updated from overlapping introductions to successive finished/active runs.
- Wait `ObserverBaseline.finalized: BTreeSet<(PlanKey, CommitSha)>`: history notice
  deduplication spans many distinct finalizations, keyed by instance SHA. It is
  not an active-plan tracker or a source of new multi-open work.
- Queue and stash entries, drafts, inventory and restore/pick candidates remain
  many: there may be several independent queued or shelved runs. Admission is
  checked before any can occupy the single active slot. Directory listings of
  `.clank/plans` remain raw filesystem facts: ignored/uncommitted files can
  coexist on disk without becoming accepted plans.
- `Clank.repos`, repo registries, multi-repo TUI/site indexes and web session
  selectors remain many repos/sessions. Review maps and pending-block vectors
  remain many reviewers/questions for one selected run. `PlanStateLookup`'s
  test `MockBlocks` maps and `pset` helpers fixture those scoped input facts;
  they never replace `RepoState.active_plan` with a map.
- `PlanAction` vectors are actions for one page. Roster stand-ins are reviewers
  for one gate. Rows, history/review events, warnings and timeline sections are
  actual many-item history or diagnostics, including several rejected intros.
  Core `ad_hoc` remains many post-adoption commits, cleared only by an accepted
  intro; ignored intros never clear this review history.
- `state_cache`'s `V13Fold.plans` is the deliberately frozen, test-only decoder
  of genuine pre-migration bytes containing two old active entries. Keeping
  that map is necessary to prove the old fixture is real. Production cannot
  deserialize it as a current fold; v13/v14 filenames and headers are rejected.

The remaining `plans` text outside these categories names filesystem paths,
historical/queued batches, help/error prose, or fixture setup. There are no
current active-plan collections in core work/status projections, CLI no-argument
resolution, TUI/remote pages, monitor work frames or open inspection.
`MultiPlan`, `MultiplePlansOpen` and `new_plans` have no code/wire consumers.
Seven unconsumed legacy schema files for removed list/context/wait/ambiguous
APIs still advertised plural active plans or plan counts; those obsolete files
are removed rather than presented as current schemas.

`plan_id` remains scalar. Finish/rewrite previews can address finished runs, and
runtime/live-event streams can contain events from several repos, sessions and
successive runs after the active slot has changed. They must carry the event's
original repo/stem identity rather than infer it from whichever plan is active
at read time. Identity parsers/fixtures and typed `PlanEvent` keep that field;
there is no per-session collection of simultaneously active plan identities.

### Persistence, recovery and cost evidence

Cache format is now **v15**, and the state digest namespace is **v5**. A finished
record's intro identity, finalization identity and duration enter its digest.
The new fixture `crates/cli/tests/it/fixtures/repo-state-v14.bin` was captured
with the actual e5d34f7 v14 writer before duration edits: 322 bytes, SHA-256
`9e82c912ecee98d3697c65e6c31a71643e01dc9bbd7963c9963440d6da57db73`.
It encodes finished `first` without a duration field and active `next`. A frozen
v14 schema decodes that payload and verifies both records. The v13 fixture's
finished-record type is also frozen, so future fields cannot silently alter its
historical decoder. Neither fixture was manufactured with a current writer and
an old version label.

Both legacy filename skip and renamed-file version rejection happen before
current payload decoding. The authoritative recovery remains a Git fold, not a
translation of stale cached active maps or invented durations. A real Git
integration loads those genuine v14 bytes, refolds `first`'s 120-second interval
and accepted `next`, then verifies exact warm fold agreement and checkpoint
byte/mtime stability. The old file, HEAD, queue body and local roster metadata
are unchanged. The existing v13 overlap recovery now verifies v15 creation.

A freshly built CLI passed the same isolated v14-to-v15 acceptance independently:
scalar active `next`, JSON finalization duration 120, one `first · 2m` banner,
identical warm checkpoint bytes/mtime, preserved old bytes and untouched
HEAD/queue/local config. No live cache has yet been migrated or binary installed.

Duration computation is constant work at an existing finalization transition.
Grouping is one linear pass over the bounded requested Git window; page joining
inspects only the newly appended page and current tail. No timer, polling loop,
subscription, resident worker or additional idle rebuild was added. Mutation
admission keeps the prior canonical-repo kernel lock contract. Version bumps
cause one ordinary recovery/build when that surface is next read, after which
existing checkpoint and incremental-page reuse resumes.

### Validation and remaining step

Passed: 259 core unit tests and 17 wire round-trips; 20 log tests; 9 cache tests; 13 rebuild tests;
35 status tests; 153 TUI render, 12 scroll and 12 derive tests; 5 focused ledger
and log-view tests; 70 agent/roster tests; feedback/rewire tests; 61 wait tests;
42 HTML unit tests; 44 HTML integration tests; 3 status/cache integration tests;
13 pick integration tests; focused protocol negotiation checks; typed JSON,
Git/identity/signal/Zellij/path/web boundaries, command/README tests and orphan
comment checks. The wait tests and two monitor socket tests that hit sandbox
socket/process-start restrictions were retried individually in isolated test
fixtures with the required access and passed. Baseline unrelated warnings remain.

Remaining: reviewer validation of this final schema and complete inventory, then
migrate the live registered repos' recoverable caches using the freshly built
binary before installation, as required above. Preserve Git HEAD/history,
queue/stash content, bindings and unfinished work while doing that acceptance.
Finalization still requires the explicit FINISHED gate; this is a CONTINUE
implementation milestone, not a self-issued final verdict.

## Live rollout acceptance milestone

Grok's CONTINUE review of 9302d117 accepted the shared duration, log grouping,
scalar projections, inventory, cache v15 and monitor protocol 7. This milestone
performs the remaining migration-before-install requirement. The only source
cleanup is removing the unused `PlanKey` import left after deleting the daemon's
obsolete selection map; it changes no fold, cache, protocol or runtime behavior.

### Scope and migration procedure

There is no persistent global repository registry to enumerate. A shallow
inventory of `~/src` Clank roots, followed by each Git repository's registered
worktree list, found 246 extant Clank directories. Of these, **78** had local
roster configuration: **31 standalone repos and 47 linked worktrees**. Those
configured roots define this live rollout, including `clank`, `life`, `safepay`
and `safepay/.clank/worktrees/changeless-hub-research`. Runtime descriptors found
no additional configured roots outside that inventory. Unconfigured historical
skeletons and temporary test directories were not assigned agents, initialized,
or scanned through a migration fold.

The freshly built binary was exercised sequentially against every configured
root before installation. The operation used the normal cached authoritative
rebuild reached by this bounded, plan-scoped CLI read:

```sh
target/debug/clank log --repo "$repo" \
  --plan clank-cache-migration-probe -n 1 --json
```

The probe name was checked to have no output events. Both `no log events` and
an empty JSON array are valid empty results: pre-adoption plain history takes
the latter print path. The acceptance harness initially distinguished these
incorrectly, stopped safely, then resumed only unverified roots after the fix.
The migration itself made no plan or Git change. A plan filter is deliberate:
unscoped log/status includes the external timeline reader, which can perform its
existing legacy-message migration. This rollout used only the cache rebuild
path and did not migrate, consume or acknowledge inbox events.

Each root was checked before and after its cold/warm reads:

- Git HEAD, all refs (including stash refs) and the Git index checksum were
  identical. Queue, stash records/bodies, roster and binding configs, inboxes,
  event records, plan/finished bodies and other non-derived Clank files were
  fingerprinted and unchanged. Feedback was checked by file metadata rather
  than reading reviews outside the Clank feedback command.
- The v15 checkpoint at that root's current HEAD had the correct magic,
  version and HEAD envelope. The real CLI's warm read exercised current typed
  decoding and did not change v15 checkpoint names, bytes or modification times.
- All **78** roots passed. They held **688** v15 checkpoints after normal spaced
  checkpoint creation/pruning, and **16,280** protected file records were checked.
  Legacy generations encountered included v6, v9, v10, v11, v12 and v13.
  Existing checkpoint pruning applies only to derived cache files; authoritative
  Git history and local workflow content were not rewritten or translated.

The run was sequential and one-off, with approximately 100 seconds spent in
per-root acceptance checks. No waits were armed, command sources launched,
monitors started, tabs opened, periodic migration job added, or recurring history
rebuild introduced. Once v15 exists, ordinary warm cache use resumes.

### Typed readback and installed-build acceptance

A temporary verifier linked against the freshly built Clank library loaded each
HEAD checkpoint through `state_cache::try_load`. It inspected the real optional
active slot and checked finalization intervals against Git author timestamps
through the typed Git layer. This did not invoke external event/inbox readers.

- The **seven accepted active plans** match the committed `.clank/plans` files
  across all 78 roots, with **zero mismatches** and at most one raw plan file per
  configured root. Unfinished files and their canonical accepted identities
  survived the rollout.
- Every one of **1,167 finished records** matches its accepted intro/finalize Git
  interval, including squashed zero intervals and clock-reversal clamping.
- The fold retains **34 scoped historical introduction-rejection warnings**.
  These are the new invariant's explicit history diagnostics, not extra active
  plans, reviewer assignments or inbox acknowledgements. They do not displace
  any of the seven current accepted plans. Their underlying Git/files remain.

Only after whole-inventory migration passed was the reviewed release installed
with `cargo install --path crates/cli --locked --offline`. Both `clank` and its
required sibling `clank-web-cloudflare` were replaced. Prior executable copies
were retained under a private temporary rollback directory. The import cleanup
was then compiled and installed through the same command; the serialized/runtime
schema is unchanged and the build is back to its seven unrelated baseline
warnings.

Final acceptance compares each installed binary's SHA-256 with its corresponding
release artifact, checks six global Clank/provider/hook config fingerprints,
and exercises the installed CLI on `clank`, `life`, `safepay` and the research
fork. Those warm reads leave checkpoint bytes, filenames and mtimes unchanged.
No `setup`, roster mutation, session rebinding, source reply/ack, or hook rewrite
is part of installation. Existing monitors negotiate the new protocol through
the already tested cooperative upgrade path on their normal next client attach;
this rollout did not force-kill or manually replace them.

Final release binary hashes and acceptance completion are recorded below after
the cleanup build's installed-artifact comparison. Full local acceptance data
is available in the private temporary migration and post-install reports; no
review body, inbox content or provider credentials are included in this plan.

All implementation and rollout acceptance obligations are now addressed. The
master will still yield this milestone for review and finalize only if Clank
hands it an explicit FINISHED gate item.

Final installed-artifact and warm-read acceptance **passed** after the cleanup
build. The release executables are byte-identical before/after that unused-import
change, confirming it has no executable or schema effect:

- `clank` SHA-256:
  `dda576836159573f65b92169aa717938011adb246445ab97c3ea2fdf2c4d4181`.
- `clank-web-cloudflare` SHA-256:
  `7f7ef88538421dc4bd209a2277502bbc15caeb4cfe80f90304ebba86829b32dc`.
- Full local migration report:
  `/private/tmp/clank-one-plan-live-migration-report.json`, SHA-256
  `dd5f79ed1b08ebde283951e2edbe8a4fd2fb6e705a81acdfaea7919636b4feeb`.
- Final local installed-build report:
  `/private/tmp/clank-one-plan-post-install-report.json`, SHA-256
  `4e09a2390f842641a10105e663102eb49c03ed42f1fcaa951c4c65f28dc15d9e`.

The final readback again verified all 78 roots, seven active identities, 1,167
finished intervals, zero committed-file mismatches, six global configuration
fingerprints, and the four installed warm-reader smoke checks. No migration or
installation work remains; explicit review-gate authorization to finalize is
the remaining workflow step.
