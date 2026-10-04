# remote-ui-navigation-questions-and-stable-colors
# Remote UI navigation, interactive questions and stable colours

## Problem and outcome

The remote UI duplicates plan navigation with an Open plan link, hides the team
behind a management link, does not surface interactive harness questions in the
agent timeline, and lets repository state change unrelated text colours. Make
the remote experience coherent, useful for controlling agents and visually calm.

## Navigation and identity

- Remove redundant Open plan actions where the plan name already opens the same
  destination. Keep one clear accessible link per plan, with understandable
  names and responsive layout rather than awkward duplicated wrapping.
- Show team members directly, comparable to the TUI: agent identity, role and
  relevant status, with navigation to the agent. Use recognisable provider logos
  where suitable assets are available, with consistent sizing, accessible labels
  and a graceful fallback. Do not make provider imagery the only identity cue.
- Evaluate Team and Settings as tabs in the main pane alongside Chat and
  Terminal. Place agent-specific settings on that agent's Settings tab rather
  than conflating agent, team and remote-client settings. Produce a coherent
  final navigation model and cover mobile widths and keyboard access.
- Audit what Device settings actually controls. Retain necessary functions in
  their appropriate scope; remove unnecessary navigation. If remote credentials
  or connection identity are involved, use an idiomatic profile/identity control
  showing the connected identity or safe token label, with disconnect and local
  credential deletion. Never display the secret token. Define whether deletion
  clears this client's credentials or revokes server access, and label it
  accurately. Reconnecting, expired credentials and disconnected state must have
  understandable paths without accidentally deleting unrelated agent settings.

## Interactive questions in the agent timeline

- Research current supported Claude blocking-question and Codex background
  question interfaces (including the terminal Shift+Left answer flow). Trace
  native protocol events and the existing bridge before choosing integration.
  Prefer structured harness events and supported answer APIs over terminal text
  scraping. Document concrete support and any genuine provider limitation.
- Display pending questions as structured timeline entries with the originating
  agent/session, prompt, choices or text fields and answer controls. Support the
  actual question forms, including multiple questions where the provider offers
  them; do not reduce them to a misleading generic chat message.
- Answers must reach the exact live request, using the provider's supported
  mechanism. Distinguish pending, sending, answered, cancelled, stale and failed
  requests. Preserve retryable user input on failure. Prevent duplicate answers
  from repeated clicks, reconnect replay or a concurrent terminal response.
- Recover pending state across reconnect and remote viewer attachment where the
  harness allows it. Reconcile answers made locally in the terminal. Do not
  answer stale requests after session switches, forks or clears, or claim success
  merely because a transport accepted bytes.
- Treat question UI as separate from normal chat, event acknowledgements and wake
  receipts. Keep it usable while the agent is blocked or working in the background.

## Stable visual state

- Trace the yellow-text leakage and scope state styling to explicit status badges
  or indicators. Repository/gate state must not recolour unrelated plan links,
  headings, navigation or chat content.
- Use a consistent semantic palette with readable contrast in supported themes.
  Verify state transitions, not just a static screenshot. Preserve meaningful
  status information with deliberate text/icon cues rather than blanket colour.

## Implementation and acceptance

Map current navigation/settings ownership and question transport first, then
implement reviewable milestones for navigation/identity, question integration
and style isolation. Keep dependencies and limitations explicit in the plan.
Validate responsive and keyboard navigation, direct plan links, team display,
safe disconnect/credential clearing, live Claude and Codex question round trips,
terminal-side answers, reconnect, duplicate answers and stale-session rejection.
Use fixtures for protocol edge cases and distinguish them from authenticated live
acceptance. Check that switching repository states does not alter unrelated text
colours. Reuse the existing bridge and event-driven subscription model; add no
continuous question polling or new idle helper process. Include visual previews
of the finished layouts for review.

## Milestone: current ownership map and implementation contracts

The intro review asked for the navigation/settings map, question transport trace
and implementation milestones. The following is the implementation basis, not a
claim that remote questions or the new layout already work. No installed asset or
runtime integration has changed in this milestone.

### Current page and final navigation model

`crates/web/assets/page.html` owns the remote layout and browser interactions.
The header has agent selection and Chat/Terminal modes; Timeline becomes a mode
on narrow screens and is a persistent ledger on wide screens. Queue and stash
rows render TWO links to the identical HTML destination. Management links in
that ledger call openSitePage and reuse the plan viewer container. Browser state
for plan return, selected agent, transcript/terminal mode, draft/outbox and mobile
timeline scroll already exists; preserve those states when moving navigation.

The Team page currently combines repo roster actions (add, review tier, promote,
swap, remove) and agent controls (auto mode, session clearing, pane reopening).
It uses /agents, /agents/candidates and /agents/reopen through the existing repo
connector. Those validated backend operations stay the mutation owners.

Implement this final arrangement:

- Keep Chat and Terminal scoped to the selected agent. Add Team and Settings in
  the same main-pane navigation. Team is repo-scoped; Settings is explicitly
  scoped and labelled for the selected agent. Keep Timeline's responsive
  behaviour without making Team/Settings duplicate sidebar management pages.
- Show a compact directly navigable roster in the ledger, with agent name, role,
  provider and activity/pane cues. Navigation selects/pins the named agent; it
  must not discard that agent's draft, outbox or per-agent view preference.
- Team contains roster membership, replacement and review/master roles. Settings
  contains that agent's auto mode and session/pane controls. Session clearing
  continues to capture/revalidate the exact session and retains its confirmation.
  Agent settings never operate implicitly on a newly auto-selected agent.
- Remove the redundant queue/stash Open plan links. Preserve the linked full
  accessible name, priority/commit metadata and existing revision-aware plan
  actions. Long names may truncate visually without losing their accessible
  names. Preserve original focus when returning from a plan.
- No provider-logo assets currently exist in the bundle. Use locally bundled,
  recognisable provider marks only with established usable assets; otherwise a
  consistent provider monogram/name is the graceful initial fallback. Do not
  fetch third-party branding assets from a credential-bearing page. Names remain
  visible and decorative marks do not repeat screen-reader announcements.
- Use real tab semantics, selected state, arrow/Home/End keyboard navigation and
  focusable panels. Small widths scroll the navigation strip horizontally;
  panels/roster rows wrap internally, without widening the document. Team and
  Settings have dedicated panels rather than masquerading as plan iframe pages.

### Browser access is not agent settings

Device settings has ONE function today: Forget this device. There is no device
configuration to retain. Remove that management destination and move access to
an identity/profile control labelled Remote access / This browser, with a safe
connection state. There is no authenticated human display name or token nickname
in the current page contract, so do not invent one or display the secret token.

The built-in door stores `clank.token` in localStorage for reconnect. POST /forget
in `crates/web-cloudflare/src/host.rs` requires same origin, revokes this cookie's
session with Door::revoke_session and expires the cookie. The existing door state
machine seals credential provisioning during the request, so a late /token reply
cannot restore forgotten access. Only after successful revocation does it clear
saved access and navigate to login. Failure leaves a usable retry path and must
not be represented as successful sign-out. Retain that race protection.

Label the action Disconnect and forget browser access, and explain its exact
scope: ends this browser session and removes its saved credential; it does NOT
rotate the shared remote token, revoke every other browser, remove an agent or
clear agent sessions/settings. Other browsers retaining the shared token can
still reconnect. Hosted deployments instead use their configured signout URL;
when no signout URL exists do not offer a fake disconnect action or pretend to
clear host-managed credentials. Describe hosted sign-out according to that host
contract. Preserve login/base-path routing, expired-session recovery and storage
failure behaviour. No extra token endpoint, account store or credential identity
inference is needed for this layout change.

### Colour leakage and isolation

onStatus currently sets document.documentElement's --hue to facts.hue, while
--hue-deep derives from it. The same variables colour prose links, queue/stash
links, editor controls, management buttons, focus borders and Send. A yellow
reviewing hue therefore propagates far beyond the status indicator.

Keep a fixed accessible --accent/--accent-ink for navigation, links, controls,
focus rings and composer. Treat a changing --state-hue as indicator-only, used
for the explicitly labelled activity dot/status badge; verdict colours remain
independent. Do not put status variables on an ancestor rule that changes general
text. Gate text must remain readable with an explicit status word; colour alone
is never its meaning. Keep terminal ANSI rendering and transcript content intact.
Test transitions with equal unrelated computed colours, not just string matching
or a single screenshot. No new colour timer or palette polling is necessary.

### Question data path and the missing live control boundary

Transcript parsing in `crates/cli/src/cli/web/transcript.rs` emits text, thinking,
tool and image bodies. Claude AskUserQuestion and Codex function/custom calls
currently become generic tools; tool outputs merge by call ID. There is no
structured question body, authoritative pending registry or answer endpoint.
The transcript's call identity is useful for correlating history; lack of a tool
output is NOT proof that a live request remains answerable.

The browser currently answers Clank blocks using BrowserRequest::BlockAnswer.
These are repo workflow blocks, not harness request_user_input/AskUserQuestion.
Do not repurpose that endpoint, chat /say, event ack/reply or wake receipts to
answer a native harness request. `crates/web/src/request.rs` carries validated
browser actions through the existing bridge to the repo adapter. Extend this
route with an explicit QuestionAnswer action and correlated reply only when its
provider owner is established. Use the existing binary bridge and normal live
updates, not another question-poll stream or service.

Current primary references checked on 2026-10-04:

- Claude hooks: https://code.claude.com/docs/en/hooks#pretooluse-decision-control
  AskUserQuestion's questions/options and optional answers are structured. A
  PreToolUse hook can return allow with updatedInput containing the original
  questions plus answers keyed by question text. This is a viable live answer
  candidate; allow alone does not supply an answer. The SDK canUseTool callback
  also receives the question, but starting a separate SDK process would not
  answer the existing TUI. Defer/resume is explicitly a non-interactive flow,
  not a way to attach to this interactive session.
- Claude SDK: https://code.claude.com/docs/en/agent-sdk/user-input
  Distinguishes tool approval from clarifying questions and documents the answer
  payload. Preserve multi-question, multi-select and free-text semantics.
- Codex: https://learn.chatgpt.com/docs/app-server
  item/tool/requestUserInput carries a native request, including possible provider
  auto-resolution timing. serverRequest/resolved can mean an answer OR lifecycle
  cleanup. thread/read does not subscribe; a transcript or read-only socket is
  insufficient to recover pending requests and deliver answers.
- Codex primary request tracking:
  https://github.com/openai/codex/blob/main/codex-rs/app-server/src/outgoing_message.rs
  Stores pending thread requests and consumes a callback on response. Ordinary
  questions differ from owner-restricted user-verification requests. This makes
  request replay and first-answer arbitration plausible integration points,
  but the internal pending_requests_for_thread helper alone is not a public
  attach/answer API. Verify the exposed subscription/replay path before using it.

Two concrete prerequisites remain for the question implementation. Claude's
hook candidate must establish cancellation, hook timeout/fallback, and a usable
local-terminal answer path: simply holding PreToolUse would delay the native
terminal question, so it cannot silently claim simultaneous native terminal and
remote answering. Codex needs an ownership-verified live endpoint and supported
subscription/replay that neither loads a saved thread elsewhere nor replaces its
TUI's active request. The current master's control socket is absent, as observed
in the preceding plan; do not create a second server and call it an attachment.
These are explicit transport acceptance tasks, not reasons to omit questions.
Transcript-only rendering can ship before transport, but must show View in
Terminal rather than a Submit control that cannot answer the request safely.

### End-to-end question contract to implement

Normalize each provider question into a Question entry with canonical repo,
agent label, actual provider, exact bound session, provider request/item ID,
turn ID when present, owner generation, question revision and provider-shaped
question fields. Preserve stable question IDs, headers/prompts, choices,
selection multiplicity, custom-text/secret restrictions and provider deadlines.
Do not substitute list indexes or question text for a provider identity. A
history entry and live owner can merge only when these identities match.

The repo-side owner supplies authoritative pending/answered/cancelled/stale or
unavailable state and a monotonically changing revision. Browser sending is
local presentation state, not proof the provider accepted an answer. A resolved
notification without known cause is Resolved, not falsely Answered. Connection
loss is unknown availability until reconciliation; do not keep an enabled stale
Submit button. Question content is escaped/rendered through existing safe paths.

An answer captures the complete identity and revision plus a client attempt ID.
The repo adapter rechecks roster/session/owner generation and request revision,
validates every answer against the native form and uses one owner to serialize
browser submissions. Native provider arbitration remains authoritative against
simultaneous terminal answers. Repeated clicks and replay of the same attempt
return its recorded result rather than another provider response. A concurrent
terminal answer/cancellation resolves the old entry and refuses a late answer.
Do not resend after an uncertain mutation; retain the draft and reconcile native
state first. Definite validation/transport failures before mutation may be retried.

On remote attach/reconnect, obtain the owner's pending snapshot, then ordered
request/resolution updates with revision fencing. Browser disconnect does not
cancel a model question. Drop ownership on harness/session replacement and reject
all previous request keys; forks/clear cannot inherit an old request. Cancel and
close a hook/socket with its owning process/connector, without detached readers.
One shared provider subscription per live session only while an active connector
needs it is preferable to per-viewer connections. Claude helpers, if needed,
exist only for an actual question. No timer scans, periodic questions/list calls,
per-event permanent waiter or new idle daemon. Provider auto-resolution uses its
own contract, never an invented Clank answer deadline. All answer effects remain
separate from user-event receipt, completion and required-reply contracts.

### Three implementation milestones and acceptance

1. Navigation/identity: build dedicated Team and agent Settings panels, direct
   roster navigation, one plan link, and profile-scoped browser disconnect. Keep
   existing backend operations and credential lifecycle. Validate keyboard and
   320/390/768/desktop widths, focus return, per-agent drafts, roster/session
   changes, hosted/no-signout cases and forgetting/provisioning races. Include
   locally rendered desktop and mobile previews; no image generation is needed.
2. Questions: prove the provider ownership/answer prerequisites above, add typed
   timeline entries and browser requests, then wire state/replay/arbitration.
   Use fixtures for question forms, revision/generation fencing, duplicates,
   local answer races, cancel/timeout and reconnect. Verify an authenticated live
   Claude and Codex question/answer round trip separately from fixtures. If a
   supported live channel is unavailable, keep its terminal fallback explicit
   and record the precise limitation rather than fabricate success.
3. Style isolation: replace page-wide state accent mutation, scope indicators,
   and verify actual computed styles across repository states and responsive
   layouts. Recheck navigation/question focus and readable status/verdict labels.
   Capture the finished views for review, then refresh installed remote assets
   through the existing build/install path after implementation review.

Code ownership: page navigation/rendering in crates/web/assets; provider parsing
and live request control in the repo's crates/cli web adapter; shared browser
operation/data types in crates/web; authentication in the existing web-cloudflare
door. Keep provider protocol knowledge out of browser string heuristics.
Existing Rust asset/bridge/host tests and the CLI web boundary fixtures guard
these seams. Node is available locally; no Playwright package or exposed browser
MCP tool is currently installed. Establish a local headless-browser preview tool
for the implementation milestones instead of claiming visual checks from source
inspection. This research milestone has only changed the plan, so no new code
tests or live provider/account actions were run. Whitespace validation passes.

## Milestone: navigation and browser access implemented

The main pane now has Chat, Terminal, Team and Settings tabs, with Timeline at
phone/tablet widths. Team holds roster addition, removal, review tier,
promotion and replacement. Settings belongs to the explicitly selected agent
and holds its invocation, session binding, pane state, auto mode, session clear
and pane reopening. Opening Settings pins its agent so an automatic turn change
cannot silently change the settings subject. Controls retain the existing backend
operations, including the exact session identifier on a clear request. Rebinding
invalidates an armed clear confirmation. Replies to settings operations do not
write their messages into another agent's settings panel.

The sidebar directly lists each agent's name, role, provider, activity and pane
state. Local provider monograms (Co, Cl, G, OC) supply the fallback identified in
the contract; there are no external logo requests. Choosing a roster member pins
that agent, returns to its saved Chat/Terminal mode, and keeps its unsent draft
separate from other agents' drafts. Drafts are held in this page's memory, not
persisted across reloads. Explicit agent changes during upload/send are held until
that operation finishes, so a late response cannot attach/restore text to another
recipient. Team replacement drafts and the existing outbox survive navigation.
Queue and stash entries have one named link each, with a full-name tooltip and
accessible text, followed by priority or commit metadata. Plan editors/actions
remain on their existing revision-aware pages with Back/focus handling.

Browser access is a profile disclosure, not a device settings page. Escape
closes it and restores profile focus; clicking outside closes it. Built-in
Disconnect keeps the existing door state machine and same-origin forget POST;
hosted access uses its configured sign-out endpoint, or explains host ownership
without offering a nonfunctional sign-out action. No credential is displayed.
The page now maps the door's login navigation to the configured login path, so
mounted pages do not silently jump to a root login path. Storage refusal remains
nonfatal. This milestone introduces no periodic backend requests, subscriptions
or additional timers.

Validation:

- `cargo test -p clank --test it web_timeline_boundary::`: two tests passed. The
  existing boundary now recognizes roster navigation instead of the removed
  management links and still refuses forms/controls or mutations in Timeline.
- `cargo test -p clank-web assets::tests::`: three tests passed, including mount
  configuration, hosted login/sign-out and built-in asset embedding.
- `cargo fmt --check`, JavaScript syntax compilation and `git diff --check`
  passed.
- Chromium/Playwright used the actual page, vendored xterm/font and inlined
  composer/door scripts, with isolated status/WebSocket/backend fixtures. The
  reproducible fixture is `/private/tmp/clank-navigation-browser.cjs`; Playwright
  was already available under `/private/tmp/clank-bridge-browser-validation`.
  No live agent, team, credential or session was changed by these browser tests.
  Checks cover separate agent drafts/modes, Team replacement draft preservation,
  selected-agent settings, exact-session clear confirmation and invalidation on
  rebind, desktop/mobile plan Back, keyboard Arrow/Home/End tab navigation,
  profile Escape, hosted/no-signout cases and layouts at 320/390/768/1280px.
  Built-in forget failure retains credentials and permits retry; success clears
  them, late provisioning is suppressed while sealed, and host sign-out failure
  also permits retry. Refused browser storage does not break navigation/sign-out.
  All checks passed with no uncaught page errors.
- Screenshots inspected: `/private/tmp/clank-navigation-desktop.png`,
  `/private/tmp/clank-navigation-mobile-timeline.png`, and
  `/private/tmp/clank-navigation-mobile-settings.png`. Visual inspection caught
  and corrected a leftover plan-view grid-row rule that displaced Settings.

Questions and style isolation remain the next two milestones. This commit does
not claim a native Claude/Codex question owner or enable answer submission. The
existing global hue still needs the specified style isolation. Installed remote
assets will be refreshed after implementation review as planned.

## Milestone: recorded questions are visible; live ownership remains next

This is the display portion of the question milestone, not completion of its
native-answer acceptance. The transcript adapter now derives typed question
metadata only for Claude AskUserQuestion and Codex's native request_user_input
and request_user_input_async names (including the functions namespace). It keeps
all original tool input and output. Questions preserve provider, exact text,
Codex field IDs, headers, options/descriptions, multiple selection and private/
other flags. Codex free-text fields accept absent/null options; background tool
fields accept title/string-choice input. Unknown or malformed shapes remain
ordinary tool calls rather than losing content. No truncation or count limit
was introduced. Old retained tool bodies deserialize with no question metadata.

The browser renders question cards directly in Chat, outside command groups.
It uses text nodes for every provider string. Cards show all normalized prompts
and choices, and View in Terminal when the selected agent has a live pane. They
explicitly say live answers are not connected here yet. A recorded result does
not produce an Answered state: it may be a cancellation or another tool outcome.
Result details for private-answer questions are hidden and their text is not
inserted into the DOM; this is display handling, not transport-level redaction.
The existing feed correlates result updates to the same call ID and fences them
by agent/session/generation/transcript owner. Browser rendering handles an old
ordinary tool becoming a typed question without duplicate cards or closed-command
groups hiding it. No answer endpoint, hook, native subscription, timer or new
polling was added in this step.

### Updated native protocol evidence and next implementation boundary

Rechecked the [official Codex app-server documentation](https://learn.chatgpt.com/docs/app-server)
and [Claude hook contract](https://code.claude.com/docs/en/hooks#pretooluse-decision-control).
Generated the installed Codex binary's experimental JSON schemas into
`/private/tmp/clank-question-codex-schema` without starting a model turn or
changing its configuration. ToolRequestUserInputParams requires threadId,
turnId, itemId and isBlocking. autoResolutionMs is deprecated there; do not invent
a deadline from that field. Responses map field IDs to arrays of answer strings.
The generated client methods include resume/unsubscribe and elicitation counters,
but no standalone pending-question snapshot method; resume's response does not
carry a pending-request collection. This does not prove replay is unsupported.
The next step must demonstrate pending request delivery/replay on the exact
already-loaded owner connection before enabling Submit. Official documentation
also distinguishes thread/read from subscription and notes that
serverRequest/resolved can mean lifecycle cleanup rather than a user answer.

Current Clank native Codex send/wake RPC clients are one-shot controls, not
persistent request owners. Do not route a response through their numeric client
request counter or a transcript call ID. Prove subscription/reattachment on the
correct live profile/thread/cwd, share that connection for the active connector,
then retain native request identity and resolution state. For Claude, prove the
PreToolUse updatedInput answer path and the concurrent local-terminal/fallback
contract before installing a question hook. Neither a new CLI process resuming
another server's saved session nor a synthetic terminal keystroke establishes
native ownership. The planned session/revision fencing, first-response
arbitration, reconnect, cancellation and uncertain-mutation handling still apply.
No authenticated live Claude/Codex question/answer round trip has been claimed.

Validation:

- `cargo test -p clank --lib cli::web::`: 50 tests passed; five bridge tests hit
  sandbox Unix-socket permission errors. Rerunning the bridge module with socket/
  child-process access (`cargo test -p clank --lib cli::web::bridge_instance::`)
  passed all six tests, including large transcripts and connector outages.
- New tests exercise Claude and Codex normalization, background/free-text fields,
  exact keys, private flags, malformed/foreign tools, untruncated input, older
  serialized bodies and same-ID result updates fenced against obsolete owners.
- `cargo test -p clank --test it web_timeline_boundary::`: both tests passed.
  JavaScript syntax compilation, formatting and whitespace checks passed.
- Isolated Chromium fixture `/private/tmp/clank-questions-browser.cjs` passed:
  multiple/free-text prompts, questions outside command groups, an ordinary tool
  becoming a typed question, HTML-looking text remaining inert, Terminal
  navigation, obsolete-session result rejection, recorded/cancel wording,
  private-result DOM suppression and 320px layout. It also reran the preceding
  navigation, draft, keyboard and credential lifecycle checks without page errors.
  Screenshots inspected: `/private/tmp/clank-question-desktop.png` and
  `/private/tmp/clank-question-mobile.png`.

Next remains native question ownership/answers, followed by colour isolation and
installed asset refresh. The read-only card is an explicit incremental fallback;
it does not satisfy or remove the plan's live-answer requirements.

## Milestone: installed Codex question ownership prerequisites verified

Added an explicitly invoked, ignored integration test in
`crates/cli/tests/it/codex_question_protocol.rs`. It runs the installed Codex
app-server with a disposable canonical workspace/profile, cleared environment,
a dummy provider key and a localhost Responses fixture. It uses neither user
credentials nor external inference, and never attaches to a real agent session.
The fixture owns and reaps its child process group on success and failure.
Its startup/event deadlines and startup socket checks are test machinery;
this milestone adds no production process, timer or question polling.

The acceptance demonstrates:

- A client checks that the target thread is already loaded and its recorded cwd
  matches before subscribing through thread/resume. Unknown threads and wrong
  workspaces are rejected before resume. This tests the intended attachment
  policy; it is not yet a production profile/session ownership implementation.
- A second subscribed client receives the identical pending native request ID,
  thread/turn/item identity and content. Closing and reattaching that client
  preserves and replays the pending question without cancelling it.
- A terminal-side response resolves the request for both clients. A subsequent
  conflicting remote response cannot replace the first answer; the mock model
  receives the first value. This tests a late duplicate, not every ordering of
  simultaneous competing responses.
- Default-mode questions report isBlocking=false. The remote client can answer
  such a question and the mock model receives its value. Plan-mode questions
  report isBlocking=true and retain their exact native identity.
- Interrupting a pending turn also broadcasts serverRequest/resolved. Resolution
  alone therefore cannot establish that a remote answer won or that the user
  answered at all. The UI must retain uncertainty when it lacks answer evidence.

This closes the preceding pending-request replay uncertainty for the installed
binary. The upstream [request replay implementation](https://github.com/openai/codex/blob/main/codex-rs/app-server/src/outgoing_message.rs)
and [question protocol tests](https://github.com/openai/codex/blob/main/codex-rs/app-server/tests/suite/v2/request_user_input.rs)
provide the primary protocol references; the installed-runtime test is the
compatibility evidence. Run it explicitly with:

```sh
CLANK_CODEX_PROTOCOL_BINARY=/Users/llfourn/.local/bin/codex \
  cargo test -p clank --test it codex_question_protocol:: -- --ignored
```

Validation: the installed-runtime test passed, covering blocking/background
questions, second-client answers, terminal resolution, replay after disconnect,
foreign attachment rejection, late duplicates and cancellation. Formatting and
whitespace checks passed. This is isolated runtime acceptance, not an
authenticated live terminal/UI round trip.

Next implement the connector-owned native subscription, retained pending state
and validated browser answer operation. Keep one event-driven connection per
bound live session, fence by repo/agent/profile/session/request generation, and
invalidate it on clear/rebind/connector shutdown. A resumed saved session in a
new process is never a substitute for attaching to the actual owner. Preserve
input on failed or uncertain sends; do not blindly retry a possible mutation or
claim Answered from socket delivery or resolution alone. Claude's hook/local
terminal arbitration still needs its own prerequisite proof. Native answer
wiring, colour isolation and installed asset refresh remain unfinished.

## Milestone: repository state no longer recolours ordinary UI

The page now has a fixed accent/contrast palette for navigation, links, controls,
focus rings, composer borders and the block strip. Status updates set only
`--state-hue`; its consumers are activity/away dots and a new dot beside the
explicit gate word. Gate text uses ordinary ink. Unknown review verdicts use a
neutral mark, while known verdicts keep their independent semantic colours.
Queue/new-plan/stash links use the darker accent consistently for readability.
No transport, status payload, timer, polling or terminal ANSI change is needed.

Added `crates/cli/tests/browser/colors.js` using the existing browser harness and
documented its invocation. This checks actual computed colours, including a
focused composer, prose links, question cards, queue/stash navigation, block
controls, gate text and historical verdicts. Across green/yellow/red/purple and
missing-hue statuses, ordinary styles remain identical while status dots change;
readable gate words and draft text survive. At 320/390/768/1280px the page stays
within its viewport and state changes still leave those colours unchanged.
Canvas-resolved text/link/button contrast meets 4.5:1 for the sampled surfaces.

Validation:

- `NODE_PATH=/private/tmp/clank-bridge-browser-validation/node_modules
  CLANK_CHROMIUM=/Applications/Google\ Chrome.app/Contents/MacOS/Google\ Chrome
  node crates/cli/tests/browser/colors.js` passed using local Chromium and
  synthetic frames. No live agent, credentials or repository roster was changed.
- Inspected the yellow-state desktop and 320px Timeline screenshots under
  `/private/tmp/clank-stable-colors-{1280,320}.png`. Yellow is confined to the
  explicit status dots; readable text and the purple action palette stay stable.
- Existing isolated question/navigation fixtures passed again: typed cards,
  inert strings, private-result display suppression, stale-session fencing,
  draft/mode preservation, scoped settings, keyboard navigation and browser
  disconnect/credential races remain intact.
- `cargo test -p clank-web assets::tests::`: all three asset tests passed.
  Formatting and whitespace checks passed.

Live question submission/ownership remains the unfinished implementation
milestone, including Claude's terminal arbitration proof. The installed asset
refresh still follows implementation review; this commit does not install the
page or claim that read-only question cards can accept answers remotely.

## Milestone: connector-owned native Codex question subscriptions

`BridgeInstance` now owns the Codex question subscriptions for its lifetime.
Existing status observations reconcile targets; this adds no question poll,
helper process, server spawn or saved-session restoration. Each bound Codex
agent gets one task/socket only when its profile advertises an accessible native
socket. Unchanged targets retain the same owner and never open another socket.
The profile comes from the target's CODEX_HOME/HOME launch overrides or the
supplied default home, never the monitor's CODEX_HOME. Relative profile paths
are refused. Current Codex advertises its daemon socket through a symlink, so
this owner resolves that candidate and verifies socket type and uid. Native
loaded-thread and canonical cwd checks still authorize the actual attachment.

Each attachment initializes with string client RPC IDs, verifies loaded/read
identity, rechecks the binding, and subscribes with excludeTurns. Native requests
that precede resume's reply are retained rather than discarded or mistaken for
client replies. Attachment has a finite unresponsive-server deadline; a healthy
idle socket simply awaits native frames with no periodic probe. The negotiated
optOutNotificationMethods capability suppresses high-volume text/reasoning,
command/file output, item-history, token-usage and diff/plan notifications.
Question requests, request resolution and turn completion remain enabled.

Native request windows preserve the server request ID, thread/session, turn,
item, blocking flag and typed question fields. Duplicate replay is idempotent;
a reused ID with changed content or malformed native question makes this channel
unavailable instead of authorizing guessed input. Requests for other threads do
not populate the window. Resolution and turn completion remove pending requests;
there is no invented Answered state or retained private answer output.

A random connector epoch plus monotonic generation fences each owner. The Feed
claims/replaces/removes its window synchronously before cancelling an obsolete
task. Binding metadata, profile socket identity or agent membership changes
replace the owner; old tasks cannot publish into the new window. A duplicate
binding of one session to multiple roster labels disables those subscriptions.
Stop/drop invalidate windows and abort their owned tasks; normal stop also joins
them. Native connection failure clears pending authority and records a Terminal
fallback. Unchanged failed targets do not spin on reconnect attempts.

The existing binary bridge now carries a retained questions frame. The shared
host replay reducer replaces this native window atomically and accepts an empty
window to clear it, so slow connectors and late browsers receive current state.
The large-harness/outage regression caught the missing reducer support during
implementation; it passes with that integration in place. Existing browsers
ignore the new event until the answer UI is wired; recorded question cards remain
explicitly read-only in this milestone.

Validation:

- `cargo test -p clank --lib cli::web::`: 61 web-adapter tests passed with local
  socket/child-process access, including the large transcript/outage and startup
  image regressions. After adding the final quiet-target case, all seven native
  question tests passed again.
- Native tests cover exact request/replay identity, obsolete owner/removal
  fencing, resolution/cancellation without an answer claim, malformed requests,
  reused IDs, replay before RPC completion, foreign workspace/unloaded thread/
  changed binding rejection before resume, negotiated notification suppression,
  profile-selected symlink socket delivery, one idle subscription, native
  resolution, rebind invalidation and joined shutdown. Test-only timeouts verify
  absence of extra traffic; production has no idle question timer.
- `cargo test -p clank-web replay::tests::`: both host replay tests passed,
  including new/rebound/empty question images and malformed window rejection.
  Formatting and whitespace checks passed. Only isolated fixtures were used;
  this is not an authenticated live terminal-to-browser answer round trip.

Next add the serialized, revalidated browser answer operation and correlate its
native outcome without treating bytes/resolution as proof an answer won. Native
recovery after a transient loss at the same socket remains explicit unfinished
work: it must be triggered by lifecycle/user activity or a bounded recovery
operation, not a continual idle retry loop. Browser reconnect itself already
receives the retained window without opening another native socket. Claude's
hook/local-terminal arbitration proof and installed asset refresh also remain.

## Milestone: native Codex answer operation and editable cards

The shared browser service now accepts an admitted, same-origin
`POST /questions/answer`, carried as a typed QuestionAnswer through the existing
binary connector. It names agent, bound session, connector owner generation,
native request ID, turn/item, client attempt identity and answers keyed by the
native field IDs. Native IDs are JSON-encoded opaque strings in browser frames
and requests, preserving u64 precision; only the owner uses the original ID in
the JSON-RPC response. This route adds no answer byte/count ceiling or truncation;
a real admitted HTTP/connector test passes a 100 KB answer intact.

RepoSession shares the BridgeInstance's owner rather than opening an answer
socket. Each owner serializes its answer commands alongside native lifecycle
frames, preferring an already-readable resolution. It validates exact pending
identity, current binding/profile and new duplicate-session ambiguity before
writing. Answers must cover exactly the native fields, obey single/multiple
selection, contain nonblank values and use offered choices unless free text or
Other is allowed. Field IDs must be nonempty and unique. Invalid/stale requests
produce no native write. The actor records attempt identity and a local payload
digest before awaiting transport; identical retries do not write again and
competing attempts cannot replace the sent answer. Attempt digests are discarded
when their native requests close. The command mailbox permits one waiting command
and refuses additional queued work rather than accumulating an unbounded burst.

The protocol does not provide a correlated acknowledgement proving which
competing client answer won. A successful write returns Submitted (202), with
acceptance explicitly unconfirmed. Resolution removes answer authority without
inventing Answered. A retained last-turn completion distinguishes a known
interrupted/failed turn from generic resolution; it does not claim acceptance.
Write errors/deadlines close the owner and return an unknown outcome, forbidding
another blind attempt. Stopping/rebinding during an outstanding operation also
retains uncertainty where delivery cannot be proved. No answer plaintext is added
to question replay frames, logs or the idempotency ledger.

The page renders pending native questions directly in Chat, even before a
transcript is available, while keeping transcript-delivery capability separate
from question-only Chat availability. It merges an arriving recorded call into
the stable native card instead of duplicating the question or rebuilding focused
inputs. Cards support multiple questions, radio/checkbox choices, Other and
free-text fields, and masked private inputs with autocomplete disabled. Provider
strings remain text nodes. The private input is local page memory, never browser
storage or another viewer's replay. The existing transcript's raw recorded tool
output remains a separate transport contract; this step does not claim to redact
that existing transport. Private question result text still stays out of the DOM.

Forms preserve drafts/focus across replay and agent navigation. Repeated clicks
are disabled immediately. Rejections that prove no send remain editable;
Submitted/unknown outcomes remain locked. Native resolution/cancellation or a
rebind arriving before the HTTP reply cannot be reopened by that late reply.
Unknown state survives a native socket loss and subsequent unavailable snapshots.
Session changes remove obsolete forms/drafts from the page; a captured in-flight
reply cannot retarget a new session. Closed drafts offer explicit discard.
Undrafted closed metadata follows retained history/the last native turn rather
than growing forever. There is no new poll, timer or browser storage mechanism.
Recorded-only cards remain read-only and contain no form controls for answering.

Validation:

- `cargo test -p clank --lib cli::web::`: 64 tests passed with local fixture
  socket/process access. New acceptance covers exact native response payload,
  invalid field rejection, identical retry/competing-attempt suppression,
  post-resolution refusal, stale turn/binding rejection, broken-socket unknown
  outcomes, choice validation and untruncated private/free-text payloads. Existing
  large-transcript/outage/startup and owner lifecycle regressions still pass.
- `cargo test -p clank-web service::tests::`: seven tests passed. The mounted
  admitted service test was subsequently extended and passed again with an actual
  100 KB native-answer POST through the shared connector, opaque u64 identity,
  browser revocation and disconnected-action refusal. Private payload validation
  does not echo the private value in errors.
- New reproducible `crates/cli/tests/browser/questions.js` passed in isolated
  local Chromium: native-only cards, multi/Other/free/private fields, inert strings,
  opaque IDs, focus/draft preservation, recorded/native merge, duplicate clicks,
  cancellation before HTTP reply while switching agents, uncertain delivery/replay
  fencing, native loss before HTTP reply, session clearing, explicit draft discard,
  private storage exclusion, UUID fallback and 320/390/768/1280px layouts.
- Existing question/navigation fixtures and computed-colour acceptance passed.
  Syntax, formatting and whitespace checks passed. Inspected native form previews
  `/private/tmp/clank-native-answer-{1280,320}.png`: private values are masked,
  long option labels wrap, and the focused lower fields/Submit remain usable in
  the scrolling Chat pane. Preview fixtures use synthetic frames and no account.

This is a working Codex owner-to-form-to-native-response path under fixtures,
not an authenticated live acceptance claim or installed rollout. Remaining work:
explicit native connection recovery with preserved uncertainty and no idle retry;
Claude's hook/local-terminal arbitration proof and integration; authenticated live
round trips where available; and the final reviewed build/asset refresh. The
question protocol's missing winner receipt must remain visible rather than
manufacturing an Answered state from writes or resolution.

### Explicit native connection recovery

The admitted `POST /questions/reconnect` action addresses the exact agent,
session and existing owner. It never contains or dispatches an answer. The
connector revalidates the current binding, profile socket and unique native
session before starting a replacement subscription. It refuses a stale owner or
an already running/finishing connection; concurrent clicks cannot create another
socket. Finished tasks are reaped during recovery. The unchanged failed target
still stays quiet: no retry loop, idle timeout, poll, new helper process or new
background resource is introduced. Attachment retains its finite operation
deadline; healthy native readers continue to block on socket activity.

A dropped socket now keeps pending request metadata and delivery fences. Recovery
uses the same owner and rebuilds pending authority from the native resume replay,
including requests arriving after the resume result. Exact request identity
(id, turn, item and question) restores attempted delivery state. Originally
unsent drafts may be answered after recovery; Sending/Submitted requests cannot
become fresh sends. The actor refuses a replayed attempted answer with an unknown
outcome rather than writing it again. This fence contains no answer plaintext
and survives the actor replacement. Resolution and turn completion remove it.
Missing replayed questions become read-only resolved cards; none of these states
claims which client's answer won.

Chat offers explicit reconnect both on unavailable question cards and at the
native connection level when there is no pending question. Repeated unavailable
frames preserve uncertain drafts. Successful recovery preserves their locked
state and enables only previously unsent drafts. Recovery requests name captured
identities, so changing agents/sessions while HTTP is outstanding cannot redirect
a reconnect or its notice. Native frame updates clear old connection notices.
The recorded-only fallback now says there is no pending native answer authority,
rather than implying Codex answers have never been connected.

Validation: the native socket tests pass, including actual disconnect and
same-socket reconnect, duplicate/stale recovery refusal, late native replay,
retained Submitted state and zero second response writes. The affected web adapter
suite and seven service tests pass. Browser question acceptance covers explicit
reconnect payloads without answer fields, zero extra answer requests, preservation
of unknown drafts, re-enabled unsent drafts, and recovery with an empty pending
set. Existing computed-colour checks, formatting and whitespace checks pass.
These remain isolated fixture tests, not a live-account or installed-rollout claim.

Remaining: Claude hook/local-terminal arbitration proof and integration;
authenticated live acceptance where available; reviewed build and installed asset
refresh. Native winner receipts remain unavailable and must not be invented.

### Installed interactive Claude arbitration proof

Added `claude_question_protocol::` to the single integration harness, with
`fixtures/claude_questions.py` as its standard-library-only installed-provider
fixture. It launches Claude 2.1.280 in an actual PTY, not `-p`, with an isolated
HOME/CLAUDE_CONFIG_DIR, a fixture-only API key and localhost model responses.
It installs hooks only inside that temporary profile. The fixture controls its
own process group, reaps its child and closes its model server/PTY. No account,
real agent session, installed Clank hooks or production assets are changed.

The interactive evidence resolves the previously unproven local-terminal issue:

- PermissionRequest runs alongside the actual AskUserQuestion terminal dialog.
  Parking that hook does not hide the dialog. Returning its allow decision with
  the unchanged original questions and an answers map resolves the question.
  The fixture's remote South answer reaches the native tool result and next
  model request exactly once.
- Answering North in the terminal while the permission hook is parked produces
  a native North result. An opposite late South hook response does not replace
  it. This release can keep a PermissionRequest hook alive after a local answer,
  so production cleanup must observe native completion rather than assume the
  hook has been killed by local selection.
- Escape rejects the native tool call and cancels the parked permission hook,
  while the interactive Claude process remains alive. The fixture verifies that
  the answer channel has lost its reader before terminating Claude itself.
- PreToolUse supplies the real tool_use_id; PermissionRequest does not. Both
  supply matching session_id, prompt_id and original tool_input. PostToolUse
  supplies the same tool_use_id/session and its actual tool_response.answers.
  The fixture checks the returned North/South winner, not just hook stdout,
  HTTP delivery or a tool-resolution notification.

The documented mechanisms agree with this installed acceptance:
[Claude's hook reference](https://code.claude.com/docs/en/hooks) describes
PermissionRequest updatedInput and AskUserQuestion's original questions plus
answers keyed by question text. The production design below is an implementation
contract inferred from the interactive fixture, not a claim that it is installed.

#### Concrete Claude integration contract for the next implementation

1. Use the existing repo-owned connector/monitor lifetime for the broker. Hooks
   contact only an already available local question owner; they must not launch
   a web host, spawn an idle helper or fall back to watching files. No active
   remote owner means immediate normal terminal behavior. Transport negotiation
   must distinguish an unsupported old monitor from a broken connection without
   inventing answer authority.
2. A fast PreToolUse companion registers the immutable native tool_use_id,
   session_id, prompt_id and original tool_input, then returns normally. Validate
   canonical cwd against the repo and derive the unique Claude agent from its
   current roster binding. The hook never declares an agent's role. Correlation
   metadata is volatile, owned by that native request and retired with it.
3. PermissionRequest matches that pre-registration by session, prompt and exact
   input. Only a single unambiguous candidate grants remote authority; a missing
   or ambiguous match leaves the native terminal in control. Generate a fresh
   owner/request capability and expose the real tool id for transcript merging.
   Hold only this per-question hook connection while the native dialog remains
   usable. Revalidate the binding before publication and immediately before
   returning a remotely supplied answer.
4. Reuse the admitted browser QuestionAnswer route, exact authority checks,
   field validation, one-attempt fence and private in-page drafts. Claude fields
   receive stable per-request IDs corresponding to the original question array.
   Convert the validated response to Claude's answers map keyed by original
   question text; multi-select uses its native joined-label representation.
   Duplicate question text cannot represent distinct map entries and must remain
   terminal-only rather than silently overwrite one answer. Preserve the entire
   original input, including unknown supported fields, when adding answers.
5. Write a PermissionRequest allow decision plus updatedInput to the exact
   parked hook. This proves only Submitted. A failed/closed response channel is
   an unknown outcome, never permission to retry. A local answer can race this
   response; native Claude arbitrates. No synthetic terminal keystrokes are used
   for submitting answers, and the integration must not approve other tools or
   change native permission rules.
6. PostToolUse is the correlated completion receipt. Match native session,
   prompt/tool id and original questions before retiring authority. Compare its
   actual answers to the submitted normalized response in memory to distinguish
   the remote attempt from a local winner. Publish only completion/winner state,
   never private answer values or raw hook payloads into shared question replay.
   A late remote response cannot override a completed request. Release any still
   parked PermissionRequest connection on this receipt, as the fixture proves
   local answers do not always kill it themselves.
7. Hook cancellation/EOF retires the request without an Answered claim; known
   tool failure may use the matching PostToolUseFailure receipt. Rebind, clear,
   provider/profile replacement, bridge stop and terminal cancellation invalidate
   authority before releasing channels. Prompt lifecycle transitions also clear
   abandoned pre-registrations. Recovery can reattach a still-live hook owner;
   an ended hook cannot be reconstructed from a transcript. A stale recorded
   question remains readable with Terminal fallback, never a new answer form.
8. Keep all waits event-driven and tied to existing process/socket/native hook
   events. There is no idle retry interval, transcript scan for pending requests,
   deadline that submits a default answer, or continuously running hook helper.
   Provider-imposed hook termination is cancellation, not a completed answer.

Validation: `CLANK_CLAUDE_PROTOCOL_BINARY=/Users/llfourn/.local/bin/claude cargo
 test -p clank --test it claude_question_protocol:: -- --ignored` passed against
installed Claude 2.1.280 (remote, local/late competitor and cancellation cases).
The fixture also passed directly after adding the exact PostToolUse winner
assertion. Formatting and whitespace checks pass. This milestone proves native
interactive feasibility, not the as-yet unimplemented Clank Claude broker.
Remaining work is that broker/hook/form integration, its lifecycle fixtures,
and the reviewed build/installed asset refresh.

### Claude broker, native hooks and remote forms

The Claude path is now wired into the repo adapter. `claude_questions::Registry`
registers the immutable PreToolUse identity, holds only its exact matching
PermissionRequest connection, validates browser answers, and reconciles native
PostToolUse/PostToolUseFailure receipts. It derives the unique agent from current
roster/session bindings and checks canonical repo ownership; it never accepts a
role from the hook. Captured agent descriptions fence launch/profile changes.
Duplicate question text and ambiguous permission correlations decline remote
ownership and retain ordinary terminal behavior. Full original input survives
answer conversion, including additional native fields. Stable per-request field
IDs map browser controls to Claude's original question text.

The hidden `clank question-hook` command is a native stdin/stdout adapter. Setup
installs it only for AskUserQuestion under PreToolUse, PermissionRequest,
PostToolUse and PostToolUseFailure. Installation is idempotent and preserves
unrelated hooks, mixed wrappers and wrapper metadata. Provider inspection uses
the same definitions. The adapter connects only to an existing broker: missing,
unsupported or inaccessible owners return an empty decision immediately. It does
not spawn a web host, alter tool permissions, or park non-question hooks.

One Unix listener belongs to the existing BridgeInstance and uses the existing
uid-owned external repo runtime namespace. A retained lease prevents duplicate
owners; same-uid socket checks, mode 0600, canonical repo identity and protocol
negotiation fence discovery. This is a separate narrowly scoped hook endpoint,
not a change to monitor wait negotiation. Starting the endpoint requires no
additional process. The healthy accept loop and held hooks block on socket or
channel events with no timer, polling, file scan or idle retry. Existing status
observations skip registry work entirely when there are no question contexts;
otherwise they resolve the roster/read bindings once rather than rediscovering
Git for every retained question. Finite deadlines
bound only initial communication and output writes. Bridge stop invalidates
registry authority first, then shuts down and joins broker connections. Drop
also invalidates before aborting the listener. Completed connection tasks are
reaped by the owned JoinSet. Broker startup failures warn and retain Terminal
fallback instead of claiming that a live remote question owner exists.

The shared admitted QuestionAnswer route now dispatches Claude bindings to this
registry. Exact agent/session/owner/native-id/prompt/item identity and current
binding/profile precede any write. A single attempted-answer fence prevents
repeat sends. A socket write proves Submitted only; failure remains unknown.
Private/free-text answer values exist only in the response path and native hook
input, not in shared question replay or its digest ledger. Existing recorded
transcript transport is still a separate contract, not a new redaction claim.

Native completion compares the actual normalized answer map against the submitted
map in memory. The UI says Claude confirmed these answers only for a matching
receipt, and otherwise says Answered in Claude. Identical values from competing
inputs cannot prove which input won, so the wording deliberately does not claim
remote authorship. Native failure closes the form. Completion releases any
still-held permission connection without submitting another decision. A late
HTTP reply or a later prompt snapshot cannot downgrade a known completion or
reopen inputs.

EOF alone does not prove native completion. It closes remote authority while
retaining Unavailable or unknown delivery metadata; identical pre-hook replay
cannot recreate permission to answer. A later correlated native receipt can still
resolve that metadata. Binding changes use a shared atomic generation release: old Claude cleanup
cannot clear a replacement Codex window, and old Codex cleanup cannot clear a
replacement Claude window. Closed metadata is bounded to the last question
prompt cohort, and a new question prompt retires abandoned/closed registrations.
This refines the earlier prompt-cleanup contract: ordinary prompts need no extra
hook/helper process just to delete a bounded last-question cohort. Reattaching a
browser replays a live held request through the existing bridge. An ended native
hook is never reconstructed from transcript history.

Validation completed in this milestone:

- Claude registry/socket tests cover remote receipts, differing native answers,
  private value exclusion, original-input preservation, repeat refusal, local
  completion releasing a parked hook, EOF and replay fencing, duplicate text,
  ambiguous native identity, prompt replacement, session clearing, generation
  isolation, the actual socket handler, single-owner lease and shutdown joining.
- The affected web adapter suite passes (72); setup tests pass (70), including new
  question-hook idempotence and mixed-wrapper preservation. Git-boundary and
  typed-config boundary checks pass. Formatting and whitespace checks pass.
- Browser question acceptance covers Claude forms and a native completion arriving
  before a late Submitted HTTP response; inputs stay disabled and the completion
  state survives. Existing Codex/draft/recovery checks and computed-colour checks
  still pass.

This adds production wiring with protocol fixtures. The installed-Claude PTY
proof from the preceding milestone verifies the native mechanism independently;
it has not yet been rerun with these Clank hook commands and broker end to end.
Remaining: that combined installed-provider acceptance, any resulting lifecycle
corrections, and the reviewed build/installed hook/asset refresh. No global hooks,
installed binary or real agent session was changed by this milestone.

### Combined installed-Claude and production-Clank acceptance

The ignored `installed_claude_uses_the_real_clank_hook_and_broker_end_to_end`
test now runs installed interactive Claude 2.1.280 with a real PTY, isolated
profile, fake credential and localhost model. Each native question hook forwards
its exact stdin to the freshly built `clank question-hook` command. The real
repo-scoped broker and registry own the connection; the Rust controller submits
through the production answer method. There is no fixture FIFO in this mode.
The forwarding wrapper records native identity solely in its disposable fixture
directory, not in production replay.

The combined test verifies all three paths: remote South produces Submitted
and then a correlated native completion; terminal North produces Local and
releases the parked permission hook; terminal Escape closes remote authority
without an Answered claim. Each case checks the exact bound session, native
prompt/tool identity, one native result, and rejection of subsequent answer
attempts. A controller handshake keeps Claude alive until the broker state is
asserted, so fixture teardown cannot manufacture the cancellation EOF being
tested. Fixture-owned process groups, PTYs, server and temporary profiles are
cleaned up on success and failure. No real inference or account configuration
is involved.

This acceptance exposed a native input detail missing from the earlier socket
fixtures: Claude adds `annotations: {}` after a terminal answer. Completion
matching now excludes the native response fields `answers` and `annotations`
while still comparing the original question input and other original fields.
Answer conversion continues to preserve the entire original input. A regression
test verifies that annotations cannot mask a changed question or reopen remote
authority; private native answers still never enter shared question replay.

Validation: `cargo build -p clank --bin clank` passed, followed by all nine
`cli::web::claude_questions::` tests with `--include-ignored`, selecting installed
Claude and that rebuilt Clank binary through `CLANK_CLAUDE_PROTOCOL_BINARY` and
`CLANK_QUESTION_BINARY`. Python syntax, formatting and whitespace checks pass.
This proves the installed provider plus production hook/broker path against a
local model fixture; it does not claim an authenticated live-account session.
Remaining is the reviewed build/installed hook/asset refresh. No installed
binary, global hook or real agent session was changed by this milestone.
