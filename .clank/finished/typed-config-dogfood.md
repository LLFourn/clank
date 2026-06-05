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

Replace `RepoConfigFile.hooks: BTreeMap<String, Value>` and `UserConfigFile.hooks: BTreeMap<String, Value>` with a typed `HooksSection`. Each entry is `Option<Option<String>>` so presence-aware semantics work (matches the existing `HooksFile.get(event) -> Option<Option<String>>` shape): `Some(Some("cmd"))` = explicitly set; `Some(None)` = explicitly null (disable); `None` = absent (use default).

```rust
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct HooksSection {
    /// `Some(Some("cmd"))` = explicitly set;
    /// `Some(None)` = explicitly null (disable);
    /// `None` = absent (use default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_work: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_work: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_finalized: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<Option<String>>,
    /// Forward-compat catchall for hook event names this build
    /// doesn't know about yet (e.g. newer clank wrote it).
    /// Preserved on round-trip. Ruthless caught the gap on
    /// b6323be: `RepoConfigFile.extra` catches unknown SECTIONS
    /// at the top level, NOT unknown FIELDS inside the hooks
    /// section. Without this `extra`, round-trip would silently
    /// drop unknown hooks (or `deny_unknown_fields` would error
    /// loudly — both wrong).
    #[serde(flatten)]
    pub extra: BTreeMap<String, Option<String>>,
}
```

**Round-trip vs lossy-reader equivalence**: `RepoConfigFile` (round-trip; used by writers) and the private `HooksFile` (lossy reader; used by `apply_layer`) co-exist. Writes through the typed path MUST yield the same `Config.hooks` as the lossy reader would. Ruthless review of b6323be pointed out the silent-drift risk. New unit test:

```rust
#[test]
fn hooks_section_round_trips_through_apply_layer() {
    let written = RepoConfigFile {
        hooks: Some(HooksSection {
            master_work: Some(Some("echo hello".into())),
            idle: Some(None), // explicitly disabled
            ..Default::default()
        }),
        ..Default::default()
    };
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(".clank/config.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_string_pretty(&written).unwrap()).unwrap();
    let cfg = load_with_home(tmp.path(), None);
    assert_eq!(cfg.hooks.get(&HookEvent::MasterWork), Some(&Some("echo hello".to_string())));
    assert_eq!(cfg.hooks.get(&HookEvent::Idle), Some(&None));
    assert!(!cfg.hooks.contains_key(&HookEvent::ReviewerWork));
}
```

Also add a "forward-compat unknown hook" test: write a config with `{"hooks": {"some_future_event": "cmd"}}` via the JSON serializer, round-trip through `RepoConfigFile`, assert `extra.get("some_future_event")` returns the value.

### Phase 2: Migrate the 21 JSON-literal sites in tests

Per the verified count: 21 sites across 8 files. For each, swap the literal for one of these patterns:

- **Single-section write** (e.g. just setting `agents`): `RepoConfigFile { agents: Some(vec![DefaultAgent{...}]), ..Default::default() }` + `serde_json::to_string_pretty`. Already done in `agent_add_remove_integration.rs` — copy that pattern.
- **Multi-section composition** (e.g. tests that compose agents + review + hooks): drop the existing Value-based `merge_repo_config` helper from `wfw_integration.rs:160-180`; instead construct a single typed `RepoConfigFile` with the relevant `Some(...)` fields.
- **Skeleton writes** (`.clank/agents/<label>/config.json`): use `clank_core::agent_config::AgentConfig` + `serde_json::to_string` (or the existing `save_agent_config` helper which already does this).

### Phase 3: Drop the `merge_repo_config` Value-based helper

`wfw_integration.rs:160-180` defines `merge_repo_config(repo, |v: &mut Value| {...})` — added as a workaround when the typed structs didn't cover all fields. With Phase 1 + 2, this helper has no callers; remove it.

### Phase 4: Lock in via negative regression test (best-effort, with opt-out)

Ruthless review of b6323be flagged real fragility with a naive regex approach: multi-line literals get missed; tests that assert on JSON OUTPUT (not write JSON config) produce false positives. Pinned approach (best-effort):

```rust
#[test]
fn no_json_literal_config_writes_in_tests() {
    // Match raw-string literals that look like config JSON
    // (contain one of the known top-level field names AND end
    // with .clank/config.json or similar writer hints).
    // `(?s)` flag enables multi-line `.`.
    let pattern = regex::RegexBuilder::new(
        r#"(?s)r#"\{[^"]*"(role|agents|review|diff|hooks|default_agents)""#,
    )
    .build()
    .unwrap();
    let allow_marker = "// allow-json-literal: ";
    for entry in walkdir::WalkDir::new("crates/cli/tests") {
        let entry = entry.unwrap();
        if entry.path().extension().is_some_and(|e| e == "rs") {
            let body = std::fs::read_to_string(entry.path()).unwrap();
            // Strip lines with the explicit allow-marker before
            // matching, so a deliberate test (e.g. asserting on
            // JSON output shape) can opt out with a comment
            // citing the reason.
            let filtered: String = body
                .lines()
                .filter(|l| !l.contains(allow_marker))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !pattern.is_match(&filtered),
                "JSON literal config-shaped write found in `{}`. Either use the typed \
                 RepoConfigFile/UserConfigFile struct, or add `// allow-json-literal: <reason>` \
                 on the same line if this is a legitimate output-assertion.",
                entry.path().display()
            );
        }
    }
}
```

The `// allow-json-literal: <reason>` opt-out handles false positives (tests that check JSON output shape rather than write config). The pattern itself is best-effort, NOT a formal grammar — relies on the field-name heuristic. AST-based alternatives (parse each test file as Rust syntax, walk literal nodes) are more precise but ~10x the code; defer unless the heuristic produces too much friction.

Alternative if the in-tree test approach proves too fragile: ship as a CI grep step (or `xtask check-typed-config`) with the same allow-marker semantics. Decision at implementation.

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
