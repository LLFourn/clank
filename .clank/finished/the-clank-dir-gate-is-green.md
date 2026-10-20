# the-clank-dir-gate-is-green
# The .clank/ path gate is green

`env -u CLANK_AGENT cargo test -p clank --test it -- clank_dir_boundary::`
fails on master, and failed the same way before
`the-git-boundary-gate-sees-test-code` changed the shared stripper. It
reports five production paths under a repo's `.clank/` that are neither
declared nor computed:

- `cli/agent.rs:1509` and `:1510`: `.clank/{format!("{}.md", sha.as_str()}`
  (computed, undeclared)
- `cli/agent.rs:1692`: `.clank/roster.lock`
- `cli/setup.rs:814`: `.clank/setup-install.lock`
- `cli/setup.rs:1095`: `.clank/SKILL.md`

## Change

For each site, establish what the path is and whether it is right:

- If it is runtime state, meaningful only while its process lives (the
  locks look like it), move it to `monitor::runtime_path`, outside the
  repo, as the gate's message says.
- If it belongs to the repo, add it to `DECLARED` or `COMPUTED` with the
  reason.
- If the gate is misreading the code (for example `setup.rs`'s `SKILL.md`
  may be under `~/.claude/skills/<name>/`, not a repo's `.clank/`), fix the
  gate's recognition, with a fixture of that shape.

## Acceptance

- `clank_dir_boundary::` is green, and an undeclared `.clank/` path added
  to production still fails it.

## As built

Three cases, by who owns the storage:

- `agent.rs` feedback paths (`agents/<label>/feedback/<sha>.md`) and
  `setup.rs`'s skill-directory `SKILL.md` were the gate misreading: a local
  bound to `.clank` in one function (`roster_lock`'s, `installation_lock`'s
  `dir`) was treated as `.clank` across the whole file. The gate's scanner
  now reads the parsed source (`syn`, with `proc-macro2` span locations for
  line numbers): one scope per block, item and closure, a local a root
  exactly while the binding that made it one is in scope, shadowing with
  its own provenance, `if let`/`while let` (let chains included) binding
  only for the body they guard, `#[cfg(test)]` items and statements
  skipped, and a file that does not parse reported. Macro arguments are
  split at `,`, `;` and `=>` and read as expressions; a piece that does not
  parse is searched inside its groups, and reported if anything in it
  reaches the root (a `.clank` literal, a local bound to the root in this
  scope, or `clank_dir`), so syntax the gate cannot read never passes as
  safe.
  Test-only files are skipped through `Production::is_test_only`.
- `roster.lock` was real runtime state in the working tree. It now lives in
  the uid runtime namespace through `plan_mutation::runtime_lock(repo,
  "roster")`, the plan-mutation lock's opening code extracted unchanged:
  canonical repo identity, `O_NOFOLLOW`, mode 0600, blocking `flock` with
  EINTR retry, never unlinked in production. `TempRepo` removes it.
- `setup-install.lock` is keyed by setup's installation domain, the shared
  manifest home or a provider's own home, never a repo: declared in
  `HOME_SITES` for `setup.rs`'s `domain`, with its locking unchanged.

Tests: scoping fixtures (the feedback subtree, an unrelated `dir` after a
neighbour's block, a nested use inside the block, a computed child through
a binding, a repo variable named `home`, a `}` inside a string, a shadow
under `agents`, an inner shadow that ends with its block, `if let`, `while
let` and a let chain with an `else`, a macro argument, a repeat-form `vec!`,
unreadable macro syntax with and without a grouped expression, a bound
root used in a `tokio::select!` arm, unreadable syntax reaching a bound root
and its quiet unrelated control, a quiet macro, test code, and an unparseable file), the declared domain site and its
negative, the concurrent-add test with one add through a symlinked alias
and no `.clank/roster.lock` left behind, and a direct alias exclusion test.
Mutants caught: a new undeclared production child (behind a literal `}`),
inner scopes never ending, a non-root shadow not rebinding, macros unread,
`if let` and `while let` traversed without scopes, an unparsed macro piece
dropped, its groups not searched, bindings ignored in unread syntax, arms
not split at `=>`,
test code scanned, an unparseable file passing, the domain undeclared, the
lock keyed by spelling, the lock never taken, and the roster lock moved back
into the repo.
