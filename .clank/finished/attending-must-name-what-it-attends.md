# attending-must-name-what-it-attends

`clank attending <task-id>` accepts a wait with no description. The
row then reads `⌛ br9711ewy` — an opaque tool-internal id that names
nothing to a human. That is the exact failure `--desc` was added to
fix, and the fix is unenforced: the arg is `Option<String>`, and the
stop-hook nudge only ASKS for it.

Worse, a blank one is swallowed silently:

    .map(|d| d.trim().to_string())
    .filter(|d| !d.is_empty())      // attending.rs

`--desc "   "` records no description and reports success.

## Change: enforce at the boundary, stay lenient on read

- `--desc` becomes required unless `--clear` (clap
  `required_unless_present = "clear"`), so the failure is a parse
  error naming the right invocation, not a mystery row later.
- A desc that survives normalisation (first line, control bytes
  dropped, trimmed) as empty is a hard error, not a silent `None`.
  Refusing beats recording a wait nobody can identify.
- `--clear` keeps working with no desc.

The record type does NOT change. `Attending`/`Attended.desc` stay
`Option<String>` because:

- records written before this change must keep loading —
  `an_attended_record_without_a_description_still_loads_and_names_itself`
  pins that, and it stays pinned;
- `subject()`'s task-id fallback is the READER's tolerance for those
  legacy records, and it stays.

One place mints records, so one place enforces. Pushing the
requirement into the type would break old records on disk to restate
a rule the writer already guarantees.

## Long descriptions are truncated, never rejected

Already true and staying true: `render::fit_marker` drops age and pid
from the right, then truncates the subject, refusing to render only
below `MIN_SUBJECT = 6` columns. No new truncation code — add the
test that pins it against an over-long `--desc`.

## Sweep

- `AttendingArgs::desc` doc comment — currently reads as optional.
- The live-task nudge (stop_hook.rs ~1104) explains desc as
  optional-with-a-downside ("without it the row can only name the
  opaque task id"). It must read as required, and keep showing a
  concrete two-word example.
- `attending.rs` module docs and the success line.

Deliberately untouched: the positional `task_id` is also
required-unless-`--clear`, enforced by a runtime `bail!` rather than a
clap rule. Converting it would change `--clear`'s error surface for no
gain here; the inconsistency is noted, not fixed.

## Tests (in-process; no binary spawning)

- No `--desc` → parse error; the message shows the required flag.
- `--desc "   "` (and a control-only desc) → error, and NO record is
  written.
- `--clear` with no desc → succeeds.
- A successful write always yields a record with a non-empty desc.
- A legacy record with no desc still loads and still names itself by
  task id (existing test, kept).
- An over-long desc is stored whole and truncated only at render.

## Acceptance

- No new attending record can exist without a human-readable subject.
- Old records keep loading and keep rendering.
- fmt/clippy/suites green at baseline.
