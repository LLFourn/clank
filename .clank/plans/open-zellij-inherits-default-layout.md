# open-zellij-inherits-default-layout

`clank open zellij` currently emits a fully self-contained
layout in `.clank/zellij/layout.kdl` — including the
`zellij:tab-bar` / `zellij:status-bar` plugin panes nailed
inline inside `tab name="<repo>"`. Two consequences:

1. Tab chrome is duplicated in clank instead of inherited
   from the user's normal zellij setup. Theme, plugin
   aliases, custom bars (e.g. `compact-bar`), or any other
   default-layout customization the user has are ignored.
2. New tabs spawned at runtime (`Ctrl-t n` etc.) drop the
   bars entirely, because the bars are panes of the clank
   tab — not part of a `default_tab_template`. The user
   loses their UI chrome the moment they open a second
   tab.

## Goal

Make the clank layout *delegate* tab-chrome to the user's
default zellij layout. clank should only specify the panes
that are repo-specific (the master + reviewer agent
panes); everything else inherits.

## Verified before promotion (2026-06-06)

Ran `zellij setup --dump-layout default` on this machine
(no custom `~/.config/zellij/layouts/default.kdl`).
**Actual output:**

```
layout {
    pane size=1 borderless=true { plugin location="tab-bar" }
    pane
    pane size=1 borderless=true { plugin location="status-bar" }
}
```

The built-in default has **no `default_tab_template`** —
bars are top-level `pane`s. The original draft's claim that
the dumped layout always declares `default_tab_template` is
empirically false for this case.

**Pinned scope (lloyd 2026-06-06)**: clank inherits ONLY
the user's `default_tab_template` if present. Users on a
layout without one (including the zellij built-in default)
get the simpler behavior: clank's tab is spliced as a
sibling of whatever's already in `layout { ... }`, with no
chrome inheritance. **Documented as the contract** so users
who want clank's tab to share their bars know to set up a
`default_tab_template` in `~/.config/zellij/layouts/default.kdl`.
The plan does NOT synthesize a `default_tab_template` from
bare top-level panes — that would be us trying to be
smarter than the user's config.

Other surfaces verified:
- `open_zellij.rs:138-165` (`compose_kdl`, `push_pane`) —
  confirmed via grep.
- `kdl_escape` at `:188` — unchanged.

## Approach

At `clank open zellij` time:

1. Shell out to `zellij setup --dump-layout default` to
   get the user's resolved default layout as KDL on stdout.
2. Find the top-level `layout {` block via brace-walker;
   strip any existing `tab` blocks inside it (the user's
   default may have an empty `tab` placeholder); splice
   clank's `tab name="<repo>" { ... }` block as a sibling
   of whatever else is in the layout (panes, plugins,
   `default_tab_template`, etc. — preserved verbatim).
3. Write the merged KDL to `.clank/zellij/layout.kdl`.
4. `zellij --layout <path>` as today.

**Inheritance contract**: clank inherits the user's
`default_tab_template` if defined. If the user wants
bars / chrome on the clank tab, they must declare a
`default_tab_template` block in their default layout. Users
on the built-in zellij default (or any layout that puts
chrome panes at the top level without a template) get a
bar-less clank tab — clank does not synthesize a template
from bare panes (that would override user intent).

This is documented in the surfaced stderr message AND in the
generated `<repo>/.clank/zellij/layout.kdl` header comment
so users who first encounter the bar-less behavior have a
single place to look for the fix.

## Surfaces touched

- `crates/cli/src/cli/open_zellij.rs`:
  - Replace `compose_kdl` (lines ~138-163) with a two-step
    composer: (a) fetch the base layout via
    `zellij setup --dump-layout default`, (b) inject the
    clank tab block.
  - The clank tab block becomes just:
    ```kdl
    tab name="<repo>" {
        pane split_direction="horizontal" {
            pane name="<master> (master)" cwd="..." { ... }
            pane name="<reviewer> (reviewer)" cwd="..." { ... }
            ...
        }
    }
    ```
    No `tab-bar`/`status-bar` panes anymore.
  - Need a small KDL-aware splicer. Cheapest route: treat
    the dumped layout as text, find the top-level
    `layout {` opening brace, walk to its matching `}`,
    strip any sibling `tab` blocks, insert the clank tab
    just before the closing `}`. Brace-matching is fine
    here because KDL block strings can't contain
    unescaped `{`/`}` (we already escape via
    `kdl_escape`).
  - Keep `kdl_escape` and `push_pane` — only the bars and
    the outer shell change.
- Tests in the same file:
  - Update existing `compose_kdl_*` cases — they currently
    assert the presence of `zellij:tab-bar` / `zellij:status-bar`
    in the OUTPUT (clank-emitted). After this change clank
    doesn't emit bars itself; tests inject fixture base
    layouts and assert bars appear via inheritance OR are
    absent (Shape B input → bar-less clank tab, by design).
  - Add `splice_preserves_default_tab_template`: fixture base
    layout has a custom `default_tab_template` with a
    `compact-bar` plugin; assert the merged layout preserves
    that template verbatim AND has `tab name="<repo>"` as
    its sibling.
  - Add `splice_into_bare_pane_layout_emits_no_template`:
    fixture base layout is the zellij built-in (top-level
    bar panes, no `default_tab_template`); assert the merged
    layout has the clank tab AS A SIBLING of the bare panes
    (preserving them verbatim), and that we did NOT synthesize
    a template. This pins the "no override of user intent"
    decision per lloyd 2026-06-06.
  - Add `splice_strips_empty_tab_placeholder`: fixture has
    `layout { tab }` (the built-in default-layout-with-empty-tab
    shape); assert the empty `tab` is stripped before clank's
    tab is inserted.

## Fallbacks

- `zellij setup --dump-layout default` fails or zellij is
  not on `PATH`: fall back to today's hardcoded layout so
  the command still works in environments without zellij
  available at compose time. Surface a one-line stderr
  note: `note: using built-in fallback layout; install
  zellij or set --base-layout to inherit your default`.
- `zellij` is installed but the default layout has no
  `default_tab_template`: don't synthesize one — pass it
  through. The user will see a bar-less clank tab, which
  is what their `zellij` would do anyway. Don't paper
  over user config.

## Out of scope

- A `--base-layout <name>` flag to inherit a layout other
  than `default`. Easy to add later; default behavior is
  the win.
- Reading user's `~/.config/zellij/layouts/clank.kdl` and
  using it as the base. The dump-layout route is more
  general (works for users with no custom layouts file
  too) and avoids two competing sources of layout truth.
- Auto-detecting which zellij plugin (`tab-bar` vs
  `compact-bar` vs custom) the user prefers — that's
  whatever their `default_tab_template` says, and we just
  inherit.
- Touching the `--print` (dry-run) path beyond what the
  surface change naturally implies — its argv format is
  unchanged.

## Why

Two architectural wins:

1. **One source of truth for tab chrome.** Today
   `~/.config/zellij/layouts/clank.kdl` (hand-written)
   and `.clank/zellij/layout.kdl` (generated) both
   declare bars. Whichever zellij actually loads wins.
   After this change clank stops declaring bars at all —
   the user's normal config is authoritative.
2. **Runtime tabs inherit chrome for free.** Because the
   user's default layout has `default_tab_template`,
   spawning new tabs in a clank session keeps the bars
   without any extra work in clank.
