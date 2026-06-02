# clank-html-commit-message-and-copyable-shas

Two related polish items for `clank html`:

1. Show the full commit message body on each per-commit
   page — currently we only render the subject. The body
   often carries the reasoning a reviewer actually wants.
2. Make every rendered SHA copyable in one press, anywhere
   it appears (timeline rows, commit page header,
   `<p class="sha-full">`, future per-plan pages).

## Commit message body

`render_commit_page` currently takes the subject from
`commit_subject(repo, sha)`. Add a sibling helper:

```rust
fn commit_body(repo: &Path, sha: &CommitSha) -> String {
    // `git log -1 --format=%B` returns subject + blank +
    // body. We only want the body part.
}
```

Render under the existing `<h2 class="subject">` block:

```html
<pre class="commit-body">…multi-line body…</pre>
```

Empty body → skip the section entirely (don't render an
empty `<pre>`).

The body's first paragraph often references the plan; keep
it raw text (no markdown parsing — commit messages aren't
markdown). If the body grows past N lines (~20) consider
wrapping in a `<details>` collapse; v1 just renders inline.

## Copyable SHAs

UX (per frontend-design principles applied to a static
site):

- Treat the SHA as a primary affordance. Wrap each rendered
  SHA in a button-like element: `<button class="sha-copy"
  data-sha="<full>">abc1234</button>`.
- On click, copy the FULL SHA (not the abbreviated form
  visible in the button) to the clipboard via
  `navigator.clipboard.writeText`.
- Confirmation: swap the text to `copied!` for ~1s, then
  back to the original. Inline change, no toast — toasts on
  static review pages feel out of place and pull focus.
- Hover state: faint background (`var(--pill-bg)`) so
  affordance reads as clickable. Pointer cursor.
- Tooltip on hover (`title="<full-sha>"`) so the full hash
  is also readable without clicking.
- No-JS fallback: a `<button>` without a JS handler is just
  text the user can select. Already works — the
  `data-sha` attribute is metadata; the visible content is
  always the abbreviated form. Add a small `<noscript>`
  notice on the index page if we want to be explicit, but
  honestly the in-button text is already readable.

CSS:

```css
.sha-copy {
  font: 500 .85rem/1 var(--mono);
  background: transparent;
  border: 0;
  color: inherit;
  padding: 0 .2em;
  border-radius: 3px;
  cursor: pointer;
}
.sha-copy:hover { background: var(--pill-bg); }
.sha-copy.copied { color: var(--approve); }
.sha-copy.copied::after { content: " copied"; font-size: .75em; }
```

Inline JS (added to the existing relative-time `<script>`
block at the bottom of every page):

```js
document.querySelectorAll('.sha-copy').forEach(function (btn) {
  btn.addEventListener('click', function () {
    var full = btn.getAttribute('data-sha') || btn.textContent;
    navigator.clipboard.writeText(full).then(function () {
      btn.classList.add('copied');
      setTimeout(function () { btn.classList.remove('copied'); }, 1000);
    });
  });
});
```

## Surfaces touched

- `crates/cli/src/cli/html.rs`:
  - `commit_body(repo, sha)` helper + render call in
    `render_commit_page`.
  - Replace `<code class="sha">…</code>` (timeline row),
    `<h1><code>…</code> …</h1>` (commit page), and
    `<p class="sha-full"><code>…</code></p>` (commit page)
    with `<button class="sha-copy" data-sha="<full>">…</button>`.
  - Extend the inline `<script>` with the copy-on-click
    handler.
  - CSS additions for `.sha-copy`, `.sha-copy:hover`,
    `.sha-copy.copied`.
- No core changes. No new deps.

## Tests

- `html_commit_page_renders_commit_body` — seed a commit
  with a multi-line message; assert the body lines appear
  inside a `<pre class="commit-body">` block.
- `html_commit_page_omits_body_section_for_subject_only_commits`
  — commit message has subject only; assert no
  `<pre class="commit-body">` element appears.
- `html_shas_are_copy_buttons` — assert each SHA on the
  index timeline AND the commit page header carries
  `class="sha-copy"` with a `data-sha="<full-sha>"` attr.
- `html_inline_script_handles_sha_copy` — assert the
  inline `<script>` block contains the
  `document.querySelectorAll('.sha-copy')` selector.
- `html_no_js_fallback_keeps_sha_text_visible` — assert
  the abbreviated SHA is the button's text content (not
  hidden behind a JS-only label) so it's readable when
  JS is disabled.

## Out of scope

- A copy-to-clipboard for the FULL commit subject /
  message. Single SHA copy is the request.
- Replacing copy-on-click with select-on-click. Click-to-
  copy is the more familiar pattern on review sites.
- Toast / floating popup confirmation. Inline swap reads
  cleaner against the existing layout.
- Tooltips for non-SHA elements. Scope creep.
