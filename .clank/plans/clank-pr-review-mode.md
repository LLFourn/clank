# clank-pr-review-mode

Run the clank multi-agent review loop against a GitHub PR. Master
drafts a set of inline review comments; reviewers iterate on them
until the team agrees; the agreed set is submitted to the PR as
one published review for the user to watch.

## Core architecture

The review scratch IS a GitHub **pending review** — there is no
local comment store, no hidden ref, no cycles-as-commits. Because
all clank agents authenticate as the **same `gh` identity**, they
all operate inside the *one* pending review GitHub allows per user
per PR, which is a threaded draft workspace:

- **Master** adds top-level pending inline comments — its proposed
  review — and maintains a local `master.md` (running summary,
  general concerns, the draft body for the final submit).
- **Reviewers** add **pending threaded replies** under master's
  comments, each marked `🤖<label>🤖` (or a 👍 / ✅ reaction to
  approve a comment without commenting). Substance lives on
  GitHub.
- **Reviewers** also write their authoritative verdict to a local
  file (see Layout): which round they reviewed, REQUEST_CHANGES |
  FINISHED, and a short summary.
- **Master** integrates each reply (edits its own comment to
  reflect the feedback), then **deletes** the reviewer's pending
  reply.
- **Converged** = every reviewer's local verdict is FINISHED for
  master's current round AND only master's comments remain in the
  draft.
- **Submit** = publish the pending review atomically, with
  `master.md`'s body as the review summary. Hidden iteration → one
  clean published review. This is "post comments but don't submit
  … then submit the final review."

**Master vs reviewer is a STRUCTURAL distinction, not a text one**
(ruthless e9d1bbb #3). Master posts **top-level** review comments;
reviewers post **threaded replies** (`inReplyTo` set). The
destructive "only master comments remain" sweep keys on that
structure — delete all replies, keep top-level — never on scanning
bodies. A body-text emoji must NOT be the load-bearing key for a
destructive op against a live PR: a reviewer who omits the marker
would evade deletion, and master quoting `🤖` in its own comment
would self-delete. The `🤖<label>🤖` marker is retained for HUMAN
attribution only (all agents share one GitHub author, so the
marker is how a person reading the PR tells codex's reply from
ruthless's).

## Why this shape (resolves the prior review)

The earlier hidden-ref design drew two REQUEST_CHANGES from codex
(@8499cf3); both dissolve here:

- **Feedback resolution** — there is no `feedback write --commit
  <cycle-sha>`; PR review has its own verb writing its own local
  verdict files. The sha-keyed feedback resolver is untouched.
- **wfw wake** — there is no shared hidden ref to watch. Every
  wake is a write under `.clank/pr-reviews/` in the worktree,
  which the existing `.clank/` watcher already sees. No
  common-git-dir watching, no ref plumbing.

## Layout

All review state lives with the forked team in the PR worktree
(see Worktree model). Gitignored under `.clank/pr-reviews/`, so it
is structurally uncommittable onto the PR branch.

```
<pr-worktree>/.clank/pr-reviews/<pr>/        (gitignored)
  pr.json        { repo, number, head_sha (pinned), round,
                   submitting }
  master.md      master's running summary · general concerns ·
                 the draft body for the final submit
  reviews/
    codex.md     { reviewed_round, verdict, summary }
    ruthless.md
```

`round` is master's current revision counter (see Rounds);
`submitting` is the TOCTOU freeze flag (see Submit).

The pending review's id is **resolved on demand, not stored** —
see the phase-4 refinement under GitHub mechanics. The
node-vs-numeric footgun (ruthless da957d7 note 1) closes by never
persisting either form: each call takes the right one straight
from the resolve query (REST `id` for submit/discard, `node_id`
for reply creation).

## Rounds (the freeze/staleness mechanism, as an integer)

Cycles-as-commits gave freeze + staleness detection for free.
Here a plain `round` counter in `pr.json` does the same:

- Master bumps `round` whenever it changes the pending comments
  (after integrating feedback, or on the first draft). The bump is
  a `.clank/` write → wakes reviewers.
- A reviewer records `reviewed_round` when they review. They are
  "current" iff `reviewed_round == pr.round`.
- **Convergence** = for every reviewer, `reviewed_round ==
  pr.round` AND `verdict == FINISHED`, AND only top-level master
  comments remain in the draft (no pending threaded replies) —
  the structural check, independent of any marker text.

So if master edits after a reviewer approved, the round advances,
that reviewer is no longer current, and the gate reopens — the
stale-approval race is closed by construction, exactly as the
content-addressed cycles would have.

## Tier semantics — milestone gating, unchanged policy

Reuse the existing two-tier milestone rule: commit reviewers
(codex) review EVERY round; the gate tier (ruthless) wakes only
when the commit tier is FINISHED ("ready to submit"). Same policy
as `gate-reviewers-only-plan-change-and-finish`; zero new gate
code.

VERIFIED mechanism (wait.rs:215 read at phase-3 design): feed
`compute_gate` the CURRENT-round verdicts (the `reviews/*.md`
entries whose `reviewed_round == pr.round`, mapped to
`ReviewEntry`) with the team's `commit_reviewers`/`gate_reviewers`
and **`latest_touched_plan = false`**. The tier semantics then
fall out unchanged:

- `Unreviewed` → a commit reviewer hasn't posted a current verdict
  → that commit reviewer's turn.
- `ChangesRequested` → master integrates (revise → bump round).
- `ApprovedPendingGate` → commit tier FINISHED is the milestone
  (`commit_finished` path, since `latest_touched_plan=false`) →
  gate reviewers wake.
- `Finished` → both tiers FINISHED → master submits.
- `Approved` (mid-flight `approve`, no milestone) → master's call;
  PR reviewers are expected to use `finished`/`request_changes`,
  so this is a rare nudge state.

So the gate LOGIC is reused verbatim; only the adapter (verdict
files → current-round `ReviewEntry`s) and the PR-keyed wait items
are new. `pending_reviewers` stays for the human `status` display
("waiting on X").

## Wait-surface integration

Precedent: blocks (non-commit FS state joined via a side
projection in `derive_status`). A projection scans
`.clank/pr-reviews/*/pr.json` + `reviews/*.md` and contributes
`WaitItem::PrReviewer { pr, round }` / `WaitItem::PrMaster { pr }`
into `WorkStatus`; `work_for` routes per role like plan items. The
stop-hook hint is one line (`pr-review: review #123 round 3`).

## Verbs (names provisional)

```
clank pr-review start <pr>   # fetch pull/<pr>/head, pin head_sha,
                             #   scaffold local state. NO GitHub I/O
                             #   — the pending review is born when
                             #   master posts its first comment.
                             #   Reuses fork --pr's fetch+pin helper;
                             #   works in any checkout of the PR.
clank pr-review note --verdict <request-changes|finished> -m "…"
                             # reviewer: write reviews/<label>.md,
                             #   stamp reviewed_round = pr.round.
                             #   (Posting the 🤖 pending replies
                             #   themselves is done via gh api per
                             #   the skill — this records the
                             #   authoritative verdict.)
clank pr-review submit       # MASTER-ONLY. Gate must be converged.
                             #   Re-sweep replies, then publish the
                             #   pending review with master.md's
                             #   body. Tears down NOTHING.
clank pr-review abort        # MASTER-ONLY. Resolve + discard the
                             #   pending review (if any) + local
                             #   scratch. No worktree teardown.
clank pr-review status       # who we're waiting on (round +
                             #   per-reviewer verdict) — also folded
                             #   into clank status / TUI.
```

`submit` and `abort` are role-gated to master (ruthless e9d1bbb
minor a): a reviewer running `abort` would delete the team's
shared pending review out from under everyone.

### Submit is a destructive, racy op — guard it

Convergence ("no reviewer replies remain") and the publish are NOT
atomic against a reviewer reply landing in between (the round
counter closes the stale-*approval* race but not this TOCTOU —
ruthless e9d1bbb #2). A reply in that window would publish a
half-baked thread onto a real PR. Guards, belt-and-suspenders for
a user-visible destructive action:

1. master freezes the round (a `submitting` flag in `pr.json`) so
   reviewers hold off, then
2. master RE-SWEEPS all replies (structural: delete every reply)
   immediately before the publish call, then publishes.

In the converged state reviewers are quiescent, so the window is
narrow — but a live-PR publish earns the explicit guard.

**Reactions at submit** (ruthless da957d7 note 2): the structural
sweep deletes replies, not reactions, so a reviewer 👍 on a master
comment would publish as a self-reaction (shared identity).
Decision: ACCEPT it — cosmetic, and it's the team's own account
reacting to its own comment. Submit does not clear reactions;
revisit only if it ever reads as noise.

## GitHub mechanics + the one sizing-time spike

- Create pending review (an AGENT, posting the first comment via
  raw `gh`): `POST …/pulls/{n}/reviews` with no `event` → PENDING.
  clank doesn't create it or capture the id — it RESOLVES the id
  per-call (below).
- Resolve (clank): `GET …/pulls/{n}/reviews`, filter
  `state == PENDING` (singleton → ≤1); the entry carries both
  `id` (numeric) and `node_id` (GraphQL).
- Submit: `POST …/pulls/{n}/reviews/{review_id}/events` with
  `event: COMMENT` + `body` = master.md's summary.
- Discard: `DELETE …/pulls/{n}/reviews/{review_id}`.
- Master edits/deletes its own and reviewers' pending comments:
  same gh identity + write access, always permitted.

### Phase-4 refinement: RESOLVE the review id, don't store it

The spike confirmed at most ONE pending review per user per PR, so
clank never persists `review_id` — `submit`/`abort`/`status`
RESOLVE it on demand: `GET …/pulls/{n}/reviews`, filter
`state == PENDING` (the singleton guarantees ≤1), take its
`id`/`node_id`. This is staleness-free: a stored id would dangle if
the review were discarded + recreated, whereas the query always
returns the live one (or none → nothing to submit/discard).

This SUPERSEDES the earlier "store both id forms in pr.json"
note (ruthless da7ab89 #1): the node-vs-numeric distinction still
matters, but it's resolved per-call from the query result (REST
`id` for submit/discard, `node_id` for reply creation) rather than
cached — so the footgun closes by never persisting either. The
`review_id`/`review_node_id` fields drop from `PrReviewState`.

Division of labor: clank owns the pending-review LIFECYCLE
(resolve / submit / discard) as testable pure argv-builders +
response-parsers behind a thin `gh` spawn. The comment SUBSTANCE
(master's top-level comments, reviewers' threaded replies,
reactions) is posted by agents via raw `gh` per the skill — clank
doesn't wrap every comment op. `start` therefore does NO GitHub
I/O (the pending review is born when master posts its first
comment); only `submit`/`abort`/`status` query.
- **SPIKE (verify on a scratch PR before building) — THREE
  load-bearing unknowns, not one** (ruthless e9d1bbb #1). The
  whole model is founded on unverified GitHub behavior; confirm
  all three before writing code:
  1. **Single-pending-review singleton.** When a second agent
     `POST …/pulls/{n}/reviews` with no `event` while a pending
     review already exists (same identity), does GitHub return the
     SAME `review_id`, error, or create a SECOND draft? The
     "one shared draft" model REQUIRES the singleton. If GitHub
     permits multiple pending reviews per user, the model
     collapses and needs a different coordination primitive — so
     this is the make-or-break check.
  2. **Replies stay draft until submit.** The exact call that adds
     a reply to the pending review keeping it a draft (not
     publishing on the spot) — likely GraphQL
     `addPullRequestReviewComment` with the pending `review_id` +
     `inReplyTo`; plain REST reply endpoints tend to publish
     immediately.
  3. **Reaction visibility on pending comments.** A 👍 / ✅ on a
     draft comment may publish immediately even while the comment
     is unsubmitted. If so, reactions CANNOT be the
     lightweight-approve channel and approval is local-file-only.

  lloyd confirmed the UI supports pending threaded replies +
  reactions; the spike pins the API paths and the singleton
  semantics. Lesson from this session: verify tool capability
  against the live API before designing on it.

### Spike findings — ALL CONFIRMED (2026-06-15, scratch PR)

Ran against a private throwaway PR. The model holds; exact
incantations (the plan promised to record these):

1. **Singleton — HOLDS (foundation safe).** A second
   `POST …/pulls/{n}/reviews` with no `event`, same user, returns
   `422 "User can only have one pending review per pull request"`.
   The shared-draft model is valid.
2. **Draft replies — GraphQL only.** REST
   `POST …/pulls/{n}/comments -F in_reply_to=…` FAILS with the same
   422 (it tries to open its own pending review). The working path
   is GraphQL `addPullRequestReviewComment(input:{
   pullRequestReviewId:<review node_id>, inReplyTo:<comment
   node_id>, body})` → reply lands in the existing pending review,
   `state: PENDING`, threaded (`in_reply_to_id` set). Reviewers
   need the review's + comment's NODE ids (REST responses carry
   `node_id`).
3. **Reactions — work and don't leak.**
   `POST …/pulls/comments/{id}/reactions -f content=+1` succeeds on
   a pending comment, is readable, and does NOT publish the comment
   (it stays out of `GET …/pulls/{n}/comments`). So 👍 is a viable
   lightweight-approve signal — though local files stay
   authoritative.
4. **Structural sweep + submit — clean.** Pending comments are
   invisible in `GET …/pulls/{n}/comments` until submit (count 0
   throughout iteration). Delete a reply:
   `DELETE …/pulls/comments/{reply_id}`. Top-level vs reply is
   `in_reply_to_id == null`. Submit:
   `POST …/pulls/{n}/reviews/{review_id}/events -f event=COMMENT -f
   body=…` → `state: COMMENTED`, only the surviving (master)
   comments publish.

Create-pending shape:
`POST …/pulls/{n}/reviews -f commit_id=<sha>
-f "comments[][path]=…" -F "comments[][line]=N"
-f "comments[][side]=RIGHT" -f "comments[][body]=…"` (no `event`).

## Anchoring & PR advances

Comments anchor to the pinned `head_sha`. Pin + warn, NO automatic
re-pinning: if `pull/<pr>/head` has moved past the pin, `note`/
`submit` WARN; master decides whether to re-pin (re-fetch + new
pin + bump round). GitHub validates a comment's line against the
diff at post time, so a bad anchor fails in-loop when master adds
it — no separate anchor-validation engine.

If master submits anyway without re-pinning (ruthless e9d1bbb
minor b), GitHub keeps the comments anchored to the old sha and
renders them as OUTDATED — not an error, just stale-positioned.
`submit` states this in its warning so master chooses with eyes
open.

## Worktree & session model

The forked team (from `clank fork --pr`) lives in the PR worktree;
all review state is the worktree's gitignored
`.clank/pr-reviews/`. `fork --pr` does NOT auto-start review
(decision 1) — its orientation prompt MENTIONS `clank pr-review
start <pr>` so master kicks it off as its first act. `submit` and
`abort` tear down NOTHING (decision 2); they print a `git worktree
remove <path>` hint and the team may keep working (e.g. pushing
fix-commits to the PR branch).

## Dedicated skill file

PR-review mode has its own protocol distinct from plan review, so
it ships a `clank-pr-review` skill (loaded in PR-review mode) that
teaches agents:

- master: draft inline comments, maintain master.md, integrate +
  delete reviewer replies, bump the round, submit when converged.
- reviewers: read master's pending comments, post `🤖<label>🤖`
  pending replies / reactions for substance, record the
  authoritative verdict with `clank pr-review note`.
- the marking convention and the "only master comments remain"
  cleanup invariant (structural: master = top-level, reviewers =
  replies).
- **EXACT `gh`/GraphQL incantations** for posting pending replies,
  reactions, edits, and deletes (ruthless e9d1bbb minor c). Since
  reviewers post the substance via raw `gh` while `clank pr-review
  note` records the local verdict, the two can diverge if an agent
  fat-fingers a gh call; verbatim incantations + the two-sided
  convergence (local FINISHED AND no replies remain) keep them
  reconciled.

Keeps the main clank skill uncluttered.

## Implementation phases (suggested)

1. **Spike** — ✅ DONE 2026-06-15. All three unknowns confirmed on
   a scratch PR (see Spike findings): singleton holds, draft
   replies via GraphQL `addPullRequestReviewComment`, reactions
   work without leaking. Foundation validated; phases 2–6 cleared
   to proceed.
2. **Local state + verbs** — ✅ DONE. `clank_core::pr_review` (2a:
   types + parsers + `pending_reviewers`, 8 pure tests) +
   `cli::pr_review` (2b: `start`/`note`/`abort`/`status`, slug
   parse, master-only role gate, single-active-PR inference; 7
   in-process integration tests + a parse_slug unit test).
   `/pr-reviews/` added to the canonical gitignore set. `start`
   does no GitHub I/O; the pending review is born when master
   posts its first comment (the id is resolved on demand, never
   stored — see the phase-4 refinement).
3. **Wait surface + gate** — ✅ DONE. PR-keyed `WaitItem::PrReviewer
   {pr,round}` / `PrMaster {pr,round,next}` (+ `PrMasterNext`);
   `WorkStatus.pr_reviews`; `PlanStateLookup::pr_reviews` (default
   empty) fed by `cli::pr_review::pr_review_inputs` scanning
   `.clank/pr-reviews/` and passing only current-round verdicts;
   `derive_status` runs `compute_gate(current, commit, gate,
   false)` per PR + `missing_for_gate` to find the owing tier;
   `work_for` routes `PrReviewer` to missing tier members and
   `PrMaster` to master by gate (Integrate/Submit/Continue);
   `wfw` JSON+human + `stop_hook` gain `pr_reviewer`/`pr_master`
   hint lines; `clank status` gains the `pr` line. PR items fire no
   proactive OS hook (HookFiring is plan+sha keyed) — they surface
   via the stop-hook's `clank wfw` pull, like ad-hoc/queue items.
   Tests: core tier-progression + routing (7 cases), in-process
   projection incl. stale-round drop.
4. **GitHub I/O** — ✅ DONE. `cli::pr_review::gh`: the
   pending-review LIFECYCLE only — `resolve_pending_review`
   (`GET pulls/N/reviews` → the singleton PENDING, both id forms),
   `submit_review` (events event=COMMENT + body), and
   `discard_pending_review` (DELETE, no-op if none). Pure
   argv-builders + `parse_pending_review` unit-tested; only
   `run_gh` touches the network. `abort` now best-effort discards
   the pending review (warn, never fail) before removing the local
   scratch. Comment substance (post/edit/delete/reply/react) is
   agent raw-`gh` — phase 5's skill.
5. **Skill file** `clank-pr-review` — ✅ DONE.
   `setup_assets/pr_review_skill.md`, installed by `clank setup` to
   `~/.{claude,codex}/skills/clank-pr-review/SKILL.md` (+ doctor
   check). Carries the model, the verbs, and EXACT incantations
   ALL verified live before writing: create-with-first-comment;
   `addPullRequestReviewThread` for further top-level comments;
   `addPullRequestReviewComment` + `inReplyTo` for threaded
   replies; REST reactions + reply delete; and the resolve
   one-liner using `--paginate --jq` (NOT `--slurp --jq` — gh
   rejects that pairing, a footgun the cleanup pass surfaced).
   Content-guard test pins the verbs + incantations + the
   slurp/jq guard.
6. **`submit`** — ✅ DONE. `clank pr-review submit` (master-only):
   convergence gate (fail-closed team resolve → current-round
   verdicts through `compute_gate(..., false)` must be Finished),
   `extract_submit_body` from master.md's `## Submit body`
   (refuses empty/placeholder), then the TOCTOU-safe sequence —
   set `submitting`, RE-SWEEP reviewer replies
   (`gh::sweep_replies`, structural: delete `in_reply_to_id`-set
   comments) IMMEDIATELY before `gh::submit_review`, then remove
   the local scratch (the published review is the durable record,
   and leaving it would re-surface as submit-again on the wait
   surface). Tests: extract_submit_body matrix, structural
   parse_reply_ids (marker-agnostic), submit master-only +
   not-converged refusal before any gh call.

Phases 2–3 are pure/local and fully testable in-process; phase 4
is the gh-shelling layer (pure argv/parse tested; the spawn is a
shell seam, never unit-tested against live GitHub; the clank
binary is never spawned in tests).

## Status

Design converged with lloyd 2026-06-15: GitHub pending review as
the single shared draft workspace (all agents = one gh identity),
local `.clank/pr-reviews/<pr>/` as the authoritative verdict +
round-coordination layer, dedicated `clank-pr-review` skill. Both
of codex's hidden-ref concerns (@8499cf3) are dissolved by the new
model.

Redesign APPROVED by codex + ruthless @e9d1bbb. This revision
folds in ruthless's hardening before implementation: the spike now
covers all three load-bearing GitHub unknowns (singleton pending
review, draft-reply path, reaction visibility); the destructive
sweep is STRUCTURAL (delete replies, keep top-level) not
text-marker-keyed; submit has a round-freeze + pre-publish
re-sweep against the TOCTOU; submit/abort are master-only; the
outdated-anchor submit behavior is stated; the skill must carry
exact gh incantations. Ready to implement after the phase-1 spike.
