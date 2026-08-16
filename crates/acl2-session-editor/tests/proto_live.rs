//! Drive the built daemon over its real stdio protocol against a real ACL2 --
//! the exact surface an editor client will sit on.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

fn acl2() -> Option<String> {
    std::env::var("ACL2").ok()
}

struct Daemon {
    child: Child,
    r: BufReader<ChildStdout>,
    w: Option<ChildStdin>,
    next_id: u64,
    /// Notifications that arrived while waiting for a response.
    pending: Vec<(String, Value)>,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // A panicking test must not strand the daemon and its ACL2: kill
        // whatever is still there and reap it. After a graceful shutdown the
        // child is already gone and this is a no-op.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn start(acl2: &str) -> Daemon {
        let mut child = Command::new(env!("CARGO_BIN_EXE_acl2-session-editor"))
            .arg("--default")
            .arg(acl2)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn daemon");
        let r = BufReader::new(child.stdout.take().expect("stdout"));
        let w = child.stdin.take().expect("stdin");
        Daemon { child, r, w: Some(w), next_id: 0, pending: Vec::new() }
    }

    fn frame(&mut self, v: Value) {
        let s = v.to_string();
        let w = self.w.as_mut().expect("stdin");
        write!(w, "Content-Length: {}\r\n\r\n{s}", s.len()).expect("write frame");
        w.flush().expect("flush");
    }

    fn read_frame(&mut self) -> Value {
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            assert!(
                self.r.read_line(&mut line).expect("read header") > 0,
                "daemon closed its stdout"
            );
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("Content-Length:") {
                len = v.trim().parse().expect("length");
            }
        }
        let mut buf = vec![0u8; len];
        self.r.read_exact(&mut buf).expect("read body");
        serde_json::from_slice(&buf).expect("parse frame")
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.frame(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let v = self.read_frame();
            if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                assert!(v.get("error").is_none(), "request {method} failed: {v}");
                return v["result"].clone();
            }
            if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
                self.pending.push((m.to_string(), v["params"].clone()));
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.frame(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// The next notification, in arrival order.
    fn next_event(&mut self) -> (String, Value) {
        if !self.pending.is_empty() {
            return self.pending.remove(0);
        }
        loop {
            let v = self.read_frame();
            if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
                return (m.to_string(), v["params"].clone());
            }
        }
    }

    /// Everything through the next `settled`, inclusive.
    fn events_until_settled(&mut self) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        loop {
            let e = self.next_event();
            let done = e.0 == "settled";
            out.push(e);
            if done {
                return out;
            }
        }
    }

    fn shutdown(mut self) {
        drop(self.w.take()); // EOF ends the protocol loop; drivers join
        for _ in 0..50 {
            if self.child.try_wait().expect("wait").is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // The Drop guard kills whatever would not exit.
    }
}

fn last_state(events: &[(String, Value)]) -> Value {
    events
        .iter()
        .rev()
        .find(|(m, _)| m == "state")
        .map(|(_, p)| p.clone())
        .expect("a state event")
}

fn statuses(state: &Value) -> Vec<String> {
    state["units"]
        .as_array()
        .expect("units array")
        .iter()
        .map(|u| u["status"].as_str().expect("status").to_string())
        .collect()
}

#[test]
#[ignore = "needs ACL2"]
fn the_daemon_triages_a_book_and_holds_a_retraction() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("demo.lisp");
    let text = "(in-package \"ACL2\")\n\n\
                (defun alpha (x) (if (consp x) (alpha (cdr x)) nil))\n\n\
                (defthm ed-bad (equal (alpha x) (cons 1 x)))\n\n\
                (defthm ed-below (equal (alpha (cons a b)) (cons 1 (cons a b))))\n";
    std::fs::write(&book_path, text).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();
    assert!(!opened["image"].as_str().unwrap_or("").is_empty());

    // One update; the whole triage arrives as an event stream.
    d.notify("update", json!({ "book": book, "text": text, "revision": 7 }));
    let events = d.events_until_settled();
    let settled = &events.last().expect("settled").1;
    assert_eq!(settled["reason"], "reached_target", "events: {events:?}");
    assert_eq!(settled["revision"], 7);

    let outcome_of = |ix: u64| -> Option<String> {
        events
            .iter()
            .filter(|(m, _)| m == "unit")
            .find(|(_, p)| p["index"] == json!(ix))
            .map(|(_, p)| p["outcome"].as_str().expect("outcome").to_string())
    };
    assert_eq!(outcome_of(0).as_deref(), Some("admitted"));
    assert_eq!(outcome_of(2).as_deref(), Some("skipped"));
    assert_eq!(outcome_of(3).as_deref(), Some("admitted"));

    let state = last_state(&events);
    assert_eq!(state["conditional_from"], 2);
    assert_eq!(statuses(&state), ["admitted", "admitted", "skipped", "conditional"]);

    // The red unit's event carries the envelope and a usable log handle.
    let skipped = events
        .iter()
        .find(|(m, p)| m == "unit" && p["outcome"] == "skipped")
        .map(|(_, p)| p.clone())
        .expect("skipped unit event");
    assert_eq!(skipped["status"], "proof_failure");
    let out = d.request(
        "output",
        json!({
            "book": book,
            "output_ref": skipped["output_ref"],
            "mode": "tail",
            "lines": 80,
        }),
    );
    assert!(out["total_lines"].as_u64().expect("total_lines") > 0);
    assert!(out["text"].as_str().expect("text").to_lowercase().contains("ed-bad"));

    // Idle status answers between runs.
    let st = d.request("status", json!({ "book": book }));
    assert!(st["running"].is_null());

    // Retract to the defun; the held target keeps the rest pending.
    d.notify("retract", json!({ "book": book, "keep": 2 }));
    let more = d.events_until_settled();
    assert_eq!(more.last().expect("settled").1["reason"], "reached_target");
    let state = last_state(&more);
    assert_eq!(statuses(&state), ["admitted", "admitted", "pending", "pending"]);
    assert!(state["conditional_from"].is_null());

    // Back to the whole book, fully settled again.
    d.notify("update", json!({ "book": book, "text": text, "revision": 8 }));
    let full = d.events_until_settled();
    assert_eq!(
        statuses(&last_state(&full)),
        ["admitted", "admitted", "skipped", "conditional"]
    );

    // An edit to a fully-settled book must roll back and re-check -- not
    // reconcile the suffix to pending and park there (the stale-frontier
    // settle bug: "done" decided before the diff had run).
    let text2 = text.replace("ed-below", "ed-below2");
    d.notify("update", json!({ "book": book, "text": text2, "revision": 9 }));
    let after = d.events_until_settled();
    assert_eq!(after.last().expect("settled").1["revision"], 9);
    assert_eq!(
        statuses(&last_state(&after)),
        ["admitted", "admitted", "skipped", "conditional"],
        "the edited suffix re-checks rather than parking gray"
    );

    let closed = d.request("close", json!({ "book": book }));
    assert_eq!(closed["closed"], true);
    d.shutdown();

    let _ = std::fs::remove_dir_all(&dir);
}

/// Go to definition, across a file boundary.
///
/// The case the feature exists for: a name that is *not* in the buffer. The
/// world says which book defines it -- `pe`'s own include-book-path -- and
/// the definer scan finds the form inside that book's text. Nothing here is
/// guessed from the buffer, and nothing needs the census: the name is written
/// in its book, so it is tier 3, exact.
///
/// The in-file half is checked too, and deliberately for a form the session
/// has *not* admitted: the scan runs over the snapshot, so a definition below
/// the frontier is still found. That is the difference between locating a name
/// in the text and asking the world where it is.
#[test]
#[ignore = "needs ACL2"]
fn definition_crosses_into_an_included_book() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-def-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("d.lisp");
    let text = "(in-package \"ACL2\")\n\n\
                (include-book \"std/util/bstar\" :dir :system)\n\n\
                (defun alpha (x) (if (consp x) (alpha (cdr x)) nil))\n\n\
                (defun beta (x) (b* ((y (alpha x))) y))\n";
    std::fs::write(&book_path, text).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();

    // Hold the target before the last form, so `beta` is never admitted.
    d.notify("update", json!({ "book": book, "text": text, "target_line": 5, "revision": 1 }));
    let events = d.events_until_settled();
    assert_eq!(events.last().expect("settled").1["reason"], "reached_target");

    // In this file, and admitted.
    let a = d.request("definition", json!({ "book": book, "name": "alpha" }));
    assert_eq!(a["found"], true, "alpha is defined here: {a}");
    assert!(a["path"].is_null(), "a name defined here answers with no path");
    assert_eq!(a["unit"], 2, "the unit that defines it");
    assert_eq!(a["line"], 5, "and its line");
    assert_eq!(a["how"], "definer");

    // In this file, and *not* admitted -- found because the scan reads the
    // snapshot, not the world.
    let b = d.request("definition", json!({ "book": book, "name": "beta" }));
    assert_eq!(b["found"], true, "a form below the frontier is still located: {b}");
    assert_eq!(b["line"], 7);

    // The cross-file jump: bstar's own file, at the line that defines `b*`.
    let m = d.request("definition", json!({ "book": book, "name": "b*" }));
    assert_eq!(m["found"], true, "b* comes from an included book: {m}");
    let path = m["path"].as_str().expect("a path into the included book");
    assert!(path.ends_with("bstar.lisp"), "into bstar, got {path}");
    assert!(std::path::Path::new(path).is_file(), "which exists: {path}");
    let line = m["line"].as_u64().expect("and a line") as usize;
    let src = std::fs::read_to_string(path).expect("read bstar");
    let at = src.lines().nth(line - 1).expect("that line exists");
    assert!(
        at.contains("b*"),
        "line {line} of bstar.lisp should define b*, but reads {at:?}"
    );

    let closed = d.request("close", json!({ "book": book }));
    assert_eq!(closed["closed"], true);
    d.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
