//! What the factory's command runners (`gh`, the lease's `git` and `vims`) share about the
//! child they start: killing its whole process group, and reading a pipe to its end with a
//! bound on what is kept.

use std::io::Read;
use std::process::Child;
use std::thread::{self, JoinHandle};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

/// Kills the child's process group and reaps the child. The leader may already be reaped when
/// a descendant still holds a pipe; its id still names the group while any member is alive,
/// which an unfinished reader means unless the holder left the group.
pub fn kill_group(child: &mut Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Reads `pipe` to its end on a thread of its own, keeping at most `limit` bytes, and says
/// whether there was more.
pub fn drain<R: Read + Send + 'static>(
    pipe: Option<R>,
    limit: usize,
) -> JoinHandle<(Vec<u8>, bool)> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut over = false;
        let Some(mut pipe) = pipe else {
            return (kept, over);
        };
        let mut buf = [0u8; 64 * 1024];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let room = limit.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                    over |= n > room;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        (kept, over)
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use nix::sys::signal::kill;

    use super::*;

    /// A reader of `left` bytes of `x` that counts what it has handed out.
    struct Counted {
        left: usize,
        read: Arc<AtomicUsize>,
    }

    impl Read for Counted {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.left).min(1000);
            buf[..n].fill(b'x');
            self.left -= n;
            self.read.fetch_add(n, Ordering::SeqCst);
            Ok(n)
        }
    }

    /// A reader that answers each read with the next of `steps`, then with the end of the pipe.
    struct Scripted {
        steps: std::collections::VecDeque<std::io::Result<&'static [u8]>>,
    }

    impl Scripted {
        fn new(steps: Vec<std::io::Result<&'static [u8]>>) -> Self {
            Self {
                steps: steps.into(),
            }
        }
    }

    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.steps.pop_front() {
                None => Ok(0),
                Some(Ok(bytes)) => {
                    buf[..bytes.len()].copy_from_slice(bytes);
                    Ok(bytes.len())
                }
                Some(Err(error)) => Err(error),
            }
        }
    }

    #[test]
    fn a_drain_reads_on_after_an_interrupted_read() {
        let pipe = Scripted::new(vec![
            Ok(b"ab"),
            Err(std::io::ErrorKind::Interrupted.into()),
            Ok(b"cd"),
        ]);
        assert_eq!(
            drain(Some(pipe), 4).join().unwrap(),
            (b"abcd".to_vec(), false)
        );
    }

    #[test]
    fn a_drain_ends_at_a_read_that_fails_and_keeps_what_came_before() {
        let pipe = Scripted::new(vec![
            Ok(b"ab"),
            Err(std::io::ErrorKind::BrokenPipe.into()),
            Ok(b"cd"),
        ]);
        assert_eq!(
            drain(Some(pipe), 4).join().unwrap(),
            (b"ab".to_vec(), false)
        );
    }

    #[test]
    fn a_drain_keeps_everything_up_to_the_limit_and_reports_no_overflow() {
        let (kept, over) = drain(Some(Cursor::new(b"abcd".to_vec())), 4)
            .join()
            .unwrap();
        assert_eq!(kept, b"abcd");
        assert!(!over);

        let (kept, over) = drain(Some(Cursor::new(b"ab".to_vec())), 4).join().unwrap();
        assert_eq!(kept, b"ab");
        assert!(!over);
    }

    #[test]
    fn a_drain_keeps_at_most_the_limit_and_reports_that_there_was_more() {
        let (kept, over) = drain(Some(Cursor::new(b"abcdefghij".to_vec())), 4)
            .join()
            .unwrap();
        assert_eq!(kept, b"abcd");
        assert!(over);
    }

    #[test]
    fn a_drain_reads_the_pipe_to_its_end_past_the_limit_over_many_reads() {
        let read = Arc::new(AtomicUsize::new(0));
        let pipe = Counted {
            left: 200_000,
            read: read.clone(),
        };
        let (kept, over) = drain(Some(pipe), 150_001).join().unwrap();
        assert_eq!(kept.len(), 150_001);
        assert!(kept.iter().all(|b| *b == b'x'));
        assert!(over);
        assert_eq!(
            read.load(Ordering::SeqCst),
            200_000,
            "the pipe was left unread"
        );
    }

    #[test]
    fn a_drain_of_no_pipe_keeps_nothing() {
        let (kept, over) = drain(None::<Cursor<Vec<u8>>>, 4).join().unwrap();
        assert!(kept.is_empty());
        assert!(!over);
    }

    /// The pid the script wrote to `file`, once it has.
    fn pid_in(file: &Path) -> i32 {
        let begun = Instant::now();
        while begun.elapsed() < Duration::from_secs(10) {
            if let Some(pid) = fs::read_to_string(file)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                return pid;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("no pid in {}", file.display());
    }

    /// Whether `pid` answers no signal within 5 seconds.
    fn goes_away(pid: i32) -> bool {
        let begun = Instant::now();
        while kill(Pid::from_raw(pid), None).is_ok() && begun.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(10));
        }
        kill(Pid::from_raw(pid), None).is_err()
    }

    #[test]
    fn kill_group_ends_a_grandchild_that_still_holds_the_pipe_after_the_leader_exits() {
        let dir = tempfile::tempdir().unwrap();
        let gpid_file = dir.path().join("gpid");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "sleep 61.4173 &\necho $! > '{}'\nprintf ok",
                gpid_file.display()
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let group = Pid::from_raw(i32::try_from(child.id()).unwrap());
        let stdout = drain(child.stdout.take(), 1024);
        let gpid = pid_in(&gpid_file);

        // The leader exits and is reaped; the sleeper keeps the pipe open.
        let begun = Instant::now();
        while child.try_wait().unwrap().is_none() {
            assert!(
                begun.elapsed() < Duration::from_secs(10),
                "the shell never exited"
            );
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(200));
        assert!(
            !stdout.is_finished(),
            "the pipe closed while the sleeper held it"
        );

        kill_group(&mut child);

        let gone = goes_away(gpid);
        let begun = Instant::now();
        while !stdout.is_finished() && begun.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(10));
        }
        let finished = stdout.is_finished();
        // A failing run leaves no sleeper behind.
        let _ = killpg(group, Signal::SIGKILL);
        assert!(gone, "the grandchild outlived kill_group");
        assert!(finished, "the pipe stayed open after kill_group");
        assert_eq!(stdout.join().unwrap(), (b"ok".to_vec(), false));
    }

    #[test]
    fn kill_group_ends_and_reaps_a_running_leader_and_its_grandchild() {
        let dir = tempfile::tempdir().unwrap();
        let gpid_file = dir.path().join("gpid");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "sleep 61.4179 &\necho $! > '{}'\nwait",
                gpid_file.display()
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let group = Pid::from_raw(i32::try_from(child.id()).unwrap());
        let gpid = pid_in(&gpid_file);

        kill_group(&mut child);

        // An unreaped leader is a zombie that still answers a signal; `try_wait` would reap it.
        let reaped = goes_away(group.as_raw());
        let gone = goes_away(gpid);
        // A failing run leaves no sleeper behind.
        let _ = killpg(group, Signal::SIGKILL);
        let _ = child.wait();
        assert!(reaped, "the leader was not reaped");
        assert!(gone, "the grandchild outlived kill_group");
    }
}
