# alt-arrows-belong-to-the-pane

> One nit I have about my current setup is that codex uses option +
> up arrow to edit previous prompt or answer questions — but zellij
> seems to be capturing that with my current set up. Can you figure
> out a better config for me and ensure that's passed through. If we
> could ensure it in the clank zellij layout generation code that
> would be good. — lloyd

## The key never arrives

zellij 0.46.0's default keybinds put `Alt Up` / `Alt Down` /
`Alt Left` / `Alt Right` on pane focus in every mode but locked
(`shared_except "locked"`, alongside `Alt h/j/k/l`), and this
machine's config binds them in locked mode too. So in a clank
session the codex pane never sees Alt+Up — zellij moves focus
instead — and codex's "edit the previous message" is unreachable.
The programs in clank's panes are agent TUIs with composers, and
Alt+arrows are composer keys to them; `Alt h/j/k/l` already cover
focus and are what zellij's own docs recommend keeping.

## What a layout can carry (measured on zellij 0.46.0)

A layout file may hold any configuration a config file can, at the
layout's root, beside the `layout` node; the docs say it takes
precedence over the loaded config and is ignored by `new-tab`. What
matters for clank is the attach path, since keybinds are handed to
the server per client. Measured with a two-pane layout whose focused
pane runs `cat -v`, a client on a pty, and `ESC [ 1 ; 3 A` written to
it (the probe's cursor advances seven columns when the bytes reach
the pane; focus moves when zellij eats them):

- create with a plain layout: focus moved — the control.
- create with `keybinds { unbind "Alt Up" "Alt Down" "Alt Left"
  "Alt Right" }` in the layout: the probe's cursor went 0 → 7. The
  rest of the config stayed (`Ctrl o` `d` still detached).
- a later plain `zellij attach` to that session, a fresh client with
  only config.kdl: 7 → 14. The session keeps the keybinds it was
  created with.
- `zellij -l <plain layout> attach` to it: 14 → 21 — an attach-time
  layout changes nothing, in either direction.
- `zellij action override-layout <layout with the block>`: exit 0,
  tab count unchanged, keybinds unchanged — the live re-layout
  tolerates the block and ignores it, as `new-tab` does.

So the block in the layout `clank open` creates the session with is
enough for the life of that session, every attach included; the
paths that add a tab or re-layout a live tab can carry the same
layout without harm.

## The design

Every layout clank composes carries ONE effective override: the
first root `keybinds` node's first global `unbind` names the four
keys.

    keybinds {
        unbind "Alt Up" "Alt Down" "Alt Left" "Alt Right"
    }

Why merged, not appended (codex on 5262fa7, checked against
zellij-utils at main ff0925f): a layout's configuration is read by
`Config::from_kdl`, which takes the FIRST root `keybinds` node
(`kdl/mod.rs:5002`), and `Keybinds::from_kdl` takes the FIRST
`unbind` child of it as the global unbind (`:4738`). A user template
that already has a `keybinds` node — for its own binds, or its own
unbinds — would make an appended second block a no-op. The global
unbind is applied AFTER every `bind` block in the node, so once the
four keys are in it nothing else in that node can bind them again.

So the composer edits the parsed document rather than the text:
find the first root `keybinds` node, else append one; find its first
`unbind` child, else insert one; add whichever of the four keys its
arguments lack. Everything else in the node — other modes, binds,
unbinds, `clear-defaults` — is untouched, and a document that already
has the four is unchanged. One function, `ensure_pane_keys(&mut
KdlDocument)`, run by the single-tab `compose_kdl` on both arms
(built-in and user template — the `clank_agents` marker contract is
untouched, this is a root sibling) and by the multi-tab
`compose_multitab`. `compose_live_layout` goes through
`compose_kdl`, so `override-layout` receives it too, measured
harmless. Not configurable: nothing asked for it, and `Alt h/j/k/l`
remain for focus in every mode.

Rejected: clank editing `~/.config/zellij/config.kdl` — clank does
not own that file (this machine's was edited by hand today, Alt+arrow
binds dropped from `locked` and `shared_except "locked"`, and the
live config reload applies it to running sessions); `--config` on
attach — it replaces the user's config rather than adding to it; a
config knob — a second way to get the same four keys.

## The build

- `open_zellij.rs`: `PANE_KEYS: [&str; 4]`; `fn ensure_pane_keys(doc:
  &mut kdl::KdlDocument)` as above; `compose_kdl` parses each arm's
  result, ensures, and serializes; `compose_multitab` does the same
  in place of its parse-only validation.
- README, the `clank open` paragraph: one sentence — Alt+arrows reach
  the agents (codex edits its last message on Alt+Up); focus moves on
  Alt+h/j/k/l.

## Tests

- Built-in template and `compose_multitab`: the document parses, has
  exactly one root `keybinds` node whose first `unbind` child's
  arguments are exactly the four keys; `layout` is still the first
  root node and, for the built-in, still holds both
  `swap_tiled_layout` variants.
- A user template that already has a `keybinds` node with a global
  `unbind "Ctrl q"`, a `locked { bind "Alt Up" { MoveFocus "Up"; } }`
  and `clear-defaults=true`: afterwards there is still one `keybinds`
  node, its first `unbind` lists `Ctrl q` and the four keys, the
  `locked` block and `clear-defaults` are as written, and the marker
  substitution still happened.
- A user template whose `keybinds` node has modes but no global
  `unbind`: one is inserted with the four keys; the modes are as
  written.
- A user template that already unbinds the four: the document is
  unchanged.
- Mutations: skip the ensure in either composer — that composer's
  test fails; append a node instead of merging — the existing-node
  test fails; add to a later `unbind` instead of the first — it
  fails.

## Out of scope

- Other keys the harnesses use (Alt+Enter and friends are unbound by
  default); users add more to their own config or template.
- A `clank doctor` line for sessions created before this change —
  the fix is `clank open` after the session is deleted, as for any
  layout change.

## Acceptance

- [ ] every composed layout's first root `keybinds` node has, as its
      first global `unbind`, the four Alt+arrows — for built-in,
      user-template (with or without its own `keybinds`) and multi-tab
- [ ] a template's own keybinds, unbinds and `clear-defaults` survive
- [ ] the `layout` node and its swap variants are unchanged
- [ ] README says what the keys do
- [ ] tests as above, mutation-checked
