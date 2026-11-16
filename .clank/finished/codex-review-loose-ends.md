# codex-review-loose-ends

## Why

These are smaller findings from the review of codex's 13 commits (6f031d48..9af10ef3) that no other plan covers.

## Build

1. The outage notice (`OutageNotified`, now `bound.outage` in `state.json`) was written under two meanings: "this session was told there is no monitor" and the guard against telling it twice. Establish whether `one-state-file-per-agent` left one meaning; if two remain, split them into named parts, otherwise remove the one nothing reads.
2. Dead code:
   - `unmonitored_verdict`
   - the `MonitorOutage` and `MonitorUnanswered` notices
   - `Announcement::Undelivered`
3. Stale text:
   - the OpenCode work-loop text (`WORK_LOOP_OPENCODE` in `setup.rs`), which still says the plugin long-polls
   - the README's monitor and block-delivery paragraphs that the blocks and one-state-file plans changed
   - the github skill text that the blocks and watch plans made wrong
4. CHANGELOG for the next release:
   - the outage behaviour change in 0.2.0
   - the Grok `hooks/clank.json` timeout drift, which makes existing Grok hook files report as drifted
5. The Codex research diagnostics (77cfdeb3, 06447442):
   - tests in `tests/support/codex_completion.rs` and `codex_tui.rs` that report a negative they never assert (e.g. a PostToolUse absence) either assert it or are deleted;
   - the parked cases that measure something structurally impossible are deleted;
   - the fork probe (`installed_codex_tui_fork_probe`) uses the production composer or is deleted.

## Acceptance

- Each item is fixed or deleted.
- Each deleted diagnostic is listed in the commit message with the reason.
