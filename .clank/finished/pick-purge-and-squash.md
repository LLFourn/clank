# pick-purge-and-squash

Give `clank pick` the finish flow's two collapse modes, adapted to pick's
cross-base cherry-pick:

- `clank pick --purge <plan> --from X` — copy the plan's commits but STRIP its
  `.clank/` artifacts out of the copied stack (like `clank purge`): you get the
  code, not the plan file / review scaffolding.
- `clank pick --squash <plan> --from X` — collapse the plan's copied commit
  stack into ONE commit on the current branch.
- The two INTEROPERATE: `--purge --squash` yields a single clean commit
  carrying the plan's code with no `.clank/` in it.

## Why pick can't just reuse finish's engine

`clank finish --purge/--squash` runs the rewrite engine (`apply_squash`,
`replay_commit`, `strip_tree`) which is TREE-PRESERVING — sound only onto the
SAME base. `pick` deliberately cherry-picks (diff-based 3-way, git_plumbing
`cherry_pick`, pick.rs) because the plans land on a DIFFERENT base. So the
CONTENT decisions are shared with finish, but the APPLICATION differs:

- **What to strip** — reuse the existing `.clank/` strip-path computation
  (`preview.rs` `head_strip_paths` / `git_io::tree_clank_paths`) and
  `git_plumbing::strip_tree`. Do NOT reinvent the path set.
- **The squash subject/body** — reuse `compose_squash_message`
  (tui-squash-message-body) so a squash carries the plan's own finalize WHY,
  same as everywhere else. Do NOT invent a message.

## STUDY the 3-way cherry-pick implications DEEPLY, then FLAG them (lloyd)

The cross-base 3-way cherry-pick is where the genuine design risk lives, and it
interacts with both new modes in non-obvious ways. Before writing code, the
implementing agent MUST study each of the following against LIVE git behavior —
not assume it — settle the approach, and FLAG the implications and the chosen
mechanism to the reviewers in the implementation commit. Do NOT paper any of
these over with an unverified "git does X here" comment; verify against real
git and state what you found.

- **Empty-after-strip commits (--purge).** A commit whose ONLY change is a
  `.clank/` artifact (e.g. the intro that adds `plans/<stem>.md`, or a plan-body
  revision) becomes EMPTY once `.clank/` is stripped. Drop it, keep it empty,
  or fold it? finish's engine has a `Drop` disposition for exactly this; the
  3-way path needs its own deliberate answer, and the answer changes what
  `--purge` (without `--squash`) even produces.
- **Stacking `cherry-pick --no-commit` onto a DIRTY index (--squash).** The
  squash wants to accumulate several commits in the index before one commit.
  Verify git actually permits a `--no-commit` cherry-pick onto an index that
  already holds the previous commit's staged changes, and what merge/conflict
  semantics result — this is the assumption the whole squash mechanism rests on.
- **Conflict mid-accumulation.** A 3-way conflict partway through a squash
  leaves a PARTIALLY staged index, not the clean single-commit-state pick has
  today. Define the bail (abort guidance) and prove the source stays untouched.
- **3-way base vs cumulative tree.** Each cherry-pick merges against the
  commit's own parent in the SOURCE; replayed onto a different target base and
  an accumulating index, the squashed result may not equal the plan's
  end-state tree the way a same-base squash would. Study whether the collapsed
  tree matches the plan's cumulative end state, or can silently drift — this is
  the correctness heart of the feature.

The implementation should present these findings explicitly so reviewers can
check the reasoning, not just the code.

## Mechanics (as a design to settle, not prescribe)

Pick currently cherry-picks each commit committing-as-it-goes (`cherry_pick`
returns false on conflict, pick.rs:197). Both new modes want to accumulate
before committing, which suggests a `cherry_pick_no_commit` git_plumbing
primitive (stage the changes, don't commit):

- **--squash (per plan):** cherry-pick every commit of the plan `--no-commit`
  so the changes stack in the index, then make ONE commit with
  `compose_squash_message`. With multiple plans, squash EACH plan into its own
  one commit (N plans → N squashed commits, in source order) — NOT all plans
  into one; the plan is still the unit.
- **--purge:** strip the plan's `.clank/` paths from what lands. Cleanest is to
  strip the index (or the resulting tree via `strip_tree`) before the commit,
  so no `.clank/<stem>` blob is ever committed in the copy. Decide: strip
  per-commit (verbatim stack minus `.clank/`) vs only meaningful with --squash.
- **--purge --squash:** cherry-pick `--no-commit` the whole plan, strip
  `.clank/` from the accumulated tree, commit once.

All existing pick refusals still apply UNCONDITIONALLY: interleaved foreign
commits inside a plan's span (named offenders, pick.rs:114), and a dirty target
tree (checked BEFORE --dry). Conflicts still stop in git's cherry-pick state
with --abort guidance; the source is never touched either way.

## --dry MUST reflect the collapse (one-computation rule)

pick --dry already prints the resolved order + exact commit list from the same
computation the live run applies. Extend it, NOT with hand-rolled narration:
--dry with --squash prints the resulting single-commit-per-plan shape (and the
composed subject), and with --purge notes the `.clank/` strip — computed from
the SAME code the live run executes, so the preview can never diverge from what
lands. The dirty-tree refusal stays above the --dry branch so --dry refuses
exactly what execute would (the rule lloyd enforced on finish and pick).

## Decisions to flag for review

- **Squash message source.** Lean: reuse the plan's finalize body via
  `compose_squash_message` (consistent with TUI/finish), no new `-m` on pick.
  Reviewer's call whether pick also accepts an explicit `-m` override.
- **--purge without --squash.** Does stripping `.clank/` from a multi-commit
  verbatim stack make sense, or is --purge only meaningful WITH --squash?
  Lean: allow both, purge each cherry-picked commit's tree; but confirm the
  per-commit strip produces sensible individual commits.
- **git-layer:** any new `cherry_pick_no_commit` lands INSIDE git_plumbing with
  its why-not-gix note; pick.rs stays free of inline git (boundary test).

## Tests (in-process; git fixtures OK)

- `pick --squash <plan>`: the plan's N commits land as ONE commit whose subject
  is the composed message; source-order preserved across multiple plans.
- `pick --purge <plan>`: no `.clank/<stem>` path exists in any landed commit's
  tree; the code changes do.
- `pick --purge --squash <plan>`: one commit, code present, `.clank/` absent.
- `--dry` for each mode is a strict no-op (HEAD, refs) AND prints the shape the
  live run then produces (assert dry-vs-live agree on commit count + subject).
- Interleaved-foreign and dirty-tree refusals still fire, in both dry and live.

## Acceptance

- `clank pick --purge` / `--squash` / `--purge --squash` work and interoperate,
  reusing finish's strip-path set and squash-message composition.
- --dry reflects the collapsed shape from the same computation as execute.
- Source never modified; existing pick refusals intact; boundary/clippy/fmt/
  suites green.
