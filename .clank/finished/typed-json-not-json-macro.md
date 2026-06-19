# typed-json-not-json-macro

Replace ad-hoc `serde_json::json!` map-building with typed structs that
`#[derive(Serialize)]`. clank's convention is proper structs + serde
derives; `json!` scatters the wire shape across format-string-like
literals where it can't be type-checked and silently drifts (the
`head_correction` addition in commit-tag-fixup had to follow the bad
pattern — lloyd).

## Scope

56 `json!` uses across 11 files (`grep -rn "json!" crates/*/src`):
wfw.rs (13), status.rs (10), setup.rs (10), stop_hook.rs (8), log.rs
(6), init.rs (3), team.rs (2), feedback.rs / doctor.rs / config.rs /
auto.rs (1 each).

## Approach

For each command's JSON OUTPUT we fully own (status `to_json`, wfw
`render_json`, log `--json`, stop_hook output, doctor `--json`,
feedback/team/config/auto outputs): define a typed `…Json` struct
(or reuse existing domain structs) deriving `Serialize`, build it from
the snapshot/state, and serialize via `serde_json::to_value` /
`to_string_pretty`. One struct per output shape; nested shapes get
nested structs, not inline `json!`.

**The JSON output is a CONTRACT** (machine consumers: stop-hook, `wfw`,
`status --json`, `log --json`). The typed structs MUST serialize to the
EXACT same keys/shape — `#[serde(rename/skip_serializing_if)]` as needed
to match today's bytes. Pin each converted surface with a test asserting
the serialized JSON equals the pre-change output (snapshot or explicit
key assertions), so the refactor is provably wire-compatible.

## Distinguish: our-shape vs foreign-doc patching

- **We own the whole shape** → typed struct (the bulk; status/wfw/log/
  stop_hook/etc.).
- **Read-modify-write into a USER-owned file** (setup.rs merging rules
  into `.claude/settings.local.json`; init.rs perms) — here partial
  `serde_json::Value` manipulation is legitimate (we're patching a doc
  we don't fully own). Convert the FRAGMENTS we contribute to typed
  structs serialized to `Value`, but keep `Value`-level merge where
  that's the actual operation. The implementer judges per-site; the
  principle is "type what we own."

## Testing (in-process; no binary spawning — [[no-binary-spawning-tests]])

- Per converted surface: a test that the typed serialization matches the
  current JSON (same keys, same values for representative inputs).
- Existing `--json` consumer tests stay green (wire-compatible).
- `cargo fmt`; clippy not above baseline.

## Non-goals

- Changing any JSON wire format / keys — this is a pure
  representation refactor, byte-compatible.
- Converting non-JSON `json!`-free code.
- New output fields.

## Suggested sequencing

Land per-file (or per-output) so each is a small, independently
reviewable, wire-compatible commit — start with the highest-count,
fully-owned surfaces (wfw, status, stop_hook, log), then the
single-use stragglers, then the foreign-doc patchers (setup/init) last
since they're the nuanced ones.
