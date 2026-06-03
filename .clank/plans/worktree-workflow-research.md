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

#### A2 — Multi-instance same cwd: actively broken upstream

Two claude code processes pointed at the same working directory
are **not safe today** (June 2026). The failure is a real runtime
constraint, not just an editor cosmetic issue:

- `~/.claude.json` (global config) is corrupted by concurrent
  writes — invalid JSON across instances (issue #28992).
- Lock acquisition fails at `~/.local/share/claude/versions/<v>/`
  with logged `Lock acquisition failed for ...` warnings (issue
  #13287).
- Stale lock files at `~/.local/state/claude/locks/` persist
  across reboots with no automatic cleanup (issue #14301).
- Shell-state corruption: concurrent instances share a snapshot
  directory keyed by cwd hash, race on writes, and surface as
  `bash: …: No such file or directory` mid-tool-call (issue
  #4014).
- Outright regression in 2.0.61: "if two Claude Code instances
  are loaded, they can no longer operate in parallel — when one
  runs, the other one stops and no more messages can be received"
  (issue #13499).
- There is an open feature request to add a session lock file
  (#19364) — i.e. upstream has not committed to fixing this
  structurally yet.

**Workaround that does help:** set `CLAUDE_CONFIG_DIR` per
instance so each claude has its own ~/.claude root. This sidesteps
the global-config-corruption case directly. It does **not** fix
the shell-snapshot-by-cwd-hash case because that key is derived
from cwd, not config root — so even with separate config dirs,
two claudes in the same cwd still race on shell-snapshot files.

**Workaround that fully helps:** distinct cwds — i.e. git
worktrees. This is what every observed orchestrator does:
claude-squad (per-session worktree), Conductor (per-workspace
isolated copy), the community claude-pool daemon (managed pool,
distinct worktrees).

A2 verdict: **worktrees are required**, not optional, the moment
clank wants two claude masters live at once. They are also the
right answer for "user is hand-driving claude in main while
clank's claude is also doing work" — without a worktree those
two claudes will fight at the runtime layer.

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
default case clank already runs today and works**. Pushing it
into a worktree is convenient (matches A2), not load-bearing.
Two-claude is the case worktrees are *required* for.

#### Implications for the workflow

- The flagship worktree workflow has to assume each worktree gets
  its own `CLAUDE_CONFIG_DIR` *and* its own cwd. Both are needed:
  cwd alone fixes the snapshot races; config dir alone fixes the
  global-config corruption.
- A "worktree spawn" command should export
  `CLAUDE_CONFIG_DIR=$WORKTREE/.clank/claude-config` (or a
  similar per-worktree path) into the master's environment
  before `claude` launches. Codex needs the analogous
  `CODEX_HOME` set per worktree.
- Plan-in-main, impl-in-worktree (B1) is unavoidable for any
  flow where the user already has a claude master running in
  main. That same constraint pushes B3 toward worktree-per-PR
  as the flagship mode.

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
