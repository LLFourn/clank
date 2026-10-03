# a-large-session-update-does-not-break-remote-viewing
# A large session update does not break remote viewing

## Report and confirmed boundary

The user cannot start a remote session for this Clank repo. The connector reports:
"session update is 1082662 bytes, above the 1048576-byte bridge limit; reduce the
content of this update before reconnecting". The reported endpoint is a trycloudflare
tunnel. This is a local bridge size refusal, not evidence of a Cloudflare failure.

`crates/web/src/bridge.rs::send_update` serializes the complete Wire::Update,
compares its encoded bytes with MAX_CONTROL_BYTES (1 MiB), reports UpdateTooLarge
to the host and disconnects. `send_snapshot` has the same whole-image ceiling.
The bridge already has 64 KiB binary chunks for bounded uploads, but that is not
implemented as a session-image/update transport. Existing tests explicitly assert
these size refusals. Host replay retains full images assembled from changed browser
frames, so fixing only startup or only live deltas can leave resync broken.

## User steering — no arbitrary session-size limits

The user explicitly rejects arbitrary limits. Do not replace the 1 MiB refusal with
a larger fixed maximum session/update/snapshot size. A transport chunk size controls
how bytes travel; it must not become a maximum on the amount of legitimate session
content that can be published or retained. Explain measured resource costs and actual
transport constraints, rather than choosing a number that merely clears this report.

## Desired behavior and contracts

- User requires **wincode** for binary encoding. Define explicit shared wire DTOs
  and protocol/schema versioning for the Rust repo connector and Rust host; do not
  treat the existing fold-cache format as the bridge schema. Measure and eliminate
  nested JSON/string duplication rather than merely wrapping a JSON blob in a
  wincode envelope. Preserve payload fidelity and ordering.
- Distinguish the repo-to-host bridge from the host-to-browser stream. The current
  browser is JavaScript with JSON frame decoding and no Rust/WASM decoder. Wincode
  between the two Rust endpoints alone does not require browser WASM. If milestone 1
  chooses binary frames all the way to the browser, use a small Rust/WASM codec built
  from the same wire types, with driven version and decode-failure recovery tests.
  A wholesale Rust/WASM frontend rewrite is outside this transport fix; retain the
  existing UI and terminal behavior while changing only necessary protocol seams.
- Large publications, disconnects and replay gaps are ordinary recovery paths:
  preserve the last complete image, reconnect and resynchronize automatically.
  Prefer eliminating failure states through ownership/ordering/flow control over
  adding user-visible refusals or requiring manual reconnect/content deletion.
  Never automatically retry a mutation with an unknown outcome.
- An ordinary session update of the reported size, and its initial/resync image,
  must cross the bridge without asking the user to delete task/transcript content.
  Preserve the complete logical state, UTF-8, sequence, generation and replay.
- Separate bounded transport frames from logical session payloads. Prefer
  bounded framing/chunking or structured publication splitting over simply raising
  the control-message limit or accepting unlimited WebSocket messages. A single
  oversized transcript/frame must work too, not only a bundle of many small frames.
- Impose no arbitrary logical payload or retained-image ceiling. Before implementation,
  account for per-frame buffers, concurrent transfers, temporary encoded/reassembly
  copies and retained authoritative state, using actual payloads. Bound transient
  overhead and queues through flow control, ownership and streaming/spooling where
  justified; no growing backlog behind a slow peer. Necessary authoritative state
  can scale with its content: distinguish that cost from avoidable duplicate buffers.
  Do not claim chunking alone bounds memory while assembling several full copies.
  Any numeric frame/buffer/concurrency parameter needs a concrete transport/resource
  rationale and must not silently truncate history or reject valid larger sessions.
  Report genuine resource/I/O failures honestly without discarding the last valid
  image or instructing the user to fit an invented session-size budget.
- Publish a snapshot/update atomically only when its complete validated transfer
  arrives. Partial bytes, disconnects, stale generations, duplicate/out-of-order
  chunks, declared-size mismatches, timeouts and replay gaps must never replace a
  valid image with partial state or advance its sequence incorrectly.
- Keep backpressure and revocation effective during large transfers. Commands,
  uploads, resync and disconnect signals retain their ordering/ownership contracts.
  Do not add a polling loop, one worker per chunk, an unbounded transfer queue or
  repeated encoding of the full state just to count and then send its bytes.
- Define bridge version negotiation and old-peer behavior before changing the wire.
  Preserve authentication/admission and existing upload/action protections. Keep
  the browser update contract stable unless a driven change is necessary.

## Milestones

1. Trace the real repo feed/status/transcript publications through RepoSession,
   bridge_instance, connector wire, host replay/service and browser attach/resync.
   Reproduce a 1,082,662-byte encoded update and identify which payload dominates;
   distinguish full-state duplication from inherently large content. Record the
   transport/reassembly/retention costs and flow control; choose one coherent model
   without a hard-coded logical session ceiling.
2. Implement that model for initial snapshot, live update and resync, sharing one
   owned framing/reassembly path. Retain logical publication atomicity, cancellation,
   generation/sequence validation, action/upload ordering and bounded memory. Fix
   reconnect/failure state so valid large sessions do not get trapped in a futile
   reconnect loop; retain honest recoverable errors for actual resource/I/O failures.
3. Drive large payloads end to end and document resource behavior, compatibility and installation.
   Convert the existing oversized tests to the revised supported-size contract;
   keep malformed framing and actual per-frame transport-boundary rejection tests,
   not tests enforcing an arbitrary new logical payload cap.

## Validation and stopping condition

- Initial attach, live publication, late browser, resync and reconnect preserve
  equivalent full content for payloads just below/at/above 1 MiB, the reported size,
  multiple large frames and one large transcript item. Count serialized bytes,
  including envelope/escaping overhead, rather than estimating text character count.
- Partial/interrupted/invalid transfers and genuine resource failures do not corrupt retained
  state or leak buffers. Slow peers and revocation cancel owned writes and clear
  incomplete transfers promptly. A large valid transfer is not rejected merely for
  taking longer than a size-independent wall-clock budget while making progress.
  Uploads/actions/pings still work.
- Exercise actual CLI repo feed/connector construction alongside reusable web
  bridge/service/replay tests; helper-only chunk tests cannot prove startup works.
- Run affected web/CLI tests and relevant markup/runtime/process boundaries. Report
  local versus live-tunnel validation and installed binary status accurately. Do not
  contact the user's tunnel or mutate its live session merely to queue this plan.
- Demonstrate larger logical images crossing many bounded frames without an
  application-defined maximum; measure transient copy/queue behavior as content
  grows and a peer slows. Do not use a larger test fixture as an implicit new ceiling.
- Finish when the reproduced startup/update/resync failure is fixed with controlled
  transient overhead and no arbitrary logical session cap. Transcript redesign and
  unrelated remote features remain outside this plan.

## Milestone 1 — evidence and chosen transport/recovery contract

### Traced publication path and reproduction

`Feed::publish` broadcasts `BrowserDelta { live }` containing only changed frames.
`Feed::bridge_connect` subscribes before reading its retained image under the same
lock. `RepoSession::subscribe_with_snapshot` delegates to that path, and the owned
`bridge_instance::connector` runs `bridge::connect_signaled`. Thus ordinary live
traffic is already incremental; sending the full image on every publication is not
the cause of this report. Initial attach and broadcast-lag resync do send full images.

Currently each browser frame is an SSE-shaped String whose data is compact JSON.
`BrowserDelta` places that String inside a serde JSON Value, and `Wire::Update`
encodes it again as JSON. Quotes, backslashes and escaped line breaks gain another
escaping layer. Host `replay::merge` parses the frame Strings and recreates its image;
`WebService::pump` sends individual text frames to `page.html`, whose JavaScript
`deliver` extracts and parses their JSON. There is no browser WASM decoder today.

The real socket bridge regression now reproduces **1,082,662 encoded bytes** with
a synthetic single status frame through the shipped delta shape. Its content is
1,082,559 ASCII bytes; inner JSON is 1,082,570 bytes; SSE frame is 1,082,592 bytes.
The real sender returns `UpdateTooLarge.bytes == 1_082_662` and the host receives
`update_too_large`. The existing snapshot refusal also passed. These are **two
passing baseline reproduction tests**, not evidence of a fix. The user's original
payload was not retained/inspected, so its dominating component remains unknown.
An additional controlled escape-heavy sample of 3,000 content bytes becomes 6,011
inner JSON bytes and 11,103 bridge JSON bytes. Binary framing must handle inherently
large single content as well as remove this avoidable outer escaping.

The sender currently serializes the whole update once to count it, then again to
send it. An oversized failure is special-cased in the CLI connector to stop retries
permanently. `ConnectionGuard` clears the host snapshot on disconnect; browser pumps
exit on a Disconnected event. These are recovery seams that must change together.
`RepoSession::dispatch` also substitutes a 413 response at half the control cap;
the new large-message path must remove that artificial response-size refusal.

### Wire schema and browser boundary

Choose bridge protocol **2**, with wincode payloads and an explicit versioned wire
schema in `clank-web`, independent of fold-cache encoding. Keep a small JSON
Hello/Ready bootstrap so protocol-1 peers can receive an intelligible upgrade refusal
before binary decoding or mutation. After Ready(2), all logical bridge messages use
the binary path; no fallback to protocol 1's session cap. Authenticate and authorize
the handle before creating a bulk receiver. Unsupported versions are actual upgrade
requirements, not large-session failures or an endless automatic reconnect loop.

Define an owned binary Value vocabulary for the generic Backend state/action/result
boundary: null, bool, signed/unsigned integer, finite floating number, UTF-8 string,
array and object. Preserve serde JSON number semantics and Unicode exactly; never
encode a serde JSON blob as one wincode String merely to rename the outer format.
Use explicit fixed schema/version and golden fixtures, not positional layouts whose
variant reordering silently changes an existing protocol. Check wincode's configured
container/preallocation behavior: the installed 0.5.5 default includes a 4 MiB
preallocation limit, which must not become a new hidden session-content ceiling.

For browser publications, replace SSE String nesting on the Rust bridge with
structured frames containing event kind and binary Value data. Move frame conversion
to the existing feed/replay seams so host replay operates on structured data instead
of encoding and reparsing SSE for every merge. Retain the public generic Backend
state semantics through explicit adapters. The browser boundary remains the existing
text-frame interface: host-to-browser SSE-shaped frames encode their data as JSON
once. No browser WASM or full frontend rewrite is needed for this milestone. Audit
the WebSocket library's browser-facing message limits and configure them so a valid
large single transcript does not simply fail at the next hop. If driven browser tests
reveal that hop needs incremental assembly, specify that narrowly before altering it.

### Framing, resources and ownership

Every logical message (snapshot, update, request, result and upload) has a transfer
ID, message kind, sequence where applicable and an ordered complete byte stream.
Use Begin / Data / End framing; each Data contains the expected byte offset and at
most **64 KiB** of encoded bytes. This is the existing bulk-block size and a tuning
unit for bounded scheduling/buffering, not a logical size limit. One message can
span any number of blocks. End verifies the actual total length and decoder EOF;
there is no need to serialize twice just to calculate a length for Begin.

Use one active logical transfer per direction per connection. Carry its encoded
stream through owned temporary storage with bounded read/write buffers, rather
than building multiple complete encoded Vecs in memory. The local wincode
`WriteAdapter`/`ReadAdapter` support `std::io` sinks/sources. Blocking codec/file work
must run off the async socket driver, with one owned job/channel at a time, joined
on cancellation; never spawn one task per block. Small controls may share the same
path with an in-memory one-block staging optimization. Temporary state is RAII-owned
and removed on completion, disconnect, supersession, revocation or decode failure.

Maintain a single partially sent block and feed/flush state in the connection driver.
Continue receiving peer traffic and processing cancellation while a large publication
is in flight; do not await a whole transfer inside one select arm. Bound channel
slots by the sole producer/consumer ownership, not an unbounded pending-message
queue. Initial snapshot precedes updates; replay lag chooses a fresh snapshot from
an atomic subscription instead of accumulating an old backlog. Actions/results retain
their request IDs. Upload transfers remain distinguished and sequentially owned,
with their existing media/admission protections; publication bytes cannot become
upload bytes. Pings and revocation can progress between blocks and while flush waits.

Transient encoding/reassembly buffers are bounded by a constant number of blocks.
The last committed image plus at most one fully decoded candidate scale with actual
session content, and existing broadcast retention has a fixed number of versions;
these necessary retained-state costs must be counted honestly. Decode by checking
declared container lengths against actual available transfer bytes before allocation,
using checked arithmetic and fallible/incremental allocation, rather than a fabricated
maximum-content number or blindly reserving an attacker-declared size. Validate
schema structure, actual UTF-8 and end-of-input. On real storage/allocation/I/O failure,
discard only the incomplete candidate and preserve committed state. Measure and avoid
extra full-state copies in conversion/merge/publication; chunking alone is not enough.

### Commit, recovery and reconnect semantics

Begin/Data never publish state or advance sequence. After End and complete decode,
validate the transfer against its owning connection generation and expected sequence,
merge/apply its candidate, then atomically replace committed state and publish the
event/Applied acknowledgement. A stale generation cannot commit or clear its successor.
Interrupted transfers vanish with their owner and leave the last complete image intact.
Offsets/IDs enforce stream order; duplicates or gaps cannot masquerade as complete data.

Retain the last complete image on connector disconnect, but mark it disconnected/stale
and refuse repo mutations until a new authenticated connector commits its initial
snapshot. The browser may retain its display and show reconnecting. A new connection
generation replaces the old image only at full snapshot commit, and existing browser
subscriptions must observe both disconnect state and eventual replacement without
requiring manual refresh. Browser revocation still clears access immediately.

A publication sequence gap requests a fresh snapshot on the owned connection; an
interrupted transport reconnects and resubscribes from the current source image.
Do not fabricate missing deltas or restart the whole UI for an ordinary gap. Connection
backoff is event-driven and cancellation-aware; no healthy-connection polling and no
logical-size refusal that parks retry forever. A progressing large transfer has no
size-independent overall deadline. Genuine malformed/version/auth traffic may be
refused safely, but routine large/slow/interrupted traffic uses the recovery path.

Mutations with a lost reply have an unknown outcome: do not redispatch automatically.
Publication/snapshot recovery is idempotent by generation/sequence and may retry.
Document stale read-only display versus live mutation readiness as distinct facts.

### Validation boundary and next milestone

This commit records the chosen contracts and the exact socket reproduction before
changing production wire code. The two baseline bridge reproduction tests passed;
formatting and diff checks are run before commit. No production wire, browser code,
installed binary or live tunnel changed. Milestone 2 must implement the shared codec,
framing/staging and recovery seams coherently. Milestone 3 must drive the real CLI
Feed/RepoSession path and browser single-large-frame path, cancellation/backpressure,
version negotiation, failed decode/storage, late attach/reconnect/resync and fidelity.


## Milestone 2 — shared binary transfers implemented

`bridge_codec` defines fixed scalar/Value tags using wincode reads/writes,
with checked variable lengths against the staged transfer and decoder EOF.
It preserves signed/unsigned numbers, finite floats, Unicode and full large
strings without inheriting wincode's default container ceiling. Traversal
metadata scales with nesting depth, not the number of array children. Golden
fixtures pin the logical and framing schema independently of enum ordering.

`bridge_transport` owns one writer and one reader per admitted connection.
Begin/Data/End use a fixed 17-byte header and 64 KiB maximum Data body, offsets,
monotonic IDs and a verified actual End byte count. Messages up to one block
stay in memory; larger encodings/reassemblies spill to unnamed temporary files.
Codec jobs run on the blocking pool, check cancellation between block writes/
reads, and are joined when cancelled. File operations complete under their
owner before releasing it. The fault slot is separate from the single decoded
message slot so a full channel cannot prevent cancellation/error reporting.
Idle Ping replies use a notification to flush the shared WebSocket sink.
There is no healthy-connection poll or overall large-transfer deadline.

One publication remains in flight until Applied; source broadcast lag replaces
an old backlog with an atomic fresh image. The connector keeps one unsent
message slot and the host allows one unsent request plus a publication ack.
Initial attach, live update, resync, request/result and upload messages use the
same binary transfer engine. Ordinary TCP closure/reset disconnects cleanly;
malformed transfers fail without publishing any candidate. Host publication
is atomic after full decode. A host-observed sequence gap requests Resync;
a same-generation snapshot cannot move the committed sequence backwards.
Disconnect preserves the image and explicitly marks SessionView disconnected;
call/upload refuse until the current connector commits its first snapshot.

CLI Feed now produces `BrowserFrame { event, data }`, and host Replay merges
structured Values. SSE/JSON string encoding occurs only at the browser boundary.
This removes the old nested JSON stringify/parse operations from the Rust feed,
bridge and replay. The generic Backend still transports arbitrary JSON Values.
The synthetic 3,000-byte escape-heavy content occupies 3,118 binary logical
bytes, versus the previously measured 11,103 bytes of JSON/SSE/JSON nesting.
Binary is not promised to shrink every plain-text value; its fixed headers can
be slightly larger for unescaped text. Framing removes the size refusal either way.
RepoSession's unrelated half-control-size response refusal was removed as well.
The browser's permanent size-failure reconnect stop and unused alert were removed.

JSON hello/ready remains before binary transfer; protocol 2 requires matched
connector/host builds. Both old-connector/new-host and new-connector/old-host
refusals are exercised. The CLI recognizes ProtocolMismatch and stops futile
retries while retaining the explicit upgrade diagnostic. Admission/authentication
and existing upload media/size checks remain enforced independently of publication
size. No installed binary or live tunnel has been changed at this milestone.

Validation: clank-web's 21 unit tests, 7 bridge socket tests and custom-host
example test pass. These cover the exact former 1,082,662-byte shape, a 5 MiB
snapshot, a large command/result, upload, late replay, gap/resync/reconnect,
seven malformed framing/codec cases, incomplete spill preservation, cancellation
of stalled writes and partial spools, idle Ping and protocol refusal. CLI web
adapter/lifecycle tests (48) and separate-host-process tests (4) pass. Codec
traversal was subsequently changed to depth-only iterator metadata and its three
codec tests passed, followed by the complete clank-web tests above.
`cargo check -p clank --all-targets --offline`, formatting and diff checks
also passed; the CLI check reports only existing warnings in other modules.

Resource claims are deliberately limited to the encoded transport: one-block
memory staging, constant block buffers/packet copies and proportional temporary
file space, plus traversal metadata by depth. Owned source/candidate Values,
Replay's merge copies, fixed broadcast retention and browser text frames still
scale with actual content. These are not removed by chunking and are not covered
by any invented session-content ceiling.

Remaining for milestone 3: drive large real CLI transcript/feed/RepoSession data
through the browser hop, exercise slow publication/broadcast-lag recovery and
mutation uncertainty, audit the WebSocket configuration/buffer costs, and verify
stale display connection reporting when a browser attaches during a connector
outage. Complete user-facing compatibility/resource and installation notes;
install only the reviewed complete result. This milestone does not claim the
frontend's stale/live reporting or the entire plan is finished.


## Milestone 3 — browser recovery and end-to-end validation

The page now derives repo readiness from `connection` events, rather than
socket-open. A browser attached during a connector outage receives the last
complete replay marked disconnected and remains subscribed. Existing and late
browsers receive the fresh complete snapshot and live notice on the same socket
when the connector returns. An initially empty host also keeps its browser
waiting for the first image. Revocation, interrupted flushes and broadcast-lag
replacement remain effective. Browser frame retries now use shared Utf8Bytes,
removing full transcript copies on every Ping interruption.

Audit found a second ten-second limit in BridgeInstance: it timed out the first
image even after transport admission. That total publication deadline is gone.
Only WebSocket establishment and the pre-publication protocol handshake have
deadlines. Initial errors report through a latest-state ConnectionStatus watch;
a queued old attachment ack cannot clear a newer disconnect. There is still no
healthy-connection poll. A 5 MiB initial image whose Applied is delayed eleven
seconds exercises the exact CLI startup path and guards this deadline removal.

The reference connector/hosts explicitly use 64 KiB read buffers and immediate
physical-message flushing. The socket's incoming message/frame bound remains
the 1 MiB bootstrap admission bound; binary Data bodies are independently checked
at 64 KiB, never a logical-image maximum. The browser server library's defaults
limit incoming client frames, not outbound session publications. Outbound whole
browser frames retain proportional memory; the pump admits once then flushes,
with revocation/ping handling. README records these actual costs and scopes.

A real CLI test parses a large Codex harness line, renders its markdown, publishes
via Feed and RepoSession, and verifies full text plus HTML at the browser socket.
It exercises initial replay, a second large live turn, late browser attachment
during an outage, retained display, and same-socket replacement after reconnect.
The slow-publication test withholds Applied, overfills a two-slot source broadcast
with large updates, then confirms one authoritative latest snapshot replaces the
backlog. The separate-host tests verify lost say/upload replies remain unknown
outcomes and neither mutation is replayed after reconnection.

A new Chrome check runs the real page script, parses and renders multi-megabyte SSE
frames, preserves full display while disconnected and resumes on connection
notices without opening another socket. The existing frontend suite reports
303 PASS and zero FAIL. The Rust CLI test covers the actual transport path; the
Chrome harness supplies synthetic frames and a stub Terminal, so this is paired
validation rather than a claim of a single driven browser-to-CLI process test.

Compatibility/installation notes cover matched protocol-2 binaries, restarting
running protocol-1 hosts once, updating embedding adapters/configuration and
reloading old frontend code. No payload trimming or session migration is needed.
Installation is left until the reviewed plan is finalized; the current running
binary and user's live tunnel have not been replaced by this commit.

Final affected checks and any limitations are recorded before commit below.


Final validation: clank-web (22 unit + 7 bridge + 1 custom-host tests), affected
CLI web (50 tests, including the eleven-second startup regression), separate
host process (4 tests, including unknown say/upload outcomes), Chrome bridge
check and all 303 existing frontend checks passed. Formatting, diff checks and
`cargo check -p clank --all-targets --offline` passed; the latter reports existing
warnings in unrelated CLI test modules. No production binaries were installed.
Actual OS disk exhaustion was not injected: ordinary storage errors propagate
through the owned transport error/cancellation path, while complete-image
preservation is driven for interrupted spills and malformed/end/decode failures.
