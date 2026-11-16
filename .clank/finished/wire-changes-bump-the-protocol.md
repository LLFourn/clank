# wire-changes-bump-the-protocol

Invariants: I8.

## Why

5410fb62 (tagged v0.2.0) added `Control::Questions` to the monitor wire without bumping protocol 23. Development builds 1ef4886b..9d130109 speak 23 without it, and against one of those the message is silently dropped, so question evidence is lost and recovery can nudge into an open question. v0.2.0 itself carries it. Nothing makes a missed bump fail.

## Build

1. Bump `PROTO` to 24, so no development build that speaks 23 without `Control::Questions` is taken for a peer.
2. **Inventory.** The golden test covers every message the monitor and its clients exchange, and every type nested in them:
   - `Hello` and `Who`, and the handshake reply `Welcome`;
   - `Control`, `Report`, `Note`, and `Frame` with `WireVerdict`;
   - the client's delivery acknowledgement `TurnResult`.

   The implementation lists the nested types it found, in the commit.
3. **Coverage by construction.** Samples are written by hand, one function per wire type, and the compiler checks they are complete:
   - **Variants:** each enum's sample function contains a `match` over a value of that enum with no wildcard arm, in a `#[deny(unreachable_patterns)]` scope, beside the list of samples it returns. A new variant fails to compile until it has a match arm, and the arm's comment-free body is its sample.
   - **Fields:** each struct and struct variant is destructured exhaustively (`let T { a, b, c } = &sample;` with no `..`), so a new field, including a `#[serde(default)]` or `Option` one, fails to compile until the sample sets it.
   - Optional fields are `Some` and collections non-empty in at least one sample, so each field reaches the serialized JSON.
4. **The pin.** The samples' JSON is hashed and compared with `PINS: &[(u32, &str)]`, a history of `(PROTO, hash)` pairs whose last entry must be `(PROTO, hash)`. Entries are strictly increasing in `PROTO`, and no hash appears twice.
   - Drifting the shape fails, with a message saying to bump `PROTO` and append a pair.
   - What the test cannot stop is someone editing the last pair's hash in place without a bump. Reviewers check that `PINS` only ever grows.
   - I8 states exactly that: drift is detected by the test, and the bump is enforced by review of `PINS`.
5. **Registry.** I8 in `docs/invariants.md` becomes `holds`, naming the golden test, with the scope above.

The question hook's ordering change, which was here before, is a separate change with its own evidence contract. It is queued as `the-question-hook-shows-the-question-first`.

## Acceptance

- The golden test passes on the bumped `PROTO`.
- Mutations, each failing to compile or failing the test:
  - add a required field to a nested struct;
  - add a `#[serde(default)] Option` field;
  - add a variant to a nested enum;
  - change a field's type;
  - edit the hash without appending a pair.
- `PROTO` 24 is what the handshake sends. An existing protocol-mismatch test still refuses a 23 peer.
