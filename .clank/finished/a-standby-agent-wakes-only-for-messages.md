# a-standby-agent-wakes-only-for-messages
# A standby agent wakes for everything but reviews

## Why

> "Along with this we need to have a new agent mode which is just
> standby i.e. isn't woken up on reviews of any kind -- only messages."

Asked whether a standby agent's own configured GitHub/command watches
should still wake it (codex on 17cea5f):

> "yes it should be woken by everything except for commits"

Every non-master agent is a reviewer in some tier — commit, plan,
final or gate — and there is no way to keep an agent on the team
without it reviewing. An agent kept for other work (answering
messages, running a task another agent hands it) is woken for every
review it is in the tier for.

Emptying its tiers is not enough. `work_for` gives an Unreviewed ad-hoc
commit to ANY reviewer, with no tier check (`core/wait.rs`, the `(Role::Reviewer, Unreviewed)` arm near line 1520),
so a reviewer in no tier would still be woken for every ad-hoc commit.
And its verdicts would be dropped from the gate silently.

## The model

**Standby is a role, not a reviewer with no tier.** The roster gains
`standby` beside `commit`/`plan`/`final`/`gate`, and the resolved role
gains `Standby` beside `Master` and `Reviewer`. What an agent is woken
for follows from that role: `work_for` gives a standby agent no repo
work of any kind — no plan, gate, ad-hoc or PR review.

**Standby takes away review work and nothing else.** Everything that is
not a review still wakes a standby agent exactly as it wakes any other:
its inbox (a-message-wakes-its-agent-until-it-is-handled), the
GitHub and command watches configured on it (`wait_events`, `--event`),
and a block's answer; a pending block still parks it, as it parks
everyone. Those sources are started and delivered without regard to
role today (`cli/wait.rs`, `EventSources::spawn` and the single return
boundary), and standby leaves that alone, so a role change mid-wait —
into standby or out of it — neither starts, stops nor strands a
source. The wait already re-derives the role on every refold, so the
next fold after `set-review standby` simply projects no review work.

A standby agent is in no review tier, so it is never a gate
participant: the gate neither waits on it nor counts it. A roster of a
master and standby agents is master-only mode. Moving an agent out of
standby into a tier is a tier ENTRY, and moving it into standby a tier
EXIT, through the same transition code every tier change uses.

**On the agent page, standby is no boxes ticked.** The review
checkboxes say what an agent reviews. Unticking the last one today does
nothing, deliberately (`tier_after_toggle`, "last coverage — keep it");
it now means "reviews nothing", which is standby. Ticking any box from
standby enters that tier. The same holds on the add page.

A standby agent reads the reviewer skill. Its role guard says the skill
is for reviewers and standby agents, and that a standby agent reviews
nothing: it writes no verdicts, and handles every other item it is
woken with — messages, its GitHub and command events, a block's
answer — as any agent does.

## Deliverables

1. **`standby` on the roster and the resolved role.**
   - `RosterRole::Standby` and `ReviewKind::Standby`, serialized
     `standby`, and a `Standby` resolved role.
   - Every match the compiler flags handles it: `role_word`, doctor's
     role strings, `team list/show --json` buckets and `team show`'s
     lines, `tier_label`, `tier_boxes`, `role_review_kind`,
     `review_kind_role`, the skill composer and the hook's `(role)`
     header.
   - The catch-all sites that would silently treat it as a reviewer are
     made explicit: `resolve_registered_set` and the role mapping in
     `agent_store`.
2. **`work_for` gives standby no repo work.** Its arms are decided by
   role, so standby gets nothing from repo state. Everything not
   produced by `work_for` — inbox messages, the agent's configured
   GitHub/command sources, a block's answer — reaches it unchanged.
3. **Setting it.** `clank agent add --review standby` and
   `clank agent set-review <name> standby` go through the existing
   transition core. The TUI's last-box untick becomes standby, on the
   agent page and the add page, and the page's desc says
   `standby — reviews nothing; woken by everything else`.
4. **Listing it.** `clank agent list`, `clank status` and the TUI name
   it `standby` wherever they name a tier. Its zellij pane stacks with
   the reviewers, titled `standby` by the TUI's worker — pane titles'
   one writer, which retitles every pane from the roster on its first
   pass, so the stamp a pane is created with is not a second source.
5. **The reviewer skill's role guard** names standby agents, and says
   a standby agent writes no verdicts and handles its other items —
   messages, GitHub and command events, block answers — as usual. The guard is GENERATED from the role
   name (`compose_skill_with`), so without this a standby agent reading
   the reviewer skill is told its role is not `reviewer` and to switch
   to `clank-master`. The words it adds count against the codex
   reviewer skill's ceiling, which the inbox left about 25 bytes under;
   the ceiling rises by exactly what they cost, with the reason in the
   test, as the inbox's did.

## Tests

- **No review wakes a standby agent**: with the agent standby, a plan
  commit, a gate stage, an Unreviewed ad-hoc commit and a PR review each
  give it no items. The ad-hoc case is the one a tierless reviewer
  gets wrong. Mutation check: map standby to `Reviewer` and the ad-hoc
  case fails.
- **Everything else still does**: a standby agent's wait returns a
  message in its inbox, and a command source configured on it wakes it
  when the command completes.
- **A switch mid-wait**: a reviewer parked on a pending review is
  switched to standby; its next fold projects no review item and it
  stays parked, and a message landing afterwards still wakes it.
- **The gate ignores it**: a standby agent's verdict neither satisfies
  nor blocks a gate. A roster of master plus standby is master-only.
- **Setting it**: `set-review standby` writes `"role": "standby"`, and
  back to `commit` enters the commit tier. The TUI's last untick goes
  to standby; ticking from standby enters that tier.
- **Round trip**: `standby` parses from config and team templates and
  is written back unchanged.
- **The guard fits standby**: the composed reviewer skill names the
  standby role in its guard, so a standby agent is never told to use
  the master skill, and it tells a standby agent to write no verdicts
  without telling it to ignore anything else it is woken with.

Run the modules touched, then the whole `-p clank --lib` suite.

## Out of scope

- A standby master. The master implements plans; standby is a
  non-master role.
- Any new wake source for standby agents beyond their inbox.
