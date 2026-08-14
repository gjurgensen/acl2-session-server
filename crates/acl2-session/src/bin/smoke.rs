//! Walking-skeleton smoke test: prove the whole vertical works end to end.
//!
//!   acl2-session-smoke <acl2-binary> <book-dir> [form ...]
//!
//! With no forms given, runs a built-in sequence that exercises the parts the
//! design depends on: the inherited environment, a form that succeeds, a form
//! that fails, and structured extraction after a failure.

use std::path::Path;
use std::time::Duration;

use acl2_session::{Outcome, Session};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: acl2-session-smoke <acl2-binary> <book-dir> [form ...]");
        std::process::exit(2);
    }
    let (acl2, dir) = (Path::new(&args[1]), Path::new(&args[2]));

    let forms: Vec<String> = if args.len() > 3 {
        args[3..].to_vec()
    } else {
        vec![
            // Did the directory's acl2-customization.lsp set the package?
            "(acl2::cw \"current-package = ~x0~%\" (acl2::@ acl2::current-package))".into(),
            // A form that should be admitted.
            "(acl2::defun sk-len (x) (acl2::if (acl2::consp x) \
              (acl2::+ 1 (sk-len (acl2::cdr x))) 0))"
                .into(),
            // A form that should fail, with checkpoints.
            "(acl2::thm (acl2::equal (sk-len x) x))".into(),
            // Structured extraction should survive that failure.
            "(acl2::get-event-data 'acl2::abort-causes acl2::state)".into(),
        ]
    };

    eprintln!("starting {} in {}", acl2.display(), dir.display());
    let mut s = match Session::start(acl2, dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to start ACL2: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("session up\n");

    let mut failures = 0;
    for (i, form) in forms.iter().enumerate() {
        let short: String = form.split_whitespace().collect::<Vec<_>>().join(" ");
        let short = if short.len() > 68 { format!("{}...", &short[..65]) } else { short };
        println!("── form {i}: {short}");
        match s.submit(form, Duration::from_secs(60)) {
            Ok(sub) => {
                println!("   outcome: {:?}   ({} ms)", sub.outcome, sub.elapsed.as_millis());
                for line in sub.output.lines().filter(|l| !l.trim().is_empty()).take(200) {
                    println!("   | {}", line.trim_end());
                }
                if sub.outcome == Outcome::StillRunning {
                    println!("   (deadline expired; interrupting)");
                    match s.interrupt() {
                        Ok(true) => println!("   interrupt: session recovered"),
                        Ok(false) => {
                            println!("   interrupt: FAILED to recover");
                            failures += 1;
                        }
                        Err(e) => {
                            println!("   interrupt error: {e}");
                            failures += 1;
                        }
                    }
                }
            }
            Err(e) => {
                println!("   ERROR: {e}");
                failures += 1;
            }
        }
        println!();
    }
    if failures > 0 {
        eprintln!("{failures} problem(s)");
        std::process::exit(1);
    }
    eprintln!("smoke test completed");
}
