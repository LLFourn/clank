# an-agent-with-no-work-says-so
# An agent that owes nothing reads "no work", without nudge or resume caveats it cannot act on

## Why

The user asked what to do with this TUI and `clank status` line (2026-10-16):

`codex → not observed · waiting for work · resumed: retries only, by Codex's own turn/start · not nudged: its version is not known yet`

- **`not observed`** is `Activity::Unobserved` (`agent_state.rs`): clank is not reading the agent's record. That is clank's internal bookkeeping, not a fact about the agent the user can act on. Why it is not reading is the useful fact, and it has two causes (see below).
- **The resume and nudge clauses** explain why clank could not resume or nudge the agent. With nothing owed there is nothing to resume or nudge, so they read as a fault where there is none.

## The facts already published

`Activity::Unobserved` is not by itself proof that the agent owes nothing:
- `recovery::activity` returns it when the agent's record is not read and `!w.obligated`.
- `w.obligated = f.obligated && !f.delivering` (`Driver::reconcile`).
- `monitor::recovery_facts` sets `obligated = !delivering && !owed.is_empty()`. When `delivering` holds and there is no repeat record, `owed` is an empty vector that was never folded.

So `Unobserved` covers two different facts, and the published `AgentState` already separates them through its suppressors, because `Suppressor::Delivering` is set exactly when `w.delivering` is (`recovery::suppressors`):
- **`Unobserved` with `Delivering`:** a waiter is parked that can take the agent's work. Whether work is pending is not established, so this is not "no work".
- **`Unobserved` without `Delivering`:** the fold ran and `owed` was empty, so the agent truly has no work.

No new fact, fold, watch or poll is needed to choose the words.

## What to build

In `agent_state::brief` and `line`, the one rendering both `clank status` and the TUI use:
- **`Unobserved` with `Delivering`** renders `waiting for work`, the suppressor's existing wording, once. No separate activity word, and no resume or "not nudged" clauses, since a parked waiter takes the work without a nudge.
- **`Unobserved` without `Delivering`** renders `no work`, again with no resume or "not nudged" clauses. Other suppressors (`auto off`, `attended`) still show.
- **Every other activity is unchanged**, including `unknown` with its reason and every caveat. So is the stale marking, which comes from `View::stale`, not the activity: a stale or unknown state never reads as `no work`.

## Acceptance

- **Genuinely empty work** (`Unobserved`, no `Delivering`): `no work`, with no resume or nudge clause.
- **A registered waiter** (`Unobserved` with `Delivering`, pending work or not): `waiting for work`, once, with no `not observed` and no resume or nudge clause.
- **Owed work without a waiter** (`Idle`, `Working` and the rest): unchanged, caveats included.
- **Unknown** (`Activity::Unknown`): unchanged, with its reason and caveats. A stale state keeps its stale rendering and never reads as `no work`.
- A TUI render test and an `agent_state` line test cover the first two cases.
- Grep the repo (README, skills and tests included) for `not observed`, and update every reference to the old word.

## As built

- **`agent_state::brief`**, shared by the TUI row and `clank status`:
  - `Unobserved` with `Suppressor::Delivering` renders `waiting for work`;
  - `Unobserved` without it renders `no work`.
- **`agent_state::line`**:
  - for an `Unobserved` agent, it skips the `Delivering` suppressor (already said by `brief`) and returns before the `resumed:` and `not nudged:` clauses;
  - other suppressors still show (`no work · auto off`);
  - every other activity renders as before.
- **Tests:**
  - in `agent_state`: a waiter renders exactly `waiting for work`; no waiter renders exactly `no work`, and `no work · auto off`; the TUI's `brief` tells the two apart; an owing (`Idle`) agent keeps `waiting for work`, `resumed:` and `not nudged:`;
  - the existing unknown and stale tests are unchanged and pass;
  - `status_tui::render`'s row test now expects `no work`.
- **Mutations, each caught:** dropping the waiter guard; keeping the caveats for `Unobserved`; saying `waiting for work` twice.
- **Old wording:** a repo grep finds `not observed` only in this plan, in an unrelated sentence in `.clank/finished/`, and in a `status_tui/mod.rs` comment about a blocking receive. None of these describe the old activity word.
