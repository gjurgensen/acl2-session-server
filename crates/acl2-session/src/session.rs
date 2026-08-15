//! A live ACL2 session: one process, one book directory, one form at a time.
//!
//! One **reader thread** per session owns the PTY's read side. Everything
//! ACL2 prints lands in a shared stream the moment it arrives; the callers
//! that used to do the reading are now *waiters* on that stream. That split
//! is what the whole concurrency story hangs on: a submission waits for its
//! completion marker on a condvar and is woken the instant it arrives, an
//! abort request wakes the same condvar instead of waiting out a poll slice,
//! a status reader sees the complete, current output without any session
//! lock, and the child's death is one EOF observed in one place. Attribution
//! -- which bytes belong to which form -- becomes offset arithmetic on one
//! stream, replacing the carried-`pending` shuffling where misattribution
//! bugs used to live.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::pty::{become_session_leader, set_no_echo, wait_readable, Pty};

/// How long to wait between escalating interrupts before giving up on a
/// graceful abort.
const INTERRUPT_STEP: Duration = Duration::from_millis(1500);
const INTERRUPT_TRIES: usize = 4;

/// How often the reader thread looks up from the descriptor to notice a
/// shutdown request. This is teardown responsiveness only -- data and EOF
/// wake it immediately, and no *caller* polls anything: waiters sit on a
/// condvar and are notified per append.
const READER_POLL: Duration = Duration::from_millis(500);

/// How much of the in-flight output a lock-free status snapshot copies.
/// Bounds the copy, not the stream; the full text is in the capture.
const SNAPSHOT_TAIL: usize = 16384;

/// Deadlines are capped so `Instant` arithmetic cannot overflow; a month is
/// "forever" for a tool call.
const DEADLINE_CAP: Duration = Duration::from_secs(60 * 60 * 24 * 30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The form was admitted.
    Ok,
    /// ACL2 finished with the form but did not admit it.
    Failed,
    /// The deadline expired; the form is still running. Not an error.
    StillRunning,
}

/// A snapshot of a form that is still running.
#[derive(Debug, Clone)]
pub struct Progress {
    pub elapsed: Duration,
    /// The last few lines ACL2 has printed. Provisional by nature: the
    /// authoritative result comes from the structured probes once the form
    /// finishes.
    pub tail: String,
    pub bytes_so_far: usize,
}

/// What a status reader may learn about an in-flight form *without* the
/// session lock. See [`SessionControl`].
#[derive(Debug, Clone)]
pub struct StatusSnapshot {
    pub elapsed: Duration,
    pub bytes_so_far: usize,
    /// The file unit in flight, when the form came from a sync. Set by the
    /// book layer, which is the only place that knows.
    pub unit: Option<usize>,
    /// The tail of the output so far, bounded, decoded lossily. Display only.
    pub tail: String,
}

/// The stream every party meets at, and the handle other threads hold on a
/// running session.
///
/// The reader thread appends; session calls wait and consume; status readers
/// peek; an interrupt asks the blocked waiter to yield. One mutex and one
/// condvar carry all of it:
///
/// - **Abort.** `request_abort` sets a flag and notifies; a blocked waiter
///   wakes immediately and returns as if its deadline expired, the session
///   lock frees, and the interrupt's escalation runs at once. (The book-level
///   lock still serialises operations -- that is what makes a session a
///   session -- so the handoff is still needed; it is just no longer paced
///   by a poll.)
/// - **Status.** A snapshot reads the actual stream: complete, current, no
///   session lock, no shadow copy to fall behind.
/// - **Consumption.** A completed form's capture is the stream from the last
///   consumption point to its completion marker; consuming advances the
///   base offset. What one form leaves past its marker is simply what the
///   next capture starts with -- the old carried-`pending` dance, now a
///   number.
///
/// One control block belongs to one [`sync::Book`](crate::sync::Book) for
/// its whole life. Sessions come and go beneath it (rebuild starts a new
/// process); the stream is *epoch-tagged* so a predecessor's reader thread,
/// still draining during teardown, cannot write into the successor's stream.
pub struct SessionControl {
    inner: Mutex<Shared>,
    cond: Condvar,
}

struct Shared {
    /// Output not yet consumed by a completed submission.
    buf: Vec<u8>,
    /// Absolute stream offset of `buf[0]`. Only consumption advances it, and
    /// only session calls consume, one at a time, so a waiter's offsets stay
    /// valid for the whole wait.
    base: u64,
    /// The reader saw end-of-file: every copy of the slave side is closed,
    /// which in practice means the child is gone.
    eof: bool,
    /// Set by `request_abort`; consumed by the waiter it wakes.
    abort: bool,
    /// Which session generation owns the stream. Appends from any other are
    /// dropped: on rebuild the replacement session is constructed while its
    /// predecessor -- and the predecessor's reader -- still lives.
    epoch: u64,
    // What a lock-free status reader may know. Metadata, not stream.
    running: bool,
    started: Option<Instant>,
    unit: Option<usize>,
    /// Absolute offset where the in-flight form's output begins.
    form_start: u64,
}

/// Why a wait ended.
enum WaitEnd {
    /// The marker is in the stream; the payload is the absolute offset just
    /// past it.
    Found(u64),
    Timeout,
    Aborted,
    /// No marker, and no more bytes are ever coming.
    Eof,
}

impl SessionControl {
    pub fn new() -> Arc<SessionControl> {
        Arc::new(SessionControl {
            inner: Mutex::new(Shared {
                buf: Vec::new(),
                base: 0,
                eof: false,
                abort: false,
                epoch: 0,
                running: false,
                started: None,
                unit: None,
                form_start: 0,
            }),
            cond: Condvar::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        // A poisoned lock means a panic while appending or snapshotting;
        // the data is plain bytes and counters, valid regardless.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ask the call currently blocked on this session to return early.
    pub fn request_abort(&self) {
        self.lock().abort = true;
        self.cond.notify_all();
    }

    pub fn clear_abort(&self) {
        self.lock().abort = false;
    }

    /// Name the file unit about to run, for lock-free status readers.
    pub fn set_unit(&self, unit: Option<usize>) {
        self.lock().unit = unit;
    }

    /// Progress of the form in flight, or `None` when nothing is.
    pub fn snapshot(&self) -> Option<StatusSnapshot> {
        let g = self.lock();
        if !g.running {
            return None;
        }
        let from = (g.form_start.max(g.base) - g.base) as usize;
        let form = &g.buf[from.min(g.buf.len())..];
        let tail = &form[form.len().saturating_sub(SNAPSHOT_TAIL)..];
        Some(StatusSnapshot {
            elapsed: g.started.map(|s| s.elapsed()).unwrap_or_default(),
            bytes_so_far: form.len(),
            unit: g.unit,
            tail: String::from_utf8_lossy(tail).into_owned(),
        })
    }

    fn begin(&self) {
        let mut g = self.lock();
        g.running = true;
        g.started = Some(Instant::now());
        g.form_start = g.base + g.buf.len() as u64;
    }

    fn end(&self) {
        let mut g = self.lock();
        g.running = false;
        g.unit = None;
    }

    /// Claim the stream for a new session, orphaning any predecessor's
    /// reader. Returns the epoch that session's reader must present.
    fn adopt(&self) -> u64 {
        let mut g = self.lock();
        g.buf.clear();
        g.base = 0;
        g.eof = false;
        g.abort = false;
        g.running = false;
        g.started = None;
        g.unit = None;
        g.form_start = 0;
        g.epoch += 1;
        g.epoch
    }

    /// Reader-side: file freshly arrived output.
    fn append(&self, epoch: u64, bytes: &[u8]) {
        let mut g = self.lock();
        if g.epoch == epoch {
            g.buf.extend_from_slice(bytes);
            self.cond.notify_all();
        }
    }

    /// Reader-side: no more bytes are coming.
    fn mark_eof(&self, epoch: u64) {
        let mut g = self.lock();
        if g.epoch == epoch {
            g.eof = true;
            self.cond.notify_all();
        }
    }

    /// Where the unconsumed region begins.
    fn consumed_offset(&self) -> u64 {
        self.lock().base
    }

    /// Everything printed and not yet consumed, as of now.
    fn peek_all(&self) -> Vec<u8> {
        self.lock().buf.clone()
    }

    /// Take the stream from its base up to `end`, advancing the base: this
    /// is a completed submission claiming its capture. Bytes past `end`
    /// remain for whatever comes next.
    fn consume_to(&self, end: u64) -> Vec<u8> {
        let mut g = self.lock();
        let n = (end.saturating_sub(g.base) as usize).min(g.buf.len());
        let head: Vec<u8> = g.buf.drain(..n).collect();
        g.base += n as u64;
        head
    }

    /// Block until `marker` appears at or after absolute offset `from`, the
    /// deadline passes, an abort is requested, or the stream ends.
    ///
    /// Scanning is incremental with a marker-length overlap, so a wait over a
    /// six-figure-line capture rescans only what each wakeup added. The base
    /// cannot move during the wait -- only session calls consume, and they
    /// hold the session's lock -- so absolute offsets stay valid throughout.
    fn wait_for_marker(&self, marker: &[u8], from: u64, deadline: Duration) -> WaitEnd {
        let deadline = Instant::now() + deadline.min(DEADLINE_CAP);
        let overlap = (marker.len() - 1) as u64;
        let mut scanned = from;
        let mut g = self.lock();
        loop {
            let end = g.base + g.buf.len() as u64;
            if end > scanned {
                let scan_from = scanned.saturating_sub(overlap).max(from);
                let a = (scan_from - g.base) as usize;
                if let Some(i) = find_bytes(&g.buf[a..], marker) {
                    return WaitEnd::Found(scan_from + (i + marker.len()) as u64);
                }
                scanned = end;
            }
            if g.abort {
                g.abort = false;
                return WaitEnd::Aborted;
            }
            if g.eof {
                return WaitEnd::Eof;
            }
            let now = Instant::now();
            if now >= deadline {
                return WaitEnd::Timeout;
            }
            let (g2, _) = self
                .cond
                .wait_timeout(g, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            g = g2;
        }
    }
}

#[derive(Debug)]
pub struct Submission {
    pub outcome: Outcome,
    /// Everything ACL2 printed for this form, sentinels stripped.
    pub output: String,
    /// Single-line structured values, via `<<<ASS-DATA key value>>>`.
    /// This is how the caller gets facts out of the session without parsing
    /// prover prose: the body computes them in ACL2 and prints them under a
    /// key we chose.
    ///
    /// One-line only, and ACL2's fmt wraps at its right margin, so a value
    /// that can grow -- a path, a list -- does not belong here: a wrapped
    /// marker loses its `>>>` and the datum silently vanishes. Anything
    /// potentially wide goes in `blocks`.
    pub data: Vec<(String, String)>,
    /// Multi-line structured values, delimited by `<<<ASS-BEGIN key>>>` and
    /// `<<<ASS-END key>>>`. Needed because most of what is worth extracting --
    /// a rune list, a checkpoint goal -- does not fit on one line and ACL2
    /// wraps at its right margin.
    pub blocks: Vec<(String, String)>,
    pub elapsed: Duration,
}

impl Submission {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.data.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn block(&self, key: &str) -> Option<&str> {
        self.blocks.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// The form a session is currently working on.
struct InFlight {
    generation: u64,
    started: Instant,
    /// Submitted raw at the REPL rather than through an `ld` wrapper. A raw
    /// submission has no OK sentinel -- the top level continues past a soft
    /// error, so an OK would print regardless and mean nothing -- and
    /// therefore completes as `Ok` in the sense of "finished", never "passed".
    /// The caller verifies success by probing.
    raw: bool,
}

pub struct Session {
    child: Child,
    /// The PTY master's write side, from this thread's point of view; the
    /// reader thread holds its own clone for the read side. One descriptor,
    /// two directions, two owners.
    writer: Arc<OwnedFd>,
    /// The book's directory, absolute.
    ///
    /// Every submission re-establishes this as ACL2's connected book
    /// directory. See `submit`.
    cwd: PathBuf,
    /// Set while ACL2 is still working on a submitted form.
    ///
    /// A session drives one ACL2 over one pipe, so it is strictly
    /// single-threaded: submitting again while a form is in flight would
    /// interleave two forms' input and output on the same channel. That is
    /// rejected rather than queued, because queueing would hide from the
    /// caller that its deadline did not actually end the work.
    inflight: Option<InFlight>,
    tmpdir: PathBuf,
    generation: u64,
    control: Arc<SessionControl>,
    shutdown: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

/// The reader thread: own the PTY's read side, file everything into the
/// stream, and say so when it ends. Exits on EOF, on a read error, or when
/// the session asks it to stand down.
fn reader_loop(fd: Arc<OwnedFd>, control: Arc<SessionControl>, epoch: u64, shutdown: Arc<AtomicBool>) {
    let mut chunk = [0u8; 65536];
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        match wait_readable(fd.as_fd(), READER_POLL) {
            Ok(false) => continue,
            Ok(true) => match rustix::io::read(fd.as_fd(), &mut chunk) {
                Ok(0) | Err(_) => {
                    control.mark_eof(epoch);
                    return;
                }
                Ok(n) => control.append(epoch, &chunk[..n]),
            },
            Err(_) => {
                control.mark_eof(epoch);
                return;
            }
        }
    }
}

impl Session {
    /// Start ACL2 with its working directory set to `book_dir`.
    ///
    /// `ACL2_CUSTOMIZATION` is deliberately left alone. ACL2 then finds
    /// `acl2-customization.lsp` in the connected book directory, falling back
    /// to the user's home directory, which by convention establishes the
    /// packages and the `in-package`. That is the whole environment story: no
    /// `.acl2` parsing and no `cert.pl` emulation, so the session is the one a
    /// person gets by `cd`-ing here and starting ACL2.
    ///
    /// `control` is the shared stream; the caller that owns several sessions
    /// over one book passes the same block to each, and this session adopts
    /// it (see `SessionControl`).
    pub fn start(
        acl2: &Path,
        book_dir: &Path,
        control: Arc<SessionControl>,
    ) -> io::Result<Session> {
        // Check the directory first. It becomes the child's working directory,
        // so a missing one fails the spawn with the same ENOENT a missing
        // program gives -- and would then be reported as "cannot start ACL2",
        // blaming a binary that is perfectly fine.
        if !book_dir.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no such directory: {}", book_dir.display()),
            ));
        }
        let Pty { master, slave } = Pty::open()?;
        set_no_echo(&master)?;

        // Each Stdio takes ownership of the descriptor it is given and closes
        // it, so hand out three independent clones and keep `slave` for
        // ourselves. Passing one descriptor to all three would give it three
        // owners; with `OwnedFd` that no longer compiles, which is the point.
        let mut cmd = Command::new(acl2);
        cmd.current_dir(book_dir)
            // TERM=dumb: no cursor addressing or colour to strip back out.
            .env("TERM", "dumb")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave.try_clone()?));
        // `pre_exec` is unsafe by nature -- it runs between fork and exec,
        // where only async-signal-safe operations are legal -- and no wrapper
        // crate can discharge that contract. It is the only `unsafe` left in
        // this crate. The closure must be 'static, so it captures the raw
        // descriptor and re-borrows it in the child, where it is still open.
        let slave_raw = slave.as_raw_fd();
        unsafe {
            cmd.pre_exec(move || become_session_leader(BorrowedFd::borrow_raw(slave_raw)));
        }
        // Say which program failed. Bare "No such file or directory" here is
        // indistinguishable from the *book* path being wrong, which is the
        // other thing that produces it.
        let child = cmd.spawn().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("cannot start ACL2 at {}: {e}", acl2.display()),
            )
        })?;
        drop(slave);

        let tmpdir = std::env::temp_dir().join(format!("acl2-session-{}", child.id()));
        std::fs::create_dir_all(&tmpdir)?;
        // Absolute: it is interpolated into ACL2 source, where a relative path
        // would be resolved against whatever cbd happened to be.
        let cwd = std::fs::canonicalize(book_dir).unwrap_or_else(|_| book_dir.to_path_buf());

        // Adopt the stream before the reader exists, so nothing from a
        // predecessor session can interleave with this one's epoch.
        let epoch = control.adopt();
        let writer = Arc::new(master);
        let shutdown = Arc::new(AtomicBool::new(false));
        let reader = {
            let fd = writer.clone();
            let control = control.clone();
            let shutdown = shutdown.clone();
            std::thread::Builder::new()
                .name("acl2-session-reader".into())
                .spawn(move || reader_loop(fd, control, epoch, shutdown))?
        };

        let mut s = Session {
            child,
            writer,
            cwd,
            tmpdir,
            generation: 0,
            inflight: None,
            control,
            shutdown,
            reader: Some(reader),
        };
        // Drain the startup banner and everything the customization file
        // prints, by handshake rather than by looking for a prompt.
        //
        // Prompt matching does not work here and is a trap worth naming: while
        // `acl2-customization.lsp` runs its `ld` forms, ACL2 emits *nested*
        // prompts (`ACL2 !>>`), so a scan for the prompt stops partway through
        // startup and every later form then reports the previous one's tail.
        // A sentinel we chose cannot appear early by accident. This is P2
        // applied to startup: never parse ACL2's prose for control flow.
        s.handshake(Duration::from_secs(120))?;
        Ok(s)
    }

    /// The control block shared with lock-free readers. The owner of several
    /// sessions hands this same block to each replacement session it starts.
    pub fn control(&self) -> Arc<SessionControl> {
        self.control.clone()
    }

    /// The ACL2 process id. Exposed for tests that must be able to kill the
    /// child out from under the session and watch the bookkeeping notice.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether the ACL2 process itself is still there.
    ///
    /// `try_wait` rather than bookkeeping: `inflight` says a form was
    /// submitted and never reported, which is exactly the lie a crashed child
    /// leaves behind. An error from `try_wait` counts as alive, because
    /// condemning a working session is the worse mistake.
    pub fn alive(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// Round-trip a sentinel and discard everything before it. Returns false
    /// if the session did not answer within `deadline`.
    fn handshake(&mut self, deadline: Duration) -> io::Result<bool> {
        self.generation += 1;
        let genid = self.generation;
        let done = format!("<<<ASS-DONE {genid}>>>");
        let line = format!(
            "(acl2::prog2$ (acl2::cw \"~%<<<ASS-DONE {genid}>>>~%\") \
             (acl2::value :invisible))\n"
        );
        self.write_all(line.as_bytes())?;
        let from = self.control.consumed_offset();
        match self.control.wait_for_marker(done.as_bytes(), from, deadline) {
            WaitEnd::Found(end) => {
                self.control.consume_to(end);
                Ok(true)
            }
            // Keep what was read -- it stays in the stream: a failed
            // handshake during an interrupt escalation must not eat the
            // output the form printed meanwhile.
            _ => Ok(false),
        }
    }

    /// A trivial round trip, for asking "are you answering?" without putting
    /// anything in flight.
    ///
    /// This is what a health probe must use. Probing with `submit` marks the
    /// session busy until the probe reports, so a probe that timed out on a
    /// slow-but-alive session would *wedge* it -- and a later resume would
    /// solemnly report the probe as a finished eval. A handshake that times
    /// out leaves nothing behind but a stale marker in the stream, which the
    /// sentinel stripper removes from any later capture.
    pub fn ping(&mut self, deadline: Duration) -> io::Result<bool> {
        if self.inflight.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "a form is still running in this session",
            ));
        }
        self.handshake(deadline)
    }

    /// Submit one form and wait up to `deadline` for it to finish.
    ///
    /// The form is written to a temp file and `ld`-ed rather than typed at the
    /// REPL. A form the splitter mis-scans would otherwise provoke a raw-Lisp
    /// *reader* error at top level, which aborts and discards pending input --
    /// including our own completion sentinel -- leaving the session hung with
    /// no result. Inside an `ld`, reader errors are trapped by
    /// `:ld-error-action` and come back as an ordinary failure.
    ///
    /// Completion is detected by sentinels we choose, never by parsing ACL2's
    /// prose: its wording varies by event kind, by package, and even by
    /// underlying Lisp. The generation counter -- rather than a form index --
    /// ensures that markers left over from an aborted submission cannot be
    /// mistaken for this one's.
    pub fn submit(&mut self, form: &str, deadline: Duration) -> io::Result<Submission> {
        if self.inflight.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "a form is still running in this session; resume, interrupt, or wait",
            ));
        }
        self.generation += 1;
        let genid = self.generation;
        let path = self.tmpdir.join(format!("form-{genid}.lisp"));

        // OK is the last thing in the file, so `:ld-error-action :return!`
        // means a failing form never reaches it. DONE is printed by the REPL
        // afterwards and therefore always arrives.
        // `set-cbd` first, and this is not optional. `ld` sets the connected
        // book directory to the directory of the file it is loading, so
        // without this every relative `include-book` in a submitted form would
        // be resolved against our temp directory -- and relative includes are
        // the norm in real books. The symptom is a book that syncs fine until
        // its first `(include-book "internal/whatever")`, which reports that a
        // file under /tmp does not exist.
        //
        // Both interpolated paths are escaped: `"` and `\` are legal in
        // directory names, and unescaped they turn the wrapper into a reader
        // error about a string that never closes.
        //
        // `(value :invisible)` keeps ACL2 from printing the marker form's own
        // return value, which would otherwise leave a stray `nil` in the
        // captured output of every single form.
        let cwd = lisp_string(&format!("{}/", self.cwd.display()));
        std::fs::write(
            &path,
            format!(
                "(acl2::er-progn (acl2::set-cbd \"{cwd}\") \
                   (acl2::value :invisible))\n\
                 {form}\n(acl2::prog2$ (acl2::cw \"~%<<<ASS-OK {genid}>>>~%\") \
                 (acl2::value :invisible))\n"
            ),
        )?;

        // Everything is acl2::-qualified so the wrapper reads correctly
        // whatever package the session is currently in.
        //
        // `:ld-verbose nil` suppresses the header `ld` otherwise prints on
        // every call (the project-dir-alist and "Type :help for help" lines),
        // which would otherwise be prepended to every single form's output and
        // look exactly like startup banner leaking across forms.
        // `:ld-prompt nil` suppresses the nested `!>>` prompts.
        // `ld-post-eval-print` is deliberately left at its default so that a
        // query form's *value* is still printed -- that is the whole point of
        // submitting one.
        // The `ld` is consumed by an `mv-let` so that its own return value
        // (`:eof`) is not printed either, and the whole submission is a single
        // silent REPL form that emits exactly one marker.
        let line = format!(
            "(acl2::mv-let (acl2::erp acl2::val acl2::state) \
               (acl2::ld \"{}\" :ld-error-action :return! \
                          :ld-verbose acl2::nil :ld-prompt acl2::nil) \
               (acl2::declare (acl2::ignore acl2::erp acl2::val)) \
               (acl2::prog2$ (acl2::cw \"~%<<<ASS-DONE {genid}>>>~%\") \
                             (acl2::value :invisible)))\n",
            lisp_string(&path.display().to_string())
        );
        self.control.begin();
        if let Err(e) = self.write_all(line.as_bytes()) {
            self.control.end();
            return Err(e);
        }
        self.inflight = Some(InFlight { generation: genid, started: Instant::now(), raw: false });
        self.await_completion(deadline)
    }

    /// Submit one short, lexically validated form raw at the REPL.
    ///
    /// This exists for exactly one caller: making a top-level
    /// `(in-package "P")` *stick*. Inside an `ld` the reader's package switch
    /// reverts when the `ld` exits, so the only way to change the session's
    /// persistent package is to type the form at the top level, the way a
    /// person would.
    ///
    /// Raw submission carries no failure signal -- the top level continues
    /// past a soft error, so an OK sentinel would print regardless -- which is
    /// why this must only be used for forms already probed through the normal
    /// wrapper, and why the caller verifies the effect rather than trusting
    /// the outcome. `Ok` here means "completed", nothing more.
    pub fn submit_raw(&mut self, form: &str, deadline: Duration) -> io::Result<Submission> {
        if self.inflight.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "a form is still running in this session; resume, interrupt, or wait",
            ));
        }
        self.generation += 1;
        let genid = self.generation;
        // The completion sentinel is fully qualified, so it reads correctly
        // in whatever package the submitted form just switched to.
        let line = format!(
            "{form}\n(acl2::prog2$ (acl2::cw \"~%<<<ASS-DONE {genid}>>>~%\") \
             (acl2::value :invisible))\n"
        );
        self.control.begin();
        if let Err(e) = self.write_all(line.as_bytes()) {
            self.control.end();
            return Err(e);
        }
        self.inflight = Some(InFlight { generation: genid, started: Instant::now(), raw: true });
        self.await_completion(deadline)
    }

    /// Keep waiting for the form already in flight.
    ///
    /// Returns `None` when the session is idle. This is what makes a deadline
    /// bound the *call* rather than the *proof*: an expired deadline hands
    /// control back without disturbing ACL2, and the caller decides whether to
    /// wait longer, ask for progress, or interrupt.
    pub fn resume(&mut self, deadline: Duration) -> io::Result<Option<Submission>> {
        if self.inflight.is_none() {
            return Ok(None);
        }
        self.await_completion(deadline).map(Some)
    }

    fn await_completion(&mut self, deadline: Duration) -> io::Result<Submission> {
        let Some(InFlight { generation: genid, started, raw }) = self.inflight else {
            return Err(io::Error::other("nothing in flight"));
        };
        let done = format!("<<<ASS-DONE {genid}>>>");
        let from = self.control.consumed_offset();
        let end = match self.control.wait_for_marker(done.as_bytes(), from, deadline) {
            WaitEnd::Found(end) => end,
            WaitEnd::Eof if !self.alive() => {
                // The child is gone; the form will never report. Without this
                // check nothing ever completes and the bookkeeping says
                // "busy", forever, about a corpse. The partial output stays
                // in the stream for `captured_so_far`.
                self.inflight = None;
                self.control.end();
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the ACL2 process exited mid-form (crashed or killed); \
                     close the session and open it again",
                ));
            }
            // Timeout, abort, or an EOF with the child somehow still alive
            // (its descriptors handed to something else): the form is still
            // in flight and the stream keeps accumulating.
            _ => {
                return Ok(Submission {
                    outcome: Outcome::StillRunning,
                    output: String::new(),
                    data: Vec::new(),
                    blocks: Vec::new(),
                    elapsed: started.elapsed(),
                });
            }
        };
        // This form's capture is the stream up to its marker; anything past
        // the marker belongs to whatever comes next.
        let kept = self.control.consume_to(end);
        self.inflight = None;
        self.control.end();
        // One decode for the whole capture. Doing this per read chunk split
        // multi-byte sequences at arbitrary 64 KB boundaries.
        let captured = String::from_utf8_lossy(&kept).into_owned();
        let outcome = if raw || captured.contains(&format!("<<<ASS-OK {genid}>>>")) {
            Outcome::Ok
        } else {
            Outcome::Failed
        };
        let (output, data, blocks) = strip_sentinels(&captured, genid);
        Ok(Submission { outcome, output, data, blocks, elapsed: started.elapsed() })
    }

    /// Whether ACL2 is still working on a submitted form.
    pub fn is_running(&self) -> bool {
        self.inflight.is_some()
    }

    /// Progress on the form in flight: how long it has been running, and the
    /// tail of what it has printed so far.
    ///
    /// The reader thread has already ingested everything available, so this
    /// is a snapshot, not a drain; safe to call repeatedly.
    pub fn progress(&mut self, tail_lines: usize) -> io::Result<Option<Progress>> {
        let Some(InFlight { started, .. }) = self.inflight else {
            return Ok(None);
        };
        let bytes = self.control.peek_all();
        let text = String::from_utf8_lossy(&bytes);
        let tail: Vec<&str> = text
            .lines()
            .rev()
            .take(tail_lines)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        Ok(Some(Progress {
            elapsed: started.elapsed(),
            tail: tail.join("\n"),
            bytes_so_far: bytes.len(),
        }))
    }

    /// Everything read for the form in flight, so far.
    ///
    /// A form that is interrupted never reports, so its output would otherwise
    /// be unreachable -- and a proof interrupted because it was going badly is
    /// exactly the one whose partial output you want. A peek, not a drain:
    /// the bytes stay in the stream.
    pub fn captured_so_far(&mut self) -> String {
        String::from_utf8_lossy(&self.control.peek_all()).into_owned()
    }

    /// Interrupt a running form.
    ///
    /// This is an escalating loop, not a single signal, and it verifies rather
    /// than assuming. Under ACL2's default `abort-soft`, one SIGINT only
    /// *schedules* an exit at the prover's next safe point; a proof grinding
    /// inside a raw-Lisp executable counterpart never reaches one. Measured: a
    /// single SIGINT was still pending after twenty seconds, while a second
    /// returned the session cleanly. Fire-and-forget interrupts are exactly
    /// what produces the "interrupt twice" ritual users complain about.
    ///
    /// Verified to behave identically on ACL2 images built on SBCL and CCL.
    pub fn interrupt(&mut self) -> io::Result<bool> {
        // A stale abort request must not cut the next call short; whoever
        // asked for this interrupt is being answered right now.
        self.control.clear_abort();
        for _ in 0..INTERRUPT_TRIES {
            if let Some(pid) = rustix::process::Pid::from_raw(self.child.id() as i32) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::INT);
            }
            // Verify by handshake: a session that answers a round-trip is
            // genuinely back, whereas a prompt in the output proves nothing
            // (ACL2 prints nested prompts while an `ld` is still running).
            if self.handshake(INTERRUPT_STEP)? {
                // The interrupted form will never report; stop waiting on it.
                self.inflight = None;
                self.control.end();
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn write_all(&mut self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let n = rustix::io::write(self.writer.as_fd(), buf)?;
            if n == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
            buf = &buf[n..];
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Kill first: the child's death closes the slave side, which is what
        // wakes a reader blocked on the descriptor. The shutdown flag covers
        // the corner where something inherited the slave and keeps the PTY
        // open; the reader notices within its poll.
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
        // `writer` is the last owner of the master once the reader is gone.
        let _ = std::fs::remove_dir_all(&self.tmpdir);
    }
}

/// Find `needle` in `hay`, returning its offset.
fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// Escape a string for interpolation into Lisp source between double quotes.
fn lisp_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Whether a line carries an OK/DONE completion marker of *any* generation.
///
/// Outcome decisions match the exact current generation, and only that -- a
/// stale OK must never vouch for this form. But for display the rule is the
/// reverse: a marker from a timed-out health handshake or an aborted
/// submission is our bookkeeping wherever it surfaces, and leaving it in the
/// output because the generation moved on just shows the client a magic
/// string it should never have seen.
fn is_completion_marker(l: &str) -> bool {
    for pat in ["<<<ASS-OK ", "<<<ASS-DONE "] {
        if let Some(i) = l.find(pat)
            && let Some(end) = l[i + pat.len()..].find(">>>")
        {
            let id = &l[i + pat.len()..i + pat.len() + end];
            if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

/// What a capture separates into: the display text, the one-line `ASS-DATA`
/// pairs, and the `ASS-BEGIN`/`ASS-END` blocks.
type Stripped = (String, Vec<(String, String)>, Vec<(String, String)>);

/// Remove our own markers, returning what ACL2 printed plus any structured
/// values reported via `<<<ASS-DATA key value>>>`.
///
/// Also trims the prompt fragment that ACL2 emits *before* reading our line and
/// which therefore lands at the head of the capture. This is cosmetic only --
/// the prompt is never used to decide anything (P2); it is just noise at the
/// front of otherwise clean output.
fn strip_sentinels(s: &str, _genid: u64) -> Stripped {
    let mut data = Vec::new();
    let mut blocks: Vec<(String, String)> = Vec::new();
    let mut kept: Vec<&str> = Vec::new();
    let mut open: Option<(String, Vec<&str>)> = None;
    for l in s.lines() {
        if is_completion_marker(l) {
            continue;
        }
        // Multi-line block delimiters. Collected out of the output entirely,
        // so a block's contents never appear twice.
        if let Some(i) = l.find("<<<ASS-BEGIN ")
            && let Some(end) = l[i + 13..].find(">>>")
        {
            open = Some((l[i + 13..i + 13 + end].trim().to_string(), Vec::new()));
            continue;
        }
        if let Some(i) = l.find("<<<ASS-END ")
            && let Some(end) = l[i + 11..].find(">>>")
        {
            let key = l[i + 11..i + 11 + end].trim().to_string();
            if let Some((k, body)) = open.take() {
                if k == key {
                    blocks.push((k, body.join("\n").trim().to_string()));
                    continue;
                }
                open = Some((k, body));
            }
            continue;
        }
        if let Some((_, body)) = open.as_mut() {
            body.push(l);
            continue;
        }
        // Our planted labels: `with-output` hides their summary, but `ld`
        // still echoes the event name as the form's value. It is our
        // bookkeeping, not something ACL2 said about the user's form. The
        // name may arrive package-qualified (`ACL2::ACL2-SESSION-MARK-3`)
        // when the session's current package is not "ACL2".
        {
            let t = l.trim();
            let t = t.strip_prefix("ACL2::").or_else(|| t.strip_prefix("acl2::")).unwrap_or(t);
            if t.to_ascii_lowercase().starts_with("acl2-session-mark-")
                && t.len() < 40
                && t["acl2-session-mark-".len()..].chars().all(|c| c.is_ascii_digit())
            {
                continue;
            }
        }
        if let Some(rest) = l.find("<<<ASS-DATA ").map(|i| &l[i + 12..])
            && let Some(end) = rest.find(">>>")
        {
            let mut it = rest[..end].splitn(2, ' ');
            if let (Some(k), Some(v)) = (it.next(), it.next()) {
                data.push((k.to_string(), v.trim().to_string()));
                continue;
            }
        }
        kept.push(l);
    }
    // Strip leading newlines *first*: with them attached, the "is the prompt
    // on the first line?" test below counts an empty leading line and never
    // fires.
    let mut out = kept.join("\n").trim_start_matches(['\n', '\r']).to_string();
    // Drop a leading "PKG !>" left over from the prompt preceding our input.
    if let Some(i) = out.find("!>")
        && !out[..i].contains('\n')
    {
        out = out[i + 2..].to_string();
    }
    (out.trim_start_matches(['\n', '\r']).to_string(), data, blocks)
}
