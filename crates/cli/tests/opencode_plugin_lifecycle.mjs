// Deterministic lifecycle regression for the clank opencode plugin
// (no live opencode, no model): a fake client/$ drives the event
// hook through two delivered work turns and asserts the stop-hook
// wait re-arms for the second one.
//
// The regression this pins (codex d7c8908): the wait guard held
// across the injected model turn means the turn's terminal
// session.idle is ignored, so the loop delivers ONE continuation
// and goes dormant. The injection promise here stays UNRESOLVED
// while the second idle fires — with the guard correctly scoped to
// the wait alone, the second wait still arms.
//
// NOT run by cargo (the suite spawns no binaries and declares no JS
// toolchain — plan acceptance). Manual verification alongside the
// in-process model + source invariants in setup.rs:
//
//   node crates/cli/tests/opencode_plugin_lifecycle.mjs \
//        crates/cli/src/cli/setup_assets/opencode_plugin.js

import { pathToFileURL } from "node:url"

const assert = (cond, msg) => {
  if (!cond) {
    console.error("FAIL: " + msg)
    process.exit(1)
  }
}

const pluginPath = process.argv[2]
assert(pluginPath, "plugin path argument missing")
const { ClankPlugin } = await import(pathToFileURL(pluginPath))

// The Bun-shell fake: tagged template returning a chainable,
// awaitable handle. Each wait pops the next scripted stop-hook
// result and counts an "arm".
const waits = []
let arms = 0
const $ = () => {
  arms += 1
  const result = waits.shift() ?? { exitCode: 0, stdout: "", stderr: "" }
  const handle = {
    env: () => handle,
    quiet: () => handle,
    nothrow: () => handle,
    then: (resolve) => resolve(result),
  }
  return handle
}

// promptAsync resolves only when the test says the injected turn was
// accepted — the plugin must NOT need it resolved to re-arm.
const injections = []
const injectionReleases = []
// Both endpoints exist, both stay PENDING until released — prompt()
// mirrors its synchronous wait-for-the-turn semantics, so a plugin
// that holds the wait guard across either call fails the re-arm
// assertion below instead of erroring on a missing method.
const inject = (req) => {
  injections.push(req.body.parts[0].text)
  return new Promise((resolve) => {
    injectionReleases.push(resolve)
  })
}

// The wider client surface the plugin uses: promptAsync for
// continuations, session.status for idle verification, tui.showToast
// for diagnostics, app.log for the loop's own trace.
const toasts = []
const logs = []
const statuses = new Map() // sessionID -> "idle" | "busy" (absent = not idle)
const client = {
  session: {
    prompt: inject,
    promptAsync: inject,
    status: async () => ({
      data: Object.fromEntries([...statuses].map(([k, v]) => [k, { type: v }])),
    }),
  },
  tui: {
    showToast: async (req) => {
      toasts.push(req.body)
    },
  },
  app: {
    log: async (req) => {
      logs.push(req.body)
    },
  },
}

const hooks = await ClankPlugin({ client, $, directory: "/fake/repo" })
const fire = (type, sessionID) => hooks.event({ event: { type, properties: { sessionID } } })
const settle = () => new Promise((r) => setTimeout(r, 0))

const SID = "ses_0test0000000000000000000000"
const userMsg = (mid) => ({
  event: {
    type: "message.updated",
    properties: { sessionID: SID, info: { id: mid, role: "user" } },
  },
})

// Turn 1: the turn's own user prompt is seen before idle; opencode
// RE-emits its message.updated right after idle (housekeeping,
// observed live) — a re-sighting must NOT mark the wait stale.
await hooks.event(userMsg("msg_user_1"))
waits.push({ exitCode: 0, stdout: "work-1\n", stderr: "" })
const idle1 = fire("session.idle", SID)
await hooks.event(userMsg("msg_user_1")) // post-idle re-update, during the wait
await settle()
assert(arms === 1, `first idle must arm a wait (arms=${arms})`)
assert(injections.length === 1 && injections[0] === "work-1", "first continuation must inject")

// The injected turn runs: assistant-role housekeeping arrives while
// the injection promise is still pending.
await fire("message.updated", SID) // role undefined: housekeeping, not activity
await fire("session.updated", SID)
await hooks.event(userMsg("msg_inject_1")) // the injection's own user message

// Turn 1 ends: terminal idle. THIS is the regression point — a guard
// held across the un-resolved injection would swallow it.
waits.push({ exitCode: 0, stdout: "work-2\n", stderr: "" })
const idle2 = fire("session.idle", SID)
await settle()
assert(arms === 2, `terminal idle of the injected turn must re-arm (arms=${arms})`)
assert(injections.length === 2 && injections[1] === "work-2", "second continuation must inject")

// Hygiene: resolve the pending injection promises so the handler
// invocations settle.
injectionReleases.forEach((r) => r())
await idle1
await idle2

// Staleness still holds: a NEW user message id between idle and
// wait completion discards the continuation (the wait still armed).
waits.push({ exitCode: 0, stdout: "work-3\n", stderr: "" })
const idle3 = fire("session.idle", SID)
const armedAt = arms
await hooks.event(userMsg("msg_user_2")) // genuinely new, during the wait
await idle3
await settle()
assert(armedAt === 3, `third idle must arm (arms=${armedAt})`)
assert(injections.length === 2, "stale continuation must be discarded, not injected")

// ── An interrupted turn is not a finished one ─────────────────
// Esc produces no new USER message, so the staleness guard cannot
// see it; opencode marks the aborted turn's ASSISTANT message.
const abortMsg = () => ({
  event: {
    type: "message.updated",
    properties: {
      sessionID: SID,
      info: {
        id: "msg_asst_abort",
        role: "assistant",
        error: { name: "MessageAbortedError" },
      },
    },
  },
})

// Abort observed BEFORE the idle: no wait may even arm. A wait here
// would long-poll holding the guard, and the next genuine idle would
// be ignored as in-flight.
await hooks.event(abortMsg())
const armsBeforeAbort = arms
await fire("session.idle", SID)
await settle()
assert(arms === armsBeforeAbort, `an interrupted turn must not arm a wait (arms=${arms})`)
assert(injections.length === 2, "an interrupted turn must inject nothing")

// Prompting again ends the stop the interrupt began.
await hooks.event(userMsg("msg_user_3"))
waits.push({ exitCode: 0, stdout: "work-4\n", stderr: "" })
const idle4 = fire("session.idle", SID)
await settle()
assert(arms === armsBeforeAbort + 1, `a new prompt must re-arm the loop (arms=${arms})`)
assert(
  injections.length === 3 && injections[2] === "work-4",
  "the loop must deliver again after the user resumes",
)
injectionReleases.forEach((r) => r())
await idle4

// Abort observed AFTER the idle, while the wait is in flight — the
// ordering opencode actually produces for post-idle housekeeping.
// The wait armed, so the discard has to happen at injection time.
await hooks.event(userMsg("msg_user_4"))
waits.push({ exitCode: 0, stdout: "work-5\n", stderr: "" })
const idle5 = fire("session.idle", SID)
const armedAtAbort = arms
await hooks.event(abortMsg())
await idle5
await settle()
assert(armedAtAbort === armsBeforeAbort + 2, `the wait must have armed (arms=${armedAtAbort})`)
assert(injections.length === 3, "an abort during the wait must discard its continuation")

// ── bootstrap / diagnostics / ownership (opencode-wake-bootstraps-and-surfaces) ──
// Per-section fakes: arms tracked BY SESSION, stop-hook results
// scripted per section, env var controlled per section.

const makeSection = () => {
  const armSessions = []
  const results = []
  const $f = (strings, ...values) => {
    let sid
    try {
      sid = JSON.parse(values[0]).session_id
    } catch {}
    armSessions.push(sid)
    const handle = {
      env: () => handle,
      quiet: () => handle,
      nothrow: () => handle,
      then: (resolve) => {
        if (results[0] === "pending") {
          results.shift()
          return // never resolves: the wait stays in flight
        }
        resolve(results.shift() ?? { exitCode: 0, stdout: "", stderr: "" })
      },
    }
    return handle
  }
  const injections2 = []
  const toasts2 = []
  const logs2 = []
  const statuses2 = new Map()
  const state = { deferStatus: false }
  const statusReleases = []
  const client2 = {
    session: {
      promptAsync: async (req) => {
        injections2.push(req.path.id)
      },
      status: async () => {
        // A deferred answer still carries the state at CALL time —
        // that staleness is exactly what the generation guard is for.
        // The SDK shape: { data: map } on success, { error } on failure.
        if (state.statusError) return { error: state.statusError }
        const snapshot = {
          data: Object.fromEntries([...statuses2].map(([k, v]) => [k, { type: v }])),
        }
        if (state.deferStatus) {
          return new Promise((r) => statusReleases.push(() => r(snapshot)))
        }
        return snapshot
      },
    },
    tui: { showToast: async (req) => toasts2.push(req.body) },
    app: { log: async (req) => logs2.push(req.body) },
  }
  return { $f, armSessions, results, client2, injections2, toasts2, logs2, statuses: statuses2, state, statusReleases }
}

const TOKEN = "ses_0bootstrap00000000000000000"
const setToken = (v) => {
  if (v === undefined) delete process.env.CLANK_BOOTSTRAP_SESSION_ID
  else process.env.CLANK_BOOTSTRAP_SESSION_ID = v
}

// 1. The penlock case: a resumed, zero-event session whose launch
// handed over its id gets its wait at plugin load, and work is
// delivered without any turn having completed. The status fake uses
// opencode's REAL shape: idle sessions are ABSENT from the map
// (opencode deletes them), so an empty map here means idle.
{
  const s = makeSection()
  s.results.push({ exitCode: 0, stdout: "bootstrap-work\n", stderr: "" })
  setToken(TOKEN)
  await ClankPlugin({ client: s.client2, $: s.$f, directory: "/fake/repo" })
  setToken(undefined)
  await settle()
  assert(
    s.armSessions.length === 1 && s.armSessions[0] === TOKEN,
    `bootstrap must arm the token session: ${JSON.stringify(s.armSessions)}`,
  )
  assert(
    s.injections2.length === 1 && s.injections2[0] === TOKEN,
    "bootstrap work must inject without any turn",
  )
  assert(
    s.logs2.some((l) => l.message === "arm"),
    "the arm must be logged",
  )
}

// 2. Detached init: a wait that never resolves must not block plugin
// construction — opencode startup never waits on the long-poll.
{
  const s = makeSection()
  s.results.push("pending")
  setToken(TOKEN)
  const hooks2 = await ClankPlugin({ client: s.client2, $: s.$f, directory: "/fake/repo" })
  setToken(undefined)
  assert(hooks2.event, "plugin construction resolves with a pending bootstrap wait")
  await settle()
  assert(s.armSessions.length === 1, "the detached arm still started")
}

// 3. One process, one owned session: with TWO bound sessions idle,
// init arms and injects ONLY for the token session.
{
  const s = makeSection()
  const OTHER = "ses_0other0000000000000000000"
  s.results.push({ exitCode: 0, stdout: "a-work\n", stderr: "" })
  setToken(TOKEN)
  await ClankPlugin({ client: s.client2, $: s.$f, directory: "/fake/repo" })
  setToken(undefined)
  await settle()
  assert(
    s.armSessions.length === 1 && s.armSessions[0] === TOKEN,
    `init must not arm another bound session: ${JSON.stringify(s.armSessions)}`,
  )
  assert(
    !s.injections2.includes(OTHER),
    "init must never promptAsync into another session",
  )
}

// 4. A busy status blocks the bootstrap arm: the resume's own prompt
// turn may still be running — never arm into it.
{
  const s = makeSection()
  s.statuses.set(TOKEN, "busy")
  setToken(TOKEN)
  await ClankPlugin({ client: s.client2, $: s.$f, directory: "/fake/repo" })
  setToken(undefined)
  await settle()
  assert(s.armSessions.length === 0, "a busy session must not be armed at load")
}

// 5. First-sighting is idle-verified: an arbitrary first event arms
// only when session.status says idle — busy means no arm.
{
  const s = makeSection()
  const hooks3 = await ClankPlugin({ client: s.client2, $: s.$f, directory: "/fake/repo" })
  const IDLE_S = "ses_0seenidle0000000000000000"
  const BUSY_S = "ses_0seenbusy0000000000000000"
  // IDLE_S is ABSENT from the status map — opencode's idle shape.
  s.statuses.set(BUSY_S, "busy")
  const evt = (sessionID) =>
    hooks3.event({ event: { type: "session.updated", properties: { sessionID } } })
  await evt(IDLE_S)
  await evt(BUSY_S)
  await settle()
  assert(
    s.armSessions.length === 1 && s.armSessions[0] === IDLE_S,
    `first-sighting arms only the status-idle session: ${JSON.stringify(s.armSessions)}`,
  )
}

// 6. Diagnostics: exit 0 + empty stdout + stderr → toast + log,
// NEVER a prompt. Repeated diagnostics still do not prompt or arm
// through their own presentation.
{
  const s = makeSection()
  const D = "ses_0diag00000000000000000000"
  const hooks4 = await ClankPlugin({ client: s.client2, $: s.$f, directory: "/fake/repo" })
  const diag = "hook: cannot resolve `x`'s role; arming no wait"
  s.results.push(
    { exitCode: 0, stdout: "", stderr: diag + "\n" },
    { exitCode: 0, stdout: "", stderr: diag + "\n" },
  )
  const fire4 = () => hooks4.event({ event: { type: "session.idle", properties: { sessionID: D } } })
  await fire4()
  await settle()
  assert(s.injections2.length === 0, "a diagnostic must not be injected")
  assert(
    s.toasts2.length === 1 && s.toasts2[0].message === diag,
    `the diagnostic must toast: ${JSON.stringify(s.toasts2)}`,
  )
  assert(
    s.logs2.some((l) => l.message === "diagnostic"),
    "the diagnostic must be logged",
  )
  await fire4()
  await settle()
  assert(s.injections2.length === 0, "a repeated diagnostic still does not prompt")
  assert(s.toasts2.length === 2, "a repeated diagnostic toasts again, not loops")
}

// 7. Deferred-status race: the activity generation is captured
// BEFORE the status call. A user turn starting while status() is
// pending must kill the arm, or the injection lands mid-turn with
// the post-turn generation baked in as its baseline.
{
  const s = makeSection()
  s.state.deferStatus = true
  setToken(TOKEN)
  const hooks7 = await ClankPlugin({ client: s.client2, $: s.$f, directory: "/fake/repo" })
  setToken(undefined)
  // The bootstrap's status call is now parked. A user turn starts —
  // opencode would mark the session busy in the live map.
  s.statuses.set(TOKEN, "busy")
  await hooks7.event({
    event: {
      type: "message.updated",
      properties: { sessionID: TOKEN, info: { id: "m_turn", role: "user" } },
    },
  })
  // The status answer arrives (empty map = idle) — too late.
  s.statusReleases.forEach((r) => r())
  await settle()
  assert(
    s.armSessions.length === 0,
    `an arm whose generation moved during the status check must drop: ${JSON.stringify(s.armSessions)}`,
  )
}

// 8. A first-ever session.idle marks the session seen: the idle's
// own housekeeping event must NOT read as a first sighting and
// start a second wait behind the first.
{
  const s = makeSection()
  const H = "ses_0housekeep000000000000000"
  const hooks8 = await ClankPlugin({ client: s.client2, $: s.$f, directory: "/fake/repo" })
  s.results.push({ exitCode: 0, stdout: "h-work\n", stderr: "" })
  const fire8 = (type) => hooks8.event({ event: { type, properties: { sessionID: H } } })
  await fire8("session.idle")
  await fire8("session.updated") // the idle's own housekeeping
  await settle()
  const hArms = s.armSessions.filter((x) => x === H).length
  assert(hArms === 1, `housekeeping after a first idle must not re-arm (arms for H=${hArms})`)
}

// 9. A rejected injection is caught and logged, never an unhandled
// rejection — the bootstrap arm is detached.
{
  const s = makeSection()
  s.client2.session.promptAsync = async () => {
    throw new Error("tui gone")
  }
  s.results.push({ exitCode: 0, stdout: "doomed\n", stderr: "" })
  setToken(TOKEN)
  await ClankPlugin({ client: s.client2, $: s.$f, directory: "/fake/repo" })
  setToken(undefined)
  await settle()
  assert(
    s.logs2.some((l) => l.message === "inject failed"),
    `the failed injection must be logged: ${JSON.stringify(s.logs2.map((l) => l.message))}`,
  )
}

// 10. Unknown never arms: the SDK answers HTTP errors WITHOUT
// throwing, as { error, request, response } with no `data`. Reading
// that wrapper's absent key as "idle" would arm on a FAILED call.
{
  const s = makeSection()
  s.state.statusError = { name: "ApiError", data: { message: "500" } }
  setToken(TOKEN)
  await ClankPlugin({ client: s.client2, $: s.$f, directory: "/fake/repo" })
  setToken(undefined)
  await settle()
  assert(
    s.armSessions.length === 0,
    `a failed status call must never arm: ${JSON.stringify(s.armSessions)}`,
  )
}

console.log("OK")
