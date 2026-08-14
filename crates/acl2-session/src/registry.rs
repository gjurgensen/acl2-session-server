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

use crate::image::{ImageError, ImagePolicy};
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
    Image(ImageError),
}

impl From<ImageError> for RegistryError {
    fn from(e: ImageError) -> Self {
        RegistryError::Image(e)
    }
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::Unknown(id) => {
                write!(f, "no session {id}; it was never opened, or has been closed")
            }
            RegistryError::Sync(e) => write!(f, "{e}"),
            RegistryError::Image(e) => write!(f, "{e}"),
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
    policy: ImagePolicy,
    next: u64,
    books: HashMap<SessionId, SharedBook>,
    /// The image each session is running, for reporting.
    ///
    /// A session keeps its image for life: the world was built by that image,
    /// so changing it would mean discarding and re-proving everything, which
    /// is what closing and reopening already does.
    images: HashMap<SessionId, PathBuf>,
}

impl Registry {
    pub fn new(policy: ImagePolicy) -> Registry {
        Registry {
            policy,
            next: 0,
            books: HashMap::new(),
            images: HashMap::new(),
        }
    }

    pub fn policy(&self) -> &ImagePolicy {
        &self.policy
    }

    /// Open a book and start a session for it.
    ///
    /// `image` names one explicitly; omitted, it is derived from the book's
    /// own checkout. Either way it must be one the policy permits.
    pub fn open(
        &mut self,
        file: &Path,
        image: Option<&Path>,
    ) -> Result<(SessionId, PathBuf), RegistryError> {
        let acl2 = match image {
            Some(i) => self.policy.resolve_explicit(i)?,
            None => self.policy.resolve(file)?,
        };
        let book = Book::open(&acl2, file)?;
        self.next += 1;
        let id = SessionId(format!("s{}", self.next));
        self.books.insert(id.clone(), Arc::new(Mutex::new(book)));
        self.images.insert(id.clone(), acl2.clone());
        Ok((id, acl2))
    }

    /// The image a session is running.
    pub fn image_of(&self, id: &SessionId) -> Option<&Path> {
        self.images.get(id).map(|p| p.as_path())
    }

    /// Close a session, killing its ACL2 and discarding its captured output.
    ///
    /// Returns whether there was anything to close, so a double close is a
    /// reported fact rather than an error.
    pub fn close(&mut self, id: &SessionId) -> bool {
        self.images.remove(id);
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

    /// Open sessions, with the file and image of each, in id order.
    pub fn list(&self) -> Vec<(SessionId, PathBuf, PathBuf)> {
        let mut v: Vec<_> = self
            .books
            .iter()
            .filter_map(|(id, b)| {
                let p = b.lock().ok()?.path().to_path_buf();
                let img = self.images.get(id).cloned().unwrap_or_default();
                Some((id.clone(), p, img))
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

    fn policy() -> ImagePolicy {
        ImagePolicy::new().default_image("/nonexistent/acl2")
    }

    #[test]
    fn unknown_ids_are_rejected_rather_than_guessed() {
        let mut r = Registry::new(policy());
        let bogus = SessionId::from("s99");
        assert!(matches!(r.get(&bogus), Err(RegistryError::Unknown(_))));
        assert!(!r.close(&bogus), "closing an unknown id is reported, not an error");
        assert!(r.is_empty());
    }

    #[test]
    fn a_failed_open_leaves_no_session_behind() {
        let mut r = Registry::new(policy());
        assert!(r.open(Path::new("/tmp/nope.lisp"), None).is_err());
        assert_eq!(r.len(), 0, "a session that never started must not be registered");
    }
}
