# strip-verdict-restatement

Reviewers habitually restate the verdict at the start of their `-m`
message ("CONTINUE: …", "Finished — …"), so feedback files' first lines
read `CONTINUE CONTINUE: The M3 resolver does…` and every summary
surface (TUI review rows, log, html) shows the verdict twice — once as
the ✓/✗ mark derived from the real header, once as literal text inside
the summary. The `--verdict` flag is the single source of truth;
a restatement in the message is derived duplication. Normalize it away
at WRITE time, at the boundary where the file is composed. Write-time
only — no display-time stripping, no rewriting of existing files.

## Change

In `clank feedback write` (`crates/cli/src/cli/feedback.rs:37`, where
the body is composed as `{verdict_header} {message}`), strip a leading
restatement of the DECLARED verdict from the message first.

One pure function, e.g. in `clank_core::feedback_body` next to the
parser it mirrors:

```
pub fn strip_verdict_restatement(verdict: Verdict, message: &str) -> &str
```

Strip rules — deliberately conservative:

- Case-INSENSITIVE match of the declared verdict's token at the start
  of the message's FIRST line only: `CONTINUE`, `FINISHED`,
  `REQUEST_CHANGES`. For `REQUEST_CHANGES`, accept underscore, space,
  or hyphen between the words ("Request changes:",
  "request-changes:").
- The token must be followed by a punctuation separator — `:`, `—`,
  `–`, or `-` — with optional surrounding whitespace. A bare verdict
  word followed by plain prose is NOT stripped ("Continue polishing
  the API" is a legitimate summary, not a restatement).
- Only the DECLARED verdict's token is stripped. A mismatched
  restatement (`--verdict continue -m "FINISHED: …"`) is left intact —
  it is information, not duplication.
- Strip once, not in a loop. Lines after the first are untouched.
- If stripping leaves the first line empty, the header line is written
  as the bare verdict token (already a valid on-disk form:
  `parse_verdict` accepts `CONTINUE` with no summary). The rest of the
  message body still follows.

## Tests

Unit tests on the pure function:

- strips: `"CONTINUE: msg"`, `"Continue: msg"`, `"continue — msg"`,
  `"FINISHED — msg"`, `"REQUEST_CHANGES: msg"`,
  `"Request changes: msg"`, `"request-changes: msg"` (each against the
  matching declared verdict).
- does NOT strip: `"Continue polishing the API"` (no separator),
  `"FINISHED: …"` under `--verdict continue` (mismatch),
  a mid-message occurrence, second-line occurrences.
- empty remainder: `-m "CONTINUE:"` → header-only first line.

One write-path test pinning the composed file: `feedback write
--verdict continue -m "CONTINUE: looks good"` produces a file whose
first line is `CONTINUE looks good` and whose parsed summary
(`parse_summary`) is `looks good`.

## Out of scope

- Display-time stripping in `parse_summary` (existing files keep their
  duplicates; they age out).
- Rewriting existing feedback files.
- Prompt/skill wording changes.
