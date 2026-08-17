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
    let bsrc = std::fs::read_to_string(bap).expect("read axioms");
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
    let psrc = std::fs::read_to_string(pp).expect("read source");
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
    let gsrc = std::fs::read_to_string(gp).expect("read bstar");
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
        let f = d.request("rules", json!({ "book": book, "name": "ru-app" }));
        assert_eq!(f["found"], true, "answered mid-proof, not declined: {f}");
        assert_eq!(f["symbol_class"], ":ideal", "guards are not verified here: {f}");
        assert!(f["justification"].as_str().unwrap_or("").contains("acl2-count"),
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
        let t = d.request("rules", json!({ "book": book, "name": "true-listp-of-ru-app" }));
        let rw = t["runes"].as_array().expect("runes").iter()
            .find(|r| r["class"] == ":rewrite").expect("a rewrite rune");
        let lhs = rw["fields"].as_array().unwrap().iter()
            .find(|x| x["key"] == ":lhs").expect("an lhs")["value"].as_str().unwrap_or("");
        assert!(lhs.contains("true-listp (ru-app"), "the derived lhs: {lhs}");

        // A forward-chaining rule: its trigger term.
        let fc = d.request("rules", json!({ "book": book, "name": "ru-fc" }));
        let r = fc["runes"].as_array().expect("runes").iter()
            .find(|r| r["class"] == ":forward-chaining").expect("an fc rune");
        let trig = r["fields"].as_array().unwrap().iter()
            .find(|x| x["key"] == ":trigger").expect("a trigger")["value"].as_str().unwrap_or("");
        assert!(trig.contains("consp"), "the trigger term: {trig}");

        // A name this file never introduced is simply not found.
        let none = d.request("rules", json!({ "book": book, "name": "binary-append" }));
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
