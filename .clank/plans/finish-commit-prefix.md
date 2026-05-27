# finish-commit-prefix

## Problem

`clank finish` and `clank unfinish` produce commits without
the `[plan-name]` prefix convention. This means the fold
doesn't attribute them to the plan, and they show up in
git log without the plan context.

## Fix

- `clank finish <plan>` commit message: `[<plan>] finish`
- `clank unfinish <plan>` commit message: `[<plan>] unfinish`
- Update dry-run preview message too
- Update tests that assert on commit messages
