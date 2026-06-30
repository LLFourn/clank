# release-readiness-and-readme

**Type: research / opinion plan — no code change required.** The
deliverable is a *document*: a prioritized release-readiness gap analysis
plus a README redesign sketch. FINISHED only when the document is
complete **and every reviewer has contributed their own independent
assessment** (see "Reviewer instructions" — that is the heart of this
plan).

## Why

clank is dogfooded daily but has never been prepared for anyone outside
this machine. Before any release we need two things, and both must
reflect **multiple independent judgments, not one author's view**:

1. A clear-eyed, prioritized list of what's actually missing for release.
2. A README that *sells* the idea to someone who has never seen clank.

## Current state (grounding — verify, don't trust)

A quick survey; reviewers should re-check rather than take these as given:

- `README.md` exists (~236 lines): install + workflow, but reads like a
  reference manual, not a pitch.
- **No `LICENSE` file** anywhere.
- `crates/cli/Cargo.toml` carries only `name`/`version = 0.0.1`/`edition`
  — no `description`, `license`, `repository`, `keywords`, `authors`,
  `readme`.
- **No CI** (`.github/workflows` absent), **no `CHANGELOG`**, **no
  `docs/`**.
- Install today = `git clone` + `cargo install --path crates/cli`. Not on
  crates.io.
- ~74 source/test files carry tests; heavy zellij/OS coupling (~296
  `target_os`/`zellij` references) — the cross-platform story is unclear.
- Hard prerequisites: a Claude Code and/or Codex CLI, `git`, a terminal
  multiplexer (zellij), plus the gix stack.

## Part A — release-readiness gap analysis

Produce a **prioritized** gap list. For each gap give: **severity**
(blocker / should-have / nice-to-have), *why it matters for an outside
user*, and a concrete remediation. Cover at least:

1. **Licensing & legal** — choose/add a `LICENSE`; audit dependency
   licenses; the vendored `vt100` fork's licensing + attribution
   (`vendor/vt100/VENDOR.md`).
2. **Distribution & install** — crates.io publish readiness (metadata,
   two-crate publish order `core`→`cli`, a real version off `0.0.1`);
   prebuilt binaries / Homebrew tap; the friction of clone-and-build.
3. **Prerequisites & onboarding** — what a new user must install
   (claude/codex/zellij/git); first-run experience; `init` / `setup` /
   `doctor`; failure modes when a prerequisite is missing or the agent
   CLIs change their hook schema.
4. **Cross-platform** — macOS vs Linux vs Windows; the zellij / PTY /
   console assumptions; what is actually supported vs aspirational.
5. **Documentation** — README; per-command `--help` quality; the skill
   docs; a guided end-to-end walkthrough/tutorial; troubleshooting.
6. **Stability & correctness** — test-coverage gaps; known rough edges;
   error-message quality; Stop-hook reliability (including the
   background-work yield just shipped); behavior under misconfiguration.
7. **Release process & versioning** — a semver path off `0.0.1`;
   `CHANGELOG`; tagging; CI that runs tests + clippy + the git-boundary
   gate; release automation.
8. **Safety & footguns** — history rewrite / `purge` / `shelve`; the
   install-replaces-the-running-binary hazard; multi-agent token cost;
   anything that can surprise or destroy.
9. **Positioning & scope** — the elevator pitch; the target audience; the
   differentiator vs raw Claude/Codex or other agent orchestrators; and
   what clank deliberately is **not**.

## Part B — README redesign sketch

Sketch how `README.md` should change to **sell** the project — an outline
plus key copy and positioning, not necessarily a finished rewrite.
Address:

- The hook/tagline in the first three lines — what makes someone keep
  reading.
- The problem it solves, and for whom.
- The core model, fast: plans, the master/reviewer roles, peer review
  *between agents*, everything in git, no daemon/server.
- A 60-second quickstart that actually works from zero.
- "How it works" with a concrete example/flow (e.g. a `clank log` /
  `status --tui` snippet showing the review gate).
- Requirements, honest platform support, and a candid maturity/status +
  caveats section.
- Credibility/visual elements (asciinema or a `status --tui` screenshot).
- Tone: opinionated, dogfooded, git-native.

## Reviewer instructions — INDEPENDENT due diligence is the point

Do **not** rubber-stamp the author's draft, and do **not** defer to the
other reviewer. Each reviewer must investigate independently and form
their own opinion:

- Audit the repo yourself: try a clean install path, read the code / help
  / docs, exercise (or carefully reason about) the real flows, and hunt
  for gaps the author missed.
- In your review feedback, give **your own**: (a) top release blockers,
  ranked, with severity + rationale; and (b) your own opinion on the
  README pitch/positioning — the angle, what's missing, what you'd cut.
- **Divergence is the goal.** Where you disagree with the author or the
  other reviewer, say so explicitly and argue it — dissent is signal, not
  noise. Do not converge just to agree.
- Verdict semantics here: **REQUEST_CHANGES** if the document misses a gap
  you consider a real blocker or misrepresents the project;
  **CONTINUE** if it's progressing but you have more to add; **FINISHED**
  only when you believe the document honestly and completely captures the
  release picture *and* your independent assessment (including any
  dissent) is reflected in it.

## Done criteria

The plan document ends up containing:

- A prioritized release-readiness gap table (severity, rationale,
  remediation) that **synthesizes the author's and every reviewer's
  independent findings, preserving disagreements as named dissent.**
- A README redesign sketch (outline + key copy + positioning).
- A clear "release blockers vs post-release" cut line.

FINISHED when the document is complete and all reviewers have contributed
their own independent assessments — not merely signed off.

## Out of scope

Implementing fixes, writing the final README, publishing to crates.io,
standing up CI. Those become follow-up plans informed by this research.
