# an-agents-launch-is-its-command-line
# An agent's launch is written as its command line

## Why

Lloyd, 2026-10-20, on an agent building a browser reviewer:

```
clank agent add browser2 --tool codex --review standby --launch-arg=--no-daemon
  --launch-arg=--disable --launch-arg=shell_snapshot --launch-arg=-c
  --launch-arg='mcp_servers.chrome.command="npx"' --launch-arg=-c
  --launch-arg='mcp_servers.chrome.args=[...'
```

"It looks crazy. How are we meant to do this? surely not a million
--launch-arg." And then: "can we make the launch arg cli api to not suck
so badly?"

Today `clank agent add` takes a tool's arguments one token at a time
(`--launch-arg`, repeatable), with `--launch-cmd` and `--launch-env`
beside it. There are three problems:

- **Every token needs its own flag.** `-c key=value` is two flags. A
  JSON or TOML value has to survive a second layer of quoting, so it is
  easy to truncate: the command above lost its closing `]`.
- **Reading it back means un-flagging it.** The command does not look
  like the command that will run.
- **There is no way to change an existing agent's launch.** The only
  route is `agent remove` and `agent add` again, which loses the
  roster entry's place and invites mistakes, or hand-editing
  `.clank/config.json`.

## The contract

- **`clank agent add <label> --tool <tool> [...] -- <args...>`.**
  Everything after `--` is the tool's arguments, verbatim and in order,
  stored as `launch.args`. They are quoted once, by the shell, exactly
  as they would be typed to the tool itself:

  ```sh
  clank agent add browser2 --tool codex --review standby -- \
    --no-daemon -c mcp_servers.chrome.enabled=true
  ```

  `--env KEY=VAL` (repeatable) and `--cmd <executable>` replace
  `--launch-env` and `--launch-cmd`, and sit before the `--`.
- **`clank agent launch <label>`** shows the agent's launch profile, and
  the command line `clank agent start` would run, quoted so it can be
  pasted.
  - `clank agent launch <label> [--env K=V]... [--cmd X] [-- <args...>]`
    sets it in place, in one config write. What is written is the agent's
    whole new launch: nothing is kept from before, so no flag is needed to
    say what a missing part means. Lloyd, 2026-10-20: "clis are not meant
    to be so convoluted you need some magic flags to determine how things
    are meant to be interpreted." The roster entry, its
    role, its session binding and its pane are untouched, as with
    `agent set-review`. The new launch applies from the agent's next
    start.
  - `--clear` empties it, so the bare tool runs again.
  - It works on a running agent: the preview reports a pane that runs
    it, and never refuses as a real start does.
- **The old flags keep working, hidden, for one release.** They are
  `--launch-arg`, `--launch-cmd` and `--launch-env`, so commands
  already in scripts and agents' habits do not break. Mixing them with
  `--` in one command is refused, naming both.
- **The guidance points at configuration first.** `agent add --help`,
  the README and the master skill say:
  - structured tool configuration (MCP servers, feature switches)
    belongs in the tool's own config file or a per-agent home
    (`--env CODEX_HOME=...`), not on the command line;
  - the launch line is for the few flags that select it.

## Scope

- `cli/mod.rs`: the add arguments (a trailing `--` argv, `--env`,
  `--cmd`, and the hidden old flags); the new `agent launch`
  subcommand.
- `cli/agent.rs`: building the launch from the new arguments; `agent
  launch` show, replace and clear through the existing typed config
  writer; the quoted command line, shared with `agent start --print`.
- README, `agent add --help`, the master skill text: the new form, and
  the configuration-first guidance.
- Sweep the whole repo for `--launch-arg`, `--launch-cmd` and
  `--launch-env` (README, skills, error strings, tests): convert every
  reference except the hidden aliases and their one test.

## Acceptance

Through the in-process cores, with no binary spawning:

- `agent add ... -- --no-daemon -c 'k=["a","b c"]'` stores exactly
  `["--no-daemon", "-c", "k=[\"a\",\"b c\"]"]`. A value starting with
  `-`, and one containing spaces and quotes, survive as one argument.
- `agent launch <label>` prints a command line that, run by a shell,
  gives that same argv: round-trip it through a POSIX-quote parser in
  the test.
- `agent launch <label> -- <new>` replaces the args.
  - The role, review tier, session binding and other agents are
    byte-identical in the config before and after.
  - `agent start <label> --print` shows the new args.
- `agent launch <label> --env K=V` with no `--` keeps the args.
- `--clear` empties the profile.
- The old flags still build the same launch.
- Mixing an old flag with `--` is refused before any write.
- Mutation-check each: the `--` argv being dropped, replacing instead
  of keeping the args when no `--` is given, and the old flags' alias
  mapping.

## As built

- **`cli/mod.rs`:**
  - `AgentAddArgs` gains `--cmd`, `--env` (repeatable) and `tool_args`, a `last = true` trailing argv after `--`.
  - `--launch-cmd`, `--launch-arg` and `--launch-env` stay, `hide = true`. Each conflicts with `--` and with its new counterpart, so clap refuses a mix and names both.
  - New `AgentCmd::Launch(AgentLaunchArgs)`: the name, `--cmd`, `--env`, `--clear` (which conflicts with the other three), `--repo`, and the trailing argv.
- **`cli/agent.rs`:**
  - `launch_from_add` builds the profile from either spelling.
  - `LaunchChange` (`Clear`, or `Update` where each `None` field is kept) and `launch_change` derive it from the arguments. No `--cmd`, no `--env` and no `--` means show only.
  - `set_repo_launch` makes the one in-place config write through `write_repo_transition(PreserveGate)`, as `set_repo_review` does.
  - `launch` shows `describe_launch` (cmd, args and env, shell-quoted), then runs `start` with `print: true`. That is the existing side-effect-free preview, so `agent launch` and `agent start --print` print the same composed line.
  - `composed_line` is factored out of `print_composed` so a test can parse it back.
  - The env errors now say `--env`.
- **Whole-profile replacement, not a patch.** The first build kept any part not given. A part-wise patch needs a way to say "empty the args, keep the rest", and clap gives a bare trailing `--` the same empty value as no `--`, which would have meant a special flag. Lloyd rejected that kind of flag. So `agent launch` now sets the whole profile from what is written (`LaunchRequest::Set`), and `set_repo_launch` takes the new `Option<LaunchConfig>`. Emptying only the args is `agent launch <name> --env …` with no `--`.
- **The preview is separate from start's eligibility.** `start_with(args, Purpose, find_holder)` holds the one composer. `Purpose::Start` keeps the duplicate-pane refusal; `Purpose::Preview` (which `agent launch` uses, and which implies `print`) reports a running pane on stderr and composes the next start's command. `find_holder` is `session_holder::find` in production and a test's answer in tests.
- **Docs:** the README gains "An agent's launch", including the configuration-first guidance, and the opencode model example now uses `-- --model …`. The master skill describes the form. The sweep converted the error strings and the one test comment. The remaining old spellings are the hidden aliases and their test, plus plan narrative.
- **Tests (`cli::agent::tests`), through the real `Cli` parser:**
  - the trailing argv stores verbatim: a JSON value with quotes and spaces, an empty string, a token like a clank flag (`--review`), and embedded quotes;
  - `add` itself puts them on the roster;
  - the old flags build the same launch as the new form;
  - each mix is refused;
  - `agent launch` with no change only shows, and contradictory input is refused;
  - a set is exactly what was written: the old env is gone, and the role, prompt and other agents are byte-identical; the composed launch carries the new arguments;
  - `--cmd`/`--env` with no `--` means no arguments, and `--clear` is the bare tool;
  - a running agent (the holder lookup answers "a pane runs it"): `agent launch` shows and sets successfully and the binding is untouched, while a real start still refuses;
  - the shown and printed lines parse back, through `shell_words`, to the same argv.
- **Mutations, each caught:**
  - dropping the trailing argv;
  - unmapping the old `--launch-arg`;
  - treating a `--` change as show-only;
  - the preview refusing a running pane like a start, and `agent launch` using the start purpose.
