# the-ledger-opens-the-page-it-names

> Commits and plans need to be clickable and they should open in a
> new window. We already have the HTML renderer of this content so
> that can fully be reused on this site. The tray icon is way too
> small. — lloyd

## The ledger names things it cannot show

The web ledger lists the plan, its commits and their verdicts as
text. Each has a page already: `clank html` renders the whole
history — index, `plan/<stem>.html`, `commit/<sha>.html`,
`queue/…`, `stash/…` — into `<repo>/.clank/html`, incrementally,
and the TUI opens those pages in the browser today. The web page
should open the same pages, and on a phone the `≡` that opens the
ledger is a target a thumb misses.

## The design

**The server serves the site.** `GET /html/<path>` answers from
`<repo>/.clank/html`, and only for the shapes the builder writes —
`index.html`, `plan/<stem>.html`, `commit/<hex>.html`,
`queue/<name>.html`, `stash/<name>.html`, and the one shared asset
every page links relatively, `style.css`, as `text/css` (codex on
e2e138a) — so a path is a page name, never a file walk. The site is
built by the same `build_site` the `clank html` command runs, in the
server's status loop after each snapshot (off the request path,
incremental as it is today); a request for a page that is not there
yet gets a plain "not built yet" answer rather than a 404 with
nothing to do. The site's own relative links — index to plan, plan
to commit, page to stylesheet — work unchanged under the prefix.

**Rows carry their page.** `web_facts` gives each ledger row a
`page`: a commit row its `commit/<full sha>.html`, a plan header its
`plan/<stem>.html` (the ad-hoc header has none), a review row the
page of the commit it reviewed. The page renders a row with a page
as a link that opens in a new window (`target="_blank"
rel="noopener"`), styled as the row is now — the sha or plan name is
the link text, nothing else changes colour — and a row without one
as it is.

**The tray target.** The `≡` becomes a 44px target with a glyph to
match, on the phone layout only; the desktop column has no button.

## The build

- `web/mod.rs`: the `/html/` route with the page-name allowlist;
  `build_site` in the status loop.
- `status_tui/mod.rs`: `LedgerRow.page`.
- `page.html`: anchors on rows with a page; the tray button.

## Tests

- Route: each allowed shape serves the file, `style.css` as
  `text/css`; a generated page's stylesheet link resolves to it under
  the prefix; `..`, absolute paths, and other names are refused; a
  missing page answers "not built yet".
- `web_facts`: commit, header and review rows carry the pages
  above; the ad-hoc header carries none.
- Mutations: the allowlist widened to any path — caught; a review
  row without its commit's page — caught.

## Out of scope

- Re-rendering the pages in the web page's own style.
- Editing anything from a page.

## Acceptance

- [ ] a commit or plan in the ledger opens its rendered page in a
      new window
- [ ] the pages are the `clank html` ones, served by `clank web`,
      with their stylesheet
- [ ] the tray button is a thumb-sized target
- [ ] tests as above, mutation-checked
