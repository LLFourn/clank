# A page is bounded in size, not in time

## Why

`a-page-nobody-can-open-is-not-a-page` bounded what a commit page
CONTAINS. Nothing bounds what building one COSTS, and on the same
repository that produced the 166 GB that cost is now the visible one.

Measured on `~/src/penlock-experiment`, commit `840907e` — 48,199
changed paths (39,413 deletions, 8,541 additions, 245 modifications):

- The metadata pass reads 5.0 MB. Cheap, and bounded by file count.
- Deletions and binaries are never fetched. Also cheap.
- The rest are fetched one subprocess each. Those patches average
  1,114 source bytes, about 6.7 KB rendered, so the 4 MiB ceiling
  fills at roughly **628 files** — and every one of them is fetched
  before the ceiling stops the page.
- A fetch costs **5.5 ms warm** and **~98 ms cold** (first read of
  that blob from disk). So one page costs ~3.5 s warm and **~60 s
  cold**.

The TUI hosts the remote in-process and rebuilds per snapshot, and the
cold case is exactly what follows the `rm -rf` this whole line of work
was triggered by.

## What the cost is NOT, with numbers, so nobody re-derives them

- **Not the pathspec tree walk.** `git show <sha> -- <path>` diffs the
  whole commit and filters, so it looked O(files) per call. Measured
  against the blob-to-blob form on equally cold paths: **5.6 ms**
  pathspec vs **5.1 ms** for `git diff <oldsha> <newsha>` using the
  blob shas `--raw` already hands us. No material win — and it would
  put patch identity back on a sha pair rather than the path `--raw`
  states, which is the hazard the previous plan closed. Rejected.
- **Not mostly process spawn, when it matters.** A bare `git` spawn is
  **3.5 ms**, so for a WARM build most of the 5.5 ms is process
  startup and batching would help. Cold, content I/O is 20x that and
  batching helps nothing. The cold case is the one that hurts, so
  batching is not the fix either.

What is left is the only real lever: **fetch fewer files.**

## The shape of the fix

The byte ceiling already guarantees that a commit touching 48,199
files cannot show them all. So the number of files a page will fetch
is knowable up front, and capping it bounds build time by
construction:

- A cap on how many files one page ATTEMPTS to fetch — attempts, not
  successes, because an error, an empty result and an unparsable one
  each cost the same subprocess as a patch that renders. Counted
  before the call, therefore. Past the cap, files take the
  summary-row path they already have.
- Skips must NOT spend it: a deletion, a binary, and a file reached
  after the byte ceiling all cost nothing, so charging them would
  make a commit that deleted 39,000 files show none of the ones it
  changed.
- The cap belongs beside `SUMMARY_ROW_CAP` and `PAGE_BYTE_BUDGET` as
  one more thing a page will not exceed.
- The number has to satisfy something stated, not taste: a page's
  fetch phase should stay within a few seconds on the workload
  measured above. At ~98 ms cold, **50** works out to roughly 5 s and
  200 to roughly 20 s. That is an ESTIMATE FROM THAT WORKLOAD, not a
  wall-clock guarantee — the metadata passes and each blocking git
  read still have no deadline, and adding timeout machinery is not
  part of this (codex on 5ae2b3c). 50 also covers the overwhelming
  majority of real commits, and the ones it does not were going to be
  truncated by the byte ceiling anyway.

## Deliverables

1. A per-page fetch cap enforced at the point the fetch decision is
   made, so build cost is bounded by construction rather than by how
   small a repo's files happen to be.
2. Its doc comment carries the measured cost that justifies the
   number — 5.5 ms warm, ~98 ms cold — and names what was ruled out
   (blob-sha fetch, batching) so the next reader does not re-litigate
   it.
3. The ordinary case is untouched: a commit of a few files still shows
   every one of them, with the same bytes as today.

## Tests

- A commit with more files than the cap attempts EXACTLY the cap, on
  patches small enough that the byte ceiling cannot be what stopped
  it. The fetch count is what the test asserts, not only the rendered
  output — the point is work not done, and a test on output alone
  passes while the work still happens. Acquisition is injected for
  this reason.
- **A failed fetch and an unparsable one each spend an attempt.** Both
  leave nothing on the page, so only a count taken before the call
  can see them.
- Deletions, binaries and post-ceiling files spend nothing: a fixture
  of 500 deletions and 300 binaries followed by a full cap's worth of
  real changes reads and shows every one of the real ones.
- A commit under the cap renders its patch WHOLE, asserted against
  the complete expected output: hunk header, context, removal,
  addition, and the line numbers each advances. The counting tests
  cannot stand in for this — `<details>` is emitted for a file with no
  hunks at all, so they pass on an empty shell, which is how a fixture
  with a broken `@@` line went unnoticed (codex on 56e3d2b). The
  fixture's hunk starts at different old and new line numbers and
  carries a context line after the removal, or swapping the two
  columns and failing to advance them render identically.
- Mutation-check the cap: removing it must fail a named test, and the
  fetch-count assertion must be the thing that fails.

## Out of scope

- Batching the fetch and blob-sha acquisition — both measured above
  and rejected, not deferred.
- How often the TUI rebuilds. Settled in the previous plan: rebuilding
  is by design, the page just has to be sane.
