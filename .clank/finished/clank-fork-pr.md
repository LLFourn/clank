# clank-fork-pr — `clank fork --pr <number>` sugar

Mini plan (lloyd 2026-06-10): fork a worktree directly onto a
GitHub PR with one flag. Everything it composes already shipped
in clank-fork-worktree-sessions; this derives the inputs:

```
clank fork --pr 123            # name pr-123, on the PR head,
                               #   team oriented to the review
clank fork mything --pr 123    # explicit name, PR head + prompt
```

- **name** defaults to `pr-<number>` when the positional is
  omitted (`--pr` makes the positional optional; without --pr it
  stays required).
- **base**: fetch the PR head — `git fetch <remote> pull/<n>/head`
  (GitHub's refspec; works for same-repo and fork PRs) and use
  FETCH_HEAD as the `--branch` base. Remote: `origin` for v1.
- **orientation prompt** defaults to
  `reviewing PR #<n>: <title>` — title via
  `gh pr view <n> --json title` BEST-EFFORT (gh missing/unauth →
  fall back to `reviewing PR #<n>`; the fetch above needs no gh).
  An explicit `--prompt` still wins.

## Surfaces

- `ForkArgs`: positional `name` becomes optional + `--pr <N>`
  (`u32`); validation: name xor derivable (no --pr and no name →
  the current clap required error, via `required_unless_present`).
- `fork.rs run_fork`: when `--pr` is set — derive name, run the
  fetch (one git subprocess; error surfaced verbatim on bad
  PR/remote), resolve FETCH_HEAD to a sha (pin it — FETCH_HEAD
  is volatile), pass as the base; compose the default prompt.
- Keep the core env-free/testable: the PR-derivation step
  (name/base/prompt from (pr, explicit-name, explicit-prompt,
  title-lookup-result)) is a PURE function; the fetch + gh title
  lookup stay at the shell. Tests pin the derivation matrix +
  an integration test forking onto a fetched ref from a local
  "remote" (a second TestEnv repo added as origin — no network,
  no gh; the gh path is best-effort by construction).

## Out of scope

- Non-GitHub forges (the pull/N/head refspec is GitHub's).
- clank-pr-review-mode's review loop — this is just the worktree
  half; the review artifact machinery comes separately and will
  use exactly this flag.

## Status

Mini stub — queued lloyd 2026-06-10 ("it would be super cool if
we could just do clank fork --pr <pr-number>").
