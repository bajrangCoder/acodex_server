//! Fallback PTY implementation using the Linux TIOCGPTPEER ioctl.
//!
//! When the standard `openpty()` path fails — typically because SELinux blocks
//! `open("/dev/pts/N")` — this module creates the master/slave pair via
//! `/dev/ptmx` + `TIOCGPTPEER`, then spawns the child with
//! `std::process::Command`.

use anyhow::{bail, Error};
use portable_pty::{Child, MasterPty, PtySize};
use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

/// `TIOCGPTPEER` — obtain the slave fd directly from the master fd.
/// Defined in `<linux/tty.h>` as `_IO('T', 0x41)` = `0x5441`.
/// Architecture-independent on Linux.
const TIOCGPTPEER: libc::c_ulong = 0x5441;

// ---------------------------------------------------------------------------
// OwnedFd — thin RAII wrapper around a raw file descriptor
// ---------------------------------------------------------------------------

struct OwnedFd(RawFd);

impl OwnedFd {
    fn try_clone(&self) -> io::Result<Self> {
        let fd = unsafe { libc::dup(self.0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // Set CLOEXEC so cloned fds don't leak into spawned child processes.
        let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(err);
        }
        Ok(OwnedFd(fd))
    }
}

impl AsRawFd for OwnedFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

impl Drop for OwnedFd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

impl Read for OwnedFd {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = unsafe { libc::read(self.0, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n < 0 {
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::EIO) => return Ok(0), // slave closed → EOF
                    _ => return Err(err),
                }
            }
            return Ok(n as usize);
        }
    }
}

impl Write for OwnedFd {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            let n = unsafe { libc::write(self.0, buf.as_ptr() as *const _, buf.len()) };
            if n < 0 {
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    _ => return Err(err),
                }
            }
            return Ok(n as usize);
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FallbackMasterPty — implements portable_pty::MasterPty
// ---------------------------------------------------------------------------

struct FallbackMasterPty {
    fd: OwnedFd,
    took_writer: RefCell<bool>,
}

impl std::fmt::Debug for FallbackMasterPty {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("FallbackMasterPty")
            .field("fd", &self.fd.0)
            .finish()
    }
}

impl MasterPty for FallbackMasterPty {
    fn resize(&self, size: PtySize) -> Result<(), Error> {
        let ws = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCSWINSZ as _, &ws as *const _) } != 0
        {
            bail!("ioctl(TIOCSWINSZ) failed: {:?}", io::Error::last_os_error());
        }
        Ok(())
    }

    fn get_size(&self) -> Result<PtySize, Error> {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                libc::TIOCGWINSZ as _,
                &mut ws as *mut _,
            )
        } != 0
        {
            bail!("ioctl(TIOCGWINSZ) failed: {:?}", io::Error::last_os_error());
        }
        Ok(PtySize {
            rows: ws.ws_row,
            cols: ws.ws_col,
            pixel_width: ws.ws_xpixel,
            pixel_height: ws.ws_ypixel,
        })
    }

    fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, Error> {
        Ok(Box::new(self.fd.try_clone()?))
    }

    fn take_writer(&self) -> Result<Box<dyn Write + Send>, Error> {
        if *self.took_writer.borrow() {
            bail!("cannot take writer more than once");
        }
        *self.took_writer.borrow_mut() = true;
        Ok(Box::new(FallbackMasterWriter {
            fd: self.fd.try_clone()?,
        }))
    }

    fn process_group_leader(&self) -> Option<libc::pid_t> {
        match unsafe { libc::tcgetpgrp(self.fd.as_raw_fd()) } {
            pid if pid > 0 => Some(pid),
            _ => None,
        }
    }

    fn as_raw_fd(&self) -> Option<RawFd> {
        Some(self.fd.as_raw_fd())
    }

    fn tty_name(&self) -> Option<std::path::PathBuf> {
        None
    }
}

// ---------------------------------------------------------------------------
// FallbackMasterWriter — sends EOT on drop, matching portable-pty behaviour
// ---------------------------------------------------------------------------

struct FallbackMasterWriter {
    fd: OwnedFd,
}

impl Write for FallbackMasterWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.fd.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.fd.flush()
    }
}

impl Drop for FallbackMasterWriter {
    fn drop(&mut self) {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(self.fd.as_raw_fd(), &mut t) == 0 {
                let eot = t.c_cc[libc::VEOF];
                if eot != 0 {
                    let _ = self.fd.write_all(&[b'\n', eot]);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: close leaked fds in the child process
// ---------------------------------------------------------------------------

/// Close all fds above stderr in the child process.
///
/// This runs inside `pre_exec` (between `fork()` and `exec()`), so it MUST
/// only use async-signal-safe operations — no heap allocation, no Rust
/// stdlib I/O, no iterators that allocate.
unsafe fn close_fds_above_stderr() {
    // Try close_range(3, UINT_MAX, 0) first (Linux 5.9+).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let res = libc::syscall(libc::SYS_close_range, 3u64, u32::MAX as u64, 0u64);
        if res == 0 {
            return;
        }
    }

    // Fallback: close(3..RLIMIT_NOFILE) loop — no allocation needed.
    let mut rl: libc::rlimit = std::mem::zeroed();
    let max_fd: libc::rlim_t = if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) == 0
        && rl.rlim_cur != libc::RLIM_INFINITY
    {
        rl.rlim_cur
    } else {
        1024
    };
    let mut fd: libc::c_int = 3;
    while (fd as libc::rlim_t) < max_fd {
        libc::close(fd);
        fd += 1;
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Open a PTY using TIOCGPTPEER and spawn a command.
///
/// Fallback for when `portable_pty::native_pty_system().openpty()` fails
/// (e.g. SELinux blocks `open("/dev/pts/N")`).
pub fn fallback_open_and_spawn(
    size: PtySize,
    program: &str,
    args: &[String],
) -> anyhow::Result<(Box<dyn MasterPty + Send>, Box<dyn Child + Send + Sync>)> {
    use std::os::unix::process::CommandExt;

    // 1. Open master PTY
    let master_fd = unsafe { libc::open(c"/dev/ptmx".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if master_fd < 0 {
        bail!("open(/dev/ptmx) failed: {:?}", io::Error::last_os_error());
    }
    let master = OwnedFd(master_fd);

    // 2. Grant & unlock
    if unsafe { libc::grantpt(master.as_raw_fd()) } != 0 {
        bail!("grantpt failed: {:?}", io::Error::last_os_error());
    }
    if unsafe { libc::unlockpt(master.as_raw_fd()) } != 0 {
        bail!("unlockpt failed: {:?}", io::Error::last_os_error());
    }

    // 3. Obtain slave fd via TIOCGPTPEER (bypasses /dev/pts)
    let slave_fd = unsafe {
        libc::ioctl(
            master.as_raw_fd(),
            TIOCGPTPEER as _,
            libc::O_RDWR | libc::O_NOCTTY,
        )
    };
    if slave_fd < 0 {
        bail!(
            "ioctl(TIOCGPTPEER) failed: {:?}",
            io::Error::last_os_error()
        );
    }

    // 4. Set window size (non-fatal — the first resize from the client will
    //    correct it anyway, so we only log on failure rather than aborting).
    let ws = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.pixel_width,
        ws_ypixel: size.pixel_height,
    };
    if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws as *const _) } == -1 {
        tracing::warn!(
            "ioctl(TIOCSWINSZ) failed (non-fatal): {:?}",
            io::Error::last_os_error()
        );
    }

    // 5. Prepare Stdio from slave fd (one dup per stream).
    //    Wrap slave_fd in OwnedFd so it is closed on all paths
    //    (including early ? returns from mk_stdio).
    let (child_stdin, child_stdout, child_stderr) = {
        let slave = OwnedFd(slave_fd);
        let mk_stdio = || -> anyhow::Result<std::process::Stdio> {
            let fd = unsafe { libc::dup(slave.as_raw_fd()) };
            if fd < 0 {
                bail!("dup(slave_fd) failed: {:?}", io::Error::last_os_error());
            }
            Ok(unsafe { std::process::Stdio::from_raw_fd(fd) })
        };
        let stdin = mk_stdio()?;
        let stdout = mk_stdio()?;
        let stderr = mk_stdio()?;
        (stdin, stdout, stderr)
        // `slave` (OwnedFd) is dropped here, closing the original slave_fd.
    };

    // 6. Spawn command
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    unsafe {
        cmd.stdin(child_stdin)
            .stdout(child_stdout)
            .stderr(child_stderr)
            .pre_exec(|| {
                // Reset signal dispositions
                for signo in &[
                    libc::SIGCHLD,
                    libc::SIGHUP,
                    libc::SIGINT,
                    libc::SIGQUIT,
                    libc::SIGTERM,
                    libc::SIGALRM,
                ] {
                    libc::signal(*signo, libc::SIG_DFL);
                }
                let empty_set: libc::sigset_t = std::mem::zeroed();
                libc::sigprocmask(libc::SIG_SETMASK, &empty_set, std::ptr::null_mut());

                // New session
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }

                // Set controlling terminal
                #[allow(clippy::cast_lossless)]
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }

                // Close leaked fds
                close_fds_above_stderr();

                Ok(())
            });
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn '{}' failed: {}", program, e))?;

    // Detach child stdio handles (master side is our I/O path)
    child.stdin.take();
    child.stdout.take();
    child.stderr.take();

    let master_pty = FallbackMasterPty {
        fd: master,
        took_writer: RefCell::new(false),
    };

    Ok((Box::new(master_pty), Box::new(child)))
}
