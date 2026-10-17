//! PTY runtime: spawns the child on a pseudo-TTY, runs a reader thread, and
//! exposes a non-blocking event stream + writer.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

const READ_BUF: usize = 4096;

/// How many pending writes toward the PTY we hold before dropping them.
///
/// The writer sits on its own thread for one reason: `write_all` on the master
/// blocks once the child stops reading its stdin. A remote stuck in a terminal
/// query loop that never reads its own input makes `ssh` stop reading ours, the
/// master fills after ~20 KiB, and doing that write on the frame loop parked
/// every tab, input and rendering included — measured, not theorised. Off the
/// frame loop the only cost of a wedged remote is that writes aimed at it pile
/// up, and this is where the pile stops: a session that cannot be written to is
/// one where dropping is the honest outcome. Human typing never queues this
/// deep, so a full queue always means the far end is gone.
const WRITE_QUEUE_LEN: usize = 64;

/// Env var carrying the stderr FIFO path into the `sh` wrapper.
const STDERR_FIFO_ENV: &str = "SSHUB_STDERR_FIFO";
/// Env var carrying the ssh log FIFO path (`ssh -E`) into the `sh` wrapper.
const LOG_FIFO_ENV: &str = "SSHUB_LOG_FIFO";

/// Event from the PTY reader thread to the main thread.
#[derive(Debug)]
pub enum PtyEvent {
    /// Bytes read from the master side of the PTY (stdout / the live shell).
    Bytes(Vec<u8>),
    /// Bytes read from ssh's stderr, routed through a side FIFO so the verbose
    /// `-v` handshake never pollutes the terminal grid.
    Stderr(Vec<u8>),
    /// Bytes of ssh's own log (`ssh -E`): debug lines, `Authenticated to …`,
    /// host-key errors. Unlike [`Self::Stderr`] a server cannot write here —
    /// its pre-auth banner goes to stderr — so only this stream is evidence.
    SshLog(Vec<u8>),
    /// Child exited; carries a human-readable status string.
    Exited(String),
}

/// Named FIFOs used to siphon the child's stderr away from the PTY, plus (for
/// ssh) a second one for its `-E` log so banner text cannot pose as ssh output.
/// Created inside a private `0700` random-named temp dir (a predictable name in
/// a shared `/tmp` can be pre-created by another user), opened non-blocking on
/// the read side, and removed with its dir on drop.
struct StderrFifo {
    _dir: tempfile::TempDir,
    path: PathBuf,
    read: File,
    log: Option<(PathBuf, File)>,
}

/// `mkfifo` + open read+write, non-blocking. O_RDWR keeps a writer permanently
/// attached (ourselves), so an empty FIFO yields EAGAIN rather than a
/// premature EOF before the child has opened its write end — otherwise the
/// reader thread would see Ok(0) on its very first read and quit. The thread
/// instead stops on the drop flag. (Linux/BSD extension; Unix-only TUI.)
fn make_fifo(path: &std::path::Path) -> Result<File> {
    let c_path = CString::new(path.as_os_str().as_bytes()).context("fifo path nul")?;
    // SAFETY: c_path is a valid NUL-terminated C string.
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    if rc != 0 {
        return Err(anyhow!(
            "mkfifo({}) failed: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open fifo {}", path.display()))
}

impl StderrFifo {
    fn create(with_log: bool) -> Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("sshub-stderr-")
            .tempdir()
            .context("create private dir for the stderr fifo")?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                .context("restrict the stderr fifo dir")?;
        }
        let path = dir.path().join("stderr.fifo");
        let read = make_fifo(&path)?;
        let log = if with_log {
            let log_path = dir.path().join("ssh-log.fifo");
            let file = make_fifo(&log_path)?;
            Some((log_path, file))
        } else {
            None
        };
        Ok(Self {
            _dir: dir,
            path,
            read,
            log,
        })
    }
}

/// Whether `argv[0]` is OpenSSH's client, the only child that understands `-E`.
fn is_ssh(argv0: &str) -> bool {
    std::path::Path::new(argv0)
        .file_name()
        .is_some_and(|n| n == "ssh")
}

pub struct PtyRuntime {
    master: Box<dyn MasterPty + Send>,
    /// Sender into the writer thread. `None` only while dropping, which is what
    /// tells the thread to exit.
    write_tx: Option<SyncSender<Vec<u8>>>,
    rx: Receiver<PtyEvent>,
    /// Set when the reader has signalled EOF / child exit. Used so we don't
    /// keep spinning on a dead PTY.
    closed: Arc<AtomicBool>,
    reader_thread: Option<JoinHandle<()>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    /// Set on drop to stop the stderr reader promptly even if the child never
    /// opened the FIFO write end.
    stderr_stop: Arc<AtomicBool>,
    fifo_readers: Vec<JoinHandle<()>>,
    /// Kept alive so the FIFO is unlinked when the runtime drops.
    _stderr_fifo: Option<StderrFifo>,
}

impl PtyRuntime {
    /// Spawn `argv` on a fresh PTY. When `siphon_stderr` the child's stderr
    /// is routed through a side FIFO into [`PtyEvent::Stderr`] (the ssh
    /// `-v` split); when false stderr stays merged with stdout on the PTY,
    /// which is what an interactive local shell needs — shells print PS1
    /// to stderr, so siphoning leaves a blank grid the reveal logic never
    /// fires on.
    pub fn spawn(
        argv: &[String],
        rows: u16,
        cols: u16,
        env: &[(String, String)],
        siphon_stderr: bool,
    ) -> Result<Self> {
        if argv.is_empty() {
            return Err(anyhow!("empty argv"));
        }

        // Route the child's stderr through a side FIFO so ssh's `-v` debug
        // output never lands on the PTY grid. Without it the session could
        // never see `Authenticated to …`, so it would never accept keystrokes:
        // fail the spawn loudly instead of producing a dead session.
        let stderr_fifo = siphon_stderr
            .then(|| StderrFifo::create(is_ssh(&argv[0])))
            .transpose()?;

        let (program, prog_args): (String, Vec<String>) = if stderr_fifo.is_some() {
            // sh redirects fd 2 to the FIFO (opened by path, after portable-pty's
            // close_random_fds), then execs the real command in-place so the PID
            // stays ssh's for signal delivery.
            // For ssh, `-E` goes right after the program so the debug log leaves
            // through its own FIFO instead of sharing stderr with the banner.
            let script = if stderr_fifo.as_ref().is_some_and(|f| f.log.is_some()) {
                format!(
                    "p=$1; shift; exec \"$p\" -E \"${LOG_FIFO_ENV}\" \"$@\" 2>\"${STDERR_FIFO_ENV}\""
                )
            } else {
                format!("exec \"$@\" 2>\"${STDERR_FIFO_ENV}\"")
            };
            let mut args = vec!["-c".to_string(), script, "sshub".to_string()];
            args.extend(argv.iter().cloned());
            ("/bin/sh".to_string(), args)
        } else {
            (argv[0].clone(), argv[1..].to_vec())
        };

        let mut cmd = CommandBuilder::new(&program);
        for arg in &prog_args {
            cmd.arg(arg);
        }
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        if let Some(fifo) = &stderr_fifo {
            cmd.env(STDERR_FIFO_ENV, fifo.path.as_os_str());
            if let Some((log_path, _)) = &fifo.log {
                cmd.env(LOG_FIFO_ENV, log_path.as_os_str());
            }
        }
        // Override TERM. Our vt100 emulator is xterm-compatible; advertising
        // `xterm-kitty` (often inherited from the user's host kitty session)
        // leaves the remote without a matching terminfo entry — breaking
        // `clear`, `tput`, ncurses apps, etc. Force a portable default.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("spawn child on pty slave")?;
        // Slave is no longer needed in this process.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
        let mut writer = pair.master.take_writer().context("take pty writer")?;

        // Everything written toward the child goes through here, in order:
        // keystrokes, the auto-typed secret, pastes, and the emulator's answers
        // to terminal queries. One thread keeps that ordering while taking the
        // blocking write off the frame loop. It ends when the sender drops.
        let (write_tx, write_rx) = mpsc::sync_channel::<Vec<u8>>(WRITE_QUEUE_LEN);
        thread::Builder::new()
            .name("sshub-pty-writer".into())
            .spawn(move || {
                for chunk in write_rx {
                    if writer.write_all(&chunk).is_err() {
                        break;
                    }
                    writer.flush().ok();
                }
            })
            .context("spawn pty writer thread")?;

        let (tx, rx) = mpsc::channel();
        let stderr_tx = tx.clone();
        let closed = Arc::new(AtomicBool::new(false));
        let closed_thread = Arc::clone(&closed);

        let reader_thread = thread::Builder::new()
            .name("sshub-pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; READ_BUF];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            let _ = tx.send(PtyEvent::Exited("eof".into()));
                            break;
                        }
                        Ok(n) => {
                            if tx.send(PtyEvent::Bytes(buf[..n].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(PtyEvent::Exited(format!("read error: {e}")));
                            break;
                        }
                    }
                }
                closed_thread.store(true, Ordering::Relaxed);
            })
            .context("spawn pty reader thread")?;

        // Second reader: siphon stderr from the FIFO. Non-blocking + poll so it
        // never wedges and honours the stop flag on drop.
        let stderr_stop = Arc::new(AtomicBool::new(false));
        // The stderr siphon is best-effort: if cloning the FIFO handle or
        // spawning the reader fails (resource exhaustion), degrade to no
        // `-v` debug capture rather than panicking the whole connect.
        let mut fifo_readers = Vec::new();
        if let Some(fifo) = stderr_fifo.as_ref() {
            fifo_readers.extend(spawn_fifo_reader(
                &fifo.read,
                stderr_tx.clone(),
                &stderr_stop,
                PtyEvent::Stderr,
            ));
            if let Some((_, log)) = &fifo.log {
                fifo_readers.extend(spawn_fifo_reader(
                    log,
                    stderr_tx,
                    &stderr_stop,
                    PtyEvent::SshLog,
                ));
            }
        }

        Ok(Self {
            master: pair.master,
            write_tx: Some(write_tx),
            rx,
            closed,
            reader_thread: Some(reader_thread),
            child: Some(child),
            stderr_stop,
            fifo_readers,
            _stderr_fifo: stderr_fifo,
        })
    }

    /// Non-blocking poll for one event.
    pub fn try_recv(&self) -> Option<PtyEvent> {
        self.rx.try_recv().ok()
    }

    /// Hand bytes to the writer thread. Never blocks: see [`WRITE_QUEUE_LEN`]
    /// for why the write cannot happen on the caller's thread. The cost is that
    /// a write failure is no longer reported synchronously — a full queue is,
    /// and it is the only failure a caller can do anything about (nothing is
    /// reaching the remote).
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let tx = self
            .write_tx
            .as_ref()
            .ok_or_else(|| anyhow!("pty writer is gone"))?;
        match tx.try_send(bytes.to_vec()) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                Err(anyhow!("pty write queue full — the remote is not reading"))
            }
            Err(TrySendError::Disconnected(_)) => Err(anyhow!("pty writer has exited")),
        }
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("pty resize")?;
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Whether ssh's own log arrives as [`PtyEvent::SshLog`], separate from
    /// stderr (true for the `ssh` client only).
    pub fn has_log_channel(&self) -> bool {
        self._stderr_fifo.as_ref().is_some_and(|f| f.log.is_some())
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.child.as_ref()?.process_id()
    }

    /// Reap a child that has already exited. Prevents zombies while the
    /// [`Session`] object stays alive in a detached tab.
    pub fn reap_child(&mut self) -> Option<portable_pty::ExitStatus> {
        if let Some(mut child) = self.child.take() {
            child.wait().ok()
        } else {
            None
        }
    }

    pub(super) fn terminate_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            terminate_child_process(&mut *child);
        }
    }
}

/// Reader thread draining one FIFO into `PtyEvent`s. Non-blocking + poll so it
/// never wedges and honours the stop flag on drop. Best-effort: if cloning the
/// handle or spawning fails (resource exhaustion) the stream is just lost.
fn spawn_fifo_reader(
    fifo: &File,
    tx: mpsc::Sender<PtyEvent>,
    stop: &Arc<AtomicBool>,
    wrap: fn(Vec<u8>) -> PtyEvent,
) -> Option<JoinHandle<()>> {
    let mut read = fifo.try_clone().ok()?;
    let stop = Arc::clone(stop);
    thread::Builder::new()
        .name("sshub-stderr-reader".into())
        .spawn(move || {
            let mut buf = [0u8; READ_BUF];
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match read.read(&mut buf) {
                    Ok(0) => break, // writer (ssh) closed → EOF
                    Ok(n) => {
                        if tx.send(wrap(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        })
        .ok()
}

/// Kill the embedded ssh child and its process group, then reap it.
fn terminate_child_process(child: &mut dyn portable_pty::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.process_id() {
        let pgid = pid as libc::pid_t;
        // portable-pty calls setsid() in the slave pre_exec, so `-pid` hits the
        // whole session (ssh and any local helpers).
        unsafe {
            libc::kill(-pgid, libc::SIGHUP);
        }
        std::thread::sleep(Duration::from_millis(50));
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

impl Drop for PtyRuntime {
    fn drop(&mut self) {
        self.terminate_child();
        // Closing the channel is what ends the writer thread; it is deliberately
        // not joined. A write blocked on a child that stopped reading only
        // returns once the slave closes, and while `terminate_child` above makes
        // that happen, waiting on it here would move the freeze we just fixed
        // into session teardown. The thread owns nothing but its own fd.
        self.write_tx = None;
        self.stderr_stop.store(true, Ordering::Relaxed);
        for handle in self.fifo_readers.drain(..) {
            let _ = handle.join();
        }
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_stderr_fifo_lives_in_a_private_random_dir_and_goes_with_it() {
        // A fixed name in a shared /tmp can be pre-created by another user,
        // which used to leave the session without its debug stream.
        let a = StderrFifo::create(true).unwrap();
        let b = StderrFifo::create(false).unwrap();
        let dir = a.path.parent().unwrap().to_path_buf();
        assert_ne!(dir, b.path.parent().unwrap());
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        drop(a);
        assert!(!dir.exists());
    }
}

#[cfg(test)]
mod ssh_log_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn ssh_log_and_banner_arrive_on_separate_streams() {
        // A server banner is printed on ssh's stderr; the real "Authenticated
        // to" goes to the `-E` log. The two must never share an event kind.
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("ssh");
        std::fs::write(
            &fake,
            "#!/bin/sh\n[ \"$1\" = -E ] || exit 3\necho 'debug1: real-log' > \"$2\"\necho 'banner-text' >&2\nsleep 0.3\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let argv = vec![fake.to_string_lossy().into_owned(), "host".into()];
        let rt = PtyRuntime::spawn(&argv, 24, 80, &[], true).unwrap();
        let (mut err, mut log) = (String::new(), String::new());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && !(err.contains("banner") && log.contains("real"))
        {
            match rt.try_recv() {
                Some(PtyEvent::Stderr(b)) => err.push_str(&String::from_utf8_lossy(&b)),
                Some(PtyEvent::SshLog(b)) => log.push_str(&String::from_utf8_lossy(&b)),
                Some(_) => {}
                None => thread::sleep(Duration::from_millis(10)),
            }
        }
        assert!(
            err.contains("banner-text") && !err.contains("real-log"),
            "{err:?}"
        );
        assert!(
            log.contains("real-log") && !log.contains("banner-text"),
            "{log:?}"
        );
    }
}
