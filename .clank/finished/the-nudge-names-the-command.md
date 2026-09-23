# the-nudge-names-the-command

The wake item that says feedback is waiting says how to read it.

> "btw why don't you use clank feedback read to read feedback instead
> of this glob thing. If there's something wrong with clank feedback
> read and you could improve it so that it renders stuff so you'd
> never do those glob reads again queue that plan too." — lloyd

## Why

Nothing is wrong with `clank feedback read`. It takes `--commit`,
defaults to HEAD, prints every reviewer's verdict, summary and body
for that commit, and ends each with the file it came from. The master
skill has said to use it since it existed. The glob was habit, not a
gap — and habit is what a surface has to beat.

What the surface can do is be named where the need appears. The item
that sends a master back to work says

    master: revise <plan> @ <sha> (address_commit_changes)

which carries the sha and stops. The command that turns that sha into
the feedback is one the agent has to remember, and an agent that does
not remember it reaches for the files, which are right there and
wrong in the same way every time the layout changes.

The other gap is the shape of the question. Feedback is read a commit
at a time, but a review cycle is a plan: what was asked on the intro,
what was asked on the first milestone, what is still being asked now.
That is four commands and a `git log` today.

## The build

**The item names the command.** A `revise` wake item carries
`clank feedback read --commit <sha>` in the detail line under it,
where the wait already explains what fires it. The same for the
finalize item that follows a FINISHED, which is read the same way.
Wording is the wait's, so the JSON shape of `WaitItem` does not
change — only what is rendered under the item.

**A plan is a readable unit.** `clank feedback read --plan <name>`
prints the plan's reviewed commits oldest first, each with its
verdicts, so a cycle reads as a cycle; `--plan` with no name is the
active plan. The per-commit output is what it already is, so one
reader serves both.

**The skill says never the files.** The master skill's "Reading
feedback" names `clank feedback read` as the only way in, and says
the feedback files are not to be read by hand — the same way the git
layer's rule is stated, because the reason is the same: the layout is
the layer's business.

## Tests

- A `revise` item renders the exact command for its sha, and the sha
  in it is the one the item names.
- `--plan` prints every reviewed commit of that plan oldest first with
  each reviewer's verdict; the default is the active plan; an unknown
  name is an error that names the plans there are.
- A plan with no feedback yet prints that, not an empty success.
- `--plan` and `--commit` together are refused by the parser.
- The existing per-commit read is unchanged, `-j` included.

## Out of scope

- The feedback file format, and `feedback write`.
- The TUI, which shows verdicts on its own rows already.
