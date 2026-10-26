# a-pane-is-named-by-its-agent
# An agent's pane is named by its label, and that name is its identity

## Problem

Clank finds an agent's pane by its exact command text. `open_zellij::agent_pane_label` (open_zellij.rs:1780) and `pane_target::locate` (pane_target.rs:30) both match `clank agent start <label> --repo <path>`. The pane's name, `"<label> (<role>)"` from `agent_pane_title`, is cosmetic.

That breaks in three ways:
- **A pane started any other way is invisible.** In safepay story-short a master opened one by hand with `zellij run --name painter -- clank agent start painter`. Without `--repo`, the command didn't match, so the TUI opened a second painter pane.
- **A session that clank didn't launch can never be found.** That includes one bound by `clank as X`, such as this repo's own master. So nothing can locate, nudge or check in on it.
- **The role suffix repeats what the TUI already shows** and goes stale when the role changes.

The user's direction: name panes by the agent and nothing else, and treat that name as the identity. Renaming a pane by hand to fool clank is the user's own doing.

## Change

### The workspace comes first: a repo is the tab holding its TUI pane

A bare label only means something inside one repo's workspace. Two repos sharing a session routinely both have `claude` and `codex` panes. `open_zellij::session_name` is a convention, not a fact: `open_one` adds a repo's tab to whatever session the caller is in (open_zellij.rs:1816). So the scope is a verified **(session, tab)**, established by something other than the name being matched.

- **The anchor is the repo's TUI pane**, matched by `is_status_pane`. That pane's command, `clank tui --repo <path>` (or the older `clank status --repo <path> --tui`), is written by clank and names the repo. The repo's workspace is the (session, tab) that holds exactly one such pane.
- **`repo_tab_id` loses its fallback to agent panes,** so the workspace never depends on the agent matcher it scopes.
- **Discovery across sessions** (`pane_target::locate`'s scan, and the session-holder checks) finds the anchor in every answering session:
  - exactly one anchor gives the workspace, but only when the listing succeeded and every session answered;
  - otherwise the workspace is **unresolved**: zero anchors, more than one, a failed listing, or any session that didn't answer, however many anchors the others showed. A silent session could hold this repo's other tab.
- A caller that already has an independently verified local (session, tab), such as the TUI's own tab, uses that local scope. No all-session scan is added to TUI refreshes, and there is no polling.
- **An unresolved workspace authorizes nothing:** no rename, close, start refusal, reconcile mutation or typed-input target. Each caller says why in its own terms, as `locate` does today.
- **What is given up:** a repo with agent panes but no TUI pane open has no resolvable workspace, so it can't be nudged or reconciled until a TUI is open. The reconciler already lives in the TUI.
- **Everyone uses the same scope:** open and idempotence, reconcile, removal and relayout, `pane_target`, session-holder checks and the remote pane table. A same-named pane in another repo's tab is neither a duplicate nor a target.

### The name is the label, and every title writer agrees

- **`agent_pane_title` becomes the label alone,** with no role.
- **The status emoji stays,** because the user relies on it. `StatusGlyphs::of` (status_tui/zellij.rs:217) writes `"<glyph> <label>"`.
- **One function reads the identity from a title:**
  1. The title equals the label exactly.
  2. Or it equals one leading glyph from the TUI's closed status-glyph set, then a space, then the label.
  3. Old titles from earlier builds also match: `"<label> (<role>)"` and `"<glyph> <label> (<role>)"`, with the role from the closed role-word set.
- **One resolver collects every candidate before choosing.** It reads a title as each of the forms above (exact, current glyph, old title with or without a glyph) and keeps each reading whose label is on the roster.
  - Identical labels are deduplicated.
  - One distinct candidate is the identity. An exact label with no other candidate is used as-is, not stripped.
  - Two or more distinct candidates are an ambiguity, and the pane is refused, not guessed. Examples: a roster with both `bob` and `<glyph> bob` seeing `<glyph> bob`; or `x (plan)` and `x` seeing `x (plan)`.
  - No new label restriction or identity token is added.
- **Role and state changes don't change identity.** They only change the glyph.
- Plugin panes stay excluded. A crashed agent's exited pane is still that agent's pane. Two live panes with the same identity in one workspace are an error.
- Matching by command text is removed from the agent matcher.

### Ownership is not membership: clank remembers which labels it owns panes for

The resolver's candidate set can't be the current roster alone. If it were, removing `painter` from the roster would make its still-running pane unrecognisable. `plan_panes` (status_tui/zellij.rs:436) would then never count it and never schedule its removal, and the same goes for a whole-team replacement. Today's command-text discovery finds owned labels independently of the roster, and that property has to survive.

- **The repo keeps a durable record of owned labels:** a per-repo file in the runtime namespace (`monitor::runtime_path(repo, "pane-owners.json")`), written atomically like the other runtime records.
  - **A label is added** when clank opens a pane for it (the reconciler and `clank open`) and when `clank as X` renames its pane to `X`.
  - **A label is dropped** when the reconciler closes that label's last pane in the workspace, or a relist finds no pane with that identity in the workspace.
- **Candidates for ownership are the current roster plus the recorded owned labels,** with the same verified workspace scope and the same rule that refuses a title matching two distinct candidates. Desired membership (`plan_panes`'s `cur.labels`) stays the current roster. So a removed label's pane is still discovered and counted as excess, and closed.
- **Unrelated terminals are never claimed.** A pane whose title resolves to no candidate, such as a shell or an editor, is not an agent pane and is never a removal target. Only labels on the roster or in the record count. A pane renamed by hand to an owned label is claimed, per the user's rule.
- **Restart and relist:** the record lives in the runtime namespace, so it survives a TUI restart but not a reboot (zellij sessions don't survive one either).
  - After a restart, the first relist resolves recorded labels as usual, so a stale `painter` pane left by a removal while no TUI was running is still found and closed.
  - If the record is missing or unreadable, the safe outcome is that nothing outside the roster is claimed. The TUI then shows such panes as `not on roster, not closed` for any title that is a valid label in the workspace tab and isn't a candidate. It never closes them, and the user can close them by hand.
- Remote and UI membership filtering may keep using the current roster. Lifecycle operations use the ownership candidates.

### `clank as X`: check first, bind, then rename, and report honestly

- **Check before changing anything.** Inside Zellij, before `bind_session_to_agent`, it resolves the workspace from its own pane's (`ZELLIJ_SESSION_NAME`, `ZELLIJ_PANE_ID`) tab. If that tab is this repo's workspace and another live pane there already has identity `X`, it refuses before binding anything, naming that pane. No existing binding is moved or cleared.
- **If the workspace is unresolved, or its own tab isn't the repo's workspace,** it binds as today and says the pane wasn't renamed, and why.
- **Then it binds, as today.** Intentional session reassignment and the bound-session start guard are unchanged.
- **Then it renames its own pane to `X`** through the Zellij owner in `open_zellij`.
- **If the rename fails, the binding stands,** because it's the command's purpose. It prints that the session is bound but the pane was not renamed, with the Zellij error, and exits non-zero so the result is never read as full success.
- **Outside Zellij it does nothing to panes.**
- Renaming makes a hand-started pane *locatable*. It doesn't create launch provenance, so typed delivery to that pane still needs a launch through clank. This plan doesn't promise that it's nudgeable.

### `clank agent start X` won't start a second copy of a fresh agent

When this repo's workspace has a live pane whose identity is `X` and which isn't the pane `agent start` runs in, it refuses. If the workspace is unresolved, it doesn't refuse on this ground. This adds to the existing bound-session duplicate guard rather than replacing it.

Launch provenance (pid, start time and harness version for typed input) is unchanged and still keyed by session and pane id.

## Acceptance

All tests are in-process, with Zellij listings and actions injected and nothing spawned.

**Workspace scope**
- One session holds two repos' tabs, each with `claude` and `codex` panes. Each repo resolves only its own panes, and neither repo's panes count as the other's duplicates.
- A repo hosted in a session not named by `session_name` resolves through its TUI anchor.
- These leave the workspace unresolved, with no rename, close, start refusal or typed target:
  - no anchor;
  - two anchors;
  - a session that didn't answer, with no anchor found;
  - one anchor found, but another session didn't answer;
  - a failed listing.

**Identity**
- Each of these titles matches:
  - the exact label;
  - `"<glyph> <label>"`;
  - the old `"<label> (<role>)"`;
  - the old `"<glyph> <label> (<role>)"`, using real glyphs.
- These titles, each a candidate for two distinct roster labels, are refused:
  - a literal label versus another label with a glyph: `bob` and `<glyph> bob` seeing `<glyph> bob`;
  - a literal label versus another label's old title: `x (plan)` and `x` seeing `x (plan)`;
  - a mix of glyph and old title: `<glyph> x (plan)` with `x (plan)` and `x` on the roster.
- A literal ` (plan)` or glyph-prefixed label that is the only candidate matches itself.
- A plugin pane never matches.
- An exited pane keeps its identity.
- Two live panes with the same identity are an error.
- A command-text-only pane with another name doesn't match.
- A status change and a role change rewrite the glyph and keep the identity.

**Ownership through `plan_panes`** (a transition fixture at the real reconciliation seam: discovery, then `plan_panes`, then its removal list)
- An owned `painter` pane is running. Removing `painter` from the roster without renaming the pane selects exactly that pane for removal.
- An unrelated terminal in the same tab, and another repo's `painter` in a different tab of the same session, are untouched.
- A whole-team replacement removes every old owned label's pane and opens the new ones.
- After a restart with the record present, a stale owned pane is still selected for removal.
- With the record absent, the stale pane is not closed and is reported as `not on roster, not closed`.
- After the last pane closes, the label leaves the record.

**`locate`**
- Its tests are rewritten to workspace-scoped, name-based identity.

**`clank as`**
- It renames its pane.
- A duplicate is refused *before* binding, and the existing bindings are unchanged.
- A failed rename leaves the binding, reports the failure and exits non-zero.
- An unresolved workspace binds without renaming and says so.
- Outside Zellij it touches no pane.

**`agent start`**
- The fresh-agent duplicate guard refuses in a resolved workspace and doesn't refuse in an unresolved one.

**README**
- Pane naming, the TUI-anchored workspace, and `clank as` renaming its pane.

This is one end-to-end change to pane ownership, delivered in one implementation commit, with no new polling.
