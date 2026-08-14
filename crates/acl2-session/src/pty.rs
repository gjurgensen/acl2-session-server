//! Minimal Unix PTY support.
//!
//! A PTY is not a stylistic choice here. ACL2 block-buffers its output when
//! stdout is not a tty: measured over a proof running at 65% CPU, a pipe
//! delivered zero bytes while a PTY delivered output incrementally. Live
//! progress on a running form is impossible without one.
//!
//! `posix_openpt` and friends are used rather than `openpty` so that no
//! dependency on libutil is required.

use std::ffi::CStr;
use std::io;
use std::os::unix::io::RawFd;

pub struct Pty {
    /// Our end. Everything ACL2 prints arrives here; everything we send it
    /// goes out here.
    pub master: RawFd,
    /// The child's end, which becomes its controlling terminal. Closed in the
    /// parent once the child has been spawned.
    pub slave: RawFd,
}

impl Pty {
    pub fn open() -> io::Result<Pty> {
        unsafe {
            let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            if master < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::grantpt(master) < 0 || libc::unlockpt(master) < 0 {
                let e = io::Error::last_os_error();
                libc::close(master);
                return Err(e);
            }
            let mut buf = [0i8; 128];
            if libc::ptsname_r(master, buf.as_mut_ptr(), buf.len()) != 0 {
                let e = io::Error::last_os_error();
                libc::close(master);
                return Err(e);
            }
            let name = CStr::from_ptr(buf.as_ptr());
            let slave = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
            if slave < 0 {
                let e = io::Error::last_os_error();
                libc::close(master);
                return Err(e);
            }
            Ok(Pty { master, slave })
        }
    }
}

/// Make the calling process a session leader and adopt `slave` as its
/// controlling terminal.
///
/// Runs in the child between fork and exec, so it must stay async-signal-safe.
///
/// `setsid` alone is **not** enough. Without `TIOCSCTTY` the pty is not the
/// controlling terminal, so the line discipline has no foreground process
/// group and a `^C` written to the master is inert. That was measured, and it
/// cost an hour of chasing a phantom ACL2 behaviour. We signal the process
/// directly rather than via `^C`, but a correct controlling terminal is still
/// what makes ACL2 treat the session as interactive.
///
/// # Safety
/// Must only be called in a freshly forked child, before `exec`.
pub unsafe fn become_session_leader(slave: RawFd) -> io::Result<()> {
    // An `unsafe fn` body is not an implicit unsafe block, so the genuinely
    // unsafe calls are marked as such rather than the whole function.
    unsafe {
        if libc::setsid() < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::ioctl(slave, libc::TIOCSCTTY as libc::c_ulong, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Turn off input echo and output post-processing on the terminal.
///
/// Echo must be off for correctness, not tidiness. The line we send contains
/// the completion sentinel as literal text (it is inside a `cw` format
/// string), so with echo on, the sentinel comes straight back the instant the
/// line is written and every form appears to finish in zero milliseconds
/// before ACL2 has done anything at all.
///
/// `OPOST` off additionally stops the tty translating `\n` into `\r\n`, which
/// keeps captured output byte-identical to what ACL2 printed.
///
/// `ICANON` and `ISIG` are deliberately left on, so the terminal behaves like
/// a terminal. Note that canonical mode caps a single input line (`MAX_CANON`,
/// typically 4096 bytes) -- one more reason forms are submitted by `ld`-ing a
/// temp file rather than by typing them at the REPL.
pub fn set_no_echo(fd: RawFd) -> io::Result<()> {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) < 0 {
            return Err(io::Error::last_os_error());
        }
        t.c_lflag &= !(libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ECHONL);
        t.c_oflag &= !libc::OPOST;
        if libc::tcsetattr(fd, libc::TCSANOW, &t) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Wait until `fd` is readable, or `timeout_ms` elapses.
/// Returns `true` if readable.
pub fn wait_readable(fd: RawFd, timeout_ms: i32) -> io::Result<bool> {
    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let n = unsafe { libc::poll(&mut p, 1, timeout_ms) };
    if n < 0 {
        let e = io::Error::last_os_error();
        // A signal interrupted the wait; treat as "nothing yet".
        if e.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(e);
    }
    Ok(n > 0)
}
