// clank's opencode plugin — installed by `clank setup`. Do not edit
// in place; setup overwrites on upgrade.
//
// Two jobs (opencode-agent-tool):
//
// 1. BINDING (shell.env): every tool shell gets OPENCODE_SESSION_ID
//    so `clank as <label>` binds the EXACT session. User PTYs invoke
//    the same hook with no session context — inject only when the
//    hook call carries a sessionID, never guess from cwd or global
//    state. Foreign agent identity vars inherited from a parent
//    agent's shell are scrubbed BY BLANKING (shell.env can only set
//    vars; clank reads a blank var as unset).
//
// 2. WORK LOOP: spawn `clank stop-hook --tool opencode` — it
//    long-polls clank for work. NON-EMPTY stdout is a continuation
//    prompt injected via client.session.promptAsync; EMPTY stdout
//    means quiescent, inject nothing (an unconditional nudge would
//    idle-loop the session forever). The wait is ARMED on:
//    - session.idle — the steady-state trigger;
//    - plugin load, when the launch handed us
//      CLANK_BOOTSTRAP_SESSION_ID — a resumed session never produces
//      a turn, so without this no wait ever exists (the penlock
//      incident), and the arm must be DETACHED so opencode startup
//      never blocks on a long-poll;
//    - first sighting of a session — but only when an explicit
//      `session.status()` says idle: an arbitrary first event is not
//      an idle signal and can arrive mid-turn.
//
// Loop discipline:
// - At most ONE in-flight stop-hook wait per session; an idle firing
//   while a wait is pending is ignored.
// - An INTERRUPTED turn is not a finished one. `session.idle` means
//   the session stopped being busy, which is equally true after the
//   user hits Esc — prodding then restarts exactly what they stopped.
// - A completed wait's continuation is DISCARDED unless the session
//   is still idle at injection time: any session activity observed
//   after the triggering idle invalidates it. Discarding is safe —
//   clank never auto-acks, so the next idle re-presents the work.
// - Diagnostics (exit 0 + stderr) surface as a TUI toast and a log
//   entry, never as an injected prompt: a model turn spent on a
//   diagnostic re-arms the same diagnostic on that turn's idle.

const FOREIGN_IDENTITY_VARS = [
  "CLAUDE_CODE_SESSION_ID",
  "CODEX_THREAD_ID",
  "GROK_AGENT",
  "CLANK_AGENT",
  // The bootstrap ownership token (set only on clank's resumed
  // launches): a fresh/forked child must not inherit its parent's.
  "CLANK_BOOTSTRAP_SESSION_ID",
]

const blanks = () =>
  Object.fromEntries(FOREIGN_IDENTITY_VARS.map((v) => [v, ""]))

export const ClankPlugin = async ({ client, $, directory }) => {
  const inflight = new Set()
  // Per-session activity counter, counting only FIRST SIGHTINGS of
  // user message ids: a wait remembers the count at its triggering
  // idle and injects only if it is unchanged. Anything the session
  // does next begins with a NEW user message, and nothing else is a
  // reliable signal — opencode emits housekeeping right AFTER
  // session.idle (session.updated, session.diff, assistant
  // message.updated, and a RE-update of the turn's own user
  // message), so counting events rather than new ids marks every
  // wait stale (both observed live).
  const activity = new Map()
  const seenUserMessages = new Set()
  const seenSessions = new Set()
  // Sessions whose current turn a HUMAN stopped. opencode marks the
  // interrupted turn's ASSISTANT message with MessageAbortedError —
  // read from there rather than from `session.error`, whose schema
  // makes sessionID OPTIONAL, so it cannot attribute an abort to a
  // session at all.
  const aborted = new Set()

  const sessionOf = (event) => {
    const p = event?.properties ?? {}
    return p.sessionID ?? p.info?.sessionID ?? p.part?.sessionID
  }

  const log = (message, extra) =>
    client.app
      .log({ body: { service: "clank", level: "info", message, ...(extra ? { extra } : {}) } })
      .catch(() => {})

  // Explicit idleness, never a guess. `session.status()` answers a
  // map that is ACTIVE-ONLY: opencode DELETES a session from it on
  // idle (packages/opencode/src/session/status.ts), so a quiescent
  // session reads as a successful map with NO entry. Missing on
  // success = idle; a busy/retry entry = not; a failed call =
  // unknown, and unknown never arms — arming blind can inject into
  // a running turn.
  const isIdle = async (id) => {
    try {
      const res = await client.session.status({ query: { directory } })
      // Only a real payload counts as evidence: the SDK answers HTTP
      // errors WITHOUT throwing, as { error, request, response } with
      // no `data` — reading that wrapper's absent key as "idle" would
      // arm on a FAILED call. Unknown never arms.
      const data = res?.data
      if (!data || typeof data !== "object") return false
      return data[id] === undefined
    } catch {
      return false
    }
  }

  const arm = async (id, why, gen) => {
    if (inflight.has(id)) return
    if (aborted.has(id)) {
      log("arm suppressed after Esc", { sessionID: id, why })
      return
    }
    // For async idle evidence (bootstrap / first-sighting): the
    // activity generation captured BEFORE the status call must still
    // hold, or a turn that started mid-check receives a mid-turn
    // injection with its baseline baked in.
    if (gen !== undefined && (activity.get(id) ?? 0) !== gen) {
      log("arm dropped: session moved during status check", { sessionID: id, why })
      return
    }
    inflight.add(id)
    log("arm", { sessionID: id, why })
    const seen = activity.get(id) ?? 0
    let continuation = ""
    let diagnostic = ""
    try {
      // The full HookInput shape — stop_hook_active is REQUIRED by
      // the parser (a partial shape is silently swallowed: the hook
      // exits 0 with empty stdout on parse errors).
      const input = JSON.stringify({
        session_id: id,
        cwd: directory,
        stop_hook_active: false,
      })
      const proc = await $`echo ${input} | clank stop-hook --tool opencode`
        .env({ ...process.env, OPENCODE_SESSION_ID: id, ...blanks() })
        .quiet()
        .nothrow()
      if (proc.exitCode === 0) {
        continuation = proc.stdout.toString().trim()
        diagnostic = proc.stderr.toString().trim()
      } else {
        log("stop-hook non-zero exit", { sessionID: id, exitCode: proc.exitCode })
      }
    } catch (e) {
      log("stop-hook spawn failed", { sessionID: id, error: String(e) })
    } finally {
      // The guard covers ONLY the wait. It must be released before
      // the injected turn runs: that turn's terminal session.idle
      // is what arms the NEXT wait, and a guard held across it
      // (as an awaited synchronous prompt() would) makes the loop
      // deliver once and go dormant (codex d7c8908).
      inflight.delete(id)
    }
    if (!continuation) {
      // Exit 0 + stderr is a Diagnostic. Toast + log, never a prompt:
      // an injected diagnostic's turn would re-arm this same
      // diagnostic on its idle — a paid loop.
      if (diagnostic) {
        log("diagnostic", { sessionID: id, diagnostic })
        try {
          await client.tui.showToast({
            body: { title: "clank", message: diagnostic, variant: "warning" },
          })
        } catch {}
      }
      return
    }
    if ((activity.get(id) ?? 0) !== seen) return log("discard stale continuation", { sessionID: id })
    // The abort may be observed on EITHER side of the triggering
    // idle — opencode emits housekeeping after it — so the decision
    // is made here, where both orderings have been seen.
    if (aborted.has(id)) {
      log("discard after Esc", { sessionID: id })
      return
    }
    log("inject", { sessionID: id })
    // promptAsync: return-on-accept. The handler must not pin the
    // whole model turn. A rejected injection is logged, never an
    // unhandled rejection (the bootstrap arm is detached).
    try {
      await client.session.promptAsync({
        path: { id },
        body: { parts: [{ type: "text", text: continuation }] },
      })
    } catch (e) {
      log("inject failed", { sessionID: id, error: String(e) })
    }
  }

  // Load-time bootstrap: a resumed launch hands over the ONE session
  // this process owns. Detached AND idle-verified — opencode startup
  // must not block on the long-poll, and the arm must not fire while
  // the resume's own prompt turn is still running.
  const bootstrapId = process.env.CLANK_BOOTSTRAP_SESSION_ID
  if (bootstrapId) {
    const gen = activity.get(bootstrapId) ?? 0
    void (async () => {
      if (await isIdle(bootstrapId)) await arm(bootstrapId, "bootstrap", gen)
    })().catch((e) => log("bootstrap failed", { sessionID: bootstrapId, error: String(e) }))
  }

  return {
    "shell.env": async (input, output) => {
      if (!input.sessionID) return
      output.env.OPENCODE_SESSION_ID = input.sessionID
      Object.assign(output.env, blanks())
    },

    event: async ({ event }) => {
      const id = sessionOf(event)
      if (!id) return
      // Mark seen on EVERY event, including a first-ever idle —
      // otherwise the idle's own housekeeping reads as a first
      // sighting and starts a second wait behind the first.
      const firstSighting = !seenSessions.has(id)
      seenSessions.add(id)
      if (event.type !== "session.idle") {
        const info = event.properties?.info
        if (
          event.type?.startsWith("message.") &&
          info?.role === "assistant" &&
          info.error?.name === "MessageAbortedError"
        ) {
          aborted.add(id)
        }
        if (
          event.type?.startsWith("message.") &&
          info?.role === "user" &&
          info.id &&
          !seenUserMessages.has(info.id)
        ) {
          seenUserMessages.add(info.id)
          activity.set(id, (activity.get(id) ?? 0) + 1)
          // The human prompting again is what ends the stop Esc began.
          aborted.delete(id)
        }
        // First sighting: this session may already be idle with work
        // pending (plugin reload, a hand resume) — but arming on an
        // arbitrary event can inject mid-turn, so the arm is gated on
        // an explicit idle status AND the activity generation captured
        // before the check.
        if (firstSighting && !inflight.has(id)) {
          const gen = activity.get(id) ?? 0
          void (async () => {
            if (await isIdle(id)) await arm(id, "first-sighting", gen)
          })().catch((e) => log("first-sighting failed", { sessionID: id, error: String(e) }))
        }
        return
      }
      await arm(id, "idle")
    },
  }
}
