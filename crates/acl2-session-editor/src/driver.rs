//! The stepping driver: one thread per book, owning its shared handle,
//! turning snapshot updates into the event stream an editor paints from.
//!
//! This is the stepping loop, verbatim: split the snapshot, sync one unit
//! at a time (`through_line` = the next unit's last line), follow an
//! in-flight proof with short resumes, and let each outcome's counts advance
//! the frontier. Commands are polled between steps and between slices, so an
//! edit lands within a slice of arriving -- and only an edit that touches the
//! prefix through the running unit aborts its proof.

use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;

use acl2_session::registry::SharedBook;
use acl2_session::{split, Form, Resumed, SyncError};

use crate::wire;
use crate::Out;

/// One instruction from the protocol loop.
pub enum Cmd {
    Update { bytes: Vec<u8>, target_line: Option<usize>, revision: u64 },
    Retract { keep: usize },
    Pause,
    Close,
}

/// Per-step sync deadline. Short on purpose: an expiry is not a failure, it
/// just switches the driver to following the proof with `resume`.
const STEP: Duration = Duration::from_secs(2);
/// Resume slice while a proof runs; also the `tail` event cadence.
const SLICE: Duration = Duration::from_millis(500);
/// Rollbacks are sub-second; the rebuild fallback is the long tail this
/// deadline is really for.
const RETRACT_DEADLINE: Duration = Duration::from_secs(600);
const TAIL_LINES: usize = 40;
/// How often an otherwise idle driver looks at the books its world rests on.
///
/// Filesystem only -- the dependency *list* changes only when an
/// `include-book` is admitted, so between syncs this is a few hundred
/// `stat`s and no ACL2 round trip at all. Polling rather than inotify keeps
/// a rustix-only core free of a watcher dependency, and at this cadence the
/// cost is unmeasurable.
const DEP_TICK: Duration = Duration::from_secs(2);

enum Flow {
    Continue,
    Exit,
}

pub struct Driver {
    id: String,
    book: SharedBook,
    rx: Receiver<Cmd>,
    out: Out,
    bytes: Vec<u8>,
    units: Vec<Form>,
    target_line: Option<usize>,
    revision: u64,
    /// Set by an interrupt, a terminal failure, or a split error; cleared by
    /// the next update. A parked driver reconciles nothing and proves
    /// nothing -- without this, a terminal failure would re-prove itself in a
    /// tight loop forever.
    parked: bool,
    have_snapshot: bool,
    /// Unit of the last terminal failure, painted `failed` until an update.
    failed_unit: Option<usize>,
    /// Books reported stale on the previous tick. A change is acted on only
    /// once it has been seen twice, which keeps a dependency being saved
    /// repeatedly in another tab from restarting this session on every save.
    stale_seen: Vec<String>,
    /// Whether the current snapshot's diff has run. The frontier can only
    /// be read honestly after it: the world may still hold units the new
    /// text rolled out from under.
    reconciled: bool,
}

impl Driver {
    pub fn new(id: String, book: SharedBook, rx: Receiver<Cmd>, out: Out) -> Driver {
        Driver {
            id,
            book,
            rx,
            out,
            bytes: Vec::new(),
            units: Vec::new(),
            target_line: None,
            revision: 0,
            parked: true,
            stale_seen: Vec::new(),
            have_snapshot: false,
            failed_unit: None,
            reconciled: false,
        }
    }

    pub fn run(mut self) {
        loop {
            let cmd = match self.rx.recv_timeout(DEP_TICK) {
                Ok(c) => c,
                // Nothing to do: look at the books the world rests on. The
                // frontier is *visible* here, so a green gutter resting on a
                // book that has since changed is a false claim the user is
                // looking at -- and unlike a tool reply, it stays on screen.
                Err(RecvTimeoutError::Timeout) => {
                    if let Flow::Exit = self.tick() {
                        return;
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            };
            if let Flow::Exit = self.apply(cmd) {
                return;
            }
            if let Flow::Exit = self.drain() {
                return;
            }
            if let Flow::Exit = self.drive() {
                return;
            }
        }
    }

    /// An idle look at the dependencies.
    ///
    /// Reports on the first sighting and acts on the second: a dependency
    /// mid-save is a moving target, and re-driving on every save would make
    /// a session on a dependent book thrash. Acting means simply re-driving
    /// the snapshot we already hold -- `sync` folds staleness into the diff,
    /// so the frontier retracts to the including form and re-admits it, with
    /// the client seeing the ordinary painting rather than a special case.
    fn tick(&mut self) -> Flow {
        if !self.have_snapshot {
            return Flow::Continue;
        }
        let stale = {
            let Ok(mut book) = self.book.try_lock() else { return Flow::Continue };
            book.stale_includes_cached()
        };
        if stale.is_empty() {
            self.stale_seen.clear();
            return Flow::Continue;
        }
        self.out.notify(
            "stale_deps",
            &wire::stale_deps(&self.id, self.revision, &stale),
        );
        let names: Vec<String> = stale.iter().map(|s| s.book.clone()).collect();
        if names != self.stale_seen {
            self.stale_seen = names;
            return Flow::Continue; // seen once; act when it settles
        }
        // A dependency change is news the same way an edit is, so it also
        // unparks: the terminal failure this driver parked on may be exactly
        // what the neighbour's edit fixes.
        self.stale_seen.clear();
        self.parked = false;
        self.reconciled = false;
        self.drive()
    }

    /// Apply everything already queued, without blocking.
    fn drain(&mut self) -> Flow {
        loop {
            match self.rx.try_recv() {
                Ok(c) => {
                    if let Flow::Exit = self.apply(c) {
                        return Flow::Exit;
                    }
                }
                Err(TryRecvError::Empty) => return Flow::Continue,
                Err(TryRecvError::Disconnected) => return Flow::Exit,
            }
        }
    }

    fn apply(&mut self, cmd: Cmd) -> Flow {
        match cmd {
            Cmd::Update { bytes, target_line, revision } => {
                self.revision = revision;
                self.target_line = target_line;
                self.parked = false;
                self.failed_unit = None;
                self.reconciled = false;
                match split(&bytes) {
                    Ok(units) => {
                        self.units = units;
                        self.bytes = bytes;
                        self.have_snapshot = true;
                    }
                    Err(e) => {
                        // The old snapshot no longer describes the buffer and
                        // the new one does not lex: hold still and say why.
                        self.have_snapshot = false;
                        self.parked = true;
                        self.settle("split_error", Some(e.to_string()));
                    }
                }
                Flow::Continue
            }
            Cmd::Retract { keep } => {
                if !self.have_snapshot {
                    return Flow::Continue;
                }
                self.failed_unit = None;
                self.parked = false;
                self.reconciled = false;
                // Hold the target where the retraction puts the world, so the
                // drive that follows does not immediately re-admit.
                self.target_line = Some(match keep.checked_sub(1).and_then(|k| self.units.get(k)) {
                    Some(u) => u.line_end,
                    None => 0,
                });
                let r = {
                    let mut book = self.book.lock().expect("book lock");
                    book.retract_to(&self.bytes, keep, RETRACT_DEADLINE)
                };
                if let Err(e) = r {
                    self.parked = true;
                    self.settle("error", Some(e.to_string()));
                }
                self.emit_state(None);
                Flow::Continue
            }
            Cmd::Pause => {
                // The protocol loop already sent request_abort, so a blocked
                // call has yielded; the escalation itself runs here, where
                // the book is ours. Pinging an idle session is harmless.
                self.parked = true;
                let _ = self.book.lock().expect("book lock").interrupt();
                self.emit_state(None);
                self.settle("interrupted", None);
                Flow::Continue
            }
            Cmd::Close => Flow::Exit,
        }
    }

    /// Step toward the target until it is reached, something parks the run,
    /// or a new command arrives.
    fn drive(&mut self) -> Flow {
        loop {
            if let Flow::Exit = self.drain() {
                return Flow::Exit;
            }
            if self.parked || !self.have_snapshot {
                return Flow::Continue;
            }

            // The first sync after a new snapshot reconciles without
            // admitting anything: rollback is driven by the text, never the
            // target, and the frontier can only be read honestly after the
            // diff has run. Deciding "done" from the pre-rollback frontier
            // once made a fully-green book settle as reached_target right
            // after an edit -- one reconcile rolled the suffix back to
            // pending, nothing re-admitted it, and the book sat gray.
            let goal = self.goal_unit();
            let through = if !self.reconciled {
                Some(0)
            } else {
                let frontier = self.frontier();
                match goal {
                    Some(g) if frontier <= g => Some(self.units[frontier].line_end),
                    _ => {
                        self.emit_state(None);
                        self.settle("reached_target", None);
                        return Flow::Continue;
                    }
                }
            };
            let reconciling = !self.reconciled;
            let o = {
                let mut book = self.book.lock().expect("book lock");
                book.sync_bytes(&self.bytes, through, STEP)
            };
            let o = match o {
                Ok(o) => o,
                Err(SyncError::Busy { .. }) => {
                    // A proof from an earlier goal is still in flight.
                    if let Flow::Exit = self.follow_running() {
                        return Flow::Exit;
                    }
                    continue;
                }
                Err(e) => {
                    self.parked = true;
                    self.settle("error", Some(e.to_string()));
                    return Flow::Continue;
                }
            };
            if reconciling {
                self.reconciled = true;
            }
            // Say so as soon as it is known. The frontier is *visible* here:
            // a green gutter is an assertion, and one resting on a book that
            // has since changed is a claim the user is looking at. Reported
            // only -- acting on it is the next step, and belongs to a
            // deliberate choice rather than a surprise mid-proof.
            if !o.stale_includes.is_empty() {
                self.out.notify(
                    "stale_deps",
                    &wire::stale_deps(&self.id, self.revision, &o.stale_includes),
                );
            }
            for u in &o.admitted {
                self.out.notify("unit", &wire::unit_admitted(&self.id, self.revision, u));
            }
            for f in &o.skipped {
                self.out.notify("unit", &wire::unit_failure(&self.id, self.revision, f, "skipped"));
            }
            if let Some(f) = &o.failed {
                if f.still_running {
                    self.emit_state(Some(f.index));
                    if let Flow::Exit = self.follow_running() {
                        return Flow::Exit;
                    }
                    continue;
                }
                self.failed_unit = Some(f.index);
                self.out.notify("unit", &wire::unit_failure(&self.id, self.revision, f, "failed"));
                self.emit_state(None);
                self.parked = true;
                self.settle("failed", f.error_line.clone());
                return Flow::Continue;
            }
            self.emit_state(None);
        }
    }

    /// Follow the in-flight unit to completion, streaming its tail.
    fn follow_running(&mut self) -> Flow {
        let running = self.book.lock().expect("book lock").running_unit();
        // Hashes of the prefix through the running unit: a new snapshot that
        // keeps them intact leaves the proof valid, so it keeps running.
        let guard: Vec<u64> = match running {
            Some(i) => self.units.iter().take(i + 1).map(|u| u.hash).collect(),
            None => Vec::new(),
        };
        loop {
            if let Flow::Exit = self.drain() {
                return Flow::Exit;
            }
            if self.parked {
                // A Pause already interrupted and settled.
                return Flow::Continue;
            }
            if self.have_snapshot {
                let intact = guard.len() <= self.units.len()
                    && guard.iter().zip(&self.units).all(|(h, u)| *h == u.hash);
                if !intact {
                    // The edit invalidated the running proof; the next sync's
                    // diff decides what re-runs.
                    let _ = self.book.lock().expect("book lock").interrupt();
                    return Flow::Continue;
                }
            }
            let r = {
                let mut book = self.book.lock().expect("book lock");
                book.resume(SLICE)
            };
            match r {
                Ok(None) => return Flow::Continue,
                Ok(Some(Resumed::StillRunning { .. })) => {
                    let p = {
                        let mut book = self.book.lock().expect("book lock");
                        book.progress(TAIL_LINES)
                    };
                    if let Ok(Some(p)) = p {
                        self.out.notify("tail", &wire::TailEvent {
                            book: self.id.clone(),
                            unit: running,
                            elapsed_ms: p.elapsed.as_millis(),
                            text: p.tail,
                        });
                    }
                }
                Ok(Some(Resumed::Admitted(u))) => {
                    self.out.notify("unit", &wire::unit_admitted(&self.id, self.revision, &u));
                    self.emit_state(None);
                    return Flow::Continue;
                }
                Ok(Some(Resumed::Skipped(f))) => {
                    self.out.notify("unit", &wire::unit_failure(&self.id, self.revision, &f, "skipped"));
                    self.emit_state(None);
                    return Flow::Continue;
                }
                Ok(Some(Resumed::Failed(f))) => {
                    if f.still_running {
                        // The skip-proofs retry is itself in flight now.
                        continue;
                    }
                    self.failed_unit = Some(f.index);
                    self.out.notify("unit", &wire::unit_failure(&self.id, self.revision, &f, "failed"));
                    self.emit_state(None);
                    self.parked = true;
                    self.settle("failed", f.error_line.clone());
                    return Flow::Continue;
                }
                // No eval verb exists, so nothing eval-shaped can be pending.
                Ok(Some(Resumed::Evaluated(_))) => return Flow::Continue,
                Err(e) => {
                    self.parked = true;
                    self.settle("error", Some(e.to_string()));
                    return Flow::Continue;
                }
            }
        }
    }

    /// The unit the target selects, or `None` when it selects nothing.
    fn goal_unit(&self) -> Option<usize> {
        match self.target_line {
            None => self.units.len().checked_sub(1),
            Some(l) => self.units.iter().rposition(|u| u.line_start <= l),
        }
    }

    /// Units currently in the world (skipped ones included).
    fn frontier(&self) -> usize {
        self.book.lock().expect("book lock").admitted_lines().len()
    }

    fn emit_state(&self, running: Option<usize>) {
        let (admitted, skipped) = {
            let book = self.book.lock().expect("book lock");
            (book.admitted_lines().len(), book.skipped_units())
        };
        let conditional_from = skipped.first().copied();
        let units = self
            .units
            .iter()
            .enumerate()
            .map(|(i, u)| {
                let status = if skipped.contains(&i) {
                    "skipped"
                } else if i < admitted {
                    match conditional_from {
                        Some(c) if i > c => "conditional",
                        _ => "admitted",
                    }
                } else if Some(i) == running {
                    "running"
                } else if Some(i) == self.failed_unit {
                    "failed"
                } else {
                    "pending"
                };
                wire::UnitState {
                    index: i,
                    line_start: u.line_start,
                    line_end: u.line_end,
                    status,
                }
            })
            .collect();
        self.out.notify("state", &wire::StateEvent {
            book: self.id.clone(),
            revision: self.revision,
            conditional_from,
            running,
            units,
        });
    }

    fn settle(&self, reason: &'static str, message: Option<String>) {
        self.out.notify("settled", &wire::SettledEvent {
            book: self.id.clone(),
            revision: self.revision,
            reason,
            message,
        });
    }
}
