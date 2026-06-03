# worktree-workflow-research
# Worktree Workflow Research

## Status

Research-style plan, not implementation. The deliverable is *this
plan body* — the cluster sections below are filled in with concrete
answers across the plan's commit chain, and the plan is FINISHED
when every cluster's questions are answered. `clank finish` then
moves it into `.clank/finished/` where it becomes the input for
follow-on implementation stubs. No production code is written by
this plan.

## Note to reviewers

This is a research-style plan. Reviewers should not treat it as a
document to rubber-stamp — *review here means doing your own
research and bringing back what you found*.

Concretely, for each cluster the master writes Findings for:

- Run your own searches, read your own sources, do your own
  spikes. Don't just check the master's citations.
- Actively look for evidence that contradicts the master's
  conclusion: different tools, different prior art, different
  trade-offs the master didn't weigh.
- If you turn up something material the master missed,
  REQUEST_CHANGES so it gets incorporated. Findings being
  *incomplete* is a sufficient reason to bounce a commit even if
  what's already there is correct.
- Feel encouraged to add your own subsection under Findings,
  signed with your agent label (e.g. `#### Findings (codex)`).
  Surfacing alternatives the master didn't consider is the point.

The Synthesis section at the end has to reconcile multiple
perspectives, which only works if reviewers brought their own.

## Why now

Clank today is sequential: one repo, one plan in flight, master and
reviewers all touching the same working tree through their own
terminal sessions. That works for "queue up plans and execute them
serially" but breaks down for:

- running an independent second plan in parallel without disturbing
  the first
- letting clank produce PRs against repos that won't tolerate
  `.clank/` in the tree
- using a second model (codex) to review claude's master work in a
  way that doesn't fight the editor's single-claude-per-cwd
  assumption

A worktree-shaped workflow is the obvious answer, but the shape of
*which* worktree workflow is wide open. Before we commit to a model
we want to know what's available, what's hard, and what's worth
copying.

## Research clusters

Every cluster below is phrased as **questions to answer**, with a
**method** line saying how. Findings land *in this document* — each
cluster gets a `### Findings` subsection appended as the work
progresses, so the final FINISHED plan body is the complete memo.

### Cluster A — Tool capability landscape

What can the underlying agent runtimes actually do?

A1. **Session forking.** Can claude code and codex both fork a
session into a new working directory while preserving conversation
state? What is the exact mechanism (CLI flag, file copy, API call)?
Are forks first-class or are we stitching together transcripts?

A2. **Multi-instance on one cwd.** Why does claude code freeze when
opened twice in the same directory? Is it a lockfile, an IPC
socket, a process-singleton check? Can it be configured around
(e.g. distinct config dirs, distinct session dirs)? What do
orchestration tools like Conductor, Aider, opencode, claude-squad
actually do — separate worktrees, separate cwds with shared
project root, or something else?

A3. **Cross-agent review on shared work.** Does the codex-reviews-
claude case work in a single shared cwd, or does it need a
worktree per agent? What breaks: file locks, editor state, hook
reentrancy? *This plan's own production is the empirical evidence*
— every commit was authored by claude in this cwd and reviewed by
codex in the same cwd, with no observed failures.

### Findings (master)

#### A1 — Session forking (cross-references F2)

The session-fork mechanics were answered while researching F2.
Summary; full detail is in Cluster F's findings:

*Revised after user correction (see F2 revision).*

| Agent       | Resume          | Fork in place   | Into a claude-managed worktree              | Into an arbitrary cwd                  | Notes |
|-------------|-----------------|-----------------|---------------------------------------------|----------------------------------------|-------|
| claude code | `--resume <id>` | `--fork-session` | **yes** via `--resume <id> -w <name>`; compose with `--tmux` for iTerm2-native or classic tmux | **no** for arbitrary user-supplied cwd (issues #58591, #28745, #28314, #30906, #42596) | `--from-pr` for PR-linked resume |
| codex CLI   | `codex resume <id>` | `/fork`       | n/a (no worktree-managed flag); use `--cd` | **yes** (`--cd <dir>`, `--add-dir`)    | Sessions in `~/.codex/sessions/YYYY/MM/DD/`; `codex resume --all` cross-cwd |

A1 verdict: fork-into-worktree is **clean for both runtimes** as
long as we go through the *supported* mechanism (claude's `-w`,
codex's `--cd`). Continuity rides along in both cases. Clank's
own artifacts (plan body, commits, feedback) remain the
durable-across-restart source of truth, but we don't have to
rely on them to compensate for missing session state.

#### A2 — Multi-instance same cwd: two orthogonal isolation problems

*Revised after codex review (REQUEST_CHANGES on d4b8c36) and
on-disk verification.*

Two claude code processes against the same `~/.claude/` *and* the
same cwd hit two distinct classes of failure. Conflating them was
the bug in the pre-revision finding. Listing them separately:

**Runtime-state collisions** — caused by sharing `~/.claude/`,
not by sharing cwd:

- `~/.claude.json` global config corrupted by concurrent writes
  (issue #28992).
- Lock acquisition at `~/.local/share/claude/versions/<v>/`
  failing across instances (issue #13287); stale lock files at
  `~/.local/state/claude/locks/` (issue #14301).
- Shell-snapshot race: snapshots live at
  `~/.claude/shell-snapshots/snapshot-<shell>-<TIMESTAMP>-<NONCE>.sh`.
  Verified on disk in this session. The naming is **timestamp +
  nonce**, *not* cwd-derived (correcting the pre-revision
  claim). The race is that instance B's periodic cleanup pass
  sees instance A's snapshots in the same shared directory and
  deletes them as "old", after which instance A tries to source
  a now-missing file (issue #4014; the issue itself recommends
  per-process state dirs as the fix).
- 2.0.61 regression: two instances loaded simultaneously can no
  longer operate in parallel — one freezes (issue #13499).
- Open RFE #19364 for a structural session-lock fix.

**Edit collisions** — caused by sharing cwd:

- Both claudes have the same files in their editing surface;
  concurrent `Edit` calls or one-edit-one-read can interleave
  unpredictably.
- One agent's `git rebase -i` blocks the other's `git status`
  view; reentrancy on user-interactive git ops is undefined.
- Hooks (post-commit, post-rewrite, etc.) fire twice and may
  fight or interleave their side effects.

The two classes are **orthogonal**. The right tool for each:

| Isolation goal           | Tool                       | Why                                                                                   |
|--------------------------|----------------------------|---------------------------------------------------------------------------------------|
| Runtime state            | `CLAUDE_CONFIG_DIR` per instance | Per docs, relocates *every* `~/.claude` path including shell-snapshots, locks, sessions, config; instance B's cleanup can't see instance A's files because they live in a different root |
| Filesystem / edit state  | git worktree per instance  | Distinct working trees so concurrent edits don't trample; distinct git index per worktree |
| Both                     | Both, together             | Required for full safety. CLAUDE_CONFIG_DIR alone leaves edit collisions; worktree alone leaves runtime-state collisions |

A2 verdict (revised): both isolations are needed for the
two-claude case, but **for different reasons** and each fixes
its own class of problem. The pre-revision claim that "cwd alone
fixes snapshot races" was wrong: cwd has no bearing on the
snapshot naming. The correct prescription is *per-worktree
CLAUDE_CONFIG_DIR + per-worktree cwd*, which the spawn command
already had to do anyway. Cluster F2's `claude --resume <id> -w
<name>` flow handles the cwd half cleanly; clank's spawn
command needs to also export `CLAUDE_CONFIG_DIR=$WORKTREE/.clank/claude-config`
(or similar) before invoking claude.

#### A3 — Cross-agent review on shared cwd: workable, with rules

Mixing **one claude + one codex** in the same cwd is materially
safer than two claudes, because the failure modes above are
internal to claude code's shared state directories and do not
touch codex's `~/.codex/` tree:

- Codex supports `--sandbox read-only`, an OS-enforced sandbox
  that prevents any writes from the codex process. A read-only
  reviewer cannot corrupt the master's work-in-progress.
- The community pattern documented in the SmartScope and
  shakacode write-ups: generate a `REVIEW_ID` per review,
  namespace temp files (`/tmp/claude-plan-${REVIEW_ID}.md`,
  `/tmp/codex-review-${REVIEW_ID}.md`). Clank's
  per-`<commit-sha>` feedback paths achieve the same isolation
  natively.
- Risks that *remain* in shared cwd: editor reentrancy (only one
  process can hold an interactive `git rebase -i` etc.), hook
  reentrancy if two agents trip the same post-write hook, and
  the user's own clarity about "which agent is touching the tree
  right now".

A3 verdict: **codex-reviewing-claude in the same cwd is the
default case clank already runs today and works**. Codex and
claude don't share `~/.claude/` (codex uses `~/.codex/`), so the
runtime-state collisions in A2 don't apply across runtimes. The
remaining shared-cwd risks are pure edit collisions, which
read-only-sandbox codex avoids by construction.

Pushing the mixed case into a worktree is convenient, not
load-bearing. The two-claude case (or two-codex, by analogy with
codex's own `~/.codex/` state) is the case worktrees are
*required* for — and even there, only for the edit-isolation
half. The runtime-state half is fixed by `CLAUDE_CONFIG_DIR` /
`CODEX_HOME` per instance, which is cheaper than a worktree.

#### Implications for the workflow

- A spawn command for a new worktree exports
  `CLAUDE_CONFIG_DIR=$WORKTREE/.clank/claude-config` and
  `CODEX_HOME=$WORKTREE/.clank/codex-home` (or similar) before
  invoking the agent. This is the **runtime-state** half. It is
  cheap (just env vars) and removes the upstream lock/cleanup
  races without needing a worktree.
- The worktree itself is the **filesystem-state** half — the
  cwd that `claude --resume <id> -w <name>` sets up.
- Plan-in-main, impl-in-worktree (B1) remains unavoidable for any
  flow where the user already has a claude master running in
  main, *but the reason is the edit half, not the runtime-state
  half*. The runtime-state half could be handled with per-session
  `CLAUDE_CONFIG_DIR` even inside one cwd — useful to know for
  cheaper "two reviewers, one tree" cases later.
- B3's flagship is still pushed toward worktree-per-PR, but the
  pressure is now correctly attributed to "two masters cannot
  share an edit surface", not to a debunked snapshot-race claim.

### Cluster B — Workflow shape

What does the lifecycle of a worktree-shaped clank session look
like end-to-end?

B1. **Plan-in-main, impl-in-worktree.** Should plans be drafted in
the main repo (where the user can read them, where stubs live) and
then "checked out" into a worktree for implementation? When the
plan finishes, do the commits return to main by merge, by
cherry-pick, or by leaving the worktree as the PR branch?

B2. **Plan ↔ PR cardinality.** Is the unit of a worktree one plan
or one PR? A PR might bundle several plans (e.g. an "umbrella"
refactor with three sub-plans). What does umbrella-style mapping
look like in the worktree? Does each sub-plan get its own commit,
its own commit chain, or a single squash at finalize?

B3. **Flagship workflow.** Pick a north-star UX. The two candidate
modes from the user:
- *solo*: queue plans, run sequentially in one tree
- *worktree-per-PR*: each worktree becomes a PR, dies on merge
What does "happy path" look like for each — what commands does the
user type, what does clank do automatically, where is the editor?
This is the deliverable the rest of clank's roadmap orients
around.

### Findings (master)

#### B1 — Plan-in-main, impl-in-worktree (with one important caveat)

Stubs and the queue live in main. Promotion either stays in main
(solo mode, no worktree) or runs into a worktree (multi-worktree
mode). The natural lifecycle:

```
stubs/  (main)        →  user authors stubs from any session
queue/  (main)        →  `clank queue add` orders & priorities
plans/  (main or wt)  →  `clank queue promote` activates
                          • solo: in main, no worktree
                          • multi-wt: in the worktree
finished/ (main)      →  `clank finish` always re-lands here so
                          main has the merged history of plans
```

The **caveat**: clank's plan-state files (the markdown body, the
finished/ dir) need to be on a branch the user wants to merge.
In multi-worktree mode the plan body is *born* in the worktree
and rides into main through the same PR as the code changes —
unless we squash it out via `clank purge --squash` (Cluster C).
The two policies — "keep `.clank/` in the merged history" vs
"squash it out before opening the PR" — are user-configurable
not architecturally fixed; Cluster C explores the trade-off.

Implication for B1: clank's worktree spawn does *not* need to
move plan files around. The plan is created in the worktree from
the promoted stub; `clank finish` in the worktree writes to the
worktree's `.clank/finished/`; on merge the file lands in main's
`.clank/finished/` (or doesn't, if the policy is squash-out).

#### B2 — Plan ↔ PR cardinality: 1:1 with deferred stacked-PR support

The 2026 stacked-PR landscape is now serious:

- **Graphite** (AI-augmented review, stack-aware merge queue,
  free CLI) is the commercial lead.
- **git-spice** is the open-source stacked-branches tool.
- **jj-spice** wraps jj users into the same workflow.
- **GitHub itself** shipped `gh stack` on 2026-04-13 (private
  preview, waitlist at `gh.io/stacksbeta`).
- The pattern documented across multiple 2026 write-ups: AI
  agent writes the code, then *stacks* it into reviewable PRs
  for a human (or another agent) to review.

Two clank cardinalities are credible:

- **v1 default — 1 plan = 1 PR**: simplest, matches the
  "worktree-per-PR" framing, and is what every prior-art article
  describes for claude code's existing `-w` flow. Each
  finalized plan becomes a PR.
- **v2 optional — umbrella plan = stacked PR set**: clank
  already has umbrella-grouping logic in the HTML render. A
  later plan can extend this to "umbrella finalize → stacked
  PRs", driven by `gh stack` (once GA) or `git-spice`. v1
  should not foreclose this — the worktree-and-finalize
  primitives need to be able to chain.

Recommendation: ship 1:1 in v1. Don't build umbrella→stack
until either GitHub ships `gh stack` GA or a user need actually
shows up; until then it's premature complexity. Watch the
landscape for 6–12 months.

#### B3 — Flagship workflow: worktree-per-PR with solo as a degenerate case

The two modes from the brief are not separate workflows — they
are the same workflow with the worktree count varied. Stating
them as one workflow simplifies the CLI surface and the mental
model:

- **solo mode** = the workflow run in the user's main cwd, no
  worktree ever spawned. Useful when the user wants one task at
  a time and doesn't care about parallel work.
- **multi-worktree mode** = the same workflow, but each plan
  promotion spawns a worktree.

A2's two-claude finding lands as a soft push, not a hard rule:
*if* the user wants parallel masters or wants to hand-drive
claude in main alongside clank's claude, they need
multi-worktree. Otherwise solo is fine.

##### Day in the life — solo mode

```sh
# user already has a clank-bound claude session running in cwd
clank queue add my-feature -m "..."     # stub → queue
clank queue promote my-feature          # active plan in this cwd
# (claude master commits, codex reviews via stop hook, iterate)
clank finish my-feature                 # finished/, ready to PR
gh pr create                            # or `clank pr` later
```

No new commands needed. This is mostly what works today.

##### Day in the life — multi-worktree mode

```sh
# user is in main repo; clank's claude master and codex reviewer
# are running here for any in-main work
clank queue add my-feature -m "..."
clank queue promote my-feature --worktree   # NEW: spawns wt
# clank runs internally:
#   claude --resume <master-id> -w my-feature --tmux
#   codex resume <reviewer-id> --cd <worktree>/
#   (export CLAUDE_CONFIG_DIR / CODEX_HOME per A2)
# user's terminal switches into the new worktree's tmux session
# (master tile focused; reviewer tile in background per F3 chrome)
# ... iterate ...
clank finish my-feature                 # in the worktree
clank pr                                # NEW: opens the PR
# after merge:
clank worktree cleanup my-feature       # NEW: prune worktree+branch
```

New surface introduced by multi-worktree mode:

- `clank queue promote --worktree` — promotes into a fresh
  worktree (uses `claude --resume -w` under the hood).
- `clank pr` — convenience over `gh pr create` that fills the
  body from the plan markdown. Optional; the user can keep
  using `gh` directly.
- `clank worktree cleanup` — prune the worktree post-merge.
- A `MuxBackend` (Cluster F) wires the new tmux/iTerm2 panes
  into the user's terminal session.

What clank does *automatically* in multi-wt mode:

- Creates the worktree via claude's own `-w` flag (not custom
  git plumbing).
- Sets per-worktree `CLAUDE_CONFIG_DIR` and `CODEX_HOME` (A2's
  runtime-state isolation).
- Spawns the reviewer (codex) inside the same worktree with
  `codex resume <id> --cd`.
- Composes the mux chrome line with role + gate state per tile
  (F3).
- On `clank pr`, fills the PR body from the plan markdown and
  optionally squashes `.clank/` out (Cluster C policy).

Where the editor lives: in multi-wt mode the canonical UX is
"clank owns the terminal session, the editor follows". The
editor opens the worktree path (via `clank open --worktree
<name>`); the agent terminals stay inside the clank-managed
tmux/iTerm2 panes. Cluster D (now mostly collapsed) covers the
fallback if the user's editor can't follow.

##### Recommendation

**Flagship = the unified workflow above.** Ship solo and
multi-wt as the same code path with `--worktree` as the toggle.
The user's day-to-day choice is "do I want to keep working in
main, or do I want to fork to a worktree for this plan?", not
"which clank mode am I in today".

### Cluster C — Repo and filesystem hygiene

How does clank coexist with a repo whose maintainers won't accept
`.clank/` upstream?

C1. **Local-only ignores.** What are the options for hiding
`.clank/` from a repo we don't own? Survey: `.git/info/exclude`,
`core.excludesFile`, skip-worktree bit, sparse-checkout, assume-
unchanged, per-worktree gitignore. Which of these survive `git
status` cleanly and which fight back?

C2. **`clank purge --squash` as the bridge.** If `.clank/` lives in
the worktree as normal tracked files, the path back into main
involves squashing them out. Is `clank purge --squash` already
doing this, or just removing in-tree state? What would a "finalize
into PR" command look like that:
- collapses the plan's commits onto a clean branch (no `.clank/`)
- pushes that branch
- opens the PR with the plan body as the description

C3. **`clank open` under worktrees.** Today `clank open` tells the
editor how to open the project. With worktrees, the answer
branches: open the main repo? the active worktree? both? How does
that interact with emacs (current user), VS Code (windows-per-
folder), JetBrains (project-per-window)? What's the minimum viable
spec?

### Findings (master)

#### C1 — Local-only ignores: the cleanest path is "never tracked"

Git's four local-only ignore mechanisms, ranked by fit for the
"don't push `.clank/` upstream" goal:

| Mechanism                       | Scope                   | Works on tracked files? | Survives `checkout` / `pull`? | Fit for `.clank/`         |
|---------------------------------|-------------------------|-------------------------|------------------------------|---------------------------|
| Repo `.gitignore` (committed)   | Repo-wide               | No (untracked only)     | Yes                          | **Best when accepted**    |
| `.git/info/exclude`             | Per-clone (shared across worktrees) | No (untracked only) | Yes                | Best when repo `.gitignore` is rejected |
| `core.excludesFile` (global)    | All repos for this user | No (untracked only)     | Yes                          | Useful as a user-wide net |
| `update-index --skip-worktree`  | Per-file                | Yes                     | Auto-unsets on upstream change | Wrong tool (per-file, fragile)   |
| `update-index --assume-unchanged` | Per-file              | Yes                     | Errors on branch checkout    | Wrong tool (performance hint, not ignore) |

The two takeaways:

- **`.gitignore` and `.git/info/exclude` only work for untracked
  files.** Once `.clank/` is committed upstream by anyone, those
  mechanisms can't help — the path is tracked and the worktree
  is forced to materialise it. The recovery path is *not* a flag
  flip; it's a history rewrite.
- **`skip-worktree` and `assume-unchanged` are not the right
  tool** even though they sound close. The first auto-unsets on
  upstream change; the second is documented as a performance
  hint that errors on branch checkout. Either would fail under
  normal git operations the user runs every day.

Recommendation:

- **Tier 1 — repo accepts `.clank/` in `.gitignore`**: commit it
  to the repo's `.gitignore`. Cleanest, no per-user setup.
- **Tier 2 — repo will NOT carry the ignore entry**: append
  `.clank/` to the local clone's `.git/info/exclude`. Per-user,
  no upstream artifact. Verify the user has never `git add`ed
  `.clank/` first. *Today this is a manual one-shot* (`echo
  '.clank/' >> "$(git rev-parse --git-common-dir)/info/exclude"`);
  **proposed**: extend `clank setup` (or a new `clank init
  --local-ignore`) to do this for the user. The current `clank
  setup --help` only documents user-scope installation
  (~/.claude, ~/.codex), so this is net-new repo-scope work,
  not just renaming an existing flag.
- **Tier 3 — `.clank/` is already tracked upstream**: a one-shot
  `clank purge --all --into-branch wipe-clank` rewrites history
  to remove all `.clank/` paths. The user then pushes that branch
  and asks maintainers to switch. This is the "rip the bandaid"
  case.

Note on per-worktree scope: `info/exclude` lives in
`$GIT_COMMON_DIR/info/exclude` — the main repo's `.git/info/`,
which linked worktrees share (each linked worktree has a private
`$GIT_DIR` at `<main>/.git/worktrees/<name>/` but `$GIT_COMMON_DIR`
points back to the main `.git/`). So `info/exclude` is repo-wide,
not per-worktree. That's fine for our case — we want `.clank/`
ignored everywhere — but worth recording.

#### C2 — `clank purge --squash` is the load-bearing PR primitive

`clank purge --help` (read locally) already exposes everything
the finalize-to-PR flow needs:

- `--squash "<msg>"` collapses the plan-attributed range into a
  single commit.
- `--all` strips *every* `.clank/` path (plans, finalize
  snapshots, queue, the `.clank/.gitignore`).
- `--into-branch <name>` writes the rewritten chain to a fresh
  branch instead of touching the current one — strictly safer.
- `--dry` previews without mutating, `--yes` skips the prompt.
- `--amend` for amending a HEAD finalize commit in place.

What this means for the PR finalize flow (real commands today,
no fabricated flags). Two ordering gotchas need calling out
explicitly:

1. `clank purge --into-branch pr-<plan-name>` writes the
   rewritten chain to a fresh branch ref but **does not check it
   out**. The user's current branch stays selected.
2. The squash strips `.clank/finished/<plan>.md` from the
   rewritten branch, so the plan body has to be captured *before*
   the rewrite (or read from the pre-purge branch).

Given those two, the correct sequence is:

```sh
# in the worktree, after `clank finish`, on the worktree branch
PR_BODY="$(cat .clank/finished/<plan>.md)"
clank purge --squash "<plan-title>: <one-line>" \
            --into-branch pr-<plan-name>
git push -u origin pr-<plan-name>
gh pr create --head pr-<plan-name> --body "$PR_BODY"
```

Four commands. The capture-first / push-rewritten / pass-`--head`
shape is exactly the kind of thing a wrapper should hide. A
**proposed** `clank pr` does all four, defaults the squash
message to the plan title, and reads the body from the
pre-purge tree before invoking the rewrite. No new purge code
needed — the rewrite primitive is already shippable.

The `clank log` surface (which exposes `--json`, `--oneline`,
`--plan`, no `--format=pr-body`) could grow a PR-body formatter
later, but it doesn't have to for v1: the captured plan markdown
covers the common case.

Open question for the user: should `clank pr` *always* squash
`.clank/` out, or should it offer a `--keep-clank` flag for
users in clank-friendly repos (Tier 1 above)? The default of
"strip" is the safer surprise.

Open question for the spec: when umbrella plans land in v2
(B2), the "plan-attributed range" gets ambiguous — do we squash
per-sub-plan into a stack, or roll them all? Defer until v2;
flag it here so future planning doesn't miss it.

#### C3 — `clank open` under worktrees: minimum viable spec

Read `clank open --help`: it takes a path, classifies what's at
that path (no-repo / git-without-clank / clank-initialised), and
returns structured data. **Read-only, no editor invocation.**
That design already composes with worktrees without changes —
a worktree path is just a path, and the editor reads the
structured result to decide what to do.

So `clank open` itself does not need worktree awareness. What's
missing is an *enumeration* command for editors that want to
present a picker:

- `clank worktree list` — return the active worktrees for this
  clank repo (name, path, branch, current plan, gate state). The
  editor calls `clank open <picked-path>` after the user picks.
- The current-worktree resolution is the same as git's: derived
  from cwd via `git rev-parse --show-toplevel`. No new resolver
  needed.

For specific editors:

- **emacs** (current user): the `clank open` JSON already feeds
  the user's existing emacs hook. The new `clank worktree list`
  command lets emacs present a picker before calling `clank open
  <path>`. No change to `clank open`'s contract.
- **VS Code / JetBrains** (windows-per-folder / project-per-
  window): each worktree opens as its own window. The editor
  consumes `clank worktree list` for the picker, then opens the
  picked path natively. clank doesn't need editor-specific code.

C3 verdict: **no change to `clank open` required**. Add a
sibling `clank worktree list` so editors can present a picker;
that single command is enough to make the worktree flow
ergonomic across emacs, VS Code, and JetBrains.

### Cluster D — Terminal and window orchestration (fallback path)

This cluster is the *fallback* if Cluster F's multiplexer doesn't
pan out. If F succeeds, clank owns the terminals and most of D
collapses — D2 disappears entirely, D3 reduces to "the TUI manages
session lifecycle". D is only load-bearing in the F5 world where
we delegate to the editor or to tmux.

D1. **Delegation shape.** Assuming clank does not ship its own
multiplexer, who is responsible for spawning the agent terminals
when a worktree opens? Options:
- delegate entirely (user opens new emacs frames / iTerm tabs)
- clank emits a hook (`clank open --new-worktree`) that the editor
  implements
The user is on emacs; what does an emacs binding look like?

D2. **Agent-pair binding.** The current emacs setup leaves the
codex and claude buffers as independent windows — easy to lose one,
no "which one is active" affordance. Design options:
- single buffer that shows the currently-active agent (master if
  none active)
- always-paired side-by-side
- "agent stack" with focus follows gate state

D3. **Spawn cost and lifecycle.** When does an agent terminal
appear and when does it die? A worktree might exist for a week of
reviews; the agent terminal might not. Does `clank wfw` re-attach
to an existing session, spawn fresh, or refuse? What's the
contract?

### Findings (master)

This cluster largely collapses now that F5 settled on the tmux
backend (with `claude --tmux` invoking iTerm2 native panes when
available). The pre-revision D framed itself as "the fallback if
F1 fails"; F1 didn't fail, so most D questions are no longer
load-bearing. Recording the residual answers for completeness:

#### D1 — Delegation shape

Subsumed by F5. The chosen model is **clank delegates through
claude's own multiplexer integration**: `clank queue promote
--worktree` invokes `claude --resume <id> -w <name> --tmux`,
which handles the PTY/spawn/tmux/iTerm2 work natively. Clank
adds the reviewer tile (`codex resume --cd <worktree>` into the
same tmux session) and the chrome line (F3).

The user's editor (emacs, VS Code, JetBrains) opens the worktree
path independently via `clank open <worktree-path>`. The agent
terminals stay inside the tmux session — they're not embedded in
the editor. This matches claude code's own opinion that the
terminal is the agent's home.

For emacs users who prefer in-editor terminals, `vterm` or `eat`
can attach to the clank-managed tmux session (`tmux attach -t
<session-name>`) inside an emacs buffer. That's a user-side
configuration, not a clank-side feature.

#### D2 — Agent-pair binding

Subsumed by F5. The binding *is* the tmux session: master and
reviewer panes live in the same session and switch with the
hotkey. There is no need for a separate "agent-pair" concept —
"the panes in this tmux session" is the pair.

The pre-revision design options (single-active-buffer, always-
paired, focus-follows-gate) become user-side tmux config
choices, not clank concerns. Recommended default: `claude
--tmux` opens one window per agent (claude=window 1,
codex=window 2), user switches with `prefix-1` / `prefix-2`,
chrome indicates which one wants attention.

#### D3 — Spawn cost and lifecycle

The tmux session itself outlives both the agent terminals and
`clank wfw`. Concrete contract:

- `clank queue promote --worktree` creates the worktree and the
  tmux session if they don't exist, then spawns
  `claude --resume <id> -w <name> --tmux` and `codex resume
  <reviewer-id> --cd <worktree>` inside it.
- `clank wfw` re-attaches to the existing tmux session — does
  not spawn a new one. If the session is missing (user killed
  it), `clank wfw` exits with an instruction to re-promote or to
  manually attach.
- `clank worktree cleanup <name>` (B3) tears down the tmux
  session, the worktree, and the local branch.

So spawn happens once at promote-time; cleanup happens explicitly
at merge-time. The middle is the user re-attaching as needed,
which is the standard tmux story.

D verdict: no fallback path needed. Cluster D is fully subsumed
by F5's choice. Marking complete.

### Cluster E — Strategic

E1. **Critical assessment.** Honestly, does clank serve a purpose
alongside Conductor, claude-squad, opencode-orchestrate, plain
git-worktree + tmux, etc.? Where does clank's peer-review-as-gate
model give a unique edge, and where is it reinventing a wheel?

### Findings (master)

#### E1 — Clank's reason to exist is the gate, not the orchestration

**Where clank is unique.** None of the surveyed tools have what
clank's peer-review loop produces:

- **A typed verdict state machine.** `APPROVE`,
  `REQUEST_CHANGES`, `FINISHED` aren't comments — they drive a
  waiting-on field, a gate state, and the stop-hook
  continuation logic. claude-squad and Conductor isolate
  parallel sessions; they don't have a notion of "this commit is
  pending one approval before master can finalize". The 2026
  orchestrator survey (Shipyard, ccpm, Mission Control, etc.)
  doesn't surface this primitive either.
- **Plan as a durable, multi-commit artifact.** Other tools'
  unit of work is "a session" or "a workspace". Clank's is "a
  plan" — a markdown body that accumulates commits and
  feedback across many turns, eventually landing in
  `.clank/finished/`. That gives clank an artifact-driven
  continuity model that works *even when sessions can't be
  forked* (e.g. claude code's arbitrary-cwd resume gap from A1).
- **Master/reviewer role split with role-aware stop hooks.**
  Stop hook continuations differ by role: master gets
  "continue/finish/revise" prompts, reviewer gets "review
  this commit at <sha>" prompts. The other orchestrators run
  agents in parallel; they don't structure the handoff between
  roles.

**Where clank overlaps with prior art (and shouldn't compete).**

- **Session isolation via git worktrees.** claude-squad does
  this. claude code's `-w` does this. Conductor does this.
  Clank should *use* claude's `-w` (per F2/B3) and not build
  parallel worktree plumbing.
- **Terminal multiplexing.** tmux exists; claude code's `--tmux`
  drives it; claude-squad delegates to it. Clank should do the
  same (per F5). Building a custom TUI puts us in the same lane
  as r3bl_tui PTYMux without a reason to win.
- **PR creation, merge orchestration.** `gh` does this. Clank's
  `clank pr` is a thin capture-body-then-squash-then-create
  wrapper (per C2) and that's all it should be. Not a merge
  queue, not a CI orchestrator.
- **Generic "parallel agent" runner.** Conductor and claude-squad
  do this. If a user wants 5 unrelated tasks running in
  parallel, those tools serve that need better than clank
  could. Clank's parallelism is in service of *one peer-review
  loop per plan*, not generic fan-out.

**Honest scope recommendation: narrow inward.**

Clank should stop short of competing with the orchestrator
landscape. Specifically:

1. **Don't build a bespoke PTY/VT multiplexer.** Delegate to
   tmux (via `claude --tmux`) for v1; keep r3bl_tui::PTYMux as
   a spike behind a `MuxBackend` trait. The trait is a thin
   abstraction *over existing libraries*, not custom PTY/VT
   code. Already settled in F5.
2. **Don't build a queue scheduler.** The existing priority
   queue is fine for ordering; resist features that turn it
   into a job runner.
3. **Don't build agent-pool management.** A clank plan is a
   *single* gate-driven loop, not a swarm. The "claude pool"
   pattern (community claude-pool daemon, etc.) is a different
   product.
4. **Don't build merge orchestration.** Defer to `gh stack` /
   Graphite when stacked PRs become relevant (B2 v2).

**Where clank *should* invest:**

- Make the gate state machine richer (umbrella plans → stacked
  PRs, concurrent review cycles per the queued `concurrent-
  review-cycles` stub, parallel plan reviews per
  `parallel-plan-reviews` stub).
- Improve the chrome surface (F3) so the gate state is visible
  without switching panes.
- Tighten the artifact model so plans carry their full review
  history into `.clank/finished/` and the HTML render.
- Better cross-agent handoff: explicit "the reviewer
  REQUEST_CHANGES'd X, here is the structured patch suggestion"
  rather than free-text feedback.

**The one-sentence positioning.** Clank is *peer-review
infrastructure for AI coding agents*. It lives one layer above
claude code / codex (which provide the agent sessions and the
worktree primitives) and one layer below an orchestrator like
claude-squad (which provides parallel session management for
fundamentally independent tasks). Its reason to exist is the
*gated, multi-agent, peer-review loop on a single plan*. Any
feature outside that scope is overreach.

### Cluster F — Clank as agent-multiplexer TUI (the linchpin)

**Do this cluster first.** Its answer changes the rest of the
design more than any other question here.

If clank itself owns a small TUI that spawns and multiplexes the
agent sessions — claude, codex, any third reviewer — the worktree
workflow becomes ergonomic in a way nothing else makes it. Opening
a worktree is no longer a window-management dance; it is one
command that forks the active sessions (A1) into the new tree and
hands the user a hotkey to switch between them. The TUI shows
exactly one agent at a time (no split-screen complexity), keeps
the others alive in memory, and surfaces background activity in a
chrome bar. The hidden agents keep working; the user just isn't
looking at them.

If this is feasible, Cluster D collapses, D2 (agent-pair binding)
becomes a non-question, and `clank open` under worktrees (C3) gets
much simpler. If it is *not* feasible, the worktree workflow has
to be designed around an external multiplexer (tmux) or editor
binding (D fallback) and the UX gets meaningfully worse.

F1. **Is PTY-multiplexing in a Rust CLI tractable?** What does it
take to spawn N child processes (`claude`, `codex`, ...) each in
their own pseudo-terminal, forward stdin/stdout to whichever one
the user has focused, and keep the others alive with their output
buffered? Specifically:
- PTY crates: `portable-pty`, `nix::pty`. What's the API cost?
- ANSI/VT parsing for the hidden tiles so their screen state can
  be redrawn on switch: `vt100`, `alacritty_terminal`, fragments
  of `wezterm-term`.
- Raw-mode and redraw on the user's real terminal: `crossterm`,
  `ratatui` for chrome.

F2. **Spawn-into-worktree flow.** Once the multiplexer exists,
"open a new worktree" should be: `clank worktree new <name>` →
clank creates the git worktree, forks (or resumes) the current
master + reviewer sessions inside it (A1), spawns them as new
tiles in the TUI, hands focus to the new master tile. What does
that command surface look like end-to-end? What state has to be
copied from the source sessions to the forks — cwd, conversation
history, env, agent role binding?

F3. **Background-activity surfacing.** The user is looking at the
master tile; codex is mid-review on a different worktree. How does
the user discover that codex wants attention? Options:
- chrome bar with per-tile indicators (`*` for new output, `!` for
  stop-hook fire, role + gate state visible)
- transient notification line when a hidden tile's gate flips
- bell on any output (probably too noisy)

F4. **Existing tools that already do this.** Survey: claude-squad,
Conductor, opencode, aider, embedding zellij as a library,
embedding wezterm-mux. For each: how close is it to what we want,
what's the integration cost, is "shell out to it" or "use it as a
library" cheaper than rolling our own?

F5. **Graceful degradation.** If F1 turns out to require months,
what is the cheapest fallback that gives a *near-equivalent* UX?
Likely: a shipped tmux config plus emacs binding that together
emulate the switch-between-tiles experience, with the CLI surface
unchanged. Verify that the rest of the worktree workflow can be
designed against an abstract "agent terminal" interface so we can
swap implementations without re-shipping plans.

### Findings (master)

#### F1 — PTY multiplexing in Rust: cheaper than first reported

*Revised after codex review (reviewer-supplied contradictory
evidence on r3bl_tui).*

Building a native multiplexer is **technically possible at two
very different price points**:

- **Library path (cheap, surprising):** `r3bl_tui` v0.7.8 (2026-01-23)
  exposes `r3bl_tui::core::pty_mux` as a **public** module with
  `PTYMux`, `PTYMuxBuilder`, `Process`, `ProcessManager`,
  `InputRouter`, `OutputRenderer`, and the constants
  `MAX_PROCESSES = 9` and `STATUS_BAR_HEIGHT`. The documented
  usage example is:

  ```rust
  let processes = vec![
      Process::new("bash",    "bash", vec![], terminal_size),
      Process::new("editor",  "nvim", vec![], terminal_size),
      Process::new("monitor", "htop", vec![], terminal_size),
  ];
  let multiplexer = PTYMux::builder()
      .processes(processes)
      .build()?;
  multiplexer.run().await?;
  ```

  That is almost verbatim what a `clank tui` entry point would
  call, substituting `claude` and `codex`. F1–F9 hotkey switching
  is built in. Each process gets an `OffscreenBuffer` that
  receives ANSI output through a vt-100 parser, so hidden tiles
  preserve screen state and switch instantly.

  My earlier WebFetch summary missed this — codex's independent
  research caught the mismatch. The reviewer note's directive to
  do independent research paid for itself on the very first
  review.

- **Primitives path (expensive):** Build on `portable-pty` +
  `vt100` / `alacritty_terminal` + `crossterm`/`ratatui` directly.
  Working open-source proofs of this pattern exist (`term39`,
  `croft`, `wtmux`, `psmux`), each itself a multi-thousand-line
  project. Weeks of focused work plus ongoing rendering-fidelity
  maintenance as the agent CLIs evolve.

Caveats on the library path:

- r3bl_tui is **pre-1.0 (0.7.8)** with no explicit stability
  claim and a single maintaining organisation (r3bl-org). A
  breaking change in a minor version is plausible.
- `MAX_PROCESSES = 9` is well above our master+reviewers use case
  (typically 2–4) but worth noting.
- Pulls in 48+ transitive deps (tokio, crossterm, portable-pty,
  vte, serde, syntect, ...). Clank's existing dep graph already
  overlaps most of these.
- A spike is still needed to confirm `claude` and `codex`
  themselves render correctly inside `r3bl_tui`'s OffscreenBuffer
  — claude code in particular does heavy interactive UI work
  that may exercise corners of the VT-100 parser that simpler
  workloads don't.

Verdict on F1 alone: the *library path* changes the answer from
"feasible but expensive" to "feasible and cheap *if the spike
passes*". The spike (host two interactive shells, then host
`claude` + `codex`, confirm no visible corruption) is now the
gating sub-task, not the multiplexer architecture itself.

#### F4 — Prior art makes the call obvious: delegate to tmux

The closest existing tool is **claude-squad** (smtg-ai), and its
architectural choice is the single most informative data point in
this whole research plan:

| Tool          | Language | Multiplexer       | Isolation     | Notes                                 |
|---------------|----------|-------------------|---------------|---------------------------------------|
| **claude code itself** | TS | **tmux + iTerm2 native panes** (built-in via `--tmux` on `--worktree`) | git worktree | Designed-in worktree flow; informs F5 |
| claude-squad  | Go       | **tmux** (delegated) | git worktree | Multi-agent, hotkey-switch, sessions  |
| Conductor     | Swift / macOS | macOS GUI (own) | own workspaces | macOS-only, handles PR/merge, paid    |
| r3bl_tui PTYMux | Rust   | **own (public lib)** | n/a (lib)   | Documented `core::pty_mux` API, F1–F9 hotkey switch, OffscreenBuffer per tile, 0.7.8 / Jan 2026 |
| zellij        | Rust     | own               | n/a (mux)     | WASM plugin system; community is building claude-code plugins for zellij |
| Shipyard etc. | varies   | varies            | varies        | 6+ orchestrators surveyed for 2026    |

Two viable hosts emerge, not one:

- **claude-squad's pattern (tmux + worktrees + thin TUI driver).**
  Most relevant fact in the table: claude-squad faced almost
  exactly our problem statement and chose tmux. They got hotkey
  switching, session isolation, parallel agent execution, and
  worktree binding without writing PTY/VT code. Maturity and
  ubiquity arguments below.
- **r3bl_tui PTYMux (in-process, pure-Rust embed).** The library
  path from F1. The selling point versus tmux: no external runtime
  dep, no IPC dance, native control of the chrome and the
  spawning semantics. The risk versus tmux: pre-1.0, single
  upstream maintainer, no widely-reported track record with the
  agent CLIs.

Zellij is a more distant third — Rust, has a WASM plugin runtime,
and a community plugin for claude-code orchestration is under
construction. Embedding as a plugin host is heavier than driving
tmux and gives us a comparable result; it's worth tracking but
not leading with.

#### F2 — Spawn-into-worktree is a first-class flow in claude code

*Revised after user correction: I read the issue tracker too
narrowly and missed the canonical `-w` flag. Verified against
`claude --help` on the installed CLI (June 2026).*

claude code ships **first-class flags for the session-into-
worktree dance**:

- `-w, --worktree [name]` — "Create a new git worktree for this
  session (optionally specify a name)". Composes with `--resume`
  and `--fork-session`: `claude --resume <id> -w <name>` lands an
  existing session inside a fresh worktree; `--fork-session -w
  <name>` does the same with a fork copy of the conversation.
- `--tmux` — "Create a tmux session for the worktree (requires
  --worktree). Uses iTerm2 native panes when available; use
  --tmux=classic for traditional tmux." So claude code's *own*
  designers picked tmux (with iTerm2-native-panes promotion) as
  the multi-window UX for the worktree flow.
- `--from-pr` — resumes a session linked to a PR by
  number/URL. Useful for the PR-review use case in Cluster B.

The earlier finding (referencing issues #58591, #28745, #28314,
#30906, #42596) was about a *different* scenario — resuming into
an arbitrary user-supplied cwd that claude does not own. That
limitation still holds for "I want to point an existing session
at any directory on disk", but it does **not** block the worktree
spawn flow because `-w` is the supported path: claude creates the
worktree and binds the session to it in one step.

For codex, `codex resume <id> --cd <dir>` already supported the
arbitrary-cwd case, as previously noted.

Net for the worktree workflow:
- Master (claude) spawn-into-worktree is **clean** via `claude
  --resume <id> -w <name>` (or `--fork-session` to branch). Plus
  `--tmux` gives a multiplexed UX for free, with iTerm2-native
  panes auto-detected.
- Reviewer (codex) spawn-into-worktree is **clean** via `codex
  resume <id> --cd <worktree>`.
- The "conversation history doesn't ride along" pessimism in the
  pre-revision finding was wrong. Both runtimes preserve history
  through their resume mechanisms when targeting a worktree they
  manage.

This is a strict upgrade to F2 and reshapes the F5 recommendation
(see below).

#### F3 — Background-activity surfacing is the waiting-on field

There is no separate "activity" model to design. The signal is
already canonical and clank already computes it: `clank wfw`'s
`waiting on` field per plan. Each agent tile shows whether *that
agent* is currently in the waiting-on set. When the gate flips
(REQUEST_CHANGES posted, master moves to revise, reviewer's turn
to weigh in), the waiting-on field changes; the tile labels
update from the same source.

Concretely:

- Each agent tile displays its plan + role + whether-it's-
  waiting-on-me, rendered into whichever chrome surface the
  terminal exposes (tmux `pane-border-format`, iTerm2 session
  title, etc.).
- Source of truth: poll `clank wfw --json` (or have clank push
  the state when it changes — TBD in the implementation stub).
  No parsing of agent output streams, no terminal-emulator
  activity-monitor, no OSC-1337 dance.
- The "you have work waiting" prompt the user sees is the same
  prompt `clank wfw` already serves to the agents. Same logic,
  rendered in a different place.

F3 verdict: not a research question, a rendering question.
Whatever the v1 backend is (tmux), clank reads `wfw` state and
formats it into the tile chrome. No new state machine.

#### F5 — Recommended choice: two viable backends, ship behind a trait

Given F1 (one library path is cheap if the spike passes; the
primitives path is weeks), F4 (claude-squad demonstrates tmux
works at scale; r3bl_tui PTYMux is a real public-API alternative),
and F2 (the worktree session-fork story is constrained by upstream
and not unblocked by *either* multiplexer), the recommended path:

- **Design clank to a `MuxBackend` trait** that captures the
  minimal surface: spawn-tile-with-process, focus-tile,
  emit-status-line, kill-tile, attach-existing. Both backends
  below have to implement it. The trait is the contract that
  keeps the worktree workflow stable across backends.

- **Backend A — tmux driver (recommended for v1).** Lowest risk.
  Maturity, ubiquity, *and a working precedent in claude code
  itself* (the `--tmux` flag on `--worktree`, which auto-prefers
  iTerm2 native panes) all favour it. Two corollaries the
  pre-revision recommendation underweighted:
  1. Clank may not need to ship a tmux config at all for the
     master tile — `claude --resume <id> -w <name> --tmux` does
     it natively. Clank wraps the *outer* tmux session and adds
     reviewer tiles + chrome.
  2. On macOS+iTerm2 the user gets *native iTerm2 panes* instead
     of in-terminal tmux UI, for free. The chrome story has to
     handle both — iTerm2 panes need their indicators set via
     iTerm2's escape sequences, tmux panes via the status line.

- **Backend B — r3bl_tui::PTYMux driver (recommended as a
  parallel spike, candidate for v2).** Pure-Rust, in-process,
  removes the external tmux dependency, and gives clank direct
  control of the chrome and spawning semantics. Pre-1.0 + single
  upstream is the only real blocker; a spike that hosts `claude`
  + `codex` for an hour of real use will tell us whether the
  rendering holds up. If it does, this becomes the v2 default
  and tmux is kept as a fallback backend for users who prefer it
  or who want shared tmux sessions across non-clank workflows.

- **Not recommended for now:** rolling our own PTY+VT
  multiplexer from primitives. With r3bl_tui::PTYMux on the
  table, "build from `portable-pty` + `vt100`" stops being a
  serious option — it's strictly more work for the same UX.
  Zellij-as-host stays on the watch list but doesn't lead.

This is a *reframe* of what the user asked: clank still spawns
the TUI, still hands the user a hotkey, still keeps the inactive
agents alive, still ties one terminal to one worktree. v1 ships
tmux-backed; v2 may flip to r3bl_tui::PTYMux based on spike
results. The CLI surface stays the same across the two.

The remaining open questions for the user:

- Are we comfortable shipping v1 with a hard tmux dependency, or
  do we want to gate v1 behind the r3bl_tui spike landing first?
- For the emacs binding: tmux session *embedded in emacs* (via
  `vterm` / `eat`), or emacs calling out to a real terminal app
  holding the tmux session? This decision feeds Cluster D.

## Deliverable

This plan body, FINISHED. Each cluster gets a `### Findings`
subsection added in-place. Cluster F is the gating cluster — its
Findings should land first, because its answer determines whether
Cluster D matters at all and reshapes B3's flagship workflow.

The plan gains a final `## Synthesis` section before FINISHED,
containing:

1. **F verdict.** Native multiplexer feasible? If yes, what does
   the spike look like and what's the build estimate. If no,
   which F5 fallback we are committing to.
2. **Recommended flagship workflow** (B3 answered), assuming the F
   verdict.
3. **Required new clank commands / hooks** distilled from the
   answers across all clusters.
4. **Open questions to take back to the user** before any
   implementation stub is promoted.

Once `clank finish` lands this in `.clank/finished/`, the synthesis
section is what follow-on implementation stubs cite.

## Out of scope

- Writing any production code in this plan.
- Picking final command names — those follow design.
- Building the emacs binding — that's its own plan.

## Synthesis

This is the final section the Deliverable promised: F verdict
first (since it gates everything else), then the recommended
flagship workflow, then the distilled list of new clank
commands/hooks, then the open questions to take back to the
user before any implementation stub is promoted.

### 1. F verdict

**No bespoke PTY/VT multiplexer.** This is the *shape* of the F
verdict, and it matches E1's "narrow inward" recommendation
exactly. Clank does not write its own PTY layer, its own VT
parser, or its own raw-mode renderer. Whichever backend clank
runs against is one *we already get for free* from an existing
library (r3bl_tui) or external process (tmux). The `MuxBackend`
trait introduced in F5 is a thin abstraction *over those
libraries*, not custom code.

**Native (in-process) backend is feasible.**
`r3bl_tui::core::pty_mux` exposes a public `PTYMux` + `Process`
API with F1–F9 hotkey switch, per-process OffscreenBuffer, and
an in-docs example that maps almost verbatim to `claude` +
`codex` invocations. F1 is settled in our favour — the
*library* path is cheap. Building from `portable-pty`
primitives is off the table.

**v1 backend recommendation: tmux, via claude code's own
integration.** `claude --resume <id> -w <name> --tmux` already
creates the worktree, opens the tmux session, and prefers iTerm2
native panes when available. clank wraps this by adding the
reviewer tile (`codex resume --cd <worktree>` in the same
session) and the chrome line (F3). Tmux dependency caveat: v1
*requires tmux on the user's PATH*. The implementation has to
ship presence detection and a clear "install tmux" path; see
§3 for the stub and §4 for the user-policy question.

**v2 candidate: r3bl_tui::PTYMux native backend.** A short spike
hosting `claude` + `codex` inside `PTYMux`'s OffscreenBuffer
will tell us whether the rendering holds up. If it does,
`r3bl_tui::PTYMux` becomes the v2 default; tmux remains a
configurable backend for users who want shared sessions across
non-clank workflows. Either backend lives behind `MuxBackend`,
so the choice is configurable not architectural.

### 2. Recommended flagship workflow

**Worktree-per-PR with solo as the degenerate case (B3).** Solo
mode is the same workflow with zero spawned worktrees; multi-wt
mode spawns one per plan promotion. Day-in-the-life sketches in
B3 show the user-facing surface for each. The two are exposed
as a single CLI surface with `--worktree` as the per-plan
toggle.

The flagship is constrained by two findings the rest of the
research surfaced:

- **A2's isolation matrix** — per-instance `CLAUDE_CONFIG_DIR` +
  `CODEX_HOME` plus per-instance cwd (worktree). Both halves
  needed for the two-master case; the env half is also useful
  for "two reviewers in one tree".
- **F2's worktree integration** — claude code's `-w` is the
  supported session-into-worktree mechanism. The arbitrary-cwd
  resume gap (#58591) doesn't bite this flow.

### 3. Required new clank commands / hooks

Across all clusters, the surface that needs to land:

| Command                                  | Purpose                                                                          | Source cluster |
|------------------------------------------|----------------------------------------------------------------------------------|----------------|
| `clank queue promote --worktree`         | Spawn a worktree via `claude -w`, set `CLAUDE_CONFIG_DIR`/`CODEX_HOME`, promote the plan, spawn master+reviewer tiles | B3, F2, A2 |
| `clank pr`                               | Capture body → `clank purge --squash --into-branch` → push → `gh pr create --head`. Wraps C2's four-step sequence | B3, C2 |
| `clank worktree list`                    | Enumerate worktrees + per-worktree plan/gate state for editor pickers            | C3            |
| `clank worktree cleanup <name>`          | Tear down tmux session, worktree, local branch                                   | B3, D3        |
| `clank chrome` *(internal)*              | Render `clank wfw`'s waiting-on state into each tile's chrome surface (tmux pane title, iTerm2 session title, etc.) | F3 |
| **Net-new**: trait `MuxBackend` (Rust)   | Abstract spawn-tile / focus-tile / emit-status / kill-tile / attach across tmux + r3bl_tui::PTYMux backends | F5            |

Implementation stubs that should be queued *after* this plan
finishes:

- **`worktree-spawn-and-env`** — implements `clank queue
  promote --worktree`, the env-var export, the claude `-w` call,
  and the codex `--cd` call.
- **`muxbackend-trait-and-tmux-driver`** — defines the trait,
  ships the tmux driver as the v1 default. Includes the F3
  chrome emitter.
- **`muxbackend-r3bl-spike`** — proves (or disproves) the
  r3bl_tui::PTYMux backend by hosting `claude` + `codex` and
  observing the render fidelity over real use.
- **`clank-pr-wrapper`** — implements the four-command capture+
  squash+push+create sequence behind a single command, with
  `--keep-clank` for clank-friendly repos.
- **`clank-worktree-list`** — read-only enumeration for editor
  pickers; powers the C3 emacs/VS Code/JetBrains integration.
- **`local-ignore-setup`** — extend `clank setup` (or add
  `clank init --local-ignore`) to write `.clank/` into
  `$GIT_COMMON_DIR/info/exclude` for repos that won't carry the
  ignore upstream.
- **`tmux-presence-detection`** — `clank doctor` learns to flag
  missing `tmux` for users opting into `--worktree`. The CLI
  refuses `clank queue promote --worktree` with a clear "install
  tmux: brew install tmux / apt install tmux / ..." message
  when `tmux` is absent from `PATH`. Real concern: this very
  research environment does not have `tmux` installed (verified
  by `which tmux` returning 127), so the assumption "every clank
  user has tmux" is false out of the gate. Whether clank should
  *install* tmux itself or only *detect+document* is a §4
  question.

The order matters: `worktree-spawn-and-env`, `muxbackend-trait-
and-tmux-driver`, and `tmux-presence-detection` are the
foundation; the spike, the PR wrapper, the list command, and
the local-ignore setup can land in parallel after them.

### 4. Open questions back to the user

These need user input before any of the implementation stubs
above get promoted:

- **Flagship scope.** B3 recommends one unified workflow with
  `--worktree` as the toggle. Confirm: do you want both modes
  shipped together, or just multi-wt with solo as a
  documentation footnote?
- **r3bl_tui spike.** Recommended as a v2 candidate. Confirm:
  proceed with the spike alongside v1, defer until users push
  back on the tmux dependency, or skip entirely?
- **`.clank/` ignore policy default.** Should `clank pr`
  *always* squash `.clank/` out of the PR branch (Tier 1
  default), or offer `--keep-clank` for clank-friendly repos
  with the safer default being strip?
- **tmux installation policy.** v1's multi-worktree mode
  requires `tmux` on `PATH`. This research env doesn't have it.
  Should clank: (a) detect and refuse with a "please install
  tmux" message, (b) attempt to install via the user's package
  manager when missing, or (c) ship an embedded tmux build?
  Recommendation is (a) for v1 — fewest surprises — but the
  user picks.
- **Editor binding scope.** Recommendation in D1 is that clank
  delegates to `clank open` / `clank worktree list` and the
  editor follows. Confirm: do you want a clank-side emacs
  integration shipped, or is the JSON contract enough for the
  user's existing hook?
- **Critical assessment licence (E1).** The synthesis takes E1's
  "narrow clank inward" recommendation literally — no
  multiplexer, no queue scheduler, no agent-pool management,
  no merge orchestration. Confirm this scoping or push back.
- **Out-of-scope items the research touched but didn't resolve.**
  Umbrella plans → stacked PRs (B2 v2), concurrent review
  cycles per the existing stub, parallel plan reviews per the
  existing stub. These should be their own plans once stacked-
  PR tooling stabilises.

When the user answers these, the implementation stubs in §3 can
be promoted in the recommended order. Until then, this plan is
the design memo their answers operate on.

