# the-readme-should-get-a-stranger-running

The README is accurate and cannot get anyone started. Following it
end to end leaves you with a roster and no running agents, because it
never launches one.

## What is actually wrong

Verified against the current file, not impressions:

- **It never starts an agent.** `clank open` appears NOWHERE in the
  README (the one grep hit is the string "clank opencode plugin"), and
  `clank agent start` appears once, incidentally, inside a bullet
  about grok. The Quickstart scaffolds `.clank/`, adds a roster, binds
  sessions with `clank as`, and shows a reviewer writing a verdict —
  with no step that makes an agent exist. This is the whole defect in
  one line: the happy path dead-ends.
- **The install is not copy-pasteable.** `git clone <this-repo>` is a
  placeholder. The first command in the document cannot be run.
- **The Quickstart does not say WHERE each command runs.** `clank
  init` and `clank agent add` are yours; `clank as alice` and `clank
  feedback write` belong to an agent inside its own session. A
  newcomer has no way to tell, and the distinction is the thing that
  makes clank make sense.
- **Depth sits where onboarding belongs.** "How agents stay awake" is
  ~90 lines of hook internals — asyncRewake, waiter generations,
  in-hook long-polls, hook-runner ceilings — and it is the FOURTH
  section, ahead of the command reference. It is excellent
  troubleshooting material in an onboarding slot.
- **An advanced subsystem is nested inside it.** "The wider wait
  surface" is ~50 lines on github watches, webhook delivery, prompts,
  the inbox and ack semantics. A reader who wants to try the review
  loop meets webhook admin requirements first.
- **The primary interface is one table cell.** `status --tui` is where
  the work is actually watched and driven — agent pages, tier
  toggles, event pages — and it gets a row in a table.
- **The core concept is prose-only.** The plan → intro → review →
  implement → verdict → finish cycle is the one idea a reader must
  hold, and there is no diagram of it.

Nothing here is inaccurate: the command table matches the real
subcommand list exactly, checked. The problem is shape, not truth.

## Change

Reorder so the document answers, in order: what is this, what does it
look like, how do I run it, what do I type, and only then how does it
work inside.

- **Open with the cycle**, including a diagram, so the model is
  visible before any command.
- **One runnable Quickstart** that ends with agents actually running
  and a plan under review. It must include the launch step — `clank
  open` and/or `clank agent start` — and label every block with WHOSE
  shell it belongs to: yours, or an agent's.
- **Show the TUI** where it belongs, as the way you watch the loop.
- **Move the wake mechanics below the command reference**, retitled as
  reference. Keep every word that helps someone debug a silent agent;
  the content is good, the position is wrong.
- **Extract the github/event surface** out of the wake section into
  its own section, after the basics, marked as optional.
- Fix the clone line to the real URL.

## Beautiful means legible, not decorated

The bar is a reader who has never seen clank reaching a working
two-agent review loop without asking anyone a question. Prefer a
diagram over a paragraph, a labelled block over an unlabelled one, and
a short section over a complete one. Length is not the enemy —
undifferentiated length is.

No screenshots: they go stale silently and nothing in this repo can
regenerate them. A copy of real `clank status --tui` output, which the
troubleshooting section already does well for `doctor`, carries the
same weight and can be re-pasted.

## Tests

Prose cannot be unit-tested, but its CLAIMS can, and the accuracy that
exists today should not rot:

- Every `clank <subcommand>` token in the README names a real
  subcommand. True right now; a test keeps it true.
- The Quickstart's launch step exists — the README mentions the
  command that actually starts an agent. This is the defect that
  motivated the plan, so it gets a regression.

Keep both cheap and string-level. A test that tries to verify prose
QUALITY would be a worse version of a human reading it.

## Out of scope

- The skills' own text (`skill_master.md`, `skill_reviewer.md`), which
  is the agents' contract, not the human's.
- `CLAUDE.md` and `RELEASE-CHECKLIST.md`.
- Any new feature. If the README cannot explain something, that is a
  finding to report, not a licence to change the tool.
