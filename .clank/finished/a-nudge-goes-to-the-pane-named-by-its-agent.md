# a-nudge-goes-to-the-pane-named-by-its-agent
# A nudge goes to the pane named by its agent

## Problem

An idle agent with pending work whose turn-end wait was lost — most often to Claude's "Compacted while idle" — is only rescued if the monitor can type a nudge into its pane. Today that needs **launch provenance**: the `pane-launch-<hash>.json` record `clank agent start` writes (pid, birth, session, pane, harness version), read by `pane_target::resolve_process`. Any session clank didn't launch, or whose record doesn't match its current session and pane, is reported as `launch provenance is missing; restart this agent through clank before nudging` and is never nudged.

Observed 2026-10-27: fs-war-room's master compacted while idle at 19:28 AEDT on Oct 26 and sat for ~10 hours with pending work. The monitor was current and saw it (`waiting for you`) but refused to nudge for lack of provenance.

The user's decision: the pane's name is the agent's identity (plan a-pane-is-named-by-its-agent). Launch provenance is to be deleted as a concept, not patched. If someone renames panes by hand to fool clank, that's their fault.

## What changes

Provenance did three jobs. Each gets replaced or removed:

1. **Which pane to type into** — the workspace lookup from a-pane-is-named-by-its-agent: the pane whose identity is the agent's label, within the repo's TUI-anchored (session, tab). `Lookup::Known` with exactly one pane is the target. No pane, or `Uncertain`, means no nudge, with that reason shown in status ("no pane named X in the repo's tab" / "two panes are named X").
2. **Whether that pane's process is the agent** (pid + birth check) — removed. The name is the identity.
3. **The harness version gate** (`harness_identity`, `Process.harness`, "its version is not known yet") — removed. Whether a typed submission is safe is decided by the tool from the roster: `resume::candidate(tool)`. The `EVIDENCE` table stays as documentation of why each tool's candidate is enabled, but nothing reads a running version anymore. Grok stays `None` (never typed into).

Delete: the `pane-launch-*.json` writer in `agent start`, `pane_target`'s record read/verify/`Launch`/`Process` provenance fields, `harness_identity`'s per-process version lookup, and every status string that says provenance or "version is not known yet". Stale record files in the runtime dir are left to rot; nothing reads them.

The whole concept goes: after this plan, no code, test, status string or README passage (README.md "Launch provenance", near line 1209) refers to launch provenance or a launch record. Other uses of the word "provenance" in the tree (event/watch-prompt origin, git error sources, setup manifests) are unrelated and stay.

Sweep the belief, not the wording: grep for `provenance`, `Process {`, `harness`, `birth`, `discover_process`, `cannot_resume`, and the recovery/monitor paths that branch on a missing process; each must either go or move onto the pane lookup.

### The TUI's pane-exit feed goes too

Launch records had a fourth reader: the status TUI's `RuntimeWatch`/`LaunchFeed` (status_tui/zellij.rs) reads this session's `pane-launch-*` records, arms `proc_identity::exit_watch` on each record's pid (`Watches`, `Arm`), and turns `WorkerMsg::Launches`/`WorkerMsg::Exited` into an immediate probe, so a pane exit showed up without anyone listing panes.

Disposition: **immediate pane-exit notification is removed with the concept.** Delete `LaunchFeed`, `Watches`, `Arm`, `WorkerMsg::{Launches, Exited}`, `WorkerBatch.{launches, exits}`, `PaneLaunch`, `launches_in`/`launches_at`, and the `ExitWatch<PaneLaunch>` the TUI holds. Nothing replaces them with another source. Zellij's pane listing carries no pid to arm a watch on, and a polling loop or a new per-launch record is ruled out.

`RuntimeWatch` keeps its other job unchanged: it watches the per-repo runtime directory for this repo's `clank remote` reopen requests and hands them to the loop. A `pane-launch-*` file appearing, changing or lingering in that directory triggers nothing.

Afterwards the TUI learns that a pane exited at its next listing, which happens only on the triggers that already exist:
- a probe, which the loop sends when the agents panel or an agent's page is entered;
- a retitle, when an agent's status glyph changes (an exiting harness ends or abandons its session, which the monitor and hooks report as a state change);
- a roster change that reconciles.

A quiet TUI with no state change lists nothing, and so shows a dead pane as present until one of those happens. That's the accepted cost. The idle worker still costs Zellij nothing.

## Out of scope

- Fixing Claude's idle compaction dropping the parked wait. The nudge is the fallback that makes that survivable.
- Changing when the monitor decides a nudge is due (idle + pending work + no wait). Only *whether it can deliver* changes.

## Acceptance

All in-process, with Zellij listings and actions injected; nothing spawned.

- An idle master with pending work, no wait, no launch record, and a pane titled with its label in the repo's TUI tab is nudged: the typed submission goes to that pane id.
- The same with the pane titled `💤 <label>` (status glyph) is nudged to that pane.
- Two panes named the label in the tab: no nudge, and status names the ambiguity.
- No pane named the label: no nudge, and status says so.
- A pane named the label in a *different* tab or session is not a target.
- A Grok agent is never typed into.
- `clank status` for fs-war-room's shape (idle, pending, no record) no longer mentions provenance or an unknown version.
- TUI: a hand-started agent pane (no launch record, no config edit) that exits is shown as gone after the next probe batch, through the real batch handler with an injected listing.
- TUI: a `pane-launch-*.json` file written into the runtime directory produces no worker message and no listing; a `clank remote` reopen request in the same directory still reaches the loop.
- TUI: a quiet worker (no probe, no roster or glyph change) performs zero listings across repeated idle iterations.
- Mutation checks: making the lookup accept `Uncertain` fails the ambiguity test; re-adding a provenance requirement fails the first test.
