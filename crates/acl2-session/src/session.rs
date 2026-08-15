//! A live ACL2 session: one process, one book directory, one form at a time.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::pty::{become_session_leader, set_no_echo, wait_readable, Pty};

/// How long to wait between escalating interrupts before giving up on a
/// graceful abort.
const INTERRUPT_STEP: Duration = Duration::from_millis(1500);
const INTERRUPT_TRIES: usize = 4;

/// The longest a blocked read waits before looking up from the descriptor.
///
/// This is the reaction time to an abort request: a caller blocked for its
/// whole deadline would otherwise be exactly the wall an interrupt cannot get
/// past, since every call on a session serialises on one lock. Ten polls a
/// second costs nothing measurable and bounds that wait at a tenth of a
/// second.
const POLL_SLICE: Duration = Duration::from_millis(100);

/// How much of the in-flight output the status mirror retains.
const MIRROR_TAIL: usize = 4096;

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

/// The handle other threads hold on a running session.
///
/// Every call on a session serialises on one lock, and a blocking call holds
/// it for its whole deadline. This block lives *outside* that lock so that the
/// two things which must not wait behind a ten-minute proof do not:
///
/// - **Abort.** `request_abort` sets a flag the blocked reader polls every
///   [`POLL_SLICE`]; the blocked call returns early exactly as if its deadline
///   had expired, the lock frees, and the interrupt's own escalation runs
///   within a poll slice instead of after the caller's deadline.
/// - **Status.** The reader keeps a small mirror of the in-flight form's
///   progress here, so "how far along is it?" needs no lock at all.
///
/// One control block belongs to one [`sync::Book`](crate::sync::Book) for its
/// whole life and is threaded into every `Session` the book starts, so a
/// rebuild does not orphan the handle a registry already gave out.
///
/// This is the interim shape of a fuller design in which one reader thread
/// owns the PTY and every waiter consumes from a shared buffer; that version
/// also frees `acl2_output`-style readers from the lock, and is the end
/// state. The flag-and-mirror gets the two capabilities
/// that matter without restructuring the session's ownership story.
pub struct SessionControl {
    /// Set to ask whatever call is blocked on the session to hand back
    /// control. Consumed by the reader; cleared by the interrupt path before
    /// escalation so a stale request cannot cut the *next* call short.
    ///
    /// Relaxed ordering: no data is published through the flag, it is only a
    /// nudge that something else wants the lock.
    abort: AtomicBool,
    mirror: Mutex<Mirror>,
}

struct Mirror {
    running: bool,
    started: Option<Instant>,
    bytes: usize,
    /// Set by the book layer before a unit's submission; cleared when a form
    /// completes. `begin` leaves it alone, because the book sets it first.
    unit: Option<usize>,
    tail: Vec<u8>,
}

impl SessionControl {
    pub fn new() -> Arc<SessionControl> {
        Arc::new(SessionControl {
            abort: AtomicBool::new(false),
            mirror: Mutex::new(Mirror {
                running: false,
                started: None,
                bytes: 0,
                unit: None,
                tail: Vec::new(),
            }),
        })
    }

    /// Name the file unit about to run, for lock-free status readers.
    pub fn set_unit(&self, unit: Option<usize>) {
        if let Ok(mut m) = self.mirror.lock() {
            m.unit = unit;
        }
    }

    /// Ask the call currently blocked on this session to return early.
    pub fn request_abort(&self) {
        self.abort.store(true, Ordering::Relaxed);
    }

    pub fn clear_abort(&self) {
        self.abort.store(false, Ordering::Relaxed);
    }

    fn take_abort(&self) -> bool {
        self.abort.swap(false, Ordering::Relaxed)
    }

    /// Progress of the form in flight, or `None` when nothing is.
    pub fn snapshot(&self) -> Option<StatusSnapshot> {
        let m = self.mirror.lock().ok()?;
        if !m.running {
            return None;
        }
        Some(StatusSnapshot {
            elapsed: m.started.map(|s| s.elapsed()).unwrap_or_default(),
            bytes_so_far: m.bytes,
            unit: m.unit,
            tail: String::from_utf8_lossy(&m.tail).into_owned(),
        })
    }

    fn begin(&self) {
        if let Ok(mut m) = self.mirror.lock() {
            m.running = true;
            m.started = Some(Instant::now());
            m.bytes = 0;
            m.tail.clear();
        }
    }

    fn end(&self) {
        if let Ok(mut m) = self.mirror.lock() {
            m.running = false;
            m.unit = None;
        }
    }

    fn update(&self, buf: &[u8]) {
        if let Ok(mut m) = self.mirror.lock() {
            if m.running {
                m.bytes = buf.len();
                let from = buf.len().saturating_sub(MIRROR_TAIL);
                m.tail.clear();
                m.tail.extend_from_slice(&buf[from..]);
            }
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

/// What `read_until` came back with, and why it stopped.
struct ReadEnd {
    buf: Vec<u8>,
    /// The marker was seen.
    found: bool,
    /// The descriptor reported end-of-file or an error: the child has gone
    /// away, or every copy of the slave side has been closed.
    eof: bool,
}

pub struct Session {
    child: Child,
    master: OwnedFd,
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
    /// Output read past the end of the previous form. Carried forward so it is
    /// never misattributed to the next one.
    ///
    /// Bytes, not `String`. Output arrives in arbitrary 64 KB chunks, and a
    /// multi-byte sequence split across a chunk boundary decodes to U+FFFD if
    /// each chunk is decoded alone -- corrupting even perfectly valid UTF-8.
    /// Decoding happens once, at the submission boundary. (ISO-8859-1 high
    /// bytes a book prints still become U+FFFD there; that is the decode
    /// being lossy on genuinely non-UTF-8 text, not a framing artefact.)
    pending: Vec<u8>,
    control: Arc<SessionControl>,
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
    /// `control` is shared with whoever must reach a busy session without its
    /// lock; the caller that owns several sessions over one book passes the
    /// same block to each.
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

        control.clear_abort();
        control.end();
        let mut s = Session {
            child,
            master,
            cwd,
            tmpdir,
            generation: 0,
            inflight: None,
            pending: Vec::new(),
            control,
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

    /// Whether the ACL2 process itself is still there.
    ///
    /// `try_wait` rather than bookkeeping: `inflight` says a form was
    /// submitted and never reported, which is exactly the lie a crashed child
    /// leaves behind -- the EOF reader returns, nothing completes, and the
    /// session reads as busy forever. An error from `try_wait` counts as
    /// alive, because condemning a working session is the worse mistake.
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
        let end = self.read_until(Some(done.as_bytes()), deadline)?;
        if end.found {
            self.split_after(end.buf, done.as_bytes());
            Ok(true)
        } else {
            // Keep what was read: a failed handshake during an interrupt
            // escalation must not eat the output the form printed meanwhile.
            self.pending = end.buf;
            Ok(false)
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

    /// Keep everything up to and including `marker`; carry the remainder
    /// forward as `pending` so it can never be attributed to the next form.
    fn split_after(&mut self, mut buf: Vec<u8>, marker: &[u8]) -> Vec<u8> {
        match find_bytes(&buf, marker) {
            Some(i) => {
                self.pending = buf.split_off(i + marker.len());
                buf
            }
            None => buf,
        }
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
        self.write_all(line.as_bytes())?;

        self.inflight = Some(InFlight { generation: genid, started: Instant::now(), raw: false });
        self.control.begin();
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
        self.write_all(line.as_bytes())?;
        self.inflight = Some(InFlight { generation: genid, started: Instant::now(), raw: true });
        self.control.begin();
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
            return Err(io::Error::new(io::ErrorKind::Other, "nothing in flight"));
        };
        let done = format!("<<<ASS-DONE {genid}>>>");
        let end = self.read_until(Some(done.as_bytes()), deadline)?;

        if !end.found {
            // Keep everything read so far so a later resume continues where
            // this one stopped rather than losing the head of the output.
            self.pending = end.buf;
            if end.eof && !self.alive() {
                // The child is gone; the form will never report. Without this
                // check the EOF reader returns instantly, `resume` says
                // `still_running` on every call, and health says `busy`,
                // forever, about a corpse.
                self.inflight = None;
                self.control.end();
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the ACL2 process exited mid-form (crashed or killed); \
                     close the session and open it again",
                ));
            }
            return Ok(Submission {
                outcome: Outcome::StillRunning,
                output: String::new(),
                data: Vec::new(),
                blocks: Vec::new(),
                elapsed: started.elapsed(),
            });
        }

        // Anything read past our sentinel belongs to whatever comes next.
        let kept = self.split_after(end.buf, done.as_bytes());
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
    /// Drains without blocking, so it is safe to call repeatedly. The tail is
    /// the only thing offered here; anything more belongs behind an output
    /// handle once the form completes.
    pub fn progress(&mut self, tail_lines: usize) -> io::Result<Option<Progress>> {
        let Some(InFlight { started, .. }) = self.inflight else {
            return Ok(None);
        };
        // A short poll rather than the deadline: this reports, it does not wait.
        let end = self.read_until(None, Duration::from_millis(50))?;
        self.pending = end.buf;
        let text = String::from_utf8_lossy(&self.pending);
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
            bytes_so_far: self.pending.len(),
        }))
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
    /// Everything read for the form in flight, so far.
    ///
    /// A form that is interrupted never reports, so its output would otherwise
    /// be unreachable -- and a proof interrupted because it was going badly is
    /// exactly the one whose partial output you want. Call before interrupting:
    /// the interrupt handshake reads more, which would be mixed in.
    pub fn captured_so_far(&mut self) -> String {
        if let Ok(end) = self.read_until(None, Duration::from_millis(50)) {
            self.pending = end.buf;
        }
        String::from_utf8_lossy(&self.pending).into_owned()
    }

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
            let n = rustix::io::write(self.master.as_fd(), buf)?;
            if n == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
            buf = &buf[n..];
        }
        Ok(())
    }

    /// Accumulate output until `marker` appears, the deadline expires, the
    /// child's descriptor reports EOF, or an abort is requested. Waits in
    /// [`POLL_SLICE`] steps so an abort is honoured promptly.
    ///
    /// Only the tail of the buffer is rescanned for the marker on each chunk
    /// -- the unscanned suffix plus one marker-length of overlap -- because
    /// rescanning from the start made a 600k-line capture cost gigabytes of
    /// re-scanning over its lifetime.
    fn read_until(&mut self, marker: Option<&[u8]>, deadline: Duration) -> io::Result<ReadEnd> {
        let mut buf = std::mem::take(&mut self.pending);
        let mut found = match marker {
            Some(m) => find_bytes(&buf, m).is_some(),
            None => false,
        };
        let mut eof = false;
        let mut scanned = buf.len();
        let start = Instant::now();
        let mut chunk = [0u8; 65536];
        while !found {
            if self.control.take_abort() {
                break;
            }
            let left = match deadline.checked_sub(start.elapsed()) {
                Some(d) if !d.is_zero() => d,
                _ => break,
            };
            if !wait_readable(self.master.as_fd(), left.min(POLL_SLICE))? {
                continue;
            }
            let n = match rustix::io::read(self.master.as_fd(), &mut chunk) {
                Ok(0) | Err(_) => {
                    eof = true;
                    break;
                }
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);
            if let Some(m) = marker {
                let from = scanned.saturating_sub(m.len().saturating_sub(1));
                found = find_bytes(&buf[from..], m).is_some();
            }
            scanned = buf.len();
            self.control.update(&buf);
        }
        Ok(ReadEnd { buf, found, eof })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // `master` is an OwnedFd and closes itself.
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
        if let Some(i) = l.find(pat) {
            if let Some(end) = l[i + pat.len()..].find(">>>") {
                let id = &l[i + pat.len()..i + pat.len() + end];
                if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
                    return true;
                }
            }
        }
    }
    false
}

/// Remove our own markers, returning what ACL2 printed plus any structured
/// values reported via `<<<ASS-DATA key value>>>`.
///
/// Also trims the prompt fragment that ACL2 emits *before* reading our line and
/// which therefore lands at the head of the capture. This is cosmetic only --
/// the prompt is never used to decide anything (P2); it is just noise at the
/// front of otherwise clean output.
fn strip_sentinels(s: &str, _genid: u64) -> (String, Vec<(String, String)>, Vec<(String, String)>) {
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
        if let Some(i) = l.find("<<<ASS-BEGIN ") {
            if let Some(end) = l[i + 13..].find(">>>") {
                open = Some((l[i + 13..i + 13 + end].trim().to_string(), Vec::new()));
                continue;
            }
        }
        if let Some(i) = l.find("<<<ASS-END ") {
            if let Some(end) = l[i + 11..].find(">>>") {
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
        if let Some(rest) = l.find("<<<ASS-DATA ").map(|i| &l[i + 12..]) {
            if let Some(end) = rest.find(">>>") {
                let mut it = rest[..end].splitn(2, ' ');
                if let (Some(k), Some(v)) = (it.next(), it.next()) {
                    data.push((k.to_string(), v.trim().to_string()));
                    continue;
                }
            }
        }
        kept.push(l);
    }
    // Strip leading newlines *first*: with them attached, the "is the prompt
    // on the first line?" test below counts an empty leading line and never
    // fires.
    let mut out = kept.join("\n").trim_start_matches(['\n', '\r']).to_string();
    // Drop a leading "PKG !>" left over from the prompt preceding our input.
    if let Some(i) = out.find("!>") {
        if !out[..i].contains('\n') {
            out = out[i + 2..].to_string();
        }
    }
    (out.trim_start_matches(['\n', '\r']).to_string(), data, blocks)
}
