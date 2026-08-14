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

/// What part of an output to return.
#[derive(Debug, Clone)]
pub enum Slice {
    /// The whole thing. The client asked; the client gets it.
    All,
    Head(usize),
    Tail(usize),
    /// 1-based, inclusive.
    Range(usize, usize),
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

struct Entry {
    path: PathBuf,
    lines: usize,
    superseded: bool,
}

/// Per-session store. Spills to disk: even a large book's text output is tens
/// of megabytes, and "I can go back and look" is worth far more than the bytes.
pub struct OutputStore {
    dir: PathBuf,
    session: u64,
    next: u64,
    index: HashMap<String, Entry>,
}

impl OutputStore {
    pub fn new(session: u64) -> io::Result<OutputStore> {
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
            Slice::Find { needle, context, ignore_case } => {
                let hay = |l: &str| {
                    if ignore_case { l.to_lowercase() } else { l.to_string() }
                };
                let pat = if ignore_case { needle.to_lowercase() } else { needle.clone() };
                let hits: Vec<usize> = all
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| hay(l).contains(&pat))
                    .map(|(i, _)| i)
                    .collect();
                let mut keep: Vec<usize> = Vec::new();
                for h in hits {
                    let lo = h.saturating_sub(context);
                    let hi = (h + context + 1).min(all.len());
                    for i in lo..hi {
                        if keep.last() != Some(&i) {
                            keep.push(i);
                        }
                    }
                }
                keep.dedup();
                let mut out: Vec<String> = Vec::new();
                let mut prev: Option<usize> = None;
                for i in keep {
                    if let Some(p) = prev {
                        if i > p + 1 {
                            out.push("...".to_string());
                        }
                    }
                    out.push(all[i].to_string());
                    prev = Some(i);
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

    /// A store per test. Tests run in parallel and every store owns (and on
    /// drop deletes) its own directory, so they must not share a session id.
    fn store() -> OutputStore {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = (std::process::id() as u64) * 1000 + N.fetch_add(1, Ordering::SeqCst);
        OutputStore::new(id).unwrap()
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

    #[test]
    fn unknown_ref_is_an_error() {
        let s = store();
        let bogus = OutputRef("s0:f999".to_string());
        assert!(s.get(&bogus, Slice::All).is_err());
    }
}
