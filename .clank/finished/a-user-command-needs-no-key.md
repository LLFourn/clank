# a-user-command-needs-no-key
# A user command needs no key

## Why

`clank user-command` (1f268e0d, then named keys in 4d3bdb69) types
`!<command>` into an agent's idle Claude prompt, for the story site's Run
button. It only accepts a request signed with an Ed25519 key that clank
trusts. LLFourn, 2026-11-26: "there's no need for any protection
whatsoever ... not getting any extra security by adding this public key
at this point. All we're just doing is breaking things."

He is right about the key:
- Every agent and the site run as the same Unix user.
- Any agent can already `zellij action write-chars '!…'` into any pane
  without clank.
- The site's private key sits in `~/src/fs-war-room/site.command-key`,
  inside a repo agents work in.
- The deny rules the README relies on were never installed
  (`~/.claude/settings.json` has no `permissions.deny`).
- The person pressing Run can already tell any agent to run anything.

So the key protects nothing. Its only effect is that the button refuses:
"clank refused it: no trusted key".

## Scope

1. Remove the key machinery entirely:
   - `trust-key`, `untrust-key` and `keys`;
   - the signed envelope (`envelope.rs`, `trust.rs`, the `clank
     user-command v1\n` signature domain);
   - `user_command.keys` and the legacy `user_command.site_key` in config;
   - re-verification at dispatch;
   - the ledger's key and key-name fields;
   - the Ed25519 dependency, if nothing else uses it.

   Delete the code. Don't leave it disabled behind a flag.
2. `clank user-command submit` takes the plain request on stdin:
   `{repo, agent, session, id, command}`.
   - Keep the target checks: the roster label, the current binding's
     session, and a Claude tool.
   - Keep the one-line command with no control characters.
   - Keep an expiry only if something real needs it; otherwise drop the
     issued/expires window.
3. Keep what guards against a repeated or misaimed press. These are not
   protection against an attacker:
   - typed only at that agent's idle, empty prompt, by the monitor on its
     existing wakes;
   - at most once per id, with the same id and payload returning its
     state;
   - `unconfirmed` is never retried;
   - every outcome goes in the nudge history;
   - `status`.

   Simplify these where the key's removal leaves them over-built, for
   example re-verification steps that only existed for the signature.
4. Keep the guard that stops ordinary pane writes from entering shell
   mode: the leading `!` gets `·`, and admission reads the input first.
   It is not part of the key. It keeps `!` reachable only through the
   explicitly named `user-command` path.
5. The README's "Not active yet / Evidence a human records" gate is about
   correctness, not security: whether Claude 2.1.x runs a typed `!<line>`
   as one bash command, and what bash mode looks like in a dump. Record
   that evidence as part of this plan, in a throwaway pane against the
   installed Claude, never typing an outward command, so the feature
   actually works when this lands. If it cannot be recorded, keep the
   gate and say why.
6. README and book: rewrite the User commands section without keys,
   envelopes or deny rules. Drop the "what it does not protect" list. Say
   plainly that the path is a convenience with no security boundary,
   since agents run as the same user. Regenerate the book reference
   pages the `cli_reference` gate checks.

## Acceptance

- No source, README or book text mentions a trusted key, a signature, an
  envelope, `trust-key`, `untrust-key` or `site_key`, except finished/
  history.
- A `submit` of a plain request for a bound, idle Claude agent is
  delivered. A test drives submit through dispatch with the existing
  in-process seams.
- The at-most-once behaviour (same id and payload, same id with a
  different payload) and the idle-prompt-only behaviour each keep a test.
- The `·` guard on ordinary writes keeps its existing tests.
- The tests behind the removed checks go with them. No test asserts the
  key commands are gone.

## Out of scope

- The site's side (clankstack signing, and `site.command-key`). That is
  another repo. When this lands, the master sends its controller the new
  request shape.
