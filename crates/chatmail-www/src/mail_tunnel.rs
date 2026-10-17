// Copyright (C) 2026 themadorg
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: AGPL-3.0-or-later

//! IMAP/SMTP over engine.io (`/engine.io/?EIO=4&port=993`): one engine.io
//! session is one raw byte tunnel to *this* server's own IMAP or submission
//! listener, so browser clients need no third-party WebSocket-TCP bridge.
//! engine.io gives WebSocket with an HTTP long-polling fallback.
//!
//! TLS still terminates in the client: the tunnel only relays the bytes the
//! client would otherwise send to the port directly, and can reach nothing the
//! client couldn't reach without it. Binary engine.io messages carry the
//! stream; text messages are ignored.
//!
//! Abuse limits (each one closes the offending session, never the server):
//! [`MAX_TUNNELS`] open at once server-wide and [`MAX_TUNNELS_PER_CLIENT`] per
//! client network; 64 KiB per WebSocket message and 128 KiB per polling
//! request; about 1 MiB of queued bytes per tunnel; and [`IDLE`] without a
//! byte either way. Logging in, and its rate limits, stay with the IMAP/SMTP
//! listener behind the tunnel: it sees the same bytes a direct client sends.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::ConnectInfo;
use chatmail_state::AppState;
use engineioxide::config::EngineIoConfig;
use engineioxide::handler::EngineIoHandler;
use engineioxide::service::EngineIoService;
use engineioxide::{DisconnectReason, Socket, Str};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{self, error::TrySendError};

/// Tunnels (each one a TCP connection to a mail listener) open at once.
pub const MAX_TUNNELS: usize = 256;
/// …of which one client network (IPv4 address, IPv6 /64) may hold this many.
/// Loopback is exempt: behind a reverse proxy every client arrives from it.
pub const MAX_TUNNELS_PER_CLIENT: usize = 16;
/// A tunnel that moved no byte either way for this long is closed. IMAP IDLE
/// clients re-issue IDLE well within it (RFC 2177: at most every 29 minutes).
pub const IDLE: Duration = Duration::from_secs(35 * 60);
/// Largest client message: one WebSocket message, or one polling request.
const MAX_WS_MESSAGE: usize = 64 * 1024;
const MAX_POLL_PAYLOAD: u64 = 128 * 1024;
/// Messages queued client→server (×64 KiB) before a stalled tunnel is closed,
/// and packets queued server→client (×16 KiB) before the reader waits.
const UP_QUEUE: usize = 8;
const DOWN_QUEUE: usize = 32;
const DOWN_CHUNK: usize = 16 * 1024;

pub struct MailTunnel {
    app: Arc<AppState>,
    slots: Mutex<Slots>,
}

#[derive(Default)]
struct Slots {
    total: usize,
    per_client: HashMap<IpAddr, usize>,
}

/// One open tunnel's share of the limits, given back on drop.
struct Slot {
    tunnel: Arc<MailTunnel>,
    client: Option<IpAddr>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut slots = self.tunnel.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots.total -= 1;
        if let Some(client) = self.client {
            if let Some(n) = slots.per_client.get_mut(&client) {
                *n -= 1;
                if *n == 0 {
                    slots.per_client.remove(&client);
                }
            }
        }
    }
}

impl std::fmt::Debug for MailTunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MailTunnel")
    }
}

#[derive(Default)]
pub struct Conn {
    up: OnceLock<mpsc::Sender<Bytes>>,
}

pub fn service(app: Arc<AppState>) -> EngineIoService<MailTunnel> {
    let config = EngineIoConfig::builder()
        .max_payload(MAX_POLL_PAYLOAD)
        .ws_max_message_size(MAX_WS_MESSAGE)
        .ws_max_frame_size(MAX_WS_MESSAGE)
        .max_buffer_size(DOWN_QUEUE)
        .build();
    let tunnel = MailTunnel {
        app,
        slots: Mutex::default(),
    };
    EngineIoService::with_config(Arc::new(tunnel), config)
}

/// The network a client counts against [`MAX_TUNNELS_PER_CLIENT`] by: its
/// IPv4 address or IPv6 /64, none for loopback (a reverse proxy) or unknown.
fn client_net(peer: Option<SocketAddr>) -> Option<IpAddr> {
    match peer?.ip().to_canonical() {
        ip if ip.is_loopback() => None,
        IpAddr::V6(ip) => Some(IpAddr::V6(Ipv6Addr::from(
            u128::from(ip) & !((1u128 << 64) - 1),
        ))),
        ip => Some(ip),
    }
}

impl MailTunnel {
    fn take_slot(self: &Arc<Self>, client: Option<IpAddr>) -> Option<Slot> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if slots.total >= MAX_TUNNELS {
            return None;
        }
        if let Some(client) = client {
            let n = slots.per_client.entry(client).or_default();
            if *n >= MAX_TUNNELS_PER_CLIENT {
                return None;
            }
            *n += 1;
        }
        slots.total += 1;
        Some(Slot {
            tunnel: Arc::clone(self),
            client,
        })
    }

    /// The local listener serving `port`, if it is an IMAP/submission one (or
    /// the HTTPS port while it multiplexes IMAP/SMTP by ALPN).
    fn target(&self, port: u16) -> Option<SocketAddr> {
        let snap = self.app.listener_ports.snapshot();
        let alpn = self.app.shared_port.imap() || self.app.shared_port.smtp();
        [
            snap.imap_plain_addr,
            snap.imap_tls_addr,
            snap.submission_plain_addr,
            snap.submission_tls_addr,
            snap.http_tls_addr.filter(|_| alpn),
        ]
        .into_iter()
        .flatten()
        .filter_map(|a| a.parse::<SocketAddr>().ok())
        .find(|a| a.port() == port)
        .map(dialable)
    }
}

/// A wildcard bind (`0.0.0.0` / `[::]`) is dialed on loopback.
fn dialable(mut addr: SocketAddr) -> SocketAddr {
    match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => addr.set_ip(Ipv4Addr::LOCALHOST.into()),
        IpAddr::V6(ip) if ip.is_unspecified() => addr.set_ip(Ipv6Addr::LOCALHOST.into()),
        _ => {}
    }
    addr
}

fn requested_port(query: Option<&str>) -> Option<u16> {
    query?
        .split('&')
        .find_map(|kv| kv.strip_prefix("port="))?
        .parse()
        .ok()
}

impl EngineIoHandler for MailTunnel {
    type Data = Conn;

    fn on_connect(self: Arc<Self>, socket: Arc<Socket<Conn>>) {
        let Some(addr) = requested_port(socket.req_parts.uri.query()).and_then(|p| self.target(p))
        else {
            socket.close(DisconnectReason::TransportClose);
            return;
        };
        let peer = socket
            .req_parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(peer)| *peer);
        let Some(slot) = self.take_slot(client_net(peer)) else {
            tracing::debug!(?peer, "mail tunnel: limit reached, session refused");
            socket.close(DisconnectReason::TransportClose);
            return;
        };
        let (tx, rx) = mpsc::channel(UP_QUEUE);
        let _ = socket.data.up.set(tx);
        tokio::spawn(pump(socket, addr, rx, slot));
    }

    fn on_disconnect(&self, _socket: Arc<Socket<Conn>>, _reason: DisconnectReason) {}

    fn on_message(self: &Arc<Self>, _msg: Str, _socket: Arc<Socket<Conn>>) {}

    fn on_binary(self: &Arc<Self>, data: Bytes, socket: Arc<Socket<Conn>>) {
        let full = socket
            .data
            .up
            .get()
            .is_some_and(|up| up.try_send(data).is_err());
        if full {
            socket.close(DisconnectReason::TransportError);
        }
    }
}

/// Relays one tunnel until either side closes or it idles out; `_slot` holds
/// its share of the limits for exactly as long as the TCP connection lives.
async fn pump(
    socket: Arc<Socket<Conn>>,
    addr: SocketAddr,
    mut up: mpsc::Receiver<Bytes>,
    _slot: Slot,
) {
    let tcp = match TcpStream::connect(addr).await {
        Ok(tcp) => tcp,
        Err(e) => {
            tracing::debug!(%addr, error = %e, "mail tunnel: local connect failed");
            socket.close(DisconnectReason::TransportError);
            return;
        }
    };
    let _ = tcp.set_nodelay(true);
    let (mut rd, mut wr) = tcp.into_split();
    // Seconds since `start` at the last byte either way.
    let start = Instant::now();
    let last = AtomicU64::new(0);
    let touch = || last.store(start.elapsed().as_secs(), Ordering::Relaxed);

    let upstream = async {
        while let Some(chunk) = up.recv().await {
            touch();
            wr.write_all(&chunk).await?;
        }
        std::io::Result::Ok(())
    };
    let downstream = async {
        let mut buf = vec![0u8; DOWN_CHUNK];
        loop {
            let n = rd.read(&mut buf).await?;
            if n == 0 {
                return std::io::Result::Ok(());
            }
            touch();
            let mut chunk = Bytes::copy_from_slice(&buf[..n]);
            loop {
                match socket.emit_binary(chunk) {
                    Ok(()) => break,
                    // ponytail: engineioxide has no async reserve, so a full
                    // send buffer (slow poller) is waited out by sleeping.
                    Err(TrySendError::Full(back)) => {
                        chunk = back;
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(TrySendError::Closed(_)) => return Ok(()),
                }
            }
        }
    };

    // ponytail: checked once a minute, so a tunnel lives up to IDLE + 1 min.
    let idle = async {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            if start.elapsed().as_secs() - last.load(Ordering::Relaxed) >= IDLE.as_secs() {
                return;
            }
        }
    };

    tokio::select! {
        _ = upstream => {}
        _ = downstream => {}
        _ = idle => {}
        _ = socket.closed() => {}
    }
    socket.close(DisconnectReason::TransportClose);
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use chatmail_db::{init_memory_db, set_setting, settings_keys};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    use tower::ServiceExt;

    /// "a1 NOOP\r\n" and its engine.io v4 long-polling form.
    const NOOP: &[u8] = b"a1 NOOP\r\n";
    const NOOP_B64: &str = "bYTEgTk9PUA0K";

    /// The www router with the tunnel switched `on` (or not) and a local echo
    /// server registered as the IMAP TLS listener; returns it and the echo port.
    async fn setup(on: bool) -> (axum::Router, u16) {
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = echo.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        let pool = init_memory_db().await.unwrap();
        if on {
            set_setting(&pool, settings_keys::MAIL_TUNNEL_ENABLED, "true")
                .await
                .unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let app_state = Arc::new(AppState::new(dir.path(), pool.clone()));
        app_state.listener_ports.set_runtime(
            "0.0.0.0:25",
            None,
            Some(format!("0.0.0.0:{port}")),
            None,
            None,
            None,
            None,
        );
        let app = crate::www_router(crate::WwwState::new(
            pool,
            app_state,
            chatmail_config::AppConfig::default(),
            dir.keep(),
        ));
        (app, port)
    }

    /// Serves `app` on a loopback port, for the WebSocket transport.
    async fn serve(app: axum::Router) -> std::net::SocketAddr {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        addr
    }

    async fn poll(
        app: &axum::Router,
        method: &str,
        query: &str,
        body: &str,
    ) -> (StatusCode, String) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!("/engine.io/?EIO=4&transport=polling&{query}"))
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.headers()["access-control-allow-origin"], "*");
        let status = resp.status();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    /// A long-polling response without the server's pings and noops.
    fn data(payload: &str) -> String {
        payload
            .split('\u{1e}')
            .filter(|p| *p != "2" && *p != "6")
            .collect::<Vec<_>>()
            .join("\u{1e}")
    }

    fn sid(open: &str) -> &str {
        open.split("\"sid\":\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
    }

    /// Next frame that is not an engine.io ping (`2`) or noop (`6`).
    async fn next_frame<S>(ws: &mut S) -> Message
    where
        S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Text(t) if t.as_str() == "2" || t.as_str() == "6" => {}
                frame => return frame,
            }
        }
    }

    #[tokio::test]
    async fn long_polling_echoes_through_local_listener() {
        let (app, port) = setup(true).await;
        let (_, open) = poll(&app, "GET", &format!("port={port}"), "").await;
        let sid = sid(&open);
        assert_eq!(
            poll(&app, "POST", &format!("sid={sid}"), NOOP_B64).await.1,
            "ok"
        );
        assert_eq!(
            data(&poll(&app, "GET", &format!("sid={sid}"), "").await.1),
            NOOP_B64
        );
    }

    #[tokio::test]
    async fn websocket_echoes_through_local_listener() {
        let (app, port) = setup(true).await;
        let addr = serve(app).await;
        let url = format!("ws://{addr}/engine.io/?EIO=4&transport=websocket&port={port}");
        let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        assert!(matches!(next_frame(&mut ws).await, Message::Text(t) if t.starts_with("0{")));
        ws.send(Message::Binary(NOOP.to_vec().into()))
            .await
            .unwrap();
        assert_eq!(
            next_frame(&mut ws).await,
            Message::Binary(NOOP.to_vec().into())
        );
    }

    /// The engine.io client default: open by polling, then upgrade the same
    /// session (and the same TCP stream behind it) to WebSocket.
    #[tokio::test]
    async fn polling_session_upgrades_to_websocket() {
        let (app, port) = setup(true).await;
        let (_, open) = poll(&app, "GET", &format!("port={port}"), "").await;
        let sid = sid(&open).to_string();
        assert_eq!(
            poll(&app, "POST", &format!("sid={sid}"), NOOP_B64).await.1,
            "ok"
        );
        assert_eq!(
            data(&poll(&app, "GET", &format!("sid={sid}"), "").await.1),
            NOOP_B64
        );

        let addr = serve(app).await;
        let url = format!("ws://{addr}/engine.io/?EIO=4&transport=websocket&sid={sid}");
        let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        ws.send(Message::Text("2probe".into())).await.unwrap();
        assert_eq!(next_frame(&mut ws).await, Message::Text("3probe".into()));
        ws.send(Message::Text("5".into())).await.unwrap();
        ws.send(Message::Binary(NOOP.to_vec().into()))
            .await
            .unwrap();
        assert_eq!(
            next_frame(&mut ws).await,
            Message::Binary(NOOP.to_vec().into())
        );
    }

    #[tokio::test]
    async fn non_listener_port_gets_no_tunnel() {
        let (app, _) = setup(true).await;
        let (_, open) = poll(&app, "GET", "port=22", "").await;
        let sid = sid(&open);
        let (_, gone) = poll(&app, "GET", &format!("sid={sid}"), "").await;
        assert!(gone.contains("Session ID unknown"), "{gone}");
    }

    #[tokio::test]
    async fn switched_off_by_default() {
        let (app, port) = setup(false).await;
        let (status, body) = poll(&app, "GET", &format!("port={port}"), "").await;
        assert_eq!(
            (status, body.as_str()),
            (StatusCode::NOT_FOUND, r#"{"error":"not found"}"#)
        );
    }

    /// Opens a polling session for `port` from `peer`; its sid.
    async fn open_from(app: &axum::Router, port: u16, peer: &str) -> String {
        let mut req = Request::builder()
            .uri(format!("/engine.io/?EIO=4&transport=polling&port={port}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        let body = to_bytes(
            app.clone().oneshot(req).await.unwrap().into_body(),
            usize::MAX,
        )
        .await
        .unwrap();
        sid(std::str::from_utf8(&body).unwrap()).to_string()
    }

    /// Whether the session still tunnels (a POST is accepted).
    async fn alive(app: &axum::Router, sid: &str) -> bool {
        poll(app, "POST", &format!("sid={sid}"), NOOP_B64).await.1 == "ok"
    }

    #[tokio::test]
    async fn one_client_network_gets_a_bounded_share() {
        let (app, port) = setup(true).await;
        let mut held = Vec::new();
        for i in 0..MAX_TUNNELS_PER_CLIENT {
            // one IPv6 /64, different addresses in it
            held.push(open_from(&app, port, &format!("[2001:db8:1:2::{:x}]:4000", i + 1)).await);
        }
        for sid in &held {
            assert!(alive(&app, sid).await);
        }
        let over = open_from(&app, port, "[2001:db8:1:2::ffff]:4000").await;
        assert!(!alive(&app, &over).await, "a 17th tunnel from the same /64");
        let other = open_from(&app, port, "198.51.100.7:4000").await;
        assert!(alive(&app, &other).await, "another client is unaffected");
    }

    #[tokio::test]
    async fn oversized_websocket_message_closes_the_session() {
        let (app, port) = setup(true).await;
        let addr = serve(app).await;
        let url = format!("ws://{addr}/engine.io/?EIO=4&transport=websocket&port={port}");
        let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        assert!(matches!(next_frame(&mut ws).await, Message::Text(t) if t.starts_with("0{")));
        // exactly the limit still tunnels (echoed back in 16 KiB chunks)…
        ws.send(Message::Binary(vec![7u8; MAX_WS_MESSAGE].into()))
            .await
            .unwrap();
        let mut echoed = 0;
        while echoed < MAX_WS_MESSAGE {
            match next_frame(&mut ws).await {
                Message::Binary(b) => echoed += b.len(),
                other => panic!("unexpected {other:?}"),
            }
        }
        // …one byte more ends the session
        let _ = ws
            .send(Message::Binary(vec![0u8; MAX_WS_MESSAGE + 1].into()))
            .await;
        let end = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match ws.next().await {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                    Some(Ok(Message::Text(t))) if t.as_str() == "1" => return,
                    Some(Ok(_)) => {}
                }
            }
        })
        .await;
        assert!(end.is_ok(), "session still open after a 64 KiB + 1 message");
    }

    #[tokio::test]
    async fn slots_cap_per_client_and_in_total() {
        let tunnel = Arc::new(MailTunnel {
            app: Arc::new(AppState::new(
                tempfile::tempdir().unwrap().keep(),
                init_memory_db().await.unwrap(),
            )),
            slots: Mutex::default(),
        });
        let net = |i: u32| Some(IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + i)));
        let a: Vec<_> = (0..MAX_TUNNELS_PER_CLIENT)
            .map(|_| tunnel.take_slot(net(1)).unwrap())
            .collect();
        assert!(tunnel.take_slot(net(1)).is_none());
        drop(a);
        assert!(
            tunnel.take_slot(net(1)).is_some(),
            "slots come back on drop"
        );

        let mut all: Vec<_> = (0..MAX_TUNNELS as u32)
            .map(|i| tunnel.take_slot(net(100 + i)).unwrap())
            .collect();
        assert!(tunnel.take_slot(net(9999)).is_none());
        assert!(
            tunnel.take_slot(None).is_none(),
            "loopback still counts in total"
        );
        all.pop();
        assert!(tunnel.take_slot(None).is_some());
        assert!(tunnel.slots.lock().unwrap().per_client.len() <= MAX_TUNNELS);
    }

    #[test]
    fn client_networks() {
        let net = |p: &str| client_net(Some(p.parse().unwrap()));
        assert_eq!(net("198.51.100.7:1"), Some("198.51.100.7".parse().unwrap()));
        assert_eq!(
            net("[2001:db8:1:2:aaaa::1]:1"),
            Some("2001:db8:1:2::".parse().unwrap())
        );
        assert_eq!(
            net("[::ffff:198.51.100.7]:1"),
            Some("198.51.100.7".parse().unwrap())
        );
        assert_eq!(net("127.0.0.1:1"), None);
        assert_eq!(net("[::1]:1"), None);
        assert_eq!(client_net(None), None);
    }

    #[test]
    fn port_query_and_wildcard_dial() {
        assert_eq!(
            requested_port(Some("EIO=4&transport=polling&port=993")),
            Some(993)
        );
        assert_eq!(requested_port(Some("EIO=4&port=x")), None);
        assert_eq!(requested_port(None), None);
        assert_eq!(
            dialable("0.0.0.0:993".parse().unwrap()),
            "127.0.0.1:993".parse().unwrap()
        );
        assert_eq!(
            dialable("[::]:465".parse().unwrap()),
            "[::1]:465".parse().unwrap()
        );
        assert_eq!(
            dialable("10.0.0.1:465".parse().unwrap()),
            "10.0.0.1:465".parse().unwrap()
        );
    }
}
