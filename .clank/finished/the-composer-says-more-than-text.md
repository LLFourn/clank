# the-composer-says-more-than-text
# The composer says more than text

## Why

> "especially the fully featured text entry down the bottom. We need
> the '+' to attach files or images. And the ability to stop the agent
> when it's in progress (do we have the capability to see when the
> agent is working?)"

The box at the bottom of the page is an `<input>` and a Send button.
The reference has a composer: an attach control, the model and mode it
will run as, and a stop button while the agent is working.

Three asks, and they need different things — one is blocked on a fact
we do not record.

## The model

**The pane is a keyboard, so everything the composer does must be
expressible as keystrokes — except the bytes, which become a file.**

`/say` ends at `zellij action write-chars -p <pane> -- <text>` followed
by byte 13 for Return. That settles each ask:

- **Stop** is a keystroke: byte 27 is Escape, which is how the harness
  itself is interrupted. The mechanism already exists — `say_argvs`
  sends byte 13 the same way.
- **Attach** cannot be keystrokes. An image reaches an agent as a
  FILE it is told to read: the page uploads, the server writes it
  somewhere gitignored, and the message names the path. Claude Code
  does the same thing for its own pastes — every one lands in
  `~/.claude/image-cache/<session>/<n>.png`.
- **Working** is not a keystroke at all; it is a fact about the
  harness, and we do not keep it.

### What "working" needs, and what it cannot have

The first draft of this plan said clank runs at every turn end, so one
per-agent stamp makes "working" exact. codex checked that against the
adapters and it is false four times over — all four confirmed here:

1. **Two harnesses have no transcript at all.**
   `transcript::has_adapter` is `Claude | Codex`. For grok and opencode
   there is no activity to compare a stamp against, so "working" is not
   merely unknown, it is unknowable from this evidence.
2. **Grok has no stop-hook adapter.** Its hooks are passive by design
   (`LoopPolicy::Passive` answers with a diagnostic), so no end is ever
   recorded for it.
3. **An abort suppresses the end signal, by design.** The opencode
   plugin adds a session to `aborted` on `MessageAbortedError` and then
   `arm` returns early — the log line is literally "arm suppressed
   after Esc". Pressing Stop is what produces that state, so the
   control would destroy its own evidence.
4. **The clock is epoch seconds.** A turn and a stamp in the same
   second cannot be ordered.

### The model

**Working is EVIDENCE, and the composer must not depend on it.**

The first draft made Stop *replace* Send while working. That is the
fault: it lets an uncertain signal control a mode, so every gap above
becomes a person on a phone who cannot send a message. Instead:

> **Send is always there. Stop appears BESIDE it whenever the agent
> might be working.**

Escape at an idle prompt is harmless; losing the ability to speak is
not. With that asymmetry respected, every one of the four gaps costs
nothing: a missing adapter shows Stop that does nothing, an abort that
reports no end leaves Stop on screen a while longer, a same-second tie
resolves either way. Nothing strands the composer.

So `working` is three-valued and says which it is:

```rust
enum Working { Yes, No, Unknown }   // Unknown: no adapter, or no evidence yet
```

`Unknown` and `Yes` both show Stop; only `No` hides it. The page never
needs certainty because nothing irreversible hangs on it.

The remaining questions are about the evidence itself, and each has one
answer:

- **Where the end is recorded.** In `compute_outcome_with`, once the
  label is resolved and BEFORE anything waits — the hook parks a
  long-poll, so a stamp written after it would be a stamp written when
  the NEXT turn's work arrived.
- **What the stamp is keyed by.** `(agent, session, generation)`. A
  rebound session or a new incarnation must not be measured against its
  predecessor's clock, which is exactly what `mint_wait_generation`
  already exists to express.
- **Which way a tie falls.** Activity at or after the stamp counts as
  working. A one-second tie shows Stop for a moment, which is the
  harmless direction.
- **What Stop does to the state.** The page clears `Yes` for that agent
  optimistically when it sends Escape, so the button settles even for a
  harness that will never report the end.

## Deliverables

1. **A turn-end stamp**, written in `compute_outcome_with` before the
   hook parks its wait, keyed by agent, session and generation.
2. **`working: Yes | No | Unknown` on each agent's facts entry**,
   beside `owes`, not instead of it: one says the gate is waiting, the
   other says the agent is busy, and they are different sentences.
   `Unknown` wherever the evidence cannot exist — no transcript
   adapter, no stop-hook adapter, no stamp yet.
3. **Stop beside Send, never instead of it.** Stop shows while the
   shown agent is `Yes` or `Unknown`, and sends Escape to that agent's
   pane. Send is always available, so no evidence gap can leave a
   person unable to speak.
4. **Attach, behind `+`.** An upload endpoint with its own size cap
   (`/say`'s 64KB limit stays where it is), the bytes written under a
   gitignored per-session directory with a name the SERVER chooses,
   and the sent message naming the path.
5. **A retention rule, decided before it is written.** Every
   attachment is a file nobody deletes on a machine that runs for
   weeks. Claude Code's own cache is the cautionary example: it keeps
   every pasted image forever.
6. **The composer looks like one**: the attach control, the box, Send,
   and Stop when there is something to stop.

## Unknowns to MEASURE first, not reason about

- Whether each agent tool actually reads an image named by path
  mid-session. Claude does. Codex, grok, kimi and opencode are
  unverified, and the answer decides whether this works for reviewers
  or only for the master.
- What Safari hands over for a photo-library pick. iPhone photos are
  HEIC, which the model APIs refuse; whether the `accept` list makes
  Safari transcode is a five-minute test on a real phone.
- Whether Escape interrupts every harness, or only some. With Stop
  beside Send this is no longer load-bearing — a harness that ignores
  Escape gets a button that does nothing rather than a composer that
  does nothing — but it is worth knowing, and it is measurable here.

## Tests

- The stamp and the derivation, case by case: activity after the stamp
  is `Yes`; a stamp with nothing after it is `No`; an agent that has
  never run is `Unknown`; a harness with no transcript adapter is
  `Unknown` whatever its stamp says; a harness with no stop-hook
  adapter is `Unknown`; activity in the SAME SECOND as the stamp is
  `Yes`, because the tie falls the harmless way.
- A session rebound or an incarnation reminted does not inherit the
  previous stamp — the old clock cannot make the new session look busy
  or idle.
- An aborted turn that reports no end (the opencode `arm suppressed
  after Esc` path) leaves the composer usable: Send present, Stop
  eventually settling.
- Stop sends byte 27 to the shown agent's pane, and only while working.
- An upload lands under the gitignored directory, with a name the
  client did not choose, refused over the cap.
- The retention rule, at its boundary.
- In the browser harness: the button's two states, `+` reaching the
  upload, and a draft surviving an attach.
- Mutation-check each.
