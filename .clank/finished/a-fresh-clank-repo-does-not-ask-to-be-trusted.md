# a-fresh-clank-repo-does-not-ask-to-be-trusted

Opening a clank repo in claude should not present a trust warning
clank itself caused.

> "I just opened a new clank dir and I got: ⚠ This folder pre-approves
> 3 tool permissions … so can you make a plan to address having to do
> this? If we can't opt out of this question I think we can just
> remove the rules." — lloyd

## Why

`clank init` merges three rules into `.claude/settings.local.json`
(`init_facts.rs:59`, written by `write_claude_perms` in
`cli/init.rs:281`):

    Write(.clank/agents/**)
    Edit(.clank/agents/**)
    Read(.clank/agents/**)

Claude Code shows a folder-trust prompt whenever a directory carries
pre-approved permissions. So every fresh clank repo greets its
operator with a warning about a file clank wrote, naming rules the
operator never chose. That is a scary dialog for a setup step, and it
trains people to click through trust prompts — which is the one habit
the prompt exists to prevent.

## The direct access, and the decision

Most `.clank/agents/**` access is through the clank binary: `clank
feedback read`, `clank feedback write`, and `clank inbox` do not use
Claude's file tools. The bundled master skill does, however, tell an
agent to add persistent `wait_events` to its
`.clank/agents/<label>/config.json`. This is a real direct edit, not a
reason to preapprove all Read, Edit, and Write access in every new
repo. An agent making that optional config change can request the
ordinary per-edit permission when needed. The repeatable `clank wait
--event` flag also covers an ad-hoc event without editing that file.

Stop writing the three blanket rules. On a rerun of `clank init`,
remove only those three exact entries from an existing
`.claude/settings.local.json` and leave every unrelated setting and
allow rule in place. If the file contained only clank's three rules,
remove the now-empty file; if none of the three entries was present,
leave the file byte-identical. Remove `write_claude_perms` as a writer,
its doctor check, the `open.rs` staleness branch that treats the file
as an init gap, and the tests that pin the merge. Check every bundled
skill, setup asset, prompt and doc for another direct path, updating
any claim that these rules are required. Do not replace the blanket
rules with a narrower preapproval: a repo initialized by this version
should carry no clank-made permission approval for Claude to ask the
operator to trust.

The operator deferred the live Claude checks: no Claude login, routine
review-loop smoke run, or separate optional config-edit smoke run is
required to finish this plan. The CLI is not logged in here. The code
tests, installed-binary smoke checks, and migration of this repo's
legacy-only settings file are the accepted verification.

## Out of scope

- Claude Code's trust prompt itself. Whether it can be opted out of is
  not clank's to change; this plan removes clank's REASON for it.
- Codex. Its permission model is shell-command based, so it has no
  equivalent file and nothing to remove.
- The rest of `clank init` — the gitignore, the post-rewrite hook, the
  agent scaffolding. Only the `.claude/settings.local.json` merge is
  in question.

## Tests

- A repo `clank init` just created has no `.claude/settings.local.json`
  that clank wrote — asserted on the file, since the point is what an
  operator's editor finds there.
- Re-running `clank init` on a repo with a file made only of the three
  clank rules removes that file. Re-running it on a mixed file removes
  only the three exact rules and preserves every unrelated setting and
  allow entry. A second run makes no further change.
- `clank init` in a repo where the OPERATOR has their own
  `.claude/settings.local.json` without the three clank rules leaves
  it byte-identical. An operator who added other rules keeps them.
- `clank doctor` reports nothing about a file clank no longer writes,
  and does not report a fresh repo as broken for its absence.
- Removing the rules leaves no stranded reference: a full-repo grep
  for `settings.local.json` and for each rule string, including the
  README, the bundled skills and the setup assets.
