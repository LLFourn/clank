# status-warns-only-about-amendable-head-problems
# Status warns only about amendable HEAD problems

## Problem and outcome

The status TUI repeatedly shows historical warnings such as ignored plan introductions from the former multi-plan workflow. These cannot be fixed by amending the latest commit and obscure current actionable work. Keep historical diagnostics available without presenting them as current warnings.

Implement the complete plan in one implementation commit, without internal named
milestones.

## Contracts

- A current status warning about a commit appears only when the latest commit is problematic and amending that commit can resolve the problem. Define and test this rule explicitly; do not merely hide all diagnostics.
- Separate history reconstruction diagnostics from current actionable status. Preserve deterministic interpretation of old histories, including ignored introductions, without rewriting repository history as a prerequisite.
- Retain historical evidence in a deliberate history/log diagnostic surface, including the responsible commit and explanation. It must not flood the ordinary TUI or ordinary status output.
  Provide an explicit `clank log --diagnostics` view with human and JSON output;
  respect its commit range and plan filter, and retain ordinary log behavior when
  the option is absent. Use existing typed diagnostic provenance rather than
  message-text matching or a second history interpretation.
- Apply the rule consistently to TUI and CLI status. If the underlying problem was introduced earlier and cannot be repaired by amending HEAD, it is historical information rather than an amend-HEAD warning. Keep genuine present operational failures distinct from these commit diagnostics.
- Do not suppress actionable latest-commit problems, change review gates, reinterpret active plans, or lose diagnostics needed to investigate malformed history.
- Warning text in the TUI uses a stable warning style determined by its meaning,
  never the current repository, plan or gate state color. Prevent inherited parent
  styles from recoloring warning labels or message text when state changes. Keep
  the warning readable in supported themes and retain state colors on the actual
  state indicators.

## Implementation and validation

Trace warning production through history replay and status rendering. Give diagnostics enough provenance and repair semantics to select amendable HEAD warnings at the presentation boundary, rather than matching message strings. Cover old overlapping introductions, valid subsequent commits, a malformed latest introduction, a corrected/amended HEAD and equivalent CLI/TUI behavior. Verify the existing Clank history no longer produces the quoted historical warning flood while history diagnostics remain inspectable. Document the model and migration-free behavior.

Verify that the same warning keeps the same text style across repository and gate
state transitions, including continued, blocked, dirty and ready states. Include
the warning label and wrapped message text; neither may inherit the state color.

## Implementation

`RepoWarning::is_amendable_at` defines the typed presentation contract: a rejected
introduction is a status warning only at its own HEAD. A dangling reference is
projection history rather than an amendable introduction. Status filters at
snapshot construction, so human output, JSON and TUI share the selection. Fold
warnings and ignored-run provenance remain untouched, as do review gates, tag
corrections and operational errors.

`clank log --diagnostics` is an explicit diagnostic-only view. Its human output
includes full commit IDs and explanations; JSON retains the existing typed
records. Commit ranges, limits and plan filters apply. The shared checkpoint/range
fold captures newly emitted diagnostics before later successful introductions or
deletions retire their workflow provenance. This query includes malformed first
introductions before adoption. Ordinary log retains its existing adoption floor
and output, and ordinary rebuilds do not collect an additional archive. There is
no cache format change, idle polling, new persisted state or history rewrite.

Both the warning label and every wrapped message span reset inherited styling
and select the terminal's ANSI warning palette color. Repository and gate hues
remain on state indicators. README documents the model and diagnostic command.

## Validation

- Five status/log integration tests pass. They exercise current overlapping
  introduction warnings, subsequent valid commits, cold/warm cache agreement,
  malformed multi-introduction HEAD before adoption, actual HEAD amendment,
  later successful reintroduction retiring fold provenance, and diagnostic
  human/JSON, range, limit and plan filtering.
- Core repo-state tests: 41 pass. Status tests: 35 pass. Log tests: 24 pass.
  Shared replay/cache tests: 13 pass, including read-only range replay and stable
  snapshots not churning the cache.
- TUI suite: 471 pass in the sandbox; ten socket-dependent remote tests encounter
  sandbox permission errors. The complete remote subset (16 tests) passes when
  rerun with local socket permission. The new warning tests pass, including
  byte-identical label and wrapped-text styling across idle, continued, dirty,
  ready and blocked states while state indicator hues still differ.
- Running the new commands against Clank's actual history produces zero current
  introduction warnings in ordinary status and 31 inspectable historical
  rejection records in the diagnostic view.
- Formatting and diff whitespace checks pass. Installation remains deferred to
  the reviewed plan's finalization.
