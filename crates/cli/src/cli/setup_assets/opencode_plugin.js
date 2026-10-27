// clank's opencode plugin — installed by `clank setup`. Do not edit
// in place; setup overwrites on upgrade.
//
// Contract: OpenCode 2.x server plugins (verified 2026-10-27 against the
// 2.1.4 loader bundled in the opencode binary, and
// https://opencode.ai/v2/docs/build/plugins/migrate-v1). The loader
// requires a DEFAULT export `{ id, setup }` and rejects anything else
// ("Plugin must export a default definition with an id and an effect or
// setup function"); 1.x's named `ClankPlugin` export never loads in 2.x.
// `setup(ctx)` registers through the context and returns its cleanup:
// `ctx.shell.hook("create.before")` replaces `shell.env`,
// `ctx.event.subscribe()` replaces the `event` hook, and
// `ctx.session.wait/prompt` replace `client.session.status/promptAsync`.
// There is no `$` and no toast API in a server plugin, and the file
// imports nothing from `@opencode-ai/plugin`, so no dependency pin
// matters to it. 1.x is not served: its loader calls every export.
//
// One plugin instance runs per OpenCode location (directory) inside a
// service shared by every session and directory, and setup may run again
// on reload. So nothing here belongs to "the" directory or "the" session:
//
// - Each session is OWNED by exactly one live instance, through a
//   process-wide registry keyed by session id. An instance never acts on
//   a session another owns, and gives up its sessions on disposal.
// - Each session's cwd is its own (the event's location, else the
//   session's record), never the instance's. clank resolves the repo and
//   the agent from that cwd and session; a guessed cwd would bind the
//   session to the wrong repo.
//
// Two jobs (opencode-agent-tool):
//
// 1. BINDING. OpenCode itself exports OPENCODE_SESSION_ID into the shell
//    of the bash tool it runs for a session, AFTER the create.before hook.
//    That is the only shell that knows its session, so the plugin never
//    sets the variable: it BLANKS it, with every foreign agent identity
//    var, in every shell, so a shell opencode does not stamp (a user's
//    `!` shell, a terminal) carries no inherited identity at all. clank
//    reads a blank var as unset.
//
// 2. WORK LOOP: spawn `clank stop-hook --tool opencode` — it long-polls
//    clank for work. NON-EMPTY stdout is a continuation prompt; EMPTY
//    stdout means quiescent, inject nothing (an unconditional nudge would
//    idle-loop the session forever). The wait is ARMED on:
//    - session.execution.succeeded — the steady-state trigger;
//    - load, for CLANK_BOOTSTRAP_SESSION_ID — a resumed session never
//      produces a turn, so without this no wait ever exists (the penlock
//      incident). DETACHED: setup must never block on a long-poll;
//    - first sighting of a session — only once `session.wait` has said
//      it is idle, and only if nothing happened to it meanwhile. Its
//      session.created counts as a sighting even when the bootstrap arm
//      came first: clank launches a fresh session with
//      `--session <id>`, so the bootstrap id can name a session that
//      does not exist yet, whose idle check failed.
//
// Loop discipline:
// - At most ONE in-flight stop-hook wait per session.
// - Only a SUCCEEDED execution arms. An interrupted one is not finished:
//   prodding after Esc restarts exactly what the user stopped. A failed
//   one is not either: re-injecting after a provider error repeats the
//   failure for as long as it lasts; clank's monitor decides whether a
//   retry is made, and the plugin only waits for that decision as a
//   recovery helper. Both latch until an execution takes up a person's
//   next input.
// - Every inbox and execution event is activity. An asynchronous arm
//   (bootstrap, first sighting, renewal) carries the activity count from
//   before its idleness check and is dropped if it moved. A completed
//   wait's continuation is DISCARDED unless the count is unchanged since
//   the triggering settle and no latch is set — clank never auto-acks, so
//   the next settle re-presents the work.
// - Continuations are prompted with delivery "queue": if a turn began in
//   the gap, the prompt waits behind it rather than steering into it.
// - OpenCode keeps no record clank can read, so the plugin reports each
//   turn's start and end (`--observe`). A turn is one EXECUTION, named by
//   the user input it took up (its inbox id), and its end is that
//   execution's settle — never "whichever input was delivered last".
//   `pending` holds only inputs waiting for a FUTURE execution:
//   - delivered into a running execution that has no input yet: it is
//     that execution's;
//   - delivered while idle, or queued behind a running execution that has
//     one: it waits, and the next execution takes the latest one waiting;
//   - delivered (steered) into a running execution that has one: the
//     execution absorbs it, and it is consumed when that execution
//     settles — unless the execution settles as SUPERSEDED, which is the
//     steered input taking over: it then waits for the successor.
//   Queued or steered is the input's mode when it is delivered: a person
//   can switch it while it waits (`session.inbox.delivery.changed`).
//   Per session, each report waits for the one before it, since the
//   monitor reads them as the session's history.
//   OpenCode's message ids must start `msg_`, so a recovery retry is
//   submitted as `msg_<tag>` and reported back under its tag.
// - Diagnostics (exit 0 + stderr) go to OpenCode's log, never into a
//   prompt: a model turn spent on a diagnostic re-arms the same
//   diagnostic when it ends.
// - A deadline returns a renewal control message. The plugin starts
//   another wait while the session is still idle, without creating a
//   model turn.
// - Disposal aborts everything the instance owns: the event subscription
//   and every stop-hook child. Nothing it started may prompt or re-arm
//   afterwards.

import { spawn } from "node:child_process"

const RENEW_WAIT_WIRE = '{"clank":"renew_wait"}'

const FOREIGN_IDENTITY_VARS = [
  "CLAUDE_CODE_SESSION_ID",
  "CODEX_THREAD_ID",
  "GROK_AGENT",
  // Newer grok exports the id alongside the marker; the marker alone
  // used to be the whole of grok's identity in a tool subprocess.
  "GROK_SESSION_ID",
  "CLANK_AGENT",
  // The bootstrap ownership token (set only on clank's resumed
  // launches): a fresh/forked child must not inherit its parent's.
  "CLANK_BOOTSTRAP_SESSION_ID",
]

const SHELL_BLANKS = Object.fromEntries(
  [...FOREIGN_IDENTITY_VARS, "OPENCODE_SESSION_ID"].map((v) => [v, ""]),
)

const OWNERS = Symbol.for("clank.opencode.sessions")

export const runClank = (args, { input, env, cwd, signal }) =>
  new Promise((resolve) => {
    const out = []
    const err = []
    let child
    try {
      child = spawn("clank", args, { cwd, env, signal, stdio: ["pipe", "pipe", "pipe"] })
    } catch (error) {
      return resolve({ exitCode: null, stdout: "", stderr: "", error })
    }
    child.stdout.on("data", (c) => out.push(c))
    child.stderr.on("data", (c) => err.push(c))
    child.on("error", (error) => resolve({ exitCode: null, stdout: "", stderr: "", error }))
    child.on("close", (exitCode) =>
      resolve({
        exitCode,
        stdout: Buffer.concat(out).toString(),
        stderr: Buffer.concat(err).toString(),
      }),
    )
    child.stdin.on("error", () => {})
    child.stdin.end(input)
  })

const consoleLog = (message, extra) => console.error(`[clank] ${message}`, extra ? JSON.stringify(extra) : "")

export const makePlugin = ({ run = runClank, log = consoleLog } = {}) => ({
  id: "clank",
  setup: async (ctx) => {
    const owners = (globalThis[OWNERS] ??= new Map())
    const me = {}
    const abort = new AbortController()
    let disposed = false
    const sessions = new Map()

    const owns = (id) => {
      if (disposed) return false
      const owner = owners.get(id)
      if (owner === undefined) owners.set(id, me)
      return (owner ?? me) === me
    }

    const session = (id) => {
      let s = sessions.get(id)
      if (!s) {
        s = {
          id,
          dir: undefined,
          seen: false,
          inflight: false,
          activity: 0,
          aborted: false,
          failed: false,
          running: undefined,
          pending: [],
          latest: undefined,
          reports: Promise.resolve(),
          inputs: new Map(),
          tags: new Map(),
        }
        sessions.set(id, s)
      }
      return s
    }

    const directory = async (s) => {
      if (s.dir) return s.dir
      try {
        const res = await ctx.session.get({ sessionID: s.id })
        s.dir ??= (res?.data ?? res)?.location?.directory
      } catch (e) {
        log("session lookup failed", { sessionID: s.id, error: String(e) })
      }
      return s.dir
    }

    // The full HookInput shape — stop_hook_active is REQUIRED by the
    // parser (a partial shape is silently swallowed: the hook exits 0
    // with empty stdout on parse errors).
    const clank = (s, dir, args) =>
      run(["stop-hook", "--tool", "opencode", ...args], {
        input: JSON.stringify({ session_id: s.id, cwd: dir, stop_hook_active: false }),
        env: { ...process.env, ...SHELL_BLANKS, OPENCODE_SESSION_ID: s.id },
        cwd: dir,
        signal: abort.signal,
      })

    // `session.wait` resolves once the session's loop is idle. A failed
    // call is unknown, and unknown never arms — arming blind can inject
    // into a running turn.
    const idle = async (s) => {
      try {
        await ctx.session.wait({ sessionID: s.id })
        return !disposed
      } catch {
        return false
      }
    }

    const armWhenIdle = (s, why) => {
      const gen = s.activity
      void (async () => {
        if (await idle(s)) await arm(s, why, gen)
      })().catch((e) => log(`${why} failed`, { sessionID: s.id, error: String(e) }))
    }

    const arm = async (s, why, gen) => {
      if (disposed || s.inflight) return
      if (s.aborted) return log("arm suppressed after an interruption", { sessionID: s.id, why })
      if (s.failed) return log("arm suppressed after a failed turn", { sessionID: s.id, why })
      if (gen !== undefined && s.activity !== gen) {
        return log("arm dropped: session moved during the idle check", { sessionID: s.id, why })
      }
      s.inflight = true
      const seen = s.activity
      let proc
      try {
        const dir = await directory(s)
        if (!dir) return log("arm dropped: the session's directory is unknown", { sessionID: s.id, why })
        if (disposed) return
        log("arm", { sessionID: s.id, why })
        proc = await clank(s, dir, [])
      } finally {
        // The guard covers ONLY the wait. It must be released before the
        // injected turn runs: that turn's settle is what arms the NEXT
        // wait, and a guard held across it makes the loop deliver once
        // and go dormant (codex d7c8908).
        s.inflight = false
      }
      if (disposed || !proc) return
      if (proc.exitCode !== 0) {
        return log("stop-hook did not exit 0", { sessionID: s.id, exitCode: proc.exitCode, error: proc.error && String(proc.error) })
      }
      const output = proc.stdout.trim()
      if (output === RENEW_WAIT_WIRE) {
        if (s.activity === seen && !s.aborted && !s.failed) armWhenIdle(s, "deadline")
        return
      }
      if (!output) {
        const diagnostic = proc.stderr.trim()
        if (diagnostic) log("diagnostic", { sessionID: s.id, diagnostic })
        return
      }
      if (s.activity !== seen) return log("discard stale continuation", { sessionID: s.id })
      if (s.aborted || s.failed) return log("discard after the turn was stopped", { sessionID: s.id })
      log("inject", { sessionID: s.id })
      try {
        await ctx.session.prompt({ sessionID: s.id, text: output, delivery: "queue" })
      } catch (e) {
        log("inject failed", { sessionID: s.id, error: String(e) })
      }
    }

    // Report what clank cannot read for itself. The answer to a failure
    // says whether a recovery helper should wait for an authorized retry.
    const observe = async (s, kind, turn, error) => {
      const args = ["--observe", kind]
      if (turn) args.push("--turn-id", turn)
      if (error?.type) args.push("--class", error.type)
      if (Number.isInteger(error?.status)) args.push("--status", String(error.status))
      const dir = await directory(s)
      if (!dir || disposed) return
      const proc = await clank(s, dir, args)
      try {
        return JSON.parse(proc.stdout.trim() || "{}").recovery
      } catch (e) {
        log("observe failed", { sessionID: s.id, error: String(e) })
      }
    }

    const report = (s, kind, turn, error) => {
      const next = s.reports.then(() => observe(s, kind, turn, error))
      s.reports = next.catch(() => {})
      return next
    }

    const turnName = (s, turn) => s.tags.get(turn) ?? turn

    const bind = (s, turn) => {
      s.running.input = turn
      s.latest = turn
      // The person prompting again is what ends the stop an interruption
      // began, and takes a failed session back.
      s.aborted = false
      s.failed = false
      void report(s, "started", turnName(s, turn))
    }

    const settled = (s, kind, error) => {
      const turn = s.running?.input
      s.running = undefined
      if (!turn) return
      if (kind !== "failed") return void report(s, kind, turnName(s, turn))
      void (async () => {
        const recovery = await report(s, "failed", turnName(s, turn), error)
        if (recovery === "helper" && s.latest === turn) await recoveryHelper(s)
      })().catch((e) => log("failure report failed", { sessionID: s.id, error: String(e) }))
    }

    const reply = (s, dir, answer, dispatch) =>
      clank(s, dir, ["--recovery-reply", answer, "--attempt", String(dispatch.n), "--opened-by", dispatch.opened_by])

    // Park until clank authorizes one retry. The local activity check only
    // drops a stale retry: the server admits the prompt later, so it is
    // not a guarantee the session is still idle.
    const recoveryHelper = async (s) => {
      const seen = s.activity
      const dir = await directory(s)
      if (!dir || disposed) return
      let dispatch
      try {
        const proc = await clank(s, dir, ["--recovery-helper"])
        const out = proc.stdout.trim()
        if (disposed || !out) return
        dispatch = JSON.parse(out)
      } catch (e) {
        log("recovery helper failed", { sessionID: s.id, error: String(e) })
        return
      }
      if (s.activity !== seen || !s.failed) return void (await reply(s, dir, "refused", dispatch))
      const id = `msg_${dispatch.tag}`
      s.tags.set(id, dispatch.tag)
      let answer = "indeterminate"
      try {
        await ctx.session.prompt({ sessionID: s.id, id, text: dispatch.wake, delivery: "queue" })
        answer = "submitted"
      } catch (e) {
        log("recovery prompt failed", { sessionID: s.id, error: String(e) })
      }
      await reply(s, dir, answer, dispatch)
    }

    const onEvent = (event) => {
      const id = event?.data?.sessionID
      if (typeof id !== "string" || !owns(id)) return
      const s = session(id)
      const first = !s.seen
      s.seen = true
      if (event.location?.directory) s.dir = event.location.directory
      const data = event.data
      switch (event.type) {
        case "session.inbox.enqueued":
          s.activity += 1
          s.inputs.set(data.inboxID, data.item)
          break
        case "session.inbox.delivery.changed": {
          const item = s.inputs.get(data.inboxID)
          if (item) s.inputs.set(data.inboxID, { ...item, delivery: data.delivery })
          break
        }
        case "session.inbox.delivered":
          s.activity += 1
          if (s.inputs.get(data.inboxID)?.type === "user") {
            const queued = s.inputs.get(data.inboxID).delivery === "queue"
            if (!s.running) s.pending.push(data.inboxID)
            else if (!s.running.input) bind(s, data.inboxID)
            else if (queued) s.pending.push(data.inboxID)
            else s.running.absorbed.push(data.inboxID)
          }
          s.inputs.delete(data.inboxID)
          break
        case "session.execution.started":
          s.activity += 1
          s.running = { input: undefined, absorbed: [] }
          if (s.pending.length) bind(s, s.pending.at(-1))
          s.pending = []
          break
        case "session.execution.succeeded":
          s.activity += 1
          settled(s, "completed")
          void arm(s, "settled").catch((e) => log("arm failed", { sessionID: id, error: String(e) }))
          return
        case "session.execution.failed":
          s.activity += 1
          settled(s, "failed", data.error)
          s.failed = true
          return
        case "session.execution.interrupted":
          s.activity += 1
          if (data.reason === "shutdown") return
          if (data.reason === "superseded") s.pending.push(...(s.running?.absorbed ?? []))
          settled(s, "interrupted")
          s.aborted = true
          return
        case "session.retry.scheduled":
          void report(s, "retry", s.running?.input && turnName(s, s.running.input))
          break
      }
      if ((first || event.type === "session.created") && !s.inflight) armWhenIdle(s, "first-sighting")
    }

    const pump = async () => {
      while (!disposed) {
        try {
          for await (const event of ctx.event.subscribe({ signal: abort.signal })) {
            if (disposed) break
            try {
              onEvent(event)
            } catch (e) {
              log("event handling failed", { type: event?.type, error: String(e) })
            }
          }
        } catch (e) {
          if (!disposed) log("event stream failed", { error: String(e) })
        }
        // A volatile stream that overflowed is resubscribed after a
        // pause, so a broken stream cannot spin.
        if (!disposed) await new Promise((r) => setTimeout(r, 1000))
      }
    }

    await ctx.shell.hook("create.before", (shell) => {
      Object.assign(shell.env, SHELL_BLANKS)
    })

    void pump()

    const bootstrapId = process.env.CLANK_BOOTSTRAP_SESSION_ID
    if (bootstrapId && owns(bootstrapId)) {
      const s = session(bootstrapId)
      s.seen = true
      armWhenIdle(s, "bootstrap")
    }

    return () => {
      disposed = true
      abort.abort()
      for (const [id, owner] of owners) if (owner === me) owners.delete(id)
    }
  },
})

export default makePlugin()
