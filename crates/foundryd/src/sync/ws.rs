//! Peer links over WebSockets: the dialer and the routes a peer dials. The daemon serves
//! [`keyed_router`]; [`router`] proves nothing and is for tests.

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
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite};

use super::{Conn, Dialer, Peers};
use crate::join::Keys;

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
            let socket = open(url).await.map_err(io::Error::other)?;
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

/// A WebSocket to `url`, with Nagle's algorithm off, as on the connections the API accepts, so
/// a key typed into another machine's terminal, and its echo, cross the link at once.
async fn open(
    url: String,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, tungstenite::Error> {
    let (socket, _) = tokio_tungstenite::connect_async_with_config(url, None, true).await?;
    Ok(socket)
}

/// The route that serves links peers open to this machine. A link names its machine in its
/// first message and nothing proves it, and a link that opens with a relay gets this machine's
/// terminals: shell access. The daemon mounts [`keyed_router`] instead; this one is for tests
/// only.
pub fn router(peers: Peers) -> Router {
    Router::new()
        .route(PEER_PATH, get(upgrade))
        .with_state(peers)
}

async fn upgrade(State(peers): State<Peers>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| async move { peers.accept(served(socket)).await })
}

/// The route that serves links peers open to this machine, each taken only once both ends have
/// proved their keys ([`Keys::accept`]). This is the one the daemon mounts.
pub fn keyed_router(peers: Peers, keys: Keys) -> Router {
    Router::new()
        .route(PEER_PATH, get(keyed_upgrade))
        .with_state((peers, keys))
}

async fn keyed_upgrade(
    State((peers, keys)): State<(Peers, Keys)>,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade.on_upgrade(move |socket| async move { keys.accept(&peers, served(socket)).await })
}

/// The link on a socket a peer opened to this machine.
fn served(socket: WebSocket) -> Conn {
    pump(
        socket,
        |message| match message {
            ws::Message::Text(text) => Some(text.as_str().to_owned()),
            _ => None,
        },
        |text| ws::Message::Text(text.into()),
    )
}

/// The URL of a peer's link: `address` is an IP address or a host name.
fn url(address: &str, port: u16) -> String {
    match address.parse::<IpAddr>() {
        Ok(ip) => format!("ws://{}{PEER_PATH}", SocketAddr::new(ip, port)),
        Err(_) => format!("ws://{address}:{port}{PEER_PATH}"),
    }
}

/// Carries text frames between `socket` and the returned [`Conn`] until either closes. The
/// two directions run side by side, so a frame that waits on a slow peer never stops this end
/// reading it, and the socket is dropped as soon as the link closes, even mid-send.
fn pump<S, M, E>(socket: S, text: fn(M) -> Option<String>, frame: fn(String) -> M) -> Conn
where
    S: Stream<Item = Result<M, E>> + Sink<M> + Send + 'static,
    M: Send + 'static,
    E: Send + 'static,
{
    let (conn, Conn { send, mut recv }) = Conn::pair();
    let (mut sink, mut stream) = socket.split();
    tokio::spawn(async move {
        let outgoing = async {
            while let Some(out) = recv.recv().await {
                if sink.send(frame(out)).await.is_err() {
                    return;
                }
            }
            let _ = sink.close().await;
        };
        let incoming = async {
            while let Some(Ok(message)) = stream.next().await {
                if let Some(incoming) = text(message)
                    && send.send(incoming).await.is_err()
                {
                    return;
                }
            }
        };
        tokio::select! {
            () = outgoing => {}
            () = incoming => {}
            () = send.closed() => {}
        }
    });
    conn
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use futures::channel::mpsc as socket;

    use super::*;

    /// A socket the test drives: what the peer sends arrives on `incoming`, and what this end
    /// sends goes to `outgoing`, which holds one frame until the test reads it.
    struct Socket {
        incoming: socket::UnboundedReceiver<Result<String, ()>>,
        outgoing: socket::Sender<String>,
    }

    impl Stream for Socket {
        type Item = Result<String, ()>;
        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Pin::new(&mut self.get_mut().incoming).poll_next(cx)
        }
    }

    impl Sink<String> for Socket {
        type Error = socket::SendError;
        fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Pin::new(&mut self.get_mut().outgoing).poll_ready(cx)
        }
        fn start_send(self: Pin<&mut Self>, item: String) -> Result<(), Self::Error> {
            Pin::new(&mut self.get_mut().outgoing).start_send(item)
        }
        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Pin::new(&mut self.get_mut().outgoing).poll_flush(cx)
        }
        fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Pin::new(&mut self.get_mut().outgoing).poll_close(cx)
        }
    }

    /// A pump over a socket whose sends never drain, with the frames this end sent kept so
    /// they are never read.
    fn stuck_pump() -> (
        Conn,
        socket::UnboundedSender<Result<String, ()>>,
        socket::Receiver<String>,
    ) {
        let (peer, incoming) = socket::unbounded();
        let (outgoing, sent) = socket::channel(0);
        let conn = pump(Socket { incoming, outgoing }, Some, |text| text);
        (conn, peer, sent)
    }

    #[tokio::test(start_paused = true)]
    async fn the_pump_reads_the_peer_while_a_send_waits() {
        let (mut conn, peer, _sent) = stuck_pump();
        for i in 0..3 {
            conn.send.send(format!("out {i}")).await.unwrap();
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        peer.unbounded_send(Ok("in".into())).unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), conn.recv.recv()).await;
        assert_eq!(got, Ok(Some("in".to_owned())));
    }

    #[tokio::test(start_paused = true)]
    async fn the_pump_drops_its_socket_once_the_link_closes_while_a_send_waits() {
        let (conn, peer, _sent) = stuck_pump();
        for i in 0..3 {
            conn.send.send(format!("out {i}")).await.unwrap();
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        drop(conn);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(peer.is_closed(), "the socket is still held");
    }

    /// A key typed into another machine's terminal crosses the link as a few bytes, which with
    /// Nagle's algorithm on wait for the acknowledgement of the last write before they go.
    #[tokio::test]
    async fn a_dialed_link_sends_small_writes_at_once() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio_tungstenite::accept_async(stream).await.unwrap()
        });

        let socket = open(url("127.0.0.1", port)).await.unwrap();

        let MaybeTlsStream::Plain(tcp) = socket.get_ref() else {
            panic!("the link is not a plain TCP stream");
        };
        assert!(tcp.nodelay().unwrap(), "Nagle's algorithm is on");
        drop(served.await);
    }

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
