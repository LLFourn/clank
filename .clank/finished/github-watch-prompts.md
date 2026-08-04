# github-watch-prompts
# Per-watch prompts: github wake events carry operator instructions

## Why

A github watch today is purely mechanical (repo, kinds, cadence).
The wake hint tells the agent WHAT happened; nothing tells it what
the operator WANTS DONE. The generic clank-github skill teaches the
inspect→react→ack discipline, but "react" is unspecified — the
operator has no way to say "a PR comment here means: triage and
reply", "an issue opened here means: label and take it to the
queue". The instruction belongs on the WATCH, declared once, and
should ride with every wake event that watch produces.

## What

- `GithubSource` gains `prompt: Option<String>` — free text the
  operator writes when declaring the watch. Per-kind granularity
  comes from SPLITTING watches (already supported: an agent lists
  as many sources as it likes, same repo included), not from a
  per-kind map.
- Every surface that presents an event from that source carries the
  prompt:
  - The wait/stop-hook work item: the mechanical hint line stays
    first (it's the pinned, greppable shape), the prompt renders as
    an indented follow-on line. Typed JSON items (`github_event`)
    gain an `instructions` field.
  - `clank events list`/`show`: the prompt shows with the event, so
    the react-then-ack loop sees it at inspection time too.
- The clank-github skill mentions the field: when an event carries
  instructions, they are the operator's standing intent — follow
  them, then ack as usual.

## How (constraints)

- **The prompt is PRESENTATION config, not event data**: it is
  never written into the event WAL. Presentation joins WAL rows to
  the CURRENT config's source prompt at render time, so editing the
  watch retitles the standing intent for already-logged unhandled
  events, and the version-stable log format is untouched (no new
  record fields, no compat surface).
- Watch splitting changes source identity (the per-source WAL key
  derives from the source config): note in docs that reshaping
  watches starts fresh WAL keys with a baseline — existing behavior,
  called out, not changed.
- Prompt text is untrusted-ish operator config rendered into agent
  prompts: collapse control characters/newlines in the ONE-LINE
  hint context (same treatment as PR titles in fork purposes), but
  the indented block may keep simple multi-line text.

## Acceptance

- A `wait_events` github source with `"prompt": "..."` produces
  wake items whose text includes the mechanical hint line AND the
  instruction; JSON items carry `instructions`; sources without a
  prompt render exactly as today (byte-stable).
- `clank events list --all`/`show` display the instruction for
  events whose source declares one, including events logged BEFORE
  the prompt was added to the config (presentation-time join).
- No change to WAL record shapes; event-log format tests untouched.
- Skill/README document the field with a concrete example.
