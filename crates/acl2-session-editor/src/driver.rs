//! The stepping driver: one thread per book, owning its shared handle,
//! turning snapshot updates into the event stream an editor paints from.
//!
//! This is the stepping loop, verbatim: split the snapshot, sync one unit
//! at a time (`through_line` = the next unit's last line), follow an
//! in-flight proof with short resumes, and let each outcome's counts advance
//! the frontier. Commands are polled between steps and between slices, so an
//! edit lands within a slice of arriving -- and only an edit that touches the
//! prefix through the running unit aborts its proof.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;

use acl2_session::registry::SharedBook;
use acl2_session::{split, Form, NameDetail, Outcome, Resumed, SyncError, WorldChange};
use serde_json::Value;

use crate::wire;
use crate::Out;

/// One instruction from the protocol loop.
pub enum Cmd {
    Update { bytes: Vec<u8>, target_line: Option<usize>, revision: u64 },
    Retract { keep: usize },
    /// A form to run outside the file. `id` is the JSON-RPC request's, since
    /// the driver owns the session and therefore sends the reply itself.
    Eval { id: Value, form: String },
    /// What does the world know about this name? Answered from the cache, or
    /// from a 10 ms probe when the session is free, and otherwise declined --
    /// never queued. See [`Driver::describe`].
    Describe { id: Value, name: String },
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
/// Deadline for one `describe` probe. Generous next to the 10 ms it measures
/// at, because the only thing that could make it slow is
/// `decode-logical-name` scanning a very large world -- and short enough that
/// a pathological one cannot stall the driver's loop.
const DESCRIBE: Duration = Duration::from_secs(2);

enum Flow {
    Continue,
    Exit,
}

fn to_value(r: impl serde::Serialize) -> Value {
    serde_json::to_value(r).expect("serialize")
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
    /// Evals waiting for the session to be free, oldest first.
    ///
    /// A queue rather than a rejection: the session is single-threaded, so an
    /// eval typed while a proof runs can only wait or be refused, and refusing
    /// asks the user to watch the status bar and retype. It waits exactly one
    /// form -- the pump runs between units, where the world is a prefix and
    /// nothing is in flight -- so a query never sits behind a whole file.
    eval_queue: VecDeque<(Value, String)>,
    /// The eval whose form is in flight, so the resume loop knows whom to
    /// answer when it finishes.
    eval_in_flight: Option<Value>,
    /// Answers to `describe`, including the negative ones -- hovering a
    /// variable or a word in a comment is the commonest case there is, and
    /// asking ACL2 about it twice is pure waste.
    ///
    /// Emptied by `emit_state`, i.e. whenever the world moves. That is
    /// deliberately blunt: admitting a unit can turn a cached "unknown" into
    /// a name, and a rollback can do the reverse, so anything subtler would
    /// be a correctness argument to maintain forever for a 10 ms saving. It
    /// costs nothing where it matters, because a driver with nothing to do
    /// emits no state -- and an idle session is exactly when someone is
    /// reading code and hovering.
    details: HashMap<String, Option<NameDetail>>,
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
            eval_queue: VecDeque::new(),
            eval_in_flight: None,
            details: HashMap::new(),
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
            Cmd::Eval { id, form } => {
                let ahead = self.eval_queue.len();
                // Whether it waits is decided by the pump, not here; saying
                // `queued` now and `running` when it goes means the client
                // never has to guess which of the two the silence is.
                self.eval_state("queued", ahead);
                self.eval_queue.push_back((id, form));
                Flow::Continue
            }
            Cmd::Describe { id, name } => {
                self.describe(&id, &name);
                Flow::Continue
            }
            Cmd::Pause => {
                // The protocol loop already sent request_abort, so a blocked
                // call has yielded; the escalation itself runs here, where
                // the book is ours. Pinging an idle session is harmless.
                self.parked = true;
                let _ = self.book.lock().expect("book lock").interrupt();
                // An interrupt is aimed at whatever is in flight, and that may
                // be an eval. Its caller is holding an unanswered request, so
                // say so rather than leaving the promise open forever. Evals
                // merely *queued* are untouched: the pump runs even on a
                // parked driver, and a query is worth more, not less, once
                // the proof it was waiting behind has been called off.
                self.abandon_in_flight("interrupted");
                self.emit_state(None);
                self.settle("interrupted", None);
                Flow::Continue
            }
            Cmd::Close => {
                self.abandon_all_evals("the session is closing");
                Flow::Exit
            }
        }
    }

    /// Answer a hover, without ever making it wait.
    ///
    /// The opposite policy to `eval`, and for the opposite reason. An eval is
    /// something the user typed and meant, so it queues: refusing it would ask
    /// them to watch the status bar and retype. A describe is something the
    /// editor asked because a cursor moved, hundreds of times a session, and
    /// the user is not waiting for it -- so it answers now or declines. The
    /// alternative was watched for: a hover queued behind a four-minute proof
    /// would arrive long after the mouse had moved on, and VSCode would render
    /// a tooltip for a symbol nobody is looking at any more.
    ///
    /// This runs in `drain`, which `follow_running` calls at the top of every
    /// resume slice -- so a cached answer comes back inside ~500 ms even while
    /// a proof is grinding, and a miss is declined just as promptly, because
    /// `Book::describe` reports `Busy` without blocking.
    ///
    /// `available: false` is not an error and not "no such name": the client
    /// shows nothing and asks again on the next hover.
    fn describe(&mut self, id: &Value, name: &str) {
        if let Some(d) = self.details.get(name) {
            self.out.reply(id, to_value(wire::described(name, d.as_ref())));
            return;
        }
        let r = {
            let mut book = self.book.lock().expect("book lock");
            book.describe(name, DESCRIBE)
        };
        match r {
            Ok(d) => {
                self.out.reply(id, to_value(wire::described(name, d.as_ref())));
                self.details.insert(name.to_string(), d);
            }
            Err(SyncError::Busy { .. }) => {
                self.out.reply(id, to_value(wire::describe_busy(name)))
            }
            // Anything else is the session being unwell, which the client
            // learns about through `status`; a hover is not the place to
            // surface it, so it degrades to "ask again" like a busy one.
            Err(_) => self.out.reply(id, to_value(wire::describe_busy(name))),
        }
    }

    /// Run queued evals until the queue is empty or the session is busy.
    ///
    /// Called only where nothing should be in flight; `Busy` is nevertheless
    /// handled, because "should" is not "is" and a queue put back is a
    /// one-tick delay where an unwrapped error would be a lost reply.
    fn pump_evals(&mut self) -> Flow {
        while let Some((id, form)) = self.eval_queue.pop_front() {
            self.eval_state("running", self.eval_queue.len());
            let r = {
                let mut book = self.book.lock().expect("book lock");
                book.eval(&form, STEP)
            };
            match r {
                // The deadline is a slice, not a limit: a `thm` that grinds
                // past it is followed like any other in-flight form, tails
                // and all, and answers when it finishes.
                Ok(r) if r.outcome == Outcome::StillRunning => {
                    self.eval_in_flight = Some(id);
                    if let Flow::Exit = self.follow_running() {
                        return Flow::Exit;
                    }
                    // Whatever ended it -- completion, interrupt, exit --
                    // answered or abandoned the request; nothing is owed here.
                }
                Ok(r) => self.finish_eval(&id, &r),
                Err(SyncError::Busy { .. }) => {
                    self.eval_queue.push_front((id, form));
                    return Flow::Continue;
                }
                Err(e) => self.out.error(&id, -32603, e.to_string()),
            }
        }
        Flow::Continue
    }

    /// Answer an eval's caller and act on what it did to the world.
    fn finish_eval(&mut self, id: &Value, r: &acl2_session::EvalResult) {
        self.out.reply(id, serde_json::to_value(wire::evaluated(r)).expect("serialize"));
        if r.world_change == WorldChange::Dirty {
            // The rollback did not verify, so the prefix invariant no longer
            // holds and the frontier on screen is a claim we cannot support.
            // Re-reconciling makes the next drive rebuild by replay, which is
            // the core's own fallback -- reached here rather than silently.
            self.reconciled = false;
            self.emit_state(None);
        }
    }

    /// Tell whoever is waiting on the in-flight eval that no result is coming.
    ///
    /// A JSON-RPC request has exactly one ending, and a client that never gets
    /// it waits forever; every path that stops an eval short comes through
    /// here or through [`Driver::abandon_all_evals`].
    fn abandon_in_flight(&mut self, why: &str) {
        if let Some(id) = self.eval_in_flight.take() {
            self.out.error(&id, -32603, format!("eval abandoned: {why}"));
        }
    }

    /// The same, plus every eval still waiting -- for a driver that is going
    /// away, where "queued" would otherwise mean "queued forever".
    fn abandon_all_evals(&mut self, why: &str) {
        self.abandon_in_flight(why);
        let waiting: Vec<Value> = self.eval_queue.drain(..).map(|(id, _)| id).collect();
        for id in waiting {
            self.out.error(&id, -32603, format!("eval abandoned: {why}"));
        }
    }

    fn eval_state(&self, state: &'static str, ahead: usize) {
        self.out.notify("eval_state", &wire::EvalStateEvent {
            book: self.id.clone(),
            state,
            ahead,
        });
    }

    /// Step toward the target until it is reached, something parks the run,
    /// or a new command arrives.
    fn drive(&mut self) -> Flow {
        loop {
            if let Flow::Exit = self.drain() {
                return Flow::Exit;
            }
            // Between units, which is the whole busy policy: the session is
            // idle here and the world is a prefix, so a queued query goes as
            // soon as the form in flight is done rather than after the file
            // is. Ahead of the parked check on purpose -- a driver parked on
            // a red form is exactly when a query is worth asking.
            if let Flow::Exit = self.pump_evals() {
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
                Ok(None) => {
                    // Nothing is running after all. For a unit that is the
                    // ordinary race with a sync step; for an eval it means the
                    // form went away without a result, so its caller is owed
                    // an answer it will otherwise never get.
                    self.abandon_in_flight("the form is no longer running");
                    return Flow::Continue;
                }
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
                // An eval outran its slice and has now finished. It is not
                // part of the file, so it moves no frontier and paints
                // nothing -- it just answers the caller who is holding the
                // request open.
                Ok(Some(Resumed::Evaluated(r))) => {
                    if let Some(id) = self.eval_in_flight.take() {
                        self.finish_eval(&id, &r);
                    }
                    return Flow::Continue;
                }
                Err(e) => {
                    self.parked = true;
                    self.abandon_in_flight(&e.to_string());
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

    fn emit_state(&mut self, running: Option<usize>) {
        // The world just moved, so every cached answer about a name is a
        // claim about a world that no longer exists. See `Driver::details`
        // on why this is blunt rather than incremental.
        self.details.clear();
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
