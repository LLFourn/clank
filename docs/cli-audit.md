# CLI audit

An audit of every clank command path. It records what is hard to understand,
why it is hard, and which queued plan fixes it. This document changes nothing.
The fixes are follow-up plans, listed at the end.

The command set is not typed by hand. The inventory at the bottom is generated
from clap's `Cli::command()` by `cli::command::tests::inventory`. Two tests guard
this document:

- `the_cli_audit_inventory_is_the_real_tree` fails when the embedded inventory
  differs from the real tree. Run it with `CLANK_BLESS_AUDIT=1` to regenerate.
- `the_cli_audit_judges_every_command_path` fails when a command path has no
  verdict, has two verdicts, or a verdict names a path that does not exist.

Sources checked for each path:

- its `--help` (the about and long about, as clap renders them);
- `README.md`, both the `## Command reference` table and every `clank <path>`
  in code;
- the skills `setup` installs (`crates/cli/src/cli/setup_assets/*.md`), plus
  the per-tool work-loop text in `crates/cli/src/cli/setup.rs`;
- the code that invokes it: hooks written by `setup`/`init`, argv clank spawns
  itself, and text clank prints to agents at runtime.

## Summary

The surface has 37 visible top-level commands and 101 command paths, hidden ones included. Very
little is broken. The confusion comes from seven causes, each of which shows
up in several commands:

1. **Plumbing is listed beside user commands.** Hook adapters, the monitor
   daemon and one-off migrations share `clank --help` with
   `status` and `finish`.
2. **One concept lives in two places.** Examples: the user-scope agent
   library sits under `agent`, the team library under `team`; asking is
   `block create` but answering is top-level `unblock`; bare `open` and
   `open zellij` are one command.
3. **Group help lists its children by hand, and the lists have gone stale.**
   `events`, `agent`, `queue` and `auto` each name an older subset of what
   they contain.
4. **Help describes the implementation or its history** instead of when you
   would run the command.
5. **The usage line hides what is required.** clap marks a positional or flag
   optional, and the command then refuses at runtime. Some `--repo` flags are
   only accepted before the subcommand.
6. **One word, two meanings, or one meaning, many words.** For example
   `promote`, `--dry` against `--dry-run`, and five different verbs for
   deleting.
7. **No organising rule for the top level.** The source groups commands with
   comments (`// ── getting set up ──` in `crates/cli/src/cli/command.rs`), but
   `--help` shows a flat list of 37.

Documentation coverage is a separate gap: the README reference table omits
whole commands that agents are told to run.

## Findings

### F1. Plumbing is listed beside user commands

Some commands are run only by a hook clank installed, a process clank spawned,
or a one-time migration. Each appears in `clank --help` as if a user should
learn it.

- `stop-hook` is written into each tool's hook config by `setup` and run by
  the opencode plugin (`setup_assets/opencode_plugin.js`). Its own help says
  "not typically invoked directly". It is visible.
- `question-hook` is installed by `setup` (`setup.rs:1362`). It is already
  hidden, which is the precedent for `stop-hook`.
- `rewire` is the `post-rewrite` git hook body (`init_facts.rs:55`,
  `exec clank rewire --from-stdin`). Without `--from-stdin` it refuses
  (`rewire.rs:15`), so its only working form is the hook's.
- `monitor start` is spawned by clank itself (`monitor.rs:2269`).
- `monitor upgrade` moves monitors that predate self-retirement onto a newer
  build (`monitor_upgrade.rs:1-12`). It is a one-time migration, and nothing
  in the README, the skills or the install steps tells anyone to run it.
- `block migrate` flattens the legacy plan-scoped block layout, which the
  reader no longer sees (`block.rs:17`). It is a migration, not a daily verb.
- `shelve`, `shelve clean` and `unshelve` are hidden aliases whose source
  comment promises "one release of back-compat". Clank has no release
  boundary to measure that against (the crate is `0.0.1` and the repo has no
  tags), so whether the promise has expired is undecided. They are already
  hidden, so they add nothing to `--help`.
- `status --tui` is an alias for `tui`, kept for panes opened before the
  rename. Its help text is corrupted: "sized for a small The full-screen pane
  — an alias for `clank tui`…" (two docs were spliced together).
- `open dry` classifies a path as JSON. Nothing in the codebase or the docs
  calls it outside its own tests (`open.rs:797`, `open.rs:810`). Its help
  still narrates its rename and a review ("codex caught on 30a0194").
- Hidden `agent add --launch-cmd/--launch-arg/--launch-env` are the old launch
  flags, kept for back-compat.

**Recommendation.** Hide the hook and daemon entry points: `stop-hook`,
`rewire`, `monitor`. They keep working; they leave `--help`. Leave the
`shelve` aliases alone until someone sets a compatibility boundary; deleting
them now would rest on an assumption. For the migrations, verify before removing anything: legacy
block directories and pre-retirement monitors live on other machines, and
checking this machine discharges nothing. Hide `block migrate` and
`monitor upgrade`, or make each migration run automatically where the old
shape is first read. Hide or delete `open dry`, depending on whether the
follow-up finds any external caller.

### F2. One concept lives in two places

- **Agent library vs team library.** `agent add` has three modes, and its help
  needs a paragraph to say so. One of them (`--global --tool`) writes the
  user-scope agent library without touching the repo. `agent remove --global`
  deletes from that library. The user-scope team library is managed by
  `team`. So the `agent` group mixes this repo's roster with a user-scope
  library, while a sibling group owns the other library.
- **Blocks.** `block create` asks, top-level `unblock` answers, and
  `block clean` tidies answered pairs. The group's about says "Ask the human a
  blocking question", but the group itself is not that action.
- **Opening the workspace.** Bare `open` and `open zellij` take the same five
  flags and do the same thing. The inventory shows the duplicated flag lists.
- **Seeing the roster.** `export` prints the raw roster config as JSON
  (its README row is `README.md:387`). `agent list -j` prints resolved
  runtime rows, and `team show` prints a saved template. These are three
  shapes, not duplicates. Nothing inside clank calls `export`; whether a
  person or script outside it depends on the raw shape is unknown.
- **Live views.** `status --watch` (re-printing, not full screen), `tui` (the
  full-screen pane) and `status --tui` (an alias). The README table credits
  `status --watch` with "Enter on a github row opens the event page", which is
  the TUI's behaviour (`README.md:366`).
- **Waiting on background work.** `run` (wrap the command) and `attending`
  (declare a task already running) are one concept, and that is correct:
  `run` is the default and `attending` the fallback where `exec` is
  unavailable. But they are taught only by the text the Stop hook prints at
  runtime (`stop_hook.rs:2222`). Neither appears in the README or any skill.
- **Messaging.** `send`, `send --interrupt` and `nudge` are three deliberate
  levels: a durable inbox row the agent must handle; the same row plus a
  best-effort interrupt; and raw terminal input that bypasses the inbox.
  `nudge` does keep a record, of a different kind: an attempt history
  written after each outcome (`nudge.rs:11-13`, `nudge_log`), not an item
  that waits to be acknowledged. Its help says it "leaves no durable
  record", which is stale. The README has one line for `send` and none for
  the other two.

**Recommendation.** Make each group own one noun at one scope. Move the
user-scope agent library under `team` (or a `library` group) so that `agent`
means this repo's roster only, and `agent add` has one mode. Give blocking one
group (`block ask` / `block answer` / `block clean`, or top-level
`block`/`unblock` as plain actions with the cleanup automatic). Make `open`
one action. Keep `export`: `agent list -j` is not a substitute, because its rows are
resolved rather than raw. Keep `run`/`attending` and the three
messaging levels, and document them (F8).

### F3. Group help lists its children by hand, and the lists are stale

clap already prints a group's subcommands. Each group about that also
enumerates them is a second copy, and the copies have drifted:

- `events`: about says "list, ack, show". It also has `reply` and `read`.
- `agent`: about names six subcommands. `swap` and `launch` are missing.
- `queue`: about names `add`, `promote`, `remove`, `reprioritise`. `unqueue`
  is missing.
- `auto`: about promises "an optional role designation". `--role` is ignored
  (`auto.rs:49-55`), and `on`/`off` both say "Optionally update role".
- `agent set-review`: the `<REVIEW>` doc says "`commit`, `plan`, `final`, or
  `gate`". The value list clap prints beneath it includes `standby`.
- `agent list`: "(master + reviewers)". Standby agents are neither.

**Recommendation.** A group's about states what the group is for and never
enumerates its children. Remove `auto --role`; it has been a no-op since
roles moved to the roster.

### F4. Help describes the implementation or its history

A user reads `--help` to learn when to run a command and what it changes.
These texts answer a different question:

- `agent start` ends with "(agent-start-bootstraps-missing-skeleton)", the
  name of the plan that added the behaviour.
- `agent promote` gives its config write, `repo.agents[<name>].role = master`.
- `open dry`: "Renamed from `clank open <path>`", and its argument doc cites a
  review sha.
- `monitor`'s long about narrates the design it replaced ("A repo was being
  watched once per WAITER…").
- `agent remove`: "Per-agent skeleton dir + feedback history are preserved".
- `agent add`: "ONE step — definition + role. Three modes: …" (see F2).
- `html open` lists the browser launchers (`open` / `xdg-open` /
  `explorer`).
- `rereview`: "Use after a promotion auto-continued a demoted master's
  self-review." It is accurate, but it is the only hint of when a person would
  run it, and it assumes the reader knows the gate's internals.
- `status --tui` is corrupted (F1).

**Recommendation.** Rewrite these from the caller's seat: what it does, when
you would run it, what it changes. Move history and rationale to the source
comments, or delete them.

### F5. The usage line hides what is required

- `unfinish [PLAN]`: the argument doc says "Required". The command bails
  without it (`unfinish.rs:14`), and clap marks it optional.
- `rewire`: `--from-stdin` is shown as an option, but the command refuses
  without it (`rewire.rs:15`).
- `--repo` on a group: `queue`, `pr-review`, `html`, `config` and `open`
  declare `--repo` on the group without `global = true`. So
  `clank queue promote foo --repo .` fails with "unexpected argument '--repo'"
  (reproduced). `remote` declares it `global = true` and accepts it in both
  places. Every leaf command accepts `--repo` after its own name, so the
  group-level ones are a trap. `html_open_argv` has a test pinning the
  flags-before-subcommand order, which is a symptom of the same trap.
- `feedback write --author` is required. Every other command that records an
  author (`events ack`, `block create`, `run`, `attending`) defaults it from
  the session binding. No rationale for the difference was found in the
  source. The follow-up must establish whether the explicit author is
  deliberate before changing it.
- `config <key> [ACTION] [VALUE]`: `ACTION` is a free string, `get` or
  `set`, so a write is `clank config diff.wait set true`. The help cannot show
  which form reads and which writes, and a typo in `ACTION` is a runtime
  error.

**Recommendation.** Let clap express each constraint, so that the usage line
is the contract: required positionals, a required flag (or none, for
`rewire`), `global = true` on every group-level `--repo`, and a `config`
shape where the presence of a value means set (`clank config <key> [value]`,
as `git config` does).

### F6. One word, two meanings, or one meaning, many words

- **`promote`**: `queue promote` activates a queued plan; `agent promote`
  makes an agent master. The `agent promote` help has to add "(Distinct from
  `clank queue promote` …)". Both are frequent, both are taught in the master
  skill, and the stop hook says "promote:" for the queue one.
- **Delete verbs**: `queue remove`, `agent remove`, `fork remove`,
  `team delete`, `stash drop`, `block clean`, `shelve clean`, `purge --drop`.
  Some differences are real (`stash drop` discards commits; `queue unqueue`
  keeps the body where `queue remove` deletes it). The rest are accidents.
- **Dry runs**: `setup --dry-run`; `--dry` on `finish`, `stash`, `pick`,
  `purge` and `block migrate`; `open dry` is a subcommand.
- **Printing instead of acting**: `--print` on `open`, `agent start` and
  `diff`; `--print-path` on `html open`.
- **The agent argument**: `<LABEL>` for `agent add`, `agent remove` and `as`;
  `<NAME>` for `agent start`, `agent promote`, `agent set-review` and
  `agent launch`. Both mean the roster label.
- **`events show` vs `events read`**: the difference is real and important.
  `list` and `show` give metadata without content; `read` returns content
  (external, untrusted text) and records delivery. The names do not carry it,
  and the group about mentions neither `read` nor the distinction (F3).
  Renaming is optional; the about must say it.
- **`queue remove` vs `queue unqueue`**: real (delete vs return to drafts) and
  documented in `unqueue`'s help. Keep both. Name the difference in the
  group's about.

**Recommendation.** Rename `agent promote` to a verb that cannot be read as
the queue one (for example `agent make-master`), with the old name as a hidden
alias for one release. Pick one dry-run flag (`--dry-run`, as `git` does,
with `--dry` hidden) and one print flag. Call the agent positional `<AGENT>`
everywhere. Use `remove` for taking an entry out of a list and keep `drop`
only where commits are discarded. Leave real distinctions alone and document
them.

### F7. No organising rule for the top level

`clank --help` lists 37 commands in source order. The source groups them by
comment, which clap does not render, so a reader sees `as` next to `auto`
next to `open` with no sign that one is a session binding, one a hook toggle
and one a workspace launcher.

A `plan` group (`clank plan finish`, `clank plan stash`, …) was considered and
is not recommended. `finish`, `stash`, `pick`, `purge`, `unfinish`,
`rereview` and `diff` all act on "the plan", the way git's top-level verbs act
on "the repository". Making the most-typed command (`finish`) longer to buy
grouping is a poor trade.

**Proposed rule.**

- A **group** is a noun with its own lifecycle at one scope: `queue`,
  `agent` (this repo's roster), `team` (user-scope templates and agent
  descriptions), `stash`, `fork`, `events`, `feedback`, `block`, `remote`,
  `config`, `pr-review`.
- A **top-level verb** acts on the repo or its active plan, and a person or
  agent types it: `init`, `setup`, `doctor`, `status`, `tui`, `log`,
  `finish`, `unfinish`, `pick`, `purge`, `rereview`, `diff`, `html`, `open`,
  `send`, `nudge`, `run`, `attending`, `as`, `auto`, `wait`.
- **Plumbing** (hook adapters, the daemon, migrations, aliases) is hidden,
  and still runnable.
- `--help` renders the top level in the source's sections. clap 4 cannot
  group subcommands under several headings, so this needs a help template
  generated from one table in `command.rs`. A test should fail when a
  command is missing from that table, so the table cannot become a second,
  stale list.

**Resulting tree** (visible commands only; changes from F1, F2 and F6
applied):

```
getting set up    init  setup  doctor
the daily flow    status  tui  log  wait  as  auto  run  attending
plans             queue {add promote remove unqueue reprioritise}
                  finish  unfinish  stash {push pop show drop list}
                  pick  purge  rereview  diff  html {open}
review            feedback {write read}  pr-review {start note propose abort finish status}
                  block {ask answer clean}
the team          agent {list add remove make-master set-review swap launch start}
                  team {save list show remove, agent library add/remove}
                  open  fork {create list remove}
talking           send  nudge  events {list show read ack reply}
running remote    remote {status on off}
settings          config <key> [value]  export
```

### F8. The README reference does not cover the surface

The README table (`README.md:354-389`) has no row for `remote`, `monitor`,
`nudge`, `attending`, `run` or `rereview`. Inside groups it omits
`agent swap`, `agent launch`, `events reply`, `events read` and
`queue unqueue`. `run` and `attending` are absent from every skill as well;
an agent learns them only from a stop-hook message. The existing test
`the_readme_names_only_real_subcommands` checks one direction only: every
command the README names exists. Nothing checks the other direction, that
every visible command is in the table.

**Recommendation.** Add every visible top-level command and every visible
subcommand to the table. Extend the README test to the other direction for
visible paths. Hidden plumbing (F1) stays out of the table by design. A
command missing from the README is a gap; a hook entry point missing from it
is correct.

## Verdicts

Audience: **human** (typed by a person), **agent** (taught to agents by a
skill or runtime text), **both**, **hook** (run by something clank installed
or spawned), **legacy** (an alias or migration). Documented: **R** = README,
**S** = a skill or setup work-loop text, **H** = `--help` only, **rt** =
runtime text only. Verdicts: keep / fix doc / rename / merge / hide / remove.

<!-- verdicts:start -->
| path | when you would run it | audience | documented | verdict | finding |
|---|---|---|---|---|---|
| `init` | once per repo, to start using clank there | human | R | keep | |
| `setup` | after installing or upgrading clank | human | R | rename flag | F6: `--dry-run` is the one to keep |
| `doctor` | when something is not waking or binding | both | R | keep | |
| `status` | to see the plan, its gate and the agents | both | R S | fix doc | F1/F2: `--tui` text corrupted; README credits `--watch` with TUI keys |
| `wait` | Grok agents arm it; people use `--for` to block on an event | both | R S | keep | Grok agents arm it themselves (`setup.rs` WORK_LOOP_GROK); `--for` serves observers; Claude/codex/opencode skills forbid it, correctly |
| `queue` | to see what plans are lined up | both | R S | fix doc | F3: about omits `unqueue`; F5: `--repo` only before the subcommand |
| `queue add` | to line up a drafted plan | agent | R S | keep | |
| `queue remove` | to discard a queued plan | both | H | fix doc | F6: real distinction from `unqueue`, not in the README |
| `queue promote` | when clank says the next plan can start | agent | R S | keep | F6: the clash is fixed on the `agent` side |
| `queue reprioritise` | to reorder the queue | both | H | fix doc | F8 |
| `queue unqueue` | to take a plan back to drafts for editing | both | H | fix doc | F8 |
| `feedback` | (group) | agent | R S | keep | |
| `feedback write` | a reviewer recording its verdict | agent | R S | fix usage | F5: `--author` required, unlike every other author flag; establish why |
| `feedback read` | the master reading a commit's verdicts | agent | S | fix doc | F8 |
| `events` | (group) | agent | R S | fix doc | F3: about lists three of five; F6: show/read distinction unstated |
| `events list` | to see unhandled github and other events | agent | R S | keep | |
| `events ack` | when an event is dealt with | agent | R S | keep | |
| `events reply` | to answer an event without closing it | agent | R | fix doc | F8: missing from the reference table |
| `events show` | to inspect an event's metadata safely | agent | R S | fix doc | F6 |
| `events read` | to read an event's untrusted content | agent | R S | fix doc | F6, F8 |
| `send` | to message another agent | both | R S | keep | |
| `nudge` | when an agent cannot receive `send` | human | H | fix doc | F2: help says no durable record, but each attempt is logged; F8 |
| `monitor` | (group, plumbing) | hook | none | hide | F1 |
| `monitor start` | never by hand; clank spawns it | hook | none | hide | F1: spawned by clank (`monitor.rs:2269`) |
| `monitor upgrade` | once, to retire monitors from old builds | legacy | H | hide | F1: one-time migration; verify before removing |
| `remote` | (group) | human | R | fix doc | F8: missing from the reference table |
| `remote status` | to see whether the remote is serving | human | R | keep | |
| `remote on` | to start serving the repo remotely | human | R | keep | |
| `remote off` | to stop serving it | human | H | keep | |
| `log` | to read the commit and review timeline | human | R | keep | |
| `finish` | when the gate says FINISHED | agent | R S | rename flag | F6: `--dry` |
| `unfinish` | to reopen a plan finished by mistake | human | R | fix usage | F5: `[PLAN]` is required |
| `agent` | (group) | human | R S | fix doc | F3: about lists six of eight |
| `agent list` | to see the roster | both | S | fix doc | F3: "(master + reviewers)" |
| `agent start` | to launch or resume one agent | human | R | fix doc | F4: plan name in help; F6: `<NAME>` |
| `agent add` | to put an agent on the roster | human | R S | merge | F2: three modes; the library mode moves to `team` |
| `agent promote` | to make an agent master | human | S | rename | F6: clashes with `queue promote`; F4: help shows the config write |
| `agent remove` | to take an agent off the roster | human | S | merge | F2: `--global` moves to `team`; F4 |
| `agent set-review` | to change what a reviewer reviews | human | S | fix doc | F3: tier list omits `standby` |
| `agent swap` | to replace one reviewer with another | human | H | fix doc | F8 |
| `agent launch` | to see or set how an agent's tool is launched | human | R S | keep | F6: `<NAME>` |
| `team` | (group) | human | R | merge | F2: gains the agent library |
| `team save` | to reuse this repo's roster elsewhere | human | R | keep | |
| `team list` | to see saved teams | human | H | fix doc | F8 |
| `team show` | to see one saved team | human | H | fix doc | F8 |
| `team delete` | to discard a saved team | human | H | rename | F6: `remove` |
| `as` | in a session you started by hand | both | R S | keep | |
| `auto` | (group) | both | R S | fix doc | F3: promises a role designation |
| `auto on` | to let the Stop hook drive the session | both | R S | remove flag | F3: `--role` is a no-op |
| `auto off` | to drive the session by hand | both | R S | remove flag | F3 |
| `auto status` | to check which it is | both | S | keep | |
| `open` | to open the team's workspace | human | R | merge | F2: bare `open` is `open zellij` |
| `open dry` | no known reason | legacy | H | hide | F1: no internal caller outside its tests, external use unknown; F4 |
| `open zellij` | the same as bare `open` | human | H | merge | F2 |
| `fork` | (group) | human | R | keep | |
| `fork create` | to try something in a parallel worktree | human | R | keep | |
| `fork list` | to see the forks | human | H | fix doc | F8 |
| `fork remove` | to clean one up | human | H | fix doc | F8 |
| `stash` | to set a plan aside for another | agent | R S | rename flag | F6: `--dry` |
| `stash push` | the same as bare `stash` | agent | S | keep | |
| `stash pop` | to bring the plan back | agent | S | keep | |
| `stash show` | to see what was set aside | both | R | keep | |
| `stash drop` | to discard it | both | R | keep | F6: `drop` is right, commits are discarded |
| `stash list` | to see what is set aside | both | R | keep | |
| `shelve` | old spelling of `stash` | legacy | none | keep | F1: already hidden; expiry undecided, no release boundary exists |
| `shelve clean` | old spelling of `stash drop` | legacy | none | keep | F1 |
| `unshelve` | old spelling of `stash pop` | legacy | none | keep | F1 |
| `pick` | to bring plans over from another branch | human | R | rename flag | F6: `--dry` |
| `purge` | to remove a plan's traces from history | agent | R S | rename flag | F6: `--dry`, `--drop` |
| `pr-review` | (group) | agent | S | fix usage | F5: `--repo` only before the subcommand; F8 |
| `pr-review start` | the master starting a review of a GitHub PR | agent | S | keep | |
| `pr-review note` | a reviewer recording its verdict on the draft | agent | S | keep | |
| `pr-review propose` | the master asking for another round | agent | S | keep | |
| `pr-review abort` | to abandon the review | agent | S | keep | |
| `pr-review finish` | to hand the agreed review over | agent | S | keep | |
| `pr-review status` | to see who is still reviewing | agent | S | keep | |
| `diff` | to read a plan's changes in your editor | human | R | rename flag | F6: `--print` kept; target heuristic is documented |
| `html` | to build the static site | human | R | fix usage | F5: `--repo` only before the subcommand |
| `html open` | to look at it in a browser | human | R | fix doc | F4; F6: `--print-path` |
| `block` | (group) | agent | R S | merge | F2: ask and answer in one group |
| `block create` | an agent asking the human a question it cannot answer | agent | R S | rename | F2: `block ask` |
| `block clean` | tidying answered questions | agent | S | keep | |
| `block migrate` | once, on a repo with the old layout | legacy | H | hide | F1: verify legacy layouts are gone or migrate on read |
| `unblock` | the human answering | human | R | merge | F2: `block answer` |
| `attending` | an agent waiting on a task it started some other way | agent | rt | fix doc | F2, F8: taught only by stop-hook text |
| `tui` | to drive a running team | human | R | keep | |
| `run` | an agent backgrounding a long command | agent | rt | fix doc | F2, F8: taught only by stop-hook text |
| `config` | to see every setting | human | S | fix usage | F5: `[ACTION] [VALUE]`; `--repo` only before the key |
| `config review.adhoc_feedback` | to set this value | human | H | fix usage | F5 |
| `config review.plan_feedback` | to set this value | human | H | fix usage | F5 |
| `config review.require_commit_prefix` | to set this value | human | H | fix usage | F5 |
| `config hooks.blocked` | to set this value | human | H | fix usage | F5 |
| `config diff.editor.command` | to set this value | human | H | fix usage | F5 |
| `config diff.wait` | to set this value | human | H | fix usage | F5 |
| `config finish.autosquash` | to set this value | human | H | fix usage | F5 |
| `config recovery.blind_nudge` | to set this value | human | H | fix usage | F5 |
| `export` | to get the raw roster JSON | human | R | keep | F2: raw shape differs from `agent list -j`; no internal caller, external use unknown |
| `rereview` | to make reviewers look at a commit again | human | H | fix doc | F4, F8 |
| `rewire` | never by hand; the git hook runs it | hook | R | hide | F1, F5: only the hook form works |
| `stop-hook` | never by hand; the tool's hook runs it | hook | R | hide | F1 |
| `question-hook` | never by hand; the tool's hook runs it | hook | none | keep | already hidden |
<!-- verdicts:end -->

## Follow-up plans

Each one is queued as a draft. Each fixes one cause and names its boundary.
They are ordered by how much confusion each removes per unit of breakage:
non-breaking documentation first, then hiding, then usage contracts, then
the renames and merges that change what people type.

| priority | plan | fixes | breaks anything typed? |
|---|---|---|---|
| 510 | `cli-help-says-what-it-is-for` | F3, F4, `status --tui` text | no |
| 515 | `cli-help-is-beautiful` | F7: sectioned, styled top-level help, one-line abouts, tested examples | no |
| 520 | `cli-readme-covers-the-surface` | F8, plus the README test's reverse direction | no |
| 530 | `cli-plumbing-out-of-sight` | F1 | no |
| 540 | `cli-usage-is-the-contract` | F5 | `config` gains the git shape; the old one stays as a hidden form |
| 550 | `cli-one-noun-one-group` | F2 | yes, with hidden aliases for one release |
| 560 | `cli-one-word-one-meaning` | F6 | yes, with hidden aliases for one release |

## Inventory

Generated. Do not edit by hand.

<!-- inventory:start -->
| path | kind | hidden | aliases | about | positionals | flags |
|---|---|---|---|---|---|---|
| `init` | action |  |  | Scaffold `.clank/` in a repo |  | --repo --force-hooks --force --team |
| `setup` | action |  |  | Install skills and hooks into ~/.claude, ~/.codex, ~/.grok |  | --grok-agent --repo --force --dry-run |
| `doctor` | action |  |  | Check the clank setup across repo, user, and session |  | --repo -j/--json |
| `status` | action |  |  | Show the repo's clank state |  | --repo -j/--json --no-cache --plan --watch --tui |
| `wait` | action |  |  | Block until this agent has actionable work |  | --repo --author --die-with-owner --deliver-to-session (hidden) -j/--json --event --for --peek --no-cache --poll --no-poll |
| `queue` | group, bare runs |  |  | Manage the plan queue |  | --repo |
| `queue add` | action |  |  | Add a queued plan at `.clank/queue/<NNN>-<name>.md`. Body comes from `.clank/drafts/<name>.md` (write the draft, then `queue add <name>` consumes it) or `-m "<body>"` for a trivial inline body. Empty / header-only content is rejected | <NAME> | --priority -m/--message |
| `queue remove` | action |  |  | Remove an item from the queue | <NAME> |  |
| `queue promote` | action |  |  | Promote a queued item to an active plan | <NAME> |  |
| `queue reprioritise` | action |  | reprioritize | Change a queued item's priority (renames its `NNN-` prefix) | <NAME> <PRIORITY> |  |
| `queue unqueue` | action |  |  | Take an item out of the queue and back to `.clank/drafts/<name>.md` — the inverse of `add`. `remove` deletes; this keeps the body | <NAME> |  |
| `feedback` | group |  |  | Read or write review feedback |  |  |
| `feedback write` | action |  |  | Write a feedback file |  | --repo --commit (required) --verdict (required) --author (required) -m (required) |
| `feedback read` | action |  |  | Show all feedback for a commit |  | --commit --plan --repo -j/--json |
| `events` | group |  |  | The github event inbox: list, ack, show |  |  |
| `events list` | action |  |  | List event metadata without reading content (unhandled by default) |  | --repo --author --all -j/--json |
| `events ack` | action |  |  | Mark events handled so they stop waking you | <ID> | --reply --repo --author |
| `events reply` | action |  |  | Send a further reply to an event whose backend takes replies, without acknowledging it | <ID> | --repo --author --operation -m/--message -F/--file one of --message/--file (required) |
| `events show` | action |  |  | Inspect one event's metadata without reading content | <ID> | --repo --author -j/--json |
| `events read` | action |  |  | Read an event's content and record delivery to your bound agent | <ID> | --repo --author -j/--json |
| `send` | action |  |  | Put a message in an agent's inbox |  | --reply-to --reply-repo --interrupt --repo --to --master -m/--message -F/--file one of --to/--master (required) one of --message/--file (required) |
| `nudge` | action |  |  | Interrupt an agent's current turn, then submit a message to its terminal |  | --repo --to --master -m/--message -F/--file one of --to/--master (required) one of --message/--file (required) |
| `monitor` | group |  |  | Watch a repo once, for every waiter |  |  |
| `monitor start` | action |  |  | Watch this repo once, for every waiter |  | --repo --installed-at (hidden) --installed-from (hidden) |
| `monitor upgrade` | action |  |  | Ask every reachable monitor older than this build to stand down, and start this build's in their repos. Run once after installing |  |  |
| `remote` | group |  |  | Switch this repo's remote on or off through its `clank tui` |  | --repo --timeout |
| `remote status` | action |  |  | Who owns this repo's remote, and its state and address |  |  |
| `remote on` | action |  |  | Start serving; already on is done at once |  |  |
| `remote off` | action |  |  | Stop serving; already off is done at once |  |  |
| `log` | action |  |  | Print the repo timeline: commits, reviews, github events | [RANGE] | --repo -j/--json --plan -n --oneline --no-github --diagnostics |
| `finish` | action |  |  | Finalize a plan into `finished/` | [PLAN] | --repo -m/--message --purge --squash --no-squash --into-branch --dry --force --no-cache |
| `unfinish` | action |  |  | Move a finalized plan back to active | [PLAN] | --repo |
| `agent` | group |  |  | Manage this repo's agent roster |  |  |
| `agent list` | action |  |  | Enumerate the repo's roster (master + reviewers) with their role, review tier, and bind state |  | --repo -j/--json |
| `agent start` | action |  |  | Launch an agent's CLI tool with any configured `launch` profile applied. With a bound session it RESUMES that session; without one it bootstraps, exec'ing the bare tool with a seed prompt telling the agent to run `clank as <name>` (agent-start-bootstraps-missing-skeleton) | <NAME> | --repo --print |
| `agent add` | action |  |  | Add an agent to the repo's roster (ONE step — definition + role). Three modes: `--global --tool` writes a reusable DESCRIPTION to the user-scope library (no repo change); repo-scope `--tool` defines a fresh agent inline; repo-scope by-name (no `--tool`) copies a description from the user-scope library. `--review commit\|plan\|final\|gate\|standby` sets the tier (default `commit`); `standby` reviews nothing | <LABEL> -- TOOL ARGS… | --tool --review --global --cmd --env --launch-cmd (hidden) --launch-arg (hidden) --launch-env (hidden) --initial-prompt --repo |
| `agent promote` | action |  |  | Promote an agent to the repo's master: `repo.agents[<name>].role = master`, demoting the previous master to `commit`. The agent must already be on the roster. (Distinct from `clank queue promote`, which activates a queued plan.) | <NAME> | --repo |
| `agent remove` | action |  |  | Remove an agent from the repo's roster. `--global` instead drops a DESCRIPTION from the user-scope `agents` library (and scrubs any team template referencing it). Per-agent skeleton dir + feedback history are preserved | <LABEL> | --global --repo |
| `agent set-review` | action |  |  | Change a non-master agent's tier (commit / plan / final / gate, or standby: reviewing nothing) in place — no remove/re-add, so the agent's session and zellij pane are undisturbed. Refuses on the master (not a reviewer tier) | <NAME> <REVIEW> | --repo |
| `agent swap` | action |  |  | Replace a reviewer with another, keeping the role: one config write, so the role is never observed vacant and the gate's expected reviewer set never briefly shrinks. Refuses on the master (use `agent promote`) and refuses when the incoming label is already on the roster | <OUT> <IN> | --repo |
| `agent launch` | action |  |  | Show an agent's launch profile and the command `agent start` would run. Given `--cmd`, `--env` or `-- <args>`, that is the agent's whole new launch (nothing is kept from before); `--clear` is the bare tool. The agent keeps its role, session and pane, and the change applies from its next start | <NAME> -- TOOL ARGS… | --cmd --env --clear --repo |
| `team` | group |  |  | Save and reuse roster templates across repos |  |  |
| `team save` | action |  |  | Publish THIS repo's roster as a reusable global template under `<name>` (user-scope `teams`), copying the roster's agent definitions into user-scope `agents`. Refuses if a referenced agent already exists globally with a different body; pass `--force` to overwrite an existing team name | <NAME> | --force --repo |
| `team list` | action |  |  | List the global team-template library (user-scope) |  | -j/--json |
| `team show` | action |  |  | Show a single global team template (its roster) | <TEAM> | -j/--json |
| `team delete` | action |  |  | Delete a global team template. Refuses without `--force` (clank can't introspect which repos reference it) | <TEAM> | --force |
| `as` | action |  |  | Bind this session to a roster label | <LABEL> | --repo |
| `auto` | group |  |  | Manage this agent's auto-mode |  |  |
| `auto on` | action |  |  | Enable auto-mode (wait-for-work long-poll). Optionally update role |  | --repo --role |
| `auto off` | action |  |  | Disable auto-mode. Optionally update role |  | --repo --role |
| `auto status` | action |  |  | Print current auto-mode + role for the calling agent |  | --repo -j/--json |
| `open` | group, bare runs |  |  | Open the team workspace in zellij |  | --repo --fork --pr --all --print |
| `open dry` | action |  |  | Classify a path against the active repo; emit JSON or human-readable. Renamed from `clank open <path>` | <PATH> | -j/--json |
| `open zellij` | action |  |  | Auto-generate a zellij layout (KDL) for master + reviewer panes and spawn it via `zellij --layout` |  | --repo --fork --pr --all --print |
| `fork` | group |  |  | Fork the team into a linked worktree |  |  |
| `fork create` | action |  |  | Fork the team into a linked worktree (or `--clone`) | [NAME] [SOURCE] | --fresh --pr --branch --path --clone --team -d/--draft --prompt --no-open --review |
| `fork list` | action |  |  | List this repo's forks |  | --repo |
| `fork remove` | action |  |  | Remove a fork's worktree or clone | <NAME> | --force --repo |
| `stash` | group, bare runs |  |  | Set a plan's commits aside and restore them later | [PLAN] | --repo --for --to-queue --priority --dry |
| `stash push` | action |  |  | Set a plan's commits aside (kept restorable on a protective ref) and clean them off the branch — the same as bare `clank stash` | [PLAN] | --repo --for --to-queue --priority --dry |
| `stash pop` | action |  |  | Restore a stashed plan: replay its commits onto HEAD and consume the stash. Reviews reset (the replayed commits are re-reviewed) | [PLAN] | --repo |
| `stash show` | action |  |  | Print a stashed plan's body (from its protective ref) + commits | [PLAN] | --repo |
| `stash drop` | action |  |  | Permanently discard a plan's stashed commits + record | [PLAN] | --repo |
| `stash list` | action |  |  | List the stashed plans |  | --repo |
| `shelve` | group, bare runs | hidden |  | Hidden alias for `clank stash push` (+ `shelve clean` → `stash drop`). One release of back-compat | [PLAN] | --repo --for --to-queue --priority --dry |
| `shelve clean` | action |  |  | Permanently discard a plan's shelved commits + state | [PLAN] | --repo |
| `unshelve` | action | hidden |  | Hidden alias for `clank stash pop`. One release of back-compat | [PLAN] | --repo |
| `pick` | action |  |  | Copy plans from another branch | <PLANS> | --from (required) --repo --squash --purge --dry |
| `purge` | action |  |  | Strip a plan's `.clank/` artifacts from history | [PLAN] | --all --repo --into-branch --dry --squash --amend --no-cache --drop |
| `pr-review` | group |  |  | Run the review loop against a GitHub PR |  | --repo |
| `pr-review start` | action |  |  | Start reviewing a GitHub PR: pin its head sha and scaffold `.clank/pr-reviews/<pr>/` | <PR> | --fork --checkout |
| `pr-review note` | action |  |  | Record your reviewer verdict for the current round |  | --verdict (required) -m (required) --pr --author |
| `pr-review propose` | action |  |  | Open/re-open the review for the current draft: bump the round and summon reviewers (master-only) |  | --pr |
| `pr-review abort` | action |  |  | Discard a PR review's local scratch (master-only) |  | --pr |
| `pr-review finish` | action |  |  | Hand the converged review over as a DRAFT (master-only). Nothing is submitted: the pending review stays yours to read, edit and submit on GitHub |  | --pr |
| `pr-review status` | action |  |  | Round + per-reviewer verdicts + who we're waiting on |  | --pr |
| `diff` | action |  |  | Open a plan or commit-range diff in your editor | [TARGET] | --plan --range --prompt --focus --wait --no-wait --print --repo |
| `html` | group, bare runs |  |  | Render the timeline + status to a static site |  | --repo --rebuild --quiet |
| `html open` | action |  |  | Build the site, then launch the host browser (`open` / `xdg-open` / `explorer`) on the generated `index.html` — or on `plan/<stem>.html` if a plan name is supplied | [PLAN] | --commit --queue --stash --print-path |
| `block` | group |  |  | Ask the human a blocking question |  |  |
| `block create` | action |  |  | Create a block | <NAME> | -m (required) --author --repo |
| `block clean` | action |  |  | Remove answered block+unblock pairs for this agent |  | --repo |
| `block migrate` | action |  |  | Flatten legacy plan-scoped blocks to the repo-wide layout |  | --dry --repo |
| `unblock` | action |  |  | Answer a pending block | <AGENT> <NAME> | -m (required) --repo |
| `attending` | action |  |  | Record the background task you are waiting on, so the Stop hook stops waking you with work you are already doing | [TASK_ID] | --pid --desc --expect --clear --author --repo |
| `tui` | action |  |  | The full-screen pane: the repo's state live, the agents, the plan and its gate, and the remote switch |  | --repo --no-cache |
| `run` | action |  |  | Run a command under clank, so it knows exactly when the work ends | <CMD> | --desc (required) --expect --author --repo |
| `config` | group, bare runs |  |  | Read or write clank config values |  | --repo -j/--json --global |
| `config review.adhoc_feedback` | action |  | review_adhoc_feedback | Require review for ad-hoc (non-plan) commits. bool, default: false | [ACTION] [VALUE] |  |
| `config review.plan_feedback` | action |  | review_plan_feedback | Require review for plan-attributed commits. bool, default: true | [ACTION] [VALUE] |  |
| `config review.require_commit_prefix` | action |  | review_require_commit_prefix | Require [plan] or [misc] commit title prefixes. bool, default: false | [ACTION] [VALUE] |  |
| `config hooks.blocked` | action |  | hooks_blocked | Shell command to run when an agent creates a block. string or null | [ACTION] [VALUE] |  |
| `config diff.editor.command` | action |  | diff_editor_command | Editor executable for `clank diff`. string or null | [ACTION] [VALUE] |  |
| `config diff.wait` | action |  | diff_wait | Default `--wait` behavior for `clank diff`. bool, default: false | [ACTION] [VALUE] |  |
| `config finish.autosquash` | action |  | finish_autosquash | Auto-squash a plan into ONE commit at `clank finish`, REWRITING the current branch in place (rewrites published history if already pushed). bool, default: false. `--no-squash` skips it for one finish | [ACTION] [VALUE] |  |
| `config recovery.blind_nudge` | action |  | recovery_blind_nudge | How often to blindly nudge an agent that owes work and whose record clank cannot read: at most one send per stuck interval. duration (30s, 40m, 1h) or `off`, default: off | [ACTION] [VALUE] |  |
| `export` | action |  |  | Dump the repo's roster config as JSON |  | --repo |
| `rereview` | action |  |  | Re-open review on a plan's latest reviewable commit | [PLAN] | --repo |
| `rewire` | action |  |  | Remap feedback after a rebase or amend |  | --repo --from-stdin |
| `stop-hook` | action |  |  | The per-tool Stop-hook adapter |  | --tool (required) --repo --session-start --loop --observe --turn-id --class --status --recovery-helper --recovery-reply --attempt --opened-by --started |
| `question-hook` | action | hidden |  | Native Claude question hook adapter (installed by setup) |  |  |
<!-- inventory:end -->
