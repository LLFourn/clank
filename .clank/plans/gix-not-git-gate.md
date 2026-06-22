# gix-not-git-gate

Despite several plans to purge `git` shell-outs in favor of gix, agents keep
reintroducing `std::process::Command::new("git")` (e.g. `dirty_stats`,
`worktree_status`, found in the status-tui CPU investigation) because gix's
APIs are less ergonomic and NOTHING stops the regression. This is an
enforcement gap, not a one-site fix — the reason the same problem recurs.

## Reality (measured before scoping)

A single sanctioned shim is NOT viable: there are ~57 `Command::new("git")`
sites across 19 files; roughly 30 are PRODUCTION (non-test) code, spread
across `purge`, `rewrite`, `unfinish`, `finish`, `fork`, `pr_review`, `log`,
`diff`, `queue`, `shelve`, `html`, `doctor`, `init`, `open`, `open_zellij`.
They split three ways:
- **Genuinely not gix-able yet** — `git worktree add` (fork), history rewrite
  (purge/rewrite/unfinish/finish). These must stay shell-outs.
- **Convertible, not yet converted** — `status --porcelain` (unfinish),
  `log -1 --format` (log), `remote get-url` (pr_review). Future gix work.
- **Borderline** — `git diff <range>` patch synthesis (diff), where git's
  exact textual patch is the contract.

So the gate canNOT demand "one shim" and canNOT fail on every existing site
(converting them is explicitly out of scope). The right model is an
ALLOWLIST BASELINE that RATCHETS DOWN: record today's sites, fail on NEW
ones, shrink as conversions land.

## Fix — an allowlist-baseline ratchet

Runs under `cargo test` (there is no separate "gauntlet"/CI in this repo, so
it must be a normal test).

- A test scans PRODUCTION source under `crates/*/src` for git shell-outs:
  `Command::new("git")` and equivalent `"git"`-as-program constructions.
  - It EXCLUDES test code — `#[cfg(test)]` modules and `crates/*/tests/` —
    because test fixtures legitimately spawn `git` to build repos (the
    no-binary-spawning rule bans spawning the *clank* binary, not `git`).
    Suggested mechanism: brace-track from each `#[cfg(test)]` attribute to
    skip that module; skip integration-test dirs wholesale.
- It compares the found sites to a checked-in allowlist. Recommended shape:
  a per-file EXPECTED COUNT plus a one-line reason (line numbers churn; an
  exact-count-per-file ratchet does not). Example:
  `fork.rs = 4   # git worktree add — gix worktree support insufficient`.
- FAIL conditions:
  - A file's actual count EXCEEDS its allowlisted count → a NEW shell-out
    (the regression we're stopping).
  - A file's actual count is BELOW its allowlisted count → a site was removed
    without updating the allowlist → update it DOWN (keeps the ratchet honest;
    the baseline can only shrink).
  - A git shell-out in a file with NO allowlist entry → fail.
- Growing the allowlist (raising a count / adding a file) is the REVIEWED
  event: it requires a justification reason in the allowlist, which reviewers
  scrutinize against "is this genuinely not gix-able?".
- Document the rule where agents see it (CLAUDE.md + the master skill): prefer
  gix; a new `git` shell-out requires (a) it's genuinely not gix-able yet, and
  (b) an allowlist entry with a justifying reason.

## Out of scope

- Converting the not-yet-converted sites — separate plans, as encountered.
  This plan is the GATE that stops NEW shell-outs and ENUMERATES existing ones
  (with reasons); it does not rewrite them. The baseline is expected to be
  large at first and shrink over time.
- A single `git_shim` module — rejected above as infeasible given the spread
  of legitimate, necessarily-separate git operations.

## Testing (no-binary-spawning — in-process scan, never a spawned binary)

- The scanner flags a planted `Command::new("git")` in a non-test fixture
  string and IGNORES one inside a `#[cfg(test)]` module.
- The real tree PASSES against the checked-in baseline allowlist.
- A count mismatch (a fixture with one extra / one fewer site) fails as
  described.

## Acceptance

- A NEW production `git` shell-out outside the allowlist FAILS `cargo test`.
- Test fixtures remain free to spawn `git` (test code is excluded).
- The current production sites are recorded in the allowlist, each with a
  reason; the baseline can only ratchet DOWN without review.
- The rule is documented for agents (CLAUDE.md + skill).
