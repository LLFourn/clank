# user-command-keys
# clank trusts several sites' keys, each by name

## Rules

- Commit in your repo. Do not push or post anywhere.
- No `claude.ai/code` link and no `Claude-Session:` trailer in any commit or message.
- Comments explain WHY, never WHAT. Delete any comment the code already says; keep only rationale,
  constraints, hazards and rejected alternatives.
- Report to `clank send --repo ~/src/clank-wr --to claude -F <file>`.

## Why

`user-command` (1f268e0d) trusts one site key (`user_command.site_key`). Two story sites run on this
machine (one per controller), and each now has its own Ed25519 key for its Run presses. Sharing one
private key between them would mean neither key can be replaced alone, and the ledger couldn't say
which site signed a command. So clank should trust a small set of keys, each under a name.

## Build

- `clank user-command trust-key --name <site>` reads a public key on stdin and stores it under that
  name, printing its fingerprint. Replacing a name's key needs `--replace`. `clank user-command
  untrust-key --name <site>` and `clank user-command keys` (names and fingerprints) complete it.
- `submit` accepts an envelope that verifies under any trusted key. The ledger record and `status`
  name the key that verified it. At dispatch, re-verify under that same named key: if it was removed
  or replaced since, refuse with `key changed`, as today.
- Migrate an existing single `site_key` into the set under the name `default`, so nothing already
  trusted stops working.
- README: the keys, and that each is a separate capability to guard (the deny rules list each
  site's key file).

## Tests

- Two keys trusted: each one's envelope verifies, and a third key's doesn't.
- Untrusting or replacing a key refuses its admitted, undelivered commands at dispatch.
- The old single key migrates as `default`.
- The repo's checks pass.

## Done when

- It's merged on `master`. Leave installing to LLFourn, as with `user-command`, and say whether
  this can be installed together with it.
