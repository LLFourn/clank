# ngrok-asks-too-much
# ngrok asks too much

## Why

> "since ngrok adds friction can we queue a plan to remove any ngrok code
> and dependencies."

Three separate costs, and the first two are asked of the user before
anything works at all:

1. **An account.** ngrok's own docs are explicit that the agent
   authenticates with an authtoken from the dashboard, and the free plan
   requires signing up. There is no anonymous mode.
2. **A reserved domain, which is OUR demand, not ngrok's.**
   `TunnelSection::Ngrok { domain: String }` takes a domain as a required
   field and claims the endpoint before starting. ngrok would hand out an
   ephemeral hostname for an authtoken alone; clank refuses to ask for
   one.
3. **An interstitial on the free plan.** ngrok's pricing page lists
   "Interstitial page on HTTP/S endpoints" as a free-plan
   characteristic, removed on paid plans. What it intercepts is
   UNMEASURED — three ngrok doc pages say nothing about it, and this
   machine has no authtoken to find out with.

Against which: `{"provider": "quick"}` now works natively, with no
account, no domain and no binary. The reason ngrok existed — a tunnel
whose hostname you own — is real but is served by `command` for anyone
who wants it, including with ngrok's own agent.

## The model

**A provider earns its place by being reachable.** Two of clank's three
made a promise the user had to buy their way into; one of them could
never work at all and has just been repaired. ngrok is the remaining one,
and it is the only code path in the tree that cannot be exercised on this
machine at all — not by a test, not by a live run, not by a capture.
That is not a provider, it is an assertion.

`command` is the escape hatch it leaves behind, and a better one: it runs
ngrok's agent, or ssh, or anything else, and clank does not have to
model any of them.

## Deliverables

1. **Remove the `ngrok` crate**, version 0.19. It is a large dependency
   with a family of transitive crates (`muxado`, `awaitdrop`, and
   others) that nothing else in the tree uses.
2. **Remove `Ngrok`, `NgrokHandle`, `ngrok_authtoken` and `authtoken_in`**
   from `tunnel.rs` — roughly 38 lines of it mention ngrok — and the
   remote page's mentions.
3. **Keep `TunnelSection::Ngrok` in the SCHEMA**, refused at use with a
   reason that repairs the config, exactly as `quick` was handled and for
   exactly the same reason — which is measured, not assumed. A config
   naming a provider the enum does not know:

   ```
   $ clank team list
   parsing ~/.clank/config.json: unknown variant `nonexistent-provider`,
   expected one of `quick`, `ngrok`, `command`
   ```

   `read_user_config` is shared with the roster and team commands, so a
   variant removed from the enum turns a retired tunnel into an
   unreadable config and a dead CLI.
4. **The refusal names the replacement**: a `command` provider running
   `ngrok http {port}`, plus `url_contains`, so someone who wants ngrok
   keeps it without clank modelling it.
5. **README**: the ngrok bullet goes; the `command` recipe covers it.
6. **`Isolated` stays.** The native quick tunnel uses it.

## Tests

- A config naming `ngrok` is refused with a message containing
  `command` and `ngrok http`, and the rest of the config still parses —
  the roster still reads.
- No `ngrok` symbol remains in the crate, asserted by the same kind of
  source scan the git boundary uses, so it cannot creep back.
- `Cargo.toml` no longer names ngrok, and the lock loses its family.
- The quick and command providers are untouched: their live tests still
  pass.
- Mutation-check each.

## Out of scope

- Whether ngrok's SDK has the websocket defect the Cloudflare crate had.
  It becomes moot.
- The interstitial's real behaviour. Also moot, and unmeasurable here.
