# harness-adapters-own-each-tools-facts

Invariants: I1, I4.

## Why

Harnesses keep changing. Claude Code, Codex and Grok each changed hook payloads or semantics during the last few months (`asyncRewake`, `background_tasks`, Codex's synchronous Stop, Grok's handoff). Knowledge of each tool's hook behaviour is spread across the hook modules, turn custody, turn policy, the monitor and wait, as `match tool` / `tool == Tool::X` decisions (for example `stop_hook.rs` output shapes and exit codes per tool, `turn_policy.rs` deadline verdicts per tool, `turn_custody.rs` deciding which tools list background tasks). A harness change therefore surfaces as a silent behaviour bug in whichever of those sites was missed.

Two premises were also adopted without an experiment: "Codex has no background notifications" and "the harness tells you when a run ends".

## Scope

The wake path only: how a tool's hooks are parsed, what a tool's hooks can and cannot report, and how clank answers each hook. Launch composition (`agent.rs`, `composer.rs`), setup asset installation (`setup.rs`), transcripts (`web/transcript*`) and pane input also match on `Tool`, but they are not hook semantics and are out of scope; their tool knowledge is static per-tool data rather than behaviour that a harness update silently changes.

## Build

1. **One adapter per tool, the only place on the wake path that knows the tool.** A `harness` module in core with one submodule per tool (Claude, Codex, Grok, OpenCode). Each adapter:
   - parses that tool's hook payloads into clank's typed facts (the reports `turn_policy` consumes);
   - declares the tool's capabilities as data: whether it lists background tasks, whether its Stop hook is async (re-wakes) or synchronous (blocks), whether it reports a background run's completion to a parked agent (`Tool::reports_background_completion` today), what a deadline means for it, and the session environment variable;
   - renders clank's hook answers (continue / renew / quiet) into that tool's output shape and exit code.
   The adapters live in core and stay pure and wasm-compatible: payloads in, typed facts and answers out. Filesystem, config and runtime orchestration (reading a transcript file, resolving a launch home, spawning) stay in the CLI.
2. **Callers ask the adapter, never the tool.** `stop_hook`, `grok_hook`, `question_hook`, `recovery_hook`, `turn_custody`, `turn_policy`, `monitor`, `wait` and `recovery` consult capabilities or call the adapter to decide hook behaviour. The implementation first inventories every `Tool` decision site in those modules (derived from the source, not recalled) and classifies each:
   - **hook semantics** (payload shape, hook answer shape and exit code, whether the tool lists background tasks, async vs blocking Stop, completion reporting, what a deadline means, session env var): moves behind the adapter;
   - **naming** (display, a label in an error): allowed;
   - **out of scope, kept in the CLI** with a one-line reason each: transcript format and location (`recovery::RecordReader::read`, `recovery::record_source`), launch environment (`turn_custody::Inputs::of` choosing Grok's launch home), and any other site the inventory finds that is not hook semantics.
   The gate (Acceptance) enforces exactly this classification, so a new hook-semantics match fails while the listed out-of-scope sites pass by name and reason, and an entry whose site no longer exists also fails.
3. **Recorded fixtures with their tool version.** Each adapter is tested against hook payloads recorded from real runs, stored under `crates/core/tests/fixtures/<tool>/` with a manifest naming the tool version each was recorded with. The existing `stop_hook/` Claude fixtures move there with the version they came from (from git history or the payloads themselves; if unknowable, say so in the manifest rather than invent one).
4. **`clank doctor` warns on version drift.** When an installed tool's version (from its own `--version`, run once by doctor) differs from the manifest's, doctor warns, naming the fixtures to re-record. A tool that is not installed is not a warning.
5. **Behavioural capabilities carry provenance.** A recorded payload proves parsing, not behaviour: it does not show that a tool delivers a background run's completion to a parked agent, that its Stop re-wakes asynchronously, or how it times out. Each behavioural capability value (not the parsing facts) gets a committed provenance entry in `docs/harness-support.toml`, next to the existing per-harness evidence, naming:
   - its status: **observed** (a reproducible experiment, with its conditions, notably a parked Stop versus an active turn), **source** (the tool's versioned source, file and line), **policy** (clank's chosen compatibility behaviour, not a claim about the tool), or **unknown**;
   - the version and the experiment, source reference or decision it rests on, and what remains uncertain.
   Existing evidence is documented honestly as it stands; nothing is promoted to observed without one. Behaviour is preserved unless the evidence supports a change. A gate checks that every behavioural capability the adapters declare has an entry, so a new capability cannot be added as a bare boolean.
6. **The master skill** says a plan built on a claim about harness behaviour cites a recorded fixture or an experiment, and that the claim's provenance entry says which.

## Acceptance

- A source gate in the `it` harness fails if a wake-path module listed in step 2 matches on `Tool` variants outside the adapters, except at the sites its committed inventory classifies as naming or out of scope, each named with its reason; a listed site that no longer exists also fails.
- Each adapter has fixture tests for every hook it parses.
- Every behavioural capability has a provenance entry with a status (observed, source, policy, unknown); a gate fails on a capability without one. No capability whose evidence is unknown is described as a fact in docs or skills.
- The doctor warning is tested with a fixture version that differs from the installed version (the version probe is a seam, so no tool binary runs in the test).
- Existing invariant-listed tests for I1 and I4 still pass; `hook_reach` stays green.
