# the-opencode-plugin-loads-in-opencode-2
# The OpenCode plugin loads in OpenCode 2

## Problem

OpenCode 2.x refuses to load clank's plugin, so no OpenCode agent is ever woken. The OpenCode log (`~/.local/share/opencode/log/opencode.log`) shows, from the first 2.x start (2026-10-26 09:59 UTC, version 2.1.3, now 2.1.4):

> failed to load plugin target=~/.config/opencode/plugin/clank.js … PluginModule.LoadError: Plugin must export a default definition with an id and an effect or setup function. (cause: SchemaError(Missing key at ["default"]))

clank's plugin (`crates/cli/src/cli/setup_assets/opencode_plugin.js`, installed by `clank setup`) exports `export const ClankPlugin = async ({ client, $, directory }) => {…}`, the 1.x shape. `~/.config/opencode/package.json` still pins `@opencode-ai/plugin` 1.18.11.

Observed 2026-10-27 in `~/tmp/backup-bip-experiment`: agent `qwen` (OpenCode) has a message from `claude` in its inbox and was never woken. No `clank stop-hook --tool opencode` process exists, no `--observe` record was ever written (state `unknown: its record could not be read`), and a manual `clank nudge` is refused because OpenCode is never typed into. Binding still worked only because the TUI prompt told the model to run `clank as qwen`; the plugin's `shell.env` injection of `OPENCODE_SESSION_ID` is also dead.

## What changes

1. Research first, with sources and the date recorded in the plan's notes: OpenCode 2's plugin contract from the installed 2.1.4 (its bundled `@opencode-ai/plugin` types/schema, release notes, docs). Establish the default-export shape (`id`, `setup`/`effect`), how a plugin receives the client and shell, and whether the hooks clank uses still exist with the same meaning: `event` (`session.idle`, `session.status`, `session.error`, turn start/end), `shell.env`, `client.session.promptAsync`, `client.tui.showToast` (or its replacement). Note anything renamed or gone, and the 2.x server/TUI split (`opencode serve --service` hosts plugins; a TUI attaches).
2. Port `opencode_plugin.js` to the 2.x contract, keeping every job and loop-discipline rule its header documents (binding env, one in-flight wait per session, idle/interrupt/failure distinctions, ordered `--observe` reports, discard-unless-still-idle, diagnostics as toasts, renewal). Since plugins now run in a shared service across sessions and directories, confirm per-session and per-directory scoping is still right.
3. Decide 1.x support from the research: if one file can serve both contracts cheaply (default export plus the named export), do that; otherwise target 2.x and have `clank setup` / `clank doctor` say plainly when the installed OpenCode is older than the plugin supports.
4. `clank setup` updates the plugin dependency pin in `~/.config/opencode/package.json` if 2.x needs a different `@opencode-ai/plugin` version, without touching unrelated keys.
5. `clank doctor` (or status) reports when OpenCode has failed to load the plugin, so this is visible instead of silent. Find the cheapest reliable signal from the research (e.g. no `--observe` record after a bound session's first turn); don't scrape OpenCode's log if a better signal exists.

## Out of scope

- Changing how clank's monitor or waits work.
- Typed input into OpenCode panes (still never).

## Acceptance

- The existing plugin lifecycle test (`crates/cli/tests/opencode_plugin_lifecycle.mjs`) runs against the 2.x contract: a stub of the 2.x plugin loader validates the default export's shape and drives the same lifecycle (idle arms one wait; a returned prompt is injected via promptAsync only while still idle; interrupt and failure arm nothing; observe reports are ordered; renewal re-arms).
- A test that loads the plugin module and asserts the 2.x schema keys exist, so a future export-shape regression fails in CI rather than in the field.
- `clank setup` writes the new plugin and, where needed, the dependency pin; existing user keys in `package.json` survive.
- Doctor/status shows the load-failure signal for an OpenCode agent that is bound but has never reported.
- Manual check (not gating): after `clank setup` and restarting OpenCode, the `qwen` agent in backup-bip-experiment is woken for its pending message.

## Notes

Research, 2026-10-27, from the installed OpenCode 2.1.4 (strings of `/opt/homebrew/Cellar/opencode/2.1.4/bin/opencode`; nothing was run) and https://opencode.ai/v2/docs/build/plugins/migrate-v1:

- The loader (`PluginModule.load`) decodes the module against `{ default: { id, effect } | { id, setup } }`; `setup(ctx)` may return a cleanup, run on unload. Local plugins are still discovered as `.js`/`.ts` files under `plugin/` and `plugins/`. `Plugin.define` is the identity, so the plugin imports nothing and the `@opencode-ai/plugin` pin is irrelevant to it (left alone).
- One plugin host per location; `ctx.location.directory` is the host's, not a session's. `ctx.event.subscribe({ signal })` yields `{ id, type, data, location? }`.
- Turn lifecycle events: `session.inbox.enqueued { inboxID, item: { type } }`, `session.inbox.delivered { inboxID }`, `session.execution.started | succeeded | failed { error: { type, message, status? } } | interrupted { reason: user | shutdown | superseded | inactivity }`, `session.retry.scheduled`. V1's `message.updated` / `session.error` are not in the stream.
- `ctx.shell.hook("create.before", e)` gets `{ command, cwd, timeout, shell, env }`, no session; the bash tool sets `OPENCODE_SESSION_ID` itself after the hook.
- `ctx.session.prompt({ sessionID, text, id?, delivery: "steer" | "queue" })` admits durably and returns; message ids must start `msg_`. `ctx.session.wait({ sessionID })` resolves when the session's loop is idle. No toast and no `$` in a server plugin.
- OpenCode records each session's last settle in `opencode.db` (`session_v2.time_idle`, `idle_outcome`): the doctor's plugin-independent evidence that a turn ended.
