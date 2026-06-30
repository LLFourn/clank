# release-readiness-and-readme

**Type: research / opinion plan — no code change required.** Deliverable =
this document: a prioritized release-readiness gap analysis (Part A) and a
README redesign sketch (Part B), synthesizing the author's and **every
reviewer's independent findings, with disagreements preserved as named
dissent**. FINISHED only when both are complete and each reviewer's own
assessment is reflected.

> Status: **findings drafted** (master synthesis below incorporates codex's
> and ruthless's independent intro audits of `fba0cde`). Reviewers: this
> commit is where you do your *own* due diligence again and push back —
> correct facts, re-rank, dissent. The brief + reviewer instructions are
> retained at the bottom unchanged.

---

## Part A — release-readiness gap analysis (synthesized)

Severity: **BLOCKER** = cannot release. **SHOULD** = before a confident
release. **NICE** = post-release. Each line notes consensus vs dissent.

### BLOCKERS

**A1 · No LICENSE — the single hard gate.** *(unanimous: author, codex,
ruthless-#1)* No `LICENSE` file anywhere → the code is all-rights-reserved
by default, so a public release is legally unusable. Not a should-have.
**Fix:** dual **MIT OR Apache-2.0** (Rust-ecosystem norm; also matches the
vendored `vt100`'s MIT, confirmed intact at `vendor/vt100/LICENSE`). Add
`LICENSE-MIT` + `LICENSE-APACHE`, and the SPDX `license = "MIT OR
Apache-2.0"` to **both** crates' `Cargo.toml`.

**A2 · Crate metadata + a publish decision.** *(codex-#1; ruthless)*
Verified: `crates/cli/Cargo.toml` has only `name`/`version = 0.0.1`/
`edition` — no `description`/`license`/`repository`/`keywords`/`readme`.
`clank-core` *does* carry a description but is **`publish = false`**, which
blocks publishing `clank` (it depends on core by path). Version is pinned
at `0.0.1`, which itself signals "not ready." **Fix:** fill cli metadata;
**decide the distribution story** — either (a) flip core to publishable and
publish `core` → `cli` to crates.io, or (b) stay git-install-only and *say
so plainly*; then cut a real **0.1.0**.

**A3 · Agent-CLI hook-schema coupling has no compatibility check.**
*(ruthless-#2, ELEVATED — see dissent; author had this under
onboarding/stability)* The entire review loop depends on Claude Code's and
Codex's **hook STDIN schemas**, which clank does not control and which *do*
change (`background_tasks` landed in claude v2.1.145+; we shipped a fix for
it last cycle). **Verified:** `clank doctor` checks that hooks are
*installed* but validates **no agent-CLI version and no live hook schema**;
a repo-wide search finds *no* version/compat check anywhere. So an outside
user on an incompatible claude/codex version gets **silent breakage or
degraded gating** of the core loop, with no signal. **Fix:** a doctor check
that probes the agent-CLI version + live hook schema, a documented
supported-version range, and **loud failure rather than a silent hang**.

### SHOULD-HAVE

**A4 · No CI.** *(codex-#2; ruthless)* No `.github/workflows`; tests,
clippy, fmt, and the `git_boundary` gate run only locally — nothing
enforces them on a change. **Fix:** CI running fmt + clippy (at the agreed
baseline) + test + the git-boundary test on PRs; release automation later.

**A5 · Cross-platform honesty.** *(ruthless)* Verified: `libc` PTY + the
zellij/console UX → **macOS + Linux only**. Windows `cfg`s exist (2 sites)
but are effectively stubs/aspirational. Releasing Unix-only is fine;
*misrepresenting* it is not. **Fix:** state platform support plainly in the
README and surface it in `doctor`/build.

**A6 · Prerequisites & install friction — and zellij is optional for the
core loop.** *(both)* A new user needs a Claude and/or Codex CLI + `git` +
the Rust toolchain, then clone-and-`cargo install` (not on crates.io, no
prebuilt binaries) at `0.0.1`. **Resolved the open question** ruthless
raised: **zellij is NOT required for the core review loop.** All zellij
*command-spawning* is confined to `cli/open_zellij.rs` (the `clank open` /
`fork` workspace UX), and clank ships its *own* built-in console PTY
multiplexer as the alternative backend; the core loop
(queue/promote/wait/feedback/finish/stop-hook) spawns no zellij. **Fix:**
foreground the zellij-free core in onboarding and present zellij/console as
optional UX; a `setup`/`doctor` that checks and guides each prerequisite;
crates.io or prebuilt binaries to come.

**A7 · Documentation: pitch + a guided walkthrough.** *(both)* README is
reference-first (see Part B); there's no zero-to-first-review tutorial and
no troubleshooting section.

### NICE-TO-HAVE (post-release)

**A8 · CHANGELOG, a `docs/` site, an asciinema/`status --tui` screenshot, a
Homebrew tap / prebuilt binaries.** *(both)*

### SAFETY FOOTGUNS — document prominently *(author A.8 + ruthless)*

- **Install replaces the running binary:** `cargo install --force` swaps
  the binary the live session's own Stop hook uses, mid-flight (we hit this
  every deploy). Surprising; must be documented.
- **History rewrite** via `purge` / `shelve`.
- **Multi-agent token cost:** a multi-agent review loop burns real money —
  an outside user must be told up front. Candor here builds trust.

---

## Part B — README redesign sketch (synthesized; codex + ruthless largely agree)

**Core move: lead with the hook, not the concept.** The current opener
("Multi-agent peer review around plans…") states *what it is*, not *why you'd
want it*. First three lines should land the insight: **AI agents
peer-reviewing each other commit-by-commit, gated like a real team's PR
process — everything in git, no daemon, no server.**

**The differentiator** (vs raw claude/codex or a single agent looping): the
structured **between-agents gate** — commit-tier + gate-tier reviewers,
`CONTINUE` / `FINISHED` / `REQUEST_CHANGES` verdicts. *That gate is the
product.* Say it early.

**Proposed section order** (move internals off the first screen):

1. **Tagline + one-paragraph hook** — the insight above.
2. **The problem / why** — one agent can't peer-review itself; clank makes
   agents gate each other against a plan, in git.
3. **60-second quickstart that works from zero** — `cargo install`,
   `clank setup`, `clank doctor`, then a first plan → review → finish.
   *Before* any command reference.
4. **How it works** — master/reviewer roles, plans, the gate, all-in-git /
   no-daemon — with a concrete `clank status` / `clank log --oneline`
   snippet showing the review gate in action.
5. **Requirements & platform support** — Unix-only (macOS/Linux), a Claude
   and/or Codex CLI, git, Rust toolchain; **zellij optional** (console is
   built in).
6. **Status & caveats (non-optional, near the top)** — dogfooded daily but
   **pre-release (0.0.1)**, Unix-only, depends on specific agent-CLI
   versions, multi-agent cost. Candor is a feature for this audience.
7. **What clank is NOT** — not a hosted service, not a daemon/server, not a
   general agent framework; a git-native multi-agent review workflow.
   (Scoping is half the pitch.)
8. **Command reference / deeper docs** — link out.

**Credibility:** an asciinema cast or a `status --tui` screenshot of the
gate.

---

## Reviewer synthesis & preserved dissent

- **Consensus (unanimous):** A1 LICENSE is a hard blocker. Also agreed:
  crate metadata + publish decision (A2), CI (A4), cross-platform honesty
  (A5), and a pitch-first README led by the between-agents gate.
- **Named dissent / elevation — kept, not smoothed:** ruthless **elevates
  A3 (agent-CLI hook-schema coupling) to a top blocker**, where the
  author's brief filed it under onboarding/stability. Master concurs after
  verifying nothing in the codebase validates agent-CLI versions or hook
  schemas — recorded as ruthless's position with master's agreement;
  codex's ranking placed it third (still a release item). *Reviewers: if
  you disagree with the elevation, say so here.*
- **Fact corrections folded in (from ruthless's independent audit):**
  clank-core *has* a description (only `clank`/cli lacks metadata); core is
  `publish = false`; the vendored vt100's MIT license/attribution is
  intact; OS/zellij coupling is ~259 refs in `cli/src` (the brief's ~296
  was high). None change the picture.
- **Open question resolved by master:** zellij is optional for the core
  loop (A6).

## Release-blockers vs post-release cut line

- **Must-fix before ANY public release:** A1 (license), A2 (metadata +
  publish decision + 0.1.0), A3 (agent-CLI compat check + documented
  version range), Part B (pitch + honest caveats), A5 (platform statement).
- **Before a *confident* release:** A4 (CI), A6 (prereq-guiding
  setup/doctor), A7 (walkthrough).
- **Post-release:** A8 (CHANGELOG, docs/, prebuilt binaries, screenshots).

Each blocker is naturally its own follow-up plan once this research lands.

---

## Reviewer instructions — INDEPENDENT due diligence is the point

Do **not** rubber-stamp this synthesis, and do **not** defer to the other
reviewer. Re-audit yourself; give **your own** ranked blockers (severity +
rationale) and **your own** README/positioning opinion in your feedback.
**Divergence is the goal** — where you disagree with the author or each
other, say so and argue it; dissent is preserved above, not smoothed.
Verdicts: **REQUEST_CHANGES** if a real blocker is missing or the project is
misrepresented; **CONTINUE** if it's progressing but you have more to add;
**FINISHED** only when the document honestly and completely captures the
release picture *and* your independent assessment (including any dissent) is
reflected.

## Done criteria

The document contains: a prioritized gap table (severity, rationale,
remediation) synthesizing all independent findings with dissent preserved;
a README redesign sketch; and a blockers-vs-post-release cut line. FINISHED
when complete and every reviewer has contributed their own assessment — not
merely signed off.

## Out of scope

Implementing fixes, writing the final README, publishing to crates.io,
standing up CI. Those are follow-up plans informed by this research.

---

## Appendix — original research brief (the 9-category scope, unchanged)

Part A was to cover, at minimum: (1) licensing & legal; (2) distribution &
install / crates.io readiness; (3) prerequisites & onboarding; (4)
cross-platform; (5) documentation; (6) stability & correctness incl.
Stop-hook reliability; (7) release process & versioning; (8) safety &
footguns; (9) positioning & scope. Part B was to sketch the README that
sells the project (tagline, problem, the model, 60-second quickstart, "how
it works" with a real flow, requirements/platform, honest status, what it's
NOT, credibility visuals).
