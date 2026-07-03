# tui-spinner-on-agent-rows
# Activity spinner moves to the AGENTS rows; the log becomes pure history

## Goal (lloyd)

In `clank status --tui` the ACTIVE agent should be visible in the AGENTS
section: put the spinner there, and remove the in-progress placeholder rows
from the log. Activity lives where the actors are; the log shows what
HAPPENED, not what's pending.

## Today

`in_progress_rows` (scroll.rs) synthesizes placeholder rows from the active
plan's `WaitingOn` — a spinner row per pending reviewer, one for a producing
master — and `build_scroll` SPLICES them into the log at their final slots
(master after the plan header; pending reviews merged into the latest
commit's review block). The AGENTS rows show only `auto_mark label tier` —
no activity.

## Change (validated by a working-tree prototype lloyd previewed)

1. **AGENTS rows carry the activity.** For each agent whose label matches an
   `InProgress` item (reviewer label, or master's name for
   `MasterWorking`), append the braille spinner + italic wait-verb to their
   row: `▶ codex  commit  ⠧ reviewing…`. Same `spinner_glyph(frame)` +
   italic-verb styling the log placeholders used ("what we await" stays
   italic). `frame` is already a `render_at` parameter.
2. **The log stops splicing placeholders.** `build_scroll` no longer emits
   `Seg::InProg`; the review-block merge machinery for pending reviewers
   goes with it (`merge_review_block`'s pending half, `Seg::InProg`, and
   `in_progress_spans` become dead — DELETE them rather than leaving a
   false model in the file; `InProgress`/`in_progress_rows` stay, now
   feeding the panel).
3. **The animation gate follows the panel.** `spinner_visible` currently
   asks build_scroll whether an InProg seg is inside the log WINDOW; the
   panel is always on screen, so it becomes
   `!in_prog.is_empty() && !snapshot.agents.is_empty()` — no window math.

## Known trades (state them; reviewers weigh in)

- The old placeholders showed WHERE a pending ✓/✗ would land in the
  timeline and were replaced in place when the verdict arrived. That
  where-it-lands preview is lost; the panel is the single "who's active"
  surface. (Middle path if wanted later: a static dim `·` slot in the
  review block — NOT part of this plan.)
- Teamless repos (no agents panel) lose the in-progress display entirely
  — the log carried it before. Acceptable: activity presupposes a team;
  a teamless repo has no reviewers to wait on and master is the human.
  Note it in the commit.
- The scroll sequence shrinks (no placeholder rows), so cursor indices/
  overlay targeting shift by a row or two — `build_scroll` remains the
  single source for both render and Enter-targeting, so they cannot
  disagree.

## Tests (pure layer)

- Agent row spans: an active reviewer's row carries the spinner + verb;
  an idle agent's row is unchanged; master's row spins with the producing
  verb when `MasterWorking`.
- `build_scroll` emits NO `Seg::InProg` for a snapshot with pending
  reviewers/working master (and the seq length matches history rows only).
- The spinner gate: active + roster → animate; idle or teamless → slow
  tick. (The gate expression is loop-side; test the pure predicate if
  extracted, else pin via existing loop tests' shape.)
- Update/remove the existing placeholder tests (`in_progress_rows`
  spliced-position tests, `merge_review_block` pending cases) — the
  invariant they pinned ("a waiting state is never invisible") moves to
  the agent-row test.

## Acceptance

- With a plan mid-review: pending reviewers' AGENTS rows spin with
  `reviewing…`/`gate-reviewing…`; on master's turn, master's row spins
  with the producing verb; the log shows no placeholder rows.
- Idle repo: no spinner anywhere, slow tick preserved.
- clippy at baseline; status_tui suites green.
