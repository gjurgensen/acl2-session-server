//! What the ACL2 in front of us can actually do.
//!
//! # Why not the version string
//!
//! ACL2 has a version, and it is the wrong instrument. `(@ acl2-version)` is
//! `"ACL2 Version 8.7"` on an image built from the commit that introduced
//! `INDUCTION-RECORDS` **and** on one built three days earlier that has never
//! heard of it: the global bumps at *release*, while the features this crate
//! depends on land in the development stream between releases. The field is
//! documented under `note-8-8` on an image that reports 8.7. Measured, both
//! ways, on the same machine.
//!
//! So a version comparison cannot gate anything this crate cares about. It is
//! still worth *recording* -- [`Capabilities::version`] goes in diagnostics,
//! where a human reading a bug report wants it -- but nothing branches on it.
//!
//! # What is used instead
//!
//! One witness symbol per capability, looked up in the logical world with
//! `getpropc` and a default. That is total: asking about a symbol ACL2 has
//! never heard of yields the default, and costs nothing.
//!
//! Being total is the entire point, because the alternative is not "the probe
//! returns less". A symbol *named in a form* that ACL2 does not have is a
//! translate error, and a probe is one form, so it takes the whole probe down
//! -- silently, since a failed probe is indistinguishable from a probe with
//! nothing to say. That is not hypothetical: `rules_probe` names
//! `world-to-next-non-deeper-event`, and on ACL2 8.6 every form's rule detail
//! goes missing for real, with the test suite the only thing that notices
//! (`tests/live.rs`). [`Capability::RuleDetail`] is that floor, written down.
//!
//! A capability therefore gates the *construction* of a probe, not the reading
//! of its results: see `inner_events_probe`, which emits its induction streams
//! only when [`Capability::InductionRecords`] is present.

use std::collections::BTreeSet;

/// A thing some ACL2s can do and others cannot.
///
/// Each is a *witness symbol* and the release that introduced it. Adding one
/// means adding it to [`Capability::ALL`] and nowhere else; the probe is
/// generated from that list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    /// `get-event-data` reports `INDUCTION-RECORDS`: the term that suggested
    /// each induction, what it accommodated, the `:induction` runes behind it,
    /// and the scheme.
    ///
    /// ACL2 commit `31b8c8aff5`, 2026-08-25, "Induction information is now
    /// returned by get-event-data" -- after the 8.7 release, documented in
    /// `note-8-8`. Witnessed by `weak-induction-record-p`, the recognizer
    /// `(defrec induction-record ...)` generates, which that commit added.
    ///
    /// Without it a unit's [`crate::sync::InnerEvent::induction`] is empty on
    /// every event, exactly as it is for an event that did not induct. The two
    /// are told apart here and nowhere else.
    InductionRecords,
    /// `:pr`-style rule detail is readable, which `Book::rules_for` needs.
    ///
    /// ACL2 8.7 introduced `world-to-next-non-deeper-event` and moved `:pr`
    /// onto it, to fix the runes a `defstobj` introduces. This is the crate's
    /// oldest floor and predates this module -- it is recorded here so that an
    /// image below it is *diagnosable* rather than merely disappointing.
    RuleDetail,
}

impl Capability {
    /// Every capability, with the symbol that witnesses it.
    ///
    /// The witness must be something the feature's own commit introduced, and
    /// something no *earlier* ACL2 defined under that name -- otherwise the
    /// probe reports a capability the image does not have, which is worse than
    /// reporting none.
    pub const ALL: [(Capability, &'static str); 2] = [
        (Capability::InductionRecords, "weak-induction-record-p"),
        (Capability::RuleDetail, "world-to-next-non-deeper-event"),
    ];

    /// The symbol whose presence in the world witnesses this capability.
    pub fn witness(self) -> &'static str {
        Self::ALL.iter().find(|(c, _)| *c == self).map(|(_, s)| *s).unwrap_or("")
    }
}

/// What one session's image can do, read once when the session starts.
///
/// Read at start rather than per use because the world only ever *grows*
/// during a session -- an `include-book` cannot retract a system function --
/// so a capability present at startup stays present, and one absent at startup
/// cannot appear.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    have: BTreeSet<Capability>,
    /// ACL2's own version string, verbatim: `"ACL2 Version 8.7"`.
    ///
    /// For humans reading diagnostics. Nothing branches on it -- see the
    /// module docs for why it cannot.
    pub version: Option<String>,
}

impl Capabilities {
    pub fn has(&self, c: Capability) -> bool {
        self.have.contains(&c)
    }

    pub fn insert(&mut self, c: Capability) {
        self.have.insert(c);
    }

    /// What is present, for a diagnostic line.
    pub fn present(&self) -> impl Iterator<Item = Capability> + '_ {
        self.have.iter().copied()
    }

    /// What is missing, which is the half a reader chasing an empty result
    /// actually needs.
    pub fn missing(&self) -> impl Iterator<Item = Capability> + '_ {
        Capability::ALL.iter().map(|(c, _)| *c).filter(move |c| !self.have.contains(c))
    }
}
