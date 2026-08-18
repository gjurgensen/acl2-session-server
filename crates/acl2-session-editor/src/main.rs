#![forbid(unsafe_code)]

//! Editor daemon over a live ACL2 session.
//!
//! JSON-RPC 2.0 over stdio with `Content-Length` framing -- the wire
//! jsonrpc.el and vscode-jsonrpc speak natively. Snapshot updates arrive as
//! notifications and state flows back as notifications; only
//! queries are requests. One driver thread per book owns its session; this
//! loop answers `output`/`status`/`interrupt` from the registry's lock-free
//! handles, so a grinding proof never blocks a query.

mod driver;
mod wire;

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use acl2_session::{
    Book, ImagePolicy, OutputRef, OutputStore, Registry, SessionControl, SessionId, SyncMode,
};
use serde_json::{json, Value};

use driver::{Cmd, Driver};

/// How many frames may be waiting to go out before stale ones are dropped.
///
/// Reached only when the client has stopped reading, which is the condition
/// this whole queue exists to survive; a client keeping up never sees it.
const OUT_BACKLOG: usize = 256;

/// The shared stdout, one frame at a time, writable from any thread.
///
/// Frames are queued and written by a thread of their own, and this is not
/// tidiness -- writing under the lock deadlocks the daemon. The old shape
/// took a mutex on stdout and then wrote *while holding it*. stdout is a
/// socket to the client, with a fixed buffer; when the client stopped
/// draining it, the write blocked in the kernel with the mutex held, and the
/// main loop -- the only thread reading stdin -- blocked trying to take that
/// same mutex to send a reply. Nothing recovered. Every book went dead at
/// once, new sessions could not be opened, and the process sat alive and
/// silent. Observed in the field, and confirmed by its thread states:
///
///   main    S futex_wait_queue        (waiting for the stdout mutex)
///   driver  S sock_alloc_send_pskb    (blocked writing, holding it)
///
/// So the writer thread holds no lock while it writes, and the queue is
/// bounded rather than unbounded: a client that never reads again must not
/// turn into a daemon that grows without limit. When the backlog is full,
/// `tail` frames are dropped oldest-first -- they are display, and a newer
/// one supersedes them -- and everything else is kept, because a dropped
/// reply is a client waiting for ever.
#[derive(Clone)]
pub struct Out(Arc<OutQueue>);

struct OutQueue {
    frames: Mutex<std::collections::VecDeque<(bool, Vec<u8>)>>,
    ready: std::sync::Condvar,
}

impl Out {
    /// Start the writer thread that owns stdout.
    fn new() -> Out {
        let q = Arc::new(OutQueue {
            frames: Mutex::new(std::collections::VecDeque::new()),
            ready: std::sync::Condvar::new(),
        });
        let mine = q.clone();
        std::thread::Builder::new()
            .name("acl2-session-out".into())
            .spawn(move || {
                let mut w = io::stdout();
                loop {
                    let frame = {
                        let mut g = mine.frames.lock().expect("out queue");
                        while g.is_empty() {
                            g = mine.ready.wait(g).expect("out queue");
                        }
                        g.pop_front().expect("non-empty")
                    };
                    // Outside the lock, on purpose: this is the call that
                    // blocks when the client stops reading.
                    if w.write_all(&frame.1).is_err() || w.flush().is_err() {
                        return;
                    }
                }
            })
            .expect("spawn stdout writer");
        Out(q)
    }

    fn send(&self, v: &Value) {
        self.push(v, false);
    }

    /// Queue a frame. `droppable` marks one that may be discarded under
    /// backlog rather than held.
    fn push(&self, v: &Value, droppable: bool) {
        let s = v.to_string();
        let frame = format!("Content-Length: {}\r\n\r\n{s}", s.len()).into_bytes();
        let mut g = self.0.frames.lock().expect("out queue");
        if g.len() >= OUT_BACKLOG {
            // Drop the oldest droppable frame to make room. If there is none,
            // the backlog is all replies and events that matter, so the frame
            // is queued anyway: growing is bad, but lying to the client about
            // what its session did is worse.
            if let Some(i) = g.iter().position(|(d, _)| *d) {
                g.remove(i);
            }
        }
        g.push_back((droppable, frame));
        self.0.ready.notify_one();
    }

    pub fn notify(&self, method: &str, params: &impl serde::Serialize) {
        // A `tail` is the newest slice of a running form's output. If the
        // client is too far behind to have read the last one, it has no use
        // for it at all -- the next carries the same ground.
        let droppable = method == "tail" || method == "cert_tail";
        self.push(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": serde_json::to_value(params).expect("serialize params"),
        }), droppable);
    }

    /// Wait, briefly, for queued frames to reach the client.
    ///
    /// Writes used to complete before `send` returned, so anything the daemon
    /// said on its way out had been said by the time it exited. Queuing broke
    /// that: without this, the last `settled` of a shutdown can still be in
    /// the queue when the process ends, and the client is left waiting for an
    /// answer that was written but never flushed. Bounded, because a client
    /// that has stopped reading is exactly why we might be shutting down.
    pub fn drain(&self, limit: std::time::Duration) {
        let until = std::time::Instant::now() + limit;
        while std::time::Instant::now() < until {
            if self.0.frames.lock().expect("out queue").is_empty() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    pub fn reply(&self, id: &Value, result: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    pub fn error(&self, id: &Value, code: i64, message: String) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }));
    }
}

/// What the protocol loop keeps per book: the driver's inbox and the two
/// handles that answer queries without the book lock.
struct BookHandle {
    tx: Sender<Cmd>,
    control: Arc<SessionControl>,
    outputs: Arc<Mutex<OutputStore>>,
    /// Fallback revision counter for clients that send none.
    auto_rev: AtomicU64,
    /// Joined at shutdown, so daemon exit always runs the driver's `Book`
    /// drop -- the thing that kills the ACL2 child.
    driver: std::thread::JoinHandle<()>,
    /// The book's path, so an `update` carrying no text can read it.
    path: PathBuf,
}

enum RequestError {
    UnknownMethod,
    Failed(String),
}

/// What a request produced.
///
/// Most methods are answered from the registry's lock-free handles and reply
/// here. `eval` cannot: the driver thread owns the session, so the request's
/// id travels with the command and the driver sends the reply whenever the
/// form finishes -- which may be after a proof it queued behind.
enum Answer {
    Now(Value),
    ByDriver,
}

struct Daemon {
    registry: Mutex<Registry>,
    books: Mutex<HashMap<String, BookHandle>>,
    /// Drivers of closed books that may still be unwinding.
    ///
    /// `close` takes a book out of the map, so the EOF sweep can no longer
    /// see it -- and dropping its `JoinHandle` detaches the thread rather
    /// than waiting for it, while the driver still has to notice the abort,
    /// leave its loop and drop its `Book`. That drop is what kills ACL2. An
    /// editor shutting down closes its books and ends the daemon in the same
    /// breath, so the handles have to be kept somewhere the sweep still
    /// reaches or the process exits mid-unwind and the child is left running.
    closing: Mutex<Vec<std::thread::JoinHandle<()>>>,
    out: Out,
}

impl Daemon {
    fn handle_of<T>(&self, book: &str, f: impl FnOnce(&BookHandle) -> T) -> Result<T, String> {
        let books = self.books.lock().expect("books lock");
        books.get(book).map(f).ok_or_else(|| format!("unknown book {book:?}"))
    }

    fn request(&self, id: &Value, method: &str, params: Value) -> Result<Answer, RequestError> {
        fn args<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RequestError> {
            serde_json::from_value(params).map_err(|e| RequestError::Failed(e.to_string()))
        }
        let fail = RequestError::Failed;
        let now = |r: Result<Value, String>| r.map(Answer::Now).map_err(RequestError::Failed);
        match method {
            "open" => now(self.open(args(params)?)),
            "close" => {
                let p: wire::BookParams = args(params)?;
                now(self.close(&p.book))
            }
            "output" => now(self.output(args(params)?)),
            "induction" => now(self.induction(args(params)?)),
            "status" => {
                let p: wire::BookParams = args(params)?;
                now(self.status(&p.book))
            }
            "eval" => {
                let p: wire::EvalParams = args(params)?;
                let id = id.clone();
                self.handle_of(&p.book, |h| {
                    h.tx.send(Cmd::Eval { id, form: p.form }).map_err(|e| e.to_string())
                })
                .map_err(fail)?
                .map_err(fail)?;
                Ok(Answer::ByDriver)
            }
            // Answered by the driver like `eval`, because it needs the
            // session -- but never queued behind one: see `Driver::describe`.
            "describe" => {
                let p: wire::DescribeParams = args(params)?;
                let id = id.clone();
                self.handle_of(&p.book, |h| {
                    h.tx.send(Cmd::Describe { id, name: p.name }).map_err(|e| e.to_string())
                })
                .map_err(fail)?
                .map_err(fail)?;
                Ok(Answer::ByDriver)
            }
            "definition" => {
                let p: wire::DefinitionParams = args(params)?;
                let id = id.clone();
                self.handle_of(&p.book, |h| {
                    h.tx.send(Cmd::Definition { id, name: p.name }).map_err(|e| e.to_string())
                })
                .map_err(fail)?
                .map_err(fail)?;
                Ok(Answer::ByDriver)
            }
            "links" => {
                let p: wire::BookParams = args(params)?;
                let id = id.clone();
                self.handle_of(&p.book, |h| {
                    h.tx.send(Cmd::Links { id }).map_err(|e| e.to_string())
                })
                .map_err(fail)?
                .map_err(fail)?;
                Ok(Answer::ByDriver)
            }
            "complete" => {
                let p: wire::CompleteParams = args(params)?;
                let id = id.clone();
                self.handle_of(&p.book, |h| {
                    h.tx.send(Cmd::Complete { id, prefix: p.prefix }).map_err(|e| e.to_string())
                })
                .map_err(fail)?
                .map_err(fail)?;
                Ok(Answer::ByDriver)
            }
            "symbols" => {
                let p: wire::BookParams = args(params)?;
                let id = id.clone();
                self.handle_of(&p.book, |h| {
                    h.tx.send(Cmd::Symbols { id }).map_err(|e| e.to_string())
                })
                .map_err(fail)?
                .map_err(fail)?;
                Ok(Answer::ByDriver)
            }
            // Served from the scrape, so unlike `describe` it is never
            // declined for a busy session: see `Driver::rules`.
            "rules" => {
                let p: wire::RulesParams = args(params)?;
                let id = id.clone();
                self.handle_of(&p.book, |h| {
                    h.tx.send(Cmd::Rules { id, names: p.names }).map_err(|e| e.to_string())
                })
                .map_err(fail)?
                .map_err(fail)?;
                Ok(Answer::ByDriver)
            }
            "doc" => {
                let p: wire::DocParams = args(params)?;
                let id = id.clone();
                self.handle_of(&p.book, |h| {
                    h.tx.send(Cmd::Doc { id, name: p.name }).map_err(|e| e.to_string())
                })
                .map_err(fail)?
                .map_err(fail)?;
                Ok(Answer::ByDriver)
            }
            "certify" => {
                let p: wire::CertifyParams = args(params)?;
                let id = id.clone();
                self.handle_of(&p.book, |h| {
                    h.tx.send(Cmd::Certify { id, jobs: p.jobs }).map_err(|e| e.to_string())
                })
                .map_err(fail)?
                .map_err(fail)?;
                Ok(Answer::ByDriver)
            }
            "interrupt" => {
                let p: wire::BookParams = args(params)?;
                self.handle_of(&p.book, |h| {
                    // Wake any blocked call first, so the driver reaches the
                    // Pause promptly; the escalation runs in the driver.
                    h.control.request_abort();
                    let _ = h.tx.send(Cmd::Pause);
                })
                .map_err(fail)?;
                Ok(Answer::Now(json!({})))
            }
            "images" => {
                let reg = self.registry.lock().expect("registry lock");
                let p = reg.policy();
                Ok(Answer::Now(
                    serde_json::to_value(wire::ImagesReply {
                        images: p.available().iter().map(|x| x.display().to_string()).collect(),
                        default: p.default_path().map(|x| x.display().to_string()),
                    })
                    .expect("serialize"),
                ))
            }
            _ => Err(RequestError::UnknownMethod),
        }
    }

    fn open(&self, p: wire::OpenParams) -> Result<Value, String> {
        let path = PathBuf::from(&p.path);
        let image = p.image.as_ref().map(PathBuf::from);
        let acl2 = self
            .registry
            .lock()
            .expect("registry lock")
            .resolve(&path, image.as_deref())
            .map_err(|e| e.to_string())?;
        // Start unlocked: the customization handshake can take a while, and
        // the registry must stay answerable meanwhile (same split as the MCP
        // server, for the same reason).
        let mut book = Book::open(&acl2, &path).map_err(|e| e.to_string())?;
        if p.continue_past_failure.unwrap_or(true) {
            book.set_sync_mode(SyncMode::ContinuePastFailure);
        }
        // The editor renders rather than pays per token: complete per-event
        // detail on compound units, not the agent-facing default slots.
        book.set_inner_detail(usize::MAX);
        let control = book.control();
        let outputs = book.output_store();
        let (id, shared) = {
            let mut reg = self.registry.lock().expect("registry lock");
            let id = reg.register(book, acl2.clone());
            let shared = reg.get(&id).map_err(|e| e.to_string())?;
            (id, shared)
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let d = Driver::new(id.to_string(), shared, rx, self.out.clone());
        let driver = std::thread::spawn(move || d.run());
        self.books.lock().expect("books lock").insert(
            id.to_string(),
            BookHandle { tx, control, outputs, auto_rev: AtomicU64::new(0), driver,
                         path: path.clone() },
        );
        Ok(serde_json::to_value(wire::OpenReply {
            book: id.to_string(),
            image: acl2.display().to_string(),
        })
        .expect("serialize"))
    }

    fn close(&self, book: &str) -> Result<Value, String> {
        let handle = self.books.lock().expect("books lock").remove(book);
        let had = handle.is_some();
        if let Some(h) = handle {
            // Wake a blocked driver call, then let the driver exit; its Arc
            // keeps the session alive until then, and the drop kills ACL2.
            h.control.request_abort();
            let _ = h.tx.send(Cmd::Close);
            let mut closing = self.closing.lock().expect("closing lock");
            // Reap the ones that finished while we were not looking, so a
            // long editing session does not accumulate a handle per close.
            closing.retain(|h| !h.is_finished());
            closing.push(h.driver);
        }
        let closed = self.registry.lock().expect("registry lock").close(&SessionId::from(book));
        Ok(json!({ "closed": closed && had }))
    }

    /// Served from the outputs handle, like `output` and for the same reason:
    /// nothing here touches the session, so a grinding proof never delays it.
    fn induction(&self, p: wire::InductionParams) -> Result<Value, String> {
        let outputs = self.handle_of(&p.book, |h| h.outputs.clone())?;
        let r = OutputRef::from(p.output_ref.as_str());
        let found = {
            let outs = outputs.lock().expect("outputs lock");
            acl2_session::induction::from_output(&outs, &r).map_err(|e| e.to_string())?
        };
        Ok(serde_json::to_value(wire::InductionReply {
            inductions: found.iter().map(wire::induction).collect(),
        })
        .expect("serialize"))
    }

    fn output(&self, p: wire::OutputParams) -> Result<Value, String> {
        let (outputs, control) =
            self.handle_of(&p.book, |h| (h.outputs.clone(), h.control.clone()))?;
        let slice = wire::slice_from(&p)?;
        let r = OutputRef::from(p.output_ref.as_str());
        // A form still running has printed into memory, not into its file.
        // Neither lookup touches the book, so this answers while a proof is
        // in flight -- which is the only time the live branch is taken.
        let s = match control.live_slice(&p.output_ref, slice.clone()) {
            Some(live) => live,
            None => outputs
                .lock()
                .expect("outputs lock")
                .get(&r, slice)
                .map_err(|e| e.to_string())?,
        };
        Ok(serde_json::to_value(wire::OutputReply {
            text: s.text,
            total_lines: s.total_lines,
            superseded: s.superseded,
        })
        .expect("serialize"))
    }

    fn status(&self, book: &str) -> Result<Value, String> {
        let control = self.handle_of(book, |h| h.control.clone())?;
        let running = control.snapshot().map(|s| wire::RunningReply {
            unit: s.unit,
            elapsed_ms: s.elapsed.as_millis(),
            bytes_so_far: s.bytes_so_far,
            tail: s.tail,
        });
        Ok(serde_json::to_value(wire::StatusReply { running }).expect("serialize"))
    }

    fn notification(&self, method: &str, params: Value) {
        let r = match method {
            "update" => serde_json::from_value::<wire::UpdateParams>(params)
                .map_err(|e| e.to_string())
                .and_then(|p| {
                    self.handle_of(&p.book, |h| {
                        let revision = p
                            .revision
                            .unwrap_or_else(|| h.auto_rev.fetch_add(1, Ordering::Relaxed) + 1);
                        // No text means the client is telling us its buffer
                        // is the file, so read the file: its bytes are the
                        // truth, and for a book that is not UTF-8 they are
                        // the only place the truth still exists.
                        let bytes = match p.text {
                            Some(t) => t.into_bytes(),
                            None => std::fs::read(&h.path).map_err(|e| {
                                format!("{}: {e}", h.path.display())
                            })?,
                        };
                        h.tx.send(Cmd::Update { bytes, target_line: p.target_line, revision })
                            .map_err(|e| e.to_string())
                    })?
                }),
            // How wide the pane is, so terms cross the wire already laid out
            // for it. Not per book: one client, one pane, however many books
            // it has open.
            "layout" => serde_json::from_value::<wire::LayoutParams>(params)
                .map_err(|e| e.to_string())
                .map(|p| wire::set_pane_cols(p.cols)),
            "retract" => serde_json::from_value::<wire::RetractParams>(params)
                .map_err(|e| e.to_string())
                .and_then(|p| {
                    self.handle_of(&p.book, |h| {
                        h.tx.send(Cmd::Retract { keep: p.keep }).map_err(|e| e.to_string())
                    })?
                }),
            m => Err(format!("unknown notification {m:?}")),
        };
        if let Err(message) = r {
            // A notification has no reply to carry the error, so it goes out
            // as one -- silently dropping a client's update would be worse.
            self.out.notify("error", &json!({ "message": message }));
        }
    }
}

/// One framed message, or `None` at EOF.
fn read_frame(r: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse().ok();
        }
    }
    let n = len
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame without Content-Length"))?;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// Mirror of the MCP server's trust flags: `--trusted <dir>` (repeatable),
/// `--default <image>` or a bare image path, with `ACL2_TRUSTED` /
/// `ACL2_DEFAULT` as the environment spelling.
fn policy_from_args(args: Vec<String>) -> Result<ImagePolicy, String> {
    let mut trusted: Vec<String> = Vec::new();
    let mut default: Option<String> = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--trusted" => {
                trusted.push(it.next().ok_or("--trusted needs a directory")?);
            }
            "--default" => {
                default = Some(it.next().ok_or("--default needs an image")?);
            }
            bare if !bare.starts_with("--") => default = Some(bare.to_string()),
            other => return Err(format!("unknown flag {other:?}")),
        }
    }
    if default.is_none() {
        default = std::env::var("ACL2_DEFAULT").ok();
    }
    if trusted.is_empty()
        && let Ok(v) = std::env::var("ACL2_TRUSTED")
    {
        trusted.extend(v.split(':').filter(|s| !s.is_empty()).map(String::from));
    }
    let mut policy = ImagePolicy::new();
    for d in &trusted {
        let p = PathBuf::from(d);
        if !p.is_dir() {
            return Err(format!("--trusted {d}: not a directory"));
        }
        policy = policy.trust(p);
    }
    if let Some(d) = &default {
        policy = policy.default_image(PathBuf::from(d));
    }
    if default.is_none() && trusted.is_empty() {
        return Err("no ACL2 to run.\n\
             usage: acl2-session-editor [--default <image>] [--trusted <dir>]...\n\
             --default names the image for books outside any trusted checkout;\n\
             --trusted vouches for the ACL2 checkouts under a directory.\n\
             ACL2_DEFAULT and ACL2_TRUSTED (colon-separated) work too."
            .into());
    }
    Ok(policy)
}

fn main() {
    let policy = match policy_from_args(std::env::args().skip(1).collect()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let daemon = Daemon {
        registry: Mutex::new(Registry::new(policy)),
        books: Mutex::new(HashMap::new()),
        closing: Mutex::new(Vec::new()),
        out: Out::new(),
    };
    let stdin = io::stdin();
    let mut r = io::BufReader::new(stdin.lock());
    // Say why the loop ends. `while let Ok(Some(_))` treated three different
    // endings -- the client closing stdin, a read error, and a malformed
    // frame -- as one silent exit, so a daemon that went away left nothing
    // behind but the client's guess about what had happened. An afternoon of
    // it: the session was reported dead, and the process had left no word at
    // all about whether it was told to stop or fell over. stderr is the right
    // channel because the editor already pipes it to its log.
    loop {
        let frame = match read_frame(&mut r) {
            Ok(Some(f)) => f,
            Ok(None) => {
                eprintln!("acl2-session-editor: stdin closed; shutting down");
                break;
            }
            Err(e) => {
                eprintln!("acl2-session-editor: cannot read stdin ({e}); shutting down");
                break;
            }
        };
        let v: Value = match serde_json::from_slice(&frame) {
            Ok(v) => v,
            Err(e) => {
                daemon.out.error(&Value::Null, -32700, e.to_string());
                continue;
            }
        };
        let id = v.get("id").cloned().filter(|i| !i.is_null());
        let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        match id {
            Some(id) => match daemon.request(&id, &method, params) {
                Ok(Answer::Now(result)) => daemon.out.reply(&id, result),
                // The driver has the id and will reply when the form is done.
                Ok(Answer::ByDriver) => {}
                Err(RequestError::UnknownMethod) => {
                    daemon.out.error(&id, -32601, format!("unknown method {method:?}"));
                }
                Err(RequestError::Failed(m)) => daemon.out.error(&id, -32603, m),
            },
            None => daemon.notification(&method, params),
        }
    }
    // EOF: wake anything blocked, close every driver, and JOIN them.
    // Exiting with drivers unjoined skips their `Book` drops -- the drop is
    // what kills the ACL2 child -- and an ACL2 abandoned mid-proof never
    // reaches the read that would tell it to exit: it grinds and allocates
    // unattended, which is how a machine ends up out of memory. The joins
    // make "the daemon exited" mean "its sessions are dead", always.
    let handles: Vec<BookHandle> = {
        let mut books = daemon.books.lock().expect("books lock");
        books.drain().map(|(_, h)| h).collect()
    };
    for h in &handles {
        h.control.request_abort();
        let _ = h.tx.send(Cmd::Close);
    }
    for h in handles {
        let _ = h.driver.join();
    }
    // And the books already closed by the client, whose drivers left the map
    // before this sweep could see them. An editor shuts down by closing its
    // books and then ending the daemon, so this is the ordinary path rather
    // than a corner: without it the process reaches here with nothing in the
    // map, exits at once, and takes the still-unwinding driver with it --
    // leaving exactly the abandoned ACL2 the joins above exist to prevent.
    let closing: Vec<_> = std::mem::take(&mut *daemon.closing.lock().expect("closing lock"));
    for h in closing {
        let _ = h.join();
    }
    // Everything the drivers said as they wound up is still queued; give the
    // writer a moment to get it out before the process ends.
    daemon.out.drain(std::time::Duration::from_secs(2));
}
