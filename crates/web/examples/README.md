# Custom web host

[`custom_host.rs`](custom_host.rs) is a runnable host built only from
`clank-web` and common HTTP libraries. It has its own listener, login
form, browser cookie, logout/revocation, and connector credential. It
does not load a Clank repo or use Clank's Door or Cloudflare tunnel.

On the host computer, choose separate secrets and run:

```sh
export CUSTOM_HOST_SESSION=team
export CUSTOM_HOST_BROWSER_TOKEN='a-long-browser-login-secret'
export CUSTOM_HOST_CONNECTOR_TOKEN='a-different-long-connector-secret'
cargo run -p clank-web --example custom_host
```

The example listens on `127.0.0.1:8181` by default. Set
`CUSTOM_HOST_BIND=127.0.0.1:PORT` to choose another port. It requires
loopback binding; use your own TLS reverse proxy or encrypted tunnel
to make the browser URL and connector WebSocket reachable from other
computers. The proxy must forward the original `Host` header and
WebSocket upgrades. The example's browser URL is `/app/`, and its
connector endpoint is `/connector` on the same listener.
HTTPS browser logins receive a `Secure` cookie.

In the repo computer's gitignored `.clank/config.json`, set:

```json
"web": {
  "attach": {
    "endpoint": "wss://host.example/connector",
    "session": "team",
    "credential": "a-different-long-connector-secret",
    "browser_url": "https://host.example/app/"
  }
}
```

Turn on `remote` in `clank tui`. The TUI connects outward and
publishes that repo's session; the host never receives a repo path.
Open the browser URL and sign in with `CUSTOM_HOST_BROWSER_TOKEN`.
Logging out revokes that browser's stream and cookie while leaving
the repo connector running. Turning `remote` off detaches this repo
without stopping the host.

For a same-machine smoke, use `ws://127.0.0.1:8181/connector` and
`http://127.0.0.1:8181/app/` in `web.attach`. The TUI accepts plain
WebSocket and HTTP only on loopback.
