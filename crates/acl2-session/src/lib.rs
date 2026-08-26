//! A file-anchored, structured interface to a live ACL2 session.
//!
//! A session keeps an ACL2 process in agreement with a `.lisp` file: its world
//! is the result of admitting file forms `1..N` in a plain interactive session
//! started in the book's own directory. Editing the file and asking for a sync
//! is the whole interface; the prefix that did not change is reused. The
//! snapshot can equally be an editor's unsaved buffer (`Book::sync_bytes`);
//! the path then anchors only what content does not decide.
//!
//! Results are read out of ACL2's own state -- `get-event-data` and
//! `gag-state` -- rather than scraped from its prose, and the complete log is
//! kept behind a handle for the things no field can carry.
//!
//! Transport-neutral on purpose: an editor is a plausible second client, so
//! nothing here knows about MCP.
//!
//! # The four principles
//!
//! Comments through this crate cite these by number -- "per P3", "this is P2
//! applied to startup" -- because they are the reasons behind the hardest
//! code, and repeating a reason at every site it governs would bury it. They
//! are stated once, here, so a citation always has something to point at.
//!
//! **P1. Structure, don't summarize. Project, don't decide.** Everything
//! returned is a lossless projection of ACL2's own state, and the complete
//! transcript stays retrievable behind a handle. Nothing is withheld; things
//! are *deferred*. So a reply that omits a 6 MB term still says how long it
//! was and where to read it, and a surface that cannot answer
//! says so rather than guessing -- `describe` returning "ask me again" is not
//! the same answer as "no such name", and the two must never render alike.
//!
//! **P2. Read state, don't scrape text.** ACL2's printed output is a
//! *rendering* of structured state; read the state. Control flow never turns
//! on ACL2's prose. That is why submissions are bracketed by sentinels we
//! chose rather than by watching for a prompt -- ACL2 prints nested prompts
//! while an `ld` runs, so a prompt scan stops partway through startup and
//! every later form reports the previous one's tail (`session.rs`). Facts come
//! from `get-event-data` and `gag-state` through probes, and the probes print
//! under keys we picked.
//!
//! One sanctioned exception, and it is narrow. Progress on a form still
//! *running* is read from the stream, because the structured state is locked
//! inside a busy process until it finishes.
//!
//! There was a second: a module that parsed the printed induction narrative,
//! because ACL2 computes the scheme inside a printing function and records it
//! nowhere. It is gone, and the two faults are worth keeping apart. Reading
//! prose was the *sanctioned* part -- there was no other source. What made it
//! unfixable is that an announcement cannot say which of a form's events it
//! belongs to, and a form is often many events each numbering its goals from
//! `*1`. Structured induction data in `get-event-data` is the replacement.
//!
//! **P3. Fast paths are verified against a recorded invariant; failure
//! degrades to replay.** Undo is a performance optimization, never a
//! correctness dependency. `ubu!` is fast where replaying two hundred forms
//! is minutes -- so rollback uses it, records
//! `max-absolute-command-number` before and checks it after, and rebuilds from
//! nothing if the check does not pass. A *failed* check does not mean the
//! world is wrong, only that it cannot be confirmed right, and those get the
//! same treatment: a wrong prefix is a silent failure that makes a client
//! report success about something not on disk.
//!
//! **P4. The file is the source of truth.** The session's world is a cache of
//! a prefix of a real file, never a parallel universe. The client never says
//! what changed; the diff is re-derived from content hashes, so a reindent
//! costs nothing and an edit above the frontier rolls back to it. A book whose
//! own source changed under the session invalidates the form that included it,
//! for the same reason. ([`Book::sync_bytes`](sync::Book::sync_bytes) lets the
//! snapshot be an editor's unsaved buffer; the path then anchors only what
//! content does not decide.)
//!
//! # Two invariants worth knowing before reading `sync`
//!
//! **The prefix invariant.** The world is always the result of admitting file
//! forms `1..N` in order, in a plain session started in the book's directory,
//! for some `N`. Not "some set of forms that happen to be admitted" -- a
//! prefix, and one a person could reproduce by hand. Every operation preserves
//! it or rebuilds until it holds. It is what makes a frontier meaningful and
//! what lets a rollback be a number.
//!
//! **One budget per call.** A deadline bounds the *call*, not each submission
//! inside it: units, rollbacks and probes all draw down one
//! [`Budget`]. Otherwise a call with twenty submissions could
//! take twenty deadlines while reporting that it honoured one. Session startup
//! during a rebuild sits outside it, under its own fixed deadlines -- a hung
//! customization file was never this call's proof work. Running out is not a
//! failure: the session then holds a valid shorter prefix plus one form in
//! flight, which is the ordinary shape of an expiry, and `resume` finishes it.

pub mod capability;
pub mod cert;
pub mod image;
pub mod output;
pub mod pretty;
pub mod pty;
pub mod registry;
pub mod session;
mod locate;
pub mod split;
pub mod sync;

pub use session::{Outcome, Session, SessionControl, StatusSnapshot, Submission};
pub use locate::{locate, How, Located};
pub use split::{
    decode, definer_sites, head_of, include_sites, split, split_str, DefinerSite, Form,
    IncludeSite, SplitError,
};
pub use registry::{Registry, RegistryError, SessionId};
pub use capability::{Capabilities, Capability};
pub use cert::{Build, CertError, CertReport, CertStatus};
pub use image::{find_checkout, ImageError, ImagePolicy};
pub use output::{OutputRef, OutputSlice, OutputStore, Slice, SliceRequest};
pub use session::Progress;
pub use sync::{
    Book, Budget, CensusEntry, Checkpoint, DocTopic, EvalFailure, EvalResult, Health, InnerEvent,
    InterruptEnvelope, InterruptReport, NameDetail, NameKind, NameRules, Origin, QueryResult,
    Resumed, RuneDetail, StaleInclude, Status, SyncError, SyncMode, SyncOptions, SyncOutcome,
    WorldChange,
};
