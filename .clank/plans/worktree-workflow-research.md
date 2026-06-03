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
- Method: read claude code docs (`claude-code-guide` agent) +
  codex docs / source; verify with a small spike that forks a
  session into a worktree and confirms history is intact.

A2. **Multi-instance on one cwd.** Why does claude code freeze when
opened twice in the same directory? Is it a lockfile, an IPC
socket, a process-singleton check? Can it be configured around
(e.g. distinct config dirs, distinct session dirs)? What do
orchestration tools like Conductor, Aider, opencode, claude-squad
actually do — separate worktrees, separate cwds with shared
project root, or something else?
- Method: read claude code source/docs around session/lock state;
  web search "claude code multiple instances", "orchestrate claude
  code", "conductor.build" architecture; if needed prototype two
  claudes pointed at the same repo via distinct CLAUDE_CONFIG_DIR
  / cwd.

A3. **Cross-agent review on shared work.** Could codex review
claude's commits in the same working tree without a worktree (just
distinct sessions and disciplined writes)? What breaks: file locks,
editor state, hook reentrancy? This bounds *whether* worktrees are
required vs merely convenient.
- Method: small spike running both clients against the same cwd in
  read-only review mode; document failure modes.

### Cluster B — Workflow shape

What does the lifecycle of a worktree-shaped clank session look
like end-to-end?

B1. **Plan-in-main, impl-in-worktree.** Should plans be drafted in
the main repo (where the user can read them, where stubs live) and
then "checked out" into a worktree for implementation? When the
plan finishes, do the commits return to main by merge, by
cherry-pick, or by leaving the worktree as the PR branch?
- Method: design exercise; sketch the state machine on paper, cite
  prior art (git-branchless, jj, Graphite, Conductor).

B2. **Plan ↔ PR cardinality.** Is the unit of a worktree one plan
or one PR? A PR might bundle several plans (e.g. an "umbrella"
refactor with three sub-plans). What does umbrella-style mapping
look like in the worktree? Does each sub-plan get its own commit,
its own commit chain, or a single squash at finalize?
- Method: design exercise; survey how Graphite / Reviewable / git
  spice / jj handle stacked PRs; produce a recommendation.

B3. **Flagship workflow.** Pick a north-star UX. The two candidate
modes from the user:
- *solo*: queue plans, run sequentially in one tree
- *worktree-per-PR*: each worktree becomes a PR, dies on merge
What does "happy path" look like for each — what commands does the
user type, what does clank do automatically, where is the editor?
This is the deliverable the rest of clank's roadmap orients
around.
- Method: write two concrete day-in-the-life scripts; identify the
  commands clank would need to expose.

### Cluster C — Repo and filesystem hygiene

How does clank coexist with a repo whose maintainers won't accept
`.clank/` upstream?

C1. **Local-only ignores.** What are the options for hiding
`.clank/` from a repo we don't own? Survey: `.git/info/exclude`,
`core.excludesFile`, skip-worktree bit, sparse-checkout, assume-
unchanged, per-worktree gitignore. Which of these survive `git
status` cleanly and which fight back?
- Method: docs + small spikes; produce a table of trade-offs.

C2. **`clank purge --squash` as the bridge.** If `.clank/` lives in
the worktree as normal tracked files, the path back into main
involves squashing them out. Is `clank purge --squash` already
doing this, or just removing in-tree state? What would a "finalize
into PR" command look like that:
- collapses the plan's commits onto a clean branch (no `.clank/`)
- pushes that branch
- opens the PR with the plan body as the description
- Method: read current `clank purge` + finalize code; sketch the
  delta needed.

C3. **`clank open` under worktrees.** Today `clank open` tells the
editor how to open the project. With worktrees, the answer
branches: open the main repo? the active worktree? both? How does
that interact with emacs (current user), VS Code (windows-per-
folder), JetBrains (project-per-window)? What's the minimum viable
spec?
- Method: read `clank open` code; user-test with emacs to
  understand current binding; propose a CLI shape (e.g. `clank
  open --worktree <name>` vs implicit current-worktree
  resolution).

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
- Method: prototype an emacs-side hook that, given a worktree
  path, spawns master+reviewer buffers bound together.

D2. **Agent-pair binding.** The current emacs setup leaves the
codex and claude buffers as independent windows — easy to lose one,
no "which one is active" affordance. Design options:
- single buffer that shows the currently-active agent (master if
  none active)
- always-paired side-by-side
- "agent stack" with focus follows gate state
- Method: design exercise; user input required on which feels
  right (see Open Questions).

D3. **Spawn cost and lifecycle.** When does an agent terminal
appear and when does it die? A worktree might exist for a week of
reviews; the agent terminal might not. Does `clank wfw` re-attach
to an existing session, spawn fresh, or refuse? What's the
contract?
- Method: design exercise; tie back to A1 (session forking) and A2
  (multi-instance).

### Cluster E — Strategic

E1. **Critical assessment.** Honestly, does clank serve a purpose
alongside Conductor, claude-squad, opencode-orchestrate, plain
git-worktree + tmux, etc.? Where does clank's peer-review-as-gate
model give a unique edge, and where is it reinventing a wheel?
- Method: write a one-page positioning memo. Be willing to
  conclude clank should narrow its scope.

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
- Method: read crate docs; build a ~200-line spike that hosts two
  bash shells and switches between them with a hotkey. If this
  spike works, F1 is settled in our favour.

F2. **Spawn-into-worktree flow.** Once the multiplexer exists,
"open a new worktree" should be: `clank worktree new <name>` →
clank creates the git worktree, forks (or resumes) the current
master + reviewer sessions inside it (A1), spawns them as new
tiles in the TUI, hands focus to the new master tile. What does
that command surface look like end-to-end? What state has to be
copied from the source sessions to the forks — cwd, conversation
history, env, agent role binding?
- Method: design exercise; depends on A1.

F3. **Background-activity surfacing.** The user is looking at the
master tile; codex is mid-review on a different worktree. How does
the user discover that codex wants attention? Options:
- chrome bar with per-tile indicators (`*` for new output, `!` for
  stop-hook fire, role + gate state visible)
- transient notification line when a hidden tile's gate flips
- bell on any output (probably too noisy)
- Method: design exercise. Pick the cheapest indicator that is
  not irritating; cross-reference how zellij / tmux / wezterm do
  it.

F4. **Existing tools that already do this.** Survey: claude-squad,
Conductor, opencode, aider, embedding zellij as a library,
embedding wezterm-mux. For each: how close is it to what we want,
what's the integration cost, is "shell out to it" or "use it as a
library" cheaper than rolling our own?
- Method: web search + read the top three repos; produce a
  comparison table covering license, embed story, session-resume
  story, and what it would cost to retrofit clank semantics onto
  it.

F5. **Graceful degradation.** If F1 turns out to require months,
what is the cheapest fallback that gives a *near-equivalent* UX?
Likely: a shipped tmux config plus emacs binding that together
emulate the switch-between-tiles experience, with the CLI surface
unchanged. Verify that the rest of the worktree workflow can be
designed against an abstract "agent terminal" interface so we can
swap implementations without re-shipping plans.
- Method: sketch the tmux + emacs fallback alongside the native
  TUI; confirm clank's user-facing commands look the same under
  both.

### Findings (master)

#### F1 — PTY multiplexing in Rust is tractable but expensive

Building a native multiplexer is **technically possible**:

- `portable-pty` (the cross-platform PTY crate `wezterm` ships)
  plus `vt100` or `alacritty_terminal` for parsing the hidden
  tiles' screen state, plus `crossterm`/`ratatui` for chrome and
  raw mode, is the proven recipe. Working open-source examples
  exist: `term39`, `croft` (VSCode-style TUI), `wtmux`, `psmux`.
- `r3bl_tui` v0.7.7+ ships an *internal* "Terminal Multiplexer
  with VT-100 ANSI Parsing" module, but the public API is not
  stable yet — the doc coverage is ~62% and there's no documented
  embed surface. We can't shortcut F1 by depending on r3bl_tui;
  we'd be building on `portable-pty` directly.

Engineering reality: every working example above is *itself* a
sizeable Rust project (thousands of lines). The ANSI/VT
escape-sequence surface is large; "good enough that claude code
and codex render correctly in the hidden tile" is more work than
"draws ASCII". Estimate **weeks of focused work** to ship
something that doesn't visually corrupt under the agent CLIs we
care about, plus an ongoing maintenance cost as those CLIs evolve
their rendering.

Verdict on F1 alone: feasible, but not cheap.

#### F4 — Prior art makes the call obvious: delegate to tmux

The closest existing tool is **claude-squad** (smtg-ai), and its
architectural choice is the single most informative data point in
this whole research plan:

| Tool          | Language | Multiplexer       | Isolation     | Notes                                 |
|---------------|----------|-------------------|---------------|---------------------------------------|
| claude-squad  | Go       | **tmux** (delegated) | git worktree | Multi-agent, hotkey-switch, sessions  |
| Conductor     | Swift / macOS | macOS GUI (own) | own workspaces | macOS-only, handles PR/merge, paid    |
| r3bl_tui      | Rust     | own (internal)    | n/a (lib)     | Library, no stable public mux API     |
| zellij        | Rust     | own               | n/a (mux)     | WASM plugin system; community is building claude-code plugins for zellij |
| Shipyard etc. | varies   | varies            | varies        | 6+ orchestrators surveyed for 2026    |

The single most relevant fact: **claude-squad faced almost exactly
our problem statement and chose to delegate to tmux instead of
building a multiplexer**. They got hotkey switching, session
isolation, parallel agent execution, and worktree binding without
writing their own PTY/VT code. The Go vs Rust difference doesn't
change the trade-off.

Zellij is the other credible host: it's Rust, embeds a WASM plugin
runtime, and at least one community project is already building
claude-code orchestration as a zellij plugin. Embedding as a
*plugin host* is heavier than driving tmux, but lighter than
rolling our own multiplexer and gives us layout/session features
for free.

#### F2 — Spawn-into-worktree blocked by claude code session limitations

This is the **most important and least pleasant finding** in
Cluster F. The "open a new worktree → fork the active session into
it → resume" flow runs into a hard constraint:

- **claude code** has `--fork-session` and `--resume <id>` and `-w
  <worktree>` flags, but **cannot resume a session in a different
  cwd**. Open issues #58591, #28745, #28314, #30906, #42596 all
  describe this. `claude -w` starts a *new* session in a worktree;
  it cannot move an existing session into one.
- **codex** is materially better: `codex resume <id> --cd <dir>`
  *does* work; `/fork` exists; `--add-dir` allows cross-project
  coordination.

So the asymmetry is:
- master (claude) sessions can be forked *or* moved to a worktree,
  but not both at once. Fork lands in original cwd; worktree start
  is a fresh session.
- reviewer (codex) sessions can be both forked and relocated.

This forces the worktree workflow to **accept that the master
session in a new worktree is a fresh session, not a fork** (unless
upstream claude code lands `--cwd` on resume). Conversation
history doesn't ride along. The continuity must come from the
plan body + commit log + feedback files — which is exactly the
clank model. So the constraint hurts less than it looks: clank's
*existing* artifacts already carry the state that matters.

#### F3 — Background-activity surfacing (design preview only)

Not full Findings yet — flagging the design direction:

- chrome bar carrying `<agent> <role> <gate-state> <activity-mark>`
  per tile. Activity mark = `*` for new output since last view,
  `!` for stop-hook fire (clank already emits stop-hook signal).
- when delegated to tmux, this becomes a status-line config we
  ship plus a clank subcommand (`clank chrome` or similar) that
  emits the per-pane chrome line tmux can poll.
- the indicator is *cheap* to add in either delegated-tmux or
  native-mux worlds. Not load-bearing on the F1-vs-F5 choice.

#### F5 — Recommended choice: ship the F5 path as the default

Given F1 (feasible-but-weeks), F4 (claude-squad already solved
this with delegation), and F2 (the worktree session-fork story is
already constrained by upstream and won't be unblocked by a
native multiplexer), the recommended path is:

- **Default and shipped:** clank drives **tmux** as the
  multiplexer. Clank owns the session model, the worktree binding,
  the chrome line, and the agent-spawn commands. tmux handles the
  PTYs, the input forwarding, the hotkey switching, and the
  always-alive hidden tiles.
- **Optional and later:** consider zellij as an alternate backend
  via its plugin system if and when there's a concrete user need
  the tmux path can't serve.
- **Not recommended:** rolling our own PTY/VT multiplexer.
  claude-squad — a project with the same scope as us, more
  engineering hours, and a year head start — chose not to. There
  is no evidence we'd do better with less.

This is a *reframe* of what the user asked: clank still spawns the
TUI, still hands the user a hotkey, still keeps the inactive
agents alive, still ties one terminal to one worktree. The only
difference is the multiplexing engine is tmux, not custom Rust
code. From the user's seat in emacs, the experience can be
identical.

The remaining open question is the emacs binding: does the user
want a *tmux session embedded in emacs* (via `vterm` / `eat`), or
do they want emacs to call out to a real terminal app holding the
tmux session? That decision feeds Cluster D.

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
- Building the emacs binding — that's its own plan once D1/D2 land.

## Open questions for the user

These are flagged here so we ask up front rather than guess:

- Is "worktree-per-PR" the assumed flagship, or are we genuinely
  comparing it against solo-sequential?
- If Cluster F's spike succeeds, are you willing to absorb the
  build cost of a native multiplexer, or would you rather always
  ship the tmux+emacs fallback even if the native option is
  feasible?
- For agent-pair binding (D2 if F doesn't pan out), do you have a
  preference among single-active-tile, always-paired, or
  focus-follows-gate? Or do you want the synthesis to recommend?
- Is critical assessment (E1) for your own gut-check, or should
  the synthesis be allowed to recommend "narrow clank's scope to
  X"?
