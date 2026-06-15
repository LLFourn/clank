---
name: clank-pr-review
description: Run the clank multi-agent review loop against a GitHub PR. Master drafts inline review comments; reviewers iterate on them until the team agrees; the agreed set is submitted as one published review. Load this when working in a `clank pr-review` session.
---

# Clank PR review

Reviewing a GitHub PR with the clank loop. The review scratch IS a
GitHub **pending review** (an unsubmitted draft): all agents share
one `gh` identity, so they all work inside the single pending
review GitHub allows per user per PR.

- **Master** posts TOP-LEVEL inline comments — the proposed review.
- **Reviewers** post THREADED REPLIES under master's comments,
  each marked `🤖<label>🤖` (or a 👍 reaction to approve a comment
  silently), and record their verdict with `clank pr-review note`.
- **Master** integrates each reply, deletes it, and bumps the round.
- **Converged** = every reviewer's verdict is FINISHED for the
  current round and only master's top-level comments remain.
- **Submit** publishes the pending review as one review.

The verdict files under `.clank/pr-reviews/<pr>/reviews/` are the
AUTHORITATIVE convergence signal — the GitHub comments are the
substance, the files are the gate. Master vs reviewer is
STRUCTURAL: top-level comment = master, threaded reply (`inReplyTo`
set) = reviewer. The `🤖` marker is for human attribution only.

## clank verbs (run via your shell)

- `clank pr-review start <pr>` — pin the PR head, scaffold
  `.clank/pr-reviews/<pr>/`. No GitHub I/O; the pending review is
  born when master posts its first comment.
- `clank pr-review status` — round + per-reviewer verdict + who
  we're waiting on.
- `clank pr-review note --verdict <finished|request-changes> -m "<summary>"`
  — reviewer: record your authoritative verdict for the current
  round. Author + PR inferred from your binding + the single
  active review.
- `clank pr-review submit` — MASTER ONLY. Gate must be converged;
  re-sweeps reviewer replies, then publishes the pending review
  with `master.md`'s body.
- `clank pr-review abort` — MASTER ONLY. Discard the pending review
  + local scratch.

## gh incantations (verified) — the comment substance

clank owns the lifecycle (start/submit/abort); the comments
themselves are raw `gh`. `SLUG` is the `repo` from
`.clank/pr-reviews/<pr>/pr.json` (`owner/name`); `SHA` is its
pinned `head_sha`. Run these EXACTLY — small mistakes post to a
real PR.

**Master — first comment (creates the pending review):**
```
gh api repos/$SLUG/pulls/$PR/reviews \
  -f commit_id=$SHA \
  -f 'comments[][path]=path/to/file.rs' -F 'comments[][line]=42' \
  -f 'comments[][side]=RIGHT' -f 'comments[][body]=<your comment>'
```
(no `event` → the review stays PENDING/draft.)

**Master — add another top-level comment to the existing draft:**
```
# resolve the pending review's GraphQL node id (note: --jq, NOT --slurp):
RNODE=$(gh api repos/$SLUG/pulls/$PR/reviews --paginate \
  --jq '.[] | select(.state=="PENDING") | .node_id')
gh api graphql -f query='
  mutation($r:ID!,$b:String!){
    addPullRequestReviewThread(input:{
      pullRequestReviewId:$r, path:"path/to/file.rs", line:42,
      side:RIGHT, body:$b}){ thread{ id } } }' \
  -f r="$RNODE" -f b='<your comment>'
```

**Reviewer — threaded reply under a master comment:**
```
# find the master comment's node id (from the pending review):
RID=$(gh api repos/$SLUG/pulls/$PR/reviews --paginate \
  --jq '.[] | select(.state=="PENDING") | .id')
gh api repos/$SLUG/pulls/$PR/reviews/$RID/comments \
  --jq '.[] | "\(.id)\t\(.node_id)\t\(.path):\(.line)\t\(.body)"'
RNODE=$(gh api repos/$SLUG/pulls/$PR/reviews --paginate \
  --jq '.[] | select(.state=="PENDING") | .node_id')
gh api graphql -f query='
  mutation($r:ID!,$reply:ID!,$b:String!){
    addPullRequestReviewComment(input:{
      pullRequestReviewId:$r, inReplyTo:$reply, body:$b}){
      comment{ databaseId } } }' \
  -f r="$RNODE" -f reply="<master comment node_id>" \
  -f b='🤖<your-label>🤖 <your point>'
```

**Reviewer — approve a comment with a reaction (no reply):**
```
gh api repos/$SLUG/pulls/comments/<comment_id>/reactions -f content=+1
```

**Master — delete a reviewer reply after integrating it:**
```
gh api repos/$SLUG/pulls/comments/<reply_id> --method DELETE
```

The `--paginate --jq` form is required for resolves — `--slurp`
cannot combine with `--jq`.

## The loop

**Master**
1. `clank pr-review start <pr>`; read the PR; draft top-level
   comments (first via the create call, more via
   `addPullRequestReviewThread`). Keep `master.md` updated with
   your summary + the body you'll submit.
2. Bump the round so reviewers re-review: any write under
   `.clank/pr-reviews/<pr>/` wakes them (e.g. editing `pr.json`'s
   round, which `clank pr-review` manages).
3. When reviewers reply: integrate each into your comments, then
   DELETE the reply. Bump the round again.
4. When `clank pr-review status` shows converged → `clank pr-review
   submit`.

**Reviewer**
1. On a wake, read master's pending comments + the PR.
2. Reply (marked `🤖<label>🤖`) on anything you'd change, or react
   👍 to approve a comment.
3. Record your verdict: `clank pr-review note --verdict
   <finished|request-changes> -m "<summary>"`. FINISHED only when
   you'd publish the current comment set as-is.

## Stop-hook continuations

A `pr-review: review #<pr> round <n>` / `pr-review: master <next>
#<pr>` hint means act now — the same minimal-hint convention as
plan review. Run `clank pr-review status` for the full picture.
