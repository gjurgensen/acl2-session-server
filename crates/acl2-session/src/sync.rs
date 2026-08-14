//! The file-prefix invariant.
//!
//! > The session world equals the result of admitting file units `1..N` in a
//! > plain interactive ACL2 session started in the book's own directory.
//!
//! The client never tells us what changed. Every sync re-reads and re-hashes
//! the file, so the mechanism is self-correcting: there is no edit-notification
//! protocol to fall out of step with. Rollback stops being something a client
//! requests and becomes a consequence of editing.

#![forbid(unsafe_code)]
// This module is pure logic; all FFI lives in pty.rs and session.rs.
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::session::{Outcome, Session};
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
    output_ref: OutputRef,
}

/// What ACL2 recorded about an event, read from `get-event-data` rather than
/// parsed out of the prover's prose.
///
/// Values are ACL2-printed text. Deliberately: the *envelope* is structured --
/// each fact is its own field and can never be elided by a truncated blob --
/// while the values stay in the notation a reader of ACL2 already knows. There
/// is no gain in re-encoding a rune list as JSON.
#[derive(Debug, Clone, Default)]
pub struct EventData {
    pub prover_steps: Option<u64>,
    /// Runes the proof used.
    pub rules: Option<String>,
    pub warnings: Option<String>,
    /// `(prove print proof-tree other)`, in seconds.
    pub time: Option<String>,
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
    pub rules: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Checkpoints {
    /// Whether `gag-state-saved` was populated at all -- i.e. whether a proof
    /// actually ran. This is what separates `ProofFailure` from `Rejected`.
    pub proof_ran: bool,
    /// Goals at the top level, prettified as ACL2 would print them.
    pub top: Vec<String>,
    /// Goals under a top-level induction.
    pub sub: Vec<String>,
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
}

impl FailureReport {
    /// The inner event that actually failed, when the unit was compound.
    ///
    /// For a bare `defthm` this is the unit itself and adds nothing; for a
    /// `defrule` with prep-lemmas it distinguishes a failing prep-lemma from a
    /// failing main theorem, which the checkpoints alone cannot.
    pub fn failed_event(&self) -> Option<&InnerEvent> {
        self.inner.iter().find(|e| e.abort_causes.is_some())
    }
}

#[derive(Debug, Clone, Default)]
pub struct SyncOutcome {
    /// Index of the last unit the target resolved to, if any.
    pub resolved_through: Option<usize>,
    /// Prefix units left untouched because their hashes still matched.
    pub reused: usize,
    /// Units undone because the file changed beneath them.
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
    pub output_ref: OutputRef,
    pub output_lines: usize,
    pub elapsed: Duration,
}

/// What resuming an in-flight unit produced.
#[derive(Debug)]
pub enum Resumed {
    /// Still running. The deadline bounded this call, not the proof.
    StillRunning { elapsed: Duration },
    Admitted(UnitReport),
    Failed(FailureReport),
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
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Io(e) => write!(f, "{e}"),
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
const OWN_OUTPUT: &str = "(acl2::prog2$ \
    (acl2::cw \"~%<<<ASS-DATA inhibit ~x0>>>~%\" (acl2::@ acl2::inhibit-output-lst)) \
    (acl2::er-progn \
      (acl2::set-inhibit-output-lst \
        (acl2::remove-eq 'acl2::prove (acl2::@ acl2::inhibit-output-lst))) \
      (acl2::value acl2::nil)))";

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
const INNER_EVENTS_PROBE: &str = r#"(acl2::let*
  ((acl2::f0 (acl2::@ acl2::event-data-fal))
   (acl2::e0 (acl2::and (acl2::consp acl2::f0) (acl2::car acl2::f0)))
   (acl2::f1 (acl2::and (acl2::consp acl2::f0) (acl2::cdr acl2::f0)))
   (acl2::e1 (acl2::and (acl2::consp acl2::f1) (acl2::car acl2::f1)))
   (acl2::f2 (acl2::and (acl2::consp acl2::f1) (acl2::cdr acl2::f1)))
   (acl2::e2 (acl2::and (acl2::consp acl2::f2) (acl2::car acl2::f2)))
   (acl2::f3 (acl2::and (acl2::consp acl2::f2) (acl2::cdr acl2::f2)))
   (acl2::d0 (acl2::and (acl2::consp acl2::e0) (acl2::car (acl2::cdr acl2::e0))))
   (acl2::d1 (acl2::and (acl2::consp acl2::e1) (acl2::car (acl2::cdr acl2::e1))))
   (acl2::d2 (acl2::and (acl2::consp acl2::e2) (acl2::car (acl2::cdr acl2::e2)))))
  (acl2::prog2$
    (acl2::cw "~%<<<ASS-BEGIN inner>>>~%more ~x0~%n0 ~x1~%a0 ~x2~%r0 ~x3~%n1 ~x4~%a1 ~x5~%r1 ~x6~%n2 ~x7~%a2 ~x8~%r2 ~x9~%<<<ASS-END inner>>>~%"
      (acl2::if (acl2::consp acl2::f3) acl2::t acl2::nil)
      (acl2::and (acl2::consp acl2::e0) (acl2::car acl2::e0))
      (acl2::cdr (acl2::assoc-eq 'acl2::abort-causes acl2::d0))
      (acl2::cdr (acl2::assoc-eq 'acl2::rules acl2::d0))
      (acl2::and (acl2::consp acl2::e1) (acl2::car acl2::e1))
      (acl2::cdr (acl2::assoc-eq 'acl2::abort-causes acl2::d1))
      (acl2::cdr (acl2::assoc-eq 'acl2::rules acl2::d1))
      (acl2::and (acl2::consp acl2::e2) (acl2::car acl2::e2))
      (acl2::cdr (acl2::assoc-eq 'acl2::abort-causes acl2::d2))
      (acl2::cdr (acl2::assoc-eq 'acl2::rules acl2::d2)))
    (acl2::value :invisible)))"#;

fn parse_inner(sub: &crate::session::Submission) -> (Vec<InnerEvent>, usize) {
    let Some(body) = sub.block("inner") else { return (Vec::new(), 0) };
    let pairs = keyed_block(body, &["more", "n0", "a0", "r0", "n1", "a1", "r1",
                                    "n2", "a2", "r2"]);
    let get = |k: &str| pairs.iter().find(|(pk, _)| pk == k)
        .map(|(_, v)| v.trim().to_string()).filter(|v| !is_nil(v));
    let mut out = Vec::new();
    for (n, a, r) in [("n0", "a0", "r0"), ("n1", "a1", "r1"), ("n2", "a2", "r2")] {
        if let Some(name) = get(n) {
            out.push(InnerEvent { name, abort_causes: get(a), rules: get(r) });
        }
    }
    // Only a fixed number of slots is read; say when there were more rather
    // than let a client mistake a truncation for the whole story.
    let total = out.len() + usize::from(get("more").is_some());
    (out, total)
}

/// Report what ACL2 recorded about the event just executed.
///
/// Must run *before* the `deflabel`: `get-event-data` describes the
/// immediately preceding event, and planting a label first would make it
/// describe the label.
const EVENT_DATA_PROBE: &str = r#"(acl2::prog2$
  (acl2::cw "~%<<<ASS-BEGIN evd>>>~%steps ~x0~%rules ~x1~%warnings ~x2~%time ~x3~%<<<ASS-END evd>>>~%"
    (acl2::get-event-data 'acl2::prover-steps-counted acl2::state)
    (acl2::get-event-data 'acl2::rules acl2::state)
    (acl2::get-event-data 'acl2::warnings acl2::state)
    (acl2::get-event-data 'acl2::time acl2::state))
  (acl2::value :invisible))"#;

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
/// Split across two `cw` calls because ACL2's fmt directives index arguments
/// `~x0` to `~x9` only -- eleven values do not fit in one call, and `~xa` is
/// not an argument reference at all.
///
/// A fixed number of slots is read rather than mapping over the stacks,
/// because `state` is not available inside a `loop$` body and defining a
/// recursive helper would reintroduce exactly the world modification being
/// avoided. `nth` past the end returns `nil` and the whole chain degrades to
/// `NIL`, so out-of-range slots are harmless. The true stack lengths are
/// reported alongside, so a client can tell when there were more.
const CHECKPOINT_PROBE: &str = r#"(acl2::let*
  ((acl2::gs (acl2::f-get-global 'acl2::gag-state-saved acl2::state))
   (acl2::tp (acl2::access acl2::gag-state acl2::gs :top-stack))
   (acl2::sb (acl2::access acl2::gag-state acl2::gs :sub-stack))
   (acl2::wr (acl2::w acl2::state)))
  (acl2::prog2$
    (acl2::cw "~%<<<ASS-BEGIN ckpt>>>~%gag ~x0~%ntop ~x1~%nsub ~x2~%forced ~x3~%abort ~x4~%"
      (acl2::if acl2::gs acl2::t acl2::nil)
      (acl2::len acl2::tp)
      (acl2::len acl2::sb)
      (acl2::access acl2::gag-state acl2::gs :forcep)
      (acl2::let ((acl2::ai (acl2::access acl2::gag-state acl2::gs :abort-info)))
        (acl2::if (acl2::consp acl2::ai)
                  (acl2::and (acl2::symbolp (acl2::car acl2::ai)) (acl2::car acl2::ai))
                  acl2::ai)))
    (acl2::prog2$
      (acl2::cw "top0 ~x0~%top1 ~x1~%top2 ~x2~%sub0 ~x3~%sub1 ~x4~%sub2 ~x5~%<<<ASS-END ckpt>>>~%"
        (acl2::prettyify-clause (acl2::access acl2::gag-info (acl2::nth 0 acl2::tp) :clause) acl2::nil acl2::wr)
        (acl2::prettyify-clause (acl2::access acl2::gag-info (acl2::nth 1 acl2::tp) :clause) acl2::nil acl2::wr)
        (acl2::prettyify-clause (acl2::access acl2::gag-info (acl2::nth 2 acl2::tp) :clause) acl2::nil acl2::wr)
        (acl2::prettyify-clause (acl2::access acl2::gag-info (acl2::nth 0 acl2::sb) :clause) acl2::nil acl2::wr)
        (acl2::prettyify-clause (acl2::access acl2::gag-info (acl2::nth 1 acl2::sb) :clause) acl2::nil acl2::wr)
        (acl2::prettyify-clause (acl2::access acl2::gag-info (acl2::nth 2 acl2::sb) :clause) acl2::nil acl2::wr))
      (acl2::value :invisible))))"#;

/// Split a `key value` block into pairs, tolerating values that ACL2 wrapped
/// across lines.
fn keyed_block(body: &str, keys: &[&str]) -> Vec<(String, String)> {
    let mut cur: Option<(String, Vec<String>)> = None;
    let mut done: Vec<(String, String)> = Vec::new();
    for line in body.lines() {
        let hit = keys.iter().find(|k| {
            line.strip_prefix(**k).map_or(false, |r| r.starts_with(' '))
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
    let keys = ["gag", "ntop", "nsub", "forced", "abort",
                "top0", "top1", "top2", "sub0", "sub1", "sub2"];
    let pairs = keyed_block(body, &keys);
    let get = |k: &str| pairs.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.trim().to_string());
    c.proof_ran = get("gag").map_or(false, |v| !is_nil(&v));
    c.n_top = get("ntop").and_then(|v| v.parse().ok()).unwrap_or(0);
    c.n_sub = get("nsub").and_then(|v| v.parse().ok()).unwrap_or(0);
    c.forced = get("forced").map_or(false, |v| !is_nil(&v));
    c.abort_cause = get("abort").filter(|v| !is_nil(v));
    for k in ["top0", "top1", "top2"] {
        if let Some(v) = get(k).filter(|v| !is_nil(v)) {
            c.top.push(v);
        }
    }
    for k in ["sub0", "sub1", "sub2"] {
        if let Some(v) = get(k).filter(|v| !is_nil(v)) {
            c.sub.push(v);
        }
    }
    c
}

fn parse_event_data(sub: &crate::session::Submission) -> EventData {
    let mut e = EventData::default();
    let Some(body) = sub.block("evd") else { return e };
    // ACL2 wraps long values at its right margin, so a value runs until the
    // next known key rather than to end of line.
    let done = keyed_block(body, &["steps", "rules", "warnings", "time"]);
    let pick = |k: &str| -> Option<String> {
        done.iter()
            .find(|(dk, _)| dk == k)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !is_nil(v))
    };
    e.prover_steps = pick("steps").and_then(|v| v.parse().ok());
    e.rules = pick("rules");
    e.warnings = pick("warnings");
    e.time = pick("time");
    e
}

/// A session anchored to one file.
pub struct Book {
    acl2: PathBuf,
    path: PathBuf,
    session: Session,
    admitted: Vec<Admitted>,
    label_gen: u64,
    rebuilds: usize,
    outputs: OutputStore,
    /// The unit whose submission has not reported yet.
    pending: Option<PendingUnit>,
    /// Command number just after the base label, i.e. the empty prefix.
    base_command_number: Option<i64>,
    /// Set when an undo could not be verified; the next sync rebuilds.
    dirty: bool,
    /// `inhibit-output-lst` as inherited, before the server removed PROVE.
    inherited_inhibit: Option<String>,
}

/// Enough to finish recording a unit whose deadline expired mid-flight.
#[derive(Debug, Clone)]
struct PendingUnit {
    index: usize,
    hash: u64,
    line_start: usize,
    line_end: usize,
    label: u64,
}

impl Book {
    /// Open `path` and start a session in its directory.
    pub fn open(acl2: &Path, path: &Path) -> Result<Book, SyncError> {
        let dir = path.parent().unwrap_or(Path::new("."));
        let session = Session::start(acl2, dir)?;
        let mut b = Book {
            acl2: acl2.to_path_buf(),
            path: path.to_path_buf(),
            session,
            admitted: Vec::new(),
            label_gen: 0,
            rebuilds: 0,
            outputs: OutputStore::new(std::process::id() as u64)?,
            pending: None,
            base_command_number: None,
            dirty: false,
            inherited_inhibit: None,
        };
        // A baseline label, so "roll back to the empty prefix" is the same
        // operation as any other rollback rather than a special case.
        let sub = b.session.submit(OWN_OUTPUT, Duration::from_secs(60))?;
        b.inherited_inhibit = sub.get("inhibit").map(str::to_string);
        b.plant_base_label()?;
        Ok(b)
    }

    pub fn session(&mut self) -> &mut Session {
        &mut self.session
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

    fn label_name(genid: u64) -> String {
        format!("acl2-session-mark-{genid}")
    }

    fn plant_base_label(&mut self) -> Result<(), SyncError> {
        let body = format!(
            "(acl2::with-output :off :all (acl2::deflabel {}))\n\
             (acl2::prog2$ (acl2::cw \"~%<<<ASS-DATA cmd ~x0>>>~%\" \
               (acl2::max-absolute-command-number (acl2::w acl2::state))) \
               (acl2::value :invisible))",
            Self::label_name(0)
        );
        let sub = self.session.submit(&body, Duration::from_secs(60))?;
        self.base_command_number = sub.get("cmd").and_then(|v| v.parse().ok());
        Ok(())
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
            .map_or(false, |v| v.to_lowercase().contains("prove"))
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
        let body = format!(
            "{form}\n(acl2::prog2$ (acl2::cw \"~%<<<ASS-DATA cmd ~x0>>>~%\" \
               (acl2::max-absolute-command-number (acl2::w acl2::state))) \
               (acl2::value :invisible))"
        );
        let sub = self.session.submit(&body, deadline)?;
        let output_ref = self.outputs.put(&sub.output)?;
        let output_lines = self.outputs.lines(&output_ref).unwrap_or(0);
        let after: Option<i64> = sub.get("cmd").and_then(|v| v.parse().ok());

        let mut world_change = WorldChange::None;
        if let (Some(a), Some(e)) = (after, self.expected_command_number()) {
            if a != e {
                // Events were introduced. Undo back to the prefix, which is
                // where the last planted label sits.
                let keep = self.admitted.len();
                world_change = match self.rollback_to(keep, deadline) {
                    Ok(()) => WorldChange::Undone,
                    Err(SyncError::RollbackFailed { .. })
                    | Err(SyncError::RollbackUnverified { .. }) => {
                        self.dirty = true;
                        WorldChange::Dirty
                    }
                    Err(e) => return Err(e),
                };
            }
        }
        Ok(EvalResult {
            outcome: sub.outcome,
            world_change,
            output_ref,
            output_lines,
            elapsed: sub.elapsed,
        })
    }

    /// Round-trip a trivial form to see whether the session is responsive.
    /// Round-trip a trivial form to see whether the session is responsive.
    ///
    /// The three failure answers are genuinely different actions:
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
    ///   This is the one answer resting on reasoning rather than measurement:
    ///   nothing in the test suite can wedge ACL2 on demand, so the branch is
    ///   untested. It is also the answer that matters most when something has
    ///   genuinely gone wrong.
    /// - `Dead` -- the descriptor itself failed. The process is gone; only a
    ///   rebuild will help.
    ///
    /// Reporting `Dead` for a session that is merely busy would be the worst
    /// of these, since a client would discard a session that is working.
    pub fn health(&mut self, deadline: Duration) -> Health {
        if self.session.is_running() {
            return Health::Busy;
        }
        match self.session.submit("(acl2::value :invisible)", deadline) {
            Ok(s) if s.outcome != Outcome::StillRunning => Health::Alive,
            Ok(_) => Health::Unresponsive,
            Err(_) => Health::Dead,
        }
    }

    /// Bring the session into agreement with the file on disk.
    ///
    /// `through_line` is optional and omitted means "to the end of the file",
    /// which should be the common call: because sync stops at the first
    /// failure, the edit-fix-edit loop never needs a target.
    ///
    /// A target bounds how far *forward* to admit; it never retracts. Syncing
    /// through line 3 when six units are already admitted leaves all six in
    /// place and simply admits nothing new. Undoing is driven by the file
    /// changing, not by asking for less of it.
    pub fn sync(
        &mut self,
        through_line: Option<usize>,
        deadline: Duration,
    ) -> Result<SyncOutcome, SyncError> {
        if self.session.is_running() {
            return Err(SyncError::Busy { index: self.running_unit() });
        }
        if self.dirty {
            // An undo we could not verify: rebuild rather than trust it.
            self.rebuild(0, deadline)?;
            self.dirty = false;
        }
        let bytes = std::fs::read(&self.path)?;
        let units = split(&bytes).map_err(SyncError::Split)?;

        let mut out = SyncOutcome::default();
        let target = match resolve_target(&units, through_line) {
            Some(t) => t,
            None => return Ok(out), // nothing selected; nothing to do
        };
        out.resolved_through = Some(target);

        // Longest common prefix by hash. This is the whole diff: the client
        // never says what changed, we just re-derive it.
        let hashes: Vec<u64> = self.admitted.iter().map(|a| a.hash).collect();
        let keep = common_prefix(&hashes, &units);
        out.reused = keep;

        if keep < self.admitted.len() {
            out.rolled_back = self.admitted.len() - keep;
            match self.rollback_to(keep, deadline) {
                Ok(()) => {}
                // P3: a fast path that cannot be verified degrades to replay
                // rather than being trusted. Both an outright rejection and an
                // unverifiable landing get the same treatment.
                Err(SyncError::RollbackFailed { .. })
                | Err(SyncError::RollbackUnverified { .. }) => {
                    self.rebuild(keep, deadline)?;
                    out.rebuilt = true;
                }
                Err(e) => return Err(e),
            }
        }

        for (i, u) in units.iter().enumerate().take(target + 1).skip(keep) {
            let (sub, output_ref) = self.admit_unit(u, i, &bytes, deadline)?;
            let output_lines = self.outputs.lines(&output_ref).unwrap_or(0);
            match sub.outcome {
                Outcome::Ok => {
                    out.admitted.push(UnitReport {
                        index: i,
                        line_start: u.line_start,
                        line_end: u.line_end,
                        elapsed: sub.elapsed,
                        event_data: parse_event_data(&sub),
                        inner: {
                            let (i, _) = parse_inner(&sub);
                            i
                        },
                        inner_total: parse_inner(&sub).1,
                        output_ref,
                        output_lines,
                    });
                }
                Outcome::Failed | Outcome::StillRunning => {
                    // A failing unit aborts its own `ld`, so the trailing
                    // probes never ran. Ask again in a separate submission:
                    // `gag-state-saved` and `last-event-data` are state
                    // globals and survive the failure.
                    let (ed, cp, inner, inner_total) = self.probe_after_failure(deadline);
                    let status = if sub.outcome == Outcome::StillRunning {
                        Status::DeadlineExceeded
                    } else if cp.proof_ran {
                        Status::ProofFailure
                    } else {
                        Status::Rejected
                    };
                    let error_line = sub
                        .output
                        .lines()
                        .find(|l| l.contains("ACL2 Error"))
                        .map(|l| l.trim().to_string());
                    // Stop here. The world is sequential, so later units almost
                    // always depend on this one and continuing produces
                    // cascading noise rather than information.
                    out.failed = Some(FailureReport {
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
                    });
                    break;
                }
            }
        }
        Ok(out)
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
        self.label_gen += 1;
        let label = self.label_gen;
        let body = format!(
            "{CLEAR_STALE}\n{text}\n{EVENT_DATA_PROBE}\n{INNER_EVENTS_PROBE}\n\
             (acl2::with-output :off :all (acl2::deflabel {}))\n\
             (acl2::prog2$ (acl2::cw \"~%<<<ASS-DATA cmd ~x0>>>~%\" \
               (acl2::max-absolute-command-number (acl2::w acl2::state))) \
               (acl2::value :invisible))",
            Self::label_name(label)
        );
        let sub = self.session.submit(&body, deadline)?;
        if sub.outcome == Outcome::StillRunning {
            // Deadline expired; ACL2 is untouched and still working. Remember
            // what is needed to finish recording it when it reports.
            self.pending = Some(PendingUnit {
                index,
                hash: u.hash,
                line_start: u.line_start,
                line_end: u.line_end,
                label,
            });
        }
        let output_ref = self.outputs.put(&sub.output)?;
        if sub.outcome == Outcome::Ok {
            self.admitted.push(Admitted {
                hash: u.hash,
                line_start: u.line_start,
                line_end: u.line_end,
                label,
                command_number: sub.get("cmd").and_then(|v| v.parse().ok()),
                output_ref: output_ref.clone(),
            });
        }
        Ok((sub, output_ref))
    }

    /// Finish a unit whose deadline expired.
    ///
    /// Returns `None` when nothing is in flight. A completed unit is recorded
    /// exactly as `sync` would have recorded it, so the prefix invariant holds
    /// either way; the caller then syncs again to continue through the file.
    pub fn resume(&mut self, deadline: Duration) -> Result<Option<Resumed>, SyncError> {
        let Some(p) = self.pending.clone() else { return Ok(None) };
        let Some(sub) = self.session.resume(deadline)? else {
            self.pending = None;
            return Ok(None);
        };
        if sub.outcome == Outcome::StillRunning {
            return Ok(Some(Resumed::StillRunning { elapsed: sub.elapsed }));
        }
        self.pending = None;
        let output_ref = self.outputs.put(&sub.output)?;
        let output_lines = self.outputs.lines(&output_ref).unwrap_or(0);
        if sub.outcome == Outcome::Ok {
            self.admitted.push(Admitted {
                hash: p.hash,
                line_start: p.line_start,
                line_end: p.line_end,
                label: p.label,
                command_number: sub.get("cmd").and_then(|v| v.parse().ok()),
                output_ref: output_ref.clone(),
            });
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
        let (ed, cp, inner, inner_total) = self.probe_after_failure(deadline);
        let error_line = sub
            .output
            .lines()
            .find(|l| l.contains("ACL2 Error"))
            .map(|l| l.trim().to_string());
        Ok(Some(Resumed::Failed(FailureReport {
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
        })))
    }

    /// Progress on the unit in flight, if any.
    pub fn progress(&mut self, tail_lines: usize) -> io::Result<Option<crate::session::Progress>> {
        self.session.progress(tail_lines)
    }

    /// Abort the unit in flight and make the session usable again.
    pub fn interrupt(&mut self) -> io::Result<bool> {
        let ok = self.session.interrupt()?;
        if ok {
            self.pending = None;
        }
        Ok(ok)
    }

    /// The unit currently in flight, if any.
    pub fn running_unit(&self) -> Option<usize> {
        self.pending.as_ref().map(|p| p.index)
    }

    /// Read part of a captured output.
    pub fn output(&self, r: &OutputRef, slice: Slice) -> io::Result<OutputSlice> {
        self.outputs.get(r, slice)
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
    pub fn rebuild(&mut self, keep: usize, deadline: Duration) -> Result<(), SyncError> {
        let bytes = std::fs::read(&self.path)?;
        let units = split(&bytes).map_err(SyncError::Split)?;
        let keep = keep.min(units.len());

        let dir = self.path.parent().unwrap_or(Path::new(".")).to_path_buf();
        // Assigning drops the old Session, whose Drop kills the child.
        self.session = Session::start(&self.acl2, &dir)?;
        self.admitted.clear();
        self.label_gen = 0;
        self.rebuilds += 1;
        self.plant_base_label()?;

        for u in units.iter().take(keep) {
            let (sub, _) = self.admit_unit(u, 0, &bytes, deadline)?;
            if sub.outcome != Outcome::Ok {
                // A unit that admitted before but not now means the world we
                // were trying to restore was never reachable. Fail loudly.
                return Err(SyncError::ReplayFailed {
                    line_start: u.line_start,
                    output: sub.output.clone(),
                });
            }
        }
        Ok(())
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
        let target_label = if keep == 0 {
            0
        } else {
            self.admitted[keep - 1].label
        };
        // `ubu!` is a macro that passes its argument into a *function* call, so
        // the label is evaluated and must be quoted. Unquoted it is simply an
        // unbound variable, and the resulting translate error aborts the `ld`
        // silently -- which is why the outcome is checked below rather than
        // only the reported command number.
        let body = format!(
            "(acl2::ubu! '{})\n\
             (acl2::prog2$ (acl2::cw \"~%<<<ASS-DATA cmd ~x0>>>~%\" \
               (acl2::max-absolute-command-number (acl2::w acl2::state))) \
               (acl2::value :invisible))",
            Book::label_name(target_label)
        );
        let sub = self.session.submit(&body, deadline)?;
        if sub.outcome != Outcome::Ok {
            return Err(SyncError::RollbackFailed { output: sub.output });
        }
        let actual: Option<i64> = sub.get("cmd").and_then(|v| v.parse().ok());

        // P3: the fast path is verified against a recorded invariant. A subtle
        // undo surprise should cost latency, not correctness.
        if keep > 0 {
            if let Some(expected) = self.admitted[keep - 1].command_number {
                if actual != Some(expected) {
                    return Err(SyncError::RollbackUnverified { expected, actual });
                }
            }
        }
        // Output from rolled-back units stays readable -- comparing against
        // what a form used to print is useful -- but is flagged so it cannot be
        // mistaken for a description of the current world.
        for a in &self.admitted[keep..] {
            self.outputs.mark_superseded(&a.output_ref);
        }
        self.admitted.truncate(keep);
        Ok(())
    }

    /// Lines currently believed to be admitted, for reporting.
    pub fn admitted_lines(&self) -> Vec<(usize, usize)> {
        self.admitted.iter().map(|a| (a.line_start, a.line_end)).collect()
    }
}

/// Whether a session is answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Alive,
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
