# there-is-one-door-not-two
# There is one door, not two

## Why

Turning the remote on opens a browser at the **loopback** URL, while the
row beside it shows the **tunnel** URL. The user pressed `r`, watched a
tunnel come up, and got `http://127.0.0.1:56904/login?t=…`:

> "I just turned remote on and it opened localhost url in my browser."
> "yes it should prefer the tunnel url."

## The model

`Remote` has two link functions that differ in exactly one expression:

- `login_link()` — `instance.url` (loopback), used by `open_again`, which
  fires on the transition to `On` and on `o`.
- `phone_link()` — `instance.public_url` falling back to `instance.url`,
  used by `p`, which draws a QR.

Once the preference changes, they are not two functions that happen to
agree. They are **one function**, and the only real difference is what
the caller does with the string: hand it to a browser, or draw it as a
QR. Keeping two would leave a second place for the preference to drift
out of step with the row's own `detail()`, which already prefers the
public URL — and that drift is the whole of this bug.

## Deliverables

1. **One `link()`** on `Remote`, preferring the tunnel's proven
   `public_url` and falling back to the local one. `login_link` and
   `phone_link` both go; `open_again` and the phone/QR path call `link()`.
2. **`o` and the auto-open use it**, so the URL opened is the URL the row
   shows.
3. The fallback stays honest: with no tunnel (not a state the remote
   reaches today, since it refuses to run unconfigured) the local URL is
   still what there is.
4. **The `BrowserFailed` notice names the same URL it failed to open**
   — it currently prints `instance.url` regardless, which under this
   change would tell the user to visit an address the feature no longer
   prefers.

## The tradeoff, recorded deliberately

The minted link is a one-use, five-minute capability. Opening it at the
loopback URL kept it on this machine; opening it at the tunnel URL sends
it to Cloudflare's edge, which terminates TLS. That is already true of
every `p` link and of the session cookie that follows, so this does not
introduce an exposure the feature did not have — it applies it to one
more link. It is worth stating because it is a real widening, chosen
rather than stumbled into.

## Tests

- `link()` prefers `public_url` when the instance has one.
- `link()` falls back to the local URL when it does not.
- The transition to `On` opens the PUBLIC url — the recorder's opened
  list is the assertion, and it is what this bug would have failed.
- `o` opens the same URL the row's `detail()` shows, asserted against
  each other rather than against a literal, so they cannot drift apart.
- A browser failure names the URL it actually tried.
- Mutation-check each: make `link()` prefer local, and let the notice
  name the local URL.

## Out of scope

- Whether the desktop should get a faster loopback route. Deliberately
  not kept: one visible URL is worth more than one saved hop, and a
  second preference is what produced this bug.
