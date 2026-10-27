// Deterministic lifecycle regression for clank's opencode plugin, run by
// `cargo test` through crates/cli/tests/it/opencode_plugin_lifecycle.rs
// (no live opencode, no model, no clank binary):
//
//   node crates/cli/tests/opencode_plugin_lifecycle.mjs \
//        crates/cli/src/cli/setup_assets/opencode_plugin.js
//
// `load` below is a stub of OpenCode 2.1.4's loader: it accepts a module
// only by the schema 2.1.4 enforces on its default export, calls
// `setup(ctx)` with a stub of the promise-style context the loader builds
// (event.subscribe, shell.hook, session.wait/prompt/get), and keeps the
// returned cleanup for unload. Lifecycle cases load `makePlugin` with a
// fake `run` in place of the spawner, through the same loader.

import { pathToFileURL } from "node:url"

const assert = (cond, msg) => {
  if (!cond) {
    console.error("FAIL: " + msg)
    process.exit(1)
  }
}

const pluginPath = process.argv[2]
assert(pluginPath, "plugin path argument missing")
const mod = await import(pathToFileURL(pluginPath))

const loaderAccepts = (m) => {
  const d = m?.default
  return (
    d !== null &&
    typeof d === "object" &&
    typeof d.id === "string" &&
    (typeof d.setup === "function" || typeof d.effect === "function")
  )
}

const load = async (definition, ctx) => {
  assert(loaderAccepts({ default: definition }), "Plugin must export a default definition with an id and an effect or setup function.")
  const cleanup = await definition.setup(ctx)
  return { unload: async () => cleanup && (await cleanup()) }
}

const settle = async () => {
  for (let i = 0; i < 6; i++) await new Promise((r) => setTimeout(r, 0))
}
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b)

// One event bus for the whole service: every subscriber sees every event,
// whichever location it was published for.
const makeBus = () => {
  const subscribers = new Set()
  return {
    subscribers,
    publish(event) {
      for (const s of subscribers) s.push(event)
    },
    subscribe(opts) {
      const queue = []
      let wake = () => {}
      const sub = {
        push(e) {
          queue.push(e)
          wake()
        },
      }
      subscribers.add(sub)
      const signal = opts?.signal
      signal?.addEventListener("abort", () => {
        subscribers.delete(sub)
        wake()
      })
      return {
        [Symbol.asyncIterator]() {
          return {
            next: async () => {
              while (!queue.length && !signal?.aborted) await new Promise((r) => (wake = r))
              if (signal?.aborted) return { done: true, value: undefined }
              return { done: false, value: queue.shift() }
            },
            return: async () => {
              subscribers.delete(sub)
              return { done: true, value: undefined }
            },
          }
        },
      }
    },
  }
}

let eventSeq = 0
const ev = (type, sessionID, data = {}, directory) => ({
  id: `evt_${++eventSeq}`,
  type,
  data: { sessionID, ...data },
  ...(directory ? { location: { directory } } : {}),
})

// A service: one bus, one fake `clank`, one session store. Each `instance`
// is one location's plugin, loaded through the stub loader.
const makeService = () => {
  delete globalThis[Symbol.for("clank.opencode.sessions")]
  const bus = makeBus()
  const commands = []
  const waitResults = []
  const state = { observeAnswer: '{"recovery":"notice"}', holdReports: false, heldReports: [] }
  const prompts = []
  const promptResults = []
  const logs = []
  const shellHooks = []
  const waitCalls = []
  const waitGates = new Map()
  const sessionDirs = new Map()
  const signals = []

  const run = (args, { input, env, cwd, signal }) => {
    const command = { args: args.slice(3), input: JSON.parse(input), env, cwd }
    commands.push(command)
    signals.push(signal)
    const aborted = () => ({ exitCode: null, stdout: "", stderr: "" })
    if (signal?.aborted) return Promise.resolve(aborted())
    if (command.args.includes("--observe")) {
      const answer = { exitCode: 0, stdout: state.observeAnswer + "\n", stderr: "" }
      if (state.holdReports) return new Promise((r) => state.heldReports.push(() => r(answer)))
      return Promise.resolve(answer)
    }
    if (command.args.includes("--recovery-reply")) return Promise.resolve({ exitCode: 0, stdout: "", stderr: "" })
    const next = waitResults.shift() ?? { exitCode: 0, stdout: "", stderr: "" }
    if (next === "pending" || typeof next === "function") {
      return new Promise((resolve) => {
        signal?.addEventListener("abort", () => resolve(aborted()))
        if (typeof next === "function") next(resolve)
      })
    }
    return Promise.resolve(next)
  }
  const waits = () => commands.filter((c) => c.args.length === 0)
  const reports = () => commands.filter((c) => c.args.includes("--observe")).map((c) => c.args)
  const log = (message, extra) => logs.push({ message, ...extra })

  const ctxFor = (directory) => ({
    location: { directory },
    event: { subscribe: (opts) => bus.subscribe(opts) },
    shell: {
      hook: async (name, fn) => {
        shellHooks.push({ name, fn })
        return { dispose: () => {} }
      },
    },
    session: {
      // Resolves at once for an idle session; a gated session stays
      // pending until the test opens it, as a busy loop would.
      wait: async ({ sessionID }) => {
        waitCalls.push(sessionID)
        const gate = waitGates.get(sessionID)
        if (gate === "error") throw new Error("session not found")
        if (gate) await gate.promise
      },
      get: async ({ sessionID }) => ({ data: { id: sessionID, location: { directory: sessionDirs.get(sessionID) } } }),
      prompt: async (input) => {
        prompts.push(input)
        const r = promptResults.shift()
        if (r === "pending") return new Promise(() => {})
        if (r instanceof Error) throw r
        return { data: { id: input.id ?? "msg_auto", sessionID: input.sessionID } }
      },
    },
  })
  const gate = (sessionID) => {
    let open
    const promise = new Promise((r) => (open = r))
    waitGates.set(sessionID, { promise })
    return () => {
      waitGates.delete(sessionID)
      open()
    }
  }
  const instance = (directory, deps = {}) => load(mod.makePlugin({ run, log, ...deps }), ctxFor(directory))
  return {
    bus,
    commands,
    waitResults,
    state,
    prompts,
    promptResults,
    logs,
    shellHooks,
    waitCalls,
    waitGates,
    sessionDirs,
    signals,
    waits,
    reports,
    gate,
    instance,
    ctxFor,
    emit: (...args) => bus.publish(ev(...args)),
  }
}

// A session the plugin has already seen, whose first-sighting idle check
// failed, so the case starts with no wait out.
const meet = async (svc, sid, dir) => {
  svc.waitGates.set(sid, "error")
  svc.emit("session.updated", sid, {}, dir)
  await settle()
  svc.waitGates.delete(sid)
}

// One person's prompt going through OpenCode's inbox into an execution.
const userTurn = (svc, sid, inbox, dir) => {
  svc.emit("session.inbox.enqueued", sid, { inboxID: inbox, item: { type: "user", delivery: "queue" } }, dir)
  svc.emit("session.execution.started", sid, {}, dir)
  svc.emit("session.inbox.delivered", sid, { inboxID: inbox }, dir)
}

const withToken = async (token, f) => {
  const old = process.env.CLANK_BOOTSTRAP_SESSION_ID
  if (token === undefined) delete process.env.CLANK_BOOTSTRAP_SESSION_ID
  else process.env.CLANK_BOOTSTRAP_SESSION_ID = token
  try {
    return await f()
  } finally {
    if (old === undefined) delete process.env.CLANK_BOOTSTRAP_SESSION_ID
    else process.env.CLANK_BOOTSTRAP_SESSION_ID = old
  }
}

const DIR = "/fake/repo"
const SID = "ses_0test0000000000000000000000"
const TOKEN = "ses_0bootstrap00000000000000000"

// ── The shipped module, through the 2.x loader ───────────────────────
{
  assert(loaderAccepts(mod), "the installed plugin's default export must satisfy the 2.x loader schema")
  assert(mod.default.id === "clank", `the plugin id is stable: ${mod.default.id}`)
  assert(!loaderAccepts({ ClankPlugin: async () => ({}) }), "the stub loader rejects the 1.x named-export shape")
  const svc = makeService()
  await withToken(undefined, async () => {
    const plugin = await load(mod.default, svc.ctxFor(DIR))
    assert(
      svc.shellHooks.length === 1 && svc.shellHooks[0].name === "create.before",
      `setup registers one shell creation hook: ${JSON.stringify(svc.shellHooks.map((h) => h.name))}`,
    )
    await settle()
    assert(svc.bus.subscribers.size === 1, "setup subscribes to the event stream")
    await plugin.unload()
    await settle()
    assert(svc.bus.subscribers.size === 0, "unload ends the subscription")
  })
}

// ── Binding: blank in every shell, opencode stamps its own ──────────
{
  const svc = makeService()
  await withToken(undefined, () => svc.instance(DIR))
  const [hook] = svc.shellHooks
  const inherited = {
    PATH: "/bin",
    CLAUDE_CODE_SESSION_ID: "parent-claude",
    CODEX_THREAD_ID: "parent-codex",
    GROK_AGENT: "1",
    GROK_SESSION_ID: "g",
    CLANK_AGENT: "parent",
    CLANK_BOOTSTRAP_SESSION_ID: "ses_parent",
    OPENCODE_SESSION_ID: "ses_inherited",
  }
  const userShell = { command: "ls", cwd: DIR, env: { ...inherited } }
  await hook.fn(userShell)
  assert(userShell.env.PATH === "/bin", "unrelated vars survive")
  for (const v of Object.keys(inherited).filter((k) => k !== "PATH")) {
    assert(userShell.env[v] === "", `${v} is blanked in a shell opencode does not stamp: ${JSON.stringify(userShell.env)}`)
  }
  const toolShell = { command: "clank as qwen", cwd: DIR, env: { ...inherited } }
  await hook.fn(toolShell)
  toolShell.env.OPENCODE_SESSION_ID = SID
  assert(toolShell.env.OPENCODE_SESSION_ID === SID && toolShell.env.CLAUDE_CODE_SESSION_ID === "", "the bash tool's own session survives the hook")
}

// ── The loop re-arms across an injected turn (codex d7c8908) ──────────
{
  const svc = makeService()
  await withToken(undefined, () => svc.instance(DIR))
  await meet(svc, SID, DIR)
  userTurn(svc, SID, "msg_user_1", DIR)
  svc.waitResults.push({ exitCode: 0, stdout: "work-1\n", stderr: "" })
  svc.promptResults.push("pending")
  svc.emit("session.execution.succeeded", SID, {}, DIR)
  await settle()
  assert(svc.waits().length === 1, `a succeeded execution arms one wait (waits=${svc.waits().length})`)
  const w = svc.waits()[0]
  assert(w.cwd === DIR && w.input.cwd === DIR && w.input.session_id === SID && w.input.stop_hook_active === false, `the wait's hook input: ${JSON.stringify(w)}`)
  assert(w.env.OPENCODE_SESSION_ID === SID && w.env.CLANK_AGENT === "", "the wait speaks for its session only")
  assert(svc.prompts.length === 1 && svc.prompts[0].text === "work-1" && svc.prompts[0].sessionID === SID, "the continuation is prompted")
  assert(svc.prompts[0].delivery === "queue", "a continuation never steers into a turn")

  // The injected turn runs and ends while its prompt() is still pending.
  svc.waitResults.push({ exitCode: 0, stdout: "work-2\n", stderr: "" })
  userTurn(svc, SID, "msg_inject_1", DIR)
  svc.emit("session.execution.succeeded", SID, {}, DIR)
  await settle()
  assert(svc.waits().length === 2, `the injected turn's settle re-arms (waits=${svc.waits().length})`)
  assert(svc.prompts.length === 2 && svc.prompts[1].text === "work-2", "the second continuation is prompted")

  // A person's prompt while the wait is out makes its continuation stale.
  let release
  svc.waitResults.push((resolve) => (release = () => resolve({ exitCode: 0, stdout: "work-3\n", stderr: "" })))
  svc.emit("session.execution.succeeded", SID, {}, DIR)
  await settle()
  assert(svc.waits().length === 3, "the third settle arms")
  svc.emit("session.inbox.enqueued", SID, { inboxID: "msg_user_2", item: { type: "user" } }, DIR)
  await settle()
  release()
  await settle()
  assert(svc.prompts.length === 2, "a stale continuation is discarded")
  assert(svc.logs.some((l) => l.message === "discard stale continuation"), "the discard is logged")

  // A second settle while a wait is out arms nothing more.
  svc.waitResults.push("pending")
  svc.emit("session.execution.succeeded", SID, {}, DIR)
  svc.emit("session.execution.succeeded", SID, {}, DIR)
  await settle()
  assert(svc.waits().length === 4, `one wait per session (waits=${svc.waits().length})`)
}

// ── An interrupted turn is not a finished one ─────────────────────────
{
  const svc = makeService()
  await withToken(undefined, () => svc.instance(DIR))
  const S = "ses_0interrupt000000000000000"
  await meet(svc, S, DIR)
  const base = svc.waits().length
  userTurn(svc, S, "msg_u1", DIR)
  svc.emit("session.execution.interrupted", S, { reason: "user" }, DIR)
  await settle()
  assert(svc.waits().length === base, "an interrupted execution arms nothing")
  svc.emit("session.execution.succeeded", S, {}, DIR)
  await settle()
  assert(svc.waits().length === base, "the stop latches until a person prompts again")

  svc.waitResults.push({ exitCode: 0, stdout: "work-after\n", stderr: "" })
  userTurn(svc, S, "msg_u2", DIR)
  svc.emit("session.execution.succeeded", S, {}, DIR)
  await settle()
  assert(svc.waits().length === base + 1, "a person's next prompt restores the loop")
  assert(svc.prompts.at(-1)?.text === "work-after", "and the loop delivers again")

  // Stopped while the wait is out: the continuation is dropped.
  let release
  svc.waitResults.push((resolve) => (release = () => resolve({ exitCode: 0, stdout: "work-x\n", stderr: "" })))
  userTurn(svc, S, "msg_u3", DIR)
  svc.emit("session.execution.succeeded", S, {}, DIR)
  await settle()
  svc.emit("session.execution.interrupted", S, { reason: "user" }, DIR)
  await settle()
  const prompted = svc.prompts.length
  release()
  await settle()
  assert(svc.prompts.length === prompted, "a stop during the wait discards its continuation")
  assert(
    same(svc.reports().filter((r) => r.includes("msg_u1")), [
      ["--observe", "started", "--turn-id", "msg_u1"],
      ["--observe", "interrupted", "--turn-id", "msg_u1"],
    ]),
    `an interrupted turn is reported as one: ${JSON.stringify(svc.reports())}`,
  )

  // A superseded execution's successor takes up the superseding input and
  // arms as usual.
  svc.waitResults.push({ exitCode: 0, stdout: "", stderr: "" })
  userTurn(svc, S, "msg_u4", DIR)
  svc.emit("session.inbox.enqueued", S, { inboxID: "msg_u5", item: { type: "user" } }, DIR)
  svc.emit("session.inbox.delivered", S, { inboxID: "msg_u5" }, DIR)
  svc.emit("session.execution.interrupted", S, { reason: "superseded" }, DIR)
  svc.emit("session.execution.started", S, {}, DIR)
  svc.emit("session.execution.succeeded", S, {}, DIR)
  await settle()
  assert(svc.logs.filter((l) => l.message === "arm" && l.why === "settled").length === 3, "a superseded turn's successor arms")
}

// ── Bootstrap, first sighting, idleness ─────────────────────────────
{
  // A resumed, zero-event session gets its wait at load.
  const svc = makeService()
  svc.sessionDirs.set(TOKEN, DIR)
  svc.waitResults.push({ exitCode: 0, stdout: "bootstrap-work\n", stderr: "" })
  await withToken(TOKEN, () => svc.instance("/somewhere/else"))
  await settle()
  assert(svc.waits().length === 1 && svc.waits()[0].input.session_id === TOKEN, `bootstrap arms the token session: ${JSON.stringify(svc.waits())}`)
  assert(svc.waits()[0].input.cwd === DIR, "with the session's own directory, not the instance's")
  assert(svc.prompts.length === 1 && svc.prompts[0].sessionID === TOKEN, "bootstrap work is delivered without a turn")
}
{
  // Setup never blocks on a long-poll.
  const svc = makeService()
  svc.sessionDirs.set(TOKEN, DIR)
  svc.waitResults.push("pending")
  const loaded = await withToken(TOKEN, () => svc.instance(DIR))
  assert(loaded, "setup resolves with a pending bootstrap wait")
  await settle()
  assert(svc.waits().length === 1, "the detached arm started")
}
{
  // A busy session is not armed at load, and one that finishes while the
  // idle check is out is armed by its own settle, once.
  const svc = makeService()
  svc.sessionDirs.set(TOKEN, DIR)
  const open = svc.gate(TOKEN)
  await withToken(TOKEN, () => svc.instance(DIR))
  await settle()
  assert(svc.waits().length === 0, "a busy session is not armed at load")
  userTurn(svc, TOKEN, "msg_resume", DIR)
  svc.emit("session.execution.succeeded", TOKEN, {}, DIR)
  await settle()
  open()
  await settle()
  assert(svc.waits().length === 1, `the settle armed, the stale idle check did not: ${svc.waits().length}`)
  assert(svc.logs.some((l) => l.message.startsWith("arm dropped: session moved")), "the stale arm is dropped")
}
{
  // An idle check that fails never arms.
  const svc = makeService()
  svc.waitGates.set(TOKEN, "error")
  await withToken(TOKEN, () => svc.instance(DIR))
  await settle()
  assert(svc.waits().length === 0, "unknown idleness never arms")
}
{
  // First sighting arms only an idle session, once.
  const svc = makeService()
  await withToken(undefined, () => svc.instance(DIR))
  const IDLE_S = "ses_0seenidle0000000000000000"
  const BUSY_S = "ses_0seenbusy0000000000000000"
  svc.gate(BUSY_S)
  svc.emit("session.updated", IDLE_S, {}, DIR)
  svc.emit("session.updated", BUSY_S, {}, DIR)
  svc.emit("session.updated", IDLE_S, {}, DIR)
  await settle()
  assert(same(svc.waits().map((w) => w.input.session_id), [IDLE_S]), `first sighting arms only the idle session: ${JSON.stringify(svc.waits())}`)
}

// ── Diagnostics, injection failures, renewal ─────────────────────────
{
  const svc = makeService()
  await withToken(undefined, () => svc.instance(DIR))
  const D = "ses_0diag00000000000000000000"
  const diag = "hook: cannot resolve `x`'s role; arming no wait"
  await meet(svc, D, DIR)
  userTurn(svc, D, "msg_d1", DIR)
  svc.waitResults.push({ exitCode: 0, stdout: "", stderr: diag + "\n" }, { exitCode: 0, stdout: "", stderr: diag + "\n" })
  svc.emit("session.execution.succeeded", D, {}, DIR)
  await settle()
  svc.emit("session.execution.succeeded", D, {}, DIR)
  await settle()
  assert(svc.prompts.length === 0, "a diagnostic is never prompted")
  assert(svc.logs.filter((l) => l.message === "diagnostic" && l.diagnostic === diag).length === 2, "each diagnostic is logged")

  svc.waitResults.push({ exitCode: 0, stdout: "doomed\n", stderr: "" })
  svc.promptResults.push(new Error("session gone"))
  svc.emit("session.execution.succeeded", D, {}, DIR)
  await settle()
  assert(svc.logs.some((l) => l.message === "inject failed"), "a rejected prompt is logged, not thrown")
}
{
  const svc = makeService()
  await withToken(undefined, () => svc.instance(DIR))
  const R = "ses_0renew0000000000000000000"
  await meet(svc, R, DIR)
  userTurn(svc, R, "msg_r1", DIR)
  svc.waitResults.push({ exitCode: 0, stdout: '{"clank":"renew_wait"}\n', stderr: "" }, { exitCode: 0, stdout: "renewed-work\n", stderr: "" })
  svc.emit("session.execution.succeeded", R, {}, DIR)
  await settle()
  assert(svc.waits().length === 2, `a deadline starts one new wait: ${svc.waits().length}`)
  assert(svc.prompts.length === 1 && svc.prompts[0].text === "renewed-work", "only the renewed work prompts")
  svc.waitResults.push({ exitCode: 0, stdout: "", stderr: "" })
  svc.emit("session.execution.succeeded", R, {}, DIR)
  await settle()
  assert(svc.waits().length === 3, "an empty result does not renew")

  // A turn starting while the renewal's idle check is out cancels it.
  const open = svc.gate(R)
  svc.waitResults.push({ exitCode: 0, stdout: '{"clank":"renew_wait"}\n', stderr: "" })
  svc.emit("session.execution.succeeded", R, {}, DIR)
  await settle()
  svc.emit("session.inbox.enqueued", R, { inboxID: "msg_r2", item: { type: "user" } }, DIR)
  await settle()
  open()
  await settle()
  assert(svc.waits().length === 4, `activity during the check cancels the renewal: ${svc.waits().length}`)
}

// ── Failures and recovery ─────────────────────────────────────────────
{
  const svc = makeService()
  await withToken(undefined, () => svc.instance(DIR))
  const F = "ses_0failed000000000000000000"
  await meet(svc, F, DIR)
  userTurn(svc, F, "msg_f1", DIR)
  svc.emit("session.execution.failed", F, { error: { type: "provider.rate-limit", message: "slow down", status: 429 } }, DIR)
  await settle()
  assert(
    same(svc.reports(), [
      ["--observe", "started", "--turn-id", "msg_f1"],
      ["--observe", "failed", "--turn-id", "msg_f1", "--class", "provider.rate-limit", "--status", "429"],
    ]),
    `the turn and its failure are reported: ${JSON.stringify(svc.reports())}`,
  )
  svc.emit("session.execution.succeeded", F, {}, DIR)
  await settle()
  assert(svc.waits().length === 0, "nothing is armed after a provider error")
  svc.waitResults.push({ exitCode: 0, stdout: "", stderr: "" })
  userTurn(svc, F, "msg_f2", DIR)
  svc.emit("session.execution.succeeded", F, {}, DIR)
  await settle()
  assert(svc.waits().length === 1, "a person's prompt restores the loop")
  assert(
    same(svc.reports().slice(2), [
      ["--observe", "started", "--turn-id", "msg_f2"],
      ["--observe", "completed", "--turn-id", "msg_f2"],
    ]),
    `a finished turn is reported finished: ${JSON.stringify(svc.reports())}`,
  )
}
for (const [label, promptResult, stale, expected] of [
  ["admitted", undefined, false, "submitted"],
  ["rejected", new Error("400"), false, "indeterminate"],
  ["stale", undefined, true, "refused"],
]) {
  const svc = makeService()
  await withToken(undefined, () => svc.instance(DIR))
  const H = "ses_0helper000000000000000000"
  await meet(svc, H, DIR)
  svc.state.observeAnswer = '{"recovery":"helper"}'
  if (promptResult) svc.promptResults.push(promptResult)
  let release
  svc.waitResults.push((resolve) => {
    release = () => resolve({ exitCode: 0, stdout: '{"opened_by":"msg_h1","n":1,"tag":"clank-recovery-msg_h1-1","wake":"retry now"}\n', stderr: "" })
  })
  userTurn(svc, H, "msg_h1", DIR)
  svc.emit("session.execution.failed", H, { error: { type: "provider.internal", message: "boom", status: 500 } }, DIR)
  await settle()
  assert(typeof release === "function", `${label}: the helper parks: ${JSON.stringify(svc.commands.map((c) => c.args))}`)
  if (stale) svc.emit("session.inbox.enqueued", H, { inboxID: "msg_h9", item: { type: "user" } }, DIR)
  await settle()
  release()
  await settle()
  const replies = svc.commands.filter((c) => c.args.includes("--recovery-reply"))
  assert(replies.length === 1 && same(replies[0].args, ["--recovery-reply", expected, "--attempt", "1", "--opened-by", "msg_h1"]), `${label}: ${JSON.stringify(replies.map((r) => r.args))}`)
  if (stale) {
    assert(svc.prompts.length === 0, `${label}: a stale retry prompts nothing`)
  } else {
    assert(svc.prompts.length === 1 && svc.prompts[0].id === "msg_clank-recovery-msg_h1-1" && svc.prompts[0].text === "retry now", `${label}: ${JSON.stringify(svc.prompts)}`)
  }
  if (label === "admitted") {
    userTurn(svc, H, "msg_clank-recovery-msg_h1-1", DIR)
    await settle()
    assert(
      same(svc.reports().at(-1), ["--observe", "started", "--turn-id", "clank-recovery-msg_h1-1"]),
      `the retry's turn is reported under its tag: ${JSON.stringify(svc.reports())}`,
    )
  }
}
{
  // Reports reach clank in the order the session produced them.
  const svc = makeService()
  await withToken(undefined, () => svc.instance(DIR))
  const O = "ses_0ordered00000000000000000"
  await meet(svc, O, DIR)
  svc.state.holdReports = true
  userTurn(svc, O, "msg_o1", DIR)
  svc.emit("session.execution.failed", O, { error: { type: "provider.auth", message: "no key" } }, DIR)
  await settle()
  assert(svc.reports().length === 1, `the failure waits for the start: ${JSON.stringify(svc.reports())}`)
  svc.state.holdReports = false
  svc.state.heldReports.shift()()
  await settle()
  assert(
    same(svc.reports(), [
      ["--observe", "started", "--turn-id", "msg_o1"],
      ["--observe", "failed", "--turn-id", "msg_o1", "--class", "provider.auth"],
    ]),
    `in order, against its turn: ${JSON.stringify(svc.reports())}`,
  )
  userTurn(svc, O, "msg_o2", DIR)
  svc.emit("session.retry.scheduled", O, { attempt: 1 }, DIR)
  await settle()
  assert(same(svc.reports().at(-1), ["--observe", "retry", "--turn-id", "msg_o2"]), "a native retry is reported against the running turn")
}

// ── A turn is an execution, named by the input it took up ────────────
{
  // The successor is delivered while the old execution still runs, and
  // the old one settles as superseded afterwards (codex on 54144ebf).
  const svc = makeService()
  await withToken(undefined, () => svc.instance(DIR))
  const S = "ses_0superseded000000000000"
  await meet(svc, S, DIR)
  userTurn(svc, S, "msg_old", DIR)
  svc.emit("session.inbox.enqueued", S, { inboxID: "msg_new", item: { type: "user" } }, DIR)
  svc.emit("session.inbox.delivered", S, { inboxID: "msg_new" }, DIR)
  svc.emit("session.execution.interrupted", S, { reason: "superseded" }, DIR)
  svc.emit("session.execution.started", S, {}, DIR)
  svc.emit("session.execution.succeeded", S, {}, DIR)
  await settle()
  assert(
    same(svc.reports(), [
      ["--observe", "started", "--turn-id", "msg_old"],
      ["--observe", "interrupted", "--turn-id", "msg_old"],
      ["--observe", "started", "--turn-id", "msg_new"],
      ["--observe", "completed", "--turn-id", "msg_new"],
    ]),
    `each execution ends against the input it took up: ${JSON.stringify(svc.reports())}`,
  )
  assert(svc.waits().length === 1, "the successor's success arms")

  // Delivered while idle, ahead of its execution; an input steered into
  // a running execution is absorbed by it, and the next execution takes
  // the input delivered for it.
  svc.waitResults.push({ exitCode: 0, stdout: "", stderr: "" }, { exitCode: 0, stdout: "", stderr: "" })
  svc.emit("session.inbox.enqueued", S, { inboxID: "msg_queued", item: { type: "user" } }, DIR)
  svc.emit("session.inbox.delivered", S, { inboxID: "msg_queued" }, DIR)
  svc.emit("session.execution.started", S, {}, DIR)
  svc.emit("session.inbox.enqueued", S, { inboxID: "msg_steer", item: { type: "user", delivery: "steer" } }, DIR)
  svc.emit("session.inbox.delivered", S, { inboxID: "msg_steer" }, DIR)
  svc.emit("session.execution.succeeded", S, {}, DIR)
  svc.emit("session.inbox.enqueued", S, { inboxID: "msg_next", item: { type: "user" } }, DIR)
  svc.emit("session.inbox.delivered", S, { inboxID: "msg_next" }, DIR)
  svc.emit("session.execution.started", S, {}, DIR)
  svc.emit("session.execution.failed", S, { error: { type: "provider.auth", message: "no key" } }, DIR)
  await settle()
  assert(
    same(svc.reports().slice(4), [
      ["--observe", "started", "--turn-id", "msg_queued"],
      ["--observe", "completed", "--turn-id", "msg_queued"],
      ["--observe", "started", "--turn-id", "msg_next"],
      ["--observe", "failed", "--turn-id", "msg_next", "--class", "provider.auth"],
    ]),
    `ends follow executions: ${JSON.stringify(svc.reports())}`,
  )
}

// `pending` holds only inputs waiting for a future execution: a steer is
// consumed with the execution that absorbed it, and only a supersede
// carries it to the successor — whichever of delivery and start comes
// first (codex on d82fee7e), and in the mode it was delivered in, not the
// one it was enqueued with (codex on b2c4591f).
{
  const steer = (svc, S, id) => {
    svc.emit("session.inbox.enqueued", S, { inboxID: id, item: { type: "user", delivery: "steer" } }, DIR)
    svc.emit("session.inbox.delivered", S, { inboxID: id }, DIR)
  }
  const queued = (svc, S, id) => {
    svc.emit("session.inbox.enqueued", S, { inboxID: id, item: { type: "user", delivery: "queue" } }, DIR)
    svc.emit("session.inbox.delivered", S, { inboxID: id }, DIR)
  }
  const switched = (svc, S, id, from, to) => {
    svc.emit("session.inbox.enqueued", S, { inboxID: id, item: { type: "user", delivery: from } }, DIR)
    svc.emit("session.inbox.delivery.changed", S, { inboxID: id, delivery: to }, DIR)
    svc.emit("session.inbox.delivered", S, { inboxID: id }, DIR)
  }
  const cases = [
    [
      "queued, switched to steer before delivery: absorbed",
      (svc, S) => {
        userTurn(svc, S, "msg_first", DIR)
        switched(svc, S, "msg_toggle", "queue", "steer")
        svc.emit("session.execution.succeeded", S, {}, DIR)
        userTurn(svc, S, "msg_next", DIR)
        svc.emit("session.execution.succeeded", S, {}, DIR)
      },
      [["started", "msg_first"], ["completed", "msg_first"], ["started", "msg_next"], ["completed", "msg_next"]],
    ],
    [
      "steered, switched to queue before delivery: waits",
      (svc, S) => {
        userTurn(svc, S, "msg_first", DIR)
        switched(svc, S, "msg_toggle", "steer", "queue")
        svc.emit("session.execution.succeeded", S, {}, DIR)
        svc.emit("session.execution.started", S, {}, DIR)
        svc.emit("session.execution.succeeded", S, {}, DIR)
      },
      [["started", "msg_first"], ["completed", "msg_first"], ["started", "msg_toggle"], ["completed", "msg_toggle"]],
    ],
    [
      "absorbed, next starts before its delivery",
      (svc, S) => {
        userTurn(svc, S, "msg_first", DIR)
        steer(svc, S, "msg_steer")
        svc.emit("session.execution.succeeded", S, {}, DIR)
        userTurn(svc, S, "msg_next", DIR)
        svc.emit("session.execution.succeeded", S, {}, DIR)
      },
      [["started", "msg_first"], ["completed", "msg_first"], ["started", "msg_next"], ["completed", "msg_next"]],
    ],
    [
      "absorbed, next delivered before it starts",
      (svc, S) => {
        userTurn(svc, S, "msg_first", DIR)
        steer(svc, S, "msg_steer")
        svc.emit("session.execution.succeeded", S, {}, DIR)
        queued(svc, S, "msg_next")
        svc.emit("session.execution.started", S, {}, DIR)
        svc.emit("session.execution.succeeded", S, {}, DIR)
      },
      [["started", "msg_first"], ["completed", "msg_first"], ["started", "msg_next"], ["completed", "msg_next"]],
    ],
    [
      "absorbed by a turn the person stopped",
      (svc, S) => {
        userTurn(svc, S, "msg_first", DIR)
        steer(svc, S, "msg_steer")
        svc.emit("session.execution.interrupted", S, { reason: "user" }, DIR)
        userTurn(svc, S, "msg_next", DIR)
        svc.emit("session.execution.succeeded", S, {}, DIR)
      },
      [["started", "msg_first"], ["interrupted", "msg_first"], ["started", "msg_next"], ["completed", "msg_next"]],
    ],
    [
      "superseding steer, successor starts after it",
      (svc, S) => {
        userTurn(svc, S, "msg_first", DIR)
        steer(svc, S, "msg_takeover")
        svc.emit("session.execution.interrupted", S, { reason: "superseded" }, DIR)
        svc.emit("session.execution.started", S, {}, DIR)
        svc.emit("session.execution.succeeded", S, {}, DIR)
      },
      [["started", "msg_first"], ["interrupted", "msg_first"], ["started", "msg_takeover"], ["completed", "msg_takeover"]],
    ],
    [
      "queued behind a running turn",
      (svc, S) => {
        userTurn(svc, S, "msg_first", DIR)
        queued(svc, S, "msg_behind")
        svc.emit("session.execution.succeeded", S, {}, DIR)
        svc.emit("session.execution.started", S, {}, DIR)
        svc.emit("session.execution.succeeded", S, {}, DIR)
      },
      [["started", "msg_first"], ["completed", "msg_first"], ["started", "msg_behind"], ["completed", "msg_behind"]],
    ],
  ]
  for (const [label, drive, expected] of cases) {
    const svc = makeService()
    await withToken(undefined, () => svc.instance(DIR))
    const S = "ses_0pending0000000000000000"
    await meet(svc, S, DIR)
    drive(svc, S)
    await settle()
    assert(
      same(svc.reports(), expected.map(([kind, id]) => ["--observe", kind, "--turn-id", id])),
      `${label}: ${JSON.stringify(svc.reports())}`,
    )
  }
}

// ── A shared service: sessions in different directories ──────────────
{
  const svc = makeService()
  const A = "ses_0dirA000000000000000000000"
  const B = "ses_0dirB000000000000000000000"
  const C = "ses_0dirC000000000000000000000"
  const DIR_A = "/repos/alpha"
  const DIR_B = "/repos/beta"
  const DIR_C = "/repos/gamma"
  svc.sessionDirs.set(C, DIR_C)
  await withToken(undefined, () => svc.instance(DIR_A))
  await withToken(undefined, () => svc.instance(DIR_B))
  await settle()
  assert(svc.bus.subscribers.size === 2, "one subscription per location")
  await meet(svc, A, DIR_A)
  await meet(svc, B, DIR_B)
  await meet(svc, C)

  let releaseA
  svc.waitResults.push((resolve) => (releaseA = () => resolve({ exitCode: 0, stdout: "work-for-A\n", stderr: "" })))
  svc.waitResults.push({ exitCode: 0, stdout: "work-for-B\n", stderr: "" })
  userTurn(svc, A, "msg_a1", DIR_A)
  svc.emit("session.execution.succeeded", A, {}, DIR_A)
  await settle()
  userTurn(svc, B, "msg_b1", DIR_B)
  svc.emit("session.execution.succeeded", B, {}, DIR_B)
  await settle()
  const bySession = (sid) => svc.waits().filter((w) => w.input.session_id === sid)
  assert(bySession(A).length === 1 && bySession(B).length === 1, `one wait per session across the service, and A's pending wait does not suppress B's: ${JSON.stringify(svc.waits().map((w) => w.input.session_id))}`)
  assert(bySession(A)[0].cwd === DIR_A && bySession(A)[0].input.cwd === DIR_A, "A waits in A's directory")
  assert(bySession(B)[0].cwd === DIR_B && bySession(B)[0].input.cwd === DIR_B, "B waits in B's directory")
  assert(svc.prompts.length === 1 && svc.prompts[0].sessionID === B && svc.prompts[0].text === "work-for-B", `B's work goes to B only: ${JSON.stringify(svc.prompts)}`)
  releaseA()
  await settle()
  assert(svc.prompts.length === 2 && svc.prompts[1].sessionID === A && svc.prompts[1].text === "work-for-A", `A's work goes to A only: ${JSON.stringify(svc.prompts)}`)
  const reportsOf = (sid) => svc.commands.filter((c) => c.args.includes("--observe") && c.input.session_id === sid)
  assert(reportsOf(A).length === 2 && reportsOf(A).every((c) => c.cwd === DIR_A), "each session's reports are its own, once each")

  // A session whose events name no location is placed by its own record.
  svc.waitResults.push({ exitCode: 0, stdout: "", stderr: "" })
  userTurn(svc, C, "msg_c1")
  svc.emit("session.execution.succeeded", C, {})
  await settle()
  assert(bySession(C).length === 1 && bySession(C)[0].cwd === DIR_C, `C waits in its own directory: ${JSON.stringify(bySession(C))}`)
}

// ── Unload while a wait is pending ────────────────────────────────────
{
  const svc = makeService()
  const U = "ses_0unload00000000000000000"
  const first = await withToken(undefined, () => svc.instance(DIR))
  await meet(svc, U, DIR)
  let resolveLate
  svc.waitResults.push((resolve) => (resolveLate = resolve))
  userTurn(svc, U, "msg_u1", DIR)
  svc.emit("session.execution.succeeded", U, {}, DIR)
  await settle()
  assert(svc.waits().length === 1, "the wait is out")
  const signal = svc.signals.at(-1)
  const commandsBefore = svc.commands.length
  await first.unload()
  await settle()
  assert(signal?.aborted === true, "unload aborts the stop-hook child")
  resolveLate({ exitCode: 0, stdout: "late-work\n", stderr: "" })
  await settle()
  assert(svc.prompts.length === 0, "a wait that ends after unload prompts nothing")
  svc.emit("session.execution.succeeded", U, {}, DIR)
  await settle()
  assert(svc.commands.length === commandsBefore, `an unloaded instance spawns nothing: ${JSON.stringify(svc.commands.slice(commandsBefore).map((c) => c.args))}`)

  // An idle check that answers after unload arms nothing either.
  const V = "ses_0unloadcheck0000000000000"
  const svc2 = makeService()
  svc2.sessionDirs.set(V, DIR)
  const open = svc2.gate(V)
  const second = await withToken(V, () => svc2.instance(DIR))
  await settle()
  await second.unload()
  open()
  await settle()
  assert(svc2.waits().length === 0, "a late idle answer after unload arms nothing")

  // Its sessions are free: the next live instance takes them over.
  const third = await withToken(undefined, () => svc.instance(DIR))
  svc.waitResults.push({ exitCode: 0, stdout: "", stderr: "" })
  svc.emit("session.execution.succeeded", U, {}, DIR)
  await settle()
  assert(svc.waits().length === 2, `a reloaded instance owns the session again: ${svc.waits().length}`)
  await third.unload()
}

console.log("OK")
