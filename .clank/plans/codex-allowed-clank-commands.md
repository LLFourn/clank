# codex-allowed-clank-commands
# Seed codex's allowed-commands list with every clank subcommand so the user isn't prompted to approve each one

## Problem

Reported by lloyd 2026-06-04: codex prompted to confirm `clank finish`. Same prompt class fires for every `clank <verb>` shape not previously seen. Every gated approval interrupts the workflow.

The clank/setup pipeline writes the Stop hook into codex's `~/.codex/hooks.json` and seeds the SKILL into `~/.codex/skills/clank/`. But it does NOT register `clank` in codex's command rules file. That gap is why each new `clank X` shape requires user approval.

## Verified before promotion

**Q1 — codex allow-list location and schema (verified by reading `~/.codex/rules/default.rules`):**

File: `~/.codex/rules/default.rules`
Format: line-based DSL; each line is `prefix_rule(pattern=[<token>, ...], decision="allow"|"deny")`. Pattern is a token-level prefix that the invoked command must start with.

Example existing lines in the user's file:
```
prefix_rule(pattern=["cargo", "test"], decision="allow")
prefix_rule(pattern=["clank", "init"], decision="allow")
prefix_rule(pattern=["clank"], decision="allow")
```

The last line — `pattern=["clank"]` — is the exact entry this plan wants to install. It should match every `clank <subcommand>` invocation as a prefix.

Note: the user ALREADY has this line (line 57 of their file), yet was still prompted for `clank finish` in the reporting session. Possible reasons:
- Codex doesn't hot-reload rules during a running session; the rule was added late and won't apply until next session start.
- Or codex's prefix matching has a subtle gap with bare single-token patterns.

Either way, automating the write closes the gap idempotently — new users get it pre-installed, existing users see a no-op.

**Q2 — clank init vs clank setup wiring (verified by reading `crates/cli/src/cli/init.rs` + `setup.rs`):**

`clank init` writes only repo-scope assets. It calls `write_claude_perms` for claude's repo-scope permissions. It does NOT invoke `clank setup`. `clank setup` is the user-scope installer (touches `~/.claude/` and `~/.codex/`). Layering: setup = user-scope; init = repo-scope. Don't conflate.

Therefore: the rule write belongs in `clank setup`, not `clank init`. Users who run only `clank init` get told to run `clank setup` via the doctor check (existing pattern; see `clank doctor`'s user-scope section).

## Approach

1. **Extend `clank setup` to ensure the codex rule is present.** Read `~/.codex/rules/default.rules` (or create it if missing). If a line matching `prefix_rule(pattern=["clank"], decision="allow")` exists (exact match on the pattern + decision), no-op. Otherwise, append the line. Same fail-closed semantics as the rest of setup: malformed file → error, never silently overwrite other rules.

2. **Doctor check**: extend `clank doctor`'s user-scope section with a "codex command rules include `clank` prefix" entry. Reads the file, parses for the matching line, Warn if missing. Names the fix command (`clank setup`).

3. **Allow-list granularity decision: bare `["clank"]` prefix.** Whitelists every `clank <subcommand>`. clank is a peer-review workflow tool; agents are expected to run any of its commands, and the gate state machine itself enforces "did this agent have the right to do that." A second layer of per-command approval-checks via codex's rules file is redundant and adds maintenance burden every time a new subcommand ships.

## Out of scope

- The claude side: claude's hook + permissions surface is already handled by `write_claude_perms` in `init.rs`. Codex parity is the gap this plan closes.
- Restructuring codex's existing config format. The DSL line-append + grep-for-presence pattern is sufficient.
- Generalizing to per-subcommand rules. If a user wants finer-grained control, they edit the file by hand — that's the codex UX.
- Touching `clank init`. User-scope installation stays in `clank setup`.

## Acceptance

- After `clank setup`, running any `clank <subcommand>` from inside a codex session does NOT prompt the user for approval (modulo any session-cache caveats outside our control).
- Idempotent: re-running `clank setup` doesn't duplicate the entry.
- A malformed `default.rules` file errors out cleanly (fail-closed); the existing rules aren't overwritten.
- `clank doctor` includes a check for the codex allow-list entry; Warn when missing with `clank setup` as the suggested fix.
- Existing claude-side permissions writes unchanged.

## Tests

- **Integration test (setup writes rule)**: temp HOME, run `clank setup`, read the resulting `~/.codex/rules/default.rules`, assert the `prefix_rule(pattern=["clank"], decision="allow")` line is present.
- **Integration test (idempotent)**: pre-populate the file with the line, run `clank setup`, assert exactly one matching line afterwards.
- **Integration test (creates file)**: temp HOME with no `~/.codex/rules/` dir at all, run `clank setup`, assert the dir and file are created with the entry.
- **Integration test (other rules preserved)**: pre-populate file with unrelated rules, run setup, assert the original rules are still present + the clank line was appended.
- **Doctor test**: temp HOME with no rule → Warn entry naming "clank setup" as fix; with rule → no warn.

## Implementation note

The line should be appended at the end of the file (not inserted at a specific position) to avoid disrupting the user's existing rule order. Codex's docs (or testing) should confirm whether rule order affects matching — if last-match-wins, append is safe; if first-match-wins, the position might matter for the clank rule to take effect ahead of any potential deny rule. Current observation: the user's file has the bare `["clank"]` at line 57 of ~57 lines (the end) and all earlier rules are more specific allows, so trailing-append is consistent with the existing convention.
