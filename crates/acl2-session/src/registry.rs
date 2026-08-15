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
use crate::output::OutputStore;
use crate::session::SessionControl;
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
/// Within one session the operations serialise on this lock, and a blocking
/// call holds it for up to its whole deadline. That is why the entry also
/// carries the session's control block: an interrupt *asks through the block
/// first* (`request_abort`), which wakes the blocked call off its condvar at
/// once, and only then does the interrupt take the lock to escalate -- so
/// stopping a proof you regret costs a wakeup, not the proof's deadline.
/// Status reads the session's output stream and needs no lock at all.
pub type SharedBook = Arc<Mutex<Book>>;

/// Everything the registry records about one session. `path` and `image` are
/// fixed at open, so listing sessions never has to take any book's lock --
/// `acl2_sessions` used to hang behind whichever session had a sync in
/// flight.
struct Entry {
    book: SharedBook,
    /// A session keeps its image for life: the world was built by that image,
    /// so changing it would mean discarding and re-proving everything, which
    /// is what closing and reopening already does.
    image: PathBuf,
    path: PathBuf,
    control: Arc<SessionControl>,
    /// Shared for the same reason as `control`: reading a captured log must
    /// not wait behind whatever call holds the book.
    outputs: Arc<Mutex<OutputStore>>,
}

/// Several open books, each with its own ACL2 process.
pub struct Registry {
    policy: ImagePolicy,
    next: u64,
    entries: HashMap<SessionId, Entry>,
}

impl Registry {
    pub fn new(policy: ImagePolicy) -> Registry {
        Registry { policy, next: 0, entries: HashMap::new() }
    }

    pub fn policy(&self) -> &ImagePolicy {
        &self.policy
    }

    /// Which image the policy assigns this book.
    ///
    /// `image` names one explicitly; omitted, it is derived from the book's
    /// own checkout. Either way it must be one the policy permits.
    ///
    /// Split from [`register`](Self::register) so a caller holding the
    /// registry behind a lock can resolve, release, start ACL2 unlocked, and
    /// lock again only to insert. Starting a session takes up to the
    /// two-minute customization handshake, and holding the registry across it
    /// froze every other session's lookups for the duration.
    pub fn resolve(&self, file: &Path, image: Option<&Path>) -> Result<PathBuf, RegistryError> {
        Ok(match image {
            Some(i) => self.policy.resolve_explicit(i)?,
            None => self.policy.resolve(file)?,
        })
    }

    /// Adopt an already-started book under a fresh id.
    pub fn register(&mut self, book: Book, image: PathBuf) -> SessionId {
        self.next += 1;
        let id = SessionId(format!("s{}", self.next));
        self.entries.insert(
            id.clone(),
            Entry {
                path: book.path().to_path_buf(),
                control: book.control(),
                outputs: book.output_store(),
                book: Arc::new(Mutex::new(book)),
                image,
            },
        );
        id
    }

    /// Open a book and start a session for it: resolve, start, register.
    ///
    /// The convenience form for callers that do not share the registry across
    /// threads; it holds `&mut self` across the ACL2 startup.
    pub fn open(
        &mut self,
        file: &Path,
        image: Option<&Path>,
    ) -> Result<(SessionId, PathBuf), RegistryError> {
        let acl2 = self.resolve(file, image)?;
        let book = Book::open(&acl2, file)?;
        Ok((self.register(book, acl2.clone()), acl2))
    }

    /// The image a session is running.
    pub fn image_of(&self, id: &SessionId) -> Option<&Path> {
        self.entries.get(id).map(|e| e.image.as_path())
    }

    /// The control block for reaching a session without its lock.
    pub fn control_of(&self, id: &SessionId) -> Option<Arc<SessionControl>> {
        self.entries.get(id).map(|e| e.control.clone())
    }

    /// The output store for reading a session's captured logs without its
    /// lock.
    pub fn outputs_of(&self, id: &SessionId) -> Option<Arc<Mutex<OutputStore>>> {
        self.entries.get(id).map(|e| e.outputs.clone())
    }

    /// Close a session, killing its ACL2 and discarding its captured output.
    ///
    /// Returns whether there was anything to close, so a double close is a
    /// reported fact rather than an error.
    ///
    /// The abort request matters: if a call is blocked on the session, the
    /// process itself survives inside that call's `Arc` until the call
    /// returns. Without the request that meant "until its deadline expires" --
    /// an unreachable ACL2 grinding on for ten minutes after its session was
    /// closed. With it, the call wakes and returns at once, and the drop
    /// kills the child.
    pub fn close(&mut self, id: &SessionId) -> bool {
        match self.entries.remove(id) {
            Some(e) => {
                e.control.request_abort();
                true
            }
            None => false,
        }
    }

    /// The book for `id`, if it is open.
    ///
    /// Returns the shared handle rather than a borrow, so the registry lock can
    /// be released before the caller does any blocking work with the book.
    pub fn get(&self, id: &SessionId) -> Result<SharedBook, RegistryError> {
        self.entries
            .get(id)
            .map(|e| e.book.clone())
            .ok_or_else(|| RegistryError::Unknown(id.clone()))
    }

    /// Open sessions, with the file and image of each, in id order.
    /// Locks nothing: both facts are fixed at open.
    pub fn list(&self) -> Vec<(SessionId, PathBuf, PathBuf)> {
        let mut v: Vec<_> = self
            .entries
            .iter()
            .map(|(id, e)| (id.clone(), e.path.clone(), e.image.clone()))
            .collect();
        // Ids are "s<n>"; sorted as strings, s10 lands before s2.
        v.sort_by_key(|(id, ..)| {
            id.as_str()
                .strip_prefix('s')
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(u64::MAX)
        });
        v
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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
