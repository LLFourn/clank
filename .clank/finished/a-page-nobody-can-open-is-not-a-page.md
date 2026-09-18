# a-page-nobody-can-open-is-not-a-page
# A page nobody can open is not a page

## Why

> "when deleting stuff clank generated html in .clank/html of 166 gb
> see ~/src/penlock-experiment -- queue a plan to fix. Diffs should not
> include binary or deleted file content."

Measured there: **166 GB across 661 HTML files**, several over 100 MB
each. A 100 MB page does not open in a browser, so the bytes are worse
than useless — they are unreadable AND they fill the disk.

The cause is one line with no limits in it:

```rust
// git_io.rs
read_git_stdout(repo, &["show", "--no-color", "--pretty=format:", sha])
```

`commit_diff` hands that to `parse_unified_diff` and the page embeds
every hunk, HTML-escaped. That repo deleted ~39,000 files including
`result.json` files of 2.4 MILLION lines and 67 MB each — so a single
commit page reproduces a hundred megabytes of JSON the operator
deleted on purpose, one `<span>` per line.

## What triggers it, and why deleting the directory makes it worse

Nobody ran `clank html`. The TUI hosts the remote in-process, and
`remote.observe(&snapshot)` fires on EVERY snapshot rebuild; with the
remote on that reaches `Instance::observe`, which asks the
`SiteBuilder` thread for a build. So the site is regenerated because
the TUI redrew.

And `rm -rf .clank/html` does not help until this is fixed — it makes
the next build the most expensive one possible. `read_prior_build`
finds no `index.html`, answers `PriorBuild::None`, and a cold build
regenerates every commit and plan page. Deleting the 166 GB hands the
next TUI tick a full 166 GB rebuild.

Which fixes the order of operations: **this lands, then the directory
is deleted, then it rebuilds small.** The other way round just
re-inflates it.

## The model

**A diff on a page is for READING. Content nobody will read does not
belong in it.**

Two kinds of content are never read and are exactly what blew this up:

- **A deleted file's body.** Its removal is the fact; the 2.4 million
  lines that used to be there are not. Git has them if anyone wants
  them, addressed by the sha the page is already about.
- **Binary.** `git show` already declines to inline it, but the page
  should say what changed rather than pass the placeholder through as
  if it were a hunk.

And a third, which is the general form of the same mistake: **any
single file's patch, past the point of legibility.** A 40,000-line
modification is not more informative than its first few hundred lines
plus how much was left. The cap is what makes the page bounded
regardless of what the next repo does — the two rules above are what
make it bounded for the cases we have actually hit.

## Where it has to become bounded

Not in the formatter. `git_io::commit_diff_text` runs `git show` with
`.output()`, which buffers the WHOLE diff into a `Vec<u8>`;
`html::commit_diff` copies that into a `String`; `parse_unified_diff`
then keeps every hunk. So a commit deleting 105 million lines is
gigabytes of resident memory before any renderer could decline to
print it (codex on 9ed0daf). Truncating at the end fixes the file on
disk and leaves the machine swapping.

And the budget must be in **rendered bytes**, not lines. One minified
JSON line can be tens of megabytes on its own, and escaping plus
highlighting only expands it. A line cap does not bound a page; a byte
cap does.

## The shape of the fix

**Metadata first, then fetch only what will be shown, and never more
than the budget.**

1. **Status from `--raw`, counts from `--numstat`** — both NUL-delimited
   (`-z`), both bounded by the file COUNT rather than by content.
   Numstat alone cannot decide anything, which the first draft of this
   plan got wrong (codex on d9779e0). Measured on three files changed
   in one commit — one edited to only remove lines, one deleted, one
   emptied in place:

   ```
   $ git show --numstat --pretty=format: HEAD
   0   2   empty.txt        # emptied in place   → M
   0   2   gone.txt         # deleted            → D
   0   2   keep.txt         # lines removed      → M
   ```

   Identical. Only `--raw` separates them (`M`/`D`), and binary
   add, modify and delete are all `-  -` in numstat too. Inferring
   deletion from counts would hide two ordinary modifications and
   mislabel every binary change — breaking the one promise this plan
   makes about the ordinary case.

   `-z` for both, because a display path is not safely splittable on
   whitespace and rename rows carry two paths (`R100\0old\0new`).
2. A deleted file needs no patch: the row already carries how many
   lines went. A binary file needs no patch either. Neither is ever
   fetched, so the bodies that caused this are never read at all.
3. What remains is fetched per file, and read through a reader that
   stops at its budget and discards the rest instead of buffering it.

## Deliverables

1. **A bounded git read** in `git_io` — spawn, read at most N bytes,
   then close the pipe and TERMINATE AND REAP the child: dropping a
   `Child` does neither, so a truncated producer would otherwise keep
   writing into a closed pipe or linger as a zombie (codex on
   d9779e0). Truncation is detected by asking for one byte more than
   the cap, so output landing exactly on the cap is not reported as
   cut. The layer owns git access, so the limit belongs there and not
   in a caller's loop.
2. **Metadata-first acquisition**: status and paths from `--raw -z`,
   counts from `--numstat -z`, then only renderable files are fetched,
   one at a time, through that reader. Counts never decide status —
   see above.

   **The same file must be on both sides of that boundary**, and two
   things make that so. `--` takes a PATHSPEC, so a file actually
   named `a*.txt` fetches every `a….txt` in the commit and the
   renderer shows the first — which can be the body of a deleted
   file, defeating the exclusion this plan exists for. Patch reads
   are therefore literal, and a fetch that returns anything but
   exactly one file is named rather than guessed at. And the join
   itself must not shorten a path: `-z` does not quote, only the two
   count separators are structure, and a filename may contain a tab.
   A path that joins to nothing is UNKNOWN, never binary — calling it
   binary would hide the file, since a binary is deliberately never
   fetched (all codex on ae2082a).
3. **A deleted file is a row**, not a body: path, `deleted`, and the
   line count from `--numstat`.
4. **Binary is named, never inlined**, with its real status — added,
   removed or changed — from the raw row, not guessed from a pair of
   dashes.
5. **Two bounds, each in the units it can actually enforce, and
   NEITHER derived from the other.** Per file, an ACQUISITION cap in
   source bytes — the only bound applicable before the bytes are
   held, and all it governs is memory. Per page, a ceiling in
   RENDERED bytes, because that is the size that lands on disk.

   There is no usable ratio between them. Rendering costs about
   `source + 199 * lines`, so the expansion depends entirely on line
   LENGTH: four-byte lines expand ~50x, and a file of blank added
   lines — two source bytes a line — expands ~100x. Any constant
   picked as "the worst case" is wrong for some real file, and being
   wrong means a patch renders past the ceiling and is reduced to a
   row, showing NOTHING. So the page does not predict; it MEASURES,
   element by element, and keeps the prefix that fits (codex on
   ae2082a).
6. **Truncation is VISIBLE**, not an internal flag: the page says what
   was omitted and names the command that reads the rest — a command
   that must actually WORK.

   Literal spelling has exactly one mechanism, and mixing two breaks
   it: `--literal-pathspecs` disables pathspec magic, so a `:(literal)`
   prefix added on top becomes part of the filename git looks for and
   every printed command finds nothing. The page prints the same
   mechanism the fetch uses — the flag, plus the path shell-quoted and
   otherwise untouched (codex on 5096f03).
7. **Every append is checked, and the page NEVER exceeds its
   ceiling.** A budget consulted once per file is not a budget: the
   summary rows, the cut notices and the closing markup are page
   bytes like any other. So the ceiling is spent per rendered element
   — a whole diff line, a whole row — with the remainder held in two
   reserves: one for closing whatever is open plus the tally, one for
   the named rows, so a page whose patches filled up still names the
   files it could not show. Half an element is never appended, and
   the finished page is `<= PAGE_BYTE_BUDGET` exactly. Rows are
   capped too — 39,000 filenames is itself a page nobody can open.

   The cut notice stays inside its reserve for ANY name git can
   produce: past 200 characters it drops the recovery command rather
   than growing with the path, because a wrong command is worse than
   none.
8. **`BUILDER_VERSION` bumps**, so pages already on disk adopt the new
   policy instead of keeping the old rendering.

## Tests

- **Acquisition, not just output**: the bounded reader returns exactly
  its cap and reports truncation for a file far larger than it, and a
  commit that deletes a huge file is rendered WITHOUT its patch ever
  being fetched.
- A deleted large text file becomes a row naming the path and the line
  count; its content appears nowhere in the rendered bytes.
- A binary file is named and inlined nowhere.
- **One enormous single LINE** — minified JSON, no newlines — is
  bounded. This is the case a line cap silently misses.
- **Rendered expansion counts**: twelve files of 20 KiB each — 240
  KiB of source, 12 MiB of page — must not all be shown. Counting
  source bytes admits every one; counting rendered bytes admits four.
  The test asserts BOTH that some render and that the rest become
  rows, because a page that showed nothing would pass a size
  assertion while proving nothing.
- **A cut patch is still shown**: a big MANY-LINE file renders as far
  as the page allows with the cut notice, rather than being reduced to
  a row. A single enormous line cannot prove this — it renders as one
  cheap row — so the two cases are tested apart.
- **Two-byte lines**: a file of blank added lines, the ~100x case, is
  shown and cut. This is what no ratio survives.
- **The real ceiling, on a nearly-full page.** A patch that fills the
  page followed by more long-named files than the remainder can show:
  the page is `<= PAGE_BYTE_BUDGET` exactly — no allowance in the
  assertion — some files are named, and the rest are tallied. The
  fixture has to earn this: rows sorted BEFORE the patch fit while
  the page is empty and test nothing, and rows short enough to fit
  inside their reserve do not exercise the check either.
- **A file named `a*.txt`** shows its own patch and not its deleted
  sibling's body. A path too long to spell gets no command rather than
  a wrong one.
- **The printed command is EXECUTED** by the test, un-escaped from the
  page's own bytes, for an ordinary name and for one carrying both an
  apostrophe and a glob character — each must return its own patch and
  never the deleted sibling's. Asserting the command STRING is
  circular: it only proves two copies of the same mistake agree, which
  is how the `:(literal)` collision survived a passing test.
- **A tab inside a filename** joins to its counts, end to end through
  a real repository, and unknown counts do not read as binary.
- **Summary-only overflow**: enough files to exhaust the ceiling with
  summaries alone still yields valid HTML, ending with the omitted
  count.
- **The three cases numstat cannot tell apart**, each rendered for
  what it is: a file edited to only remove lines, and a file emptied
  in place, both render as ordinary modifications with their hunks; a
  deleted file renders as a row. This is the regression the first
  draft would have introduced.
- **Binary add, modify and delete** each named with the right status.
- A path containing a space and a rename row survive acquisition,
  because `-z` is what makes that true rather than luck.
- The ordinary case is untouched: a small diff renders exactly as it
  does today.
- Mutation-check each limit: removing any one must fail a named test.

## Out of scope

- Reclaiming the 166 GB already written. `rm -rf .clank/html` does
  that once this has landed — and not before, per the order above.
- **That a redraw rebuilds the site at all.** Accepted by design —
  "that's fine that it rebuilds we just have to stop it from being
  insane". Once the pages are bounded a rebuild per tick is cheap,
  which is why bounding them is the whole job.
- Anything about the dirty-status figure in the same repo, which is
  accurate: git reports the same 104,982,991 deletions.
- `clank diff` on the terminal, which pipes to a pager and is a
  different question.
