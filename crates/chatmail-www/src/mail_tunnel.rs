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

//! A byte tunnel from browser clients (which cannot open TCP sockets) to
//! *this* server's own IMAP and submission listeners. Beta: off until
//! `madmail mailtunnel enable` (`__MAILTUNNEL_ENABLED__`).
//!
//! - `GET /mailtunnel/{port}` with a WebSocket upgrade: binary messages carry
//!   the stream both ways and nothing else, so any WebSocket client works. A
//!   text message closes the tunnel; keepalive uses WebSocket pings.
//! - Long-polling fallback, for networks that block WebSocket, in plain HTTP
//!   with raw bodies: `POST /mailtunnel/{port}` opens a session and answers
//!   its id; `GET /mailtunnel/poll/{id}` waits up to 25 s and answers the
//!   bytes that arrived (empty: nothing yet; 404: the tunnel ended);
//!   `POST /mailtunnel/poll/{id}` sends bytes; `POST
//!   /mailtunnel/poll/{id}/close` ends it. All of these are CORS "simple"
//!   requests, so a browser sends no preflight.
//! - `GET /mailtunnel` answers `mailtunnel 1`, for clients to detect it.
//!
//! `port` must be one of this server's live IMAP/submission listeners (or
//! the HTTPS port while it multiplexes IMAP/SMTP by ALPN), dialed over
//! loopback. TLS still terminates in the client: the tunnel relays the bytes
//! a client would otherwise send to the port directly, and reaches nothing
//! that client couldn't reach without it.
//!
//! Abuse limits (each one ends the offending tunnel, never the server):
//! [`MAX_TUNNELS`] open at once server-wide and [`MAX_TUNNELS_PER_CLIENT`] per
//! client network; 64 KiB per WebSocket message or request body; about
//! 1 MiB of queued bytes per tunnel; [`IDLE`] without a byte either way; a
//! WebSocket that stops answering pings, or a poller that stops polling,
//! after about a minute. Logging in, and its rate limits, stay with the
//! IMAP/SMTP listener behind the tunnel.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use chatmail_state::AppState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Notify};
use tokio::time::timeout;

/// Tunnels (each one a TCP connection to a mail listener) open at once.
pub const MAX_TUNNELS: usize = 256;
/// …of which one client network (IPv4 address, IPv6 /64) may hold this many.
/// Loopback is exempt: behind a reverse proxy every client arrives from it.
pub const MAX_TUNNELS_PER_CLIENT: usize = 16;
/// A tunnel that moved no byte either way for this long is closed. IMAP IDLE
/// clients re-issue IDLE well within it (RFC 2177: at most every 29 minutes).
pub const IDLE: Duration = Duration::from_secs(35 * 60);
/// Largest WebSocket message, request body and (about) poll reply.
const MAX_CHUNK: usize = 64 * 1024;
/// Chunks queued towards the listener (×64 KiB) and the client (×16 KiB).
const UP_QUEUE: usize = 8;
const DOWN_QUEUE: usize = 32;
const DOWN_CHUNK: usize = 16 * 1024;
/// How long a poll waits for bytes, and how long without a poll means gone.
const POLL_WAIT: Duration = Duration::from_secs(25);
const POLL_GONE: Duration = Duration::from_secs(60);
/// WebSocket ping interval; three missed answers end the tunnel.
const WS_PING: Duration = Duration::from_secs(25);
/// Longest wait for the other side to take bytes before giving up on it.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct MailTunnel(Arc<Inner>);

struct Inner {
    app: Arc<AppState>,
    slots: Mutex<Slots>,
    polls: Mutex<HashMap<u128, Arc<Poll>>>,
}

#[derive(Default)]
struct Slots {
    total: usize,
    per_client: HashMap<IpAddr, usize>,
}

/// One open tunnel's share of the limits, given back on drop.
struct Slot {
    tunnel: MailTunnel,
    client: Option<IpAddr>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut slots = self
            .tunnel
            .0
            .slots
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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

/// A long-polling session: the client's half of the tunnel's two queues.
struct Poll {
    up: mpsc::Sender<Bytes>,
    down: tokio::sync::Mutex<mpsc::Receiver<Bytes>>,
    born: Instant,
    /// Milliseconds since `born` at the last poll.
    polled: AtomicU64,
    closed: Notify,
    drained: Notify,
}

impl Poll {
    fn touch(&self) {
        self.polled
            .store(self.born.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// Resolves once the client closed the session or stopped polling.
    async fn gone(&self) {
        let stale = async {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let since =
                    self.born.elapsed().as_millis() as u64 - self.polled.load(Ordering::Relaxed);
                // a reader blocked in a poll holds the lock: still there
                if since > POLL_GONE.as_millis() as u64 && self.down.try_lock().is_ok() {
                    return;
                }
            }
        };
        tokio::select! {
            _ = stale => {}
            _ = self.closed.notified() => {}
        }
    }
}

/// The client's address, if the listener recorded one (axum `ConnectInfo`).
struct Peer(Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for Peer {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        Ok(Peer(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(peer)| *peer),
        ))
    }
}

pub fn router(app: Arc<AppState>) -> Router {
    let tunnel = MailTunnel(Arc::new(Inner {
        app,
        slots: Mutex::default(),
        polls: Mutex::default(),
    }));
    Router::new()
        .route("/mailtunnel", get(|| async { "mailtunnel 1" }))
        .route("/mailtunnel/{port}", get(ws_open).post(poll_open))
        .route("/mailtunnel/poll/{id}", get(poll_read).post(poll_write))
        .route("/mailtunnel/poll/{id}/close", post(poll_close))
        .layer(DefaultBodyLimit::max(MAX_CHUNK))
        .with_state(tunnel)
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

/// A wildcard bind (`0.0.0.0` / `[::]`) is dialed on loopback.
fn dialable(mut addr: SocketAddr) -> SocketAddr {
    match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => addr.set_ip(Ipv4Addr::LOCALHOST.into()),
        IpAddr::V6(ip) if ip.is_unspecified() => addr.set_ip(Ipv6Addr::LOCALHOST.into()),
        _ => {}
    }
    addr
}

impl MailTunnel {
    /// The local listener serving `port`, if it is an IMAP/submission one (or
    /// the HTTPS port while it multiplexes IMAP/SMTP by ALPN).
    fn target(&self, port: u16) -> Option<SocketAddr> {
        let app = &self.0.app;
        let snap = app.listener_ports.snapshot();
        let alpn = app.shared_port.imap() || app.shared_port.smtp();
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

    fn take_slot(&self, client: Option<IpAddr>) -> Option<Slot> {
        let mut slots = self.0.slots.lock().unwrap_or_else(|e| e.into_inner());
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
            tunnel: self.clone(),
            client,
        })
    }

    /// Checks `port` and the limits before any work: 404 for a port that is
    /// none of our listeners, 503 when the client or the server is at its cap.
    fn admit(&self, port: u16, peer: Option<SocketAddr>) -> Result<(SocketAddr, Slot), StatusCode> {
        let addr = self.target(port).ok_or(StatusCode::NOT_FOUND)?;
        let slot = self.take_slot(client_net(peer)).ok_or_else(|| {
            tracing::debug!(?peer, "mail tunnel: limit reached, refused");
            StatusCode::SERVICE_UNAVAILABLE
        })?;
        Ok((addr, slot))
    }

    fn poll(&self, id: &str) -> Option<Arc<Poll>> {
        let id = u128::from_str_radix(id, 16).ok()?;
        self.0
            .polls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
    }

    fn forget(&self, id: u128) {
        self.0
            .polls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }
}

async fn connect(addr: SocketAddr) -> Option<TcpStream> {
    match timeout(Duration::from_secs(10), TcpStream::connect(addr)).await {
        Ok(Ok(tcp)) => {
            let _ = tcp.set_nodelay(true);
            Some(tcp)
        }
        Ok(Err(e)) => {
            tracing::debug!(%addr, error = %e, "mail tunnel: local connect failed");
            None
        }
        Err(_) => None,
    }
}

/// Pipes `tcp` to and from the client's queues until either side ends, the
/// tunnel idles out, or `gone` resolves. Dropping `down` on the way out lets
/// the client drain what is queued, then see the end.
async fn relay(
    tcp: TcpStream,
    mut up: mpsc::Receiver<Bytes>,
    down: mpsc::Sender<Bytes>,
    gone: impl std::future::Future<Output = ()>,
) {
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
            if down.send(Bytes::copy_from_slice(&buf[..n])).await.is_err() {
                return Ok(());
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
        _ = gone => {}
    }
}

async fn ws_open(
    State(tunnel): State<MailTunnel>,
    Path(port): Path<u16>,
    Peer(peer): Peer,
    ws: WebSocketUpgrade,
) -> Response {
    let (addr, slot) = match tunnel.admit(port, peer) {
        Ok(admitted) => admitted,
        Err(status) => return status.into_response(),
    };
    ws.max_message_size(MAX_CHUNK)
        .max_frame_size(MAX_CHUNK)
        .on_upgrade(move |socket| ws_tunnel(socket, addr, slot))
}

async fn ws_tunnel(mut ws: WebSocket, addr: SocketAddr, _slot: Slot) {
    let Some(tcp) = connect(addr).await else {
        return; // dropping the socket closes it
    };
    let (up_tx, up_rx) = mpsc::channel(UP_QUEUE);
    let (down_tx, mut down_rx) = mpsc::channel::<Bytes>(DOWN_QUEUE);
    let pump = async move {
        let mut ping = tokio::time::interval(WS_PING);
        let mut heard = Instant::now();
        loop {
            tokio::select! {
                msg = ws.recv() => match msg {
                    Some(Ok(Message::Binary(bytes))) => {
                        heard = Instant::now();
                        if up_tx.send(bytes).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => heard = Instant::now(),
                    // a text message, a close, an error or the end
                    _ => return,
                },
                chunk = down_rx.recv() => {
                    let Some(bytes) = chunk else {
                        let _ = ws.send(Message::Close(None)).await;
                        return;
                    };
                    if !matches!(timeout(WRITE_TIMEOUT, ws.send(Message::Binary(bytes))).await, Ok(Ok(()))) {
                        return;
                    }
                }
                _ = ping.tick() => {
                    if heard.elapsed() > 3 * WS_PING
                        || ws.send(Message::Ping(Bytes::new())).await.is_err()
                    {
                        return;
                    }
                }
            }
        }
    };
    // join, not select: when the listener ends first, the pump still drains
    // what it sent; when the client ends first, dropping `up_tx` ends the relay.
    tokio::join!(relay(tcp, up_rx, down_tx, std::future::pending()), pump);
}

async fn poll_open(
    State(tunnel): State<MailTunnel>,
    Path(port): Path<u16>,
    Peer(peer): Peer,
) -> Response {
    let (addr, slot) = match tunnel.admit(port, peer) {
        Ok(admitted) => admitted,
        Err(status) => return status.into_response(),
    };
    let Some(tcp) = connect(addr).await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let (up_tx, up_rx) = mpsc::channel(UP_QUEUE);
    let (down_tx, down_rx) = mpsc::channel(DOWN_QUEUE);
    let poll = Arc::new(Poll {
        up: up_tx,
        down: tokio::sync::Mutex::new(down_rx),
        born: Instant::now(),
        polled: AtomicU64::new(0),
        closed: Notify::new(),
        drained: Notify::new(),
    });
    let id = rand::random::<u128>();
    tunnel
        .0
        .polls
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, Arc::clone(&poll));
    tokio::spawn(async move {
        relay(tcp, up_rx, down_tx, poll.gone()).await;
        // let the client read what is still queued, then forget the session
        tokio::select! {
            _ = poll.drained.notified() => {}
            _ = poll.closed.notified() => {}
            _ = tokio::time::sleep(POLL_GONE) => {}
        }
        tunnel.forget(id);
        drop(slot);
    });
    format!("{id:032x}").into_response()
}

async fn poll_read(State(tunnel): State<MailTunnel>, Path(id): Path<String>) -> Response {
    let Some(poll) = tunnel.poll(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // one reader at a time
    let Ok(mut down) = poll.down.try_lock() else {
        return StatusCode::CONFLICT.into_response();
    };
    poll.touch();
    let first = timeout(POLL_WAIT, down.recv()).await;
    poll.touch();
    match first {
        Err(_) => StatusCode::OK.into_response(), // nothing yet
        Ok(None) => {
            poll.drained.notify_one();
            StatusCode::NOT_FOUND.into_response()
        }
        Ok(Some(bytes)) => {
            let mut body = bytes.to_vec();
            while body.len() < MAX_CHUNK {
                match down.try_recv() {
                    Ok(more) => body.extend_from_slice(&more),
                    Err(_) => break,
                }
            }
            ([(header::CONTENT_TYPE, "application/octet-stream")], body).into_response()
        }
    }
}

async fn poll_write(
    State(tunnel): State<MailTunnel>,
    Path(id): Path<String>,
    body: Bytes,
) -> StatusCode {
    let Some(poll) = tunnel.poll(&id) else {
        return StatusCode::NOT_FOUND;
    };
    if body.is_empty() {
        return StatusCode::NO_CONTENT;
    }
    match timeout(WRITE_TIMEOUT, poll.up.send(body)).await {
        Ok(Ok(())) => StatusCode::NO_CONTENT,
        Ok(Err(_)) => StatusCode::NOT_FOUND,
        Err(_) => {
            poll.closed.notify_one();
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

async fn poll_close(State(tunnel): State<MailTunnel>, Path(id): Path<String>) -> StatusCode {
    if let Some(poll) = tunnel.poll(&id) {
        poll.closed.notify_one();
    }
    StatusCode::NO_CONTENT
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use chatmail_db::{init_memory_db, set_setting, settings_keys};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    use tower::ServiceExt;

    const NOOP: &[u8] = b"a1 NOOP\r\n";

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

    /// Serves `app` on a loopback port, for the WebSocket tests.
    async fn serve(app: axum::Router) -> SocketAddr {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        addr
    }

    async fn call(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: &[u8],
        peer: Option<&str>,
    ) -> (StatusCode, Vec<u8>) {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::from(body.to_vec()))
            .unwrap();
        if let Some(peer) = peer {
            req.extensions_mut()
                .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        }
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.headers()["access-control-allow-origin"], "*");
        let status = resp.status();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, body.to_vec())
    }

    async fn open(app: &axum::Router, port: u16, peer: Option<&str>) -> (StatusCode, String) {
        let (status, body) = call(app, "POST", &format!("/mailtunnel/{port}"), b"", peer).await;
        (status, String::from_utf8(body).unwrap())
    }

    /// Next message that is not a WebSocket ping or pong.
    async fn next_message<S>(ws: &mut S) -> Option<WsMessage>
    where
        S: futures_util::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        loop {
            match ws.next().await? {
                Ok(WsMessage::Ping(_) | WsMessage::Pong(_)) => {}
                Ok(m) => return Some(m),
                Err(_) => return None,
            }
        }
    }

    #[tokio::test]
    async fn detection_endpoint_and_off_by_default() {
        let (app, port) = setup(true).await;
        assert_eq!(
            call(&app, "GET", "/mailtunnel", b"", None).await,
            (StatusCode::OK, b"mailtunnel 1".to_vec())
        );
        let (off, _) = setup(false).await;
        for path in ["/mailtunnel".to_string(), format!("/mailtunnel/{port}")] {
            let (status, body) = call(&off, "GET", &path, b"", None).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(body, br#"{"error":"not found"}"#);
        }
    }

    #[tokio::test]
    async fn websocket_is_a_plain_binary_pipe() {
        let (app, port) = setup(true).await;
        let addr = serve(app).await;
        let (mut ws, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/mailtunnel/{port}"))
                .await
                .unwrap();
        ws.send(WsMessage::Binary(NOOP.to_vec().into()))
            .await
            .unwrap();
        assert_eq!(
            next_message(&mut ws).await,
            Some(WsMessage::Binary(NOOP.to_vec().into()))
        );
        // exactly the limit still tunnels (echoed back in 16 KiB chunks)…
        ws.send(WsMessage::Binary(vec![7u8; MAX_CHUNK].into()))
            .await
            .unwrap();
        let mut echoed = 0;
        while echoed < MAX_CHUNK {
            match next_message(&mut ws).await {
                Some(WsMessage::Binary(b)) => echoed += b.len(),
                other => panic!("unexpected {other:?}"),
            }
        }
        // …one byte more ends the tunnel
        let _ = ws
            .send(WsMessage::Binary(vec![0u8; MAX_CHUNK + 1].into()))
            .await;
        let end = timeout(Duration::from_secs(5), async {
            while let Some(m) = next_message(&mut ws).await {
                if matches!(m, WsMessage::Close(_)) {
                    return;
                }
            }
        })
        .await;
        assert!(end.is_ok(), "tunnel still open after a 64 KiB + 1 message");
    }

    #[tokio::test]
    async fn websocket_text_message_ends_the_tunnel() {
        let (app, port) = setup(true).await;
        let addr = serve(app).await;
        let (mut ws, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/mailtunnel/{port}"))
                .await
                .unwrap();
        ws.send(WsMessage::Text("a1 NOOP\r\n".into()))
            .await
            .unwrap();
        let end = timeout(Duration::from_secs(5), async {
            while let Some(m) = next_message(&mut ws).await {
                assert!(matches!(m, WsMessage::Close(_)), "{m:?}");
            }
        })
        .await;
        assert!(end.is_ok());
    }

    #[tokio::test]
    async fn non_listener_port_gets_no_tunnel() {
        let (app, _) = setup(true).await;
        assert_eq!(open(&app, 22, None).await.0, StatusCode::NOT_FOUND);
        let addr = serve(app).await;
        assert!(
            tokio_tungstenite::connect_async(format!("ws://{addr}/mailtunnel/22"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn long_polling_round_trip_and_close() {
        let (app, port) = setup(true).await;
        let (status, id) = open(&app, port, None).await;
        assert_eq!((status, id.len()), (StatusCode::OK, 32));
        let session = format!("/mailtunnel/poll/{id}");
        assert_eq!(
            call(&app, "POST", &session, NOOP, None).await.0,
            StatusCode::NO_CONTENT
        );
        let mut got = Vec::new();
        while got.len() < NOOP.len() {
            let (status, body) = call(&app, "GET", &session, b"", None).await;
            assert_eq!(status, StatusCode::OK);
            got.extend(body);
        }
        assert_eq!(got, NOOP);
        assert_eq!(
            call(&app, "POST", &format!("{session}/close"), b"", None)
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        let gone = timeout(Duration::from_secs(5), async {
            while call(&app, "GET", &session, b"", None).await.0 != StatusCode::NOT_FOUND {}
        })
        .await;
        assert!(gone.is_ok(), "session still answers after close");
    }

    #[tokio::test]
    async fn long_polling_rejects_oversized_bodies_and_unknown_sessions() {
        let (app, port) = setup(true).await;
        let (_, id) = open(&app, port, None).await;
        let session = format!("/mailtunnel/poll/{id}");
        let big = vec![0u8; MAX_CHUNK + 1];
        assert_eq!(
            call(&app, "POST", &session, &big, None).await.0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            call(&app, "GET", "/mailtunnel/poll/0123", b"", None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&app, "GET", "/mailtunnel/poll/not-hex", b"", None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn one_client_network_gets_a_bounded_share() {
        let (app, port) = setup(true).await;
        for i in 0..MAX_TUNNELS_PER_CLIENT {
            // one IPv6 /64, different addresses in it
            let peer = format!("[2001:db8:1:2::{:x}]:4000", i + 1);
            assert_eq!(open(&app, port, Some(&peer)).await.0, StatusCode::OK);
        }
        let over = open(&app, port, Some("[2001:db8:1:2::ffff]:4000")).await;
        assert_eq!(
            over.0,
            StatusCode::SERVICE_UNAVAILABLE,
            "a 17th from the same /64"
        );
        let other = open(&app, port, Some("198.51.100.7:4000")).await;
        assert_eq!(other.0, StatusCode::OK, "another client is unaffected");
    }

    #[tokio::test]
    async fn slots_cap_per_client_and_in_total() {
        let dir = tempfile::tempdir().unwrap();
        let tunnel = MailTunnel(Arc::new(Inner {
            app: Arc::new(AppState::new(dir.path(), init_memory_db().await.unwrap())),
            slots: Mutex::default(),
            polls: Mutex::default(),
        }));
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
    }

    #[test]
    fn client_networks_and_wildcard_dial() {
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
