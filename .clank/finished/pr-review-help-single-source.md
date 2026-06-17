# pr-review-help-single-source

The `clank pr-review` command summary hand-maintains its subcommand
list, which duplicates clap's auto-generated table and drifts: it
reads `start|note|abort|status` and never picked up the `propose`
verb. lloyd's call (block `pr-review-help-summary-stale`): option B —
don't hand-maintain the list; let clap be the single source of
truth.

## Change

`crates/cli/src/main.rs:45-46`, the `PrReview` command doc comment:

```
/// Run the multi-agent review loop against a GitHub PR
/// (`clank pr-review start|note|abort|status`).
```

Drop the parenthetical verb list so it's just:

```
/// Run the multi-agent review loop against a GitHub PR.
```

clap already prints the full, current subcommand table under the
summary (`start`, `propose`, `note`, `abort`, `submit`, `status`),
so the hand-list was a second, drifting source of truth — removing
it is the fix, not adding `propose` to it.

## Scope / notes

- Doc-comment only; no behavior change.
- Quick scan confirms `pr-review` is the ONLY command variant in
  `main.rs` that hardcodes a subcommand list — this is a one-line
  edit, not a sweep.
- No test: there's nothing to pin (the subcommand list is clap's,
  generated). The point is the ABSENCE of a duplicated list.
