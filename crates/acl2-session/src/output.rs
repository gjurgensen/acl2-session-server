//! Captured output, addressed by handle rather than carried inline.
//!
//! The structured envelope is a fast path over a complete log, never a
//! replacement for it. A whole category of proof-debugging information exists
//! *only* in ACL2's printed narrative -- the induction scheme it chose, why it
//! chose that one, per-goal rule attribution, the output of arbitrary macros --
//! and none of it is recoverable from `get-event-data` or `gag-state`.
//!
//! So every submission's output is retained in full and handed back as a
//! handle. Nothing is ever sampled, truncated at capture, or reconstructed.
//! A client that wants the narrative asks for it.

#![forbid(unsafe_code)]
// This module is pure logic; all FFI lives in pty.rs and session.rs.
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::PathBuf;

/// An opaque handle to one submission's output.
///
/// Not a path, not a line range, and not meaningful to a client beyond being
/// handed back. Clients never construct one: every ref is produced by the call
/// that created the output, which keeps them unforgeable and makes lifetime
/// unambiguous. Valid until the owning session is dropped.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OutputRef(String);

impl fmt::Display for OutputRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl OutputRef {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for OutputRef {
    /// Rebuild a handle from its string form.
    ///
    /// Needed because a handle makes a round trip through a client: it is
    /// issued as text and comes back as text. This does not weaken the
    /// guarantee, since a handle is only ever meaningful to the store that
    /// issued it -- an invented one is simply unknown.
    fn from(s: &str) -> Self {
        OutputRef(s.to_string())
    }
}

/// What part of an output to return.
#[derive(Debug, Clone)]
pub enum Slice {
    /// The whole thing. The client asked; the client gets it.
    All,
    Head(usize),
    Tail(usize),
    /// 1-based, inclusive.
    Range(usize, usize),
    /// Everything ACL2 printed under one goal, from its header to the next.
    ///
    /// The natural way to navigate a proof: a client that has been handed
    /// `Subgoal *1/2` as a checkpoint asks for that goal rather than guessing
    /// line numbers. On a six-figure log it is the difference between a
    /// targeted question and a fishing expedition.
    ///
    /// Recognising headers is a text operation, like every other slice here.
    /// ACL2 can do it exactly -- `pso` accepts a goal name and replays the
    /// saved io-records for it -- but that needs output saving switched on, an
    /// ACL2 round trip per slice, and per-unit retention of records in session
    /// state that grows without bound. Text works on every retained log, costs
    /// nothing, and slices are display, never control flow.
    Goal { name: String },
    /// Literal substring match with surrounding context lines.
    ///
    /// Substring rather than regex, to keep the core dependency-free. In
    /// practice searches here are literal -- "Subgoal", "Warning", a rule
    /// name -- so the gap is small. Revisit if real use says otherwise.
    Find { needle: String, context: usize, ignore_case: bool },
}

#[derive(Debug, Clone)]
pub struct OutputSlice {
    pub text: String,
    /// Total lines behind the handle, so a client can always tell how much it
    /// is *not* being shown. A sixty-line log and a two-hundred-thousand-line
    /// log must not look alike.
    pub total_lines: usize,
    /// True once the unit this output came from has been rolled back. The
    /// output stays readable -- comparing against what a form used to print is
    /// useful -- but it no longer describes the current world.
    pub superseded: bool,
}

/// Is this line a bare goal header, such as `Goal`, `Goal'` or
/// `Subgoal *1/2''`?
///
/// Deliberately strict: prose that merely mentions a goal -- "A goal of NIL,
/// Subgoal *1/2'', has been generated!" -- must not be mistaken for a header,
/// or a slice would stop early.
fn is_goal_header(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || t.len() > 40 {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    if !(lower.starts_with("goal") || lower.starts_with("subgoal")) {
        return false;
    }
    t.chars()
        .all(|c| c.is_ascii_alphanumeric() || " *'/.".contains(c))
}

struct Entry {
    path: PathBuf,
    lines: usize,
    superseded: bool,
}

/// Per-session store. Spills to disk: even a large book's text output is tens
/// of megabytes, and "I can go back and look" is worth far more than the bytes.
pub struct OutputStore {
    dir: PathBuf,
    session: String,
    next: u64,
    index: HashMap<String, Entry>,
}

impl OutputStore {
    /// A store of its own, sharing nothing with any other.
    ///
    /// The id is minted here rather than supplied, because a caller that
    /// derives one can collide. This previously took the process id, which was
    /// unique while one process held one session -- and stopped being unique
    /// the moment a registry held several. Two sessions then shared a directory
    /// *and* a key space, so each handed out `s<pid>:f1`, and asking one
    /// session for its own handle silently returned another session's output.
    /// Worse, closing either session removed the shared directory and broke
    /// every other session's handles.
    pub fn new() -> io::Result<OutputStore> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let session = format!(
            "{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        );
        let dir = std::env::temp_dir().join(format!("acl2-session-out-{session}"));
        std::fs::create_dir_all(&dir)?;
        Ok(OutputStore { dir, session, next: 0, index: HashMap::new() })
    }

    pub fn put(&mut self, text: &str) -> io::Result<OutputRef> {
        self.next += 1;
        let key = format!("s{}:f{}", self.session, self.next);
        let path = self.dir.join(format!("f{}.txt", self.next));
        std::fs::write(&path, text)?;
        let lines = if text.is_empty() { 0 } else { text.lines().count() };
        self.index.insert(key.clone(), Entry { path, lines, superseded: false });
        Ok(OutputRef(key))
    }

    pub fn lines(&self, r: &OutputRef) -> Option<usize> {
        self.index.get(&r.0).map(|e| e.lines)
    }

    /// Mark output as no longer describing the current world. Never removes it.
    pub fn mark_superseded(&mut self, r: &OutputRef) {
        if let Some(e) = self.index.get_mut(&r.0) {
            e.superseded = true;
        }
    }

    pub fn get(&self, r: &OutputRef, slice: Slice) -> io::Result<OutputSlice> {
        let e = self.index.get(&r.0).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("unknown output ref {}", r.0))
        })?;
        let body = std::fs::read_to_string(&e.path)?;
        let all: Vec<&str> = body.lines().collect();
        let text = match slice {
            Slice::All => body.clone(),
            Slice::Head(n) => all.iter().take(n).cloned().collect::<Vec<_>>().join("\n"),
            Slice::Tail(n) => {
                let skip = all.len().saturating_sub(n);
                all[skip..].join("\n")
            }
            Slice::Range(from, to) => {
                let lo = from.saturating_sub(1).min(all.len());
                let hi = to.min(all.len());
                if lo >= hi { String::new() } else { all[lo..hi].join("\n") }
            }
            Slice::Goal { name } => {
                let want = name.trim();
                let start = all.iter().position(|l| {
                    is_goal_header(l) && l.trim().eq_ignore_ascii_case(want)
                });
                match start {
                    None => String::new(),
                    Some(i) => {
                        let end = all[i + 1..]
                            .iter()
                            .position(|l| is_goal_header(l))
                            .map(|k| i + 1 + k)
                            .unwrap_or(all.len());
                        all[i..end].join("\n").trim_end().to_string()
                    }
                }
            }
            Slice::Find { needle, context, ignore_case } => {
                let pat = if ignore_case { needle.to_lowercase() } else { needle.clone() };
                let matches = |l: &str| {
                    if ignore_case { l.to_lowercase().contains(&pat) } else { l.contains(&pat) }
                };
                // Merge the context windows into disjoint ranges. Windows
                // around nearby matches overlap, and emitting each window
                // separately repeats their shared lines -- which reads as ACL2
                // having printed something twice.
                let mut ranges: Vec<(usize, usize)> = Vec::new();
                for (i, _) in all.iter().enumerate().filter(|(_, l)| matches(l)) {
                    let lo = i.saturating_sub(context);
                    let hi = (i + context + 1).min(all.len());
                    match ranges.last_mut() {
                        Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
                        _ => ranges.push((lo, hi)),
                    }
                }
                let mut out: Vec<String> = Vec::new();
                for (n, (lo, hi)) in ranges.iter().enumerate() {
                    if n > 0 {
                        out.push("...".to_string());
                    }
                    out.extend(all[*lo..*hi].iter().map(|l| l.to_string()));
                }
                out.join("\n")
            }
        };
        Ok(OutputSlice { text, total_lines: e.lines, superseded: e.superseded })
    }
}

impl Drop for OutputStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> OutputStore {
        OutputStore::new().unwrap()
    }

    const BODY: &str = "one\ntwo\nSubgoal *1/2\nfour\nfive\nsix\nSubgoal *1/1\neight";

    #[test]
    fn round_trip_and_line_count() {
        let mut s = store();
        let r = s.put(BODY).unwrap();
        assert_eq!(s.lines(&r), Some(8));
        let got = s.get(&r, Slice::All).unwrap();
        assert_eq!(got.text, BODY);
        assert_eq!(got.total_lines, 8);
        assert!(!got.superseded);
    }

    #[test]
    fn slices() {
        let mut s = store();
        let r = s.put(BODY).unwrap();
        assert_eq!(s.get(&r, Slice::Head(2)).unwrap().text, "one\ntwo");
        assert_eq!(s.get(&r, Slice::Tail(2)).unwrap().text, "Subgoal *1/1\neight");
        assert_eq!(s.get(&r, Slice::Range(2, 3)).unwrap().text, "two\nSubgoal *1/2");
        // Asking for more than exists is not an error.
        assert_eq!(s.get(&r, Slice::Tail(99)).unwrap().text, BODY);
        assert_eq!(s.get(&r, Slice::Range(99, 120)).unwrap().text, "");
    }

    #[test]
    fn overlapping_context_windows_do_not_repeat_lines() {
        let mut s = store();
        let r = s.put("a\nhit\nhit\nhit\nz").unwrap();
        let got = s.get(&r, Slice::Find {
            needle: "hit".into(), context: 1, ignore_case: false,
        }).unwrap();
        // Three adjacent matches with context 1 cover the whole thing exactly
        // once; naive per-match windows would repeat the middle lines.
        assert_eq!(got.text, "a\nhit\nhit\nhit\nz");
    }

    #[test]
    fn find_with_context_and_gaps() {
        let mut s = store();
        let r = s.put(BODY).unwrap();
        let got = s.get(&r, Slice::Find {
            needle: "Subgoal".into(), context: 1, ignore_case: false,
        }).unwrap();
        // Two disjoint regions, so the gap between them is marked.
        assert_eq!(got.text, "two\nSubgoal *1/2\nfour\n...\nsix\nSubgoal *1/1\neight");
        // Total is always the whole log, never the size of the slice.
        assert_eq!(got.total_lines, 8);
    }

    #[test]
    fn find_is_case_insensitive_on_request() {
        let mut s = store();
        let r = s.put(BODY).unwrap();
        let hit = |ic| s.get(&r, Slice::Find {
            needle: "subgoal".into(), context: 0, ignore_case: ic,
        }).unwrap().text;
        assert_eq!(hit(false), "");
        // Non-adjacent hits, so the elision between them is marked even at
        // zero context: a client must not read two distant lines as contiguous.
        assert_eq!(hit(true), "Subgoal *1/2\n...\nSubgoal *1/1");
    }

    #[test]
    fn superseded_output_stays_readable() {
        let mut s = store();
        let r = s.put(BODY).unwrap();
        s.mark_superseded(&r);
        let got = s.get(&r, Slice::All).unwrap();
        assert!(got.superseded, "must be flagged");
        assert_eq!(got.text, BODY, "but must not be discarded");
    }

    const PROOF: &str = "\
Goal
(equal (na x) x)

This simplifies, using nothing, to
Subgoal *1/2
(implies (not (consp x)) (not x))
and that is false.
Subgoal *1/1
(implies (consp x) t)
Q.E.D.";

    #[test]
    fn goal_slice_runs_to_the_next_header() {
        let mut s = store();
        let r = s.put(PROOF).unwrap();
        let got = s.get(&r, Slice::Goal { name: "Subgoal *1/2".into() }).unwrap();
        assert_eq!(
            got.text,
            "Subgoal *1/2\n(implies (not (consp x)) (not x))\nand that is false."
        );
        // The first goal runs up to the first subgoal, not to end of file.
        let got = s.get(&r, Slice::Goal { name: "Goal".into() }).unwrap();
        assert!(got.text.starts_with("Goal\n(equal (na x) x)"));
        assert!(!got.text.contains("Subgoal"));
        // The last goal runs to the end.
        let got = s.get(&r, Slice::Goal { name: "Subgoal *1/1".into() }).unwrap();
        assert!(got.text.ends_with("Q.E.D."));
    }

    #[test]
    fn an_unknown_goal_is_empty_not_an_error() {
        let mut s = store();
        let r = s.put(PROOF).unwrap();
        assert_eq!(s.get(&r, Slice::Goal { name: "Subgoal 9.9".into() }).unwrap().text, "");
    }

    #[test]
    fn prose_mentioning_a_goal_is_not_a_header() {
        assert!(is_goal_header("Subgoal *1/2"));
        assert!(is_goal_header("  Goal'  "));
        assert!(is_goal_header("Subgoal *1/2''"));
        // These would truncate a slice if treated as headers.
        assert!(!is_goal_header("A goal of NIL, Subgoal *1/2'', has been generated!"));
        assert!(!is_goal_header("Goal of NIL, see :DOC nil-goal"));
        assert!(!is_goal_header("(implies (not (consp x)) (not x))"));
        assert!(!is_goal_header(""));
    }

    /// Two stores must share neither a key space nor a directory: one session
    /// asking for its own handle must never receive another session's output,
    /// and closing one must not break the other.
    #[test]
    fn stores_are_independent() {
        let mut a = store();
        let mut b = store();
        let ra = a.put("from A").unwrap();
        let rb = b.put("from B").unwrap();
        assert_ne!(ra, rb, "first handles of two stores must differ");
        assert_eq!(a.get(&ra, Slice::All).unwrap().text, "from A");
        assert_eq!(b.get(&rb, Slice::All).unwrap().text, "from B");
        // A's handle means nothing to B, rather than meaning the wrong thing.
        assert!(b.get(&ra, Slice::All).is_err());

        drop(b);
        assert_eq!(a.get(&ra, Slice::All).unwrap().text, "from A",
                   "closing one session must not disturb another");
    }

    #[test]
    fn unknown_ref_is_an_error() {
        let s = store();
        let bogus = OutputRef("s0:f999".to_string());
        assert!(s.get(&bogus, Slice::All).is_err());
    }
}
