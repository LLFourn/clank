# status-is-one-model-rendered-two-ways

`clank status` has one model; `-j` serializes it and the pane draws it.

> "queue a plan to fix status -j -- status should all be rendered from
> the same model. -j should just serialize it to json." — lloyd, after
> `status -j` answered nothing about a roster the pane was showing

## Why

`StatusSnapshot` is the model: the roster with roles and tiers, the
plans and their gates, the blocks, the queue, the stash, the remote,
the head. The pane renders all of it. `-j` renders something else —
`to_json`, a hand-written subset that mirrors "the prior shape":
`plans`, `finished_plans`, `blocks`, `branch`, `head_sha`,
`head_subject`, `repo_basename`, `worktree_dirty`. Nothing else in the
model reaches it.

So `status -j` cannot answer who is on the roster, and does not say
that it cannot: a repo with a master and two reviewers serializes
exactly like a repo with none. That is not a missing feature, it is a
second model that silently disagrees with the first — and it misled a
reader today, who read an empty answer as an empty roster while the
pane beside it showed `claude master` and `codex commit`.

Two renderings of one model can disagree only about presentation. Two
models disagree about facts.

## The build

**The snapshot serializes itself.** The model gets one derived
serialization — every fact the pane can draw, named as the model names
it — and `-j` emits that. The hand-written mirror goes with its
"mirror the prior shape" comment: the shape is the model's now.

**The pane keeps its rendering.** The human view is formatting over
the same value, not a second source: what it shows and what `-j`
prints come from one snapshot in one read, so a field added to the
model appears in both without being added twice.

**What today's readers use keeps its name.** The keys already emitted
mean what they meant — this is what the JSON was missing, not a
renaming of what it had — so a script, a hook or a skill reading
`plans`, `head_sha` or `worktree_dirty` is untouched.

## Tests

- A repo with a roster serializes it: master and reviewers, each with
  the role and tier the pane shows. A teamless repo serializes an
  empty roster, which is distinguishable from a repo that has one.
- The keys today's consumers read are present with their current
  meanings, on a fixture that exercises plans, blocks and a dirty tree.
- One model: a scan holds the pane's own fields against the serialized
  ones, so a fact the pane can draw and the JSON cannot omit fails
  here rather than in somebody's script — the same shape as the web
  facts' key test.

## Out of scope

- `clank log --json`, which is the log's surface; the snapshot's log
  rows stay out of `status -j` as they are today.
- The TUI's web facts, already derived from this snapshot.
