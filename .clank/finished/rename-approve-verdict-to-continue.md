# rename-approve-verdict-to-continue

Rename the clank review verdict **APPROVE → CONTINUE** everywhere user- and
code-facing. The CLI rejects `approve` (agents learn from the rejection); only
the on-disk feedback-file parser still reads legacy `APPROVE` (read-compat for
review files written before the rename). The "dumb first" fix for
reviewers picking APPROVE when they mean FINISHED (a codex got stuck doing
exactly this). "APPROVE" carries a ship-it/sign-off connotation, so a reviewer
who judges the work *good* reaches for it — precisely when the work is *done*
and the verdict should be FINISHED. "CONTINUE" encodes the mid-flight meaning in
the name: a reviewer who thinks the plan is finished won't write "continue."
Clean trichotomy with no overlapping connotations: **CONTINUE** (good, more to
do) / **FINISHED** (good, done) / **REQUEST_CHANGES** (not good). Try this before
the heavier master-declares-finished machinery; measure whether the confusion
recurs.

## The rename set (names open to review)

- `Verdict::Approve` → `Verdict::Continue` (`crates/core/src/vocab.rs:84`).
- Gate states associated with the verb (lloyd: rename these too):
  - `CommitGateState::Approved` → `Continued` (vocab.rs:130; doc "≥1 reviewer
    voted APPROVE").
  - `CommitGateState::ApprovedPendingGate` → `ContinuedPendingGate` (vocab.rs:140).
  - `WaitingReason::GateApproved` → `GateContinue` (vocab.rs:174; doc "is APPROVE
    … master keeps working" — literally the continue state). Its `as_str`
    `"gate_approved"` → `"gate_continue"` surfaces in the agent wake hint.

## Back-compat — the load-bearing part (the `wfw_timeout` lesson)

The verdict is **persisted on disk** in `agents/<label>/feedback/<sha>.md`: the
first line is the verdict header (`APPROVE`, …). Existing files say `APPROVE`.
So the alias MUST live on the READ/parse path, not just CLI input:

- **`parse_verdict` (`crates/core/src/feedback_body.rs:38,53`)** — currently
  `l == "APPROVE" || l.starts_with("APPROVE ")` → `Approve`. Accept BOTH
  `CONTINUE`/`CONTINUE ` (new) AND legacy `APPROVE`/`APPROVE ` → `Verdict::Continue`
  (and the summary-extraction at :53). Without this, every historical feedback
  file mis-gates.
- **PR-review `ReviewerVerdict::parse` (`crates/core/src/pr_review.rs:135`)** —
  the SAME shape on the PR-review path: a `reviews/<label>.md` frontmatter
  `verdict:` value. `ReviewerVerdict` is NOT serde-derived (it has a custom
  text parser/renderer), so the legacy `approve` alias lives in that parser
  (`"continue" | "approve"` → `Continue`), NOT in a serde alias. Without it an
  in-flight PR review that recorded `verdict: approve` pre-rename loses its
  verdict.
- **Write side (`crates/cli/src/cli/feedback.rs:29,168`)** — the header clank
  WRITES becomes `Continue => "CONTINUE"`. New files say `CONTINUE`.
- **CLI `--verdict`** — `continue` is the ONLY positive value; NO clap alias
  (lloyd: agents that type `approve` learn from the rejection). `clank feedback
  write --verdict approve` now errors. (Input-compat is dropped; on-disk
  read-compat below is the only back-compat kept.)
- **serde — break it, NO aliases (lloyd).** The serde wire strings (`Verdict`,
  `CommitGateState`, `WaitingReason` all derive `rename_all = "snake_case"`) are
  render-only / computed-fresh — confirmed there is NO deserialize-from-persisted
  path (`WaitingReason` is built by the gate logic and only rendered to the wake
  hint / html; nothing reads it back). So `approve → continue`,
  `approved → continued`, `gate_approved → gate_continue` change freely; just
  update the wire assertions (`crates/core/tests/round_trip.rs:63`, etc.). No
  `serde(alias = …)` anywhere — the ONLY back-compat is the on-disk parsers
  (`parse_verdict` for gate feedback + the PR-review frontmatter parser), not
  serde and not the CLI.
- **wincode (state cache)** — `Verdict` and `CommitGateState` derive
  `wincode::SchemaRead/Write`, which is POSITION-encoded. Renaming a variant in
  place keeps its position, so cached payloads stay decodable — do NOT reorder
  variants. (No `CACHE_FORMAT_VERSION` bump needed; the rename is name-only.)

Pin BOTH read-path aliases with tests: a gate feedback body of
`"APPROVE\n\nLGTM\n"` parses to `Verdict::Continue`
(`parse_verdict_legacy_approve_is_continue`), and a PR-review frontmatter
`verdict: approve` parses to `Verdict::Continue`
(`verdict_continue_parses_and_legacy_approve_is_continue`). (No CLI-alias test —
the CLI deliberately rejects `approve`; no serde-alias test — there is no serde
verdict path to alias.)

## Out of scope — do NOT rename

- **The GitHub PR-review `approve` event** (`pr_review_skill.md`, the
  `clank pr-review --event approve` mapping to `gh pr review --approve`). That's
  GitHub's vocabulary, a different surface from the clank gate verdict. Leave it.

## Sweep (one up-front full-repo grep for `approve`/`APPROVE`/`Approve`)

- Skills: `skill_reviewer.md`, `skill_shared_core.md`, `skill_slash_command.md`
  — APPROVE → CONTINUE for the clank verdict, and tighten the CONTINUE vs
  FINISHED definitions (CONTINUE = good-but-not-done; FINISHED = done).
- `crates/core/src/vocab.rs` (enums + `as_str`), `feedback_body.rs` (parse),
  `feedback.rs` (write headers + render), `wait.rs` + `status` hint rendering,
  `stop_hook` wake text, README / RELEASE-CHECKLIST.
- Tests: mechanical `Verdict::Approve` → `Verdict::Continue`; update assertions
  on `"approved"`/`"gate_approved"` strings. KEEP: the legacy-feedback back-compat
  test (on-disk `APPROVE` → `Continue`) and the PR-event `approve` (GitHub). NO
  CLI-alias test — the CLI rejects `approve`, and per no-tests-for-removed we do
  NOT add a test asserting the dropped alias / old variant is "gone" (clap + the
  framework enforce absence). Test the kept on-disk read path instead.

## Rollout

Non-breaking for HISTORICAL files: the new binary reads legacy `APPROVE` via the
parse alias. The only mixed-version window is a NEW reviewer (new binary) writing
`CONTINUE` while an OLD long-running master/wait process (old binary) reads it
and doesn't recognize it → transient mis-gate, self-heals on restart, no data
loss. Mitigate by restarting the live agents after install (the stale TUIs need
restarting anyway). After `cargo install`, run `clank setup --force` to re-sync
the master/reviewer skills. (If intro review judges the window too risky, the
conservative variant is to keep WRITING `APPROVE` for one release while reading
both — flag for the reviewers.)

## Acceptance

- The clank verdict is `CONTINUE` across skills, CLI, code, docs. `approve` is
  accepted ONLY by the two on-disk parsers — `parse_verdict` (gate feedback) and
  `ReviewerVerdict::parse` (PR-review frontmatter) — each pinned by a legacy-
  `approve` test. The CLI `--verdict approve` is rejected by clap (no alias —
  enforced structurally, not a separate test, per no-tests-for-removed); there
  is no serde verdict path, so no serde alias.
- A legacy `APPROVE` feedback file parses + gates identically to `CONTINUE`.
- Gate states / waiting reason renamed to track the verb; wincode positions
  unchanged so caches stay valid.
- GitHub PR-review `approve` event untouched.
- Skills re-synced after install; live agents restarted to clear the version skew.
