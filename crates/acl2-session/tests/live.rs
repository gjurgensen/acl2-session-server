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

use acl2_session::{SyncOptions,
    Book, Budget, Health, NameKind, Resumed, Session, SessionControl, Slice, Status, SyncMode,
    WorldChange,
};

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
    let mut s = Session::start(&acl2, &dir, SessionControl::new()).expect("start");

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

/// A buffer snapshot drives the world exactly as a file would, the disk is
/// never consulted, and the two entries interleave freely on one `Book`.
#[test]
#[ignore = "needs ACL2"]
fn sync_bytes_follows_the_buffer_not_the_disk() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("bytes");
    let book = dir.join("demo.lisp");
    write(&book, &["(in-package \"ACL2\")", ALPHA]);
    let mut b = Book::open(&acl2, &book).expect("open");
    assert_eq!(b.sync(None, D).expect("file sync").admitted.len(), 2);

    // The buffer holds a theorem the disk does not.
    let buffer =
        format!("(in-package \"ACL2\")\n\n{ALPHA}\n\n(defthm b1 (equal (alpha nil) nil))\n");
    let o = b.sync_bytes(buffer.as_bytes(), None, D).expect("buffer sync");
    assert_eq!(o.reused, 2, "the saved prefix is recognised inside the buffer");
    assert_eq!(o.admitted.len(), 1, "only the unsaved theorem is admitted");
    assert!(o.failed.is_none());

    // An identical buffer is a no-op, like any unchanged snapshot.
    let o = b.sync_bytes(buffer.as_bytes(), None, D).expect("same buffer");
    assert_eq!((o.reused, o.admitted.len(), o.rolled_back), (3, 0, 0));

    // Back to the disk file: the unsaved theorem is not in it, so it rolls
    // back -- a save that never happened leaves no residue in the world.
    let o = b.sync(None, D).expect("file sync again");
    assert_eq!(o.reused, 2);
    assert_eq!(o.rolled_back, 1);
    assert!(o.admitted.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// Retraction undoes admitted units on request; under an edited buffer it
/// keeps only what is both requested and still current.
#[test]
#[ignore = "needs ACL2"]
fn retraction_keeps_what_is_asked_and_still_current() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("retract");
    let book = dir.join("demo.lisp");
    write(&book, &["(in-package \"ACL2\")", ALPHA,
                   "(defthm r1 (equal (alpha nil) nil))",
                   "(defthm r2 (equal (alpha (cons 1 nil)) nil))"]);
    let mut b = Book::open(&acl2, &book).expect("open");
    assert_eq!(b.sync(None, D).expect("sync").admitted.len(), 4);
    let bytes = std::fs::read(&book).expect("read book");

    // Asking to keep more than exists is a no-op, spending no submission.
    let o = b.retract_to(&bytes, 9, D).expect("no-op retract");
    assert_eq!((o.reused, o.rolled_back), (4, 0));

    // Undo the two theorems; r1 is really gone from the world.
    let probe = "(acl2::pe 'acl2::r1)";
    assert_eq!(b.eval(probe, D).expect("probe").outcome, acl2_session::Outcome::Ok);
    let o = b.retract_to(&bytes, 2, D).expect("retract");
    assert_eq!((o.reused, o.rolled_back, o.rebuilt), (2, 2, false));
    assert!(o.admitted.is_empty() && o.failed.is_none());
    assert_eq!(b.eval(probe, D).expect("probe").outcome, acl2_session::Outcome::Failed,
               "r1 must not survive the retraction");

    // Retraction alone pauses nothing: an untargeted sync re-admits...
    let o = b.sync(None, D).expect("re-sync");
    assert_eq!((o.reused, o.admitted.len()), (2, 2));

    // ...while the daemon's paired target holds the world where it was put.
    b.retract_to(&bytes, 2, D).expect("retract again");
    let o = b.sync(Some(3), D).expect("sync held by target");
    assert_eq!((o.reused, o.admitted.len(), o.rolled_back), (2, 0, 0));

    // A buffer edited above the request bounds it: unit 2 differs, so only
    // the in-package before it is both requested and still current.
    let edited = format!("(in-package \"ACL2\")\n\n{}\n", ALPHA.replace("alpha", "alfa"));
    let o = b.retract_to(edited.as_bytes(), 2, D).expect("bounded retract");
    assert_eq!((o.reused, o.rolled_back), (1, 1));

    let _ = std::fs::remove_dir_all(&dir);
}

/// ContinuePastFailure admits a red unit under skip-proofs and keeps going;
/// fixing it is an ordinary edit, and its conditional dependents are honest
/// casualties.
#[test]
#[ignore = "needs ACL2"]
fn continue_past_failure_triages_and_recovers() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("cpf");
    let book = dir.join("demo.lisp");
    let below = "(defthm cpf-below (equal (alpha (cons a b)) (cons 1 (cons a b))))";
    write(&book, &["(in-package \"ACL2\")", ALPHA,
                   "(defthm cpf-bad (equal (alpha x) (cons 1 x)))", below]);
    let mut b = Book::open(&acl2, &book).expect("open");

    // Default mode: stop at the failure, exactly as before.
    let o = b.sync(None, D).expect("stop-mode sync");
    assert_eq!(o.admitted.len(), 2);
    assert!(o.failed.is_some() && o.skipped.is_empty() && o.conditional_from.is_none());

    // Triage mode: the red unit is admitted under skip-proofs carrying its
    // original envelope, and the unit below proves -- conditionally, via the
    // unproved claim.
    b.set_sync_mode(SyncMode::ContinuePastFailure);
    let o = b.sync(None, D).expect("cpf sync");
    assert_eq!(o.reused, 2);
    assert_eq!(o.skipped.len(), 1);
    let s = &o.skipped[0];
    assert_eq!((s.index, s.status), (2, Status::ProofFailure));
    assert!(s.event_data.prover_steps.is_some(), "the original envelope rides along");
    assert!(s.retry_output_ref.is_some());
    assert_eq!(o.admitted.len(), 1);
    assert_eq!(o.admitted[0].index, 3);
    assert_eq!(o.conditional_from, Some(2));
    assert!(o.failed.is_none());
    assert_eq!(b.skipped_units(), vec![2]);

    // An unchanged red is prefix, not a re-proof: the resync is a no-op.
    let o = b.sync(None, D).expect("resync");
    assert_eq!((o.reused, o.admitted.len(), o.skipped.len()), (4, 0, 0));
    assert_eq!(o.conditional_from, Some(2));

    // Fixing the red form is an ordinary edit: rollback, a real admission --
    // and the unit below, whose proof leaned on the false claim, becomes the
    // casualty. (The skipped rule was live, as skip-proofs rules are.)
    write(&book, &["(in-package \"ACL2\")", ALPHA,
                   "(defthm cpf-bad (equal (alpha x) nil))", below]);
    let o = b.sync(None, D).expect("fixed sync");
    assert_eq!((o.reused, o.rolled_back), (2, 2));
    assert_eq!(o.admitted.len(), 1, "the fixed unit admits for real");
    assert_eq!(o.skipped.len(), 1, "the conditional unit below is the casualty");
    assert_eq!(o.skipped[0].index, 3);
    assert_eq!(o.conditional_from, Some(3));

    // What skip-proofs cannot rescue stays terminal.
    write(&book, &["(in-package \"ACL2\")", ALPHA,
                   "(defthm cpf-bad (equal (alpha x) nil))",
                   "(in-theory (enable no-such-rune-xyz))"]);
    let o = b.sync(None, D).expect("terminal sync");
    let f = o.failed.expect("a non-proof failure stops even this mode");
    assert_eq!((f.index, f.status), (3, Status::Rejected));
    assert!(f.retry_output_ref.is_some(), "the refused retry is on record");
    assert!(o.skipped.is_empty());
    assert_eq!(o.conditional_from, None, "no skipped unit remains in the world");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Raised inner detail reaches every event of a compound unit, not just the
/// default three slots -- the editor asks for everything.
#[test]
#[ignore = "needs ACL2"]
fn raised_inner_detail_reaches_every_event() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("inner-detail");
    let book = dir.join("demo.lisp");
    write(&book, &["(in-package \"ACL2\")",
                   "(progn\n\
                     (defthm w1 (equal (car (cons x y)) x) :rule-classes nil)\n\
                     (defthm w2 (equal (cdr (cons x y)) y) :rule-classes nil)\n\
                     (defthm w3 (equal (car (cons y x)) y) :rule-classes nil)\n\
                     (defthm w4 (equal (cdr (cons y x)) x) :rule-classes nil)\n\
                     (defthm w5 (equal (car (cons x x)) x) :rule-classes nil))"]);
    let mut b = Book::open(&acl2, &book).expect("open");
    b.set_inner_detail(usize::MAX);
    let o = b.sync(None, D).expect("sync");
    assert!(o.failed.is_none(), "{:?}", o.failed);
    let u = &o.admitted[1];
    assert_eq!(u.inner.len(), 5, "{:?}", u.inner);
    for e in &u.inner {
        assert_eq!(e.kind.as_deref(), Some("defthm"), "every event has its kind: {e:?}");
        assert!(!e.rules.is_empty(), "and its rules: {e:?}");
        assert!(e.time.is_some(), "and its time: {e:?}");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// A rebuild replays a skipped unit under skip-proofs too -- replaying it
/// plainly would re-run the failed proof and read as an unreachable world.
#[test]
#[ignore = "needs ACL2"]
fn rebuild_replays_skipped_units_under_skip_proofs() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("cpf-rebuild");
    let book = dir.join("demo.lisp");
    write(&book, &["(in-package \"ACL2\")", ALPHA,
                   "(defthm cpr-bad (equal (alpha x) (cons 1 x)))"]);
    let mut b = Book::open(&acl2, &book).expect("open");
    b.set_sync_mode(SyncMode::ContinuePastFailure);
    let o = b.sync(None, D).expect("cpf sync");
    assert_eq!((o.skipped.len(), b.skipped_units()), (1, vec![2]));

    let bytes = std::fs::read(&book).expect("read book");
    let r = b.rebuild(&bytes, 3, &Budget::new(D)).expect("rebuild");
    assert!(r.is_none(), "the replay fits the budget");
    assert_eq!(b.skipped_units(), vec![2], "skipped-ness survives the replay");
    let o = b.sync(None, D).expect("sync after rebuild");
    assert_eq!((o.reused, o.admitted.len(), o.skipped.len()), (3, 0, 0));
    assert_eq!(o.conditional_from, Some(2));

    let _ = std::fs::remove_dir_all(&dir);
}

/// A unit that outruns its deadline and then fails at resume gets the same
/// skip-proofs treatment -- the reason the mode is a book property.
#[test]
#[ignore = "needs ACL2"]
fn a_resumed_failure_is_skipped_under_continue_past_failure() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("cpf-resume");
    let book = dir.join("demo.lisp");
    // The red unit must genuinely outrun a ~zero deadline, so it needs real
    // work before it fails: a ground fib computation, not a fast rewrite.
    write(&book, &["(in-package \"ACL2\")", FIB,
                   "(defthm cps-bad (equal (fib 35) 1))"]);
    let mut b = Book::open(&acl2, &book).expect("open");
    b.set_sync_mode(SyncMode::ContinuePastFailure);
    b.sync(Some(3), D).expect("prefix");

    // A ~zero budget puts the red unit in flight instead of retrying it:
    // there is nothing to retry while it runs.
    let o = b.sync(None, Duration::from_millis(1)).expect("tiny sync");
    let f = o.failed.expect("the unit must be in flight");
    assert!(f.still_running && o.skipped.is_empty());

    let mut resumed = None;
    for _ in 0..60 {
        match b.resume(Duration::from_secs(5)).expect("resume") {
            Some(Resumed::StillRunning { .. }) => continue,
            other => {
                resumed = Some(other);
                break;
            }
        }
    }
    match resumed {
        Some(Some(Resumed::Skipped(f))) => {
            assert_eq!((f.index, f.status), (2, Status::ProofFailure));
            assert!(f.retry_output_ref.is_some());
        }
        other => panic!("expected the resumed failure to be skipped, got {other:?}"),
    }
    assert_eq!(b.skipped_units(), vec![2]);
    let o = b.sync(None, D).expect("sync after");
    assert_eq!((o.reused, o.skipped.len()), (3, 0));
    assert_eq!(o.conditional_from, Some(2));

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
    let bytes = std::fs::read(&book).expect("read book");
    let r = b.rebuild(&bytes, 2, &Budget::new(D)).expect("rebuild");
    assert!(r.is_none(), "the replay fits comfortably in the budget");
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

    let ir = b.interrupt().expect("interrupt");
    assert!(ir.ok, "the session recovers");
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

/// `describe` answers the hover question, and answers it about the right file.
///
/// The three shapes a name comes in are all here, because they read different
/// world properties: a function (`formals` + `guard`), a macro (`macro-args`,
/// and no formals at all), and a theorem (`theorem`). So is the case that
/// makes the feature worth having -- a name defined in an *included* book,
/// which must come back attributed to that book's file rather than to this
/// one. Names from this file report no book, which is how the caller tells
/// "defined here" from "defined elsewhere".
///
/// The unknown-token case is the one that runs constantly in practice: every
/// hover over a variable, a keyword, or a word in a comment lands here, and it
/// has to be an ordinary `None` rather than a failed submission.
#[test]
#[ignore = "needs ACL2"]
fn describe_answers_about_names_and_says_where_they_came_from() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("describe");
    let book = dir.join("d.lisp");
    write(&book, &[
        "(in-package \"ACL2\")",
        "(include-book \"std/util/bstar\" :dir :system)",
        ALPHA,
        "(defthm alpha-of-nil (equal (alpha nil) nil))",
    ]);
    let mut b = Book::open(&acl2, &book).expect("open");
    assert!(b.sync(None, D).expect("sync").failed.is_none(), "the book goes in");

    let f = b.describe("alpha", D).expect("query").expect("alpha is known");
    assert_eq!(f.kind, NameKind::Function);
    assert_eq!(f.formals.as_deref(), Some("(x)"), "formals come back as written");
    assert!(f.book.is_none(), "defined in this file, so no book: {:?}", f.book);

    let t = b.describe("alpha-of-nil", D).expect("query").expect("known");
    assert_eq!(t.kind, NameKind::Theorem);
    assert!(t.statement.is_some_and(|s| s.contains("alpha")), "with its statement");

    // From an included book: a macro, whose formals live under `macro-args`,
    // and whose attribution is the whole point -- `pe`'s include-book-path
    // read back as a path we can open.
    let m = b.describe("b*", D).expect("query").expect("b* is known");
    assert_eq!(m.kind, NameKind::Macro);
    assert!(m.formals.is_some_and(|a| a.contains("bindlist")), "macro args");
    let bk = m.book.expect("a name from an included book reports one");
    assert!(bk.contains("bstar.lisp"), "attributed to bstar, got {bk}");
    let p = m.book_path.expect("and it resolves to a file");
    assert!(p.is_file(), "which exists on disk: {}", p.display());

    // Every hover over a variable or a comment word lands here.
    assert!(b.describe("no-such-name-at-all", D).expect("query").is_none());
    assert!(b.describe("(", D).expect("query").is_none(), "and unsafe tokens too");

    // A query must be invisible: it neither disturbs the prefix nor, unlike
    // `eval`, clears the prover state the failure machinery reads.
    let o = b.sync(None, D).expect("resync");
    assert_eq!((o.reused, o.admitted.len()), (4, 0), "the prefix is untouched");
    assert_eq!(b.health(D), Health::Ready);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Relative paths in a submitted form resolve against the book's directory.
///
/// Forms are submitted by `ld`-ing a temp file, and `ld` sets the connected
/// book directory to the directory of the file it loads. Without an explicit
/// `set-cbd` every relative `include-book` resolves against our temp directory
/// instead -- so a real book syncs happily until its first
/// `(include-book "internal/whatever")`, which then reports that a file under
/// /tmp does not exist. Nothing in the suite had a relative path, which is why
/// this survived until a real Kestrel book was pointed at it.
#[test]
#[ignore = "needs ACL2"]
fn relative_paths_resolve_against_the_book_directory() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("cbd");
    let book = dir.join("main.lisp");
    // A sibling the book refers to by a relative path.
    write(&dir.join("helper.lisp"), &["(in-package \"ACL2\")",
                                      "(defconst *helper-marker* 42)"]);
    write(&book, &["(in-package \"ACL2\")",
                   "(cw \"CBD=[~s0]~%\" (cbd))",
                   "(ld \"helper.lisp\")",
                   "(cw \"MARKER=~x0~%\" *helper-marker*)"]);

    let mut b = Book::open(&acl2, &book).expect("open");
    let o = b.sync(None, D).expect("sync");
    assert!(o.failed.is_none(), "a relative ld must resolve: {:?}",
            o.failed.as_ref().map(|f| f.error_line.clone()));
    assert_eq!(o.admitted.len(), 4);

    let cbd = b.output(&o.admitted[1].output_ref, Slice::All).unwrap().text;
    let want = dir.canonicalize().unwrap();
    assert!(cbd.contains(want.to_str().unwrap()),
            "cbd should be the book's directory, got: {cbd}");
    assert!(!cbd.contains("acl2-session-"),
            "cbd must not be the submission temp directory");

    let marker = b.output(&o.admitted[3].output_ref, Slice::All).unwrap().text;
    assert!(marker.contains("42"), "the sibling file was actually loaded");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A form started by `eval` must be resumable.
///
/// `resume` used to consult the pending *unit*, which only `sync` sets, so an
/// eval that outran its deadline could never be waited for: resume reported
/// nothing in flight while ACL2 was still working. The session, not the unit
/// bookkeeping, is the authority on whether something is running.
#[test]
#[ignore = "needs ACL2"]
fn an_eval_that_outruns_its_deadline_can_be_resumed() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("resume-eval");
    let file = dir.join("ev.lisp");
    std::fs::write(&file, "(in-package \"ACL2\")\n").unwrap();
    let mut b = Book::open(&acl2, &file).unwrap();
    b.sync(None, Duration::from_secs(60)).unwrap();

    // Long enough to outrun the deadline below, short enough to finish.
    let slow = "(thm (equal (append (append a b) c) (append a (append b c))) \
                :hints ((\"Goal\" :do-not-induct nil)))";
    let r = b.eval(slow, Duration::from_millis(1)).unwrap();
    assert_eq!(r.outcome, acl2_session::Outcome::StillRunning, "test needs a form that outruns 1ms");

    let mut resumed = None;
    for _ in 0..30 {
        match b.resume(Duration::from_secs(5)).unwrap() {
            Some(Resumed::StillRunning { .. }) => continue,
            other => {
                resumed = Some(other);
                break;
            }
        }
    }
    match resumed {
        Some(Some(Resumed::Evaluated(r))) => {
            assert_eq!(r.outcome, acl2_session::Outcome::Ok);
            assert_eq!(r.world_change, WorldChange::None, "a thm introduces no events");
        }
        other => panic!("expected the eval to finish, got {other:?}"),
    }

    // And the session is usable afterwards.
    let r = b.eval("(+ 1 1)", Duration::from_secs(30)).unwrap();
    assert_eq!(r.outcome, acl2_session::Outcome::Ok);
}

/// Checkpoints must be the real goals, not ACL2's post-revert placeholder.
///
/// When a proof reverts to induction, ACL2 replaces `:top-stack` with a
/// gag-info whose clause is the literal symbol `<goal>` and moves the real
/// pre-revert checkpoints into `abort-info`. Reading `:top-stack` alone
/// reported `<goal>` as the checkpoint. ACL2's own printer prefers the
/// abort-stack, and so must we.
#[test]
#[ignore = "needs ACL2"]
fn a_proof_that_reverts_to_induction_reports_real_checkpoints() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("revert");
    let file = dir.join("rev.lisp");
    std::fs::write(
        &file,
        "(in-package \"ACL2\")\n\
         (defun my-rev (x)\n\
           (if (endp x) nil (append (my-rev (cdr x)) (list (car x)))))\n\
         (defthm car-of-my-rev-bogus\n\
           (equal (car (my-rev x)) (car x)))\n",
    )
    .unwrap();
    let mut b = Book::open(&acl2, &file).unwrap();
    let o = b.sync(None, Duration::from_secs(120)).unwrap();
    let f = o.failed.expect("the theorem is false and must fail");
    assert_eq!(f.status, Status::ProofFailure);

    let c = &f.checkpoints;
    assert!(c.pre_revert, "this proof reverts to induction");
    let top = c.top.first().expect("a top-level checkpoint");
    assert!(!top.term.contains("<goal>"), "placeholder leaked through: {top:?}");
    assert!(
        top.term.contains("my-rev") && top.term.contains("equal"),
        "expected the real goal term, got {top:?}"
    );
    assert!(
        top.goal.as_deref().is_some_and(|g| g.starts_with("Goal") || g.starts_with("Subgoal")),
        "the checkpoint must carry its goal name for mode:goal, got {:?}",
        top.goal
    );

    // And the error message is not cut mid-sentence.
    let e = f.error_line.as_deref().expect("an error line");
    assert!(e.ends_with("failure."), "message truncated: {e:?}");
}

/// A failure that does *not* revert still reads the ordinary top-stack.
///
/// Induction has to be disabled to get one: ACL2 reverts to induction on
/// almost any goal it cannot simplify away, and installs the `<goal>`
/// placeholder when it does. With `:do-not-induct t` the abort-stack is empty
/// and `:top-stack` holds the real term, so this covers the fallback arm of
/// `(or abort-stack top-stack)`.
#[test]
#[ignore = "needs ACL2"]
fn an_ordinary_failure_still_reports_top_stack_checkpoints() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("norevert");
    let file = dir.join("plain.lisp");
    std::fs::write(
        &file,
        "(in-package \"ACL2\")\n\
         (defthm plainly-false\n\
           (equal (car x) 17)\n\
           :hints ((\"Goal\" :do-not-induct t)))\n",
    )
    .unwrap();
    let mut b = Book::open(&acl2, &file).unwrap();
    let o = b.sync(None, Duration::from_secs(120)).unwrap();
    let f = o.failed.expect("must fail");
    let c = &f.checkpoints;
    assert!(!c.pre_revert, "induction was disabled, so nothing reverted");
    assert_eq!(c.abort_cause.as_deref(), Some("do-not-induct"));
    assert!(
        c.top.first().is_some_and(|g| g.term.contains("17")),
        "expected the goal, got {:?}",
        c.top
    );
}

/// Rules come back as a list of runes, not as a picture of one.
#[test]
#[ignore = "needs ACL2"]
fn rules_are_reported_as_separate_runes() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("runes");
    let file = dir.join("r.lisp");
    std::fs::write(
        &file,
        "(in-package \"ACL2\")\n\
         (defun my-app (x y)\n\
           (if (endp x) y (cons (car x) (my-app (cdr x) y))))\n\
         (defthm true-listp-of-my-app\n\
           (implies (true-listp y) (true-listp (my-app x y))))\n",
    )
    .unwrap();
    let mut b = Book::open(&acl2, &file).unwrap();
    let o = b.sync(None, Duration::from_secs(120)).unwrap();
    assert!(o.failed.is_none(), "this book should go in");

    let thm = o.admitted.last().expect("the defthm");
    let rules = &thm.event_data.rules;
    assert!(rules.len() > 1, "expected several runes, got {rules:?}");
    assert!(
        rules.iter().all(|r| !r.contains('\n')),
        "a rune must not carry ACL2's line breaks: {rules:?}"
    );
    assert!(
        rules.iter().all(|r| r.starts_with('(') && r.ends_with(')')),
        "each item must be one whole rune: {rules:?}"
    );
    assert!(
        rules.iter().any(|r| r.contains(":induction my-app")),
        "expected the induction rune among {rules:?}"
    );
}

/// Certification blames the book that actually failed, not the one asked for.
///
/// This is the whole reason for calling cert.pl rather than certifying one
/// book: it chases dependencies, so the failure is usually somewhere else, and
/// reporting "your book failed" when a dependency broke would send the user to
/// the wrong file.
#[test]
#[ignore = "needs ACL2"]
fn certification_names_the_failing_dependency() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("certify");
    std::fs::write(
        dir.join("dep.lisp"),
        "(in-package \"ACL2\")\n\
         (defun h (x) x)\n\
         (defthm h-bogus (equal (h x) 42))\n",
    )
    .unwrap();
    let main = dir.join("main.lisp");
    std::fs::write(
        &main,
        "(in-package \"ACL2\")\n\
         (include-book \"dep\")\n\
         (defthm trivial (equal x x) :rule-classes nil)\n",
    )
    .unwrap();

    let mut b = Book::open(&acl2, &main).unwrap();
    let r = b.certify(Duration::from_secs(300), 2).unwrap();
    assert_eq!(r.status, acl2_session::cert::CertStatus::Failed);
    assert!(r.failed_in_dependency(), "dep.lisp is to blame, not main.lisp");
    assert_eq!(
        r.failed_books.first().and_then(|p| p.file_name()),
        Some(std::ffi::OsStr::new("dep.lisp"))
    );
    let e = r.error_line.as_deref().expect("the dependency's own .cert.out");
    assert!(e.contains("H-BOGUS"), "expected the failing event named: {e:?}");
    assert!(r.output_ref.is_some(), "a finished build hands back its log");

    // The session is untouched by certification and still works.
    let ev = b.eval("(+ 1 1)", Duration::from_secs(60)).unwrap();
    assert_eq!(ev.outcome, acl2_session::Outcome::Ok);
}

/// A book with no bad dependency certifies, and blames nobody.
#[test]
#[ignore = "needs ACL2"]
fn a_good_book_certifies() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("certify-ok");
    let book = dir.join("good.lisp");
    std::fs::write(
        &book,
        "(in-package \"ACL2\")\n\
         (defun f (x) x)\n\
         (defthm f-id (equal (f x) x))\n",
    )
    .unwrap();
    let mut b = Book::open(&acl2, &book).unwrap();
    let r = b.certify(Duration::from_secs(300), 2).unwrap();
    assert_eq!(r.status, acl2_session::cert::CertStatus::Certified,
               "expected success; log: {:?}", r.error_line);
    assert!(r.failed_books.is_empty());
    assert!(!r.failed_in_dependency());
    assert!(book.with_extension("cert").is_file(), "a .cert must exist now");
}

/// Certification works for an ACL2 that does not live inside a checkout.
///
/// cert.pl used to be found by walking up from the image looking for
/// `acl2.lisp`. That finds nothing for an installed or copied ACL2 -- there is
/// no checkout above it -- even though the image knows perfectly well where its
/// books are. The books directory is now asked for rather than inferred, so
/// this case works; before, it failed outright.
#[test]
#[ignore = "needs ACL2"]
fn an_image_outside_a_checkout_can_still_certify() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("no-checkout");
    let elsewhere = dir.join("bin");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let copied = elsewhere.join("acl2");
    std::fs::copy(&acl2, &copied).unwrap();
    let mut perms = std::fs::metadata(&copied).unwrap().permissions();
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
    }
    std::fs::set_permissions(&copied, perms).unwrap();

    // Nothing resembling a checkout sits above the copy.
    assert!(
        acl2_session::find_checkout(&copied).is_none(),
        "this test is pointless if the copy is inside a checkout"
    );

    let book = dir.join("solo.lisp");
    std::fs::write(
        &book,
        "(in-package \"ACL2\")\n(defun k (x) x)\n(defthm k-id (equal (k x) x))\n",
    )
    .unwrap();
    let mut b = Book::open(&copied, &book).unwrap();
    let r = b.certify(Duration::from_secs(300), 2).unwrap();
    assert_eq!(
        r.status,
        acl2_session::cert::CertStatus::Certified,
        "cert.pl must be found via the books directory ACL2 reports"
    );
}

/// A certification that outruns its deadline is continued by asking again.
///
/// The deadline bounds the call, not the build: cert.pl keeps going, and a
/// second `certify` waits on the same build rather than starting a competing
/// one. A large book takes minutes, so this is the normal path, not an edge
/// case.
#[test]
#[ignore = "needs ACL2"]
fn a_certification_that_outruns_its_deadline_is_resumed_by_calling_again() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("cert-resume");
    let book = dir.join("r.lisp");
    std::fs::write(
        &book,
        "(in-package \"ACL2\")\n(defun m (x) x)\n(defthm m-id (equal (m x) x))\n",
    )
    .unwrap();
    let mut b = Book::open(&acl2, &book).unwrap();

    // Nothing finishes in a millisecond; perl's own startup outlasts it.
    let first = b.certify(Duration::from_millis(1), 2).unwrap();
    assert_eq!(first.status, acl2_session::cert::CertStatus::StillRunning);
    assert!(first.output_ref.is_none(), "a partial log gets no handle");

    // The build is visible while it runs, without waiting on it.
    let seen = b.build_progress(5);
    assert!(seen.is_some(), "a running build must show up in status");
    assert_eq!(seen.unwrap().book, book);

    // Asking again continues the same build through to its result.
    let mut done = None;
    for _ in 0..60 {
        let r = b.certify(Duration::from_secs(10), 2).unwrap();
        if r.status != acl2_session::cert::CertStatus::StillRunning {
            done = Some(r);
            break;
        }
    }
    let r = done.expect("the build must finish");
    assert_eq!(r.status, acl2_session::cert::CertStatus::Certified);
    assert!(r.output_ref.is_some(), "the finished build hands back its log");
    assert!(book.with_extension("cert").is_file());

    // And it is over: nothing is left in flight to report.
    assert!(b.build_progress(5).is_none(), "a finished build is not progress");
}

/// The handle a timed-out form hands out is the one its output lands in.
///
/// It used to be allocated, left empty, and abandoned: the output went to a
/// fresh handle when the form finished, so a caller holding the first ref read
/// an empty file and concluded the form printed nothing. A successful read
/// giving a wrong answer is the same failure class as the handle collision.
#[test]
#[ignore = "needs ACL2"]
fn a_timed_out_forms_handle_is_filled_when_it_finishes() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("handle-fill");
    let file = dir.join("h.lisp");
    std::fs::write(&file, "(in-package \"ACL2\")\n").unwrap();
    let mut b = Book::open(&acl2, &file).unwrap();
    b.sync(None, Duration::from_secs(60)).unwrap();

    // Prints for a while, so it certainly outruns the deadline.
    let noisy = "(ld (make-list 60 :initial-element '(cw \"~x0~%\" (make-list 200))))";
    let r = b.eval(noisy, Duration::from_millis(1)).unwrap();
    assert_eq!(r.outcome, acl2_session::Outcome::StillRunning);
    let handed_out = r.output_ref.clone();

    let mut done = None;
    for _ in 0..60 {
        match b.resume(Duration::from_secs(5)).unwrap() {
            Some(Resumed::StillRunning { .. }) => continue,
            other => { done = other; break; }
        }
    }
    let Some(Resumed::Evaluated(fin)) = done else { panic!("the eval must finish") };

    // The same handle, and it has the output in it.
    assert_eq!(fin.output_ref, handed_out,
               "the finished form must land under the handle already handed out");
    let got = b.output(&handed_out, Slice::All).expect("readable");
    assert!(got.total_lines > 10, "the handle must hold the output, not nothing");
    assert!(got.text.contains("("), "expected the form's printing: {:?}", &got.text[..got.text.len().min(80)]);
}

/// An interrupted form's output stays readable.
///
/// The capture was only written on completion, so a form that was interrupted
/// lost everything it had printed -- exactly the output you want, since you
/// interrupted because it was going badly.
#[test]
#[ignore = "needs ACL2"]
fn an_interrupted_forms_output_survives() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("interrupt-capture");
    let file = dir.join("i.lisp");
    std::fs::write(&file, "(in-package \"ACL2\")\n").unwrap();
    let mut b = Book::open(&acl2, &file).unwrap();
    b.sync(None, Duration::from_secs(60)).unwrap();

    // Large enough that the printing certainly outlives the deadline: the
    // capture path got fast enough that the old 4000-element version could
    // finish inside it.
    let noisy = "(ld (make-list 60000 :initial-element '(cw \"~x0~%\" (make-list 200))))";
    let r = b.eval(noisy, Duration::from_millis(200)).unwrap();
    assert_eq!(r.outcome, acl2_session::Outcome::StillRunning);
    let handed_out = r.output_ref.clone();

    assert!(b.interrupt().expect("interrupt").ok, "the form must be interruptible");

    let got = b.output(&handed_out, Slice::All).expect("still readable after interrupt");
    assert!(got.total_lines > 0,
            "what the form printed before the interrupt must survive");
    assert!(got.superseded,
            "it describes an abandoned form, not the current world");

    // And the session is usable.
    let ev = b.eval("(+ 1 1)", Duration::from_secs(30)).unwrap();
    assert_eq!(ev.outcome, acl2_session::Outcome::Ok);
}

/// Inner names are complete; detail is bounded, and the failing event is in it.
#[test]
#[ignore = "needs ACL2"]
fn inner_names_are_complete_and_the_failing_event_carries_detail() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("inner-detail");
    let file = dir.join("p.lisp");
    std::fs::write(
        &file,
        "(in-package \"ACL2\")\n\
         (progn\n\
           (defthm q1 (equal (car (cons a b)) a) :rule-classes nil)\n\
           (defthm q2 (equal (cdr (cons a b)) b) :rule-classes nil)\n\
           (defthm q3 (equal (append nil x) (append nil x)) :rule-classes nil)\n\
           (defthm q4-bogus (equal (car x) 99) :rule-classes nil))\n",
    )
    .unwrap();
    let mut b = Book::open(&acl2, &file).unwrap();
    let o = b.sync(None, Duration::from_secs(120)).unwrap();
    let f = o.failed.expect("q4-bogus is false");

    let names: Vec<&str> = f.inner.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["q4-bogus", "q3", "q2", "q1"],
               "every inner event is named, newest first");

    // The event that failed is at index 0 -- failing is what puts it there --
    // so it is always inside the bounded detail window.
    assert!(f.inner[0].abort_causes.is_some(), "the failing event carries its cause");
    assert!(!f.inner[0].rules.is_empty(), "and its rules");
    assert_eq!(f.failed_event().map(|e| e.name.as_str()), Some("q4-bogus"));

    // Past the window an entry is a name and nothing more. A consumer is told
    // where that starts, so an empty `rules` is never mistaken for "used none".
    for e in f.inner.iter().skip(acl2_session::sync::INNER_DETAIL_SLOTS) {
        assert!(e.rules.is_empty() && e.abort_causes.is_none(),
                "no detail is read past the window: {}", e.name);
    }
}

/// A failed eval carries the same envelope a sync failure does.
///
/// The experimental `thm` is what eval exists for, and a failed experiment
/// used to report nothing but "failed" plus a log handle. Also pins two
/// rules: a failure that ran no prover must not inherit an earlier proof's
/// checkpoints (eval bodies clear the failure globals exactly as sync units
/// do), and events introduced before a later form in the same eval fails are
/// found and undone even though the failure aborted the trailing probe.
#[test]
#[ignore = "needs ACL2"]
fn a_failed_eval_carries_the_failure_envelope() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("eval-env");
    let file = dir.join("e.lisp");
    write(&file, &["(in-package \"ACL2\")", ALPHA]);
    let mut b = Book::open(&acl2, &file).unwrap();
    b.sync(None, D).unwrap();

    // A false thm: a proof runs, fails, and the envelope says why.
    let r = b.eval("(acl2::thm (acl2::equal (alpha x) x))", D).unwrap();
    assert_eq!(r.outcome, acl2_session::Outcome::Failed);
    let f = r.failed.as_ref().expect("a failed eval must carry the envelope");
    assert_eq!(f.status, Status::ProofFailure);
    assert_eq!(f.checkpoints.abort_cause.as_deref(), Some("empty-clause"),
               "a false conjecture is distinguished from an unproven one");
    assert!(!f.checkpoints.top.is_empty() || !f.checkpoints.sub.is_empty(),
            "the checkpoints are where the debugging starts");

    // A translate-level failure right after: no proof ran, and the thm's
    // checkpoints above must not be inherited.
    let r = b.eval("(acl2::no-such-function-anywhere x)", D).unwrap();
    assert_eq!(r.outcome, acl2_session::Outcome::Failed);
    let f = r.failed.as_ref().expect("still an envelope");
    assert_eq!(f.status, Status::Rejected, "no proof ran");
    assert!(f.checkpoints.top.is_empty() && f.checkpoints.sub.is_empty(),
            "the earlier thm's checkpoints must not leak in");
    assert!(f.error_line.is_some(), "the error line is surfaced");

    // A batch eval that introduces an event before a later form fails: the
    // event must be undone although the failure aborted the trailing probe.
    let r = b
        .eval("(acl2::defun ev-stray (acl2::x) acl2::x) (acl2::no-such-fn)", D)
        .unwrap();
    assert_eq!(r.outcome, acl2_session::Outcome::Failed);
    assert_eq!(r.world_change, WorldChange::Undone,
               "events before the failing form are found and undone");
    let q = b.eval("(acl2::pe 'ev-stray)", D).unwrap();
    let text = b.output(&q.output_ref, Slice::All).unwrap().text.to_lowercase();
    assert!(text.contains("error") || text.contains("not"),
            "the stray defun must be gone: {text}");

    // Success carries nothing.
    let r = b.eval("(acl2::+ 1 1)", D).unwrap();
    assert_eq!(r.outcome, acl2_session::Outcome::Ok);
    assert!(r.failed.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A broken image must report its own complaint, at open, as the diagnosis.
///
/// An "ACL2 image" is often a wrapper script, and one that dies -- an unset
/// environment variable, a missing core -- used to yield an apparently open
/// session whose first submission then failed "mid-form" with advice to
/// close and reopen: wrong diagnosis, wrong remedy, and the script's own
/// words nowhere. Needs no ACL2, so it is not gated and runs in CI.
#[test]
fn a_failing_image_reports_its_own_error() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("badimage");
    let image = dir.join("saved_acl2");
    std::fs::write(
        &image,
        "#!/bin/sh\n\
         echo \"FATAL: MY_ACL2_HOME is not set; cannot locate the ACL2 core\" >&2\n\
         exit 1\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&image).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&image, perms).unwrap();
    let book = dir.join("b.lisp");
    std::fs::write(&book, "(in-package \"ACL2\")\n").unwrap();

    let err = Book::open(&image, &book).err().expect("a dead image cannot open");
    let msg = err.to_string();
    assert!(msg.contains("MY_ACL2_HOME"),
            "the script's own words are the diagnosis: {msg}");
    assert!(msg.contains("exited during startup"), "{msg}");
    assert!(!msg.contains("mid-form"),
            "no misleading mid-form report at startup: {msg}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A stopped session reads as unresponsive -- the one health verdict that
/// had never been observed -- and interrupt reports honestly that it could
/// not help.
///
/// SIGSTOP is the deterministic wedge: the process is alive and its
/// descriptors work, but nothing answers, which is the verdict's definition.
/// (An external SIGINT is *not* a wedge, also measured here: an idle ACL2
/// given a moment shrugs one off -- see the interrupt escalation's notes on
/// why signalling idle sessions is still forbidden.)
#[test]
#[ignore = "needs ACL2"]
fn a_stopped_session_reads_as_unresponsive() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("wedged");
    let file = dir.join("w.lisp");
    std::fs::write(&file, "(in-package \"ACL2\")\n").unwrap();
    let mut b = Book::open(&acl2, &file).unwrap();
    b.sync(None, D).unwrap();

    // First, the non-wedge: a stray SIGINT to an idle session is shrugged
    // off. This pins the measurement behind interrupt's ping-first design.
    let pid = b.session().pid();
    let signal = |sig: &str| {
        let ok = std::process::Command::new("kill")
            .args([sig, &pid.to_string()])
            .status()
            .expect("run kill")
            .success();
        assert!(ok, "the signal must reach the child");
    };
    signal("-INT");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(b.health(Duration::from_secs(5)), Health::Ready,
               "an idle ACL2 shrugs off a lone external SIGINT");

    // Now the wedge. Alive, idle by the books, answering nothing:
    // unresponsive, distinct from busy (silence we caused) and from dead
    // (no process).
    signal("-STOP");
    assert_eq!(b.health(Duration::from_secs(2)), Health::Unresponsive);

    // Interrupt cannot lift this -- signals queue against a stopped process
    // -- and must say so rather than claim recovery.
    let r = b.interrupt().expect("the interrupt call itself must not error");
    assert!(!r.ok, "no recovery may be claimed for a stopped process");

    // The wedge lifted from outside, the session is usable again: the
    // verdict was about reachability, not damage.
    signal("-CONT");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(b.health(Duration::from_secs(5)), Health::Ready);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A renamed anchor orphans the session, and says so.
///
/// A session is anchored to one path for life. When the book moves out from
/// under it the read fails, and the bare `NotFound` this used to surface
/// named no file and prescribed nothing -- so a rename looked like a
/// transient I/O hiccup rather than "this session is finished, open another".
#[test]
#[ignore = "needs ACL2"]
fn a_renamed_book_orphans_the_session_loudly() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("renamed");
    let before = dir.join("normalize.lisp");
    write(&before, &["(in-package \"ACL2\")", "(defun rn-f (x) x)"]);
    let mut b = Book::open(&acl2, &before).unwrap();
    assert_eq!(b.sync(None, D).expect("first sync").admitted.len(), 2);

    std::fs::rename(&before, dir.join("rebalance.lisp")).expect("rename");

    let err = b.sync(None, D).expect_err("an orphaned session cannot sync");
    assert!(matches!(err, acl2_session::SyncError::BookMissing { .. }),
            "a missing anchor is its own error, not a generic I/O one: {err}");
    let msg = err.to_string();
    assert!(msg.contains("normalize.lisp"), "it names the path: {msg}");
    assert!(msg.contains("Open a new session"), "and the remedy: {msg}");

    // Certification runs against the same vanished path, so it says the same.
    match b.certify(Duration::from_secs(30), 2) {
        Err(e) => assert!(matches!(e, acl2_session::SyncError::BookMissing { .. }), "{e}"),
        Ok(_) => panic!("certify must refuse a book that is not there"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// `retract` parks the frontier where asked, and only when asked.
#[test]
#[ignore = "needs ACL2"]
fn retract_parks_the_frontier_on_request() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("retract");
    let file = dir.join("r.lisp");
    // Forms land on lines 1, 3, 5, 7.
    write(&file, &["(in-package \"ACL2\")",
                   "(defun rt-f (x) x)",
                   "(defthm rt-1 (equal (rt-f x) x))",
                   "(defthm rt-2 (equal (rt-f (rt-f x)) x))"]);
    let mut b = Book::open(&acl2, &file).unwrap();
    assert_eq!(b.sync(None, D).expect("sync").admitted.len(), 4);

    // Without the flag a smaller target is still purely a forward bound.
    let o = b.sync(Some(3), D).expect("targeted sync");
    assert_eq!((o.reused, o.rolled_back), (4, 0), "a target alone never retracts");

    // With it, the frontier moves back to the form that line selects.
    let o = b.sync_with(&SyncOptions { through_line: Some(3), retract: true, ..Default::default() }, D).expect("retracting sync");
    assert_eq!((o.reused, o.rolled_back), (2, 2), "{o:?}");
    assert_eq!(b.admitted_lines().len(), 2, "two units remain admitted");
    assert!(!o.rebuilt, "retraction is an undo, not a replay");

    // The retracted forms are gone from the world, not merely unreported.
    let r = b.eval("(acl2::pe 'rt-1)", D).expect("query");
    let text = b.output(&r.output_ref, Slice::All).unwrap().text.to_lowercase();
    assert!(text.contains("error") || text.contains("not"),
            "the retracted theorem must be absent: {text}");

    // A plain sync puts them back, and nothing before the mark re-proves.
    let o = b.sync(None, D).expect("resync");
    assert_eq!((o.reused, o.admitted.len(), o.rolled_back), (2, 2, 0));

    // Retracting to a line past the frontier is an ordinary sync.
    let o = b.sync_with(&SyncOptions { through_line: Some(999), retract: true, ..Default::default() }, D).expect("forward retract");
    assert_eq!((o.reused, o.rolled_back), (4, 0));

    let _ = std::fs::remove_dir_all(&dir);
}

/// A dependency edited and recertified under a live session invalidates the
/// form that included it -- and a merely touched one costs nothing.
///
/// The field case, reproduced: a rule is added to a dependency, the
/// dependency is recertified, the dependent is re-synced -- and the rule was
/// still absent while the reply said everything was reused and nothing was
/// amiss. The session was never wrong about its own file; it was out of step
/// with what that file includes, which the prefix invariant never covered.
#[test]
#[ignore = "needs ACL2"]
fn a_recertified_dependency_invalidates_the_form_that_included_it() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("deps");
    let dep = dir.join("dep.lisp");
    let main = dir.join("main.lisp");
    let certify = |path: &Path| {
        let mut b = Book::open(&acl2, path).unwrap();
        assert_eq!(b.certify(Duration::from_secs(300), 2).unwrap().status,
                   acl2_session::cert::CertStatus::Certified);
    };
    write(&dep, &["(in-package \"ACL2\")", "(defun dep-id (x) x)"]);
    // Forms on lines 1, 3, 5.
    write(&main, &["(in-package \"ACL2\")",
                   "(include-book \"dep\")",
                   "(defthm main-thm (equal (dep-id (dep-id x)) x))"]);
    certify(&dep);

    let mut b = Book::open(&acl2, &main).unwrap();
    let o = b.sync(None, D).expect("sync");
    assert!(o.failed.is_none(), "{:?}", o.failed.as_ref().map(|f| &f.error_line));
    assert!(o.stale_includes.is_empty(), "nothing is stale yet: {:?}", o.stale_includes);

    // Touching without changing must cost nothing: the write date moves, the
    // content does not, and re-proving here would be pure waste.
    let same = std::fs::read(&dep).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(&dep, &same).unwrap();
    let o = b.sync(None, D).expect("sync after touch");
    assert!(o.stale_includes.is_empty(), "a touch is not a change: {:?}", o.stale_includes);
    assert_eq!((o.reused, o.rolled_back), (3, 0), "and nothing moves");

    // Now the real thing: add a rule to the dependency and recertify it.
    write(&dep, &["(in-package \"ACL2\")", "(defun dep-id (x) x)",
                  "(defthm dep-id-is-id (equal (dep-id x) x))"]);
    certify(&dep);

    // Asked to ignore it, the session reports and does nothing -- for
    // finishing what you are doing before picking up a neighbour's edit.
    let o = b
        .sync_with(&SyncOptions { ignore_stale_includes: true, ..Default::default() }, D)
        .expect("sync ignoring staleness");
    assert_eq!(o.stale_includes.len(), 1, "{:?}", o.stale_includes);
    let st = &o.stale_includes[0];
    assert!(st.path.ends_with("dep.lisp"), "{st:?}");
    assert!(!st.missing);
    assert_eq!(st.unit, Some(1), "attributed to the include-book form: {st:?}");
    assert_eq!((o.reused, o.rolled_back), (3, 0), "and the world is untouched");

    // By default it folds into the diff: the including form is undone and
    // re-admitted, exactly as editing that form would have done.
    let o = b.sync(None, D).expect("sync folding staleness");
    assert_eq!(o.stale_includes.len(), 1, "still says why it moved: {:?}", o.stale_includes);
    assert_eq!(o.rolled_back, 2, "the include and everything after it: {o:?}");
    assert_eq!(o.admitted.len(), 2, "and both go back in");
    assert!(o.failed.is_none());

    // The new rule really is in the world, and the session settles clean.
    let r = b.eval("(acl2::pe 'dep-id-is-id)", D).expect("query");
    let text = b.output(&r.output_ref, Slice::All).unwrap().text;
    assert!(text.to_lowercase().contains("dep-id-is-id"), "{text}");
    let o = b.sync(None, D).expect("settled");
    assert!(o.stale_includes.is_empty(), "re-including re-baselines: {:?}", o.stale_includes);
    assert_eq!((o.reused, o.rolled_back), (3, 0));

    // A dependency that vanishes is reported missing rather than changed.
    std::fs::remove_file(&dep).unwrap();
    let o = b
        .sync_with(&SyncOptions { ignore_stale_includes: true, ..Default::default() }, D)
        .expect("sync after removal");
    assert!(o.stale_includes.iter().any(|s| s.missing), "{:?}", o.stale_includes);

    let _ = std::fs::remove_dir_all(&dir);
}

/// `skip_proofs` admits without proving, and says the world is conditional.
#[test]
#[ignore = "needs ACL2"]
fn skip_proofs_admits_without_proving_and_says_so() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("skipmode");
    let file = dir.join("s.lisp");
    // The theorem is false, so it can only go in with proofs skipped.
    write(&file, &["(in-package \"ACL2\")",
                   "(defun sk-f (x) x)",
                   "(defthm sk-false (equal (sk-f x) (cons x x)))"]);
    let mut b = Book::open(&acl2, &file).unwrap();

    let o = b.sync(None, D).expect("honest sync");
    assert!(o.failed.is_some(), "a false theorem fails when proofs run");
    assert!(o.conditional_from.is_none(), "and nothing is conditional");

    let o = b
        .sync_with(&SyncOptions { skip_proofs: true, ..Default::default() }, D)
        .expect("skipping sync");
    assert!(o.failed.is_none(), "with proofs skipped it goes in: {o:?}");
    // Only the form that needed skipping is conditional: the two before it
    // were admitted honestly by the failing sync and were reused untouched,
    // so the claim is as narrow as the truth allows.
    assert_eq!(o.conditional_from, Some(2),
               "conditional from the skipped form on: {o:?}");
    assert_eq!(o.reused, 2, "the honest prefix is not re-admitted: {o:?}");

    // The claim persists until those forms are admitted honestly again, which
    // is what retracting and re-syncing does.
    let o = b.sync(None, D).expect("resync");
    assert_eq!(o.conditional_from, Some(2), "reused forms stay conditional: {o:?}");
    let o = b
        .sync_with(&SyncOptions { through_line: Some(1), retract: true, ..Default::default() }, D)
        .expect("retract");
    assert!(o.conditional_from.is_none(), "retracting drops the claim: {o:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A crashed ACL2 must read as dead, not busy forever.
///
/// `is_running` is bookkeeping; a child that dies mid-form leaves it set. The
/// EOF reader then returns instantly, resume says still_running on every
/// call, health says busy, and nothing ever tells the client the truth. The
/// exit is close-and-reopen, and the session must say so.
#[test]
#[ignore = "needs ACL2"]
fn a_killed_acl2_reads_as_dead_not_busy() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("killed");
    let file = dir.join("k.lisp");
    std::fs::write(&file, "(in-package \"ACL2\")\n").unwrap();
    let mut b = Book::open(&acl2, &file).unwrap();
    b.sync(None, Duration::from_secs(60)).unwrap();

    // Put a form in flight, then kill the child the way an oom-killer would.
    let noisy = "(ld (make-list 4000 :initial-element '(cw \"~x0~%\" (make-list 200))))";
    let r = b.eval(noisy, Duration::from_millis(1)).unwrap();
    assert_eq!(r.outcome, acl2_session::Outcome::StillRunning);
    let pid = b.session().pid();
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("run kill");
    assert!(killed.success(), "kill -9 must reach the child");

    // Resume must not report still_running about a corpse.
    let err = loop {
        match b.resume(Duration::from_secs(2)) {
            Err(e) => break e,
            Ok(Some(Resumed::StillRunning { .. })) => continue,
            Ok(other) => panic!("a dead child cannot finish a form: {:?}", other.is_some()),
        }
    };
    let msg = err.to_string();
    assert!(msg.contains("exited"), "the error must say what happened: {msg}");
    assert!(msg.contains("close"), "and what to do about it: {msg}");

    assert_eq!(b.health(Duration::from_secs(2)), Health::Dead);
}

/// A value ACL2's fmt wraps at the margin survives the block protocol.
///
/// The environment report used to travel in one-line `ASS-DATA` markers, and
/// ACL2 wraps `~x` at the right margin, so a books path longer than the
/// margin lost its closing `>>>` -- the datum silently vanished and the
/// marker fragments leaked into the capture. The report now travels in a
/// BEGIN/END block. This drives a value long past the margin through the real
/// fmt and asserts nothing is lost and nothing leaks; the width cannot be
/// exercised via `system-books-dir` itself, since that path is baked into the
/// image, not something a test controls.
#[test]
#[ignore = "needs ACL2"]
fn a_wrapped_block_value_survives_the_real_fmt() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("wrap");
    let mut s = Session::start(&acl2, &dir, SessionControl::new()).expect("start");

    let long: String =
        (0..14).map(|i| format!("/deeply-nested-directory-{i:02}")).collect::<String>() + "/books/";
    assert!(long.len() > 120, "the value must outrun ACL2's margin");
    let probe = format!(
        "(acl2::prog2$ (acl2::cw \"~%<<<ASS-BEGIN env>>>~%inhibit ~x0~%sbd ~x1~%<<<ASS-END env>>>~%\" \
           '(acl2::proof-tree acl2::observation acl2::warning acl2::history acl2::summary) \
           \"{long}\") \
         (acl2::value :invisible))"
    );
    let sub = s.submit(&probe, D).expect("submit");
    assert_eq!(sub.outcome, acl2_session::Outcome::Ok);

    let body = sub.block("env").expect("the block must arrive as a block");
    let flat: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains(&long),
            "the wrapped value must survive whole; got block: {body:?}");
    assert!(flat.contains("proof-tree") || flat.to_lowercase().contains("proof-tree"),
            "the inhibit list survives too: {body:?}");
    assert!(!sub.output.contains("<<<ASS-"),
            "no marker fragments may leak into the capture: {:?}", sub.output);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A deeply nested book directory works end to end, and the environment
/// report survives it.
#[test]
#[ignore = "needs ACL2"]
fn a_deeply_nested_book_directory_works() {
    let acl2 = acl2_or_skip!();
    let mut dir = scratch("deep");
    for _ in 0..6 {
        dir = dir.join("a-rather-deeply-nested-directory-level");
    }
    std::fs::create_dir_all(&dir).expect("deep scratch dir");
    assert!(dir.to_str().unwrap().len() > 120, "the path must be genuinely deep");
    let file = dir.join("deep.lisp");
    std::fs::write(&file, "(in-package \"ACL2\")\n(defun deep-f (x) x)\n").unwrap();

    let mut b = Book::open(&acl2, &file).unwrap();
    let sbd = b.books_dir().expect("the books dir must survive").to_path_buf();
    assert!(sbd.is_absolute(), "ACL2 reports an absolute books dir: {sbd:?}");
    assert!(sbd.join("system").is_dir() || sbd.join("std").is_dir(),
            "expected a real books tree, got {sbd:?}");

    let o = b.sync(None, D).expect("sync");
    assert!(o.failed.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

/// Interrupting a waterfall proof yields an envelope, not a bare boolean.
///
/// The interrupted form never reports, so everything a client learns must
/// come from the interrupt itself: what was interrupted, after how long, and
/// -- this is what the test settles -- whether `gag-state-saved` carries real
/// checkpoints when the interrupt lands mid-waterfall rather than inside an
/// executable grind.
#[test]
#[ignore = "needs ACL2"]
fn interrupting_a_waterfall_proof_reports_an_envelope() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("wf-intr");
    let file = dir.join("wf.lisp");
    // A boolean tautology over many variables: clausification and rewriting
    // grind through an exponential case split, all inside the waterfall.
    let vars: Vec<String> = (1..=20).map(|i| format!("b{i}")).collect();
    let mut chain = vars.last().unwrap().clone();
    for v in vars.iter().rev().skip(1) {
        chain = format!("(xor {v} {chain})");
    }
    let mut rev_chain = vars.first().unwrap().clone();
    for v in vars.iter().skip(1) {
        rev_chain = format!("(xor {v} {rev_chain})");
    }
    let hyps: Vec<String> = vars.iter().map(|v| format!("(booleanp {v})")).collect();
    std::fs::write(
        &file,
        format!(
            "(in-package \"ACL2\")\n\
             (defthm xor-grind (implies (and {}) (iff {chain} {rev_chain}))\n\
               :rule-classes nil)\n",
            hyps.join(" ")
        ),
    )
    .unwrap();

    let mut b = Book::open(&acl2, &file).unwrap();
    let o = b.sync(None, Duration::from_secs(3)).expect("sync");
    assert_eq!(o.failed.as_ref().map(|f| f.status), Some(Status::DeadlineExceeded),
               "the grind must outrun the deadline for this test to mean anything");

    let r = b.interrupt().expect("interrupt");
    assert!(r.ok, "the session recovers");
    let env = r.interrupted.expect("an interrupted proof must carry an envelope");
    assert!(
        env.abort_cause.as_deref().unwrap_or("").to_lowercase().contains("interrupt"),
        "the envelope's cause is the interrupt: {:?}", env.abort_cause
    );
    assert!(env.time.is_some(), "the elapsed prove time is recorded");
    assert!(env.event.as_deref().unwrap_or("").to_lowercase().contains("xor-grind"),
            "the envelope names what was interrupted: {:?}", env.event);
    assert!(env.output_ref.is_some(), "the partial capture is attached");
    // Whether a waterfall interrupt carries checkpoints was an open question;
    // record the answer loudly either way rather than asserting a guess.
    eprintln!(
        "waterfall interrupt checkpoints: n_top={} n_sub={} (top={}, sub={})",
        env.checkpoints.n_top, env.checkpoints.n_sub,
        env.checkpoints.top.len(), env.checkpoints.sub.len()
    );

    assert_eq!(b.health(Duration::from_secs(5)), Health::Ready);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A top-level `(in-package "P")` sticks, and rollback restores the package.
///
/// In a directory with no `acl2-customization.lsp`, a human session that
/// types the book's forms reads everything after `(in-package "P")` in `P`.
/// The `ld`-per-form transport reverts the switch on every exit, so without
/// the second phase the session silently reads later forms in "ACL2" --
/// symbols intern elsewhere and the world diverges from the reference. The
/// package is state, not world, so undoing units must also restore it.
#[test]
#[ignore = "needs ACL2"]
fn in_package_sticks_and_rollback_restores_it() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("inpkg");
    let file = dir.join("p.lisp");
    write(&file, &[
        "(in-package \"ACL2\")",
        "(defpkg \"IPTEST\" '(defun defthm equal))",
        "(in-package \"IPTEST\")",
        "(defun foo (x) x)",
    ]);
    let mut b = Book::open(&acl2, &file).unwrap();
    let o = b.sync(None, D).expect("sync");
    assert!(o.failed.is_none(), "{:?}", o.failed.as_ref().and_then(|f| f.error_line.clone()));
    assert_eq!(o.admitted.len(), 4);

    // The switch persisted: an eval reads in IPTEST, so the unqualified name
    // `foo` names IPTEST::FOO -- exactly what a human session would see.
    let r = b.eval("(acl2::cw \"FOOPKG=[~s0]~%\" (acl2::symbol-package-name 'foo))", D)
        .expect("eval");
    let text = b.output(&r.output_ref, Slice::All).unwrap().text;
    assert!(text.contains("FOOPKG=[IPTEST]"),
            "later forms must read in the switched package: {text}");

    // Roll back across the switch by truncating the file above it. The label
    // planted before the switch must still be reachable (labels intern in the
    // ACL2 package for exactly this reason), and the session's package must
    // come back too.
    write(&file, &[
        "(in-package \"ACL2\")",
        "(defpkg \"IPTEST\" '(defun defthm equal))",
    ]);
    let o = b.sync(None, D).expect("rollback sync");
    assert_eq!((o.reused, o.rolled_back, o.rebuilt), (2, 2, false),
               "an undo, not a replay, must cross the package switch");
    let r = b.eval(
        "(acl2::cw \"PKGNOW=[~s0]~%\" \
         (acl2::f-get-global 'acl2::current-package acl2::state))", D)
        .expect("eval");
    let text = b.output(&r.output_ref, Slice::All).unwrap().text;
    assert!(text.contains("PKGNOW=[ACL2]"),
            "rollback must restore the package along with the world: {text}");

    // And an unknown package still fails cleanly through the wrapper -- the
    // failure signal is the reason for the two-phase scheme.
    write(&file, &[
        "(in-package \"ACL2\")",
        "(defpkg \"IPTEST\" '(defun defthm equal))",
        "(in-package \"NOPE\")",
    ]);
    let o = b.sync(None, D).expect("bad package sync");
    let f = o.failed.expect("an unknown package must fail the unit");
    assert_eq!(f.status, Status::Rejected);
    assert!(f.error_line.is_some(), "with the error surfaced");
    assert_eq!(b.health(Duration::from_secs(5)), Health::Ready, "and no hang");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The deadline bounds the call, not each unit within it.
///
/// Under the old per-unit deadline, a long book of individually fast forms
/// ran to completion however small the deadline: five hundred forms at a few
/// milliseconds each never tripped a per-unit clock, and the "bounded" call
/// ran for however long the file needed.
#[test]
#[ignore = "needs ACL2"]
fn the_deadline_bounds_the_whole_call() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("budget");
    let file = dir.join("many.lisp");
    let mut forms = vec!["(in-package \"ACL2\")".to_string()];
    for i in 0..500 {
        forms.push(format!("(defun budget-f{i} (x) x)"));
    }
    let joined: Vec<&str> = forms.iter().map(|s| s.as_str()).collect();
    write(&file, &joined);

    let mut b = Book::open(&acl2, &file).unwrap();
    let started = std::time::Instant::now();
    let o = b.sync(None, Duration::from_millis(200)).expect("sync");
    let took = started.elapsed();
    assert!(o.failed.as_ref().is_some_and(|f| f.still_running),
            "the call must stop at the budget with a unit in flight");
    assert!(o.admitted.len() < 500, "and cannot have admitted everything");
    assert!(took < Duration::from_secs(30),
            "a 200ms budget must not run the whole book: took {took:?}");

    // The in-flight unit finishes on resume, and the next sync carries on.
    let mut done = false;
    for _ in 0..30 {
        match b.resume(Duration::from_secs(5)).expect("resume") {
            Some(Resumed::StillRunning { .. }) => continue,
            Some(Resumed::Admitted(_)) => { done = true; break; }
            other => panic!("expected the trivial unit to admit, got {:?}", other.is_some()),
        }
    }
    assert!(done, "the pending unit must complete");
    let o = b.sync(None, D).expect("continue");
    assert!(o.failed.is_none());
    assert_eq!(o.reused + o.admitted.len(), 501, "the book completes across calls");

    let _ = std::fs::remove_dir_all(&dir);
}
