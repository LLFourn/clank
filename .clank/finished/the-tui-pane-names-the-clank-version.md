# the-tui-pane-names-the-clank-version
# The TUI pane names the clank version, and the monitor line appears only when something is wrong

## Problem

The TUI shows `monitor  pid 87304 · 66635f62 current` all the time. Lloyd finds it
unattractive, and it tells a person nothing they can use:

- `66635f62` is the first 8 hex characters of the random build stamp
  (`build_stamp::Stamp`). It names no commit, so nobody can look it up.
- In the healthy case the line always reads the same, so people learn to ignore it.
  Then they miss it on the one occasion it matters.
- `clank --version` prints `0.0.1` for every build, so clank has no version a
  person can read anywhere.

We still want to see the version in the TUI. Real releases are coming soon, and
most users will then be running a tagged release.

## Change

1. **A human version, computed at build time.** Format, following semver build
   metadata:
   - A release (built from a tagged commit, or with no git checkout at all, e.g.
     a crates.io install): `1.0.0`, which is plain `CARGO_PKG_VERSION`.
   - A checkout build: `0.0.1+c5eaf416`, the short sha of HEAD.
   - A checkout build with uncommitted changes: `0.0.1+c5eaf416.dirty`.

   `crates/cli/build.rs` computes it and writes it into the generated file next to
   `STAMP`. It must rerun when HEAD moves: add `rerun-if-changed` for the HEAD file
   and the ref it points at. Check that the `git_boundary` gate's view of `build.rs`
   is respected. If the gate scans it, read `.git` directly or justify the exception
   the way the gate expects. Don't loosen the gate.

   `clank --version` prints this version.
2. **The TUI pane is named `clank <version>`.** On startup the TUI renames its own
   pane, using the existing `open_zellij::rename_pane` with its own pane id.
   Workspace anchoring finds the TUI pane by `terminal_command`, never by its
   title, so renaming it is safe. Confirm that holds, and that no other pane lookup
   reads the TUI's title. If the TUI is not running inside zellij, do nothing.
3. **The monitor line is shown only when something is wrong.**
   `status_tui/render.rs::monitor_spans`: when the monitor is running the build
   that is installed, draw no row at all (not an empty row). Otherwise use the
   warning colour, with wording a person can act on:
   - Running a build older than the one installed:
     `monitor  running <its version>, installed <installed version> — restart to update`.
     This needs the human version of both builds. The monitor knows its own; the
     installed binary's version has to be read the same way its stamp already is
     (`build_stamp::find` / `Install::Replaced`). Embed the version where it can be
     found next to the stamp, rather than inventing a second lookup.
   - Process exited: `monitor  stopped`.
   - None running: `monitor  not running — nothing will wake agents`.
   - Can't tell: `monitor  status unknown`.
   - Install not yet observed, or the install can't be checked: treat both as healthy
     (no row). These are transient or about the environment, and are not something
     the user can act on in the TUI.

   The pid and the random stamp leave the TUI. `clank status` and `clank doctor`
   keep whatever they already show.

## Out of scope

- Release tooling, tagging, publishing.
- Changing how the monitor detects a reinstall. The random stamp stays the
  mechanism; this plan only stops displaying it.

## Acceptance

- The version function is unit-tested on its inputs: a tagged commit, a plain
  commit, a dirty tree, no git. The tests use a fixture, not the real repo.
- The pane-rename argv is asserted without spawning zellij.
- `monitor_spans` (or the row layout that hosts it) has tests showing: no row when
  healthy, and each warning's text when not. Mutation-check that the healthy case
  really omits the row: a mutant that draws a blank row must fail.
- The TUI renders without the monitor row and with no leftover blank line.
- No WHAT comments.
