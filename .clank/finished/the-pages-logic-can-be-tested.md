# the-pages-logic-can-be-tested

The browser's half of the door is covered by tests, not by reading.

> "removing the promised JS tests leaves the failing lifecycle paths
> unexercised" — codex, on 1c932ab

## Why

Two pages ship inside the binary — `login.html` and `page.html` — and
between them they decide things that matter: whether a refusal means
the stored credential is dead or that this minute was a bad one,
whether an automatic sign-in may be retried, whether a forget that
failed is a forget. Every one of those was got wrong at least once in
`the-phone-scans-the-token-and-keeps-it`, and each was found by a
reviewer reading the script rather than by a test.

The server's half is covered because it is Rust. The browser's half
is not covered at all: this repo has no JS runtime (no `node`, no
`deno`) and no engine in its dependencies, so there is nowhere for a
test to run. That is the gap — not the pages' shape.

## The build

**The decisions leave the markup, as a state machine.** The first
attempt made them pure PREDICATES — `credentialIsDead(reply)`,
`provisionWrites({token, forgetting})` — and that was the wrong model.
Every bug this plan names is about ORDER, not about a value: a
recoverable failure that could not be retried, a sign-in that
succeeded and looped, a sign-out that failed and cleared anyway, a
reply that landed after a forget. Predicates take the ordering as an
argument, so a test supplies `tried` or `forgetting` by hand and
answers for a state the page reaches by a path nothing checks. Three
mutants survived that shape, one of them a successful sign-in going
unrecognised (codex fd3ae49).

So: events in, state and effects out. `doorStart` makes the state,
`doorStep(state, event)` returns the next one and a list of named
effects — `signIn`, `store`, `discard`, `guard`, `navigate`, `say` —
and the pages hold the state, dispatch what happened, and perform what
comes back. No DOM, no `fetch`, no storage on that side.

This is what makes the provisioning race unwritable rather than
merely tested: a page cannot capture `forgetting` early and decide on
it late, because it never passes flags. It passes an event, and the
step reads the state as it is at that moment.

**A runner: an engine as a dev-dependency.** The plan offered a
choice, and this machine settles it — `node`, `deno` and `bun` are all
absent here, so the "use `node` when the machine has one" option
skips on the machine that runs these tests most. A test that always
skips is not a test, and a gate whose verdict depends on what happens
to be installed is the fault the sibling plan
`the-watcher-test-waits-for-quiet` just finished removing.

So: a JS engine as a dev-dependency, `cargo test` alone, no toolchain
beyond Rust. Precedent: the four `oxc_*` dev-dependencies the parse
guard already carries.

Measured, clean build of a bare crate with one dependency, run
sequentially on this machine:

    rquickjs 0.14   3s    12 dependency-tree lines
    boa_engine 0.22 16s   319 dependency-tree lines

`rquickjs` it is. It needs a C compiler, which the Rust toolchain
already requires for linking, so it adds no toolchain this repo did
not have. Both were probed for what this plan needs rather than
assumed from the build: evaluating a function and returning a typed
verdict, and reporting a syntax error instead of swallowing it.

**The pages load the module.** Under the same `include_str!`
discipline as the pages themselves — and INLINED into each at a door
anchor rather than served as a route. One file, so one source of
truth; inlined, so the sign-in page needs no second request before it
can decide whether to sign itself in, and its copy is trivially on the
unauthenticated side of the door instead of a route that has to be
kept there.

The tests read the pages AS RENDERED, not the raw constants: those
carry the anchor and nothing else, so checking them would read a
comment where the decisions will be. The parse guard
(`every-script-this-binary-writes-is-parsed`) then covers the module
for free, as part of the page it lands in.

## Tests

Every one is a SEQUENCE — events dispatched one after another, each
reading the state the last left. A test that supplies `tried` or
`forgetting` directly is the shape that let three mutants live.

- A refusal that marks the credential discards it, and nothing signs
  in afterwards; a rate limit, a store failure, a 403 from another
  origin and an unreachable server each keep it AND retry on the next
  load.
- A sign-in that succeeds navigates and spends the guard, and the
  next load then asks nothing — the loop between the two pages.
- Arriving signed in releases the guard, so a cookie lost later signs
  back in rather than stranding at the form.
- A forget the door refused discards nothing and navigates nowhere,
  and the button works again; a second press mid-flight is not a
  second sign-out.
- A provisioning reply dispatched BETWEEN a forget and its answer
  writes nothing, while the same reply with no forget in flight is
  kept. Only the ordering tells those apart.
- Each page drives the machine it ships: the module body is cut out
  of the rendered page and the wiring that remains must step it and
  dispatch the events that page owns.

## Out of scope

- The pages' appearance, and anything needing a DOM. If a decision
  cannot be made pure, it stays wiring and stays read.
