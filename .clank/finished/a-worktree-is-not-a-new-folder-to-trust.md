# a-worktree-is-not-a-new-folder-to-trust

Why spawning a clank team meets Claude Code's folder-trust prompt:
what is established, what is not, and what would settle it. No change
is proposed.

> "I am getting a lot of these when spawning new clank teams with
> claude." — lloyd, on Claude Code's folder-trust prompt naming
> `.claude/settings.local.json`'s three pre-approved permissions

## Why

The report, and the facts around it. Everything about MECHANISM is in
the findings below, because two guesses at it were wrong and a third
asserted here would put a retracted thesis at the top of the document
(ruthless f9b9b50).

Observed:

- `clank init` tag-merges three rules into
  `.claude/settings.local.json` (`init_facts.rs`,
  `CLAUDE_ALLOW_RULES`, written by `write_claude_perms` in
  `cli/init.rs`): `Write`, `Edit` and `Read` on `.clank/agents/**`.
- `.claude/` is gitignored, so the file is not inherited through git;
  each directory that has one had it written there.
- The dialog in the report names those three permissions.
- The reported path, `/Users/llfourn/src/fs-wt-dashboard`, holds
  exactly those three rules and nothing else — so clank wrote that
  file, even though clank did not create the directory.

Whether the rules cause the dialog, or merely appear in one that any
unseen path gets, is the question the findings keep open.

## Findings: stored state, and what it does not tell us

Read-only, against this machine's Claude Code state (2.1.280).
`~/.claude.json` records `projects["<path>"].hasTrustDialogAccepted`
per ABSOLUTE path; 101 paths are recorded here.

**Established.**

- A worktree's own entry can be `false` while its main checkout is
  `true`. Verified on `/Users/llfourn/src/fswt/frostsnap_nostr-taipei`:
  its `.git` file points at `/Users/llfourn/src/frostsnap/.git`, and
  that main checkout is recorded trusted.
- The path in the report, `/Users/llfourn/src/fs-wt-dashboard`, has a
  `.git` DIRECTORY, so it is not a linked worktree. It also carries no
  `.clank/fork.json` and is not under a `clones` namespace, so it is
  not a clank fork of either kind.
- Nothing in `~/.claude/settings.json` configures trust.

**NOT established, and the first version of these findings claimed it
was** (codex eae7f25, who read the installed binary rather than
guessing):

- *That worktrees are prompted.* A leaf entry's value is not the
  effective decision. 2.1.280's trust code resolves a canonical main
  checkout through `.git`, `commondir` and the worktree backlink, so
  that `false` leaf is consistent with the worktree being trusted via
  its parent — which is also what current documentation describes.
- *That removing the three rules would leave the dialog.* Accepted
  paths with no allow rules TODAY say nothing about what those files
  held when the dialog appeared.
- *That pre-trusting every new path is the only fix.* That followed
  only from the two claims above.

Stored state was all this probe could see. The runtime questions need
controlled interactive runs, and a `-p` run cannot show either, because
that mode skips the prompt. They are two different runs:

- *Which key is consulted* — a fresh worktree of a trusted repo. That
  separates trusted-path from untrusted-path.
- *Whether the rules matter at all* — two opens differing ONLY in
  whether `CLAUDE_ALLOW_RULES` is present. Varying the path cannot
  answer this; only varying the rules can, and this is the question
  the reported dialog actually raises.

  Both opens must start from the same trust state, and the naive
  version does not: trust is recorded per absolute path, so reopening
  one path leaves the second run facing a path that now remembers.
  Then silence on run two shows nothing — it is equally explained by
  "the rules did not matter" and by "this path was already answered
  for" — while a re-prompt only shows that a changed set re-asks,
  which is itself unestablished. Use two paths Claude Code has never
  seen, or clear the entry from `~/.claude.json` between opens. With
  that control both outcomes inform; without it, only a re-prompt
  does, and a null result must not be read as an answer
  (ruthless b4f4913, codex b4f4913).

## Three kinds of directory, not two

clank makes forks TWO ways, and `.git` being a directory rules out
only one of them (codex 60411a9):

- **A worktree fork** (`git worktree add`, `--path` anywhere) is a
  linked worktree, so the canonical lookup resolves it to the main
  checkout. A fork of a trusted repo should not prompt.
- **A clone fork** (`clank fork create --clone`) is an INDEPENDENT
  repository at `<main>/.clank/clones/<name>`, with its own git root
  and so its own trust. It has nothing to inherit from, and this is
  the case where clank really does make a directory that must be
  trusted afresh. One is recorded here —
  `frostsnap/.clank/clones/sec-review-0bbc18be`,
  `hasTrustDialogAccepted: false`, since removed — while
  `/Users/llfourn/src/frostsnap` itself is trusted.
- **A checkout opened some other way** was not created by clank — but
  clank may still have WRITTEN in it, and in the reported case did:
  that directory's `.claude/settings.local.json` holds clank's three
  rules and nothing else.

The reported directory is the third kind. Created by somebody else,
written into by clank, and unexplained.

## Conclusion: nothing established, so nothing proposed

No actionable defect has been established from the evidence available
here, so this plan proposes no implementation. That is weaker than the
conclusion I wrote first, and the first one was wrong in the same way
the findings above had just been corrected for.

I argued that both fork-shaped paths were used heavily
(`$45` and `$28` of sessions) while their flag stayed `false`, so
neither can have been asked. It does not follow: a non-interactive run
incurs cost and skips the dialog entirely, and the records do not say
which mode, which version, or which key was consulted. Stored state
cannot answer a runtime question — which is what the section above
this one exists to say, and I did it again anyway (codex 889baf6).

**What the installed 2.1.280 code says** (codex read it; offsets in
the review):

- A linked worktree resolves to its canonical main checkout, so a
  worktree fork of a trusted repo plausibly does not prompt.
- The nearest git root is a BOUNDARY: the search returns false on
  reaching it without finding trust, before ascending. So an
  independent clone does not inherit from a directory above it, even
  at `<trusted-main>/.clank/clones/<name>`. Current documentation says
  the same — nested repositories are excluded from a parent's trust.

So a clone fork remains a plausible case where clank creates a root
that must be trusted afresh. Plausible, not observed: nobody has
watched one open.

**What is established**, and it is little: trust is recorded per
absolute path; the two mechanisms above are in the shipped binary and
the documentation; and the reported directory is not a clank fork of
either kind — no `.clank/fork.json`, not under `.clank/clones/`, not a
linked worktree.

**What would establish a defect**: a trust dialog in a directory clank
MADE, or in one clank WROTE INTO. Those are different sets, and only
the second contains the dialog anyone has actually seen.

- Made by clank: a path carrying `.clank/fork.json`, a path under
  `<main>/.clank/clones/`, or a linked worktree on a fork branch,
  which `--path` may put anywhere and which carries no descriptor.
- Written into by clank: any directory whose
  `.claude/settings.local.json` carries `CLAUDE_ALLOW_RULES`. The
  reported path is this kind.

Narrowing to the first set would have excluded the only evidence in
hand — the mistake this document has made in one direction or the
other throughout.

## Out of scope

- The other agent tools. If codex or others have an equivalent, that
  is their own plan.
- Widening what agents may write. Whatever the dialog is reacting to,
  nothing here asks for more permission than clank already writes.

## Tests

None: nothing changes. The claims above are evidence from this
machine's own state, recorded in the commits, and a test of clank
would not be a test of them.
