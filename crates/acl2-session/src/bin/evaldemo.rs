//! Exercises `eval` against a live ACL2.
//!
//!   acl2-session-evaldemo <acl2-binary>
//!
//! eval cannot change the world; it may change state.

use std::path::{Path, PathBuf};
use std::time::Duration;

use acl2_session::{Book, Health, Slice, WorldChange};

const D: Duration = Duration::from_secs(60);

fn write(path: &Path, forms: &[&str]) {
    std::fs::write(path, forms.join("\n\n") + "\n").expect("write");
}

fn expect(cond: bool, what: &str, fails: &mut usize) {
    println!("   {} {what}", if cond { "ok  " } else { "FAIL" });
    if !cond { *fails += 1; }
}

fn main() {
    let acl2 = PathBuf::from(std::env::args().nth(1).expect("usage: <acl2-binary>"));
    let dir = std::env::temp_dir().join(format!("acl2-eval-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let book = dir.join("b.lisp");
    write(&book, &["(in-package \"ACL2\")",
                   "(defun alpha (x) (if (consp x) (alpha (cdr x)) nil))",
                   "(defun fib (n) (if (zp n) 0 (if (= n 1) 1 \
                      (+ (fib (- n 1)) (fib (- n 2))))))"]);

    let mut b = Book::open(&acl2, &book).expect("open");
    let mut fails = 0usize;

    let o = b.sync(None, D).expect("sync");
    expect(o.admitted.len() == 3 && o.failed.is_none(), "book syncs", &mut fails);

    println!("\n── a query leaves the world alone");
    let r = b.eval("(acl2::pe 'alpha)", D).expect("eval pe");
    let text = b.output(&r.output_ref, Slice::All).unwrap().text;
    expect(r.world_change == WorldChange::None, "world_change = None", &mut fails);
    expect(text.contains("alpha"), "and the definition came back", &mut fails);

    println!("\n── a thm introduces no event either");
    let r = b.eval("(acl2::thm (acl2::equal (alpha acl2::nil) acl2::nil))", D).unwrap();
    expect(r.world_change == WorldChange::None, "world_change = None", &mut fails);

    println!("\n── a defthm IS an event, and is undone");
    let r = b.eval("(acl2::defthm ev-tmp (acl2::equal (alpha acl2::nil) acl2::nil))", D)
        .expect("eval defthm");
    expect(r.world_change == WorldChange::Undone, "world_change = Undone", &mut fails);

    println!("\n── so it is not visible to the next eval");
    let r = b.eval("(acl2::pe 'ev-tmp)", D).expect("eval pe2");
    let text = b.output(&r.output_ref, Slice::All).unwrap().text;
    expect(text.contains("Error") || text.contains("not") || text.contains("no "),
           "ev-tmp is gone from the world", &mut fails);

    println!("\n── state, unlike the world, does persist");
    b.eval("(acl2::set-gag-mode acl2::nil)", D).expect("set gag");
    let r = b.eval("(acl2::@ acl2::gag-mode)", D).expect("read gag");
    let text = b.output(&r.output_ref, Slice::All).unwrap().text;
    expect(text.to_lowercase().contains("nil"), "gag-mode setting stuck", &mut fails);

    println!("\n── the prefix still syncs cleanly afterwards");
    let o = b.sync(None, D).expect("resync");
    expect(o.reused == 3 && o.admitted.is_empty() && o.failed.is_none(),
           "prefix intact: 3 reused, nothing re-admitted", &mut fails);

    println!("\n── an eval that outruns its deadline");
    let slow = "(acl2::thm (acl2::equal (fib 44) 701408733))";
    let r = b.eval(slow, Duration::from_secs(2)).expect("slow eval");
    expect(r.outcome == acl2_session::Outcome::StillRunning,
           "eval reports StillRunning", &mut fails);
    let h = b.health(Duration::from_secs(2));
    println!("   health says: {h:?}");
    expect(h == Health::Busy, "health says Busy, not Dead", &mut fails);
    expect(b.interrupt().expect("interrupt"), "and it can be interrupted", &mut fails);

    println!("\n── health");
    expect(b.health(D) == Health::Alive, "session reports Alive", &mut fails);

    let _ = std::fs::remove_dir_all(&dir);
    if fails > 0 { eprintln!("\n{fails} expectation(s) not met"); std::process::exit(1); }
    eprintln!("\nall expectations met");
}
