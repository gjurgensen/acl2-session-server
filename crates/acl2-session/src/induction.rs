//! What ACL2 decided about an induction, read from its own records.
//!
//! The module name is deliberate. A module of this name held a *parser* for
//! the induction narrative until `fb7bf7f`, and everything here comes instead
//! from `get-event-data`'s `INDUCTION-RECORDS` -- structure ACL2 files at the
//! point it picks a scheme rather than prose it prints and forgets.
//!
//! The read itself lives in `sync::probe`, beside the fal it reads from.
//! What is here is the shape of the answer and the per-unit store that keeps
//! it alive after a sync returns, which is what lets a caller ask about an
//! induction long after the form that performed it went in.

/// One induction ACL2 performed, from `get-event-data`'s `INDUCTION-RECORDS`.
///
/// ACL2 gained this in commit `31b8c8aff5` (2026-08-25, "Induction information
/// is now returned by get-event-data"), which builds an `induction-record` at
/// the point the winning candidate is chosen and files it in the event's ttree
/// -- the same values `induct-msg/continue` prints, kept instead of dropped.
/// `books/tools/show-induction-records.lisp` is ACL2's own reader for it, and
/// [`crate::capability::Capability::InductionRecords`] is how a session learns
/// whether the image in front of it has any of this.
///
/// This crate carried a *parser* for the printed narrative until `fb7bf7f`.
/// What killed it was not fragility but attribution: an announcement never
/// names the event that printed it, and a form is often many events each
/// numbering its goals from `*1`. The records come out of `event-data-fal`
/// keyed by event name, so that defect is gone rather than mitigated --
/// measured on a `progn` of three theorems, each carrying its own records,
/// and on one theorem carrying two (`*1` and its sub-induction `*1.1`, which
/// ACL2 does not print at all under default settings).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Induction {
    /// The goal this induction was for -- `*1`, `*1.1`, `[2]*1.3`.
    ///
    /// ACL2 holds this as a `msgp`, not a string: a format string plus its
    /// arguments, carrying the pool list as numbers and the forcing round
    /// separately. The probe has ACL2 print it with `~@` rather than
    /// reassembling the name here, so the spelling stays ACL2's (P2) --
    /// including the `[round]` prefix, which only appears off round zero.
    pub goal: String,
    /// The term that suggested the scheme, untranslated: `(revappend x z)`.
    pub suggested: String,
    /// The other terms merged into the scheme alongside `suggested`.
    /// Untranslated, and usually empty.
    pub accommodating: Vec<String>,
    /// The `:induction` runes that produced the suggestion.
    ///
    /// A list, not a phrase. The old parser could offer only ACL2's English
    /// ("the :induction rules BINARY-APPEND and REVAPPEND") because that is
    /// what `tilde-*-simp-phrase` renders; this is the rune list itself.
    ///
    /// Excludes splitter and forcing-round information, which ACL2's record
    /// deliberately leaves out.
    pub runes: Vec<String>,
    /// `(:p x y z)` -- the conjecture's variables, in ACL2's term order.
    /// This is what `:p` stands for inside [`Self::scheme`].
    pub p_formula: String,
    /// The scheme itself, with `:p` for the conjecture.
    ///
    /// Empty only when the probe could not read the record at all. When
    /// [`Self::scheme_cases`] is `Some`, this is the *first case* rather than
    /// the whole scheme -- see that field for why.
    pub scheme: String,
    /// `Some(n)` when `scheme` holds one case out of `n`, rather than all of
    /// them; `None` when it is whole.
    ///
    /// A mutual recursion inducts over its entire clique, and one such scheme
    /// measured 6,831,709 characters. That is a *transport* problem before it
    /// is a display one -- it crosses a pty and is parsed on every admission
    /// -- so the probe bounds it, and the surfaces cut again to their own
    /// budgets on top.
    ///
    /// The bound is structural rather than by character count, and that was
    /// forced rather than chosen: `fmt1-to-string` has no badge, so it cannot
    /// be called from the `loop$` the probe is built on, and a character-exact
    /// prefix is therefore not available in ACL2. Keeping whole cases is the
    /// better failure anyway -- what arrives is a readable term rather than a
    /// term cut off mid-symbol.
    pub scheme_cases: Option<usize>,
}

impl Induction {
    /// Whether [`Self::scheme`] is the whole scheme rather than one case.
    pub fn scheme_complete(&self) -> bool {
        self.scheme_cases.is_none()
    }
}


/// One event's inductions, under the name ACL2 filed them by.
///
/// Empty `event` is the nameless case, and is one specific thing: a `thm`.
/// It proves a goal and introduces nothing, so `event-data-name` gives it no
/// name -- which is a fact to show, not a reason to drop what it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventInductions {
    pub event: String,
    pub records: Vec<Induction>,
}

/// Every induction a unit performed, kept past the sync that observed it.
///
/// Kept, rather than fetched when asked, because it cannot be fetched when
/// asked: the records live in `event-data-fal`, and `CLEAR_STALE` blanks that
/// at the start of every unit and every eval. By the time a caller asks, the
/// session has moved on. So the capture is eager and only the *serving* is on
/// demand -- which costs nothing for the units that never induct, and those
/// are nearly all of them.
///
/// Keyed by the same `OutputRef` a unit's log is keyed by, so a client that
/// has a handle for one has a handle for the other, and lives beside that log
/// for the same reason: reading it must not queue behind whatever call
/// currently holds the book.
#[derive(Debug, Clone, Default)]
pub struct InductionStore {
    by_ref: std::collections::HashMap<String, Vec<EventInductions>>,
}

impl InductionStore {
    /// Record a unit's inductions, if it had any.
    ///
    /// Nothing is stored for a unit that did not induct, so an absent entry
    /// and an empty one mean the same thing here and a reader need not tell
    /// them apart. That is only true because *every* admitted unit is offered
    /// to this function -- see the call in `sync`.
    pub fn put(&mut self, r: &str, events: Vec<EventInductions>) {
        let events: Vec<_> = events.into_iter().filter(|e| !e.records.is_empty()).collect();
        if !events.is_empty() {
            self.by_ref.insert(r.to_string(), events);
        }
    }

    /// What a unit inducted, newest event first.
    pub fn get(&self, r: &str) -> &[EventInductions] {
        self.by_ref.get(r).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Drop a unit's records, for a handle whose log is being discarded.
    pub fn forget(&mut self, r: &str) {
        self.by_ref.remove(r);
    }
}
