//! Stamp the binary with the commit it was built from.
//!
//! The version alone cannot answer the question that matters in the field --
//! "is the server I am talking to the one I just built?" -- because it stays
//! `0.0.1` across weeks of development. A client once spent several days
//! reporting fixed behaviour against a stale installed binary, and nothing in
//! any reply could have revealed it. The commit can, and it costs one short
//! string in the open reply.
//!
//! Absent git (a published tarball), the stamp is empty and the reply carries
//! the bare version, which is the honest answer there.

use std::process::Command;

fn main() {
    // Narrowed reruns: specifying any `rerun-if-changed` replaces cargo's
    // default of "any file in the package", so the sources are named too.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=../../.git/HEAD");

    let stamp = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|h| format!("+g{}", h.trim()))
        .unwrap_or_default();
    println!("cargo:rustc-env=ACL2_MCP_BUILD={stamp}");
}
