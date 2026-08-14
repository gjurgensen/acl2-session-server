//! The JSON shapes an agent sees.
//!
//! These are defined here rather than derived from the core's types, and the
//! duplication is deliberate. This is the product surface: it should be free to
//! evolve without dragging internal representation along, and the core should
//! not acquire a serialisation dependency to serve one interface.
//!
//! The doc comments matter as much as the fields. `schemars` turns them into
//! JSON Schema descriptions, so this is where an agent is told what a value
//! means -- above all, that the structured result is a *projection* and that
//! the log behind `output_ref` holds things no field can carry.

use serde::Serialize;

use acl2_session as core;

/// One event ACL2 recorded inside a form.
///
/// Most real code is compound -- `define`, `defrule`, `encapsulate` -- and ACL2
/// reports almost nothing about the outer form: no rules, no abort cause. These
/// fill that in per inner event, and are how you tell a failing prep-lemma from
/// a failing main theorem.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct InnerEvent {
    pub name: String,
    /// Present only on the inner event that failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort_causes: Option<String>,
    /// Runes this inner event used. Answers questions like "did the
    /// prep-lemma actually get used?", which the outer form cannot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<String>,
}

impl From<&core::InnerEvent> for InnerEvent {
    fn from(e: &core::InnerEvent) -> Self {
        InnerEvent {
            name: e.name.clone(),
            abort_causes: e.abort_causes.clone(),
            rules: e.rules.clone(),
        }
    }
}

/// Key checkpoints of a failed proof, as ACL2 would print them.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Checkpoints {
    /// Where the top-level proof stalled.
    pub top: Vec<String>,
    /// Goals under a top-level induction. Often where the actual defect is,
    /// which is why these are kept separate from `top` rather than merged.
    pub sub: Vec<String>,
    /// How many the prover pushed. May exceed the number listed: a fixed
    /// number of slots is read, and this says when there were more.
    pub n_top: usize,
    pub n_sub: usize,
    /// Forcing took place, so the checkpoints are probably not the whole story.
    pub forced: bool,
}

/// A form that was admitted.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Admitted {
    pub index: usize,
    pub line_start: usize,
    pub line_end: usize,
    pub elapsed_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prover_steps: Option<u64>,
    /// Runes the proof used.
    ///
    /// Absent for compound forms such as `define` and `defrule`: ACL2 does not
    /// aggregate rules for the outer event. Look at `inner` instead, which
    /// reports them per inner event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<String>,
    pub inner: Vec<InnerEvent>,
    /// Handle to everything ACL2 printed for this form. Pass to `acl2_output`.
    pub output_ref: String,
    /// Lines behind that handle, so you can tell how much you are not seeing.
    pub output_lines: usize,
}

/// A form that did not go in.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Failed {
    pub index: usize,
    pub line_start: usize,
    pub line_end: usize,
    /// `proof_failure` -- a proof ran and did not finish; checkpoints and an
    /// abort cause are available.
    /// `rejected` -- ACL2 declined the form before any proof: a translate
    /// error, a redefinition, a guard violation, a failed `include-book`. No
    /// checkpoints exist for it.
    /// `deadline_exceeded` -- your deadline expired; ACL2 is still working.
    /// Call `acl2_resume` to wait longer, or `acl2_interrupt` to stop it.
    ///
    /// The first two call for completely different responses, which is why
    /// they are distinguished rather than both reported as "failed".
    pub status: String,
    /// Why the prover gave up. `empty-clause` means the conjecture is **false**
    /// rather than merely unproven -- stop trying hints and fix the statement.
    /// `induction-depth-limit-exceeded` means it ran out of depth.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort_cause: Option<String>,
    /// The first `ACL2 Error` line, verbatim. Matters most for `rejected`,
    /// where no proof ran and the other fields are empty. For a compound form
    /// it also names the inner event that failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prover_steps: Option<u64>,
    pub checkpoints: Checkpoints,
    pub inner: Vec<InnerEvent>,
    /// The inner event that failed, when the form was compound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_event: Option<String>,
    pub output_ref: String,
    pub output_lines: usize,
}

/// The result of bringing a session into agreement with its file.
///
/// **The structured fields are a projection, not the whole record.** The
/// induction scheme the prover chose, why it chose that one, per-goal rewriting
/// detail, and the output of arbitrary macros exist only in the printed
/// narrative and are unavailable as data. When a failure is not explained by
/// the fields here, fetch the log with `acl2_output` rather than concluding the
/// information does not exist.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SyncReply {
    /// Index of the last form the target resolved to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_through: Option<usize>,
    /// Forms left untouched because the file had not changed under them.
    pub reused: usize,
    /// Forms undone because the file had changed under them.
    pub rolled_back: usize,
    /// The prefix had to be restored by discarding the ACL2 process and
    /// replaying, rather than by undo. Rare, and expensive: it explains a sync
    /// that took far longer than the proofs alone would suggest.
    pub rebuilt: bool,
    pub admitted: Vec<Admitted>,
    /// Sync stops at the first failure, since later forms usually depend on it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<Failed>,
}

impl From<core::SyncOutcome> for SyncReply {
    fn from(o: core::SyncOutcome) -> Self {
        SyncReply {
            resolved_through: o.resolved_through,
            reused: o.reused,
            rolled_back: o.rolled_back,
            rebuilt: o.rebuilt,
            admitted: o.admitted.iter().map(admitted).collect(),
            failed: o.failed.as_ref().map(failed),
        }
    }
}

pub fn admitted(u: &core::sync::UnitReport) -> Admitted {
    Admitted {
        index: u.index,
        line_start: u.line_start,
        line_end: u.line_end,
        elapsed_ms: u.elapsed.as_millis(),
        prover_steps: u.event_data.prover_steps,
        rules: u.event_data.rules.clone(),
        warnings: u.event_data.warnings.clone(),
        inner: u.inner.iter().map(InnerEvent::from).collect(),
        output_ref: u.output_ref.to_string(),
        output_lines: u.output_lines,
    }
}

pub fn failed(f: &core::sync::FailureReport) -> Failed {
    Failed {
        index: f.index,
        line_start: f.line_start,
        line_end: f.line_end,
        status: match f.status {
            core::Status::ProofFailure => "proof_failure",
            core::Status::Rejected => "rejected",
            core::Status::DeadlineExceeded => "deadline_exceeded",
        }
        .to_string(),
        abort_cause: f.checkpoints.abort_cause.clone(),
        error_line: f.error_line.clone(),
        prover_steps: f.event_data.prover_steps,
        checkpoints: Checkpoints {
            top: f.checkpoints.top.clone(),
            sub: f.checkpoints.sub.clone(),
            n_top: f.checkpoints.n_top,
            n_sub: f.checkpoints.n_sub,
            forced: f.checkpoints.forced,
        },
        inner: f.inner.iter().map(InnerEvent::from).collect(),
        failed_event: f.failed_event().map(|e| e.name.clone()),
        output_ref: f.output_ref.to_string(),
        output_lines: f.output_lines,
    }
}

/// The result of evaluating a one-off form.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EvalReply {
    /// `ok`, `failed`, or `still_running` if the deadline expired.
    pub outcome: String,
    /// `none` -- the form introduced no events. Queries and `thm` land here.
    /// `undone` -- it introduced events, which were undone. Expect it not to
    /// be visible afterwards; put it in the file if you want it to persist.
    /// `dirty` -- events were introduced and the undo could not be verified;
    /// the next sync rebuilds by replay.
    pub world_change: String,
    pub output_ref: String,
    pub output_lines: usize,
    pub elapsed_ms: u128,
}

/// A slice of one form's captured output.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct OutputReply {
    pub text: String,
    /// Lines in the whole log, not in this slice.
    pub total_lines: usize,
    /// The form this came from has since been rolled back, so the output no
    /// longer describes the current world. Still readable for comparison.
    pub superseded: bool,
}

/// Progress on a form that is still running.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ProgressReply {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit_index: Option<usize>,
    pub elapsed_ms: u128,
    /// The last few lines printed so far. Provisional: the authoritative
    /// result arrives when the form finishes.
    pub tail: String,
    pub bytes_so_far: usize,
}

/// What resuming a form that had outrun its deadline produced.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ResumeReply {
    /// `still_running`, `admitted`, `failed`, or `nothing_in_flight`.
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admitted: Option<Admitted>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<Failed>,
    pub elapsed_ms: u128,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct OpenReply {
    pub session_id: String,
    pub file: String,
    /// The server re-enabled PROVE output so proofs could be captured, because
    /// this environment inhibits it. Prover output in the log is therefore
    /// output the user's own session would have suppressed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prove_output_overridden: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct HealthReply {
    /// `ready` submit now; `busy` a form is in flight, wait or interrupt;
    /// `unresponsive` nothing should be running yet it is not answering;
    /// `dead` the process is gone.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running_unit: Option<usize>,
    pub rebuilds: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SessionsReply {
    pub sessions: Vec<SessionEntry>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SessionEntry {
    pub session_id: String,
    pub file: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Ack {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}
