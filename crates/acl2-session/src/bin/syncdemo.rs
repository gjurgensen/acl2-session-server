//! End-to-end exercise of the file-prefix invariant against a live ACL2.
//!
//!   acl2-session-syncdemo <acl2-binary>
//!
//! Writes a small book into a temp directory and drives it through the cases
//! the invariant has to get right: first sync, no-op re-sync, a reformat that
//! must cost nothing, a real edit that must roll back, and a failure that must
//! stop the run without cascading.

use std::path::{Path, PathBuf};
use std::time::Duration;

use acl2_session::{Book, SyncOutcome};

const DEADLINE: Duration = Duration::from_secs(120);

fn write(path: &Path, forms: &[&str]) {
    std::fs::write(path, forms.join("\n\n") + "\n").expect("write book");
}

fn report(label: &str, o: &SyncOutcome) -> bool {
    println!("── {label}");
    println!(
        "   through={:?}  reused={}  rolled_back={}  admitted={}",
        o.resolved_through,
        o.reused,
        o.rolled_back,
        o.admitted.len()
    );
    for u in &o.admitted {
        let steps = u.event_data.prover_steps.map(|s| format!(" {s} steps"))
            .unwrap_or_default();
        println!("     + unit {} (lines {}-{}) {} ms{}", u.index, u.line_start, u.line_end,
                 u.elapsed.as_millis(), steps);
        if let Some(r) = &u.event_data.rules {
            println!("         rules: {}", one_line(r, 66));
        }
    }
    if let Some(f) = &o.failed {
        println!("     ! unit {} (lines {}-{}) failed{}", f.index, f.line_start,
                 f.line_end, if f.still_running { " [deadline]" } else { "" });
        if let Some(a) = &f.event_data.abort_causes {
            println!("         abort-causes : {}", one_line(a, 60));
        }
        if let Some(s) = f.event_data.prover_steps {
            println!("         prover steps : {s}");
        }
        if let Some(r) = &f.event_data.rules {
            println!("         rules        : {}", one_line(r, 60));
        }
        match (&f.checkpoints.top, &f.checkpoints.sub) {
            (None, None) => println!("         checkpoints  : (none reported)"),
            (t, s) => {
                if let Some(t) = t {
                    println!("         checkpoint top : {}", one_line(t, 58));
                }
                if let Some(s) = s {
                    println!("         checkpoint sub : {}", one_line(s, 58));
                }
            }
        }
    }
    o.failed.is_none()
}

fn one_line(s: &str, max: usize) -> String {
    let j: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if j.len() > max { format!("{}...", &j[..max]) } else { j }
}

fn expect(cond: bool, what: &str, fails: &mut usize) {
    if !cond {
        println!("   ✗ EXPECTED: {what}");
        *fails += 1;
    }
}

fn main() {
    let acl2: PathBuf = match std::env::args().nth(1) {
        Some(a) => PathBuf::from(a),
        None => {
            eprintln!("usage: acl2-session-syncdemo <acl2-binary>");
            std::process::exit(2);
        }
    };
    let dir = std::env::temp_dir().join(format!("acl2-syncdemo-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let book = dir.join("demo.lisp");

    let f_alpha = "(defun alpha (x) (if (consp x) (alpha (cdr x)) nil))";
    let f_beta = "(defun beta (x) (cons x x))";
    let f_thm = "(defthm alpha-of-nil (equal (alpha nil) nil))";

    write(&book, &["(in-package \"ACL2\")", f_alpha, f_beta, f_thm]);

    eprintln!("book: {}", book.display());
    let mut b = Book::open(&acl2, &book).expect("open book");
    eprintln!("session up\n");

    println!("checkpoints available: {}\n", b.checkpoints_available());
    let mut fails = 0usize;

    let o = b.sync(None, DEADLINE).expect("sync 1");
    report("first sync: everything is new", &o);
    expect(o.admitted.len() == 4 && o.reused == 0, "4 admitted, 0 reused", &mut fails);

    let o = b.sync(None, DEADLINE).expect("sync 2");
    report("re-sync, file untouched: nothing to do", &o);
    expect(o.reused == 4 && o.admitted.is_empty() && o.rolled_back == 0,
           "4 reused, nothing re-admitted", &mut fails);

    // Reindent and add a comment inside a unit. Normalised hashing means this
    // must cost no re-proving at all.
    write(&book, &[
        "(in-package \"ACL2\")",
        f_alpha,
        "(defun beta (x)\n  ; a passing remark\n  (cons x\n        x))",
        f_thm,
    ]);
    let o = b.sync(None, DEADLINE).expect("sync 3");
    report("reformat a unit: layout is not a change", &o);
    expect(o.reused == 4 && o.admitted.is_empty(), "still 4 reused", &mut fails);

    // A real edit to unit 2. Everything from there on must be undone and redone.
    write(&book, &["(in-package \"ACL2\")", f_alpha,
                   "(defun beta (x) (cons x (cons x x)))", f_thm]);
    let o = b.sync(None, DEADLINE).expect("sync 4");
    report("edit unit 2: roll back and re-admit from there", &o);
    expect(o.reused == 2 && o.rolled_back == 2 && o.admitted.len() == 2,
           "2 reused, 2 rolled back, 2 re-admitted", &mut fails);

    // A failing unit must stop the run, leaving the prefix before it intact.
    write(&book, &["(in-package \"ACL2\")", f_alpha,
                   "(defun beta (x) (cons x (cons x x)))", f_thm,
                   "(defthm obviously-false (equal (alpha x) x))",
                   "(defun never-reached (x) x)"]);
    let o = b.sync(None, DEADLINE).expect("sync 5");
    report("a failing unit stops the run", &o);
    expect(o.failed.is_some(), "the false theorem is reported as a failure", &mut fails);
    expect(o.admitted.is_empty(), "nothing new admitted before the failure", &mut fails);
    expect(o.failed.as_ref().map(|f| f.index) == Some(4), "failure is unit 4", &mut fails);
    let f = o.failed.as_ref().unwrap();
    expect(f.event_data.abort_causes.is_some(), "an abort cause is reported", &mut fails);
    expect(f.checkpoints.top.is_some() || f.checkpoints.sub.is_some(),
           "at least one key checkpoint is reported", &mut fails);

    // Fixing the failing unit needs no rollback: it was never admitted.
    write(&book, &["(in-package \"ACL2\")", f_alpha,
                   "(defun beta (x) (cons x (cons x x)))", f_thm,
                   "(defthm now-true (equal (alpha nil) nil))",
                   "(defun never-reached (x) x)"]);
    let o = b.sync(None, DEADLINE).expect("sync 6");
    report("fix the failing unit: no rollback needed", &o);
    expect(o.rolled_back == 0 && o.admitted.len() == 2 && o.failed.is_none(),
           "0 rolled back, 2 admitted, no failure", &mut fails);

    // Targeting a line inside a unit selects that unit.
    let o = b.sync(Some(3), DEADLINE).expect("sync 7");
    report("sync through line 3 only", &o);
    expect(o.resolved_through == Some(1), "resolves to unit 1", &mut fails);

    // Exercise rebuild-by-replay directly. Its real trigger -- a rollback that
    // cannot be verified -- is not reproducible on demand, so drive the
    // mechanism explicitly and check the world it leaves behind.
    println!("── rebuild by replay (mechanism, driven directly)");
    b.rebuild(4, DEADLINE).expect("rebuild");
    println!("   rebuilds={}", b.rebuilds());
    let o = b.sync(None, DEADLINE).expect("sync after rebuild");
    println!(
        "   after rebuild: reused={} admitted={} failed={}",
        o.reused,
        o.admitted.len(),
        o.failed.is_some()
    );
    expect(b.rebuilds() == 1, "one rebuild recorded", &mut fails);
    expect(o.reused == 4, "the replayed prefix is recognised as reusable", &mut fails);
    expect(o.admitted.len() == 2 && o.failed.is_none(),
           "the remaining 2 units are admitted on top", &mut fails);

    let _ = std::fs::remove_dir_all(&dir);
    if fails > 0 {
        eprintln!("\n{fails} expectation(s) not met");
        std::process::exit(1);
    }
    eprintln!("\nall expectations met");
}
