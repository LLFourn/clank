# codex-allowed-clank-commands
# Install a `clank` allow rule in codex's command-rules file during `clank setup`

## Scope honesty (per ruthless review of 3688c72)

This plan was originally framed as "fix the `clank finish` approval prompt." On verification that framing turns out to be misleading. The user already has `prefix_rule(pattern=["clank"], decision="allow")` at line 57 of `~/.codex/rules/default.rules` AND they were still prompted for `clank finish`. Two possible causes:

1. **Running-session reload gap**: codex may not hot-reload rules. The bare `["clank"]` was added late (likely by the user clicking "always allow" earlier in the session), and the running session had cached rules from before that. New sessions would pick up the rule fine.
2. **Prefix-arity mismatch**: codex's `prefix_rule` may require exact-arity match (pattern length == command length), in which case `["clank"]` matches only the bare `clank` invocation, never `clank finish`. The user's file having BOTH `["clank", "init"]` and `["clank"]` is consistent with this — the more specific entry would be redundant under true prefix matching.

Without reading codex's source (it ships as a binary), neither hypothesis can be confirmed from inspection alone.

**This plan installs a baseline `clank` rule during `clank setup` so new users start with it. It does NOT guarantee fixing the symptom that triggered the report — that fix depends on which of the two hypotheses above is correct. If hypothesis 1: the rule takes effect on next codex session start. If hypothesis 2: this plan ships the wrong rule shape and a follow-up plan must install per-subcommand entries.**

The implementer's first task post-promotion is the empirical test: in a fresh codex session with ONLY the bare `["clank"]` rule, attempt `clank finish` and observe whether the prompt fires. Result drives the rule shape choice (see "Granularity decision" below).

## Problem (narrowed)

Currently `clank setup` installs the Stop hook and SKILL into `~/.codex/` but doesn't touch `~/.codex/rules/default.rules`. New users running `clank setup` get the workflow assets but NOT the command-approval rule. Every fresh `clank <verb>` invocation triggers an "always allow / once / deny" prompt the first time codex sees that exact command shape.

The user-scope assets pipeline should write the rule that puts `clank` in codex's allow-list, whatever the right rule shape turns out to be after the empirical test.

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

(The two-hypothesis ambiguity around whether bare `["clank"]` matches `clank finish` is called out in the "Scope honesty" section above — both hypotheses share the same plan response: install the bare rule as a starting point, run the empirical test to decide whether per-subcommand entries are required.)

**Q2 — clank init vs clank setup wiring (verified by reading `crates/cli/src/cli/init.rs` + `setup.rs`):**

`clank init` writes only repo-scope assets. It calls `write_claude_perms` for claude's repo-scope permissions. It does NOT invoke `clank setup`. `clank setup` is the user-scope installer (touches `~/.claude/` and `~/.codex/`). Layering: setup = user-scope; init = repo-scope. Don't conflate.

Therefore: the rule write belongs in `clank setup`, not `clank init`. Users who run only `clank init` get told to run `clank setup` via the doctor check (existing pattern; see `clank doctor`'s user-scope section).

## Approach

### Step 0 — empirical test (BEFORE writing code)

In a fresh codex session (cleanly started, not the one in which the rule was added), with ONLY the bare `prefix_rule(pattern=["clank"], decision="allow")` rule in `default.rules`, attempt to run `clank finish` (or any other `clank <verb>` codex hasn't seen before). Observe whether the approval prompt fires.

Outcomes:
- **No prompt → bare rule works.** Hypothesis 1 (running-session reload gap) confirmed. Implementation uses the bare `["clank"]` rule. Continue to step 1.
- **Prompt fires → bare rule does NOT match.** Hypothesis 2 (prefix-arity mismatch) confirmed. Implementation must enumerate per-subcommand rules. Continue to step 1 with the rule-shape adjusted: instead of one bare-prefix line, write one line per subcommand in the `Command` enum at `crates/cli/src/main.rs:15-78`.

Document the test outcome in the implementation commit message.

### Steps 1-3 (the same regardless of step-0 outcome — only the rule SHAPE changes)

1. **Extend `clank setup` to ensure the codex rule(s) are present.** Read `~/.codex/rules/default.rules` (or create it + parent dirs if missing). For each line the plan needs:
   - **No matching line exists → append the line at the end of the file.** Trailing-append matches the file's existing convention; the user's file accumulates allows in order of approval.
   - **Exact-match allow line already present → no-op.** Idempotent.
   - **A `decision="deny"` line for the same pattern exists → ERROR.** Don't silently override a user's explicit denial; print a diagnostic naming the file path and the line content, and ask the user to remove it manually. Fail-closed.
   - **Malformed file (un-parseable lines, missing closing bracket, etc.) → ERROR.** Same fail-closed semantics as the rest of setup; never overwrite existing rules.

2. **Doctor check**: extend `clank doctor`'s user-scope section with a "codex command rules include `clank` allow rule" entry. Reads the file, parses for the matching line(s), Warn if missing. Names the fix command (`clank setup`). For the per-subcommand case, Warn lists which entries are missing.

3. **Allow-list granularity decision.** Determined by step 0:
   - **Bare prefix (`["clank"]`)** if hypothesis 1 — minimal entry, zero maintenance.
   - **Per-subcommand (`["clank", "init"]`, `["clank", "finish"]`, ...)** if hypothesis 2 — one entry per subcommand from the `Command` enum. Adds maintenance: every new subcommand requires a new entry. `clank setup` can derive the list from the enum so the maintenance is "do nothing; setup picks up new variants automatically."

   Either way: clank is a peer-review workflow tool; agents are expected to run any of its commands, and the gate state machine itself enforces "did this agent have the right to do that." A second layer of per-command approval-checks via codex's rules file is redundant; we want all clank commands allowed.

### Rule precedence note

The plan assumes last-match-wins or first-match-wins doesn't matter for our case because:
- We never write a `deny` rule (only `allow`).
- We error out if a user has an existing `deny` for the same pattern (step 1 above).

So precedence between two `allow` rules is irrelevant — they have the same decision. If a future plan ever needs to OVERRIDE an existing rule, precedence becomes load-bearing and gets revisited then.

## Out of scope

- The claude side: claude's hook + permissions surface is already handled by `write_claude_perms` in `init.rs`. Codex parity is the gap this plan closes.
- Restructuring codex's existing config format. The DSL line-append + grep-for-presence pattern is sufficient.
- Generalizing to per-subcommand rules. If a user wants finer-grained control, they edit the file by hand — that's the codex UX.
- Touching `clank init`. User-scope installation stays in `clank setup`.

## Acceptance

- **Fresh codex session started after `clank setup`** runs any `clank <subcommand>` without an approval prompt. (Acknowledges hypothesis-1 reload requirement; running sessions need restart to reload rules.)
- Idempotent: re-running `clank setup` doesn't duplicate any entry.
- A malformed `default.rules` file errors out cleanly (fail-closed); existing rules aren't overwritten.
- An existing `decision="deny"` rule for the same pattern triggers an error naming the offending line; the user removes it manually before re-running setup.
- `clank doctor` includes a check for the codex allow-list entry; Warn when missing with `clank setup` as the suggested fix.
- Existing claude-side permissions writes unchanged.

## Tests

- **Integration test (setup writes rule)**: temp HOME, run `clank setup`, read the resulting `~/.codex/rules/default.rules`, assert the expected rule shape (per step-0 outcome) is present.
- **Integration test (idempotent)**: pre-populate the file with the rule(s), run `clank setup`, assert exactly one matching line per entry afterwards.
- **Integration test (creates file)**: temp HOME with no `~/.codex/rules/` dir at all, run `clank setup`, assert the dir and file are created with the entry.
- **Integration test (other rules preserved)**: pre-populate file with unrelated rules, run setup, assert the original rules are still present + the clank line(s) were appended.
- **Integration test (deny-collision errors)**: pre-populate file with `prefix_rule(pattern=["clank"], decision="deny")`, run `clank setup`, assert it exits non-zero with a diagnostic naming the offending line. File is left unchanged.
- **Doctor test**: temp HOME with no rule → Warn entry naming "clank setup" as fix; with rule → no warn.

## Implementation note

Appended at the end of the file (matches the user's file's existing convention; the deny-collision check in step 1 covers the "deny precedes our allow" corner case explicitly rather than relying on file order). For the per-subcommand case under hypothesis 2, sort the new entries before appending so the file stays grep-friendly.
