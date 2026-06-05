# status-blocks-dominate-gate
# Make open blocks dominate `clank status` output: a blocked plan's `gate`, `waiting on`, and `reason` should reflect the block (and its creator), not the underlying review gate.

## Problem

`clank status` shows misleading "waiting on / reason" text when a plan has an open block. Real output from master today on `runner-emits-tx-correlation-hints`:

```
plan: runner-emits-tx-correlation-hints
  latest reviewable: 20b9003
  gate:              unreviewed
  waiting on:        codex, ruthless
  reason:            missing approval from codex, ruthless

blocks:
  BLOCKED (claude, scope: runner-emits-tx-correlation-hints): Codex approved (c84cfcd). User wants to read through the plan before implementation starts.
```

The plan is BLOCKED. The block creator (claude) is the only one who can clear it. But the per-plan summary says master is waiting on the reviewers (codex, ruthless), with `reason: missing approval from codex, ruthless`. The block is acknowledged later in a "blocks" footer that is easy to miss.

lloyd, 2026-06-05: "it's not it's waiting on whoever created the block to remove the block. blocks should be screaming loudly in clank status. The reason should always be 'blocked'. It doesn't mean that you wake the agent up to remove the block -- of course not that's what the block is for but it should appear that way in things like status."

### Architectural framing — this is a missing-model bug, not a renderer typo

The core model says: a plan that has an open block is in the `Blocked` state. Today nothing in the type system says that. `clank-core`'s `derive_status` produces a `PlanWorkState { plan, sha, gate: CommitGateState, waiting_on: WaitingOn, touched_code }` that is computed purely from the latest reviewable commit + review feedback — `derive_status` has no idea blocks exist (`crates/core/src/wait.rs:228-355`). Blocks are a parallel state stream scanned in the CLI (`crates/cli/src/cli/block.rs:156` `scan_blocks` → `Vec<BlockEntry>`), and the status renderer simply concatenates them at the bottom (`crates/cli/src/cli/status.rs:207-227`) without ever folding them into the per-plan view.

That is the smell. The status output bug — wrong `gate:` / `waiting on:` / `reason:` — is the visible symptom; the cause is that "blocked" is not a representable state of a plan in the model that `derive_status` produces.

The fix is to make blocks first-class in the gate-state computation so the symptom becomes impossible:

- A new variant `WaitingOn::BlockedOnAgent { creator: AgentLabel, block_name: String, message: String }` (in `crates/core/src/plan_view.rs`).
- A new variant `CommitGateState::Blocked` (in `crates/core/src/vocab.rs`), OR — alternative pinned at promotion-time — keep `CommitGateState` describing the *underlying review gate* and surface "blocked" only through `WaitingOn` plus a sibling `PlanWorkState::block: Option<PlanBlock>` field. Trade-off discussed in Phase 1 below.
- `derive_status` learns to accept a block lookup and apply "blocks dominate gate" precedence.

The status renderer then just renders whatever `derive_status` emits. No special-case override at the render layer; no parallel-source-of-truth reconciliation.

Counter-shape (rejected): patch only `status.rs`'s `to_human` to inspect `self.blocks` and rewrite the per-plan lines. This is the post-hoc patch pattern the codebase's review guidance flags as a smell — it keeps blocks-as-a-separate-source-of-truth, leaves `--json` output internally inconsistent (`gate_state: "unreviewed"` while a plan is actually blocked), and doesn't help wfw if it ever wants a single "what's the resolved state of this plan" answer.

## Verified before promotion (2026-06-05)

- Status output is rendered in `crates/cli/src/cli/status.rs:152-230` (`to_human`) and `:92-150` (`to_json`). Both pull `gate` / `waiting_on` from `PlanWorkState` returned by `derive_status` and render blocks separately from `self.blocks: Vec<crate::cli::block::BlockEntry>` (`status.rs:30`, `status.rs:75`).
- `to_human` lines 184-192 unconditionally call `waiting_actor(&v.waiting_on)` and `waiting_reason(&v.waiting_on)` — no awareness of blocks. `waiting_actor` / `waiting_reason` are defined at `:400-453`.
- `WaitingOn` is defined at `crates/core/src/plan_view.rs:22-49`. Five variants today: `ReviewerApprovalsMissing`, `MasterToRevise`, `MasterToContinue`, `MasterToFinalize`, `MasterToCommit`. None mention blocks.
- `CommitGateState` is defined at `crates/core/src/vocab.rs:116-132`. Four variants: `Unreviewed`, `Approved`, `Finished`, `ChangesRequested`. None mention blocks. (Note its `as_str()` is `"unreviewed"`/`"approved"`/etc. — the wire form would gain `"blocked"`.)
- `derive_status` is at `crates/core/src/wait.rs:228-355`. It takes `&impl ReviewLookup` and `&WorkPolicy`. Adding block awareness means extending one of these (likely `ReviewLookup` gets a `blocks_for(plan: &PlanKey) -> Vec<PlanBlock>` method) or threading a new lookup parameter.
- `BlockEntry` is defined at `crates/cli/src/cli/block.rs:147-154` and lives in the CLI crate. To use it inside `clank-core::wait` the type (or a trimmed-down `PlanBlock` projection of it) needs to move to `clank-core`. `BlockEntry` currently has `agent`, `name`, `plan: Option<String>`, `question`, `answer`. The `question` field actually holds the FULL block-message body — see `block.rs:210` `std::fs::read_to_string(file.path())`.
- wfw's block handling is independent of `derive_status`: `crates/cli/src/cli/wfw.rs:404-447` (`check_blocks`) runs its own `scan_blocks` pass, emits `WaitItem::Blocked` items, and suppresses per-plan `Master` / `Reviewer` items via `suppressed_plans`. This is correct as-is and lloyd has explicitly scoped it out.
- The current `clank status --json` shape includes `"gate_state": v.gate` and `"waiting_on": format!("{:?}", v.waiting_on)` (`status.rs:97-103`) plus a sibling top-level `"blocks": [...]` array (`:121-134`). Adding `Blocked` to either enum changes the `gate_state` string value when a block is open; the `waiting_on` Debug string also changes. Flag this as a wire-format change in Acceptance.
- **Wincode cache schema** (ruthless e5d5451 pin 1): `CommitGateState` derives `wincode::SchemaWrite`+`SchemaRead` under `cache-encoding` (`vocab.rs:110-115`). `WaitingOn` does NOT (verified at `plan_view.rs:21-49`). Wincode is position-encoded; the safe addition is to APPEND `Blocked` at the END of `CommitGateState`'s variant list (position 4, after `ChangesRequested`). Existing cached payloads with variants 0-3 remain readable verbatim. The cache layer at `crates/cli/src/state_cache.rs:30` uses a filename-encoded version (`CACHE_FORMAT_VERSION: u32 = 7`, written into `<head>.v<version>.bin`) and self-heals on mismatch via `try_load` (`:73-86`) which removes the offending file before propagating the error. **Pin: bump to `CACHE_FORMAT_VERSION = 8`** in the impl commit so stale v7 caches become orphans that wincode never tries to read; no error-path heroics needed.
- **`derive_status` early-continue on empty reviewable** (ruthless e5d5451 pin 2): `wait.rs:236-238` skips plans with `reviewable_shas().is_empty()`. A plan with intro-only commits + an open block is currently NOT in `derive_status`'s per-plan output. **Pin: move the block-precedence check BEFORE the early-continue** so blocked intro-only plans surface in the per-plan view ("blocked plans should scream loudly" applies even when no reviewable commit exists). Implication: `PlanWorkState.sha` must become `Option<CommitSha>` to represent the "blocked but no reviewable commit" case. Renderer omits the `latest reviewable:` line when `sha == None`.

## Approach

### Phase 1 — Make "blocked" a first-class state of `PlanWorkState` (PINNED)

**Pinned: add the `Blocked` variant to BOTH `WaitingOn` AND `CommitGateState`.** Rationale: this is the architecture-first call — make the misleading "unreviewed + actually blocked" state unrepresentable in the type system. The renderer stays trivial; symptoms become compile-time impossible. The alternative ("keep `CommitGateState` per-commit-pure, surface blocked-ness only via a sibling `Option<PlanBlock>` field on `PlanWorkState`") was considered but rejected: it leaves `gate_state: "unreviewed"` round-tripping through `--json` while the plan is actually blocked. Two sources of truth for "what state is this plan in" is exactly the smell this plan exists to remove.

Concrete schema additions:

- New type in `crates/core/src/plan_view.rs` (next to `WaitingOn`):
  ```rust
  /// Projection of a `BlockEntry` for use inside `derive_status`'s
  /// fold output. The CLI `BlockEntry` (on-disk scan result) stays
  /// as-is; this type carries only what the gate fold needs. The
  /// `creator` field name (vs `BlockEntry::agent`) makes the role
  /// explicit at the projection boundary: this is specifically
  /// the agent that CREATED the block, not "an agent" generically.
  #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
  pub struct PlanBlock {
      pub creator: AgentLabel,
      pub name: String,    // the block's identifying slug
      pub message: String, // full block-body text
  }
  ```
- New variant `WaitingOn::Blocked { block: PlanBlock }`. Serializes as `{"kind": "blocked", "block": {"creator": "<label>", "name": "<slug>", "message": "<body>"}}` via the existing `#[serde(tag = "kind", rename_all = "snake_case")]` attr on `WaitingOn` (`plan_view.rs:21`). Test #7 asserts this verbatim.
- New variant `CommitGateState::Blocked` — **unit variant only** (no payload; block details live on `WaitingOn::Blocked.block`). `as_str()` returns `"blocked"`. Appended at the END of the variant list (position 4) for wincode-schema forward-compat. Precedence documented in rustdoc.
- `PlanWorkState.sha` changes from `CommitSha` to `Option<CommitSha>` (per Pin 2 above) — represents "blocked plan with no reviewable commit yet."

### Phase 2 — Teach `derive_status` to fold blocks in (PINNED)

- **Pinned: extend the `ReviewLookup` trait AND rename it to `PlanStateLookup`** (`crates/core/src/wait.rs:131`). Add `fn blocks_for(&self, plan: &PlanKey) -> Vec<PlanBlock>` with a default impl returning `vec![]` so existing test mocks keep compiling. The rename closes ruthless e5d5451's pin 4 — after adding `blocks_for`, the name `ReviewLookup` is a lie at the call site (`reviews_for` + `blocks_for` is "plan state", not just review state). Cost is ~5 call sites in `clank-core` + 1 production impl (`FsReviewLookup` → `FsPlanStateLookup`) + test mocks. Alternative (separate `BlockLookup` arg) rejected for the same role-fragmentation reason.
- In `derive_status`'s per-plan loop, BEFORE computing the review-driven gate, query `blocks_for(key)`. If a pending (unanswered) block exists, set `gate = CommitGateState::Blocked` and `waiting_on = WaitingOn::Blocked { block: <first pending> }`. Skip the review-gate branch entirely for that plan.
- **Pinned: tie-breaker for multiple pending blocks** = lexicographically-first by `(agent, name)`, matching `scan_blocks`'s existing sort order. Test #4 asserts this.
- **Pinned: repo-wide blocks (`plan: None`) are NOT folded** into `derive_status` per-plan. Status's per-plan section reflects only plan-scoped blocks; repo-wide blocks continue to appear in the `blocks:` footer (with the footer becoming a smaller residual list since plan-scoped ones now surface on their own per-plan line). Rationale: repo-wide blocks are a global thing — surfacing them on every plan would clutter the per-plan summary without adding signal beyond what the footer already provides. wfw's `suppress_all` mechanism for repo-wide blocks remains independent and unchanged. (Ruthless e5d5451 pin 3 — the prior plan body had a contradictory hedge in Out-of-scope; that's removed.)

### Phase 3 — Adapt the CLI lookup

- `FsReviewLookup` at `crates/cli/src/fs_review_lookup.rs` is the production `ReviewLookup` impl that `status.rs:64` constructs. Extend it to scan `.clank/agents/<agent>/blocks/<plan>/*.md` for the requested plan and project unanswered ones into `PlanBlock`. The block-scan logic in `crates/cli/src/cli/block.rs:156-189` (`scan_blocks` + `scan_blocks_in_dir`) is the reference implementation; the new method can call into it or share a helper.
- Watch-mode and the wfw paths construct their own lookups too; verify each call site passes a lookup that includes blocks where the new behavior is desired.

### Phase 4 — Render the new state

In `crates/cli/src/cli/status.rs`:

- `waiting_actor` (`:400-412`) gains a `WaitingOn::Blocked { block }` arm returning `block.creator.as_str().to_string()`.
- `waiting_reason` (`:414-453`) gains a `WaitingOn::Blocked { block }` arm returning `format!("blocked: {}", first_line(&block.message))`.
- **`first_line` helper pinned** (ruthless e5d5451 bonus): sibling free fn in `status.rs` (next to `waiting_reason`). Trims leading whitespace, takes characters up to the first `\n` (exclusive), then truncates to `BLOCK_REASON_MAX_LEN` characters + `…` ellipsis if longer. Constant `BLOCK_REASON_MAX_LEN: usize = 80`. The footer still shows the full untruncated block body.
- **Pinned: uppercase `BLOCKED` in the human `gate:` line only.** Other gate states stay lowercase. This is a renderer-only special case in `to_human` (not in `CommitGateState::as_str()` — that stays lowercase `"blocked"` for the wire form). lloyd's "screaming loudly" directive justifies the inconsistency for this one state. Concrete output:
  ```
  gate:              BLOCKED
  waiting on:        claude
  reason:            blocked: Codex approved (c84cfcd). User wants to read through the plan…
  ```
- The bottom `blocks:` footer (`:207-227`) stays as the audit list. Plan-scoped blocks now appear BOTH on their per-plan line AND in the footer (the footer remains the full chronological audit). No "surfaced on plan: X" hint in v1 — the user can see the plan section above; adding a backref would clutter both renderings.
- **Pinned: `to_json` `waiting_on` becomes structured.** Currently `format!("{:?}", v.waiting_on)` (a `Debug` string, not stable). Take the opportunity to serialize `WaitingOn` directly via its existing serde derive. The Debug-string format was never a stable wire contract — no external consumer is known. Wire-format change flagged in Acceptance + commit message.
- `plans[].gate_state` field will now emit `"blocked"` when applicable (lowercase, via `as_str()` — only the human renderer special-cases uppercase).

### Phase 5 — Verify wfw is unchanged

- `crates/cli/src/cli/wfw.rs:404-447`'s `check_blocks` continues to drive wfw's `Blocked` / `Unblocked` items and `suppressed_plans` exactly as today. wfw's loop emits a `Reviewer` item to a reviewer who hasn't reviewed yet — the underlying review-gate state machine in `work_for` (`wait.rs:357-439`) STILL sees `WaitingOn::ReviewerApprovalsMissing` etc., not `WaitingOn::Blocked`, because `check_blocks` runs alongside (not through) `derive_status`. The wfw flow is: run `derive_status` (gives review-driven items) → run `check_blocks` (gives `Blocked` items + suppresses overlapping plan items). Phase 2's change DOES affect this: if `derive_status` now returns `WaitingOn::Blocked`, the `work_for` arms in `wait.rs:357-439` need a new no-op arm so blocked plans don't accidentally emit a `Master` item with a `Blocked` waiting state.
- **`work_for` Blocked arm pinned** (ruthless e5d5451 bonus + codex 005213d correction): add `(_, WaitingOn::Blocked { .. }) => {}` as the FIRST arm of the match (before the role-specific arms), with comment `// blocked plans emit no work for any role — block-creator clears the block out-of-band`. Catches all roles uniformly. Empty no-op arm (NOT `return`/`return None`) — `work_for` returns `Vec<WaitItem>` aggregated across plans via `for ps in &self.plans` (`wait.rs:358-360`), so this arm just falls through and the loop continues to the next plan. Placement first makes the precedence visible at a glance.
- Net effect on wfw output: identical. Master no longer gets a `Master { next: …, reason: … }` item for a blocked plan (currently it would NOT either, because `suppressed_plans` already drops it — but this change makes the dropping explicit at the model level, not just at the suppression-list level). Reviewers don't get a `Reviewer` item for a blocked plan (same suppression path covers them). Verify both with the existing wfw integration tests pass unchanged.

### Phase 6 — Tests

Unit tests (`crates/core/src/wait.rs` tests module):

1. `compute_gate_unaffected_by_blocks`: `compute_gate` operates on review entries only and stays untouched; the blocked-precedence lives in `derive_status` not `compute_gate`.
2. `derive_status_plan_with_pending_block_returns_blocked_gate`: mock `ReviewLookup` returns one plan with a pending block + a review set that would otherwise be `Unreviewed`. Assert resulting `PlanWorkState.gate == Blocked` and `waiting_on == WaitingOn::Blocked { creator: <expected> }`.
3. `derive_status_plan_with_answered_block_uses_review_gate`: same plan with an unblock answer present → falls back to the normal review-driven gate.
4. `derive_status_picks_first_pending_block_when_multiple`: two pending blocks on same plan → lexicographically-first wins. Pin tie-breaker.
5. `work_for_blocked_plan_emits_no_master_or_reviewer_items`: `WaitingOn::Blocked` arm in `work_for` returns nothing for both roles.

Integration test (new file `crates/cli/tests/status_blocked_plan_integration.rs` — pattern adapted from `block_create_scope_integration.rs` + `log_and_status_integration.rs`):

6. `status_shows_blocked_gate_and_creator_for_plan_block`:
   - init repo, intro plan `foo`, add agent `claude` (master), add expected reviewers `codex` + `ruthless`.
   - `clank block create --plan foo blah --message "wait — checking the design"` as claude.
   - `clank status` stdout assertions:
     - contains `gate:              BLOCKED` (or `gate:              blocked` depending on Phase 4 pick).
     - contains `waiting on:        claude`.
     - contains `reason:            blocked: wait — checking the design`.
     - does NOT contain `missing approval from codex, ruthless`.
     - the `blocks:` footer still lists the BLOCKED entry.
7. `status_blocked_plan_json_emits_blocked_gate_state`: same setup, `clank status --json`, parse, assert:
   - `plans[0].gate_state == "blocked"` (lowercase wire form).
   - `plans[0].waiting_on == {"kind": "blocked", "block": {"creator": "claude", "name": "blah", "message": "wait — checking the design"}}` (structured serde, not a `Debug` string).
   The exact JSON shape is locked in here so the wire-format migration is testable end-to-end.
8. `status_unblocked_plan_returns_to_review_gate`: same setup then `clank unblock claude blah --plan foo --message ok` → `clank status` shows the original `gate: unreviewed` / `waiting on: codex, ruthless` shape.
9. `status_two_pending_blocks_on_same_plan_picks_first` (**REQUIRED**, not optional — ruthless e5d5451 pin 5): lexicographic order is pinned in Phase 2; a pinned property without a defending test is aspirational.

Existing tests to re-run untouched:

- `crates/cli/tests/log_and_status_integration.rs` — should pass without modification.
- `crates/cli/tests/wfw_integration.rs` + `wfw_optional_flags_integration.rs` — wfw output must be byte-identical.
- All `crates/core/src/wait.rs` `compute_gate_*` tests — `compute_gate` is unchanged.

## Out of scope

- Changing how blocks are CREATED, ANSWERED, or LIFECYCLE-MANAGED. `clank block create` / `clank unblock` / `clank block clean` keep their semantics.
- Changing wfw's wake / suppress behavior. lloyd was explicit: blocks dominating `status` does NOT mean wfw should wake reviewers or the block-creator to "clear" the block. wfw stays exactly as today.
- The stop-hook's `Blocked` hook event firing. That fires at block-create time (`crates/cli/src/cli/block.rs:54-69`) and is unrelated to status display.
- Repo-wide (`plan: None`) blocks changing every plan's `gate:` line. **Pinned in Phase 2**: only plan-scoped blocks flip per-plan `gate:`; repo-wide blocks remain in the `blocks:` footer (alternative explicitly rejected — see Phase 2 rationale).
- TUI / HTML rendering changes outside `clank status`'s human + json paths.
- Renaming `CommitGateState` if Phase 1 pursues the "don't extend `CommitGateState`" alternative — that's a follow-up plan, not in this scope.

## Acceptance

- `WaitingOn` gains a `Blocked { block: PlanBlock }` variant. Serializes as `{"kind": "blocked", "block": {...}}`.
- `CommitGateState` gains a unit `Blocked` variant **appended at the end** of the variant list (wincode position 4). `as_str()` returns `"blocked"`.
- `PlanWorkState.sha` becomes `Option<CommitSha>` to represent blocked plans with no reviewable commit.
- `PlanBlock` type added in `clank-core::plan_view` with derives `Debug, Clone, PartialEq, Eq, Serialize, Deserialize`.
- `ReviewLookup` trait renamed to `PlanStateLookup`; gains `blocks_for()` with a default impl returning `vec![]`. `FsReviewLookup` → `FsPlanStateLookup`. Both renamed consistently across `clank-core` and `clank-cli`.
- `CACHE_FORMAT_VERSION` bumped from 7 to 8 in `crates/cli/src/state_cache.rs` (forces stale v7 caches to become orphans on filename mismatch — no error-path heroics).
- `derive_status` returns `gate == Blocked` and `waiting_on == Blocked { block: … }` for any plan with at least one pending plan-scoped block.
- `compute_gate` (the per-commit pure function) is UNCHANGED. Block precedence lives in the multi-plan fold, not the per-commit verdict computation.
- `work_for` returns nothing for a blocked plan regardless of role.
- `clank status` human output for a blocked plan:
  - `gate:` line displays the blocked state prominently.
  - `waiting on:` shows the block CREATOR's label, not reviewers.
  - `reason:` starts with `blocked:` followed by the first line of the block message.
  - the `blocks:` footer continues to list the same entries (no regression).
- `clank status --json`:
  - `plans[].gate_state == "blocked"` when applicable.
  - `plans[].waiting_on` becomes a structured value via serde (was a `Debug` string). Flag this as a wire-format change in the commit / plan finalization message; no external consumer of this field is known.
  - `blocks` top-level array stays as today.
- `clank wfw` (human and json) output is byte-identical to today for every test in `wfw_integration.rs` + `wfw_optional_flags_integration.rs`. Add a new wfw test that pins "blocked plan + reviewer who hasn't reviewed yet → no `Reviewer` item, just the `Blocked` item" (this is true today via `suppressed_plans` — the test pins it survives the model change).
- All existing tests across `clank-core` and `clank-cli` pass with no modifications other than mock `ReviewLookup` impls picking up a default `blocks_for` (the default returning `vec![]` keeps them compiling).
- `cargo test --workspace` passes.

## Tests

(Listed by phase above. Quick index:)

- Core unit: `compute_gate` unchanged tests pass; new `derive_status_*` blocked-precedence tests; new `work_for_blocked_plan_emits_nothing` test.
- CLI integration: new `status_blocked_plan_integration.rs` covering human + json + unblock-restores-review-gate + multi-block tie-breaker.
- Regression: existing `log_and_status_integration.rs`, `wfw_integration.rs`, `block_create_scope_integration.rs` unchanged.

## Related

- `clank-open-zellij-layout-file` (FINISHED `841cdf2`): unrelated, no overlap.
- `agent-start-initial-prompt` (queued `500-agent-start-initial-prompt.md`): unrelated, no overlap. This plan touches `core/wait.rs` and `cli/status.rs`; agent-start touches `core/agent_config.rs` and `cli/agent.rs`.
- The architectural framing here echoes the "Architecture-First Reviews" directive in `~/.claude/CLAUDE.md`: the user-visible bug (wrong `waiting on:` text) is a symptom of blocks not being a first-class state in the gate fold. Lead with the model fix, not a render-layer patch.
