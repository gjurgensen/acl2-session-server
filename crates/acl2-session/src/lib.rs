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

pub mod cert;
pub mod image;
pub mod output;
pub mod pty;
pub mod registry;
pub mod session;
pub mod split;
pub mod sync;

pub use session::{Outcome, Session, SessionControl, StatusSnapshot, Submission};
pub use split::{split, split_str, Form, SplitError};
pub use registry::{Registry, RegistryError, SessionId};
pub use cert::{Build, CertError};
pub use image::{find_checkout, ImageError, ImagePolicy};
pub use output::{OutputRef, OutputSlice, OutputStore, Slice};
pub use session::Progress;
pub use sync::{
    Book, Budget, Checkpoint, EvalFailure, EvalResult, Health, InnerEvent,
    InterruptEnvelope, InterruptReport, Resumed, Status, SyncError, SyncMode,
    SyncOutcome, WorldChange,
};
