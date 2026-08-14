//! The file-prefix invariant.
//!
//! > The session world equals the result of admitting file units `1..N` in a
//! > plain interactive ACL2 session started in the book's own directory.
//!
//! The client never tells us what changed. Every sync re-reads and re-hashes
//! the file, so the mechanism is self-correcting: there is no edit-notification
//! protocol to fall out of step with. Rollback stops being something a client
//! requests and becomes a consequence of editing.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::session::{Outcome, Session};
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
}

#[derive(Debug, Clone)]
pub struct UnitReport {
    pub index: usize,
    pub line_start: usize,
    pub line_end: usize,
    pub elapsed: Duration,
    pub output: String,
}

#[derive(Debug, Clone)]
pub struct FailureReport {
    pub index: usize,
    pub line_start: usize,
    pub line_end: usize,
    pub output: String,
    /// True when the deadline expired rather than ACL2 rejecting the unit.
    pub still_running: bool,
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

#[derive(Debug)]
pub enum SyncError {
    Io(io::Error),
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

/// A session anchored to one file.
pub struct Book {
    acl2: PathBuf,
    path: PathBuf,
    session: Session,
    admitted: Vec<Admitted>,
    label_gen: u64,
    rebuilds: usize,
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
        };
        // A baseline label, so "roll back to the empty prefix" is the same
        // operation as any other rollback rather than a special case.
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
            "(acl2::deflabel {})",
            Self::label_name(0)
        );
        self.session.submit(&body, Duration::from_secs(60))?;
        Ok(())
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
        let keep = common_prefix(&self.admitted, &units);
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
            let sub = self.admit_unit(u, &bytes, deadline)?;
            match sub.outcome {
                Outcome::Ok => {
                    out.admitted.push(UnitReport {
                        index: i,
                        line_start: u.line_start,
                        line_end: u.line_end,
                        elapsed: sub.elapsed,
                        output: sub.output,
                    });
                }
                Outcome::Failed | Outcome::StillRunning => {
                    // Stop here. The world is sequential, so later units almost
                    // always depend on this one and continuing produces
                    // cascading noise rather than information.
                    out.failed = Some(FailureReport {
                        index: i,
                        line_start: u.line_start,
                        line_end: u.line_end,
                        output: sub.output,
                        still_running: sub.outcome == Outcome::StillRunning,
                    });
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Submit one unit together with its label and command-number probe.
    ///
    /// One round trip: the unit, our `deflabel`, and the command number travel
    /// in a single submission, so the bookkeeping cannot drift from the world.
    /// On success the unit is recorded in `admitted`.
    fn admit_unit(
        &mut self,
        u: &Form,
        bytes: &[u8],
        deadline: Duration,
    ) -> Result<crate::session::Submission, SyncError> {
        let text = String::from_utf8_lossy(&bytes[u.start..u.end]).into_owned();
        self.label_gen += 1;
        let label = self.label_gen;
        let body = format!(
            "{text}\n(acl2::deflabel {})\n\
             (acl2::prog2$ (acl2::cw \"~%<<<ASS-DATA cmd ~x0>>>~%\" \
               (acl2::max-absolute-command-number (acl2::w acl2::state))) \
               (acl2::value :invisible))",
            Self::label_name(label)
        );
        let sub = self.session.submit(&body, deadline)?;
        if sub.outcome == Outcome::Ok {
            self.admitted.push(Admitted {
                hash: u.hash,
                line_start: u.line_start,
                line_end: u.line_end,
                label,
                command_number: sub.get("cmd").and_then(|v| v.parse().ok()),
            });
        }
        Ok(sub)
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
            let sub = self.admit_unit(u, &bytes, deadline)?;
            if sub.outcome != Outcome::Ok {
                // A unit that admitted before but not now means the world we
                // were trying to restore was never reachable. Fail loudly.
                return Err(SyncError::ReplayFailed {
                    line_start: u.line_start,
                    output: sub.output,
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
        self.admitted.truncate(keep);
        Ok(())
    }

    /// Lines currently believed to be admitted, for reporting.
    pub fn admitted_lines(&self) -> Vec<(usize, usize)> {
        self.admitted.iter().map(|a| (a.line_start, a.line_end)).collect()
    }
}

/// Length of the longest prefix whose hashes still agree.
fn common_prefix(admitted: &[Admitted], units: &[Form]) -> usize {
    let mut n = 0;
    while n < admitted.len() && n < units.len() && admitted[n].hash == units[n].hash {
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

    fn adm(hashes: &[u64]) -> Vec<Admitted> {
        hashes
            .iter()
            .enumerate()
            .map(|(i, &h)| Admitted {
                hash: h,
                line_start: i + 1,
                line_end: i + 1,
                label: i as u64 + 1,
                command_number: Some(i as i64),
            })
            .collect()
    }

    #[test]
    fn prefix_diff() {
        let u = units(SRC);
        let all: Vec<u64> = u.iter().map(|f| f.hash).collect();

        // Unchanged file: everything is reused.
        assert_eq!(common_prefix(&adm(&all), &u), 3);
        // Nothing admitted yet.
        assert_eq!(common_prefix(&[], &u), 0);
        // Edit to the middle unit invalidates it and everything after.
        let mut changed = all.clone();
        changed[1] = 0xdead;
        assert_eq!(common_prefix(&adm(&changed), &u), 1);
        // Edit to the first unit invalidates everything.
        let mut first = all.clone();
        first[0] = 0xdead;
        assert_eq!(common_prefix(&adm(&first), &u), 0);
        // A file truncated below what was admitted.
        assert_eq!(common_prefix(&adm(&all), &u[..1]), 1);
    }

    #[test]
    fn reformatting_does_not_invalidate_the_prefix() {
        let original = units(SRC);
        let hashes: Vec<u64> = original.iter().map(|f| f.hash).collect();
        let reformatted = units("(alpha)\n\n(beta\n\n  ; note\n  gamma)\n\n(delta)\n");
        assert_eq!(
            common_prefix(&adm(&hashes), &reformatted),
            3,
            "reindenting and adding a comment must reuse the whole prefix"
        );
    }
}
