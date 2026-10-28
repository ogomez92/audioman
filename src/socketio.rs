//! A minimal Socket.IO v4 client: exactly what SonicRoom signaling needs.
//!
//! SonicRoom's server only offers the WebSocket transport, so there is no
//! long-polling handshake: one WebSocket to `/socket.io/?EIO=4&transport=websocket`
//! carrying Engine.IO text packets, with Socket.IO packets inside type `4`:
//!
//! * `0{...}` engine open, `2` / `3` ping / pong, `1` close
//! * `40` namespace connect (sent by us, echoed with the sid)
//! * `41` namespace disconnect: the **server** dropped us on purpose
//! * `42<id>["event",data]` an event, `43<id>[reply]` its acknowledgement
//!
//! The socket is switched to non-blocking once connected so the streaming
//! loop can [`Client::poll`] it between audio ticks; [`Client::request`]
//! busy-waits (2 ms naps) for its acknowledgement and is only used while
//! setting up.

use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use serde_json::Value;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// How long connecting (TCP + TLS + WebSocket upgrade) may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Why the connection is gone.
#[derive(Debug, PartialEq)]
pub enum Closed {
    /// The server disconnected us deliberately (`41`). For a caster that
    /// means someone in the room removed it.
    ByServer,
    /// The network or the server went away.
    Lost(String),
}

pub struct Client {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    next_ack: u64,
}

impl Client {
    /// Connect to `ws_url` (see [`websocket_url`]) and join the default
    /// namespace.
    pub fn connect(ws_url: &str, abort: &dyn Fn() -> bool) -> Result<Client, String> {
        let uri: tungstenite::http::Uri = ws_url
            .parse()
            .map_err(|_| "the server address is not valid".to_string())?;
        let host = uri.host().ok_or("the server address has no host")?;
        let secure = uri.scheme_str() == Some("wss");
        let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
        let addrs: Vec<_> = (host, port)
            .to_socket_addrs()
            .map_err(|_| format!("could not find {}", host))?
            .collect();
        let tcp = addrs
            .iter()
            .find_map(|a| TcpStream::connect_timeout(a, CONNECT_TIMEOUT).ok())
            .ok_or_else(|| format!("could not reach {}", host))?;
        tcp.set_read_timeout(Some(CONNECT_TIMEOUT))
            .and_then(|_| tcp.set_write_timeout(Some(CONNECT_TIMEOUT)))
            .and_then(|_| tcp.set_nodelay(true))
            .map_err(|e| format!("socket setup failed: {}", e))?;
        let (ws, _) = tungstenite::client_tls_with_config(ws_url, tcp, None, None)
            .map_err(|e| match e {
                tungstenite::HandshakeError::Failure(tungstenite::Error::Http(r)) => format!(
                    "{} answered with error {}, check that it is a SonicRoom server",
                    host,
                    r.status().as_u16()
                ),
                tungstenite::HandshakeError::Failure(tungstenite::Error::Tls(_)) => {
                    format!("the secure connection to {} failed", host)
                }
                _ => format!("could not connect to {}", host),
            })?;

        let mut client = Client { ws, next_ack: 0 };
        // Engine open packet, then join the default namespace.
        client.expect(|p| p.starts_with('0'), abort)?;
        client.send("40")?;
        client.expect(|p| p.starts_with("40"), abort)?;

        let tcp = match client.ws.get_mut() {
            MaybeTlsStream::Plain(s) => s,
            MaybeTlsStream::NativeTls(s) => s.get_mut(),
            _ => return Err("unsupported connection type".to_string()),
        };
        tcp.set_nonblocking(true)
            .map_err(|e| format!("socket setup failed: {}", e))?;
        Ok(client)
    }

    /// Emit `event` with `data` and wait up to `timeout` for the server's
    /// acknowledgement, returning its first argument.
    pub fn request(
        &mut self,
        event: &str,
        data: Value,
        timeout: Duration,
        abort: &dyn Fn() -> bool,
    ) -> Result<Value, String> {
        let id = self.next_ack;
        self.next_ack += 1;
        let packet = format!("42{}{}", id, Value::Array(vec![event.into(), data]));
        self.send(&packet)?;
        let prefix = format!("43{}", id);
        let deadline = Instant::now() + timeout;
        loop {
            if abort() {
                return Err("cancelled".to_string());
            }
            match self.read_packet().map_err(closed_text)? {
                Some(p) if p.starts_with(&prefix) => {
                    let reply: Value = serde_json::from_str(&p[prefix.len()..])
                        .map_err(|_| format!("bad reply to {}", event))?;
                    return Ok(reply.get(0).cloned().unwrap_or(Value::Null));
                }
                Some(_) => {}
                None if Instant::now() >= deadline => {
                    return Err(format!("the server did not answer {}", event))
                }
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
    }

    /// Service the connection without blocking: answer pings, flush queued
    /// writes, and report a closed connection. Server events are not needed
    /// by a send-only caster and are dropped.
    pub fn poll(&mut self) -> Result<(), Closed> {
        while self.read_packet()?.is_some() {}
        match self.ws.flush() {
            Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(lost(e)),
            Ok(()) => Ok(()),
        }
    }

    /// Leave politely: namespace disconnect, then the WebSocket close.
    pub fn close(mut self) {
        let _ = self.send("41");
        let _ = self.ws.close(None);
        let _ = self.ws.flush();
    }

    /// One Engine.IO packet if one is waiting. Pings are answered here.
    fn read_packet(&mut self) -> Result<Option<String>, Closed> {
        let msg = match self.ws.read() {
            Ok(m) => m,
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                return Ok(None)
            }
            Err(e) => return Err(lost(e)),
        };
        let Message::Text(text) = msg else {
            return Ok(Some(String::new()));
        };
        let text = text.as_str();
        if text == "2" {
            self.send("3").map_err(Closed::Lost)?;
        } else if text.starts_with("41") {
            return Err(Closed::ByServer);
        } else if text.starts_with('1') {
            return Err(Closed::Lost("the server closed the connection".to_string()));
        }
        Ok(Some(text.to_string()))
    }

    /// Block (in 2 ms naps) until a packet matching `want` arrives.
    fn expect(&mut self, want: impl Fn(&str) -> bool, abort: &dyn Fn() -> bool) -> Result<(), String> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        while Instant::now() < deadline {
            if abort() {
                return Err("cancelled".to_string());
            }
            match self.read_packet().map_err(closed_text)? {
                Some(p) if want(&p) => return Ok(()),
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        Err("the server did not answer".to_string())
    }

    fn send(&mut self, packet: &str) -> Result<(), String> {
        match self.ws.send(Message::text(packet)) {
            Ok(()) => Ok(()),
            // Queued; the next poll flushes it.
            Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(closed_text(lost(e))),
        }
    }
}

/// Spoken wording for a broken connection. The OS text ("os error 10054",
/// localized) means nothing read aloud, so only the gist is kept.
fn lost(e: tungstenite::Error) -> Closed {
    let gist = match &e {
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            "the server closed the connection"
        }
        tungstenite::Error::Io(io)
            if matches!(
                io.kind(),
                ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
                    | ErrorKind::UnexpectedEof
            ) =>
        {
            "the server closed the connection"
        }
        _ => "lost the connection to the server",
    };
    Closed::Lost(gist.to_string())
}

fn closed_text(c: Closed) -> String {
    match c {
        Closed::ByServer => "the server disconnected us".to_string(),
        Closed::Lost(e) => e,
    }
}

/// The Socket.IO WebSocket endpoint for what a person typed as the server:
/// `host`, `host:port`, `https://host` or a whole room link. `http` maps to
/// `ws`, everything else to `wss`; any path is ignored.
pub fn websocket_url(server: &str) -> Result<String, String> {
    let server = server.trim();
    let (secure, rest) = if let Some(r) = strip_prefix_ci(server, "https://") {
        (true, r)
    } else if let Some(r) = strip_prefix_ci(server, "http://") {
        (false, r)
    } else if let Some(r) = strip_prefix_ci(server, "wss://") {
        (true, r)
    } else if let Some(r) = strip_prefix_ci(server, "ws://") {
        (false, r)
    } else {
        (true, server)
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains(char::is_whitespace) {
        return Err("the server address is empty or not valid".to_string());
    }
    let scheme = if secure { "wss" } else { "ws" };
    Ok(format!(
        "{}://{}/socket.io/?EIO=4&transport=websocket",
        scheme, authority
    ))
}

/// The room named by a pasted room link (`https://host/room/<name>`), if any.
pub fn room_from_link(server: &str) -> Option<String> {
    let after = server.split("/room/").nth(1)?;
    let room = after.split(['/', '?', '#']).next()?.trim();
    (!room.is_empty()).then(|| room.to_lowercase())
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &s[prefix.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_urls() {
        let ok = |s: &str| websocket_url(s).unwrap();
        assert_eq!(
            ok("sonic.example.com"),
            "wss://sonic.example.com/socket.io/?EIO=4&transport=websocket"
        );
        assert_eq!(
            ok("http://localhost:3100/"),
            "ws://localhost:3100/socket.io/?EIO=4&transport=websocket"
        );
        assert_eq!(
            ok(" HTTPS://sonic.example.com/room/Party?x=1 "),
            "wss://sonic.example.com/socket.io/?EIO=4&transport=websocket"
        );
        assert!(websocket_url("").is_err());
        assert!(websocket_url("https://").is_err());
    }

    #[test]
    fn rooms_from_links() {
        assert_eq!(
            room_from_link("https://sonic.example.com/room/Party?lang=es"),
            Some("party".to_string())
        );
        assert_eq!(room_from_link("https://sonic.example.com/"), None);
        assert_eq!(room_from_link("https://sonic.example.com/room/"), None);
    }
}
