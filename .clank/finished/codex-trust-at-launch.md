# codex-trust-at-launch

Launching codex in a repo whose root isn't recorded as trusted in
`~/.codex/config.toml#/projects` shows the interactive "Do you trust
the contents of this directory?" prompt. fork-codex-cd-flag fixed the
working-directory PICKER, not this — nothing in clank handles trust,
and the fork-robustness fresh-session path surfaced it (lloyd,
frostsnap-ci worktree). Any codex launch in an untrusted root can hit
it.

The user running `clank fork` / `clank open` on a repo IS the trust
decision — clank-launched agents must never see the prompt.

## Where (the chokepoint already exists)

Every agent launch funnels through `clank agent start` (zellij panes
and console screens both run it), and codex argv is composed only in
agent.rs's three composers, exec'd only by `exec_composed`. Centralize
there: before a REAL launch (not `--print`) of `Tool::Codex`, run

```
ensure_codex_project_trust(home, main_repo_root(&repo))
```

- The trusted path is the MAIN repo root (`main_repo_root`, as fork
  already computes): codex's own prompt says trust applies to the
  repository root, so trusting it covers every worktree under it.
- State the invariant in a comment at the chokepoint: codex is never
  spawned anywhere else, so trust-at-compose is trust-everywhere.

## ensure_codex_project_trust semantics

Mirror what codex itself writes when the user answers Yes:

```
[projects."/path/to/root"]
trust_level = "trusted"
```

- If `~/.codex/config.toml` already contains the exact
  `[projects."<root>"]` header — WHATEVER its trust_level — do
  nothing. This falls out of a textual containment check and is the
  right semantics: an existing entry is a prior user decision (maybe
  an explicit distrust); never override it.
- Otherwise append the two-line table at the end of the file (TOML
  table headers are position-independent; appending preserves the
  user's file byte-for-byte). Create the file if absent.
- No toml crate: exact-match check + append. The only paths we write
  are canonicalized absolute repo roots (no quote-escaping games).
- Best-effort: a failed write warns on stderr and the launch
  proceeds (the prompt appearing is degraded UX, not a broken
  launch).
- claude needs no equivalent (no such prompt); scope is codex only.

## Tests (pure/in-process)

- `ensure_codex_project_trust` unit tests against a temp home:
  appends to a missing file; appends to an existing file preserving
  its content verbatim; no-ops when the root's header exists with
  `trust_level = "trusted"`; no-ops (and does NOT rewrite) when it
  exists with a DIFFERENT trust_level; two different roots both land.
- Launch-path test: composing a real codex launch writes the entry
  for the main repo root; `--print` writes nothing; claude launches
  write nothing.
- A worktree launch trusts the MAIN root, not the worktree path.

## Acceptance

- Fresh codex sessions (fork fallback, bootstrap) and forked codex
  sessions in a previously-unseen repo launch without the trust
  prompt.
- An existing `projects` entry is never modified.
- `--print` and claude launches leave `~/.codex/config.toml`
  untouched.
- clippy/fmt/suites green at baseline.
