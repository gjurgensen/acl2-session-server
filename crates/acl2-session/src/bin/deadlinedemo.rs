//! Exercises the deadline model against a live ACL2.
//!
//!   acl2-session-deadlinedemo <acl2-binary>
//!
//! A deadline bounds the call, not the proof: an expired deadline hands back
//! control without disturbing ACL2, and the caller chooses what to do next.

use std::path::{Path, PathBuf};
use std::time::Duration;

use acl2_session::{Book, Resumed};

fn write(path: &Path, forms: &[&str]) {
    std::fs::write(path, forms.join("\n\n") + "\n").expect("write book");
}

fn expect(cond: bool, what: &str, fails: &mut usize) {
    println!("   {} {what}", if cond { "ok  " } else { "FAIL" });
    if !cond {
        *fails += 1;
    }
}

fn main() {
    let acl2 = PathBuf::from(std::env::args().nth(1).expect("usage: <acl2-binary>"));
    let dir = std::env::temp_dir().join(format!("acl2-deadline-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let book = dir.join("slow.lisp");

    // Naive fib 44 forces the prover to grind for a minute or so.
    write(&book, &[
        "(in-package \"ACL2\")",
        "(defun fib (n) (if (zp n) 0 (if (= n 1) 1 (+ (fib (- n 1)) (fib (- n 2))))))",
        "(thm (equal (fib 44) 701408733))",
    ]);

    let mut b = Book::open(&acl2, &book).expect("open");
    let mut fails = 0usize;

    println!("\n── sync with a 3s deadline against a ~minute proof");
    let o = b.sync(Some(3), Duration::from_secs(3)).expect("sync");
    println!("   admitted={} failed={:?}", o.admitted.len(),
             o.failed.as_ref().map(|f| f.status));
    let o = b.sync(None, Duration::from_secs(3)).expect("sync 2");
    let timed_out = o.failed.as_ref().map(|f| f.status);
    println!("   third unit: {:?}", timed_out);
    expect(timed_out == Some(acl2_session::Status::DeadlineExceeded),
           "the deadline expired rather than the call blocking", &mut fails);
    expect(b.running_unit().is_some(), "the unit is still recorded as running",
           &mut fails);

    println!("\n── the session refuses a second form while one is in flight");
    match b.sync(None, Duration::from_secs(2)) {
        Err(acl2_session::SyncError::Busy { index }) => {
            expect(true, &format!("rejected: unit {index} is still running"), &mut fails)
        }
        other => {
            println!("   got {other:?}");
            expect(false, "sync should have been rejected as busy", &mut fails)
        }
    }

    println!("\n── progress while it runs");
    std::thread::sleep(Duration::from_secs(2));
    match b.progress(3).expect("progress") {
        Some(p) => expect(p.elapsed >= Duration::from_secs(2),
                          &format!("running {:?}, {} bytes so far", p.elapsed, p.bytes_so_far),
                          &mut fails),
        None => expect(false, "expected progress for a running form", &mut fails),
    }

    println!("\n── resume with another short deadline: still running, undisturbed");
    match b.resume(Duration::from_secs(2)).expect("resume") {
        Some(Resumed::StillRunning { elapsed }) => {
            expect(true, &format!("still running after {elapsed:?}"), &mut fails)
        }
        other => {
            println!("   got {:?}", other.is_some());
            expect(false, "expected StillRunning", &mut fails)
        }
    }

    println!("\n── interrupt, and the session comes back");
    let ok = b.interrupt().expect("interrupt");
    expect(ok, "interrupt recovered the session", &mut fails);
    expect(b.running_unit().is_none(), "nothing is in flight any more", &mut fails);

    println!("\n── and it is usable again");
    write(&book, &["(in-package \"ACL2\")",
                   "(defun fib (n) (if (zp n) 0 (if (= n 1) 1 (+ (fib (- n 1)) (fib (- n 2))))))",
                   "(defthm easy (equal (fib 3) 2))"]);
    let o = b.sync(None, Duration::from_secs(60)).expect("final sync");
    expect(o.failed.is_none() && o.reused == 2 && o.admitted.len() == 1,
           "the fixed file syncs cleanly, reusing the prefix", &mut fails);

    let _ = std::fs::remove_dir_all(&dir);
    if fails > 0 {
        eprintln!("\n{fails} expectation(s) not met");
        std::process::exit(1);
    }
    eprintln!("\nall expectations met");
}
