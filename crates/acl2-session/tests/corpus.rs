//! Corpus check for the splitter.
//!
//! Ignored by default because it needs a book tree. Run with:
//!
//! ```text
//! ACL2_BOOKS=/path/to/acl2/books cargo test -p acl2-session -- --ignored --nocapture
//! ```
//!
//! This is the cheapest high-value test available for the splitter: thousands
//! of real files, and the failure mode we care about (a construct we cannot
//! lex) is exactly what it detects. Every bug found in the splitter so far was
//! found by running it at scale, and none by reasoning about it -- `#!PKG`,
//! `#{"""..."""}`, `#0=`/`#0#`, and both normalize/lexer divergences (`#\;`,
//! `A\ ` before `)`) all surfaced this way.
//!
//! Point `ACL2_BOOKS` at a tree of ACL2 books. `books/quicklisp/bundle` is
//! vendored raw Common Lisp -- `#p` pathnames and friends, which ACL2's own
//! reader also rejects -- and fails by design; aim at `books/kestrel` or
//! exclude the bundle when sweeping a whole community checkout.

use std::path::{Path, PathBuf};

fn lisp_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            lisp_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "lisp") {
            out.push(p);
        }
    }
}

#[test]
#[ignore = "needs ACL2_BOOKS pointing at a book tree"]
fn splits_every_book_in_the_corpus() {
    let root = match std::env::var("ACL2_BOOKS") {
        Ok(r) => PathBuf::from(r),
        Err(_) => {
            eprintln!("ACL2_BOOKS not set; skipping");
            return;
        }
    };
    let mut files = Vec::new();
    lisp_files(&root, &mut files);
    files.sort();
    assert!(!files.is_empty(), "no .lisp files under {}", root.display());

    // Read bytes, not a String. ACL2 source is ISO-8859-1, and at least one
    // book in the community tree contains raw 0x80-0xFF bytes. An earlier
    // version of this test used `read_to_string` with `let Ok(..) else
    // { continue }` and silently skipped that file while reporting success --
    // a hole that hid 284 units. A file we cannot read is a failure.
    let (mut units, mut failures) = (0usize, Vec::new());
    for f in &files {
        match std::fs::read(f) {
            Ok(bytes) => match acl2_session::split(&bytes) {
                Ok(forms) => {
                    units += forms.len();
                    // `split(normalize(u))` must give exactly the one unit
                    // back, and normalising again must change nothing. A
                    // normalize/lexer divergence that drops or extends text
                    // fails loudly here, usually as an unterminated list --
                    // the regression net for the `#\;` hash collision, and
                    // for any future divergence between the two lexers.
                    for u in &forms {
                        let norm = acl2_session::split::normalize(&bytes[u.start..u.end]);
                        match acl2_session::split(&norm) {
                            Ok(v) if v.len() == 1 => {
                                if acl2_session::split::normalize(&norm) != norm {
                                    failures.push(format!(
                                        "{}:{}: normalize is not a fixpoint",
                                        f.display(), u.line_start));
                                }
                            }
                            Ok(v) => failures.push(format!(
                                "{}:{}: normalize yields {} forms, not 1",
                                f.display(), u.line_start, v.len())),
                            Err(e) => failures.push(format!(
                                "{}:{}: normalize does not re-split: {e}",
                                f.display(), u.line_start)),
                        }
                    }
                }
                Err(e) => failures.push(format!("{}: {e}", f.display())),
            },
            Err(e) => failures.push(format!("{}: unreadable: {e}", f.display())),
        }
    }

    if let Ok(dump) = std::env::var("ACL2_SPLIT_DUMP") {
        let mut w = String::new();
        for f in &files {
            let bytes = std::fs::read(f).unwrap_or_default();
            let n = acl2_session::split(&bytes).map(|v| v.len()).unwrap_or(0);
            w.push_str(&format!("{n} {}\n", f.display()));
        }
        std::fs::write(dump, w).unwrap();
    }
    println!("books: {}  units: {}", files.len(), units);
    for f in failures.iter().take(20) {
        println!("  FAIL {f}");
    }
    assert!(failures.is_empty(), "{} book(s) failed to split", failures.len());
}

/// The definer scan, at corpus scale.
///
/// It is a convenience index rather than a strict reader, so there is no
/// "correct" answer to assert against. What can be asserted is that it never
/// panics on real source, that it finds a plausible amount (a book tree
/// defines a great many names), and -- the property that actually matters --
/// that every site it reports really is a `(<definer> <name> ...)` in the
/// text, so a jump never lands somewhere arbitrary.
#[test]
#[ignore = "needs ACL2_BOOKS pointing at a book tree"]
fn definer_sites_hold_up_across_the_corpus() {
    let root = match std::env::var("ACL2_BOOKS") {
        Ok(r) => PathBuf::from(r),
        Err(_) => {
            eprintln!("ACL2_BOOKS not set; skipping");
            return;
        }
    };
    let mut files = Vec::new();
    lisp_files(&root, &mut files);
    let (mut sites, mut with_none, mut bad) = (0usize, 0usize, Vec::new());
    for f in &files {
        let Ok(bytes) = std::fs::read(f) else { continue };
        let found = acl2_session::definer_sites(&bytes);
        if found.is_empty() {
            with_none += 1;
        }
        sites += found.len();
        for d in &found {
            // The offset must point at the paren of a form whose head is the
            // definer we reported. Checked on every site, since an index that
            // pointed one form off would be worse than no index.
            let at = bytes.get(d.offset).copied();
            if at != Some(b'(') {
                bad.push(format!("{}:{}: offset is not a form start", f.display(), d.line));
                continue;
            }
            let rest = &bytes[d.offset + 1..];
            let head_start = rest.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(0);
            if !rest[head_start..].starts_with(d.head.as_bytes()) {
                bad.push(format!("{}:{}: head is not {:?}", f.display(), d.line, d.head));
            }
            if d.line == 0 {
                bad.push(format!("{}: line 0 is not a line", f.display()));
            }
        }
    }
    println!(
        "books: {}  definer sites: {}  books defining nothing: {}",
        files.len(), sites, with_none
    );
    for b in bad.iter().take(20) {
        println!("  FAIL {b}");
    }
    assert!(bad.is_empty(), "{} bad site(s)", bad.len());
    assert!(sites > files.len(), "a book tree defines more names than it has files");
}
