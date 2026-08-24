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
// 2. WORK LOOP (session.idle): the stop-hook analog. On idle, run
//    `clank stop-hook --tool opencode` — it long-polls clank for
//    work. NON-EMPTY stdout is a continuation prompt injected via
//    client.session.prompt; EMPTY stdout means quiescent, inject
//    nothing (an unconditional nudge would idle-loop the session
//    forever).
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

const FOREIGN_IDENTITY_VARS = [
  "CLAUDE_CODE_SESSION_ID",
  "CODEX_THREAD_ID",
  "GROK_AGENT",
  "CLANK_AGENT",
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

  return {
    "shell.env": async (input, output) => {
      if (!input.sessionID) return
      output.env.OPENCODE_SESSION_ID = input.sessionID
      Object.assign(output.env, blanks())
    },

    event: async ({ event }) => {
      const id = sessionOf(event)
      if (!id) return
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
        return
      }
      if (inflight.has(id)) return
      // Do not even ARM after an interrupt. A wait spawned here would
      // long-poll holding the guard, and the next genuine idle — the
      // one ending the turn the user starts next — would be ignored
      // as in-flight, leaving the loop dormant (the d7c8908 class).
      if (aborted.has(id)) return
      inflight.add(id)
      const seen = activity.get(id) ?? 0
      let continuation = ""
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
        if (proc.exitCode === 0) continuation = proc.stdout.toString().trim()
      } finally {
        // The guard covers ONLY the wait. It must be released before
        // the injected turn runs: that turn's terminal session.idle
        // is what arms the NEXT wait, and a guard held across it
        // (as an awaited synchronous prompt() would) makes the loop
        // deliver once and go dormant (codex d7c8908).
        inflight.delete(id)
      }
      if (!continuation) return
      if ((activity.get(id) ?? 0) !== seen) return // stale: session moved on
      // The abort may be observed on EITHER side of the triggering
      // idle — opencode emits housekeeping after it — so the decision
      // is made here, where both orderings have been seen.
      if (aborted.has(id)) return
      // promptAsync: return-on-accept. The handler must not pin the
      // whole model turn.
      await client.session.promptAsync({
        path: { id },
        body: { parts: [{ type: "text", text: continuation }] },
      })
    },
  }
}
