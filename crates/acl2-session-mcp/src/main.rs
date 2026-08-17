#![forbid(unsafe_code)]

//! MCP server over a live ACL2 session.
//!
//! A thin mapping from tool calls onto `acl2-session`. The core does the work;
//! this crate owns the wire shapes and the words an agent reads.

mod wire;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acl2_session::{Book, Health, ImagePolicy, Registry, SessionId, Slice};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};
use serde::Deserialize;

/// Version plus the commit built from, as one short string. `build.rs`
/// leaves `ACL2_MCP_BUILD` empty when there is no git, so this degrades to
/// the bare version rather than lying about provenance.
const SERVER: &str = concat!(env!("CARGO_PKG_VERSION"), env!("ACL2_MCP_BUILD"));

/// Default when a call does not give one. Bounds the *call*, never the proof:
/// an expired deadline leaves ACL2 working and hands back a handle.
const DEFAULT_DEADLINE: u64 = 60;

fn secs(s: Option<u64>) -> Duration {
    Duration::from_secs(s.unwrap_or(DEFAULT_DEADLINE))
}

fn fail(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

/// A build's progress as far as the lock-free stamp knows it: the book and
/// elapsed time. The log's lines and tail live behind the book lock, which
/// is exactly what a caller reading the stamp is avoiding.
fn build_stamp(control: &acl2_session::SessionControl) -> Option<wire::BuildProgressReply> {
    control.build_snapshot().map(|(elapsed, book)| wire::BuildProgressReply {
        book: book.display().to_string(),
        elapsed_ms: elapsed.as_millis(),
        lines_so_far: 0,
        tail: String::new(),
    })
}

/// The last `n` lines of `s`, for tails served from a stream snapshot.
fn last_lines(s: &str, n: usize) -> String {
    let v: Vec<&str> = s.lines().rev().take(n).collect();
    v.into_iter().rev().collect::<Vec<_>>().join("\n")
}

/// Reply with real structured content rather than JSON stuffed into a text
/// block, which is what the protocol offers now that clients can consume it.
fn json<T: serde::Serialize>(v: T) -> Result<CallToolResult, ErrorData> {
    let value = serde_json::to_value(v)
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::structured(value))
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct OpenArgs {
    /// Path to the `.lisp` book. The session starts in its directory, so the
    /// directory's `acl2-customization.lsp` applies exactly as it would if you
    /// had started ACL2 there yourself.
    file: String,
    /// Which ACL2 image to run. Omit to use the one belonging to the book's
    /// own checkout, which is almost always what you want — a book was
    /// certified against its own checkout's image.
    ///
    /// Only images the server has been told to trust may be used, whether
    /// derived or named here. `acl2_images` lists them. The reply always says
    /// which was used.
    acl2: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SessionArgs {
    session_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SyncArgs {
    session_id: String,
    /// Sync only as far as this line. Omit for the whole file, which is the
    /// usual call. Resolved forgivingly: a line inside a form selects that
    /// form. On its own it bounds how far forward to admit and never undoes
    /// anything; pair it with `retract` to move the frontier back.
    through_line: Option<usize>,
    /// Report dependency staleness without acting on it.
    ///
    /// By default, a book this file includes whose source changed under the
    /// session invalidates the form that included it, exactly as editing
    /// that form would — the world is supposed to be what the file says.
    /// Set this to finish what you are doing first; it applies to this call
    /// only, so it cannot be set once and forgotten. `stale_includes` is
    /// reported either way.
    ignore_stale_includes: Option<bool>,
    /// Admit every form under `skip-proofs`: the forms go in, the proofs do
    /// not run.
    ///
    /// For loose interactive work where time matters more than truth —
    /// getting a long book back under you to try something. The reply's
    /// `conditional_from` says from which form the world is believed rather
    /// than proved, and stays set until those forms are re-admitted without
    /// this. Certify, or retract and sync again, before trusting anything
    /// proved under it.
    skip_proofs: Option<bool>,
    /// Also undo everything after `through_line`, if the session is already
    /// past it.
    ///
    /// Off by default, and deliberately a flag rather than an inference from
    /// a smaller line number: line numbers shift with every edit above them,
    /// and undoing proven work should never be triggered by a stale one.
    /// Editing the file is still the ordinary way to retract -- this is for
    /// deliberately parking the frontier, such as re-admitting an
    /// `include-book` whose dependency was just recertified.
    retract: Option<bool>,
    /// Seconds to wait before handing control back. ACL2 keeps working.
    deadline_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EvalArgs {
    session_id: String,
    /// A form to run outside the file: `(pe 'foo)`, `(pl 'bar)`, a `thm`, or a
    /// setting such as `(set-gag-mode nil)`.
    ///
    /// It cannot change the logical world -- anything it defines is undone --
    /// but it may change session state, which is why settings stick. Put a form
    /// in the file if you want it to persist.
    form: String,
    deadline_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ResumeArgs {
    session_id: String,
    deadline_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CertifyArgs {
    session_id: String,
    /// Seconds to wait before handing control back. The build keeps going;
    /// call again to keep waiting. Certification of a large book takes
    /// minutes, so expect to call more than once.
    deadline_seconds: Option<u64>,
    /// Parallel jobs for `cert.pl -j`. Defaults to 4. Raise it for a big
    /// dependency tree, but memory-hungry books can exhaust a machine at high
    /// parallelism, so it is deliberately not scaled to the core count.
    jobs: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct StatusArgs {
    session_id: String,
    /// Lines of the tail to include. Defaults to 20.
    tail_lines: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct InductionArgs {
    session_id: String,
    /// A handle from a previous reply's `output_ref`. Never construct one:
    /// the `f` number counts stored submissions, not forms, so an invented
    /// handle that happens to exist returns some *other* run's log -- an
    /// eval's, a certification's, or an earlier attempt at the same form --
    /// and reads as an ordinary answer. An invented one that does not exist
    /// is rejected, which is the harmless case.
    output_ref: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct OutputArgs {
    session_id: String,
    /// A handle from a previous reply's `output_ref`. Never construct one:
    /// the `f` number counts stored submissions, not forms, so an invented
    /// handle that happens to exist returns some *other* run's log -- an
    /// eval's, a certification's, or an earlier attempt at the same form --
    /// and reads as an ordinary answer. An invented one that does not exist
    /// is rejected, which is the harmless case.
    output_ref: String,
    /// `all`, `head`, `tail`, `range`, `find`, or `goal`. Defaults to `tail`.
    ///
    /// `goal` is usually what you want after a failed proof: pass a checkpoint
    /// name such as `Subgoal *1/2` and get what ACL2 printed under it.
    mode: Option<String>,
    /// Line count for `head` and `tail`.
    lines: Option<usize>,
    /// 1-based inclusive bounds for `range`.
    from_line: Option<usize>,
    to_line: Option<usize>,
    /// Literal text for `find`, or the goal name for `goal`.
    text: Option<String>,
    /// Context lines around each `find` hit. Defaults to 2.
    context: Option<usize>,
}

/// The server state.
///
/// No `tool_router` field: since rmcp 3, `#[tool_handler]` builds the router by
/// calling `Self::tool_router()` rather than reading one off the instance.
#[derive(Clone)]
struct Acl2Mcp {
    registry: Arc<Mutex<Registry>>,
}

/// Run blocking work off the async runtime.
///
/// Every core operation blocks on ACL2, for up to its deadline. Doing that on
/// the runtime would stall every other session's calls behind it.
macro_rules! blocking {
    ($body:expr) => {
        tokio::task::spawn_blocking(move || $body)
            .await
            .map_err(fail)?
    };
}

#[tool_router]
impl Acl2Mcp {
    fn new(policy: ImagePolicy) -> Self {
        Self { registry: Arc::new(Mutex::new(Registry::new(policy))) }
    }

    /// Look up one session, holding the registry lock only for the lookup.
    fn book(&self, id: &str) -> Result<acl2_session::registry::SharedBook, ErrorData> {
        self.registry
            .lock()
            .map_err(fail)?
            .get(&SessionId::from(id))
            .map_err(fail)
    }

    /// The session's control block: the way to reach a busy session without
    /// queueing behind whatever call holds its lock.
    fn control(&self, id: &str) -> Result<std::sync::Arc<acl2_session::SessionControl>, ErrorData> {
        self.registry
            .lock()
            .map_err(fail)?
            .control_of(&SessionId::from(id))
            .ok_or_else(|| fail(format!("no session {id}; it was never opened, or has been closed")))
    }

    #[tool(description = "Open an ACL2 session anchored to a .lisp book. Returns a \
        session_id used by every other tool. The session runs in the book's own \
        directory, so it is the session you would get by starting ACL2 there.")]
    async fn acl2_session_open(
        &self,
        Parameters(a): Parameters<OpenArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let reg = self.registry.clone();
        let file = PathBuf::from(&a.file);
        let want = a.acl2.clone().map(PathBuf::from);
        let (id, image, overridden) = blocking!({
            // Resolve under the lock, start ACL2 without it, lock again only
            // to insert. Startup runs the directory's customization file --
            // up to the two-minute handshake -- and holding the registry
            // across it froze every session's lookups for the duration.
            let acl2 = reg
                .lock()
                .map_err(|e| e.to_string())?
                .resolve(&file, want.as_deref())
                .map_err(|e| e.to_string())?;
            let book = Book::open(&acl2, &file).map_err(|e| e.to_string())?;
            let ov = book
                .prove_output_overridden()
                .then(|| book.inherited_inhibit_output().unwrap_or("?").to_string());
            let id = reg.lock().map_err(|e| e.to_string())?.register(book, acl2.clone());
            Ok::<_, String>((id, acl2, ov))
        })
        .map_err(fail)?;
        json(wire::OpenReply {
            session_id: id.to_string(),
            server: SERVER.to_string(),
            file: a.file,
            acl2: image.display().to_string(),
            prove_output_overridden: overridden,
        })
    }

    #[tool(description = "Close a session and kill its ACL2 process. Its output \
        handles stop working.")]
    async fn acl2_session_close(
        &self,
        Parameters(a): Parameters<SessionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let existed = self
            .registry
            .lock()
            .map_err(fail)?
            .close(&SessionId::from(a.session_id.as_str()));
        json(wire::Ack {
            ok: existed,
            detail: (!existed).then(|| "no such session".to_string()),
        })
    }

    #[tool(description = "List open sessions.")]
    async fn acl2_sessions(&self) -> Result<CallToolResult, ErrorData> {
        let list = self.registry.lock().map_err(fail)?.list();
        json(wire::SessionsReply {
            sessions: list
                .into_iter()
                .map(|(id, p, img)| wire::SessionEntry {
                    session_id: id.to_string(),
                    file: p.display().to_string(),
                    acl2: img.display().to_string(),
                })
                .collect(),
        })
    }

    #[tool(description = "List the ACL2 images this server may run, and the \
        directories it trusts. An image is used only if it is one of these, \
        whether derived from a book's checkout or named explicitly.")]
    async fn acl2_images(&self) -> Result<CallToolResult, ErrorData> {
        let reg = self.registry.clone();
        let (images, trusted, default) = blocking!({
            let r = reg.lock().map_err(|e| e.to_string())?;
            let p = r.policy();
            Ok::<_, String>((
                p.available(),
                p.trusted_roots().to_vec(),
                p.default_path().map(|d| d.display().to_string()),
            ))
        })
        .map_err(fail)?;
        json(wire::ImagesReply {
            images: images.iter().map(|p| p.display().to_string()).collect(),
            trusted_directories: trusted.iter().map(|p| p.display().to_string()).collect(),
            default,
        })
    }

    #[tool(description = "Bring the session into agreement with the file on disk. \
        Edit the file with ordinary tools, then call this: the server reuses the \
        unchanged prefix, rolls back to the first difference, and admits forward, \
        stopping at the first failure. You never say what changed. Reindenting or \
        editing a comment costs nothing.\n\n\
        A book you include whose source has changed since the session built on \
        it invalidates the form that included it, the same as editing that form; \
        the reply names it under stale_includes.\n\n\
        Undo is normally a consequence of editing. To park the frontier without \
        editing -- to re-admit an include-book after recertifying what it \
        includes, say -- pass through_line with retract:true.")]
    async fn acl2_sync(
        &self,
        Parameters(a): Parameters<SyncArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let book = self.book(&a.session_id)?;
        let d = secs(a.deadline_seconds);
        // Retraction needs something to retract *to*; silently doing nothing
        // would read as a frontier that refused to move.
        let retract = a.retract.unwrap_or(false);
        if retract && a.through_line.is_none() {
            return Err(fail("retract needs through_line: it says where to leave the frontier"));
        }
        let opts = acl2_session::SyncOptions {
            through_line: a.through_line,
            retract,
            ignore_stale_includes: a.ignore_stale_includes.unwrap_or(false),
            skip_proofs: a.skip_proofs.unwrap_or(false),
        };
        let out = blocking!({
            let mut b = book.lock().map_err(|e| e.to_string())?;
            b.sync_with(&opts, d).map_err(|e| e.to_string())
        })
        .map_err(fail)?;
        json(wire::SyncReply::from(out))
    }

    #[tool(description = "Evaluate a one-off form without changing the logical \
        world. Use for queries (pe, pl, pr, disabledp), experiments (thm), and \
        settings. Anything it defines is undone and will not be visible later. \
        A form that fails reports the same envelope a sync failure does -- \
        checkpoints with goal names, abort cause, error line -- so a failed \
        thm experiment is directly debuggable from the reply.")]
    async fn acl2_eval(
        &self,
        Parameters(a): Parameters<EvalArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let book = self.book(&a.session_id)?;
        let d = secs(a.deadline_seconds);
        let r = blocking!({
            let mut b = book.lock().map_err(|e| e.to_string())?;
            b.eval(&a.form, d).map_err(|e| e.to_string())
        })
        .map_err(fail)?;
        json(wire::evaluated(&r))
    }

    #[tool(description = "Keep waiting for a form whose deadline expired. The \
        deadline bounds the call, not the proof, so this is how a long proof \
        finishes rather than being abandoned.\n\n\
        Finishes that one form and stops -- it does not carry on through the \
        rest of the file. Call acl2_sync again to continue; the forms already \
        admitted are reused, so nothing is re-proved. For a certification, use \
        acl2_certify again instead.")]
    async fn acl2_resume(
        &self,
        Parameters(a): Parameters<ResumeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let book = self.book(&a.session_id)?;
        let d = secs(a.deadline_seconds);
        let r = blocking!({
            let mut b = book.lock().map_err(|e| e.to_string())?;
            b.resume(d).map_err(|e| e.to_string())
        })
        .map_err(fail)?;
        use acl2_session::Resumed;
        let reply = match r {
            None => wire::ResumeReply {
                outcome: "nothing_in_flight".into(),
                admitted: None,
                failed: None,
                evaluated: None,
                elapsed_ms: 0,
            },
            Some(Resumed::StillRunning { elapsed }) => wire::ResumeReply {
                outcome: "still_running".into(),
                admitted: None,
                failed: None,
                evaluated: None,
                elapsed_ms: elapsed.as_millis(),
            },
            Some(Resumed::Admitted(u)) => wire::ResumeReply {
                outcome: "admitted".into(),
                elapsed_ms: u.elapsed.as_millis(),
                admitted: Some(wire::admitted(&u)),
                failed: None,
                evaluated: None,
            },
            Some(Resumed::Failed(f)) => wire::ResumeReply {
                outcome: "failed".into(),
                elapsed_ms: 0,
                admitted: None,
                failed: Some(wire::failed(&f)),
                evaluated: None,
            },
            // Unreachable today -- this server never sets ContinuePastFailure
            // on a book -- but mapped honestly for totality.
            Some(Resumed::Skipped(f)) => wire::ResumeReply {
                outcome: "skipped".into(),
                elapsed_ms: 0,
                admitted: None,
                failed: Some(wire::failed(&f)),
                evaluated: None,
            },
            Some(Resumed::Evaluated(r)) => wire::ResumeReply {
                outcome: "evaluated".into(),
                elapsed_ms: r.elapsed.as_millis(),
                admitted: None,
                failed: None,
                evaluated: Some(wire::evaluated(&r)),
            },
        };
        json(reply)
    }

    #[tool(description = "Certify this session's book with cert.pl, chasing \
        dependencies, using the same ACL2 image the session runs. Runs outside \
        the session against the file on disk, so the session's world is \
        untouched and stays usable. A book that syncs cleanly can still fail \
        to certify -- certification is the check that catches it.\n\n\
        If the deadline expires, the build carries on: call this again to keep \
        waiting on the same build (not acl2_resume, which is for the prover), \
        acl2_status to watch it without waiting, or acl2_interrupt to stop it.")]
    async fn acl2_certify(
        &self,
        Parameters(a): Parameters<CertifyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let book = self.book(&a.session_id)?;
        let d = secs(a.deadline_seconds);
        let jobs = a.jobs.unwrap_or(4).max(1);
        let r = blocking!({
            let mut b = book.lock().map_err(|e| e.to_string())?;
            b.certify(d, jobs).map_err(|e| e.to_string())
        })
        .map_err(fail)?;
        json(wire::certified(&r))
    }

    #[tool(description = "Progress on whatever is still running -- a form in \
        the prover, a certification, or both. Reports how long each has been \
        going and the tail of what it has printed. Does not wait, even while \
        another call is blocked on the same session.")]
    async fn acl2_status(
        &self,
        Parameters(a): Parameters<StatusArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let control = self.control(&a.session_id)?;
        let book = self.book(&a.session_id)?;
        let n = a.tail_lines.unwrap_or(20);
        let reply = blocking!({
            // A blocked sync/eval holds the session lock for its whole
            // deadline, and status exists precisely to be usable then. Try
            // the lock; on contention, snapshot the output stream, which the
            // reader thread keeps current.
            match book.try_lock() {
                Ok(mut b) => {
                    let unit = b.running_unit();
                    let build = b.build_progress(n).as_ref().map(wire::build_progress);
                    let p = b.progress(n).map_err(|e| e.to_string())?;
                    Ok::<_, String>(match p {
                        Some(p) => wire::ProgressReply {
                            running: true,
                            unit_index: unit,
                            elapsed_ms: p.elapsed.as_millis(),
                            tail: p.tail,
                            bytes_so_far: p.bytes_so_far,
                            build,
                        },
                        None => wire::ProgressReply {
                            running: false,
                            unit_index: None,
                            elapsed_ms: 0,
                            tail: String::new(),
                            bytes_so_far: 0,
                            build,
                        },
                    })
                }
                Err(_) => match control.snapshot() {
                    Some(s) => Ok(wire::ProgressReply {
                        running: true,
                        unit_index: s.unit,
                        elapsed_ms: s.elapsed.as_millis(),
                        tail: last_lines(&s.tail, n),
                        bytes_so_far: s.bytes_so_far,
                        // The build stamp knows the build exists and how long
                        // it has run; its log lives behind the lock.
                        build: build_stamp(&control),
                    }),
                    // No form in flight but the lock is held: a running
                    // certification's wait holds it for its whole deadline,
                    // and the stamp answers without joining the queue.
                    None if control.build_snapshot().is_some() => {
                        Ok(wire::ProgressReply {
                            running: false,
                            unit_index: None,
                            elapsed_ms: 0,
                            tail: String::new(),
                            bytes_so_far: 0,
                            build: build_stamp(&control),
                        })
                    }
                    // Contended for an instant between submissions: a brief
                    // wait is bounded and honest.
                    None => {
                        let mut b = book.lock().map_err(|e| e.to_string())?;
                        let unit = b.running_unit();
                        let build = b.build_progress(n).as_ref().map(wire::build_progress);
                        let p = b.progress(n).map_err(|e| e.to_string())?;
                        Ok(match p {
                            Some(p) => wire::ProgressReply {
                                running: true,
                                unit_index: unit,
                                elapsed_ms: p.elapsed.as_millis(),
                                tail: p.tail,
                                bytes_so_far: p.bytes_so_far,
                                build,
                            },
                            None => wire::ProgressReply {
                                running: false,
                                unit_index: None,
                                elapsed_ms: 0,
                                tail: String::new(),
                                bytes_so_far: 0,
                                build,
                            },
                        })
                    }
                },
            }
        })
        .map_err(fail)?;
        json(reply)
    }

    #[tool(description = "Abort the form in flight and make the session usable \
        again. Works immediately even while a sync or eval is still waiting on \
        the form. Reports what was interrupted -- elapsed prove time, an \
        outline of the event, checkpoints if the prover had any -- plus a \
        handle to the partial output.")]
    async fn acl2_interrupt(
        &self,
        Parameters(a): Parameters<SessionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // Ask through the control block first: the blocked call is woken
        // off its condvar and hands the lock over at once, instead of after
        // its whole deadline -- which used to make interrupting a 600-second
        // sync wait 600 seconds.
        let control = self.control(&a.session_id)?;
        control.request_abort();
        let book = self.book(&a.session_id)?;
        let r = blocking!({
            let mut b = book.lock().map_err(|e| e.to_string())?;
            b.interrupt().map_err(|e| e.to_string())
        })
        .map_err(fail)?;
        json(wire::interrupted(&r))
    }

    #[tool(description = "Whether the session is answering: ready, busy, \
        unresponsive, or dead. Busy is normal; unresponsive and dead are not. \
        Dead means close the session and open it again.")]
    async fn acl2_health(
        &self,
        Parameters(a): Parameters<SessionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let control = self.control(&a.session_id)?;
        let book = self.book(&a.session_id)?;
        let reply = blocking!({
            // A busy session answers from the stream snapshot rather than
            // queueing behind the very call it is being asked about -- and a
            // session whose certification wait holds the lock is busy in the
            // same sense. Both conditions, deliberately: a snapshot that says
            // running with a free lock means the form outran its call's
            // deadline, and the locked path then gives the fuller answer.
            if (control.snapshot().is_some() || control.build_snapshot().is_some())
                && book.try_lock().is_err()
            {
                return Ok::<_, String>(wire::HealthReply {
                    state: "busy".to_string(),
                    running_unit: control.snapshot().and_then(|s| s.unit),
                    rebuilds: None,
                });
            }
            let mut b = book.lock().map_err(|e| e.to_string())?;
            let h = b.health(Duration::from_secs(10));
            Ok(wire::HealthReply {
                state: match h {
                    Health::Ready => "ready",
                    Health::Busy => "busy",
                    Health::Unresponsive => "unresponsive",
                    Health::Dead => "dead",
                }
                .to_string(),
                running_unit: b.running_unit(),
                rebuilds: Some(b.rebuilds()),
            })
        })
        .map_err(fail)?;
        json(reply)
    }

    #[tool(description = "Why the prover inducted, for a form whose `rules` \
        include an `:induction` rune -- the scheme it chose, the term that \
        suggested it, the rule credited, and how many subgoals it produced. \
        Read from the log, because ACL2 prints the scheme and records it \
        nowhere: expect fields to be missing rather than wrong, and nothing \
        at all from an ACL2 that words it differently. Cheap and safe to call \
        while the session is busy.")]
    async fn acl2_induction(
        &self,
        Parameters(a): Parameters<InductionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let outputs = self
            .registry
            .lock()
            .map_err(fail)?
            .outputs_of(&SessionId::from(a.session_id.as_str()))
            .ok_or_else(|| {
                fail(format!(
                    "no session {}; it was never opened, or has been closed",
                    a.session_id
                ))
            })?;
        let r = acl2_session::OutputRef::from(a.output_ref.as_str());
        let found = blocking!({
            let store = outputs.lock().map_err(|e| e.to_string())?;
            acl2_session::induction::from_output(&store, &r).map_err(|e| e.to_string())
        })
        .map_err(fail)?;
        json(wire::InductionReply {
            inductions: found.iter().map(wire::induction).collect(),
        })
    }

    #[tool(description = "Read part of a form's captured output, by handle. The \
        structured results are a projection: per-goal rewriting detail and \
        macro output exist only here. For why a proof inducted, prefer \
        acl2_induction, which reads this same log and returns it as fields. \
        Works immediately even while a sync or eval is blocked on the session.")]
    async fn acl2_output(
        &self,
        Parameters(a): Parameters<OutputArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // The store, not the book: reading a completed form's log must not
        // queue behind a blocked call's deadline.
        let outputs = self
            .registry
            .lock()
            .map_err(fail)?
            .outputs_of(&SessionId::from(a.session_id.as_str()))
            .ok_or_else(|| {
                fail(format!(
                    "no session {}; it was never opened, or has been closed",
                    a.session_id
                ))
            })?;
        let slice = match a.mode.as_deref().unwrap_or("tail") {
            "all" => Slice::All,
            "head" => Slice::Head(a.lines.unwrap_or(40)),
            "tail" => Slice::Tail(a.lines.unwrap_or(40)),
            "range" => Slice::Range(a.from_line.unwrap_or(1), a.to_line.unwrap_or(usize::MAX)),
            "goal" => Slice::Goal {
                name: a.text.clone().unwrap_or_default(),
            },
            "find" => Slice::Find {
                needle: a.text.clone().unwrap_or_default(),
                context: a.context.unwrap_or(2),
                ignore_case: true,
            },
            other => return Err(fail(format!("unknown mode {other:?}"))),
        };
        let r = acl2_session::OutputRef::from(a.output_ref.as_str());
        let out = blocking!({
            let store = outputs.lock().map_err(|e| e.to_string())?;
            store.get(&r, slice).map_err(|e| e.to_string())
        })
        .map_err(fail)?;
        json(wire::OutputReply {
            text: out.text,
            total_lines: out.total_lines,
            superseded: out.superseded,
        })
    }
}

#[tool_handler]
impl ServerHandler for Acl2Mcp {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` is non-exhaustive, so it is filled in rather than
        // built with a struct expression.
        let mut info = ServerInfo::default();
        // The default is `Implementation::from_build_env()`, whose `env!`s
        // expand inside rmcp -- so leaving it introduces us to every client as
        // "rmcp 3.1.2". The handshake is the one place a client records what
        // it is talking to, which is precisely what the build stamp exists to
        // answer; `OpenReply.server` should not be the only place it appears.
        info.server_info = Implementation::new(env!("CARGO_PKG_NAME"), SERVER);
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
                "Drives a live ACL2 session anchored to a .lisp file.\n\n\
                 Workflow: open a session on the book, edit the file with ordinary \
                 tools, then sync. The server works out what changed. Sync stops at \
                 the first failure and reports checkpoints as terms with a typed \
                 reason.\n\n\
                 The structured results are a projection over a complete log. When \
                 they do not explain a failure, read the log with acl2_output \
                 rather than assuming the detail is unavailable.\n\n\
                 A book that syncs cleanly can still fail to certify. Use \
                 acl2_certify when the work is done, or when a dependent book \
                 needs this one certified; it runs cert.pl outside the session, \
                 so the session is unaffected."
                .into(),
        );
        info
    }
}

/// Build the image policy from the command line, falling back to the
/// environment.
///
/// An MCP client starts a server as a subprocess with `command`, `args` and
/// `env` from its own JSON config, so those are the only channels available.
/// Flags are preferred over a config file of our own: the trusted set is
/// security-relevant, and it belongs in the file the user edits to install the
/// server rather than somewhere they may not connect with it.
///
///   --default <image>    used when a book is outside every trusted directory
///   --trusted <dir>      vouch for the ACL2 checkouts under a directory;
///                        repeatable
///
/// Or, for clients where editing `args` is awkward:
///
///   ACL2_DEFAULT=<image>
///   ACL2_TRUSTED=<dir>:<dir>       colon-separated, like PATH
///
/// A single bare argument is still accepted as the default image, so the
/// simplest configuration remains one path.
fn policy_from_args() -> Result<ImagePolicy, String> {
    let mut policy = ImagePolicy::new();
    let mut default: Option<String> = None;
    let mut trusted: Vec<String> = Vec::new();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--default" => {
                i += 1;
                default = Some(args.get(i).ok_or("--default needs a path")?.clone());
            }
            "--trusted" => {
                i += 1;
                trusted.push(args.get(i).ok_or("--trusted needs a path")?.clone());
            }
            bare if !bare.starts_with('-') && default.is_none() => {
                default = Some(bare.to_string());
            }
            other => return Err(format!("unrecognised argument {other:?}")),
        }
        i += 1;
    }

    if default.is_none() {
        default = std::env::var("ACL2_DEFAULT").ok();
    }
    if trusted.is_empty()
        && let Ok(v) = std::env::var("ACL2_TRUSTED")
    {
        trusted.extend(v.split(':').filter(|s| !s.is_empty()).map(String::from));
    }

    for d in &trusted {
        let p = PathBuf::from(d);
        if !p.is_dir() {
            return Err(format!("--trusted {d}: not a directory"));
        }
        policy = policy.trust(p);
    }
    if let Some(d) = &default {
        let p = PathBuf::from(d);
        if !p.exists() {
            return Err(format!("no ACL2 image at {d}"));
        }
        if p.is_dir() {
            return Err(format!(
                "{d} is a directory; the image is usually the saved_acl2 script inside it"
            ));
        }
        policy = policy.default_image(p);
    }

    if default.is_none() && trusted.is_empty() {
        return Err("no ACL2 image configured.\n\
             usage: acl2-session-mcp [--default <image>] [--trusted <dir>]...\n\
             or set ACL2_DEFAULT / ACL2_TRUSTED.\n\
             --default names the image for books outside any trusted checkout;\n\
             --trusted vouches for the ACL2 checkouts under a directory, and a\n\
             book inside one is run by its own checkout's image."
            .to_string());
    }
    Ok(policy)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let policy = match policy_from_args() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let server = Acl2Mcp::new(policy);
    // Kept past the move into `serve`, so the sweep below can still reach the
    // sessions once the transport is gone.
    let registry = server.registry.clone();
    let service = server.serve(rmcp::transport::stdio()).await?;
    let ended = service.waiting().await;

    // The client is gone and no reply will be read, but every call that can
    // block on ACL2 runs under `spawn_blocking`, and dropping the runtime
    // waits for those to finish. A sync parked on its deadline therefore
    // keeps this process -- and a grinding ACL2 -- alive for the whole
    // remainder of that deadline, working for nobody. Measured before this
    // sweep existed: a sync given 600 seconds, disconnected 8 seconds in,
    // still had both processes running two minutes later.
    //
    // Aborting is exactly what `acl2_interrupt` does to unblock a call; here
    // it is every session at once. The waiter wakes, the blocking task ends,
    // the runtime finishes dropping, and `Book::drop` kills each child. Taken
    // whatever ended the transport, since a broken pipe strands a session as
    // surely as a clean EOF. The editor daemon sweeps at EOF for the same
    // reason its comment gives: an ACL2 abandoned mid-proof grinds and
    // allocates unattended, which is how a machine runs out of memory.
    let controls: Vec<_> = {
        let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
        reg.list().into_iter().filter_map(|(id, _, _)| reg.control_of(&id)).collect()
    };
    for c in controls {
        c.request_abort();
    }

    ended?;
    Ok(())
}
