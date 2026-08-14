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
    Splitter { s: text }.forms()
}

/// Convenience wrapper for callers that already hold a `&str`.
pub fn split_str(text: &str) -> Result<Vec<Form>, SplitError> {
    split(text.as_bytes())
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
        let mut i = 0;
        loop {
            i = self.skip_ws(i)?;
            if i >= self.s.len() {
                return Ok(out);
            }
            let j = self.datum(i)?;
            out.push(Form {
                start: i,
                end: j,
                line_start: line_of(self.s, i),
                line_end: line_of(self.s, j.saturating_sub(1)),
                hash: fnv1a(&normalize(&self.s[i..j])),
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
            b'\\' => {
                let mut j = i + 3.min(self.s.len() - i);
                while j < self.s.len() && (self.s[j].is_ascii_alphanumeric() || self.s[j] == b'-') {
                    j += 1;
                }
                Ok(j)
            }
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
            b'|' => self.skip_ws(i),
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

/// Canonical form used for hashing: comments removed, runs of whitespace
/// collapsed to one space, and space dropped next to parentheses.
///
/// Hashing normalised *text* rather than a read s-expression is forced by the
/// same fact that forces lexical splitting: the file cannot be read up front.
/// It happens also to be what we want -- reindenting a form or editing a
/// comment inside it costs no re-proving.
pub fn normalize(form: &[u8]) -> Vec<u8> {
    let sp = Splitter { s: form };
    let b = form;
    let mut out: Vec<u8> = Vec::with_capacity(form.len());
    let mut i = 0usize;
    while i < b.len() {
        let j = match sp.skip_ws(i) {
            Ok(j) => j,
            Err(_) => b.len(),
        };
        if j > i {
            // A gap of whitespace or comments: significant only as a separator.
            if !matches!(out.last(), None | Some(b' ') | Some(b'(')) {
                out.push(b' ');
            }
            i = j;
            continue;
        }
        match b[i] {
            b'"' => {
                let e = sp.string(i).unwrap_or(b.len());
                out.extend_from_slice(&form[i..e]);
                i = e;
            }
            b'|' => {
                let e = sp.bars(i).unwrap_or(b.len());
                out.extend_from_slice(&form[i..e]);
                i = e;
            }
            b')' => {
                if out.last() == Some(&b' ') {
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
    while out.last() == Some(&b' ') {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn errors_are_loud_and_located() {
        let e = split_str("(a\n(b").unwrap_err();
        assert!(e.message.contains("unterminated"));
        let e = split_str("(a \"oops)").unwrap_err();
        assert!(e.message.contains("string"));
        assert!(split_str(")").is_err());
    }
}
