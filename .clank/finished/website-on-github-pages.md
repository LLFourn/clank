# website-on-github-pages
# The book is clank's website on GitHub Pages, with a CLI reference generated from clap

## Why

Lloyd chose (2026-11-05) to host the book as clank's website on GitHub
Pages instead of fly.io:
- the site stays in this repo;
- it rebuilds only when its files actually change;
- the binaries live on GitHub Releases (`release-binaries-and-installer`),
  so the site serves no large files.

He also wants the command-line documentation produced from the CLI's
own definitions, so it cannot drift from `clank --help`.

## What changes

- **The site is `book/`.** A workflow, `.github/workflows/pages.yml`,
  builds and deploys it with GitHub's Pages actions:
  - `actions/upload-pages-artifact`;
  - `actions/deploy-pages`;
  - permissions `pages: write` and `id-token: write`.

  It triggers on push to master with `paths: ["book/**",
  ".github/workflows/pages.yml"]`, and on `workflow_dispatch`. Nothing
  else rebuilds the site.

  **The deployed artifact is that commit's validated book.** The
  workflow has two jobs:
  - **`build`:**
    - checks out the pushed commit, with `permissions: contents: read`;
    - installs the pinned toolchain;
    - runs the focused validation for that exact checkout, which is
      `cargo test --locked -p clank --test it` filtered to the reference
      freshness check and `book_is_true`;
    - only then uploads `book/` with `actions/upload-pages-artifact`. A
      validation failure means no artifact.
  - **`deploy`:**
    - `needs: build`;
    - `environment: github-pages`, with its `url`;
    - `permissions: pages: write, id-token: write`, on this job only;
    - runs `actions/deploy-pages`.

  The workflow sets `concurrency: { group: pages, cancel-in-progress:
  true }`, so an older run cannot overwrite a newer site. These follow
  GitHub's documented custom-workflow contract.
- **A generated CLI reference.** A hidden `clank docs cli <dir>` command
  walks the clap `Command` tree (`cli::command`'s `Cli::command()`) and
  writes `book/reference/` in the book's existing HTML style:
  - an index page;
  - one page per command and subcommand, with usage, arguments, options
    with their help, and subcommands;
  - hidden commands and options omitted.

  The output is deterministic (sorted, no timestamps) and checked in.
- **Freshness.** CI regenerates the reference into a temp directory and
  fails on any difference, through a test in the `it` harness. The test
  calls the generator in-process; it does not spawn clank. So a change
  to a command's help must commit the regenerated pages, and that commit
  touches `book/**`, which is what triggers the Pages build. A source
  change that alters no help text changes nothing in `book/`.
- **The installer.** `book/install.sh` belongs to
  `release-binaries-and-installer`. This plan adds no installer link and
  claims no install endpoint until that plan supplies the file. It downloads from
  `https://github.com/LLFourn/clank/releases/latest/download/…`, or from
  a pinned version's URL when `CLANK_VERSION` is set.
- The book's navigation links the reference. README.md:7-9 names the
  site URL (`https://llfourn.github.io/clank/`), alongside opening it
  locally.
- **Not done by this plan:** turning on Pages in the repo settings
  (Settings → Pages → Source: GitHub Actions). That is Lloyd's step,
  stated in the As-landed note. A `gh api` call can do it on his OK.

## Acceptance

- The generator unit tests run on a small fixture clap `Command`:
  - every visible subcommand and option appears, with its help;
  - hidden ones do not;
  - two runs give byte-identical output.
- The `it` freshness gate passes on the checked-in reference. Mutation:
  change one option's help text in the CLI definition, and the gate
  fails until the reference is regenerated.
- An `it` gate on `pages.yml` asserts:
  - the `paths` filter is exactly the book and the workflow file;
  - the deploy uses the Pages actions;
  - there is no fly or secret usage.
- The existing `book_is_true` gate, which already checks relative links
  and anchors recursively, covers the reference pages too. No second
  link checker is written.
- The workflow gate (an `it` test over `pages.yml`) asserts the
  structure, not just that action names appear:
  - the `paths` filter;
  - `build` runs the validation step before `upload-pages-artifact`,
    and uploads `book`;
  - `deploy` has `needs: build`, `environment: github-pages`, and the
    pages/id-token permissions, while `build` has only `contents: read`;
  - the `concurrency` group cancels runs in progress.

  Mutations, each made in a fixture copy of the workflow, must fail it:
  removing `needs: build`, moving the validation after the upload, or
  dropping the concurrency group.
- **Reference generator contract.**
  - It builds the clap tree before reading any subcommand, so inherited
    global options and full nested invocation paths come out right.
  - It keeps long help, defaults, possible values and examples, and
    escapes HTML.
  - It excludes dynamic values (the build stamp in `--version`, and
    wrapping that depends on the terminal width), so the output is
    deterministic.
  - A fixture with a nested subcommand and an inherited global option
    checks the full path and the inherited flag on the nested page, and
    that HTML in a help string is escaped.

## As landed

- **54ff2904:**
  - `pages.yml` has two jobs:
    - `build` (`contents: read`): checkout, the pinned toolchain, then
      `book_is_true` and `cli_reference` on that checkout, then
      `upload-pages-artifact@v5` with `path: book`;
    - `deploy`: `needs: build`, the `github-pages` environment with its
      url, `pages`/`id-token: write` only, and `deploy-pages@v5`.
  - Concurrency group `pages` with `cancel-in-progress`. The workflow runs
    on `book/**` and its own file only.
  - The hidden `clank docs cli <dir>` writes `book/reference/`, 100 pages,
    from the built clap tree.
  - Gates:
    - `cli_reference` regenerates in-process and diffs against the
      checked-in pages;
    - `pages_workflow` checks the workflow's structure and has self-test
      mutations;
    - `book_is_true` covers nested pages.
  - Three help strings were reworded so the book's command checks hold.
  - The book's sidebar links the reference, and the README names the
    site.
  - There is no installer link.
- **ff94fd25:** each signature now uses clap's own `Arg` text, so the
  values follow `num_args`. Append and Count options say "May be
  repeated." `every_real_signature_is_the_one_help_prints` checks the
  real tree against `--help`. Eleven pages were regenerated.
- **Not done here:** turning Pages on (Settings → Pages → Source: GitHub
  Actions) is Lloyd's step, and the pages have not been viewed in a
  browser.
