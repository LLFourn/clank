# stash-does-what-you-mean

`clank stash` refuses to do the one thing it is for. On `master` —
the branch every clank plan is worked on — `clank stash push` says:

> refusing to rewrite protected branch `master` in place. Pass
> `--allow-rewrite-protected` to override.

Stashing IS rewriting; that is its whole point. Then, past the flag,
it asks `[y/N]` for an operation that is reversible by construction
(`pop`), and reads that answer from a stdin that is `/dev/null` for
every agent, so it aborts unless `--yes` is also passed. The command
that does the job is `clank stash push <plan> --allow-rewrite-protected
--yes`. Nobody means that.

## Protection nobody wants, enforced on nobody

`is_protected_branch` (`main`, `master`, or `branch.<x>.protect`) sits
in the rewrite engine and refuses unless `allow_rewrite_protected`.
Every caller but two sets it to `true`: `rereview`, the TUI's four
stash/purge paths, `finish`'s finalize, and `finish` with autosquash
("the natural clank workflow finalizes on the working branch, which
is often master, and collapsing the plan there is the whole point").
The two that don't are the CLI's `stash push` and `purge`, so the same
rewrite is refused from the shell and performed from the TUI. A guard
that every path bypasses is not a guard; it is a model of a danger the
tool does not believe in, and the flag is the tax for that.

The confirmation is the same shape: `push` keeps the commits on a
protective ref and `pop` puts them back, so there is nothing a `y`
protects. And clank asks no `[y/N]` anywhere: `drop`'s and `purge`'s
prompts go too, along with every `--yes` that existed to skip them.
What a prompt used to say is said on stderr and the run proceeds —
the banner is the audit trail, not a question.

## Change

**Rewrite protection goes.** `is_protected_branch` and the
`allow_rewrite_protected` field leave `RewriteOpts` and both engine
sites; `purge`'s copy of the check goes with it; the
`--allow-rewrite-protected` flag leaves `stash push`, `purge`, and
`finish`; the autosquash "implies" special case and its rationale
leave `finish`; the TUI and `rereview` stop setting a field that no
longer exists. The other blockers stay — a dirty working tree, a
target not on HEAD, foreign commits in the range — because those
protect WORK, not a branch name. The `branch.<x>.protect` git config
key stops meaning anything to clank.

**Nothing asks.** No `[y/N]` and no `--yes` on `stash`, `drop`, or
`purge`. The record and the protective ref are written before the
branch moves, exactly as today; that ordering is the safety, and it
does not need a prompt in front of it. `purge --drop` and `purge
--all` keep their banners, on stderr, as the record of what ran.

**Pop uses the engine's dirty-tree policy.** Pop refused on a raw
status, which counts clank's own untracked `.clank/` scratch — the
very record the push wrote, in a repo whose `.clank/.gitignore` does
not cover `/stash/`. The rewrite engine already ignores that scratch
(rewrite-scratch-dir-worktree); pop reads the same policy.

**Bare `clank stash` pushes, like `git stash`.** `clank stash` stashes
the one in-flight plan, `clank stash <plan>` the named one — the
resolution `push` already has. The listing moves to `clank stash
list`, git's own verb for it. `push` stays as the explicit spelling.

**`pop`, `show`, `drop` infer the single stash.** With exactly one
stashed plan the name is optional; with several, the message lists
them. Same rule `push` uses for the single active plan.

The `shelve`/`unshelve` hidden aliases follow (they are adapters over
these functions). The README's command table and the `stash` help
text say what the command does now.

## Tests

- `stash push` on a branch named `master` (a fixture repo's default)
  performs the rewrite with no flag; the same on any branch. The
  engine has no protected-branch blocker to test; the tests that set
  the field are updated, and the one that overrode it to reach a
  different assertion no longer needs to.
- `push` with stdin at EOF (what an agent has) completes; the record
  and ref exist before the branch moves, as today's ordering test
  pins.
- Bare `clank stash` with one in-flight plan pushes it; `clank stash
  list` lists; `clank stash <plan>` names it.
- `pop`/`show`/`drop` without a name: exactly one stash → that one;
  two → refused with both named; zero → "nothing stashed".
- `drop`, named, asks nothing and discards; `purge` asks nothing and
  prints its banner.
- The README names only real subcommands (existing test) — `list`
  is real and the flag is gone from the table.

## Out of scope

- Stashing with a dirty plan body or a dirty working tree (the
  data-loss refusals stay, with their messages).
- The TUI's own confirm screens and type-the-stem arming: UI
  affordances the operator sees, not questions the cores ask.
