# Vendored `vt100`

This is `vt100` **0.16.2** from crates.io, copied verbatim except for a
**single one-line patch**, and activated via the workspace root:

```toml
[patch.crates-io]
vt100 = { path = "vendor/vt100" }
```

clank still declares `vt100 = "0.16.2"` as a normal dependency; the patch
just swaps in this copy. `vendor/` is **not** a workspace member (the
workspace `members` list is explicit), so this crate isn't built as a
member, isn't covered by `cargo fmt`/`clippy`, and its own tests don't
run — keep the patch hand-formatted in upstream's style.

## The patch

`src/grid.rs`, `Grid::scroll_up`:

```diff
-            if self.scrollback_len > 0 && !self.scroll_region_active() {
+            if self.scrollback_len > 0 && self.scroll_top == 0 {
```

### Why

vt100 only saved a scrolled-off line to scrollback when there was **no**
scroll region at all (`!scroll_region_active()` ==
`scroll_top == 0 && scroll_bottom == rows-1`). A TUI that pins a fixed bar
to the bottom row sets a DECSTBM region with `scroll_bottom = rows-2`
(e.g. the codex CLI), so **every** line it scrolled was discarded and its
scrollback stayed empty — meaning clank's console (and any multiplexer
using this crate) had nothing to scroll, even though the same app scrolls
fine in Ghostty/zellij/xterm.

A line scrolled off the **top of the screen** belongs in scrollback
regardless of a bottom margin. Real terminals save it whenever the
region's top is row 0; this is exactly what zellij does
(`grid.rs::add_canonical_line`: `if scroll_region_top == 0 &&
alternate_screen.is_none() { transfer_rows_to_lines_above(1) }`). The
patched condition `self.scroll_top == 0` matches that.

It stays correct across the cases:
- **no region** (`top==0, bottom==rows-1`): saves — unchanged.
- **top-anchored region with a bottom bar** (`top==0, bottom<rows-1`):
  now saves — the fix (codex).
- **contained mid-screen region** (`top>0`): still doesn't save — correct.
- **alternate screen**: the alt grid is constructed with
  `scrollback_len == 0` (`src/screen.rs`), so the unchanged
  `scrollback_len > 0` guard keeps it from ever accumulating.

A clank-side regression test
(`crates/cli/src/cli/console/screen.rs::scroll_region_top_anchored_keeps_scrollback`)
pins the observable behavior, so a future vt100 bump that loses this patch
is caught.

## Re-applying on a version bump / upstreaming

Re-copy the new upstream version, then re-apply the one-line change above
(and this note). Upstreaming to vt100 (or switching to the maintained
`vt100-ctt` fork once it's confirmed to carry an equivalent fix) would let
us drop this vendor entirely.
