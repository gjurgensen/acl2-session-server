#![forbid(unsafe_code)]
// This module is pure bookkeeping; all FFI lives in pty.rs and session.rs.

//! Holding several sessions at once, addressed by id.
//!
//! This lives in the core rather than in an interface crate because managing
//! several sessions is about sessions, not about a transport: an editor client
//! should get it without reimplementing it, and without depending on the MCP
//! SDK to obtain it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::sync::{Book, SyncError};

/// An opaque handle to one open session.
///
/// Clients never construct one; every id is returned by `open`. Ids are not
/// reused within a registry, so a stale id from a closed session reports
/// "unknown" rather than silently addressing somebody else's book — the same
/// reasoning as output handles.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(String);

impl SessionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        SessionId(s.to_string())
    }
}

#[derive(Debug)]
pub enum RegistryError {
    Unknown(SessionId),
    Sync(SyncError),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::Unknown(id) => {
                write!(f, "no session {id}; it was never opened, or has been closed")
            }
            RegistryError::Sync(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RegistryError {}

impl From<SyncError> for RegistryError {
    fn from(e: SyncError) -> Self {
        RegistryError::Sync(e)
    }
}

/// A book plus the lock guarding it.
///
/// Locks are **per session**, not one lock over the registry, because
/// `interrupt` has to work while a `sync` is in flight -- a single lock would
/// make the deadline model useless, since the call you need in order to stop a
/// runaway proof would be waiting behind the runaway proof.
///
/// Within one session the operations do serialise, but a deadline bounds how
/// long any of them holds the lock: a `sync` returns once its deadline expires
/// whether or not ACL2 has finished, so `interrupt` waits at most that long.
pub type SharedBook = Arc<Mutex<Book>>;

/// Several open books, each with its own ACL2 process.
pub struct Registry {
    acl2: PathBuf,
    next: u64,
    books: HashMap<SessionId, SharedBook>,
}

impl Registry {
    pub fn new(acl2: impl Into<PathBuf>) -> Registry {
        Registry { acl2: acl2.into(), next: 0, books: HashMap::new() }
    }

    /// Open a book and start a session for it.
    pub fn open(&mut self, file: &Path) -> Result<SessionId, RegistryError> {
        let book = Book::open(&self.acl2, file)?;
        self.next += 1;
        let id = SessionId(format!("s{}", self.next));
        self.books.insert(id.clone(), Arc::new(Mutex::new(book)));
        Ok(id)
    }

    /// Close a session, killing its ACL2 and discarding its captured output.
    ///
    /// Returns whether there was anything to close, so a double close is a
    /// reported fact rather than an error.
    pub fn close(&mut self, id: &SessionId) -> bool {
        self.books.remove(id).is_some()
    }

    /// The book for `id`, if it is open.
    ///
    /// Returns the shared handle rather than a borrow, so the registry lock can
    /// be released before the caller does any blocking work with the book.
    pub fn get(&self, id: &SessionId) -> Result<SharedBook, RegistryError> {
        self.books
            .get(id)
            .cloned()
            .ok_or_else(|| RegistryError::Unknown(id.clone()))
    }

    /// Open sessions and the file each is anchored to, in id order.
    pub fn list(&self) -> Vec<(SessionId, PathBuf)> {
        let mut v: Vec<_> = self
            .books
            .iter()
            .filter_map(|(id, b)| {
                let p = b.lock().ok()?.path().to_path_buf();
                Some((id.clone(), p))
            })
            .collect();
        v.sort();
        v
    }

    pub fn len(&self) -> usize {
        self.books.len()
    }

    pub fn is_empty(&self) -> bool {
        self.books.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_ids_are_rejected_rather_than_guessed() {
        let mut r = Registry::new("/nonexistent/acl2");
        let bogus = SessionId::from("s99");
        assert!(matches!(r.get(&bogus), Err(RegistryError::Unknown(_))));
        assert!(!r.close(&bogus), "closing an unknown id is reported, not an error");
        assert!(r.is_empty());
    }

    #[test]
    fn a_failed_open_leaves_no_session_behind() {
        let mut r = Registry::new("/nonexistent/acl2");
        assert!(r.open(Path::new("/tmp/nope.lisp")).is_err());
        assert_eq!(r.len(), 0, "a session that never started must not be registered");
    }
}
