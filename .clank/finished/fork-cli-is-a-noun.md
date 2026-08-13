# fork-cli-is-a-noun

## Problem

`clank fork` is a bare verb taking a positional name
(`clank fork [NAME] [SOURCE]`). Every other durable, multi-operation
noun in the CLI uses subcommands: `queue add/remove/promote`,
`agent add/list/remove`, `events list/ack/show`, `block create/clean`,
`pr-review start`.

Agents generalise the house style, type `clank fork list` expecting a
listing, and get a fork named `list` — a real worktree, a real branch,
and the whole team's sessions forked into it. Reported as a recurring
failure, not a one-off.

The severity is in the failure MODE, not the typo. This is not a
rejected command; it is a successful wrong action that leaves durable
state behind. A footgun that fires silently is worth more than a
grammar tweak to remove.

## The modeling error

`fork` is modelled as a verb, but a fork is a NOUN with a lifecycle:
it is created, it persists on disk as a worktree or clone with a
descriptor (`.clank/fork.json`), and it is eventually removed. The
grammar denies the lifecycle, so the only operation expressible is
creation — which is also why there is no way to list forks at all.

Agents reaching for `fork list` are not confused about clank. They are
correctly inferring a capability that ought to exist and finding the
grammar has given its slot away to a free-text name.

## Goal

`clank fork` becomes a noun with an explicit lifecycle, and listing
forks becomes possible.

## Approach

1. **Subcommands**, matching existing vocabulary (`block create`,
   `queue remove`):
   - `clank fork create [NAME]` — today's `clank fork <name>`, every
     flag unchanged INCLUDING the derived-name rule: `name` is
     `required_unless_present = "pr"` (`mod.rs:152-155`), because
     `--pr N` defaults it to `pr-<N>`. The grammar is
     `create [NAME]`, not `create <name>`.
   - `clank fork list` — worktree AND clone forks, with enough to act
     on: name, kind, path, branch.
   - `clank fork remove <name>` — teardown, specified below. It is
     the only destructive addition and needs more care than the
     other two combined.

2. **No bare-positional fallback.** Do NOT keep `clank fork <name>`
   working as an unrecognised-word alias. That compromise preserves
   the ambiguity that IS the bug: `list` stays a coin flip, every
   future subcommand (`ls`, `show`, `prune`) re-opens it, and a fork
   legitimately named `list` becomes unreachable. Requiring the
   subcommand makes the failure impossible by construction rather than
   unlikely, and clap's unknown-subcommand error teaches the new form
   for free. Accept the break deliberately.

3. **Reuse the existing enumeration.** `clone_fork_paths`
   (`fork.rs:102`) already walks descriptor-validated clones and
   `linked_worktree_on_branch` (`fork.rs:95`) resolves worktrees.
   `list` is wiring over these, not new discovery logic. Keep the
   descriptor as identity — the `--clone` help already states the
   descriptor, not the path, is the identity.

4. **Surface forks in `clank status`.** A fork is durable repo state
   of exactly the kind status already reports (queue, stashed). Its
   absence is why forks are invisible enough to strand one for 24h.
   One line per fork alongside the stash lines.

5. **Sweep the whole repo, not just `crates/*/src`.** `clank fork ` is
   named in installed skills (`setup_assets/`), docs, README, tests
   and error strings. Clank installs the very skills that teach agents
   this recipe, so the command and its documentation must move in the
   SAME commit — otherwise clank ships instructions for a command it
   no longer has. A src-only grep makes the gate find stragglers one
   cycle at a time.

## `fork remove` safety (the destructive half)

Today teardown is ADVICE, printed at create time: `git worktree remove`
for a worktree (which refuses dirty state on your behalf) and raw
`rm -rf` for a clone (`fork.rs:761-766`), executed by the human who can
see what they are about to delete. Turning that into a first-class
command moves the decision to clank and makes it look authoritative, so
it must be at least as careful as the human was.

- **Resolve only through the validated fork identity.** `remove` takes
  a NAME and resolves it via the same descriptor-validated lookup the
  rest of the feature uses (`validated_clone_fork` /
  `linked_worktree_on_branch`, `fork.rs:95-121`). It never accepts a
  path and never deletes a directory it did not resolve. A squatter
  directory, a foreign worktree, or a stale descriptor resolves to
  NOTHING and the command refuses.
- **Refuse dirty or untracked state by default, for BOTH kinds.** The
  clone path is the dangerous one: `rm -rf` has no opinion, so the
  guard has to be ours. Uncommitted changes or untracked files → refuse
  and name them.
- **Branch retention is NOT symmetric, and the plan must not pretend
  it is.** A worktree fork's branch is created in the SOURCE ref store
  (`worktree_add -b`), so removing the worktree retains it — the branch
  name is printed so the human can delete it deliberately. A clone
  fork's branch is created INSIDE the clone (`clone_local`:
  `git clone`, then `checkout -b <name>` in the destination,
  `git_plumbing.rs:400-421`), so deleting the clone directory destroys
  that ref. The source never had it.

- **Therefore, for a CLONE, removal is permitted only when every commit
  reachable from its branch is already reachable from a source ref.**
  This is not a courtesy check; it is what makes clone deletion
  non-lossy BY CONSTRUCTION rather than by promise. Unique commits →
  refuse, naming how many and the branch that holds them. For a
  worktree the same check is a nicety (the branch survives regardless);
  for a clone it is the whole safety argument.

  The rejected alternative is ARCHIVING the clone's branch into the
  source before deleting. That needs a destination ref namespace,
  existing-ref conflict semantics, and atomic rollback when the
  transfer half-succeeds — none of which this plan specifies, so it is
  not claimed. A later plan may add it.

- **`--force` overrides the two refusals — dirty state and unreachable
  commits — and NEVER the identity check.** There is no way to make
  `remove` delete something it could not resolve as a fork. Forcing a
  clone with unique commits PERMANENTLY DESTROYS them, and the command
  says so with the count before doing it.

## Required tests

- `clank fork list` on a repo with one worktree fork and one clone
  fork reports both, with kind and path.
- `clank fork list` on a repo with none reports empty, not an error.
- `clank fork create <name>` produces what `clank fork <name>` did:
  assert on the composed spec, not by launching anything.
- The bare form is REJECTED: `clank fork <name>` errors rather than
  creating. This is the regression the plan exists for — it must fail
  loudly, and the test asserts no worktree was created.
- `clank fork create` with `--pr` and NO name still derives `pr-<N>`;
  without either it errors. Pins the required-unless-pr rule.
- **`remove` refuses what it must**: an absent name, a foreign or
  squatter directory that does not resolve as a fork, a dirty worktree,
  a dirty clone, and a fork holding commits the source cannot reach —
  each asserting the target still EXISTS afterwards.
- **`remove` works when it should**: a clean worktree fork is removed
  and its source branch SURVIVES; a clean clone fork whose history is
  fully reachable from the source is removed, and its branch is gone
  with it — asserted, because that is the asymmetry a reader will
  otherwise assume away.
- **Clone history is protected**: a clone holding a commit no source
  ref can reach REFUSES removal, and the commit is still there
  afterwards. This is the test that makes clone deletion safe rather
  than merely documented.
- Skills shipped by `setup_assets` document `fork create`, and no
  shipped asset still names the bare form.

## Acceptance

- No BARE token can be interpreted as a name: `clank fork list` lists.
  `clank fork create list` remains VALID and creates a fork named
  `list` — the invariant is about ambiguity, not about forbidding the
  word.
- Forks are listable, and visible in `clank status`.
- A full-repo grep for the bare recipe finds only intentional
  historical mentions (finished plans, this plan's narrative).

## Out of scope

- The stale-binding launch bug (`fork-stale-binding-launches-fresh`,
  queued ahead of this). Keep them separate: that one is a silent
  breakage worth shipping on its own, this one is a grammar change.
- Renaming `--clone`, `--pr` or any existing flag. Flags move to
  `create` unchanged.
