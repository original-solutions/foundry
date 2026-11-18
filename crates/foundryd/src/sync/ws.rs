//! Peer links over WebSockets: the dialer and the route a peer dials.

use std::io;
use std::net::{IpAddr, SocketAddr};

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{self, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use foundry_proto::settings::Machine;
use futures::future::BoxFuture;
use futures::{Sink, SinkExt, Stream, StreamExt};
use tokio_tungstenite::tungstenite;

use super::{Conn, Dialer, Peers};

/// The path a peer's link is served on.
pub const PEER_PATH: &str = "/api/peer";

/// Dials a peer's [`router`] at its address in the machine list and `port`.
pub struct WsDialer {
    pub port: u16,
}

impl Dialer for WsDialer {
    fn dial(&self, peer: &Machine) -> BoxFuture<'static, io::Result<Conn>> {
        let url = url(&peer.address, self.port);
        Box::pin(async move {
            let (socket, _) = tokio_tungstenite::connect_async(url)
                .await
                .map_err(io::Error::other)?;
            Ok(pump(
                socket,
                |message| match message {
                    tungstenite::Message::Text(text) => Some(text.as_str().to_owned()),
                    _ => None,
                },
                tungstenite::Message::text,
            ))
        })
    }
}

/// The route that serves links peers open to this machine.
pub fn router(peers: Peers) -> Router {
    Router::new()
        .route(PEER_PATH, get(upgrade))
        .with_state(peers)
}

async fn upgrade(State(peers): State<Peers>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket: WebSocket| async move {
        let conn = pump(
            socket,
            |message| match message {
                ws::Message::Text(text) => Some(text.as_str().to_owned()),
                _ => None,
            },
            |text| ws::Message::Text(text.into()),
        );
        peers.accept(conn).await;
    })
}

/// The URL of a peer's link: `address` is an IP address or a host name.
fn url(address: &str, port: u16) -> String {
    match address.parse::<IpAddr>() {
        Ok(ip) => format!("ws://{}{PEER_PATH}", SocketAddr::new(ip, port)),
        Err(_) => format!("ws://{address}:{port}{PEER_PATH}"),
    }
}

/// Carries text frames between `socket` and the returned [`Conn`] until either closes.
fn pump<S, M, E>(socket: S, text: fn(M) -> Option<String>, frame: fn(String) -> M) -> Conn
where
    S: Stream<Item = Result<M, E>> + Sink<M> + Send + 'static,
    M: Send + 'static,
    E: Send + 'static,
{
    let (conn, Conn { send, mut recv }) = Conn::pair();
    let (mut sink, mut stream) = socket.split();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                out = recv.recv() => match out {
                    Some(out) => {
                        if sink.send(frame(out)).await.is_err() {
                            return;
                        }
                    }
                    None => {
                        let _ = sink.close().await;
                        return;
                    }
                },
                incoming = stream.next() => match incoming {
                    Some(Ok(message)) => {
                        if let Some(incoming) = text(message)
                            && send.send(incoming).await.is_err()
                        {
                            return;
                        }
                    }
                    _ => return,
                },
            }
        }
    });
    conn
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peer_url_brackets_an_ipv6_address() {
        assert_eq!(url("100.64.0.1", 7433), "ws://100.64.0.1:7433/api/peer");
        assert_eq!(
            url("fd7a:115c:a1e0::1", 7433),
            "ws://[fd7a:115c:a1e0::1]:7433/api/peer"
        );
        assert_eq!(url("trilogy-mac", 7433), "ws://trilogy-mac:7433/api/peer");
    }
}
