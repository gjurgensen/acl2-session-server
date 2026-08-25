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
    /// The event's kind -- `defun`, `defthm`, `verify-guards` -- when
    /// recorded. Says *what kind of thing* failed without parsing prose.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Runes this inner event used, one per entry. Answers questions like
    /// "did the prep-lemma actually get used?", which the outer form cannot.
    ///
    /// Inner events appear only on failures, where this is evidence about
    /// the proof being debugged; an admitted form's inner detail lives in
    /// its log (see `Admitted`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
    /// This event's own prove time, in milliseconds. For a clique, this is
    /// "which member was slow" -- the outer total cannot say. ACL2 records a
    /// `(prove print proof-tree other)` tuple of exact rationals; the prove
    /// component is the one that answers questions, so it is the one carried,
    /// and the full tuple is in the form's log for whoever wants the rest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prove_ms: Option<u64>,
    /// Prover steps this event took, when non-negative.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prover_steps: Option<u64>,
    /// This event ran out of a prover step limit of this size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_limit_reached: Option<u64>,
}

fn steps_split(steps: Option<i64>) -> (Option<u64>, Option<u64>) {
    match steps {
        Some(n) if n < 0 => (None, Some(n.unsigned_abs())),
        Some(n) => (Some(n as u64), None),
        None => (None, None),
    }
}

/// The prove component of ACL2's `(prove print proof-tree other)` time
/// tuple, rendered as milliseconds.
///
/// ACL2 records the components as exact rationals in seconds --
/// `(4929/20000 663/250000 0 877/500000)` -- which cannot be compared at a
/// glance and whose meaning an agent had to guess (wrongly, which is
/// the point). A named integer can. `None` for anything that does not parse:
/// a value we cannot read honestly is a value not reported.
fn prove_ms_of(time: Option<&str>) -> Option<u64> {
    let t = time?.trim().strip_prefix('(')?.strip_suffix(')')?;
    let first = t.split_whitespace().next()?;
    let (num, den) = match first.split_once('/') {
        Some((n, d)) => (n.parse::<u128>().ok()?, d.parse::<u128>().ok()?),
        None => (first.parse::<u128>().ok()?, 1),
    };
    if den == 0 {
        return None;
    }
    // Round half up, in integer arithmetic: exactness is the one virtue of
    // the rational form worth keeping.
    ((num * 1000 + den / 2) / den).try_into().ok()
}

/// An inner event of a form that failed: full detail.
fn inner_failed(e: &core::InnerEvent) -> InnerEvent {
    let (prover_steps, step_limit_reached) = steps_split(e.prover_steps);
    InnerEvent {
        name: e.name.clone(),
        abort_causes: e.abort_causes.clone(),
        kind: e.kind.clone(),
        rules: e.rules.clone(),
        prove_ms: prove_ms_of(e.time.as_deref()),
        prover_steps,
        step_limit_reached,
    }
}

/// One key checkpoint of a failed proof.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Goal {
    /// The goal's name exactly as ACL2 prints it -- `Subgoal *1/2''` -- which
    /// is exactly what `acl2_output mode:"goal"` takes: pass it back to read
    /// everything the prover printed under this goal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    /// The goal itself, as ACL2 would print it. Layout is preserved.
    pub term: String,
}

/// Key checkpoints of a failed proof, as ACL2 would print them.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Checkpoints {
    /// Where the top-level proof stalled.
    ///
    /// If `pre_revert` is set, these are the goals as they stood *before* the
    /// proof reverted to induction -- a phase the prover then abandoned.
    pub top: Vec<Goal>,
    /// Goals under a top-level induction. Often where the actual defect is,
    /// which is why these are kept separate from `top` rather than merged.
    pub sub: Vec<Goal>,
    /// How many the prover pushed. May exceed the number listed: a fixed
    /// number of slots is read, and this says when there were more.
    pub n_top: usize,
    pub n_sub: usize,
    /// Forcing took place, so the checkpoints are probably not the whole story.
    pub forced: bool,
    /// The proof reverted to proof by induction. `top` then describes the
    /// pre-revert goals, and `sub` the induction it fell back on.
    pub pre_revert: bool,
}

fn checkpoints(c: &core::sync::Checkpoints) -> Checkpoints {
    let goal = |g: &core::Checkpoint| Goal { goal: g.goal.clone(), term: g.term.clone() };
    Checkpoints {
        top: c.top.iter().map(goal).collect(),
        sub: c.sub.iter().map(goal).collect(),
        n_top: c.n_top,
        n_sub: c.n_sub,
        forced: c.forced,
        pre_revert: c.pre_revert,
    }
}

/// A form that was admitted.
///
/// What it is, where it is, and a handle to the evidence. Deliberately
/// *compact*: a sync reports every form that went in, so anything routine
/// here is multiplied by the length of the book -- and not once, since sync
/// reuses the untouched prefix and re-admits everything below the first
/// change, so editing near the top of a book pays this on every save.
///
/// The trimmed detail -- rules, prover steps, inner events with their
/// per-event times, and the log's length -- is per form in the log, so ask
/// the one form you care about: `acl2_output output_ref:"<this form's>"
/// mode:"find" text:"Rules:"` (or `"Prover steps"`, or `"Form:"` for the
/// events a compound form introduced). Failures keep everything, where it is
/// evidence rather than bulk.
///
/// There is deliberately no form index. Nothing on this surface accepts one
/// -- `through_line`, refined by `through_column`, is the only positional
/// input -- so it was a number you could look at and not use, and printing it
/// beside `output_ref` invited the reading that `f63` names form 63.
///
/// It never did, not even once. `f` counts everything the session has *put in
/// the store* -- and it is 1-based where indices are 0-based, so a fresh
/// session's first form is index 0 and `f1`. From there the gap only widens:
/// every `acl2_eval`, every `acl2_certify`, and every re-admission of a form
/// takes the next number, while the index stays put. (Internal probes do not:
/// they are not stored, so they are not the mechanism.) Two sessions drew the
/// inference anyway and were wrong -- one asserted the ref was derivable from
/// the index and watched a single unchanged form come back `f149`, `f150`,
/// `f151`; a third, evaluating heavily enough to see index 51 arrive as
/// `f161`, was never tempted and reported the index as simply unused.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Admitted {
    /// What this form is called -- what a reader means by "that form".
    /// Absent when it names nothing, so an `include-book` costs nothing.
    ///
    /// A label, not a key. A compound form carries several, and `names_total`
    /// says so when more existed than fit. Nor is it the full list of what
    /// landed in the world: `(fty::defprod point ...)` admits 37 events, and
    /// the one worth printing is `point`. Ask the log for the rest --
    /// `acl2_output output_ref:"..." mode:"find" text:"Form:"`.
    ///
    /// Taken from ACL2's own record of the form, read through the source: the
    /// world says which names are real, and the text says which of them the
    /// author wrote and in what order. See `UnitReport::display_names` for
    /// what each source gets wrong alone.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    /// Present only when the written names did not fit the budget.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub names_total: Option<usize>,
    pub line_start: usize,
    /// The column that line starts the form at, present only when the call
    /// asked for `columns`. See `SyncReply::target_column`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub col_start: Option<usize>,
    /// Wall time, present only for a form that took over a second.
    ///
    /// Self-suppressing, the way `warnings` is: on two measured sessions of
    /// real proofs -- not `skip_proofs` -- forms ran 1-83 ms and 1-495 ms, so an ordinary
    /// book never reaches the threshold and pays nothing, while the book with
    /// a thirty-second form still says so. The unit anyone actually waits on
    /// is the whole book, and in those same sessions that *was* worth minutes
    /// -- which is why timing stays on the `acl2_certify` reply unconditioned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u128>,
    /// The kinds of warning this form emitted -- `Double-rewrite`, `Use` --
    /// present only when there were any, since unlike rules a warning is an
    /// exception rather than a fact every form carries.
    ///
    /// Kinds, not counts: ACL2 records the set, so two double-rewrite
    /// warnings on one form look exactly like one. If you are fixing them
    /// one at a time, count the `ACL2 Warning [...]` headers in this form's
    /// log rather than reading an unchanged list as no progress.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Hints that fired -- `((:use lemma))` and friends. Present exactly when
    /// a hint did something, so "did my hint matter" is answered on success
    /// too, where it matters most.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hint_events: Vec<String>,
    /// Rules that caused case splits, when ACL2 recorded any -- the "why did
    /// this get slow" answer for a form that still went in.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub splitter_rules: Vec<String>,
    /// Handle to everything ACL2 printed for this form. Pass to `acl2_output`.
    /// Kept on every record, however compact: it is the only handle to a
    /// successful form's log that will ever exist, and it names one *run* --
    /// re-admit a form and the old ref stays readable and flagged
    /// `superseded`, which no index could express.
    pub output_ref: String,
}

/// A form that did not go in.
///
/// Everything is kept here -- this is evidence, not bulk -- except the form
/// index, which is absent for the same reason it is absent from `Admitted`:
/// nothing accepts one, and beside `output_ref` it reads as the `f` number.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Failed {
    pub line_start: usize,
    pub line_end: usize,
    /// The two columns, present only when the call asked for `columns`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub col_start: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub col_end: Option<usize>,
    /// `proof_failure` -- a proof ran and did not finish; checkpoints and an
    /// abort cause are available.
    /// `rejected` -- ACL2 declined the form before any proof: a translate
    /// error, a redefinition, a guard violation, a failed `include-book`. No
    /// checkpoints exist for it.
    ///
    /// These call for completely different responses, which is why they are
    /// distinguished rather than both reported as "failed". A deadline is
    /// neither, and is reported under `in_flight`.
    pub status: String,
    /// Why the prover gave up. `empty-clause` means the conjecture is **false**
    /// rather than merely unproven -- stop trying hints and fix the statement.
    /// `induction-depth-limit-exceeded` means it ran out of depth.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort_cause: Option<String>,
    /// The `ACL2 Error` message, with ACL2's line wrapping undone so the
    /// sentence is whole -- the *last* error paragraph, since testing
    /// utilities such as `must-fail` print expected errors earlier in a
    /// compound form. Matters most for `rejected`, where no proof ran and
    /// the other fields are empty. For a compound form it also names the
    /// inner event that failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prover_steps: Option<u64>,
    /// The proof ran out of a prover step limit of this size (set by the
    /// book or the environment). A different situation from a proof that
    /// failed on the merits: the conjecture was not refuted, the budget ran
    /// out -- raise the limit or simplify, rather than fixing a "false"
    /// statement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_limit_reached: Option<u64>,
    /// The kind of the failing event -- `defun`, `defthm`, `verify-guards` --
    /// when ACL2 recorded it. Distinguishes a definition rejected at
    /// translate level from a guard verification from a proof failure,
    /// without touching prover prose.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_kind: Option<String>,
    /// Hints that fired before the failure, when any did.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hint_events: Vec<String>,
    /// Rules that caused case splits, when ACL2 recorded any -- the "why did
    /// this explode into cases" answer.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub splitter_rules: Vec<String>,
    pub checkpoints: Checkpoints,
    pub inner: Vec<InnerEvent>,
    /// How many of `inner` carry detail (`rules`, `abort_causes`).
    ///
    /// `inner` names **every** event the form introduced, but the probe reads
    /// detail for the first few only. Past this index an entry is a name and
    /// nothing else, so an empty `rules` there means "not read", not "used no
    /// rules" -- the distinction this field exists to make.
    ///
    /// The event that failed is always within it: ACL2 records newest-first
    /// and a compound form stops at the event that failed, putting it at index
    /// 0. For the rest, search that form's `output_ref` for `Rules:`, which
    /// ACL2 prints per event under a `Form:` header.
    pub inner_detail_through: usize,
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
    /// The line this sync was *aiming* at: the file's last form, or the one
    /// `through_line` selected, reported as that form's first line.
    ///
    /// A line rather than a form index, so it round-trips: the number you get
    /// back is the number you can pass to `through_line`. An index was neither
    /// usable as input nor relatable to anything else in the reply.
    ///
    /// This is the target, not the progress -- it does not move as a long book
    /// is worked through. How far the run actually got is `admitted` and
    /// `reused`, and the form left running is `in_flight`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_line: Option<usize>,
    /// That form's column, present only when the call asked for `columns`.
    ///
    /// A line names a form exactly until two forms share one, which is rare
    /// but legal: `(defun foo (x) x) (defun bar (x) x)` is two forms, and
    /// only a column tells them apart. Ask for columns when you are working
    /// in a file that does that, or when a `through_line` selected a form
    /// you did not mean; otherwise the line alone is the smaller reply and
    /// says everything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_column: Option<usize>,
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
    ///
    /// Present only for a form ACL2 actually rejected. A form still running
    /// when the deadline expired is **not** a failure and appears under
    /// `in_flight`, so testing for this key is a sound way to ask "did
    /// anything break?".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<Failed>,
    /// The form still running when the deadline expired. Nothing is wrong:
    /// ACL2 is still working. Call `acl2_resume` to wait for it, or
    /// `acl2_interrupt` to stop it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_flight: Option<InFlight>,
    /// Books this file includes whose sources have changed since the session
    /// built its world on them -- so the world is out of step with disk, and
    /// forms reported `reused` were reused against the old ones. Present
    /// only when there are any; see `StaleInclude` for what to do.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stale_includes: Vec<StaleInclude>,
}

/// A book the session's world rests on whose source has changed since.
///
/// The world is still exactly what this file's forms admitted -- but one of
/// the books they include is no longer what it was, so the world no longer
/// describes what is on disk. Nothing has been undone: re-proving from the
/// including form may cost the rest of the book, and only you know whether
/// that is worth it now. To act, sync with `through_line` at that form and
/// `retract:true`, or close the session and open a new one.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct StaleInclude {
    /// The book, as ACL2 names it.
    pub book: String,
    /// Where it resolved to on disk.
    pub path: String,
    /// The first line of the form in *this* file that pulled it in -- the
    /// line to sync `through_line` at. Absent when no form did: a book
    /// included transitively by another book, or one the session environment
    /// supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// That form's column, present only when the call asked for `columns`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    /// The file is gone, not merely different.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub missing: bool,
}

pub fn stale_include(s: &core::StaleInclude, columns: bool) -> StaleInclude {
    StaleInclude {
        book: s.book.clone(),
        path: s.path.display().to_string(),
        line: s.line,
        column: s.column.filter(|_| columns),
        missing: s.missing,
    }
}

/// A form ACL2 is still working on.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct InFlight {
    pub line_start: usize,
    pub line_end: usize,
    /// The two columns, present only when the call asked for `columns`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub col_start: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub col_end: Option<usize>,
    /// Handle to this form's output, readable **while the form runs**.
    ///
    /// Pass it to `acl2_output` to watch a long proof as it goes: every mode
    /// works, so `mode:"find" text:"Subgoal"` follows the waterfall and
    /// `mode:"goal"` pulls one checkpoint out of a proof that has not
    /// finished. `total_lines` grows between calls; the same handle keeps
    /// working, and holds the finished log afterwards.
    pub output_ref: String,
}

/// The sync reply, with columns included only if the call asked for them.
///
/// Not a `From` impl, because `columns` is a second input: every position in
/// this reply is a line alone unless the caller said otherwise, and a
/// conversion trait has nowhere to put that.
pub fn sync_reply(o: core::SyncOutcome, columns: bool) -> SyncReply {
    // A deadline is not a failure. Reporting it under `failed` made a
    // clean book look broken to anyone keying on that field, which is the
    // obvious thing to key on.
    let running = o.failed.as_ref().is_some_and(|f| f.still_running);
    SyncReply {
        target_line: o.resolved_through_line,
        target_column: o.resolved_through_column.filter(|_| columns),
        reused: o.reused,
        rolled_back: o.rolled_back,
        rebuilt: o.rebuilt,
        admitted: o.admitted.iter().map(|u| admitted(u, columns)).collect(),
        failed: o.failed.as_ref().filter(|_| !running).map(|f| failed(f, columns)),
        stale_includes: o.stale_includes.iter().map(|s| stale_include(s, columns)).collect(),
        in_flight: o.failed.as_ref().filter(|_| running).map(|f| InFlight {
            line_start: f.line_start,
            line_end: f.line_end,
            col_start: columns.then_some(f.col_start),
            col_end: columns.then_some(f.col_end),
            output_ref: f.output_ref.to_string(),
        }),
    }
}

// The inner events are dropped here rather than at the source: the core
// still carries them on `UnitReport`, and a different client may want them.
/// How many characters of names one record may carry.
///
/// A budget rather than a count, because ACL2 names run long: four at forty
/// characters would be 180 bytes, worse than the whole record used to be,
/// while a count of four says nothing about that. Sized to hold the common
/// single name comfortably and to stop an `encapsulate` of a dozen from
/// dominating the reply.
const NAME_BUDGET: usize = 60;

/// Wall time is reported only above this, in milliseconds.
///
/// Absolute and deliberately boring: it needs no calibration to the book, and
/// a second is roughly where waiting becomes noticeable. A threshold rather
/// than a ratio because "slower than its neighbours" is not the question --
/// "is this one worth my attention" is. The measurements behind it are in
/// `Admitted::elapsed_ms`.
const SLOW_MS: u128 = 1000;

/// Names, up to the budget, and how many there really were.
///
/// Truncation is reported rather than silent, matching `inner`/`inner_total`
/// and the checkpoint counts: a list that quietly stops is a list you cannot
/// trust to be a list.
fn names_within_budget(names: &[String]) -> (Vec<String>, Option<usize>) {
    let mut out = Vec::new();
    let mut used = 0;
    for n in names {
        // Always take the first, however long: one over-budget name is more
        // use than none, and the total says what was left out.
        if !out.is_empty() && used + n.len() > NAME_BUDGET {
            break;
        }
        used += n.len();
        out.push(n.clone());
    }
    let total = (out.len() < names.len()).then_some(names.len());
    (out, total)
}

pub fn admitted(u: &core::sync::UnitReport, columns: bool) -> Admitted {
    let (names, names_total) = names_within_budget(&u.display_names());
    Admitted {
        names,
        names_total,
        line_start: u.line_start,
        col_start: columns.then_some(u.col_start),
        elapsed_ms: Some(u.elapsed.as_millis()).filter(|ms| *ms >= SLOW_MS),
        warnings: u.event_data.warnings.clone(),
        hint_events: u.event_data.hint_events.clone(),
        splitter_rules: u.event_data.splitter_rules.clone(),
        output_ref: u.output_ref.to_string(),
    }
}

pub fn failed(f: &core::sync::FailureReport, columns: bool) -> Failed {
    let (prover_steps, step_limit_reached) = steps_split(f.event_data.prover_steps);
    Failed {
        line_start: f.line_start,
        line_end: f.line_end,
        col_start: columns.then_some(f.col_start),
        col_end: columns.then_some(f.col_end),
        status: match f.status {
            core::Status::ProofFailure => "proof_failure",
            core::Status::Rejected => "rejected",
            core::Status::DeadlineExceeded => "deadline_exceeded",
        }
        .to_string(),
        abort_cause: f.checkpoints.abort_cause.clone(),
        error_line: f.error_line.clone(),
        prover_steps,
        step_limit_reached,
        failed_kind: f.failed_event().and_then(|e| e.kind.clone()),
        hint_events: f.event_data.hint_events.clone(),
        splitter_rules: f.event_data.splitter_rules.clone(),
        checkpoints: checkpoints(&f.checkpoints),
        inner: f.inner.iter().map(inner_failed).collect(),
        inner_detail_through: f.inner.len().min(acl2_session::sync::INNER_DETAIL_SLOTS),
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
    /// Why the form failed, when it did: the same envelope a sync failure
    /// carries. For the experimental `thm` -- the form eval exists for --
    /// this is where the checkpoints and abort cause live.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<EvalFailed>,
    pub output_ref: String,
    pub output_lines: usize,
    pub elapsed_ms: u128,
}

/// The failure envelope of an eval'd form. The same fields as a sync
/// failure's, minus the file coordinates an eval does not have; see `Failed`
/// for what each means.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EvalFailed {
    /// `proof_failure` -- a proof ran and did not finish; checkpoints and an
    /// abort cause are available. `rejected` -- ACL2 declined the form before
    /// any proof.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort_cause: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prover_steps: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_limit_reached: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_kind: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hint_events: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub splitter_rules: Vec<String>,
    pub checkpoints: Checkpoints,
    pub inner: Vec<InnerEvent>,
    pub inner_detail_through: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_event: Option<String>,
}

pub fn evaluated(r: &core::EvalResult) -> EvalReply {
    EvalReply {
        outcome: match r.outcome {
            core::Outcome::Ok => "ok",
            core::Outcome::Failed => "failed",
            core::Outcome::StillRunning => "still_running",
        }
        .to_string(),
        world_change: match r.world_change {
            core::WorldChange::None => "none",
            core::WorldChange::Undone => "undone",
            core::WorldChange::Dirty => "dirty",
        }
        .to_string(),
        failed: r.failed.as_ref().map(|f| {
            let (prover_steps, step_limit_reached) = steps_split(f.event_data.prover_steps);
            EvalFailed {
                status: match f.status {
                    core::Status::ProofFailure => "proof_failure",
                    core::Status::Rejected => "rejected",
                    core::Status::DeadlineExceeded => "deadline_exceeded",
                }
                .to_string(),
                abort_cause: f.checkpoints.abort_cause.clone(),
                error_line: f.error_line.clone(),
                prover_steps,
                step_limit_reached,
                failed_kind: f.failed_event().and_then(|e| e.kind.clone()),
                hint_events: f.event_data.hint_events.clone(),
                splitter_rules: f.event_data.splitter_rules.clone(),
                checkpoints: checkpoints(&f.checkpoints),
                inner: f.inner.iter().map(inner_failed).collect(),
                inner_detail_through: f
                    .inner
                    .len()
                    .min(acl2_session::sync::INNER_DETAIL_SLOTS),
                failed_event: f.failed_event().map(|e| e.name.clone()),
            }
        }),
        output_ref: r.output_ref.to_string(),
        output_lines: r.output_lines,
        elapsed_ms: r.elapsed.as_millis(),
    }
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
    /// A certification still running, if there is one. Independent of
    /// `running`: a build is a separate process, so it can be going while the
    /// prover is idle, and both can be in flight at once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildProgressReply>,
}

/// How far a certification has got. Provisional; `acl2_certify` returns the
/// authoritative result when the build finishes.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct BuildProgressReply {
    pub book: String,
    pub elapsed_ms: u128,
    pub lines_so_far: usize,
    pub tail: String,
}

pub fn build_progress(p: &acl2_session::cert::BuildProgress) -> BuildProgressReply {
    BuildProgressReply {
        book: p.book.display().to_string(),
        elapsed_ms: p.elapsed.as_millis(),
        lines_so_far: p.lines_so_far,
        tail: p.tail.clone(),
    }
}

/// What resuming a form that had outrun its deadline produced.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ResumeReply {
    /// `still_running`, `admitted`, `failed`, `evaluated`, or
    /// `nothing_in_flight`. `evaluated` means the form was one you submitted
    /// with `acl2_eval` rather than a form of the file.
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admitted: Option<Admitted>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<Failed>,
    /// Present when the form that finished came from `acl2_eval`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evaluated: Option<EvalReply>,
    pub elapsed_ms: u128,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct OpenReply {
    pub session_id: String,
    /// This server's version and the commit it was built from, e.g.
    /// `0.0.1+g5c575da`. Reported once per session so that behaviour can be
    /// attributed to a build: an installed binary that was never restarted
    /// serves the old code, and nothing else in any reply reveals it.
    pub server: String,
    pub file: String,
    /// The ACL2 image this session is running. Reported because it is usually
    /// derived rather than requested, and a session keeps it for life.
    pub acl2: String,
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
    /// `dead` the process is gone -- close the session and open it again.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running_unit: Option<usize>,
    /// Absent when the session was busy and the counter could not be read
    /// without waiting behind the in-flight call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rebuilds: Option<usize>,
}

/// What an interrupt accomplished.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct InterruptReply {
    /// The session answered a round-trip after the signal; it is usable.
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// What ACL2 recorded about the interrupted form: present when a form
    /// was actually interrupted and the record is trustworthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<Interrupted>,
}

/// The envelope of an interrupted form. The partial output behind
/// `output_ref` is the record; this is the summary.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Interrupted {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort_cause: Option<String>,
    /// The interrupted event, abbreviated. An outline, not the form.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    /// Prove time up to the interrupt, in milliseconds -- the "how long had
    /// it been grinding" number, comparable at a glance with `elapsed_ms`
    /// figures elsewhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prove_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prover_steps: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_limit_reached: Option<u64>,
    /// Whatever the prover had pushed when interrupted. Empty stacks are
    /// normal for a grind inside an executable counterpart; a waterfall
    /// interrupt can carry real checkpoints.
    pub checkpoints: Checkpoints,
    /// The partial output captured before the signal landed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_ref: Option<String>,
    pub output_lines: usize,
}

pub fn interrupted(r: &core::InterruptReport) -> InterruptReply {
    InterruptReply {
        ok: r.ok,
        detail: (!r.ok).then(|| "session did not recover; consider reopening".into()),
        interrupted: r.interrupted.as_ref().map(|e| {
            let (prover_steps, step_limit_reached) = steps_split(e.prover_steps);
            Interrupted {
                abort_cause: e.abort_cause.clone(),
                event: e.event.clone(),
                prove_ms: prove_ms_of(e.time.as_deref()),
                prover_steps,
                step_limit_reached,
                checkpoints: checkpoints(&e.checkpoints),
                output_ref: e.output_ref.as_ref().map(|o| o.to_string()),
                output_lines: e.output_lines,
            }
        }),
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SessionsReply {
    pub sessions: Vec<SessionEntry>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SessionEntry {
    pub session_id: String,
    pub file: String,
    pub acl2: String,
}

/// What the server may run.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ImagesReply {
    /// Images that may be passed to `acl2_session_open`.
    pub images: Vec<String>,
    /// Directories whose ACL2 checkouts are trusted. A book inside one of
    /// these runs under its own checkout's image.
    pub trusted_directories: Vec<String>,
    /// Used for books outside every trusted directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Ack {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What certifying a book produced.
///
/// Certification runs `cert.pl` outside the session, against the file on disk.
/// It does not touch the session's world, so a session stays usable during and
/// after one -- and an unsaved editor buffer certifies as whatever is actually
/// on disk, exactly as it would from a shell.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CertifyReply {
    /// `certified`, `failed`, or `still_running` if the deadline expired.
    ///
    /// `still_running` means the build is unaffected and continuing. Call
    /// `acl2_certify` again to keep waiting on it -- **not** `acl2_resume`,
    /// which waits on the prover, a different process. `acl2_status` shows how
    /// far it has got without waiting; `acl2_interrupt` stops it.
    pub outcome: String,
    /// The book that was asked for.
    pub book: String,
    /// Books that actually failed. Certification chases dependencies, so this
    /// is often **not** the book you asked for -- check `failed_in_dependency`
    /// before concluding your own edits broke something.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failed_books: Vec<String>,
    /// The blame falls on a dependency rather than on `book`.
    pub failed_in_dependency: bool,
    /// The ACL2 error from the failing book's own `.cert.out`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_line: Option<String>,
    /// Handle to everything `cert.pl` printed. Absent while still running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_ref: Option<String>,
    pub output_lines: usize,
    /// The last lines printed, when still running.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tail: String,
    pub elapsed_ms: u128,
}

pub fn certified(r: &acl2_session::cert::CertReport) -> CertifyReply {
    use acl2_session::cert::CertStatus;
    CertifyReply {
        outcome: match r.status {
            CertStatus::Certified => "certified",
            CertStatus::Failed => "failed",
            CertStatus::StillRunning => "still_running",
        }
        .to_string(),
        book: r.book.display().to_string(),
        failed_books: r.failed_books.iter().map(|p| p.display().to_string()).collect(),
        failed_in_dependency: r.failed_in_dependency(),
        error_line: r.error_line.clone(),
        output_ref: r.output_ref.as_ref().map(|o| o.to_string()),
        output_lines: r.output_lines,
        tail: r.tail.clone(),
        elapsed_ms: r.elapsed.as_millis(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real tuples from the field session, plus the shapes that must fail
    /// closed. The first component is prove time in seconds.
    #[test]
    fn prove_time_renders_as_milliseconds() {
        // 259459/1000000 s = 259.459 ms -> 259.
        assert_eq!(prove_ms_of(Some("(259459/1000000 2623/1000000 0 39/25000)")), Some(259));
        // Sub-millisecond rounds to zero rather than inflating.
        assert_eq!(prove_ms_of(Some("(17/200000 0 0 1551/500000)")), Some(0));
        // Integer seconds, and half-up rounding.
        assert_eq!(prove_ms_of(Some("(2 0 0 0)")), Some(2000));
        assert_eq!(prove_ms_of(Some("(1/2000 0 0 0)")), Some(1)); // 0.5 ms up
        // Unparseable shapes report nothing rather than something wrong.
        assert_eq!(prove_ms_of(Some("NIL")), None);
        assert_eq!(prove_ms_of(Some("(x y z)")), None);
        assert_eq!(prove_ms_of(Some("(1/0 0 0 0)")), None);
        assert_eq!(prove_ms_of(None), None);
    }

    fn n(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_common_case_is_one_name_and_no_total() {
        let (names, total) = names_within_budget(&n(&["segment-listp-of-append"]));
        assert_eq!(names, ["segment-listp-of-append"]);
        assert_eq!(total, None, "nothing was left out, so nothing is claimed");
    }

    #[test]
    fn nothing_written_reports_nothing() {
        let (names, total) = names_within_budget(&[]);
        assert!(names.is_empty());
        assert_eq!(total, None);
    }

    /// The budget is characters, not a count: this is the case a count of
    /// four would have let through at 180 bytes.
    #[test]
    fn long_names_stop_sooner_than_short_ones() {
        let long = n(&[
            "segment-listp-of-revappend-when-segment-listp",
            "segment-listp-of-append-when-segment-listp",
            "segment-listp-of-rev",
        ]);
        let (names, total) = names_within_budget(&long);
        assert_eq!(names.len(), 1, "one long name fills the budget");
        assert_eq!(total, Some(3));

        let short = n(&["a", "b", "c", "d", "e"]);
        let (names, total) = names_within_budget(&short);
        assert_eq!(names.len(), 5, "five short ones fit where one long one did not");
        assert_eq!(total, None);
    }

    /// A single over-budget name is still reported. One name you can search
    /// for beats none, and the total says what was dropped.
    #[test]
    fn the_first_name_is_never_dropped() {
        let huge = "x".repeat(NAME_BUDGET * 3);
        let (names, total) = names_within_budget(&[huge.clone(), "b".into()]);
        assert_eq!(names, [huge]);
        assert_eq!(total, Some(2));
    }

    /// Truncation is always accompanied by the true count -- a list that
    /// quietly stops is a list you cannot trust to be a list.
    #[test]
    fn a_truncated_list_always_says_so() {
        for k in 1..12usize {
            let v: Vec<String> = (0..k).map(|i| format!("name-number-{i:02}")).collect();
            let (names, total) = names_within_budget(&v);
            assert_eq!(
                total.is_some(),
                names.len() < v.len(),
                "k={k}: total present exactly when something was dropped"
            );
            if let Some(t) = total {
                assert_eq!(t, k);
            }
        }
    }
}
