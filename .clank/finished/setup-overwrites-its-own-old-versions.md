# setup-overwrites-its-own-old-versions

## Why

`clank setup` refuses to overwrite a file that differs from the
current expected content without `--force`:

    ~/.config/opencode/plugin/clank.js exists with different content; pass --force to overwrite.

The file in question was setup's OWN previous version — written by an
earlier `clank setup`, never touched by the user. Setup cannot tell
"old setup" from "user drift", because it matches content only against
the CURRENT expected text (plus the claude-async mode alternate). Every
upgrade of a shipped asset therefore reads as foreign drift, and the
normal upgrade path — `clank setup` after installing a new binary —
demands a flag that exists for genuinely modified files.

The user-facing rule should be: **if doctor would show it as an old
setup version, plain `clank setup` overwrites it.** `--force` stays,
but only for content setup has no record of.

## Approach

A provenance manifest so "old setup" is a defined state, not a guess:
`~/.clank/setup-manifest.json`, mapping each setup-owned relative path
to the blake3 of the content setup last wrote there.

`install_skill`'s ladder becomes:

- missing → write, record hash
- `== expected` → ok
- `== canonical_alternates` → mode migration (unchanged), record hash
- file's blake3 `==` manifest's recorded hash for that path → it is a
  known setup-written older version: **overwrite without `--force`**,
  record the new hash (summary line `upgrade`)
- otherwise → refuse without `--force`, unchanged: this is the drift
  guard, and it keeps its teeth exactly where it belongs
- `--force` → overwrite, record hash (so the NEXT upgrade is silent)

The manifest is written on every successful content write, atomically
(temp + rename). An unreadable or corrupt manifest reads as EMPTY:
unknown files then follow the refuse path, which is the safe direction
— never a silent overwrite on a guess.

Doctor consumes the same manifest to SAY which kind of drift a file
is: "old setup (upgrade: run `clank setup`)" versus "modified content
(setup refuses without `--force`)". The user's sentence is then true
by construction: what doctor calls old, setup overwrites.

**Pre-manifest state gets one honest bridge, not a blessing.** Files
written before the manifest exists have no recorded hash. Setup must
NOT accept-and-record arbitrary current content as provenance (that
would bless real user drift into the manifest forever). The first
`--force` after this lands records the hash; every later upgrade is
silent. One forced upgrade, ever.

## Required tests

- A file whose content matches a manifest-recorded older hash is
  overwritten WITHOUT `--force`, and the manifest is updated to the
  new hash.
- A file matching neither expected, nor alternate, nor the manifest
  still refuses without `--force` — the drift guard is unchanged.
- A `--force` write records the hash, so the following plain setup
  upgrades the file silently.
- Mode-alternate migration still overwrites without `--force`.
- A corrupt manifest is treated as empty and nothing is overwritten
  on its authority.
- Doctor's report distinguishes "old setup" (manifest match) from
  "modified content" (no match).
- No test writes outside a temp HOME.

## Out of scope

- Merged JSON hook entries (`settings.json`, `hooks.json`) — those
  merge into user files rather than overwriting content; the manifest
  covers full-content assets (skills, plugin) only.
- Pruning manifest entries for assets the binary no longer ships.
