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

/// What a caller asked for, before it is resolved into a [`Slice`].
///
/// Both wire layers accept the same six fields under the same names and mean
/// the same thing by them, and until this they each turned them into a `Slice`
/// themselves -- with differences nobody chose. The editor errored on a
/// `range` with no bounds; the MCP silently read it as "the whole file". The
/// editor errored on a `goal` with no text; the MCP searched for the empty
/// string, which matches the first line of anything. One verb, parsed twice,
/// answering differently.
///
/// The line count stays per-surface, because that difference *is* chosen: an
/// editor renders and pays nothing per line, an agent surface pays per token.
/// So `resolve` takes the default rather than owning one.
#[derive(Debug, Default, Clone)]
pub struct SliceRequest<'a> {
    /// `all`, `head`, `tail` (the default), `range`, `goal`, or `find`.
    pub mode: Option<&'a str>,
    pub lines: Option<usize>,
    pub from_line: Option<usize>,
    pub to_line: Option<usize>,
    /// The goal name for `goal`, the literal needle for `find`.
    pub text: Option<&'a str>,
    pub context: Option<usize>,
}

impl SliceRequest<'_> {
    /// Resolve into a [`Slice`], or say what is missing.
    ///
    /// Errors rather than defaulting wherever the answer would otherwise be a
    /// guess about what the caller meant. A `range` with no bounds and a
    /// `goal` with no name are not requests with obvious readings; returning
    /// the whole log for the first and matching everything for the second
    /// gives a caller an answer to a question they did not ask, which is
    /// harder to notice than an error.
    pub fn resolve(&self, default_lines: usize) -> Result<Slice, String> {
        let lines = self.lines.unwrap_or(default_lines);
        match self.mode.unwrap_or("tail") {
            "all" => Ok(Slice::All),
            "head" => Ok(Slice::Head(lines)),
            "tail" => Ok(Slice::Tail(lines)),
            "range" => match (self.from_line, self.to_line) {
                (Some(a), Some(b)) => Ok(Slice::Range(a, b)),
                _ => Err("range needs from_line and to_line".into()),
            },
            "goal" => match self.text {
                Some(t) if !t.is_empty() => Ok(Slice::Goal { name: t.to_string() }),
                _ => Err("goal needs text: the goal name, such as \"Subgoal *1/2\"".into()),
            },
            "find" => match self.text {
                Some(t) if !t.is_empty() => Ok(Slice::Find {
                    needle: t.to_string(),
                    context: self.context.unwrap_or(2),
                    ignore_case: true,
                }),
                _ => Err("find needs text: the literal to search for".into()),
            },
            m => Err(format!("unknown output mode {m:?}")),
        }
    }
}

/// One reading of the slice parameters, so two surfaces cannot drift on it.
///
/// The divergence these pin was not a choice anywhere: a `range` with no
/// bounds meant "the whole log" on one surface and an error on the other, and
/// a `goal` with no name searched for the empty string, which matches the
/// first line of anything.
#[cfg(test)]
mod slice_request_tests {
    use super::{Slice, SliceRequest};

    fn ask(mode: &str) -> SliceRequest<'_> {
        SliceRequest { mode: Some(mode), ..SliceRequest::default() }
    }

    #[test]
    fn the_default_is_a_tail_of_whatever_the_surface_asked_for() {
        let r = SliceRequest::default().resolve(40).expect("tail");
        assert!(matches!(r, Slice::Tail(40)));
        assert!(matches!(SliceRequest::default().resolve(50).expect("tail"), Slice::Tail(50)));
    }

    #[test]
    fn an_explicit_count_beats_the_surfaces_default() {
        let r = SliceRequest { lines: Some(7), ..ask("head") }.resolve(40).expect("head");
        assert!(matches!(r, Slice::Head(7)));
    }

    /// The half that used to differ. Neither reading is obvious, so neither is
    /// guessed: an error is easier to notice than an answer to a question
    /// nobody asked.
    #[test]
    fn a_range_needs_both_of_its_bounds() {
        assert!(ask("range").resolve(40).is_err(), "neither bound");
        assert!(SliceRequest { from_line: Some(1), ..ask("range") }.resolve(40).is_err());
        assert!(SliceRequest { to_line: Some(9), ..ask("range") }.resolve(40).is_err());
        let ok = SliceRequest { from_line: Some(1), to_line: Some(9), ..ask("range") }
            .resolve(40)
            .expect("range");
        assert!(matches!(ok, Slice::Range(1, 9)));
    }

    #[test]
    fn goal_and_find_need_text_that_says_something() {
        for mode in ["goal", "find"] {
            assert!(ask(mode).resolve(40).is_err(), "{mode} with no text");
            assert!(
                SliceRequest { text: Some(""), ..ask(mode) }.resolve(40).is_err(),
                "{mode} with empty text -- which matches everything"
            );
        }
        let g = SliceRequest { text: Some("Subgoal *1/2"), ..ask("goal") }
            .resolve(40)
            .expect("goal");
        assert!(matches!(g, Slice::Goal { name } if name == "Subgoal *1/2"));
    }

    #[test]
    fn an_unknown_mode_names_itself() {
        let e = ask("sideways").resolve(40).unwrap_err();
        assert!(e.contains("sideways"), "{e}");
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
    // A forcing round prefixes the name -- `[1]Goal`, `[2]Subgoal *1/3'`. That
    // bracket is part of what `tilde-@-clause-id-phrase` prints, so it is part
    // of the `goal` a checkpoint hands the caller, and a slice that did not
    // accept it returned nothing for exactly the name it had just supplied.
    let bare = match t.strip_prefix('[') {
        None => t,
        Some(rest) => match rest.split_once(']') {
            Some((round, after))
                if !round.is_empty() && round.bytes().all(|b| b.is_ascii_digit()) =>
            {
                after
            }
            _ => return false,
        },
    };
    let lower = bare.to_ascii_lowercase();
    if !(lower.starts_with("goal") || lower.starts_with("subgoal")) {
        return false;
    }
    bare.chars()
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

/// A directory only this user can read, that nobody else created.
///
/// Both of this crate's scratch directories live under the system temp
/// directory, which on a shared machine is world-writable and whose contents
/// are guessable: `acl2-session-{pid}` holds every form submitted to ACL2 and
/// `acl2-session-out-{pid}-{n}` holds every log. `create_dir_all` succeeds on
/// a directory that already exists, so another user who pre-created one -- or
/// planted a symlink at that name -- would have had every form and every proof
/// read into their own directory, with nothing here noticing.
///
/// Two changes close it, and the second is the one that matters:
///
/// - mode `0700` at creation, so the contents are the user's alone. Set in the
///   `mkdir` rather than afterwards, since a `chmod` after the fact leaves a
///   window where the directory exists world-readable.
/// - `create_dir`, not `create_dir_all`: a non-recursive `mkdir` fails with
///   `EEXIST` for anything already at that name, a symlink included. So a
///   pre-created directory is refused rather than used, and the property is
///   *we only ever write into a directory we made*, which holds even against
///   a name the attacker guessed exactly.
///
/// A collision with our own leftovers is possible -- pids are reused -- so a
/// taken name is retried with a suffix rather than treated as an attack. What
/// is never done is writing into it.
///
/// `$XDG_RUNTIME_DIR` is preferred when it is *usable*: the login session's own
/// directory, already private and already cleaned up at logout. The variable
/// is set in plenty of places the directory is not writable, so this falls
/// back to the system temp directory rather than refusing to open a session.
pub(crate) fn private_dir(prefix: &str) -> io::Result<PathBuf> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    // `$XDG_RUNTIME_DIR` first, then the system temp directory. Tried in turn
    // rather than chosen up front: the variable is often set where the
    // directory is not usable -- a container, a sandbox, a service account
    // whose runtime dir was never made -- and refusing to open a session over
    // that would be a worse failure than falling back to the place we used
    // before. Only a *usable* root is preferred, not merely a named one.
    let roots = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .into_iter()
        .chain(std::iter::once(std::env::temp_dir()));
    let mut last = None;
    for root in roots {
        for n in 0..64 {
            let dir =
                root.join(if n == 0 { prefix.to_string() } else { format!("{prefix}-{n}") });
            match builder.create(&dir) {
                Ok(()) => return Ok(dir),
                // Taken -- by our own leftovers, since pids are reused, or by
                // somebody else. Which it is does not matter: the name is not
                // ours, so it is stepped over rather than written into.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                // The root itself is no good. Try the next one.
                Err(e) => {
                    last = Some(e);
                    break;
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::AlreadyExists, format!("no free scratch directory for {prefix}"))
    }))
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
        let dir = private_dir(&format!("acl2-session-out-{session}"))?;
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

    /// Write content into a handle already handed out.
    ///
    /// A form that outruns its deadline gets a handle immediately, so the
    /// reply can name one, but nothing to put in it yet. When the form finally
    /// finishes, its output belongs under *that* handle: a caller who kept the
    /// ref from the timed-out call and reads it later must see the form's
    /// output, not an empty file that says it printed nothing.
    pub fn fill(&mut self, r: &OutputRef, text: &str) -> io::Result<()> {
        let Some(e) = self.index.get_mut(&r.0) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no output {}", r.0),
            ));
        };
        std::fs::write(&e.path, text)?;
        e.lines = if text.is_empty() { 0 } else { text.lines().count() };
        Ok(())
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
        Ok(OutputSlice {
            text: slice_text(&body, slice),
            total_lines: e.lines,
            superseded: e.superseded,
        })
    }
}

/// Count lines the way `lines()` does: separators, plus a trailing partial.
///
/// Public because a live read has no `Entry` to take the count from -- the
/// form is still printing, so nothing has been recorded for it anywhere.
pub fn count_lines(body: &str) -> usize {
    if body.is_empty() { 0 } else { body.lines().count() }
}

/// The last `n` lines of `body`, without walking the rest of it.
///
/// A live view asks for a tail twice a second, and the buffer it is asking
/// about is the whole of a running proof's output -- which is exactly the
/// case where `lines().collect()` over the entire thing is the wrong shape.
/// Scanning back from the end costs the tail, not the log.
pub fn tail_bytes(body: &[u8], n: usize) -> &[u8] {
    if n == 0 {
        return &[];
    }
    let mut seen = 0;
    let mut i = body.len();
    // A trailing newline ends the last line rather than starting an empty one.
    if i > 0 && body[i - 1] == b'\n' {
        i -= 1;
    }
    while i > 0 {
        if body[i - 1] == b'\n' {
            seen += 1;
            if seen == n {
                return &body[i..];
            }
        }
        i -= 1;
    }
    body
}

/// Every slice mode, over text from wherever it came.
///
/// Split out from `get` because the file is only ever the *source*: none of
/// this cares where the bytes were, and a form still running has its bytes in
/// memory rather than on disk. One implementation means a live read and a
/// finished read cannot answer the same question differently.
pub fn slice_text(body: &str, slice: Slice) -> String {
        let all: Vec<&str> = body.lines().collect();
        match slice {
            Slice::All => body.to_string(),
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
        }
}

impl Drop for OutputStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The scratch directory's own guarantees.
///
/// Worth testing rather than reading, because the property is about what
/// happens when something is *already there* -- the case that never occurs in
/// ordinary use and is the whole point of the change.
#[cfg(test)]
mod dir_tests {
    use super::private_dir;

    #[test]
    fn a_fresh_directory_is_the_users_alone() {
        let d = private_dir("acl2-session-test-mode").expect("dir");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&d).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "nobody else may read what a session submits");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The property that closes the attack: a name already taken is never
    /// written into. `create_dir_all` used to succeed on someone else's
    /// directory -- or through their symlink -- and every form and every proof
    /// went there.
    #[test]
    fn a_name_already_taken_is_never_reused() {
        let first = private_dir("acl2-session-test-taken").expect("first");
        let second = private_dir("acl2-session-test-taken").expect("second");
        assert_ne!(first, second, "the taken name was stepped over, not adopted");
        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }

    /// A symlink counts as taken. This is the shape of the attack: a
    /// world-writable temp directory and a guessable name, so another user
    /// plants a link and reads everything written through it.
    #[test]
    #[cfg(unix)]
    fn a_symlink_at_the_name_is_stepped_over_not_followed() {
        // Wherever `private_dir` would actually land: the same fallback, run
        // by making one and taking its parent, so this test follows the
        // function rather than re-deriving its choice.
        let probe = private_dir("acl2-session-test-where").expect("probe");
        let root = probe.parent().expect("a parent").to_path_buf();
        let _ = std::fs::remove_dir_all(&probe);
        let elsewhere = root.join("acl2-session-test-elsewhere");
        let _ = std::fs::remove_dir_all(&elsewhere);
        std::fs::create_dir_all(&elsewhere).expect("target");
        let planted = root.join("acl2-session-test-link");
        let _ = std::fs::remove_file(&planted);
        std::os::unix::fs::symlink(&elsewhere, &planted).expect("symlink");

        let got = private_dir("acl2-session-test-link").expect("dir");
        assert_ne!(got, planted, "the link was not adopted");
        assert!(!got.starts_with(&elsewhere), "and nothing lands behind it");

        let _ = std::fs::remove_dir_all(&got);
        let _ = std::fs::remove_file(&planted);
        let _ = std::fs::remove_dir_all(&elsewhere);
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

    /// Forcing produces rounds, and their goals carry the round in brackets.
    /// These names reach a caller through a checkpoint's `goal`, so failing to
    /// match them makes `mode:"goal"` empty for the name the envelope gave.
    #[test]
    fn a_forcing_round_goal_is_a_header() {
        assert!(is_goal_header("[1]Goal"));
        assert!(is_goal_header("[1]Subgoal *1/3''"));
        assert!(is_goal_header("[12]Subgoal 1.2"));
        // Still strict: a bracket is only a round when it holds digits and
        // what follows is itself a goal name.
        assert!(!is_goal_header("[1]Nonsense"));
        assert!(!is_goal_header("[a]Goal"));
        assert!(!is_goal_header("[]Goal"));
        assert!(!is_goal_header("[1Goal"));
        assert!(!is_goal_header("[1]A goal of NIL, has been generated!"));
    }

    #[test]
    fn a_forcing_round_goal_slices() {
        let mut s = store();
        let r = s
            .put("[1]Goal\n(equal (na x) x)\n\n[1]Subgoal *1/2\n(consp x)\nQ.E.D.\n")
            .unwrap();
        let got = s.get(&r, Slice::Goal { name: "[1]Goal".into() }).unwrap();
        assert_eq!(got.text, "[1]Goal\n(equal (na x) x)");
        let got = s.get(&r, Slice::Goal { name: "[1]Subgoal *1/2".into() }).unwrap();
        assert!(got.text.ends_with("Q.E.D."));
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
