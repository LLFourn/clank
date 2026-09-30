# the-zellij-seam-is-a-crate-not-a-grep
# clank — Research: should the zellij seam be a crate? No, and here is the measurement

> **Finding: do not build `clank_zellij`.** The plan's title asserts the opposite and is
> kept only because it is the plan key. Every benefit the proposal opened with has been
> tested; the one that survives does not need the crate. The deliverable of this plan is
> this record, not a refactor.

## The question, and why it was asked

Two enforcement gates guard clank's zellij access, and the ownership gate's own docstring
reads as a confession that they are a workaround for a missing boundary:

> A runtime test cannot enforce this. […] The real risk is code reaching AROUND the
> seam: spawning zellij directly, or calling the pane helpers, **both of which are
> reachable crate-wide. That is a source-level property, so this is a source-level
> gate.**
> — `crates/cli/tests/it/zellij_ownership_boundary.rs`

The proposal: move zellij into its own crate so the compiler enforces what the scanners
approximate. The immediate trigger was `Panes::say` (`crates/cli/src/cli/web/mod.rs:114-139`),
the complete answer to "how do you talk to an agent's terminal" — label → tool →
`codex_chat` fast path → `say_to_codex_pane` for Codex's kitty-protocol Enter →
`say_to_pane` otherwise — sitting as a private method on a web-server type, about to be
copied by a second caller.

## What was measured

**The export surface.** `open_zellij.rs` declares **54 `pub(crate)` items** in its
production half, and **37 distinct names are reached from outside it**:

    run · require_zellij · in_session · current_session · caller_identity
    repo_session_name · repo_tab_id · current_tab_info · panes_in_all_sessions
    snapshot_panes · snapshot_panes_in · ZellijPane · PaneMeta · Orientation
    PlacementCapability · placement_capability · tab_orientation
    agent_pane_table · agent_pane_pairs · agent_pane_title · agent_panes_by_command
    agent_start_command · live_agent_labels · reviewers_are_stacked
    compose_live_layout · validate_template · subscribe_panes
    open_agent_pane · remove_reviewer_panes · rename_tab · rename_pane
    override_tab_layout · focus_pane · pass_focus_target
    say_to_pane · say_to_codex_pane · stop_pane

A crate would **export 37 of 54 and hide 17** — pure helpers the tests use (`say_argvs`,
`subscribe_argv`, `agent_start_argv`, `is_status_pane`, `user_layout_template`) and a few
internals. That is the whole privacy win.

**What the ownership gate actually guards.** Not a 4.6k-line module — five pane
PLACEMENT operations (`zellij_ownership_boundary.rs:47-53`):

    focus_pane · add_reviewer_pane · stack_reviewer_panes
    remove_reviewer_panes · relocate_for_promote

classified by ownership semantics, not by whether a call mutates: *"Pane MUTATIONS. Reads
(`snapshot_panes`, `agent_pane_pairs`, `reviewers_are_stacked`, …) create no second
owner."* `say_to_pane`, `stop_pane`, `subscribe_panes` and `run` are deliberately absent
because the web bridge calls them legitimately (`web/mod.rs:112-146`), and `say_argvs`'
own doc states the rule: *"Not a pane MUTATION in the ownership gate's sense: typing into
a pane leaves its place in the layout to the reconciler, which is what that gate
protects."* The gate is already minimal and correctly scoped.

**Cross-process ownership is already enforced, and not by a gate.**
`crates/cli/src/cli/status_tui/lease.rs` holds an flock for the life of the process
(`status_tui/mod.rs:2593`): *"One status TUI per repo… A second one does not run."*

## Five mechanisms that cannot carry the ownership invariant

Listed so a sixth is not attempted. Each was proposed in this plan and refuted in review:

- **a crate boundary** — an exported mutation API is callable by every CLI module
  (codex on b26de65);
- **a `PaneOwner` capability** — its handout function is public, and Rust cannot grant a
  function to one module of a dependent crate (8107fdb);
- **a sealed `PaneIo`** — unimplementable by a reconciler outside the crate (8107fdb);
- **a declarative entry** — `match this roster` is itself a pane mutation, callable with
  a fabricated roster (3fff5ef);
- **`&StatusLease` in the signature** — flock refuses a second ACQUISITION, not a second
  CALL through a reference already held (ad909ae).

The common error is wishing for module-level privilege across a crate boundary, which
Rust does not have. Ownership has a cross-process half (the lease, real) and an
in-process half (the gate, the only demonstrated mechanism).

## The finding

| claim the proposal opened with | verdict |
|---|---|
| the compiler replaces the gates | false — Rust cannot stop a raw spawn or an exported call |
| a capability / sealed trait / declarative API enforces one owner | false — four variants refuted |
| a lease enforces one owner in-process | false — flock refuses acquisition, not calls |
| the ownership gate can be narrowed | nothing to narrow; the proposal would have WIDENED it and banned working callers |
| helper privacy | real, but 17 of 54 |
| one tool-aware `say` | real, and independent of the crate |

**Do not build the crate.** Two things are worth doing and neither needs it:

1. **Extract the tool-aware `say`** out of `web/mod.rs` into one shared home in the CLI,
   composed from `codex_chat` (an app-server channel, not zellij — it does not move) plus
   the two zellij primitives. **This work belongs to `a-nudge-types-at-an-agents-terminal`,
   which already requires it, and is NOT implemented by this plan.**
2. **Leave both gates exactly as they are.** They are correct and minimal, and carry
   invariants no language mechanism available here can.

## Acceptance

This is a research plan whose deliverable is a decision record. It is complete when:

- The measurements are in this file and reproducible from it: 54 declared, 37 exported,
  17 hidden; the ownership gate's five operations and the semantic rule it classifies by.
- The five refuted mechanisms are recorded with the reason each fails.
- The recommendation is stated, with the two actions that follow and where each lives.
- No code changes. **No crate is created, no gate is edited, no `say` is moved by this
  plan.**

## If the human overrides

A crate may still be wanted for reasons this analysis cannot price — a tidier tree, an
obvious home for the next zellij-adjacent feature. That is a legitimate call, and it
needs a NEW plan rather than a revival of this one, because the work it would do is
unrelated to the work this one proposed. Whoever writes it should carry forward:

- the gates both survive unchanged — in particular the ownership gate's five operations
  must not be widened to cover reads, `say`, `stop` or `run`;
- `codex_chat` stays in the CLI, so the tool-aware `say` is composed there;
- the cost gate is scoped to wherever the `list-clients` / `dump-layout` calls live, and
  keeps not scanning comments;
- the reconciler's test coupling is the real cost of moving it: its tests reach
  `status_tui::fixtures::{plan_state, reviewer_missing, snap, cand, detail_repo,
  library_home, two_agent_snap, agent_row}`, `apply_swap`, `input::detail_actions` and
  `render::auto_mark`. Counting production `use` lines understates this badly.

## Out of scope

`clank nudge` and the `say` extraction (`a-nudge-types-at-an-agents-terminal`). Any
change to how panes are placed, reconciled or named. The `web` crate's structure.
