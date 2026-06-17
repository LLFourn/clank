# fork-codex-cd-flag

`clank fork --pr <n>` (and any fork of a codex session) makes codex
prompt the user on launch:

```
Choose working directory to fork this session
  1. Use session directory (/…/frostsnap)            ← source repo
› 2. Use current directory (/…/frostsnap/.clank/worktrees/pr-497)  ← worktree
```

Every fork forces a manual pick of option 2. clank always wants the
worktree, so this prompt is pure friction.

## Cause (found)

`compose_fork_launch` (agent.rs:~324) launches codex as:
```rust
Tool::Codex => { args.push("fork".into()); args.push(spec.from_session.clone()); }
```
with the comment *"no `--cd` needed: the pane's cwd IS the worktree."*
That assumption is FALSE: codex detects that the launch cwd (the
worktree) differs from the session's recorded cwd (the source repo)
and asks the user to disambiguate. The pane cwd being the worktree is
exactly what triggers the picker, not what suppresses it.

`codex fork --help` (codex-cli 0.139.0) has the flag:
```
-C, --cd <DIR>   Tell the agent to use the specified directory as its working root
```

## Hypothesis + fix

Passing `-C <worktree>` on the codex fork launch should tell codex
the working root explicitly and skip the picker. The worktree path is
already available at `clank agent start` time as the resolved `repo`
(the fork's panes run `clank agent start <label> --repo <worktree>`,
cwd = worktree), but `compose_fork_launch` doesn't currently receive
it.

Fix:
- Thread the resolved worktree path into `compose_fork_launch` and,
  for `Tool::Codex`, append `-C <worktree>` (claude forks via
  `--resume … --fork-session` and does NOT prompt, so leave it).
- Correct the now-false "no `--cd` needed" comment.
- Resolve where the dir comes from: the `repo` already resolved in
  `start()` is the worktree; pass it down rather than re-deriving.

## Open question to VERIFY first (the investigation core)

Confirm `-C/--cd` actually SUPPRESSES the picker (vs just setting the
dir and still prompting). Verify live by forking a codex session into
a worktree with `-C <dir>` and checking no picker appears — analogous
to how the zellij tab spike was confirmed by running the real command.
If `--cd` alone doesn't suppress it, investigate a codex config knob
(`-c <key=value>`) or `--last`-style non-interactive flag. Do NOT ship
the arg change until the suppression is confirmed.

## Tests

- Pure: `compose_fork_launch` for `Tool::Codex` includes `-C
  <worktree>` (and claude's path is unchanged) — unit test on the
  composed argv, no spawning.
- The live suppression check is manual (codex is a TUI) and recorded
  in the plan, per the no-binary-spawning-tests rule.

## Non-goals

- Claude's fork path (it doesn't prompt).
- Broader codex launch-flag changes beyond the cwd picker.

## Verification (resolved)

The live picker-suppression check can't be observed from the headless
agent shell (codex is an interactive TUI; no non-interactive query
exists). The user SIGNED OFF on shipping by strong inference — codex
`-C/--cd <DIR>` is documented as "use the specified directory as its
working root", so naming the worktree removes the cwd ambiguity that
triggers the picker. Real-world confirmation is deferred to the
user's next `clank fork --pr`; they will report if the picker still
appears, at which point the fallback (a codex config knob) is
revisited. Shipping on that basis: the change is low-risk and
reversible, and it strictly adds an explicit `-C` that codex itself
documents as the working-root selector.
