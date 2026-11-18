//! `foundry-holder --socket <path> --cwd <dir> --env-file <path> -- <argv>`.
//!
//! Reads and deletes the env file, then forks twice so the holder runs in a session of its own
//! with no controlling terminal, outside the process group of whatever started it. The holder
//! starts the child on a new PTY and serves the socket. The process that ran the command exits 0
//! once the socket is ready, or 1 with the reason on stderr.
//!
//! For as long as it runs, the holder keeps an exclusive lock on the file beside the socket with
//! the extension `lock`. The kernel lets go of it when the holder dies, however it dies, so a
//! socket that refuses connections while its lock is held belongs to a holder that is busy, not
//! gone.

mod server;

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{PipeWriter, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitCode};

use clap::Parser;
use foundry_holder::pty::{self, Pty};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use nix::sys::stat::{Mode, umask};
use nix::sys::wait::waitpid;
use nix::unistd::{ForkResult, dup2_stderr, dup2_stdin, dup2_stdout, fork, setsid};

/// What the holder writes to its starter once the socket is served.
const READY: &str = "ready";

/// The size a terminal starts at, until a client resizes it.
const COLS: u16 = 80;
const ROWS: u16 = 24;

#[derive(Parser)]
#[command(
    name = "foundry-holder",
    version,
    about = "Owns one PTY and its child, and serves it on a Unix socket."
)]
struct Args {
    /// The Unix socket to serve. It must not exist yet.
    #[arg(long)]
    socket: PathBuf,
    /// The child's working directory.
    #[arg(long)]
    cwd: PathBuf,
    /// A JSON object of variables set for the child over the holder's own. Deleted once read.
    #[arg(long)]
    env_file: PathBuf,
    /// The command to run.
    #[arg(last = true, required = true)]
    argv: Vec<OsString>,
}

fn main() -> ExitCode {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(e) => {
            // The env file may hold secrets, so it goes even when nothing else was understood.
            if let Some(path) = env_file_arg() {
                let _ = std::fs::remove_file(path);
            }
            e.exit()
        }
    };
    match start(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("foundry-holder: {reason}");
            ExitCode::FAILURE
        }
    }
}

/// The `--env-file` value from the raw arguments, for when they do not parse.
fn env_file_arg() -> Option<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--" {
            return None;
        }
        if arg == "--env-file" {
            return args.next().map(PathBuf::from);
        }
        if let Some(path) = arg.to_str().and_then(|a| a.strip_prefix("--env-file=")) {
            return Some(PathBuf::from(path));
        }
    }
    None
}

/// Runs in the starter's process. Returns once the detached holder serves the socket.
fn start(args: Args) -> Result<(), String> {
    let env = pty::read_env_file(&args.env_file)
        .map_err(|e| format!("env file {}: {e}", args.env_file.display()))?;
    let (mut ready_rx, ready_tx) = std::io::pipe().map_err(|e| format!("pipe: {e}"))?;
    // SAFETY: no thread has been started, so the child is a copy of a single-threaded process
    // and may run any code.
    match unsafe { fork() }.map_err(|e| format!("fork: {e}"))? {
        ForkResult::Parent { child } => {
            drop(ready_tx);
            waitpid(child, None).map_err(|e| format!("wait: {e}"))?;
            let mut said = String::new();
            let _ = ready_rx.read_to_string(&mut said);
            match said.as_str() {
                READY => Ok(()),
                "" => Err("the holder stopped before it was ready".into()),
                reason => Err(reason.into()),
            }
        }
        ForkResult::Child => {
            drop(ready_rx);
            std::process::exit(detach(args, env, ready_tx))
        }
    }
}

/// The first fork: a new session, then a second fork so the holder is not a session leader and
/// can never gain a controlling terminal.
fn detach(args: Args, env: Vec<(String, String)>, mut ready: PipeWriter) -> i32 {
    if let Err(e) = setsid() {
        let _ = write!(ready, "setsid: {e}");
        return 1;
    }
    // SAFETY: still single-threaded, as in `start`.
    match unsafe { fork() } {
        Ok(ForkResult::Parent { .. }) => 0,
        Ok(ForkResult::Child) => hold(args, env, ready),
        Err(e) => {
            let _ = write!(ready, "fork: {e}");
            1
        }
    }
}

/// The holder itself. The lock is kept until it returns.
fn hold(args: Args, env: Vec<(String, String)>, mut ready: PipeWriter) -> i32 {
    let lock_path = args.socket.with_extension("lock");
    let started = quiet_stdio().and_then(|()| {
        let lock = lock(&lock_path)?;
        match open(&args, &env) {
            Ok((pty, listener, child)) => Ok((lock, pty, listener, child)),
            Err(reason) => {
                // Removed while still held, so no holder that is running loses its file.
                let _ = std::fs::remove_file(&lock_path);
                Err(reason)
            }
        }
    });
    drop(env);
    match started {
        Ok((_lock, pty, listener, child)) => {
            let _ = ready.write_all(READY.as_bytes());
            drop(ready);
            match server::run(listener, &args.socket, &lock_path, pty, child) {
                Ok(()) => 0,
                Err(_) => 1,
            }
        }
        Err(reason) => {
            let _ = ready.write_all(reason.as_bytes());
            1
        }
    }
}

/// Opens the PTY, binds the socket and starts the child on the PTY.
fn open(args: &Args, env: &[(String, String)]) -> Result<(Pty, UnixListener, Child), String> {
    let pty = Pty::open(COLS, ROWS).map_err(|e| format!("pty: {e}"))?;
    let listener = bind(&args.socket)?;
    match pty.spawn(&args.argv, &args.cwd, env) {
        Ok(child) => Ok((pty, listener, child)),
        Err(e) => {
            let _ = std::fs::remove_file(&args.socket);
            Err(e.to_string())
        }
    }
}

/// Points stdio at /dev/null, so the holder keeps none of its starter's pipes or terminal open.
fn quiet_stdio() -> Result<(), String> {
    let null = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .map_err(|e| format!("/dev/null: {e}"))?;
    dup2_stdin(&null)
        .and_then(|()| dup2_stdout(&null))
        .and_then(|()| dup2_stderr(&null))
        .map_err(|e| format!("stdio: {e}"))
}

/// Takes the lock that says this holder runs, without waiting. The file is this user's only, and
/// the descriptor is closed on exec, so the child never holds the lock.
fn lock(path: &Path) -> Result<Flock<File>, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("lock {}: {e}", path.display()))?;
    Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, e)| match e {
        Errno::EWOULDBLOCK => format!("lock {}: another holder holds it", path.display()),
        e => format!("lock {}: {e}", path.display()),
    })
}

/// Binds the socket readable and writable by this user only: it gives full control of a shell.
fn bind(path: &Path) -> Result<UnixListener, String> {
    let old = umask(Mode::from_bits_truncate(0o177));
    let bound = UnixListener::bind(path);
    umask(old);
    bound.map_err(|e| format!("socket {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::ExitStatus;
    use std::time::{Duration, Instant};

    use nix::sys::signal::Signal;

    use super::*;

    fn args(dir: &Path) -> Args {
        Args {
            socket: dir.join("t.sock"),
            cwd: dir.to_path_buf(),
            env_file: dir.join("env.json"),
            argv: vec!["sleep".into(), "30".into()],
        }
    }

    fn exit_within(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().expect("try_wait") {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    #[test]
    fn a_starter_that_is_gone_leaves_nothing_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = args(dir.path());
        let lock_path = args.socket.with_extension("lock");
        let _lock = lock(&lock_path).expect("lock");
        let (_pty, _listener, mut child) = open(&args, &[]).expect("open");
        let (reader, writer) = std::io::pipe().expect("pipe");
        drop(reader);

        let serve = tell_ready(writer, &args.socket, &lock_path, &child);

        let status = exit_within(&mut child, Duration::from_secs(5));
        if status.is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(!serve, "the holder serves though its starter never heard it");
        assert_eq!(
            status.and_then(|s| s.signal()),
            Some(Signal::SIGHUP as i32),
            "the child is not hung up"
        );
        assert!(!args.socket.exists(), "the socket is left behind");
        assert!(!lock_path.exists(), "the lock file is left behind");
    }

    #[test]
    fn a_starter_that_listens_hears_ready_and_the_child_runs_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = args(dir.path());
        let lock_path = args.socket.with_extension("lock");
        let _lock = lock(&lock_path).expect("lock");
        let (_pty, _listener, mut child) = open(&args, &[]).expect("open");
        let (mut reader, writer) = std::io::pipe().expect("pipe");

        let serve = tell_ready(writer, &args.socket, &lock_path, &child);

        let mut said = String::new();
        let read = reader.read_to_string(&mut said);
        let running = child.try_wait().expect("try_wait").is_none();
        let _ = child.kill();
        let _ = child.wait();
        assert!(serve, "the holder does not serve though its starter heard it");
        assert!(read.is_ok(), "the ready pipe is not closed after the message");
        assert_eq!(said, READY);
        assert!(running, "the child is ended though the starter heard it");
        assert!(args.socket.exists(), "the socket is removed");
        assert!(lock_path.exists(), "the lock file is removed");
    }
}
