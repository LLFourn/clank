# tui-github-event-page
# TUI: Enter on a github row opens an event page (preview + open-in-browser)

## Why

Github events interleave into the TUI timeline
(log-timeline-github-events) but they are DEAD rows: Enter does
nothing, and the only way to act on one is to leave the TUI (`clank
events show`, then hand-open the URL). Plans already have the
opposite: Enter on a plan row opens a full page in the status pane
with actions. Events deserve the same first-class treatment —
especially now that watches carry operator prompts
(github-watch-prompts) that tell the agent-or-human what reacting
means.

## What

- **Enter on a `Github` timeline row** (and on the github-backed
  rows of the agents-panel/log wherever the cursor can rest on one)
  opens an EVENT PAGE in the status pane, same navigation pattern as
  the plan page (PlanDetail): Esc/q back to the log, cursor keys
  over an actions list, Enter activates.
- **Page content — preview from what we HOLD, no network**: repo,
  event kind + detail, number/title, actor, age, transport,
  handled/unhandled state, the event URL, and the watch's standing
  prompt (the presentation-time join). Multi-line, sanitized like
  `events show`. No API fetch in v1 — the page renders instantly
  from the WAL row + config; a body-fetch can layer later.
- **Actions, mirroring existing page UX**:
  - `open in browser` — the event URL via the platform opener
    (`open` / `xdg-open`, the same mechanism `clank html --open`
    uses), with the same confirm-free activation the plan page's
    non-destructive actions have.
  - `ack` / `unack`-less: acking from the page is IN scope when the
    row is unhandled (it is the react-then-ack loop's terminal
    step) — confirm-free, it is not destructive (the event stays in
    the log).
  - Actions the page cannot honor (no URL on the event) render
    dimmed/absent, matching how other pages degrade.
- **Which event — agent-scoped member identities** (intro b996cb6):
  a (source, seq, ident) key is NOT globally unique — the timeline's
  copy provenance is (agent, source, seq), and separate agent WALs
  legally reuse a source name and sequence. The merged model gains
  SORTED typed member references, each carrying at least
  `{agent, source, seq, ident, acked, transport}`:
  - The PAGE TARGET is the FULL copy key `(agent, source, seq,
    ident)` — never the (agent, source, seq) prefix: damaged-log
    state legally holds multiple distinct events at one seq in one
    WAL, and ident is the first-class discriminator reads,
    compaction, and ack already use (intro b74ad36).
  - **Retarget lookup is retained-set based, not inferred**: at open
    and on every successful refresh the page RETAINS its component's
    full sorted member-key set (the four-tuples). When the targeted
    key is absent from a fresh snapshot, retarget to the FIRST
    retained key (sorted order) still present; if the prior
    component SPLIT (a bridge row vanished), the page follows the
    fresh component CONTAINING that first surviving key — first-key
    wins picks the side deterministically. The retained set is then
    refreshed from the newly targeted component. Close only when NO
    retained key survives. Nothing is reconstructed from display
    fields.
  - ACK fans out through the typed references: one extracted
    events-ack core (shared with `clank events ack`) receives the
    member list and writes each agent WAL's discriminated selector —
    no page-private ack logic, no primary-only ack.
  - PROMPTS are per (agent, source): the page renders the
    DEDUPLICATED set; identical prompts show once unattributed,
    differing prompts each show with their agent attribution.
    Transport is likewise per member and shown per member when it
    differs.

## How (constraints)

- Reuse the page machinery the plan page established (mode enum
  arm, input routing seam, render fn, refetch-on-refresh identity
  rule: the page tracks its event by (source, seq, ident) — a
  refresh that drops the row closes the page to the log, same as a
  vanished plan).
- The browser-open goes through ONE helper shared with the existing
  `--open` path (no second platform-dispatch table); in tests it is
  a recorded no-op (no process spawns).
- Ack writes through the SAME code path as `clank events ack`
  (resolver + discriminated acks) — no page-private ack logic.
- Keyboard summary line and any help text update; the clank-github
  skill mentions the page as the TUI-side inspection surface.

## Acceptance

- Enter on a github timeline row opens the page; Esc returns with
  cursor/scroll preserved (same round-trip contract as the plan
  page). Pinned at the input/render seams like the plan page tests.
- Identity pins (intro b996cb6/b74ad36): two agents with the SAME
  source name and seq resolve to distinct page targets; one agent's
  damaged WAL holding two DISTINCT idents at one seq selects each
  correctly by full key; acking a merged component acks every
  unhandled member across both agents' WALs (fanout through the
  shared core); deleting the targeted member's rows retargets via
  the retained sorted key set (first surviving key), including
  across a component SPLIT where first-key-wins picks the side;
  two members with DIFFERING prompts render both with attribution
  while identical prompts collapse to one.
- The page shows the watch prompt for events whose source declares
  one (including events logged before the prompt existed).
- `open in browser` invokes the shared opener with the event URL
  (recorded in tests, spawned in production); rows without a URL
  render the action disabled.
- Acking from the page marks every unhandled copy handled and the
  row re-renders handled on the next refresh; the wait stops
  re-presenting it (same observable as CLI ack).
- In-process tests only; no network in the page path.

## Delivered (2026-08-08)

Implemented as planned, with the review cycle sharpening three
contracts along the way:

- **Identity**: `MemberKey` (agent, source, seq, ident) carries
  Eq/Ord alone; `MemberRef` adds mutable ack state + typed
  transport. Rows carry the ORIGINAL snapshot index (captured
  before the floor filter and reversal — a derived-list position
  inverted the mapping).
- **Retargeting**: the retained-set contract is the pure
  `refetch_event_page` seam — first surviving key wins, pinned
  across component splits.
- **Ack**: two-phase fanout (read/resolve everything, then append)
  through one `resolve_ack_target`/`AckTarget` seam shared with the
  CLI, discriminated selectors pinned on a damaged same-seq WAL.
- **Actions**: `event_action_effect` resolves the executed step to
  typed effects (exact URL / exact fanout keys); the loop only
  performs IO. Prompts join per (agent, source), deduplicated with
  attribution only when they differ.

No network anywhere in the page path; skills/README updated.

Review 0fbcbac then caught the UI shortfall behind the first
"delivered" claim; now landed and pinned: relative age in the facts,
control-char sanitization through the shared events-show seam,
wrapped multiline prompts in a windowed details body with the plan
page's document-scroll model (long prompts reachable, never
clipped), the pinned key-summary row, and Esc AND q both returning
to the log (the page never quits the TUI).
