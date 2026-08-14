#![forbid(unsafe_code)]

//! Certifying a book, by running `cert.pl`.
//!
//! Certification is deliberately *not* done in the session. `certify-book`
//! would change the session's world, and the invariant everything else rests
//! on is that the world equals the result of admitting file forms `1..N` in a
//! plain interactive session. So this runs a separate process against the file
//! on disk -- which is the source of truth anyway, so "certify this session's
//! book" is well defined without consulting the session at all.
//!
//! `cert.pl` is invoked rather than reimplemented. It chases dependencies,
//! reads `.acl2` and `cert.acl2` files, and drives `make`; a server that
//! guessed at any of that would answer a different question than the one the
//! user's build answers, silently. Two things are worth taking care over:
//!
//! **Which ACL2.** The session already resolved an image under the trust
//! policy, from the book's own checkout. `--acl2` passes that same image, so
//! certification and the interactive session cannot disagree about what they
//! are running -- the "cert-environment mismatch" whose failure mode is that
//! in-session proofs quietly do not certify.
//!
//! **Which cert.pl.** Derived from that image's checkout, never supplied by a
//! caller. Running `cert.pl` executes Perl, `make`, and a fleet of ACL2s; if
//! its path were an argument, it would be a way to execute an arbitrary
//! program that bypassed the image allowlist entirely.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where `cert.pl` sits inside an ACL2 checkout.
pub const CERT_PL: &str = "books/build/cert.pl";

#[derive(Debug)]
pub enum CertError {
    /// The image's checkout has no `cert.pl`.
    NoCertPl(PathBuf),
    Spawn(std::io::Error),
    Io(std::io::Error),
}

impl std::fmt::Display for CertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CertError::NoCertPl(p) => write!(
                f,
                "no {CERT_PL} under {}; certification needs the build scripts \
                 from the same checkout as the ACL2 image",
                p.display()
            ),
            CertError::Spawn(e) => write!(f, "could not start cert.pl: {e}"),
            CertError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CertError {}

/// `cert.pl` belonging to the checkout that provides `image`.
pub fn cert_pl_for(image: &Path) -> Result<PathBuf, CertError> {
    let checkout = crate::image::find_checkout(image)
        .ok_or_else(|| CertError::NoCertPl(image.to_path_buf()))?;
    let p = checkout.join(CERT_PL);
    if p.is_file() {
        Ok(p)
    } else {
        Err(CertError::NoCertPl(checkout))
    }
}

/// `cert.pl` inside a books directory ACL2 named itself.
///
/// Preferred over `cert_pl_for`, which can only find a checkout the image
/// still lives inside. `image` is carried only for the error message.
pub fn cert_pl_in(books_dir: &Path, image: &Path) -> Result<PathBuf, CertError> {
    let p = books_dir.join("build/cert.pl");
    if p.is_file() {
        return Ok(p);
    }
    // The books directory is known but has no build scripts; the checkout walk
    // is a long shot from here, but costs nothing and covers odd layouts.
    cert_pl_for(image)
}

/// How far a certification has got.
pub struct BuildProgress {
    pub book: PathBuf,
    pub elapsed: Duration,
    pub lines_so_far: usize,
    pub tail: String,
}

/// A certification in progress.
pub struct Build {
    child: Child,
    book: PathBuf,
    started: Instant,
    /// Bytes rather than text: a read can land mid-character, and a log is not
    /// worth risking a replacement character over. Decoded once, at the end.
    buf: Arc<Mutex<Vec<u8>>>,
}

fn pump(mut r: impl Read + Send + 'static, buf: Arc<Mutex<Vec<u8>>>) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut b) = buf.lock() {
                        b.extend_from_slice(&chunk[..n]);
                    }
                }
            }
        }
    });
}

impl Build {
    /// Start certifying `book`, running in the book's own directory.
    ///
    /// The child gets its own process group. `cert.pl` runs `make`, which runs
    /// many ACL2s; killing only the child we spawned would leave those behind,
    /// and at `-j 12` that is a dozen orphaned provers.
    pub fn start(
        cert_pl: &Path,
        image: &Path,
        book: &Path,
        jobs: usize,
    ) -> Result<Build, CertError> {
        use std::os::unix::process::CommandExt;
        let dir = match book.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let mut cmd = Command::new("perl");
        cmd.arg(cert_pl)
            .arg("--acl2")
            .arg(image)
            .arg("-j")
            .arg(jobs.to_string())
            .arg(book)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = cmd.spawn().map_err(CertError::Spawn)?;
        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        if let Some(o) = child.stdout.take() {
            pump(o, buf.clone());
        }
        if let Some(e) = child.stderr.take() {
            pump(e, buf.clone());
        }
        Ok(Build {
            child,
            book: book.to_path_buf(),
            started: Instant::now(),
            buf,
        })
    }

    /// Wait up to `deadline`. `None` means it is still going.
    pub fn wait(&mut self, deadline: Duration) -> Result<Option<ExitStatus>, CertError> {
        let start = Instant::now();
        loop {
            match self.child.try_wait().map_err(CertError::Io)? {
                Some(st) => return Ok(Some(st)),
                None if start.elapsed() >= deadline => return Ok(None),
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    /// Everything printed so far.
    pub fn output(&self) -> String {
        self.buf
            .lock()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn book(&self) -> &Path {
        &self.book
    }

    /// Stop the build and everything it started.
    pub fn kill(&mut self) -> Result<(), CertError> {
        // Negating the pid addresses the process group, which is why the child
        // was put in one of its own.
        if let Ok(pgid) = rustix::process::Pid::from_raw(self.child.id() as i32)
            .ok_or(())
            .map(rustix::process::Pid::from)
        {
            let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::TERM);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        Ok(())
    }
}

/// Every book `cert.pl` reported as failing, in the order it said so.
///
/// The blamed book is often *not* the one asked for: certification chases
/// dependencies, so a failure usually belongs to something further down. Saying
/// which is the difference between a useful report and "your book failed".
pub fn blamed_books(log: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for l in log.lines() {
        if let Some(p) = l.trim().strip_prefix("**CERTIFICATION FAILED** for ") {
            let p = PathBuf::from(p.trim());
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// The ACL2 error recorded in a book's `.cert.out`.
///
/// Each book certified gets its own `.cert.out`, so a dependency's failure is
/// readable even though no session was ever open on it.
pub fn cert_out_error(book: &Path) -> Option<String> {
    let text = std::fs::read_to_string(cert_out_path(book)).ok()?;
    crate::sync::error_message(&text)
}

/// `foo.lisp` -> `foo.cert.out`.
pub fn cert_out_path(book: &Path) -> PathBuf {
    book.with_extension("cert.out")
}

/// How a certification ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertStatus {
    Certified,
    Failed,
    /// The deadline expired. `cert.pl` is still building.
    StillRunning,
}

/// What a certification produced.
pub struct CertReport {
    pub status: CertStatus,
    /// The book that was asked for.
    pub book: PathBuf,
    /// Books `cert.pl` blamed, which are often dependencies rather than
    /// `book` itself. Empty on success.
    pub failed_books: Vec<PathBuf>,
    /// The ACL2 error from the first blamed book's `.cert.out`.
    pub error_line: Option<String>,
    /// Handle to everything `cert.pl` printed. Absent while still running,
    /// because a partial log under a handle that later says something else is
    /// worse than no handle.
    pub output_ref: Option<crate::output::OutputRef>,
    pub output_lines: usize,
    /// The last lines printed so far, when still running.
    pub tail: String,
    pub elapsed: Duration,
}

impl CertReport {
    /// Whether the blame falls somewhere other than the book asked for.
    pub fn failed_in_dependency(&self) -> bool {
        self.failed_books
            .first()
            .is_some_and(|b| normalise(b) != normalise(&self.book))
    }
}

fn normalise(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_blamed_book_is_the_one_that_failed_not_the_one_requested() {
        // cert.pl's own output when certifying main.lisp, which depends on dep.
        let log = "Making /tmp/c/main.cert on 14-Aug-2026\n\
                   **CERTIFICATION FAILED** for /tmp/c/dep.lisp\n\
                   make: *** [dep.cert] Error 1\n\
                   **CERTIFICATION FAILED** for /tmp/c/dep.lisp\n";
        assert_eq!(blamed_books(log), vec![PathBuf::from("/tmp/c/dep.lisp")],
                   "the dependency is to blame, and is named once");
    }

    #[test]
    fn a_clean_build_blames_nobody() {
        assert!(blamed_books("Built /tmp/c/good.cert (0.061s)\n").is_empty());
    }

    #[test]
    fn cert_out_sits_beside_the_book() {
        assert_eq!(
            cert_out_path(Path::new("/tmp/c/dep.lisp")),
            PathBuf::from("/tmp/c/dep.cert.out")
        );
    }

    #[test]
    fn cert_pl_must_come_from_a_checkout() {
        // A bare path with no checkout above it cannot yield a cert.pl.
        assert!(matches!(
            cert_pl_for(Path::new("/nonexistent/saved_acl2")),
            Err(CertError::NoCertPl(_))
        ));
    }
}
