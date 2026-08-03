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
const client = { session: { prompt: inject, promptAsync: inject } }

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

console.log("OK")
