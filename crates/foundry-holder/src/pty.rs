//! The PTY and the detached child it runs, and the env file that carries the child's variables.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};

use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::libc;
use nix::pty::{Winsize, openpty};

/// A PTY pair. The holder keeps the slave open as long as it runs, so reading the master never
/// ends early because a child closed its side.
#[derive(Debug)]
pub struct Pty {
    master: OwnedFd,
    slave: OwnedFd,
}

impl Pty {
    /// Opens a PTY of the given size. Neither end leaks into the child beyond its stdio.
    pub fn open(cols: u16, rows: u16) -> io::Result<Self> {
        let pair = openpty(&winsize(cols, rows), None).map_err(io::Error::from)?;
        for fd in [&pair.master, &pair.slave] {
            fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(io::Error::from)?;
        }
        Ok(Pty {
            master: pair.master,
            slave: pair.slave,
        })
    }

    /// Starts `argv` on the slave as the leader of a new session with the PTY as its controlling
    /// terminal. The child inherits this process's environment with `env` set over it.
    pub fn spawn(
        &self,
        argv: &[OsString],
        cwd: &Path,
        env: &[(String, String)],
    ) -> io::Result<Child> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no command to run"))?;
        if !cwd.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("cwd {} is not a directory", cwd.display()),
            ));
        }
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::from(self.slave.try_clone()?))
            .stdout(Stdio::from(self.slave.try_clone()?))
            .stderr(Stdio::from(self.slave.try_clone()?));
        // SAFETY: the hook runs in the forked child before exec, and `take_terminal` makes only
        // the async-signal-safe `setsid` and `ioctl` calls.
        unsafe {
            command.pre_exec(take_terminal);
        }
        command.spawn().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("cannot start {}: {e}", program.to_string_lossy()),
            )
        })
    }

    /// The master, as a non-blocking file the holder reads output from and writes input to.
    /// The slave stays open with the returned guard.
    pub fn into_master(self) -> io::Result<(File, OwnedFd)> {
        let flags = fcntl(&self.master, FcntlArg::F_GETFL).map_err(io::Error::from)?;
        let flags = OFlag::from_bits_retain(flags) | OFlag::O_NONBLOCK;
        fcntl(&self.master, FcntlArg::F_SETFL(flags)).map_err(io::Error::from)?;
        Ok((File::from(self.master), self.slave))
    }
}

/// Sets the terminal size on the master. The kernel sends the foreground process `SIGWINCH`.
pub fn set_size(master: BorrowedFd<'_>, cols: u16, rows: u16) -> io::Result<()> {
    let size = winsize(cols, rows);
    // SAFETY: TIOCSWINSZ reads one `winsize` through the pointer, which points at a live local.
    let rc = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The code `Status` reports: the exit code, or 128 plus the signal that ended the child.
pub fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// Reads the env file, deletes it, then parses it: a JSON object of variable names to values.
/// The file is deleted even when it does not parse, since it may hold secrets.
pub fn read_env_file(path: &Path) -> io::Result<Vec<(String, String)>> {
    let text = std::fs::read(path)?;
    std::fs::remove_file(path)?;
    // The parser's message can quote a value from the file, so only its position is reported.
    let vars: BTreeMap<String, String> = serde_json::from_slice(&text).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "not a JSON object of strings (line {}, column {})",
                e.line(),
                e.column()
            ),
        )
    })?;
    Ok(vars.into_iter().collect())
}

/// Writes an env file the holder can read: new, readable only by this user.
pub fn write_env_file(path: &Path, vars: &[(String, String)]) -> io::Result<()> {
    let vars: BTreeMap<&str, &str> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&serde_json::to_vec(&vars).map_err(io::Error::other)?)?;
    file.sync_all()
}

fn winsize(cols: u16, rows: u16) -> Winsize {
    Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// In the child: leave the holder's session and take the PTY on stdin as controlling terminal.
fn take_terminal() -> io::Result<()> {
    nix::unistd::setsid().map_err(io::Error::from)?;
    // SAFETY: TIOCSCTTY on stdin, which `spawn` made the slave, takes an integer argument and
    // touches no memory of ours.
    let rc = unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
