# adding-an-agent-must-choose-its-role

Adding an agent through the status TUI always makes it a `commit`
reviewer. The choice cannot be expressed, so adding `ruthless` to a
repo where it belongs at `gate` produces the wrong roster and the user
has to notice and correct it afterwards.

## The capability is there; only the choice is missing

`clank agent add --review commit|plan|final|gate` exists, and the core
the TUI calls, `add_repo_roster_agent_by_name`, ALREADY takes a
`RosterRole`. The TUI passes a literal (`status_tui/mod.rs:198`):

    crate::cli::teams_config::RosterRole::Commit

`ConfirmAction::AddCandidate { idx }` carries an index and nothing
else, so a chosen role has nowhere to live even if the user made one.
That is the entire defect.

## Correcting a tier ALREADY works — do not rebuild it

An earlier draft of this plan claimed the TUI could not change an
existing agent's tier and scoped that as new work. That was wrong,
from a search too narrow to find it (codex on 3976a9b).

The agent detail page has `DetailAction::TierCommit | TierPlan |
TierFinal`, which persists through `set_repo_review`
(`status_tui/mod.rs:871`). The core preserves other fields and refuses
master (`agent.rs:1489`), with coverage at `agent.rs:2315` and
`:2344`. All four tiers are reachable: `tier_after_toggle` models the
tier as two checkboxes, plan-stage and final-stage, and `gate` is both
checked.

So tier editing is the CORRECTION path after an add, and this plan
must not duplicate it.

## Change

`AddCandidate` carries a reviewer role, chosen before confirming, and
the confirm-apply path passes it to the core instead of a literal.

`commit` stays the default so the common add remains one keystroke.

Prefer the tier vocabulary the detail page already uses. Two flows that
set the same field with different mental models is a worse outcome
than either model alone.

## Master is unrepresentable, not merely rejected

There is exactly one master, so "add as master" means demoting the
incumbent — a different operation, already owned by `clank agent
promote`, and the core rejects it anyway.

Carry a REVIEWER-role type through `AddCandidate` rather than a
`RosterRole` that happens never to be `Master`. A type that cannot
express the invalid state beats a check that must be remembered.

## Tests

- Each of the four reviewer roles chosen at add lands that role in the
  roster, driven through the confirm-apply path rather than by calling
  the core.
- The default is `commit`, so the existing one-keystroke add is
  unchanged.
- `master` is not constructible in the add flow — a type-level
  guarantee if the type makes it so, otherwise an explicit test.

## Out of scope

- `clank agent add`'s CLI surface, which already does this.
- Tier editing after add, which already works.
- Promotion/demotion of the master.
