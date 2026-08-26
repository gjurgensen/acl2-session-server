//! Integration tests for the wire itself: spawn the built server binary,
//! speak JSON-RPC over stdio, and assert on the JSON an agent actually
//! receives.
//!
//! The core's unit and live suites verify what the server *does*; nothing
//! else verifies what it *says*. Both findings of the first field session
//! lived entirely in this layer -- reply volume and value rendering -- and
//! the fixes (compact admitted records, `prove_ms`) are shapes a refactor
//! could silently regrow without any core test noticing. These pin them.
//!
//! Ignored by default because they need a real ACL2 -- all but the handshake
//! test, which never opens a session and so runs in CI. Run with:
//!
//! ```text
//! ACL2=/path/to/saved_acl2 cargo test -p acl2-session-mcp --test wire_live \
//!     -- --ignored --nocapture --test-threads=1
//! ```

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The ACL2 to drive, or `None` to skip. See `acl2-session/tests/live.rs`
/// for which version these suites expect.
fn acl2() -> Option<PathBuf> {
    std::env::var("ACL2").ok().map(PathBuf::from)
}

macro_rules! acl2_or_skip {
    () => {
        match acl2() {
            Some(a) => a,
            None => {
                eprintln!("ACL2 not set; skipping");
                return;
            }
        }
    };
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("acl2-wire-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("scratch dir");
    d
}

/// One server process and the reader that files its replies by id.
struct Server {
    child: Child,
    stdin: ChildStdin,
    replies: Arc<(Mutex<HashMap<u64, Value>>, Condvar)>,
    next_id: u64,
}

impl Server {
    /// Spawn the binary Cargo built for this workspace -- the actual product,
    /// not a harness around the library.
    fn start(acl2: &std::path::Path) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_acl2-session-mcp"))
            .args(["--default", acl2.to_str().expect("utf-8 image path")])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn server");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        let replies = Arc::new((Mutex::new(HashMap::new()), Condvar::new()));
        let filed = replies.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    let (map, cvar) = &*filed;
                    map.lock().unwrap().insert(id, v);
                    cvar.notify_all();
                }
            }
        });
        let mut s = Server { child, stdin, replies, next_id: 100 };
        let id = s.send(
            "initialize",
            json!({"protocolVersion": "2024-11-05", "capabilities": {},
                   "clientInfo": {"name": "wire-live", "version": "0"}}),
        );
        s.wait(id, Duration::from_secs(30));
        s.notify("notifications/initialized");
        s
    }

    fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{msg}").expect("write request");
        self.stdin.flush().expect("flush");
        id
    }

    fn notify(&mut self, method: &str) {
        let msg = json!({"jsonrpc": "2.0", "method": method});
        writeln!(self.stdin, "{msg}").expect("write notification");
        self.stdin.flush().expect("flush");
    }

    /// Fire a tool call without waiting -- for driving a second call while
    /// the first is still blocked on the session.
    fn send_tool(&mut self, name: &str, args: Value) -> u64 {
        self.send("tools/call", json!({"name": name, "arguments": args}))
    }

    fn wait(&mut self, id: u64, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        let (map, cvar) = &*self.replies;
        let mut guard = map.lock().unwrap();
        loop {
            if let Some(v) = guard.remove(&id) {
                return v;
            }
            let left = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("no reply to request {id} within {timeout:?}"));
            let (g, _) = cvar.wait_timeout(guard, left).unwrap();
            guard = g;
        }
    }

    /// The structured payload of a tool reply.
    fn payload(reply: Value) -> Value {
        let result = &reply["result"];
        if let Some(s) = result.get("structuredContent") {
            return s.clone();
        }
        // Older shape: JSON stuffed into the first text block.
        serde_json::from_str(result["content"][0]["text"].as_str().expect("text content"))
            .expect("parse text payload")
    }

    /// Call a tool and wait for its structured payload.
    fn tool(&mut self, name: &str, args: Value) -> Value {
        let id = self.send_tool(name, args);
        Self::payload(self.wait(id, Duration::from_secs(180)))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The compact admitted record: exactly these keys on a clean form, and the
/// unease fields only where something fired. `inner` and `time` must not
/// regrow -- they were measured at half of every clean record, read never.
#[test]
#[ignore = "needs ACL2"]
fn clean_admitted_records_are_compact_and_unease_surfaces() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("compact");
    let book = dir.join("b.lisp");
    std::fs::write(
        &book,
        "(in-package \"ACL2\")\n\
         (defun w-app (x y) (if (endp x) y (cons (car x) (w-app (cdr x) y))))\n\
         (defthm w-app-assoc\n\
           (equal (w-app (w-app a b) c) (w-app a (w-app b c))))\n\
         (defthm w-app-assoc-again\n\
           (equal (w-app (w-app a b) c) (w-app a (w-app b c)))\n\
           :hints ((\"Goal\" :use w-app-assoc)) :rule-classes nil)\n",
    )
    .unwrap();

    let mut s = Server::start(&acl2);
    let sid = s.tool("acl2_session_open", json!({"file": book}))["session_id"]
        .as_str()
        .expect("session id")
        .to_string();
    let o = s.tool("acl2_sync", json!({"session_id": sid, "deadline_seconds": 120}));
    let admitted = o["admitted"].as_array().expect("admitted array");
    assert_eq!(admitted.len(), 4, "{o}");

    // `in-package` writes nothing, so `names` is absent from it and present
    // on the two definers: the same omit-when-empty rule the unease fields
    // follow, which is what keeps a preamble of include-books cheap.
    let want: [&[&str]; 3] = [
        &["line_start", "output_ref"],
        &["line_start", "names", "output_ref"],
        &["line_start", "names", "output_ref"],
    ];
    for (rec, want) in admitted[..3].iter().zip(want) {
        let mut sorted: Vec<&str> =
            rec.as_object().unwrap().keys().map(String::as_str).collect();
        sorted.sort_unstable();
        assert_eq!(sorted, want, "a clean record carries only what it has: {rec}");
    }
    assert_eq!(admitted[1]["names"][0].as_str(), Some("w-app"), "{}", admitted[1]);
    assert_eq!(
        admitted[2]["names"][0].as_str(),
        Some("w-app-assoc"),
        "the name is read from the source, not the world: {}",
        admitted[2]
    );
    // These forms run in single-digit milliseconds, so the threshold holds
    // them back; a book slow enough to notice is what turns it on.
    for rec in &admitted[..3] {
        assert!(rec.get("elapsed_ms").is_none(), "a fast form reports no time: {rec}");
    }
    let hinted = &admitted[3];
    assert_eq!(
        hinted["hint_events"][0].as_str(),
        Some("(:use w-app-assoc)"),
        "the fired hint surfaces on the form that fired it: {hinted}"
    );
    let kinds: Vec<&str> =
        hinted["warnings"].as_array().expect("warnings is a JSON list")
            .iter().map(|w| w.as_str().unwrap_or("")).collect();
    assert!(kinds.contains(&"Use"),
            "and so does its warning, as list entries rather than ACL2's \
             printed list: {hinted}");
    for rec in admitted {
        assert!(rec.get("inner").is_none(), "inner must not regrow: {rec}");
        assert!(rec.get("time").is_none(), "nor raw time tuples: {rec}");
        assert!(rec.get("index").is_none(), "nor the form index: {rec}");
        assert!(rec.get("output_lines").is_none(), "nor the log's length: {rec}");
        assert!(rec.get("line_end").is_none(), "nor the closing line: {rec}");
        assert!(rec.get("output_ref").is_some(), "every record keeps its handle");
    }
    s.tool("acl2_session_close", json!({"session_id": sid}));
}

/// A failure record's numbers are named integers, not rational tuples, and
/// the envelope names the kind of thing that failed.
#[test]
#[ignore = "needs ACL2"]
fn a_failure_reports_named_numbers_not_rationals() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("failure");
    let book = dir.join("f.lisp");
    std::fs::write(
        &book,
        "(in-package \"ACL2\")\n\
         (defun w-app (x y) (if (endp x) y (cons (car x) (w-app (cdr x) y))))\n\
         (defthm w-bogus (equal (w-app a b) (w-app b a)))\n",
    )
    .unwrap();

    let mut s = Server::start(&acl2);
    let sid = s.tool("acl2_session_open", json!({"file": book}))["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let o = s.tool("acl2_sync", json!({"session_id": sid, "deadline_seconds": 120}));
    let f = &o["failed"];
    assert_eq!(f["status"].as_str(), Some("proof_failure"), "{o}");

    let inner0 = &f["inner"][0];
    assert_eq!(inner0["kind"].as_str(), Some("defthm"), "{inner0}");
    assert!(inner0["prove_ms"].is_u64(), "prove time is a named integer: {inner0}");
    assert!(inner0["prover_steps"].is_u64(), "{inner0}");
    assert!(inner0["abort_causes"].is_string(), "{inner0}");
    assert!(
        !serde_json::to_string(f).unwrap().contains("\"time\""),
        "no raw time tuples anywhere in a failure: {f}"
    );

    let top = &f["checkpoints"]["top"][0];
    assert!(top["term"].is_string(), "{top}");
    let goal = top["goal"].as_str().unwrap_or("");
    assert!(
        goal.starts_with("Goal") || goal.starts_with("Subgoal"),
        "checkpoints carry the name mode:goal takes: {top}"
    );
    s.tool("acl2_session_close", json!({"session_id": sid}));
}

/// The calls that exist to escape a blocked session actually escape it, on
/// the wire: status and health answer promptly from the stream, a completed
/// form's log stays readable, interrupt lands within the abort-wakeup regime
/// and reports its envelope, and the blocked sync itself comes back as
/// in-flight.
#[test]
#[ignore = "needs ACL2"]
fn a_blocked_session_answers_status_health_output_and_interrupt() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("blocked");
    let book = dir.join("grind.lisp");
    let vars: Vec<String> = (1..=20).map(|i| format!("b{i}")).collect();
    let mut chain = vars.last().unwrap().clone();
    for v in vars.iter().rev().skip(1) {
        chain = format!("(xor {v} {chain})");
    }
    let mut rev_chain = vars.first().unwrap().clone();
    for v in vars.iter().skip(1) {
        rev_chain = format!("(xor {v} {rev_chain})");
    }
    let hyps: Vec<String> = vars.iter().map(|v| format!("(booleanp {v})")).collect();
    std::fs::write(
        &book,
        format!(
            "(in-package \"ACL2\")\n\
             (defun wire-mark (x) (cons :wire-mark x))\n\
             (defthm xor-grind (implies (and {}) (iff {chain} {rev_chain}))\n\
               :rule-classes nil)\n",
            hyps.join(" ")
        ),
    )
    .unwrap();

    let mut s = Server::start(&acl2);
    let sid = s.tool("acl2_session_open", json!({"file": book}))["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Bank a completed form's handle before anything blocks: sync only
    // through the defun on line 2.
    let o = s.tool("acl2_sync", json!({"session_id": sid, "through_line": 2,
                                       "deadline_seconds": 120}));
    let banked = o["admitted"][1]["output_ref"].as_str().expect("a banked ref").to_string();

    // A long-deadline sync we do not wait for: the session lock is now held.
    let sync_id = s.send_tool("acl2_sync", json!({"session_id": sid, "deadline_seconds": 120}));
    std::thread::sleep(Duration::from_secs(3));

    let t = Instant::now();
    let st = s.tool("acl2_status", json!({"session_id": sid}));
    assert!(t.elapsed() < Duration::from_secs(5),
            "status must not wait behind the sync deadline: {:?}", t.elapsed());
    assert_eq!(st["running"].as_bool(), Some(true), "{st}");
    assert!(st["bytes_so_far"].as_u64().unwrap_or(0) > 0,
            "the stream serves live progress: {st}");

    // Reading a *completed* form's log goes through the shared store, not
    // the book lock, so it answers while the sync grinds.
    let t = Instant::now();
    let out = s.tool("acl2_output", json!({"session_id": sid, "output_ref": banked,
                                           "mode": "all"}));
    assert!(t.elapsed() < Duration::from_secs(5),
            "output must not wait behind the sync deadline: {:?}", t.elapsed());
    assert!(out["text"].as_str().unwrap_or("").to_lowercase().contains("wire-mark"),
            "and it is the banked form's own log: {out}");

    let t = Instant::now();
    let h = s.tool("acl2_health", json!({"session_id": sid}));
    assert!(t.elapsed() < Duration::from_secs(5), "health must answer promptly");
    assert_eq!(h["state"].as_str(), Some("busy"), "{h}");

    let t = Instant::now();
    let ir = s.tool("acl2_interrupt", json!({"session_id": sid}));
    assert!(
        t.elapsed() < Duration::from_secs(15),
        "interrupt must land in the abort-wakeup regime, not the deadline: {:?}",
        t.elapsed()
    );
    assert_eq!(ir["ok"].as_bool(), Some(true), "{ir}");
    let env = &ir["interrupted"];
    assert!(
        env["abort_cause"].as_str().unwrap_or("").to_lowercase().contains("interrupt"),
        "the envelope names its cause: {ir}"
    );
    assert!(env["prove_ms"].is_u64(), "grind time as a named integer: {env}");
    assert!(env["output_ref"].is_string(), "the partial capture is attached: {env}");

    // The blocked call hands back the standard in-flight shape shortly after.
    let sy = Server::payload(s.wait(sync_id, Duration::from_secs(20)));
    assert!(sy.get("failed").is_none(), "an abort is not a failure: {sy}");
    assert!(sy["in_flight"].is_object() || sy["admitted"].as_array().is_some_and(|a| !a.is_empty()),
            "the sync reports where it stood: {sy}");

    let h = s.tool("acl2_health", json!({"session_id": sid}));
    assert_eq!(h["state"].as_str(), Some("ready"), "usable again: {h}");
    s.tool("acl2_session_close", json!({"session_id": sid}));
}

/// The open reply says which build answered.
///
/// A client once spent days reporting behaviour fixed the day before,
/// against an installed binary that had never been restarted; no reply could
/// have revealed it. This one can.
/// `acl2_induction` answers on a handle, per event, and says when it cannot.
///
/// The per-event grouping is the assertion that matters. Both theorems here
/// induct, both call their goal `*1`, and both are inside one `progn` -- the
/// exact shape that made the deleted log parser attach every scheme to the
/// wrong theorem.
#[test]
#[ignore = "needs ACL2"]
fn induction_is_reported_per_event_on_a_handle() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("induction");
    let book = dir.join("b.lisp");
    std::fs::write(
        &book,
        "(in-package \"ACL2\")\n\
         (defun i-app (x y) (if (consp x) (cons (car x) (i-app (cdr x) y)) y))\n\
         (defun i-rev (x) (if (consp x) (i-app (i-rev (cdr x)) (cons (car x) nil)) nil))\n\
         (progn\n\
           (defthm i-tl-rev (true-listp (i-rev x)))\n\
           (defthm i-app-assoc\n\
             (equal (i-app (i-app x y) z) (i-app x (i-app y z)))))\n",
    )
    .unwrap();

    let mut s = Server::start(&acl2);
    let sid = s.tool("acl2_session_open", json!({"file": book}))["session_id"]
        .as_str()
        .expect("session id")
        .to_string();
    let o = s.tool("acl2_sync", json!({"session_id": sid, "deadline_seconds": 180}));
    let admitted = o["admitted"].as_array().expect("admitted array").clone();

    // The `defun`s induct nothing, and that is an empty answer rather than an
    // error or an absent key.
    let defun_ref = admitted[1]["output_ref"].as_str().expect("ref").to_string();
    let none = s.tool("acl2_induction", json!({"session_id": sid, "output_ref": defun_ref}));
    assert_eq!(none["events"].as_array().map(Vec::len), Some(0), "{none}");
    assert_eq!(none["supported"].as_bool(), Some(true),
               "an empty answer from a capable image still says it is capable: {none}");

    let progn_ref = admitted.last().expect("the progn")["output_ref"]
        .as_str()
        .expect("ref")
        .to_string();
    let ind = s.tool("acl2_induction", json!({"session_id": sid, "output_ref": progn_ref}));
    let events = ind["events"].as_array().expect("events").clone();
    assert_eq!(events.len(), 2, "both theorems inducted: {ind}");

    let by = |n: &str| {
        events
            .iter()
            .find(|e| e["event"].as_str() == Some(n))
            .unwrap_or_else(|| panic!("no {n} in {ind}"))
            .clone()
    };

    // Each theorem's own scheme, though both goals are called `*1`.
    let a = by("i-app-assoc");
    let a = &a["inductions"];
    assert_eq!(a[0]["goal"].as_str(), Some("*1"), "{ind}");
    assert_eq!(a[0]["suggested"].as_str(), Some("(i-app x y)"), "{ind}");
    assert_eq!(a[0]["runes"][0].as_str(), Some("(:induction i-app)"), "{ind}");

    let t = by("i-tl-rev");
    let t = &t["inductions"];
    assert_eq!(t.as_array().map(Vec::len), Some(2), "*1 and its sub-induction: {ind}");
    assert_eq!(t[0]["goal"].as_str(), Some("*1.1"), "{ind}");
    assert_eq!(t[1]["suggested"].as_str(), Some("(i-rev x)"), "{ind}");

    // An ordinary scheme arrives whole, so neither cut is announced.
    assert!(t[1]["scheme"].as_str().unwrap_or("").contains("(:p x)"), "{ind}");
    assert!(t[1].get("scheme_chars").is_none(), "not cut to length: {ind}");
    assert!(t[1].get("scheme_cases").is_none(), "not cut to one case: {ind}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "needs ACL2"]
fn the_open_reply_identifies_the_build() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("stamp");
    let book = dir.join("s.lisp");
    std::fs::write(&book, "(in-package \"ACL2\")\n").unwrap();

    let mut s = Server::start(&acl2);
    let o = s.tool("acl2_session_open", json!({"file": book}));
    let server = o["server"].as_str().expect("the open reply names the server");
    assert!(server.starts_with(env!("CARGO_PKG_VERSION")),
            "it starts with the version: {server}");
    assert!(server.contains("+g"),
            "and carries the commit, which is what makes staleness visible: {server}");
    assert!(server.len() < 32, "one short string, not a banner: {server}");
    s.tool("acl2_session_close", json!({"session_id": o["session_id"].as_str().unwrap()}));
}

/// Retraction is available over the wire, and only when asked for.
#[test]
#[ignore = "needs ACL2"]
fn retraction_is_asked_for_never_inferred() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("wireretract");
    let book = dir.join("r.lisp");
    // Forms on lines 1, 2, 3.
    std::fs::write(
        &book,
        "(in-package \"ACL2\")\n\
         (defun wr-f (x) x)\n\
         (defthm wr-1 (equal (wr-f x) x))\n",
    )
    .unwrap();

    let mut s = Server::start(&acl2);
    let sid = s.tool("acl2_session_open", json!({"file": book}))["session_id"]
        .as_str().unwrap().to_string();
    let o = s.tool("acl2_sync", json!({"session_id": sid, "deadline_seconds": 120}));
    assert_eq!(o["admitted"].as_array().unwrap().len(), 3, "{o}");

    // A backward target alone leaves the frontier where it is.
    let o = s.tool("acl2_sync", json!({"session_id": sid, "through_line": 2,
                                       "deadline_seconds": 120}));
    assert_eq!(o["rolled_back"].as_u64(), Some(0), "a target alone never retracts: {o}");

    // The flag moves it back.
    let o = s.tool("acl2_sync", json!({"session_id": sid, "through_line": 2,
                                       "retract": true, "deadline_seconds": 120}));
    assert_eq!((o["reused"].as_u64(), o["rolled_back"].as_u64()),
               (Some(2), Some(1)), "{o}");

    // And it needs somewhere to retract to, rather than silently doing nothing.
    let id = s.send_tool("acl2_sync", json!({"session_id": sid, "retract": true,
                                             "deadline_seconds": 120}));
    let reply = s.wait(id, Duration::from_secs(60));
    let complaint = serde_json::to_string(&reply).unwrap();
    assert!(complaint.contains("through_line"),
            "retract without a target is refused, and says why: {complaint}");

    s.tool("acl2_session_close", json!({"session_id": sid}));
}

/// A changed dependency reaches the wire, and the opt-out is honoured.
#[test]
#[ignore = "needs ACL2"]
fn a_changed_dependency_reaches_the_wire() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("wiredeps");
    let dep = dir.join("wd.lisp");
    let main = dir.join("wmain.lisp");
    std::fs::write(&dep, "(in-package \"ACL2\")\n(defun wd-id (x) x)\n").unwrap();
    std::fs::write(&main, "(in-package \"ACL2\")\n(include-book \"wd\")\n").unwrap();

    let mut s = Server::start(&acl2);
    // Certify the dependency through its own session, as a client would.
    let dsid = s.tool("acl2_session_open", json!({"file": dep}))["session_id"]
        .as_str().unwrap().to_string();
    let c = s.tool("acl2_certify", json!({"session_id": dsid, "deadline_seconds": 300}));
    assert_eq!(c["outcome"].as_str(), Some("certified"), "{c}");
    s.tool("acl2_session_close", json!({"session_id": dsid}));

    let sid = s.tool("acl2_session_open", json!({"file": main}))["session_id"]
        .as_str().unwrap().to_string();
    let o = s.tool("acl2_sync", json!({"session_id": sid, "deadline_seconds": 300}));
    assert!(o.get("stale_includes").is_none(), "nothing stale yet: {o}");

    // Change the dependency and recertify it.
    std::fs::write(&dep,
        "(in-package \"ACL2\")\n(defun wd-id (x) x)\n         (defthm wd-id-is-id (equal (wd-id x) x))\n").unwrap();
    let dsid = s.tool("acl2_session_open", json!({"file": dep}))["session_id"]
        .as_str().unwrap().to_string();
    s.tool("acl2_certify", json!({"session_id": dsid, "deadline_seconds": 300}));
    s.tool("acl2_session_close", json!({"session_id": dsid}));

    // Asked to ignore it: named, nothing undone.
    let o = s.tool("acl2_sync", json!({"session_id": sid, "ignore_stale_includes": true,
                                       "deadline_seconds": 300}));
    let stale = o["stale_includes"].as_array().expect("named on the wire");
    assert_eq!(stale.len(), 1, "{o}");
    assert!(stale[0]["path"].as_str().unwrap_or("").ends_with("wd.lisp"), "{o}");
    // Line 2 is the `include-book`, and a line is what `through_line` takes:
    // the reply hands back the number you would act on.
    assert_eq!(stale[0]["line"].as_u64(), Some(2), "attributed to the include: {o}");
    assert!(stale[0].get("unit").is_none(), "and not as a form index: {o}");
    assert_eq!(o["rolled_back"].as_u64(), Some(0), "and nothing undone: {o}");

    // By default it folds into the diff.
    let o = s.tool("acl2_sync", json!({"session_id": sid, "deadline_seconds": 300}));
    assert!(o["rolled_back"].as_u64().unwrap_or(0) >= 1, "the include is re-admitted: {o}");
    assert!(o["stale_includes"].is_array(), "and the reply says why: {o}");

    let o = s.tool("acl2_sync", json!({"session_id": sid, "deadline_seconds": 300}));
    assert!(o.get("stale_includes").is_none(), "settled: {o}");
    s.tool("acl2_session_close", json!({"session_id": sid}));
}

/// A certification wait holds the book's lock like any blocked call, and the
/// same escape hatches must work: status and health answer from the build
/// stamp, and an interrupt cuts through the wait at once and kills the build.
#[test]
#[ignore = "needs ACL2"]
fn a_blocked_certification_answers_status_health_and_interrupt() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("certblock");
    let book = dir.join("slowcert.lisp");
    // The include makes the scratch book's own certification take seconds --
    // its dependency is already certified, so nothing outside the scratch
    // directory is built or written.
    std::fs::write(
        &book,
        "(in-package \"ACL2\")\n\
         (include-book \"std/strings/top\" :dir :system)\n\
         (defun certblock-f (x) x)\n",
    )
    .unwrap();

    let mut s = Server::start(&acl2);
    let sid = s.tool("acl2_session_open", json!({"file": book}))["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let cert_id = s.send_tool("acl2_certify", json!({"session_id": sid, "deadline_seconds": 60}));
    std::thread::sleep(Duration::from_secs(1));

    let t = Instant::now();
    let st = s.tool("acl2_status", json!({"session_id": sid}));
    assert!(t.elapsed() < Duration::from_secs(5),
            "status must not wait behind the certify deadline: {:?}", t.elapsed());
    assert!(st["build"]["book"].as_str().unwrap_or("").contains("slowcert"),
            "the build stamp names the book: {st}");

    let t = Instant::now();
    let h = s.tool("acl2_health", json!({"session_id": sid}));
    assert!(t.elapsed() < Duration::from_secs(5), "health must answer promptly");
    assert_eq!(h["state"].as_str(), Some("busy"), "a building session is busy: {h}");

    let t = Instant::now();
    let ir = s.tool("acl2_interrupt", json!({"session_id": sid}));
    assert!(t.elapsed() < Duration::from_secs(15),
            "interrupt must cut through the certify wait: {:?}", t.elapsed());
    assert_eq!(ir["ok"].as_bool(), Some(true), "{ir}");

    // The blocked certify call hands back still_running promptly; its build
    // was killed by the interrupt it yielded to.
    let cr = Server::payload(s.wait(cert_id, Duration::from_secs(20)));
    assert_eq!(cr["outcome"].as_str(), Some("still_running"), "{cr}");

    let h = s.tool("acl2_health", json!({"session_id": sid}));
    assert_eq!(h["state"].as_str(), Some("ready"), "usable again: {h}");
    s.tool("acl2_session_close", json!({"session_id": sid}));
}

/// A failed eval's envelope arrives on the wire, and a successful one
/// carries nothing.
#[test]
#[ignore = "needs ACL2"]
fn a_failed_evals_envelope_arrives_on_the_wire() {
    let acl2 = acl2_or_skip!();
    let dir = scratch("evalwire");
    let book = dir.join("e.lisp");
    // A conjecture that fails with *term-shaped* checkpoints. A false
    // equality that simplifies straight to the empty clause would leave only
    // a checkpoint whose term prettyprints as literal `nil`, which the slot
    // filter drops by design (the n_top count still says it existed).
    std::fs::write(
        &book,
        "(in-package \"ACL2\")\n\
         (defun w-app (x y) (if (endp x) y (cons (car x) (w-app (cdr x) y))))\n",
    )
    .unwrap();

    let mut s = Server::start(&acl2);
    let sid = s.tool("acl2_session_open", json!({"file": book}))["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    s.tool("acl2_sync", json!({"session_id": sid, "deadline_seconds": 120}));

    let r = s.tool(
        "acl2_eval",
        json!({"session_id": sid,
               "form": "(acl2::thm (acl2::equal (w-app a b) (w-app b a)))",
               "deadline_seconds": 120}),
    );
    assert_eq!(r["outcome"].as_str(), Some("failed"), "{r}");
    let f = &r["failed"];
    assert_eq!(f["status"].as_str(), Some("proof_failure"), "{r}");
    assert!(f["checkpoints"]["top"].as_array().is_some_and(|a| !a.is_empty())
            || f["checkpoints"]["sub"].as_array().is_some_and(|a| !a.is_empty()),
            "a failed experiment hands over its checkpoints: {f}");

    let r = s.tool(
        "acl2_eval",
        json!({"session_id": sid, "form": "(acl2::+ 1 1)", "deadline_seconds": 60}),
    );
    assert_eq!(r["outcome"].as_str(), Some("ok"), "{r}");
    assert!(r.get("failed").is_none(), "success carries no envelope: {r}");
    s.tool("acl2_session_close", json!({"session_id": sid}));
}

/// The handshake identifies *this* server.
///
/// `ServerInfo::default()` fills `server_info` from rmcp's own build
/// environment -- its `env!`s expand inside that crate -- so leaving the field
/// alone introduces us to every client as "rmcp <its version>". The handshake
/// is where a client records what it is talking to, which is the question the
/// build stamp exists to answer, and nothing else in this suite looks at it.
///
/// Needs no ACL2: the image is validated as a path and no session is opened.
#[test]
fn the_handshake_names_this_server() {
    let dir = scratch("handshake");
    let image = dir.join("saved_acl2");
    std::fs::write(&image, "").expect("dummy image");

    let mut child = Command::new(env!("CARGO_BIN_EXE_acl2-session-mcp"))
        .args(["--default", image.to_str().expect("utf-8 image path")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn server");
    let mut stdin = child.stdin.take().expect("child stdin");
    let mut out = BufReader::new(child.stdout.take().expect("child stdout"));
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                     "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                                "clientInfo": {"name": "wire-live", "version": "0"}}});
    writeln!(stdin, "{msg}").expect("write initialize");
    stdin.flush().expect("flush");

    let mut line = String::new();
    out.read_line(&mut line).expect("initialize reply");
    let v: Value = serde_json::from_str(&line).expect("json reply");
    let info = &v["result"]["serverInfo"];
    assert_eq!(info["name"].as_str(), Some(env!("CARGO_PKG_NAME")),
               "rmcp's identity, not ours: {v}");
    assert!(info["version"].as_str().is_some_and(|s| s.starts_with(env!("CARGO_PKG_VERSION"))),
            "the version carries our build stamp: {v}");

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
