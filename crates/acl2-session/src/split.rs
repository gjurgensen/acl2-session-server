//! Splitting a book into top-level submission units.
//!
//! This is purely lexical: no symbols are interned and no package needs to
//! exist. That is not a compromise, it is the only thing that can work. ACL2's
//! own reader cannot be used to split a book up front, because reading form
//! `K` may require packages introduced by an `include-book` at form `J < K` --
//! measured on real Kestrel books, where `omap::delete` on line 269 depends on
//! an `include-book` on line 21. Reading and evaluation are interleaved;
//! lexing is not.
//!
//! The unit produced is a **submission unit**, not an ACL2 datum. A reader
//! conditional such as `#+skip (defthm ...)` is one unit that yields zero
//! events. That is the correct granularity for the file-prefix invariant.
//!
//! Validated against all 4111 books under `books/kestrel`: 93,319 units, no
//! failures, and agreement with ACL2's own reader wherever the two can be
//! compared.

#![forbid(unsafe_code)]
// This module is pure logic; all FFI lives in pty.rs and session.rs.
use std::fmt;

/// One top-level submission unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    /// Byte offset of the first character.
    pub start: usize,
    /// Byte offset one past the last character.
    pub end: usize,
    /// 1-based line of `start`.
    pub line_start: usize,
    /// 1-based line of the last character.
    pub line_end: usize,
    /// Hash of the *normalised* text (see [`normalize`]). Reformatting and
    /// comment-only edits leave this unchanged, so they cost no re-proving.
    pub hash: u64,
    /// The names this form introduces, as written, in the order they appear.
    ///
    /// Lexical, from the same scan [`definer_sites`] performs: these are the
    /// names in the *text*, not the ones ACL2 ends up with. A `define`
    /// introduces a dozen names in the world and writes exactly one, and the
    /// written one is what a reader means by "that form". Empty for
    /// `include-book`, `in-theory` and anything else that defines nothing.
    pub names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitError {
    pub offset: usize,
    pub line: usize,
    pub message: String,
}

impl fmt::Display for SplitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for SplitError {}

/// Split a book into top-level units.
///
/// Takes **bytes**, not `&str`, deliberately. ACL2 source is not UTF-8: its
/// default external format is ISO-8859-1, and real books in the community
/// tree contain raw bytes 0x80-0xFF. Decoding to `String` would either fail
/// outright or, if done lossily, shift every byte offset. Lexing bytes keeps
/// offsets faithful to the file on disk and sidesteps the encoding question
/// entirely.
///
/// Fails loudly rather than guessing. The prefix invariant depends on unit
/// boundaries being exactly right, so a construct we cannot lex must be an
/// error, never a best-effort split.
pub fn split(text: &[u8]) -> Result<Vec<Form>, SplitError> {
    let mut forms = Splitter { s: text }.forms()?;
    attach_names(text, &mut forms);
    Ok(forms)
}

/// Bucket the definer sites into the forms that contain them.
///
/// One extra linear pass rather than a scan per form: `definer_sites` is
/// already a single pass that never re-lexes a subtree, and both it and the
/// forms come out in offset order, so this walks the two in step. A
/// per-form scan would re-lex the whole file once per form, which is the
/// cost `definer_sites` was written to avoid.
fn attach_names(text: &[u8], forms: &mut [Form]) {
    let sites = definer_sites(text);
    let mut at = 0;
    for f in forms.iter_mut() {
        while at < sites.len() && sites[at].offset < f.start {
            at += 1; // between forms: whitespace or a comment
        }
        while at < sites.len() && sites[at].offset < f.end {
            f.names.push(sites[at].name.clone());
            at += 1;
        }
    }
}

/// Convenience wrapper for callers that already hold a `&str`.
pub fn split_str(text: &str) -> Result<Vec<Form>, SplitError> {
    split(text.as_bytes())
}

/// One `(<definer> <name> ...)` found in a book's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinerSite {
    /// The definer, as written: `defun`, `std::define`, `fty::defprod`.
    pub head: String,
    /// The name it introduces, as written, package prefix and all.
    pub name: String,
    /// Byte offset of the opening paren.
    pub offset: usize,
    /// 1-based line of that paren.
    pub line: usize,
}

/// Whether a head symbol looks like something that introduces a name.
///
/// Package-stripped and case-folded, then "starts with `def`" -- which covers
/// `defun`, `defthm`, `define`, `defines`, `defrule`, `deflist`, `defprod`
/// and the rest of the open-ended set books invent, without this having to
/// know any of them. That openness is the point: a fixed list would go stale
/// against the next macro someone writes.
///
/// It over-matches slightly -- `default-…` starts with `def` too -- and that
/// is the right direction to err. The cost is an unused entry in an index;
/// the cost of under-matching is a name that cannot be jumped to at all.
///
/// The first three *bytes* are compared, not the first three characters: a
/// head carrying a raw high byte reaches here as replacement characters (the
/// caller builds it with `from_utf8_lossy`), and slicing a `str` at 3 would
/// land inside one and panic. This runs on every buffer change, so a panic
/// here takes the editor daemon down rather than costing an index entry.
fn is_definer(head: &str) -> bool {
    let bare = head.rsplit("::").next().unwrap_or(head);
    bare.len() >= 3 && bare.as_bytes()[..3].eq_ignore_ascii_case(b"def")
}

/// Every `(<definer> <name> …)` in `text`, at any depth.
///
/// Lexical like the rest of this module, and structural rather than a regex
/// over lines: `mutual-recursion`, `defines`, `encapsulate` and `defsection`
/// hold their definers *inside* them, so a scan that only looked at top-level
/// forms would miss most of what a real book defines. Strings, `;` comments,
/// `#| |#` blocks and `#\(` character literals are handled because the
/// splitter's own reader is doing the walking -- this is the reason it lives
/// here rather than in a caller.
///
/// Text that will not lex yields what was found before the trouble, rather
/// than an error: an index is a convenience, and half of one still answers
/// most questions. `split` remains the strict reader.
pub fn definer_sites(text: &[u8]) -> Vec<DefinerSite> {
    let sp = Splitter { s: text };
    let mut out = Vec::new();
    let mut lines = LineCursor::new();
    let mut i = 0usize;
    // One linear pass: every byte is lexed once. The obvious shape -- read a
    // form, then recurse into it -- re-lexes each subtree at every level it
    // is nested under, so a token at depth d costs d times. That is invisible
    // on a tree of small books and ruinous on ACL2's own sources, which are a
    // few very large files of deeply nested forms: measured at 23 s for 14 MB
    // before this, against about a second after.
    //
    // The trick is that descending into a list costs nothing -- step past the
    // paren and keep scanning -- so `datum` is only ever called on an *atom*,
    // to skip it. Everything that is not a paren goes through `datum`, which
    // is what keeps `"a(b"`, `#\(` and `|a(b|` from being counted as lists.
    while i < text.len() {
        let Ok(j) = sp.skip_ws(i) else { return out };
        if j >= text.len() {
            return out;
        }
        match text[j] {
            b'(' => {
                if let Some(site) = sp.definer_at(j, &mut lines) {
                    out.push(site);
                }
                i = j + 1;
            }
            b')' => i = j + 1,
            _ => match sp.datum(j) {
                Ok(k) => i = k,
                Err(_) => return out,
            },
        }
    }
    out
}

/// The head symbol of the first form in `text`, as written.
///
/// `(mutual-recursion (defun f …) …)` yields `mutual-recursion`. Needed
/// because a form that introduces nothing itself still has to be *named* in
/// an outline, and its definers are its children rather than its identity.
/// `None` when the text does not start with a list, or the head is itself a
/// list.
pub fn head_of(text: &[u8]) -> Option<String> {
    let sp = Splitter { s: text };
    let start = sp.skip_ws(0).ok()?;
    if !sp.at(start, b'(') {
        return None;
    }
    let h0 = sp.skip_ws(start + 1).ok()?;
    if h0 >= text.len() || text[h0] == b'(' || text[h0] == b')' {
        return None;
    }
    let h1 = sp.datum(h0).ok()?;
    Some(decode(&text[h0..h1]))
}

/// One `(include-book "…" …)` found in a book's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludeSite {
    /// The book as written, without the quotes and without `.lisp`:
    /// `std/util/bstar`.
    pub book: String,
    /// The `:dir` argument as written -- `:system`, or a project alias --
    /// when one was given.
    pub dir: Option<String>,
    /// 1-based line of the opening paren.
    pub line: usize,
}

/// Every `(include-book …)` in `text`, at any depth.
///
/// At any depth because they are routinely wrapped: `(local (include-book
/// …))` is the common case, and `encapsulate` bodies hold more. Reusing the
/// reader means a book name inside a string or a commented-out include is not
/// mistaken for a real one -- and, less obviously, that `#+skip` and friends
/// do not confuse it, since the walk is over data rather than lines.
///
/// The `:dir` value is returned as written rather than resolved: only the
/// caller knows the session's books directory, and only ACL2 knows a
/// project's own `:dir` aliases.
pub fn include_sites(text: &[u8]) -> Vec<IncludeSite> {
    let sp = Splitter { s: text };
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        let Ok(j) = sp.skip_ws(i) else { return out };
        if j >= text.len() {
            return out;
        }
        let Ok(end) = sp.datum(j) else { return out };
        sp.includes_in(j, end, &mut out);
        i = end;
    }
}

/// Find `needle` in `hay`, returning its offset.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

struct Splitter<'a> {
    s: &'a [u8],
}

impl<'a> Splitter<'a> {
    fn err(&self, at: usize, msg: impl Into<String>) -> SplitError {
        SplitError { offset: at, line: line_of(self.s, at), message: msg.into() }
    }

    fn forms(&self) -> Result<Vec<Form>, SplitError> {
        let mut out = Vec::new();
        let mut lines = LineCursor::new();
        let mut i = 0;
        loop {
            i = self.skip_ws(i)?;
            if i >= self.s.len() {
                return Ok(out);
            }
            let j = self.datum(i)?;
            // One cursor serves both bounds: start offsets and end offsets
            // interleave monotonically (i <= j-1 < next i).
            let line_start = lines.advance(self.s, i);
            let line_end = lines.advance(self.s, j.saturating_sub(1));
            out.push(Form {
                start: i,
                end: j,
                line_start,
                line_end,
                hash: fnv1a(&normalize(&self.s[i..j])),
                names: Vec::new(), // filled by `attach_names`
            });
            i = j;
        }
    }

    /// Skip whitespace, `;` line comments and nested `#| |#` block comments.
    fn skip_ws(&self, mut i: usize) -> Result<usize, SplitError> {
        while i < self.s.len() {
            match self.s[i] {
                b' ' | b'\t' | b'\n' | b'\r' | 0x0c | 0x0b => i += 1,
                b';' => {
                    i = match self.s[i..].iter().position(|&c| c == b'\n') {
                        Some(k) => i + k + 1,
                        None => self.s.len(),
                    }
                }
                b'#' if self.at(i + 1, b'|') => {
                    let start = i;
                    let mut depth = 1usize;
                    i += 2;
                    while i < self.s.len() && depth > 0 {
                        if self.at(i, b'#') && self.at(i + 1, b'|') {
                            depth += 1;
                            i += 2;
                        } else if self.at(i, b'|') && self.at(i + 1, b'#') {
                            depth -= 1;
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    if depth > 0 {
                        return Err(self.err(start, "unterminated #| block comment"));
                    }
                }
                _ => return Ok(i),
            }
        }
        Ok(i)
    }

    fn at(&self, i: usize, c: u8) -> bool {
        i < self.s.len() && self.s[i] == c
    }

    /// The elements of the list spanning `[start, end)`, head included.
    fn elements(&self, start: usize, end: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut i = start + 1;
        loop {
            let Ok(j) = self.skip_ws(i) else { return out };
            if j >= end || self.s[j] == b')' {
                return out;
            }
            let Ok(k) = self.datum(j) else { return out };
            if k > end {
                return out;
            }
            out.push((j, k));
            i = k;
        }
    }

    fn text_at(&self, r: (usize, usize)) -> String {
        decode(&self.s[r.0..r.1])
    }

    /// Record the `include-book` calls in `[start, end)`, then recurse.
    /// See [`include_sites`].
    fn includes_in(&self, start: usize, end: usize, out: &mut Vec<IncludeSite>) {
        if !self.at(start, b'(') {
            return;
        }
        let els = self.elements(start, end);
        if let Some(head) = els.first() {
            let h = self.text_at(*head);
            let bare = h.rsplit("::").next().unwrap_or(&h).to_ascii_lowercase();
            if bare == "include-book"
                && let Some(name) = els.get(1)
                && self.at(name.0, b'"')
            {
                // The `:dir` value is the datum after the keyword, wherever
                // in the argument list it sits.
                let dir = els.iter().skip(2).position(|r| {
                    self.text_at(*r).eq_ignore_ascii_case(":dir")
                }).and_then(|k| els.get(k + 3).map(|r| self.text_at(*r)));
                out.push(IncludeSite {
                    book: self.text_at(*name).trim_matches('"').to_string(),
                    dir,
                    line: line_of(self.s, start),
                });
            }
        }
        for e in els {
            self.includes_in(e.0, e.1, out);
        }
    }

    /// The definer site opening at `at`, if this form introduces a name.
    ///
    /// Reads only the head and the datum after it -- both atoms, so both
    /// cheap. Nothing here descends: [`definer_sites`] walks into every list
    /// itself, which is what keeps the whole scan linear.
    ///
    /// A head that is itself a list -- `((lambda …) x)` -- introduces
    /// nothing, and neither does a form whose second element is a list.
    fn definer_at(&self, at: usize, lines: &mut LineCursor) -> Option<DefinerSite> {
        let h0 = self.skip_ws(at + 1).ok()?;
        if h0 >= self.s.len() || self.s[h0] == b'(' || self.s[h0] == b')' {
            return None;
        }
        let h1 = self.datum(h0).ok()?;
        let n0 = self.skip_ws(h1).ok()?;
        if n0 >= self.s.len() || self.s[n0] == b'(' || self.s[n0] == b')' {
            return None;
        }
        let n1 = self.datum(n0).ok()?;
        let head = decode(&self.s[h0..h1]);
        if !is_definer(&head) {
            return None;
        }
        let name = decode(&self.s[n0..n1]);
        // A keyword is never an introduced name, and `def…` forms do put one
        // in the name slot: `(std::defredundant :names (f g))` would index
        // itself as `:names`. Cheap to rule out and impossible to get right
        // later, since nothing downstream can tell a keyword from a symbol
        // that merely looks odd.
        if name.is_empty() || name.starts_with(':') {
            return None;
        }
        // The cursor only moves forward, and the scan visits opening parens
        // in increasing order, so counting newlines stays linear too --
        // `line_of` from scratch per site would put the quadratic cost back.
        Some(DefinerSite { head, name, offset: at, line: lines.advance(self.s, at) })
    }

    /// Return the offset just past the datum starting at or after `i`.
    fn datum(&self, i: usize) -> Result<usize, SplitError> {
        let i = self.skip_ws(i)?;
        if i >= self.s.len() {
            return Err(self.err(i, "end of file while expecting a form"));
        }
        match self.s[i] {
            b'(' => self.list(i),
            b')' => Err(self.err(i, "unbalanced ')'")),
            b'"' => self.string(i),
            b'\'' | b'`' => self.datum(i + 1),
            b',' => {
                let j = if self.at(i + 1, b'@') { i + 2 } else { i + 1 };
                self.datum(j)
            }
            b'#' => self.hash(i),
            b'|' => self.bars(i),
            _ => self.atom(i),
        }
    }

    fn list(&self, mut i: usize) -> Result<usize, SplitError> {
        let open = i;
        i += 1;
        loop {
            i = self.skip_ws(i)?;
            if i >= self.s.len() {
                return Err(self.err(open, "unterminated list"));
            }
            if self.s[i] == b')' {
                return Ok(i + 1);
            }
            i = self.datum(i)?;
        }
    }

    fn string(&self, mut i: usize) -> Result<usize, SplitError> {
        let open = i;
        i += 1;
        while i < self.s.len() {
            match self.s[i] {
                b'\\' => i += 2,
                b'"' => return Ok(i + 1),
                _ => i += 1,
            }
        }
        Err(self.err(open, "unterminated string"))
    }

    fn bars(&self, mut i: usize) -> Result<usize, SplitError> {
        let open = i;
        i += 1;
        while i < self.s.len() {
            match self.s[i] {
                b'\\' => i += 2,
                b'|' => return Ok(i + 1),
                _ => i += 1,
            }
        }
        Err(self.err(open, "unterminated |symbol|"))
    }

    /// `#`-dispatch. Four of these are ACL2-specific or otherwise easy to miss,
    /// and every one of them appears in real Kestrel books; a generic Common
    /// Lisp splitter gets them wrong.
    fn hash(&self, i: usize) -> Result<usize, SplitError> {
        let next = if i + 1 < self.s.len() { self.s[i + 1] } else { 0 };
        match next {
            // Character literal: #\a, #\(, #\Space. The character after the
            // backslash is taken literally whatever it is, then any name tail.
            b'\\' => Ok(char_literal_end(self.s, i)),
            // Reader conditional: reads the feature expression AND the guarded
            // form, and the pair is ONE unit. `#+skip (defthm ...)` is the
            // common idiom for disabling a top-level form; treating it as two
            // units desynchronises the prefix.
            b'+' | b'-' => {
                let j = self.datum(i + 2)?;
                self.datum(j)
            }
            // ACL2-specific: #!PKG datum -- read the datum in that package.
            b'!' => {
                let j = self.datum(i + 2)?;
                self.datum(j)
            }
            // ACL2-specific "fancy string": #{"""..."""}
            b'{' => match find(&self.s[i + 2..], b"\"\"\"}") {
                Some(k) => Ok(i + 2 + k + 4),
                None => Err(self.err(i, "unterminated #{\"\"\"...\"\"\"} string")),
            },
            b'\'' => self.datum(i + 2),
            b'.' => self.datum(i + 2),
            b'(' => self.list(i + 1),
            // No `b'|'` arm: `#|...|#` never reaches datum, because skip_ws
            // consumes block comments everywhere a datum may begin.
            // #b101 #x1F #2A(..) #0=(..) #0#
            _ => {
                let mut j = i + 1;
                while j < self.s.len() && self.s[j].is_ascii_digit() {
                    j += 1;
                }
                if j >= self.s.len() {
                    return Err(self.err(i, "end of file after '#'"));
                }
                let disp = self.s[j];
                j += 1;
                match disp {
                    b'#' => Ok(j),               // #0# label reference: complete
                    b'=' => self.datum(j),       // #0=datum : labelled datum
                    _ if self.at(j, b'(') => self.list(j),
                    _ => self.atom(j),
                }
            }
        }
    }

    fn atom(&self, mut i: usize) -> Result<usize, SplitError> {
        let start = i;
        while i < self.s.len() {
            match self.s[i] {
                b'\\' => i += 2,
                b'|' => i = self.bars(i)?,
                b' ' | b'\t' | b'\n' | b'\r' | 0x0c | 0x0b | b'(' | b')' | b'"' | b';' => break,
                _ => i += 1,
            }
        }
        if i == start {
            return Err(self.err(i, "empty token"));
        }
        Ok(i.min(self.s.len()))
    }
}

fn line_of(s: &[u8], offset: usize) -> usize {
    s[..offset.min(s.len())].iter().filter(|&&b| b == b'\n').count() + 1
}

/// Offset just past the `#\...` character literal starting at `i`.
///
/// Shared by the splitter and [`normalize`] so the two can never disagree
/// about where a literal ends. The character after the backslash is taken
/// literally whatever it is -- `#\;`, `#\"`, `#\|`, `#\(` -- then any
/// alphanumeric name tail (`#\Space`, `#\Tab`).
fn char_literal_end(s: &[u8], i: usize) -> usize {
    let mut j = i + 3.min(s.len() - i);
    while j < s.len() && (s[j].is_ascii_alphanumeric() || s[j] == b'-') {
        j += 1;
    }
    j
}

/// A cursor that turns byte offsets into 1-based line numbers incrementally.
///
/// `line_of` recounts newlines from offset 0 on every call, which is quadratic
/// over the forms of a file; form offsets are non-decreasing, so one pass
/// suffices. Offsets given to `advance` must not go backwards.
struct LineCursor {
    pos: usize,
    line: usize,
}

impl LineCursor {
    fn new() -> LineCursor {
        LineCursor { pos: 0, line: 1 }
    }

    /// The 1-based line of `to`, which must be `>=` every earlier argument.
    fn advance(&mut self, s: &[u8], to: usize) -> usize {
        let to = to.min(s.len());
        debug_assert!(to >= self.pos, "LineCursor must advance monotonically");
        self.line += s[self.pos..to].iter().filter(|&&b| b == b'\n').count();
        self.pos = to;
        self.line
    }
}

/// Canonical form used for hashing: comments removed, runs of whitespace
/// collapsed to one space, and space dropped next to parentheses.
///
/// Hashing normalised *text* rather than a read s-expression is forced by the
/// same fact that forces lexical splitting: the file cannot be read up front.
/// It happens also to be what we want -- reindenting a form or editing a
/// comment inside it costs no re-proving.
///
/// Comment handling is free: the splitter already has to skip both comment
/// forms to balance parentheses, so this reuses that lexer rather than
/// reimplementing it. That also means a `;` or `#|` inside a string or a
/// `|bar symbol|` is correctly *not* treated as a comment -- which is why this
/// cannot be done with a regular expression.
///
/// Whitespace collapses to a single space rather than vanishing, so tokens are
/// never merged: `(f a b)` must not normalise to the same thing as `(f ab)`.
///
/// All errors are in the safe direction. Normalising only merges inputs that
/// are certainly equivalent, so the worst case is a spurious *difference* --
/// costing re-proving that was not strictly needed. A spurious *match* would be
/// a correctness bug. Keeping that true requires copying every span whose body
/// the lexer must not reinterpret -- strings, `|bars|`, `#\` character
/// literals, `#{"""..."""}` fancy strings -- verbatim: a `;` that is the
/// *character* `#\;` must not open a comment here, or everything after it on
/// the line vanishes from the hash input and forms differing only there
/// collide. Note also that case is preserved even though ACL2's reader upcases
/// unescaped symbols, so re-casing a form does force a re-admit.
///
/// # Examples
///
/// ```
/// use acl2_session::split::normalize;
/// let n = normalize(b"(defun f (x)  ; note\n  x)");
/// assert_eq!(String::from_utf8(n).unwrap(), "(defun f (x) x)");
///
/// // A comment character inside a string is not a comment.
/// let n = normalize(b"(f \";)\" y)");
/// assert_eq!(String::from_utf8(n).unwrap(), "(f \";)\" y)");
///
/// // Nested block comments.
/// let n = normalize(b"(f #| a #| b |# c |# y)");
/// assert_eq!(String::from_utf8(n).unwrap(), "(f y)");
/// ```
pub fn normalize(form: &[u8]) -> Vec<u8> {
    let sp = Splitter { s: form };
    let b = form;
    let mut out: Vec<u8> = Vec::with_capacity(form.len());
    // Bytes at or below this index were copied verbatim and are *content*.
    // The space that is the character `#\ `, or an escaped `A\ `, must not be
    // taken for a separator -- popped before a `)` or merged with a gap --
    // and a content `(` (the literal `#\(`, an escaped `A\(`) must not
    // suppress the separator, or `A\( B` and `A\(B` would collide. Both
    // found by the corpus invariant, not by reasoning.
    let mut verbatim_end = 0usize;
    // A separator space or an opening paren, but only when structural.
    let sep_last = |out: &Vec<u8>, verbatim_end: usize| {
        out.len() > verbatim_end
            && matches!(out.last(), Some(b' ') | Some(b'('))
    };
    let mut i = 0usize;
    while i < b.len() {
        let j = match sp.skip_ws(i) {
            Ok(j) => j,
            Err(_) => b.len(),
        };
        if j > i {
            // A gap of whitespace or comments: significant only as a separator.
            if !out.is_empty() && !sep_last(&out, verbatim_end) {
                out.push(b' ');
            }
            i = j;
            continue;
        }
        match b[i] {
            // `#\` and `#{`: spans whose bodies must not be re-lexed. A
            // character literal's character may be `;`, `"` or `|`, and a
            // fancy string's body may hold unbalanced quotes; handing either
            // to the arms below drops or extends text, and two *different*
            // forms can then normalise -- and hash -- identically. `#\;` did
            // exactly that: its `;` opened a "comment" running to end of
            // line, so any two forms differing only after it collided.
            b'#' if sp.at(i + 1, b'\\') => {
                let e = char_literal_end(b, i);
                out.extend_from_slice(&form[i..e]);
                i = e;
                verbatim_end = out.len();
            }
            b'#' if sp.at(i + 1, b'{') => {
                let e = find(&b[i + 2..], b"\"\"\"}").map_or(b.len(), |k| i + 2 + k + 4);
                out.extend_from_slice(&form[i..e]);
                i = e;
                verbatim_end = out.len();
            }
            // A backslash escapes the next byte inside an atom -- `A\;B`,
            // `A\"B` -- exactly as `atom` lexes it. The escaped byte is
            // content: found in the corpus as an escaped `;` that opened a
            // phantom comment here.
            b'\\' => {
                let e = (i + 2).min(b.len());
                out.extend_from_slice(&form[i..e]);
                i = e;
                verbatim_end = out.len();
            }
            b'"' => {
                let e = sp.string(i).unwrap_or(b.len());
                out.extend_from_slice(&form[i..e]);
                i = e;
                verbatim_end = out.len();
            }
            b'|' => {
                let e = sp.bars(i).unwrap_or(b.len());
                out.extend_from_slice(&form[i..e]);
                i = e;
                verbatim_end = out.len();
            }
            b')' => {
                if out.last() == Some(&b' ') && out.len() > verbatim_end {
                    out.pop();
                }
                out.push(b')');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    while out.last() == Some(&b' ') && out.len() > verbatim_end {
        out.pop();
    }
    out
}

/// FNV-1a. Chosen over `DefaultHasher` because that one is explicitly not
/// stable across Rust releases, and a prefix cache is a thing we may want to
/// persist later.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Decode ACL2's bytes into text without destroying any of them.
///
/// ACL2's own model is Latin-1: it reads and writes bytes, and each byte is
/// one character. `books/kestrel/acl2pl/reader-tests.lisp` means it
/// literally, writing `#\<byte>` for all 256. Everything downstream of the
/// session -- a JSON reply, the editor protocol, a webview -- is Unicode
/// text, so a decode has to happen somewhere; the only question is what it
/// does with a byte that is not UTF-8.
///
/// `from_utf8_lossy` answers "replace it", which is the one answer that
/// cannot be undone: ACL2 printed `caf\xe9` and the caller received
/// `caf\u{fffd}`, with nothing left to say what had been there.
///
/// This decodes UTF-8 where the bytes are UTF-8 and Latin-1 where they are
/// not, so it is the identity on both populations that exist -- the 13406
/// `.lisp` files in a community checkout that are valid UTF-8, and the one
/// that is not. Nothing is ever replaced.
pub fn decode(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let mut out = String::with_capacity(bytes.len() + 8);
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                break;
            }
            Err(e) => {
                let good = e.valid_up_to();
                // The prefix is valid by construction; the byte that ends it
                // is a Latin-1 character, which is what ACL2 meant by it and
                // what re-encoding would have to produce to round-trip.
                out.push_str(std::str::from_utf8(&rest[..good]).expect("valid prefix"));
                out.push(rest[good] as char);
                rest = &rest[good + 1..];
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_form_carries_the_names_it_writes() {
        let f = split_str("(defthm foo (equal x x))\n").unwrap();
        assert_eq!(f[0].names, ["foo"]);
    }

    #[test]
    fn a_form_that_defines_nothing_carries_no_names() {
        let f = split_str("(include-book \"std/lists/top\" :dir :system)\n").unwrap();
        assert!(f[0].names.is_empty(), "got {:?}", f[0].names);
    }

    /// The plural case the cap exists for -- and the reason names are a label
    /// rather than a key.
    #[test]
    fn a_compound_form_carries_every_name_in_it() {
        let f = split_str(
            "(encapsulate ()\n  (defun a (x) x)\n  (defthm b (equal (a x) x)))\n",
        )
        .unwrap();
        assert_eq!(f.len(), 1, "one top-level form");
        assert_eq!(f[0].names, ["a", "b"]);
    }

    /// A `define` writes one name however many it introduces in the world.
    /// This is what makes the feared macro explosion a non-problem.
    #[test]
    fn a_definer_macro_writes_one_name() {
        let f = split_str("(define seg-fix ((x seg-p)) :returns (y seg-p) x)\n").unwrap();
        assert_eq!(f[0].names, ["seg-fix"]);
    }

    /// Names must land in the form that contains them, not the one before:
    /// a comment between two forms is not part of either.
    #[test]
    fn names_land_in_the_right_form() {
        let f = split_str(
            "(defun a (x) x)\n\n; a comment mentioning defthm nothing\n\n(defthm b (equal (a x) x))\n",
        )
        .unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].names, ["a"]);
        assert_eq!(f[1].names, ["b"]);
    }

    fn counts(s: &str) -> usize {
        split_str(s).expect("split failed").len()
    }

    #[test]
    fn basics() {
        assert_eq!(counts("(a) (b)"), 2);
        assert_eq!(counts("  \n (a)\n\n(b) ; trailing\n"), 2);
        assert_eq!(counts("; only a comment\n"), 0);
        assert_eq!(counts(""), 0);
    }

    #[test]
    fn comments_and_strings_do_not_break_nesting() {
        assert_eq!(counts("(a \";)\" b)"), 1);
        assert_eq!(counts("(a #| ) |# b)"), 1);
        assert_eq!(counts("(a #| nested #| |# |# b)"), 1);
        assert_eq!(counts("(a ; )\n b)"), 1);
        assert_eq!(counts(r#"(a "\"" b)"#), 1);
    }

    #[test]
    fn character_literals() {
        assert_eq!(counts(r"(a #\( b)"), 1);
        assert_eq!(counts(r"(a #\) b)"), 1);
        assert_eq!(counts(r"(a #\; b)"), 1);
        assert_eq!(counts(r"(a #\Space b)"), 1);
    }

    /// `#+skip (defthm ...)` is ONE unit. Emacs `scan-sexps` reports two,
    /// which is the specific way a frontier desynchronises on real books.
    #[test]
    fn reader_conditional_is_one_unit() {
        assert_eq!(counts("#+skip (defthm foo t)"), 1);
        assert_eq!(counts("#-acl2data (assert-event t)"), 1);
        assert_eq!(counts("#-(or a b) (defthm foo t)"), 1);
        assert_eq!(counts("(a) #+skip (b) (c)"), 3);
    }

    #[test]
    fn acl2_specific_reader_macros() {
        assert_eq!(counts("#!ACL2 (defthm foo t)"), 1);
        assert_eq!(counts("#!AC x"), 1);
        assert_eq!(counts("(a #{\"\"\"KEYWORD\"\"\"} b)"), 1);
        assert_eq!(counts("(equal '(#0=(a b) #0#) x)"), 1);
    }

    #[test]
    fn positions_are_reported() {
        let src = "(alpha)\n\n(beta\n  gamma)\n";
        let f = split_str(src).unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!((f[0].line_start, f[0].line_end), (1, 1));
        assert_eq!((f[1].line_start, f[1].line_end), (3, 4));
        assert_eq!(&src[f[1].start..f[1].end], "(beta\n  gamma)");
    }

    #[test]
    fn hash_ignores_layout_and_comments() {
        let a = split_str("(defthm foo (equal x y))").unwrap()[0].hash;
        let b = split_str("(defthm   foo\n   ; a comment\n   (equal x y))").unwrap()[0].hash;
        let c = split_str("(defthm foo (equal x z))").unwrap()[0].hash;
        assert_eq!(a, b, "reformatting must not change the hash");
        assert_ne!(a, c, "a real change must change the hash");
    }

    #[test]
    fn hash_does_not_merge_tokens() {
        let a = split_str("(f ab)").unwrap()[0].hash;
        let b = split_str("(f a b)").unwrap()[0].hash;
        assert_ne!(a, b, "whitespace between tokens is significant");
    }

    /// Two forms differing only *after* a `#\;` on the same line must not hash
    /// alike. Without the `#` arm in `normalize`, the `;` opened a line
    /// comment and both normalised to `"(member c (list #\"`.
    #[test]
    fn char_literal_semicolon_does_not_truncate_the_hash() {
        let a = split_str(r"(member c (list #\; #\, #\.))").unwrap()[0].hash;
        let b = split_str(r"(member c (list #\; #\! #\?))").unwrap()[0].hash;
        assert_ne!(a, b, "text after #\\; must stay in the hash input");
    }

    /// An escaped `(` is content, so the separating space after it is
    /// significant: `A\( B` is two atoms where `A\(B` is one.
    #[test]
    fn a_space_after_escaped_content_is_not_dropped() {
        let a = split_str(r"(f A\( B)").unwrap()[0].hash;
        let b = split_str(r"(f A\(B)").unwrap()[0].hash;
        assert_ne!(a, b, "two atoms must not normalise like one");
    }

    /// `split(normalize(f))` must yield exactly one form, and normalising
    /// again must change nothing. The corpus test checks this at scale; these
    /// pin the reader macros whose bodies invite re-lexing.
    #[test]
    fn normalize_survives_reader_macros() {
        for src in [
            r"(member c (list #\; #\, #\.))",
            r#"(a #\" b)"#,
            r"(a #\| b)",
            r"(a #\( #\) #\Space b)",
            r#"(a #{"""x ; y"""} b)"#,
            "(defun f (x)  ; note\n  x)",
            // Atom-level escapes, from the corpus (acl2pl/reader-tests.lisp).
            r#"(assert-event (equal (symbol-name 'A\;B) "A;B"))"#,
            r#"(equal (symbol-name 'A\"B) x)"#,
            r"(equal (symbol-name 'A\|B) x)",
            // Content that *ends* in a space or `(`: the space of `#\ ` and
            // an escaped `A\ ` are data, not separators (infix/latex-init).
            r"(list #\ )",
            r"(equal (symbol-name 'A\ B) x)",
            r"(f A\( B)",
        ] {
            let n = normalize(src.as_bytes());
            let forms = split(&n)
                .unwrap_or_else(|e| panic!("normalize broke {src:?}: {e}"));
            assert_eq!(forms.len(), 1, "normalize of {src:?} must stay one form");
            assert_eq!(normalize(&n), n, "normalize must be a fixpoint on {src:?}");
        }
    }

    #[test]
    fn errors_are_loud_and_located() {
        let e = split_str("(a\n(b").unwrap_err();
        assert!(e.message.contains("unterminated"));
        let e = split_str("(a \"oops)").unwrap_err();
        assert!(e.message.contains("string"));
        assert!(split_str(")").is_err());
    }

    fn sites(src: &str) -> Vec<(String, usize)> {
        super::definer_sites(src.as_bytes())
            .into_iter()
            .map(|d| (d.name, d.line))
            .collect()
    }

    /// Most of a real book's definitions are *inside* something, so a scan of
    /// top-level heads would find almost none of them.
    #[test]
    fn definers_are_found_at_every_depth() {
        let src = "(defun alpha (x) x)\n\
                   (mutual-recursion\n\
                     (defun beta (x) x)\n\
                     (defun gamma (x) x))\n\
                   (encapsulate ()\n\
                     (local (defthm delta (equal 1 1))))\n\
                   (std::define epsilon (x) x)\n";
        assert_eq!(
            sites(src),
            vec![
                ("alpha".into(), 1),
                ("beta".into(), 3),
                ("gamma".into(), 4),
                ("delta".into(), 6),
                ("epsilon".into(), 7),
            ]
        );
    }

    /// A definer name inside a string, a comment or a character literal is
    /// not a definition. This is exactly why the scan reuses the reader
    /// rather than matching lines.
    #[test]
    fn definers_respect_strings_comments_and_char_literals() {
        let src = "; (defun commented (x) x)\n\
                   (defconst *doc* \"(defun instring (x) x)\")\n\
                   #|\n(defun blocked (x) x)\n|#\n\
                   (defun real-one (c) (equal c #\\())\n";
        let found: Vec<String> = sites(src).into_iter().map(|(n, _)| n).collect();
        assert_eq!(found, vec!["*doc*".to_string(), "real-one".to_string()],
                   "only the two genuine definers, got {found:?}");
    }

    /// Includes are routinely wrapped in `local`, and a commented-out or
    /// stringified one is not an include at all.
    #[test]
    fn includes_are_found_wherever_they_sit() {
        let src = "(include-book \"std/util/bstar\" :dir :system)\n\
                   (local (include-book \"arithmetic/top\"))\n\
                   ; (include-book \"commented/out\")\n\
                   (defconst *s* \"(include-book \\\"in/a/string\\\")\")\n\
                   (encapsulate ()\n\
                     (local (include-book \"deep/one\" :dir :teachpacks)))\n";
        let got: Vec<(String, Option<String>, usize)> = super::include_sites(src.as_bytes())
            .into_iter()
            .map(|i| (i.book, i.dir, i.line))
            .collect();
        assert_eq!(
            got,
            vec![
                ("std/util/bstar".into(), Some(":system".into()), 1),
                ("arithmetic/top".into(), None, 2),
                ("deep/one".into(), Some(":teachpacks".into()), 6),
            ]
        );
    }

    #[test]
    fn a_forms_head_is_its_name_when_it_defines_nothing() {
        assert_eq!(super::head_of(b"(mutual-recursion (defun f (x) x))").as_deref(),
                   Some("mutual-recursion"));
        assert_eq!(super::head_of(b"  ; a comment\n(encapsulate () x)").as_deref(),
                   Some("encapsulate"));
        assert_eq!(super::head_of(b"(std::define f (x) x)").as_deref(), Some("std::define"));
        assert_eq!(super::head_of(b"((lambda (x) x) 1)"), None, "a list head names nothing");
        assert_eq!(super::head_of(b"bare-atom"), None);
        assert_eq!(super::head_of(b"()"), None);
    }

    /// The head may be a list, and the name may be absent entirely; neither
    /// should be mistaken for a definition or panic the walk.
    #[test]
    fn odd_forms_introduce_nothing() {
        assert!(sites("((lambda (x) x) 1)").is_empty());
        assert!(sites("(defun)").is_empty(), "a definer with no name");
        assert!(sites("(defun (x) x)").is_empty(), "nor a list where a name goes");
        assert!(sites("(not-a-definer foo)").is_empty());
        // An unterminated form still names what it was going to introduce.
        // The scan reads only the head and the name, so it never has to
        // reach the missing paren -- which is the behaviour worth having,
        // because a half-typed form is exactly when its name is wanted. (The
        // earlier recursive walk lexed the whole form and so lost it; `split`
        // remains the strict reader that refuses such text outright.)
        assert_eq!(sites("(defun good (x) x)\n(defun bad (x"),
                   vec![("good".to_string(), 1), ("bad".to_string(), 2)]);
    }

    /// `decode` must never destroy a byte, and must not mojibake the UTF-8
    /// that is what almost every book actually contains. Both populations
    /// are the identity; a Latin-1 byte survives as the character ACL2 meant.
    #[test]
    fn decoding_acl2s_bytes_loses_nothing() {
        // The overwhelming case: ASCII and real UTF-8, unchanged.
        assert_eq!(super::decode(b"(defun f (x) x)"), "(defun f (x) x)");
        assert_eq!(super::decode("caf\u{e9} \u{2200}x".as_bytes()), "caf\u{e9} \u{2200}x");

        // The case that motivated it: ACL2 printed one Latin-1 byte, which is
        // one character to ACL2 and must be one character here.
        assert_eq!(super::decode(b"caf\xe9"), "caf\u{e9}");
        assert!(!super::decode(b"caf\xe9").contains('\u{fffd}'),
                "no byte is ever replaced");

        // All 256, which is literally what reader-tests.lisp writes.
        let all: Vec<u8> = (0..=255u8).collect();
        let d = super::decode(&all);
        assert_eq!(d.chars().count(), 256, "one character per byte: {d:?}");
        assert_eq!(d.chars().map(|c| c as u32).collect::<Vec<_>>(),
                   (0..=255u32).collect::<Vec<_>>());

        // Mixed: valid UTF-8 either side of a byte that is not. The UTF-8
        // stays UTF-8 -- decoding the whole capture as Latin-1 would have
        // turned the first word into mojibake.
        let mixed = [b"caf\xc3\xa9 ".as_slice(), b"\xe9 end".as_slice()].concat();
        assert_eq!(super::decode(&mixed), "caf\u{e9} \u{e9} end");

        // A truncated sequence at the end -- a tail sliced mid-character --
        // is bytes, not a panic and not a replacement.
        assert_eq!(super::decode(b"a\xc3"), "a\u{c3}");
    }

    /// Community books carry raw bytes 0x80-0xFF (see the module docs), which
    /// `from_utf8_lossy` turns into three-byte replacement characters. A head
    /// whose first three bytes straddle one must not panic the walk.
    #[test]
    fn a_non_ascii_head_does_not_panic_the_walk() {
        // One raw high byte after `a`: lossy-decoded it becomes a three-byte
        // replacement character, so byte 3 falls inside it. Only `definer_sites`
        // can carry this -- the `sites` helper takes a `str`.
        let found = super::definer_sites(b"(a\xe9bc foo)\n(defun good (x) x)");
        assert_eq!(found.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["good"]);

        // The same boundary, reached through characters that are genuinely
        // multi-byte rather than through lossy decoding.
        assert_eq!(sites("(ééx foo)\n(defun good (x) x)"),
                   vec![("good".to_string(), 2)]);

        // Shorter than the three bytes `def` needs, but not shorter than three
        // bytes -- the length guard alone would have let this through.
        assert!(sites("(é foo)").is_empty());
        assert!(sites("(éé foo)").is_empty());
    }
}
