//! A file-anchored, structured interface to a live ACL2 session.
//!
//! Status: walking skeleton. This is the thinnest end-to-end slice --
//! spawn a real ACL2 in a real book directory over a PTY, submit a real form,
//! detect completion, hand back what was captured. It exists to hold the
//! integration seams still while the rest is built on top.
//!
//! Not yet implemented: the file-prefix invariant and sync, structured result
//! extraction via `get-event-data` and `gag-state`, output handles, and the
//! MCP transport.

pub mod pty;
pub mod session;
pub mod split;
pub mod sync;

pub use session::{Outcome, Session, Submission};
pub use split::{split, split_str, Form, SplitError};
pub use sync::{Book, SyncError, SyncOutcome};
