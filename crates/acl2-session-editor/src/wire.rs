//! Wire shapes for the editor protocol.
//!
//! Owned here deliberately: the core stays serde-free, and these shapes
//! are deliberately richer than the MCP crate's -- an editor renders; it does
//! not pay per token.

use serde::{Deserialize, Serialize};

use acl2_session::sync::{Checkpoints, EvalResult, FailureReport, UnitReport};
use acl2_session::{NameDetail, NameKind, Outcome, Slice, Status, WorldChange};

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
}

#[derive(Debug, Deserialize)]
pub struct BookParams {
    pub book: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateParams {
    pub book: String,
    /// The full snapshot. The daemon syncs this string's UTF-8 bytes.
    pub text: String,
    /// Auto-admit only through this 1-based line. Omitted means the whole
    /// book; 0 holds before the first unit.
    pub target_line: Option<usize>,
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

pub fn described(name: &str, d: Option<&NameDetail>) -> DescribeReply {
    match d {
        None => DescribeReply {
            available: true,
            found: false,
            name: name.to_string(),
            kind: None,
            formals: None,
            guard: None,
            statement: None,
            book: None,
            path: None,
        },
        Some(d) => DescribeReply {
            available: true,
            found: true,
            name: d.name.clone(),
            kind: Some(kind_str(d.kind)),
            formals: d.formals.clone(),
            guard: d.guard.clone(),
            statement: d.statement.clone(),
            book: d.book.clone(),
            path: d.book_path.as_ref().map(|p| p.display().to_string()),
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
        formals: None,
        guard: None,
        statement: None,
        book: None,
        path: None,
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
    /// `pending`, `admitted`, `conditional`, `skipped`, `failed`, `running`.
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prover_steps: Option<i64>,
    /// Runes this inner event used. Present only within the core's detail
    /// slots (`INNER_DETAIL_SLOTS`), like the rest of the per-event detail.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
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

#[derive(Debug, Serialize)]
pub struct SettledEvent {
    pub book: String,
    pub revision: u64,
    /// `reached_target`, `failed`, `interrupted`, `split_error`, `error`.
    pub reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
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
            time: e.time.clone(),
            prover_steps: e.prover_steps,
            rules: e.rules.clone(),
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
        hint_events: u.event_data.hint_events.clone(),
        splitter_rules: u.event_data.splitter_rules.clone(),
        rules: u.event_data.rules.clone(),
        checkpoints: Vec::new(),
        pre_revert: false,
        forced: false,
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
        out.push(CheckpointEvent { kind: "top", goal: k.goal.clone(), term: k.term.clone() });
    }
    for k in &c.sub {
        out.push(CheckpointEvent { kind: "sub", goal: k.goal.clone(), term: k.term.clone() });
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
        book: book.to_string(),
        revision,
        index: f.index,
        outcome,
        line_start: f.line_start,
        line_end: f.line_end,
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
        hint_events: f.event_data.hint_events.clone(),
        splitter_rules: f.event_data.splitter_rules.clone(),
        rules: f.event_data.rules.clone(),
        checkpoints,
        pre_revert: f.checkpoints.pre_revert,
        forced: f.checkpoints.forced,
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

pub fn slice_from(p: &OutputParams) -> Result<Slice, String> {
    let lines = p.lines.unwrap_or(50);
    match p.mode.as_deref().unwrap_or("tail") {
        "all" => Ok(Slice::All),
        "head" => Ok(Slice::Head(lines)),
        "tail" => Ok(Slice::Tail(lines)),
        "range" => match (p.from_line, p.to_line) {
            (Some(a), Some(b)) => Ok(Slice::Range(a, b)),
            _ => Err("range needs from_line and to_line".into()),
        },
        "goal" => match &p.text {
            Some(t) => Ok(Slice::Goal { name: t.clone() }),
            None => Err("goal needs text".into()),
        },
        "find" => match &p.text {
            Some(t) => Ok(Slice::Find {
                needle: t.clone(),
                context: p.context.unwrap_or(2),
                ignore_case: true,
            }),
            None => Err("find needs text".into()),
        },
        m => Err(format!("unknown output mode {m:?}")),
    }
}
