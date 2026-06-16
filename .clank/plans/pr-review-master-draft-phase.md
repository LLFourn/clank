# pr-review-master-draft-phase

A freshly-started PR review shows `👀 CODEX reviewing pr #n` in
`clank status` the instant `clank pr-review start` runs — before
master has drafted a single comment. Summoning a reviewer to review
nothing. The fix is structural: model the master-first "draft, then
hand off" phase that a PR review has and a plan doesn't.

## The architectural mismatch

The model a PR review claims: each round, **master produces/updates
the draft, then reviewers review it**, then master integrates —
until converged → submit. Master acts first; the reviewable
artifact (the GitHub pending-review draft) is master's output.

What the code does: the PR path reuses the **plan** gate engine
(`compute_gate` + `missing_for_gate`, `wait.rs:606`/`:633`)
verbatim. That engine assumes the reviewable artifact exists the
moment the work-unit exists — true for a plan (the commit exists
before reviewers are summoned), FALSE for a PR review (master's
draft doesn't exist at `start`). There is no "master is drafting /
hasn't handed off yet" state.

Concretely at `start`: round 0, no verdicts → `compute_gate([], …)`
= `Unreviewed` → `missing_for_gate` returns the whole commit tier
= `[codex]` → `pr_bar_text` (`status_tui.rs:355`) shows the first
missing reviewer "reviewing".

The confirming smell: **nothing ever advances the round.** No
handoff verb exists (the `propose`/open step was deferred — the
integration test fakes the bump by hand-editing `pr.json`,
`pr_review_integration.rs:233`). So today:
1. Nothing distinguishes "master preparing round N" from "round N
   open for review".
2. The whole round mechanism (stale-approval, freeze,
   convergence-per-round) is DEAD CODE — round is permanently 0.
3. Reviewers are summoned at t=0 (the visible bug).

All one missing state. Plans gate reviewer-summon on a commit
existing; PR reviews gate it on nothing.

## Design: the round counter is the handoff signal

Make `round` the single source of truth for "has master handed this
off for review" — do NOT add a separate `phase` field (a second
source of truth that can drift from `round`). Define:

- **round 0** = master drafting; review never opened.
- **round ≥ 1** = the Nth review pass is open.

### Core (`wait.rs`)
- `derive_status` PR loop: when `input.round == 0`, force
  `missing_reviewers = []` (master's drafting turn — reviewers not
  summoned). At round ≥ 1, behave exactly as today. Keep the gate
  value as computed (`Unreviewed`); round, not gate, is the
  "opened?" signal — note this where it's read.
- `work_for` Master arm (`wait.rs:779`): when `pr.round == 0`, emit
  `PrMaster { next: Draft }` (new `PrMasterNext::Draft` variant)
  before the gate match. The existing gate match (Integrate/Submit/
  Continue) is unchanged and only reached at round ≥ 1.
- The reviewer arm needs no change: it keys off `missing_reviewers`,
  which is now empty at round 0.

Why round 0 is the only special case: after a reviewer requests
changes at round N, master integrates (gate `ChangesRequested` →
`PrMaster::Integrate`, already works) then `propose` bumps to N+1;
the old verdicts go stale (`reviewed_round = N < N+1`) so reviewers
are correctly re-summoned. The initial draft is the sole new
master-turn.

### CLI: the `propose` verb (`pr_review.rs` + `mod.rs`)
- `clank pr-review propose` — MASTER ONLY (like submit/abort).
  Bumps `state.round` by 1 (0→1 opens the first review pass; N→N+1
  re-opens after an integration) and persists `pr.json`. This is
  the deferred handoff the round machinery always implied.
- Wire `PrReviewCmd::Propose` in `mod.rs` and dispatch in `main.rs`.
- It's master asserting "draft is ready" — the local model can't
  (and shouldn't) verify GitHub state; consistent with master
  choosing when to commit a plan.

### Rendering (`status_tui.rs` + `status.rs`)
- `pr_bar_text`: at round 0 (master turn, gate not Finished/
  ChangesRequested) show a "drafting" verb rather than the generic
  "refining", so the bar reads e.g. `🔨 CLAUDE drafting pr #n`.
- `to_human` PR line (`status.rs:353`): a round-0 review reads as
  master drafting / awaiting `propose`, not "waiting on <reviewer>".

### PR URL in status, clickable (`wait.rs` + `status*.rs`)

`clank status` should surface the PR's full URL —
`https://github.com/<slug>/pull/<n>` — in both the text and `--tui`
surfaces, made clickable where the terminal supports it.

- Carry the slug: `PrReviewWorkState`/`PrReviewInput` currently have
  the PR number but not the `repo` slug (it lives in `pr.json`'s
  `repo`). Add `repo: String` to both (plain data) and populate it
  in `pr_review_inputs` (`pr_review.rs`). Core stays URL-agnostic —
  it carries the slug; the CLI rendering layer formats the github
  URL (a small `pr_url(slug, n)` helper in the CLI, NOT core, so
  github-URL knowledge doesn't leak into `wait.rs`).
- Text (`to_human`): print the full URL on the PR line. A bare URL
  is auto-linkified by most terminals; no escapes needed there.
- TUI: render the full URL on its OWN body line (like the `git` /
  `dirty` gauges — the bar stays compact with `pr #n`), wrapped in
  an **OSC 8 hyperlink** so it's clickable:
  `\x1b]8;;<url>\x1b\\<text>\x1b]8;;\x1b\\`. zellij passes OSC 8
  through to the host terminal (iTerm2/kitty/WezTerm/VTE). The link
  TARGET is always the full URL even if the visible text is
  truncated to the pane width, so a click still works on a narrow
  pane.
- Width/visibility plumbing (the real work): the TUI's `emit`
  width-truncation and the `visible()` test helper only strip CSI
  `\x1b…m` sequences today. OSC 8 (`\x1b]8;;…\x1b\\`, ST- or
  BEL-terminated) is zero display-width and MUST be skipped by the
  width accounting and stripped by `visible()`, or truncation math
  and every snapshot test break. Treat this as a `Span` style
  (e.g. `Style::Link(url)`) so `emit` owns the escape and the rest
  of the renderer stays unaware — don't hand-inject raw OSC 8 into
  span text.

### Skill (`setup_assets/pr_review_skill.md`)
- Master loop: after drafting the initial top-level comments, run
  `clank pr-review propose` to open the review (this is what
  summons reviewers). After integrating reviewer replies, `propose`
  again to re-open at a new round. Add `propose` to the verbs list.
- Reviewers are summoned only once master has proposed.
- Bump the content-guard test's pinned verb set accordingly.

## Testing

In-process, no clank-binary spawning (git/gh spawns fine):
- Pure (`wait.rs` tests): round 0 → `missing_reviewers` empty and
  `work_for(master)` yields `PrMaster{Draft}`, `work_for(reviewer)`
  yields nothing. round 1, no verdicts → reviewers summoned (today's
  behavior preserved). Integrate→propose→round 2 re-summons.
- Integration (`pr_review_integration.rs`): `propose` is
  master-only (reviewer call errors), bumps the round, and is
  reflected in `pr_review_inputs`. Replace the hand-edited round
  bump in `pr_review_inputs_drop_stale_round_verdicts` with a real
  `propose` call.
- Render: a round-0 `PrReviewWorkState` renders master "drafting",
  NOT a reviewer "reviewing" (this is the reported bug — pin it).
- URL: text `to_human` contains `https://github.com/<slug>/pull/<n>`;
  TUI emits the OSC 8 sequence with the full URL as target, and
  `visible()` strips it so the visible text + width assertions still
  hold (assert both the raw escape and the cleaned text).

## Non-goals

- No GitHub-side verification that master actually posted comments
  before `propose` (core does no IO; resolve-on-demand stands).
- No new gate state — `compute_gate` stays the shared plan engine;
  round is the PR-only "opened" signal layered on top.
- Auto-proposing / collapsing `propose` into another verb — keep
  the handoff explicit and master-driven.
