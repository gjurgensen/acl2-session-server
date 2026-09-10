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
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use crate::capability::{Capabilities, Capability};
use crate::induction::{EventInductions, Induction, InductionStore};
use crate::session::{Outcome, Session, SessionControl};
use crate::output::{OutputRef, OutputSlice, OutputStore, Slice};
use crate::split::{split, Form, SplitError};

mod probe;

/// The probes this module submits and the parsers for what they print.
///
/// Named here rather than reached through `probe::` at each site, because the
/// call sites read as ACL2 verbs -- `submit(CHECKPOINT_PROBE, ...)` -- and a
/// path prefix on each would say only where the string is kept. What the
/// module buys is that a probe and its parser live beside each other; see its
/// own documentation for why that matters.
use probe::{
    detail_probe, doc_topic_probe, flatten, inner_names,
    inner_events_probe, is_nil, is_safe_symbol, items, keyed_block, parse_census,
    parse_checkpoints, parse_event_data, parse_inner, parse_value, rules_probe,
    sexp_elements, sexp_field, strip_symbol_acl2, symbol_name_of, unquote, CENSUS_PROBE, CHECKPOINT_PROBE, CLEAR_STALE,
    capability_probe, parse_capabilities,
    CMD_PROBE, DEPS_PROBE, DOC_INDEX_PROBE, EVENT_DATA_PROBE, INTERRUPT_PROBE, OWN_OUTPUT,
};

pub(crate) use probe::error_message;

/// One unit the session has actually admitted.
#[derive(Debug, Clone)]
struct Admitted {
    hash: u64,
    at: Span,
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
    /// Length of `include-book-alist-all` just after this unit.
    ///
    /// This is how a stale dependency is attributed to the unit that pulled
    /// it in -- which matters because a transitively or locally included book
    /// corresponds to no form in this file at all. The list is newest-first,
    /// so an entry at position `p` of a list of length `L` was the `L - p`th
    /// ever pushed, and the unit that added it is the first whose count
    /// reached that. Arithmetic, from one integer per unit.
    dep_count: Option<usize>,
}

/// A book the session's world was built from, as it looked when first seen.
#[derive(Debug, Clone)]
struct Dep {
    /// The cheap filter: ACL2 keys certification on a source's length and
    /// write date, and so do we -- but only to decide whether to bother
    /// reading.
    len: u64,
    mtime: Option<SystemTime>,
    /// The verdict. Content, like the file-prefix invariant itself: a
    /// `git checkout`, a rebase, an editor's save-without-change or a restore
    /// from backup all move the write date without changing a byte, and each
    /// would otherwise re-prove everything downstream for nothing.
    ///
    /// Raw bytes, deliberately *not* normalised the way this file's own forms
    /// are. Normalising someone else's book would mean splitting hundreds of
    /// files we have no business parsing, on every check; and a comment-only
    /// edit to a dependency genuinely does put it out of step with its
    /// certificate, which is worth knowing rather than hiding.
    hash: u64,
}

/// A dependency whose source has changed since the world was built on it.
#[derive(Debug, Clone)]
pub struct StaleInclude {
    /// As ACL2 names it: an absolute path, or `(:system . "rel.lisp")`.
    pub book: String,
    /// Where it resolved to.
    pub path: PathBuf,
    /// The unit that pulled it in, when it was one of this file's own.
    /// `None` for a book the session environment supplied.
    pub unit: Option<usize>,
    /// That unit's first line, for callers that speak in lines rather than
    /// indices. Tracks `unit` exactly: both are set or neither is.
    pub line: Option<usize>,
    /// And its column, which is what tells it from a form sharing its line.
    pub column: Option<usize>,
    /// The file is gone entirely, rather than merely different.
    pub missing: bool,
}

/// What [`Book::includer_of`] found: the unit, and where it starts.
struct Includer {
    unit: Option<usize>,
    line: Option<usize>,
    column: Option<usize>,
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

/// The floor under bookkeeping submissions -- rollbacks, the post-failure
/// probes, and the world queries behind hover, completion and the doc views.
/// A submission sent with a zero deadline still goes in flight and instantly
/// times out, wedging the session behind our own bookkeeping; and a genuine
/// failure at budget exhaustion would lose its envelope. These are sub-second
/// in practice, so the floor is the bound on how far a call can overrun its
/// deadline, not a cost every call pays.
///
/// Every probe this module generates itself belongs under it. The caller
/// asked for a name's documentation, not for a submission of ours to become
/// the thing their next sync is blocked behind.
const PROBE_FLOOR: Duration = Duration::from_secs(10);

/// How long the rules scrape may run before it is called off.
///
/// Fixed, and deliberately not derived from the caller's deadline the way
/// every other probe's is. The caller's deadline is for the *form*, which may
/// legitimately prove for minutes; this is optional bookkeeping, and taking a
/// proof's patience for it is how a best-effort probe ends up owning a
/// session for the hour an agent asked for.
///
/// Two seconds is about ten times the largest real scrape measured -- 668
/// names from `kestrel/c/syntax/validator.lisp`'s `defines` took 190-207 ms,
/// of which untranslation was 5-21 ms -- and about fourteen hundred times the
/// slowest single term: 1.4 ms, for the flag-equivalences theorem of that
/// file's thirty-way mutual recursion. Untranslating *every* term in that
/// book's whole world, all 39,192 of them, takes 0.80 s.
///
/// Not smaller, though the waste on a hang is the obvious thing to minimise,
/// because the costs are not symmetric. Recovery from a timeout is an
/// interrupt handshake of up to four 1.5-second rounds plus a second probe,
/// and it leaves that form's terms translated until it is admitted again. So
/// being wrong in the "too short" direction costs more than being wrong in
/// the other, and only the "too short" direction is reachable by anything
/// anyone has run.
const SCRAPE_BUDGET: Duration = Duration::from_secs(2);

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
    /// The *kinds* of warning the event emitted -- `Double-rewrite`, `Use`
    /// -- one per entry, as ACL2 records them.
    ///
    /// Kinds only: `get-event-data` deduplicates, so a form emitting five
    /// double-rewrite warnings and one emitting a single warning record
    /// identically here. Multiplicity exists solely in the printed output,
    /// which is where a client counting them must look.
    pub warnings: Vec<String>,
    /// How many xdoc topics the world held *after* this event.
    ///
    /// A running total, not this form's contribution: the difference against
    /// the previous form is what says whether this one documented anything,
    /// and the caller has both. Kept as the total because that is what
    /// survives a rollback correctly -- the table is world state, so `ubu!`
    /// restores it, and a stored delta would not know that had happened.
    ///
    /// `Some(0)` in a world with no xdoc, which is an ordinary state and not
    /// a failure. `None` only when the probe did not run at all.
    ///
    /// The probe names no package. `xdoc::` in a world without XDOC is a
    /// *read* error that aborts from raw Lisp and takes the session with it --
    /// measured, and it ends the whole submission rather than one form -- so
    /// both symbols are interned from strings at runtime behind a guard on
    /// `known-package-alist`. That is what lets this ride along on the probe
    /// every form already pays for, instead of needing one of its own.
    ///
    /// The guard is `find-non-hidden-package-entry`, not a plain `assoc`, and
    /// the difference is a whole class of false failures. An `encapsulate`
    /// whose second pass drops a `local` `include-book` that had introduced
    /// XDOC does not forget the package: `encapsulate-fix-known-package-alist`
    /// keeps the entry and marks it *hidden*. A plain `assoc` still finds it,
    /// `intern$` does not accept it -- `pkg-imports` tests exactly the
    /// non-hidden predicate used here -- and the guard violation failed the
    /// unit the probe was spliced into. Measured: `(encapsulate () (local
    /// (include-book "arithmetic/top" :dir :system)) (defthm ...))`, admitted
    /// by ACL2 with a `Q.E.D.`, reported Rejected. Asking ACL2's own question
    /// is what keeps the two from drifting apart again.
    pub xdoc_topics: Option<usize>,
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
    /// The name ACL2 keyed this event under, or empty where it has none.
    ///
    /// A `thm` is the nameless case, and the only one: it proves a goal and
    /// introduces nothing, so `event-data-name` returns nil for it. Empty
    /// rather than absent because every other field here is positional
    /// against it -- and because "this event has no name" is a thing to show,
    /// not a reason to drop what it cost.
    pub name: String,
    /// Non-empty when this is the inner event that failed -- and only then.
    /// `revert`/`maybe-revert` are filtered out by [`failure_causes`] before
    /// this is set, because ACL2 records them for a proof that reverted to
    /// induction and *succeeded*, which is not a failure.
    pub abort_causes: Option<String>,
    /// Runes this inner event used, one per entry.
    pub rules: Vec<String>,
    /// The event's kind -- `defun`, `defthm`, `verify-guards` -- read from
    /// what ACL2 recorded, so a failure can say *what kind of thing* failed
    /// without touching prover prose.
    pub kind: Option<String>,
    /// What a `thm` set out to prove, as ACL2 recorded the form.
    ///
    /// Only a `thm` has one, and only a `thm` needs one: every other event
    /// writes a name, and what it proved can be looked up in the world under
    /// that name. A `thm` writes nothing, so this is the only record of what
    /// it was about -- the `form` field ACL2 keeps for it is the elided
    /// string "( THM ...)".
    pub goal: Option<String>,
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
    /// Hints this event's proof appealed to -- `(:use lemma)`, `(:by thm)`,
    /// `(:clause-processor fn)`.
    ///
    /// Exactly those three, which is narrower than "the hints you wrote":
    /// `:in-theory`, `:expand`, `:induct` and the rest record nothing here,
    /// measured. What the three have in common is that each makes the proof
    /// depend on something *named and outside itself*, which is why ACL2
    /// reports them beside the runes and shapes them like runes.
    ///
    /// Read per event, and that is the whole point of reading it here.
    /// [`EventData::hint_events`] is the same field asked of the *outer*
    /// event, and ACL2 leaves that empty for every compound form -- a `progn`
    /// of hinted theorems reports nothing at all there, measured. So the
    /// form-level field answers only for a form that is a single event, and
    /// this one answers for the rest.
    pub hint_events: Vec<String>,
    /// Rules whose application split this event's proof into cases.
    ///
    /// Per event for the same reason as [`Self::hint_events`], and built by
    /// ACL2 in the same breath: `print-rules-and-hint-events-summary` writes
    /// both from one accumulated ttree and then clears it, which is why
    /// neither survives to the outer event of a compound form.
    pub splitter_rules: Vec<String>,
    /// The inductions this event performed, newest first.
    ///
    /// Empty for the overwhelming majority of events, which never induct, and
    /// empty past the detail slots -- the same "not read" ambiguity every
    /// other per-event field here carries, resolved by `inner_total`.
    ///
    /// Per *event*, which is the whole point and was the thing the old
    /// log-parsing module could not do. See [`Induction`].
    pub induction: Vec<Induction>,
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
    /// Columns of the same two positions. See [`Form::col_start`].
    pub col_start: usize,
    pub col_end: usize,
    /// The names this form writes, from the split. A label, not a key: a
    /// compound form writes several and an `include-book` writes none.
    pub names: Vec<String>,
    pub elapsed: Duration,
    pub event_data: EventData,
    /// What the form evaluated to, when that says anything the events do not.
    /// See [`FormValue`].
    pub value: Option<FormValue>,
    /// Events recorded inside this unit, newest first. See `InnerEvent`.
    pub inner: Vec<InnerEvent>,
    /// Total inner events ACL2 recorded, which may exceed `inner.len()`: a
    /// fixed number of slots is read, and the count says when there were more.
    pub inner_total: usize,
    /// The *rest* of any recursion clique these events belong to: members
    /// the world knows about and `inner` does not name.
    ///
    /// Needed because ACL2 records a `mutual-recursion` as a *single* event.
    /// Admit `(mutual-recursion (defun f …) (defun g …))` and `inner` names
    /// `f` alone with `inner_total` 1, while `g` appears nowhere in the event
    /// data at all -- not as thinned detail, but as a name the record simply
    /// does not have. The world's `recursivep` property does, and answers in
    /// microseconds against a 250k-entry world, so this is asked for on every
    /// admission rather than reconstructed from the text.
    ///
    /// Empty for the overwhelming majority of forms: a singly-recursive
    /// function's own clique is itself, which `inner` already named.
    pub clique: Vec<String>,
    /// Handle to everything ACL2 printed for this unit. Not inlined: a single
    /// `defthm` can emit six figures of output, and a report that carried it
    /// would be the very payload this design exists to avoid.
    pub output_ref: OutputRef,
    pub output_lines: usize,
}

/// How many events a form may record and still be *naming* rather than
/// generating.
///
/// Only consulted when the source gave nothing usable, which is the one case
/// where ACL2's list stands alone and its order -- newest first -- decides
/// what fits the budget. The gap between the two situations is enormous
/// rather than marginal: a form that merely hid its name behind a macro
/// records one or two events, while `std::deflist` records **36** for one
/// line of source and `fty::defprod` **37**. Past this, no honest name can
/// be recovered, and two arbitrary generated lemmas are worse than silence.
const GENERATED_NAMES: usize = 3;

impl UnitReport {
    /// What to call this form, best available.
    ///
    /// ACL2's record is the authority and the source supplies the reading of
    /// it. Neither alone is right, and the failures are not hypothetical --
    /// every case below was measured against a real image.
    ///
    /// **ACL2 vetoes the scan.** The scan takes the second element of any
    /// `def…` form, which is the name in nearly every definer there is but is
    /// wrong in two ways it cannot detect: `(std::defredundant :names (f g))`
    /// puts a keyword there, and `(defattach stub impl)` puts the name of a
    /// function defined elsewhere. So when ACL2 recorded events for this form
    /// and *none* of the written names is among them, the scan misread the
    /// form and its answer is discarded.
    ///
    /// **The scan supplies what ACL2 does not name.** When one written name
    /// *is* confirmed, the written list is used whole. `defines` records
    /// `p-h` and a pile of flag machinery, and the clique's own name
    /// `p-defines` is not an event at all -- it is an xdoc topic, and only
    /// the text has it.
    ///
    /// Clique *members* are no longer among those, though: ACL2 records a
    /// `mutual-recursion` as one event, and `UnitReport::clique` asks the
    /// world's `recursivep` for the rest. So a clique the text cannot see --
    /// one a macro generated -- is now named in full from the world alone.
    ///
    /// **And the scan is the only source for a whole class of events.**
    /// `defmacro`, `defconst`, `deftheory`, `defabbrev` and `defxdoc` record
    /// no event data whatever, so an empty record means "ACL2 does not track
    /// this kind", not "this form defined nothing".
    ///
    /// **The written name wins on generators.** `(fty::defprod point …)`
    /// records 37 events -- `point-fix$inline` twice, two `tmp-deftypes-…`
    /// scaffolding lemmas -- and orders them newest first, so `point` lands
    /// 12th and a budget spent on that list never reaches it. The thing a
    /// reader means by that form is the word they typed.
    ///
    /// The one case still misreported is `defattach`, which records nothing
    /// and so cannot be vetoed; it is labelled with the function it attaches
    /// to rather than with nothing. `defun-inline` reports ACL2's
    /// `p-inl$inline` rather than the written `p-inl`, the veto working as
    /// intended on a genuine difference.
    pub fn display_names(&self) -> Vec<String> {
        // ACL2 records newest first, and deduped: `point-fix$inline` is
        // recorded twice for one `defprod`. The clique members follow, since
        // they are names of this form too -- just ones the event data never
        // mentions.
        let mut recorded: Vec<&str> = Vec::new();
        // A nameless event -- a `thm` -- writes nothing, so it names nothing.
        for name in self.inner.iter().rev().map(|e| &e.name)
            .filter(|n| !n.is_empty())
            .chain(self.clique.iter())
        {
            if !recorded.contains(&name.as_str()) {
                recorded.push(name);
            }
        }
        if recorded.is_empty() {
            return self.names.clone(); // ACL2 tracks nothing of this kind.
        }
        if self.names.iter().any(|n| recorded.contains(&n.as_str())) {
            return self.names.clone(); // Vouched for, clique members and all.
        }
        if self.inner_total <= GENERATED_NAMES {
            return recorded.into_iter().map(str::to_string).collect();
        }
        Vec::new()
    }
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
    /// Columns of the same two positions. See [`Form::col_start`].
    pub col_start: usize,
    pub col_end: usize,
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
    /// The same unit's first line.
    ///
    /// Carried because no caller can work it out: the form a target resolves
    /// to is usually one that was *reused*, and reused units are reported as
    /// a count with no records to look it up in.
    pub resolved_through_line: Option<usize>,
    /// That unit's first column, which is what tells it from a neighbour
    /// sharing its line.
    pub resolved_through_column: Option<usize>,
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
    /// Books the world rests on whose sources have changed since it was
    /// built on them. Reported, not acted on: the world is still exactly
    /// what these forms admitted, but it no longer describes what is on
    /// disk, and only the caller knows whether re-proving now is worth it.
    pub stale_includes: Vec<StaleInclude>,
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
    /// What the form evaluated to. See [`FormValue`].
    ///
    /// Not filtered against the names it introduced, the way a unit's is: an
    /// eval's events are undone, so there is no event list beside it for the
    /// value to be repeating.
    pub value: Option<FormValue>,
    pub output_ref: OutputRef,
    pub output_lines: usize,
    pub elapsed: Duration,
}

/// What a server-generated read-only probe reported. See [`Book::query`].
///
/// There is deliberately no `output_ref` and no failure envelope: a query's
/// log is not worth an `OutputStore` slot, and a query that fails is a broken
/// probe rather than something a user is debugging -- so what a caller needs
/// is the raw text, to see what the probe actually printed.
#[derive(Debug)]
pub struct QueryResult {
    pub outcome: Outcome,
    /// One-line `<<<PROBE-DATA key value>>>` pairs the probe printed.
    pub data: Vec<(String, String)>,
    /// `<<<PROBE-BEGIN key>>>`/`<<<PROBE-END key>>>` blocks -- where anything that
    /// can wrap at ACL2's right margin has to go.
    pub blocks: Vec<(String, String)>,
    /// Everything ACL2 printed, for diagnosing a probe. Not a parsing surface.
    pub output: String,
    pub elapsed: Duration,
    /// The form moved the command number, so it was not the read-only probe
    /// its caller promised. Undone regardless; surfaced because it is a bug.
    pub world_changed: bool,
}

impl QueryResult {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.data.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn block(&self, key: &str) -> Option<&str> {
        self.blocks.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// What kind of thing a name is, as the world records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameKind {
    Function,
    Macro,
    Theorem,
    Constant,
    /// Known to the world, but none of the above -- a label, a stobj, a
    /// package. Reported rather than hidden: "it exists" is already the
    /// answer to "did my form go in".
    Other,
}

/// Where a name came from.
///
/// The three cases a reader actually cares about, and they are not
/// distinguishable from the book alone: ACL2's own definitions belong to no
/// `include-book`, and neither do this file's, so both report nothing there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Introduced by a form in this file.
    File,
    /// Introduced by a book this file includes.
    Book,
    /// Already present when the session opened: ACL2 itself, the image's own
    /// build, or a portcullis. Not something the session can point into.
    Builtin,
}

/// What the world knows about one name. See [`Book::describe`].
#[derive(Debug, Clone)]
pub struct NameDetail {
    pub name: String,
    pub kind: NameKind,
    pub origin: Origin,
    /// Formals for a function, the argument list for a macro.
    pub formals: Option<String>,
    /// The guard term, when it is not trivially `t`.
    pub guard: Option<String>,
    /// The statement, for a theorem.
    pub statement: Option<String>,
    /// The book whose *text* defines it, as ACL2 names it. `None` means the
    /// name came from this file or from ACL2 itself -- the two are told apart
    /// by whether the file's own units introduced it, which the caller knows
    /// and the world does not.
    pub book: Option<String>,
    /// `book` resolved to a path, when it could be.
    pub book_path: Option<PathBuf>,
}

/// One documented topic: its name and its one-line summary, as written.
///
/// The summary is *raw xdoc markup* -- it routinely contains `@(see foo)` and
/// `@('bar')`, which a renderer resolves. Shipping it unrendered is the whole
/// of the point: the machinery that would expand it is not in the world.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocTopic {
    pub name: String,
    pub short: String,
}

/// One name the world knows. See [`Book::census`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CensusEntry {
    /// As ACL2 prints it, package prefix included where one is needed.
    pub name: String,
    pub kind: NameKind,
    /// The book whose text defines it, as ACL2 names it. `None` means ACL2's
    /// own -- or this file's, which the caller separates by its own units.
    pub book: Option<String>,
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
    /// The anchored book is gone from disk.
    ///
    /// A session is anchored to one path for life, so a rename or a delete
    /// orphans it -- and the bare `NotFound` this used to surface named no
    /// file, prescribed nothing, and read like a transient I/O hiccup. The
    /// remedy is always the same, so the error says it.
    BookMissing { path: PathBuf },
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
            SyncError::BookMissing { path } => write!(
                f,
                "the book this session is anchored to no longer exists: {}. \
                 It was there when the session opened, so it has been renamed, \
                 moved, or deleted; a session cannot follow it. Open a new \
                 session on the current path.",
                path.display()
            ),
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

/// The default number of inner events the probes read detail for; see
/// [`Book::set_inner_detail`] to raise it.
///
/// Names are complete however many there are; this bounds only the detail.
/// A consumer is told where it stops, because "no rules" and "rules not read"
/// look identical otherwise -- the silent-incompleteness trap that made a
/// three-name `inner` list read as the whole story.
///
/// Three remains the right *default* because the detail that matters most is
/// always in reach -- `event-data-fal` is newest-first and a compound form
/// stops at the event that failed, so that event is at index 0 -- and because
/// an `include-book` unit can hold thousands of events whose per-event runes
/// would flood every admission's capture. An editor client, which renders
/// rather than pays per token, raises it to "everything".
pub const INNER_DETAIL_SLOTS: usize = 3;










/// Content hash of a file, or 0 when it cannot be read. The splitter's
/// hash, so the whole design keys on content in one way rather than two.
fn hash_file(path: &Path) -> u64 {
    std::fs::read(path).map(|b| crate::split::fnv1a(&b)).unwrap_or(0)
}













/// What a caller wants from one sync, beyond the deadline.
///
/// A struct rather than a run of positional booleans: the defaults are the
/// ordinary call, and every departure from it reads by name at the call site.
#[derive(Debug, Clone, Default)]
pub struct SyncOptions {
    /// Admit only as far as this line. `None` means the end of the snapshot.
    pub through_line: Option<usize>,
    /// The column on that line, for a caller that knows it.
    ///
    /// Only ever needed when two forms share a line, which is rare enough
    /// that a caller who cannot supply one loses nothing by omitting it: the
    /// line alone then means the whole line, and takes the last form on it.
    /// A client reading a real cursor -- an editor -- should always send it,
    /// since it is the only thing that can say *which* of the two.
    ///
    /// Ignored without `through_line`, which is the thing being refined.
    pub through_column: Option<usize>,
    /// Also undo everything after `through_line`. See [`Book::sync_with`].
    pub retract: bool,
    /// Report dependency staleness without acting on it.
    ///
    /// By default a book whose source changed under the session invalidates
    /// the unit that included it, exactly as editing that unit would: the
    /// world is supposed to be what the file says, and a world resting on a
    /// book that has since changed is not. This says "not now" -- for
    /// finishing the proof in front of you before picking up a neighbour's
    /// edit -- and is deliberately per call, so it cannot be set once and
    /// forgotten back into the silent staleness it exists to escape.
    pub ignore_stale_includes: bool,
    /// Admit every unit under `skip-proofs`.
    ///
    /// For loose interactive work where time matters more than truth: the
    /// forms go in, the proofs do not run, and `conditional_from` says from
    /// where the world is believed rather than proved. Never coupled to
    /// staleness recovery, and for a good reason -- the proofs a changed
    /// dependency would break are exactly the ones this skips, so recovering
    /// that way would turn the detection into theatre. Certify, or re-admit
    /// without the flag, before trusting anything proved under it.
    pub skip_proofs: bool,
}

/// What a form evaluated to.
///
/// Asked of `ld-history` by `VALUE_PROBE` rather than read back out of what
/// `ld` printed, because the printed form cannot be told apart from the
/// form's own output -- see the probe for the measurements.
///
/// Absent rather than empty when there is nothing to show. ACL2's own rule
/// decides that, in the probe: a triple whose error component is non-nil or
/// whose value is `:invisible` prints nothing, and so yields no block here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormValue {
    /// The value, printed by ACL2 with `~x` and laid out as ACL2 laid it out.
    pub text: String,
    /// Whether the form returned an error triple rather than a single value.
    ///
    /// Carried because it is free and because it is a real distinction --
    /// ACL2 marks it itself, printing a leading `triple-print-prefix` space
    /// before a triple's value. Nothing in this crate branches on it.
    pub triple: bool,
}


/// Drop a value that only repeats a name the form introduced.
///
/// An event's value *is* its own name -- `(defun f ...)` returns `f` -- and
/// that name is already the heading of the event's own block. Saying it
/// twice is noise. Compared case-insensitively, because ACL2 prints symbols
/// in whatever case the current print settings call for while the split
/// records them as they were written.
fn value_unless_a_name(v: Option<FormValue>, names: &[String], inner: &[InnerEvent]) -> Option<FormValue> {
    let v = v?;
    let t = v.text.trim().to_ascii_lowercase();
    let repeats = names.iter().any(|n| n.to_ascii_lowercase() == t)
        || inner.iter().any(|e| e.name.to_ascii_lowercase() == t);
    if repeats { None } else { Some(v) }
}

/// One rune a name introduced, and what the world knows about it.
///
/// The fields are ACL2's own, keyword-keyed: `:hyps`, `:equiv`, `:lhs`,
/// `:rhs`, `:subclass`, `:loop-stopper`, `:trigger`, `:concls`, `:basic-ts`,
/// `:corollary`, `:clique`, `:controller-alist` -- whatever the rule's class
/// carries. They are kept as ACL2 rendered them (untranslated), in the order
/// it gave them, rather than mapped onto a fixed struct: the set differs per
/// rule class, and inventing a union of them would go stale against the next
/// class ACL2 grows.
#[derive(Debug, Clone)]
pub struct RuneDetail {
    /// `(:rewrite foo)`, `(:type-prescription bar)`.
    pub rune: String,
    /// Whether it is enabled in the session's current theory.
    pub enabled: Option<bool>,
    pub fields: Vec<(String, String)>,
}

impl RuneDetail {
    /// The rule class: `:rewrite`, `:forward-chaining`, `:definition`.
    pub fn class(&self) -> Option<&str> {
        self.rune.trim_start_matches('(').split_whitespace().next()
    }

    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// What the world knows about one name the file introduced.
///
/// Gathered when its form was admitted, not when someone asks. A session
/// serialises on one ACL2, so a question asked during a proof cannot be
/// answered by the prover -- which is exactly when an editor wants to show
/// what an earlier form defined. Scraping while the form is in hand and
/// serving from memory is what makes the answer available at all.
///
/// This is the `OutputStore` doctrine applied to the world: capture in full
/// while we can, hand it over on request.
#[derive(Debug, Clone)]
pub struct NameRules {
    pub name: String,
    /// Index of the unit that introduced it, so a rollback drops it.
    pub unit: usize,
    /// `:ideal` (logic mode, guards unverified), `:common-lisp-compliant`
    /// (guards verified), or `:program`.
    pub symbol_class: Option<String>,
    /// The measure term, for a recursive function -- read off the
    /// `justification` record with ACL2's own accessor rather than shipped as
    /// the record, which also carries the ruler-extenders and a subversivity
    /// flag and reads as neither.
    pub measure: Option<String>,
    /// The well-founded relation the measure decreases under. Absent when it
    /// is `o<`, which is the one every ordinary `defun` gets and would be
    /// noise on all of them.
    pub relation: Option<String>,
    /// Formals for a function, argument list for a macro.
    pub formals: Option<String>,
    pub guard: Option<String>,
    /// The statement, for a theorem.
    pub statement: Option<String>,
    pub constant: Option<String>,
    /// Every rune the name introduced, in ACL2's order. A rune with no rule
    /// info -- `:executable-counterpart`, `:induction` -- appears with an
    /// empty `fields`, which `:pr` does not show at all.
    pub runes: Vec<RuneDetail>,
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
    inductions: Arc<Mutex<InductionStore>>,
    /// Captured output, shared so `acl2_output`-style reads never queue
    /// behind this book's lock: a client reading a *completed* form's log
    /// while a sync grinds gets it immediately. Session calls lock it
    /// briefly per operation.
    outputs: Arc<Mutex<OutputStore>>,
    /// The unit whose submission has not reported yet.
    pending: Option<PendingUnit>,
    /// What the world knows about each name this file's forms introduced,
    /// scraped as each form went in. See [`NameRules`].
    rules: HashMap<String, NameRules>,
    /// Every book the world is currently built on, keyed by ACL2's name for
    /// it, as it looked when first observed. See `stale_includes`.
    deps: HashMap<String, Dep>,
    /// The names in ACL2's order, newest first, as of the last probe.
    ///
    /// Kept so that staleness can be re-checked from the filesystem alone.
    /// The list only changes when an `include-book` is admitted, so an idle
    /// poller needs no ACL2 round trip at all -- which is what lets the
    /// editor daemon watch dependencies on a timer without keeping its ACL2
    /// permanently busy.
    dep_names: Vec<String>,
    /// Length of `include-book-alist-all` before any unit was admitted: the
    /// books the session's own environment supplied, which belong to no unit.
    base_dep_count: Option<usize>,
    /// Command number just after the base label, i.e. the empty prefix.
    base_command_number: Option<i64>,
    /// `current-package` at the empty prefix, as the customization left it.
    /// The rollback target when everything is undone.
    base_package: Option<String>,
    /// Set when an undo could not be verified; the next sync rebuilds.
    dirty: bool,
    /// What a sync does at a failed unit. See [`SyncMode`].
    mode: SyncMode,
    /// The inner-events probe, generated for this book's detail depth. See
    /// [`Book::set_inner_detail`].
    inner_probe: String,
    /// Kept alongside `inner_probe` because the probe is a function of this
    /// *and* the capabilities; see `rebuild_inner_probe`.
    inner_detail: usize,
    capabilities: Capabilities,
    /// `inhibit-output-lst` as inherited, before the server removed PROVE.
    inherited_inhibit: Option<String>,
    /// The handle handed out for a form that outran its deadline.
    ///
    /// Held so the form's output lands under the handle the caller already
    /// has, rather than under a fresh one while the original stays empty
    /// forever. Cleared once filled.
    in_flight_output: Option<OutputRef>,
    /// Whether what is in flight is the *user's* `eval`.
    ///
    /// `resume` has to tell an eval from a probe, and until this it could not:
    /// with no `PendingUnit` attached it assumed an eval, because that was the
    /// only other thing that put a form in flight. It is not. Every generated
    /// probe -- `query`, the census, the deps and doc walks, the post-mortems
    /// -- submits the same way, and one that outruns its floored deadline came
    /// back through `resume` and was announced as `Resumed::Evaluated`: our
    /// own bookkeeping reported to the caller as the result of something they
    /// typed.
    ///
    /// Marked from the eval side rather than at every probe, because there is
    /// one `eval` and a dozen probes, and a probe added later would otherwise
    /// have to remember.
    in_flight_eval: bool,
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

/// Where a unit sat in the snapshot it was submitted from.
///
/// Four `usize`s that a positional argument list would happily transpose,
/// travelling as one value instead. Carried rather than re-derived for the
/// reason `PendingUnit::names` is: by the time a unit finishes, the file may
/// have changed under it.
#[derive(Debug, Clone, Copy)]
struct Span {
    line_start: usize,
    line_end: usize,
    col_start: usize,
    col_end: usize,
}

impl Span {
    fn of(u: &Form) -> Span {
        Span {
            line_start: u.line_start,
            line_end: u.line_end,
            col_start: u.col_start,
            col_end: u.col_end,
        }
    }
}

/// Enough to finish recording a unit whose deadline expired mid-flight.
#[derive(Debug, Clone)]
struct PendingUnit {
    index: usize,
    hash: u64,
    /// Carried rather than looked up: by the time a unit finishes, the file
    /// may have changed under it, so the split that produced it is the only
    /// place these names were ever right.
    names: Vec<String>,
    at: Span,
    label: u64,
    /// The package this unit switches to, when it is a top-level
    /// `(in-package ...)` that will need its second phase on completion.
    /// Detected at submission, because the file may change on disk while the
    /// unit runs.
    in_package: Option<String>,
    /// The unit's bytes exactly as submitted. A `ContinuePastFailure` retry
    /// at resume time must resubmit precisely what failed, and the snapshot
    /// it came from may be gone by then -- the file changed on disk, or the
    /// buffer moved on. Bytes, not text: see `Session::submit_bytes`.
    text: Vec<u8>,
    /// This submission *is* the skip-proofs retry. Its failure is terminal:
    /// never retry a retry.
    skipped: bool,
    /// The failure this submission is retrying under skip-proofs.
    ///
    /// Set only on a retry, and it is what makes the slow path answer like the
    /// fast one. A retry that finishes inside its deadline reports
    /// `skipped` with the original diagnosis attached; one that outruns it
    /// came back through `resume`, which had nothing to attach and so reported
    /// a plain `admitted` -- a green unit for a form whose proof failed, with
    /// its checkpoints, abort cause and error line dropped on the floor.
    failure: Option<Box<FailureReport>>,
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
            inductions: Arc::new(Mutex::new(InductionStore::default())),
            pending: None,
            rules: HashMap::new(),
            deps: HashMap::new(),
            dep_names: Vec::new(),
            base_dep_count: None,
            base_command_number: None,
            base_package: None,
            dirty: false,
            mode: SyncMode::StopAtFailure,
            // Rebuilt in `post_start_setup` once the image's capabilities are
            // known; until then the induction streams stay out, which is the
            // safe direction (see `induction_streams`).
            inner_probe: inner_events_probe(INNER_DETAIL_SLOTS, false),
            inner_detail: INNER_DETAIL_SLOTS,
            capabilities: Capabilities::default(),
            inherited_inhibit: None,
            books_dir: None,
            in_flight_output: None,
            in_flight_eval: false,
            build: None,
        };
        b.post_start_setup()?;
        Ok(b)
    }

    /// Everything a fresh ACL2 needs before its first unit.
    ///
    /// One function, called from `open` and from `rebuild`, so the recovery
    /// path cannot drift from the first-start path. A rebuild that skipped
    /// `OWN_OUTPUT` quietly re-inhibited PROVE under a customization that
    /// inhibits it -- losing the induction narrative from every capture
    /// after the first rebuild, a failure already on record,
    /// reintroduced by the recovery path -- and kept reporting the
    /// pre-rebuild environment answers.
    fn post_start_setup(&mut self) -> Result<(), SyncError> {
        // First, because it decides how other probes are *built*: an image
        // without `InductionRecords` must never be sent a probe that names
        // `weak-induction-record-p`, which would fail to translate and take
        // the whole inner-event probe with it.
        if let Ok(sub) = self.session.submit(&capability_probe(), Duration::from_secs(60)) {
            self.capabilities = parse_capabilities(&sub);
        }
        self.rebuild_inner_probe();
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

    /// How many inner events of a compound unit the probes read detail for
    /// (kind, rules, time, steps, abort causes). Names are always complete;
    /// this raises only the detail depth, [`INNER_DETAIL_SLOTS`] by default.
    ///
    /// Any `n` at least the unit's event count is complete. The editor
    /// daemon asks for everything; the agent-facing default stays small,
    /// because an `include-book` unit can hold thousands of events whose
    /// per-event runes would flood every admission's capture.
    pub fn set_inner_detail(&mut self, n: usize) {
        self.inner_detail = n;
        self.rebuild_inner_probe();
    }

    /// What the image can do. See [`crate::capability`].
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// Regenerate the inner probe from the detail depth *and* the image's
    /// capabilities, which is why both are kept rather than only the probe:
    /// either can change without the other, and the probe is a function of
    /// both. Raising the detail depth used to silently drop the induction
    /// streams, because it rebuilt the probe from the depth alone.
    fn rebuild_inner_probe(&mut self) {
        self.inner_probe = inner_events_probe(
            self.inner_detail,
            self.capabilities.has(Capability::InductionRecords),
        );
    }

    /// The induction store, shared for the same reason as the output store.
    pub fn induction_store(&self) -> Arc<Mutex<InductionStore>> {
        self.inductions.clone()
    }

    /// File a unit's inductions under its output handle, so they outlive the
    /// sync that saw them.
    ///
    /// Called for *every* unit, including the ones that inducted nothing --
    /// that is what makes an absent entry and an empty one mean the same
    /// thing to a reader, and it is why this is a method rather than a
    /// condition at each call site.
    fn record_inductions(&mut self, r: &OutputRef, inner: &[InnerEvent]) {
        let events: Vec<EventInductions> = inner
            .iter()
            .filter(|e| !e.induction.is_empty())
            .map(|e| EventInductions { event: e.name.clone(), records: e.induction.clone() })
            .collect();
        if events.is_empty() {
            return;
        }
        if let Ok(mut store) = self.inductions.lock() {
            store.put(r.as_str(), events);
        }
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
        self.base_dep_count = sub.get("ndeps").and_then(|v| v.parse().ok());
        // The environment's own books are the baseline; a rebuild re-observes
        // everything, so the map starts empty either way.
        self.deps.clear();
        self.dep_names.clear();
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
        let body =
            format!("{CLEAR_STALE}\n{form}\n{}\n{CMD_PROBE}", crate::session::VALUE_PROBE);
        // Before the submit, so that a form which outruns the deadline is
        // already marked when `resume` comes looking. Cleared by `finish_eval`
        // on whichever call completes it.
        self.in_flight_eval = true;
        let sub = self.session.submit(&body, deadline)?;
        self.finish_eval(sub, deadline)
    }

    /// Ask the session a read-only question, on behalf of the server.
    ///
    /// This is `eval`'s infrastructure sibling, and the two differ in exactly
    /// the ways an editor feature needs them to:
    ///
    /// - **No `CLEAR_STALE`.** An eval blanks `gag-state-saved`,
    ///   `last-event-data` and `event-data-fal` so that its own failure cannot
    ///   inherit a previous form's checkpoints. A query must do the opposite:
    ///   it runs *between* units, on behalf of a hover the user did not think
    ///   of as an action, and the machinery that reads those globals -- the
    ///   post-failure probe, and continue-past-failure's probe-before-retry,
    ///   which exists precisely because `gag-state-saved` survives a
    ///   skip-proofs admission -- must see the world exactly as it would have
    ///   without the query. A query that cleared them would make moving the
    ///   cursor change what the next retry reports.
    /// - **Nothing reaches the `OutputStore`.** Hovering a file walks hundreds
    ///   of names; storing a log apiece would evict the unit logs the form view
    ///   exists to show. The markers come back inline instead.
    /// - **The answer is structured, not prose.** Callers get the `PROBE-DATA`
    ///   pairs and `PROBE-BEGIN`/`PROBE-END` blocks their probe printed, which is
    ///   the same wrapping-tolerant protocol every other probe uses. Nobody
    ///   parses printed Lisp out of `output`; it is carried only so a
    ///   misbehaving probe can be diagnosed, which is the failure mode probes
    ///   actually have (they are strings, so `cargo` never checks them).
    ///
    /// **The contract is on the caller: `form` must be server-generated and
    /// world-neutral** -- `getpropc`, `global-val`, `table-alist`,
    /// `decode-logical-name`. This is not a user surface; user text goes to
    /// `eval`, which is built to survive it. The world is still checked
    /// afterwards, and a change is still undone, because a probe that defines
    /// something is a bug that should surface loudly rather than quietly leave
    /// the world describing something the file does not.
    pub fn query(&mut self, form: &str, deadline: Duration) -> Result<QueryResult, SyncError> {
        if self.session.is_running() {
            return Err(SyncError::Busy { index: self.running_unit() });
        }
        // Floored here rather than at each caller, because it follows from the
        // contract above: every `form` reaching this function is one we
        // generated. A caller's short deadline must not leave a probe of ours
        // in flight -- that wedges every later sync behind our own bookkeeping
        // and, worse, a later `resume` reports the probe as the user's eval.
        let deadline = deadline.max(PROBE_FLOOR);
        let body = format!("{form}\n{CMD_PROBE}");
        let sub = self.session.submit(&body, deadline)?;
        // Same reasoning as `finish_eval`: a form that failed aborted its `ld`
        // before the trailing probe could run, so the number has to be asked
        // for again rather than assumed unchanged.
        let after: Option<i64> = match sub.get("cmd") {
            Some(v) => v.parse().ok(),
            None if sub.outcome == Outcome::Failed => self
                .session
                .submit(CMD_PROBE, deadline.max(PROBE_FLOOR))
                .ok()
                .and_then(|s| s.get("cmd").and_then(|v| v.parse().ok())),
            None => None,
        };
        let mut world_changed = false;
        if let (Some(a), Some(e)) = (after, self.expected_command_number())
            && a != e
        {
            world_changed = true;
            let keep = self.admitted.len();
            match self.rollback_to(keep, deadline.max(PROBE_FLOOR)) {
                Ok(()) => {}
                Err(SyncError::RollbackFailed { .. })
                | Err(SyncError::RollbackUnverified { .. }) => self.dirty = true,
                Err(e) => return Err(e),
            }
        }
        Ok(QueryResult {
            outcome: sub.outcome,
            data: sub.data,
            blocks: sub.blocks,
            output: sub.output,
            elapsed: sub.elapsed,
            world_changed,
        })
    }

    /// What the world knows about one name -- the hover answer.
    ///
    /// Deliberately one round trip rather than a table lookup: measured at
    /// 10 ms including `decode-logical-name`'s world scan, which is fast
    /// enough that hover and go-to-definition never wait on the census walk.
    /// The census exists for completion, which is the
    /// one feature that has to enumerate.
    ///
    /// `Ok(None)` means the token is not a name this world knows -- an
    /// ordinary answer for a word under a cursor, not an error.
    pub fn describe(
        &mut self,
        name: &str,
        deadline: Duration,
    ) -> Result<Option<NameDetail>, SyncError> {
        if !is_safe_symbol(name) {
            return Ok(None);
        }
        // A name this file introduced was scraped when its form went in, so
        // answer from memory. That is not merely faster: a session
        // serialises on one ACL2, so during a proof this is the only way the
        // question can be answered at all -- and hovering over an earlier
        // form while a later one grinds is exactly when it is asked.
        if let Some(r) = self.rules.get(name) {
            let kind = if r.formals.is_some() {
                NameKind::Function
            } else if r.statement.is_some() {
                NameKind::Theorem
            } else if r.constant.is_some() {
                NameKind::Constant
            } else {
                NameKind::Other
            };
            return Ok(Some(NameDetail {
                name: name.to_string(),
                kind,
                origin: Origin::File,
                formals: r.formals.clone(),
                guard: r.guard.clone(),
                statement: r.statement.clone(),
                book: None,
                book_path: None,
            }));
        }
        let r = self.query(&detail_probe(name), deadline)?;
        let Some(body) = r.block("detail") else { return Ok(None) };
        let pairs = keyed_block(
            body,
            &["found", "formals", "guard", "margs", "thm", "const", "book", "evcmd"],
        );
        let get = |k: &str| {
            pairs
                .iter()
                .find(|(pk, _)| pk == k)
                .map(|(_, v)| flatten(v))
                .filter(|v| !v.is_empty() && v != ":none")
        };
        let found = get("found").is_some_and(|v| !is_nil(&v));
        let formals = get("formals");
        let margs = get("margs");
        let theorem = get("thm");
        let constant = get("const");
        if !found && formals.is_none() && margs.is_none() && theorem.is_none()
            && constant.is_none()
        {
            return Ok(None);
        }
        // Formals first: a name carrying both is a function with a macro
        // wrapper, and the function is the richer answer -- it has a guard.
        let kind = if formals.is_some() {
            NameKind::Function
        } else if margs.is_some() {
            NameKind::Macro
        } else if theorem.is_some() {
            NameKind::Theorem
        } else if constant.is_some() {
            NameKind::Constant
        } else {
            NameKind::Other
        };
        let book = get("book").filter(|b| !is_nil(b));
        let book_path = book.as_deref().and_then(|b| self.resolve_dep(b));
        // A name introduced after the base label came from this file's forms;
        // one at or below it was already in the world when the session opened.
        // Only asked when there is no book, since an included book's names are
        // above the baseline too -- they arrived with an `include-book` the
        // file admitted.
        let origin = if book.is_some() {
            Origin::Book
        } else {
            let ev: Option<i64> = get("evcmd").and_then(|v| v.parse().ok());
            match (ev, self.base_command_number) {
                (Some(e), Some(b)) if e > b => Origin::File,
                _ => Origin::Builtin,
            }
        };
        Ok(Some(NameDetail {
            name: name.to_string(),
            kind,
            origin,
            formals: formals.or(margs),
            guard: get("guard").filter(|g| !is_nil(g)),
            statement: theorem,
            book,
            book_path,
        }))
    }

    /// Every name in the world, with its kind and defining book.
    ///
    /// One walk over `(w state)`. Measured on `std/top` + `centaur/fty/top`
    /// (347k triples): **2.5 s, 25,582 names across 625 books**, about 1.1 MB
    /// of output. That is a background job at open, not
    /// something on any interactive path -- which is why `describe` exists
    /// separately and answers one name in 10 ms without any of this.
    ///
    /// Re-run when the include set changes, and not otherwise: the dependency
    /// baseline already watches for that.
    pub fn census(&mut self, deadline: Duration) -> Result<Vec<CensusEntry>, SyncError> {
        let r = self.query(CENSUS_PROBE, deadline)?;
        Ok(r.block("census").map(parse_census).unwrap_or_default())
    }

    /// Every documented topic's name and summary. See [`DOC_INDEX_PROBE`].
    ///
    /// Empty when the world has no xdoc at all, which is a real state and not
    /// an error -- the probe cannot even be read there, and that is fine.
    pub fn doc_index(&mut self, deadline: Duration) -> Result<Vec<DocTopic>, SyncError> {
        let r = self.query(DOC_INDEX_PROBE, deadline)?;
        let (Some(names), Some(shorts)) = (r.block("docnames"), r.block("docshorts")) else {
            return Ok(Vec::new());
        };
        let names = keyed_block(names, &["n"]);
        let shorts = keyed_block(shorts, &["s"]);
        Ok(names
            .iter()
            .zip(shorts.iter())
            .filter_map(|((_, n), (_, s))| {
                let short = flatten(s);
                // `nil` is ACL2 printing "this topic has no summary".
                let short = unquote(&short).unwrap_or(short);
                (!n.trim().is_empty() && !is_nil(&short))
                    .then(|| DocTopic { name: flatten(n), short })
            })
            .collect())
    }

    /// One topic's full documentation, as raw markup. See [`doc_topic_probe`].
    ///
    /// `Ok(None)` means the world documents no such topic, which is the
    /// ordinary answer for most names.
    pub fn doc_topic(
        &mut self,
        name: &str,
        deadline: Duration,
    ) -> Result<Option<String>, SyncError> {
        if !is_safe_symbol(name) {
            return Ok(None);
        }
        let bare = name.rsplit("::").next().unwrap_or(name).to_ascii_uppercase();
        let r = self.query(&doc_topic_probe(&bare), deadline)?;
        // `nil` is ACL2 printing "this topic has no body", the same answer
        // `doc_index` already reads that way for a missing summary. Passed
        // through, it reaches a reader as the word "nil" at the top of a
        // topic, and a caller asking "is there a body" as yes.
        Ok(r.block("topic")
            .map(str::to_string)
            .filter(|t| !t.trim().is_empty() && !is_nil(t)))
    }

    /// Turn a completed (or timed-out) eval submission into a result, undoing
    /// any world change. Shared with `resume` so the two cannot drift.
    fn finish_eval(
        &mut self,
        sub: crate::session::Submission,
        deadline: Duration,
    ) -> Result<EvalResult, SyncError> {
        if sub.outcome != Outcome::StillRunning {
            self.in_flight_eval = false;
        }
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
            value: parse_value(&sub),
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
        self.sync_with(&SyncOptions { through_line, ..Default::default() }, deadline)
    }

    /// `sync`, with the frontier allowed to move *backward* to the target.
    ///
    /// Retraction is asked for, never inferred. A target's line number is the
    /// one input in this system that goes stale constantly -- every edit above
    /// it shifts it -- and `resolve_target` is forgiving precisely because of
    /// that. Reading a *smaller* number as "undo" would make the stalest input
    /// available the trigger for discarding proven work; today the worst a
    /// stale number costs is admitting less than you meant to.
    ///
    /// With `retract`, the target becomes an upper bound on the reusable
    /// prefix as well as on admission: units past it are undone, and the
    /// reply reports them under `rolled_back` exactly as an edit would.
    /// Without a target there is nothing to retract to, and the call is an
    /// ordinary sync.
    pub fn sync_with(
        &mut self,
        opts: &SyncOptions,
        deadline: Duration,
    ) -> Result<SyncOutcome, SyncError> {
        let bytes = self.read_anchor()?;
        self.sync_bytes_with(&bytes, opts, deadline)
    }

    /// Read the anchored file, distinguishing "gone" from any other I/O
    /// failure. See [`SyncError::BookMissing`].
    fn read_anchor(&self) -> Result<Vec<u8>, SyncError> {
        std::fs::read(&self.path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => SyncError::BookMissing { path: self.path.clone() },
            _ => SyncError::Io(e),
        })
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
        self.sync_bytes_with(
            bytes,
            &SyncOptions { through_line, ..Default::default() },
            deadline,
        )
    }

    /// `sync_bytes`, with everything a caller can ask for. See
    /// [`SyncOptions`].
    pub fn sync_bytes_with(
        &mut self,
        bytes: &[u8],
        opts: &SyncOptions,
        deadline: Duration,
    ) -> Result<SyncOutcome, SyncError> {
        let (through_line, retract) = (opts.through_line, opts.retract);
        if self.session.is_running() {
            return Err(SyncError::Busy { index: self.running_unit() });
        }
        // One budget for the whole call: units, rollbacks and probes all draw
        // it down, so the deadline bounds the call as documented rather than
        // each submission separately. (Session startup during a rebuild sits
        // outside it, bounded by its own fixed deadlines: a hung
        // customization file was never this call's proof work.)
        let budget = Budget::new(deadline);
        let was_dirty = self.dirty;
        if was_dirty {
            // An undo we could not verify: rebuild rather than trust it. With
            // `keep` 0 nothing is replayed, so this cannot report in-flight;
            // rebuild itself clears the flag.
            self.rebuild(bytes, 0, &budget)?;
        }
        let units = split(bytes).map_err(SyncError::Split)?;

        // `rebuilt` is carried across the outcome's construction because the
        // rebuild had to happen first: it is a whole ACL2 process discarded and
        // every unit re-proved from nothing, which is precisely the latency
        // `rebuilt` exists to attribute. `retract_to` already reports its own
        // version of this situation; reporting `rebuilt: false` here made the
        // most expensive path the one that looked cheapest.
        let mut out = SyncOutcome { rebuilt: was_dirty, ..SyncOutcome::default() };
        // Before the diff, because it describes the world as this call found
        // it: what the *previous* sync built, measured against disk now.
        let deps_before = self.dep_count_now();
        out.stale_includes = self.stale_includes(budget.floored());

        // Longest common prefix by hash. This is the whole diff: the client
        // never says what changed, we just re-derive it.
        //
        // The diff runs *before* the target is even resolved: rollback is
        // driven by the file changing, never by the target. Resolving first
        // and returning early left the whole stale world in place -- for a
        // `through_line` above the first unit, or a file emptied of forms --
        // while reporting `reused: 0` as though nothing were admitted.
        let hashes: Vec<u64> = self.admitted.iter().map(|a| a.hash).collect();
        let mut keep = common_prefix(&hashes, &units);

        // A book that changed under us invalidates the unit that included it,
        // exactly as editing that unit would. The world is meant to be what
        // the file says, and a world resting on a book that has since changed
        // is not -- so the ordinary diff and this one converge on the same
        // rollback, and the reply reports the cost under `rolled_back` either
        // way. `stale_includes` still names what happened, so a sync that
        // suddenly re-proves a book is attributable rather than mysterious.
        //
        // Only a unit can be invalidated this way. A book the *environment*
        // supplied is reported with no unit and cannot be acted on here:
        // re-admitting units would not re-include it, and only a fresh
        // session would.
        let mut folded = false;
        if !opts.ignore_stale_includes {
            for st in &out.stale_includes {
                if let Some(u) = st.unit {
                    keep = keep.min(u);
                    folded = true;
                    // Forget what this book looked like. Re-including it is
                    // about to build the world on the *new* file, and an
                    // entry surviving the round trip would compare the new
                    // file against the old baseline forever -- reporting it
                    // stale on every later sync.
                    self.deps.remove(&st.book);
                }
            }
        }

        // Retraction bounds the reusable prefix, and is applied before the
        // rollback below so that undoing on request and undoing because the
        // file changed are the same operation, reported the same way. A
        // target that resolves to nothing -- a line above the first unit, or
        // a snapshot with no units at all -- retracts everything.
        let target = resolve_target(&units, through_line, opts.through_column);
        if retract {
            keep = keep.min(target.map_or(0, |t| t + 1));
        }
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

        let Some(target) = target else {
            // Nothing selected; nothing to admit.
            out.conditional_from = self.first_skipped();
            self.baseline_new_deps(deps_before, folded, &budget);
            return Ok(out);
        };
        out.resolved_through = Some(target);
        // The units are in hand here and nowhere downstream: a target usually
        // resolves to a reused form, and reused forms leave no record to look
        // a line up in.
        out.resolved_through_line = units.get(target).map(|u| u.line_start);
        out.resolved_through_column = units.get(target).map(|u| u.col_start);

        for (i, u) in units.iter().enumerate().take(target + 1).skip(keep) {
            let (sub, output_ref) =
                self.admit_unit(u, i, bytes, opts.skip_proofs, budget.remaining())?;
            let output_lines = self.outs().lines(&output_ref).unwrap_or(0);
            match sub.outcome {
                Outcome::Ok => {
                    let (inner, inner_total, clique) = parse_inner(&sub);
                    self.record_inductions(&output_ref, &inner);
                    out.admitted.push(UnitReport {
                        index: i,
                        line_start: u.line_start,
                        line_end: u.line_end,
                        col_start: u.col_start,
                        col_end: u.col_end,
                        names: u.names.clone(),
                        elapsed: sub.elapsed,
                        event_data: parse_event_data(&sub),
                        value: value_unless_a_name(parse_value(&sub), &u.names, &inner),
                        inner,
                        inner_total,
                        clique,
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
                    self.record_inductions(&output_ref, &inner);
                    let mut report = FailureReport {
                        index: i,
                        line_start: u.line_start,
                        line_end: u.line_end,
                        col_start: u.col_start,
                        col_end: u.col_end,
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
                        let (rsub, rref) = self.admit_bytes(
                            &bytes[u.start..u.end],
                            u.hash, Span::of(u), &u.names, i, true,
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
                                // And the envelope goes with the pending unit,
                                // so `resume` can report the same thing this
                                // path would have. Without it the retry
                                // finished as a plain `admitted`: a green unit
                                // for a form whose proof failed, and the whole
                                // diagnosis lost.
                                if let Some(p) = self.pending.as_mut() {
                                    p.failure = Some(Box::new(report.clone()));
                                }
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
        self.baseline_new_deps(deps_before, folded, &budget);
        Ok(out)
    }

    /// Length of `include-book-alist-all` as the last admitted unit saw it.
    fn dep_count_now(&self) -> Option<usize> {
        match self.admitted.last() {
            Some(a) => a.dep_count,
            None => self.base_dep_count,
        }
    }

    /// Record a baseline for books this sync newly included.
    ///
    /// Only a sync that admitted an `include-book` pays for this, and it is
    /// how the check stays honest: a book first seen at the *start* of some
    /// later sync would be baselined against a file that may already have
    /// changed, and would then never be reported stale. Baselining right
    /// after inclusion records the state the world was actually built on.
    fn baseline_new_deps(&mut self, before: Option<usize>, folded: bool, budget: &Budget) {
        // `folded` matters on its own: undoing an include and re-admitting it
        // leaves the count exactly where it started, so the count alone would
        // miss the very case that most needs a fresh baseline.
        if folded || self.dep_count_now() != before {
            let _ = self.stale_includes(budget.floored());
        }
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
                    // The control is told too: it is what a reader consults
                    // to decide whether a handle names a form still printing,
                    // and it must know before the handle is handed out.
                    self.control.set_in_flight(Some(fresh.to_string()));
                    self.in_flight_output = Some(fresh.clone());
                    fresh
                }
            };
            return Ok(r);
        }
        match self.in_flight_output.take() {
            Some(r) => {
                self.outs().fill(&r, &sub.output)?;
                self.control.set_in_flight(None);
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
        let (inner, inner_total, _clique) = match self.session.submit(&self.inner_probe, deadline) {
            Ok(sub) => parse_inner(&sub),
            Err(_) => (Vec::new(), 0, Vec::new()),
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
        skip_proofs: bool,
        deadline: Duration,
    ) -> Result<(crate::session::Submission, OutputRef), SyncError> {
        self.admit_bytes(
            &bytes[u.start..u.end],
            u.hash,
            Span::of(u),
            &u.names,
            index,
            skip_proofs,
            deadline,
        )
    }

    /// The submission itself, for a unit's text however obtained -- sliced
    /// from a snapshot (`admit_unit`), replayed by a rebuild, or retried by
    /// `ContinuePastFailure`, which is what SKIPPED means: the text goes in
    /// wrapped as `(acl2::skip-proofs <text>)`, the per-form mechanism
    /// chosen -- honest in the history (`pe` shows the wrap), and no
    /// ld-special mode for an abort to leak.
    #[allow(clippy::too_many_arguments)]
    fn admit_bytes(
        &mut self,
        text: &[u8],
        hash: u64,
        at: Span,
        names: &[String],
        index: usize,
        skipped: bool,
        deadline: Duration,
    ) -> Result<(crate::session::Submission, OutputRef), SyncError> {
        self.label_gen += 1;
        let label = self.label_gen;
        // The newline before the closing paren guards a unit that ends in a
        // line comment.
        // Assembled as bytes: the unit is ISO-8859-1 in general, and a
        // `String` in the middle of this would silently replace every byte
        // ACL2 needs verbatim. Everything we add around it is our own ASCII.
        let mut body: Vec<u8> = Vec::with_capacity(text.len() + 512);
        body.extend_from_slice(CLEAR_STALE.as_bytes());
        body.push(b'\n');
        if skipped {
            body.extend_from_slice(b"(acl2::skip-proofs\n");
            body.extend_from_slice(text);
            // The newline before the closing paren guards a unit that ends
            // in a line comment.
            body.extend_from_slice(b"\n)");
        } else {
            body.extend_from_slice(text);
        }
        // Immediately after the unit's own text and before every other probe:
        // this one reads the head of `ld-history`, so whatever runs between
        // the two is what it would describe instead.
        body.push(b'\n');
        body.extend_from_slice(crate::session::VALUE_PROBE.as_bytes());
        body.extend_from_slice(
            format!(
                "\n{EVENT_DATA_PROBE}\n{}\n\
                 (acl2::with-output :off :all (acl2::deflabel {}))\n{CMD_PROBE}",
                self.inner_probe,
                Self::label_name(label)
            )
            .as_bytes(),
        );
        // Named before submission, so a lock-free status reader can say which
        // unit is running while this call still holds the session.
        self.control.set_unit(Some(index));
        let sub = self.session.submit_bytes(&body, deadline)?;
        if sub.outcome == Outcome::StillRunning {
            // Deadline expired; ACL2 is untouched and still working. Remember
            // what is needed to finish recording it when it reports.
            self.pending = Some(PendingUnit {
                index,
                hash,
                names: names.to_vec(),
                at,
                label,
                in_package: in_package_target(text),
                text: text.to_vec(),
                skipped,
                // Filled in by the caller when this is a retry: `admit_bytes`
                // is told only that proofs are skipped, never what failed.
                failure: None,
            });
        }
        let output_ref = self.record_output(&sub)?;
        if sub.outcome == Outcome::Ok {
            self.admitted.push(Admitted {
                hash,
                at,
                label,
                command_number: sub.get("cmd").and_then(|v| v.parse().ok()),
                package: sub.get("pkg").and_then(unquote),
                dep_count: sub.get("ndeps").and_then(|v| v.parse().ok()),
                output_ref: output_ref.clone(),
                skipped,
            });
            // Second phase for a unit that is a top-level `(in-package ...)`:
            // its switch was scoped to the `ld` that just exited, so make it
            // stick. Unconditional rather than skipped-when-current: the raw
            // re-issue is idempotent and one round trip, and a staleness bug
            // in a skip condition here would silently misread every form
            // after the switch.
            if let Some(target) = in_package_target(text) {
                self.make_package_stick(&target, deadline.max(PROBE_FLOOR))?;
            }
            // Gather while the form is in hand. Asking later is not an
            // option: the next thing the session does may be a proof, and
            // then nothing can be asked of ACL2 at all.
            let introduced = inner_names(&sub);
            self.scrape_rules(&introduced, index);
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
             (acl2::prog2$ (acl2::cw \"~%<<<PROBE-DATA pkg ~x0>>>~%\" \
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
            if !self.in_flight_eval {
                // A probe of ours, not the caller's eval. Discarded rather
                // than reported: nobody asked for it, and announcing it as an
                // `Evaluated` handed the caller our bookkeeping as the result
                // of something they typed. `Ok(None)` is the truth -- nothing
                // the caller is waiting for was running.
                //
                // The world check `query` runs afterwards is skipped by
                // arriving here, which is acceptable on the same grounds that
                // make the check belt-and-braces in the first place: every
                // form that reaches a probe is server-generated and
                // world-neutral by contract.
                return Ok(None);
            }
            // No unit attached and the eval marker set: an eval finished.
            let r = self.finish_eval(sub, deadline)?;
            return Ok(Some(Resumed::Evaluated(r)));
        };
        let output_ref = self.record_output(&sub)?;
        let output_lines = self.outs().lines(&output_ref).unwrap_or(0);
        if sub.outcome == Outcome::Ok
            && let Some(mut failed) = p.failure.clone()
        {
            // A skip-proofs retry that finished. The form is in the world, so
            // the bookkeeping below still runs -- but what it is is `skipped`,
            // not `admitted`, and the report is the original failure's: the
            // retry proves nothing and has no diagnosis of its own. Exactly
            // what the fast path returns for the same unit, which is the point
            // -- two paths, one logical outcome, one answer.
            self.admitted.push(Admitted {
                hash: p.hash,
                at: p.at,
                label: p.label,
                command_number: sub.get("cmd").and_then(|v| v.parse().ok()),
                package: sub.get("pkg").and_then(unquote),
                dep_count: sub.get("ndeps").and_then(|v| v.parse().ok()),
                output_ref: output_ref.clone(),
                skipped: p.skipped,
            });
            if let Some(target) = p.in_package.clone() {
                self.make_package_stick(&target, deadline.max(PROBE_FLOOR))?;
            }
            let introduced = inner_names(&sub);
            self.scrape_rules(&introduced, p.index);
            // The retry's log, so a reader can see what the skip attempt
            // printed; the failure envelope above is the original's.
            failed.retry_output_ref = Some(output_ref);
            failed.still_running = false;
            return Ok(Some(Resumed::Skipped(*failed)));
        }
        if sub.outcome == Outcome::Ok {
            self.admitted.push(Admitted {
                hash: p.hash,
                at: p.at,
                label: p.label,
                command_number: sub.get("cmd").and_then(|v| v.parse().ok()),
                package: sub.get("pkg").and_then(unquote),
                dep_count: sub.get("ndeps").and_then(|v| v.parse().ok()),
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
            // Exactly as `admit_bytes` does, and for exactly its reason: this
            // is the moment the session is free, and the next thing it does
            // may be a proof. Missing here, the scrape was taken only for
            // units that finished inside their deadline -- so a book's fast
            // forms could be described and its slow ones could not, which is
            // the wrong way round. A `defines` in
            // kestrel/c/syntax/validator.lisp ran for 193 seconds and
            // introduced 620 names, and every one of them read as unknown.
            let introduced = inner_names(&sub);
            self.scrape_rules(&introduced, p.index);
            let (inner, inner_total, clique) = parse_inner(&sub);
            self.record_inductions(&output_ref, &inner);
            return Ok(Some(Resumed::Admitted(UnitReport {
                index: p.index,
                line_start: p.at.line_start,
                line_end: p.at.line_end,
                col_start: p.at.col_start,
                col_end: p.at.col_end,
                names: p.names.clone(),
                elapsed: sub.elapsed,
                event_data: parse_event_data(&sub),
                value: value_unless_a_name(parse_value(&sub), &p.names, &inner),
                inner,
                inner_total,
                clique,
                output_ref,
                output_lines,
            })));
        }
        let (ed, cp, inner, inner_total) = self.probe_after_failure(deadline.max(PROBE_FLOOR));
        let error_line = error_message(&sub.output);
        self.record_inductions(&output_ref, &inner);
        let mut report = FailureReport {
            index: p.index,
            line_start: p.at.line_start,
            line_end: p.at.line_end,
            col_start: p.at.col_start,
            col_end: p.at.col_end,
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
            let (rsub, rref) = self.admit_bytes(
                &p.text,
                p.hash,
                p.at,
                &p.names,
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
        // Whoever requested the abort is being answered right now, which is
        // the same rationale `Session::interrupt` states -- and holding the
        // book lock is the proof: a call still blocked on the session would
        // have it, so there is nobody left for the flag to wake.
        //
        // Without this, the flag survived to poison the idle path. Both
        // frontends set it before queuing the command, because a *blocked*
        // call has to be woken off its condvar rather than waited out; with
        // nothing in flight there is no such call and nothing consumes it. The
        // ping below then went into `wait_for_marker`, which sees the flag,
        // clears it and returns `Aborted` before waiting at all -- so the ping
        // could only ever report false, and control fell through into the very
        // escalation the ping exists to avoid. Measured: six seconds, four
        // SIGINTs at a healthy ACL2, and `ok: false` telling the caller the
        // interrupt failed on a working session.
        self.control.clear_abort();
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
        //
        // Stripped like every other read of a capture, finished or live. This
        // is the one log written by a path other than consumption, and filing
        // it verbatim made the interrupted form the only form whose permanent
        // record carried our own `<<<PROBE-BEGIN ...>>>` scaffolding -- worst
        // exactly when the interrupt lands during the trailing probes, which
        // is when the form was closest to finishing.
        let partial = crate::session::display_of(&self.session.captured_so_far());
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
        // Whatever was in flight is not any more, eval or probe, so the marker
        // must not survive to describe the next thing that is.
        self.in_flight_eval = false;
        if let Some(r) = self.in_flight_output.take() {
            // It describes a form that was abandoned rather than the world
            // as it now stands, which is what `superseded` is for.
            self.control.set_in_flight(None);
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
            // Certification runs against the file on disk, so an orphaned
            // session must say so here too rather than leaving cert.pl to
            // fail about a path the caller never typed.
            if !self.path.is_file() {
                return Err(SyncError::BookMissing { path: self.path.clone() });
            }
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

    /// Books whose sources have changed since the world was built on them.
    ///
    /// The invariant this server keeps is that the world equals the result of
    /// admitting the file's forms -- but the forms `include-book`, and what
    /// they include is not the file. A dependency edited and recertified
    /// under a live session leaves that session's world quietly describing
    /// nothing on disk: measured in the field as a rule added, the dependency
    /// recertified, the dependent re-synced, and the rule still absent with
    /// the reply reporting everything reused and nothing amiss.
    ///
    /// Cheap in the common case by construction: one probe, then a `stat` per
    /// dependency, and a file is only *read* when its length or write date
    /// moved. A read that hashes equal means someone touched it -- a
    /// checkout, a re-save -- and updates the recorded stat rather than
    /// crying stale.
    ///
    /// Best effort, like every probe that runs beside real work: a session
    /// that cannot answer reports nothing rather than failing the caller's
    /// sync.
    pub fn stale_includes(&mut self, deadline: Duration) -> Vec<StaleInclude> {
        let Some(names) = self.probe_deps(deadline) else { return Vec::new() };
        self.dep_names = names;
        self.scan_deps()
    }

    /// The same check against the *last known* dependency list, with no ACL2
    /// round trip at all.
    ///
    /// The list only grows when an `include-book` is admitted, so between
    /// syncs the names are already known and staleness is a question for the
    /// filesystem alone. This is what an idle poller should call: a few
    /// hundred `stat`s cost nothing and leave the session's ACL2 untouched,
    /// where probing on a timer would keep it permanently busy.
    pub fn stale_includes_cached(&mut self) -> Vec<StaleInclude> {
        self.scan_deps()
    }

    fn scan_deps(&mut self) -> Vec<StaleInclude> {
        let names = std::mem::take(&mut self.dep_names);
        let total = names.len();
        let mut stale = Vec::new();
        let mut seen: Vec<String> = Vec::with_capacity(total);
        for (pos, name) in names.iter().enumerate() {
            seen.push(name.clone());
            let Some(path) = self.resolve_dep(name) else { continue };
            let meta = std::fs::metadata(&path).ok();
            let Some(known) = self.deps.get(name).cloned() else {
                // First sight: record what it looks like now. A book is only
                // ever first seen just after the unit that included it, so
                // this baseline is the state the world was built on.
                if let Some(m) = meta {
                    let hash = hash_file(&path);
                    self.deps.insert(
                        name.clone(),
                        Dep { len: m.len(), mtime: m.modified().ok(), hash },
                    );
                }
                continue;
            };
            let Includer { unit, line, column } = self.includer_of(pos, total);
            let Some(m) = meta else {
                stale.push(StaleInclude {
                    book: name.clone(),
                    path,
                    unit,
                    line,
                    column,
                    missing: true,
                });
                continue;
            };
            if m.len() == known.len && m.modified().ok() == known.mtime {
                continue; // Unmoved: no read at all.
            }
            let hash = hash_file(&path);
            if hash == known.hash {
                // Touched, not changed. Re-baseline the cheap filter so the
                // next check is free again.
                if let Some(d) = self.deps.get_mut(name) {
                    d.len = m.len();
                    d.mtime = m.modified().ok();
                }
                continue;
            }
            stale.push(StaleInclude {
                book: name.clone(),
                path,
                unit,
                line,
                column,
                missing: false,
            });
        }
        // Books the world no longer rests on -- undone by a rollback -- stop
        // being watched, so a later re-inclusion re-baselines rather than
        // comparing against a world that no longer exists.
        self.deps.retain(|k, _| seen.contains(k));
        self.dep_names = names;
        stale
    }

    /// Ask ACL2 which books the world rests on. `None` when it could not say.
    fn probe_deps(&mut self, deadline: Duration) -> Option<Vec<String>> {
        if self.session.is_running() {
            return None;
        }
        let sub = self.session.submit(DEPS_PROBE, deadline.max(PROBE_FLOOR)).ok()?;
        let body = sub.block("deps")?;
        Some(items(&keyed_block(body, &["dep"]), "dep"))
    }

    /// Turn ACL2's name for a book into a path on disk.
    ///
    /// Two shapes, both measured: a plain string is already absolute, and
    /// `(:system . "rel.lisp")` is relative to the books directory ACL2
    /// reported at startup. Anything else -- a project's own `:dir` alias --
    /// is declined rather than guessed at, so an unresolvable name simply
    /// goes unwatched instead of raising a false alarm.
    fn resolve_dep(&self, name: &str) -> Option<PathBuf> {
        let t = name.trim();
        if let Some(abs) = unquote(t) {
            return Some(PathBuf::from(abs));
        }
        let inner = t.strip_prefix('(')?.strip_suffix(')')?;
        let (key, rel) = inner.split_once('.')?;
        if !key.trim().eq_ignore_ascii_case(":system") {
            return None;
        }
        let rel = unquote(rel.trim())?;
        Some(self.books_dir.as_ref()?.join(rel))
    }

    /// The unit that pulled in the book at `pos` of a newest-first list of
    /// length `total`. See `Admitted::dep_count`.
    /// The unit that included the dependency at `pos`, as both an index and
    /// that unit's first line. Returned together so the two can never drift.
    /// The unit an included book came in under, and where that unit starts.
    fn includer_of(&self, pos: usize, total: usize) -> Includer {
        let unit = self.unit_that_included(pos, total);
        let at = unit.and_then(|i| self.admitted.get(i)).map(|a| a.at);
        Includer { unit, line: at.map(|a| a.line_start), column: at.map(|a| a.col_start) }
    }

    fn unit_that_included(&self, pos: usize, total: usize) -> Option<usize> {
        let nth = total.checked_sub(pos)?; // it was the nth ever pushed
        if nth <= self.base_dep_count.unwrap_or(0) {
            return None; // the environment's, not any unit's
        }
        self.admitted
            .iter()
            .position(|a| a.dep_count.is_some_and(|c| c >= nth))
    }

    /// Scrape what the world knows about the names a unit just introduced.
    ///
    /// Best effort by design: this runs beside real work, and a session that
    /// cannot answer should cost the caller nothing. A failure here leaves
    /// the store without those names, which the reader reports as "not
    /// known" rather than as an error.
    ///
    /// Two passes at most. The first asks ACL2 to `untranslate` what it
    /// stores, because what it stores is not what anyone wrote: a guard of
    /// `(and (natp x) (natp y))` is held as `(if (natp x) (natp y) 'nil)` and
    /// a `let` as a lambda application. If that pass does not answer inside
    /// [`SCRAPE_BUDGET`] the session is taken back and the same names are
    /// asked for raw, because a translated term is worse to read than an
    /// untranslated one and far better than the nothing this used to
    /// report.
    ///
    /// The retry is insurance rather than an expected path. ACL2 untranslates
    /// every goal it prints, so a hang here would be a famous bug -- but
    /// `untranslate1` carries its own warning that it "may not terminate",
    /// and this is a probe with a deadline, so the honest thing is to have
    /// somewhere to fall back to.
    fn scrape_rules(&mut self, names: &[String], unit: usize) {
        // Filtered here as well as in `rules_probe`, for a different reason:
        // there it is the invariant, here it is not paying for a round trip
        // that would ask about nothing.
        let names: Vec<String> =
            names.iter().filter(|n| is_safe_symbol(n)).cloned().collect();
        if names.is_empty() {
            return;
        }
        let names = &names[..];
        if self.scrape_rules_pass(names, unit, SCRAPE_BUDGET, true) != Scrape::TimedOut {
            return;
        }
        // The probe is still running in there, and until it stops nothing
        // else can be submitted. The session's own interrupt rather than
        // `Book::interrupt`: this is not the user calling off a proof, and it
        // must not take a `certify` down with it.
        if self.session.interrupt().unwrap_or(false) {
            self.scrape_rules_pass(names, unit, SCRAPE_BUDGET, false);
        }
    }

    /// One pass of the scrape, with or without asking ACL2 to untranslate.
    fn scrape_rules_pass(
        &mut self,
        names: &[String],
        unit: usize,
        budget: Duration,
        untranslated: bool,
    ) -> Scrape {
        let probe = rules_probe(names, untranslated);
        let Ok(sub) = self.session.submit(&probe, budget) else {
            // Not a timeout: the session is in trouble, and asking it a
            // second question is not the remedy.
            return Scrape::Failed;
        };
        let Some(body) = sub.block("rules") else {
            return if sub.outcome == Outcome::StillRunning {
                Scrape::TimedOut
            } else {
                Scrape::Failed
            };
        };
        let keys = ["rname", "rrunes", "rclass", "rmeas", "rformals", "rguard",
                    "rthm", "rconst", "rmargs", "rrel", "rinfo"];
        let pairs = keyed_block(body, &keys);
        // One value per key per name, emitted in that order, so the columns
        // line up positionally -- the same discipline `items` already uses
        // for runes and warnings.
        let col = |k: &str| -> Vec<String> {
            pairs.iter().filter(|(pk, _)| pk == k).map(|(_, v)| flatten(v)).collect()
        };
        let (rn, rr, rc, rj) = (col("rname"), col("rrunes"), col("rclass"), col("rmeas"));
        let (rf, rg, rt) = (col("rformals"), col("rguard"), col("rthm"));
        let (rk, rm, ri) = (col("rconst"), col("rmargs"), col("rinfo"));
        let rl = col("rrel");
        let pick = |v: &Vec<String>, i: usize| -> Option<String> {
            v.get(i).filter(|x| !is_nil(x) && *x != ":none").cloned()
        };
        for (i, name) in rn.iter().enumerate() {
            let detail = ri.get(i).map(|x| x.as_str()).unwrap_or("");
            let mut by_rune: Vec<RuneDetail> = Vec::new();
            for entry in sexp_elements(detail) {
                let fields: Vec<(String, String)> =
                    sexp_elements(&entry).iter().filter_map(|f| sexp_field(f)).collect();
                let Some(rune) = fields.iter().find(|(k, _)| k == ":rune").map(|(_, v)| v.clone())
                else {
                    continue;
                };
                let enabled = fields
                    .iter()
                    .find(|(k, _)| k == ":enabled")
                    .map(|(_, v)| !is_nil(v));
                by_rune.push(RuneDetail {
                    rune,
                    enabled,
                    fields: fields
                        .into_iter()
                        .filter(|(k, _)| k != ":rune" && k != ":enabled")
                        .map(|(k, v)| (k, strip_symbol_acl2(v)))
                        .collect(),
                });
            }
            // Runes ACL2 records no rule info for -- `:executable-counterpart`,
            // `:induction` -- exist all the same, and a reader asking what a
            // name introduced wants to see them. `:pr` shows neither.
            for rune in sexp_elements(rr.get(i).map(|x| x.as_str()).unwrap_or("")) {
                if !by_rune.iter().any(|r| r.rune == rune) {
                    by_rune.push(RuneDetail { rune, enabled: None, fields: Vec::new() });
                }
            }
            // The body is the one thing here the user *did* write, so it is
            // the one thing not worth storing: it is in the file, and for a
            // generated function it is one world query away when the session
            // is free.
            for r in by_rune.iter_mut() {
                if r.class() == Some(":definition") {
                    r.fields.retain(|(k, _)| k != ":rhs");
                }
            }
            self.rules.insert(
                name.clone(),
                NameRules {
                    name: name.clone(),
                    unit,
                    symbol_class: pick(&rc, i),
                    measure: pick(&rj, i),
                    // `o<` is what a `defun` gets when nobody said otherwise,
                    // so reporting it says nothing; a different relation is
                    // a deliberate choice and worth a line.
                    relation: pick(&rl, i).filter(|r| r != "o<"),
                    formals: pick(&rf, i).or_else(|| pick(&rm, i)),
                    guard: pick(&rg, i),
                    statement: pick(&rt, i),
                    constant: pick(&rk, i),
                    runes: by_rune,
                },
            );
        }
        Scrape::Answered
    }

    /// What the world knows about a name this file introduced, from memory.
    ///
    /// Answers while a proof is running, which is the whole reason the
    /// scrape is eager: the session's ACL2 is busy exactly when an editor
    /// wants to describe an earlier form.
    pub fn rules_for(&self, name: &str) -> Option<&NameRules> {
        self.rules.get(name)
    }

    /// Every name this file has introduced, in no particular order.
    pub fn known_names(&self) -> impl Iterator<Item = &str> {
        self.rules.keys().map(|s| s.as_str())
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
    /// The handle the running form's log will be filed under, if it has one.
    ///
    /// A form only gets a handle once it has outrun a deadline, which is also
    /// when it becomes worth watching. Readable while it runs -- see
    /// `output` -- so a client that wants more than a pushed tail asks for it
    /// by this handle.
    pub fn in_flight_ref(&self) -> Option<OutputRef> {
        self.in_flight_output.clone()
    }

    /// Read part of a captured output -- including one still being written.
    ///
    /// A form that is still running has its log in memory, not on disk, so
    /// the control is asked first. It answers only for the handle of the form
    /// actually in flight; every other handle names a file that has stopped
    /// changing, and is read from disk as before.
    pub fn output(&self, r: &OutputRef, slice: Slice) -> io::Result<OutputSlice> {
        if let Some(live) = self.control.live_slice(r.as_str(), slice.clone()) {
            return Ok(live);
        }
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
        //
        // On failure the old session cannot be kept: `start` adopts the stream
        // before its last two failure paths, so by now the session we still
        // hold owns a reader of a superseded epoch and everything it prints is
        // being dropped. It would look alive and answer nothing, and `health`
        // would spend a full deadline to report ACL2 unresponsive for a
        // process that is fine. End it, so what we report is what is true;
        // `dirty` is still set, so the next sync starts a fresh one.
        self.session = match Session::start(&self.acl2, &dir, self.control.clone()) {
            Ok(s) => s,
            Err(e) => {
                self.session.terminate();
                // And say so, because `admitted` still describes a world that
                // now has no session behind it. Reaching here through the
                // `dirty` check leaves the flag set anyway; reaching here
                // through a direct call would not, and the book would carry a
                // dead session with nothing recording that it needs a rebuild.
                self.dirty = true;
                return Err(e.into());
            }
        };
        self.admitted.clear();
        // A new process: nothing at all is in flight in it.
        self.in_flight_eval = false;
        // The same pruning `rollback_to` does, for the same reason: units past
        // `keep` are not going to exist in the rebuilt world, so the store must
        // not keep answering `describe` for the names they introduced. Without
        // it a `rebuild(bytes, 0)` -- the dirty path -- left the whole map
        // behind, and its stale `unit` indices then aliased re-admitted units,
        // so a *later* rollback pruned by a number that no longer meant what it
        // did when it was written.
        self.rules.retain(|_, r| r.unit < keep);
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
            let (sub, output_ref) = self.admit_bytes(
                &bytes[u.start..u.end],
                u.hash,
                Span::of(u),
                &u.names,
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
                        col_start: u.col_start,
                        col_end: u.col_end,
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
        // Undone units take their names with them: the world no longer has
        // them, so neither should the store.
        self.rules.retain(|_, r| r.unit < keep);
        self.admitted.truncate(keep);
        Ok(())
    }

    /// Lines currently believed to be admitted, for reporting.
    pub fn admitted_lines(&self) -> Vec<(usize, usize)> {
        self.admitted.iter().map(|a| (a.at.line_start, a.at.line_end)).collect()
    }

    /// How many books the world currently rests on.
    ///
    /// The length of `include-book-alist-all` as of the last admitted unit,
    /// or of the session's own environment when nothing has been admitted.
    /// Recorded per unit already, for attributing a stale dependency to the
    /// form that pulled it in; exposed because it is also the cheapest honest
    /// signal that *the set of included books has changed* — which is the one
    /// event that invalidates a census.
    ///
    /// A count rather than a set: it moves whenever an `include-book` is
    /// admitted or rolled back, which is exactly the question being asked,
    /// and it costs nothing to read.
    pub fn dep_count(&self) -> Option<usize> {
        self.admitted.last().and_then(|a| a.dep_count).or(self.base_dep_count)
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

/// Ask the world everything it knows about `names`, in one submission.
///
/// **Not `:pr`.** `pr-fn` is `print-info-for-rules (info-for-rules ...)` --
/// a printer over a structure -- so this calls `info-for-rules` and keeps
/// the structure. The difference is the one this design keeps making: `:pr`
/// output is an aligned label layout whose terms wrap at the right margin,
/// and reading it would mean depending on ACL2's *wording*; the structure is
/// keyword-keyed data ACL2 cannot reword without breaking its own callers.
/// The keys are also richer than the print: an entry carries both the
/// untranslated term (for display) and the translated one, and we keep the
/// first.
///
/// The world-segment dance mirrors `pr-fn` exactly: `decode-logical-name` to
/// the event, `world-to-next-non-deeper-event` for its segment,
/// `actual-props` for the rules it laid down, filtered to this name's numes.
///
/// These are `:program`-mode system internals, in the same category as
/// `get-event-data`: pinned to an ACL2 version and confined to this module.
/// `world-to-next-non-deeper-event` dates the pin precisely -- ACL2 8.7
/// introduced it, and `:pr` itself moved to it from `world-to-next-event`,
/// to fix runes introduced by `defstobj`. Against an older ACL2 the name
/// does not translate.
///
/// And that is *quiet*, which is worth knowing before debugging it. The
/// probe is one form, so an unknown name takes all of it down rather than
/// the one field that used it, and nothing is printed at all;
/// `scrape_rules` is best-effort, so the store is simply left empty and
/// every `Book::rules_for` answers `None`. On an older image the whole
/// scrape is therefore off, with no error anywhere -- and the live tests
/// divide accordingly: the ones reading a sync's own `event_data` pass,
/// and only the ones reading `rules_for` fail. If they are failing, check
/// the ACL2 version before reading any further.
/// How a pass of the rules scrape ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scrape {
    /// The block came back; whatever it held is in the store.
    Answered,
    /// The probe outran its budget and is still running in the session.
    TimedOut,
    /// Anything else -- a dead session, a malformed answer. Not worth a
    /// second question.
    Failed,
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

/// Resolve a target position to a unit index, forgivingly.
///
/// Never fails on misalignment. Line numbers shift on every edit above, so a
/// strict boundary check would burn client turns on off-by-one errors it cannot
/// see. A position inside a unit selects that unit -- if you pointed at a
/// `defthm`, you meant that `defthm`. A position between units selects
/// everything ending at or before it.
///
/// The **last** unit the position touches, not the first, which matters only
/// when two forms share a line and the caller named no column: "through line
/// 5" then means through everything on line 5, the reading that makes the
/// target an upper bound rather than a trap. With a column the two agree
/// except at the zero-width seam of `)(`, where either answer is defensible.
fn resolve_target(units: &[Form], line: Option<usize>, column: Option<usize>) -> Option<usize> {
    if units.is_empty() {
        return None;
    }
    let Some(line) = line else {
        return Some(units.len() - 1);
    };
    if let Some(i) = units.iter().rposition(|u| u.covers(line, column)) {
        return Some(i);
    }
    units.iter().rposition(|u| u.ends_by(line, column))
}


#[cfg(test)]
mod tests {



















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

    /// The line-only call, which is what a caller with no cursor can make.
    fn at_line(u: &[Form], line: usize) -> Option<usize> {
        resolve_target(u, Some(line), None)
    }

    #[test]
    fn no_target_means_end_of_file() {
        assert_eq!(resolve_target(&units(SRC), None, None), Some(2));
        assert_eq!(resolve_target(&[], None, None), None);
        // A column without a line refines nothing, and is ignored.
        assert_eq!(resolve_target(&units(SRC), None, Some(3)), Some(2));
    }

    #[test]
    fn a_line_inside_a_unit_selects_that_unit() {
        let u = units(SRC);
        assert_eq!(at_line(&u, 3), Some(1)); // first line of beta
        assert_eq!(at_line(&u, 4), Some(1)); // last line of beta
        assert_eq!(at_line(&u, 1), Some(0));
    }

    #[test]
    fn a_line_between_units_selects_everything_before_it() {
        let u = units(SRC);
        assert_eq!(at_line(&u, 2), Some(0)); // blank line after alpha
        assert_eq!(at_line(&u, 5), Some(1)); // blank line after beta
    }

    #[test]
    fn a_line_before_any_unit_selects_nothing() {
        // A file that opens with blank lines: line 1 precedes every unit.
        let u = units("\n\n(alpha)\n");
        assert_eq!(at_line(&u, 1), None);
    }

    /// Two forms on one line, which is the whole reason a column exists: a
    /// line alone cannot say which, and the answer differs by ten characters
    /// of cursor.
    #[test]
    fn a_column_picks_between_two_forms_on_one_line() {
        //                      1         2         3
        //             1234567890123456789012345678901234
        let u = units("(defun foo (x) x) (defun bar (x) x)\n");
        assert_eq!(resolve_target(&u, Some(1), Some(1)), Some(0));
        assert_eq!(resolve_target(&u, Some(1), Some(17)), Some(0));
        assert_eq!(resolve_target(&u, Some(1), Some(18)), Some(0));
        assert_eq!(resolve_target(&u, Some(1), Some(19)), Some(1));
        assert_eq!(resolve_target(&u, Some(1), Some(35)), Some(1));
        // Past the end of the line: everything on it, which is also what the
        // line alone means.
        assert_eq!(resolve_target(&u, Some(1), Some(80)), Some(1));
        assert_eq!(at_line(&u, 1), Some(1));
    }

    /// A column is forgiven exactly as a line is, and for the same reason:
    /// it goes stale on the edit above it.
    #[test]
    fn a_column_between_units_selects_everything_before_it() {
        let u = units("(alpha) (beta)\n\n(gamma)\n");
        assert_eq!(resolve_target(&u, Some(1), Some(9)), Some(1)); // on beta
        assert_eq!(resolve_target(&u, Some(2), Some(1)), Some(1)); // the blank line
        assert_eq!(resolve_target(&u, Some(1), Some(0)), None); // before alpha
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
