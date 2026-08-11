# zellij-pane-placement-and-cost

Added agents land against the status pane instead of the reviewer
stack, and the zellij passes are slow. Both are measured below.

## Reported

"Very often when I add an agent it appears in a stack above the
status pane. It's meant to be part of the reviewer stack." Plus:
the zellij integration is "still super slow and unreliable".

## Evidence from the live session (2026-08-09)

Pane geometry, `zellij action list-panes --json`:

- tab 2 (frostsnap-ci): `status` occupies y=1..133 at x=125 w=53.
  `💤 kimi (reviewer)` is at **y=134, height 1**, same x/width — a
  ONE-ROW agent pane glued under the instrument pane.
- tab 8 (stacksign): `kimi (reviewer)` y=1 h=67, then a bare
  `Pane #3` (cmd=None, h=1), then `codex (reviewer)` y=69 h=66 —
  three separate side panes, no stack.
- tabs 6 and 10 also carry stray `Pane #N` panes with `cmd=None`.

Call costs. My first sample was wrong and is corrected here — the
numbers below are best-of-3, reproduced by two independent timing
methods, on a session of **28 panes across 11 tabs**:

| call | time |
|---|---|
| `zellij --version` (spawn only) | 12ms |
| `zellij action query-tab-names` | 22ms |
| `zellij action dump-layout` | **1019ms** |
| `zellij action list-panes --json` | **5580ms** |

Two things follow. Per-call overhead is ~22ms, so the cost is not
process spawning — it is these two queries. And `list-panes` costs
roughly **200ms per pane in the whole session**, not per tab: every
pass for one repo pays for every pane the user has open anywhere.

That also reconciles the code's own recorded measurement (0.12s
dump-layout vs 1.1s listing, from `zellij-one-listing-per-pass`).
Both have grown ~8x on the same ratio as the session grew. The
comment was RIGHT when written and is still right about which is
cheaper — my earlier "the comment is inverted" claim was an artifact
of one bad sample, and is withdrawn.

## Cause 1: the layout is a roster SNAPSHOT and zellij re-flows into it

Corrected twice now. The first draft read kimi's 1-row height as a
sliver from a bad split; the second read it as `--stacked` applied
against the status pane by `add_reviewer_pane`. It is neither —
`add_reviewer_pane`'s flags are not what puts status in a stack.

The layout file for the affected worktree
(`.clank/zellij/layout.kdl`, written 8 Jul) hard-codes the roster it
was opened with:

```
pane size="35%" split_direction="vertical" {
    pane stacked=true {
        pane name="codex (reviewer)" …
        pane name="ruthless (reviewer)" …
    }
    pane size="30%" name="status" …
}
```

That tab today runs a DIFFERENT roster — kimi, with no codex or
ruthless pane at all — and the live structure is:

```
pane size="30%" stacked=true {
    pane name="status" expanded=true
    pane name="💤 kimi (reviewer)"
}
```

The composed layout also ships `swap_tiled_layout` variants (2 in the
live dump) that embed the same fixed roster. zellij applies a swap
layout when the PANE COUNT changes, and it fills the variant's slots
POSITIONALLY — not by pane identity. So once the live roster drifts
from the snapshot, the side column has fewer panes than the variant
has slots, everything slides up, and the status pane is assigned into
the reviewer STACK slot. That is exactly the reported shape.

This is why it happens "very often" rather than always: it needs the
roster to differ from whatever the tab was opened with, which is the
normal state of a long-lived session — every add, remove, or promote
since `clank open` widens the gap.

The modelling error: the tab's layout is a snapshot of the roster at
open time, the roster is live, and nothing keeps them in sync. clank
then places panes imperatively with `new-pane` while zellij
re-arranges them declaratively from the stale variants. Two writers,
one layout, no agreement.

Any fix that only adjusts `new-pane` flags will keep losing to the
re-flow.

## Cause 2: verification is blind to any agent that has STARTED

This is the likely root of "slow and unreliable", and it is
independent of placement.

`collect_agent_panes` matches a pane only when
`command="clank"` and its args are `agent start <label> --repo
<repo>`. But `clank agent start` EXECS the tool, and dump-layout
reports the RUNNING command. In this session's dump:

- `command="clank"` panes: 13 — all still `start_suspended`
- `command="claude"`: 2, `command="opencode"`: 3 — started agents,
  which verify CANNOT match
- `list-panes --json` meanwhile reports 16 `clank agent start …`
  panes, because it reports the CONFIGURED command

So for any repo whose agents have actually started, `verify_pairs`
returns a set missing them, `plan_panes(...).is_converged()` is
false, `self.converged` is never set, and the worker reconciles
again — every pass paying the 456ms dump-layout plus a listing plus
a focus capture/restore, forever. That matches "very often": it
depends on which agents have exec'd.

The two sources disagreeing about a pane's command is the modelling
error underneath both symptoms. One of them has to be authoritative
for identity, and the code currently uses whichever is convenient
per call site.

## Scope

- **Placement.** Resolve the two-writer problem, do not paper over
  it. The options worth weighing: regenerate the tab's layout and
  swap variants when the roster changes so the snapshot stops going
  stale; stop shipping roster-shaped swap variants at all (they
  exist to make alt+[ / alt+] flip orientation, which is a real
  feature to trade away deliberately, not silently); or keep the
  variants roster-INDEPENDENT so slot assignment cannot depend on a
  count that drifts. Whichever is chosen, the instrument pane must
  not be assignable to a reviewer slot.
- **One identity source.** Decide which listing is authoritative for
  "is this agent's pane present" and use it everywhere. Whatever is
  chosen must survive the agent EXECING its tool, since that is the
  normal state of a working session, not an edge case.
- **Verify cost and correctness — the tension to resolve.** The
  CHEAP source (dump-layout, 1.0s) is the one blind to started
  agents. The EXEC-PROOF source (the listing, 5.6s) is 5x dearer and
  scales with every pane in the session. Picking either as-is trades
  a correctness bug for a latency bug.

  The way out is that dump-layout already carries what identity
  needs without the command: each pane keeps its clank-stamped
  `name` ("💤 kimi (reviewer)") and its `cwd`, and neither is
  disturbed by the exec. Role is ALREADY read from the title in
  `agent_pane_pairs`, so trusting the title for the label too is
  consistent rather than novel. Repo scoping comes from `cwd`
  against the layout's root, since a bare label repeats across
  worktree tabs. Confirm that shape before building on it.
- **Convergence must be reachable.** A pass that can never converge
  is worse than a slow one: it repeats forever. Whatever verify
  becomes, prove it reports converged for a session whose agents
  have started.
- **Pass cost.** At 5.6s for a listing, the pass is dominated by ONE
  call, not by the number of calls: spawn overhead is 22ms. Cutting
  the session-wide listing out of the common path is worth more than
  trimming any number of cheap actions. Establish whether a pass can
  avoid the listing entirely when nothing needs adding.
- **Stray panes.** Diagnose the `cmd=None` `Pane #N` panes before
  assuming they are unrelated. If clank creates them (a `new-pane`
  whose command failed to launch, say) that is part of this bug; if
  the user creates them by hand, the reconciler must at least not
  mistake them for agent panes or anchor off them.

## Investigate first

- Reproduce deterministically: a tab with a status pane and no
  reviewer, then add one. Capture BOTH sources before and after —
  the JSON listing for geometry, the dump for stack structure —
  since neither alone shows the failure.
- Confirm the re-flow directly: open a tab, change the roster so it
  differs from the layout snapshot, add a pane, and watch whether
  zellij moves EXISTING panes between slots. If it does, no change
  to `new-pane` can be sufficient on its own.
- Establish whether a first reviewer added into a tab that HAS a
  stack region behaves differently from one added into a tab opened
  with zero reviewers — the two paths differ and only one is in the
  report.
- Time a full reconcile pass end to end, so the fix can be judged
  against a number rather than a feeling.

## Non-goals

- Rewriting the layout model. The stage/stack/tui shape is fine;
  what is broken is where a LATER add puts itself and what the pass
  spends getting there.
- Upgrading zellij. The machine runs a HEAD build (0.45.0) that is
  already ahead of the latest release (0.44.3), so there is no
  upgrade to hide behind.

## Acceptance — status against what was delivered

An earlier version of this section overclaimed; corrected here
(codex on ae770ff).

MET:

- Two reviewers added in one pass end up in the SAME stack, pinned
  by `two_adds_from_a_reviewerless_snapshot_still_get_stacked`.
- A session whose agents have EXEC'd their tools reconciles to
  converged and stops issuing actions — the loop behind "slow and
  unreliable". Identity no longer depends on a command the exec
  replaces, and an empty query is no longer an authoritative answer.
- Verification reads the source that can see a started agent, with
  the measured numbers recorded at the call site.
- A repairable placement failure stays unconverged and retries;
  reconciliation stays off the render loop.
- Detection of the broken arrangement, including the lone-reviewer
  form, from real captured geometry.

NOT MET — do not read the above as more than it is:

- "A reviewer is never stacked with the instrument pane" is NOT
  established. Explicit ids constrain only clank's OWN `stack-panes`
  call. The stale roster-shaped swap variants (Cause 1) still let
  zellij re-flow the status pane into the stack on a pane-count
  change, and that is untouched.
- "The status pane keeps its size and position across an add" is NOT
  enforced. The predicate only requires status geometry to DIFFER
  from the reviewers'; a status pane that moved or was severely
  resized still passes. Enforcing it needs the size/position to be
  compared against what the layout asked for, and a test for it.
- The first-reviewer geometry criterion (a usable size, minimum
  height/width) is NOT implemented.
- A lone-reviewer tab that is already broken stays broken AND is
  cached as converged — bounded on purpose, but that is a known
  wrong state accepted, not a fix.
- The full-pass before/after measurement is NOT discarded, only not
  yet taken. The cost analysis explains why repetition dominated,
  which is a reason to re-measure, not a substitute for it.
- The zellij capability requirement IS surfaced: `doctor` warns when
  the installed CLIENT has no `stack-panes`, probed by capability
  rather than version arithmetic, injected at the command boundary
  so the checks stay deterministic, and reported outside a session
  too. Two gaps remain, both stated rather than glossed: no floor is
  ENFORCED anywhere beyond that warning, and the probe proves only
  what the client parser accepts — a session whose SERVER predates a
  binary replacement can still reject the action until restarted,
  and nothing checks that mismatch.

## Cause 1 is a technical dead end (measured 2026-08-11)

The human's decision was: stay open, explore the latest zellij, take
the cleanest solution, and if there is no clean solution, say so and
stop. There is no clean solution today. The unmet items above stand as
written — this section records WHY, so the next attempt starts from
evidence rather than repeating the search.

**The objective is not expressible in zellij's tiled layout system.**
`swap_tiled_layout` assigns existing panes to slots breadth-first by
ORDER, not by identity (documented, zellij.dev/documentation/
swap-layouts). Nothing clank emits can therefore bind the instrument
pane to a slot once pane order drifts. Zellij switches swap layouts
automatically when a pane is opened or closed and the current variant's
constraints stop being met — which is exactly the operation that adds
or removes an agent. Roster-free or constraint-covered variants do not
help: the assignment is still positional.

**Every escape route is closed upstream, verified today:**

- `override-layout --apply-only-to-active-tab` — the regeneration
  path — is still a silent no-op from a transient CLI client.
  Upstream #5250, filed 2026-06-11, OPEN on main. Its root cause is
  recorded there (`get_active_tab_mut(client_id)` resolves against a
  client with no active tab; the fix is
  `cli_client_id.unwrap_or(client_id)`). The plain form remains
  forbidden: it replaces the whole session's tab set.
- `dump-layout` is the SAME bug family: exit 0 and zero bytes from a
  transient CLI client, measured against both the current session and
  another. Clank cannot read a tab's layout back at all. (Geometry is
  still readable — `list-panes --json --command` works and is what the
  placement predicate already uses.)
- No pane-extraction action exists. The complete 0.45.0 action list has
  no `break-pane`, confirming the earlier measurement for item 3.
- Nothing on main helps. The machine runs a `main` build (0.45.0;
  newest tagged release is v0.44.3) and the unreleased changelog
  carries stack UI work, nested sessions and CLI-open-without-focus —
  nothing on layout identity, #5250, or stack extraction.

**The one structural solution, deliberately not taken.** A FLOATING
instrument pane cannot be assigned to a tiled slot: `swap_tiled_layout`
governs tiled panes only, floating panes have their own
`swap_floating_layout`. That makes the invariant true by construction
and dissolves items 1-3 rather than repairing them. It is not done here
because it is a UX redesign — the instrument stops being a tile and
becomes an overlay — and because pinned-visibility behaviour under a
floating-pane hide was not verified. Shipping that silently into a
daily driver is not a bug fix.

Everything else available is post-hoc repair: detect, `stack-panes`,
re-measure. That can only ever make the invariant approximately true,
which is what the code already does and why the criterion above stays
NOT MET rather than being reworded into something weaker.

The highest-leverage next move is upstream, not here: #5250, plus a
request for pane extraction from a stack. Neither has been filed
beyond the existing #5250 report.


Testing note: the geometry fixtures carry real captured geometry
rather than driving a live zellij, because spawning zellij in tests
is what leaked servers here before. The probes behind those numbers
were manual and cleaned up. An end-to-end harness needs its own plan
with session lifecycle as the subject.

## RESUME HERE — newer zellij actions supersede the A/B tradeoff (2026-08-10)

The human uses alt+[ / alt+], so the "drop the swap variants" option
is off the table. It turns out not to be needed: two `zellij action`
subcommands exist on the installed build that this code predates and
never uses.

- **`override-layout [LAYOUT] --apply-only-to-active-tab`** (upstream
  Dec 2025 "change layout at runtime" #4566, plus the Jan 2026 Layout
  manager #4601). This dissolves the two-writer problem instead of
  working around it: clank recomposes the layout from the CURRENT
  roster and hands zellij the whole arrangement, so the snapshot
  cannot drift and the swap variants are regenerated with it —
  keeping alt+[ / alt+] working.
- **`stack-panes -- terminal_1 terminal_2 …`**. The exact operation
  `add_reviewer_pane` fakes today by focusing an anchor and passing
  `--stacked`, including the focus-dependence that lets the status
  pane get swept into the stack. Also gives a repair path for tabs
  already in the broken state.

VERIFIED 2026-08-10, and the answer is NO — `override-layout` cannot
be used from clank. Both forms were measured in throwaway sessions
(deleted afterwards, orphans reaped):

- It DOES re-flow a live tab without respawning: two panes running
  `sleep 900` as PIDs 52257/52258 were re-arranged into
  `stacked=true` and kept both PIDs. So the idea was sound.
- But the PLAIN form replaces the WHOLE SESSION's tab set. On a
  two-tab probe, applying a one-tab layout took tabs from
  `[alpha beta]` to `[alpha]` — `beta` was destroyed and its process
  orphaned, still running with no pane. On this machine that would
  delete 10 of 11 tabs and orphan every agent in them.
- And `--apply-only-to-active-tab`, the form that would be safe,
  is a SILENT NO-OP: exit 0, other tabs preserved, nothing applied.

That is upstream issue #5250 (opened 2026-06-11, still OPEN):
"`override-layout --apply-only-to-active-tab` silently does nothing
when invoked from a transient CLI client", cause given as "the active
tab is resolved against the transient CLI client's id, which has no
active tab". `zellij action` IS a transient CLI client, which is
clank's only way to drive zellij. Upgrading does not help; the issue
is open, and it reproduces on 0.44.1, 0.44.3, and this 0.45.0 HEAD.

So the declarative fix is blocked upstream, and the ONE thing that
must not happen is reaching for the plain form as a substitute.

## Revised approach: `stack-panes`, not `override-layout`

`stack-panes -- terminal_1 terminal_2 …` takes explicit pane ids and
is inherently scoped — no tab replacement, no active-tab resolution,
nothing to destroy. It is also the exact operation `add_reviewer_pane`
fakes today by focusing an anchor and passing `--stacked`, which is
where the focus-dependence that sweeps the status pane into the stack
comes from.

Design consequences to work out:

- Stack the reviewer panes BY ID after creating one, rather than
  relying on what happens to be focused. The status pane is simply
  never in the id list, so it cannot be captured.
- The swap variants still re-flow panes positionally on a pane-count
  change (Cause 1 above). `stack-panes` repairs the arrangement after
  the fact; establish whether that is enough in practice or whether
  the roster-shaped variants must also stop being emitted.
- Worth reporting #5250's impact upstream, since clank is a concrete
  consumer blocked by it.

Consequence to accept deliberately: both actions raise the required
zellij version. The machine runs a HEAD build (0.45.0) ahead of the
latest release (0.44.3), so a floor needs stating and `doctor` should
say so rather than failing obscurely on an older build. No upgrade is
needed to build this — both actions are present on the installed
build, which is worth stating because replacing the binary under a
live session breaks the client/server version match and would force a
restart of every running agent tab.

## Landed before this was stashed

- Convergence terminates: an empty `dump-layout` is no longer read as
  "no agent panes" (that path is since deleted), and identity no
  longer depends on a command the exec replaces.
- `agent_pane_label` round-trips the full legal label domain, so a
  spaced or emoji label is no longer permanently absent.
- Verification reads the `--json` listing, the only source that keeps
  the configured command through an exec.

Not started: the placement fix itself, and keeping the 5.6s
session-wide listing off the common path.

## OPEN: no zellij action extracts a pane from a stack (2026-08-10)

`stack-panes` reliably BUILDS a correct stack from two or more ids
(measured: processes preserved, panes outside the id list untouched,
stale ids tolerated). Nothing available reliably takes a pane OUT of
one, which is what the single-reviewer-with-status tab needs:

- `move-pane <direction>` on the focused stack member: no effect.
- `break-pane`: not a subcommand on this build.
- `toggle-pane-embed-or-floating`: destructive — it floated an
  unrelated pane and collapsed the rest into one geometry.
- Stacking the reviewer with a temporary pane: RE-TESTED on a clean
  session and ruled out — `stack-panes` merges the temporary INTO the
  existing stack rather than forming a new one, whether the temporary
  is created inside or outside it. `stack-panes` builds stacks; it
  does not move panes between them.

Consequence, handled rather than left to bite: such a tab is
correctly reported UNPLACED, and because nothing can act on it the
pass records convergence anyway — bounded on purpose. Retrying an
impossible repair every refresh would reinstate exactly the unbounded
work this plan removes. The record is keyed to the roster view, so a
second reviewer arriving both invalidates it and makes repair
possible. A failure that IS repairable (two or more reviewers) still
stays unconverged and retries.

Ways out, for the next pass at this:
- Re-open the tab from a fresh layout (heavy, but `clank open`
  already composes exactly the right arrangement).
- Report it once and stop retrying — needs a "known-unrepairable"
  state so the loop is bounded.
- Upstream: ask for a pane-extraction action, alongside the #5250
  report.

## The cost was dominated by non-convergence (2026-08-10)

Re-examined rather than optimised blindly. The expensive listing is
already OFF the steady-state path:

- `reconcile` returns before taking any listing when the cached
  convergence matches the current roster view.
- the retitler keeps a pane-map cache and re-lists only when priming
  or when a wanted label has no cached pane.

So in a converged session the pass costs nothing. What made it cost
5.6s repeatedly was that convergence was UNREACHABLE: the empty
`dump-layout` read as "no agent panes", and identity matched a
command the exec replaces, so `converged` was never set and every
refresh ran a full pass — listing plus dump plus actions, forever.
Those are fixed, which removes the repetition rather than the price
of one call.

What remains genuinely dear is the price itself: ~200ms per pane
across the WHOLE session, so a pass for one repo pays for every pane
open anywhere (5.6s at 28 panes). That is worth attacking only if it
still bites once passes are rare — measure before assuming, since
the earlier ratio measurement was already wrong once.

## Still open

- The swap variants remain roster-shaped, so zellij can still
  re-flow panes positionally when the pane count changes. Placement
  repair now corrects the result, but the re-flow itself is
  untouched; whether that is good enough in daily use needs
  observation rather than argument.
- No zellij action extracts a pane from a stack (see above), so a
  single-reviewer tab still needs `clank open` to rebuild.
