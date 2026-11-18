//! The holder's loop: read the PTY into the scrollback, serve the socket, record the exit.
//!
//! One task owns all state. It never waits on a client: output goes to each attached client
//! through a bounded queue, and a client whose queue is full is disconnected. A child is therefore
//! never blocked by a slow reader, and memory stays bounded with or without clients.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::Path;
use std::process::Child;
use std::time::Duration;

use bytes::Bytes;
use foundry_holder::protocol::{self, ChildStatus, Reply, Request};
use foundry_holder::pty::{self, Pty};
use foundry_holder::scrollback::{MAX_BYTES, Scrollback};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::unix::AsyncFd;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

const READ_CHUNK: usize = 64 * 1024;

/// Replies queued for one client before it counts as too slow and is disconnected. It can attach
/// again and gets the snapshot.
const CLIENT_QUEUE: usize = 64;

/// Typed input held while the child is not reading it. Input over this is dropped.
const MAX_PENDING_INPUT: usize = 1024 * 1024;

/// Serves until a client sends `Close`, then removes the socket.
pub fn run(listener: StdUnixListener, socket: &Path, pty: Pty, child: Child) -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(serve(listener, pty, child));
    let _ = std::fs::remove_file(socket);
    result
}

enum Event {
    Request(u64, Request),
    Gone(u64),
}

struct Client {
    replies: mpsc::Sender<Reply>,
    attached: bool,
}

#[derive(Default)]
struct Terminal {
    scrollback: Scrollback,
    clients: HashMap<u64, Client>,
    input: Vec<u8>,
    exit: Option<i32>,
}

async fn serve(listener: StdUnixListener, pty: Pty, mut child: Child) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let listener = UnixListener::from_std(listener)?;
    let group = Pid::from_raw(i32::try_from(child.id()).map_err(io::Error::other)?);
    let (master, _slave) = pty.into_master()?;
    // SAFETY: the `File` owns the master descriptor and the `AsyncFd` owns the `File`, so the
    // descriptor stays open and the same for as long as it is registered.
    let master = unsafe { AsyncFd::register(master) }?;

    let (exit_tx, mut exit_rx) = oneshot::channel();
    std::thread::spawn(move || {
        let code = child.wait().map_or(-1, pty::exit_code);
        let _ = exit_tx.send(code);
    });

    let (events_tx, mut events) = mpsc::channel(256);
    let mut term = Terminal::default();
    let mut next_id = 0u64;
    let mut buf = vec![0u8; READ_CHUNK];
    let mut reading = true;
    loop {
        tokio::select! {
            ready = master.readable(), if reading => {
                let mut guard = ready?;
                match guard.try_io(|fd| read(fd.get_ref(), &mut buf)) {
                    Ok(Ok(n)) if n > 0 => term.output(&buf[..n]),
                    // The holder keeps the slave open, so this is not expected. Stop reading
                    // rather than spin on a master that is always readable.
                    Ok(_) => reading = false,
                    Err(_would_block) => {}
                }
            }
            ready = master.writable(), if !term.input.is_empty() => {
                let mut guard = ready?;
                match guard.try_io(|fd| write(fd.get_ref(), &term.input)) {
                    Ok(Ok(n)) => {
                        term.input.drain(..n);
                    }
                    Ok(Err(_)) => term.input.clear(),
                    Err(_would_block) => {}
                }
            }
            code = &mut exit_rx, if term.exit.is_none() => {
                drain(master.get_ref(), &mut buf, &mut term);
                term.exited(code.unwrap_or(-1));
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    next_id += 1;
                    term.connect(next_id, stream, events_tx.clone());
                }
                // Out of descriptors or similar: back off instead of spinning.
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            },
            Some(event) = events.recv() => match event {
                Event::Request(_, Request::Close) => {
                    if term.exit.is_none() {
                        let _ = killpg(group, Signal::SIGHUP);
                    }
                    return Ok(());
                }
                Event::Request(id, request) => term.handle(id, request, master.get_ref()),
                Event::Gone(id) => {
                    term.clients.remove(&id);
                }
            },
        }
    }
}

impl Terminal {
    fn connect(&mut self, id: u64, stream: UnixStream, events: mpsc::Sender<Event>) {
        let (mut rd, mut wr) = stream.into_split();
        let (replies, mut queue) = mpsc::channel::<Reply>(CLIENT_QUEUE);
        self.clients.insert(
            id,
            Client {
                replies,
                attached: false,
            },
        );
        // Ends when the client is dropped from `clients` or stops reading; dropping the write
        // half shuts the connection down.
        tokio::spawn(async move {
            while let Some(reply) = queue.recv().await {
                if protocol::write_reply(&mut wr, &reply).await.is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            while let Ok(Some(request)) = protocol::read_request(&mut rd).await {
                if events.send(Event::Request(id, request)).await.is_err() {
                    return;
                }
            }
            let _ = events.send(Event::Gone(id)).await;
        });
    }

    fn handle(&mut self, id: u64, request: Request, master: &File) {
        match request {
            Request::Attach => {
                if let Some(client) = self.clients.get_mut(&id) {
                    client.attached = true;
                }
                self.send(id, Reply::Snapshot(Bytes::from(self.scrollback.snapshot())));
                if let Some(code) = self.exit {
                    self.send(id, Reply::Status(ChildStatus::Exited(code)));
                }
            }
            Request::Input(bytes) => {
                if self.exit.is_none() && self.input.len() + bytes.len() <= MAX_PENDING_INPUT {
                    self.input.extend_from_slice(&bytes);
                }
            }
            Request::Resize { cols, rows } => {
                if cols > 0 && rows > 0 {
                    let _ = pty::set_size(master.as_fd(), cols, rows);
                }
            }
            Request::Status => {
                let status = match self.exit {
                    Some(code) => ChildStatus::Exited(code),
                    None => ChildStatus::Running,
                };
                self.send(id, Reply::Status(status));
            }
            Request::Close => {}
        }
    }

    fn output(&mut self, data: &[u8]) {
        self.scrollback.push(data);
        if self.clients.values().any(|c| c.attached) {
            let bytes = Bytes::copy_from_slice(data);
            self.broadcast(|| Reply::Output(bytes.clone()));
        }
    }

    fn exited(&mut self, code: i32) {
        self.exit = Some(code);
        self.input.clear();
        self.broadcast(|| Reply::Status(ChildStatus::Exited(code)));
    }

    /// Sends to every attached client and drops each one that cannot take it now.
    fn broadcast(&mut self, reply: impl Fn() -> Reply) {
        self.clients
            .retain(|_, c| !c.attached || c.replies.try_send(reply()).is_ok());
    }

    fn send(&mut self, id: u64, reply: Reply) {
        if let Some(client) = self.clients.get(&id)
            && client.replies.try_send(reply).is_err()
        {
            self.clients.remove(&id);
        }
    }
}

/// Reads what the child wrote before it exited, so `Status` never reports an exit ahead of the
/// output that came before it. Capped, in case something the child left behind keeps writing.
fn drain(master: &File, buf: &mut [u8], term: &mut Terminal) {
    let mut total = 0;
    while total < MAX_BYTES {
        match read(master, buf) {
            Ok(n) if n > 0 => {
                term.output(&buf[..n]);
                total += n;
            }
            _ => break,
        }
    }
}

fn read(mut file: &File, buf: &mut [u8]) -> io::Result<usize> {
    file.read(buf)
}

fn write(mut file: &File, data: &[u8]) -> io::Result<usize> {
    file.write(data)
}
