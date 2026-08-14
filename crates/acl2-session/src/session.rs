//! A live ACL2 session: one process, one book directory, one form at a time.

use std::io;
use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::pty::{become_session_leader, set_no_echo, wait_readable, Pty};

/// How long to wait between escalating interrupts before giving up on a
/// graceful abort.
const INTERRUPT_STEP: Duration = Duration::from_millis(1500);
const INTERRUPT_TRIES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The form was admitted.
    Ok,
    /// ACL2 finished with the form but did not admit it.
    Failed,
    /// The deadline expired; the form is still running. Not an error.
    StillRunning,
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

pub struct Session {
    child: Child,
    master: RawFd,
    tmpdir: PathBuf,
    generation: u64,
    /// Output read past the end of the previous form. Carried forward so it is
    /// never misattributed to the next one.
    pending: String,
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
    pub fn start(acl2: &Path, book_dir: &Path) -> io::Result<Session> {
        let pty = Pty::open()?;
        let (master, slave) = (pty.master, pty.slave);
        set_no_echo(master)?;

        // Each Stdio takes ownership of the fd it is given and will close it,
        // so hand out three *duplicates* and keep `slave` for ourselves. Using
        // the same raw fd three times closes it three times, which can shut a
        // completely unrelated descriptor once the number is reused.
        let dup = |fd: RawFd| -> io::Result<Stdio> {
            let d = unsafe { libc::dup(fd) };
            if d < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(unsafe { Stdio::from_raw_fd(d) })
        };
        let mut cmd = Command::new(acl2);
        cmd.current_dir(book_dir)
            // TERM=dumb: no cursor addressing or colour to strip back out.
            .env("TERM", "dumb")
            .stdin(dup(slave)?)
            .stdout(dup(slave)?)
            .stderr(dup(slave)?);
        unsafe {
            cmd.pre_exec(move || become_session_leader(slave));
        }
        let child = cmd.spawn()?;
        unsafe { libc::close(slave) };

        let tmpdir = std::env::temp_dir().join(format!("acl2-session-{}", child.id()));
        std::fs::create_dir_all(&tmpdir)?;

        let mut s = Session {
            child,
            master,
            tmpdir,
            generation: 0,
            pending: String::new(),
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
        let captured = self.read_until(|b| b.contains(&done), deadline)?;
        Ok(self.split_after(captured, &done).is_some())
    }

    /// Keep everything up to and including `marker`; carry the remainder
    /// forward as `pending` so it can never be attributed to the next form.
    fn split_after(&mut self, mut captured: String, marker: &str) -> Option<String> {
        let i = captured.find(marker)?;
        let cut = i + marker.len();
        self.pending = captured[cut..].to_string();
        captured.truncate(cut);
        Some(captured)
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
        self.generation += 1;
        let genid = self.generation;
        let path = self.tmpdir.join(format!("form-{genid}.lisp"));

        // OK is the last thing in the file, so `:ld-error-action :return!`
        // means a failing form never reaches it. DONE is printed by the REPL
        // afterwards and therefore always arrives.
        // `(value :invisible)` keeps ACL2 from printing the marker form's own
        // return value, which would otherwise leave a stray `nil` in the
        // captured output of every single form.
        std::fs::write(
            &path,
            format!(
                "{form}\n(acl2::prog2$ (acl2::cw \"~%<<<ASS-OK {genid}>>>~%\") \
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
            path.display()
        );
        self.write_all(line.as_bytes())?;

        let done = format!("<<<ASS-DONE {genid}>>>");
        let start = Instant::now();
        let raw = self.read_until(|b| b.contains(&done), deadline)?;
        let elapsed = start.elapsed();

        // Anything read past our sentinel belongs to whatever comes next.
        let finished = self.split_after(raw.clone(), &done);
        let captured = finished.unwrap_or(raw);

        let outcome = if captured.contains(&done) {
            if captured.contains(&format!("<<<ASS-OK {genid}>>>")) {
                Outcome::Ok
            } else {
                Outcome::Failed
            }
        } else {
            Outcome::StillRunning
        };
        let (output, data, blocks) = strip_sentinels(&captured, genid);
        Ok(Submission { outcome, output, data, blocks, elapsed })
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
        for _ in 0..INTERRUPT_TRIES {
            unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGINT) };
            // Verify by handshake: a session that answers a round-trip is
            // genuinely back, whereas a prompt in the output proves nothing
            // (ACL2 prints nested prompts while an `ld` is still running).
            if self.handshake(INTERRUPT_STEP)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn write_all(&mut self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let n = unsafe {
                libc::write(self.master, buf.as_ptr() as *const libc::c_void, buf.len())
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            buf = &buf[n as usize..];
        }
        Ok(())
    }

    /// Accumulate output until `done` is satisfied or the deadline expires.
    /// Anything read past the match is retained in `self.pending`, so output
    /// from one form is never attributed to the next.
    fn read_until<F>(&mut self, done: F, deadline: Duration) -> io::Result<String>
    where
        F: Fn(&str) -> bool,
    {
        let mut buf = std::mem::take(&mut self.pending);
        if done(&buf) {
            return Ok(buf);
        }
        let start = Instant::now();
        let mut chunk = [0u8; 65536];
        loop {
            let left = match deadline.checked_sub(start.elapsed()) {
                Some(d) if !d.is_zero() => d,
                _ => break,
            };
            let ms = left.as_millis().min(i32::MAX as u128) as i32;
            if !wait_readable(self.master, ms)? {
                continue;
            }
            let n = unsafe {
                libc::read(self.master, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len())
            };
            if n <= 0 {
                break; // EOF or the child went away
            }
            buf.push_str(&String::from_utf8_lossy(&chunk[..n as usize]));
            if done(&buf) {
                break;
            }
        }
        Ok(buf)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        unsafe { libc::close(self.master) };
        let _ = std::fs::remove_dir_all(&self.tmpdir);
    }
}

/// Remove our own markers, returning what ACL2 printed plus any structured
/// values reported via `<<<ASS-DATA key value>>>`.
///
/// Also trims the prompt fragment that ACL2 emits *before* reading our line and
/// which therefore lands at the head of the capture. This is cosmetic only --
/// the prompt is never used to decide anything (P2); it is just noise at the
/// front of otherwise clean output.
fn strip_sentinels(s: &str, genid: u64) -> (String, Vec<(String, String)>, Vec<(String, String)>) {
    let ok = format!("<<<ASS-OK {genid}>>>");
    let done = format!("<<<ASS-DONE {genid}>>>");
    let mut data = Vec::new();
    let mut blocks: Vec<(String, String)> = Vec::new();
    let mut kept: Vec<&str> = Vec::new();
    let mut open: Option<(String, Vec<&str>)> = None;
    for l in s.lines() {
        if l.contains(&ok) || l.contains(&done) {
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
