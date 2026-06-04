# manage-clank-agents
# Read-only CLI ergonomics: `clank agent list` + doctor checks for unbound reviewers

## Rescope notice

This plan went through three trims:

1. **`all-reviewers-gate`** (FINISHED `01fd08f`): consensus gating using the directory scan as the reviewer source of truth.
2. **`clank-init-seeds-default-agents`** (FINISHED `65887e4`): user-scope `default_agents` + `clank init` seeding skeletons + the declared-set guard in `bootstrap_agent_identity`.

What was *originally* left in this plan — repo-scope `agents` array schema migration, four `clank agent` write subcommands, gate-projection data-source migration, doctor checks — has been re-examined and trimmed further. The schema migration is **dropped**:

- The existing `<repo>/.clank/config.json` `master` field + directory scan already model the agent set adequately. The all-reviewers-gate consensus logic reads from this.
- Moving to a canonical `agents` array doesn't unlock new behavior — only relocates the data.
- Backward-compat logic (synthesize from `master` field + legacy directory entries, two parallel paths during migration) carries real regression risk (see clank-init-seeds-default-agents's three-round guard logic with two codex-caught bugs).

The write subcommands (`add/remove/set-role`) are also **deferred** — they're sugar over filesystem operations and not blocking any current workflow.

What remains: the two small ergonomic wins.

## Problem

After `all-reviewers-gate` + `clank-init-seeds-default-agents`, the agent system works correctly but has two ergonomic gaps:

1. **No `clank agent list` command.** To see what agents this repo has registered, you `ls .clank/agents/` and `cat config.json` per entry. There's no single command that prints the structured agent list.

2. **`clank doctor` doesn't warn about unbound reviewers.** With `clank-init-seeds-default-agents`, a seeded skeleton has a `role: Reviewers` config but no `session` field — it's a *declared but unbound* reviewer. Under the all-reviewers gate, that reviewer's missing feedback blocks the gate forever (master can never reach `Approved`). The user has no clear signal that the cause is "ruthless was declared but never ran `clank as`".

Both gaps are small and additive — read-only commands over existing state. No schema migration, no new on-disk representation.

## Approach

### 1. `clank agent list` subcommand

New top-level subcommand. Enumerates the current repo's registered agents from `.clank/agents/<label>/config.json` files (the existing source of truth). Output:

**Human form** (default):
```
$ clank agent list
LABEL     ROLE       BOUND    TOOL    SESSION
claude    master     yes      claude  9c96e…
codex     reviewers  yes      codex   019e5…
ruthless  reviewers  NO       —       —
```

**JSON form** (`-j` / `--json`):
```json
[
  {"label": "claude", "role": "master", "bound": true, "tool": "claude", "session_id": "9c96e..."},
  {"label": "codex", "role": "reviewers", "bound": true, "tool": "codex", "session_id": "019e5..."},
  {"label": "ruthless", "role": "reviewers", "bound": false, "tool": null, "session_id": null}
]
```

Implementation: thin wrapper over `agent_store::load_all_agent_configs(repo)?` with a formatter. Uses strict loading (errors on malformed config — surfaces the problem rather than silently dropping). Sort by (role with master first, then label) for stable output.

### 2. `clank doctor` adds an "unbound reviewer" check

Extend the existing doctor's checks. For each agent registered in `.clank/agents/<label>/config.json`:
- If `role == Reviewers` and `session.is_none()` → emit a WARN-level check: "reviewer `<label>` is registered but has no bound session. The all-reviewers gate will wait on `<label>` indefinitely. Run `clank as <label>` from inside the agent to bind."

Doctor's existing severity levels (Pass/Warn/Fail) already exist; this is a new entry in the Warn bucket.

The master-with-no-session case is also relevant but less common — most masters are bound during `clank init`'s `bootstrap_agent_identity`. Emit the same Warn check for `role == Master` && `session.is_none()` for symmetry.

## Out of scope

- Repo-scope `agents` array schema migration. The directory scan already works; no functional gain from relocating the data.
- `clank agent add/remove/set-role` — sugar over filesystem operations, deferred until a workflow concretely needs them.
- Auto-binding seeded skeletons on first launch of their agent (a future plan; the existing `clank as` flow is the registration mechanism).
- Repo-scope `default_agents` (the field is intentionally user-scope only; see `clank-init-seeds-default-agents`'s rationale).
- Tool-detection (`clank doctor` already checks per-tool installation; this plan doesn't extend that).

## Acceptance

- `clank agent list` prints the structured agent list from `.clank/agents/` with role + bound state. JSON output validates against a stable schema.
- `clank doctor` emits a Warn entry for every registered reviewer with no bound session, naming the label and the fix (`clank as <label>`).
- Existing behavior unchanged: no config schema changes, no migration, no new on-disk state.
- `cargo test --workspace` passes; `cargo fmt --all --check` clean.

## Tests

In `clank-cli`:

- **`clank agent list`** integration tests:
  - Repo with only master bound → output shows master with bound=yes, no reviewers.
  - Repo with master + bound codex + unbound ruthless (seeded skeleton) → output shows all three, ruthless's bound=NO.
  - Empty repo (no `.clank/agents/` entries) → empty list, exits 0.
  - JSON output round-trips through serde for the documented schema.
  - Malformed agent config → errors with the parse diagnostic (consistent with the strict gate-input policy).

- **`clank doctor` unbound-reviewer check**:
  - Seeded reviewer with no session → Warn entry with the label and remediation.
  - Bound reviewer → no Warn entry for it.
  - Seeded master with no session → also Warn (symmetric).
  - Doctor exit status semantics unchanged (Warn doesn't fail the doctor; Fail still does).
