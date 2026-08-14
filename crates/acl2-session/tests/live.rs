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
    assert!(!top.contains("<goal>"), "placeholder leaked through: {top:?}");
    assert!(
        top.contains("my-rev") && top.contains("equal"),
        "expected the real goal term, got {top:?}"
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
        c.top.first().map_or(false, |g| g.contains("17")),
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

    let noisy = "(ld (make-list 4000 :initial-element '(cw \"~x0~%\" (make-list 200))))";
    let r = b.eval(noisy, Duration::from_millis(400)).unwrap();
    assert_eq!(r.outcome, acl2_session::Outcome::StillRunning);
    let handed_out = r.output_ref.clone();

    assert!(b.interrupt().expect("interrupt"), "the form must be interruptible");

    let got = b.output(&handed_out, Slice::All).expect("still readable after interrupt");
    assert!(got.total_lines > 0,
            "what the form printed before the interrupt must survive");
    assert!(got.superseded,
            "it describes an abandoned form, not the current world");

    // And the session is usable.
    let ev = b.eval("(+ 1 1)", Duration::from_secs(30)).unwrap();
    assert_eq!(ev.outcome, acl2_session::Outcome::Ok);
}
