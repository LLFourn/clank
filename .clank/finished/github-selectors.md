# github-selectors
# clank — a GitHub watch selects by object and event, and a submitted review is its own event

## Rules

- Commit in this repo. Do not push or post anywhere.
- No `claude.ai/code` link and no `Claude-Session:` trailer in any commit or message.
- Comments explain WHY, never WHAT. Delete any comment the code already says; keep only rationale,
  constraints, hazards and rejected alternatives.
- Tests never reach GitHub.
- Report to `clank send --repo ~/src/clank-wr --to claude -F <file>`, and tell the fs-war-room
  controller when it's done (`clank send --repo ~/src/fs-war-room --to claude`), since it will switch
  its config over. Begin each report with a line telling the reader to read the whole message.

## Why

On 2026-09-29 LLFourn submitted a review on frostsnap/frostsnap#603, and nothing woke the fs-war-room
controller. Its comment watch is:

```json
{"kind": "github", "events": ["pr_comment"], "include_own_actions": false}
```

`pr_comment` lumps together three different things: an issue comment on a PR, a submitted review, and
an inline diff comment. Review teams draft pending-review comments through LLFourn's `gh` login, and
those drafts show up as his own inline comments. So the watch has to exclude his login entirely, and
that also excludes his submitted reviews.

Drafting a comment and submitting a review are different events. A controller should hear about
every submitted review, anyone's including LLFourn's, and not about drafts. The event items already
carry the distinction (`detail`: `review`, `issue_comment_on_pr`, the inline one), but a watch can
only select on the flat kind, and its filters apply to the whole watch.

## Model (LLFourn)

The watch selects by **object**, as the major type, and then by that object's **events**. Filters
sit at the level they describe:

```json
{"kind": "github", "repo": "frostsnap/frostsnap", "watch": [
  {"pr": {"numbers": [603, 522], "events": ["review_submitted", "comment", "review_comment", "updated", "merged"]}},
  {"pr": {"events": ["opened", "merged", "review_submitted"]}},
  {"issue": {"events": ["opened"]}}
]}
```

- **Object-level filters** describe which objects: PR numbers, and author or label if cheap. They are
  written once on the object and apply to every event under it. That is why `pr` is a major type.
- **Event-level filters** are about the event, not the object, e.g. `include_own_actions` on
  comments, alongside everyone's `review_submitted`. The grammar is yours, e.g. an event name
  expanded into an object: `{"comment": {"include_own_actions": false}}`.
- **`review_submitted` is its own event.** It carries the state (approved, changes requested,
  commented), the reviewer and the review's id. It fires for any reviewer, the watcher's own login
  included, and **never** for a pending review or its draft comments.
- **One grammar** covers every kind that exists today. The existing flat names (`pr_comment`,
  `pr_merged`, …) and the top-level `events` / `include_own_actions` keep working as aliases, so
  current configs are unchanged.
- **Documented and validated:** an unknown object, event or filter is refused, listing the valid
  ones.

## Compatibility (added in review)

The grammar is additive. Three things make that more than a promise.

**A legacy watch is not re-encoded.** Its flat fields keep their exact meaning,
including one the new shape does not share: `GithubEventKind::is_pr_scoped`
(`core/src/agent_config.rs:175`) restricts only `pr_updated`, `pr_merged`,
`pr_comment` and the two CI kinds to `prs`, so a watch with `prs: [603]` still
wakes on `pr_opened` for ANY pr. Under `{"pr": {"numbers": [603], …}}` the
numbers scope every event beneath them, which is the clearer rule and a
DIFFERENT one. Translating old JSON into the new form would therefore change
what an existing watch wakes on, so nothing translates it: the flat shape stays
a first-class input with its own semantics, and the new shape is an alternative
way to say something more precise.

**A legacy watch keeps its source key, byte for byte.** `supervisor::source_key`
(`github/supervisor.rs:310`) hashes `repo`, `events`, `branches`,
`include_own_actions` and `prs` into the WAL and seen identity, and its own
comment already says what re-keying costs: the old log's rows stay listed as
unhandled with nothing to re-present them, the prompt sidecar orphans, and
while both are armed two loops ingest the same repo into two logs. So a config
that does not use the new grammar must hash exactly as it does today —
unchanged bytes, not merely an equivalent value.

A watch that DOES use the new grammar gets a key derived from its selectors in
a canonical form, so reordering clauses or listing PR numbers differently does
not silently re-key a live watch. Rewriting a watch from the flat shape into
the nested one IS a re-key, with the costs above; that is the author's choice
to make deliberately, and no migration performs it for them.

**Clauses select; they do not fan out.** The example's two `pr` clauses can both
match one `review_submitted`, and that must wake the agent once. Selection is a
union — an event is ingested if any clause admits it — and the existing event
identity (feed id or action key) is what makes the second match a no-op, which
is the same mechanism that already stops a poll and a webhook delivering one
event twice.

## What landed, where it differs (written at finish)

Two of the model's open choices were resolved differently and one of its lines is now
wrong. Recorded here rather than by editing the model above, so the ask and the answer
stay distinguishable.

**Event-level filters are a second CLAUSE, not an event expanded into an object.** The
syntax is not `{"comment": {"include_own_actions": false}}`; it is:

```json
{"watch": [{"pr": {"events": ["review_submitted"]}},
           {"pr": {"events": ["comment"], "include_own_actions": false}}]}
```

Clauses union, so writing two clauses is how two events get different filters — which
makes a per-event form redundant and leaves one shape to document, validate and key.
`include_own_actions` sits on the clause and defaults to the watch-level value. Ask if
you want the per-event form as well.

**The flat shape is not an alias.** "the top-level `events` / `include_own_actions` keep
working as aliases" is the one line the implementation contradicts. `prs` scopes fewer
events than `numbers` does, so the two are not two spellings of one thing: they are two
first-class inputs with different meanings, mixing them is refused, and nothing
translates between them. That — not aliasing — is what leaves current configs unchanged,
and it is what the Compatibility section above argues for.

**Not done:** object-level `author` / `label` filters. The model hedged them with "if
cheap" and they are not: each needs a payload field the events feed does not reliably
carry, and neither is needed by the case this plan opens with.

`review_comment` and `issue_comment` narrow `pr_comment` by its `detail` rather than
becoming kinds of their own, so the EVENT identity is untouched: a narrowed watch logs the
same `pr_comment` rows under the same action keys as before, and no other watch's rows
change meaning.

The WATCH identity is a different thing, and editing the selectors changes it. A nested
watch's source key hashes what it selects, selector details included, so `comment` ->
`review_comment` is a different watch. It starts a FRESH log, and the old log's unhandled
rows stay under the old key with nothing left to present them — the full cost the
Compatibility section above spells out, the same for any change to what a watch admits.
A stable action identity does not save that backlog; acking it before the edit does.

## Tests

- A pending review and its draft comments produce no `review_submitted` event. Submitting it
  produces exactly one, with its state, for the watcher's own login too.
- Every legacy config shape wakes on exactly what it wakes on today, `prs` NOT restricting
  `pr_opened` included — the asymmetry is pinned, not assumed to survive.
- A legacy config's source key is unchanged, compared against the bytes today's code produces
  rather than against a recomputed expectation.
- A nested watch's key is stable when its clauses are reordered and its numbers relisted.
- One `review_submitted` matched by two clauses wakes the agent ONCE and leaves one row.
- Object-level PR numbers scope every event under that object. An event-level
  `include_own_actions: false` drops the watcher's own comments but not its submitted reviews.
- Every existing config shape parses to the same behaviour as before.
- An unknown selector is refused with the valid list.
- The workspace's usual lint and test commands pass.

## Done when

- The fs-war-room controller can write one watch that wakes on every submitted review on
  frostsnap/frostsnap (LLFourn's included) and on other people's comments, and never on its own
  drafts. Include that watch, verbatim, in the report.
- The docs cover the grammar.
- Report as above.
