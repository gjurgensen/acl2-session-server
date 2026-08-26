//! Drive the built daemon over its real stdio protocol against a real ACL2 --
//! the exact surface an editor client will sit on.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

/// The ACL2 to drive, or `None` to skip. See `acl2-session/tests/live.rs`
/// for which version these suites expect.
fn acl2() -> Option<String> {
    std::env::var("ACL2").ok()
}

/// Read a file of ACL2 source, for the tests that check where a jump landed.
///
/// Never `read_to_string`. ACL2 source is ISO-8859-1: `axioms.lisp` credits
/// a `Mart\u{ed}n` whose byte is not valid UTF-8, and a `read_to_string`
/// fails on the whole file for it -- so a test that reads one is asserting
/// about the encoding as much as about the jump. `split::decode` is the
/// decoder the daemon reads these same files with.
fn acl2_source(path: &str) -> String {
    acl2_session::split::decode(&std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}")))
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
        // A panicking test must not strand the daemon and its ACL2. Close
        // stdin first: EOF ends the protocol loop, and only an orderly exit
        // lets the daemon reach `Session::drop` and kill the ACL2 it spawned.
        // `Child::kill` sends SIGKILL, which the daemon cannot catch, so
        // killing outright reaps the daemon and orphans its ACL2 grandchild.
        // After a graceful shutdown the child is already gone and the wait
        // below returns at once.
        drop(self.w.take());
        for _ in 0..50 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // Whatever would not exit on its own still has to go.
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

    /// Send a request and do not wait for -- or read -- the reply.
    fn send_only(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.frame(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }

    /// Drop our end of the daemon's stdin, which is its EOF.
    fn close_stdin(&mut self) {
        self.w = None;
    }

    /// Whether the daemon exits within `limit`.
    fn wait_for_exit(&mut self, limit: std::time::Duration) -> bool {
        let until = std::time::Instant::now() + limit;
        while std::time::Instant::now() < until {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        false
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

/// The one-name case, now that `rules` answers a list.
fn first_rule(d: &mut Daemon, book: &str, name: &str) -> Value {
    let batch = d.request("rules", json!({ "book": book, "names": [name] }));
    batch["rules"][0].clone()
}

/// Terms cross the wire already laid out for the pane the client reported.
///
/// The client used to do this itself, which meant every client would have to.
/// The width is the one part only the client can know -- it is pixels and a
/// font -- so it sends the number and the daemon does the work.
#[test]
#[ignore = "needs ACL2"]
fn terms_arrive_laid_out_for_the_width_the_client_asked_for() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-lay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("lay.lisp");
    // A checkpoint wide enough that the width has to show in it.
    let text = "(in-package \"ACL2\")\n\n\
                (defun lay-alpha (x) (if (consp x) (lay-alpha (cdr x)) nil))\n\n\
                (defthm lay-bad\n  (implies (and (true-listp xxxxxxxxxx) (consp yyyyyyyyyy))\n\
                \x20          (equal (lay-alpha xxxxxxxxxx) (cons yyyyyyyyyy xxxxxxxxxx))))\n";
    std::fs::write(&book_path, text).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();

    d.notify("layout", json!({ "cols": 30 }));
    d.notify("update", json!({ "book": book, "text": text, "revision": 1 }));
    let events = d.events_until_settled();
    let narrow = checkpoint_of(&events).expect("the failed form has a checkpoint");
    assert!(narrow.contains('\n'), "a term laid out at 30 columns must wrap: {narrow:?}");
    let widest = narrow.lines().map(str::len).max().unwrap_or(0);
    assert!(widest <= 30, "laid out to {widest} columns, not 30: {narrow}");

    // The same form again at a width that fits it, to show the number is
    // being used rather than some fixed margin.
    d.notify("layout", json!({ "cols": 200 }));
    d.notify("retract", json!({ "book": book, "keep": 2 }));
    let _ = d.events_until_settled();
    d.notify("update", json!({ "book": book, "text": text, "revision": 2 }));
    let events = d.events_until_settled();
    let wide = checkpoint_of(&events).expect("the failed form has a checkpoint");
    assert!(wide.lines().count() < narrow.lines().count(), "wide: {wide}\nnarrow: {narrow}");

    // Same term either way: only the whitespace moved.
    let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(flat(&wide), flat(&narrow));

    d.request("close", json!({ "book": book }));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A hover's terms are laid out for the hover, not for the form view.
///
/// Two things at once. The world stores terms translated, so a `defthm` whose
/// statement was written with `and` is held as `(if x y 'nil)` and used to be
/// shown that way. And a hover has a width but nothing can measure it -- the
/// extension API exposes no geometry for a tooltip -- so the client says what
/// it wants rather than inheriting the form view's, which is a different box.
#[test]
#[ignore = "needs ACL2"]
fn a_hover_lays_terms_out_at_the_width_it_asked_for() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-hov-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("hov.lisp");
    let text = "(in-package \"ACL2\")\n\n\
                (defun hv-f (x y)\n  (declare (xargs :guard (and (natp x) (natp y))))\n  \
                (let ((z (+ x y))) (* z z)))\n\n\
                (defthm hv-thm\n  (implies (and (natp xxxxxxxxxx) (natp yyyyyyyyyy))\n\
                \x20          (equal (hv-f xxxxxxxxxx yyyyyyyyyy)\n\
                \x20                 (* (+ xxxxxxxxxx yyyyyyyyyy) (+ xxxxxxxxxx yyyyyyyyyy)))))\n";
    std::fs::write(&book_path, text).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();
    // A wide form view, so inheriting its width would be visible.
    d.notify("layout", json!({ "cols": 200 }));
    d.notify("update", json!({ "book": book, "text": text, "revision": 1 }));
    let events = d.events_until_settled();
    assert_eq!(events.last().expect("settled").1["reason"], "reached_target");

    let g = d.request("describe", json!({ "book": book, "name": "hv-f", "cols": 40 }));
    let guard = g["guard"].as_str().expect("a guard");
    assert!(guard.contains("(and "), "the guard is untranslated: {guard}");
    assert!(!guard.contains("'nil"), "{guard}");

    let t = d.request("describe", json!({ "book": book, "name": "hv-thm", "cols": 40 }));
    let narrow = t["statement"].as_str().expect("a statement").to_string();
    assert!(narrow.contains("(and "), "the statement is untranslated: {narrow}");
    let widest = narrow.lines().map(str::len).max().unwrap_or(0);
    assert!(widest <= 40, "laid out to {widest} columns, not the 40 asked for: {narrow}");

    // The same name at a different width: the number is used, and it is the
    // hover's rather than the form view's 200.
    let t2 = d.request("describe", json!({ "book": book, "name": "hv-thm", "cols": 100 }));
    let wide = t2["statement"].as_str().expect("a statement").to_string();
    assert!(wide.lines().count() < narrow.lines().count(), "wide:\n{wide}\nnarrow:\n{narrow}");
    let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(flat(&wide), flat(&narrow));

    d.request("close", json!({ "book": book }));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A term already sent can be laid out again at a new width.
///
/// The gap `reflow` fills: checkpoints ride on `unit` events, which are pushed
/// once and never re-sent, so after a pane drag the client holds text laid out
/// for the width that is gone. It sends that text back rather than the
/// original, which it no longer has -- and that works because laying out is
/// idempotent across widths.
#[test]
#[ignore = "needs ACL2"]
fn a_term_can_be_laid_out_again_at_a_new_width() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-rfl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("rfl.lisp");
    let text = "(in-package \"ACL2\")\n\n\
                (defun rfl-alpha (x) (if (consp x) (rfl-alpha (cdr x)) nil))\n\n\
                (defthm rfl-bad\n  (implies (and (true-listp xxxxxxxxxx) (consp yyyyyyyyyy))\n\
                \x20          (equal (rfl-alpha xxxxxxxxxx) (cons yyyyyyyyyy xxxxxxxxxx))))\n";
    std::fs::write(&book_path, text).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();

    // A form lands at one width; the pane then becomes another.
    d.notify("layout", json!({ "cols": 30 }));
    d.notify("update", json!({ "book": book, "text": text, "revision": 1 }));
    let events = d.events_until_settled();
    let narrow = checkpoint_of(&events).expect("the failed form has a checkpoint");
    assert!(narrow.lines().count() > 1, "{narrow}");

    d.notify("layout", json!({ "cols": 200 }));
    let r = d.request("reflow", json!({ "terms": [narrow.clone()] }));
    let wide = r["terms"][0].as_str().expect("one term back").to_string();

    assert!(wide.lines().count() < narrow.lines().count(), "wide:\n{wide}\nnarrow:\n{narrow}");
    let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(flat(&wide), flat(&narrow), "re-flowing must not change the term");

    // And back again: the width is the only thing that decides the answer, so
    // the original width gives the original text.
    d.notify("layout", json!({ "cols": 30 }));
    let back = d.request("reflow", json!({ "terms": [wide] }));
    assert_eq!(back["terms"][0].as_str(), Some(narrow.as_str()));

    d.request("close", json!({ "book": book }));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The first checkpoint term of the first failed unit in an event stream.
fn checkpoint_of(events: &[(String, Value)]) -> Option<String> {
    events
        .iter()
        .filter(|(m, _)| m == "unit")
        .find_map(|(_, p)| p["checkpoints"].as_array()?.first()?["term"].as_str())
        .map(str::to_string)
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

    // Retract to the defun; the held target keeps the rest out. `beyond`
    // rather than `pending`: a retraction leaves a target behind it, and
    // these two forms are not queued for anything until it moves.
    d.notify("retract", json!({ "book": book, "keep": 2 }));
    let more = d.events_until_settled();
    assert_eq!(more.last().expect("settled").1["reason"], "reached_target");
    let state = last_state(&more);
    assert_eq!(statuses(&state), ["admitted", "admitted", "beyond", "beyond"]);
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
    let src = acl2_source(path);
    let at = src.lines().nth(line - 1).expect("that line exists");
    assert!(
        at.contains("b*"),
        "line {line} of bstar.lisp should define b*, but reads {at:?}"
    );

    // A built-in. `symbolp` reports no book, exactly like a name defined
    // here -- which is how hover came to claim it was defined in this file
    // while definition found nothing. They must now agree, and neither may
    // call it local.
    let sp = d.request("describe", json!({ "book": book, "name": "symbolp" }));
    assert_eq!(sp["found"], true, "symbolp is known to the world: {sp}");
    assert_eq!(sp["origin"], "builtin", "and is ACL2's, not this file's");
    assert!(sp["book"].is_null(), "a built-in reports no book, like a local name");
    let a_origin = d.request("describe", json!({ "book": book, "name": "alpha" }));
    assert_eq!(a_origin["origin"], "file", "while alpha really is local");
    let m_origin = d.request("describe", json!({ "book": book, "name": "b*" }));
    assert_eq!(m_origin["origin"], "book");

    // The doc summary rides along on describe, from the world's xdoc table.
    // It arrives as raw markup -- the machinery that would expand it is not
    // in the world -- so the client renders it.
    let mut summarised = None;
    for _ in 0..20 {
        let r = d.request("describe", json!({ "book": book, "name": "patbind-unless-casematch" }));
        if r["short"].is_string() {
            summarised = Some(r);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let r = summarised.expect("the doc index should arrive on a tick");
    let short = r["short"].as_str().expect("a summary");
    assert!(short.contains("binder"), "the topic summary: {short}");
    assert!(short.contains("@("), "and it is raw markup, unexpanded: {short}");

    // The full topic. Its line structure must survive the wire, because
    // xdoc code blocks depend on it -- and the markup contains `~x0`, which
    // fmt would have eaten as its own directive.
    let doc = d.request("doc", json!({ "book": book, "name": "patbind-unless-casematch" }));
    assert_eq!(doc["found"], true, "the topic is documented: {doc}");
    let long = doc["long"].as_str().expect("a body");
    assert!(long.contains("@({"), "with a code block");
    assert!(long.contains("~x0"), "whose fmt directives survive verbatim");
    assert!(long.lines().count() > 3, "and whose lines are preserved");
    assert!(doc["short"].is_string(), "and the summary rides along");

    let none = d.request("doc", json!({ "book": book, "name": "not-a-topic-at-all" }));
    assert_eq!(none["found"], false, "an undocumented name says so: {none}");

    // Most built-ins are ordinary defuns in ACL2's own source, and jumping
    // into them works. `symbolp` is a primitive -- axiomatised, defined in no
    // file -- so it honestly answers nothing rather than guessing.
    let ba = d.request("definition", json!({ "book": book, "name": "binary-append" }));
    assert_eq!(ba["found"], true, "binary-append is defined in ACL2's source: {ba}");
    assert_eq!(ba["how"], "builtin");
    let bap = ba["path"].as_str().expect("a path into the checkout");
    assert!(bap.ends_with("axioms.lisp"), "into axioms.lisp, got {bap}");
    let bal = ba["line"].as_u64().expect("and a line") as usize;
    let bsrc = acl2_source(bap);
    assert!(
        bsrc.lines().nth(bal - 1).is_some_and(|l| l.contains("binary-append")),
        "line {bal} of axioms.lisp should define binary-append"
    );

    // A primitive is axiomatised rather than defined, so ACL2's only source
    // form for it is the executable counterpart -- which is worth opening,
    // and is the reason `*1*` sites are kept as a fallback rather than
    // dropped. It must never outrank a real definition, which is why
    // binary-append lands in axioms.lisp above and not in interface-raw.
    let prim = d.request("definition", json!({ "book": book, "name": "symbolp" }));
    assert_eq!(prim["found"], true, "the *1* form is what a primitive has: {prim}");
    let pp = prim["path"].as_str().expect("a path");
    let pl = prim["line"].as_u64().expect("a line") as usize;
    let psrc = acl2_source(pp);
    assert!(
        psrc.lines().nth(pl - 1).is_some_and(|l| l.contains("symbolp")),
        "line {pl} of {pp} should mention symbolp"
    );

    // Links: every include-book, resolved to a file that exists. The one in
    // this book is `:dir :system`, so it exercises the books-directory path.
    let lk = d.request("links", json!({ "book": book }));
    let links = lk["links"].as_array().expect("a links array");
    assert_eq!(links.len(), 1, "one include-book in this file: {lk}");
    assert_eq!(links[0]["book"], "std/util/bstar");
    assert_eq!(links[0]["line"], 3, "on the line it is written");
    let lp = links[0]["path"].as_str().expect("resolved to a path");
    assert!(lp.ends_with("std/util/bstar.lisp"), "got {lp}");
    assert!(std::path::Path::new(lp).is_file(), "which exists: {lp}");

    // An include that cannot be resolved is reported without a path rather
    // than with a wrong one -- the client then draws no link at all.
    let text2 = text.replace(
        "(include-book \"std/util/bstar\" :dir :system)",
        "(include-book \"std/util/bstar\" :dir :system)\n\n\
         (local (include-book \"no/such/book\"))",
    );
    d.notify("update", json!({ "book": book, "text": text2, "target_line": 5, "revision": 2 }));
    let _ = d.events_until_settled();
    let lk2 = d.request("links", json!({ "book": book }));
    let links2 = lk2["links"].as_array().expect("array");
    assert_eq!(links2.len(), 2, "both includes are seen, wrapped or not: {lk2}");
    let missing = links2.iter().find(|l| l["book"] == "no/such/book").expect("the bad one");
    assert!(missing["path"].is_null(), "a book that is not there resolves to nothing");

    // Completion. The first call answers from the buffer alone and says so,
    // because the census runs on an idle tick rather than inline -- building
    // it in the request would stall an edit behind seconds of world walking.
    let c1 = d.request("complete", json!({ "book": book, "prefix": "alph" }));
    let names = |v: &Value| -> Vec<String> {
        v["items"].as_array().expect("items").iter()
            .map(|i| i["name"].as_str().expect("name").to_string()).collect()
    };
    assert!(names(&c1).contains(&"alpha".to_string()),
            "a name from the buffer completes at once: {c1}");
    assert_eq!(c1["incomplete"], true, "and the census is still to come");

    // Keywords are offered only behind a colon, so they do not crowd out
    // names while an ordinary symbol is being typed.
    let kw = d.request("complete", json!({ "book": book, "prefix": ":in-th" }));
    assert!(names(&kw).contains(&":in-theory".to_string()), "keyword families: {kw}");
    let noskw = d.request("complete", json!({ "book": book, "prefix": "in-th" }));
    assert!(!names(&noskw).iter().any(|n| n.starts_with(':')),
            "no keywords without the colon: {noskw}");

    // The census lands on an idle tick; after it, the world is offered too.
    let mut world = None;
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let c = d.request("complete", json!({ "book": book, "prefix": "binary-app" }));
        if names(&c).contains(&"binary-append".to_string()) {
            world = Some(c);
            break;
        }
    }
    let world = world.expect("the census should arrive on an idle tick");
    assert!(names(&world).contains(&"binary-append".to_string()),
            "a name only the world knows: {world}");

    // And a generated name from an included book -- in no source text
    // anywhere, which is exactly what no scan of any file could offer.
    let made = d.request("complete", json!({ "book": book, "prefix": "patbind-unless" }));
    assert!(names(&made).iter().any(|n| n == "patbind-unless-casematch"),
            "a generated name from an included book: {made}");

    // Tier 4. `patbind-unless-casematch` occurs *nowhere* in bstar.lisp -- it
    // is generated by `(def-b*-binder unless-casematch ...)`. Before the
    // census it answered with the file and no line; now the world order for
    // that book windows a decomposition of the symbol, and it lands on the
    // form that made it.
    let g = d.request("definition", json!({ "book": book, "name": "patbind-unless-casematch" }));
    assert_eq!(g["found"], true, "a generated name resolves: {g}");
    let gp = g["path"].as_str().expect("into bstar");
    assert!(gp.ends_with("bstar.lisp"), "got {gp}");
    let gl = g["line"].as_u64().expect("with a line, now that the census is up") as usize;
    let how = g["how"].as_str().unwrap_or("");
    assert!(how == "decomposed" || how == "anchored", "and says how it got there: {g}");
    let gsrc = acl2_source(gp);
    let at = gsrc.lines().nth(gl - 1).expect("that line exists");
    assert!(
        at.contains("unless-casematch"),
        "line {gl} should be the def-b*-binder that generates it, but reads {at:?}"
    );
    assert_eq!(how, "decomposed", "the symbol carries the source name, so exactly");

    // And the name is genuinely absent from the text, which is what makes
    // this tier necessary rather than a fallback nobody reaches.
    assert!(!gsrc.contains("patbind-unless-casematch"),
            "if this ever appears literally, tier 3 would have handled it");

    // Adding an include-book must invalidate the census: the world now rests
    // on a book whose names the table has never seen. Invalidation is eager
    // and the rebuild is demand-driven, so a sync admitting several includes
    // costs one walk on the next completion rather than one per include.
    let text3 = text.replace(
        "(include-book \"std/util/bstar\" :dir :system)",
        "(include-book \"std/util/bstar\" :dir :system)\n\n\
         (include-book \"std/lists/rev\" :dir :system)",
    );
    d.notify("update", json!({ "book": book, "text": text3, "revision": 3 }));
    let _ = d.events_until_settled();

    let mut fresh = None;
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let c = d.request("complete", json!({ "book": book, "prefix": "rev" }));
        if names(&c).iter().any(|n| n == "rev") {
            fresh = Some(c);
            break;
        }
    }
    let fresh = fresh.expect("a newly included book's names must reach completion");
    assert!(names(&fresh).iter().any(|n| n == "rev"),
            "the census rebuilt against the new include set: {fresh}");
    // And the old world is still there -- invalidation replaces, never loses.
    let still = d.request("complete", json!({ "book": book, "prefix": "binary-app" }));
    assert!(names(&still).contains(&"binary-append".to_string()),
            "the rebuilt census still holds what the old one did: {still}");

    let closed = d.request("close", json!({ "book": book }));
    assert_eq!(closed["closed"], true);
    d.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Hover keeps working while the session is busy.
///
/// The policy is that a query never queues behind the prover, and
/// the consequence used to be that hover went dead for the whole of a long
/// proof -- which is exactly when someone is reading rather than writing.
/// Two things fix that without waiting on anything: this file's own names are
/// recorded from the unit envelopes as they admit, and library names, being
/// unable to change while the include set holds, survive in the cache instead
/// of being cleared by every unit.
#[test]
#[ignore = "needs ACL2"]
fn hover_answers_while_the_session_is_busy() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-busy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("b.lisp");
    // The slow unit is a large `include-book`, not a slow proof: it reliably
    // outruns the step deadline and puts the driver into its resume loop,
    // where a hover has to be answered without the session.
    let text = "(in-package \"ACL2\")\n\n\
                (defun alpha (x) (if (consp x) (alpha (cdr x)) nil))\n\n\
                (include-book \"std/top\" :dir :system)\n";
    std::fs::write(&book_path, text).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();

    // Warm both kinds while the session is free. The builtin is what the
    // retention rule then has to preserve across the slow unit.
    d.notify("update", json!({ "book": book, "text": text, "target_line": 3, "revision": 1 }));
    let _ = d.events_until_settled();
    let warm = d.request("describe", json!({ "book": book, "name": "binary-append" }));
    assert_eq!(warm["origin"], "builtin", "warmed before the slow unit: {warm}");

    // Now let the include-book run, and ask while it is in flight.
    d.notify("update", json!({ "book": book, "text": text, "revision": 2 }));
    let mut asked_while_busy = false;
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let st = d.request("status", json!({ "book": book }));
        if st["running"].is_null() {
            continue;
        }
        asked_while_busy = true;

        // This file's own name: from the unit envelope, no session needed.
        let a = d.request("describe", json!({ "book": book, "name": "alpha" }));
        assert_eq!(a["available"], true, "a hover mid-unit is not declined: {a}");
        assert_eq!(a["found"], true, "and finds this file's own definition: {a}");
        assert_eq!(a["origin"], "file", "reported as local, not as a built-in: {a}");

        // A builtin: from a cache the admitting units no longer clear.
        let m = d.request("describe", json!({ "book": book, "name": "binary-append" }));
        assert_eq!(m["available"], true, "nor is a stable name declined: {m}");
        assert_eq!(m["found"], true, "it survived the units that admitted: {m}");
        assert_eq!(m["origin"], "builtin");
        break;
    }
    assert!(asked_while_busy, "the include-book should have been in flight to ask during");

    let closed = d.request("close", json!({ "book": book }));
    assert_eq!(closed["closed"], true);
    d.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// What a form introduced, answered while the session is busy proving.
///
/// The reason the scrape is taken eagerly: a session serialises on one ACL2,
/// so this question cannot reach the prover during a proof -- and reading
/// what an earlier form defined while a later one grinds is exactly when it
/// is asked. It is also the information a reader most lacks, being what the
/// author never wrote: the derived left-hand side, the trigger term, the
/// runes a `defun` quietly introduced.
#[test]
#[ignore = "needs ACL2"]
fn rules_answer_while_the_session_is_busy() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-rules-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("b.lisp");
    // As in the hover test, the slow unit is a large `include-book`: it
    // reliably outruns the step deadline and leaves the driver following a
    // form it cannot interrupt to answer a question.
    let text = "(in-package \"ACL2\")\n\n\
                (defun ru-app (x y) (if (endp x) y (cons (car x) (ru-app (cdr x) y))))\n\n\
                (defthm true-listp-of-ru-app\n\
                  (implies (true-listp y) (true-listp (ru-app x y))))\n\n\
                (defthm ru-fc (implies (consp x) (not (equal x nil)))\n\
                  :rule-classes ((:forward-chaining :trigger-terms ((consp x)))))\n\n\
                (include-book \"std/top\" :dir :system)\n";
    std::fs::write(&book_path, text).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();

    // Admit everything up to the slow include.
    d.notify("update", json!({ "book": book, "text": text, "target_line": 8, "revision": 1 }));
    let _ = d.events_until_settled();

    // Now let the include-book run, and ask while it is in flight.
    d.notify("update", json!({ "book": book, "text": text, "revision": 2 }));
    let mut asked_while_busy = false;
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let st = d.request("status", json!({ "book": book }));
        if st["running"].is_null() {
            continue;
        }
        asked_while_busy = true;

        // A function: its derived runes, the body deliberately absent.
        let f = first_rule(&mut d, &book, "ru-app");
        assert_eq!(f["found"], true, "answered mid-proof, not declined: {f}");
        assert_eq!(f["symbol_class"], ":ideal", "guards are not verified here: {f}");
        assert!(f["measure"].as_str().unwrap_or("").contains("acl2-count"),
                "the measure it was admitted on: {f}");
        let runes = f["runes"].as_array().expect("runes");
        let classes: Vec<&str> =
            runes.iter().filter_map(|r| r["class"].as_str()).collect();
        for want in [":definition", ":type-prescription", ":induction",
                     ":executable-counterpart"] {
            assert!(classes.contains(&want), "every rune is listed: {classes:?}");
        }
        let def = runes.iter().find(|r| r["class"] == ":definition").expect("definition rune");
        assert_eq!(def["enabled"], true, "{def}");
        let keys: Vec<&str> =
            def["fields"].as_array().unwrap().iter()
                .filter_map(|x| x["key"].as_str()).collect();
        assert!(!keys.contains(&":rhs"), "the body is not carried: {keys:?}");
        assert!(keys.contains(&":clique"), "but the rest of the rule is: {keys:?}");

        // A theorem: the rewrite rule ACL2 derived from it.
        let t = first_rule(&mut d, &book, "true-listp-of-ru-app");
        let rw = t["runes"].as_array().expect("runes").iter()
            .find(|r| r["class"] == ":rewrite").expect("a rewrite rune");
        let lhs = rw["fields"].as_array().unwrap().iter()
            .find(|x| x["key"] == ":lhs").expect("an lhs")["value"].as_str().unwrap_or("");
        assert!(lhs.contains("true-listp (ru-app"), "the derived lhs: {lhs}");

        // A forward-chaining rule: its trigger term.
        let fc = first_rule(&mut d, &book, "ru-fc");
        let r = fc["runes"].as_array().expect("runes").iter()
            .find(|r| r["class"] == ":forward-chaining").expect("an fc rune");
        let trig = r["fields"].as_array().unwrap().iter()
            .find(|x| x["key"] == ":trigger").expect("a trigger")["value"].as_str().unwrap_or("");
        assert!(trig.contains("consp"), "the trigger term: {trig}");

        // A name this file never introduced is simply not found.
        let none = first_rule(&mut d, &book, "binary-append");
        assert_eq!(none["found"], false, "not ours to explain: {none}");
        break;
    }
    assert!(asked_while_busy, "the include-book should have been in flight to ask during");

    let closed = d.request("close", json!({ "book": book }));
    assert_eq!(closed["closed"], true);
    d.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The outline covers the whole file, not the admitted prefix.
///
/// Built from the text, so it needs no session and does not stop at the
/// frontier -- which is the point: an outline that showed only what had been
/// checked would be least useful exactly when a book is long and half done.
/// The nesting matters too, since a flat list would hide the members of an
/// `encapsulate` or a `mutual-recursion`, the forms most worth navigating.
#[test]
#[ignore = "needs ACL2"]
fn the_outline_names_every_unit_and_nests_what_they_contain() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-out-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("o.lisp");
    let text = "(in-package \"ACL2\")\n\n\
                (include-book \"std/util/bstar\" :dir :system)\n\n\
                (defun alpha (x) (if (consp x) (alpha (cdr x)) nil))\n\n\
                (defthm alpha-of-nil (equal (alpha nil) nil))\n\n\
                (defconst *k* 7)\n\n\
                (mutual-recursion\n\
                  (defun ev-e (x) (if (consp x) (ev-l (cdr x)) x))\n\
                  (defun ev-l (x) (if (consp x) (ev-e (car x)) x)))\n";
    std::fs::write(&book_path, text).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();

    // Hold the target early: everything after line 5 is unadmitted, and the
    // outline must show it anyway.
    d.notify("update", json!({ "book": book, "text": text, "target_line": 5, "revision": 1 }));
    let _ = d.events_until_settled();

    let r = d.request("symbols", json!({ "book": book }));
    let syms = r["symbols"].as_array().expect("symbols array");
    let name_of = |i: usize| syms[i]["name"].as_str().unwrap_or("").to_string();
    let kind_of = |i: usize| syms[i]["kind"].as_str().unwrap_or("").to_string();

    assert_eq!(syms.len(), 6, "one entry per unit, admitted or not: {r}");
    assert_eq!(name_of(0), "in-package", "a form that defines nothing is named by its head");
    assert_eq!(name_of(1), "include-book \"std/util/bstar\"", "an include names its book");
    assert_eq!(kind_of(1), "include");
    assert_eq!(name_of(2), "alpha");
    assert_eq!(kind_of(2), "function");
    assert_eq!(name_of(3), "alpha-of-nil");
    assert_eq!(kind_of(3), "theorem", "a defthm is a fact, not a function");
    assert_eq!(name_of(4), "*k*");
    assert_eq!(kind_of(4), "constant");

    // The clique: named by its head, with its members as children -- and it
    // is well past the frontier, which the outline does not care about.
    assert_eq!(name_of(5), "mutual-recursion");
    let kids = syms[5]["children"].as_array().expect("children");
    let kid_names: Vec<&str> = kids.iter().map(|k| k["name"].as_str().unwrap_or("")).collect();
    assert_eq!(kid_names, vec!["ev-e", "ev-l"], "its members are nested inside it");
    assert!(kids.iter().all(|k| k["kind"] == "function"));
    assert!(
        syms[5]["line_start"].as_u64().unwrap_or(0) > 5,
        "and it sits below the target line, unadmitted: {r}"
    );

    let closed = d.request("close", json!({ "book": book }));
    assert_eq!(closed["closed"], true);
    d.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Certify a book, and follow the build rather than waiting on it.
///
/// The point of the shape is that a build is a separate process tree, not a
/// submission: the session stays answerable while `cert.pl` runs, and the
/// request is answered whenever the build ends. The failure case is the one
/// worth having -- certification chases dependencies, so the book blamed is
/// often not the one asked for.
#[test]
#[ignore = "needs ACL2"]
fn certify_runs_outside_the_session_and_reports_what_failed() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-cert-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("c.lisp");
    let text = "(in-package \"ACL2\")\n\n\
                (defun alpha (x) (if (consp x) (alpha (cdr x)) nil))\n\n\
                (defthm alpha-of-nil (equal (alpha nil) nil))\n";
    std::fs::write(&book_path, text).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();
    d.notify("update", json!({ "book": book, "text": text, "revision": 1 }));
    let _ = d.events_until_settled();

    // The session must stay answerable while the build runs: a certification
    // is not a submission, and nothing about it should block a query.
    let r = d.request("certify", json!({ "book": book, "jobs": 2 }));
    assert_eq!(r["status"], "certified", "a good book certifies: {r}");
    assert!(r["output_lines"].as_u64().expect("lines") > 0, "with a log: {r}");
    assert!(r["failed_books"].is_null(), "and nothing blamed: {r}");
    assert!(std::path::Path::new(&dir.join("c.cert")).is_file(), "the .cert exists");

    // The session is untouched by all of it -- certify runs outside.
    let st = d.request("status", json!({ "book": book }));
    assert!(st["running"].is_null(), "the session is idle, not busy: {st}");
    let a = d.request("describe", json!({ "book": book, "name": "alpha" }));
    assert_eq!(a["found"], true, "and still answers about its world: {a}");

    // Now a book that cannot certify. The report has to name what failed.
    let bad = "(in-package \"ACL2\")\n\n(defthm nope (equal x (cons 1 x)))\n";
    std::fs::write(&book_path, bad).expect("rewrite");
    let r2 = d.request("certify", json!({ "book": book, "jobs": 2 }));
    assert_eq!(r2["status"], "failed", "a false theorem does not certify: {r2}");
    let blamed = r2["failed_books"].as_array().expect("something is blamed");
    assert!(!blamed.is_empty(), "and it says which book: {r2}");
    assert!(
        blamed[0].as_str().unwrap_or("").contains("c.lisp"),
        "here, the one asked for: {r2}"
    );

    let closed = d.request("close", json!({ "book": book }));
    assert_eq!(closed["closed"], true);
    d.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A buffer that will not lex says *where*, not just that it did not.
///
/// The client paints from that line to the end of the file, so the number has
/// to be the opening paren of the form that never closes -- not end of file,
/// which is where a reader naturally notices the problem. Everything below
/// the opening paren is unread, and saying so is the whole point.
#[test]
#[ignore = "needs ACL2"]
fn an_unbalanced_buffer_settles_on_the_line_that_broke_it() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-split-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("unbalanced.lisp");
    let good = "(in-package \"ACL2\")\n\n(defun sp-a (x) x)\n";
    std::fs::write(&book_path, good).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();
    d.notify("update", json!({ "book": book, "text": good, "revision": 1 }));
    assert_eq!(
        d.events_until_settled().last().expect("settled").1["reason"],
        "reached_target"
    );

    // Line 5 opens a form that never closes. Lines 6 and 7 are inside it and
    // so are unread, however well-formed they look on their own.
    let broken = "(in-package \"ACL2\")\n\n\
                  (defun sp-a (x) x)\n\n\
                  (defthm sp-open (equal (sp-a x) x)\n\
                  (defun sp-b (y) y)\n\
                  (defun sp-c (z) z)\n";
    d.notify("update", json!({ "book": book, "text": broken, "revision": 2 }));
    let events = d.events_until_settled();
    let settled = &events.last().expect("settled").1;
    assert_eq!(settled["reason"], "split_error", "events: {events:?}");
    assert_eq!(settled["line"], 5, "the unclosed paren's own line: {settled}");
    assert!(
        settled["message"].as_str().unwrap_or("").contains("unterminated"),
        "and says what went wrong: {settled}"
    );
    // The line travels as a field, so the client never reads it back out of
    // the prose -- which is why the prose no longer repeats it.
    assert!(
        !settled["message"].as_str().unwrap_or("").contains("line 5"),
        "the position is a field, not a substring: {settled}"
    );

    // Balancing it again clears the condition rather than latching.
    let fixed = broken.replace("(defthm sp-open (equal (sp-a x) x)\n", "(defthm sp-open (equal (sp-a x) x))\n");
    d.notify("update", json!({ "book": book, "text": fixed, "revision": 3 }));
    let after = d.events_until_settled();
    let last = &after.last().expect("settled").1;
    assert_eq!(last["reason"], "reached_target", "events: {after:?}");
    assert!(last.get("line").is_none(), "and carries no position: {last}");
}

/// A client that stops reading must not wedge the daemon.
///
/// The failure this pins down: `Out::send` took the stdout mutex and then
/// wrote *while holding it*. stdout is a socket with a fixed buffer, so once
/// the client stopped draining, the write blocked in the kernel with the
/// mutex held -- and the main loop, the only thread reading stdin, blocked on
/// that same mutex to send its reply. Every book went dead at once, including
/// books doing nothing, and the process sat alive and silent.
///
/// Asserted through *exit*, which is the one consequence that separates the
/// two shapes cleanly. Closing stdin ends a healthy daemon in milliseconds. A
/// wedged one never sees the EOF at all, because the thread that would read
/// it is waiting on a lock the blocked writer holds -- so it hangs for ever
/// rather than merely being slow.
#[test]
#[ignore = "needs ACL2"]
fn a_client_that_stops_reading_does_not_wedge_the_daemon() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-backp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book: PathBuf = dir.join("noisy.lisp");
    // Gag mode off so the proof narrates: the point is a log big enough that
    // a few `all` slices of it overflow a pipe buffer.
    std::fs::write(&book, concat!(
        "(in-package \"ACL2\")\n",
        "(set-gag-mode nil)\n",
        "(defun bp-app (x y) (if (endp x) y (cons (car x) (bp-app (cdr x) y))))\n",
        "(defthm bp-assoc (equal (bp-app (bp-app a b) c) (bp-app a (bp-app b c))))\n",
    )).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book.to_str().unwrap() }));
    let id = opened["book"].as_str().expect("book id").to_string();
    let text = std::fs::read_to_string(&book).unwrap();
    d.notify("update", json!({ "book": id, "text": text, "revision": 1 }));

    // Read normally until it settles, collecting a handle to a real log.
    let events = d.events_until_settled();
    let refs: Vec<String> = events
        .iter()
        .filter(|(m, _)| m == "unit")
        .filter_map(|(_, v)| v["output_ref"].as_str().map(str::to_string))
        .collect();
    let r = refs.last().expect("some unit reported a log").clone();

    // Now stop reading, and ask for whole logs. Each reply is far larger than
    // it needs to be; a handful of them bury a 64K buffer.
    for _ in 0..40 {
        d.send_only("output", json!({ "book": id, "output_ref": r, "mode": "all" }));
    }
    std::thread::sleep(std::time::Duration::from_secs(1));

    // Close stdin without ever draining, and require the daemon to notice.
    let t0 = std::time::Instant::now();
    d.close_stdin();
    let exited = d.wait_for_exit(std::time::Duration::from_secs(20));
    assert!(
        exited,
        "the daemon saw EOF and exited while the client was {} behind; it hung instead",
        "40 whole logs"
    );
    eprintln!("exited {:?} after stdin closed with the client far behind", t0.elapsed());
}

/// Everything a form introduced is explained in one exchange.
///
/// The form view asks about every name a unit's events recorded, and for
/// macro-generated code that is far more than the buffer shows -- a `defines`
/// in kestrel/c/syntax/validator.lisp records 620. Asked one at a time, that
/// was 620 requests fired at once the moment the form landed, 620 commands
/// queued to the thread that also runs proofs, and 620 replies onto a socket
/// the client was too busy to drain. This pins the shape that cannot do that.
#[test]
#[ignore = "needs ACL2"]
fn a_forms_names_are_explained_in_one_request() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-batch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("b.lisp");
    let text = concat!(
        "(in-package \"ACL2\")\n",
        "(defun ba-app (x y) (if (endp x) y (cons (car x) (ba-app (cdr x) y))))\n",
        "(defthm ba-assoc (equal (ba-app (ba-app a b) c) (ba-app a (ba-app b c))))\n",
    );
    std::fs::write(&book_path, text).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();
    d.notify("update", json!({ "book": book, "text": text, "revision": 1 }));
    let _ = d.events_until_settled();

    // Several names, one exchange, answers in the order asked -- the client
    // pairs them positionally, so order is part of the contract.
    let asked = ["ba-app", "ba-assoc", "no-such-name-at-all"];
    let batch = d.request("rules", json!({ "book": book, "names": asked }));
    let got = batch["rules"].as_array().expect("a list of answers");
    assert_eq!(got.len(), asked.len(), "one answer per name asked: {batch}");
    for (i, name) in asked.iter().enumerate() {
        assert_eq!(got[i]["name"].as_str(), Some(*name), "in the order asked: {batch}");
    }
    assert_eq!(got[0]["found"], true, "a name this file defined is known: {batch}");
    assert_eq!(got[1]["found"], true, "and so is its theorem: {batch}");
    // A name that does not exist is reported, not omitted: dropping it would
    // shift every later answer out of step with the list the client sent.
    assert_eq!(got[2]["found"], false, "and an unknown one is still answered: {batch}");

    // An empty list is legal and cheap -- the form view asks whenever a form
    // lands, including forms that introduced nothing at all.
    let none = d.request("rules", json!({ "book": book, "names": Vec::<String>::new() }));
    assert_eq!(none["rules"].as_array().map(|a| a.len()), Some(0), "{none}");
}

/// An `update` with no text means "the buffer is the file", and the daemon
/// reads it.
///
/// This is the only way a book that is not UTF-8 can be checked from an
/// editor at all. ACL2's model is Latin-1 -- one byte, one character -- and
/// `books/kestrel/acl2pl/reader-tests.lisp` writes `#\<byte>` for all 256.
/// VSCode hands an extension characters, not bytes: it decoded the file at
/// `files.encoding` first, and for that book the decode replaced 1399 bytes
/// with U+FFFD before `getText()` was ever called. Nothing on the wire can
/// carry those back, so a client whose buffer is the file must say so and
/// let the daemon read the bytes itself.
#[test]
#[ignore = "needs ACL2"]
fn an_update_without_text_reads_the_file_bytes() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-latin1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("b.lisp");

    // Bytes, not a `str`: 0xe9 is `é` in Latin-1 and is not valid UTF-8, so
    // this book cannot be expressed as the `text` field at all.
    let mut src: Vec<u8> = Vec::new();
    src.extend_from_slice(b"(in-package \"ACL2\")\n\n");
    src.extend_from_slice(b"(assert-event (equal (char-code #\\\xe9) 233))\n\n");
    src.extend_from_slice(b"(defconst *e* \"caf\xe9\")\n");
    std::fs::write(&book_path, &src).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();

    // No `text` field. What the daemon syncs is what is on disk.
    d.notify("update", json!({ "book": book, "revision": 1 }));
    let events = d.events_until_settled();
    let settled = &events.last().expect("settled").1;
    assert_eq!(settled["reason"], "reached_target", "events: {events:?}");
    let state = last_state(&events);
    assert_eq!(statuses(&state), ["admitted", "admitted", "admitted"],
               "a Latin-1 book checks clean: {state}");

    // And the same book sent as text -- which is all a decoding editor could
    // send -- is the failure this exists to avoid. `from_utf8_lossy` is what
    // the editor's own decode does to it.
    let lossy = String::from_utf8_lossy(&src).into_owned();
    d.notify("update", json!({ "book": book, "text": lossy, "revision": 2 }));
    let after = d.events_until_settled();
    let state2 = last_state(&after);
    assert!(statuses(&state2).contains(&"failed".to_string()),
            "replaced bytes are not the book, and it shows: {state2}");

    // And it recovers. This is the step the client used to have no way to
    // take: updates were sent on content change only, so after saving such a
    // file nothing re-sent it and the book stayed failed until it was closed
    // and reopened. Saving is exactly when the buffer becomes the file again.
    d.notify("update", json!({ "book": book, "revision": 3 }));
    let saved = d.events_until_settled();
    let state3 = last_state(&saved);
    assert_eq!(statuses(&state3), ["admitted", "admitted", "admitted"],
               "reading the file again undoes it: {state3}");

    d.request("close", json!({ "book": book }));
}

/// The `open` reply says when the server had to re-enable PROVE output.
///
/// The environment -- an `acl2-customization.lsp` that inhibits `prove`,
/// usually -- turns it off, and the server turns it back on so proofs can be
/// captured. The client shows a one-time notice, so the fact has to reach it;
/// it rides on the `open` reply, present only when an override happened.
///
/// Both cases are pinned with a book-directory customization, so the outcome
/// does not depend on whatever the machine's home `acl2-customization.lsp`
/// does: one book's directory inhibits PROVE, the other clears it.
#[test]
#[ignore = "needs ACL2"]
fn open_reports_when_prove_output_was_re_enabled() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let root = std::env::temp_dir().join(format!("acl2-editord-inhibit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    // A book-dir customization is found before the home one and suppresses it,
    // so each of these directories decides its own PROVE inhibition.
    let book = "(in-package \"ACL2\")\n\n\
                (defun f (x) (if (consp x) (f (cdr x)) nil))\n";
    let inhibiting = root.join("inhibits");
    std::fs::create_dir_all(&inhibiting).expect("dir");
    std::fs::write(inhibiting.join("acl2-customization.lsp"),
        "(in-package \"ACL2\")\n(set-inhibit-output-lst '(prove proof-tree))\n").unwrap();
    std::fs::write(inhibiting.join("b.lisp"), book).unwrap();

    let clearing = root.join("clears");
    std::fs::create_dir_all(&clearing).expect("dir");
    std::fs::write(clearing.join("acl2-customization.lsp"),
        "(in-package \"ACL2\")\n(set-inhibit-output-lst nil)\n").unwrap();
    std::fs::write(clearing.join("b.lisp"), book).unwrap();

    let mut d = Daemon::start(&acl2);

    let on = d.request("open", json!({ "path": inhibiting.join("b.lisp").to_str().unwrap() }));
    // ACL2 prints symbols uppercase, so the inherited list reads `(PROVE ...)`.
    let field = on["prove_output_overridden"].as_str();
    assert!(field.is_some_and(|s| s.to_ascii_lowercase().contains("prove")),
            "an inhibiting environment is reported, with what it inherited: {on}");
    d.request("close", json!({ "book": on["book"].as_str().unwrap() }));

    let off = d.request("open", json!({ "path": clearing.join("b.lisp").to_str().unwrap() }));
    assert!(off["prove_output_overridden"].is_null(),
            "an environment that did not inhibit PROVE carries no notice: {off}");
    d.request("close", json!({ "book": off["book"].as_str().unwrap() }));
}

/// A target that names one of two forms written on the same line.
///
/// The daemon steps one unit at a time, naming each by where it ends, so a
/// line shared by two forms is the one case that arithmetic cannot express:
/// held at the line, the second form goes in with the first and a retraction
/// to the first bounces straight back off it. The column is what makes each
/// of them nameable, in the target the client sends and in every position
/// the daemon sends back.
#[test]
#[ignore = "needs ACL2"]
fn a_target_column_picks_between_two_forms_on_a_line() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-col-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("shared.lisp");
    //                            1         2         3
    //                   123456789012345678901234567890123456
    let text = "(in-package \"ACL2\")\n(defun one (x) x) (defun two (x) x)\n";
    std::fs::write(&book_path, text).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();

    // Held at the end of the first form on the line.
    d.notify("update", json!({
        "book": book, "text": text,
        "target_line": 2, "target_column": 18, "revision": 1,
    }));
    let events = d.events_until_settled();
    let state = last_state(&events);
    assert_eq!(statuses(&state), ["admitted", "admitted", "beyond"],
               "the second form on the line is held out: {state}");
    let units = state["units"].as_array().expect("units");
    assert_eq!((units[1]["line_start"].as_u64(), units[2]["line_start"].as_u64()),
               (Some(2), Some(2)), "both are on line 2, which is the difficulty");
    assert_eq!((units[1]["col_start"].as_u64(), units[1]["col_end"].as_u64()),
               (Some(1), Some(18)), "and the columns are the difference: {state}");
    assert_eq!(units[2]["col_start"].as_u64(), Some(19));

    // One column further along takes the other form too.
    d.notify("update", json!({
        "book": book, "text": text,
        "target_line": 2, "target_column": 19, "revision": 2,
    }));
    let events = d.events_until_settled();
    assert_eq!(statuses(&last_state(&events)), ["admitted", "admitted", "admitted"]);

    // And back: a retraction to the first form has to hold there rather than
    // re-admitting its neighbour on the next step.
    d.notify("retract", json!({ "book": book, "keep": 2 }));
    d.notify("update", json!({
        "book": book, "text": text,
        "target_line": 2, "target_column": 18, "revision": 3,
    }));
    let events = d.events_until_settled();
    assert_eq!(statuses(&last_state(&events)), ["admitted", "admitted", "beyond"],
               "the retraction holds: {:?}", last_state(&events));

    // Every position the daemon reports carries its column.
    let def = d.request("definition", json!({ "book": book, "name": "two" }));
    assert_eq!((def["line"].as_u64(), def["column"].as_u64()), (Some(2), Some(19)),
               "definition says which of the two: {def}");
    let syms = d.request("symbols", json!({ "book": book }));
    let outline: Vec<String> = syms["symbols"].as_array().expect("symbols").iter()
        .map(|s| format!("{}@{}:{}", s["name"].as_str().unwrap_or("?"),
                         s["line_start"], s["col_start"]))
        .collect();
    assert_eq!(outline, ["in-package@1:1", "one@2:1", "two@2:19"],
               "the outline separates them: {syms}");

    d.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Moving the target back is a request to undo, and takes effect on its own.
///
/// Rollback is otherwise driven by the text and never by the target, which
/// left a target moved backwards doing nothing at all: the barrier said stop
/// here, the world stayed where it was, and only an unrelated edit -- whose
/// reconcile rolls back for its own reasons -- ever made it take effect. The
/// statuses are the proof either way, since `admitted` is read from the
/// frontier rather than from any bookkeeping the target touches.
#[test]
#[ignore = "needs ACL2"]
fn a_target_moved_back_retracts_without_an_edit() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-back-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("back.lisp");
    let text = "(in-package \"ACL2\")\n\
                (defun one (x) x)\n\
                (defun two (x) x)\n\
                (defun three (x) x)\n";
    std::fs::write(&book_path, text).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();

    // No target: the whole book goes in.
    d.notify("update", json!({ "book": book, "text": text, "revision": 1 }));
    let events = d.events_until_settled();
    assert_eq!(statuses(&last_state(&events)), ["admitted"; 4]);

    // Now hold at the second form, with the text untouched. Nothing about the
    // buffer changed, so nothing but the target can cause this.
    d.notify("update", json!({
        "book": book, "text": text, "target_line": 3, "target_column": 17, "revision": 2,
    }));
    let events = d.events_until_settled();
    let state = last_state(&events);
    assert_eq!(statuses(&state), ["admitted", "admitted", "admitted", "beyond"],
               "the target alone rolled the last form back: {state}");
    assert_eq!(events.last().expect("settled").1["reason"], "reached_target");

    // All the way back, to before the first form.
    d.notify("update", json!({
        "book": book, "text": text, "target_line": 0, "revision": 3,
    }));
    let events = d.events_until_settled();
    assert_eq!(statuses(&last_state(&events)), ["beyond"; 4],
               "a target before the first form empties the world: {:?}", last_state(&events));

    // And forward again, which never needed the target to move anything.
    d.notify("update", json!({ "book": book, "text": text, "revision": 4 }));
    let events = d.events_until_settled();
    assert_eq!(statuses(&last_state(&events)), ["admitted"; 4]);

    // Once more, and then ask the *session* what it holds. Left to the end on
    // purpose: a request runs a drive pass of its own, so each one settles,
    // and reading events after one reads that settle instead of the next
    // update's.
    d.notify("update", json!({
        "book": book, "text": text, "target_line": 3, "target_column": 17, "revision": 5,
    }));
    let events = d.events_until_settled();
    assert_eq!(statuses(&last_state(&events)), ["admitted", "admitted", "admitted", "beyond"]);

    // `three` is gone from the world, not merely relabelled. Asked of the
    // session rather than of the buffer: `definition` answers a name defined
    // in this file lexically, and would find it in the text either way.
    let g = d.request("describe", json!({ "book": book, "name": "three", "cols": 40 }));
    assert_eq!(g["found"], false, "the retracted definition is out of the world: {g}");
    let k = d.request("describe", json!({ "book": book, "name": "two", "cols": 40 }));
    assert_eq!(k["found"], true, "and the form before the target is still in it: {k}");
}

/// A form's value reaches the wire, and only when it says something.
///
/// The three outcomes the core decides between, checked here at the layer
/// the editor actually reads: an ordinary value is carried, an event's value
/// is dropped because it only repeats the name the event already carries,
/// and a value ACL2 itself declines to print never appears.
#[test]
#[ignore = "needs ACL2"]
fn a_forms_value_reaches_the_wire_when_it_says_something() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-value-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("value.lisp");
    let text = "(in-package \"ACL2\")\n\
                (+ 1 2)\n\
                (defun one (x) x)\n\
                (thm (equal x x))\n";
    std::fs::write(&book_path, text).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();
    d.notify("update", json!({ "book": book, "text": text, "revision": 1 }));
    let events = d.events_until_settled();

    let value_at = |index: u64| -> Option<String> {
        events
            .iter()
            .filter(|(m, p)| m == "unit" && p["index"] == index)
            .find_map(|(_, p)| p["value"].as_str())
            .map(str::to_string)
    };

    assert_eq!(value_at(1).as_deref(), Some("3"), "an ordinary value is carried");
    assert_eq!(
        value_at(2),
        None,
        "an event's value is only its own name, which its event already carries"
    );
    assert_eq!(
        value_at(3),
        None,
        "and a thm returns :invisible, which ACL2 itself declines to print"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A form past the target is not queued for anything, and does not say it is.
#[test]
#[ignore = "needs ACL2"]
fn forms_past_the_target_are_beyond_rather_than_pending() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-beyond-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("beyond.lisp");
    let text = "(in-package \"ACL2\")\n\
                (defun one (x) x)\n\
                (defun two (x) x)\n";
    std::fs::write(&book_path, text).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();

    d.notify("update", json!({
        "book": book, "text": text, "target_line": 2, "target_column": 18, "revision": 1,
    }));
    let events = d.events_until_settled();
    assert_eq!(statuses(&last_state(&events)), ["admitted", "admitted", "beyond"]);

    // Without a target the same unchecked form is `pending`, which is the
    // distinction: one will be reached, the other will not.
    d.notify("retract", json!({ "book": book, "keep": 2 }));
    let _ = d.events_until_settled();
    d.notify("update", json!({ "book": book, "text": text, "revision": 2 }));
    let events = d.events_until_settled();
    assert_eq!(statuses(&last_state(&events)), ["admitted", "admitted", "admitted"],
               "clearing the target lets the run reach it: {:?}", last_state(&events));
}

/// A target given as a form's *first* character means "through this form".
///
/// Which is what the editor sends, and it sends it because that is the point
/// an edit moves least: typing inside the target form moves its end on every
/// keystroke and leaves its head alone. `goal_unit` already reads a target as
/// the last form covering or ending by the position, so a head resolves to its
/// own form -- but nothing said so, and the client now depends on it.
#[test]
#[ignore = "needs ACL2"]
fn a_target_on_a_form_head_admits_that_form() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-head-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("head.lisp");
    //                                          1         2         3
    //                                 123456789012345678901234567890123456
    let text = "(in-package \"ACL2\")\n\
                (defun one (x) x)\n\
                (defun two (x) x) (defun three (x) x)\n";
    std::fs::write(&book_path, text).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();

    // The head of `one`: through it, and not past it.
    d.notify("update", json!({
        "book": book, "text": text, "target_line": 2, "target_column": 1, "revision": 1,
    }));
    let events = d.events_until_settled();
    assert_eq!(statuses(&last_state(&events)), ["admitted", "admitted", "beyond", "beyond"],
               "a head admits its own form: {:?}", last_state(&events));

    // The head of the *second* form on a line resolves to that form, not to
    // the one it shares the line with -- the case the column exists for.
    d.notify("update", json!({
        "book": book, "text": text, "target_line": 3, "target_column": 19, "revision": 2,
    }));
    let events = d.events_until_settled();
    assert_eq!(statuses(&last_state(&events)), ["admitted"; 4],
               "the later head takes both: {:?}", last_state(&events));

    // And the head of the first of the two takes only it.
    d.notify("update", json!({
        "book": book, "text": text, "target_line": 3, "target_column": 1, "revision": 3,
    }));
    let events = d.events_until_settled();
    assert_eq!(statuses(&last_state(&events)), ["admitted", "admitted", "admitted", "beyond"],
               "the earlier head holds the line's second form out: {:?}", last_state(&events));

    // A head above every form is the top, and empties the world.
    d.notify("update", json!({
        "book": book, "text": text, "target_line": 0, "revision": 4,
    }));
    let events = d.events_until_settled();
    assert_eq!(statuses(&last_state(&events)), ["beyond"; 4]);
}

/// A question about a settled book is answered without re-announcing it.
///
/// Every command the protocol loop applies is followed by a drive pass, and a
/// book that has already reached its target reaches it again on every one --
/// so a hover, a completion or an outline request each re-emitted the whole
/// state and settled a run that finished long ago. Measured on a book this
/// size before the guard: 28 KB of unsolicited events per question asked, each
/// one repainting every decoration in the editor for news it already had.
///
/// Read by asking twice. Anything the first query provoked arrives while the
/// second's reply is being waited for, so it lands in `pending` -- which is
/// empty exactly when the driver said nothing.
#[test]
#[ignore = "needs ACL2"]
fn a_query_on_a_settled_book_says_nothing() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-quiet-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("quiet.lisp");
    let mut text = String::from("(in-package \"ACL2\")\n");
    for i in 0..40 {
        text.push_str(&format!("(defun f{i} (x) x)\n"));
    }
    std::fs::write(&book_path, &text).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();
    d.notify("update", json!({ "book": book, "text": text, "revision": 1 }));
    let events = d.events_until_settled();
    assert_eq!(events.last().expect("settled").1["reason"], "reached_target");
    assert!(d.pending.is_empty(), "nothing left over: {:?}", d.pending);

    // Every read-only verb the editor leans on while the reader is just
    // reading. `symbols` and `links` are the ones that hurt -- VSCode asks
    // for both after edits -- and `complete` runs per keystroke.
    let asks = [
        ("describe", json!({ "book": book, "name": "f7", "cols": 40 })),
        ("definition", json!({ "book": book, "name": "f7" })),
        ("symbols", json!({ "book": book })),
        ("complete", json!({ "book": book, "prefix": "f1" })),
        ("links", json!({ "book": book })),
        ("rules", json!({ "book": book, "names": ["f7"] })),
    ];
    for (verb, params) in &asks {
        let _ = d.request(verb, params.clone());
        // A second request, whose reply cannot arrive before anything the
        // first provoked. It has to be a verb the *driver* answers: the
        // driver applies one command, drives it, and only then takes the
        // next, so its events precede the next reply by construction. A verb
        // the protocol loop answers on its own -- `status` -- races them, and
        // this read silently passed for whichever query lost.
        let _ = d.request("rules", json!({ "book": book, "names": ["f0"] }));
        assert!(d.pending.is_empty(), "{verb} spoke: {:?}", d.pending);
    }

    // The guard is about repetition, not about silence: something that moves
    // still settles. A retraction moves the frontier without touching a byte.
    d.notify("retract", json!({ "book": book, "keep": 3 }));
    let events = d.events_until_settled();
    assert_eq!(events.last().expect("settled").1["reason"], "reached_target");
    assert_eq!(statuses(&last_state(&events))[3], "beyond", "the world moved and said so");

    // And having said it, it goes quiet again.
    let _ = d.request("symbols", json!({ "book": book }));
    let _ = d.request("rules", json!({ "book": book, "names": ["f0"] }));
    assert!(d.pending.is_empty(), "settled twice for one retraction: {:?}", d.pending);
}

/// A paused session forbids exactly one operation: admitting a form.
///
/// Stated as a prohibition rather than a list, because that is what makes it
/// checkable. Reconciling still happens -- it is the half of sync that
/// withdraws claims, and a paused session whose green gutter went stale would
/// be lying rather than resting. Retracting, evaluating and every question the
/// language features ask are untouched, which is most of what a session is for
/// while its reader is editing.
#[test]
#[ignore = "needs ACL2"]
fn a_paused_session_stops_admitting_and_nothing_else() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-pause-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("pause.lisp");
    let text = "(in-package \"ACL2\")\n\
                (defun one (x) x)\n\
                (defun two (x) x)\n\
                (defun three (x) x)\n";
    std::fs::write(&book_path, text).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();

    // Held at `one`, so there is work visibly left to do.
    d.notify("update", json!({
        "book": book, "text": text, "target_line": 2, "target_column": 1, "revision": 1,
    }));
    let events = d.events_until_settled();
    assert_eq!(statuses(&last_state(&events)), ["admitted", "admitted", "beyond", "beyond"]);

    // Pause, and then ask for the work: clearing the target would admit the
    // last two forms on any other day.
    let _ = d.request("pause", json!({ "book": book }));
    let events = d.events_until_settled();
    assert_eq!(events.last().expect("settled").1["reason"], "paused",
               "stopping short of the target is not reaching it");

    d.notify("update", json!({ "book": book, "text": text, "revision": 2 }));
    let events = d.events_until_settled();
    let state = last_state(&events);
    // `pending`, not `beyond`. A pause is a moment and not a barrier: these
    // forms will be reached the instant someone presses play, which is what
    // `pending` says. `beyond` is for what a *target* excludes, and stays
    // excluded until the target moves.
    assert_eq!(statuses(&state), ["admitted", "admitted", "pending", "pending"],
               "the target went away and nothing was admitted: {state}");
    assert_eq!(events.last().expect("settled").1["reason"], "paused");

    // Everything else still works. A question about the world -- asked of a
    // form that is *in* it, since one that is not would answer "no" for a
    // reason that has nothing to do with pausing:
    let q = d.request("describe", json!({ "book": book, "name": "one", "cols": 40 }));
    assert_eq!(q["found"], true, "a paused session still answers: {q}");
    // A form evaluated against it:
    let e = d.request("eval", json!({ "book": book, "form": "(one 3)" }));
    assert_eq!(e["outcome"], "ok", "a paused session still evaluates: {e}");
    // An eval touches the world and puts it back, so it clears the settle
    // mark and settles on the way out. Read that here, or the next wait for a
    // settle reads this one and reports the world as it was before.
    let after = d.events_until_settled();
    assert_eq!(after.last().expect("settled").1["reason"], "paused",
               "and is still paused afterwards");

    // And a retraction, which takes claims away rather than adding them.
    d.notify("retract", json!({ "book": book, "keep": 1 }));
    let events = d.events_until_settled();
    // A retraction leaves a target behind it, so these *are* beyond one --
    // for that reason and not for the pause.
    assert_eq!(statuses(&last_state(&events)), ["admitted", "beyond", "beyond", "beyond"],
               "a paused session still retracts: {:?}", last_state(&events));
    let gone = d.request("describe", json!({ "book": book, "name": "one", "cols": 40 }));
    assert_eq!(gone["found"], false, "and it really happened: {gone}");

    // A retraction leaves a target where it stopped, so clear it again --
    // still paused, so still nothing goes in.
    d.notify("update", json!({ "book": book, "text": text, "revision": 3 }));
    let events = d.events_until_settled();
    assert_eq!(statuses(&last_state(&events)), ["admitted", "pending", "pending", "pending"],
               "and with the target gone they are queued again, still unadmitted");

    // Resume, and the work that was refused goes through.
    let _ = d.request("resume", json!({ "book": book }));
    let events = d.events_until_settled();
    assert_eq!(statuses(&last_state(&events)), ["admitted"; 4],
               "resuming admits what pausing withheld: {:?}", last_state(&events));
    assert_eq!(events.last().expect("settled").1["reason"], "reached_target");
}

/// A second book's startup does not stop the first book being talked to.
///
/// `open` runs the whole ACL2 startup handshake, and it used to run it inline
/// on the daemon's one protocol thread -- the only thread reading stdin. So
/// while a second book started, the first book's `update`, `status`, hover and
/// `interrupt` were not being *answered*; they were not being read. With one
/// book that is invisible, which is why it survived; with two it is a freeze
/// for as long as ACL2 takes to come up, and up to the customization-file
/// timeout in the bad case.
///
/// Written as an ordering rather than a duration, because a timing threshold
/// on a machine under load is a flaky test: the question is whether the first
/// book's reply can overtake the second book's open, and inline it could not
/// have, ever.
#[test]
#[ignore = "needs ACL2"]
fn one_books_open_does_not_stall_another_book() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let root = std::env::temp_dir().join(format!("acl2-editord-two-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("dir");
    let book = "(in-package \"ACL2\")\n\n(defun f (x) (if (consp x) (f (cdr x)) nil))\n";
    std::fs::write(root.join("a.lisp"), book).unwrap();
    std::fs::write(root.join("b.lisp"), book).unwrap();

    let mut d = Daemon::start(&acl2);
    let a = d.request("open", json!({ "path": root.join("a.lisp").to_str().unwrap() }));
    let a_id = a["book"].as_str().expect("book id").to_string();

    // The second open goes out and is deliberately not waited for.
    let open_id = d.send_only("open", json!({ "path": root.join("b.lisp").to_str().unwrap() }));
    // Then a question about the first book, which the protocol thread answers
    // from lock-free handles without touching the book at all.
    let status_id = d.send_only("status", json!({ "book": a_id }));

    let mut replies: Vec<(u64, Value)> = Vec::new();
    while replies.len() < 2 {
        let v = d.read_frame();
        if let Some(i) = v.get("id").and_then(|i| i.as_u64()) {
            assert!(v.get("error").is_none(), "reply {i} failed: {v}");
            replies.push((i, v["result"].clone()));
        }
    }
    assert_eq!(
        replies.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
        vec![status_id, open_id],
        "the first book's status must come back while the second book is still starting"
    );

    // And the second book really did open -- a reply that arrives in the right
    // order is not enough if it arrives empty.
    let b_id = replies[1].1["book"].as_str().expect("the second book's id");
    assert_ne!(b_id, a_id, "the second open is a book of its own");

    d.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// A `retract` whose `keep` is past the end of the buffer keeps everything.
///
/// The client computes `keep` from the state it last painted, and an update
/// can shorten the file between that computation and the daemon reading it --
/// deleting the last forms and hitting Retract to Cursor inside the debounce
/// window is enough. `sync_with`'s doc says what a stale number is allowed to
/// cost: admitting less than you meant to. Not this. Both "retract everything"
/// (`keep: 0`) and "keep more than exists" resolved to a target held before
/// the first form, and the drive that follows reads that as `keep: 0` -- so
/// every proof in the book was discarded, and stayed discarded until the next
/// edit.
#[test]
#[ignore = "needs ACL2"]
fn a_retract_past_the_end_of_the_buffer_keeps_everything() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let root = std::env::temp_dir().join(format!("acl2-editord-stale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("dir");
    let path = root.join("b.lisp");
    let text = "(in-package \"ACL2\")\n\
                (defun f (x) (if (consp x) (f (cdr x)) nil))\n\
                (defthm f-nil (equal (f nil) nil))\n";
    std::fs::write(&path, text).unwrap();

    let mut d = Daemon::start(&acl2);
    let r = d.request("open", json!({ "path": path.to_str().unwrap() }));
    let book = r["book"].as_str().unwrap().to_string();
    d.notify("update", json!({ "book": book, "text": text, "revision": 1 }));
    let first = d.events_until_settled();
    assert_eq!(statuses(&last_state(&first)), ["admitted", "admitted", "admitted"]);

    // The number a client would hold from a longer buffer. Nothing here has
    // eight forms; three is all there is.
    d.notify("retract", json!({ "book": book, "keep": 8 }));
    let after = d.events_until_settled();
    assert_eq!(
        statuses(&last_state(&after)),
        ["admitted", "admitted", "admitted"],
        "a keep past the end keeps everything: {:?}",
        last_state(&after)
    );

    // And `keep: 0` still means what it says, so the clamp did not swallow
    // the one case that legitimately holds before the first form.
    d.notify("retract", json!({ "book": book, "keep": 0 }));
    let none = d.events_until_settled();
    let left = statuses(&last_state(&none));
    assert!(
        left.iter().all(|s| s != "admitted"),
        "keep 0 retracts everything: {left:?}"
    );

    d.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// A unit event's coordinates describe the revision it is stamped with.
///
/// A form's coordinates are recorded when it is *submitted*, because the file
/// may change while it runs -- and it does: the driver applies queued updates
/// mid-flight. The unit survives that whenever the content hashes through it
/// are intact, which is exactly what a layout-only edit leaves: a blank line
/// inserted above moves every form below it on the page without changing a
/// word of what any of them says.
///
/// So the event went out carrying the new revision and the pre-edit line
/// numbers. Latent while the client paints from `state` events, which are
/// recomputed right after -- and a lie about the snapshot it names, which the
/// first client to trust unit coordinates would inherit.
///
/// The slow unit is a large `include-book`, as in the hover test above: it
/// reliably outruns the step deadline and puts the driver into the resume loop
/// where the stamping happens.
#[test]
#[ignore = "needs ACL2"]
fn a_unit_events_lines_match_the_revision_it_carries() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let root = std::env::temp_dir().join(format!("acl2-editord-restamp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("dir");
    let path = root.join("b.lisp");
    let slow = "(include-book \"std/top\" :dir :system)";
    let text = format!(
        "(in-package \"ACL2\")\n\
         (defun app (x y) (if (consp x) (cons (car x) (app (cdr x) y)) y))\n\
         {slow}\n"
    );
    std::fs::write(&path, &text).unwrap();

    let mut d = Daemon::start(&acl2);
    let r = d.request("open", json!({ "path": path.to_str().unwrap() }));
    let book = r["book"].as_str().unwrap().to_string();

    // Hold before the slow form, so the two quick ones are in and settled.
    d.notify("update", json!({ "book": book, "text": text, "target_line": 2, "revision": 1 }));
    let _ = d.events_until_settled();

    // Let it run, and wait until it really is in flight.
    d.notify("update", json!({ "book": book, "text": text, "revision": 2 }));
    let mut running = false;
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if !d.request("status", json!({ "book": book }))["running"].is_null() {
            running = true;
            break;
        }
    }
    assert!(running, "the include-book must outrun a step for this to be the resume path");

    // A blank line above it: every hash is unchanged, so it keeps running, and
    // every line below moves by one.
    let moved = text.replace(slow, &format!("\n{slow}"));
    d.notify("update", json!({ "book": book, "text": moved, "revision": 3 }));
    let after = d.events_until_settled();

    // Whatever this snapshot's units are, that is what a unit event stamped
    // with this revision has to say.
    let state = last_state(&after);
    let rev = state["revision"].as_u64().expect("revision");
    let want: Vec<(u64, u64)> = state["units"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| (u["line_start"].as_u64().unwrap(), u["line_end"].as_u64().unwrap()))
        .collect();
    assert_eq!(want[2], (4, 4), "the blank line moved the last form down one: {state}");

    let mut checked = 0;
    for (m, p) in &after {
        if m != "unit" || p["revision"].as_u64() != Some(rev) {
            continue;
        }
        let i = p["index"].as_u64().expect("index") as usize;
        assert_eq!(
            (p["line_start"].as_u64().unwrap(), p["line_end"].as_u64().unwrap()),
            want[i],
            "unit {i} at revision {rev} must carry revision {rev}'s coordinates: {p}"
        );
        checked += 1;
    }
    assert!(checked > 0, "no unit event was stamped with the settled revision");

    d.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// The protocol's error codes, which need no ACL2 at all.
///
/// A generic JSON-RPC client reads these: `-32602` says "you asked wrongly",
/// `-32603` says "I could not". Everything that was not an unknown method
/// replied `-32603`, so a mistyped book id read as the server's fault -- and a
/// frame that was valid JSON but not an object fell through to the
/// notification path and came back as an error *notification* about an unknown
/// method, which is neither the code nor the shape the spec gives for it.
///
/// **Not ignored**, unlike everything else in this file. The daemon checks its
/// `--default` image exists and then does nothing further with it until a book
/// is opened, and this opens none -- so the test binary serves as the image
/// and the whole protocol-error surface runs in CI. It is the first test here
/// that does.
#[test]
fn the_protocol_answers_with_the_codes_the_spec_gives() {
    // Any file that exists. The daemon only checks that much before a book is
    // opened, and no book is opened here.
    let image = std::env::current_exe().expect("current exe");
    let mut d = Daemon::start(image.to_str().expect("utf-8 path"));

    fn reply(d: &mut Daemon, v: Value) -> Value {
        d.frame(v);
        loop {
            let got = d.read_frame();
            if got.get("error").is_some() || got.get("result").is_some() {
                return got;
            }
        }
    }

    // A method nobody serves.
    let e = reply(&mut d, json!({ "jsonrpc": "2.0", "id": 1, "method": "nonesuch" }));
    assert_eq!(e["error"]["code"], -32601, "unknown method: {e}");

    // Params that will not deserialize -- `book` is not a string.
    let e = reply(
        &mut d,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "status", "params": { "book": 7 } }),
    );
    assert_eq!(e["error"]["code"], -32602, "bad params: {e}");

    // Params that deserialize and name a book that does not exist. That is a
    // statement about the request, not about the daemon.
    let e = reply(
        &mut d,
        json!({ "jsonrpc": "2.0", "id": 3, "method": "status", "params": { "book": "nope" } }),
    );
    assert_eq!(e["error"]["code"], -32602, "unknown book: {e}");

    // Valid JSON, not a request.
    let e = reply(&mut d, json!([1, 2, 3]));
    assert_eq!(e["error"]["code"], -32600, "not an object: {e}");

    // Not valid JSON at all.
    let e = reply(&mut d, json!("just a string"));
    assert_eq!(e["error"]["code"], -32600, "a bare string is not a request: {e}");

    // And the daemon is still speaking afterwards -- none of the above is a
    // reason to stop reading the client.
    let ok = reply(&mut d, json!({ "jsonrpc": "2.0", "id": 4, "method": "images" }));
    assert!(ok.get("result").is_some(), "still answering: {ok}");

    d.shutdown();
}

/// Which names in a form carry documentation, and where a topic with no event
/// of its own turns up.
///
/// Two shapes, and the second is the one that needs the `names` field. A
/// `define` with a `:short` records an event named after the function, so its
/// topic attaches to that event. A standalone `defxdoc` records no event at
/// all -- it writes a table -- so the only place its name appears is what the
/// split read from the source.
#[test]
#[ignore = "needs ACL2"]
fn a_forms_documentation_is_findable_by_name() {
    let Some(acl2) = acl2() else {
        eprintln!("ACL2 not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("acl2-editord-xdoc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let book_path: PathBuf = dir.join("doc.lisp");
    let text = "(in-package \"ACL2\")\n\
                (include-book \"std/util/define\" :dir :system)\n\
                (defxdoc lone-topic :short \"documented by itself\")\n\
                (define shown ((x natp)) :returns (y natp) :short \"a define\" \
                  :long \"<p>Described at length here.</p>\" (nfix x))\n\
                (defun undocumented-fn (x) x)\n";
    std::fs::write(&book_path, text).expect("write book");

    let mut d = Daemon::start(&acl2);
    let opened = d.request("open", json!({ "path": book_path.to_str().unwrap() }));
    let book = opened["book"].as_str().expect("book id").to_string();
    d.notify("update", json!({ "book": book, "text": text, "revision": 1 }));
    let events = d.events_until_settled();

    let unit = |i: usize| -> Value {
        events
            .iter()
            .filter(|(m, _)| m == "unit")
            .map(|(_, p)| p.clone())
            .find(|p| p["index"] == i)
            .unwrap_or_else(|| panic!("no unit {i}"))
    };

    // The standalone topic is nameable only through the split.
    let lone = unit(2);
    let names: Vec<String> = lone["names"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    assert!(names.iter().any(|n| n.eq_ignore_ascii_case("lone-topic")),
            "a defxdoc names its topic and records no event: {lone}");

    // Asking about a name says whether it is documented, and that answer is
    // independent of whether it introduced a rule. `lone-topic` introduced
    // none; `shown` introduced several.
    let mut ask = |names: Value| -> Vec<Value> {
        d.request("rules", json!({ "book": book, "names": names }))["rules"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    };
    // The *first* ask, deliberately: the index is built on demand, and
    // building it on the request that wants it is what makes this answer
    // complete. Armed for a later tick instead, this returned nothing and a
    // form's documentation showed up only if the user looked away and back.
    let got = ask(json!(["lone-topic", "shown", "undocumented-fn"]));
    let short = |n: &str| -> Option<String> {
        got.iter()
            .find(|r| r["name"].as_str().is_some_and(|s| s.eq_ignore_ascii_case(n)))
            .and_then(|r| r["short"].as_str().map(str::to_string))
    };
    assert_eq!(short("lone-topic").as_deref(), Some("documented by itself"),
               "documented without introducing a rule: {got:?}");
    assert_eq!(short("shown").as_deref(), Some("a define"), "{got:?}");
    assert_eq!(short("undocumented-fn"), None, "an undocumented name says nothing: {got:?}");

    // The body rides along with the summary, so the form view's card has
    // something to open onto without a second round trip. A topic written
    // with a `:short` and no `:long` sends none, which is what tells the card
    // it has nothing to open.
    let long = |n: &str| -> Option<String> {
        got.iter()
            .find(|r| r["name"].as_str().is_some_and(|s| s.eq_ignore_ascii_case(n)))
            .and_then(|r| r["long"].as_str().map(str::to_string))
    };
    assert!(long("shown").is_some_and(|l| l.contains("at length")),
            "a documented name carries its body: {got:?}");
    assert_eq!(long("lone-topic"), None, "a short with no long sends none: {got:?}");
    assert_eq!(long("undocumented-fn"), None, "and an undocumented name none: {got:?}");

    // Now the case the index cannot see by watching the include set: a topic
    // written into the file under the cursor. No dependency moves, so the
    // dependency baseline that retires the census says nothing has changed --
    // and the index went on answering from the world as it was when it was
    // built. What retires it is the topic count the probe reports on every
    // form.
    let more = format!("{text}(defxdoc added-later :short \"written just now\")\n");
    std::fs::write(&book_path, &more).expect("rewrite book");
    d.notify("update", json!({ "book": book, "text": more, "revision": 2 }));
    let _ = d.events_until_settled();
    let after = d.request("rules", json!({ "book": book, "names": ["added-later"] }))["rules"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(after.first().and_then(|r| r["short"].as_str()), Some("written just now"),
               "documentation written into this file is findable at once: {after:?}");
}
