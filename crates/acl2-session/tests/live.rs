//! Integration tests that drive a real ACL2.
//!
//! Ignored by default because they need one. Run with:
//!
//! ```text
//! ACL2=/path/to/saved_acl2 cargo test -p acl2-session --test live \
//!     -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `--test-threads=1` is advisable: each test starts its own ACL2, and several
//! resident images at once is a lot of memory for no benefit.
//!
//! These began as demo binaries shipped from the library crate. They assert and
//! fail loudly, so they were always tests; keeping them as binaries meant they
//! would have been installed alongside it, and meant remembering to run four
//! separate commands to check for regressions.

use std::path::{Path, PathBuf};
use std::time::Duration;

use acl2_session::{Book, Health, Resumed, Session, Slice, Status, WorldChange};

/// The ACL2 to drive, or `None` to skip.
fn acl2() -> Option<PathBuf> {
    std::env::var("ACL2").ok().map(PathBuf::from)
}

macro_rules! acl2_or_skip {
    () => {
        match acl2() {
            Some(a) => a,
            None => {
                eprintln!("ACL2 not set; skipping");
                return;
            }
        }
    };
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("acl2-live-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("scratch dir");
    d
}

fn write(path: &Path, forms: &[&str]) {
    std::fs::write(path, forms.join("\n\n") + "\n").expect("write book");
}

const D: Duration = Duration::from_secs(120);
const ALPHA: &str = "(defun alpha (x) (if (consp x) (alpha (cdr x)) nil))";
const FIB: &str = "(defun fib (n) (if (zp n) 0 (if (= n 1) 1 \
                    (+ (fib (- n 1)) (fib (- n 2))))))";

/// The transport itself: spawn ACL2, submit forms, get typed outcomes back.
///
/// Below `Book`, so it is the only test covering `Session` directly.
#[test]
#[ignore = "needs ACL2"]
fn session_submits_and_classifies() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("session");
    let mut s = Session::start(&acl2, &dir).expect("start");

    let ok = s
        .submit("(acl2::cw \"pkg=~x0~%\" (acl2::@ acl2::current-package))", D)
        .expect("submit");
    assert_eq!(ok.outcome, acl2_session::Outcome::Ok);
    assert!(ok.output.contains("pkg="), "the form's own output is captured");

    let good = s.submit(ALPHA, D).expect("submit defun");
    assert_eq!(good.outcome, acl2_session::Outcome::Ok);

    let bad = s
        .submit("(acl2::thm (acl2::equal (alpha x) x))", D)
        .expect("submit thm");
    assert_eq!(bad.outcome, acl2_session::Outcome::Failed, "a false thm fails");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The file-prefix invariant, end to end.
#[test]
#[ignore = "needs ACL2"]
fn sync_reuses_rolls_back_and_stops_at_failure() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("sync");
    let book = dir.join("demo.lisp");
    let beta = "(defun beta (x) (cons x x))";
    let thm = "(defthm alpha-of-nil (equal (alpha nil) nil))";
    write(&book, &["(in-package \"ACL2\")", ALPHA, beta, thm]);

    let mut b = Book::open(&acl2, &book).expect("open");

    let o = b.sync(None, D).expect("first sync");
    assert_eq!(o.admitted.len(), 4, "everything is new");
    assert_eq!(o.reused, 0);

    let o = b.sync(None, D).expect("re-sync");
    assert_eq!((o.reused, o.admitted.len(), o.rolled_back), (4, 0, 0),
               "an untouched file costs nothing");

    // Layout is not a change: normalised hashing must reuse the whole prefix.
    write(&book, &["(in-package \"ACL2\")", ALPHA,
                   "(defun beta (x)\n  ; a passing remark\n  (cons x\n        x))", thm]);
    let o = b.sync(None, D).expect("reformat");
    assert_eq!((o.reused, o.admitted.len()), (4, 0), "reformatting is free");

    // A real edit rolls back from the first difference.
    let beta2 = "(defun beta (x) (cons x (cons x x)))";
    write(&book, &["(in-package \"ACL2\")", ALPHA, beta2, thm]);
    let o = b.sync(None, D).expect("edit");
    assert_eq!((o.reused, o.rolled_back, o.admitted.len()), (2, 2, 2));

    // A failure stops the run and reports structurally.
    write(&book, &["(in-package \"ACL2\")", ALPHA, beta2, thm,
                   "(defthm obviously-false (equal (alpha x) x))",
                   "(defun never-reached (x) x)"]);
    let o = b.sync(None, D).expect("failing sync");
    let f = o.failed.as_ref().expect("the false theorem fails");
    assert_eq!(f.index, 4);
    assert!(o.admitted.is_empty(), "nothing new before the failure");
    assert_eq!(f.status, Status::ProofFailure);
    assert_eq!(f.checkpoints.abort_cause.as_deref(), Some("empty-clause"),
               "a false conjecture is distinguished from an unproven one");
    assert!(!f.checkpoints.sub.is_empty(), "the defect is in the sub stack");
    assert_eq!(f.failed_event().map(|e| e.name.as_str()), Some("obviously-false"));

    // Fixing it needs no rollback: it was never admitted.
    write(&book, &["(in-package \"ACL2\")", ALPHA, beta2, thm,
                   "(defthm now-true (equal (alpha nil) nil))",
                   "(defun never-reached (x) x)"]);
    let o = b.sync(None, D).expect("fixed");
    assert_eq!((o.rolled_back, o.admitted.len()), (0, 2));
    assert!(o.failed.is_none());

    // A target bounds how far forward to admit; it never retracts.
    let o = b.sync(Some(3), D).expect("targeted");
    assert_eq!(o.resolved_through, Some(1));
    assert_eq!(o.reused, 6, "the rest stays admitted");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Rebuild by replay leaves a world the prefix diff accepts.
#[test]
#[ignore = "needs ACL2"]
fn rebuild_restores_the_prefix() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("rebuild");
    let book = dir.join("demo.lisp");
    write(&book, &["(in-package \"ACL2\")", ALPHA,
                   "(defthm a1 (equal (alpha nil) nil))",
                   "(defthm a2 (equal (alpha (cons 1 nil)) nil))"]);
    let mut b = Book::open(&acl2, &book).expect("open");
    assert_eq!(b.sync(None, D).expect("sync").admitted.len(), 4);

    // Its real trigger -- an unverifiable rollback -- cannot be produced on
    // demand, so drive the mechanism and check the world it leaves behind.
    b.rebuild(2, D).expect("rebuild");
    assert_eq!(b.rebuilds(), 1);
    let o = b.sync(None, D).expect("sync after rebuild");
    assert_eq!(o.reused, 2, "the replayed prefix is recognised");
    assert_eq!(o.admitted.len(), 2, "and the rest goes on top");
    assert!(o.failed.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A non-proof failure must not inherit an earlier proof's checkpoints.
#[test]
#[ignore = "needs ACL2"]
fn stale_prover_state_is_not_misattributed() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("stale");
    let book = dir.join("demo.lisp");

    write(&book, &["(in-package \"ACL2\")", ALPHA,
                   "(defthm bait (equal (alpha x) x))"]);
    let mut b = Book::open(&acl2, &book).expect("open");
    let o = b.sync(None, D).expect("prime");
    assert!(!o.failed.as_ref().unwrap().checkpoints.top.is_empty(),
            "the proof failure does report checkpoints");

    write(&book, &["(in-package \"ACL2\")", ALPHA,
                   "(defthm bait (equal (alpha nil) nil))",
                   "(include-book \"no/such/book/anywhere\" :dir :system)"]);
    let o = b.sync(None, D).expect("include-book failure");
    let f = o.failed.as_ref().expect("the include-book fails");
    assert_eq!(f.status, Status::Rejected, "no proof ran, so it is not a proof failure");
    assert!(f.checkpoints.top.is_empty() && f.checkpoints.sub.is_empty(),
            "checkpoints from the earlier failure must not be reported here");
    assert!(f.checkpoints.abort_cause.is_none(), "nor an invented abort cause");
    assert!(f.error_line.is_some(), "but the error line is surfaced");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A deadline bounds the call, not the proof.
#[test]
#[ignore = "needs ACL2"]
fn deadline_hands_back_control_without_disturbing_acl2() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("deadline");
    let book = dir.join("slow.lisp");
    // Naive fib 44 grinds for about a minute.
    write(&book, &["(in-package \"ACL2\")", FIB,
                   "(thm (equal (fib 44) 701408733))"]);
    let mut b = Book::open(&acl2, &book).expect("open");

    b.sync(Some(3), Duration::from_secs(10)).expect("prefix");
    let o = b.sync(None, Duration::from_secs(3)).expect("slow sync");
    assert_eq!(o.failed.as_ref().map(|f| f.status), Some(Status::DeadlineExceeded));
    assert!(b.running_unit().is_some(), "the unit is tracked as running");

    // Starting a second form would interleave two on one pipe.
    assert!(matches!(b.sync(None, Duration::from_secs(2)),
                     Err(acl2_session::SyncError::Busy { .. })),
            "a second submission is rejected, not queued");

    // health must say Busy, not Dead: reporting Dead would make a client throw
    // away a session that is working.
    assert_eq!(b.health(Duration::from_secs(2)), Health::Busy);

    let p = b.progress(3).expect("progress").expect("something is running");
    assert!(p.elapsed >= Duration::from_secs(1));

    match b.resume(Duration::from_secs(2)).expect("resume") {
        Some(Resumed::StillRunning { .. }) => {}
        other => panic!("expected StillRunning, got {:?}", other.is_some()),
    }

    assert!(b.interrupt().expect("interrupt"), "the session recovers");
    assert!(b.running_unit().is_none());
    assert_eq!(b.health(Duration::from_secs(5)), Health::Ready);

    // And it is usable again.
    write(&book, &["(in-package \"ACL2\")", FIB, "(defthm easy (equal (fib 3) 2))"]);
    let o = b.sync(None, D).expect("final sync");
    assert!(o.failed.is_none());
    assert_eq!((o.reused, o.admitted.len()), (2, 1));

    let _ = std::fs::remove_dir_all(&dir);
}

/// eval cannot change the world, but may change state.
#[test]
#[ignore = "needs ACL2"]
fn eval_leaves_the_world_alone_but_settings_stick() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("eval");
    let book = dir.join("b.lisp");
    write(&book, &["(in-package \"ACL2\")", ALPHA]);
    let mut b = Book::open(&acl2, &book).expect("open");
    assert_eq!(b.sync(None, D).expect("sync").admitted.len(), 2);

    let r = b.eval("(acl2::pe 'alpha)", D).expect("query");
    assert_eq!(r.world_change, WorldChange::None, "a query introduces no event");
    let text = b.output(&r.output_ref, Slice::All).unwrap().text;
    assert!(text.contains("alpha"), "and its answer comes back");

    let r = b.eval("(acl2::thm (acl2::equal (alpha acl2::nil) acl2::nil))", D).unwrap();
    assert_eq!(r.world_change, WorldChange::None, "nor does a thm");

    let r = b
        .eval("(acl2::defthm ev-tmp (acl2::equal (alpha acl2::nil) acl2::nil))", D)
        .expect("defthm");
    assert_eq!(r.world_change, WorldChange::Undone, "but a defthm is undone");

    let r = b.eval("(acl2::pe 'ev-tmp)", D).expect("query again");
    let text = b.output(&r.output_ref, Slice::All).unwrap().text.to_lowercase();
    assert!(text.contains("error") || text.contains("not"),
            "so it is invisible to the next eval");

    // State, unlike the world, persists: this is what makes settings usable.
    b.eval("(acl2::set-gag-mode acl2::nil)", D).expect("set");
    let r = b.eval("(acl2::@ acl2::gag-mode)", D).expect("read back");
    let text = b.output(&r.output_ref, Slice::All).unwrap().text.to_lowercase();
    assert!(text.contains("nil"), "the setting stuck");

    let o = b.sync(None, D).expect("resync");
    assert_eq!((o.reused, o.admitted.len()), (2, 0), "the prefix is intact");
    assert_eq!(b.health(D), Health::Ready);

    let _ = std::fs::remove_dir_all(&dir);
}
