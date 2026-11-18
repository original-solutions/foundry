//! The holder's loop: read the PTY into the scrollback, serve the socket, record the exit.
//!
//! One task owns all state. It never waits on a client: each frame is encoded once and appended to
//! every attached client's outbox, which that client's writer takes whole, and a client with more
//! than [`MAX_BACKLOG`] bytes unsent, on top of its last snapshot, is cut off in both directions
//! and its backlog freed.
//! A child is therefore never blocked by a slow reader, and memory stays bounded with or without
//! clients.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::Path;
use std::process::Child;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use foundry_holder::protocol::{
    self, ChildStatus, MAX_PENDING_INPUT, Reply, Request, UNKNOWN_EXIT,
};
use foundry_holder::pty::{self, Pty};
use foundry_holder::scrollback::{MAX_BYTES, Scrollback};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::AsyncWriteExt as _;
use tokio::io::unix::AsyncFd;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::AbortHandle;

const READ_CHUNK: usize = 64 * 1024;

/// Output one client may have unsent, on top of the size of the last snapshot it was sent, before it
/// counts as too slow and is disconnected. It can attach again and gets the snapshot. Small, so each
/// stalled connection costs the holder little.
const MAX_BACKLOG: usize = 4 * 1024 * 1024;

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
    /// The client sent a frame the holder cannot read.
    Unreadable(u64, String),
    Gone(u64),
}

struct Client {
    outbox: Arc<Outbox>,
    reader: AbortHandle,
    /// `None` once the writer is left to finish on its own.
    writer: Option<AbortHandle>,
    attached: bool,
    /// Bytes it may have unsent: [`MAX_BACKLOG`], plus its snapshot once attached.
    allowance: usize,
}

impl Client {
    /// Queues an encoded frame. False when the client is too far behind.
    fn queue(&self, frame: &[u8]) -> bool {
        let mut state = self.outbox.lock();
        if state.unsent.len() + state.writing + frame.len() > self.allowance {
            return false;
        }
        state.unsent.extend_from_slice(frame);
        self.outbox.ready.notify_one();
        true
    }

    /// Stops reading the client and gives its writer [`protocol::CLOSE_GRACE`] to send what is
    /// queued, with `last` in place of it when given, then close the connection.
    fn finish(mut self, last: Option<Reply>) {
        let mut state = self.outbox.lock();
        if let Some(reply) = last {
            state.unsent = reply.encode();
        }
        state.closed = true;
        drop(state);
        self.outbox.ready.notify_one();
        // A client that does not read could hold the writer, and what it has queued, for ever.
        if let Some(writer) = self.writer.take() {
            tokio::spawn(async move {
                tokio::time::sleep(protocol::CLOSE_GRACE).await;
                writer.abort();
            });
        }
    }
}

impl Drop for Client {
    /// Cuts the connection off in both directions and frees what was queued for it, unless
    /// `finish` gave the writer its grace period.
    fn drop(&mut self) {
        self.reader.abort();
        if let Some(writer) = &self.writer {
            writer.abort();
        }
    }
}

/// Frames waiting for one client's writer, as one buffer, so memory is what is queued and a fast
/// client gets many frames per write.
#[derive(Default)]
struct Outbox {
    state: Mutex<OutboxState>,
    ready: Notify,
}

#[derive(Default)]
struct OutboxState {
    unsent: Vec<u8>,
    /// Bytes the writer has taken and not finished writing.
    writing: usize,
    closed: bool,
}

impl Outbox {
    fn lock(&self) -> MutexGuard<'_, OutboxState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes until `finish` has closed the outbox and everything queued is sent, or a write fails,
    /// or the task is aborted. Dropping the write half
    /// at the end shuts the connection down.
    async fn write_to(&self, mut wr: OwnedWriteHalf) {
        loop {
            let (chunk, closed) = {
                let mut state = self.lock();
                let chunk = std::mem::take(&mut state.unsent);
                state.writing = chunk.len();
                (chunk, state.closed)
            };
            if !chunk.is_empty() {
                if wr.write_all(&chunk).await.is_err() {
                    return;
                }
                self.lock().writing = 0;
            } else if closed {
                return;
            } else {
                self.ready.notified().await;
            }
        }
    }
}

#[derive(PartialEq)]
enum Flow {
    Continue,
    Close,
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
        let code = child.wait().map_or(UNKNOWN_EXIT, pty::exit_code);
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
                term.exited(code.unwrap_or(UNKNOWN_EXIT));
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
                // A client already dropped (too slow, or refused) is not heard any more.
                Event::Request(id, _) | Event::Unreadable(id, _)
                    if !term.clients.contains_key(&id) => {}
                Event::Request(id, request) => {
                    if term.handle(id, request, master.get_ref()) == Flow::Close {
                        if term.exit.is_none() {
                            let _ = killpg(group, Signal::SIGHUP);
                        }
                        return Ok(());
                    }
                }
                Event::Unreadable(id, reason) => term.refuse(id, reason),
                // The client stopped sending; it may still read what is queued for it.
                Event::Gone(id) => {
                    if let Some(client) = term.clients.remove(&id) {
                        client.finish(None);
                    }
                }
            },
        }
    }
}

impl Terminal {
    fn connect(&mut self, id: u64, stream: UnixStream, events: mpsc::Sender<Event>) {
        let (mut rd, wr) = stream.into_split();
        let outbox = Arc::new(Outbox::default());
        let writer = tokio::spawn({
            let outbox = outbox.clone();
            async move { outbox.write_to(wr).await }
        });
        let reader = tokio::spawn(async move {
            loop {
                let event = match protocol::read_request(&mut rd).await {
                    Ok(Some(request)) => Event::Request(id, request),
                    Ok(None) => Event::Gone(id),
                    Err(e) => Event::Unreadable(id, e.to_string()),
                };
                let last = !matches!(event, Event::Request(..));
                if events.send(event).await.is_err() || last {
                    return;
                }
            }
        });
        self.clients.insert(
            id,
            Client {
                outbox,
                reader: reader.abort_handle(),
                writer: Some(writer.abort_handle()),
                attached: false,
                allowance: MAX_BACKLOG,
            },
        );
    }

    fn handle(&mut self, id: u64, request: Request, master: &File) -> Flow {
        match request {
            Request::Attach => {
                let snapshot = Reply::Snapshot {
                    start: self.scrollback.start(),
                    bytes: Bytes::from(self.scrollback.snapshot()),
                };
                let frame = snapshot.encode();
                if let Some(client) = self.clients.get_mut(&id) {
                    client.attached = true;
                    client.allowance = MAX_BACKLOG + frame.len();
                }
                self.send_frame(id, &frame);
                if let Some(code) = self.exit {
                    self.send(id, Reply::Status(ChildStatus::Exited(code)));
                }
            }
            Request::Input(_) if self.exit.is_some() => {}
            Request::Input(bytes) if self.input.len() + bytes.len() > MAX_PENDING_INPUT => {
                self.refuse(
                    id,
                    format!("input refused: over {MAX_PENDING_INPUT} bytes the child has not read"),
                );
            }
            Request::Input(bytes) => self.input.extend_from_slice(&bytes),
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
            Request::Close => return Flow::Close,
        }
        Flow::Continue
    }

    /// Tells the client why in place of anything still queued for it, and closes the connection.
    fn refuse(&mut self, id: u64, reason: String) {
        if let Some(client) = self.clients.remove(&id) {
            client.finish(Some(Reply::Refused(reason)));
        }
    }

    fn output(&mut self, data: &[u8]) {
        self.scrollback.push(data);
        if self.clients.values().any(|c| c.attached) {
            self.broadcast(&Reply::Output(Bytes::copy_from_slice(data)));
        }
    }

    fn exited(&mut self, code: i32) {
        self.exit = Some(code);
        self.input.clear();
        self.broadcast(&Reply::Status(ChildStatus::Exited(code)));
    }

    /// Queues for every attached client and drops each one that is too far behind.
    fn broadcast(&mut self, reply: &Reply) {
        let frame = reply.encode();
        self.clients.retain(|_, c| !c.attached || c.queue(&frame));
    }

    fn send(&mut self, id: u64, reply: Reply) {
        self.send_frame(id, &reply.encode());
    }

    fn send_frame(&mut self, id: u64, frame: &[u8]) {
        if let Some(client) = self.clients.get(&id)
            && !client.queue(frame)
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
