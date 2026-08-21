//! Finding where a name is defined inside one book's text.
//!
//! Three tiers, in the order they are tried, and the point of the third is
//! that it exists at all: most of what a modern ACL2 book defines is
//! *generated*, and appears nowhere in the file that defines it.
//!
//! 1. **Definer.** The name is written in definer position. Exact.
//! 2. **Decomposition.** Generator macros almost always build their output
//!    names by affixing to a name the user wrote -- `patbind-X`, `X-of-cons`,
//!    `X->field`, `pseudo-termp-of-X`. Split the symbol and look for the
//!    longest run of its parts that *is* written in definer position.
//! 3. **World-order anchoring.** Within one book, `include-book` replays
//!    forms in file order and each form's expansion lays down its events
//!    contiguously, so the census's order for that book is file order. The
//!    nearest preceding locatable name is then the generating form.
//!
//! **Neither 2 nor 3 is sufficient alone, and they fail in opposite places.**
//! Measured over the 25 largest attributed books of a `std/top` +
//! `centaur/fty/top` world (3,210 generated names): anchoring alone 92.6%,
//! decomposition alone 90.2%, and the two combined -- decomposition *windowed*
//! by the anchor, falling back to the anchor -- **100%**. `fty/baselists.lisp`
//! has no locatable name at all, so anchoring resolves nothing there and
//! decomposition resolves all 238; `xdoc/constructors.lisp` is the mirror
//! image. Windowing earns its keep on cases like `defind-pred-infop`, where
//! naked decomposition matches the too-short token `pred` two thousand lines
//! away and the window throws it out.

use std::collections::HashMap;

use crate::split::definer_sites;

/// How a location was arrived at, so a caller can say how sure it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// The name is written there. Exact.
    Definer,
    /// Generated; located by decomposing it to a name that is written.
    Decomposed,
    /// Generated; located by world order alone. The form is *near* here.
    Anchored,
}

impl How {
    pub fn as_str(self) -> &'static str {
        match self {
            How::Definer => "definer",
            How::Decomposed => "decomposed",
            How::Anchored => "anchored",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Located {
    /// 1-based line of the form.
    pub line: usize,
    /// 1-based column of the form on that line, which is what tells it from
    /// another form written beside it.
    pub column: usize,
    pub how: How,
}

/// Case-folded and package-stripped: the key both sides of a lookup agree on.
///
/// A book that is `(in-package "FTY")` writes `(define fixtype …)` while the
/// world reports `fty::fixtype`, so the prefix cannot be required to appear.
fn fold(name: &str) -> String {
    name.rsplit("::").next().unwrap_or(name).to_ascii_lowercase()
}

/// The parts a generated name might have been built from.
fn parts(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let bytes = fold(name);
    let mut it = bytes.chars().peekable();
    while let Some(c) = it.next() {
        // `->` is one separator, not a `-` followed by a `>`: fty accessors
        // are `prod->field`, and splitting on the `-` alone would offer
        // `prod` and `>field` instead of the two names actually joined.
        if c == '-' && it.peek() == Some(&'>') {
            it.next();
            out.push(std::mem::take(&mut cur));
        } else if c == '-' || c == '.' {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out.push(cur);
    out.retain(|p| !p.is_empty());
    out
}

/// The longest contiguous run of `wanted`'s parts that names something in
/// `index`, preferring longer runs -- i.e. the most specific match.
fn decompose(wanted: &str, index: &HashMap<String, (usize, usize)>) -> Option<(usize, usize)> {
    let p = parts(wanted);
    let whole = fold(wanted);
    for len in (1..=p.len()).rev() {
        for i in 0..=(p.len() - len) {
            for sep in ["-", "->"] {
                let cand = p[i..i + len].join(sep);
                if cand != whole
                    && let Some(&at) = index.get(&cand)
                {
                    return Some(at);
                }
            }
        }
    }
    None
}

/// Where `wanted` is defined in `text`.
///
/// `book_names` is every name the world attributes to this book, **in file
/// order** -- which is census order for that book, reversed, since the world
/// is newest-first. Pass an empty slice to get tier 1 only, which is what a
/// caller without a census can honestly offer.
pub fn locate(text: &[u8], book_names: &[String], wanted: &str) -> Option<Located> {
    // Positions are compared, not just returned: the window below asks
    // whether one site precedes another, and two sites on one line are
    // ordered by nothing but their columns.
    let mut index: HashMap<String, (usize, usize)> = HashMap::new();
    for d in definer_sites(text) {
        // First wins: a name written twice is defined at the first, and the
        // later one is a redefinition or a `verify-termination` rerun.
        index.entry(fold(&d.name)).or_insert((d.line, d.column));
    }
    if let Some(&(line, column)) = index.get(&fold(wanted)) {
        return Some(Located { line, column, how: How::Definer });
    }

    // The window: between the nearest locatable name before `wanted` and the
    // nearest at or after it. Both may be absent -- a book whose names are
    // *all* generated has no anchor anywhere, which is exactly the case
    // decomposition carries by itself.
    let at = book_names.iter().position(|n| fold(n) == fold(wanted));
    let (mut lo, mut hi) = (None, None);
    if let Some(at) = at {
        lo = book_names[..at].iter().rev().find_map(|n| index.get(&fold(n)).copied());
        hi = book_names[at..].iter().find_map(|n| index.get(&fold(n)).copied());
    }

    match (decompose(wanted, &index), lo) {
        // Windowed decomposition: the most specific answer, and checked
        // against where the world says this name was introduced.
        (Some(d), _) if lo.is_none_or(|l| d >= l) && hi.is_none_or(|h| d <= h) => {
            Some(Located { line: d.0, column: d.1, how: How::Decomposed })
        }
        // Out of the window, so the decomposition matched something
        // unrelated; the anchor is the better answer.
        (_, Some(l)) => Some(Located { line: l.0, column: l.1, how: How::Anchored }),
        // No anchor at all: take the decomposition even unwindowed, since
        // there is nothing to check it against and it is all there is.
        (Some(d), None) => Some(Located { line: d.0, column: d.1, how: How::Decomposed }),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live example that motivated decomposition: `patbind-unless-casematch`
    /// occurs zero times in bstar.lisp and is generated by
    /// `(def-b*-binder unless-casematch …)` -- a form that introduces no
    /// textually-present name at all, so anchoring lands one form early.
    #[test]
    fn decomposition_finds_what_anchoring_would_miss() {
        let text = b"(defun access-b*-bindings (r v p) r)\n\
                     (def-b*-binder unless-casematch :body x)\n";
        let names = vec!["access-b*-bindings".to_string(),
                         "patbind-unless-casematch".to_string()];
        let got = locate(text, &names, "patbind-unless-casematch").expect("located");
        assert_eq!(got.how, How::Decomposed);
        assert_eq!(got.line, 2, "the def-b*-binder that generated it, not the defun before");
    }

    /// The mirror case: a book with no locatable name at all, where anchoring
    /// has nothing to offer and decomposition carries it alone.
    #[test]
    fn decomposition_works_with_no_anchor_anywhere() {
        let text = b"(fty::deflist nat-list :elt-type natp)\n";
        let names = vec!["nat-list-p".to_string(), "nat-list-fix".to_string()];
        let got = locate(text, &names, "nat-list-fix").expect("located");
        assert_eq!(got.how, How::Decomposed);
        assert_eq!(got.line, 1);
    }

    /// And the case windowing exists for. `defind-pred-infop` decomposes to
    /// the short generic token `pred`, which really is defined -- two
    /// thousand lines away in the book this was measured on, and forty here.
    /// The world says the name was introduced right after `defind-pred-info`,
    /// so the window throws the decomposition out and the anchor wins.
    ///
    /// The failure this guards against is the plausible-looking one: without
    /// the window the jump lands on an unrelated function whose name happens
    /// to be a fragment of the generated one.
    #[test]
    fn a_decomposition_outside_the_window_is_rejected() {
        let mut text = String::from("(defun pred (x) x)\n");
        for _ in 0..40 {
            text.push_str(";; filler\n");
        }
        text.push_str("(fty::defprod defind-pred-info ((a natp)))\n");
        let names = vec![
            "pred".to_string(),
            "defind-pred-info".to_string(),
            "defind-pred-infop".to_string(),
        ];
        // Unwindowed, decomposition would answer line 1.
        let mut bare = HashMap::new();
        bare.insert("pred".to_string(), (1usize, 1usize));
        assert_eq!(decompose("defind-pred-infop", &bare), Some((1, 1)));

        let got = locate(text.as_bytes(), &names, "defind-pred-infop").expect("located");
        assert_eq!(got.line, 42, "the defprod it came from, not the far-off `pred`");
        assert_eq!(got.how, How::Anchored, "reached by world order, so reported as near");
    }

    /// With no census to window against, tier 1 still answers exactly, and a
    /// generated name falls to whatever decomposition can manage.
    #[test]
    fn tier_one_needs_no_census() {
        let text = b"(defun alpha (x) x)\n(defthm alpha-of-nil t)\n";
        let got = locate(text, &[], "alpha-of-nil").expect("written, so exact");
        assert_eq!((got.line, got.how), (2, How::Definer));
        let got = locate(text, &[], "alpha").expect("also written");
        assert_eq!((got.line, got.how), (1, How::Definer));
    }

    /// Two definers on one line, which the line alone cannot tell apart.
    #[test]
    fn a_column_separates_two_definitions_on_one_line() {
        let text = b"(defun alpha (x) x) (defun beta (x) x)\n";
        let a = locate(text, &[], "alpha").expect("written");
        let b = locate(text, &[], "beta").expect("written too");
        assert_eq!((a.line, a.column), (1, 1));
        assert_eq!((b.line, b.column), (1, 21));
        assert_eq!((a.how, b.how), (How::Definer, How::Definer));
    }

    #[test]
    fn an_unrelated_name_is_not_invented() {
        let text = b"(defun alpha (x) x)\n";
        assert!(locate(text, &[], "zeta").is_none());
    }

    #[test]
    fn fty_accessors_split_on_the_arrow() {
        assert_eq!(parts("point->x"), vec!["point", "x"]);
        assert_eq!(parts("natp-listp-of-cons"), vec!["natp", "listp", "of", "cons"]);
        assert_eq!(parts("fty::point->x"), vec!["point", "x"], "package prefix dropped");
    }
}
