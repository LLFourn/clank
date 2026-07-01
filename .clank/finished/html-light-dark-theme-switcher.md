# html-light-dark-theme-switcher

Give the HTML report BOTH a light theme and the phosphor-console dark theme,
defaulting to the visitor's **system preference** automatically, with a
manual toggle that persists best-effort.

## Context

A spike re-skinned the whole site to the phosphor-console dark aesthetic
(matching the `html-open` loading page) — it's currently in the working tree
(the `CSS` const in `html.rs`), dark-only. This plan turns that into a
switchable two-theme site rather than replacing the light look outright.

## The two themes (keep all class names — HTML rendering is untouched)

- **Light** — a refined version of the original palette (clean, readable):
  `--bg:#fbfbf9 --fg:#1a1a1a --rule:#e5e2dd --link:#0a5b8a --approve:#157a3e
  --finished:#3a4cc8 --changes:#b13e2c`, pills beige. No grain/glow.
- **Dark (phosphor console)** — from the spike / loading page:
  `--bg:#0b0b0d --fg:#e8e4d8 --fg-dim:#8a847a --rule:#221f1a
  --accent/--link:#ffb454 --approve:#74c98a --finished:#8ea6ff
  --changes:#f07a6a --code-bg:#131017`, monospace, top phosphor vignette +
  film-grain overlay, amber glow on the sha headline / pills, verdict-colored
  left bars on review cards. The dark-only extras (grain, vignette, glow) are
  scoped to the dark theme only.

## Detection — the "magic API"

- **`prefers-color-scheme`** (CSS media query) is the system preference and
  the reliable auto-default — a fresh visit follows the OS light/dark setting
  with zero interaction.
- **`window.matchMedia('(prefers-color-scheme: dark)')`** in JS resolves
  "auto" for the toggle label and can react to the OS flipping live.

## Mechanism (standard, flash-free)

CSS custom properties with a `data-theme` override on `<html>`:

```css
:root { /* light vars */ }
@media (prefers-color-scheme: dark) {
  :root:not([data-theme="light"]) { /* dark vars */ }
}
:root[data-theme="dark"] { /* dark vars */ }
```

So: no override → follow the OS; `data-theme="dark"` → always dark;
`data-theme="light"` → always light. (Dark vars live in a shared block
referenced by both the media rule and the attribute rule.)

- **Early inline `<head>` script** (before the body paints) sets
  `document.documentElement.dataset.theme` from the persisted choice — runs
  first so there's NO flash of the wrong theme on load.
- **Toggle button** in the page chrome (☀ / ☾ / auto) cycles
  light → dark → auto (auto = remove the attribute → back to the media
  query), and writes the choice to `localStorage`.

## The `file://` caveat (important)

The site is opened over `file://`, where `localStorage`/cookies are
unreliable — Safari often blocks storage on `file://`, Chrome shares one
`"null"` origin across all local files. So:

- **System preference is the reliable default** (the media query always
  works, no storage needed) — this is the load-bearing behavior.
- The manual toggle always works for the current session.
- **Persistence is best-effort**: wrap `localStorage` in try/catch; if it
  throws / is unavailable, degrade to session-only (the toggle still works,
  it just won't be remembered across a full reload). Never let a storage
  error break the page.

## Plumbing

- The toggle + early script + both palettes go into the SHARED page chrome
  (`write_doc_open_with_meta` / the header injector) so every page — index,
  commit, plan — gets them; the button sits in the status/plan/commit header.
- Restructure the `CSS` const: light vars in `:root`, dark vars in the
  media + `[data-theme]` selectors; keep every selector/class.

## Decisions to flag for review

- **Font in light mode:** the spike went full-monospace for the terminal
  identity. Keep mono in light too (cohesion) — or revert light to the
  original sans body? Lean: keep mono in both.
- **The loading page** (`render_loading_page`) is the phosphor identity and
  transient — leave it dark, or have it also honor `prefers-color-scheme`
  with a light variant? Lean: leave it dark (it's a brief splash); revisit
  if it clashes for light-mode users.

## Tests

- The chrome emits: the early `data-theme` script, the toggle control, a
  `prefers-color-scheme` media block, and `:root[data-theme="dark"]` /
  `:root[data-theme="light"]` selectors — assert on the rendered markup (JS
  behavior itself isn't Rust-unit-testable).
- Both palettes present (a light var + a dark var both appear in the CSS).

## Acceptance criteria

- A fresh visit follows the OS light/dark setting with no interaction.
- The toggle flips light/dark/auto with no flash on load; persists where
  `file://` storage is allowed, degrades gracefully where it isn't.
- Light theme is clean and readable; dark is the phosphor console; grain /
  glow / vignette appear ONLY in dark.
- Every page (index/commit/plan) has the toggle; clippy at baseline; tests
  pass.

## Deploy

After FINISHED: `cargo install --path crates/cli --force`, then regenerate
(`clank html --rebuild`, or it refreshes on the next open).
