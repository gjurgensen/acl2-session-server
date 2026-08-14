//! Minimal Unix PTY support.
//!
//! A PTY is not a stylistic choice here. ACL2 block-buffers its output when
//! stdout is not a tty: measured over a proof running at 65% CPU, a pipe
//! delivered zero bytes while a PTY delivered output incrementally. Live
//! progress on a running form is impossible without one.
//!
//! Everything here goes through `rustix`, so file descriptors are `OwnedFd`
//! and ownership is a property the compiler checks rather than a convention
//! a comment asks you to respect. That is not abstract tidiness: the one
//! descriptor bug this project has hit was handing the same `RawFd` to three
//! consumers, each of which closed it — which can shut an unrelated
//! descriptor once the number is reused. `OwnedFd` makes that a type error.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::Duration;

use rustix::event::{PollFd, PollFlags};
use rustix::termios::{LocalModes, OptionalActions, OutputModes};

pub struct Pty {
    /// Our end. Everything ACL2 prints arrives here; everything we send it
    /// goes out here.
    pub master: OwnedFd,
    /// The child's end, which becomes its controlling terminal. Dropped in the
    /// parent once the child has been spawned.
    pub slave: OwnedFd,
}

impl Pty {
    pub fn open() -> io::Result<Pty> {
        let master = rustix::pty::openpt(rustix::pty::OpenptFlags::RDWR)?;
        rustix::pty::grantpt(&master)?;
        rustix::pty::unlockpt(&master)?;
        let name = rustix::pty::ptsname(&master, Vec::new())?;
        let slave = rustix::fs::open(
            name.to_string_lossy().as_ref(),
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY,
            rustix::fs::Mode::empty(),
        )?;
        Ok(Pty { master, slave })
    }
}

/// Make the calling process a session leader and adopt `slave` as its
/// controlling terminal.
///
/// Runs in the child between fork and exec, so it must stay async-signal-safe.
/// `setsid` and `ioctl` both are; adding an allocation or a `println!` here
/// would not be.
///
/// `setsid` alone is **not** enough. Without `TIOCSCTTY` the pty is not the
/// controlling terminal, so the line discipline has no foreground process
/// group and a `^C` written to the master is inert. That was measured, and it
/// cost an hour of chasing a phantom ACL2 behaviour. We signal the process
/// directly rather than via `^C`, but a correct controlling terminal is still
/// what makes ACL2 treat the session as interactive.
pub fn become_session_leader(slave: BorrowedFd<'_>) -> io::Result<()> {
    rustix::process::setsid()?;
    rustix::process::ioctl_tiocsctty(slave)?;
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
pub fn set_no_echo(fd: impl AsFd) -> io::Result<()> {
    let fd = fd.as_fd();
    let mut t = rustix::termios::tcgetattr(fd)?;
    t.local_modes.remove(
        LocalModes::ECHO | LocalModes::ECHOE | LocalModes::ECHOK | LocalModes::ECHONL,
    );
    t.output_modes.remove(OutputModes::OPOST);
    rustix::termios::tcsetattr(fd, OptionalActions::Now, &t)?;
    Ok(())
}

/// Wait until `fd` is readable, or `timeout` elapses.
/// Returns `true` if readable.
pub fn wait_readable(fd: impl AsFd, timeout: Duration) -> io::Result<bool> {
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    let ts = rustix::event::Timespec {
        tv_sec: timeout.as_secs() as _,
        tv_nsec: timeout.subsec_nanos() as _,
    };
    match rustix::event::poll(&mut fds, Some(&ts)) {
        Ok(n) => Ok(n > 0),
        // A signal interrupted the wait; treat as "nothing yet".
        Err(e) if e == rustix::io::Errno::INTR => Ok(false),
        Err(e) => Err(e.into()),
    }
}
