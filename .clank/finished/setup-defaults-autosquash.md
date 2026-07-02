# setup-defaults-autosquash

Make `finish.autosquash` the DEFAULT by seeding it on first `clank setup`, and
make autosquash actually work on the natural clank workflow branch (often
`master`/`main`, which is protected) so the default isn't a footgun.

## Part 1 — `clank setup` seeds the default (first-run only)

`clank setup` installs user-scope assets to `~/.claude` / `~/.codex`. Add: it
also seeds `finish.autosquash = true` in the USER-scope config
`~/.clank/config.json` — but ONLY when the key is ABSENT (first run). A user
who later sets it `false` (or `true`) is never clobbered on re-setup.

- Implementation: after the skill/hook install in `setup::run` (which already
  has `home_dir()`), read `~/.clank/config.json`; if `finish.autosquash` is
  not present, `config::set_key(&home.join(".clank/config.json"),
  &["finish","autosquash"], "true", "bool")` and print a one-line notice.
  Reuse a `config::set_key_if_absent` helper (read → check presence → set) so
  "don't clobber" is the primitive, testable in isolation.
- `--dry-run` (setup already has one) must not write.
- Presence check keys on the raw JSON (`finish.autosquash` exists), NOT the
  effective value — so an explicit `false` counts as "set" and is preserved.

## Part 2 — autosquash must work on a protected branch (THE decision)

The clank workflow finalizes plans ON the working branch, which for clank
itself (and many repos) is `master`. `finish` calls the rewrite engine with
`allow_rewrite_protected: false`, so an autosquash finish on `master`/`main`
is REFUSED: the finalize commit lands (with its validated message, per the
earlier fix) but the squash is skipped and `finish` exits non-zero. So a
default-on autosquash breaks `clank finish` on the very workflow it targets.

RESOLVED — **option A**, chosen (codex deferred to me; ruthless recommended
**D** (push-aware bypass) and did NOT block on A-vs-D, but CONDITIONED A on
user-facing disclosure — now satisfied). lloyd's intent is autosquash working
on this on-`master` repo, which B would make a silent no-op.

- **(A, chosen) autosquash implies `allow_rewrite_protected`** for its
  in-place current-branch squash. finish.rs, in the autosquash branch, sets
  `args.allow_rewrite_protected = true` alongside `args.squash = message`.
  Rationale: autosquash is an explicit standing opt-in to collapsing the plan
  on the current branch, and clank is local-first (the plan's commits are
  fresh). CAVEAT: if the branch is already PUSHED/shared, this rewrites
  published history — `--no-squash` opts out for that finish. **Disclosure
  (required by ruthless, now added):** the setup seed notice AND the
  `finish.autosquash` config help both state the in-place rewrite +
  published-history risk + the opt-outs (not only a code comment).
- (D, not taken — ruthless's rec, better long-term) make the bypass
  push-aware: only bypass the protected guard when the branch's commits aren't
  pushed (one extra git read). Safe on shared branches; a candidate follow-up.
- (B, rejected) silently skip on protected branches — safe but inert on
  `master`; the default would do nothing on clank's own workflow.
- (C, rejected) leave as-is — `finish` errors on `master`; unacceptable
  default.

Explicit `clank finish --squash` is UNCHANGED (still needs
`--allow-rewrite-protected`); this only affects the autosquash path.

## "See it here" note

This repo is on `master` and has `finish.autosquash = true` (set this session).
Until Part 2 lands, finalizing a plan here will finalize-then-refuse-the-squash
(finish exits non-zero, finalize commit present with the real message). That IS
the observation motivating Part 2.

## Tests

- setup: seeds `finish.autosquash=true` when the user config lacks it; leaves
  an explicit `false`; idempotent on re-run; `--dry-run` writes nothing.
- `set_key_if_absent`: sets when absent, no-ops when present (true OR false).
- (if A) autosquash finish on a protected `main` squashes to one commit with
  NO refusal (mirror the existing autosquash integration test but drop
  `allow_rewrite_protected` — autosquash supplies it).

## Acceptance criteria

- Fresh `clank setup` leaves `~/.clank/config.json` with
  `finish.autosquash=true`; re-setup / an explicit value is preserved.
- Autosquash finish works on `master`/`main` per the chosen Part-2 option
  (no spurious finish error).
- clippy at baseline; tests pass.

## Deploy

`cargo install --path crates/cli --force`, then `clank setup` (seeds the
default into `~/.clank/config.json`).
