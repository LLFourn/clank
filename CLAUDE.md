# Working on clank

## All git/gix access goes through the layer

Production code must NOT call `std::process::Command::new("git")` or use
`gix` directly. Two modules own all git access; everything else calls their
typed, backend-agnostic API:

- **`crates/cli/src/git_io.rs`** — git **reads** (gix-backed; a few justified
  subprocess reads remain where gix can't reproduce git's exact output yet).
- **`crates/cli/src/git_plumbing.rs`** — git **mutations** (gix object/ref
  writes + subprocess for what gix can't do: worktree, fetch, checkout,
  cherry-pick, index/commit ops).

Callers shouldn't know or care whether an operation is gix or a subprocess —
that's the point. Prefer gix; reach for a subprocess only when gix genuinely
can't do it yet, and add it INSIDE one of these two modules with a one-line
"why not gix" justification on the function.

This is enforced: `crates/cli/tests/git_boundary.rs` fails the build if any
production code outside the two layers names `git`/`gix`. Test code is exempt
(fixtures may spawn `git` and open gix repos).

When you find a subprocess read/write that gix *could* do, convert it inside
the layer — callers don't change.

## Running tests

Run the tests for what you touched, never the world. All integration
tests are modules of ONE harness, `crates/cli/tests/it/`, so a lib
change costs one link and one fresh binary, not twenty-four:

```sh
cargo test -p clank --lib <module>            # unit tests, e.g. cli::open_zellij
cargo test -p clank --test it <module>::      # one integration module, e.g. fork_integration::
cargo test -p clank --test it                 # every integration test, one process
```

Gates that scan source (`git_boundary`, `zellij_ownership_boundary`,
`zellij_cost_boundary`, `no_json_literal_config_writes`) are modules
of the same harness; run the one that guards the files you changed.
The bin has no test target: its `Cli` and the README tests live in
`cli::command`.

On macOS, grant the terminal Developer Tools permission once, or every
fresh test binary spends a minute in Gatekeeper before its first test
(README → Troubleshooting).
