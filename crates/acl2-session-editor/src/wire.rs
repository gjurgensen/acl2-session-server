//! Wire shapes for the editor protocol.
//!
//! Owned here deliberately: the core stays serde-free, and these shapes
//! are deliberately richer than the MCP crate's -- an editor renders; it does
//! not pay per token.

use serde::{Deserialize, Serialize};

use acl2_session::sync::{FailureReport, UnitReport};
use acl2_session::{Slice, Status};

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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<CheckpointEvent>,
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
        rules: u.event_data.rules.clone(),
        checkpoints: Vec::new(),
    }
}

pub fn unit_failure(
    book: &str,
    revision: u64,
    f: &FailureReport,
    outcome: &'static str,
) -> UnitEvent {
    let mut checkpoints = Vec::new();
    for c in &f.checkpoints.top {
        checkpoints.push(CheckpointEvent { kind: "top", goal: c.goal.clone(), term: c.term.clone() });
    }
    for c in &f.checkpoints.sub {
        checkpoints.push(CheckpointEvent { kind: "sub", goal: c.goal.clone(), term: c.term.clone() });
    }
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
        rules: Vec::new(),
        checkpoints,
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
