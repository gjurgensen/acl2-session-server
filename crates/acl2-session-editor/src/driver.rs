//! The stepping driver: one thread per book, owning its shared handle,
//! turning snapshot updates into the event stream an editor paints from.
//!
//! This is the stepping loop, verbatim: split the snapshot, sync one unit
//! at a time (`through_line` = the next unit's last line), follow an
//! in-flight proof with short resumes, and let each outcome's counts advance
//! the frontier. Commands are polled between steps and between slices, so an
//! edit lands within a slice of arriving -- and only an edit that touches the
//! prefix through the running unit aborts its proof.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;

use acl2_session::registry::SharedBook;
use acl2_session::{
    split, CensusEntry, Form, NameDetail, Origin, Outcome, Resumed, SyncError,
    WorldChange,
};
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
    /// Where is this name defined? Same never-queue policy as `Describe`,
    /// which it is built on. See [`Driver::definition`].
    Definition { id: Value, name: String },
    /// Every `include-book` in the snapshot, resolved. See [`Driver::links`].
    Links { id: Value },
    /// Names the user could be typing. See [`Driver::complete`].
    Complete { id: Value, prefix: String },
    /// The outline of the snapshot. See [`Driver::symbols`].
    Symbols { id: Value },
    /// What did this form introduce -- rule classes, trigger terms, derived
    /// runes? Served from the scrape taken as the form was admitted, so it
    /// needs no session and is never declined. See [`Driver::rules`].
    Rules { id: Value, name: String },
    /// One topic's documentation. See [`Driver::doc`].
    Doc { id: Value, name: String },
    /// Certify the book on disk. See [`Driver::certify`].
    Certify { id: Value, jobs: Option<usize> },
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
/// Deadline for the census walk. Measured at 2.5 s on a std+fty world;
/// a Kestrel-heavy one is larger, and it runs on an idle
/// tick where the time is free, so this is generous on purpose.
const CENSUS: Duration = Duration::from_secs(60);

/// Keyword families worth offering, which need no world at all. Not
/// exhaustive and not meant to be -- these are the ones typed constantly.
const KEYWORDS: &[&str] = &[
    // Rule classes.
    ":rewrite", ":linear", ":definition", ":type-prescription", ":forward-chaining",
    ":elim", ":congruence", ":equivalence", ":refinement", ":induction", ":well-founded-relation",
    ":built-in-clause", ":compound-recognizer", ":meta", ":clause-processor", ":tau-system",
    // Hints.
    ":use", ":by", ":in-theory", ":induct", ":expand", ":cases", ":hands-off", ":restrict",
    ":nonlinearp", ":do-not", ":do-not-induct", ":instructions", ":otf-flg",
    // xargs and friends.
    ":guard", ":guard-hints", ":guard-debug", ":measure", ":well-founded-relation",
    ":mode", ":verify-guards", ":normalize", ":stobjs", ":ruler-extenders", ":hints",
    // Common flags.
    ":dir", ":system", ":rule-classes", ":corollary", ":trigger-terms", ":doc",
];

enum Flow {
    Continue,
    Exit,
}

fn to_value(r: impl serde::Serialize) -> Value {
    serde_json::to_value(r).expect("serialize")
}

/// Whether two written symbols name the same thing, for the purpose of
/// finding a definition in text.
///
/// Case-insensitive, because ACL2's reader upcases and books are written in
/// both. Package prefixes are compared only when *both* carry one: a book
/// that is `(in-package "FTY")` writes `(define fixtype …)` while the world
/// reports `fty::fixtype`, so requiring the prefix to appear in the text
/// would find nothing at all in exactly the books that use packages most.
/// Comparing bare tails can in principle confuse `foo::bar` with `baz::bar`
/// -- but only within one file, which is a single book's own namespace, so
/// the collision needs two packages defining the same name in the same file.
fn same_symbol(written: &str, wanted: &str) -> bool {
    let bare = |s: &str| s.rsplit("::").next().unwrap_or(s).to_ascii_lowercase();
    match (written.contains("::"), wanted.contains("::")) {
        (true, true) => written.eq_ignore_ascii_case(wanted),
        _ => bare(written) == bare(wanted),
    }
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
    /// Names ACL2's own sources define, mapped to file and line. Built on the
    /// first jump into a built-in and kept: see [`Driver::builtin_definition`].
    builtins: Option<HashMap<String, (String, usize)>>,
    /// Every name in the world, once someone has needed it.
    ///
    /// Built on an *idle tick* rather than on the request that wants it: the
    /// walk is seconds, and the driver is single-threaded, so building it
    /// inline would stall an edit or a hover behind it. The first completion
    /// therefore answers from the buffer alone and says `incomplete`, and the
    /// one after it has the world.
    symbols: Option<Vec<CensusEntry>>,
    /// A completion arrived with no census built. Cleared once one is.
    census_wanted: bool,
    /// A hover arrived with no doc index built. Cleared once one is.
    docs_wanted: bool,
    /// The certification in flight: whom to answer, and with what `-j`.
    ///
    /// A build is not a submission -- it is a separate process tree running
    /// `cert.pl`, which runs `make`, which runs its own ACL2s -- so it does
    /// not block the session and the reconciler keeps going while it runs.
    /// What it does need is following, which is what `certify_tick` does.
    certifying: Option<(Value, Option<usize>)>,
    /// Every documented topic's one-line summary, by folded name.
    ///
    /// Built on the same tick as the census and invalidated by the same
    /// signal, but far cheaper -- one table read, a sentence per topic. Held
    /// resident so a summary is on the hover even while a proof runs, which
    /// is the same reasoning as everything else here. `Some(empty)` records
    /// that the world has no xdoc, so it is attempted once rather than on
    /// every hover.
    docs: Option<HashMap<String, String>>,
    /// The include count the census was built against. When the world comes
    /// to rest on a different set of books, the table is describing a world
    /// that no longer exists.
    census_deps: Option<usize>,
    /// Names this file's own units introduced, and which unit introduced
    /// each. Free: the unit envelopes already carry them, so nothing is asked
    /// of ACL2 for this. It exists so a hover on your own definition answers
    /// *while a proof is running*, which is exactly when you are reading
    /// rather than writing. Measured as the right granularity: an
    /// `include-book` reports no inner events at all, so a library's
    /// thousands of names never land here -- those stay with `details`,
    /// where being stable makes them cache well.
    file_names: HashMap<String, (String, &'static str, usize)>,
}

/// What kind of thing a definer introduces, from the definer's own name.
///
/// Only the heads whose meaning is fixed; everything else -- `fty::defprod`,
/// `std::deflist`, and whatever a book invents next -- falls to `other`,
/// which is honest. Guessing that an unknown `def…` makes a function would
/// put a misleading icon beside half a modern book.
fn symbol_kind(head: &str) -> &'static str {
    let bare = head.rsplit("::").next().unwrap_or(head).to_ascii_lowercase();
    match bare.as_str() {
        "defun" | "defund" | "defun-sk" | "defund-sk" | "define" | "defines"
        | "defun-inline" | "defiteration" => "function",
        "defthm" | "defthmd" | "defrule" | "defruled" | "defaxiom" | "defcong"
        | "defequiv" | "defrefinement" => "theorem",
        "defmacro" | "defabbrev" => "macro",
        "defconst" | "defconstant" => "constant",
        _ => "other",
    }
}

/// Case-folded, package-stripped: the key both sides of a lookup agree on.
fn fold(name: &str) -> String {
    name.rsplit("::").next().unwrap_or(name).to_ascii_lowercase()
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
            builtins: None,
            symbols: None,
            census_wanted: false,
            census_deps: None,
            docs: None,
            docs_wanted: false,
            certifying: None,
            file_names: HashMap::new(),
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
                    self.census_tick();
                    self.certify_tick();
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
            // Not only on the timeout branch. A client that polls -- which is
            // exactly what a completion popup does -- keeps the channel warm,
            // so a driver waiting for true idleness would never build the
            // census at all and completion would never see the world. Found
            // by the protocol test, which polls every 500 ms and waited
            // forever. It runs at most once, so the cost is one pause.
            self.census_tick();
            self.certify_tick();
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
                        self.settle_split_error(&e);
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
            Cmd::Definition { id, name } => {
                self.definition(&id, &name);
                Flow::Continue
            }
            Cmd::Links { id } => {
                self.links(&id);
                Flow::Continue
            }
            Cmd::Complete { id, prefix } => {
                self.complete(&id, &prefix);
                Flow::Continue
            }
            Cmd::Symbols { id } => {
                self.symbols(&id);
                Flow::Continue
            }
            Cmd::Rules { id, name } => {
                self.rules(&id, &name);
                Flow::Continue
            }
            Cmd::Doc { id, name } => {
                self.doc(&id, &name);
                Flow::Continue
            }
            Cmd::Certify { id, jobs } => {
                self.certify(&id, jobs);
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
                // `interrupt` kills the build as well as the proof.
                self.abandon_certify("interrupted");
                self.emit_state(None);
                self.settle("interrupted", None);
                Flow::Continue
            }
            Cmd::Close => {
                self.abandon_all_evals("the session is closing");
                self.abandon_certify("the session is closing");
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
    /// What a form introduced, in the detail `:pr` gives -- from memory.
    ///
    /// Unlike `describe` this never has to decline. The scrape was taken
    /// when the form was admitted precisely so that the answer would not
    /// depend on the prover being free, and reading what an earlier form
    /// defined while a later one grinds is the case it exists for.
    fn rules(&mut self, id: &Value, name: &str) {
        let found = {
            let book = self.book.lock().expect("book lock");
            book.rules_for(name).map(|r| wire::rules_reply(name, Some(r)))
        };
        let r = found.unwrap_or_else(|| wire::rules_reply(name, None));
        self.out.reply(id, to_value(r));
    }

    fn describe(&mut self, id: &Value, name: &str) {
        match self.cached_detail(name) {
            Ok(d) => {
                let mut r = wire::described(name, d.as_ref());
                r.short = self.doc_short(name);
                self.out.reply(id, to_value(r))
            }
            // The session is busy, or unwell. Before declining, answer from
            // what needs no session at all: this file's own names, recorded
            // as their units admitted. A partial answer that is true beats an
            // empty tooltip, and this is the case that matters -- hovering
            // your own definition while a proof runs is exactly when you are
            // reading rather than writing.
            //
            // Anything else degrades to "ask again". A session that is unwell
            // is reported through `status`; a tooltip is not the place to say
            // so a second time.
            Err(()) => {
                let mut r = self.known_locally(name).unwrap_or_else(|| wire::describe_busy(name));
                // Resident, so it is there even when the rest of the answer
                // had to be declined.
                r.short = self.doc_short(name);
                self.out.reply(id, to_value(r));
            }
        }
    }

    /// Where is this name defined?
    ///
    /// Three tiers, and each says honestly how it got its answer. The world
    /// names the *file*; finding the form inside it is
    /// text work, done here.
    ///
    /// 1. A name this file defines. The world reports no book for it, so the
    ///    answer is a unit of the buffer -- located by the definer scan over
    ///    the snapshot we already hold, which needs no ACL2 and therefore
    ///    works for units below the frontier that have not been admitted yet.
    /// 2. A name from an included book: `describe` gives the file, and the
    ///    same scan over that file's text gives the line.
    /// 3. A generated name -- absent from its book's text. Measured, the
    ///    decomposition heuristic on its own is only ~83% precise however it
    ///    is guarded, so roughly one jump in six would land on the wrong
    ///    form. That is worse than not answering, so until the census
    ///    supplies the anchoring window that took it to 100%, these return
    ///    the file with **no line**: the editor opens it at the top, which is
    ///    honest about what is known.
    fn definition(&mut self, id: &Value, name: &str) {
        // The buffer first, and not merely as an optimisation. A name written
        // in this file but not yet admitted is unknown to the world -- so
        // asking the world first would fail to find exactly the definition
        // the author is in the middle of writing, which is when they are most
        // likely to want it. Scanning the snapshot needs no session at all,
        // which also means an in-file jump works while a proof is running.
        //
        // A local definition also rightly wins over an included one of the
        // same name: it is the text in front of the reader.
        if let Some(s) = self.site_in(&self.bytes.clone(), name) {
            let unit = self.units.iter().position(|u| s.offset >= u.start && s.offset < u.end);
            return self.out.reply(id, to_value(wire::DefinitionReply {
                available: true,
                found: true,
                unit,
                path: None,
                line: Some(s.line),
                how: Some("definer"),
            }));
        }
        // Not in the buffer, so ask the world which book has it.
        let detail = match self.cached_detail(name) {
            Ok(d) => d,
            Err(()) => return self.out.reply(id, to_value(wire::definition_busy())),
        };
        let Some(d) = detail else {
            return self.out.reply(id, to_value(wire::definition_missing()));
        };
        if d.origin == Origin::Builtin {
            return self.builtin_definition(id, name);
        }
        let Some(p) = d.book_path else {
            return self.out.reply(id, to_value(wire::definition_missing()));
        };
        // Tier 3, and tier 4 once the census can supply the world order for
        // that book: either the name is written there, or it was generated
        // and the decomposition/anchoring pair finds the form that made it.
        let order = self.book_order(d.book.as_deref());
        let found = std::fs::read(&p).ok().and_then(|b| acl2_session::locate(&b, &order, name));
        self.out.reply(id, to_value(wire::DefinitionReply {
            available: true,
            found: true,
            unit: None,
            path: Some(p.display().to_string()),
            line: found.map(|f| f.line),
            how: found.map(|f| f.how.as_str()),
        }));
    }

    /// The names the world attributes to one book, in **file order**.
    ///
    /// The census is newest-first, so this reverses it. Empty when no census
    /// has been built, which `locate` reads as "tier 1 only" -- an honest
    /// answer rather than a guessed one, and the reason a generated name
    /// still opens its book at the top until completion has been used once.
    fn book_order(&self, book: Option<&str>) -> Vec<String> {
        let (Some(book), Some(all)) = (book, self.symbols.as_ref()) else {
            return Vec::new();
        };
        let mut v: Vec<String> = all
            .iter()
            .filter(|e| e.book.as_deref() == Some(book))
            .map(|e| e.name.clone())
            .collect();
        v.reverse();
        v
    }

    /// Every `include-book` in the snapshot, resolved to a file.
    ///
    /// Needs no session at all -- it is the snapshot plus two directories --
    /// so it answers immediately whatever the prover is doing. It lives here
    /// rather than in the protocol loop only because the snapshot does.
    ///
    /// Resolution mirrors ACL2's: `:dir :system` is relative to the books
    /// directory the session reported at startup, and a bare name is relative
    /// to the directory of the file doing the including. A project's own
    /// `:dir` alias is declined rather than guessed at -- the same choice
    /// `resolve_dep` makes for dependencies, and for the same reason. A
    /// target that does not exist is declined too: a link that opens nothing
    /// is worse than no link.
    fn links(&mut self, id: &Value) {
        let (books, own) = {
            let book = self.book.lock().expect("book lock");
            (
                book.books_dir().map(Path::to_path_buf),
                book.path().parent().map(Path::to_path_buf),
            )
        };
        let links = acl2_session::include_sites(&self.bytes)
            .into_iter()
            .map(|s| {
                let rel = format!("{}.lisp", s.book);
                let base = match s.dir.as_deref() {
                    None => own.clone(),
                    Some(d) if d.eq_ignore_ascii_case(":system") => books.clone(),
                    Some(_) => None,
                };
                let path = base
                    .map(|b| b.join(&rel))
                    .filter(|p| p.is_file())
                    .map(|p| p.display().to_string());
                wire::BookLink { line: s.line, book: s.book, path }
            })
            .collect();
        self.out.reply(id, to_value(wire::LinksReply { links }));
    }

    /// Certify the book, and follow the build rather than waiting on it.
    ///
    /// A zero deadline both *starts* the build and polls it without blocking:
    /// the core's `certify` starts one if none is running and then returns as
    /// soon as the deadline is out, so the same call serves for both. That is
    /// what lets a build be followed exactly like a proof -- `certify_tick`
    /// polls between everything else -- instead of holding the driver for the
    /// minutes a real certification takes.
    ///
    /// A second request while one runs is refused rather than queued. Builds
    /// are long, and a queue of identical builds serves nobody.
    fn certify(&mut self, id: &Value, jobs: Option<usize>) {
        if self.certifying.is_some() {
            self.out.error(id, -32603, "already certifying this book".into());
            return;
        }
        // Certification runs against the file on **disk**, never the buffer:
        // it is what `cert.pl` would do from a shell, so an unsaved edit
        // certifies as whatever was saved. The client saves first.
        let r = {
            let mut book = self.book.lock().expect("book lock");
            book.certify(Duration::ZERO, jobs.unwrap_or(1))
        };
        match r {
            Ok(r) if r.status == acl2_session::CertStatus::StillRunning => {
                self.certifying = Some((id.clone(), jobs));
            }
            Ok(r) => self.out.reply(id, to_value(wire::certified(&r))),
            Err(e) => self.out.error(id, -32603, e.to_string()),
        }
    }

    /// Follow the build one poll at a time, streaming its tail.
    fn certify_tick(&mut self) {
        let Some((id, jobs)) = self.certifying.clone() else { return };
        let (r, progress) = {
            let Ok(mut book) = self.book.try_lock() else { return };
            let r = book.certify(Duration::ZERO, jobs.unwrap_or(1));
            let p = book.build_progress(TAIL_LINES);
            (r, p)
        };
        match r {
            Ok(rep) if rep.status == acl2_session::CertStatus::StillRunning => {
                if let Some(p) = progress {
                    self.out.notify("cert_tail", &wire::CertTailEvent {
                        book: self.id.clone(),
                        elapsed_ms: p.elapsed.as_millis(),
                        lines_so_far: p.lines_so_far,
                        tail: p.tail,
                    });
                }
            }
            Ok(rep) => {
                self.certifying = None;
                self.out.reply(&id, to_value(wire::certified(&rep)));
            }
            Err(e) => {
                self.certifying = None;
                self.out.error(&id, -32603, e.to_string());
            }
        }
    }

    /// Tell a waiting certify that its build is gone.
    ///
    /// `Book::interrupt` kills the build outright, which also means the next
    /// poll would find no build and cheerfully start a *second* one. Clearing
    /// the slot here is what stops that, and answering the request is what
    /// stops a client waiting forever -- the same discipline evals have.
    fn abandon_certify(&mut self, why: &str) {
        if let Some((id, _)) = self.certifying.take() {
            self.out.error(&id, -32603, format!("certification {why}"));
        }
    }

    /// One topic's documentation.
    ///
    /// The summary comes from the resident index, so it is there whatever the
    /// prover is doing; the body needs the session, because keeping every
    /// topic's `:long` in memory would be pages apiece for thousands of
    /// topics to serve the one someone opened. Busy therefore declines the
    /// body but still answers with the summary -- a heading and a sentence
    /// beats an empty window, and the client can ask again.
    fn doc(&mut self, id: &Value, name: &str) {
        let short = self.doc_short(name);
        let long = {
            let mut book = self.book.lock().expect("book lock");
            book.doc_topic(name, DESCRIBE)
        };
        let (available, long) = match long {
            Ok(l) => (true, l),
            Err(_) => (false, None),
        };
        self.out.reply(id, to_value(wire::DocReply {
            available,
            found: short.is_some() || long.is_some(),
            name: name.to_string(),
            short,
            long,
        }));
    }

    /// The outline of the snapshot.
    ///
    /// Built entirely from the text, so it needs no session and works while a
    /// proof runs, and -- more to the point -- it covers the whole file
    /// rather than the admitted prefix. An outline that stopped at the
    /// frontier would be least useful exactly when a book is long and only
    /// partly checked, which is when someone reaches for it.
    ///
    /// One entry per unit, named by what it introduces. A unit that
    /// introduces nothing of its own is named by its head -- `encapsulate`,
    /// `mutual-recursion`, `in-theory` -- with any definers inside it as
    /// children, because a flat outline would hide the members of exactly the
    /// forms most worth navigating. `include-book` is named by the book it
    /// pulls in, which is what a reader is looking for when scanning.
    fn symbols(&mut self, id: &Value) {
        let defs = acl2_session::definer_sites(&self.bytes);
        let mut out: Vec<wire::BookSymbol> = Vec::new();
        for (i, u) in self.units.iter().enumerate() {
            let text = &self.bytes[u.start..u.end];
            let inside: Vec<&acl2_session::DefinerSite> =
                defs.iter().filter(|d| d.offset >= u.start && d.offset < u.end).collect();
            let head = acl2_session::head_of(text);
            // The unit's own definer is the one that starts where it does;
            // anything else nested is a child. A `(local (defthm ...))` has
            // none at the start, so its inner definer names it instead.
            let own = inside.iter().find(|d| d.offset == u.start);
            let includes = acl2_session::include_sites(text);
            // Named by what it introduces, if it introduces something itself;
            // otherwise by the book it includes, otherwise by its head. Only
            // the first case consumes a definer -- in the others every
            // definer found inside is a child.
            let (name, kind) = if let Some(d) = own {
                (d.name.clone(), symbol_kind(&d.head))
            } else if let Some(inc) = includes.first() {
                (format!("include-book \"{}\"", inc.book), "include")
            } else if let Some(h) = head.as_deref() {
                (h.to_string(), "other")
            } else {
                (String::from("(form)"), "other")
            };
            // Everything nested *inside* the unit, which is exactly the sites
            // that are not the unit's own head. Identified by offset rather
            // than by position in the list: the outermost site does sort
            // first today, because `definers_in` records a form before
            // recursing into it, but relying on that is an invisible coupling
            // between two functions -- and if the walk order ever changed,
            // this would silently drop a real child instead of failing.
            let children = inside
                .iter()
                .filter(|d| d.offset != u.start)
                .map(|d| wire::BookSymbol {
                    name: d.name.clone(),
                    kind: symbol_kind(&d.head),
                    unit: None,
                    line_start: d.line,
                    line_end: d.line,
                    children: Vec::new(),
                })
                .collect();
            out.push(wire::BookSymbol {
                name,
                kind,
                unit: Some(i),
                line_start: u.line_start,
                line_end: u.line_end,
                children,
            });
        }
        self.out.reply(id, to_value(wire::SymbolsReply { symbols: out }));
    }

    /// Names the user could be typing.
    ///
    /// Three sources, and they are available at different times, which is the
    /// shape of this feature rather than an accident:
    ///
    /// - **The buffer**, always. The definer scan over the snapshot costs
    ///   nothing, needs no session, and covers exactly the names being
    ///   written right now -- including forms below the frontier, which the
    ///   world has never heard of.
    /// - **The world**, once the census has run. Everything an included book
    ///   defines, generated names included, which no scan of any text could
    ///   supply.
    /// - **Keyword families**, when the token starts with `:`. Static, so
    ///   they need neither. Gated on the colon because offering `:rewrite`
    ///   while someone types a function name is noise.
    ///
    /// Filtering is server-side because a world holds tens of thousands of
    /// names and shipping them per keystroke is a megabyte per keystroke. The
    /// cap plus `incomplete` is LSP's own contract: the client re-asks as the
    /// prefix narrows rather than filtering a truncation and concluding a
    /// name does not exist.
    ///
    /// **Package prefixes come out right for free.** The census prints
    /// through ACL2's own printer with the session's `current-package` in
    /// force -- which, after a sync, is the file's own `in-package`. So a
    /// name is already written the way this file would have to write it.
    fn complete(&mut self, id: &Value, prefix: &str) {
        const CAP: usize = 500;
        let want = fold(prefix);
        let mut items: Vec<wire::CompletionItem> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        if prefix.starts_with(':') {
            for k in KEYWORDS {
                if fold(k).starts_with(&want) && seen.insert(fold(k)) {
                    items.push(wire::CompletionItem {
                        name: k.to_string(),
                        kind: "keyword",
                        book: None,
                    });
                }
            }
        }
        for s in acl2_session::definer_sites(&self.bytes) {
            if fold(&s.name).starts_with(&want) && seen.insert(fold(&s.name)) {
                items.push(wire::CompletionItem {
                    name: s.name,
                    kind: "other",
                    book: None,
                });
            }
        }
        let mut capped = false;
        if let Some(world) = &self.symbols {
            for e in world {
                if items.len() >= CAP {
                    capped = true;
                    break;
                }
                if fold(&e.name).starts_with(&want) && seen.insert(fold(&e.name)) {
                    items.push(wire::CompletionItem {
                        name: e.name.clone(),
                        kind: wire::kind_str(e.kind),
                        book: e.book.clone(),
                    });
                }
            }
        } else {
            // Ask for one on the next idle tick, and tell the client that
            // what it has is provisional so it comes back.
            self.census_wanted = true;
            capped = true;
        }
        items.sort_by_key(|i| fold(&i.name));
        self.out.reply(id, to_value(wire::CompleteReply { items, incomplete: capped }));
    }

    /// Remember what a unit introduced, from the envelope already in hand.
    ///
    /// Costs nothing: `event-data-fal` has already recorded each event's name
    /// and kind for this unit, and the envelope carries them. Measured, an
    /// `include-book` records none of its book's internal events, so this
    /// stays exactly what it should be -- the file's own names -- without
    /// needing a cap to keep a library out.
    fn note_unit(&mut self, u: &acl2_session::sync::UnitReport) {
        for e in &u.inner {
            let kind = match e.kind.as_deref().map(|k| k.to_ascii_lowercase()) {
                Some(k) if k.starts_with("defthm") || k.starts_with("defaxiom") => "theorem",
                Some(k) if k.starts_with("defmacro") => "macro",
                Some(k) if k.starts_with("defconst") => "constant",
                Some(k) if k.starts_with("defun") || k.starts_with("defines") => "function",
                _ => "other",
            };
            self.file_names.insert(fold(&e.name), (e.name.clone(), kind, u.index));
        }
    }

    /// This name's one-line summary, if the world documents it.
    ///
    /// Asking is what makes the index wanted; it is built on the next tick,
    /// so the first hover of a session has no summary and the rest do.
    fn doc_short(&mut self, name: &str) -> Option<String> {
        let Some(docs) = self.docs.as_ref() else {
            self.docs_wanted = true;
            return None;
        };
        docs.get(&fold(name)).cloned()
    }

    /// What this file's own units introduced, if this is one of them.
    ///
    /// The answer a hover gets while a proof is running. Partial by nature --
    /// formals and a guard need the world, and the world is busy -- but name,
    /// kind and "defined in this file" is most of what the tooltip says, and
    /// it is *true*, which beats an empty tooltip.
    fn known_locally(&self, name: &str) -> Option<wire::DescribeReply> {
        let (written, kind, _) = self.file_names.get(&fold(name))?;
        Some(wire::DescribeReply {
            available: true,
            found: true,
            name: written.clone(),
            kind: Some(kind),
            origin: Some("file"),
            formals: None,
            guard: None,
            statement: None,
            book: None,
            path: None,
            short: None,
        })
    }

    /// Build the census, if a completion has asked for one.
    ///
    /// On the idle tick, where seconds are free. `try_lock` because an idle
    /// tick must never wait on anything: if the book is busy, the next tick
    /// will do.
    fn census_tick(&mut self) {
        let deps = {
            let Ok(book) = self.book.try_lock() else { return };
            book.dep_count()
        };
        // Invalidate eagerly, rebuild lazily -- which is what keeps a sync
        // from turning into a rebuild storm. A book with ten `include-book`
        // forms moves this count ten times while it is being checked, and
        // rebuilding on each would be ten multi-second walks mid-sync,
        // strictly worse than never rebuilding at all. *Dropping* the table
        // is free, and the rebuild is then demand-driven: it happens on the
        // next completion, which is a moment the user chose.
        if self.symbols.is_some() && self.census_deps != deps {
            self.symbols = None;
            // Topics arrive with books, so the same signal retires them --
            // and re-arms the want, since something clearly cares.
            self.docs = None;
            self.docs_wanted = true;
        }
        // Demand-driven, exactly like the census below, and for a reason the
        // protocol test caught: built eagerly, this submits on the tick right
        // after the driver settles, so `status` reports the session busy when
        // nothing the *user* asked for is running. A session that never hovers
        // should pay nothing, and one that does pays once.
        if self.docs_wanted && self.docs.is_none() {
            let r = {
                let Ok(mut book) = self.book.try_lock() else { return };
                book.doc_index(DESCRIBE)
            };
            // A world with no xdoc answers `Ok(empty)` -- the probe cannot
            // even be read there -- and that is recorded as built, so it is
            // attempted once rather than on every tick. `Busy` leaves it
            // unbuilt for the next one.
            if let Ok(t) = r {
                self.docs = Some(t.into_iter().map(|t| (fold(&t.name), t.short)).collect());
                self.docs_wanted = false;
            }
        }
        if !self.census_wanted || self.symbols.is_some() {
            return;
        }
        let r = {
            let Ok(mut book) = self.book.try_lock() else { return };
            book.census(CENSUS)
        };
        if let Ok(c) = r
            && !c.is_empty()
        {
            self.symbols = Some(c);
            self.census_wanted = false;
            // Recorded from *after* the walk: the world it describes is the
            // one that existed when it finished, not when it started.
            self.census_deps = self.book.try_lock().ok().and_then(|b| b.dep_count());
        }
    }

    /// A name ACL2 itself defines: jump into the image's own source.
    ///
    /// It works for most of them -- `binary-append`
    /// really is a `defun` in `axioms.lisp`. Not for all: `symbolp` is a
    /// primitive, axiomatised rather than defined, and appears in no source
    /// file at all. Those honestly answer nothing, which is the point of
    /// separating them from "defined in this file" in the first place.
    ///
    /// The checkout comes from the session's own `books_dir`, so this is the
    /// ACL2 that built this world rather than whichever one is on the path.
    /// The index is built once and kept: ~13 MB of source is about a second,
    /// which is far too slow to repeat per jump and unnoticeable once.
    /// `doc.lisp` is skipped -- 8 MB of documentation topics, no definitions
    /// anyone wants to land in.
    fn builtin_definition(&mut self, id: &Value, name: &str) {
        if self.builtins.is_none() {
            self.builtins = Some(self.index_builtins());
        }
        let hit = self.builtins.as_ref().and_then(|m| m.get(&fold(name))).cloned();
        match hit {
            Some((path, line)) => self.out.reply(id, to_value(wire::DefinitionReply {
                available: true,
                found: true,
                unit: None,
                path: Some(path),
                line: Some(line),
                how: Some("builtin"),
            })),
            None => self.out.reply(id, to_value(wire::definition_missing())),
        }
    }

    /// Scan the ACL2 checkout's own sources once. See [`Driver::builtin_definition`].
    fn index_builtins(&self) -> HashMap<String, (String, usize)> {
        let mut out = HashMap::new();
        let Some(books) = self.book.lock().expect("book lock").books_dir().map(Path::to_path_buf)
        else {
            return out;
        };
        let Some(checkout) = books.parent() else { return out };
        let Ok(entries) = std::fs::read_dir(checkout) else { return out };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "lisp"))
            .filter(|p| p.file_name().is_some_and(|n| n != "doc.lisp"))
            .collect();
        files.sort();
        // A name can appear in more than one definer form. `binary-append` is
        // a plain `defun` in axioms.lisp *and* has an executable-counterpart
        // `(defun-*1* binary-append ...)` in interface-raw.lisp; the first is
        // what anyone ctrl-clicking wants. So a `*1*` site is only ever a
        // fallback -- it fills an empty slot and yields to a real definition,
        // whichever file order happens to present them in. It is still worth
        // keeping, because for a primitive like `symbolp`, axiomatised rather
        // than defined, the `*1*` form is the only source ACL2 has.
        let mut fallback: HashMap<String, (String, usize)> = HashMap::new();
        for f in files {
            let Ok(bytes) = std::fs::read(&f) else { continue };
            let shown = f.display().to_string();
            for s in acl2_session::definer_sites(&bytes) {
                let target = if s.head.contains("*1*") { &mut fallback } else { &mut out };
                // Otherwise first wins: ACL2 redefines a few names in later
                // files (raw-Lisp shadows, `verify-termination` reruns), and
                // the original is the one worth reading.
                target.entry(fold(&s.name)).or_insert((shown.clone(), s.line));
            }
        }
        for (k, v) in fallback {
            out.entry(k).or_insert(v);
        }
        out
    }

    /// The form in `text` that defines `name`, if its text names it.
    ///
    /// Tier 3. Tier 4 -- a generated name, absent from the
    /// text -- deliberately answers `None` for now: measured, decomposition
    /// without the census's anchoring window is only ~83% precise however it
    /// is guarded, and a jump that lands on the wrong form is worse than one
    /// that opens the file at the top.
    fn site_in(&self, text: &[u8], name: &str) -> Option<acl2_session::DefinerSite> {
        acl2_session::definer_sites(text).into_iter().find(|s| same_symbol(&s.name, name))
    }

    /// `describe`, through the cache, as `definition` needs it.
    ///
    /// `Err(())` means the session was busy and nothing was cached -- the
    /// same decline `describe` itself gives, propagated rather than turned
    /// into "no such name".
    fn cached_detail(&mut self, name: &str) -> Result<Option<NameDetail>, ()> {
        if let Some(d) = self.details.get(name) {
            return Ok(d.clone());
        }
        let r = {
            let mut book = self.book.lock().expect("book lock");
            book.describe(name, DESCRIBE)
        };
        match r {
            Ok(d) => {
                self.details.insert(name.to_string(), d.clone());
                Ok(d)
            }
            Err(_) => Err(()),
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
                self.note_unit(u);
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
                    self.note_unit(&u);
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
        // The world moved, so cached answers about *volatile* names are
        // claims about a world that no longer exists: anything this file
        // introduced can have changed, and a "no such name" can have become
        // a name. Both go.
        //
        // What stays is what cannot have changed. ACL2's own definitions are
        // fixed for the life of the session, and an included book's can only
        // vanish if the include itself is rolled back -- which moves the
        // dependency count, and `census_tick` clears these on exactly that
        // signal. Keeping them is what lets a hover on a library name answer
        // from cache all the way through a long proof, instead of being
        // cleared by every unit that admits and then declined for being busy.
        self.details.retain(|_, d| {
            d.as_ref().is_some_and(|d| matches!(d.origin, Origin::Book | Origin::Builtin))
        });
        let (admitted, skipped) = {
            let book = self.book.lock().expect("book lock");
            (book.admitted_lines().len(), book.skipped_units())
        };
        // Names from units that are no longer admitted are no longer this
        // file's; the frontier is the authority.
        self.file_names.retain(|_, (_, _, unit)| *unit < admitted);
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
            line: None,
        });
    }

    /// Settle on a buffer that will not lex, saying where it stopped.
    ///
    /// The only settle with a position, and the position is worth having:
    /// `SplitError` reports an unterminated list at the *opening* paren
    /// rather than at end of file, so this is the first line the reader could
    /// not get past, not the last line of the file. Everything from there
    /// down is unreadable, which is what the client paints.
    ///
    /// The message loses its `line N:` prefix on the way out -- `Display`
    /// adds that for humans, and repeating it beside the field would only
    /// give the two ways to drift apart.
    fn settle_split_error(&self, e: &acl2_session::SplitError) {
        self.out.notify("settled", &wire::SettledEvent {
            book: self.id.clone(),
            revision: self.revision,
            reason: "split_error",
            message: Some(e.message.clone()),
            line: Some(e.line),
        });
    }
}
