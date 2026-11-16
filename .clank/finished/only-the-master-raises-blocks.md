# only-the-master-raises-blocks

Invariants: I7.

## Why

The reviewer wake text (`wake_render.rs:396`) says "Needs human input? `clank block ask`". That contradicts the reviewer skill and the rule that only the master raises blocks. Nothing in `clank block ask` enforces the rule.

## Build

1. **Wake text.** Remove the block hint from non-master wake text (`wake_render.rs`).
   - Reviewers are told to write REQUEST_CHANGES when they need human input, so the master can raise the repo block. That is the reviewer skill's rule, and it keeps the review gate's verdict. A `clank send --master` may supplement it but never replaces the verdict.
   - Non-reviewer agents, such as standby, are pointed at `clank send --master`.
2. **Admission.** `clank block ask` decides who is asking before it touches any block file or blocked hook. Identity has three outcomes, using the existing session and binding precedence:
   - **Absent:** no session variable and no `CLANK_AGENT`, so the caller is genuinely a person at a shell. Allowed.
   - **Resolved to an agent:** allowed only if that agent is the repo's master. Otherwise it is refused, `--permission` included, and the refusal says what to do instead: REQUEST_CHANGES for a reviewer, `clank send --master` for anyone else.
   - **Error:** an invalid or ambiguous session variable, an unbound harness session, or an unreadable config. Refused with the error, and nothing is created.

   `run_create` today uses `resolve_identity_from_env(&repo).ok()`, which folds every error into "a person". That is replaced by the three-way result, and no resolver error is ever read as a person.
3. **`clank block answer <name>`** stays repo-scoped, with no agent argument. Check that no surface still asks for one.
4. I7 in `docs/invariants.md` becomes `holds`, naming the tests below.

## Acceptance

Library-core tests, no binary spawns:

- A reviewer's `block ask` is refused, plain and `--permission`, and creates no block. The refusal names REQUEST_CHANGES.
- A standby agent's is refused and names `clank send --master`.
- The master's succeeds.
- A caller with no identity at all succeeds.
- A caller whose identity fails to resolve (bad session variable, unbound session) is refused and creates no block.
- Rendered reviewer and standby wakes contain no `block ask`. Reviewer wakes name REQUEST_CHANGES.
