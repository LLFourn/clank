# role-reviewers-to-reviewer-rename
# Rename `Role::Reviewers` → `Role::Reviewer` everywhere. Accept "reviewers" as a deserialize alias for backwards compat.

## Problem

The role enum variant is `Reviewers` (plural). It serializes as `"reviewers"`. But a single agent declaration entry is ONE agent — the plural form reads awkwardly:

```json
{"label": "alice", "role": "reviewers"}
```

→ "alice's role is reviewers" reads wrong. Singular ("reviewer") matches the noun phrasing — one agent, one role.

lloyd 2026-06-05: "rename role: reviewers to role: reviewer. Don't worry about breaking stuff too much but allow 'reviewers' as an alias for reviewer but make sure everything else only talks about the reviewer role."

## Verified before promotion (2026-06-05)

- **`Role` enum** at `crates/core/src/vocab.rs:215-221`. Currently:
  ```rust
  #[derive(..., Default, Serialize, Deserialize)]
  #[serde(rename_all = "snake_case")]
  pub enum Role {
      Master,
      #[default]
      Reviewers,
  }
  ```
  `as_str()` at `:223-230` returns `"reviewers"` for the Reviewers arm.
- **`RoleArg` enum** at `crates/cli/src/cli/mod.rs:503` is the clap `ValueEnum` for `--role <V>`. Has its own variant; needs the same rename (this plan covers it in Phase 1 — was buried in "Out of scope" pre-promotion).
- **42 callsites** for `Role::Reviewers` + `RoleArg::Reviewers` across the workspace (`grep -rn "Role::Reviewers\b\|RoleArg::Reviewers\b" crates/`). Most are mechanical search-and-replace.
- **Modern serde** supports `#[serde(alias = "reviewers")]` on enum variants alongside `#[serde(rename_all = ...)]`. No custom `Deserialize` impl needed.
- **Modern clap** supports `#[clap(alias = "reviewers")]` on `ValueEnum` variants. `--role reviewers` keeps working with one annotation.
- **`agent-add-cli-and-repo-scope`** FINISHED `1cf029b`; it writes `Role::Reviewers` into the declaration today. This rename changes the JSON shape that plan emits but doesn't change its acceptance criteria.

## Approach

### Phase 1: Rename variants + serde/clap aliases

- `clank_core::vocab::Role::Reviewers` → `Role::Reviewer`. Add `#[serde(alias = "reviewers")]` on the variant.
- Move `#[default]` from `Reviewers` to `Reviewer`.
- Update `Role::as_str()` to return `"reviewer"`.
- `cli::mod::RoleArg::Reviewers` → `RoleArg::Reviewer`. Add `#[clap(alias = "reviewers")]` on the variant.
- Update `From<RoleArg> for Role` mapping.
- Mechanically rename every `Role::Reviewers` / `RoleArg::Reviewers` callsite in the workspace (~42 sites).

### Phase 2: Verify on-disk compat via the aliases (no custom Deserialize impl needed)

- `#[serde(alias = "reviewers")]` makes both `"reviewer"` and `"reviewers"` deserialize to `Role::Reviewer`. Only `"reviewer"` serializes back out (Serde uses the variant name unless overridden by `rename`).
- `#[clap(alias = "reviewers")]` makes both `--role reviewer` and `--role reviewers` parse to `RoleArg::Reviewer`.

### Phase 3: Update all human-facing strings

- Error messages, docstrings, CLI help text, doctor diagnostics, skill markdown files (claude_skill.md, codex_skill.md), README mentions.
- Grep workspace for the literal `"reviewers"` (in strings, comments, docs) and replace where the singular reads naturally — e.g., the `doctor` diagnostic that says "registered {role_str} is unbound" works for either form.
- Preserve plural forms where they refer to the SET (e.g., "registered reviewers", `expected_reviewers` field name, `all-reviewers-gate` finished plan name — these stay plural).

### Phase 4: Tests

- `role_reviewers_string_deserializes_as_alias`: `serde_json::from_str::<Role>(r#""reviewers""#)` → `Role::Reviewer`.
- `role_reviewer_round_trips_as_singular`: `serde_json::to_string(&Role::Reviewer)` → `"\"reviewer\""`.
- `role_arg_clap_accepts_both_singular_and_plural`: parse `RoleArg` from both `"reviewer"` and `"reviewers"` via the clap ValueEnum trait; both yield `RoleArg::Reviewer`.
- Migrate any existing test that asserts on the literal `"reviewers"` in serialized output. Spot-check expected affected files: `crates/cli/tests/agent_list_integration.rs` asserts `arr[1]["role"], "reviewers"` (will need `"reviewer"`); `agent_start_integration.rs` has 2 occurrences. Run `cargo test --workspace` after the rename to surface the rest.

## Out of scope

- Renaming on-disk config files. Existing repos with `"reviewers"` in JSON continue working via the alias.
- Renaming Rust APIs that take a `&str` role name (e.g., CLI flag `--role reviewers` should keep accepting both forms — same alias logic).
- Plural forms in plural CONTEXTS ("all reviewers" / "registered reviewers" — keep plural where it refers to the SET, only singular where referring to a single role assignment).

## Acceptance

- Workspace builds with `Role::Reviewer` everywhere; `Role::Reviewers` no longer exists in code.
- `{"role": "reviewer"}` and `{"role": "reviewers"}` both deserialize to the same variant.
- Serialized output (clank agent list --json, doctor --json, etc.) emits `"reviewer"`.
- CLI flag `--role reviewers` and `--role reviewer` both work.
- All docstrings, error messages, and user-facing strings use singular "reviewer" when referring to a single role assignment.
- `cargo test --workspace` passes.

## Related

- `agent-add-cli-and-repo-scope` (FINISHED `1cf029b`): writes `Role::Reviewers` via the declaration. This rename changes the JSON shape that declaration emits but the field's role in gate computation, list ergonomics, etc. is unchanged.
