#![forbid(unsafe_code)]
// This module is pure path logic; all FFI lives in pty.rs and session.rs.

//! Deciding which ACL2 image runs a book.
//!
//! Two facts drive this. A book under an ACL2 checkout was certified against
//! *that* checkout's image, so running it under another one is close to a
//! correctness error rather than a preference — and users routinely have many
//! checkouts, so pinning one image per server means constant reconfiguration.
//!
//! But locating an image by walking up the tree means **executing a file
//! chosen by the directory being explored**. A hostile repository need only
//! ship an empty `acl2.lisp` beside a malicious `saved_acl2` to have it run.
//! For an agent-driven tool that is not hypothetical: the agent picks the path,
//! possibly from repository content it just read.
//!
//! So the rule is that the user declares which directories hold executables
//! they vouch for, and resolution *selects among* those. It never introduces an
//! image the user has not already accepted. That applies to explicitly
//! requested images too, or an explicit argument would walk straight through
//! the boundary it exists to enforce.
//!
//! This is mechanism, not configuration: an interface supplies the values from
//! wherever its config lives, and the rule itself has one implementation.

use std::path::{Path, PathBuf};

/// The file marking the root of an ACL2 checkout.
///
/// The source file, not the built image, so an unbuilt checkout is still
/// recognised and can be reported as unbuilt — rather than being skipped, which
/// would let an outer checkout silently supply the wrong image.
pub const CHECKOUT_MARKER: &str = "acl2.lisp";

/// The image inside a checkout.
pub const IMAGE_NAME: &str = "saved_acl2";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    /// Nothing to run: no image was requested, none was derived, and no
    /// default was configured.
    NoImage { book: PathBuf },
    /// An image was found or requested, but is not one the user vouched for.
    NotTrusted { image: PathBuf },
    /// The book's checkout has not been built.
    ///
    /// Reported rather than silently falling back, because the fallback would
    /// check the book against a world it was not certified against.
    Unbuilt { checkout: PathBuf },
    /// A requested image does not exist.
    Missing { image: PathBuf },
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageError::NoImage { book } => write!(
                f,
                "no ACL2 image for {}: it is not inside a trusted checkout, \
                 and no default image is configured",
                book.display()
            ),
            ImageError::NotTrusted { image } => write!(
                f,
                "{} is not a trusted ACL2 image; it is neither the configured \
                 default nor inside a trusted directory",
                image.display()
            ),
            ImageError::Unbuilt { checkout } => write!(
                f,
                "the checkout {} has no {IMAGE_NAME}; build it, or name an \
                 image explicitly",
                checkout.display()
            ),
            ImageError::Missing { image } => {
                write!(f, "no ACL2 image at {}", image.display())
            }
        }
    }
}

impl std::error::Error for ImageError {}

/// Which images may be run, and how one is chosen for a book.
#[derive(Debug, Clone, Default)]
pub struct ImagePolicy {
    trusted: Vec<PathBuf>,
    default: Option<PathBuf>,
}

impl ImagePolicy {
    pub fn new() -> ImagePolicy {
        ImagePolicy::default()
    }

    /// Vouch for the executables under a directory.
    ///
    /// One entry covers every checkout beneath it, including ones created
    /// later, which is what makes a tree of worktrees workable without
    /// listing each.
    pub fn trust(mut self, dir: impl Into<PathBuf>) -> Self {
        self.trusted.push(normalise(dir.into()));
        self
    }

    /// The image for books that resolve to nothing else.
    ///
    /// Trusted by virtue of being named in the configuration, which is the
    /// most explicit declaration available.
    pub fn default_image(mut self, image: impl Into<PathBuf>) -> Self {
        self.default = Some(normalise(image.into()));
        self
    }

    pub fn trusted_roots(&self) -> &[PathBuf] {
        &self.trusted
    }

    pub fn default_path(&self) -> Option<&Path> {
        self.default.as_deref()
    }

    /// Whether an image may be run.
    pub fn is_trusted(&self, image: &Path) -> bool {
        let image = normalise(image.to_path_buf());
        if self.default.as_deref() == Some(image.as_path()) {
            return true;
        }
        self.trusted.iter().any(|root| image.starts_with(root))
    }

    /// The image to run for `book`.
    ///
    /// Derived from the book's own checkout when that checkout is trusted,
    /// otherwise the configured default. A derived image is never used from an
    /// untrusted location, however plausible it looks.
    pub fn resolve(&self, book: &Path) -> Result<PathBuf, ImageError> {
        if let Some(checkout) = find_checkout(book)
            && self.is_trusted(&checkout)
        {
            let image = checkout.join(IMAGE_NAME);
            if image.is_file() {
                return Ok(image);
            }
            // Trusted but unbuilt. Falling back here would run the book
            // against a different checkout's world.
            return Err(ImageError::Unbuilt { checkout });
        }
        self.default
            .clone()
            .ok_or_else(|| ImageError::NoImage { book: book.to_path_buf() })
    }

    /// The image to run when one was named explicitly.
    ///
    /// Checked against the same allowlist: an explicit argument must not be a
    /// way around the boundary, since the caller may be acting on content it
    /// read from the repository it is exploring.
    pub fn resolve_explicit(&self, image: &Path) -> Result<PathBuf, ImageError> {
        let image = normalise(image.to_path_buf());
        if !self.is_trusted(&image) {
            return Err(ImageError::NotTrusted { image });
        }
        if !image.is_file() {
            return Err(ImageError::Missing { image });
        }
        Ok(image)
    }

    /// Built images under the trusted roots, plus the default.
    ///
    /// Lets a client show what may be asked for; without it a caller has no way
    /// to discover the permitted set.
    pub fn available(&self) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = Vec::new();
        if let Some(d) = &self.default
            && d.is_file()
        {
            found.push(d.clone());
        }
        for root in &self.trusted {
            collect_images(root, 0, &mut found);
        }
        found.sort();
        found.dedup();
        found
    }
}

/// The ACL2 checkout containing `path`, if any.
///
/// Stops at the first checkout found, so a checkout nested inside another —
/// a worktree under a parent tree, say — is not shadowed by the outer one.
pub fn find_checkout(path: &Path) -> Option<PathBuf> {
    let start = normalise(path.to_path_buf());
    let mut dir: Option<&Path> = if start.is_dir() { Some(&start) } else { start.parent() };
    while let Some(d) = dir {
        if d.join(CHECKOUT_MARKER).is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// Absolute where possible, so comparisons are between comparable things.
/// `canonicalize` fails on paths that do not exist yet, so fall back rather
/// than treating that as an error.
fn normalise(p: PathBuf) -> PathBuf {
    std::fs::canonicalize(&p).unwrap_or(p)
}

fn collect_images(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    // Checkouts sit near the top of a trusted tree; descending arbitrarily far
    // would walk entire book trees for nothing.
    if depth > 3 {
        return;
    }
    let image = dir.join(IMAGE_NAME);
    if dir.join(CHECKOUT_MARKER).is_file() && image.is_file() {
        out.push(image);
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_images(&p, depth + 1, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake checkout: the marker, and optionally a built image.
    fn checkout(root: &Path, name: &str, built: bool) -> PathBuf {
        let d = root.join(name);
        std::fs::create_dir_all(d.join("books/kestrel")).unwrap();
        std::fs::write(d.join(CHECKOUT_MARKER), "").unwrap();
        if built {
            std::fs::write(d.join(IMAGE_NAME), "#!/bin/sh\n").unwrap();
        }
        d
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("acl2-image-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(&d).unwrap()
    }

    #[test]
    fn a_book_gets_its_own_checkouts_image() {
        let root = scratch("own");
        let wt = checkout(&root, "wt-a", true);
        checkout(&root, "wt-b", true);
        let book = wt.join("books/kestrel/foo.lisp");
        std::fs::write(&book, "").unwrap();

        let p = ImagePolicy::new().trust(&root);
        assert_eq!(p.resolve(&book).unwrap(), wt.join(IMAGE_NAME),
                   "not some other checkout's image");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The attack this policy exists to prevent: a repository supplying its
    /// own "ACL2".
    #[test]
    fn an_untrusted_checkout_is_never_run() {
        let root = scratch("untrusted");
        let trusted = checkout(&root, "trusted", true);
        let hostile = checkout(&root, "hostile", true);
        let book = hostile.join("books/kestrel/foo.lisp");
        std::fs::write(&book, "").unwrap();

        // Only `trusted` is vouched for; `hostile` looks identical.
        let p = ImagePolicy::new()
            .trust(&trusted)
            .default_image(trusted.join(IMAGE_NAME));
        assert_eq!(p.resolve(&book).unwrap(), trusted.join(IMAGE_NAME),
                   "falls back to the declared image, never the hostile one");
        assert!(!p.is_trusted(&hostile.join(IMAGE_NAME)));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An explicit request must not bypass the allowlist.
    #[test]
    fn an_explicit_image_is_checked_too() {
        let root = scratch("explicit");
        let trusted = checkout(&root, "trusted", true);
        let hostile = checkout(&root, "hostile", true);

        let p = ImagePolicy::new().trust(&trusted);
        assert!(p.resolve_explicit(&trusted.join(IMAGE_NAME)).is_ok());
        assert_eq!(
            p.resolve_explicit(&hostile.join(IMAGE_NAME)),
            Err(ImageError::NotTrusted { image: hostile.join(IMAGE_NAME) }),
            "naming a path must not be a way around the boundary"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unbuilt_checkout_is_reported_not_papered_over() {
        let root = scratch("unbuilt");
        let wt = checkout(&root, "wt-unbuilt", false);
        let other = checkout(&root, "wt-built", true);
        let book = wt.join("books/kestrel/foo.lisp");
        std::fs::write(&book, "").unwrap();

        let p = ImagePolicy::new()
            .trust(&root)
            .default_image(other.join(IMAGE_NAME));
        assert_eq!(p.resolve(&book), Err(ImageError::Unbuilt { checkout: wt }),
                   "silently using another checkout would check the book \
                    against a world it was not certified against");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_book_outside_any_checkout_uses_the_default() {
        let root = scratch("outside");
        let wt = checkout(&root, "wt", true);
        let elsewhere = root.join("my-project");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let book = elsewhere.join("thing.lisp");
        std::fs::write(&book, "").unwrap();

        let p = ImagePolicy::new().trust(&root).default_image(wt.join(IMAGE_NAME));
        assert_eq!(p.resolve(&book).unwrap(), wt.join(IMAGE_NAME));

        // With no default there is nothing legitimate to run.
        let bare = ImagePolicy::new().trust(&root);
        assert!(matches!(bare.resolve(&book), Err(ImageError::NoImage { .. })));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_innermost_checkout_wins() {
        let root = scratch("nested");
        let outer = checkout(&root, "outer", true);
        let inner = checkout(&outer, "inner", true);
        let book = inner.join("books/kestrel/foo.lisp");
        std::fs::write(&book, "").unwrap();

        let p = ImagePolicy::new().trust(&root);
        assert_eq!(p.resolve(&book).unwrap(), inner.join(IMAGE_NAME),
                   "an outer checkout must not shadow the one being worked in");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn available_lists_what_may_be_asked_for() {
        let root = scratch("avail");
        let a = checkout(&root, "wt-a", true);
        let b = checkout(&root, "wt-b", true);
        checkout(&root, "wt-unbuilt", false);

        let p = ImagePolicy::new().trust(&root);
        let mut want = vec![a.join(IMAGE_NAME), b.join(IMAGE_NAME)];
        want.sort();
        assert_eq!(p.available(), want, "unbuilt checkouts offer nothing to run");
        let _ = std::fs::remove_dir_all(&root);
    }
}
