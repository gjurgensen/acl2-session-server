//! The file-prefix invariant.
//!
//! > The session world equals the result of admitting file units `1..N` in a
//! > plain interactive ACL2 session started in the book's own directory.
//!
//! The client never tells us what changed. Every sync re-hashes the whole
//! snapshot -- the file on disk, or the buffer bytes an editor hands in
//! (`sync_bytes`) -- so the mechanism is self-correcting: there is no
//! edit-notification protocol to fall out of step with. Rollback stops being
//! something a client requests and becomes a consequence of editing.

#![forbid(unsafe_code)]
// This module is pure logic; all FFI lives in pty.rs and session.rs.
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::session::{Outcome, Session, SessionControl};
use crate::output::{OutputRef, OutputSlice, OutputStore, Slice};
use crate::split::{split, Form, SplitError};

/// One unit the session has actually admitted.
#[derive(Debug, Clone)]
struct Admitted {
    hash: u64,
    line_start: usize,
    line_end: usize,
    /// Generation of the `deflabel` planted just after this unit; the rollback
    /// target for restoring the prefix that ends here.
    label: u64,
    /// `max-absolute-command-number` immediately after admission, used to
    /// verify that a rollback landed where we believe it did.
    command_number: Option<i64>,
    /// `current-package` immediately after admission. Recorded because the
    /// reader's package is *state*, not world: `ubu!` will not restore it, so
    /// a rollback across a sticky `(in-package ...)` re-establishes it from
    /// here.
    package: Option<String>,
    output_ref: OutputRef,
    /// Admitted with its proofs skipped (`ContinuePastFailure`). A
    /// rebuild must replay such a unit under `skip-proofs` too, and every
    /// admitted unit after the first of these is conditional.
    skipped: bool,
}

/// How far the deadline of one call has been spent.
///
/// One deadline bounds the whole call -- the tool docs promise exactly that --
/// so every submission a call makes draws down the same budget. Passing the
/// full deadline to each unit instead made a 100-unit book of 19-second proofs
/// under `deadline_seconds: 20` run for over half an hour.
///
/// Public because `rebuild` is, and a caller invoking a rebuild directly
/// supplies the budget the replay draws down.
pub struct Budget {
    total: Duration,
    started: Instant,
}

/// The floor under bookkeeping submissions -- rollbacks and the post-failure
/// probes. A submission sent with a zero deadline still goes in flight and
/// instantly times out, wedging the session behind our own bookkeeping; and a
/// genuine failure at budget exhaustion would lose its envelope. These are
/// sub-second in practice, so the floor is the bound on how far a call can
/// overrun its deadline, not a cost every call pays.
const PROBE_FLOOR: Duration = Duration::from_secs(10);

impl Budget {
    pub fn new(total: Duration) -> Budget {
        Budget { total, started: Instant::now() }
    }

    /// What is left for proof work. Zero is meaningful: a unit submitted with
    /// nothing left goes in flight and reports as such, which is the correct
    /// shape -- ACL2 keeps working and the caller resumes.
    fn remaining(&self) -> Duration {
        self.total.saturating_sub(self.started.elapsed())
    }

    /// What is left, floored for bookkeeping. See [`PROBE_FLOOR`].
    fn floored(&self) -> Duration {
        self.remaining().max(PROBE_FLOOR)
    }
}

/// What ACL2 recorded about an event, read from `get-event-data` rather than
/// parsed out of the prover's prose.
///
/// Values are ACL2-printed text. Deliberately: the *envelope* is structured --
/// each fact is its own field and can never be elided by a truncated blob --
/// while the values stay in the notation a reader of ACL2 already knows.
///
/// `rules` is the exception, and used not to be. Holding it as one printed
/// blob meant holding ACL2's *layout*: line breaks and alignment chosen for
/// whatever width the prover formatted for, which a client then had to parse
/// back before it could so much as filter by rule class. A rune list is
/// genuinely a list, so it is carried as one -- read element-wise from the
/// probe rather than recovered from the picture.
#[derive(Debug, Clone, Default)]
pub struct EventData {
    /// Signed, and the sign is load-bearing: ACL2 records a step-limited
    /// event as the *negative* of its limit -- `-200` for "exceeded a limit
    /// of 200". An unsigned parse silently dropped the field, making a
    /// step-limited failure indistinguishable from an ordinary one, which is
    /// precisely the diverging-versus-slow distinction the limit exists for.
    pub prover_steps: Option<i64>,
    /// Runes the proof used, one per entry.
    pub rules: Vec<String>,
    pub warnings: Option<String>,
    /// `(prove print proof-tree other)`, in seconds.
    pub time: Option<String>,
    /// Hints that fired, one per entry -- `((:use lemma))` and friends.
    /// Answers "did my hint do anything", which otherwise needs the log.
    pub hint_events: Vec<String>,
    /// Rules that caused case splits, when ACL2 recorded any. The "why did
    /// this explode into cases" answer.
    pub splitter_rules: Vec<String>,
}

impl EventData {
    /// The prover step limit this event ran out of, when it did.
    pub fn step_limit_reached(&self) -> Option<u64> {
        match self.prover_steps {
            Some(n) if n < 0 => Some(n.unsigned_abs()),
            _ => None,
        }
    }
}

// NOTE on compound forms -- `progn`, `encapsulate`, and every macro that
// expands to one, which is most of a real book: `define`, `defrule`, and
// friends.
//
// `get-event-data` describes the *outer* event, and for compound forms ACL2
// populates it only partially. Measured on a failing `progn`: no `ABORT-CAUSES`
// key at all, no `NAMEX`, and `RULES` empty -- against `(ABORT-CAUSES
// EMPTY-CLAUSE)` and a full rune list for the equivalent bare `defthm`. `TIME`
// and `PROVER-STEPS-COUNTED` do survive and are aggregates over the whole form.
//
// So `rules` is frequently absent on exactly the forms real code is made of.
// That is ACL2's reporting, not a loss in transit, and it is why the two fields
// that matter most are read from the prover instead: checkpoints and the abort
// cause both come from `gag-state-saved`, which is populated at proof level and
// is completely indifferent to how deeply the failing event was nested.

/// Key checkpoints from the most recent failed proof.
///
/// The two stacks stay separate because they answer different questions: `top`
/// is where the top-level proof stalled, `sub` is often where the actual defect
/// is. Flattening them buries the informative one.
/// One event ACL2 recorded *inside* a unit.
///
/// Most of a real book is compound forms -- `define`, `defrule`, `encapsulate`
/// -- and `get-event-data` describes only the outer one, reporting no rules and
/// no abort cause for it. These fill that in per inner event.
#[derive(Debug, Clone)]
pub struct InnerEvent {
    pub name: String,
    /// Non-empty when this is the inner event that failed.
    pub abort_causes: Option<String>,
    /// Runes this inner event used, one per entry.
    pub rules: Vec<String>,
    /// The event's kind -- `defun`, `defthm`, `verify-guards` -- read from
    /// what ACL2 recorded, so a failure can say *what kind of thing* failed
    /// without touching prover prose.
    pub kind: Option<String>,
    /// Whether ACL2 recorded a `namex` for this event. The failing event is
    /// the one *without* one -- it never got far enough to name anything --
    /// which identifies it even when no proof aborted (a translate-level
    /// failure has no `abort-causes` at all). `None` past the detail slots.
    pub has_namex: Option<bool>,
    /// This event's own prove time, `(prove print proof-tree other)`. For a
    /// `defines` clique this is "which member was slow", which the outer
    /// aggregate cannot say.
    pub time: Option<String>,
    /// Signed for the same reason as [`EventData::prover_steps`].
    pub prover_steps: Option<i64>,
}

/// One key checkpoint: the goal's name and its term.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    /// The goal's name as ACL2 prints it -- `Subgoal *1/2''` -- read from the
    /// gag state's clause-id. This is exactly what a goal-addressed output
    /// slice takes, so a client is never reduced to guessing names out of the
    /// log the envelope was supposed to spare it.
    pub goal: Option<String>,
    /// The goal itself, prettified as ACL2 would print it. Layout -- line
    /// breaks and indentation -- is kept deliberately: it is structure for
    /// the reader, collapsing it is trivial for a client, and recovering it
    /// is impossible.
    pub term: String,
}

#[derive(Debug, Clone, Default)]
pub struct Checkpoints {
    /// Whether `gag-state-saved` was populated at all -- i.e. whether a proof
    /// actually ran. This is what separates `ProofFailure` from `Rejected`.
    pub proof_ran: bool,
    /// Goals at the top level.
    ///
    /// When `pre_revert` is set these are the checkpoints from *before* the
    /// proof reverted to induction, since that is where ACL2 keeps them.
    pub top: Vec<Checkpoint>,
    /// Goals under a top-level induction.
    pub sub: Vec<Checkpoint>,
    /// How many the prover actually pushed. May exceed the number reported:
    /// the probe reads a fixed number of slots, and the counts say so rather
    /// than letting the client mistake a truncation for the whole story.
    pub n_top: usize,
    pub n_sub: usize,
    /// Why the prover gave up: `empty-clause` means the conjecture is false,
    /// `induction-depth-limit-exceeded` that it ran out of depth, and so on --
    /// distinctions that imply completely different next moves.
    ///
    /// Read from `gag-state-saved`'s `abort-info` rather than from
    /// `get-event-data`, because the latter reports nothing for compound forms
    /// (see the note above `EventData`). Measured: a failing `defthm`, a
    /// failing `progn` and a failing `encapsulate` all report `EMPTY-CLAUSE`
    /// here, where `get-event-data` reports it only for the bare `defthm`.
    pub abort_cause: Option<String>,
    /// The proof reverted to induction, so `top` holds the checkpoints as
    /// they stood before reverting. ACL2 labels these "before reverting to
    /// proof by induction"; the distinction matters because the goals shown
    /// are from a phase the prover abandoned.
    pub pre_revert: bool,
    /// Non-nil when forcing took place, in which case the checkpoints are
    /// probably not the full story.
    pub forced: bool,
}

#[derive(Debug, Clone)]
pub struct UnitReport {
    pub index: usize,
    pub line_start: usize,
    pub line_end: usize,
    pub elapsed: Duration,
    pub event_data: EventData,
    /// Events recorded inside this unit, newest first. See `InnerEvent`.
    pub inner: Vec<InnerEvent>,
    /// Total inner events ACL2 recorded, which may exceed `inner.len()`: a
    /// fixed number of slots is read, and the count says when there were more.
    pub inner_total: usize,
    /// Handle to everything ACL2 printed for this unit. Not inlined: a single
    /// `defthm` can emit six figures of output, and a report that carried it
    /// would be the very payload this design exists to avoid.
    pub output_ref: OutputRef,
    pub output_lines: usize,
}

/// Why a unit did not go in.
///
/// Every arm is derived from ACL2 *state*, never from its prose. That bounds
/// how fine the taxonomy can be: ACL2 does classify its own errors -- the
/// bracketed `[Failure]`, `[Translate]`, `[Evaluation]` labels -- but
/// `error1` only prints the summary and never records it, so those labels
/// exist solely in the output stream.
///
/// The distinction that survives is the one that matters most, because the two
/// call for completely different responses: the prover tried and could not
/// finish, versus ACL2 declined the form before any proof began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// A proof ran and did not succeed. Checkpoints and an abort cause are
    /// available. Covers plain theorem failures and guard verification.
    ProofFailure,
    /// ACL2 rejected the form without a proof failing: a translate error, a
    /// redefinition, a guard violation, an unreadable form, a failed
    /// `include-book`, a termination it would not even attempt. No
    /// checkpoints exist, and inventing some would be a lie.
    Rejected,
    /// The deadline expired. The form may still be running.
    DeadlineExceeded,
}

#[derive(Debug, Clone)]
pub struct FailureReport {
    pub index: usize,
    pub line_start: usize,
    pub line_end: usize,
    pub status: Status,
    /// True when the deadline expired rather than ACL2 rejecting the unit.
    pub still_running: bool,
    /// The first `ACL2 Error` line of the log, verbatim.
    ///
    /// A convenience slice of output the client could fetch itself, not a
    /// classification: nothing in this crate branches on it. It matters most
    /// for `Rejected`, where the structured envelope is otherwise empty
    /// because no proof ran to describe.
    pub error_line: Option<String>,
    pub output_ref: OutputRef,
    pub output_lines: usize,
    pub event_data: EventData,
    pub checkpoints: Checkpoints,
    pub inner: Vec<InnerEvent>,
    pub inner_total: usize,
    /// The skip-proofs retry's admission log, when `ContinuePastFailure`
    /// retried this unit. The main `output_ref` stays the original failure:
    /// the retry's chatter never replaces the informative log.
    pub retry_output_ref: Option<OutputRef>,
}

impl FailureReport {
    /// The inner event that actually failed, when the unit was compound.
    ///
    /// For a bare `defthm` this is the unit itself and adds nothing; for a
    /// `defrule` with prep-lemmas it distinguishes a failing prep-lemma from a
    /// failing main theorem, which the checkpoints alone cannot.
    ///
    /// An aborted proof carries `abort-causes`; a translate-level failure
    /// carries nothing at all -- what identifies it is the *missing* `namex`,
    /// since the event never got far enough to name anything. Both are
    /// checked, in that order.
    pub fn failed_event(&self) -> Option<&InnerEvent> {
        self.inner
            .iter()
            .find(|e| e.abort_causes.is_some())
            .or_else(|| self.inner.iter().find(|e| e.has_namex == Some(false)))
    }
}

/// What a sync does when a unit fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncMode {
    /// Stop at the first failure -- the default, and the agent-facing
    /// behavior: later units almost always depend on the failed one, and
    /// continuing produces cascading noise rather than information.
    #[default]
    StopAtFailure,
    /// Retry a completed failure under `(acl2::skip-proofs ...)` and keep
    /// going -- the editor's triage mode. The
    /// skipped unit is admitted assuming its unproved claim, every admitted
    /// unit after it is conditional on that, and the reply reports both.
    /// What skip-proofs cannot rescue -- anything that is not a proof
    /// obligation -- remains a terminal failure.
    ContinuePastFailure,
}

#[derive(Debug, Clone, Default)]
pub struct SyncOutcome {
    /// Index of the last unit the target resolved to, if any.
    pub resolved_through: Option<usize>,
    /// Prefix units left untouched because their hashes still matched.
    pub reused: usize,
    /// Units undone -- because the snapshot changed beneath them, or
    /// because a retraction asked (`Book::retract_to`).
    pub rolled_back: usize,
    pub admitted: Vec<UnitReport>,
    pub failed: Option<FailureReport>,
    /// True when the prefix had to be restored by discarding the ACL2 process
    /// and replaying, rather than by undo.
    ///
    /// This should be rare. It is surfaced not as a warning -- an agent has no
    /// use for one -- but so that the latency is *attributable*: a rebuild can
    /// take minutes, and without this a deadline expiry looks like a slow
    /// proof. An editor-facing client should turn the same signal into a
    /// visible warning, since the redundant work is unexpected from a user's
    /// point of view.
    pub rebuilt: bool,
    /// Units this call admitted under `skip-proofs` after they failed
    /// (`ContinuePastFailure` only), each carrying its original failure
    /// envelope. Always empty under `StopAtFailure`.
    pub skipped: Vec<FailureReport>,
    /// The earliest skip-proofs-admitted unit currently in the world --
    /// session state, not just this call's work. Every admitted unit after
    /// it is conditional on unproved claims; a client paints from this one
    /// number without remembering earlier replies.
    pub conditional_from: Option<usize>,
}

/// Whether an `eval` disturbed the logical world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorldChange {
    /// The form introduced no events. Queries (`pe`, `pl`, `pr`), session
    /// settings, and `thm` all land here -- verified: none of them advance the
    /// command number.
    None,
    /// The form introduced events, which were undone. Reported rather than
    /// hidden, so a client is not puzzled when its `defthm` is missing later.
    Undone,
    /// Events were introduced and the undo could not be verified. The prefix
    /// invariant is no longer trusted and the next `sync` rebuilds by replay.
    Dirty,
}

#[derive(Debug)]
pub struct EvalResult {
    pub outcome: Outcome,
    pub world_change: WorldChange,
    /// Why the form failed, when it did -- the same envelope a sync failure
    /// carries, minus the file coordinates an eval does not have. The
    /// experimental `thm` is the form evals exist for, and a failed
    /// experiment used to report nothing but "failed" plus a log handle --
    /// the agent was back to fishing in prose for exactly the form it was
    /// actively debugging.
    pub failed: Option<EvalFailure>,
    pub output_ref: OutputRef,
    pub output_lines: usize,
    pub elapsed: Duration,
}

/// The failure envelope of an eval'd form.
#[derive(Debug)]
pub struct EvalFailure {
    /// `ProofFailure` or `Rejected`; never `DeadlineExceeded`, which is an
    /// outcome (`StillRunning`), not a failure.
    pub status: Status,
    pub error_line: Option<String>,
    pub event_data: EventData,
    pub checkpoints: Checkpoints,
    pub inner: Vec<InnerEvent>,
    pub inner_total: usize,
}

impl EvalFailure {
    /// The inner event that actually failed. Same selection as
    /// [`FailureReport::failed_event`].
    pub fn failed_event(&self) -> Option<&InnerEvent> {
        self.inner
            .iter()
            .find(|e| e.abort_causes.is_some())
            .or_else(|| self.inner.iter().find(|e| e.has_namex == Some(false)))
    }
}

/// What an interrupt accomplished.
#[derive(Debug)]
pub struct InterruptReport {
    /// The session answered a round-trip after the signal; it is usable.
    pub ok: bool,
    /// What ACL2 recorded about the interrupted form. Present when the
    /// interrupt landed, something was actually in flight, and the recorded
    /// abort cause really names an interrupt -- anything less specific could
    /// describe some earlier event, since not every submission path clears
    /// the stale-failure globals first.
    pub interrupted: Option<InterruptEnvelope>,
}

/// The envelope of an interrupted form: what it was, how long it ran, and
/// where it stood. The partial output is the record; this is the summary that
/// used to be a bare `true`.
#[derive(Debug)]
pub struct InterruptEnvelope {
    /// Contains `interrupt` by construction; carried for the client.
    pub abort_cause: Option<String>,
    /// The interrupted event, evisc-abbreviated. An outline, not the form.
    pub event: Option<String>,
    /// `(prove print proof-tree other)` seconds up to the interrupt.
    pub time: Option<String>,
    pub prover_steps: Option<i64>,
    /// Whatever the prover had pushed when interrupted. Empty stacks are the
    /// common case for an executable-counterpart grind; a waterfall interrupt
    /// can carry real checkpoints.
    pub checkpoints: Checkpoints,
    /// Handle to the partial output captured before the signal.
    pub output_ref: Option<OutputRef>,
    pub output_lines: usize,
}

/// What resuming an in-flight unit produced.
#[derive(Debug)]
pub enum Resumed {
    /// Still running. The deadline bounded this call, not the proof.
    StillRunning { elapsed: Duration },
    Admitted(UnitReport),
    Failed(FailureReport),
    /// The unit failed and `ContinuePastFailure` then admitted it under
    /// `skip-proofs`: the world moved past it, the report is the original
    /// failure's envelope, and the next sync treats the unit as prefix.
    Skipped(FailureReport),
    /// A form started by `eval` finished. `eval` is not part of the file, so
    /// it completes as an eval rather than as a unit.
    Evaluated(EvalResult),
}

#[derive(Debug)]
pub enum SyncError {
    Io(io::Error),
    /// A form is still running. Resume it, interrupt it, or wait.
    ///
    /// `index` names the unit when one is mid-sync, and is `None` when the
    /// form in flight came from `eval`, which is not part of the file.
    ///
    /// Rejected rather than queued: a session drives one ACL2 over one pipe,
    /// and starting a second form would interleave both on the same channel.
    Busy { index: Option<usize> },
    Split(SplitError),
    /// The undo itself was rejected by ACL2.
    RollbackFailed { output: String },
    /// Replaying a unit that had previously been admitted failed.
    ReplayFailed { line_start: usize, output: String },
    /// A rollback did not land where the recorded command number says it
    /// should. Per P3 the fast path is always verified, and a failed check
    /// must degrade to rebuilding from scratch rather than being trusted.
    RollbackUnverified { expected: i64, actual: Option<i64> },
    /// Certification could not be started or run.
    Cert(crate::cert::CertError),
}

impl From<crate::cert::CertError> for SyncError {
    fn from(e: crate::cert::CertError) -> Self {
        SyncError::Cert(e)
    }
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Io(e) => write!(f, "{e}"),
            SyncError::Cert(e) => write!(f, "{e}"),
            SyncError::Busy { index } => match index {
                Some(i) => write!(
                    f,
                    "unit {i} is still running; resume, interrupt, or wait for it"
                ),
                None => write!(
                    f,
                    "a form is still running; resume, interrupt, or wait for it"
                ),
            },
            SyncError::Split(e) => write!(f, "cannot split file: {e}"),
            SyncError::RollbackFailed { output } => {
                let first = output.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                write!(f, "rollback rejected by ACL2: {}", first.trim())
            }
            SyncError::ReplayFailed { line_start, output } => {
                let first = output.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                write!(f, "replay failed at line {line_start}: {}", first.trim())
            }
            SyncError::RollbackUnverified { expected, actual } => write!(
                f,
                "rollback unverified: expected command number {expected}, found {actual:?}; \
                 the session must be rebuilt"
            ),
        }
    }
}

impl From<io::Error> for SyncError {
    fn from(e: io::Error) -> Self {
        SyncError::Io(e)
    }
}

/// Clear the state ACL2 leaves behind from earlier events.
///
/// `gag-state-saved` and `last-event-data` persist until something overwrites
/// them, and a failure that ran no prover -- a bad `include-book`, a translate
/// error -- overwrites neither. Without this, such a failure inherits the
/// previous proof's checkpoints and rules and reports them as its own.
/// Measured: a failing `include-book` after a failed `defthm` still reported
/// `(EQUAL (ALPHA X) X)` as its checkpoint.
///
/// Both are state globals, so clearing them does not touch the world.
const CLEAR_STALE: &str = "(acl2::pprogn \
    (acl2::f-put-global 'acl2::gag-state-saved acl2::nil acl2::state) \
    (acl2::f-put-global 'acl2::last-event-data acl2::nil acl2::state) \
    (acl2::f-put-global 'acl2::event-data-fal 'acl2::event-data-fal acl2::state) \
    (acl2::value :invisible))";

/// Take ownership of the settings that decide what gets *captured*, while
/// leaving the ones that decide what is *provable* exactly as inherited.
///
/// The session deliberately inherits the user's `acl2-customization.lsp`, so
/// that proofs behave here exactly as they do in their own session. But a very
/// common customization is `(quiet)`, which inhibits PROVE output -- and
/// PROVE inhibition also silently disables output saving entirely
/// (`prove.lisp`). Measured on one failing `defthm`: 27 captured lines with
/// `(quiet)` against 46 without, and the missing 19 are the induction-scheme
/// narrative and subgoal progression, which exist nowhere else -- not in
/// `get-event-data`, not in `gag-state`.
///
/// Re-enabling costs the user nothing, because prover text goes to a captured
/// sink rather than their terminal: they never see the flood, only the
/// structured envelope. Only PROVE is removed, so any other inhibition they
/// chose is respected, and gag-mode is left alone -- `:goals` still yields the
/// induction narrative while avoiding the six-figure waterfall dumps.
///
/// The two recorded values travel in a BEGIN/END block, not one-line markers:
/// ACL2's fmt wraps `~x` at the right margin, and a one-line marker whose
/// value wraps loses its `>>>` and silently drops the datum. A books path
/// deeper than the margin did exactly that, leaving `books_dir` empty and
/// `certify` falling back to the checkout walk. The block parser tolerates
/// wrapping by construction.
const OWN_OUTPUT: &str = "(acl2::prog2$ \
    (acl2::cw \"~%<<<ASS-BEGIN env>>>~%inhibit ~x0~%sbd ~x1~%<<<ASS-END env>>>~%\" \
      (acl2::@ acl2::inhibit-output-lst) \
      (acl2::system-books-dir acl2::state)) \
    (acl2::er-progn \
      (acl2::set-inhibit-output-lst \
        (acl2::remove-eq 'acl2::prove (acl2::@ acl2::inhibit-output-lst))) \
      (acl2::value acl2::nil)))";

/// Report the command number and current package, one short marker each.
///
/// `cmd` is how every fast path is verified (P3): rollback and eval-undo
/// check the world landed where the record says. `pkg` records the one thing
/// `ubu!` cannot restore -- the reader's package is state, not world -- so
/// rollback re-establishes it explicitly. Both values are short; the one-line
/// marker protocol is safe for them (see `Submission::data`).
const CMD_PROBE: &str = "(acl2::prog2$ \
    (acl2::cw \"~%<<<ASS-DATA cmd ~x0>>>~%<<<ASS-DATA pkg ~x1>>>~%\" \
      (acl2::max-absolute-command-number (acl2::w acl2::state)) \
      (acl2::f-get-global 'acl2::current-package acl2::state)) \
    (acl2::value :invisible))";

/// Report the events ACL2 recorded *inside* this unit.
///
/// `event-data-fal` is a state global, off by default, that records each
/// `defthm`/`defun`/`verify-guards`/`thm` keyed by name -- including events
/// nested inside a compound form, which is exactly what `get-event-data`
/// cannot see. Enabling it costs nothing in the world, and resetting it per
/// unit (see `CLEAR_STALE`) both isolates units from each other and stops it
/// growing without bound.
///
/// The alist is a fast-alist and therefore *improper*: its final cdr is the
/// symbol `event-data-fal` rather than `nil`, so `strip-cars` guard-fails on
/// it. Hence the `consp`-guarded walk. Entries are pushed newest-first, and a
/// fixed number of slots is read for the same reason as the checkpoints:
/// iterating would need a recursive helper, and defining one would put it in
/// the world.
/// Note the `true-list-fix` around `event-data-fal`. That global is a fast
/// alist whose final cdr is the symbol naming it, so it is never `alistp` --
/// and `CLEAR_STALE` blanks it to that bare symbol, so it is sometimes not a
/// cons at all. `strip-cars` needs `alistp`, and an unguarded call is a guard
/// violation that fails the *unit* the probe is spliced into, not just the
/// probe. Measured: `(alistp (@ event-data-fal))` is `nil` after two events,
/// while `(strip-cars (true-list-fix ...))` gives both names.
///
/// Per detail slot, alongside the rules and abort causes: the event's kind
/// (from `event`, falling back to `form` when its head is a symbol -- the
/// `verify-guards` entry records its form as a fmt string), whether a `namex`
/// was recorded (the failing entry is the one without), and the entry's own
/// `time` and `prover-steps-counted` -- which member of a clique was slow,
/// which the aggregate outer numbers cannot say.
const INNER_EVENTS_PROBE: &str = r#"(acl2::let*
  ((acl2::f0 (acl2::@ acl2::event-data-fal))
   (acl2::e0 (acl2::and (acl2::consp acl2::f0) (acl2::car acl2::f0)))
   (acl2::f1 (acl2::and (acl2::consp acl2::f0) (acl2::cdr acl2::f0)))
   (acl2::e1 (acl2::and (acl2::consp acl2::f1) (acl2::car acl2::f1)))
   (acl2::f2 (acl2::and (acl2::consp acl2::f1) (acl2::cdr acl2::f1)))
   (acl2::e2 (acl2::and (acl2::consp acl2::f2) (acl2::car acl2::f2)))
   (acl2::d0 (acl2::and (acl2::consp acl2::e0) (acl2::car (acl2::cdr acl2::e0))))
   (acl2::d1 (acl2::and (acl2::consp acl2::e1) (acl2::car (acl2::cdr acl2::e1))))
   (acl2::d2 (acl2::and (acl2::consp acl2::e2) (acl2::car (acl2::cdr acl2::e2))))
   (acl2::ev0 (acl2::cdr (acl2::assoc-eq 'acl2::event acl2::d0)))
   (acl2::ev1 (acl2::cdr (acl2::assoc-eq 'acl2::event acl2::d1)))
   (acl2::ev2 (acl2::cdr (acl2::assoc-eq 'acl2::event acl2::d2)))
   (acl2::fo0 (acl2::cdr (acl2::assoc-eq 'acl2::form acl2::d0)))
   (acl2::fo1 (acl2::cdr (acl2::assoc-eq 'acl2::form acl2::d1)))
   (acl2::fo2 (acl2::cdr (acl2::assoc-eq 'acl2::form acl2::d2))))
  (acl2::prog2$
    (acl2::cw "~%<<<ASS-BEGIN inner>>>~%")
    (acl2::prog2$
      (acl2::cw "~*0" (acl2::list "" "iname ~x*~%" "iname ~x*~%" "iname ~x*~%"
                        (acl2::strip-cars (acl2::true-list-fix acl2::f0))))
    (acl2::prog2$
      (acl2::cw "~*0" (acl2::list "" "r0rune ~x*~%" "r0rune ~x*~%" "r0rune ~x*~%"
                        (acl2::cdr (acl2::assoc-eq 'acl2::rules acl2::d0))))
      (acl2::prog2$
        (acl2::cw "~*0" (acl2::list "" "r1rune ~x*~%" "r1rune ~x*~%" "r1rune ~x*~%"
                          (acl2::cdr (acl2::assoc-eq 'acl2::rules acl2::d1))))
        (acl2::prog2$
          (acl2::cw "~*0" (acl2::list "" "r2rune ~x*~%" "r2rune ~x*~%" "r2rune ~x*~%"
                            (acl2::cdr (acl2::assoc-eq 'acl2::rules acl2::d2))))
          (acl2::prog2$
            (acl2::cw "a0 ~x0~%a1 ~x1~%a2 ~x2~%k0 ~x3~%k1 ~x4~%k2 ~x5~%nx0 ~x6~%nx1 ~x7~%nx2 ~x8~%"
              (acl2::cdr (acl2::assoc-eq 'acl2::abort-causes acl2::d0))
              (acl2::cdr (acl2::assoc-eq 'acl2::abort-causes acl2::d1))
              (acl2::cdr (acl2::assoc-eq 'acl2::abort-causes acl2::d2))
              (acl2::or (acl2::and (acl2::consp acl2::ev0) (acl2::car acl2::ev0))
                        (acl2::and (acl2::consp acl2::fo0)
                                   (acl2::symbolp (acl2::car acl2::fo0))
                                   (acl2::car acl2::fo0)))
              (acl2::or (acl2::and (acl2::consp acl2::ev1) (acl2::car acl2::ev1))
                        (acl2::and (acl2::consp acl2::fo1)
                                   (acl2::symbolp (acl2::car acl2::fo1))
                                   (acl2::car acl2::fo1)))
              (acl2::or (acl2::and (acl2::consp acl2::ev2) (acl2::car acl2::ev2))
                        (acl2::and (acl2::consp acl2::fo2)
                                   (acl2::symbolp (acl2::car acl2::fo2))
                                   (acl2::car acl2::fo2)))
              (acl2::if (acl2::assoc-eq 'acl2::namex acl2::d0) 'acl2::yes 'acl2::no)
              (acl2::if (acl2::assoc-eq 'acl2::namex acl2::d1) 'acl2::yes 'acl2::no)
              (acl2::if (acl2::assoc-eq 'acl2::namex acl2::d2) 'acl2::yes 'acl2::no))
            (acl2::prog2$
              (acl2::cw "t0 ~x0~%t1 ~x1~%t2 ~x2~%s0 ~x3~%s1 ~x4~%s2 ~x5~%<<<ASS-END inner>>>~%"
                (acl2::cdr (acl2::assoc-eq 'acl2::time acl2::d0))
                (acl2::cdr (acl2::assoc-eq 'acl2::time acl2::d1))
                (acl2::cdr (acl2::assoc-eq 'acl2::time acl2::d2))
                (acl2::cdr (acl2::assoc-eq 'acl2::prover-steps-counted acl2::d0))
                (acl2::cdr (acl2::assoc-eq 'acl2::prover-steps-counted acl2::d1))
                (acl2::cdr (acl2::assoc-eq 'acl2::prover-steps-counted acl2::d2)))
              (acl2::value :invisible)))))))))"#;

/// ACL2 prints a string with `~x` in quotes; the value is the text inside.
fn unquote(v: &str) -> Option<String> {
    let t = v.trim();
    t.strip_prefix('"').and_then(|t| t.strip_suffix('"')).map(str::to_string)
}

/// The package a unit switches to, when the unit is a bare top-level
/// `(in-package "P")` and nothing else.
///
/// Lexical on purpose: this decides only whether the *second phase* runs, and
/// the second phase re-issues a form the wrapper already admitted. The head
/// symbol may arrive qualified or oddly cased. A package name containing `"`
/// or `\` is declined rather than escaped -- such a unit then simply behaves
/// as before the second phase existed (the switch reverts).
fn in_package_target(text: &[u8]) -> Option<String> {
    let n = crate::split::normalize(text);
    let s = std::str::from_utf8(&n).ok()?.trim().to_string();
    let body = s.strip_prefix('(')?.strip_suffix(')')?;
    let mut it = body.splitn(2, ' ');
    if symbol_name_of(it.next()?) != "in-package" {
        return None;
    }
    let name = it.next()?.trim();
    let name = name.strip_prefix('"')?.strip_suffix('"')?;
    if name.is_empty() || name.contains('"') || name.contains('\\') {
        return None;
    }
    Some(name.to_string())
}

/// How many inner events the probe reads detail (abort causes, rules) for.
///
/// Names are complete however many there are; this bounds only the detail.
/// A consumer is told where it stops, because "no rules" and "rules not read"
/// look identical otherwise -- the silent-incompleteness trap that made a
/// three-name `inner` list read as the whole story.
///
/// Three is not a magic number: fmt indexes `~x0` to `~x9`, so one `cw` carried
/// three events' worth of fields plus a marker. It stays because the detail
/// that matters is always in reach -- `event-data-fal` is newest-first and a
/// compound form stops at the event that failed, so that event is at index 0.
/// Detail for the rest is per-event in the captured log: search a form's
/// output for `Rules:` and it is there under a `Form:` header, 13 of them for
/// a `defines` clique.
pub const INNER_DETAIL_SLOTS: usize = 3;

/// Every value recorded under a repeated key, with ACL2's wrapping undone.
///
/// The probes print list-valued data one element per line with a distinguishing
/// prefix, rather than letting `~x` lay out the whole list. A long rune still
/// wraps, but only onto lines that do not carry the prefix, so the item
/// boundaries survive regardless of the width ACL2 formatted for -- which is
/// what makes this a list we can hand over as a list, instead of a picture of
/// one that a client would have to parse back.
fn items(pairs: &[(String, String)], key: &str) -> Vec<String> {
    pairs
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|v| !v.is_empty() && !is_nil(v))
        .collect()
}

/// A probe-printed symbol, case- and package-insensitively: `DEFUN`,
/// `ACL2::DEFUN` and `defun` are all the same answer. Symbols print
/// package-qualified whenever the session's current package does not see
/// them, so a parser keying on the printed text must strip the qualifier.
fn symbol_name_of(v: &str) -> String {
    let t = v.trim();
    let t = t.rsplit("::").next().unwrap_or(t);
    t.to_ascii_lowercase()
}

fn parse_inner(sub: &crate::session::Submission) -> (Vec<InnerEvent>, usize) {
    let Some(body) = sub.block("inner") else { return (Vec::new(), 0) };
    let pairs = keyed_block(body, &["iname", "a0", "a1", "a2",
                                    "r0rune", "r1rune", "r2rune",
                                    "k0", "k1", "k2", "nx0", "nx1", "nx2",
                                    "t0", "t1", "t2", "s0", "s1", "s2"]);
    let get = |k: &str| pairs.iter().find(|(pk, _)| pk == k)
        .map(|(_, v)| v.trim().to_string()).filter(|v| !is_nil(v));
    // Every name, not a sample of them. A `defines` clique introduces far more
    // than three events, and a list of three that looks complete is worse than
    // a long one: the reporter found two of a clique's own functions in the
    // world and absent from this list, with nothing to say it was truncated.
    let names = items(&pairs, "iname");
    // Names are complete; the per-event detail is read for the first few.
    //
    // That number is a leftover -- fmt indexes `~x0` to `~x9`, so one `cw`
    // carried three events' worth -- but it costs nothing where it matters.
    // `event-data-fal` is newest-first and a compound form stops at the event
    // that failed, so **the failing event is always at index 0**. Measured on
    // a `progn` of four theorems whose fourth fails: names come back
    // `(p4-bogus p3 p2 p1)` with the abort cause at slot 0, not slot 3.
    //
    // What the limit does cost is the detail of *non-failing* inner events
    // past the third, on a form that failed. Raising it means more `~*` passes,
    // one per slot. Either way a reader must be told where the detail stops,
    // since an entry with no rules is otherwise indistinguishable from one
    // whose rules were never read -- see `INNER_DETAIL_SLOTS`.
    let detail = [("a0", "r0rune", "k0", "nx0", "t0", "s0"),
                  ("a1", "r1rune", "k1", "nx1", "t1", "s1"),
                  ("a2", "r2rune", "k2", "nx2", "t2", "s2")];
    debug_assert_eq!(detail.len(), INNER_DETAIL_SLOTS);
    let mut out = Vec::new();
    for (i, name) in names.iter().enumerate() {
        // Names are cheap and complete; the per-event detail is not, and is
        // read for the first few only.
        let d = detail.get(i).copied();
        out.push(InnerEvent {
            name: name.clone(),
            abort_causes: d.and_then(|(a, ..)| get(a)),
            rules: d.map_or(Vec::new(), |(_, r, ..)| items(&pairs, r)),
            kind: d.and_then(|(_, _, k, ..)| get(k)).map(|v| symbol_name_of(&v)),
            // Printed as yes/no rather than t/nil, because a nil would be
            // indistinguishable here from the value having gone missing.
            has_namex: d
                .and_then(|(_, _, _, nx, ..)| get(nx))
                .map(|v| symbol_name_of(&v) == "yes"),
            time: d
                .and_then(|(.., t, _)| get(t))
                .map(|v| v.split_whitespace().collect::<Vec<_>>().join(" ")),
            prover_steps: d.and_then(|(.., s)| get(s)).and_then(|v| v.parse().ok()),
        });
    }
    let total = out.len();
    (out, total)
}

/// Report what ACL2 recorded about the event just executed.
///
/// Must run *before* the `deflabel`: `get-event-data` describes the
/// immediately preceding event, and planting a label first would make it
/// describe the label.
const EVENT_DATA_PROBE: &str = r#"(acl2::prog2$
  (acl2::cw "~%<<<ASS-BEGIN evd>>>~%steps ~x0~%warnings ~x1~%time ~x2~%"
    (acl2::get-event-data 'acl2::prover-steps-counted acl2::state)
    (acl2::get-event-data 'acl2::warnings acl2::state)
    (acl2::get-event-data 'acl2::time acl2::state))
  (acl2::prog2$
    (acl2::cw "~*0"
      (acl2::list "" "rule ~x*~%" "rule ~x*~%" "rule ~x*~%"
        (acl2::get-event-data 'acl2::rules acl2::state)))
    (acl2::prog2$
      (acl2::cw "~*0"
        (acl2::list "" "hint ~x*~%" "hint ~x*~%" "hint ~x*~%"
          (acl2::get-event-data 'acl2::hint-events acl2::state)))
      (acl2::prog2$
        (acl2::cw "~*0"
          (acl2::list "" "splitter ~x*~%" "splitter ~x*~%" "splitter ~x*~%"
            (acl2::get-event-data 'acl2::splitter-rules acl2::state)))
        (acl2::prog2$
          (acl2::cw "<<<ASS-END evd>>>~%")
          (acl2::value :invisible))))))"#;

/// Report the key checkpoints of the most recent failed proof.
///
/// Deliberately introduces **nothing** into the ACL2 world: it is one
/// expression built from `access`, `nth` and `prettyify-clause`, all ACL2
/// built-ins. An earlier version included
/// `books/kestrel/utilities/checkpoints`, which was simpler but added fourteen
/// function definitions -- each carrying definition and type-prescription
/// runes -- to a world that is supposed to look like a plain interactive
/// session. Nothing would ever have mentioned those functions, but "would
/// never" is not a guarantee, and the names were a real collision surface.
///
/// Split across several `cw` calls because ACL2's fmt directives index
/// arguments `~x0` to `~x9` only -- and `~xa` is not an argument reference at
/// all.
///
/// Each slot reports its goal *name* alongside the term, rendered by
/// `tilde-@-clause-id-phrase` from the gag-info's clause-id -- `Subgoal
/// *1/2''` exactly as ACL2 prints it, which is exactly what a goal-addressed
/// output slice takes. Empty when the slot is empty, so the parser can tell
/// "no name" from "no checkpoint".
///
/// A fixed number of slots is read rather than mapping over the stacks,
/// because `state` is not available inside a `loop$` body and defining a
/// recursive helper would reintroduce exactly the world modification being
/// avoided. `nth` past the end returns `nil` and the whole chain degrades to
/// `NIL`, so out-of-range slots are harmless. The true stack lengths are
/// reported alongside, so a client can tell when there were more.
/// Key checkpoints, read from the saved gag-state.
///
/// The top-level stack is taken from `abort-info` in preference to
/// `:top-stack`, which is what ACL2's own printer does
/// (`history-management.lisp`: `(top-stack (or abort-stack top-stack0))`).
/// The two differ exactly when a proof reverts to induction: ACL2 then
/// replaces `:top-stack` with a placeholder whose clause is the literal
/// symbol `<goal>` and moves the real pre-revert checkpoints into
/// `abort-info`. Reading `:top-stack` alone therefore reported `<goal>` as
/// the checkpoint while the actual term sat in the log.
const CHECKPOINT_PROBE: &str = r#"(acl2::let*
  ((acl2::gs (acl2::f-get-global 'acl2::gag-state-saved acl2::state))
   (acl2::ai (acl2::access acl2::gag-state acl2::gs :abort-info))
   (acl2::as (acl2::abort-info-stack acl2::ai))
   (acl2::tp (acl2::or acl2::as
                       (acl2::access acl2::gag-state acl2::gs :top-stack)))
   (acl2::sb (acl2::access acl2::gag-state acl2::gs :sub-stack))
   (acl2::wr (acl2::w acl2::state)))
  (acl2::prog2$
    (acl2::cw "~%<<<ASS-BEGIN ckpt>>>~%gag ~x0~%ntop ~x1~%nsub ~x2~%forced ~x3~%abort ~x4~%reverted ~x5~%"
      (acl2::if acl2::gs acl2::t acl2::nil)
      (acl2::len acl2::tp)
      (acl2::len acl2::sb)
      (acl2::access acl2::gag-state acl2::gs :forcep)
      (acl2::abort-info-cause acl2::ai)
      (acl2::if acl2::as acl2::t acl2::nil))
    (acl2::prog2$
      (acl2::cw "top0 ~x0~%top1 ~x1~%top2 ~x2~%sub0 ~x3~%sub1 ~x4~%sub2 ~x5~%"
        (acl2::prettyify-clause (acl2::access acl2::gag-info (acl2::nth 0 acl2::tp) :clause) acl2::nil acl2::wr)
        (acl2::prettyify-clause (acl2::access acl2::gag-info (acl2::nth 1 acl2::tp) :clause) acl2::nil acl2::wr)
        (acl2::prettyify-clause (acl2::access acl2::gag-info (acl2::nth 2 acl2::tp) :clause) acl2::nil acl2::wr)
        (acl2::prettyify-clause (acl2::access acl2::gag-info (acl2::nth 0 acl2::sb) :clause) acl2::nil acl2::wr)
        (acl2::prettyify-clause (acl2::access acl2::gag-info (acl2::nth 1 acl2::sb) :clause) acl2::nil acl2::wr)
        (acl2::prettyify-clause (acl2::access acl2::gag-info (acl2::nth 2 acl2::sb) :clause) acl2::nil acl2::wr))
      (acl2::prog2$
        (acl2::cw "tid0 ~@0~%tid1 ~@1~%tid2 ~@2~%sid0 ~@3~%sid1 ~@4~%sid2 ~@5~%<<<ASS-END ckpt>>>~%"
          (acl2::if (acl2::nth 0 acl2::tp)
            (acl2::tilde-@-clause-id-phrase (acl2::access acl2::gag-info (acl2::nth 0 acl2::tp) :clause-id)) "")
          (acl2::if (acl2::nth 1 acl2::tp)
            (acl2::tilde-@-clause-id-phrase (acl2::access acl2::gag-info (acl2::nth 1 acl2::tp) :clause-id)) "")
          (acl2::if (acl2::nth 2 acl2::tp)
            (acl2::tilde-@-clause-id-phrase (acl2::access acl2::gag-info (acl2::nth 2 acl2::tp) :clause-id)) "")
          (acl2::if (acl2::nth 0 acl2::sb)
            (acl2::tilde-@-clause-id-phrase (acl2::access acl2::gag-info (acl2::nth 0 acl2::sb) :clause-id)) "")
          (acl2::if (acl2::nth 1 acl2::sb)
            (acl2::tilde-@-clause-id-phrase (acl2::access acl2::gag-info (acl2::nth 1 acl2::sb) :clause-id)) "")
          (acl2::if (acl2::nth 2 acl2::sb)
            (acl2::tilde-@-clause-id-phrase (acl2::access acl2::gag-info (acl2::nth 2 acl2::sb) :clause-id)) ""))
        (acl2::value :invisible)))))"#;

/// What ACL2 recorded about the form that was just interrupted.
///
/// `last-event-data` survives an interrupt on current ACL2: measured,
/// `(abort-causes interrupt)`, the true elapsed prove time, and the
/// interrupted event are all present. (An earlier measurement on an older
/// build recorded the contrary; it no longer holds.) The event form can be
/// arbitrarily large, so it prints under an evisc tuple -- this is an
/// envelope, not the record; the record is the partial capture.
///
/// The caller trusts this only when `iabort` actually names an interrupt.
/// Sync units and evals both run `CLEAR_STALE` first, so the record is
/// normally fresh; the gate stays as belt-and-braces for the paths that do
/// not clear (a raw submission, a handshake).
const INTERRUPT_PROBE: &str = r#"(acl2::prog2$
  (acl2::cw "~%<<<ASS-BEGIN intr>>>~%iabort ~x0~%itime ~x1~%isteps ~x2~%"
    (acl2::get-event-data 'acl2::abort-causes acl2::state)
    (acl2::get-event-data 'acl2::time acl2::state)
    (acl2::get-event-data 'acl2::prover-steps-counted acl2::state))
  (acl2::prog2$
    (acl2::cw "ievent ~Y01~%<<<ASS-END intr>>>~%"
      (acl2::or (acl2::get-event-data 'acl2::event acl2::state)
                (acl2::get-event-data 'acl2::form acl2::state))
      (acl2::evisc-tuple 3 8 acl2::nil acl2::nil))
    (acl2::value :invisible)))"#;

/// Split a `key value` block into pairs, tolerating values that ACL2 wrapped
/// across lines.
fn keyed_block(body: &str, keys: &[&str]) -> Vec<(String, String)> {
    let mut cur: Option<(String, Vec<String>)> = None;
    let mut done: Vec<(String, String)> = Vec::new();
    for line in body.lines() {
        let hit = keys.iter().find(|k| {
            line.strip_prefix(**k).is_some_and(|r| r.starts_with(' '))
        });
        if let Some(k) = hit {
            if let Some((pk, v)) = cur.take() {
                done.push((pk, v.join("\n")));
            }
            cur = Some((k.to_string(), vec![line[k.len() + 1..].trim_end().to_string()]));
        } else if let Some((_, v)) = cur.as_mut() {
            v.push(line.trim_end().to_string());
        }
    }
    if let Some((pk, v)) = cur {
        done.push((pk, v.join("\n")));
    }
    done
}

fn is_nil(v: &str) -> bool {
    let t = v.trim();
    t.is_empty() || t.eq_ignore_ascii_case("nil")
}

fn parse_checkpoints(sub: &crate::session::Submission) -> Checkpoints {
    let mut c = Checkpoints::default();
    let Some(body) = sub.block("ckpt") else { return c };
    let keys = ["gag", "ntop", "nsub", "forced", "abort", "reverted",
                "top0", "top1", "top2", "sub0", "sub1", "sub2",
                "tid0", "tid1", "tid2", "sid0", "sid1", "sid2"];
    let pairs = keyed_block(body, &keys);
    let get = |k: &str| pairs.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.trim().to_string());
    c.proof_ran = get("gag").is_some_and(|v| !is_nil(&v));
    c.n_top = get("ntop").and_then(|v| v.parse().ok()).unwrap_or(0);
    c.n_sub = get("nsub").and_then(|v| v.parse().ok()).unwrap_or(0);
    c.forced = get("forced").is_some_and(|v| !is_nil(&v));
    c.abort_cause = get("abort").filter(|v| !is_nil(v));
    c.pre_revert = get("reverted").is_some_and(|v| !is_nil(&v));
    // The goal name arrives on one line; collapse any residual whitespace so
    // it can be handed straight to a goal-addressed output slice.
    let goal_of = |k: &str| {
        get(k)
            .map(|g| g.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|g| !g.is_empty() && !is_nil(g))
    };
    // A term that prettyprints as the literal `nil` is dropped here while
    // `n_top`/`n_sub` still count it -- the counts are authoritative, the
    // filter is what keeps genuinely empty slots out.
    for (term_k, id_k) in [("top0", "tid0"), ("top1", "tid1"), ("top2", "tid2")] {
        if let Some(v) = get(term_k).filter(|v| !is_nil(v)) {
            c.top.push(Checkpoint { goal: goal_of(id_k), term: v });
        }
    }
    for (term_k, id_k) in [("sub0", "sid0"), ("sub1", "sid1"), ("sub2", "sid2")] {
        if let Some(v) = get(term_k).filter(|v| !is_nil(v)) {
            c.sub.push(Checkpoint { goal: goal_of(id_k), term: v });
        }
    }
    c
}

/// The `ACL2 Error` message, with ACL2's line wrapping undone.
///
/// ACL2 fill-wraps its errors at the right margin, so taking one physical
/// line cut the message mid-sentence -- and worse, could keep a `See :DOC`
/// while dropping the topic it pointed at. The message runs to the next blank
/// line, which is where ACL2 ends the paragraph.
///
/// The *last* error paragraph, not the first. A unit can print errors without
/// failing -- `must-fail` and the std testing utilities do exactly that, and
/// they sit inside `progn`s in real books -- so the first error in a compound
/// unit may be an expected decoy. The genuine failure is the one the `ld`
/// stopped at, and nothing prints an error after it.
///
/// With one exception: a `.cert.out` ends with certification's own wrap-up
/// (`ACL2 Error [Failure] in (CERTIFY-BOOK ...): See :DOC failure.`), printed
/// *after* the event's real error. That summary carries no information, so
/// the last error that is not one is preferred, and the summary is only ever
/// reported when nothing better exists.
pub(crate) fn error_message(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    let at = lines
        .iter()
        .rposition(|l| l.contains("ACL2 Error") && !l.contains("CERTIFY-BOOK"))
        .or_else(|| lines.iter().rposition(|l| l.contains("ACL2 Error")))?;
    let mut msg = lines[at].trim().to_string();
    for l in &lines[at + 1..] {
        if l.trim().is_empty() {
            break;
        }
        msg.push(' ');
        msg.push_str(l.trim());
    }
    Some(msg)
}

fn parse_event_data(sub: &crate::session::Submission) -> EventData {
    let mut e = EventData::default();
    let Some(body) = sub.block("evd") else { return e };
    // ACL2 wraps long values at its right margin, so a value runs until the
    // next known key rather than to end of line.
    let done = keyed_block(body, &["steps", "warnings", "time", "rule", "hint", "splitter"]);
    let pick = |k: &str| -> Option<String> {
        done.iter()
            .find(|(dk, _)| dk == k)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !is_nil(v))
    };
    // `parse::<i64>()`: the sign carries the step-limit signal. See the field.
    e.prover_steps = pick("steps").and_then(|v| v.parse().ok());
    e.rules = items(&done, "rule");
    e.hint_events = items(&done, "hint");
    e.splitter_rules = items(&done, "splitter");
    e.warnings = pick("warnings");
    e.time = pick("time");
    e
}

/// A session anchored to one file.
pub struct Book {
    acl2: PathBuf,
    path: PathBuf,
    session: Session,
    /// Shared with lock-free readers (abort, status). Created once here and
    /// threaded into every session this book starts, so a rebuild does not
    /// orphan the handle a registry already handed out.
    control: Arc<SessionControl>,
    admitted: Vec<Admitted>,
    label_gen: u64,
    rebuilds: usize,
    /// Captured output, shared so `acl2_output`-style reads never queue
    /// behind this book's lock: a client reading a *completed* form's log
    /// while a sync grinds gets it immediately. Session calls lock it
    /// briefly per operation.
    outputs: Arc<Mutex<OutputStore>>,
    /// The unit whose submission has not reported yet.
    pending: Option<PendingUnit>,
    /// Command number just after the base label, i.e. the empty prefix.
    base_command_number: Option<i64>,
    /// `current-package` at the empty prefix, as the customization left it.
    /// The rollback target when everything is undone.
    base_package: Option<String>,
    /// Set when an undo could not be verified; the next sync rebuilds.
    dirty: bool,
    /// What a sync does at a failed unit. See [`SyncMode`].
    mode: SyncMode,
    /// `inhibit-output-lst` as inherited, before the server removed PROVE.
    inherited_inhibit: Option<String>,
    /// The handle handed out for a form that outran its deadline.
    ///
    /// Held so the form's output lands under the handle the caller already
    /// has, rather than under a fresh one while the original stays empty
    /// forever. Cleared once filled.
    in_flight_output: Option<OutputRef>,
    /// Where this image keeps its books, as ACL2 itself reports it.
    ///
    /// Asked rather than inferred. Walking up from the image looking for
    /// `acl2.lisp` finds the checkout only when the image still sits inside
    /// one; an ACL2 installed or copied elsewhere has no checkout above it,
    /// and yet still knows perfectly well where its books are. This is also
    /// what `cert.pl` itself falls back on.
    books_dir: Option<PathBuf>,
    /// A `cert.pl` run that has not finished. Certification happens outside
    /// the session -- it must not touch the world the file prefix defines --
    /// so it is tracked here rather than by `Session`.
    build: Option<crate::cert::Build>,
}

/// Enough to finish recording a unit whose deadline expired mid-flight.
#[derive(Debug, Clone)]
struct PendingUnit {
    index: usize,
    hash: u64,
    line_start: usize,
    line_end: usize,
    label: u64,
    /// The package this unit switches to, when it is a top-level
    /// `(in-package ...)` that will need its second phase on completion.
    /// Detected at submission, because the file may change on disk while the
    /// unit runs.
    in_package: Option<String>,
    /// The unit's text exactly as submitted. A `ContinuePastFailure` retry
    /// at resume time must resubmit precisely what failed, and the snapshot
    /// it came from may be gone by then -- the file changed on disk, or the
    /// buffer moved on.
    text: String,
    /// This submission *is* the skip-proofs retry. Its failure is terminal:
    /// never retry a retry.
    skipped: bool,
}

impl Book {
    /// Open `path` and start a session in its directory.
    pub fn open(acl2: &Path, path: &Path) -> Result<Book, SyncError> {
        // `Path::new("book.lisp").parent()` is `Some("")`, not `None`, and a
        // process cannot be started with an empty working directory. Any path
        // with no directory component means the current one.
        let dir = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        if !path.is_file() {
            return Err(SyncError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no such book: {}", path.display()),
            )));
        }
        let control = SessionControl::new();
        let session = Session::start(acl2, dir, control.clone())?;
        let mut b = Book {
            acl2: acl2.to_path_buf(),
            path: path.to_path_buf(),
            session,
            control,
            admitted: Vec::new(),
            label_gen: 0,
            rebuilds: 0,
            outputs: Arc::new(Mutex::new(OutputStore::new()?)),
            pending: None,
            base_command_number: None,
            base_package: None,
            dirty: false,
            mode: SyncMode::StopAtFailure,
            inherited_inhibit: None,
            books_dir: None,
            in_flight_output: None,
            build: None,
        };
        b.post_start_setup()?;
        Ok(b)
    }

    /// Everything a fresh ACL2 needs before its first unit.
    ///
    /// One function, called from `open` and from `rebuild`, so the recovery
    /// path cannot drift from the first-start path. A rebuild that skipped
    /// `OWN_OUTPUT` quietly re-inhibited PROVE under a `(quiet)`
    /// customization -- losing the induction narrative from every capture
    /// after the first rebuild, a failure already on record,
    /// reintroduced by the recovery path -- and kept reporting the
    /// pre-rebuild environment answers.
    fn post_start_setup(&mut self) -> Result<(), SyncError> {
        let sub = self.session.submit(OWN_OUTPUT, Duration::from_secs(60))?;
        if let Some(body) = sub.block("env") {
            let pairs = keyed_block(body, &["inhibit", "sbd"]);
            let get = |k: &str| {
                pairs
                    .iter()
                    .find(|(pk, _)| pk == k)
                    .map(|(_, v)| v.trim().to_string())
            };
            // The inhibit list may wrap; collapse it back to one line.
            self.inherited_inhibit =
                get("inhibit").map(|v| v.split_whitespace().collect::<Vec<_>>().join(" "));
            // A path is a single string atom: fmt may break *before* it, but
            // never inside it, so after the join it is whole.
            self.books_dir = get("sbd").as_deref().and_then(unquote).map(PathBuf::from);
        }
        // A baseline label, so "roll back to the empty prefix" is the same
        // operation as any other rollback rather than a special case.
        self.plant_base_label()
    }

    pub fn session(&mut self) -> &mut Session {
        &mut self.session
    }

    /// The control block shared with lock-free readers. See `SessionControl`.
    pub fn control(&self) -> Arc<SessionControl> {
        self.control.clone()
    }

    /// Set how syncs treat a failed unit. See [`SyncMode`].
    ///
    /// A property of the book rather than of a call, because `resume` must
    /// honor it too: a unit that outruns its deadline and then fails at
    /// resume gets the same skip-proofs retry, or the next sync would
    /// re-prove the failure from scratch. Per-call flags would let sync and
    /// resume be called inconsistently and reopen exactly that hole.
    pub fn set_sync_mode(&mut self, mode: SyncMode) {
        self.mode = mode;
    }

    /// The output store, shared for the same reason as the control block:
    /// reading a captured log must not wait behind whatever call holds this
    /// book. Callers lock it per operation.
    pub fn output_store(&self) -> Arc<Mutex<OutputStore>> {
        self.outputs.clone()
    }

    fn outs(&self) -> MutexGuard<'_, OutputStore> {
        // A poisoned store means a panic mid-write elsewhere; the index and
        // files are still readable, and refusing every later read would
        // compound the damage.
        self.outputs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How many times this book has been rebuilt by replay.
    ///
    /// Expected to stay at zero. A non-zero and growing count means undo is
    /// not behaving as assumed, which is a problem in its own right: replay is
    /// correct but expensive, and a session doing it routinely is doing a
    /// great deal of redundant proving.
    pub fn rebuilds(&self) -> usize {
        self.rebuilds
    }

    /// Package-qualified, and that is load-bearing: a label name interns in
    /// whatever package is current when its `deflabel` is *read*, and the
    /// session's package can now change mid-file (a sticky `in-package`).
    /// Unqualified names would put labels planted before and after the switch
    /// in different packages, and a rollback across the switch would then ask
    /// `ubu!` for a symbol that names nothing.
    fn label_name(genid: u64) -> String {
        format!("acl2::acl2-session-mark-{genid}")
    }

    fn plant_base_label(&mut self) -> Result<(), SyncError> {
        let body = format!(
            "(acl2::with-output :off :all (acl2::deflabel {}))\n{CMD_PROBE}",
            Self::label_name(0)
        );
        let sub = self.session.submit(&body, Duration::from_secs(60))?;
        self.base_command_number = sub.get("cmd").and_then(|v| v.parse().ok());
        self.base_package = sub.get("pkg").and_then(unquote);
        Ok(())
    }

    /// The session's persistent package with the current prefix: what the
    /// last admitted unit left current, or the base if nothing is admitted.
    fn current_package(&self) -> Option<&str> {
        match self.admitted.last() {
            Some(a) => a.package.as_deref(),
            None => self.base_package.as_deref(),
        }
    }

    /// `inhibit-output-lst` as the user's environment left it, before the
    /// server removed PROVE from it.
    ///
    /// Exposed so a client rendering the log to a human can respect what the
    /// user actually asked for. The server over-captures on purpose -- PROVE
    /// inhibition silently disables output saving, losing the induction
    /// narrative permanently -- but over-capturing is not a licence to
    /// over-display.
    pub fn inherited_inhibit_output(&self) -> Option<&str> {
        self.inherited_inhibit.as_deref()
    }

    /// Whether the server had to re-enable PROVE output to capture proofs.
    ///
    /// True means the log contains prover output the user's own session would
    /// have suppressed; a human-facing client should say so rather than just
    /// showing it.
    pub fn prove_output_overridden(&self) -> bool {
        self.inherited_inhibit
            .as_deref()
            .is_some_and(|v| v.to_lowercase().contains("prove"))
    }

    /// The command number the world should be at with the current prefix.
    fn expected_command_number(&self) -> Option<i64> {
        match self.admitted.last() {
            Some(a) => a.command_number,
            None => self.base_command_number,
        }
    }

    /// Evaluate a form without letting it change the logical world.
    ///
    /// This is the path for everything that is not part of the file: queries
    /// (`pe`, `pl`, `pr`, `disabledp`), session settings, and throwaway
    /// experiments such as `thm`. Forcing those through the file would need two
    /// edits per query, pollute the source, and -- decisively -- make a query
    /// part of the prefix invariant, since `sync` would try to *admit* it.
    ///
    /// The rule is precise: **eval cannot change the world, but may change
    /// state**. `ubt!` undoes world changes; it does not undo `f-put-global`
    /// settings like `set-gag-mode` or `accumulated-persistence`, and that
    /// asymmetry is deliberate -- those are exactly the settings that should
    /// stick, and a debugging session that turns on accumulated persistence
    /// then asks for the results would break if they did not.
    ///
    /// World changes cannot be rejected statically, since macros and
    /// `make-event` expand to anything, so the form runs and is undone
    /// afterwards. No new label is needed: the prefix already ends in one, and
    /// that is precisely the world an eval should return to.
    ///
    /// Consequence: **world changes are not visible across consecutive evals.**
    pub fn eval(&mut self, form: &str, deadline: Duration) -> Result<EvalResult, SyncError> {
        if self.session.is_running() {
            return Err(SyncError::Busy { index: self.running_unit() });
        }
        // CLEAR_STALE for the same reason every sync unit gets it: a failing
        // eval must never inherit another form's checkpoints, and a
        // translate-level failure overwrites nothing. The cost is that
        // `gag-state-saved` and `last-event-data` from an earlier failure are
        // not visible to an eval'd form -- accepted, because the envelope
        // already delivered them structurally, and `:pso` (the post-mortem
        // that matters) reads `saved-output-reversed`, which is untouched.
        let body = format!("{CLEAR_STALE}\n{form}\n{CMD_PROBE}");
        let sub = self.session.submit(&body, deadline)?;
        self.finish_eval(sub, deadline)
    }

    /// Turn a completed (or timed-out) eval submission into a result, undoing
    /// any world change. Shared with `resume` so the two cannot drift.
    fn finish_eval(
        &mut self,
        sub: crate::session::Submission,
        deadline: Duration,
    ) -> Result<EvalResult, SyncError> {
        let output_ref = self.record_output(&sub)?;
        let output_lines = self.outs().lines(&output_ref).unwrap_or(0);
        // Probe before the undo: rolling back is itself a submission, and the
        // envelope must describe the form that failed, not the bookkeeping
        // after it. Floored like every post-mortem probe.
        let failed = if sub.outcome == Outcome::Failed {
            let (event_data, checkpoints, inner, inner_total) =
                self.probe_after_failure(deadline.max(PROBE_FLOOR));
            Some(EvalFailure {
                status: if checkpoints.proof_ran {
                    Status::ProofFailure
                } else {
                    Status::Rejected
                },
                error_line: error_message(&sub.output),
                event_data,
                checkpoints,
                inner,
                inner_total,
            })
        } else {
            None
        };
        let after: Option<i64> = match sub.get("cmd") {
            Some(v) => v.parse().ok(),
            // A failing form aborts its `ld` before the trailing probe runs,
            // so ask again. This is not merely bookkeeping: an eval string
            // may hold several top-level forms, and one can introduce events
            // before a later one fails -- skipping the check because the
            // marker never printed silently kept those events, which is a
            // world the file does not describe.
            None if sub.outcome == Outcome::Failed => self
                .session
                .submit(CMD_PROBE, deadline.max(PROBE_FLOOR))
                .ok()
                .and_then(|s| s.get("cmd").and_then(|v| v.parse().ok())),
            None => None,
        };

        let mut world_change = WorldChange::None;
        if let (Some(a), Some(e)) = (after, self.expected_command_number())
            && a != e
        {
            // Events were introduced. Undo back to the prefix, which is
            // where the last planted label sits. Floored: the undo must
            // run whatever is left of the deadline, because skipping it
            // leaves events the caller was promised would not persist.
            let keep = self.admitted.len();
            world_change = match self.rollback_to(keep, deadline.max(PROBE_FLOOR)) {
                Ok(()) => WorldChange::Undone,
                Err(SyncError::RollbackFailed { .. })
                | Err(SyncError::RollbackUnverified { .. }) => {
                    self.dirty = true;
                    WorldChange::Dirty
                }
                Err(e) => return Err(e),
            };
        }
        Ok(EvalResult {
            outcome: sub.outcome,
            world_change,
            failed,
            output_ref,
            output_lines,
            elapsed: sub.elapsed,
        })
    }

    /// Whether the session is answering, without ever making it worse.
    ///
    /// The failure answers are genuinely different actions:
    ///
    /// - `Busy` -- a form is in flight. Working as intended; wait, resume, or
    ///   interrupt. Asked of the session itself rather than of unit
    ///   bookkeeping, because an `eval` can be in flight with no unit attached.
    /// - `Unresponsive` -- nothing should be running, yet a trivial round trip
    ///   did not come back. The process is alive and its descriptor works, but
    ///   it is not answering: ACL2 may have dropped into a raw-Lisp debugger,
    ///   or something may have swallowed a completion sentinel. Recoverable by
    ///   interrupt or, failing that, by rebuild.
    ///
    ///   Observed on demand by stopping the child (SIGSTOP): alive,
    ///   descriptors intact, answering nothing -- and interrupt then reports
    ///   honestly that it could not help, since signals queue against a
    ///   stopped process. The verdict is about reachability, not damage: the
    ///   same session reads ready again once the stop is lifted.
    /// - `Dead` -- the process is gone. Only close-and-reopen will help, and
    ///   the check comes *first*, because a dead child otherwise wears `Busy`
    ///   forever: the reader sees EOF and returns, nothing ever completes,
    ///   and the bookkeeping keeps saying a form is in flight.
    ///
    /// Reporting `Dead` for a session that is merely busy would be the worst
    /// of these, since a client would discard a session that is working.
    ///
    /// The probe is a handshake, not a submission. A `submit` probe put the
    /// probe itself in flight, so timing out on a slow-but-alive session (a
    /// long GC) *wedged* it: the session then read as busy until something
    /// drained it, and a later resume reported the probe as a finished eval.
    /// A health check must not be able to wedge the session it checks.
    pub fn health(&mut self, deadline: Duration) -> Health {
        if !self.session.alive() {
            return Health::Dead;
        }
        if self.session.is_running() {
            return Health::Busy;
        }
        match self.session.ping(deadline) {
            Ok(true) => Health::Ready,
            Ok(false) => Health::Unresponsive,
            Err(_) => Health::Dead,
        }
    }

    /// Bring the session into agreement with the file on disk.
    ///
    /// Reads the current file contents and hands them to
    /// [`Book::sync_bytes`], which owns the semantics: where the snapshot
    /// comes from is the entire difference between the two calls.
    pub fn sync(
        &mut self,
        through_line: Option<usize>,
        deadline: Duration,
    ) -> Result<SyncOutcome, SyncError> {
        let bytes = std::fs::read(&self.path)?;
        self.sync_bytes(&bytes, through_line, deadline)
    }

    /// Bring the session into agreement with a caller-supplied snapshot.
    ///
    /// The editor entry. An editor's source of truth is its buffer, not its
    /// last save, so it hands the text in instead of having the disk read;
    /// `path` still anchors everything content does not decide -- the
    /// session's working directory, image resolution, `certify` -- and the
    /// invariant simply reads "snapshot" for "file": the world equals the
    /// result of admitting units `1..N` of the last synced snapshot.
    ///
    /// BYTES is the text exactly as a save would write it. Offsets, hashes
    /// and the splitter all work on bytes (see `split` for why: ACL2 source
    /// is not UTF-8), so a client that encodes its buffer the way it saves
    /// it gets hashes that survive a save round-trip. Getting the encoding
    /// wrong is safe -- the hashes differ and the affected suffix re-proves,
    /// wasteful but never wrong, like any other unrecognised change.
    ///
    /// File and buffer syncs interleave freely on one `Book`: nothing
    /// records where a snapshot came from, and every call diffs against the
    /// admitted hashes alone.
    ///
    /// `through_line` is optional and omitted means "to the end of the
    /// snapshot", which should be the common call: because sync stops at the
    /// first failure, the edit-fix-edit loop never needs a target.
    ///
    /// A target bounds how far *forward* to admit; it never retracts. Syncing
    /// through line 3 when six units are already admitted leaves all six in
    /// place and simply admits nothing new. Undoing is driven by the snapshot
    /// changing, not by asking for less of it.
    pub fn sync_bytes(
        &mut self,
        bytes: &[u8],
        through_line: Option<usize>,
        deadline: Duration,
    ) -> Result<SyncOutcome, SyncError> {
        if self.session.is_running() {
            return Err(SyncError::Busy { index: self.running_unit() });
        }
        // One budget for the whole call: units, rollbacks and probes all draw
        // it down, so the deadline bounds the call as documented rather than
        // each submission separately. (Session startup during a rebuild sits
        // outside it, bounded by its own fixed deadlines: a hung
        // customization file was never this call's proof work.)
        let budget = Budget::new(deadline);
        if self.dirty {
            // An undo we could not verify: rebuild rather than trust it. With
            // `keep` 0 nothing is replayed, so this cannot report in-flight;
            // rebuild itself clears the flag.
            self.rebuild(bytes, 0, &budget)?;
        }
        let units = split(bytes).map_err(SyncError::Split)?;

        let mut out = SyncOutcome::default();

        // Longest common prefix by hash. This is the whole diff: the client
        // never says what changed, we just re-derive it.
        //
        // The diff runs *before* the target is even resolved: rollback is
        // driven by the file changing, never by the target. Resolving first
        // and returning early left the whole stale world in place -- for a
        // `through_line` above the first unit, or a file emptied of forms --
        // while reporting `reused: 0` as though nothing were admitted.
        let hashes: Vec<u64> = self.admitted.iter().map(|a| a.hash).collect();
        let keep = common_prefix(&hashes, &units);
        out.reused = keep;

        if keep < self.admitted.len() {
            out.rolled_back = self.admitted.len() - keep;
            match self.rollback_to(keep, budget.floored()) {
                Ok(()) => {}
                // P3: a fast path that cannot be verified degrades to replay
                // rather than being trusted. Both an outright rejection and an
                // unverifiable landing get the same treatment.
                Err(SyncError::RollbackFailed { .. })
                | Err(SyncError::RollbackUnverified { .. }) => {
                    out.rebuilt = true;
                    if let Some(f) = self.rebuild(bytes, keep, &budget)? {
                        // The budget ran out mid-replay. The session holds a
                        // valid shorter prefix plus one unit in flight, which
                        // is exactly the shape of an ordinary deadline expiry:
                        // resume finishes the unit, the next sync continues.
                        out.failed = Some(f);
                        out.conditional_from = self.first_skipped();
                        return Ok(out);
                    }
                }
                Err(e) => return Err(e),
            }
        }

        let target = match resolve_target(&units, through_line) {
            Some(t) => t,
            None => {
                // Nothing selected; nothing to admit.
                out.conditional_from = self.first_skipped();
                return Ok(out);
            }
        };
        out.resolved_through = Some(target);

        for (i, u) in units.iter().enumerate().take(target + 1).skip(keep) {
            let (sub, output_ref) = self.admit_unit(u, i, bytes, budget.remaining())?;
            let output_lines = self.outs().lines(&output_ref).unwrap_or(0);
            match sub.outcome {
                Outcome::Ok => {
                    let (inner, inner_total) = parse_inner(&sub);
                    out.admitted.push(UnitReport {
                        index: i,
                        line_start: u.line_start,
                        line_end: u.line_end,
                        elapsed: sub.elapsed,
                        event_data: parse_event_data(&sub),
                        inner,
                        inner_total,
                        output_ref,
                        output_lines,
                    });
                }
                Outcome::Failed | Outcome::StillRunning => {
                    // A failing unit aborts its own `ld`, so the trailing
                    // probes never ran. Ask again in a separate submission:
                    // `gag-state-saved` and `last-event-data` are state
                    // globals and survive the failure. Not on a deadline
                    // expiry, though -- the unit is still *running*, so the
                    // session cannot answer a probe.
                    let (ed, cp, inner, inner_total) = if sub.outcome == Outcome::StillRunning {
                        (EventData::default(), Checkpoints::default(), Vec::new(), 0)
                    } else {
                        self.probe_after_failure(budget.floored())
                    };
                    let status = if sub.outcome == Outcome::StillRunning {
                        Status::DeadlineExceeded
                    } else if cp.proof_ran {
                        Status::ProofFailure
                    } else {
                        Status::Rejected
                    };
                    let error_line = error_message(&sub.output);
                    let mut report = FailureReport {
                        index: i,
                        line_start: u.line_start,
                        line_end: u.line_end,
                        status,
                        still_running: sub.outcome == Outcome::StillRunning,
                        error_line,
                        output_ref,
                        output_lines,
                        event_data: ed,
                        checkpoints: cp,
                        inner,
                        inner_total,
                        retry_output_ref: None,
                    };
                    // Under ContinuePastFailure a *completed* failure is
                    // retried under skip-proofs -- triage, not truth.
                    // Never a still-running one: the unit is mid-proof, and
                    // there is nothing to retry yet.
                    if self.mode == SyncMode::ContinuePastFailure
                        && sub.outcome == Outcome::Failed
                    {
                        let text = String::from_utf8_lossy(&bytes[u.start..u.end]);
                        let (rsub, rref) = self.admit_text(
                            &text, u.hash, u.line_start, u.line_end, i, true,
                            budget.floored(),
                        )?;
                        report.retry_output_ref = Some(rref);
                        match rsub.outcome {
                            Outcome::Ok => {
                                // Admitted assuming its claim. The envelope
                                // reported is the original failure's: the
                                // retry produces no proof data.
                                out.skipped.push(report);
                                continue;
                            }
                            Outcome::StillRunning => {
                                // The retry itself outran the budget -- a
                                // make-event still expands under skip-proofs.
                                // The envelope stays the original failure's;
                                // still_running says resume finishes the
                                // skip attempt.
                                report.still_running = true;
                            }
                            // Terminal: what survives skip-proofs is not a
                            // proof obligation, and it failed the same
                            // way twice. Never retry a retry.
                            Outcome::Failed => {}
                        }
                    }
                    // Stop here. The world is sequential, so later units almost
                    // always depend on this one and continuing produces
                    // cascading noise rather than information.
                    out.failed = Some(report);
                    break;
                }
            }
        }
        out.conditional_from = self.first_skipped();
        Ok(out)
    }

    /// Undo admitted units on request, keeping only the first `keep`.
    ///
    /// The editor's undo verb -- Proof General's "retract to here", the
    /// muscle memory `sync` deliberately refuses (a target bounds admission
    /// and never retracts). Keeping the two contracts on two verbs keeps
    /// both simple, and this one stays off the MCP surface: for an agent,
    /// undo remains a consequence of editing.
    ///
    /// BYTES is the caller's current snapshot, doing two jobs. It bounds
    /// the request -- the prefix kept is `min(keep, common prefix of the
    /// snapshot with the admitted units)`, so retracting under an edited
    /// buffer keeps only what is both requested and still current; a "kept"
    /// unit must mean "unit `i` of the snapshot is in the world", and units
    /// above an edit were headed for rollback at the next sync anyway. And
    /// it feeds the fallback: an unverifiable rollback degrades to
    /// `rebuild`, which replays from the snapshot, like any other undo the
    /// checks would not vouch for.
    ///
    /// The reply is the sync shape: `reused` counts the kept prefix,
    /// `rolled_back` the units undone, nothing is newly admitted, and
    /// `failed` carries the one non-error surprise -- a rebuild fallback
    /// whose replay outran the deadline, left as a valid shorter prefix
    /// plus one unit in flight for `resume`, exactly as a sync leaves it.
    ///
    /// Retraction pauses nothing by itself: with no target, the next sync
    /// re-admits what was just undone. The daemon pairs the two, moving its
    /// `through_line` back alongside.
    pub fn retract_to(
        &mut self,
        bytes: &[u8],
        keep: usize,
        deadline: Duration,
    ) -> Result<SyncOutcome, SyncError> {
        if self.session.is_running() {
            return Err(SyncError::Busy { index: self.running_unit() });
        }
        let budget = Budget::new(deadline);
        let units = split(bytes).map_err(SyncError::Split)?;
        let hashes: Vec<u64> = self.admitted.iter().map(|a| a.hash).collect();
        let allowed = keep.min(common_prefix(&hashes, &units));
        let mut out = SyncOutcome {
            reused: allowed,
            rolled_back: self.admitted.len() - allowed,
            ..SyncOutcome::default()
        };
        if self.dirty {
            // The admitted list is already under suspicion; replaying
            // exactly the kept prefix restores it by construction, and the
            // retraction rides along for free.
            out.rebuilt = true;
            out.failed = self.rebuild(bytes, allowed, &budget)?;
            out.conditional_from = self.first_skipped();
            return Ok(out);
        }
        if allowed == self.admitted.len() {
            out.conditional_from = self.first_skipped();
            return Ok(out); // nothing to undo; no submission spent
        }
        match self.rollback_to(allowed, budget.floored()) {
            Ok(()) => {}
            // P3, as in sync: an undo that cannot be verified degrades to
            // replay rather than being trusted.
            Err(SyncError::RollbackFailed { .. })
            | Err(SyncError::RollbackUnverified { .. }) => {
                out.rebuilt = true;
                out.failed = self.rebuild(bytes, allowed, &budget)?;
            }
            Err(e) => return Err(e),
        }
        out.conditional_from = self.first_skipped();
        Ok(out)
    }

    /// File a form's output under the handle a timed-out call already handed
    /// out for it.
    ///
    /// A form that outruns its deadline has printed nothing we can file yet,
    /// but the reply must still name a handle. That handle used to be
    /// allocated, left empty, and then abandoned: when the form finished, its
    /// output went to a *fresh* handle, so a caller holding the first one read
    /// an empty file and concluded the form printed nothing. The handle is
    /// remembered instead, and filled when the output arrives.
    fn record_output(&mut self, sub: &crate::session::Submission) -> Result<OutputRef, SyncError> {
        if sub.outcome == Outcome::StillRunning {
            let r = match self.in_flight_output.clone() {
                Some(r) => r,
                None => {
                    let fresh = self.outs().put("")?;
                    self.in_flight_output = Some(fresh.clone());
                    fresh
                }
            };
            return Ok(r);
        }
        match self.in_flight_output.take() {
            Some(r) => {
                self.outs().fill(&r, &sub.output)?;
                Ok(r)
            }
            None => Ok(self.outs().put(&sub.output)?),
        }
    }

    /// After a failed unit, recover what ACL2 recorded about it.
    ///
    /// Best effort by design: this runs when something has already gone wrong,
    /// so a probe that itself fails must degrade to an empty report rather than
    /// masking the original failure with a second one.
    fn probe_after_failure(
        &mut self,
        deadline: Duration,
    ) -> (EventData, Checkpoints, Vec<InnerEvent>, usize) {
        let ed = match self.session.submit(EVENT_DATA_PROBE, deadline) {
            Ok(sub) => parse_event_data(&sub),
            Err(_) => EventData::default(),
        };
        let cp = match self.session.submit(CHECKPOINT_PROBE, deadline) {
            Ok(sub) => parse_checkpoints(&sub),
            Err(_) => Checkpoints::default(),
        };
        let (inner, inner_total) = match self.session.submit(INNER_EVENTS_PROBE, deadline) {
            Ok(sub) => parse_inner(&sub),
            Err(_) => (Vec::new(), 0),
        };
        (ed, cp, inner, inner_total)
    }

    /// Submit one unit together with its label and command-number probe.
    ///
    /// One round trip: the unit, our `deflabel`, and the command number travel
    /// in a single submission, so the bookkeeping cannot drift from the world.
    /// On success the unit is recorded in `admitted`.
    ///
    /// The label is wrapped in `with-output :off :all` so our bookkeeping
    /// leaves no trace in the captured log. Without it every unit's output
    /// ends with a summary for `deflabel acl2-session-mark-N`, which is noise
    /// we introduced showing up in output the client reads as ACL2's.
    fn admit_unit(
        &mut self,
        u: &Form,
        index: usize,
        bytes: &[u8],
        deadline: Duration,
    ) -> Result<(crate::session::Submission, OutputRef), SyncError> {
        let text = String::from_utf8_lossy(&bytes[u.start..u.end]).into_owned();
        self.admit_text(&text, u.hash, u.line_start, u.line_end, index, false, deadline)
    }

    /// The submission itself, for a unit's text however obtained -- sliced
    /// from a snapshot (`admit_unit`), replayed by a rebuild, or retried by
    /// `ContinuePastFailure`, which is what SKIPPED means: the text goes in
    /// wrapped as `(acl2::skip-proofs <text>)`, the per-form mechanism
    /// chosen -- honest in the history (`pe` shows the wrap), and no
    /// ld-special mode for an abort to leak.
    #[allow(clippy::too_many_arguments)]
    fn admit_text(
        &mut self,
        text: &str,
        hash: u64,
        line_start: usize,
        line_end: usize,
        index: usize,
        skipped: bool,
        deadline: Duration,
    ) -> Result<(crate::session::Submission, OutputRef), SyncError> {
        self.label_gen += 1;
        let label = self.label_gen;
        // The newline before the closing paren guards a unit that ends in a
        // line comment.
        let payload = if skipped {
            format!("(acl2::skip-proofs\n{text}\n)")
        } else {
            text.to_owned()
        };
        let body = format!(
            "{CLEAR_STALE}\n{payload}\n{EVENT_DATA_PROBE}\n{INNER_EVENTS_PROBE}\n\
             (acl2::with-output :off :all (acl2::deflabel {}))\n{CMD_PROBE}",
            Self::label_name(label)
        );
        // Named before submission, so a lock-free status reader can say which
        // unit is running while this call still holds the session.
        self.control.set_unit(Some(index));
        let sub = self.session.submit(&body, deadline)?;
        if sub.outcome == Outcome::StillRunning {
            // Deadline expired; ACL2 is untouched and still working. Remember
            // what is needed to finish recording it when it reports.
            self.pending = Some(PendingUnit {
                index,
                hash,
                line_start,
                line_end,
                label,
                in_package: in_package_target(text.as_bytes()),
                text: text.to_owned(),
                skipped,
            });
        }
        let output_ref = self.record_output(&sub)?;
        if sub.outcome == Outcome::Ok {
            self.admitted.push(Admitted {
                hash,
                line_start,
                line_end,
                label,
                command_number: sub.get("cmd").and_then(|v| v.parse().ok()),
                package: sub.get("pkg").and_then(unquote),
                output_ref: output_ref.clone(),
                skipped,
            });
            // Second phase for a unit that is a top-level `(in-package ...)`:
            // its switch was scoped to the `ld` that just exited, so make it
            // stick. Unconditional rather than skipped-when-current: the raw
            // re-issue is idempotent and one round trip, and a staleness bug
            // in a skip condition here would silently misread every form
            // after the switch.
            if let Some(target) = in_package_target(text.as_bytes()) {
                self.make_package_stick(&target, deadline.max(PROBE_FLOOR))?;
            }
        }
        Ok((sub, output_ref))
    }

    /// Make a top-level `(in-package "P")` persist beyond its own `ld`.
    ///
    /// Inside an `ld`, the reader's package switch reverts when the `ld`
    /// exits. In a directory whose `acl2-customization.lsp` already
    /// establishes `P` that is invisible; without the convention, the session
    /// would read every later form in a different package than the session a
    /// person gets -- symbols interning elsewhere, definitions "missing" --
    /// which is precisely the divergence the prefix invariant forbids.
    ///
    /// Two phases:
    /// the unit already went through the normal wrapper (that is what gives
    /// an unknown package a real failure signal instead of a hung REPL), and
    /// only then is the switch re-issued raw at the top level, the way a
    /// person would type it. Raw submission proves nothing by finishing, so
    /// the effect is verified by reading `current-package` back.
    fn make_package_stick(&mut self, target: &str, deadline: Duration) -> Result<(), SyncError> {
        let raw = format!(
            "(acl2::in-package \"{target}\")\n\
             (acl2::prog2$ (acl2::cw \"~%<<<ASS-DATA pkg ~x0>>>~%\" \
               (acl2::f-get-global 'acl2::current-package acl2::state)) \
               (acl2::value :invisible))"
        );
        let sub = self.session.submit_raw(&raw, deadline)?;
        let now = sub.get("pkg").and_then(unquote);
        if now.as_deref() != Some(target) {
            // Should be unreachable -- the form already admitted inside the
            // wrapper -- but a switch that did not stick means every later
            // form reads differently than the reference session. Fail loudly
            // and let replay re-establish the world.
            self.dirty = true;
            return Err(SyncError::Io(io::Error::other(
                format!(
                    "in-package {target:?} did not persist (session reports {now:?}); \
                     the next sync will rebuild"
                ),
            )));
        }
        Ok(())
    }

    /// Finish a unit whose deadline expired.
    ///
    /// Returns `None` when nothing is in flight. A completed unit is recorded
    /// exactly as `sync` would have recorded it, so the prefix invariant holds
    /// either way; the caller then syncs again to continue through the file.
    pub fn resume(&mut self, deadline: Duration) -> Result<Option<Resumed>, SyncError> {
        // Ask the session, not the unit bookkeeping. `pending` records a unit
        // whose deadline expired mid-sync, so gating on it made resume answer
        // "nothing in flight" for a form started by `eval` -- which is the very
        // situation resume exists for. The same mistake was fixed in `health`
        // and its twin was left here.
        if !self.session.is_running() {
            self.pending = None;
            return Ok(None);
        }
        let pending = self.pending.clone();
        let Some(sub) = self.session.resume(deadline)? else {
            self.pending = None;
            return Ok(None);
        };
        if sub.outcome == Outcome::StillRunning {
            return Ok(Some(Resumed::StillRunning { elapsed: sub.elapsed }));
        }
        self.pending = None;

        let Some(p) = pending else {
            // No unit attached: an eval finished.
            let r = self.finish_eval(sub, deadline)?;
            return Ok(Some(Resumed::Evaluated(r)));
        };
        let output_ref = self.record_output(&sub)?;
        let output_lines = self.outs().lines(&output_ref).unwrap_or(0);
        if sub.outcome == Outcome::Ok {
            self.admitted.push(Admitted {
                hash: p.hash,
                line_start: p.line_start,
                line_end: p.line_end,
                label: p.label,
                command_number: sub.get("cmd").and_then(|v| v.parse().ok()),
                package: sub.get("pkg").and_then(unquote),
                output_ref: output_ref.clone(),
                skipped: p.skipped,
            });
            // The two-phase treatment applies here exactly as in
            // `admit_unit`; the pending record carries the detection made
            // when the unit was submitted, since the file may have changed
            // on disk while the unit ran.
            if let Some(target) = p.in_package.clone() {
                self.make_package_stick(&target, deadline.max(PROBE_FLOOR))?;
            }
            let (inner, inner_total) = parse_inner(&sub);
            return Ok(Some(Resumed::Admitted(UnitReport {
                index: p.index,
                line_start: p.line_start,
                line_end: p.line_end,
                elapsed: sub.elapsed,
                event_data: parse_event_data(&sub),
                inner,
                inner_total,
                output_ref,
                output_lines,
            })));
        }
        let (ed, cp, inner, inner_total) = self.probe_after_failure(deadline.max(PROBE_FLOOR));
        let error_line = error_message(&sub.output);
        let mut report = FailureReport {
            index: p.index,
            line_start: p.line_start,
            line_end: p.line_end,
            status: if cp.proof_ran { Status::ProofFailure } else { Status::Rejected },
            still_running: false,
            error_line,
            event_data: ed,
            checkpoints: cp,
            inner,
            inner_total,
            output_ref,
            output_lines,
            retry_output_ref: None,
        };
        // The same treatment a mid-sync failure gets -- the reason
        // the mode is a book property, not a sync argument. `p.skipped`
        // guards the retry of a retry: a skip attempt that itself failed is
        // terminal.
        if self.mode == SyncMode::ContinuePastFailure && !p.skipped {
            let (rsub, rref) = self.admit_text(
                &p.text,
                p.hash,
                p.line_start,
                p.line_end,
                p.index,
                true,
                deadline.max(PROBE_FLOOR),
            )?;
            report.retry_output_ref = Some(rref);
            match rsub.outcome {
                Outcome::Ok => return Ok(Some(Resumed::Skipped(report))),
                // The skip attempt is itself in flight now; resuming again
                // finishes it, and the p.skipped guard makes its failure
                // terminal.
                Outcome::StillRunning => report.still_running = true,
                Outcome::Failed => {}
            }
        }
        Ok(Some(Resumed::Failed(report)))
    }

    /// Progress on the unit in flight, if any.
    pub fn progress(&mut self, tail_lines: usize) -> io::Result<Option<crate::session::Progress>> {
        self.session.progress(tail_lines)
    }

    /// Progress of a certification that is still running, if there is one.
    ///
    /// Separate from `progress`, which reports the prover. A build is a
    /// different process doing a different thing, and both can be in flight at
    /// once -- certification does not touch the session.
    pub fn build_progress(&mut self, tail_lines: usize) -> Option<crate::cert::BuildProgress> {
        let b = self.build.as_mut()?;
        // A finished build is not progress. Leave it in place: the report is
        // assembled by `certify`, and inventing a second path to it would be a
        // second chance to disagree with the first.
        let log = b.output();
        let tail: Vec<&str> = log.lines().rev().take(tail_lines).collect();
        Some(crate::cert::BuildProgress {
            book: b.book().to_path_buf(),
            elapsed: b.elapsed(),
            lines_so_far: log.lines().count(),
            tail: tail.into_iter().rev().collect::<Vec<_>>().join("\n"),
        })
    }

    /// Abort the unit in flight and make the session usable again.
    ///
    /// Callers that cannot take the session lock ask via the control block
    /// first (`control().request_abort()`), which wakes whatever call is
    /// blocked on the session and makes it hand the lock over at once.
    pub fn interrupt(&mut self) -> io::Result<InterruptReport> {
        // A build is a separate process tree, so stopping it is a different
        // act from interrupting the prover -- but a caller asking to stop
        // means both, and leaving a -j build running after "interrupt" said
        // yes would be the more surprising outcome.
        if let Some(mut b) = self.build.take() {
            let _ = b.kill();
            self.control.end_build();
        }
        let was_running = self.session.is_running();
        // Capture before interrupting: the form will never report, so this is
        // the only chance to keep what it printed, and the interrupt handshake
        // reads more that would otherwise be mixed in.
        let partial = self.session.captured_so_far();
        // A dead child cannot be interrupted, and asking would surface a raw
        // I/O error from the handshake's write; report a clean non-recovery
        // instead, with health's `dead` one call away. And an *idle* session
        // that answers a ping must not be signalled at all: with nothing in
        // flight the build above was the whole job, and the escalation is
        // self-defeating at an idle reader -- it couples each SIGINT with an
        // immediately written handshake, and an interrupt discards exactly
        // that pending input, so four rounds failed four times (measured;
        // an idle ACL2 given a moment shrugs a lone SIGINT off entirely).
        // The escalation stays reserved for the running and the
        // unresponsive, where it is the recovery tool.
        let ok = if !self.session.alive() {
            false
        } else if was_running {
            self.session.interrupt()?
        } else if self.session.ping(Duration::from_millis(1500)).unwrap_or(false) {
            true
        } else {
            self.session.interrupt()?
        };
        // Filed in *both* arms: after a failed interrupt, the wedged session
        // is exactly the one whose partial log is the only evidence. The
        // handle stays linked on failure, so a form that later completes
        // still fills it with the full record.
        let mut output_ref = None;
        let mut output_lines = 0;
        if let Some(r) = &self.in_flight_output {
            let _ = self.outs().fill(r, &partial);
            output_lines = self.outs().lines(r).unwrap_or(0);
            output_ref = Some(r.clone());
        }
        if !ok {
            return Ok(InterruptReport { ok: false, interrupted: None });
        }
        self.pending = None;
        if let Some(r) = self.in_flight_output.take() {
            // It describes a form that was abandoned rather than the world
            // as it now stands, which is what `superseded` is for.
            self.outs().mark_superseded(&r);
        }
        if !was_running {
            return Ok(InterruptReport { ok: true, interrupted: None });
        }
        Ok(InterruptReport {
            ok: true,
            interrupted: self.probe_after_interrupt(output_ref, output_lines),
        })
    }

    /// Ask ACL2 what it recorded about the form just interrupted.
    ///
    /// Best effort, like every post-mortem probe: the envelope must never
    /// turn a successful interrupt into an error. Returns `None` unless the
    /// recorded abort cause actually names an interrupt -- see
    /// `INTERRUPT_PROBE` on why anything less is untrustworthy.
    fn probe_after_interrupt(
        &mut self,
        output_ref: Option<OutputRef>,
        output_lines: usize,
    ) -> Option<InterruptEnvelope> {
        let sub = self.session.submit(INTERRUPT_PROBE, PROBE_FLOOR).ok()?;
        let body = sub.block("intr")?;
        let pairs = keyed_block(body, &["iabort", "itime", "isteps", "ievent"]);
        let get = |k: &str| {
            pairs
                .iter()
                .find(|(pk, _)| pk == k)
                .map(|(_, v)| v.trim().to_string())
                .filter(|v| !is_nil(v))
        };
        let abort_cause = get("iabort")?;
        if !abort_cause.to_ascii_lowercase().contains("interrupt") {
            return None;
        }
        let checkpoints = match self.session.submit(CHECKPOINT_PROBE, PROBE_FLOOR) {
            Ok(s) => parse_checkpoints(&s),
            Err(_) => Checkpoints::default(),
        };
        Some(InterruptEnvelope {
            abort_cause: Some(abort_cause),
            event: get("ievent"),
            time: get("itime")
                .map(|v| v.split_whitespace().collect::<Vec<_>>().join(" ")),
            prover_steps: get("isteps").and_then(|v| v.parse().ok()),
            checkpoints,
            output_ref,
            output_lines,
        })
    }

    /// Certify this book with `cert.pl`, waiting up to `deadline`.
    ///
    /// Runs outside the session: `certify-book` would change the world, and
    /// the world is defined to be the file's prefix. Nothing here consults the
    /// session, because the file on disk is what gets certified -- so an
    /// unsaved buffer certifies as whatever is actually on disk, same as it
    /// would from a shell.
    ///
    /// Calling again while a build is in flight keeps waiting on that build
    /// rather than starting a second one.
    pub fn certify(
        &mut self,
        deadline: Duration,
        jobs: usize,
    ) -> Result<crate::cert::CertReport, SyncError> {
        use crate::cert::{blamed_books, cert_out_error, cert_pl_for, Build, CertReport, CertStatus};
        if self.build.is_none() {
            let cert_pl = match self.books_dir.as_deref() {
                Some(d) => crate::cert::cert_pl_in(d, &self.acl2)?,
                None => cert_pl_for(&self.acl2)?,
            };
            self.build = Some(Build::start(&cert_pl, &self.acl2, &self.path, jobs)?);
            self.control.begin_build(self.path.clone());
        }
        let b = self.build.as_mut().expect("just started");
        // The wait holds this book's lock exactly like a blocked submission,
        // and used to be the one such wait an interrupt could not reach: the
        // abort flag woke session waiters, and this was not one. Waiting on
        // the control block's condvar closes that -- an interrupt cuts
        // through at once and then kills the build it queued behind. The
        // 50 ms cadence only bounds try_wait staleness.
        let started_wait = Instant::now();
        let finished = loop {
            if let Some(status) = b.poll()? {
                break Some(status);
            }
            if started_wait.elapsed() >= deadline
                || self.control.wait_abort(Duration::from_millis(50))
            {
                break None;
            }
        };
        let elapsed = b.elapsed();
        let Some(status) = finished else {
            let log = b.output();
            let tail: Vec<&str> = log.lines().rev().take(12).collect();
            return Ok(CertReport {
                status: CertStatus::StillRunning,
                book: self.path.clone(),
                failed_books: Vec::new(),
                error_line: None,
                output_ref: None,
                output_lines: log.lines().count(),
                tail: tail.into_iter().rev().collect::<Vec<_>>().join("\n"),
                elapsed,
            });
        };
        let log = b.output();
        self.build = None;
        self.control.end_build();

        let failed_books = blamed_books(&log);
        // Trust the exit status for pass/fail, and the log only for *which*
        // book. A build can fail without naming one -- a missing dependency,
        // a make error -- and reporting that as success because no book was
        // blamed would be exactly the silent-wrong-answer failure mode.
        let ok = status.success() && failed_books.is_empty();
        let error_line = failed_books.first().and_then(|b| cert_out_error(b));
        let output_ref = self.outs().put(&log)?;
        let output_lines = self.outs().lines(&output_ref).unwrap_or(0);
        Ok(CertReport {
            status: if ok { CertStatus::Certified } else { CertStatus::Failed },
            book: self.path.clone(),
            failed_books,
            error_line,
            output_ref: Some(output_ref),
            output_lines,
            tail: String::new(),
            elapsed,
        })
    }

    /// The file this session is anchored to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where this image keeps its books, as ACL2 itself reported it at
    /// startup. `None` when the report was lost -- certify then falls back to
    /// walking for a checkout.
    pub fn books_dir(&self) -> Option<&Path> {
        self.books_dir.as_deref()
    }

    /// The unit currently in flight, if any.
    pub fn running_unit(&self) -> Option<usize> {
        self.pending.as_ref().map(|p| p.index)
    }

    /// Read part of a captured output.
    pub fn output(&self, r: &OutputRef, slice: Slice) -> io::Result<OutputSlice> {
        self.outs().get(r, slice)
    }

    /// Discard the ACL2 process and replay the first `keep` units of the file.
    ///
    /// This is the fallback half of P3. Undo is the fast path, but correctness
    /// must not rest on it, so every rollback is verified; when the check
    /// fails, the world is restored *by construction* instead of by trusting
    /// machinery just caught misbehaving.
    ///
    /// Note the distinction: a failed check does not mean the world is wrong,
    /// only that we cannot confirm it is right. A wrong prefix is a silent
    /// failure that makes a client report success about something not on disk,
    /// so "unverified" and "wrong" get the same treatment.
    ///
    /// Cost runs opposite to intuition -- it is proportional to what is *kept*,
    /// not what is undone. Rolling back one unit of a two-hundred-unit book
    /// replays a hundred and ninety-nine.
    ///
    /// `Ok(Some(report))` means the budget ran out mid-replay. That is not a
    /// failure and must not read as one: the session then holds a valid
    /// *shorter* prefix plus one unit in flight -- the same shape as any
    /// deadline expiry -- and the caller reports it as such, so resume
    /// finishes the unit and the next sync carries on. Treating it as
    /// `ReplayFailed` made a book whose replay exceeds one deadline
    /// unrecoverable: each sync killed the half-rebuilt session and started
    /// over.
    ///
    /// BYTES is the snapshot `keep` was computed against -- the caller
    /// supplies it rather than rebuild re-reading the file. The re-read left
    /// a hole: an edit landing between a sync's read and its mid-sync
    /// rebuild meant `keep` counted units of one text while the replay ran
    /// another, mixing the two in one world until the next sync's diff
    /// caught up. Taking the caller's bytes closes that by construction, and
    /// is also what lets a buffer-anchored sync rebuild without consulting
    /// the disk at all.
    pub fn rebuild(
        &mut self,
        bytes: &[u8],
        keep: usize,
        budget: &Budget,
    ) -> Result<Option<FailureReport>, SyncError> {
        let units = split(bytes).map_err(SyncError::Split)?;
        let keep = keep.min(units.len());
        // A unit admitted under skip-proofs must be REPLAYED under
        // skip-proofs: replaying it plainly re-runs the failed proof and
        // reads as a world that was never reachable -- which the plain world
        // truly is. Captured before the admitted list is cleared below.
        let skipped: Vec<bool> = self.admitted.iter().map(|a| a.skipped).collect();

        let dir = match self.path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        // Assigning drops the old Session, whose Drop kills the child. The
        // control block carries over, so handles held outside the lock keep
        // addressing this book.
        self.session = Session::start(&self.acl2, &dir, self.control.clone())?;
        self.admitted.clear();
        self.label_gen = 0;
        self.rebuilds += 1;
        self.post_start_setup()?;
        // From here the world is right by construction, whatever suspicion
        // put us on this path -- even a replay that stops short leaves a
        // valid (shorter) prefix. Clearing now, not at the end, keeps a
        // mid-replay in-flight return from queueing a second, redundant
        // rebuild behind itself.
        self.dirty = false;

        for (i, u) in units.iter().enumerate().take(keep) {
            let text = String::from_utf8_lossy(&bytes[u.start..u.end]);
            let (sub, output_ref) = self.admit_text(
                &text,
                u.hash,
                u.line_start,
                u.line_end,
                i,
                skipped.get(i).copied().unwrap_or(false),
                budget.remaining(),
            )?;
            match sub.outcome {
                Outcome::Ok => {}
                Outcome::StillRunning => {
                    let output_lines = self.outs().lines(&output_ref).unwrap_or(0);
                    return Ok(Some(FailureReport {
                        index: i,
                        line_start: u.line_start,
                        line_end: u.line_end,
                        status: Status::DeadlineExceeded,
                        still_running: true,
                        error_line: None,
                        output_ref,
                        output_lines,
                        event_data: EventData::default(),
                        checkpoints: Checkpoints::default(),
                        inner: Vec::new(),
                        inner_total: 0,
                        retry_output_ref: None,
                    }));
                }
                Outcome::Failed => {
                    // A unit that admitted before but not now means the world
                    // we were trying to restore was never reachable. Fail
                    // loudly.
                    return Err(SyncError::ReplayFailed {
                        line_start: u.line_start,
                        output: sub.output.clone(),
                    });
                }
            }
        }
        Ok(None)
    }

    /// Undo back to a prefix of length `keep`, then verify.
    ///
    /// `ubu!` rather than `ubt!` because it undoes back to but *not including*
    /// its target, so the label protecting the last kept unit survives.
    ///
    /// Labels rather than the units' own names because a top-level form does
    /// not reliably have one: `encapsulate` and `progn` introduce many events,
    /// `make-event` computes its events at admission time, and `in-theory`
    /// introduces none at all. A label we plant is always exactly one event
    /// with a name we chose.
    fn rollback_to(&mut self, keep: usize, deadline: Duration) -> Result<(), SyncError> {
        let (target_label, target_package) = if keep == 0 {
            (0, self.base_package.clone())
        } else {
            let a = &self.admitted[keep - 1];
            (a.label, a.package.clone())
        };
        // The reader's package is state, not world: `ubu!` will not restore
        // it. Re-establish the recorded package *before* undoing -- in that
        // order, because the target package is certainly known in the older
        // world (it was current there), while the package we are leaving may
        // be about to lose its very definition to the undo.
        if let Some(p) = &target_package
            && self.current_package() != Some(p.as_str())
        {
            self.make_package_stick(p, deadline)
                .map_err(|e| SyncError::RollbackFailed { output: e.to_string() })?;
        }
        // `ubu!` is a macro that passes its argument into a *function* call, so
        // the label is evaluated and must be quoted. Unquoted it is simply an
        // unbound variable, and the resulting translate error aborts the `ld`
        // silently -- which is why the outcome is checked below rather than
        // only the reported command number.
        let body = format!("(acl2::ubu! '{})\n{CMD_PROBE}", Book::label_name(target_label));
        let sub = self.session.submit(&body, deadline)?;
        if sub.outcome != Outcome::Ok {
            return Err(SyncError::RollbackFailed { output: sub.output });
        }
        let actual: Option<i64> = sub.get("cmd").and_then(|v| v.parse().ok());

        // P3: the fast path is verified against a recorded invariant. A subtle
        // undo surprise should cost latency, not correctness.
        if keep > 0
            && let Some(expected) = self.admitted[keep - 1].command_number
            && actual != Some(expected)
        {
            return Err(SyncError::RollbackUnverified { expected, actual });
        }
        // The package was re-established before the undo, and the probe
        // reports where it actually stands -- so check that too, same P3.
        if let Some(p) = &target_package
            && sub.get("pkg").and_then(unquote).as_deref() != Some(p.as_str())
        {
            return Err(SyncError::RollbackFailed {
                output: format!(
                    "package after undo is {:?}, expected {p:?}",
                    sub.get("pkg").and_then(unquote)
                ),
            });
        }
        // Output from rolled-back units stays readable -- comparing against
        // what a form used to print is useful -- but is flagged so it cannot be
        // mistaken for a description of the current world.
        for a in &self.admitted[keep..] {
            self.outs().mark_superseded(&a.output_ref);
        }
        self.admitted.truncate(keep);
        Ok(())
    }

    /// Lines currently believed to be admitted, for reporting.
    pub fn admitted_lines(&self) -> Vec<(usize, usize)> {
        self.admitted.iter().map(|a| (a.line_start, a.line_end)).collect()
    }

    /// Indices of admitted units whose proofs were skipped -- an editor's
    /// red forms -- in order. Empty outside `ContinuePastFailure` sessions.
    pub fn skipped_units(&self) -> Vec<usize> {
        self.admitted
            .iter()
            .enumerate()
            .filter(|(_, a)| a.skipped)
            .map(|(i, _)| i)
            .collect()
    }

    /// The earliest skip-proofs-admitted unit in the world, if any. Every
    /// admitted unit after it is conditional.
    fn first_skipped(&self) -> Option<usize> {
        self.admitted.iter().position(|a| a.skipped)
    }
}

/// Whether a session is answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Idle and answering: a form can be submitted now.
    ///
    /// Named for what the client can do rather than for the process being
    /// alive, since a `Busy` session is alive too.
    Ready,
    /// A form is in flight; the session is working, not stuck.
    Busy,
    /// Answered nothing within the deadline while claiming to be idle.
    /// Distinct from `Busy`, which is silence we caused and expect.
    Unresponsive,
    Dead,
}

/// Length of the longest prefix whose hashes still agree.
///
/// Takes hashes rather than the admitted records: this is the entire diff, and
/// it depends on nothing else about a unit.
fn common_prefix(admitted: &[u64], units: &[Form]) -> usize {
    let mut n = 0;
    while n < admitted.len() && n < units.len() && admitted[n] == units[n].hash {
        n += 1;
    }
    n
}

/// Resolve `through_line` to a unit index, forgivingly.
///
/// Never fails on misalignment. Line numbers shift on every edit above, so a
/// strict boundary check would burn client turns on off-by-one errors it cannot
/// see. A line inside a unit selects that unit -- if you pointed at a `defthm`,
/// you meant that `defthm`. A line between units selects everything ending at
/// or before it.
fn resolve_target(units: &[Form], through_line: Option<usize>) -> Option<usize> {
    if units.is_empty() {
        return None;
    }
    let Some(line) = through_line else {
        return Some(units.len() - 1);
    };
    if let Some(i) = units
        .iter()
        .position(|u| u.line_start <= line && line <= u.line_end)
    {
        return Some(i);
    }
    units.iter().rposition(|u| u.line_end <= line)
}

#[cfg(test)]
mod tests {
    /// A rune too long for ACL2's margin wraps, and must still come back as
    /// one item. This is the exact layout ACL2 produced for these three.
    #[test]
    fn a_wrapped_rune_stays_one_item() {
        let body = "rule (:definition endp)\n\
                    rule \n\
                    (:forward-chaining\n\
                    \x20    tree-empty-p-when-not-tree-empty-p-of-tree->right-forward-chaining)\n\
                    \n\
                    rule (:fake-rune-for-type-set nil)\n";
        let pairs = super::keyed_block(body, &["rule"]);
        let got = super::items(&pairs, "rule");
        assert_eq!(
            got,
            vec![
                "(:definition endp)",
                "(:forward-chaining tree-empty-p-when-not-tree-empty-p-of-tree->right-forward-chaining)",
                "(:fake-rune-for-type-set nil)",
            ],
            "item boundaries must survive ACL2's wrapping"
        );
        assert!(got.iter().all(|r| !r.contains('\n')),
                "a rune must not carry ACL2's line breaks");
    }

    #[test]
    fn an_empty_rune_list_yields_no_items() {
        let pairs = super::keyed_block("steps 3\n", &["steps", "rule"]);
        assert!(super::items(&pairs, "rule").is_empty());
    }

    /// ACL2 fill-wraps its errors, so the message is not one line.
    #[test]
    fn an_error_message_survives_acl2s_line_wrapping() {
        let out = "Q.E.D.\n\nACL2 Error [Failure] in ( DEFTHM FOO ...):  See :DOC\nfailure.\n\nSummary\n";
        let m = super::error_message(out).expect("an error is present");
        assert!(m.ends_with("failure."), "the sentence must be whole: {m:?}");
        assert!(m.contains("See :DOC failure."), "a :DOC reference must keep its topic: {m:?}");
        assert!(!m.contains("Summary"), "the message stops at the blank line");
    }

    #[test]
    fn output_without_an_error_has_no_message() {
        assert_eq!(super::error_message("Q.E.D.\n\nSummary\n"), None);
    }

    /// A compound unit can *print* an error without failing -- `must-fail`
    /// does exactly that -- so the first error paragraph may be an expected
    /// decoy. The genuine failure is the last one; nothing prints after it.
    #[test]
    fn an_expected_error_earlier_in_the_unit_is_not_the_message() {
        let out = "ACL2 Error in ( DEFTHM EXPECTED ...):  This error was\n\
                   expected by must-fail.\n\
                   \n\
                   Q.E.D. elsewhere\n\
                   \n\
                   ACL2 Error [Failure] in ( DEFTHM REAL ...):  The real\n\
                   failure.\n\
                   \n\
                   Summary\n";
        let m = super::error_message(out).expect("an error is present");
        assert!(m.contains("REAL"), "must pick the last error paragraph: {m:?}");
        assert!(!m.contains("EXPECTED"), "the decoy must not be reported: {m:?}");
    }

    /// A `.cert.out` ends with certification's own summary error, after the
    /// event's real one. The summary says nothing; the event error must win.
    #[test]
    fn certifications_wrapup_error_is_not_the_message() {
        let out = "ACL2 Error [Failure] in ( DEFTHM H-BOGUS ...):  See :DOC\nfailure.\n\
                   \n\
                   ACL2 Error [Failure] in (CERTIFY-BOOK \"dep\" ...):  See :DOC\nfailure.\n";
        let m = super::error_message(out).expect("an error is present");
        assert!(m.contains("H-BOGUS"), "the event's own error must win: {m:?}");
        // But when the summary is all there is, it is still reported.
        let only = "ACL2 Error in (CERTIFY-BOOK \"dep\" ...):  something.\n";
        assert!(super::error_message(only).unwrap().contains("CERTIFY-BOOK"));
    }

    /// The shapes `in_package_target` must accept, and the ones it must not.
    #[test]
    fn in_package_detection() {
        let t = |s: &str| super::in_package_target(s.as_bytes());
        assert_eq!(t(r#"(in-package "TREEMAP")"#), Some("TREEMAP".into()));
        assert_eq!(t("(IN-PACKAGE\n  \"ACL2\")"), Some("ACL2".into()));
        assert_eq!(t(r#"(acl2::in-package "X")"#), Some("X".into()));
        assert_eq!(t(r#"(in-package "X" :extra)"#), None);
        assert_eq!(t(r#"(in-package name)"#), None);
        assert_eq!(t(r#"(include-book "in-package")"#), None);
        assert_eq!(t(r#"(defthm in-package t)"#), None);
        assert_eq!(t("in-package"), None);
    }

    use super::*;
    use crate::split::split_str;

    fn units(src: &str) -> Vec<Form> {
        split_str(src).unwrap()
    }

    const SRC: &str = "(alpha)\n\n(beta\n  gamma)\n\n(delta)\n";

    #[test]
    fn no_target_means_end_of_file() {
        assert_eq!(resolve_target(&units(SRC), None), Some(2));
        assert_eq!(resolve_target(&[], None), None);
    }

    #[test]
    fn a_line_inside_a_unit_selects_that_unit() {
        let u = units(SRC);
        assert_eq!(resolve_target(&u, Some(3)), Some(1)); // first line of beta
        assert_eq!(resolve_target(&u, Some(4)), Some(1)); // last line of beta
        assert_eq!(resolve_target(&u, Some(1)), Some(0));
    }

    #[test]
    fn a_line_between_units_selects_everything_before_it() {
        let u = units(SRC);
        assert_eq!(resolve_target(&u, Some(2)), Some(0)); // blank line after alpha
        assert_eq!(resolve_target(&u, Some(5)), Some(1)); // blank line after beta
    }

    #[test]
    fn a_line_before_any_unit_selects_nothing() {
        // A file that opens with blank lines: line 1 precedes every unit.
        let u = units("\n\n(alpha)\n");
        assert_eq!(resolve_target(&u, Some(1)), None);
    }

    #[test]
    fn prefix_diff() {
        let u = units(SRC);
        let all: Vec<u64> = u.iter().map(|f| f.hash).collect();

        // Unchanged file: everything is reused.
        assert_eq!(common_prefix(&all, &u), 3);
        // Nothing admitted yet.
        assert_eq!(common_prefix(&[], &u), 0);
        // Edit to the middle unit invalidates it and everything after.
        let mut changed = all.clone();
        changed[1] = 0xdead;
        assert_eq!(common_prefix(&changed, &u), 1);
        // Edit to the first unit invalidates everything.
        let mut first = all.clone();
        first[0] = 0xdead;
        assert_eq!(common_prefix(&first, &u), 0);
        // A file truncated below what was admitted.
        assert_eq!(common_prefix(&all, &u[..1]), 1);
    }

    #[test]
    fn reformatting_does_not_invalidate_the_prefix() {
        let original = units(SRC);
        let hashes: Vec<u64> = original.iter().map(|f| f.hash).collect();
        let reformatted = units("(alpha)\n\n(beta\n\n  ; note\n  gamma)\n\n(delta)\n");
        assert_eq!(
            common_prefix(&hashes, &reformatted),
            3,
            "reindenting and adding a comment must reuse the whole prefix"
        );
    }
}
