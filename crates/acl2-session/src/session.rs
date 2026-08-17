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

use crate::output::OutputSlice;
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

/// How long EOF waits for a dying child to be reaped before deciding what
/// to call the death. See `await_completion`.
const EOF_GRACE: Duration = Duration::from_millis(250);

/// How much of the in-flight output a lock-free status snapshot copies.
/// Bounds the copy, not the stream; the full text is in the capture.
const SNAPSHOT_TAIL: usize = 16384;

/// Deadlines are capped so `Instant` arithmetic cannot overflow; a month is
/// "forever" for a tool call.
const DEADLINE_CAP: Duration = Duration::from_secs(60 * 60 * 24 * 30);

/// How long startup may take before the session is declared not to have
/// come up. Generous because the customization file may build packages.
const STARTUP_DEADLINE: Duration = Duration::from_secs(120);

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
    /// The output handle the running form's log will land under, once it has
    /// one. Kept beside the stream rather than on the `Book` so that a reader
    /// can recognise an in-flight handle without the book lock -- which is
    /// the whole reason reading a log works while the prover is busy.
    in_flight: Option<String>,
    /// A certification in progress: when it started and for which book.
    /// Belongs to the *book*, not the session -- `adopt` leaves it alone,
    /// since a rebuild does not touch a running build.
    build: Option<(Instant, PathBuf)>,
}

/// How much of a live buffer a tail read looks at: enough lines to survive
/// stripping, and a hard byte cap so one enormous line cannot defeat it.
const TAIL_WINDOW_LINES: usize = 4000;
const TAIL_WINDOW_BYTES: usize = 512 * 1024;

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
                in_flight: None,
                build: None,
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

    /// Name the handle the running form's output will be filed under.
    pub fn set_in_flight(&self, r: Option<String>) {
        self.lock().in_flight = r;
    }

    /// A slice of the running form's log, if `r` is its handle.
    ///
    /// The reason a log can be read while the prover is busy: this takes the
    /// stream mutex, which the reader thread holds only long enough to append,
    /// and never the book lock, which a proof holds for as long as it runs.
    ///
    /// Bytes are copied under the lock and decoded outside it. Decoding a
    /// ten-megabyte log takes long enough to matter, and doing it under the
    /// lock would stall the very reader that is filling the buffer. A tail --
    /// what a live view actually asks for, twice a second -- copies only the
    /// tail, so the common case touches neither.
    pub fn live_slice(&self, r: &str, slice: crate::output::Slice) -> Option<OutputSlice> {
        use crate::output::Slice as S;
        let (bytes, _raw_len) = {
            let g = self.lock();
            if !g.running || g.in_flight.as_deref() != Some(r) {
                return None;
            }
            let from = (g.form_start.max(g.base) - g.base) as usize;
            let form = &g.buf[from.min(g.buf.len())..];
            // Counted over raw bytes: a newline is a newline in UTF-8, so
            // this needs no decode, and the caller is told how much it is not
            // being shown even when it asked for forty lines of a hundred
            // thousand.
            match &slice {
                // A generous raw window, then stripped, then cut to n lines:
                // stripping removes lines, so taking exactly n raw lines
                // would come up short. The window is bounded so that a tail
                // twice a second stays cheap on a log of any size.
                S::Tail(n) => {
                    let want = (*n * 3 + 64).min(TAIL_WINDOW_LINES);
                    let bytes = crate::output::tail_bytes(form, want);
                    let cap = bytes.len().min(TAIL_WINDOW_BYTES);
                    (bytes[bytes.len() - cap..].to_vec(), form.len())
                }
                _ => (form.to_vec(), form.len()),
            }
        };
        let shown = display_of(&String::from_utf8_lossy(&bytes));
        // Counted on the stripped text, so it agrees with what the finished
        // log will say rather than with the raw stream. For a bounded tail it
        // is what that window holds, which is the honest answer available
        // without walking the whole buffer.
        let total_lines = crate::output::count_lines(&shown);
        let text = match slice {
            S::Tail(n) => {
                let lines: Vec<&str> = shown.lines().collect();
                lines[lines.len().saturating_sub(n)..].join("\n")
            }
            other => crate::output::slice_text(&shown, other),
        };
        // A form still running has not been rolled back; it has not finished
        // going in.
        Some(OutputSlice { text, total_lines, superseded: false })
    }

    /// Everything the running form has printed, if `r` is its handle.
    ///
    /// `None` when nothing is running, or when `r` names some other form's
    /// log -- in which case the caller should read the file, which is
    /// finished and will not change again.
    ///
    /// Returns bytes rather than a `String` because the tail of a live buffer
    /// can end mid-character: the reader thread has half a UTF-8 sequence and
    /// the rest is still in the pty. Decoding is the caller's, once it knows
    /// how much it wants.
    pub fn live_form(&self, r: &str) -> Option<Vec<u8>> {
        let g = self.lock();
        if !g.running || g.in_flight.as_deref() != Some(r) {
            return None;
        }
        let from = (g.form_start.max(g.base) - g.base) as usize;
        Some(g.buf[from.min(g.buf.len())..].to_vec())
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

    /// Wait up to `dur` for an abort request, consuming it.
    ///
    /// For calls that block holding the book's lock on something that is
    /// *not* a session submission -- certification's wait -- so an interrupt
    /// still cuts through at once instead of after the call's deadline.
    pub(crate) fn wait_abort(&self, dur: Duration) -> bool {
        let deadline = Instant::now() + dur.min(DEADLINE_CAP);
        let mut g = self.lock();
        loop {
            if g.abort {
                g.abort = false;
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (g2, _) = self
                .cond
                .wait_timeout(g, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            g = g2;
        }
    }

    /// Note that a certification build started, for lock-free status readers.
    pub fn begin_build(&self, book: PathBuf) {
        self.lock().build = Some((Instant::now(), book));
    }

    pub fn end_build(&self) {
        self.lock().build = None;
    }

    /// The running build's elapsed time and book, without any book lock.
    pub fn build_snapshot(&self) -> Option<(Duration, PathBuf)> {
        self.lock().build.as_ref().map(|(t, b)| (t.elapsed(), b.clone()))
    }

    /// Where the unconsumed region begins.
    fn consumed_offset(&self) -> u64 {
        self.lock().base
    }

    /// Everything printed and not yet consumed, as of now.
    fn peek_all(&self) -> Vec<u8> {
        self.lock().buf.clone()
    }

    /// A bounded copy of the unconsumed tail, plus the total unconsumed
    /// length. For displays that want a tail, not a multi-megabyte clone of
    /// a noisy form's whole stream.
    fn peek_tail(&self, max: usize) -> (Vec<u8>, usize) {
        let g = self.lock();
        let from = g.buf.len().saturating_sub(max);
        (g.buf[from..].to_vec(), g.buf.len())
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
        // Guarded from here: everything below can still fail, and until the
        // `Session` exists nothing else would kill this child.
        let child = SpawnedChild(Some(cmd.spawn().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("cannot start ACL2 at {}: {e}", acl2.display()),
            )
        })?));
        // Both of these matter, and the second is easy to miss: `Command`
        // *owns* the three Stdio clones until it drops, and this function
        // does not return until the startup handshake resolves. Held here,
        // the parent itself keeps the slave side open, no EOF can ever reach
        // the reader, and an image that dies at startup reads as a two-minute
        // silence instead of an exit. Measured, not hypothesised.
        drop(cmd);
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
            child: child.into_inner(),
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
        //
        // And the answer is *checked*. An "image" is often a wrapper script,
        // and a script that dies -- an unset variable, a missing core -- used
        // to yield an apparently open session whose first submission then
        // failed "mid-form" with advice to reopen: wrong diagnosis, wrong
        // remedy, and the script's own complaint nowhere. Whatever the child
        // printed IS the diagnosis, so the error carries it.
        let answered = match s.handshake(STARTUP_DEADLINE) {
            Ok(a) => a,
            // The write itself can fail if the child died first; that is the
            // same startup failure, not an I/O mystery.
            Err(_) if !s.alive() => false,
            Err(e) => return Err(e),
        };
        if !answered {
            let said = tail_text(&s.control.peek_all(), 2048);
            let said = if said.is_empty() {
                "it printed nothing".to_string()
            } else {
                format!("it printed:\n{said}")
            };
            let what = if s.alive() {
                format!(
                    "did not answer within {}s of starting; an \
                     acl2-customization.lsp may be hung or awaiting input",
                    STARTUP_DEADLINE.as_secs()
                )
            } else {
                "exited during startup".to_string()
            };
            return Err(io::Error::other(format!(
                "ACL2 at {} {what}; {said}",
                acl2.display()
            )));
        }
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
            WaitEnd::Eof => {
                // EOF is authoritative about the bytes; which *explanation*
                // to give needs a moment's grace. A child killed a
                // millisecond ago has closed its descriptors but may not be
                // reaped yet, and reporting "the terminal closed while the
                // process survives" for an ordinary crash is worse than
                // waiting a few tens of milliseconds to be sure.
                let grace = Instant::now();
                while self.alive() && grace.elapsed() < EOF_GRACE {
                    std::thread::sleep(Duration::from_millis(10));
                }
                // No more bytes are ever coming, so the marker cannot arrive
                // and "still running" would be a permanent lie -- reported
                // instantly on every resume, since the EOF short-circuits the
                // wait. Say what actually happened; the partial output stays
                // in the stream for `captured_so_far`.
                self.inflight = None;
                self.control.end();
                let msg = if self.alive() {
                    "the ACL2 session's terminal closed while the process \
                     survives (something else may hold its descriptors); the \
                     form cannot report, so close the session and open it again"
                } else {
                    "the ACL2 process exited mid-form (crashed or killed); \
                     close the session and open it again"
                };
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, msg));
            }
            WaitEnd::Timeout | WaitEnd::Aborted => {
                // The form is still in flight and the stream keeps
                // accumulating; the caller decides what to do with the time.
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
        // The submission's temp file is dead once its form completes; left
        // in place, a long session accumulates one per submission for its
        // whole life. Raw submissions have none; removal is best-effort
        // either way.
        let _ = std::fs::remove_file(self.tmpdir.join(format!("form-{genid}.lisp")));
        // One decode for the whole capture. Doing this per read chunk split
        // multi-byte sequences at arbitrary 64 KB boundaries.
        let captured = String::from_utf8_lossy(&kept).into_owned();
        let outcome = if raw || captured.contains(&format!("<<<ASS-OK {genid}>>>")) {
            Outcome::Ok
        } else {
            Outcome::Failed
        };
        let (output, data, blocks) = strip_sentinels(&captured);
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
        // A bounded tail, not the whole stream: a noisy form's stream runs to
        // megabytes, and cloning all of it to render twenty lines was real
        // memcpy on every status call.
        let (bytes, total) = self.control.peek_tail(64 * 1024);
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
            bytes_so_far: total,
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

/// Owns a spawned ACL2 until the [`Session`] that will kill it exists.
///
/// `std::process::Child` deliberately does *not* kill on drop, and `start`
/// does fallible work between the spawn and the struct -- a temp directory,
/// a reader thread. An early return there would leave a resident image, a
/// few hundred megabytes of it, running with nothing reading its PTY and
/// nobody left holding a handle: the abandoned-ACL2 shape that
/// once took a machine down. Disarmed by `into_inner` once the `Session`
/// takes over.
struct SpawnedChild(Option<Child>);

impl SpawnedChild {
    fn id(&self) -> u32 {
        self.0.as_ref().expect("armed until into_inner").id()
    }

    fn into_inner(mut self) -> Child {
        self.0.take().expect("into_inner called once")
    }
}

impl Drop for SpawnedChild {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Session {
    /// End this session now, rather than when it drops.
    ///
    /// For the caller that has to abandon a session it still owns. `start`
    /// adopts the stream before it can still fail, so a failed replacement
    /// leaves the session being replaced holding a reader whose epoch no
    /// longer matches: it is alive and printing, and every byte is dropped.
    /// Kept, it would answer nothing while looking merely slow -- `alive` is
    /// true, so `health` waits out its deadline and calls ACL2 unresponsive
    /// for what is our own bookkeeping. Killed, `alive` is false and the
    /// state reported is the true one.
    ///
    /// Idempotent, because `Drop` runs it too.
    pub fn terminate(&mut self) {
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

impl Drop for Session {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Find `needle` in `hay`, returning its offset.
fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// The last `max` bytes of `bytes`, decoded lossily and trimmed. For error
/// messages that must carry what a child actually printed.
fn tail_text(bytes: &[u8], max: usize) -> String {
    let tail = &bytes[bytes.len().saturating_sub(max)..];
    String::from_utf8_lossy(tail).trim().to_string()
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

/// Just the display half of `strip_sentinels`, for a capture still being
/// written.
///
/// A live read has to strip exactly what a finished one strips, or the log a
/// form is watched through and the log it leaves behind are different
/// documents. They diverge the moment the form ends: ACL2 runs our probes
/// before anything consumes the stream, so a reader watching at that instant
/// sees `<<<ASS-BEGIN evd>>>` and our planted labels, and then watches them
/// vanish. Measured: 77 raw lines against the 45 the same form filed.
pub(crate) fn display_of(raw: &str) -> String {
    // Stop at this form's completion marker. Everything past it is the prompt
    // for whatever comes next, which a finished capture never carries because
    // consumption cuts exactly there -- so without this the live view shows
    // an `ACL2 !>` that disappears the moment the form is filed.
    let upto: String = match raw.lines().position(is_completion_marker) {
        Some(i) => raw.lines().take(i).collect::<Vec<_>>().join("\n"),
        None => raw.to_string(),
    };
    strip_sentinels(&upto).0
}

/// Remove our own markers, returning what ACL2 printed plus any structured
/// values reported via `<<<ASS-DATA key value>>>`.
///
/// Also trims the prompt fragment that ACL2 emits *before* reading our line and
/// which therefore lands at the head of the capture. This is cosmetic only --
/// the prompt is never used to decide anything (P2); it is just noise at the
/// front of otherwise clean output.
fn strip_sentinels(s: &str) -> Stripped {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether a pid is still signalable. Reaped children are not, so this
    /// distinguishes "killed and waited for" from "still resident".
    fn signalable(pid: u32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            // "No such process" on stderr is the answer, not a problem.
            .stderr(Stdio::null())
            .status()
            .expect("run kill")
            .success()
    }

    fn probe() -> Child {
        Command::new("sleep").arg("300").spawn().expect("spawn sleep")
    }

    /// The guard's whole reason to exist: `start` can fail after the spawn,
    /// and `Child` does not kill on drop, so an abandoned image would stay
    /// resident with nothing reading its PTY and no handle left to kill it.
    #[test]
    fn an_abandoned_child_is_killed_and_reaped() {
        let child = probe();
        let pid = child.id();
        let guard = SpawnedChild(Some(child));
        assert!(signalable(pid), "the probe is running to begin with");
        drop(guard);
        assert!(!signalable(pid), "leaving early takes the child with it");
    }

    /// And once the `Session` owns it, the guard must keep its hands off --
    /// otherwise every successful start would kill the ACL2 it just made.
    #[test]
    fn a_child_handed_over_is_left_running() {
        let child = probe();
        let pid = child.id();
        let mut child = SpawnedChild(Some(child)).into_inner();
        assert!(signalable(pid), "the hand-over does not disturb it");
        let _ = child.kill();
        let _ = child.wait();
    }
}
