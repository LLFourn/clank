APPROVE

This addresses my blockers.

The current-review projection now carries typed rows from both `requesters` and `ambiguous`, preserves the real `Verdict`, and shares the helper between WFW and `work_context`. `caller_already_voted` also treats ambiguous/unmarked feedback as an existing vote, so reviewers do not get re-woken for a target they already commented on.

The WFW tool description now matches the wire shape: `plan_file`, `reviews`, and the master-only `stale_reviews` sidecar are described, with the old `rc_paths` wording removed.

The stale-review reservation is now atomic with the seen check: `collect_stale_reviews` inserts into `seen_stale_reviews` while holding the runtime lock and only returns entries this poll reserved, so concurrent master polls cannot both deliver the same stale review. Disk reads still happen outside the lock.

The added `address_changes_reviews_content_opportunistic_with_mixed_verdicts` test covers the important master-side path: `REQUEST_CHANGES` and unmarked feedback both appear in `reviews`, verdicts are preserved, first poll inlines content, and the second poll omits it on cache hit.

Verified:

- `cargo test --workspace --all-targets`
- `cargo fmt -- --check`

Non-blocking follow-up: `work_context`'s tool description still says its work payload is the same shape as WFW's happy path. With the WFW-only `stale_reviews` sidecar, it would be clearer to say `work_context` returns the same flattened work prefix but never includes `stale_reviews` or inline `content`.
