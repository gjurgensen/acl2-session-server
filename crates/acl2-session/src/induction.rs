//! What ACL2 said about an induction, recovered from the log.
//!
//! This is the one extraction in the crate that reads ACL2's *prose*, and it
//! is here only because there is nowhere else to read it from. The scheme is
//! computed inside `induct-msg/continue` -- a printing function -- and
//! dropped; `last-event-data` records `abort-causes`, `event`, `form`,
//! `hint-events`, `namex`, `prover-steps-counted`, `rules`, `splitter-rules`,
//! `system-attachments`, `time` and `warnings`, and nothing about induction.
//! So a probe cannot answer this and a parse must.
//!
//! Everything here is therefore **best effort, and total**: every field is
//! optional, nothing is inferred from a partial match, and a
//! reworded ACL2 yields an empty result rather than a wrong one. Callers
//! should treat what comes back as a different class of fact from a probe's
//! -- a projection over text that a future ACL2 may print differently.
//!
//! Two properties of the source make it more tractable than log-scraping
//! usually is:
//!
//! - The whole announcement is a single `fms` call, so the wording is one
//!   format string rather than many print sites.
//! - The scheme is printed with the gag-mode evisc tuple, which is `nil`
//!   whenever the global is `:default` -- so under this crate's settings the
//!   term is printed *whole*, never abbreviated with `...`.
//!
//! And one that makes it harder: `fms` wraps at the right margin, so no
//! anchor survives as contiguous text. `we'll use is` really does arrive
//! split across a newline. Anchors are matched as word sequences with
//! arbitrary whitespace between them, and terms are delimited by counting
//! parens rather than by looking for a closing pattern.
//!
//! # Not yet sufficient: an announcement cannot be attributed to its event
//!
//! **This module is incomplete in a way that makes its output unsafe to
//! present as a fact about a form, and that is why no surface exposes it.**
//!
//! A log is one *form*. A form can be many *events* -- a `progn`, an
//! `encapsulate`, a `mutual-recursion`, a `defines`, anything a macro
//! expands into several -- and each event runs its own proof and prints its
//! own announcements. [`parse`] returns them flat, in log order, with
//! nothing saying which event any of them came from. It cannot: the
//! announcement's text never names the event.
//!
//! Nothing else in the reply closes the gap either. Measured, on one form of
//! three theorems that each induct:
//!
//! - every announcement came back with `goal` = `*1`. The pool counter
//!   restarts per event, so the one field that looks like an identifier
//!   collides across every event in the form.
//! - the count does not line up with the events. One event can induct
//!   several times (`*1`, then `*1.1` beneath it) and most events do not
//!   induct at all, so the nth announcement is not the nth event's.
//! - the orders are opposite. The announcements arrive in the order ACL2
//!   printed them, which is source order; `SyncOutcome`'s inner events
//!   arrive innermost-first, which for that form was the reverse. A caller
//!   zipping the two lists -- the obvious thing to do, and the only thing
//!   available -- attached every scheme to the wrong theorem.
//!
//! So the honest reading of a result from here is "these announcements
//! appear somewhere in this form's log", and a reader who wants to know
//! which theorem inducted how is better served by [`crate::output`] on the
//! same handle, where the announcement still sits next to the event whose
//! proof printed it.
//!
//! Fixing it means giving each announcement its event, which means tracking
//! the event boundaries in the log -- ACL2 does print them, so this is
//! work rather than a dead end. Until then the parse stands as a building
//! block and not as an answer.

use std::io;

use crate::output::{OutputRef, OutputStore, Slice};

/// Read a captured log and recover its inductions.
///
/// Deliberately *not* a `Book` method and not part of `sync`. Nothing here
/// touches the session, so it needs no book lock -- it reads a file the
/// `OutputStore` already holds, exactly as `output` does. That is also why
/// this is on demand rather than folded into a unit's envelope: most units
/// never induct, most logs are never asked about, and a linear scan of every
/// capture would be paid on the sync path where it is least affordable.
///
/// Callers with an envelope in hand can skip the call entirely when its
/// `rules` carry no `:induction` rune -- ACL2 records that structurally, so
/// the cheap signal gates the expensive one.
pub fn from_output(outs: &OutputStore, r: &OutputRef) -> io::Result<Vec<Induction>> {
    Ok(parse(&outs.get(r, Slice::All)?.text))
}

/// One induction announcement: ACL2 chose a scheme, and said why.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Induction {
    /// The pool name the announcement is about -- `*1`, `*1.1`. From
    /// "If we let (:p ...) denote *1 above".
    pub goal: Option<String>,
    /// The induction term from the conjecture, untranslated: `(append x y)`.
    /// ACL2's `~xg`, the winning candidate's `:xinduction-term`.
    pub suggested_by: Option<String>,
    /// How ACL2 attributed the suggestion, in its own words: "the
    /// :induction rule binary-append".
    ///
    /// The one field here that is independent of the other two. `~*x` is a
    /// `tilde-*-simp-phrase` over the winning candidate's ttree -- which
    /// *rule* suggested the scheme, where the other two describe the *term*
    /// it was suggested for. They coincide when a function suggests its own
    /// induction, and diverge as soon as a user-written `:induction` rule
    /// suggests a scheme for a term headed by something else.
    ///
    /// A phrase and not a list of runes, because that is what ACL2 prints:
    /// `tilde-*-simp-phrase` renders a ttree as English. Consumers wanting
    /// the runes themselves should read the envelope's `rules`, where they
    /// arrive structurally from `get-event-data` and need no parsing at all.
    pub attribution: Option<String>,
    /// The scheme, with `:p` standing for the conjecture, laid out as ACL2
    /// printed it -- newlines and indentation included, because that is the
    /// structure of a big scheme made visible.
    pub scheme: Option<String>,
    /// The function whose admission justifies the induction: `binary-append`.
    ///
    /// Very nearly redundant with [`Self::suggested_by`], and worth knowing
    /// why it is not. ACL2's `~xj` is `(ffn-symb :xinduction-term)` -- the
    /// *head symbol of that same term* -- so the two differ only in being
    /// whole-versus-head and untranslated-versus-internal: `(append x y)`
    /// against `binary-append`.
    ///
    /// What it adds is its own absence. The `~#J~` branch prints this only
    /// when a termination argument licenses the induction; for `do$`
    /// induction ACL2 instead says the scheme's own measure conjectures
    /// justify it, and this is `None`. So the field answers "was this
    /// licensed by an admission, or by proof obligations?".
    pub justified_by: Option<String>,
    /// ACL2's own phrasing of how many subgoals it produced -- "two
    /// nontautological subgoals". Kept as text: the branch prints a spelled
    /// number, and re-deriving an integer from prose buys nothing.
    pub subgoals: Option<String>,
}

impl Induction {
    /// Whether anything at all was recovered. A record where every field is
    /// empty means the anchor matched and nothing after it did, which is a
    /// reworded ACL2 rather than an induction.
    pub fn is_empty(&self) -> bool {
        self.goal.is_none()
            && self.suggested_by.is_none()
            && self.attribution.is_none()
            && self.scheme.is_none()
            && self.justified_by.is_none()
            && self.subgoals.is_none()
    }
}

/// Every induction announcement in a log, in the order ACL2 printed them.
///
/// A proof can induct more than once -- `*1`, then `*1.1` beneath it -- and
/// each announcement is its own record.
pub fn parse(log: &str) -> Vec<Induction> {
    // "We will induct according to..." opens every announcement, in all three
    // of the `~#H~[...~]` branches.
    const OPEN: [&str; 4] = ["We", "will", "induct", "according"];

    // Collect where each one *starts*, then cut the log between them, so a
    // field missing from one record is never filled in from the next.
    let mut starts = Vec::new();
    let mut from = 0;
    while let Some((start, end)) = find_words(&log[from..], &OPEN) {
        starts.push(from + start);
        from += end;
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| {
            let end = starts.get(n + 1).copied().unwrap_or(log.len());
            parse_one(&log[start..end])
        })
        .filter(|i| !i.is_empty())
        .collect()
}

/// Just past the anchor -- what every field extractor wants.
fn after_words(hay: &str, words: &[&str]) -> Option<usize> {
    find_words(hay, words).map(|(_, end)| end)
}

fn parse_one(block: &str) -> Induction {
    let mut ind = Induction::default();

    // "...suggested by (append x y)." -- or, in the other branch,
    // "...suggested by (append x y), while accommodating ...". Reading a
    // balanced form stops correctly at either.
    if let Some(i) = after_words(block, &["suggested", "by"]) {
        // Collapsed: this one is printed mid-sentence and is rendered inline,
        // so its wrapping is the margin's doing and carries nothing.
        ind.suggested_by = balanced_after(&block[i..]).map(collapse);
    }
    // "This suggestion was produced using the :induction rule binary-append."
    if let Some(i) = after_words(block, &["produced", "using"]) {
        ind.attribution = sentence_after(&block[i..]);
    }
    // "If we let (:p x y) denote *1 above then ..." -- the pool name is the
    // one token after "denote".
    if let Some(i) = after_words(block, &["denote"]) {
        ind.goal = word_after(&block[i..]).map(str::to_string);
    }
    // "...the induction scheme we'll use is\n(and ...)." The words arrive
    // split across the margin, which is why this is a word sequence.
    if let Some(i) = after_words(block, &["induction", "scheme", "we'll", "use", "is"]) {
        // Kept as printed: ACL2 starts this at column zero on its own line
        // and indents the continuations to show the structure.
        ind.scheme = balanced_after(&block[i..]).map(str::to_string);
    }
    // "This induction is justified by the same argument used to admit foo."
    if let Some(i) = after_words(block, &["used", "to", "admit"]) {
        ind.justified_by = word_after(&block[i..]).map(|w| w.trim_end_matches('.').to_string());
    }
    // "...the above induction scheme produces two nontautological subgoals."
    if let Some(i) = after_words(block, &["induction", "scheme", "produces"]) {
        ind.subgoals = sentence_after(&block[i..]);
    }
    ind
}

/// Where a sequence of words separated by any run of whitespace begins and
/// ends, as `(start, past-the-end)`.
///
/// Both halves are needed and they are not interchangeable: the extractors
/// read from the end of their anchor, while cutting the log into one record
/// per announcement needs the start. Returning only one of them is how the
/// first draft glued two announcements into one.
///
/// `fms` wraps at the right margin without asking, so every anchor in this
/// file can arrive with a newline inside it; matching on a contiguous literal
/// finds nothing on exactly the logs that matter.
fn find_words(hay: &str, words: &[&str]) -> Option<(usize, usize)> {
    let bytes = hay.as_bytes();
    let mut from = 0;
    loop {
        let first = hay.get(from..)?.find(words[0])? + from;
        // The first word must start at a word boundary, or "is" matches
        // inside "this".
        let boundary = first == 0 || !is_word_byte(bytes[first - 1]);
        if boundary && let Some(end) = match_words_at(hay, first, words) {
            return Some((first, end));
        }
        from = first + 1;
    }
}

fn match_words_at(hay: &str, at: usize, words: &[&str]) -> Option<usize> {
    let bytes = hay.as_bytes();
    let mut i = at;
    for (n, w) in words.iter().enumerate() {
        if n > 0 {
            let skipped = skip_space(bytes, i);
            if skipped == i {
                return None; // words must be separated
            }
            i = skipped;
        }
        if !hay.get(i..)?.starts_with(w) {
            return None;
        }
        i += w.len();
    }
    // The last word must end at a boundary too.
    if i < bytes.len() && is_word_byte(bytes[i]) {
        return None;
    }
    Some(i)
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'\''
}

fn skip_space(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// The next balanced parenthesised form, exactly as ACL2 printed it.
///
/// Delimited by counting rather than by pattern, which is what makes the
/// wrapping irrelevant to *finding* the end: a term broken over five lines
/// is read the same as one on a single line. Strings, `#\(` character
/// literals and `|bar quoted|` symbols all hide parens that must not be
/// counted.
///
/// The line breaks are kept. An earlier version collapsed them, on the
/// theory that the margin is a display choice -- but ACL2's printer indents
/// a term to show its structure, and that indentation is the best formatting
/// of a big scheme anyone has. Collapsing threw it away and left the client
/// to re-wrap at whatever width it had, mid-argument. Callers that render
/// inline collapse it themselves.
fn balanced_after(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = skip_space(bytes, 0);
    if start >= bytes.len() || bytes[start] != b'(' {
        return None;
    }
    let mut depth = 0usize;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'|' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'|' {
                    i += 1;
                }
            }
            b'#' if bytes.get(i + 1) == Some(&b'\\') => i += 2, // #\( is a char
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
        i += 1;
    }
    // Ran out with parens open: a truncated log, not a term. Say nothing.
    None
}

/// The next whitespace-delimited token.
fn word_after(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = skip_space(bytes, 0);
    let rest = s.get(start..)?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

/// The rest of the sentence, to the period, with its wrapping collapsed.
fn sentence_after(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let start = skip_space(bytes, 0);
    let rest = s.get(start..)?;
    let end = rest.find('.').unwrap_or(rest.len());
    let text = collapse(&rest[..end]);
    (!text.is_empty()).then_some(text)
}

/// Undo `fms`'s line breaking: the margin is a display choice, not part of
/// what ACL2 said.
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a real session: `(thm (equal (append x y) (append y x)))`
    /// under this crate's own settings. Note `we'll` / `use is` split across
    /// the margin, and the scheme wrapped over four lines -- both are why the
    /// anchors are word sequences and the terms are read by counting.
    const REAL: &str = "\
*1 (the initial Goal, a key checkpoint) is pushed for proof by induction.

Perhaps we can prove *1 by induction.  Two induction schemes are suggested
by this conjecture.  We will choose arbitrarily among these.

We will induct according to a scheme suggested by (append x y).

This suggestion was produced using the :induction rule binary-append.
If we let (:p x y) denote *1 above then the induction scheme we'll
use is
(and (implies (and (not (endp x)) (:p (cdr x) y))
              (:p x y))
     (implies (endp x) (:p x y))).
This induction is justified by the same argument used to admit binary-append.
When applied to the goal at hand the above induction scheme produces
two nontautological subgoals.
Subgoal *1/2
";

    #[test]
    fn reads_a_real_announcement() {
        let v = parse(REAL);
        assert_eq!(v.len(), 1);
        let i = &v[0];
        assert_eq!(i.suggested_by.as_deref(), Some("(append x y)"));
        assert_eq!(i.goal.as_deref(), Some("*1"));
        assert_eq!(i.justified_by.as_deref(), Some("binary-append"));
        // A phrase, because that is what `tilde-*-simp-phrase` prints. An
        // earlier draft read this as a list of runes and, finding none before
        // the sentence ended, ran on and captured the *scheme* as a rune.
        assert_eq!(
            i.attribution.as_deref(),
            Some("the :induction rule binary-append")
        );
        assert_eq!(i.subgoals.as_deref(), Some("two nontautological subgoals"));
    }

    /// The point of the whole exercise: the scheme comes back whole, and laid
    /// out as ACL2 laid it out.
    ///
    /// Wrapping is irrelevant to *finding* the term -- that is done by
    /// counting parens -- but it is not noise in the term itself. ACL2's
    /// printer indents to show structure, and a big scheme is unreadable
    /// without it. An earlier version collapsed this to one line and the
    /// client re-wrapped it mid-argument.
    #[test]
    fn the_scheme_keeps_acl2s_layout() {
        let i = &parse(REAL)[0];
        assert_eq!(
            i.scheme.as_deref(),
            Some(
                "(and (implies (and (not (endp x)) (:p (cdr x) y))\n\
                 \x20             (:p x y))\n\
                 \x20    (implies (endp x) (:p x y)))"
            )
        );
    }

    /// The inline fields do collapse, because they are printed mid-sentence
    /// and rendered inline: there their wrapping really is the margin's.
    #[test]
    fn the_inline_terms_are_collapsed() {
        let i = &parse(REAL)[0];
        assert_eq!(i.suggested_by.as_deref(), Some("(append x y)"));
        assert!(!i.suggested_by.as_deref().unwrap().contains('\n'));
    }

    #[test]
    fn a_log_without_an_induction_yields_nothing() {
        assert!(parse("Q.E.D.\n\nSummary\nForm:  ( defthm foo ...)\n").is_empty());
    }

    /// A log cut mid-term must not produce half a scheme.
    #[test]
    fn a_truncated_scheme_is_no_scheme() {
        let cut = &REAL[..REAL.find("(:p x y))\n").unwrap() + 4];
        let v = parse(cut);
        assert_eq!(v.len(), 1, "the announcement is still recognised");
        assert!(v[0].scheme.is_none(), "but the unbalanced term is refused");
        assert_eq!(v[0].suggested_by.as_deref(), Some("(append x y)"));
    }

    /// Two inductions in one proof are two records, and a field missing from
    /// the first is not filled in from the second.
    #[test]
    fn nested_inductions_do_not_bleed() {
        let two = format!(
            "{REAL}\nWe will induct according to a scheme suggested by (len z).\n\
             If we let (:p z) denote *1.1 above then the induction scheme we'll use is\n\
             (implies (endp z) (:p z)).\n"
        );
        let v = parse(&two);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].goal.as_deref(), Some("*1"));
        assert_eq!(v[1].goal.as_deref(), Some("*1.1"));
        assert_eq!(v[1].suggested_by.as_deref(), Some("(len z)"));
        // The second announcement says nothing about justification; it must
        // not inherit the first's.
        assert!(v[1].justified_by.is_none());
    }

    /// The defect the module doc opens with, in the shape it was measured in.
    ///
    /// One `progn` of three theorems that each induct, captured from a real
    /// session. Three announcements come out of one form's log and *every
    /// one* of them is `*1`: the pool counter restarts per event, so the only
    /// field that looks like an identifier identifies nothing. Nothing here
    /// says which theorem is which, and the log itself is where that is still
    /// legible.
    ///
    /// A test of what is wrong rather than of what is right, so that a later
    /// fix has to come and delete it.
    #[test]
    fn announcements_from_one_form_cannot_be_told_apart() {
        const THREE: &str = "\
We will induct according to a scheme suggested by (append a b).
This suggestion was produced using the :induction rule binary-append.
If we let (:p a b c) denote *1 above then the induction scheme we'll
use is
(and (implies (endp a) (:p a b c))).
This induction is justified by the same argument used to admit
binary-append.

We will induct according to a scheme suggested by (len a).
This suggestion was produced using the :induction rules binary-append
and len.
If we let (:p a b) denote *1 above then the induction scheme we'll
use is
(and (implies (not (consp a)) (:p a b))).
This induction is justified by the same argument used to admit len.

We will induct according to a scheme suggested by (revappend a nil).
This suggestion was produced using the :induction rules revappend and
true-listp.
If we let (:p a) denote *1 above then the induction scheme we'll
use is
(and (implies (endp a) (:p a))).
This induction is justified by the same argument used to admit
revappend.
";
        let v = parse(THREE);
        assert_eq!(v.len(), 3, "three events, three announcements, one log");
        assert!(
            v.iter().all(|i| i.goal.as_deref() == Some("*1")),
            "every one is *1: the pool counter restarts per event, so `goal` \
             cannot tell them apart -- {:?}",
            v.iter().map(|i| i.goal.as_deref()).collect::<Vec<_>>()
        );
        // And the only remaining signal is position, which is log order. The
        // envelope's inner events arrive innermost-first, so zipping the two
        // -- the only association available to a caller -- reverses them.
        assert_eq!(v[0].justified_by.as_deref(), Some("binary-append"));
        assert_eq!(v[2].suggested_by.as_deref(), Some("(revappend a nil)"));
    }

    /// Anchors are word sequences, so they must still respect word
    /// boundaries -- `is` inside `this` is not the anchor.
    #[test]
    fn anchors_respect_word_boundaries() {
        assert!(find_words("this is", &["is"]).is_some());
        assert_eq!(find_words("thistle", &["is"]), None);
        assert_eq!(find_words("induct according", &["induct", "accord"]), None);
    }

    /// Parens hidden inside strings, char literals and bar symbols are not
    /// delimiters.
    #[test]
    fn terms_ignore_parens_that_are_not_parens() {
        assert_eq!(balanced_after(" (f \"a)b\" x)"), Some("(f \"a)b\" x)"));
        assert_eq!(balanced_after(" (f #\\( x)"), Some("(f #\\( x)"));
        assert_eq!(balanced_after(" (f |a)b| x)"), Some("(f |a)b| x)"));
    }
}
