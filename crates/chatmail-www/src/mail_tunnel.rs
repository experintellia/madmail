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

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::body::Bytes;
use chatmail_state::AppState;
use engineioxide::config::EngineIoConfig;
use engineioxide::handler::EngineIoHandler;
use engineioxide::service::EngineIoService;
use engineioxide::{DisconnectReason, Socket, Str};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{self, error::TrySendError};

/// Bytes queued client→server before the tunnel gives up on a stalled listener.
const UP_QUEUE: usize = 256;

pub struct MailTunnel {
    app: Arc<AppState>,
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
    // 1 MB per polling request instead of 100 kB: attachments go up through
    // here, one round trip per payload, when WebSocket is blocked.
    let config = EngineIoConfig::builder().max_payload(1 << 20).build();
    EngineIoService::with_config(Arc::new(MailTunnel { app }), config)
}

impl MailTunnel {
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
        let (tx, rx) = mpsc::channel(UP_QUEUE);
        let _ = socket.data.up.set(tx);
        tokio::spawn(pump(socket, addr, rx));
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

async fn pump(socket: Arc<Socket<Conn>>, addr: SocketAddr, mut up: mpsc::Receiver<Bytes>) {
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

    let upstream = async {
        while let Some(chunk) = up.recv().await {
            wr.write_all(&chunk).await?;
        }
        std::io::Result::Ok(())
    };
    let downstream = async {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = rd.read(&mut buf).await?;
            if n == 0 {
                return std::io::Result::Ok(());
            }
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

    tokio::select! {
        _ = upstream => {}
        _ = downstream => {}
        _ = socket.closed() => {}
    }
    socket.close(DisconnectReason::TransportClose);
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use chatmail_db::init_memory_db;
    use tower::ServiceExt;

    async fn poll(app: &axum::Router, method: &str, query: &str, body: &str) -> String {
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
        String::from_utf8(
            to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    /// Long-polling round trip through the tunnel to a local echo "IMAP" listener.
    #[tokio::test]
    async fn polling_tunnel_echoes_through_local_listener() {
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = echo.accept().await.unwrap();
            let (mut r, mut w) = s.split();
            tokio::io::copy(&mut r, &mut w).await.unwrap();
        });

        let pool = init_memory_db().await.unwrap();
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
            dir.path(),
        ));

        let open = poll(&app, "GET", &format!("port={port}"), "").await;
        let sid = open
            .split("\"sid\":\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        // "a1 NOOP\r\n" as an engine.io v4 base64 binary packet.
        assert_eq!(
            poll(&app, "POST", &format!("sid={sid}"), "bYTEgTk9PUA0K").await,
            "ok"
        );
        assert_eq!(
            poll(&app, "GET", &format!("sid={sid}"), "").await,
            "bYTEgTk9PUA0K"
        );

        // A port that is not one of our mail listeners: the session is closed at once.
        let open = poll(&app, "GET", "port=22", "").await;
        let sid = open
            .split("\"sid\":\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let gone = poll(&app, "GET", &format!("sid={sid}"), "").await;
        assert!(gone.contains("Session ID unknown"), "{gone}");
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
