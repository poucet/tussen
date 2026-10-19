//! The console's HTTP + WebSocket server on 127.0.0.1.
//!
//! Two routes, one thread per connection: `GET /` serves the embedded page
//! (`hub/console/index.html`, compiled in with `include_str!`), and
//! `GET /ws` upgrades to a WebSocket. The HTTP side is a few lines of
//! `httparse` rather than a server crate — loopback, two GETs, no bodies.
//!
//! Who may open the socket, by `Origin` (docs/guides/signals-websocket.md):
//! the console's own page and non-browser clients (no `Origin`, or
//! `file://`'s "null") get full control; **any other loopback origin — a
//! local shader sketchpad, a dev server — gets the feed read-only**
//! (`watch` and every event, no commands); anything non-loopback is
//! refused, so a remote website's JS can never reach the hub. `?readonly=1`
//! opts any connection down to read-only.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Sender, TryRecvError};
use tungstenite::protocol::Role;
use tungstenite::{Message, WebSocket};

use super::{ClientMsg, CLIENT_CAPACITY};

/// The page, embedded in the binary.
pub const PAGE: &str = include_str!("../../console/index.html");

const MAX_HEAD: usize = 16 * 1024;

pub fn bind(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port))
}

pub fn accept_loop(listener: TcpListener, inbox: Sender<ClientMsg>, stop: Arc<AtomicBool>) {
    let next_id = Arc::new(AtomicU64::new(1));
    for stream in listener.incoming() {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let Ok(stream) = stream else { continue };
        let inbox = inbox.clone();
        let next_id = Arc::clone(&next_id);
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        let _ = std::thread::Builder::new()
            .name("tussen-hub console conn".into())
            .spawn(move || {
                if let Err(e) = connection(stream, port, &inbox, &next_id) {
                    log::debug!("console connection: {e}");
                }
            });
    }
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &str) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

/// What an `Origin` may do with the socket. `None` = refused.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Access {
    Full,
    ReadOnly,
}

/// The module doc has the policy; this is the table. A loopback host on
/// another port is the user's own software (any local process could
/// already connect with no `Origin` at all), but it did not serve the
/// console page, so it reads and never writes.
fn origin_access(origin: Option<&str>, port: u16) -> Option<Access> {
    let o = match origin {
        None | Some("null") => return Some(Access::Full),
        Some(o) => o,
    };
    if o == format!("http://127.0.0.1:{port}") || o == format!("http://localhost:{port}") {
        return Some(Access::Full);
    }
    let rest = o.strip_prefix("http://").or_else(|| o.strip_prefix("https://"))?;
    let host = match rest.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => rest,
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]").then_some(Access::ReadOnly)
}

fn connection(mut stream: TcpStream, port: u16, inbox: &Sender<ClientMsg>, next_id: &AtomicU64) -> anyhow::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut buf = Vec::with_capacity(1024);
    let head_len = loop {
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD {
            return respond(&mut stream, "431 Request Header Fields Too Large", "text/plain", "too large\n")
                .map_err(Into::into);
        }
    };

    let mut headers = [httparse::EMPTY_HEADER; 48];
    let mut req = httparse::Request::new(&mut headers);
    if !matches!(req.parse(&buf[..head_len]), Ok(httparse::Status::Complete(_))) {
        return respond(&mut stream, "400 Bad Request", "text/plain", "bad request\n").map_err(Into::into);
    }
    let header = |name: &str| {
        req.headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .and_then(|h| std::str::from_utf8(h.value).ok())
    };
    let full_path = req.path.unwrap_or("/");
    let path = full_path.split('?').next().unwrap_or("/");
    let query = full_path.split_once('?').map(|(_, q)| q).unwrap_or("");
    if req.method != Some("GET") {
        return respond(&mut stream, "405 Method Not Allowed", "text/plain", "GET only\n").map_err(Into::into);
    }
    match path {
        "/" | "/index.html" => respond(&mut stream, "200 OK", "text/html; charset=utf-8", PAGE)?,
        "/ws" => {
            let upgrade = header("upgrade").is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
            let Some(key) = header("sec-websocket-key").filter(|_| upgrade) else {
                return respond(&mut stream, "426 Upgrade Required", "text/plain", "websocket only\n")
                    .map_err(Into::into);
            };
            let Some(access) = origin_access(header("origin"), port) else {
                log::warn!("console: refused a WebSocket from origin {:?}", header("origin"));
                return respond(&mut stream, "403 Forbidden", "text/plain", "foreign origin\n").map_err(Into::into);
            };
            let readonly = access == Access::ReadOnly || query.split('&').any(|p| p == "readonly=1");
            let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
            write!(
                stream,
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
            )?;
            stream.flush()?;
            let leftover = buf[head_len..].to_vec();
            let ws = WebSocket::from_partially_read(stream, leftover, Role::Server, None);
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            page(ws, id, readonly, inbox)?;
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", "not found\n")?,
    }
    Ok(())
}

/// One page's session: read its commands, write what the console thread
/// queued for it. A short read timeout interleaves the two on one thread.
fn page(mut ws: WebSocket<TcpStream>, id: u64, readonly: bool, inbox: &Sender<ClientMsg>) -> anyhow::Result<()> {
    ws.get_mut().set_read_timeout(Some(Duration::from_millis(5)))?;
    ws.get_mut().set_write_timeout(Some(Duration::from_secs(2)))?;
    ws.get_mut().set_nodelay(true)?;
    let (tx, rx) = crossbeam_channel::bounded::<Arc<str>>(CLIENT_CAPACITY);
    inbox.send(ClientMsg::Join(id, tx, readonly))?;
    let result = (|| -> anyhow::Result<()> {
        loop {
            match ws.read() {
                Ok(Message::Text(t)) => inbox.send(ClientMsg::Text(id, t.as_str().to_owned()))?,
                Ok(Message::Close(_)) => return Ok(()),
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => return Ok(()),
                Err(e) => return Err(e.into()),
            }
            let mut wrote = false;
            loop {
                match rx.try_recv() {
                    Ok(frame) => {
                        ws.write(Message::text(&*frame))?;
                        wrote = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            }
            if wrote {
                match ws.flush() {
                    Ok(()) => {}
                    Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
    })();
    let _ = inbox.send(ClientMsg::Leave(id));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_commands_other_local_pages_read_and_the_web_is_refused() {
        // Full control: the console's own page, and non-browser clients.
        assert_eq!(origin_access(None, 9098), Some(Access::Full));
        assert_eq!(origin_access(Some("null"), 9098), Some(Access::Full));
        assert_eq!(origin_access(Some("http://127.0.0.1:9098"), 9098), Some(Access::Full));
        assert_eq!(origin_access(Some("http://localhost:9098"), 9098), Some(Access::Full));
        // Read-only: any other loopback origin (a local shader page).
        assert_eq!(origin_access(Some("http://127.0.0.1:8787"), 9098), Some(Access::ReadOnly));
        assert_eq!(origin_access(Some("http://localhost:5173"), 9098), Some(Access::ReadOnly));
        assert_eq!(origin_access(Some("http://[::1]:5173"), 9098), Some(Access::ReadOnly));
        assert_eq!(origin_access(Some("http://localhost"), 9098), Some(Access::ReadOnly));
        assert_eq!(origin_access(Some("https://127.0.0.1:4443"), 9098), Some(Access::ReadOnly));
        // Refused: everything non-loopback.
        assert_eq!(origin_access(Some("http://evil.example"), 9098), None);
        assert_eq!(origin_access(Some("http://evil.example:9098"), 9098), None);
        assert_eq!(origin_access(Some("http://127.0.0.1.evil.example:80"), 9098), None);
        assert_eq!(origin_access(Some("ftp://127.0.0.1:21"), 9098), None);
    }
}
