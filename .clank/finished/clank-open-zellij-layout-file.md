# clank-open-zellij-layout-file
# Rework `clank open zellij` to write a layout file under `<repo>/.clank/zellij/` and spawn `zellij --layout <file>`, NOT `zellij action new-tab --layout-string`.

## Problem

lloyd 2026-06-05: "in the clank dir in the repo. There's a .gitignore thing you can add it to. Probably just make a dir for zellij there."

`clank-open-zellij` (FINISHED `5c2623e`) shipped with the POC's spawn mechanism: `zellij action new-tab --cwd <repo> --name <basename> --layout-string <KDL>`. That IPCs into an existing zellij session.

Lloyd wants the layout-file model instead: clank writes KDL to a file under `<repo>/.clank/zellij/`, then invokes `zellij --layout <file>` (which starts a new session with that layout). The directory gets added to `<repo>/.clank/.gitignore` so the generated file isn't tracked.

Tradeoffs:
- The action-based approach adds a tab to a RUNNING session. Requires you to already be inside zellij.
- The file-based approach starts a NEW session. Works from outside zellij (which is the more typical bootstrap).

## Verified before promotion (2026-06-05)

- **Current spawn at `crates/cli/src/cli/open_zellij.rs:40-50`**: shells out to `zellij action new-tab --cwd <repo> --name <basename> --layout-string <KDL>`.
- **Current spawn argv composer at `:120-135`**: `compose_spawn_argv` returns the action-based argv as `Vec<String>`. Used by `--print` mode's stderr `spawn:` line.
- **ZELLIJ_SESSION_NAME guard at `:31-38`**: errors when not inside zellij. Becomes irrelevant under the file-based approach.
- **`<repo>/.clank/.gitignore`** exists (canonical clank scaffolding). Currently contains `/agents/`, `/cache/` etc. Adding `/zellij/` is a one-line append.
- **KDL tab-name**: under `--layout-string` we passed `--name <basename>` as a CLI flag. Under `--layout <file>` the tab name must live inside the KDL via a `tab name="..."` wrapper block. Layout shape grows by one level of nesting.
- **3 affected tests** in `crates/cli/tests/open_zellij_integration.rs`:
  - `open_zellij_no_zellij_session_errors_without_print` — becomes irrelevant (no session needed); REMOVE.
  - `open_zellij_no_zellij_session_succeeds_with_print` — rename / repurpose.
  - `open_zellij_tab_name_in_print_spawn_metadata` — assertion shifts from `--name <basename>` argv flag to `tab name="<basename>"` in the KDL.

## Approach

### Phase 1: KDL shape — wrap panes in a tab block + pin --repo in pane commands

The current KDL is a flat `layout { pane... pane... pane... }`. Wrap the existing content in a `tab name="<basename>"` block AND pin the repo on every `clank agent start` invocation:

```
layout {
    tab name="<basename>" {
        pane size=1 borderless=true {
            plugin location="zellij:tab-bar"
        }
        pane split_direction="horizontal" {
            pane name="<master> (master)" {
                command "clank"
                args "agent" "start" "<master>" "--repo" "<absolute-repo-path>"
            }
            ... reviewer panes (same shape) ...
        }
        pane size=2 borderless=true {
            plugin location="zellij:status-bar"
        }
    }
}
```

**Why `--repo` on every pane** (codex caught on 361b104): `zellij --layout <file>` spawns a NEW session whose cwd inherits from the calling shell. If the user runs `clank open zellij --repo /path/to/repo` from a DIFFERENT directory, the spawned session lands in that other directory and the pane commands `clank agent start <label>` (no `--repo`) would resolve the wrong repo (or fail to find one). Pinning `--repo <abs-path>` makes the resolved repo unambiguous regardless of zellij's cwd.

The `tab name` value AND the absolute repo path BOTH go through `kdl_escape`. `compose_kdl`'s signature grows two parameters: `tab_name: &str` and `repo_path: &str`.

### Phase 2: Write layout file under `<repo>/.clank/zellij/`

- New function `write_layout_file(repo: &Path, kdl: &str) -> anyhow::Result<PathBuf>`:
  - Ensures `<repo>/.clank/zellij/` exists (`std::fs::create_dir_all`).
  - Writes KDL to `<repo>/.clank/zellij/layout.kdl` atomically (write to `.tmp` then rename).
  - Ensures `<repo>/.clank/.gitignore` contains a `/zellij/` line — append if absent, no-op if present. Existing `clank init` scaffolding model: idempotent append.
  - Returns the written path.

### Phase 3: Spawn mechanism

- Drop the `ZELLIJ_SESSION_NAME` guard (irrelevant — `zellij --layout` works from outside).
- Drop the `--cwd <repo>` flag from the spawn argv. The repo context lives in the KDL via the per-pane `--repo` argument (Phase 1), so the spawned zellij session's cwd doesn't need to match.
- New spawn: `zellij --layout <path>`. That's it.
- Update `compose_spawn_argv` to return `["zellij", "--layout", <path>]`.

### Phase 4: --print semantics preserved

`--print` still emits KDL on stdout + `spawn: zellij --layout <path>` on stderr. The path is what the file WOULD be written to. The `--print` path does NOT write the file (the user asked for inspection only).

### Phase 5: Tests

Migrate the 3 affected tests:
1. REMOVE `open_zellij_no_zellij_session_errors_without_print` — the guard is gone.
2. RENAME `open_zellij_no_zellij_session_succeeds_with_print` → `open_zellij_print_mode_emits_kdl_without_writing_file`. Assert KDL on stdout, no file at `<repo>/.clank/zellij/layout.kdl` afterwards.
3. UPDATE `open_zellij_tab_name_in_print_spawn_metadata`: shift assertion from `--name <basename>` argv flag to `tab name="<basename>"` substring in the KDL body. Plus `spawn:` line contains `--layout` + the would-be-written path.

Add 3 new tests:
4. `open_zellij_writes_layout_file_under_clank_dir`: non-print path; mock the spawn somehow (or just verify the file exists + has expected content even if the spawn fails because no real zellij). Acceptance is "file written," not "zellij spawned" — the latter needs zellij installed.
5. `open_zellij_adds_zellij_dir_to_gitignore`: assert `<repo>/.clank/.gitignore` contains `/zellij/` after invocation; assert idempotency on a second invocation.
6. `open_zellij_pane_commands_pin_repo_via_absolute_path` (codex 361b104 catch): invoke `clank open zellij --print --repo <abs-path>` from a DIFFERENT cwd; assert every pane's `args` line contains `"--repo" "<abs-path>"`. Locks in that the spawned session's cwd doesn't matter — the repo is pinned per-pane.

### Out of scope

- Allowing multiple per-repo layouts (e.g. `layout.<name>.kdl` for different reviewer subsets). v1 has a single layout per repo.
- Auto-rerun on declaration changes. The user re-runs `clank open zellij` after `clank agent add/remove`.

## Acceptance

- `<repo>/.clank/zellij/layout.kdl` exists after `clank open zellij` runs (in non-print mode).
- `<repo>/.clank/.gitignore` contains a `/zellij/` line (idempotent on re-run).
- Spawn shells out to `zellij --layout <path>` only — NO `zellij action`.
- `--print` mode emits KDL on stdout + `spawn: zellij --layout <path>` on stderr; does NOT write the file.
- KDL contains `tab name="<basename>"` wrapping the panes.
- Every pane's `args` line contains `"--repo" "<absolute-repo-path>"` so the resolved repo is unambiguous regardless of the spawned zellij session's cwd. Verified by `open_zellij_pane_commands_pin_repo_via_absolute_path`.
- All escape coverage from `30a0194` (label quotes, control chars) extends to the new `tab name` AND `--repo <path>` interpolations.
- `cargo test --workspace` passes.

## Related history

- `clank-open-zellij` (FINISHED `5c2623e`): shipped the action-based spawn this plan reworks.
- POC at `open-worktree.sh`: still uses `zellij action new-tab` since it operates from inside an existing session. Out of scope for this plan; the POC and `clank open zellij` will diverge intentionally.
