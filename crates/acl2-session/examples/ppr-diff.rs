//! Lay out a corpus of terms, for diffing against ACL2's own printer.
//!
//! This is how the claim in `pretty.rs` -- that the layout is ACL2's -- is
//! checked against the only authority there is. Not a test, because it needs
//! an ACL2 and a corpus; a procedure, kept here so the measurement can be
//! repeated rather than remembered.
//!
//!   cargo run --example ppr-diff -p acl2-session -- corpus.txt 40 > ours.txt
//!
//! The corpus is one term per line. The ACL2 side reads the same file with
//! `read-object` and prints each term with the same separator:
//!
//!   (defun ppr-file (chan w state)
//!     (declare (xargs :mode :program :stobjs state))
//!     (mv-let (eofp obj state) (read-object chan state)
//!       (if eofp state
//!         (pprogn (princ$ "<<<TERM>>>" *standard-co* state)
//!                 (newline *standard-co* state)
//!                 (ppr2 (ppr1 obj (print-base) (print-radix) w 0 state nil)
//!                       0 *standard-co* state nil)
//!                 (newline *standard-co* state)
//!                 (ppr-file chan w state)))))
//!
//! One trap when reading the diff: generate the corpus in the same package
//! ACL2 will read it back in. A term printed by a session in some book's
//! package carries `acl2::` prefixes that ACL2 drops on re-read, and those
//! change a term's width -- so they show up as layout differences that are
//! nothing of the sort.
//!
//! Measured this way over 406 real terms from `kestrel/c/syntax/validator.lisp`
//! at widths 30, 40 and 60: **1218 of 1218 character-identical**.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(path), Some(width)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: ppr-diff <corpus> <width>");
        std::process::exit(2);
    };
    // Bytes, not a `String`: a corpus cut from real books carries whatever
    // bytes those books do, and ACL2 source is ISO-8859-1.
    let corpus = acl2_session::split::decode(&std::fs::read(path).expect("corpus"));
    let width: usize = width.parse().expect("width");
    for line in corpus.lines().filter(|l| !l.trim().is_empty()) {
        println!("<<<TERM>>>");
        println!("{}", acl2_session::pretty::pretty(line, width));
    }
}
