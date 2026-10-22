# answer-a-block-where-you-read-it
# Answer a block where you read it

## Why

The user cannot answer blocks from `clank tui`. Today a pending block
renders as accent-coloured "ask" lines at the top of the log, and the
only way to answer is `u`, which works only while the log region has
focus and the cursor sits on one of those ask lines
(`status_tui/mod.rs`, the `Key::Char(b'u')` arm of `Mode::LogScroll`).
Enter on an ask line does nothing: the Enter arm only handles nudge,
github, commit and plan rows. A key that works in one focus state, on
one kind of row, with no affordance on screen, is not a way to answer
a question. It is a riddle.

Every other actionable thing in the TUI opens with Enter onto a page
with buttons: plans, commits, github events, the remote row. Blocks
should work the same way.

## What changes

- **Enter opens the block.** Enter on any line of a pending block's
  ask, including each wrapped continuation line of the question, opens
  a block page for exactly that block. Enter on the agent panel row of
  an agent that is asking (its block is pending) opens the same page.
  These are the places a human sees a question. Wherever one is
  visible, Enter answers it.
- **The block page.** It shows the asking agent, the block name, the
  plan it belongs to (if any) and the full question text, scrollable
  when long. Below the text sits an **Answer** button, selected by
  default, plus Back. Enter on Answer opens a text input directly
  underneath the question, on the same page, so the question stays in
  view while the human types. Enter submits through the existing
  `block::run_unblock` core (`submit_block_answer`), Esc returns to
  the buttons, and Esc again leaves the page. A failure shows on the
  page with its reason; the text typed stays in the input.
- **One question, one answer.** The page answers exactly the block it
  was opened on, identified by its ask occurrence (agent, name and
  the occurrence id `an-answer-is-received-not-cleaned` introduced),
  not by its index into `snapshot.blocks` or by name alone. Submitting
  passes that occurrence to `block::answer_block` as the browser form
  does (`block::Shown`). A re-ask of the same name while the page is
  open, even with identical text, is a different block, and the page
  refuses to answer it. A refresh that adds, reorders or removes blocks
  while the page is open must not redirect the answer to another
  block. If the block is answered or withdrawn elsewhere while the
  page is open, the page says so and submitting is refused.
- **`u` goes away.** The page replaces it. Remove the `u` arm, its
  hint line under the asks, and the separate `Mode::BlockAnswer`
  screen. The trailing key-hint `AskLine` (`block: None`) becomes a
  hint that Enter answers.
- The repo `pause` block, which is the TUI's own, keeps its `b`
  toggle. Enter on its ask opens the same page, where answering it
  resumes the repo.

Find out why `u` failed for the user (a focus state, the cursor not
landing on an ask line, the asks scrolled away, or something else),
and make sure the Enter path cannot fail the same way. Name the cause
in the commit.

## Tests

- Enter on the first line, a wrapped continuation line, and the agent
  row of an asking agent each opens the page for that block.
- Answer → type → Enter calls the unblock core with that block's
  agent and name and the typed text. Drive it through the key handler
  with a recorded unblock seam; don't test only a helper.
- A refresh that inserts a block before the open one, while typing,
  still answers the original block. The mutant is indexing by
  position.
- A re-ask of the same name with identical text while the page is
  open: submit is refused and the new ask stays unanswered. The mutant
  is matching by name.
- A block answered elsewhere while the page is open: submit is
  refused with a visible reason and nothing is written.
- Rendering: the question, the Answer button and the input, with the
  input appearing beneath the question after Answer.

## Out of scope

- Answering blocks from the remote web page.
- Bulk-answering several blocks.
