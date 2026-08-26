//! The forms this crate submits to ACL2, and the parsers for what they print.
//!
//! A *probe* is a form we generate and submit alongside — or instead of — the
//! user's, whose only purpose is to make ACL2 print something we can read. It
//! is how this crate honours P2 (read state, don't scrape text): the values
//! come from `getpropc`, `get-event-data` and `gag-state` rather than from the
//! prover's prose, and they arrive under keys we chose rather than in a shape
//! ACL2 might reword.
//!
//! # Why this is its own module
//!
//! A probe is a *pair*: a string that decides what ACL2 prints, and something
//! that reads exactly that. The two have to agree precisely, and they are the
//! only things in the crate that do — so they belong beside each other rather
//! than four hundred lines apart, which is where `CHECKPOINT_PROBE` and
//! `parse_checkpoints` had ended up. The sections below are one per probe, in
//! that pairing.
//!
//! Not every probe has a parser *here*. Five do — `inner`, `evd`, `ckpt`,
//! `census`, and the value block — and for the rest the reading is a few lines
//! inside the `Book` method that asked the question, where it is bound up with
//! caching the answer or comparing it against a baseline. Those stayed put:
//! splitting `Book` to chase symmetry is exactly what this seam was chosen to
//! avoid. Each section says which case it is.
//!
//! The seam is real rather than tidy: nothing here touches [`Book`] or the
//! session. A probe is submitted by the caller and its output arrives as a
//! [`Submission`], so this module's whole interface with the rest of `sync` is
//! that type going in and a report type coming out.
//!
//! # Why getting one wrong is expensive
//!
//! A probe fails in a way `cargo` cannot see, because a probe is a string:
//!
//! - **Unbalanced parens.** ACL2 reads past the end of its own temp file and
//!   *every later admission in the session* dies with "end of file". The
//!   session is not recoverable without a rebuild.
//! - **A two-digit `fmt` argument.** `~x10` is not the eleventh argument; it
//!   is the second, followed by a literal `0`. The probe then reports a field
//!   it never printed and drops the one it did, silently.
//! - **A one-line marker whose value wraps.** `fmt` breaks at the right
//!   margin, so a `<<<PROBE-DATA k v>>>` whose value is long loses its `>>>`
//!   and the datum vanishes. Anything that can grow goes in a
//!   `PROBE-BEGIN`/`PROBE-END` block, which tolerates wrapping by
//!   construction.
//! - **An unsafe splice.** A name reaching a probe's text unchecked can close
//!   a form early or open one that never closes. [`is_safe_symbol`] is the
//!   gate, and it is applied at each splice rather than at each caller.
//!
//! All four are checked mechanically over [`every_probe`], which is the list
//! both meta-tests run on. A probe added to this module and not to that list
//! is as unchecked as it was before either test existed — the tests are at the
//! bottom of this file for that reason, so the list is beside the thing it
//! enumerates.

use super::{
    CensusEntry, Checkpoint, Checkpoints, EventData, FormValue, InnerEvent, NameKind,
};
use crate::capability::{Capabilities, Capability};
use crate::induction::Induction;
use crate::split::split;

// ---- Setting up, and asking where we are ---------------------------------------
//
// Neither of these has a parser of its own: `CLEAR_STALE` prints nothing,
// and `CMD_PROBE`'s two values are one-line `PROBE-DATA` markers that a
// caller reads straight off the `Submission` with `get`.

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
pub(super) const CLEAR_STALE: &str = "(acl2::pprogn \
    (acl2::f-put-global 'acl2::gag-state-saved acl2::nil acl2::state) \
    (acl2::f-put-global 'acl2::last-event-data acl2::nil acl2::state) \
    (acl2::f-put-global 'acl2::event-data-fal 'acl2::event-data-fal acl2::state) \
    (acl2::value :invisible))";

/// Report the command number and current package, one short marker each.
///
/// `cmd` is how every fast path is verified (P3): rollback and eval-undo
/// check the world landed where the record says. `pkg` records the one thing
/// `ubu!` cannot restore -- the reader's package is state, not world -- so
/// rollback re-establishes it explicitly. Both values are short; the one-line
/// marker protocol is safe for them (see `Submission::data`).
pub(super) const CMD_PROBE: &str = "(acl2::prog2$ \
    (acl2::cw \"~%<<<PROBE-DATA cmd ~x0>>>~%<<<PROBE-DATA pkg ~x1>>>~%\
<<<PROBE-DATA ndeps ~x2>>>~%\" \
      (acl2::max-absolute-command-number (acl2::w acl2::state)) \
      (acl2::f-get-global 'acl2::current-package acl2::state) \
      (acl2::len (acl2::global-val 'acl2::include-book-alist-all \
        (acl2::w acl2::state)))) \
    (acl2::value :invisible))";

/// Ask the world which optional features this ACL2 has.
///
/// `getpropc` with a quoted symbol and a default, which is *total*: a symbol
/// this ACL2 never defined answers `:absent` instead of failing to translate.
/// That distinction is the whole reason this probe exists rather than each
/// caller simply trying the thing it wants -- see [`crate::capability`].
///
/// Generated from [`Capability::ALL`] so a new capability is one edit, and so
/// the probe cannot fall out of step with the enum it fills in.
pub(super) fn capability_probe() -> String {
    let symbols = Capability::ALL
        .iter()
        .map(|(_, sym)| format!("acl2::{sym}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"(acl2::let ((acl2::wrld (acl2::w acl2::state)))
  (acl2::prog2$
    (acl2::cw "~%<<<PROBE-BEGIN cap>>>~%version ~x0~%" (acl2::@ acl2::acl2-version))
    (acl2::prog2$
      (acl2::cw "~*0" (acl2::list "" "cap ~x*~%" "cap ~x*~%" "cap ~x*~%"
        (acl2::loop$ acl2::for acl2::s acl2::in '({symbols}) acl2::collect
          (acl2::list acl2::s
            (acl2::if (acl2::eq :absent
                        (acl2::getpropc acl2::s 'acl2::formals :absent acl2::wrld))
                      'acl2::no 'acl2::yes)))))
      (acl2::prog2$
        (acl2::cw "<<<PROBE-END cap>>>~%")
        (acl2::value :invisible)))))"#
    )
}

pub(super) fn parse_capabilities(sub: &crate::session::Submission) -> Capabilities {
    let mut caps = Capabilities::default();
    let Some(body) = sub.block("cap") else { return caps };
    let pairs = keyed_block(body, &["version", "cap"]);
    caps.version = pairs
        .iter()
        .find(|(k, _)| k == "version")
        .and_then(|(_, v)| unquote(v.trim()));
    for entry in items(&pairs, "cap") {
        // `(symbol yes)` / `(symbol no)`. Yes/no rather than t/nil for the
        // reason the event-data probe uses them: a `nil` here would be
        // indistinguishable from the value having gone missing.
        let f = sexp_elements(&entry);
        let (Some(sym), Some(verdict)) = (f.first(), f.get(1)) else { continue };
        if symbol_name_of(verdict) != "yes" {
            continue;
        }
        let sym = symbol_name_of(sym);
        if let Some((c, _)) = Capability::ALL.iter().find(|(_, w)| *w == sym) {
            caps.insert(*c);
        }
    }
    caps
}

// ---- What the session was handed, and what it rests on -------------------------
//
// Both are read in `Book`, where what they answer is used: the inherited
// output settings decide whether proofs can be captured at all, and the
// dependency count is compared against a baseline rather than parsed into
// anything.

/// Take ownership of the settings that decide what gets *captured*, while
/// leaving the ones that decide what is *provable* exactly as inherited.
///
/// The session deliberately inherits the user's `acl2-customization.lsp`, so
/// that proofs behave here exactly as they do in their own session. But a
/// customization that puts `prove` on `inhibit-output-lst` -- a common thing
/// to want, since it is what silences the waterfall at a terminal -- also
/// silently disables output *saving* entirely (`prove.lisp`). Measured on one
/// failing `defthm`: 27 captured lines with PROVE inhibited against 46
/// without, and the missing 19 are the induction-scheme narrative and subgoal
/// progression, which exist nowhere else -- not in `get-event-data`, not in
/// `gag-state`.
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
pub(super) const OWN_OUTPUT: &str = "(acl2::prog2$ \
    (acl2::cw \"~%<<<PROBE-BEGIN env>>>~%inhibit ~x0~%sbd ~x1~%<<<PROBE-END env>>>~%\" \
      (acl2::@ acl2::inhibit-output-lst) \
      (acl2::system-books-dir acl2::state)) \
    (acl2::er-progn \
      (acl2::set-inhibit-output-lst \
        (acl2::remove-eq 'acl2::prove (acl2::@ acl2::inhibit-output-lst))) \
      (acl2::value acl2::nil)))";

/// Name every book the world was built from, newest first.
///
/// `include-book-alist-all` rather than `include-book-alist`, and the
/// difference is the whole point: the plain alist holds what is *in* the
/// world, so it omits a book included locally inside an `encapsulate` --
/// whose events the encapsulate discarded, but whose rules were what
/// *admitted* it. Measured: a book with three includes (one plain, one
/// top-level `local`, one local to an `encapsulate`) reports 14 entries in
/// the plain alist and 23 in `-all`, and only `-all` names the
/// encapsulate-local one.
///
/// Asking ACL2 also means never parsing `include-book` forms ourselves, so
/// macros that expand to one, every `:dir` spelling, and the whole
/// transitive closure are ACL2's answer rather than our guess. Names come
/// back in two shapes, both measured: an absolute string for a book found by
/// relative path, and `(:system . "under/books.lisp")` for `:dir :system`.
///
/// `true-list-fix` for the same reason `INNER_EVENTS_PROBE` needs it: an
/// unguarded `strip-cars` on something that is not an `alistp` is a guard
/// violation, and a probe's guard violation fails the *unit* it is spliced
/// into.
pub(super) const DEPS_PROBE: &str = r#"(acl2::prog2$
  (acl2::cw "~%<<<PROBE-BEGIN deps>>>~%")
  (acl2::prog2$
    (acl2::cw "~*0"
      (acl2::list "" "dep ~x*~%" "dep ~x*~%" "dep ~x*~%"
        (acl2::strip-cars
          (acl2::true-list-fix
            (acl2::global-val 'acl2::include-book-alist-all
                              (acl2::w acl2::state))))))
    (acl2::prog2$
      (acl2::cw "<<<PROBE-END deps>>>~%")
      (acl2::value :invisible))))"#;

// ---- The events inside one form, and their parser ------------------------------
//
// `iname` and its siblings print one line per element under a repeated
// prefix, which is what makes the parse independent of the width ACL2
// formatted for -- a value may wrap, but only onto lines that do not carry
// the prefix.

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
/// symbol `event-data-fal` rather than `nil`, so it is never `alistp` -- and
/// `CLEAR_STALE` blanks it to that bare symbol, so it is sometimes not a
/// cons at all. Hence `true-list-fix` before anything touches it. Entries
/// are pushed newest-first, so the failing event of a compound form is
/// always at index 0.
///
/// Per probed event, alongside the rules and abort causes: the event's kind
/// (from `event`, falling back to `form` when its head is a symbol -- the
/// `verify-guards` entry records its form as a fmt string), whether a `namex`
/// was recorded (the failing entry is the one without), and the entry's own
/// `time` and `prover-steps-counted` -- which member of a clique was slow,
/// which the aggregate outer numbers cannot say.
///
/// The probe is generated, not constant, because the detail depth is a
/// per-[`Book`] choice (`set_inner_detail`): the iteration is `loop$` over
/// the fal *value* -- no state in the body, nothing defined into the world --
/// with LIMIT spliced in as a number and applied via `take`/`min`, and the
/// per-event rune lists carried as index-tagged pairs (`(3 :rewrite foo)` is
/// event 3's `(:rewrite foo)`) so they survive one flat `~*` stream. Every
/// symbol is `acl2::`-qualified, the loop$ keywords included: the probe must
/// read identically in any current package. Guard discipline: every accessor
/// sits behind `alistp`/`true-list-fix`, because a guard violation here
/// fails the unit the probe is spliced into, not just the probe. Measured
/// end to end.
///
/// No `let` in a `loop$` body. The clique lookup wanted one, to name the
/// property list it tests and returns, and the whole probe then failed with
/// "the guard for the function" -- but only in a book whose current package
/// is not `"ACL2"`, so the ordinary tests all passed and one `defpkg` test
/// caught it. `if` and `and` in the same position are fine; the binding is
/// what the lambda object cannot take. Filter in Rust instead.
///
/// The index iteration's `to` bound is `(len ds)`, and must never be a
/// sentinel-huge literal: loop$'s `from`/`to` MATERIALIZES the index list
/// (`from-to-by-ac`), so `to 1000000000` allocated a billion conses -- some
/// thirty gigabytes -- on every admission. "Heap exhausted, game over",
/// measured the hard way.
pub(super) fn inner_events_probe(limit: usize, induction: bool) -> String {
    let (ind_body, ind_close) = induction_streams(induction);
    format!(
        r#"(acl2::let*
  ((acl2::fal0 (acl2::true-list-fix (acl2::@ acl2::event-data-fal)))
   (acl2::wrld (acl2::w acl2::state))
   (acl2::nn (acl2::min {limit} (acl2::len acl2::fal0)))
   (acl2::ds (acl2::loop$ acl2::for acl2::e acl2::in (acl2::take acl2::nn acl2::fal0)
               acl2::collect (acl2::car (acl2::cdr (acl2::true-list-fix acl2::e))))))
  (acl2::prog2$
    (acl2::cw "~%<<<PROBE-BEGIN inner>>>~%")
    (acl2::prog2$
      (acl2::cw "~*0" (acl2::list "" "iname ~x*~%" "iname ~x*~%" "iname ~x*~%"
                        (acl2::strip-cars acl2::fal0)))
      (acl2::prog2$
        (acl2::cw "~*0" (acl2::list "" "iclq ~x*~%" "iclq ~x*~%" "iclq ~x*~%"
          (acl2::loop$ acl2::for acl2::nm acl2::in (acl2::strip-cars acl2::fal0)
            acl2::append
              (acl2::true-list-fix
                (acl2::and (acl2::symbolp acl2::nm)
                           (acl2::getpropc acl2::nm 'acl2::recursivep
                                           'nil acl2::wrld))))))
      (acl2::prog2$
        (acl2::cw "~*0" (acl2::list "" "ik ~x*~%" "ik ~x*~%" "ik ~x*~%"
          (acl2::loop$ acl2::for acl2::d acl2::in acl2::ds acl2::collect
            (acl2::or (acl2::and (acl2::alistp acl2::d)
                                 (acl2::consp (acl2::cdr (acl2::assoc-equal 'acl2::event acl2::d)))
                                 (acl2::car (acl2::cdr (acl2::assoc-equal 'acl2::event acl2::d))))
                      (acl2::and (acl2::alistp acl2::d)
                                 (acl2::consp (acl2::cdr (acl2::assoc-equal 'acl2::form acl2::d)))
                                 (acl2::symbolp (acl2::car (acl2::cdr (acl2::assoc-equal 'acl2::form acl2::d))))
                                 (acl2::car (acl2::cdr (acl2::assoc-equal 'acl2::form acl2::d))))))))
        (acl2::prog2$
        (acl2::cw "~*0" (acl2::list "" "igoal ~x*~%" "igoal ~x*~%" "igoal ~x*~%"
          (acl2::loop$ acl2::for acl2::d acl2::in acl2::ds acl2::collect
            (acl2::and (acl2::alistp acl2::d)
                       (acl2::consp (acl2::cdr (acl2::assoc-equal 'acl2::event acl2::d)))
                       (acl2::eq (acl2::car (acl2::cdr (acl2::assoc-equal 'acl2::event acl2::d)))
                                 'acl2::thm)
                       (acl2::car (acl2::cdr (acl2::cdr
                         (acl2::assoc-equal 'acl2::event acl2::d))))))))
        (acl2::prog2$
          (acl2::cw "~*0" (acl2::list "" "ia ~x*~%" "ia ~x*~%" "ia ~x*~%"
            (acl2::loop$ acl2::for acl2::d acl2::in acl2::ds acl2::collect
              (acl2::and (acl2::alistp acl2::d)
                         (acl2::cdr (acl2::assoc-equal 'acl2::abort-causes acl2::d))))))
          (acl2::prog2$
            (acl2::cw "~*0" (acl2::list "" "inx ~x*~%" "inx ~x*~%" "inx ~x*~%"
              (acl2::loop$ acl2::for acl2::d acl2::in acl2::ds acl2::collect
                (acl2::if (acl2::and (acl2::alistp acl2::d)
                                     (acl2::assoc-equal 'acl2::namex acl2::d))
                          'acl2::yes 'acl2::no))))
            (acl2::prog2$
              (acl2::cw "~*0" (acl2::list "" "it ~x*~%" "it ~x*~%" "it ~x*~%"
                (acl2::loop$ acl2::for acl2::d acl2::in acl2::ds acl2::collect
                  (acl2::and (acl2::alistp acl2::d)
                             (acl2::cdr (acl2::assoc-equal 'acl2::time acl2::d))))))
              (acl2::prog2$
                (acl2::cw "~*0" (acl2::list "" "is ~x*~%" "is ~x*~%" "is ~x*~%"
                  (acl2::loop$ acl2::for acl2::d acl2::in acl2::ds acl2::collect
                    (acl2::and (acl2::alistp acl2::d)
                               (acl2::cdr (acl2::assoc-equal 'acl2::prover-steps-counted acl2::d))))))
                (acl2::prog2$
                  (acl2::cw "~*0" (acl2::list "" "irune ~x*~%" "irune ~x*~%" "irune ~x*~%"
                    (acl2::loop$ acl2::for acl2::d acl2::in acl2::ds
                                 acl2::as acl2::i acl2::from 0 acl2::to (acl2::len acl2::ds)
                      acl2::append
                        (acl2::loop$ acl2::for acl2::r acl2::in
                            (acl2::true-list-fix
                              (acl2::and (acl2::alistp acl2::d)
                                         (acl2::cdr (acl2::assoc-equal 'acl2::rules acl2::d))))
                          acl2::collect (acl2::cons acl2::i acl2::r)))))
{ind_body}                  (acl2::prog2$
                    (acl2::cw "<<<PROBE-END inner>>>~%")
                    (acl2::value :invisible))))))))))))){ind_close}"#
    )
}


/// The induction streams of `inner_events_probe`, or nothing.
///
/// Nothing, and not an empty result, when the image lacks
/// [`Capability::InductionRecords`]: `weak-induction-record-p` and the
/// `induction-record` accessor macro would both fail to *translate* there, and
/// a probe is one form -- so naming them unconditionally would not thin this
/// probe's answer, it would delete it, taking every unit's inner events with
/// it. Silently, which is the failure mode this crate has been bitten by
/// before (`rules_probe`, and see [`crate::capability`]).
///
/// Returns the block and the extra closing parens it owes, because the two
/// have to move together and a mismatch between them is a probe that makes
/// ACL2 read past the end of its own file.
fn induction_streams(induction: bool) -> (String, &'static str) {
    if !induction {
        return (String::new(), "");
    }
    (
        IND_STREAMS.replace("{scheme_ceiling}", &SCHEME_CEILING.to_string()),
        ")))",
    )
}

/// See [`induction_streams`]. Held apart from the probe body so the
/// capability-off path is plainly the same probe with a hole where these go.
const IND_STREAMS: &str = r#"                  (acl2::prog2$
                    (acl2::cw "~*0" (acl2::list "" "~@*~%" "~@*~%" "~@*~%"
                      (acl2::loop$ acl2::for acl2::d acl2::in acl2::ds
                                   acl2::as acl2::i acl2::from 0 acl2::to (acl2::len acl2::ds)
                        acl2::append
                          (acl2::loop$ acl2::for acl2::r acl2::in
                              (acl2::true-list-fix
                                (acl2::and (acl2::alistp acl2::d)
                                  (acl2::cdr (acl2::assoc-equal 'acl2::induction-records acl2::d))))
                            acl2::as acl2::j acl2::from 0 acl2::to (acl2::len
                              (acl2::true-list-fix
                                (acl2::and (acl2::alistp acl2::d)
                                  (acl2::cdr (acl2::assoc-equal 'acl2::induction-records acl2::d)))))
                            acl2::collect
                              (acl2::cons "ipool ~x0 ~x1 ~@2"
                                (acl2::list (acl2::cons #\0 acl2::i)
                                            (acl2::cons #\1 acl2::j)
                                            (acl2::cons #\2
                                              (acl2::and (acl2::weak-induction-record-p acl2::r)
                                                (acl2::access acl2::induction-record acl2::r
                                                              :pool-name)))))))))
                  (acl2::prog2$
                    (acl2::cw "~*0" (acl2::list "" "irec ~x*~%" "irec ~x*~%" "irec ~x*~%"
                      (acl2::loop$ acl2::for acl2::d acl2::in acl2::ds
                                   acl2::as acl2::i acl2::from 0 acl2::to (acl2::len acl2::ds)
                        acl2::append
                          (acl2::loop$ acl2::for acl2::r acl2::in
                              (acl2::true-list-fix
                                (acl2::and (acl2::alistp acl2::d)
                                  (acl2::cdr (acl2::assoc-equal 'acl2::induction-records acl2::d))))
                            acl2::as acl2::j acl2::from 0 acl2::to (acl2::len
                              (acl2::true-list-fix
                                (acl2::and (acl2::alistp acl2::d)
                                  (acl2::cdr (acl2::assoc-equal 'acl2::induction-records acl2::d)))))
                            acl2::collect
                              (acl2::list acl2::i acl2::j
                                (acl2::and (acl2::weak-induction-record-p acl2::r)
                                  (acl2::access acl2::induction-record acl2::r :suggested))
                                (acl2::and (acl2::weak-induction-record-p acl2::r)
                                  (acl2::access acl2::induction-record acl2::r :accommodating))
                                (acl2::and (acl2::weak-induction-record-p acl2::r)
                                  (acl2::access acl2::induction-record acl2::r :runes))
                                (acl2::and (acl2::weak-induction-record-p acl2::r)
                                  (acl2::access acl2::induction-record acl2::r :p-formula)))))))
                  (acl2::prog2$
                    (acl2::cw "~*0" (acl2::list "" "isch ~x*~%" "isch ~x*~%" "isch ~x*~%"
                      (acl2::loop$ acl2::for acl2::d acl2::in acl2::ds
                                   acl2::as acl2::i acl2::from 0 acl2::to (acl2::len acl2::ds)
                        acl2::append
                          (acl2::loop$ acl2::for acl2::r acl2::in
                              (acl2::true-list-fix
                                (acl2::and (acl2::alistp acl2::d)
                                  (acl2::cdr (acl2::assoc-equal 'acl2::induction-records acl2::d))))
                            acl2::as acl2::j acl2::from 0 acl2::to (acl2::len
                              (acl2::true-list-fix
                                (acl2::and (acl2::alistp acl2::d)
                                  (acl2::cdr (acl2::assoc-equal 'acl2::induction-records acl2::d)))))
                            acl2::collect
                              (acl2::if (acl2::< {scheme_ceiling}
                                          (acl2::acl2-count
                                            (acl2::and (acl2::weak-induction-record-p acl2::r)
                                              (acl2::access acl2::induction-record acl2::r :scheme))))
                                (acl2::list acl2::i acl2::j
                                  (acl2::len (acl2::cdr (acl2::true-list-fix
                                    (acl2::access acl2::induction-record acl2::r :scheme))))
                                  (acl2::car (acl2::cdr (acl2::true-list-fix
                                    (acl2::access acl2::induction-record acl2::r :scheme)))))
                                (acl2::list acl2::i acl2::j 0
                                  (acl2::and (acl2::weak-induction-record-p acl2::r)
                                    (acl2::access acl2::induction-record acl2::r :scheme))))))))
"#;

/// The largest `acl2-count` of an induction scheme the probe will carry whole.
///
/// Above it the probe sends the scheme's *first case* and says how many there
/// were, which is [`Induction::scheme_cases`].
///
/// Calibrated, not guessed. Measured on this ACL2: a scheme of `acl2-count` 26
/// prints as 104 characters and one of 29 prints as 118 -- four characters per
/// unit, twice, exactly. So this ceiling is about 80,000 printed characters:
/// some thirty-five times what the MCP surface will show of a scheme, and two
/// orders of magnitude below the 6,831,709-character clique that motivated
/// cutting at all. Anything a reader would have been shown whole still arrives
/// whole; only the pathological case is trimmed, and it is told.
const SCHEME_CEILING: usize = 20_000;

/// Every key `inner_events_probe` prints, in one place because both readers
/// must name *all* of them.
///
/// `keyed_block` treats a line it does not recognise as a continuation of the
/// previous value, so a reader naming a subset does not merely miss the rest
/// -- it silently glues them onto whatever key came last. `inner_names` wants
/// two of these fields and still has to name all twelve; that is the trap,
/// and this constant is what stops the two lists drifting apart.
const INNER_KEYS: [&str; 12] = [
    "iname", "iclq", "ik", "igoal", "ia", "inx", "it", "is", "irune", "ipool", "irec", "isch",
];

pub(super) fn parse_inner(sub: &crate::session::Submission) -> (Vec<InnerEvent>, usize, Vec<String>) {
    let Some(body) = sub.block("inner") else { return (Vec::new(), 0, Vec::new()) };
    let pairs =
        keyed_block(body, &INNER_KEYS);
    // Every name, not a sample of them. A `defines` clique introduces far more
    // than three events, and a list of three that looks complete is worse than
    // a long one: the reporter found two of a clique's own functions in the
    // world and absent from this list, with nothing to say it was truncated.
    // Positional, `nil` kept as an empty name rather than dropped. Two
    // reasons, and the second is a bug the first would have hidden. ACL2 keys
    // `event-data-fal` by name and a `thm` has none -- `event-data-name`
    // returns nil for it, deliberately, since a `thm` introduces nothing to
    // name -- so dropping nils dropped every `thm` outright, along with what
    // it cost and the rules it used. And the per-event fields below are
    // *positional*: an entry dropped here shifted every field after it onto
    // the wrong event.
    let names: Vec<String> =
        items_positional(&pairs, "iname").into_iter().map(Option::unwrap_or_default).collect();
    // Members of any recursion clique these events belong to, flat and
    // unordered. ACL2 records a `mutual-recursion` as *one* event, so the
    // other members are nowhere in `iname`; asking the world for
    // `recursivep` is the only way to learn they exist. Emitted only for
    // cliques of more than one, so an ordinary book carries an empty stream.
    // Only the names the events did not already carry: the probe emits a
    // `recursivep` list for every recursive function, which for the ordinary
    // singly-recursive one is just its own name back.
    let clique: Vec<String> =
        items(&pairs, "iclq").into_iter().filter(|c| !names.contains(c)).collect();
    // The per-event fields are positional: the probe's loop$ collects visit
    // the same (limited) prefix of the fal in the same order the names walk
    // it, and print one line per event, `nil` included. `event-data-fal` is
    // newest-first and a compound form stops at the event that failed, so
    // **the failing event is always at index 0** -- the default detail depth
    // never misses it. Beyond the probed prefix the fields are simply absent,
    // which is how a consumer tells "no rules" from "rules not read".
    let kinds = items_positional(&pairs, "ik");
    // Only a `thm` fills this: the probe emits `nil` for every other event.
    // A `thm` is the one event whose statement is nowhere else -- it writes no
    // name, so there is nothing in the world to look it up under, and the
    // `form` ACL2 records for it is the elided string "( THM ...)". Collected
    // for that kind alone rather than for all: the `event` of a `defines` is
    // the whole source of the clique, and this rides on a probe every form
    // already pays for.
    let goals = items_positional(&pairs, "igoal");
    let aborts = items_positional(&pairs, "ia");
    let namexes = items_positional(&pairs, "inx");
    let times = items_positional(&pairs, "it");
    let steps = items_positional(&pairs, "is");
    // Runes arrive index-tagged -- `(3 :rewrite foo)` is event 3's
    // `(:rewrite foo)` -- so the per-event lists survive one flat stream.
    // The envelope is ours (index, space, rune, closing paren), so peeling
    // it is sentinel parsing, not prose parsing.
    let mut rules: Vec<Vec<String>> = vec![Vec::new(); names.len()];
    for pair in items(&pairs, "irune") {
        let Some(bare) = pair.strip_prefix('(').and_then(|p| p.strip_suffix(')')) else {
            continue;
        };
        let Some((idx, rune)) = bare.split_once(' ') else { continue };
        let Ok(i) = idx.parse::<usize>() else { continue };
        if let Some(slot) = rules.get_mut(i) {
            slot.push(format!("({rune})"));
        }
    }
    let induction = parse_induction(&pairs, names.len());
    let mut out = Vec::new();
    for (i, name) in names.iter().enumerate() {
        out.push(InnerEvent {
            induction: induction.get(i).cloned().unwrap_or_default(),
            name: name.clone(),
            abort_causes: failure_causes(aborts.get(i).cloned().flatten()),
            rules: rules.get(i).cloned().unwrap_or_default(),
            kind: kinds.get(i).cloned().flatten().map(|v| symbol_name_of(&v)),
            goal: goals.get(i).cloned().flatten(),
            // Printed as yes/no rather than t/nil, because a nil would be
            // indistinguishable here from the value having gone missing.
            has_namex: namexes
                .get(i)
                .cloned()
                .flatten()
                .map(|v| symbol_name_of(&v) == "yes"),
            time: times.get(i).cloned().flatten(),
            prover_steps: steps.get(i).cloned().flatten().and_then(|v| v.parse().ok()),
        });
    }
    let total = out.len();
    (out, total, clique)
}

/// Join the three induction streams into per-event records.
///
/// Each stream tags its items `(event-index, record-ordinal)` rather than
/// relying on position, which the other per-event fields here can afford and
/// this cannot: an event contributes *zero or more* records, so a positional
/// stream would have no way to say which event a missing entry belonged to.
/// Most events contribute none, so most units carry three empty streams.
///
/// Three streams and not one because the pool name is a `msgp` -- ACL2's
/// format-string-plus-arguments, which `~x` would print as its own innards.
/// It travels in a stream of its own printed with `~@`, so the name that
/// arrives is the name ACL2 would have shown. The scheme travels separately
/// again because it is the one field with a size bound (`SCHEME_CEILING`).
///
/// A `BTreeMap` so the ordinals come back in ACL2's own order within an
/// event: newest first, matching `event-data-fal` itself.
fn parse_induction(pairs: &[(String, String)], events: usize) -> Vec<Vec<Induction>> {
    let mut by_key: std::collections::BTreeMap<(usize, usize), Induction> = Default::default();
    // `ipool <i> <j> <name>`; the name is a single token ACL2 chose.
    for line in items(pairs, "ipool") {
        let mut it = line.split_whitespace();
        let (Some(i), Some(j), Some(name)) = (it.next(), it.next(), it.next()) else { continue };
        let (Ok(i), Ok(j)) = (i.parse::<usize>(), j.parse::<usize>()) else { continue };
        if i < events {
            by_key.entry((i, j)).or_default().goal = name.to_string();
        }
    }
    // `irec (<i> <j> <suggested> <accommodating> <runes> <p-formula>)`.
    for tuple in items(pairs, "irec") {
        let f = sexp_elements(&tuple);
        let [Some(i), Some(j)] = [f.first(), f.get(1)].map(|x| x.and_then(|v| v.parse().ok()))
        else {
            continue;
        };
        let (i, j): (usize, usize) = (i, j);
        if i >= events {
            continue;
        }
        let rec = by_key.entry((i, j)).or_default();
        rec.suggested = f.get(2).filter(|v| !is_nil(v)).cloned().unwrap_or_default();
        rec.accommodating =
            f.get(3).filter(|v| !is_nil(v)).map(|v| sexp_elements(v)).unwrap_or_default();
        rec.runes = f.get(4).filter(|v| !is_nil(v)).map(|v| sexp_elements(v)).unwrap_or_default();
        rec.p_formula = f.get(5).filter(|v| !is_nil(v)).cloned().unwrap_or_default();
    }
    // `isch (<i> <j> <cases> <scheme>)`, where a `cases` of 0 means the
    // scheme is whole -- ACL2 cannot report a real scheme of zero cases, so
    // the sentinel costs nothing and keeps the tuple one shape.
    for tuple in items(pairs, "isch") {
        let f = sexp_elements(&tuple);
        let [Some(i), Some(j)] = [f.first(), f.get(1)].map(|x| x.and_then(|v| v.parse().ok()))
        else {
            continue;
        };
        let (i, j): (usize, usize) = (i, j);
        if i >= events {
            continue;
        }
        let rec = by_key.entry((i, j)).or_default();
        rec.scheme = f.get(3).filter(|v| !is_nil(v)).cloned().unwrap_or_default();
        rec.scheme_cases = f
            .get(2)
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| *n > 0);
    }
    let mut out: Vec<Vec<Induction>> = vec![Vec::new(); events];
    for ((i, _), rec) in by_key {
        // Guarded on insert, so the index is in range; `if let` rather than
        // an index to keep it that way if the guard ever moves.
        if let Some(slot) = out.get_mut(i) {
            slot.push(rec);
        }
    }
    out
}

/// Just the names a unit's events introduced, without the rest of the parse.
///
/// The scrape wants names and nothing else, and runs on every admitted unit;
/// `parse_inner` builds the whole report and is the caller's business.
/// Clique members come along because a `mutual-recursion` is one event whose
/// siblings appear nowhere in `iname` -- and they are exactly the names a
/// reader will ask about.
pub(super) fn inner_names(sub: &crate::session::Submission) -> Vec<String> {
    let Some(body) = sub.block("inner") else { return Vec::new() };
    // Every key the block carries, not just the two wanted: `keyed_block`
    // treats an unrecognised line as a continuation of the previous value,
    // so naming a subset silently glues the other fields onto the names.
    let pairs =
        keyed_block(body, &INNER_KEYS);
    let mut names = items(&pairs, "iname");
    for c in items(&pairs, "iclq") {
        if !names.contains(&c) {
            names.push(c);
        }
    }
    names
}

// ---- What ACL2 recorded about the event, and its parser ------------------------

/// Report what ACL2 recorded about the event just executed.
///
/// Must run *before* the `deflabel`: `get-event-data` describes the
/// immediately preceding event, and planting a label first would make it
/// describe the label.
pub(super) const EVENT_DATA_PROBE: &str = r#"(acl2::prog2$
  (acl2::cw "~%<<<PROBE-BEGIN evd>>>~%steps ~x0~%time ~x1~%"
    (acl2::get-event-data 'acl2::prover-steps-counted acl2::state)
    (acl2::get-event-data 'acl2::time acl2::state))
  (acl2::prog2$
    (acl2::cw "~*0"
      (acl2::list "" "warning ~x*~%" "warning ~x*~%" "warning ~x*~%"
        (acl2::get-event-data 'acl2::warnings acl2::state)))
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
          (acl2::cw "xdoc ~x0~%"
            (acl2::if (acl2::assoc-equal "XDOC" (acl2::known-package-alist acl2::state))
              (acl2::len
                (acl2::cdr (acl2::assoc-eq (acl2::intern$ "DOC" "XDOC")
                  (acl2::table-alist (acl2::intern$ "XDOC" "XDOC") (acl2::w acl2::state)))))
              0))
        (acl2::prog2$
          (acl2::cw "<<<PROBE-END evd>>>~%")
          (acl2::value :invisible))))))))"#;

pub(super) fn parse_event_data(sub: &crate::session::Submission) -> EventData {
    let mut e = EventData::default();
    let Some(body) = sub.block("evd") else { return e };
    // ACL2 wraps long values at its right margin, so a value runs until the
    // next known key rather than to end of line.
    let done =
        keyed_block(body, &["steps", "warning", "time", "rule", "hint", "splitter", "xdoc"]);
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
    // Warning kinds are strings, so `~x` prints them quoted; the quotes are
    // notation, not content.
    e.warnings = items(&done, "warning")
        .into_iter()
        .map(|w| unquote(&w).unwrap_or(w))
        .collect();
    e.time = pick("time");
    e.xdoc_topics = pick("xdoc").and_then(|v| v.parse().ok());
    e
}

// ---- Where the proof stopped, and its parser -----------------------------------

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
pub(super) const CHECKPOINT_PROBE: &str = r#"(acl2::let*
  ((acl2::gs (acl2::f-get-global 'acl2::gag-state-saved acl2::state))
   (acl2::ai (acl2::access acl2::gag-state acl2::gs :abort-info))
   (acl2::as (acl2::abort-info-stack acl2::ai))
   (acl2::tp (acl2::or acl2::as
                       (acl2::access acl2::gag-state acl2::gs :top-stack)))
   (acl2::sb (acl2::access acl2::gag-state acl2::gs :sub-stack))
   (acl2::wr (acl2::w acl2::state)))
  (acl2::prog2$
    (acl2::cw "~%<<<PROBE-BEGIN ckpt>>>~%gag ~x0~%ntop ~x1~%nsub ~x2~%forced ~x3~%abort ~x4~%reverted ~x5~%"
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
        (acl2::cw "tid0 ~@0~%tid1 ~@1~%tid2 ~@2~%sid0 ~@3~%sid1 ~@4~%sid2 ~@5~%<<<PROBE-END ckpt>>>~%"
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

pub(super) fn parse_checkpoints(sub: &crate::session::Submission) -> Checkpoints {
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

// ---- What an interrupted form was doing ----------------------------------------
//
// Read in `Book::interrupt`, which is the only caller and the only place
// the answer means anything.

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
pub(super) const INTERRUPT_PROBE: &str = r#"(acl2::prog2$
  (acl2::cw "~%<<<PROBE-BEGIN intr>>>~%iabort ~x0~%itime ~x1~%isteps ~x2~%"
    (acl2::get-event-data 'acl2::abort-causes acl2::state)
    (acl2::get-event-data 'acl2::time acl2::state)
    (acl2::get-event-data 'acl2::prover-steps-counted acl2::state))
  (acl2::prog2$
    (acl2::cw "ievent ~Y01~%<<<PROBE-END intr>>>~%"
      (acl2::or (acl2::get-event-data 'acl2::event acl2::state)
                (acl2::get-event-data 'acl2::form acl2::state))
      (acl2::evisc-tuple 3 8 acl2::nil acl2::nil))
    (acl2::value :invisible)))"#;

// ---- A form's value ------------------------------------------------------------
//
// The probe is `session::VALUE_PROBE`, which lives beside the submission
// machinery that always appends it; only the parser is here.

/// Read the `value` block, if the probe left one.
///
/// The block is `vkind` and then the printed value. `vkind none` is the
/// probe having applied ACL2's rule and found nothing to show; a missing
/// block is the probe never having run, which is what a form that signalled
/// an error looks like under `:ld-error-action :return!`.
pub(super) fn parse_value(sub: &crate::session::Submission) -> Option<FormValue> {
    let body = sub.block("value")?;
    let mut lines = body.lines();
    let kind = lines.next()?.strip_prefix("vkind ")?.trim().to_string();
    if kind == "none" {
        return None;
    }
    let text = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(FormValue { text, triple: kind == "triple" })
}

// ---- Every name in the world, and its parser -----------------------------------

/// Every name in the world, with its kind and the book that defines it.
///
/// **The key is property triples, not event landmarks.**
/// `access-event-tuple-namex` and friends are unbadged `:logic` functions, so
/// ACL2 rejects any `loop$` that mentions them, and inlining bottoms out in
/// more unbadged functions. The world is a list of `(symbol key . value)`
/// triples, so the name is `(car tr)` and the kind is `(cadr tr)` -- pure
/// `car`/`cdr`/`eq`, all badged. Four keys carry the names worth having.
///
/// Attribution rides the same walk: `include-book-path` global-value triples
/// are interleaved in the world exactly as `pe` relies on. **Their
/// orientation is the trap.** The world is newest-first and `global-val`
/// scans toward *older* entries, so a name is met *before* the marker that
/// governs it -- the applicable book is the *next* marker in the walk, not
/// the previous one. Reading it the intuitive way attributes every book's
/// contents to the book included before it. The interleaving is emitted as
/// it stands and reassembled in Rust, where it is testable.
///
/// `in` over the world, never `from`/`to`: the `loop$` index list
/// materialisation cost 33 GB and a machine.
pub(super) const CENSUS_PROBE: &str = r#"(acl2::prog2$
  (acl2::cw "~%<<<PROBE-BEGIN census>>>~%")
  (acl2::prog2$
    (acl2::cw "~*0"
      (acl2::list "" "~x*~%" "~x*~%" "~x*~%"
        (loop$ for acl2::tr in (acl2::w acl2::state)
               when (or (eq (cadr acl2::tr) 'acl2::formals)
                        (eq (cadr acl2::tr) 'acl2::theorem)
                        (eq (cadr acl2::tr) 'acl2::macro-args)
                        (eq (cadr acl2::tr) 'acl2::const)
                        (and (eq (car acl2::tr) 'acl2::include-book-path)
                             (eq (cadr acl2::tr) 'acl2::global-value)))
               collect (if (eq (car acl2::tr) 'acl2::include-book-path)
                           (cons :path (car (cddr acl2::tr)))
                         (cons (car acl2::tr) (cadr acl2::tr))))))
    (acl2::prog2$
      (acl2::cw "<<<PROBE-END census>>>~%")
      (acl2::value :invisible))))"#;

/// Rebuild the census from the printed interleaving.
///
/// Newest-first, so names are collected until the marker that governs them
/// arrives; see [`CENSUS_PROBE`] on why that is the way round it is. Names
/// still pending at the end belong to no book -- ACL2's own world, and this
/// file's own forms.
pub(super) fn parse_census(body: &str) -> Vec<CensusEntry> {
    let mut out = Vec::new();
    let mut pending: Vec<(String, NameKind)> = Vec::new();
    for e in census_entries(body) {
        let inner = e.trim().trim_start_matches('(').trim_end_matches(')').trim().to_string();
        let lower = inner.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix(":path") {
            // `(:path)` is nil; `(:path :system . "x.lisp")` and
            // `(:path . "/abs/x.lisp")` are the two shapes `resolve_dep`
            // already knows, restored to the form it expects.
            let raw = inner[":path".len()..].trim();
            let book = if rest.trim().is_empty() {
                None
            } else if let Some(abs) = raw.strip_prefix('.') {
                Some(abs.trim().to_string())
            } else {
                Some(format!("({raw})"))
            };
            for (name, kind) in pending.drain(..) {
                out.push(CensusEntry { name, kind, book: book.clone() });
            }
        } else if let Some((name, kind)) = inner.rsplit_once(" . ")
            && let Some(kind) = census_kind(kind)
        {
            pending.push((name.trim().to_string(), kind));
        }
    }
    out.extend(pending.into_iter().map(|(name, kind)| CensusEntry { name, kind, book: None }));
    out
}

/// Split a printed census into one string per entry.
///
/// `~*` puts each entry on its own line, so an entry starts at a line
/// beginning with `(` in column zero and anything indented continues it.
/// That rule, rather than a marker prefix, because a wrapped continuation
/// could otherwise be mistaken for a new entry whose name happened to match.
fn census_entries(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in body.lines() {
        if line.starts_with('(') {
            out.push(line.trim_end().to_string());
        } else if let Some(last) = out.last_mut() {
            last.push(' ');
            last.push_str(line.trim());
        }
    }
    out
}

fn census_kind(k: &str) -> Option<NameKind> {
    match k.trim().to_ascii_lowercase().as_str() {
        "formals" => Some(NameKind::Function),
        "macro-args" => Some(NameKind::Macro),
        "theorem" => Some(NameKind::Theorem),
        "const" => Some(NameKind::Constant),
        _ => None,
    }
}

// ---- The documentation index, and one topic from it ----------------------------
//
// Both are read where they are asked for. The index is a pair of parallel
// lists and the topic is a block of fields, and neither survives being
// separated from the question that wanted it.

/// Every documented topic's name and one-line summary.
///
/// Two parallel blocks rather than one stream of pairs, because a `~*` element
/// is substituted whole and a short is a *string* -- ACL2 rewraps it across
/// lines, so pairs on one line would need the parser to tell a wrapped short
/// from the next entry. Two blocks in table order zip without ambiguity, and
/// `keyed_block` folds each wrapped value back together on its own.
///
/// Only `:short`. `:long` is fetched per topic when someone opens it: shorts
/// are a sentence apiece and worth having resident for hover, longs are not.
///
/// **This probe names the XDOC package, so it fails to *read* in a world that
/// has none** -- which is an ordinary outcome, not a bug: a bare image simply
/// has no xdoc. The failure is contained because `query` re-probes the command
/// number after a failed submission, and the caller caches the emptiness so it
/// is attempted once.
pub(super) const DOC_INDEX_PROBE: &str = r#"(acl2::let ((acl2::tbl (xdoc::get-xdoc-table (acl2::w acl2::state))))
  (acl2::prog2$
    (acl2::cw "~%<<<PROBE-BEGIN docnames>>>~%")
    (acl2::prog2$
      (acl2::cw "~*0"
        (acl2::list "" "n ~x*~%" "n ~x*~%" "n ~x*~%"
          (loop$ for acl2::x in acl2::tbl
                 collect (acl2::cdr (acl2::assoc-eq :name acl2::x)))))
      (acl2::prog2$
        (acl2::cw "<<<PROBE-END docnames>>>~%<<<PROBE-BEGIN docshorts>>>~%")
        (acl2::prog2$
          (acl2::cw "~*0"
            (acl2::list "" "s ~x*~%" "s ~x*~%" "s ~x*~%"
              (loop$ for acl2::x in acl2::tbl
                     collect (acl2::cdr (acl2::assoc-eq :short acl2::x)))))
          (acl2::prog2$
            (acl2::cw "<<<PROBE-END docshorts>>>~%")
            (acl2::value :invisible)))))))"#;

/// One topic's full documentation, transported verbatim.
///
/// **`princ$`, not `cw`, and that is not a style choice.** Real xdoc markup
/// contains `~x0` and `~%` inside its code blocks -- `fmt` would read those as
/// its own directives and either corrupt the text or fail for want of
/// arguments. `princ$` writes the string's characters and nothing else, which
/// also preserves the line structure that `@({...})` blocks depend on and that
/// `fmt`'s right margin would rewrap away.
///
/// The name travels as a **string**, compared against `symbol-name`, so no
/// package has to exist for the probe to read and a token from a buffer can
/// never resolve to the wrong symbol. Case is normalised by the caller.
pub(super) fn doc_topic_probe(upcased: &str) -> String {
    format!(
        "(acl2::let* ((acl2::tbl (xdoc::get-xdoc-table (acl2::w acl2::state)))\n\
         (acl2::hit (acl2::car (loop$ for acl2::x in acl2::tbl\n\
         when (acl2::equal (acl2::symbol-name\n\
         (acl2::cdr (acl2::assoc-eq :name acl2::x)))\n\
         \"{upcased}\")\n\
         collect acl2::x))))\n\
         (acl2::pprogn\n\
         (acl2::princ$ \"<<<PROBE-BEGIN topic>>>\" acl2::*standard-co* acl2::state)\n\
         (acl2::princ$ \"\n\" acl2::*standard-co* acl2::state)\n\
         (acl2::princ$ (acl2::if acl2::hit\n\
         (acl2::cdr (acl2::assoc-eq :long acl2::hit)) \"\")\n\
         acl2::*standard-co* acl2::state)\n\
         (acl2::princ$ \"\n<<<PROBE-END topic>>>\n\" acl2::*standard-co* acl2::state)\n\
         (acl2::value :invisible)))"
    )
}

// ---- What one name is, and what a form introduced ------------------------------
//
// `detail_probe` answers a hover and `rules_probe` feeds the scrape; both
// are read in `Book`, where the answers are cached against the unit that
// produced them.

/// Ask the world about one name.
///
/// `evcmd` is the command number in force at the name's own event, which is
/// what separates a name this *file* introduced from one that was already
/// there when the session opened. Both report no book -- ACL2's own
/// definitions belong to no `include-book`, and neither do this file's -- so
/// without it the two are indistinguishable, and hover cheerfully told the
/// reader that `symbolp` was "defined in this file".
///
/// `decode-logical-name` is `pe`'s own path to the event, and
/// `include-book-path` at that event's world tail is how `pe` prints the book
/// chain (history-management.lisp:7106) -- event tuples themselves record no
/// source location at all. `car` of the chain is the innermost book, i.e. the
/// file whose text holds the form.
///
/// Total by construction: an unrecognised name gives `nil` from
/// `decode-logical-name` and `:none` from every `getpropc`, measured, so a
/// word under a cursor never turns into a failed submission.
pub(super) fn detail_probe(name: &str) -> String {
    format!(
        "(acl2::let* ((acl2::wrld (acl2::w acl2::state))\n\
         (acl2::nm '{name})\n\
         (acl2::ev (acl2::decode-logical-name acl2::nm acl2::wrld))\n\
         (acl2::bk (acl2::and acl2::ev\n\
         (acl2::global-val 'acl2::include-book-path acl2::ev))))\n\
         (acl2::prog2$\n\
         (acl2::cw \"~%<<<PROBE-BEGIN detail>>>~%found ~x0~%formals ~x1~%guard ~x2~%\
         margs ~x3~%thm ~x4~%const ~x5~%book ~x6~%evcmd ~x7~%<<<PROBE-END detail>>>~%\"\n\
         (acl2::if acl2::ev acl2::t acl2::nil)\n\
         (acl2::getpropc acl2::nm 'acl2::formals :none acl2::wrld)\n\
         (acl2::untranslate\n\
         (acl2::getpropc acl2::nm 'acl2::guard :none acl2::wrld) acl2::t acl2::wrld)\n\
         (acl2::getpropc acl2::nm 'acl2::macro-args :none acl2::wrld)\n\
         (acl2::untranslate\n\
         (acl2::getpropc acl2::nm 'acl2::theorem :none acl2::wrld) acl2::t acl2::wrld)\n\
         (acl2::getpropc acl2::nm 'acl2::const :none acl2::wrld)\n\
         (acl2::car acl2::bk)\n\
         (acl2::if acl2::ev (acl2::max-absolute-command-number acl2::ev) -1))\n\
         (acl2::value :invisible)))"
    )
}

pub(super) fn rules_probe(names: &[String], untranslated: bool) -> String {
    // ACL2 stores terms translated: `(and x y)` is `(if x y 'nil)` and a
    // `let` is a lambda application. `untranslate` puts back what was
    // written. `t` where the term is read as a proposition -- a guard, a
    // theorem -- since that licenses `(if x 't y)` becoming `(or x y)`,
    // which is only iff-equivalent; `nil` for a measure, whose value is the
    // point rather than its truth.
    let (meas, guard, thm) = if untranslated {
        (
            "(acl2::untranslate (acl2::if acl2::jst \
               (acl2::access acl2::justification acl2::jst :measure) :none) \
               acl2::nil acl2::wrld)",
            "(acl2::untranslate \
               (acl2::getpropc acl2::nm 'acl2::guard :none acl2::wrld) \
               acl2::t acl2::wrld)",
            "(acl2::untranslate \
               (acl2::getpropc acl2::nm 'acl2::theorem :none acl2::wrld) \
               acl2::t acl2::wrld)",
        )
    } else {
        (
            "(acl2::if acl2::jst \
               (acl2::access acl2::justification acl2::jst :measure) :none)",
            "(acl2::getpropc acl2::nm 'acl2::guard :none acl2::wrld)",
            "(acl2::getpropc acl2::nm 'acl2::theorem :none acl2::wrld)",
        )
    };
    // Every form ends `(value :invisible)`: `ld` prints each top-level form's
    // value otherwise, and a stray `NIL` landing inside the block would be
    // read as part of the structure it follows.
    let mut out = String::from(
        "(acl2::prog2$ (acl2::cw \"~%<<<PROBE-BEGIN rules>>>~%\") \
         (acl2::value :invisible))\n",
    );
    // The gate every other splice of a name into generated Lisp applies, here
    // rather than only at the caller, because this is where the splice is: a
    // future caller cannot forget what the function itself refuses.
    //
    // These names come from a probe's own output rather than from a user,
    // which is why the omission was survivable -- but they have been through
    // `items()`'s whitespace collapse by then, so a bar-quoted name containing
    // a newline is already not the name ACL2 printed, and splicing it produces
    // a probe that means something else. An unbalanced one produces a probe
    // ACL2 reads past the end of, and then every later admission in the
    // session dies with "end of file".
    //
    // Dropped rather than escaped: the scrape is best-effort, and the cost of
    // dropping is a name nobody can `describe`.
    for n in names.iter().filter(|n| is_safe_symbol(n)) {
        out.push_str(&format!(
            "(acl2::prog2$\n\
             (acl2::let* ((acl2::wrld (acl2::w acl2::state))\n\
             (acl2::nm '{n})\n\
             (acl2::rmp (acl2::getpropc acl2::nm 'acl2::runic-mapping-pairs \
                          acl2::nil acl2::wrld))\n\
             (acl2::jst (acl2::getpropc acl2::nm 'acl2::justification \
                          acl2::nil acl2::wrld))\n\
             (acl2::w2 (acl2::decode-logical-name acl2::nm acl2::wrld)))\n\
             (acl2::prog2$\n\
             (acl2::cw \"rmeas ~x0~%rrel ~x1~%\"\n\
             {meas}\n\
             (acl2::if acl2::jst \
               (acl2::access acl2::justification acl2::jst :rel) \
               :none))\n\
             (acl2::cw \"rname ~x0~%rrunes ~x1~%rclass ~x2~%\
             rformals ~x3~%rguard ~x4~%rthm ~x5~%rconst ~x6~%rmargs ~x7~%\
             rinfo ~x8~%\"\n\
             acl2::nm\n\
             (acl2::strip-cdrs acl2::rmp)\n\
             (acl2::getpropc acl2::nm 'acl2::symbol-class :none acl2::wrld)\n\
             (acl2::getpropc acl2::nm 'acl2::formals :none acl2::wrld)\n\
             {guard}\n\
             {thm}\n\
             (acl2::getpropc acl2::nm 'acl2::const :none acl2::wrld)\n\
             (acl2::getpropc acl2::nm 'acl2::macro-args :none acl2::wrld)\n\
             (acl2::if acl2::w2\n\
               (acl2::info-for-rules\n\
                 (acl2::actual-props\n\
                   (acl2::world-to-next-non-deeper-event\n\
                     (acl2::access-event-tuple-depth (acl2::cddr (acl2::car acl2::w2)))\n\
                     (acl2::cdr acl2::w2))\n\
                   acl2::nil acl2::nil)\n\
                 (acl2::strip-cars acl2::rmp)\n\
                 (acl2::ens-maybe-brr acl2::state)\n\
                 acl2::wrld)\n\
               acl2::nil))))\n\
             (acl2::value :invisible))\n"
        ));
    }
    out.push_str(
        "(acl2::prog2$ (acl2::cw \"<<<PROBE-END rules>>>~%\") \
         (acl2::value :invisible))\n",
    );
    out
}

// ---- ACL2's prose, in the one place it is read ---------------------------------
//
// The exception that proves P2. An error message is not structured
// anywhere -- `get-event-data` records causes, not sentences -- so this is
// a parse of what the prover printed, and it is deliberately the only one.

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

// ---- The text all of the above is made of --------------------------------------
//
// Shared because the probes print in one format on purpose: keyed blocks
// that tolerate wrapping, and one-line markers for values that cannot.

/// Split a `key value` block into pairs, tolerating values that ACL2 wrapped
/// across lines.
pub(super) fn keyed_block(body: &str, keys: &[&str]) -> Vec<(String, String)> {
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

/// Every value recorded under a repeated key, with ACL2's wrapping undone.
///
/// The probes print list-valued data one element per line with a distinguishing
/// prefix, rather than letting `~x` lay out the whole list. A long rune still
/// wraps, but only onto lines that do not carry the prefix, so the item
/// boundaries survive regardless of the width ACL2 formatted for -- which is
/// what makes this a list we can hand over as a list, instead of a picture of
/// one that a client would have to parse back.
pub(super) fn items(pairs: &[(String, String)], key: &str) -> Vec<String> {
    pairs
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|v| !v.is_empty() && !is_nil(v))
        .collect()
}

/// Positional per-event values under KEY: one entry per probed event, `nil`
/// kept as `None`, so positions stay aligned with the names.
fn items_positional(pairs: &[(String, String)], key: &str) -> Vec<Option<String>> {
    pairs
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.split_whitespace().collect::<Vec<_>>().join(" "))
        .map(|v| if v.is_empty() || is_nil(&v) { None } else { Some(v) })
        .collect()
}

/// ACL2 prints a string with `~x` in quotes; the value is the text inside.
pub(super) fn unquote(v: &str) -> Option<String> {
    let t = v.trim();
    t.strip_prefix('"').and_then(|t| t.strip_suffix('"')).map(str::to_string)
}

pub(super) fn is_nil(v: &str) -> bool {
    let t = v.trim();
    t.is_empty() || t.eq_ignore_ascii_case("nil")
}

/// A probe-printed symbol, case- and package-insensitively: `DEFUN`,
/// `ACL2::DEFUN` and `defun` are all the same answer. Symbols print
/// package-qualified whenever the session's current package does not see
/// them, so a parser keying on the printed text must strip the qualifier.
pub(super) fn symbol_name_of(v: &str) -> String {
    let t = v.trim();
    let t = t.rsplit("::").next().unwrap_or(t);
    t.to_ascii_lowercase()
}

/// Drop an `acl2::` prefix from a bare classification symbol.
///
/// A rule's `:subclass` (`backchain`, `abbreviation`, ...) and `:equiv`
/// (`equal`, `iff`) are bare symbols in the ACL2 package. In a book whose
/// current package is not ACL2, `:pr` prints them qualified -- `acl2::abbreviation`
/// -- and the prefix is noise a reader did not ask for, exactly as it is on an
/// abort cause or an event kind, both of which already strip it.
///
/// Only a bare symbol, and only the `acl2::` package: a term keeps ACL2's
/// printing verbatim (the standing decision on printed values), so anything
/// with whitespace or a paren is returned untouched, and a symbol in some
/// other package keeps its prefix, which there *is* meaningful.
pub(super) fn strip_symbol_acl2(v: String) -> String {
    if v.contains([' ', '\t', '\n', '(', ')']) {
        return v;
    }
    v.strip_prefix("acl2::")
        .or_else(|| v.strip_prefix("ACL2::"))
        .map(str::to_string)
        .unwrap_or(v)
}

/// The *failure* causes among an event's `abort-causes`, or `None`.
///
/// ACL2 records `revert` -- and, on a progress step, `maybe-revert` -- here
/// whenever a proof abandons a direct attempt to prove the conjecture by
/// induction instead (`add-to-tag-tree! 'abort-cause 'revert`, prove.lisp).
/// That is a routine move that *usually succeeds*, so it is not a failure and
/// must not read as one: a successful event that reverted was showing a red
/// "revert" tag, and `failed_event` -- which finds the failing event by
/// `abort_causes.is_some()` -- could fasten on a reverted-but-successful event
/// instead of the one that truly failed.
///
/// So those two are dropped. Element-wise, not all-or-nothing: a proof that
/// reverts and then fails carries the real cause alongside `revert`
/// (`(revert empty-clause)`), and the real one is kept. Every other cause --
/// `empty-clause`, `induction-depth-limit-exceeded`, `waterfall-loop`,
/// `time-limit`, the `do-not-induct` pair, `subgoal-path-length-violation`,
/// `interrupt` -- is a genuine failure and survives. When nothing survives,
/// the event did not fail here, and the `has_namex` fallback still identifies
/// a translate-level failure that named nothing.
fn failure_causes(raw: Option<String>) -> Option<String> {
    let raw = raw?;
    let inner = raw.trim();
    let inner = inner.strip_prefix('(').unwrap_or(inner);
    let inner = inner.strip_suffix(')').unwrap_or(inner);
    let kept: Vec<&str> = inner
        .split_whitespace()
        .filter(|c| {
            // Match on the bare symbol, so a package-qualified `acl2::revert`
            // (a book whose current package is not ACL2) is caught too.
            let bare = c.rsplit("::").next().unwrap_or(c);
            !bare.eq_ignore_ascii_case("revert") && !bare.eq_ignore_ascii_case("maybe-revert")
        })
        .collect();
    if kept.is_empty() {
        None
    } else {
        Some(format!("({})", kept.join(" ")))
    }
}

/// Rejoin a value ACL2 wrapped across lines.
///
/// Safe because `fmt` breaks at whitespace and never inside an atom: a long
/// book path moves to the next line whole rather than being split, and a term
/// rejoined with single spaces is the same term. Which is also why this must
/// not be used on prover *prose*, where the layout is the content.
pub(super) fn flatten(v: &str) -> String {
    v.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a token can be spliced into a probe as a symbol.
///
/// This is the guard on the one place user text reaches generated Lisp. A
/// token holding a paren would unbalance the probe, and an unbalanced probe
/// does not fail politely -- ACL2 reads past the end of its own temp file and
/// every later admission dies with "end of file" until the session is rebuilt.
/// Quotes, `;` and `|` are the same class
/// of hazard: they change how the rest of the form reads. Whitespace would
/// splice two atoms where one was meant.
///
/// The permitted set is deliberately the ACL2 symbol constituents rather than
/// an escaping scheme: a word under a cursor that is not a plain symbol is not
/// a name this can answer about anyway.
pub(super) fn is_safe_symbol(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.chars().any(|c| {
            c.is_whitespace()
                || c.is_control()
                || matches!(c, '(' | ')' | '"' | ';' | '|' | '\'' | '`' | ',' | '#' | '\\')
        })
}

/// The top-level elements of an ACL2-printed list.
///
/// The splitter does the reading, which is the point: it already handles
/// strings, `|bars|`, character literals and comments, and it is indifferent
/// to the line breaks ACL2's prettyprinter inserts at the right margin --
/// the very thing that makes the printed *layout* an unreliable thing to
/// parse. Anything that will not lex yields nothing rather than a guess.
pub(super) fn sexp_elements(text: &str) -> Vec<String> {
    let t = text.trim();
    let Some(inner) = t.strip_prefix('(').and_then(|x| x.strip_suffix(')')) else {
        return Vec::new();
    };
    match split(inner.as_bytes()) {
        Ok(forms) => forms
            .iter()
            .map(|f| flatten(&inner[f.start..f.end]))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// One `(:key value ...)` field of an `info-for-rules` entry.
///
/// ACL2 gives `(:lhs <untranslated> <translated>)`; the first value is the
/// one a reader wants, and the second is the printer's business.
pub(super) fn sexp_field(text: &str) -> Option<(String, String)> {
    let parts = sexp_elements(text);
    let key = parts.first()?.clone();
    if !key.starts_with(':') {
        return None;
    }
    Some((key, parts.get(1).cloned().unwrap_or_default()))
}
// ---- the probes, checked mechanically ----------------------------------
//
// Both checks below run over `every_probe`, and the list is here rather than
// anywhere else so that adding a probe and forgetting to enumerate it is a
// thing you do in one file with the list in front of you. `OWN_OUTPUT` was
// missing from it while the probes and the list lived four hundred lines
// apart, which is the failure this arrangement is meant to make harder.
#[cfg(test)]
mod tests {
    /// Count parens outside string literals. Generated probes are checked
    /// mechanically because the failure mode of getting this wrong is
    /// spectacular and remote: an unbalanced probe makes ACL2 read past the
    /// end of its own temp file, and *every later admission* dies with "end
    /// of file" -- and `cargo` cannot see it, because a probe is a string.
    fn paren_balance(s: &str) -> i32 {
        let (mut depth, mut in_str, mut esc) = (0i32, false, false);
        for c in s.chars() {
            match () {
                _ if esc => esc = false,
                _ if c == '\\' && in_str => esc = true,
                _ if c == '"' => in_str = !in_str,
                _ if in_str => {}
                _ if c == '(' => depth += 1,
                _ if c == ')' => depth -= 1,
                _ => {}
            }
        }
        depth
    }

    #[test]
    fn a_bare_subclass_loses_its_acl2_prefix() {
        let f = super::strip_symbol_acl2;
        assert_eq!(f("acl2::abbreviation".into()), "abbreviation");
        assert_eq!(f("ACL2::backchain".into()), "backchain");
        assert_eq!(f("backchain".into()), "backchain", "already bare");
        // A user-package equiv keeps its prefix -- it is meaningful there.
        assert_eq!(f("foo::my-equiv".into()), "foo::my-equiv");
        // A term is left exactly as ACL2 printed it, prefix and all.
        assert_eq!(f("(acl2::car x)".into()), "(acl2::car x)");
        assert_eq!(f("(acl2::true-listp acl2::x)".into()), "(acl2::true-listp acl2::x)");
    }

    #[test]
    fn revert_is_not_a_failure_cause() {
        let f = |s: &str| super::failure_causes(Some(s.to_string()));
        // A proof that only reverted to induction has no failure cause.
        assert_eq!(f("(revert)"), None);
        assert_eq!(f("(maybe-revert)"), None);
        assert_eq!(f("(REVERT)"), None, "case-insensitive");
        assert_eq!(f("(acl2::revert)"), None, "package-qualified");
        // A real cause is kept, verbatim and parenthesised as it arrived.
        assert_eq!(f("(empty-clause)").as_deref(), Some("(empty-clause)"));
        assert_eq!(
            f("(induction-depth-limit-exceeded)").as_deref(),
            Some("(induction-depth-limit-exceeded)")
        );
        // Reverted, then failed: the real cause survives, the revert does not.
        assert_eq!(f("(revert empty-clause)").as_deref(), Some("(empty-clause)"));
        // Nothing at all stays nothing.
        assert_eq!(super::failure_causes(None), None);
    }

    /// Every probe this crate submits, named once so the two meta-tests below
    /// cannot cover different subsets of them.
    ///
    /// The list is the point. Both checks fail only against a live ACL2, and
    /// only inside whatever unit the probe was spliced into -- so a probe
    /// added without being added *here* is exactly as unchecked as it was
    /// before either test existed. Constructors are exercised over arguments
    /// that have caused trouble: a package qualifier, a `*`, an arrow, a
    /// bar-quotable shape.
    fn every_probe() -> Vec<(&'static str, String)> {
        let names = ["append", "b*", "fty::deffixequiv", "foo->bar", "*const*"];
        let mut all: Vec<(&'static str, String)> = vec![
            ("CLEAR_STALE", super::CLEAR_STALE.to_string()),
            // Missing from this list while the probes and the list lived four
            // hundred lines apart -- which is the whole reason they no longer
            // do. It is a probe like any other: a `PROBE-BEGIN env` block
            // carrying the inherited inhibit list and the system books dir.
            ("OWN_OUTPUT", super::OWN_OUTPUT.to_string()),
            ("CMD_PROBE", super::CMD_PROBE.to_string()),
            ("DEPS_PROBE", super::DEPS_PROBE.to_string()),
            ("EVENT_DATA_PROBE", super::EVENT_DATA_PROBE.to_string()),
            ("CHECKPOINT_PROBE", super::CHECKPOINT_PROBE.to_string()),
            ("INTERRUPT_PROBE", super::INTERRUPT_PROBE.to_string()),
            ("CENSUS_PROBE", super::CENSUS_PROBE.to_string()),
            ("DOC_INDEX_PROBE", super::DOC_INDEX_PROBE.to_string()),
            ("VALUE_PROBE", crate::session::VALUE_PROBE.to_string()),
            ("rules_probe/untranslated", super::rules_probe(&names.map(String::from), true)),
            ("rules_probe/raw", super::rules_probe(&names.map(String::from), false)),
            ("rules_probe/empty", super::rules_probe(&[], true)),
            ("doc_topic_probe", super::doc_topic_probe("APPEND")),
            ("doc_topic_probe/qualified", super::doc_topic_probe("FTY::DEFPROD")),
        ];
        for name in names {
            all.push(("detail_probe", super::detail_probe(name)));
        }
        // The generated inner-events probe, at the bounds its callers use and
        // at the ones that shape its `loop$`: none, one, and the editor's
        // effectively unbounded default.
        //
        // Both capability shapes, because both are real probes: an image
        // without `InductionRecords` is sent the one without the induction
        // streams, and it owes three fewer closing parens. Checking only the
        // shape this developer's ACL2 happens to produce would leave the
        // other to fail on someone else's image, where an unbalanced probe
        // makes ACL2 read past the end of its own file and kills every later
        // admission.
        for limit in [0, 1, 3, 620, usize::MAX] {
            for induction in [false, true] {
                all.push(("inner_events_probe", super::inner_events_probe(limit, induction)));
            }
        }
        all.push(("capability_probe", super::capability_probe()));
        all
    }

    /// An image without the capability must not be *named* the symbols it
    /// lacks. This is the property, and it is separate from balance: a
    /// perfectly balanced probe that says `weak-induction-record-p` to ACL2
    /// 8.6 fails to translate, and a probe is one form, so it takes every
    /// inner event down with it and says nothing about why.
    #[test]
    fn the_probe_without_induction_names_nothing_it_lacks() {
        for limit in [0, 1, 3, usize::MAX] {
            let off = super::inner_events_probe(limit, false);
            for sym in ["induction-record", "induction-records", "weak-induction-record-p"] {
                assert!(!off.contains(sym), "capability-off probe names {sym}: {off}");
            }
            // And the streams really are the only difference.
            let on = super::inner_events_probe(limit, true);
            assert!(on.contains("weak-induction-record-p"), "capability-on probe lost them");
            assert!(on.len() > off.len());
        }
    }

    #[test]
    fn every_probe_is_balanced() {
        for (name, p) in every_probe() {
            assert_eq!(paren_balance(&p), 0, "unbalanced probe {name}: {p}");
        }
    }

    /// `fmt` numbers its arguments with one digit, so `~x10` is not the
    /// eleventh argument -- it is the second, followed by a literal `0`. The
    /// probe then reports a field it never printed and drops the one it did,
    /// silently and only against a live ACL2. Ten arguments is the ceiling;
    /// past it, split the `cw`.
    ///
    /// `~Y` is the exception, and it is not a two-digit *argument*: `~Yxy`
    /// takes two of them, printing argument x under the evisc-tuple in
    /// argument y. `INTERRUPT_PROBE` uses it that way. It is listed rather
    /// than pattern-matched for, so a different two-letter directive arriving
    /// later fails here and gets read before it is waved through -- which is
    /// how this one surfaced, when the check stopped looking at four probes
    /// and started looking at all of them.
    /// The check itself, on strings whose answer is known -- because a check
    /// over probes that all pass says nothing about what it would catch.
    #[test]
    fn the_fmt_check_catches_what_it_is_for() {
        let flags = |p: &str| -> bool {
            const TWO_ARG: [u8; 1] = *b"Y";
            let b = p.as_bytes();
            (0..b.len().saturating_sub(3)).any(|i| {
                let takes = if TWO_ARG.contains(&b[i + 1]) { 2 } else { 1 };
                let last = i + 1 + takes + 1;
                b[i] == b'~'
                    && b[i + 1].is_ascii_alphabetic()
                    && last < b.len()
                    && b[i + 2..=last].iter().all(|c| c.is_ascii_digit())
            })
        };
        assert!(flags("(cw \"~x10~%\" a b)"), "the mistake it exists for");
        assert!(flags("(cw \"~Y012~%\" a b)"), "and its two-argument spelling");
        assert!(!flags("(cw \"~x0~%\" a)"), "one digit is the ordinary case");
        assert!(!flags("(cw \"~Y01~%\" a b)"), "two digits are ordinary for ~Y");
        assert!(!flags("(cw \"~x0 and 12~%\" a)"), "digits in prose are prose");
    }

    #[test]
    fn no_probe_asks_fmt_for_a_two_digit_argument() {
        const TWO_ARG: [u8; 1] = *b"Y";
        for (name, p) in every_probe() {
            let b = p.as_bytes();
            for i in 0..b.len().saturating_sub(3) {
                // For a two-argument directive the mistake is one digit
                // further along: `~Y012` is arguments 0 and 1 followed by a
                // literal `2`, exactly as `~x10` is argument 1 followed by a
                // literal `0`.
                // How many digits the directive legitimately takes; one more
                // than that is the mistake.
                let takes = if TWO_ARG.contains(&b[i + 1]) { 2 } else { 1 };
                let last = i + 1 + takes + 1;
                let bad = b[i] == b'~'
                    && b[i + 1].is_ascii_alphabetic()
                    && last < b.len()
                    && b[i + 2..=last].iter().all(|c| c.is_ascii_digit());
                assert!(!bad, "two-digit fmt argument in {name} at {i}: {}",
                        &p[i..(i + 8).min(p.len())]);
            }
        }
    }

    /// Nine is the most any probe currently asks for, and `rules_probe` is the
    /// one at it. Named as a fact rather than a limit: the ceiling is ten, and
    /// a probe that reaches it has no room left, so the next field it wants
    /// means splitting the `cw` rather than adding an argument.
    #[test]
    fn the_probe_closest_to_the_fmt_ceiling_is_where_it_is_expected() {
        let highest = |p: &str| -> Option<u32> {
            let b = p.as_bytes();
            (0..b.len().saturating_sub(2))
                .filter(|&i| b[i] == b'~' && b[i + 1].is_ascii_alphabetic()
                             && b[i + 2].is_ascii_digit())
                .map(|i| (b[i + 2] - b'0') as u32)
                .max()
        };
        let worst = every_probe()
            .into_iter()
            .filter_map(|(n, p)| highest(&p).map(|h| (h, n)))
            .max();
        let (h, name) = worst.expect("some probe formats something");
        assert!(h <= 9, "{name} asks for ~x{h}, which fmt cannot number");
        assert_eq!(
            (h, name),
            (8, "rules_probe/untranslated"),
            "if this moved, the probe nearest the ceiling changed -- check it \
             still has room before adding a field"
        );
    }

    /// The one place user text reaches generated Lisp. Everything that could
    /// change how the rest of the probe reads has to be refused here.
    #[test]
    fn unsafe_tokens_never_reach_a_probe() {
        for good in ["append", "b*", "fty::deffixequiv", "foo->bar", "*c*", "<=", "1+"] {
            assert!(super::is_safe_symbol(good), "{good} should be spliceable");
        }
        for bad in ["(", ")", "foo(", "a b", "\"s\"", "a;b", "|sym|", "a'b", "", "a\nb",
                    "a#b", "a\\b", "a`b", "a,b"] {
            assert!(!super::is_safe_symbol(bad), "{bad:?} must be refused");
        }
    }

    /// And the scrape applies it, which is the splice that did not.
    ///
    /// Not a user surface -- these names come from a probe's own output -- but
    /// they have been through `items()`'s whitespace collapse by the time they
    /// arrive, so a bar-quoted name with a newline in it is already not the
    /// name ACL2 printed. Splicing that produces a probe that means something
    /// else; splicing an unbalanced one produces a probe ACL2 reads past the
    /// end of, and then every later admission in the session dies with "end of
    /// file".
    #[test]
    fn the_rules_probe_never_carries_a_name_it_cannot_splice() {
        let bad = ["a b", "a(b", "a)b", "a\"b", "a;b", "a|b", "a\\b", "", "a\nb"];
        for n in bad {
            let probe = super::rules_probe(&[n.to_string()], true);
            assert!(
                !probe.contains(&format!("'{n}")),
                "{n:?} must never be spliced"
            );
        }
        // And an ordinary one still is, or the gate would have closed the
        // feature rather than the hole.
        let probe = super::rules_probe(&["binary-append".to_string()], true);
        assert!(probe.contains("'binary-append"), "a plain name is spliceable");
    }

    /// The census orientation, which is the whole correctness question.
    ///
    /// The world is newest-first and `global-val` scans toward older entries,
    /// so a name belongs to the *next* marker in the walk, not the previous
    /// one. Reading it the other way round is not a subtle error -- it
    /// attributes every book's contents to whatever was included before it --
    /// and it looks entirely plausible, which is why it is pinned here. This
    /// is the layout ACL2 actually printed for a book including bstar.
    #[test]
    fn a_name_belongs_to_the_marker_that_follows_it() {
        let body = "(spike-double-thm . theorem)\n\
                    (spike-double . formals)\n\
                    (:path)\n\
                    (patbind-access . macro-args)\n\
                    (access-b*-bindings . formals)\n\
                    (:path :system . \"std/util/bstar.lisp\")\n\
                    (binary-append . formals)\n";
        let got = super::parse_census(body);
        let seen: Vec<(&str, Option<&str>)> =
            got.iter().map(|e| (e.name.as_str(), e.book.as_deref())).collect();
        assert_eq!(seen, vec![
            ("spike-double-thm", None),
            ("spike-double", None),
            ("patbind-access", Some("(:system . \"std/util/bstar.lisp\")")),
            ("access-b*-bindings", Some("(:system . \"std/util/bstar.lisp\")")),
            // Past the last marker: ACL2's own world, which has no book.
            ("binary-append", None),
        ]);
        assert_eq!(got[0].kind, super::NameKind::Theorem);
        assert_eq!(got[2].kind, super::NameKind::Macro);
    }

    /// A long entry wraps, and the continuation must not read as a new one.
    #[test]
    fn a_wrapped_census_entry_stays_one_entry() {
        let body = "(some-extremely-long-generated-theorem-name-that-wraps\n\
                    \x20   . theorem)\n\
                    (:path . \"/abs/path/to/book.lisp\")\n";
        let got = super::parse_census(body);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "some-extremely-long-generated-theorem-name-that-wraps");
        assert_eq!(got[0].kind, super::NameKind::Theorem);
        assert_eq!(got[0].book.as_deref(), Some("\"/abs/path/to/book.lisp\""),
                   "the absolute-path shape resolve_dep already reads");
    }

    /// ACL2 wraps at whitespace, never inside an atom, so rejoining a wrapped
    /// value with single spaces reconstructs it exactly.
    #[test]
    fn a_wrapped_value_rejoins() {
        assert_eq!(super::flatten("\n(:system . \"std/util/bstar.lisp\")"),
                   "(:system . \"std/util/bstar.lisp\")");
        assert_eq!(super::flatten("(if (symbolp recname)\n   (access-binder-listp pairs)\n 'nil)"),
                   "(if (symbolp recname) (access-binder-listp pairs) 'nil)");
    }

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
}

/// The capture parsers, offline.
///
/// `parse_inner` and `parse_checkpoints` turn what a probe printed into the
/// structure every surface then reports. They are pure functions of a
/// `Submission`, whose fields are public -- so a fixture is a struct literal,
/// and none of this needs an ACL2. Until now all of it was live-only, which
/// meant the layer between a probe's output and the wire was checked only
/// where an image was to hand.
///
/// The fixtures are shaped the way the probes actually print: values wrapped
/// at ACL2's right margin, `nil` for a slot with nothing in it, and the
/// index-tagged rune stream.
#[cfg(test)]
mod capture_tests {
    use super::*;
    use crate::session::{Outcome, Submission};
    use std::time::Duration;

    fn sub(blocks: &[(&str, &str)]) -> Submission {
        Submission {
            outcome: Outcome::Ok,
            output: String::new(),
            data: Vec::new(),
            blocks: blocks.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            elapsed: Duration::ZERO,
        }
    }

    /// The xdoc count rides on the probe every form already pays for, so it
    /// has to survive the same block parser as everything beside it -- and
    /// mean the right thing when the world has no xdoc at all.
    #[test]
    fn the_xdoc_count_is_read_from_the_event_block() {
        let e = parse_event_data(&sub(&[("evd", "steps 12\nxdoc 7184\n")]));
        assert_eq!(e.xdoc_topics, Some(7184));
        assert_eq!(e.prover_steps, Some(12), "its neighbours still read");

        // Zero is a *state*, not an absence: the probe's guard answers 0 in a
        // world with no XDOC package rather than failing, which is the whole
        // reason it can ride along on every form. A world without xdoc must
        // read as "documented nothing", not as "unknown".
        assert_eq!(parse_event_data(&sub(&[("evd", "xdoc 0\n")])).xdoc_topics, Some(0));

        // No line at all is the probe not having run, which is a different
        // thing again -- nothing is known, rather than nothing documented.
        assert_eq!(parse_event_data(&sub(&[("evd", "steps 3\n")])).xdoc_topics, None);

        // It is a running total, so a form documents something exactly when
        // the count moves. Both readings have to come from the same place or
        // the subtraction is meaningless.
        let before = parse_event_data(&sub(&[("evd", "xdoc 7184\n")])).xdoc_topics;
        let after = parse_event_data(&sub(&[("evd", "xdoc 7186\n")])).xdoc_topics;
        assert_eq!(after.zip(before).map(|(a, b)| a - b), Some(2));
    }

    // ---- parse_inner --------------------------------------------------------

    /// A compound form's events, as the `inner` probe prints them.
    ///
    /// One line per *element*, not one line per key: the probe is a `~*0`
    /// with `"iname ~x*~%"` for each of its three slots, so every item gets
    /// its own line under a repeated prefix. That is what makes the parse
    /// independent of the width ACL2 formatted for -- a value may wrap, but
    /// only onto lines that do not carry the prefix.
    ///
    /// Newest-first, and the fields are positional across events.
    const INNER: &str = "\
iname REV-REV
iname LEN-APP
iname APP-ASSOC
iclq REV-REV
iclq MY-REV
ik DEFTHM
ik DEFTHM
ik DEFTHM
ia (INDUCTION-DEPTH-LIMIT-EXCEEDED)
ia NIL
ia NIL
inx YES
inx YES
inx YES
it 2
it 1
it 1
is 997
is 316
is 435
irune (0 :INDUCTION REVAPPEND)
irune (1 :DEFINITION LEN)
irune (1 :REWRITE CDR-CONS)";

    #[test]
    fn every_event_is_named_and_its_fields_are_positional() {
        let (events, total, _) = parse_inner(&sub(&[("inner", INNER)]));
        assert_eq!(total, 3);
        assert_eq!(
            events.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["REV-REV", "LEN-APP", "APP-ASSOC"]
        );
        // Index 0 is the failing event: `event-data-fal` is newest-first and a
        // compound form stops where it failed, so the default detail depth
        // never misses it.
        assert_eq!(events[0].abort_causes.as_deref(), Some("(INDUCTION-DEPTH-LIMIT-EXCEEDED)"));
        assert_eq!(events[1].abort_causes, None, "nil is no cause");
        assert_eq!(events[0].prover_steps, Some(997));
        assert_eq!(events[2].prover_steps, Some(435));
        // Lowercased and de-qualified by `symbol_name_of`: ACL2 prints a
        // symbol package-qualified whenever the current package cannot see it,
        // so the printed text is not a stable key and this is.
        assert_eq!(events[0].kind.as_deref(), Some("defthm"));
        assert_eq!(events[0].has_namex, Some(true));
    }

    /// `revert` is bookkeeping, not a cause: ACL2 records it whenever a proof
    /// falls back to induction, which is an ordinary thing for a proof that
    /// then succeeds. A cause list that is nothing but reverts is no cause.
    #[test]
    fn revert_is_filtered_out_of_the_causes() {
        let body = "iname A\niname B\niname C\n\
             ia (REVERT)\nia (ACL2::MAYBE-REVERT)\nia (REVERT SOMETHING-REAL)";
        let (events, _, _) = parse_inner(&sub(&[("inner", body)]));
        assert_eq!(events[0].abort_causes, None);
        assert_eq!(events[1].abort_causes, None, "package-qualified too");
        // Rewrapped as a list, because that is what it is: the filter takes
        // members out, it does not change the shape.
        assert_eq!(
            events[2].abort_causes.as_deref(),
            Some("(SOMETHING-REAL)"),
            "a real cause beside a revert survives alone"
        );
    }

    /// Runes arrive index-tagged in one flat stream, because a per-event list
    /// cannot survive `cw`'s printing. The tag is ours, so peeling it is
    /// sentinel parsing rather than prose parsing.
    #[test]
    fn runes_are_returned_to_the_event_that_earned_them() {
        let (events, _, _) = parse_inner(&sub(&[("inner", INNER)]));
        assert_eq!(events[0].rules, vec!["(:INDUCTION REVAPPEND)"]);
        assert_eq!(events[1].rules, vec!["(:DEFINITION LEN)", "(:REWRITE CDR-CONS)"]);
        assert!(events[2].rules.is_empty(), "an event with no runes gets none");
    }

    /// A rune tagged for an event that does not exist is dropped rather than
    /// panicking or landing on the wrong one -- the probe's prefix is limited,
    /// so an index past the end is a shape this really sees.
    #[test]
    fn a_rune_for_no_event_is_dropped() {
        let body = "iname A\nirune (0 :REWRITE OK)\nirune (9 :REWRITE STRAY)\nirune malformed";
        let (events, _, _) = parse_inner(&sub(&[("inner", body)]));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].rules, vec!["(:REWRITE OK)"]);
    }

    /// ACL2 records a `mutual-recursion` as one event, so its other members
    /// are nowhere in `iname`; the clique list is how they surface at all.
    /// Only the ones the events did not already carry -- the probe emits a
    /// `recursivep` list for every recursive function, which for the ordinary
    /// singly-recursive one is just its own name back.
    #[test]
    fn the_clique_reports_only_the_members_no_event_named() {
        let (_, _, clique) = parse_inner(&sub(&[("inner", INNER)]));
        assert_eq!(clique, vec!["MY-REV"], "REV-REV is already an event");
    }

    /// Beyond the probed prefix a field is simply absent, which is how a
    /// consumer tells "no rules" from "rules not read".
    #[test]
    fn fields_past_the_probed_prefix_are_absent_not_zero() {
        let body = "iname A\niname B\niname C\nis 100";
        let (events, _, _) = parse_inner(&sub(&[("inner", body)]));
        assert_eq!(events[0].prover_steps, Some(100));
        assert_eq!(events[1].prover_steps, None);
        assert_eq!(events[2].prover_steps, None);
    }

    #[test]
    fn a_submission_without_an_inner_block_reports_nothing() {
        let (events, total, clique) = parse_inner(&sub(&[]));
        assert!(events.is_empty() && clique.is_empty() && total == 0);
    }

    // ---- parse_checkpoints --------------------------------------------------

    /// What the `ckpt` probe prints for a failed proof that reverted to
    /// induction: counts, flags, and up to three goals on each side. The terms
    /// are wrapped at ACL2's margin, which is why `keyed_block` runs a value
    /// to the next known key rather than to end of line.
    const CKPT: &str = "\
gag T
ntop 1
nsub 1
forced NIL
abort (INDUCTION-DEPTH-LIMIT-EXCEEDED)
reverted T
top0 (IMPLIES (TRUE-LISTP A)
              (EQUAL (REVAPPEND (REVAPPEND A NIL) NIL) A))
top1 NIL
top2 NIL
sub0 (IMPLIES (AND (CONSP A) (EQUAL X (CDR A)))
              (EQUAL Y A))
sub1 NIL
sub2 NIL
tid0 Goal'
tid1 NIL
tid2 NIL
sid0 Subgoal *1/3'''
sid1 NIL
sid2 NIL";

    #[test]
    fn a_failed_proof_reports_its_goals_on_both_sides() {
        let c = parse_checkpoints(&sub(&[("ckpt", CKPT)]));
        assert!(c.proof_ran && c.pre_revert && !c.forced);
        assert_eq!(c.n_top, 1);
        assert_eq!(c.n_sub, 1);
        assert_eq!(c.abort_cause.as_deref(), Some("(INDUCTION-DEPTH-LIMIT-EXCEEDED)"));
        assert_eq!(c.top.len(), 1);
        assert_eq!(c.sub.len(), 1);
        assert_eq!(c.top[0].goal.as_deref(), Some("Goal'"));
        assert_eq!(c.sub[0].goal.as_deref(), Some("Subgoal *1/3'''"));
    }

    /// A wrapped term keeps its layout: ACL2 indents to show structure, and
    /// the goal name it is addressed by must not.
    #[test]
    fn a_wrapped_term_keeps_its_lines_and_a_goal_name_does_not() {
        let c = parse_checkpoints(&sub(&[("ckpt", CKPT)]));
        assert!(c.top[0].term.contains('\n'), "the term is as ACL2 laid it out");
        assert!(!c.sub[0].goal.as_deref().unwrap().contains('\n'));
    }

    /// An empty slot prints `nil`, and a `nil` term is dropped -- while the
    /// counts, which are authoritative, still say how many there were.
    #[test]
    fn nil_slots_are_dropped_but_the_counts_stand() {
        let body = "gag T\nntop 5\nnsub 0\ntop0 NIL\ntop1 NIL\ntop2 NIL";
        let c = parse_checkpoints(&sub(&[("ckpt", body)]));
        assert_eq!(c.n_top, 5, "five existed");
        assert!(c.top.is_empty(), "and none of the three slots held one");
    }

    /// No block at all is a proof that did not run, not a proof with no
    /// checkpoints -- `proof_ran` is the field that says which.
    #[test]
    fn a_submission_without_a_ckpt_block_says_no_proof_ran() {
        let c = parse_checkpoints(&sub(&[]));
        assert!(!c.proof_ran);
        assert!(c.top.is_empty() && c.sub.is_empty());
        assert!(c.abort_cause.is_none());
    }

    // ---- keyed_block, which both of them stand on ---------------------------

    /// The wrapping is the whole reason this exists: a value runs to the next
    /// *known key*, not to end of line, because ACL2 breaks long values at its
    /// right margin and a line-per-value parse loses everything after the
    /// break.
    #[test]
    fn a_value_runs_to_the_next_known_key() {
        let got = keyed_block("a one\n  two\n  three\nb four", &["a", "b"]);
        assert_eq!(got[0], ("a".to_string(), "one\n  two\n  three".to_string()));
        assert_eq!(got[1], ("b".to_string(), "four".to_string()));
    }

    /// A key is only a key at the start of a line and followed by a space, or
    /// a term mentioning `top0` would end the value it is inside.
    #[test]
    fn a_key_is_only_a_key_where_a_key_can_be() {
        let got = keyed_block("a (f top0 x)\nb 1", &["a", "b", "top0"]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].1, "(f top0 x)");
    }
}