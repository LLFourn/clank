//! Cloudflare origin connection handling.

/// Close the connection after an ordinary response, so the next
/// request arrives on a fresh one.
///
/// [`repaired`] reads a connection's FIRST head. A connector that
/// pools would send the handshake as the second request on a socket
/// it reused from the preceding `GET`, where nothing would repair it —
/// and the handshake looks poolable to it precisely BECAUSE the two
/// upgrade fields are missing.
///
/// Done per response rather than with `keep_alive(false)` on the
/// builder: that setting makes hyper write `Connection: close` over
/// the 101's own `Connection: upgrade`, so every well-formed client
/// fails the handshake. The native connector only reads the status
/// line, so it would not have noticed, which is exactly how a browser
/// would have been broken by a passing tunnel test.
///
/// The cost is one loopback connect per request on the
/// connector-to-origin hop; the browser's keep-alive is to the edge,
/// not to us.
pub fn alone_on_its_connection<B>(resp: &mut hyper::Response<B>) {
    if resp.status() == hyper::StatusCode::SWITCHING_PROTOCOLS {
        return;
    }
    resp.headers_mut().insert(
        hyper::header::CONNECTION,
        hyper::header::HeaderValue::from_static("close"),
    );
}

/// A stream that serves `head` first and then the rest of `inner`.
pub struct Prefixed<S> {
    head: std::io::Cursor<Vec<u8>>,
    inner: S,
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Prefixed<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let at = self.head.position() as usize;
        let left = self.head.get_ref().len() - at;
        if left > 0 {
            let take = left.min(buf.remaining());
            let bytes = self.head.get_ref()[at..at + take].to_vec();
            buf.put_slice(&bytes);
            self.head.set_position((at + take) as u64);
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Prefixed<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// The largest head we will buffer looking for a handshake to repair.
const HEAD_BOUND: usize = 16 * 1024;

/// One past the blank line that ends a request head, if it is here.
pub fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Read this connection's first request head, repair it if it is a
/// handshake the connector could not ask for, and hand back a stream
/// that replays every byte read — repaired head first, then whatever
/// arrived behind it, untouched.
pub async fn repaired(
    mut stream: tokio::net::TcpStream,
) -> std::io::Result<Prefixed<tokio::net::TcpStream>> {
    use tokio::io::AsyncReadExt as _;
    let mut seen = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let boundary = loop {
        if let Some(at) = find_head_end(&seen) {
            break Some(at);
        }
        if seen.len() >= HEAD_BOUND {
            break None;
        }
        match stream.read(&mut chunk).await? {
            0 => break None,
            n => seen.extend_from_slice(&chunk[..n]),
        }
    };
    // A head whose end never arrived is not ours to touch.
    let replay = match boundary.and_then(|at| restored_head(&seen[..at]).map(|h| (at, h))) {
        Some((at, mut head)) => {
            // Whatever was read past the head goes back verbatim.
            head.extend_from_slice(&seen[at..]);
            head
        }
        None => seen,
    };
    Ok(Prefixed {
        head: std::io::Cursor::new(replay),
        inner: stream,
    })
}

/// Put back the two headers a QUIC connector could not carry, in the
/// BYTES, before hyper parses them.
///
/// Cloudflare's edge terminates the websocket handshake itself and
/// reaches the connector over QUIC, where `Connection` and `Upgrade`
/// are hop-by-hop fields and so forbidden; it signals the upgrade out
/// of band instead. A connector is meant to synthesise them again on
/// the way down to HTTP/1.1, and not every one does. What arrives then
/// is a handshake in every respect but those two lines — the key and
/// the version are end-to-end fields and come through untouched — so
/// that is what this keys on.
///
/// On the wire rather than in the router because hyper decides whether
/// a connection is upgradeable while PARSING the head: a header added
/// afterwards answers 101 and then never hands over the socket, which
/// the peer sees as a reset.
///
/// `None` when there is nothing to do, so the common path copies
/// nothing.
pub fn restored_head(head: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next()?;
    let fields: Vec<(&str, &str)> = lines
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .map(|(name, value)| (name.trim(), value))
        .collect();
    let has = |want: &str| {
        fields
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(want))
    };
    // Already asked for properly, by a connector that rebuilt it.
    if has("upgrade") || !has("sec-websocket-key") || !has("sec-websocket-version") {
        return None;
    }
    let mut out = String::with_capacity(head.len() + 48);
    out.push_str(request_line);
    out.push_str("\r\n");
    for (name, value) in &fields {
        // Drop the `Connection` the connector invented: it wrote
        // `keep-alive` over the field it should have rebuilt.
        if name.eq_ignore_ascii_case("connection") {
            continue;
        }
        out.push_str(name);
        out.push(':');
        out.push_str(value);
        out.push_str("\r\n");
    }
    out.push_str("Connection: Upgrade\r\nUpgrade: websocket\r\n\r\n");
    Some(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::restored_head;

    #[test]
    fn restores_the_upgrade_missing_after_cloudflare_forwarding() {
        let broken = b"GET /events HTTP/1.1\r\nHost: example.test\r\nSec-Websocket-Key: abc\r\nSec-Websocket-Version: 13\r\nConnection: keep-alive\r\n\r\n";
        let fixed = String::from_utf8(restored_head(broken).unwrap()).unwrap();
        assert!(fixed.contains("Connection: Upgrade\r\nUpgrade: websocket\r\n"));
        assert_eq!(fixed.matches("Connection:").count(), 1);
        assert!(restored_head(b"GET / HTTP/1.1\r\nHost: example.test\r\n\r\n").is_none());
    }
}
