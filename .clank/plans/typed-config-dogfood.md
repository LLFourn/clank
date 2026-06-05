# typed-config-dogfood
# Migrate all hand-rolled JSON literals across the codebase (especially tests) to typed serde structs. The serde structs become the single source of truth for the schema.

## Problem

lloyd 2026-06-05: "why are you writing json manually in this test. Why not dog food? All the structure of config.json should be in a serde struct that is deserialized in and serialized out using serde_json."

Inspecting the codebase, many tests (and a few non-test sites) construct config JSON via raw string literals:

```rust
write(repo, ".clank/config.json", r#"{"review":{"adhoc_feedback":false},"hooks":{...}}"#);
```

Problems with this pattern:
1. **Schema changes don't type-check.** Renaming a field or changing a variant name breaks tests silently — the literal still parses as some JSON, just not the expected shape.
2. **Schema drift across tests.** Multiple tests construct similar JSON differently; subtle typos slip in.
3. **Hard to refactor.** The "rename Role::Reviewers to Role::Reviewer" plan would have been mechanical with typed values; with string literals, every test needs hand-editing.

The right pattern: typed structs are the single source of truth. Tests construct values programmatically (or use builder helpers), then `serde_json::to_string_pretty(&value)` produces the JSON.

## Verified before promotion (2026-06-05)

Quite a bit has shifted since the subagent first drafted this:

- **`RepoConfigFile`** at `crates/cli/src/cli/config.rs:217` ALREADY EXISTS (added in `agent-add-cli-and-repo-scope` for the `clank agent add` write path). Fields: `review: Option<ReviewSection>`, `hooks: BTreeMap<String, serde_json::Value>`, `agents: Option<Vec<DefaultAgent>>`, plus `#[serde(flatten)] extra: BTreeMap<String, Value>` for forward-compat.
- **`UserConfigFile`** at `config.rs:240` also exists with the same shape (`default_agents` instead of `agents`).
- `DiffConfig` / `DiffEditorFile` (from `clank-diff-editor`) are typed and used in `apply_layer`.
- `DefaultAgent`, `LaunchConfig`, `Session`, `AgentConfig` are typed.

**What's actually missing**:
1. **`hooks` field is still half-typed**: `BTreeMap<String, serde_json::Value>` instead of `BTreeMap<HookEvent, Option<String>>`. Round-trip preserves arbitrary JSON but the producer side can't construct a hook entry via the typed struct without going through `Value`.
2. **`apply_layer` uses a separate lossy reader**: `ConfigFile` + `ReviewFile` + `HooksFile` + `DiffFile`. The round-trip path (`RepoConfigFile`) and the lossy reader path co-exist. This plan does NOT unify them; that's a separate cleanup.
3. **21 config-shaped JSON literal sites** in `crates/cli/tests/` (`grep -rn 'r#"{[^}]*"role"\|r#"{[^}]*"agents"\|r#"{[^}]*"review"\|r#"{[^}]*"diff"\|r#"{[^}]*"hooks"' crates/cli/tests/ | wc -l`). Files affected: demote_integration, wfw_integration, stop_hook_integration, purge_drop_integration, agent_start_integration, open_integration, init_integration, auto_integration.

**Recent evidence**: `role-reviewers-to-reviewer-rename` (FINISHED `a238e56`) had to grep+sed 8 test files to migrate string assertions. With typed structs, the rename would have been a one-line variant change. This plan's premise is empirically demonstrated.

## Approach

### Phase 1: Type `hooks` properly + add a `HooksSection` to the umbrella

Replace `RepoConfigFile.hooks: BTreeMap<String, Value>` and `UserConfigFile.hooks: BTreeMap<String, Value>` with a typed `HooksSection`:

```rust
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct HooksSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_work: Option<Option<String>>,  // Some(Some("cmd")) = set; Some(None) = explicitly null; None = absent
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_work: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_finalized: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<Option<String>>,
}
```

Each entry is `Option<Option<String>>` so presence-aware semantics work (matches the existing `HooksFile.get(event) -> Option<Option<String>>` shape). Forward-compat: any unknown hook names land in `RepoConfigFile.extra` via the existing `#[serde(flatten)]` catchall.

### Phase 2: Migrate the 21 JSON-literal sites in tests

Per the verified count: 21 sites across 8 files. For each, swap the literal for one of these patterns:

- **Single-section write** (e.g. just setting `agents`): `RepoConfigFile { agents: Some(vec![DefaultAgent{...}]), ..Default::default() }` + `serde_json::to_string_pretty`. Already done in `agent_add_remove_integration.rs` — copy that pattern.
- **Multi-section composition** (e.g. tests that compose agents + review + hooks): drop the existing Value-based `merge_repo_config` helper from `wfw_integration.rs:160-180`; instead construct a single typed `RepoConfigFile` with the relevant `Some(...)` fields.
- **Skeleton writes** (`.clank/agents/<label>/config.json`): use `clank_core::agent_config::AgentConfig` + `serde_json::to_string` (or the existing `save_agent_config` helper which already does this).

### Phase 3: Drop the `merge_repo_config` Value-based helper

`wfw_integration.rs:160-180` defines `merge_repo_config(repo, |v: &mut Value| {...})` — added as a workaround when the typed structs didn't cover all fields. With Phase 1 + 2, this helper has no callers; remove it.

### Phase 4: Lock in via negative regression test

Add a unit test in `crates/cli/tests/` (or as a build script) that greps for the JSON-literal pattern and fails if any return:

```rust
#[test]
fn no_json_literal_config_writes_in_tests() {
    let pattern = regex::Regex::new(r#"r#"\{[^}]*"(role|agents|review|diff|hooks)"#).unwrap();
    for entry in walkdir::WalkDir::new("crates/cli/tests") {
        let entry = entry.unwrap();
        if entry.path().extension().map_or(false, |e| e == "rs") {
            let body = std::fs::read_to_string(entry.path()).unwrap();
            assert!(
                !pattern.is_match(&body),
                "JSON literal config write found in `{}`; use the typed RepoConfigFile/UserConfigFile struct instead.",
                entry.path().display()
            );
        }
    }
}
```

(Alternative: a CI grep step. Decide at implementation.)

### Out of scope for THIS plan

- **Unifying `apply_layer`'s lossy reader with `RepoConfigFile`**. Two distinct paths today; this plan migrates WRITES, not the lossy READ layer. Unification is a separate cleanup (or maybe never — the lossy semantic is intentional for review/hooks).
- **Skill markdown JSON examples**. Verified: none today (`grep "json" crates/cli/src/cli/setup_assets/*.md` returns no JSON blocks). Skip.
- **Feedback file format** (`.clank/agents/<label>/feedback/<sha>.md`). Not JSON; the verdict header is line-prefixed text. Out of scope.

## Acceptance

- `HooksSection` exists as a typed struct; `RepoConfigFile.hooks` and `UserConfigFile.hooks` use it instead of `BTreeMap<String, Value>`.
- Zero raw JSON literal config writes in `crates/cli/tests/` (currently 21 sites). Verified by either an in-tree test that greps the test files OR a CI step. Pin at implementation.
- All test config writes use `RepoConfigFile` / `UserConfigFile` / `AgentConfig` constructed in Rust, serialized via `serde_json::to_string_pretty`.
- `wfw_integration.rs::merge_repo_config` Value-based helper is dropped (no remaining callers).
- A schema change (rename a variant, add a field) shows up as compile errors in tests, not silent runtime drift.
- `cargo test --workspace` passes.

## Related

- `agent-add-cli-and-repo-scope` (FINISHED `1cf029b`): added `RepoConfigFile` + `UserConfigFile` as typed round-trip wrappers. This plan extends them with a typed `hooks` field and migrates the test surface.
- `role-reviewers-to-reviewer-rename` (FINISHED `a238e56`): grep+sed migration of 8 test files demonstrates the cost of JSON literals empirically. With this plan landed, that rename would have been a one-line variant change.

## Implementation note

This is a high-volume but low-architectural-risk plan. The patterns are already established by `agent_add_remove_integration.rs` and `doctor_unbound_reviewer_integration.rs` (both use typed `RepoConfigFile` + serde round-trip end-to-end). The work is mostly mechanical: read each test file, swap the literal for a typed construction, verify the test still passes.
