//! Wire shapes for the editor protocol.
//!
//! Owned here deliberately: the core stays serde-free, and these shapes
//! are deliberately richer than the MCP crate's -- an editor renders; it does
//! not pay per token.

use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

use acl2_session::sync::{Checkpoints, EvalResult, FailureReport, UnitReport};
use acl2_session::{
    CertReport, CertStatus, NameDetail, NameKind, Origin, Outcome, Slice, Status, WorldChange,
};

/// How wide the client's pane is, in characters.
///
/// Terms cross this wire already laid out for it. Only the client can know
/// this number -- it is pixels and a font, so it is a measurement rather than
/// a setting -- and it changes when the pane is dragged, so the client sends
/// it and the daemon remembers it.
///
/// Process-wide, and that is not laziness. A daemon speaks to exactly one
/// client over one pair of pipes, and that client has one pane; the
/// alternative is threading a display setting through every constructor in
/// this file. A daemon that ever served two clients at once would have to
/// take this apart, and nothing else here would need to change.
static PANE_COLS: AtomicUsize = AtomicUsize::new(DEFAULT_COLS);

/// What to lay out for until the client says otherwise. ACL2's own margin, so
/// a client that never reports gets what it would have got from a terminal.
const DEFAULT_COLS: usize = 77;

/// Record the width the client is reading at.
pub fn set_pane_cols(cols: usize) {
    PANE_COLS.store(cols, Ordering::Relaxed);
}

/// Lay a term out for the pane, the way ACL2 would have at that width.
///
/// Everything on this wire that is ACL2 *code* rather than prose goes through
/// here. Deliberately not in the core's own types: the MCP surface carries the
/// same terms and has no pane, so laying out belongs to the client that has
/// one.
fn laid(term: &str) -> String {
    acl2_session::pretty::pretty(term, PANE_COLS.load(Ordering::Relaxed))
}

/// The same, for a field that may be absent.
fn laid_opt(term: &Option<String>) -> Option<String> {
    term.as_deref().map(laid)
}

/// Lay a term out for a box that is not the form view.
///
/// A hover is the case: it has a width, but nothing can measure it. The
/// extension API exposes no pixel geometry at all -- not for the hover
/// widget, not even for the editor -- and a webview, the one place an
/// extension *can* measure, is not where a tooltip lives. So the client says
/// what it wants and this obeys, rather than borrowing the form view's width,
/// which belongs to a different box and moves when a different pane is
/// dragged.
fn laid_at(term: &Option<String>, cols: Option<usize>) -> Option<String> {
    let cols = cols.unwrap_or(DEFAULT_COLS);
    term.as_deref().map(|t| acl2_session::pretty::pretty(t, cols))
}

#[derive(Debug, Deserialize)]
pub struct LayoutParams {
    /// The pane width in characters, as the client measured it.
    pub cols: usize,
}

#[derive(Debug, Deserialize)]
pub struct ReflowParams {
    /// Terms to lay out again, exactly as the client already holds them.
    pub terms: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ReflowReply {
    /// One per term sent, in the same order.
    pub terms: Vec<String>,
}

/// Lay terms out again at the width the client last reported.
pub fn reflow(terms: &[String]) -> ReflowReply {
    ReflowReply { terms: terms.iter().map(|t| laid(t)).collect() }
}

#[derive(Debug, Deserialize)]
pub struct OpenParams {
    pub path: String,
    pub image: Option<String>,
    /// Defaults to on: the editor is the triage client.
    pub continue_past_failure: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct OpenReply {
    pub book: String,
    pub image: String,
    /// The server re-enabled PROVE output so proofs could be captured, because
    /// this environment (the user's `acl2-customization.lsp`, usually) inhibits
    /// it. Holds the inherited `inhibit-output-lst` when that happened, and is
    /// absent otherwise -- so the client both learns *that* it happened and can
    /// show *what* was inherited. Mirrors the MCP server's field of the same
    /// name; the editor tells a human, so the client decides whether to say so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prove_output_overridden: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BookParams {
    pub book: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateParams {
    pub book: String,
    /// The full snapshot, when the client has one that is not the file.
    ///
    /// **Absent means "the buffer is the file"**, and the daemon reads the
    /// path instead -- which is the only way a book that is not UTF-8 can be
    /// synced at all. ACL2 source is Latin-1 and one book in a community
    /// checkout is not valid UTF-8; an editor that decoded it has already
    /// replaced those bytes, and no field of this type can carry them back.
    /// A client with unsaved edits sends what it has and takes the loss; a
    /// client without them should omit this.
    #[serde(default)]
    pub text: Option<String>,
    /// Auto-admit only through this 1-based line. Omitted means the whole
    /// book; 0 holds before the first unit.
    pub target_line: Option<usize>,
    /// The column on that line, 1-based and in bytes.
    ///
    /// An editor always knows it, and should always send it: a line names a
    /// form exactly until two of them share one, and then the column is the
    /// only thing that says which. Omitted, the line means the whole line
    /// and takes the last form on it.
    #[serde(default)]
    pub target_column: Option<usize>,
    /// Stamped back on every event this snapshot produces, so the client can
    /// drop stale ones. Defaults to a daemon counter when the client has no
    /// document version to offer.
    pub revision: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct RetractParams {
    pub book: String,
    /// Keep only the first `keep` units; the target holds there afterwards.
    pub keep: usize,
}

#[derive(Debug, Deserialize)]
pub struct EvalParams {
    pub book: String,
    /// One or more top-level forms, as the user typed them. Not part of the
    /// file, so nothing here is admitted: see the core's `eval`.
    pub form: String,
}

#[derive(Debug, Deserialize)]
pub struct DescribeParams {
    pub book: String,
    /// How wide to lay the terms out, in characters. Absent falls back to
    /// ACL2's own margin: the client is the only one who can have an opinion
    /// here, and one that offers none gets what a terminal would have shown.
    #[serde(default)]
    pub cols: Option<usize>,
    /// The token under the cursor, as the client read it out of the buffer.
    /// Not a position: coordinates never cross this
    /// wire. A token that is not a plain symbol simply answers "not found".
    pub name: String,
}

/// What the world knows about one name -- the hover answer.
///
/// Shaped as its LSP counterpart's payload so the eventual facade is a
/// serializer rather than a redesign: everything here is a field
/// `MarkupContent` can be built from without asking a second question.
#[derive(Debug, Serialize)]
pub struct DescribeReply {
    /// False when the session was busy proving and the answer was not already
    /// cached. Distinct from `found: false`, and the distinction is the whole
    /// point: "ask me again" must not render as "no such name". A hover never
    /// waits on the prover.
    pub available: bool,
    pub found: bool,
    pub name: String,
    /// `function`, `macro`, `theorem`, `constant`, `other`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    /// `file`, `book`, or `builtin`. Absence of a `book` does not mean "this
    /// file": ACL2's own names have no book either.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formals: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guard: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statement: Option<String>,
    /// The book whose text defines it, as ACL2 names it. Absent means the
    /// name came from this file or from ACL2 itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub book: Option<String>,
    /// `book` resolved to a path that can be opened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The topic's one-line summary, as **raw xdoc markup** -- it will
    /// contain `@(see …)` and `@('…')`, because the machinery that expands
    /// those is not in the world. The client renders it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short: Option<String>,
}

pub fn origin_str(o: Origin) -> &'static str {
    match o {
        Origin::File => "file",
        Origin::Book => "book",
        Origin::Builtin => "builtin",
    }
}

pub fn kind_str(k: NameKind) -> &'static str {
    match k {
        NameKind::Function => "function",
        NameKind::Macro => "macro",
        NameKind::Theorem => "theorem",
        NameKind::Constant => "constant",
        NameKind::Other => "other",
    }
}

pub fn described(name: &str, d: Option<&NameDetail>, cols: Option<usize>) -> DescribeReply {
    match d {
        None => DescribeReply {
            available: true,
            found: false,
            name: name.to_string(),
            kind: None,
            origin: None,
            formals: None,
            guard: None,
            statement: None,
            book: None,
            path: None,
            short: None,
        },
        Some(d) => DescribeReply {
            available: true,
            found: true,
            name: d.name.clone(),
            kind: Some(kind_str(d.kind)),
            origin: Some(origin_str(d.origin)),
            formals: d.formals.clone(),
            // Laid out, like the form view's. `formals` is not: a short list ACL2
            // never wraps anyway.
            guard: laid_at(&d.guard, cols),
            statement: laid_at(&d.statement, cols),
            book: d.book.clone(),
            path: d.book_path.as_ref().map(|p| p.display().to_string()),
            short: None,
        },
    }
}

/// The session is proving and this name was not cached. Not an error: the
/// client shows nothing and asks again on the next hover.
pub fn describe_busy(name: &str) -> DescribeReply {
    DescribeReply {
        available: false,
        found: false,
        name: name.to_string(),
        kind: None,
        origin: None,
        formals: None,
        guard: None,
        statement: None,
        book: None,
        path: None,
        short: None,
    }
}

#[derive(Debug, Deserialize)]
pub struct DefinitionParams {
    pub book: String,
    /// The token under the cursor, like `describe`.
    pub name: String,
}

/// Where a name is defined -- a `Location`, in LSP's shape.
///
/// `path` absent with `unit` present means "in the file you are looking at";
/// the client already has that unit's span and needs no path to jump.
#[derive(Debug, Serialize)]
pub struct DefinitionReply {
    /// As in `DescribeReply`: the session was busy and nothing was cached.
    pub available: bool,
    pub found: bool,
    /// The unit of *this* file, when the name is defined here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<usize>,
    /// Another file, when it is not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 1-based line within whichever file. Absent means "the file, somewhere"
    /// -- open it at the top rather than pointing at a form we did not find.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// 1-based byte column on that line. Tracks `line`: both are set or
    /// neither is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    /// How the line was reached, so the client can say how sure it is:
    /// `definer` (the name is written there -- exact) or `unit` (the file's
    /// own envelope said so -- exact). Generated names in *other* books wait
    /// on the census; until then they answer with a path and no line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub how: Option<&'static str>,
}

pub fn definition_busy() -> DefinitionReply {
    DefinitionReply {
        available: false,
        found: false,
        unit: None,
        path: None,
        line: None,
        column: None,
        how: None,
    }
}

pub fn definition_missing() -> DefinitionReply {
    DefinitionReply {
        available: true,
        found: false,
        unit: None,
        path: None,
        line: None,
        column: None,
        how: None,
    }
}

/// Every `include-book` in the snapshot, resolved to a file where possible.
///
/// LSP's `DocumentLink` in payload, minus the range: the reply says *which
/// line* and *which book name*, and the client finds the quoted name on that
/// line itself. Lines cross this wire untranslated, because a line is a line
/// in any encoding; a *column* is the one coordinate that is not, since the
/// daemon counts bytes and TypeScript counts UTF-16 units. Columns are sent
/// in bytes and converted by the client over the one line's text it already
/// holds — cheap, exact, and equal on the ASCII these forms are written in.
#[derive(Debug, Serialize)]
pub struct LinksReply {
    pub links: Vec<BookLink>,
}

#[derive(Debug, Serialize)]
pub struct BookLink {
    /// 1-based line of the `include-book` form.
    pub line: usize,
    /// 1-based byte column of its opening paren, which is what separates two
    /// includes written on one line.
    pub column: usize,
    /// The book as written, without quotes: what to look for on that line.
    pub book: String,
    /// The file it resolves to. Absent when it could not be resolved — a
    /// project's own `:dir` alias, which only ACL2 knows — or when the file
    /// is simply not there, which is worth *not* linking to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// The outline: one entry per unit, with the definers inside it as children.
///
/// LSP's `DocumentSymbol` minus its ranges. Lines only, which is all the
/// daemon deals in and all an outline needs -- the client widens them to
/// whole lines, the same line-granular choice the decorations made.
/// The client also supplies the *status*, because it already holds the paint
/// map and the daemon has no business in the painting.
#[derive(Debug, Serialize)]
pub struct SymbolsReply {
    pub symbols: Vec<BookSymbol>,
}

#[derive(Debug, Serialize)]
pub struct BookSymbol {
    pub name: String,
    /// `function`, `macro`, `theorem`, `constant`, `include`, `other`.
    pub kind: &'static str,
    /// The unit this is, for top-level entries; absent for children.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<usize>,
    pub line_start: usize,
    pub line_end: usize,
    /// The two columns, 1-based and counted in **bytes**.
    ///
    /// The one place a coordinate has to be translated (see `BookLink`), and
    /// the translation is over a single line's text, which the client holds.
    /// For the ASCII a definer head is written in they are equal anyway.
    pub col_start: usize,
    pub col_end: usize,
    /// Definers nested inside -- an `encapsulate`'s or `mutual-recursion`'s
    /// members, which a flat outline would hide entirely.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<BookSymbol>,
}

#[derive(Debug, Deserialize)]
pub struct CertifyParams {
    pub book: String,
    /// `cert.pl`'s `-j`. From the client, which owns the setting.
    pub jobs: Option<usize>,
}

/// How a certification ended.
///
/// `failed_books` is the field that earns its place: certification chases
/// dependencies, so the book that failed is usually *not* the one asked for,
/// and saying which is the difference between a useful report and "it broke".
#[derive(Debug, Serialize)]
pub struct CertifyReply {
    /// `certified` or `failed`. (`still_running` exists in the mapping so a new
    /// `CertStatus` variant is a compile error rather than a silent `failed`,
    /// but no call site can produce it: both hold the reply back and keep
    /// polling while the build is live.)
    pub status: &'static str,
    pub book: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failed_books: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_ref: Option<String>,
    pub output_lines: usize,
    pub elapsed_ms: u128,
}

/// Progress while `cert.pl` runs, at the same cadence a proof's tail uses.
#[derive(Debug, Serialize)]
pub struct CertTailEvent {
    pub book: String,
    pub elapsed_ms: u128,
    pub lines_so_far: usize,
    pub tail: String,
}

pub fn certified(r: &CertReport) -> CertifyReply {
    CertifyReply {
        // Exhaustive, like the MCP twin it is meant to mirror: the `_` arm
        // reported a *live* build as failed, and would have gone on doing so
        // for any variant added later without a word from the compiler.
        status: match r.status {
            CertStatus::Certified => "certified",
            CertStatus::Failed => "failed",
            CertStatus::StillRunning => "still_running",
        },
        book: r.book.display().to_string(),
        failed_books: r.failed_books.iter().map(|p| p.display().to_string()).collect(),
        error_line: r.error_line.clone(),
        output_ref: r.output_ref.as_ref().map(|o| o.as_str().to_string()),
        output_lines: r.output_lines,
        elapsed_ms: r.elapsed.as_millis(),
    }
}

#[derive(Debug, Deserialize)]
pub struct DocParams {
    pub book: String,
    pub name: String,
}

/// One topic's documentation, as **raw xdoc markup**.
///
/// Unrendered on purpose: the machinery that would expand it is not in the
/// world, so the client renders it. `long` keeps its exact line
/// structure, which `@({...})` blocks depend on.
#[derive(Debug, Serialize)]
pub struct DocReply {
    /// False when the session was busy and nothing was cached.
    pub available: bool,
    pub found: bool,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub long: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CompleteParams {
    pub book: String,
    /// What the user has typed so far. Filtering happens here rather than in
    /// the client because a world holds tens of thousands of names and
    /// shipping all of them per keystroke is a megabyte a keystroke.
    pub prefix: String,
}

#[derive(Debug, Serialize)]
pub struct CompleteReply {
    pub items: Vec<CompletionItem>,
    /// The list was capped, or the census is not built yet. LSP's
    /// `isIncomplete`: the client re-asks as the user narrows, instead of
    /// filtering a truncated set and concluding a name does not exist.
    pub incomplete: bool,
}

#[derive(Debug, Serialize)]
pub struct CompletionItem {
    pub name: String,
    /// `function`, `macro`, `theorem`, `constant`, `keyword`, `other`.
    pub kind: &'static str,
    /// The book it comes from, for the detail line. Absent for this file's
    /// own names, ACL2's own, and the static keyword families.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub book: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct InductionParams {
    pub book: String,
    /// A handle from a `unit` event or an `eval` reply.
    pub output_ref: String,
}

/// One induction ACL2 performed, from its own records.
///
/// Same class of fact as the rest of this wire, which the old version of this
/// type was not: it came out of ACL2's prose because the scheme was printed
/// and never recorded, and every field was optional so that a reworded ACL2
/// dropped one instead of inventing it. These are probed out of the event
/// data like everything else here, so the fields are simply present.
#[derive(Debug, Serialize)]
pub struct InductionWire {
    /// The pool name: `*1`, `*1.1`. Unique only within its event, which is
    /// why these arrive grouped by event rather than flat.
    pub goal: String,
    /// The term whose recursion suggested the scheme, laid out for the pane.
    pub suggested: String,
    /// Terms merged into the scheme alongside `suggested`. Usually empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub accommodating: Vec<String>,
    /// The `:induction` runes behind the suggestion. Structural, where the
    /// old field could only be ACL2's English.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub runes: Vec<String>,
    /// What `:p` stands for in `scheme`.
    pub p_formula: String,
    /// The scheme, with `:p` for the conjecture -- or the head of it, when
    /// `scheme_chars` says there was more.
    pub scheme: String,
    /// How long the scheme really is, in characters, when what is above is
    /// only its head. Absent when the whole thing is there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheme_chars: Option<usize>,
    /// Present when `scheme` is the first case of a scheme that had this many
    /// -- a bound ACL2 was asked for, before any of this crate's. Absent when
    /// every case is here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheme_cases: Option<usize>,
}

/// One event's inductions.
#[derive(Debug, Serialize)]
pub struct EventInductionWire {
    /// Empty for a `thm`, which introduces nothing to name.
    pub event: String,
    pub inductions: Vec<InductionWire>,
}

#[derive(Debug, Serialize)]
pub struct InductionReply {
    /// Events that inducted, newest first. Empty is the ordinary case.
    pub events: Vec<EventInductionWire>,
    /// False when the image cannot report inductions at all, so a panel can
    /// say that instead of showing an empty section that looks like a fact
    /// about this form.
    pub supported: bool,
}

/// How much of an induction scheme crosses the wire.
///
/// The scheme is the one field here with no bound of its own. ACL2 prints it
/// with the gag-mode evisc tuple, which is `nil` under this crate's settings,
/// so nothing is abbreviated -- and for a mutual recursion the scheme *is*
/// the clique. Measured on `books/kestrel/c/syntax/validator.lisp`: the
/// `defines` at line 2775 produced three schemes of 6.83 MB each, 20.9 MB in
/// one reply, against 1.6 MB of real events for the whole book. What that did
/// to the editor is recorded in `editors/vscode/src/policy.ts`.
///
/// So the head of it crosses and the rest stays where every other long piece
/// of ACL2's output already lives -- behind the output handle, one `output`
/// request away. Generous on purpose: a scheme a person can actually read
/// arrives whole, and this is a backstop against the ones nobody can.
///
/// Chosen against the client's own limit rather than freely. The panel lays a
/// scheme out at the pane's width *before* coloring it, and laying out grows
/// the text -- 6.83 MB became 15.3 MB, about 2.2x. What the panel colors is
/// therefore what this sends, expanded; if that lands over `MAX_TERM` in
/// `editors/vscode/src/policy.ts` the scheme arrives cut *and* uncolored,
/// which is the one combination nobody asked for. This leaves room for the
/// expansion, and `webview.mjs` holds the two numbers to it.
///
/// The probe's own ceiling sits above this one and is measured in `acl2-count`
/// rather than characters, so a clique scheme is usually cut twice: once to
/// its first case, once to this length. `scheme_cases` and `scheme_chars` say
/// which happened, and they are independent.
const SCHEME_LIMIT: usize = 16 * 1024;

/// The head of a scheme, and its full length when anything was cut.
///
/// Cut on a line boundary because the scheme's indentation *is* its
/// structure: stopping mid-line would leave a term's tail sitting at a column
/// that means something else. Falling back to a character boundary covers the
/// scheme that is one very long line, which must still be cut somewhere and
/// must not be cut through a character.
///
/// Cut *after* laying out, not before, because the layout is what the panel
/// colors and therefore what has to fit its limit -- cutting the unlaid term
/// would bound the wrong string.
fn cut_scheme(s: &str) -> (String, Option<usize>) {
    if s.len() <= SCHEME_LIMIT {
        return (s.to_string(), None);
    }
    let mut cut = SCHEME_LIMIT;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let head = match s[..cut].rfind('\n') {
        Some(nl) => &s[..nl],
        None => &s[..cut],
    };
    (head.to_string(), Some(s.chars().count()))
}

pub fn induction(i: &acl2_session::Induction) -> InductionWire {
    let (scheme, scheme_chars) = cut_scheme(&laid(&i.scheme));
    InductionWire {
        goal: i.goal.clone(),
        suggested: laid(&i.suggested),
        accommodating: i.accommodating.iter().map(|t| laid(t)).collect(),
        runes: i.runes.clone(),
        p_formula: laid(&i.p_formula),
        scheme,
        scheme_chars,
        scheme_cases: i.scheme_cases,
    }
}

pub fn inductions(events: &[acl2_session::EventInductions], supported: bool) -> InductionReply {
    InductionReply {
        events: events
            .iter()
            .map(|e| EventInductionWire {
                event: e.event.clone(),
                inductions: e.records.iter().map(induction).collect(),
            })
            .collect(),
        supported,
    }
}

#[derive(Debug, Deserialize)]
pub struct OutputParams {
    pub book: String,
    /// A handle from a `unit` event. Never construct one.
    pub output_ref: String,
    /// `all`, `head`, `tail` (the default), `range`, `goal`, or `find`.
    pub mode: Option<String>,
    pub lines: Option<usize>,
    pub from_line: Option<usize>,
    pub to_line: Option<usize>,
    /// The goal name for `goal`, or the literal needle for `find`.
    pub text: Option<String>,
    pub context: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct OutputReply {
    pub text: String,
    pub total_lines: usize,
    pub superseded: bool,
}

#[derive(Debug, Serialize)]
pub struct StatusReply {
    pub running: Option<RunningReply>,
}

#[derive(Debug, Serialize)]
pub struct RunningReply {
    pub unit: Option<usize>,
    pub elapsed_ms: u128,
    pub bytes_so_far: usize,
    pub tail: String,
}

#[derive(Debug, Serialize)]
pub struct ImagesReply {
    pub images: Vec<String>,
    pub default: Option<String>,
}

/// What an eval'd form did.
///
/// The failure fields are `UnitEvent`'s, flattened the same way and meaning
/// the same things, so one renderer serves a red form and a red experiment
/// alike; the file coordinates are the only ones missing, because an eval has
/// none. `world_change` is the fact only an eval has.
#[derive(Debug, Serialize)]
pub struct EvalReply {
    /// `ok` or `failed`. Never `still_running`: the driver follows an eval to
    /// completion before it answers.
    pub outcome: &'static str,
    /// `none` -- the form introduced no events; `undone` -- it did and they
    /// were rolled back, which is the contract working; `dirty` -- the
    /// rollback did not verify, so the next sync rebuilds by replay.
    pub world_change: &'static str,
    pub elapsed_ms: u128,
    pub output_ref: String,
    pub output_lines: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort_cause: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prover_steps: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    /// What the form evaluated to. See `UnitEvent::value`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub value_triple: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hint_events: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub splitter_rules: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<CheckpointEvent>,
    #[serde(skip_serializing_if = "is_false")]
    pub pre_revert: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub forced: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inner: Vec<InnerEventWire>,
    pub inner_total: usize,
    pub n_top: usize,
    pub n_sub: usize,
}

/// Where a queued eval stands.
///
/// Sent when an eval is accepted and again when it actually starts. Without
/// it, an eval submitted behind a four-minute proof looks identical to one
/// the daemon dropped: the reply is the only other thing the client hears,
/// and it does not come until the form has run.
#[derive(Debug, Serialize)]
pub struct EvalStateEvent {
    pub book: String,
    /// `queued` -- the session is busy; this goes when the form in flight
    /// finishes. `running` -- it is the form in flight.
    pub state: &'static str,
    /// Evals ahead of this one in the queue.
    pub ahead: usize,
}

// ---- Events, daemon to client ----------------------------------------------

#[derive(Debug, Serialize)]
pub struct StateEvent {
    pub book: String,
    pub revision: u64,
    pub conditional_from: Option<usize>,
    pub running: Option<usize>,
    pub units: Vec<UnitState>,
}

#[derive(Debug, Serialize)]
pub struct UnitState {
    pub index: usize,
    pub line_start: usize,
    pub line_end: usize,
    /// The two columns, 1-based and in bytes: `col_end` is one past the
    /// closing paren, so the pair reads as a range. See `BookSymbol`.
    pub col_start: usize,
    pub col_end: usize,
    /// `pending`, `beyond`, `admitted`, `conditional`, `skipped`, `failed`,
    /// `running`. `beyond` is past an explicit target: unchecked, and not
    /// queued to be, until the target moves.
    pub status: &'static str,
}

#[derive(Debug, Serialize)]
pub struct UnitEvent {
    pub book: String,
    pub revision: u64,
    pub index: usize,
    /// `admitted`, `skipped`, or `failed`.
    pub outcome: &'static str,
    pub line_start: usize,
    pub line_end: usize,
    pub col_start: usize,
    pub col_end: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u128>,
    pub output_ref: String,
    pub output_lines: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_output_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort_cause: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prover_steps: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<String>,
    /// `(prove print proof-tree other)`, in seconds, as ACL2 prints it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    /// What the form evaluated to, as ACL2 printed it.
    ///
    /// Absent when there is nothing to show, which is ACL2's own judgement
    /// rather than ours: a triple whose error component is non-nil or whose
    /// value is `:invisible` prints nothing at the REPL and yields nothing
    /// here. Also absent when the value only repeats a name the form
    /// introduced, since that name is already the event's own heading.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Whether that value came back as an error triple rather than a single
    /// value. Carried because it is free and because ACL2 marks the
    /// distinction itself; no client is obliged to show it.
    #[serde(skip_serializing_if = "is_false")]
    pub value_triple: bool,
    /// Hints that fired -- "did my hint do anything" without the log.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hint_events: Vec<String>,
    /// Rules that caused case splits -- "why did this explode".
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub splitter_rules: Vec<String>,
    /// For failures: rules the *attempt* used before it failed; for admitted
    /// units, the proof's runes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<CheckpointEvent>,
    /// The top checkpoints are from before the proof reverted to induction.
    #[serde(skip_serializing_if = "is_false")]
    pub pre_revert: bool,
    /// Forced assumptions were present.
    #[serde(skip_serializing_if = "is_false")]
    pub forced: bool,
    /// The names this form *writes*, from the split.
    ///
    /// Not the same question as `inner`, which is what ACL2 recorded. A
    /// standalone `defxdoc` records no event at all -- it writes a table --
    /// so the only place its topic's name appears is here. Without it a form
    /// whose whole purpose is documentation looks empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    /// The rest of any recursion clique this unit's events belong to.
    ///
    /// ACL2 records a whole clique as *one* event, keyed under the first
    /// definition in it -- `event-data-name` picks that deliberately -- so
    /// the other members appear in no `inner` entry. Asking the world for
    /// `recursivep` is the only way to learn they exist, and without them a
    /// `mutual-recursion` reads as though it defined one function and wrote
    /// only that one's rules.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub clique: Vec<String>,
    /// Events inside a compound unit (encapsulate, progn, defines ...).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inner: Vec<InnerEventWire>,
    pub inner_total: usize,
    /// How many top/sub checkpoints the prover pushed. The probe reads a
    /// fixed number of slots, so `checkpoints` may hold fewer; these counts
    /// keep a truncation from reading as the whole story.
    pub n_top: usize,
    pub n_sub: usize,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Serialize)]
pub struct InnerEventWire {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// What a `thm` set out to prove, laid out. Only a `thm` carries one:
    /// every other event writes a name, and what it proved is in the world
    /// under that name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prover_steps: Option<i64>,
    /// Runes this inner event used. Present only within the core's detail
    /// slots (`INNER_DETAIL_SLOTS`), like the rest of the per-event detail.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
    /// Hints this event's proof appealed to -- `(:use l)`, `(:by t)`,
    /// `(:clause-processor f)`. Per event because the outer event of a
    /// compound form reports none, which is most real code.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hint_events: Vec<String>,
    /// Rules that split this event's proof into cases. Per event for the same
    /// reason as `hint_events`, and empty on the outer event for the same one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub splitter_rules: Vec<String>,
    /// Non-empty when this inner event is the one that failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort_causes: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CheckpointEvent {
    /// `top` or `sub`.
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    pub term: String,
}

#[derive(Debug, Serialize)]
pub struct TailEvent {
    pub book: String,
    pub unit: Option<usize>,
    pub elapsed_ms: u128,
    pub text: String,
    /// The handle this form's log will be filed under, once it has one.
    ///
    /// Present so a client can ask for more than the tail carried here: the
    /// handle is readable *while the form runs*, so a view wanting four
    /// hundred lines requests them rather than waiting for the form to land.
    /// Absent for the first tick or so, before the form has outrun a deadline
    /// and been given a handle at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_ref: Option<String>,
}

/// Books the world rests on whose sources have changed underneath it.
///
/// Sent when the daemon notices, so the client can say *why* a frontier is
/// suspect rather than leaving a green gutter asserting something that is no
/// longer true. Reported, not acted on -- the world is still exactly what
/// these forms admitted.
#[derive(Debug, Serialize)]
pub struct StaleDepsEvent {
    pub book: String,
    pub revision: u64,
    pub stale: Vec<StaleDep>,
}

#[derive(Debug, Serialize)]
pub struct StaleDep {
    /// The book, as ACL2 names it.
    pub name: String,
    pub path: String,
    /// The unit of *this* file that included it, when one did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<usize>,
    /// Gone rather than merely different.
    #[serde(skip_serializing_if = "is_false")]
    pub missing: bool,
}

pub fn stale_deps(book: &str, revision: u64, stale: &[acl2_session::StaleInclude])
    -> StaleDepsEvent
{
    StaleDepsEvent {
        book: book.to_string(),
        revision,
        stale: stale
            .iter()
            .map(|s| StaleDep {
                name: s.book.clone(),
                path: s.path.display().to_string(),
                unit: s.unit,
                missing: s.missing,
            })
            .collect(),
    }
}

#[derive(Debug, Deserialize)]
pub struct RulesParams {
    pub book: String,
    /// The names to explain, as the client read them out of the buffer or out
    /// of a unit's events.
    ///
    /// A list rather than one name because the caller almost never wants one:
    /// the form view explains everything a form introduced, and macro-generated
    /// code records far more than the buffer shows. Asked one at a time, a
    /// `defines` of 620 recorded events became 620 requests fired at once,
    /// 620 commands queued to the driver thread that also runs proofs, and
    /// 620 replies onto a socket the client was too busy to drain. One
    /// request, one book lock, one reply.
    pub names: Vec<String>,
}

/// One rune the name introduced.
#[derive(Debug, Serialize)]
pub struct RuneWire {
    /// `(:rewrite foo)`, as ACL2 writes it.
    pub rune: String,
    /// `:rewrite`, `:forward-chaining`, `:definition`, …
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    /// Whether it is enabled in the session's current theory. Absent for a
    /// rune ACL2 records no rule for -- `:executable-counterpart`,
    /// `:induction` -- which are listed because a reader asking what a form
    /// introduced wants them, though `:pr` shows neither.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// The rule's own fields in ACL2's order: `:lhs`, `:hyps`, `:subclass`,
    /// `:trigger`, `:corollary`, and whatever else its class carries. Kept
    /// as a list rather than a struct because the set differs per class.
    pub fields: Vec<RuneField>,
}

#[derive(Debug, Serialize)]
pub struct RuneField {
    pub key: String,
    pub value: String,
}

/// What a form actually introduced, beyond what its text says.
///
/// Answered from the scrape taken when the form was admitted, so it needs no
/// prover and arrives during a proof -- which is when a reader is most
/// likely to be looking at an earlier form. `found: false` means this
/// session's file never introduced the name; there is no "ask again" here,
/// because nothing is ever waited for.
#[derive(Debug, Serialize)]
pub struct RulesBatchReply {
    pub rules: Vec<RulesReply>,
}

#[derive(Debug, Serialize)]
pub struct RulesReply {
    /// This name's xdoc `:short`, when a topic of this name exists.
    ///
    /// Independent of `found`: a name can be documented without introducing a
    /// rule -- a standalone `defxdoc` is exactly that -- so a reader must not
    /// take one as a precondition for the other.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short: Option<String>,
    /// This name's xdoc body, when it has one -- raw markup, like `short`.
    ///
    /// Sent only where `short` is, since a topic is what carries both, and
    /// absent when the topic has a summary and nothing else.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub long: Option<String>,
    pub found: bool,
    pub name: String,
    /// `:common-lisp-compliant` (guards verified), `:ideal` (logic mode,
    /// guards not verified), or `:program`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_class: Option<String>,
    /// The measure term, for a recursive function.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measure: Option<String>,
    /// The well-founded relation it decreases under, when it is not `o<`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formals: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guard: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statement: Option<String>,
    pub runes: Vec<RuneWire>,
}

pub fn rules_reply(name: &str, r: Option<&acl2_session::NameRules>) -> RulesReply {
    match r {
        None => RulesReply {
            long: None,
            found: false,
            name: name.to_string(),
            symbol_class: None,
            measure: None,
            relation: None,
            formals: None,
            guard: None,
            statement: None,
            runes: Vec::new(),
            short: None,
        },
        Some(r) => RulesReply {
            long: None,
            found: true,
            name: r.name.clone(),
            symbol_class: r.symbol_class.clone(),
            // The terms are laid out; `relation` and `formals` are not, being
            // a symbol and a short list that ACL2 never wraps anyway.
            measure: laid_opt(&r.measure),
            relation: r.relation.clone(),
            formals: r.formals.clone(),
            guard: laid_opt(&r.guard),
            statement: laid_opt(&r.statement),
            runes: r
                .runes
                .iter()
                .map(|u| RuneWire {
                    rune: u.rune.clone(),
                    class: u.class().map(str::to_string),
                    enabled: u.enabled,
                    fields: u
                        .fields
                        .iter()
                        .map(|(k, v)| RuneField { key: k.clone(), value: laid(v) })
                        .collect(),
                })
                .collect(),
            short: None,
        },
    }
}

#[derive(Debug, Serialize)]
pub struct SettledEvent {
    pub book: String,
    pub revision: u64,
    /// `reached_target`, `failed`, `interrupted`, `split_error`, `error`.
    pub reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Where in the buffer the trouble starts, 1-based. Only `split_error`
    /// has one: the line of the `(` that is never closed, which is where the
    /// file stops being readable and every line after it is in doubt.
    ///
    /// A field rather than something to read back out of `message`, because
    /// the client paints with it. Prose is for people; this wire carries
    /// positions as numbers everywhere else and should here too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

// ---- Builders ---------------------------------------------------------------

pub fn status_str(s: Status) -> &'static str {
    match s {
        Status::ProofFailure => "proof_failure",
        Status::Rejected => "rejected",
        Status::DeadlineExceeded => "deadline_exceeded",
    }
}

/// The editor paints warnings as one line, so the kinds ACL2 recorded are
/// joined here rather than in every renderer. `None` means none were
/// emitted. Kinds only -- see the core's `EventData::warnings` on why there
/// are no counts.
fn join_warnings(kinds: &[String]) -> Option<String> {
    (!kinds.is_empty()).then(|| kinds.join(", "))
}

fn inner_wire(inner: &[acl2_session::InnerEvent]) -> Vec<InnerEventWire> {
    inner
        .iter()
        .map(|e| InnerEventWire {
            name: e.name.clone(),
            kind: e.kind.clone(),
            goal: e.goal.as_deref().map(laid),
            time: e.time.clone(),
            prover_steps: e.prover_steps,
            rules: e.rules.clone(),
            hint_events: e.hint_events.clone(),
            splitter_rules: e.splitter_rules.clone(),
            abort_causes: e.abort_causes.clone(),
        })
        .collect()
}

pub fn unit_admitted(book: &str, revision: u64, u: &UnitReport) -> UnitEvent {
    UnitEvent {
        book: book.to_string(),
        revision,
        index: u.index,
        outcome: "admitted",
        line_start: u.line_start,
        line_end: u.line_end,
        col_start: u.col_start,
        col_end: u.col_end,
        elapsed_ms: Some(u.elapsed.as_millis()),
        output_ref: u.output_ref.as_str().to_string(),
        output_lines: u.output_lines,
        retry_output_ref: None,
        status: None,
        error_line: None,
        abort_cause: None,
        prover_steps: u.event_data.prover_steps,
        warnings: join_warnings(&u.event_data.warnings),
        time: u.event_data.time.clone(),
        value: u.value.as_ref().map(|v| laid(&v.text)),
        value_triple: u.value.as_ref().is_some_and(|v| v.triple),
        hint_events: u.event_data.hint_events.clone(),
        splitter_rules: u.event_data.splitter_rules.clone(),
        rules: u.event_data.rules.clone(),
        checkpoints: Vec::new(),
        pre_revert: false,
        forced: false,
        names: u.names.clone(),
        clique: u.clique.clone(),
        inner: inner_wire(&u.inner),
        inner_total: u.inner_total,
        n_top: 0,
        n_sub: 0,
    }
}

/// Top checkpoints first, then sub -- the order a reader wants them, and the
/// order both envelopes render them in.
fn checkpoint_wire(c: &Checkpoints) -> Vec<CheckpointEvent> {
    let mut out = Vec::with_capacity(c.top.len() + c.sub.len());
    for k in &c.top {
        out.push(CheckpointEvent { kind: "top", goal: k.goal.clone(), term: laid(&k.term) });
    }
    for k in &c.sub {
        out.push(CheckpointEvent { kind: "sub", goal: k.goal.clone(), term: laid(&k.term) });
    }
    out
}

pub fn unit_failure(
    book: &str,
    revision: u64,
    f: &FailureReport,
    outcome: &'static str,
) -> UnitEvent {
    let checkpoints = checkpoint_wire(&f.checkpoints);
    UnitEvent {
        // A failure reports no clique: the form did not get in, so the world
        // has no recursivep to ask about.
        clique: Vec::new(),
        book: book.to_string(),
        revision,
        index: f.index,
        outcome,
        line_start: f.line_start,
        line_end: f.line_end,
        col_start: f.col_start,
        col_end: f.col_end,
        elapsed_ms: None,
        output_ref: f.output_ref.as_str().to_string(),
        output_lines: f.output_lines,
        retry_output_ref: f.retry_output_ref.as_ref().map(|r| r.as_str().to_string()),
        status: Some(status_str(f.status)),
        error_line: f.error_line.clone(),
        abort_cause: f.checkpoints.abort_cause.clone(),
        prover_steps: f.event_data.prover_steps,
        warnings: join_warnings(&f.event_data.warnings),
        time: f.event_data.time.clone(),
        // A form that signalled an error halted the `ld` before the value
        // probe could run, so there is nothing to carry here and never will
        // be. See `VALUE_PROBE`.
        value: None,
        value_triple: false,
        hint_events: f.event_data.hint_events.clone(),
        splitter_rules: f.event_data.splitter_rules.clone(),
        rules: f.event_data.rules.clone(),
        checkpoints,
        pre_revert: f.checkpoints.pre_revert,
        forced: f.checkpoints.forced,
        names: Vec::new(),
        inner: inner_wire(&f.inner),
        inner_total: f.inner_total,
        n_top: f.checkpoints.n_top,
        n_sub: f.checkpoints.n_sub,
    }
}

pub fn evaluated(r: &EvalResult) -> EvalReply {
    // An eval carries no `EventData` of its own when it succeeded: the
    // envelope the core builds is the failure envelope, and a form that ran
    // clean has nothing to explain. Everything below the outcome therefore
    // comes from `failed`, or is empty.
    let f = r.failed.as_ref();
    EvalReply {
        outcome: match r.outcome {
            Outcome::Ok => "ok",
            Outcome::Failed => "failed",
            Outcome::StillRunning => "still_running",
        },
        world_change: match r.world_change {
            WorldChange::None => "none",
            WorldChange::Undone => "undone",
            WorldChange::Dirty => "dirty",
        },
        elapsed_ms: r.elapsed.as_millis(),
        output_ref: r.output_ref.as_str().to_string(),
        output_lines: r.output_lines,
        status: f.map(|f| status_str(f.status)),
        error_line: f.and_then(|f| f.error_line.clone()),
        abort_cause: f.and_then(|f| f.checkpoints.abort_cause.clone()),
        prover_steps: f.and_then(|f| f.event_data.prover_steps),
        warnings: f.and_then(|f| join_warnings(&f.event_data.warnings)),
        time: f.and_then(|f| f.event_data.time.clone()),
        value: r.value.as_ref().map(|v| laid(&v.text)),
        value_triple: r.value.as_ref().is_some_and(|v| v.triple),
        hint_events: f.map(|f| f.event_data.hint_events.clone()).unwrap_or_default(),
        splitter_rules: f.map(|f| f.event_data.splitter_rules.clone()).unwrap_or_default(),
        rules: f.map(|f| f.event_data.rules.clone()).unwrap_or_default(),
        checkpoints: f.map(|f| checkpoint_wire(&f.checkpoints)).unwrap_or_default(),
        pre_revert: f.is_some_and(|f| f.checkpoints.pre_revert),
        forced: f.is_some_and(|f| f.checkpoints.forced),
        inner: f.map(|f| inner_wire(&f.inner)).unwrap_or_default(),
        inner_total: f.map_or(0, |f| f.inner_total),
        n_top: f.map_or(0, |f| f.checkpoints.n_top),
        n_sub: f.map_or(0, |f| f.checkpoints.n_sub),
    }
}

/// Fifty lines by default, where the MCP surface takes forty: an editor
/// renders into a pane and pays nothing per line. Everything else about the
/// reading is `SliceRequest`'s, so the two surfaces cannot drift again on what
/// a `range` with no bounds or a `goal` with no name means.
pub fn slice_from(p: &OutputParams) -> Result<Slice, String> {
    acl2_session::SliceRequest {
        mode: p.mode.as_deref(),
        lines: p.lines,
        from_line: p.from_line,
        to_line: p.to_line,
        text: p.text.as_deref(),
        context: p.context,
    }
    .resolve(50)
}
