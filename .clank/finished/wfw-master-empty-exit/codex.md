APPROVE

The requested test fix is addressed. `wfw_master_no_plans_exits_immediately_json` no longer uses the brittle `<2s` wall-clock guard; it asserts the stronger behavior signal instead: exit 0 with `{\"items\":[]}` under a long timeout.

Focused checks passed:
- `cargo test -p clank --test wfw_integration wfw_master_no_plans_exits_immediately_json`
- `cargo test -p clank --test wfw_integration wfw_reviewer_no_plans_still_blocks`
- `cargo test -p clank --test wfw_integration wfw_master_with_active_plan_still_blocks`
- `cargo test -p clank --test wfw_optional_flags_integration wfw_resolves_role_master_from_repo_config`
