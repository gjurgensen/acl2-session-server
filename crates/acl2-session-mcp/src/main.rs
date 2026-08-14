#![forbid(unsafe_code)]

//! MCP server over a live ACL2 session.
//!
//! A thin mapping from tool calls onto `acl2-session`. The core does the work;
//! this crate owns the wire shapes and the words an agent reads.

mod wire;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acl2_session::{Health, ImagePolicy, Registry, SessionId, Slice, WorldChange};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};
use serde::Deserialize;

/// Default when a call does not give one. Bounds the *call*, never the proof:
/// an expired deadline leaves ACL2 working and hands back a handle.
const DEFAULT_DEADLINE: u64 = 60;

fn secs(s: Option<u64>) -> Duration {
    Duration::from_secs(s.unwrap_or(DEFAULT_DEADLINE))
}

fn fail(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
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
    /// form. It bounds how far forward to admit and never undoes anything.
    through_line: Option<usize>,
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
struct StatusArgs {
    session_id: String,
    /// Lines of the tail to include. Defaults to 20.
    tail_lines: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct OutputArgs {
    session_id: String,
    /// A handle from a previous reply's `output_ref`. Never construct one.
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
            let mut r = reg.lock().map_err(|e| e.to_string())?;
            let (id, image) = r.open(&file, want.as_deref()).map_err(|e| e.to_string())?;
            let b = r.get(&id).map_err(|e| e.to_string())?;
            let g = b.lock().map_err(|e| e.to_string())?;
            let ov = g
                .prove_output_overridden()
                .then(|| g.inherited_inhibit_output().unwrap_or("?").to_string());
            Ok::<_, String>((id, image, ov))
        })
        .map_err(fail)?;
        json(wire::OpenReply {
            session_id: id.to_string(),
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
            default: default,
        })
    }

    #[tool(description = "Bring the session into agreement with the file on disk. \
        Edit the file with ordinary tools, then call this: the server reuses the \
        unchanged prefix, rolls back to the first difference, and admits forward, \
        stopping at the first failure. You never say what changed. Reindenting or \
        editing a comment costs nothing.")]
    async fn acl2_sync(
        &self,
        Parameters(a): Parameters<SyncArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let book = self.book(&a.session_id)?;
        let d = secs(a.deadline_seconds);
        let out = blocking!({
            let mut b = book.lock().map_err(|e| e.to_string())?;
            b.sync(a.through_line, d).map_err(|e| e.to_string())
        })
        .map_err(fail)?;
        json(wire::SyncReply::from(out))
    }

    #[tool(description = "Evaluate a one-off form without changing the logical \
        world. Use for queries (pe, pl, pr, disabledp), experiments (thm), and \
        settings. Anything it defines is undone and will not be visible later.")]
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
        json(wire::EvalReply {
            outcome: match r.outcome {
                acl2_session::Outcome::Ok => "ok",
                acl2_session::Outcome::Failed => "failed",
                acl2_session::Outcome::StillRunning => "still_running",
            }
            .to_string(),
            world_change: match r.world_change {
                WorldChange::None => "none",
                WorldChange::Undone => "undone",
                WorldChange::Dirty => "dirty",
            }
            .to_string(),
            output_ref: r.output_ref.to_string(),
            output_lines: r.output_lines,
            elapsed_ms: r.elapsed.as_millis(),
        })
    }

    #[tool(description = "Keep waiting for a form whose deadline expired. The \
        deadline bounds the call, not the proof, so this is how a long proof \
        finishes rather than being abandoned.")]
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
                elapsed_ms: 0,
            },
            Some(Resumed::StillRunning { elapsed }) => wire::ResumeReply {
                outcome: "still_running".into(),
                admitted: None,
                failed: None,
                elapsed_ms: elapsed.as_millis(),
            },
            Some(Resumed::Admitted(u)) => wire::ResumeReply {
                outcome: "admitted".into(),
                elapsed_ms: u.elapsed.as_millis(),
                admitted: Some(wire::admitted(&u)),
                failed: None,
            },
            Some(Resumed::Failed(f)) => wire::ResumeReply {
                outcome: "failed".into(),
                elapsed_ms: 0,
                admitted: None,
                failed: Some(wire::failed(&f)),
            },
        };
        json(reply)
    }

    #[tool(description = "Progress on a form that is still running: how long it \
        has been going and the tail of what it has printed. Does not wait.")]
    async fn acl2_status(
        &self,
        Parameters(a): Parameters<StatusArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let book = self.book(&a.session_id)?;
        let n = a.tail_lines.unwrap_or(20);
        let (p, unit) = blocking!({
            let mut b = book.lock().map_err(|e| e.to_string())?;
            let u = b.running_unit();
            b.progress(n).map(|p| (p, u)).map_err(|e| e.to_string())
        })
        .map_err(fail)?;
        json(match p {
            Some(p) => wire::ProgressReply {
                running: true,
                unit_index: unit,
                elapsed_ms: p.elapsed.as_millis(),
                tail: p.tail,
                bytes_so_far: p.bytes_so_far,
            },
            None => wire::ProgressReply {
                running: false,
                unit_index: None,
                elapsed_ms: 0,
                tail: String::new(),
                bytes_so_far: 0,
            },
        })
    }

    #[tool(description = "Abort the form in flight and make the session usable \
        again. Sends an escalating signal sequence and verifies recovery.")]
    async fn acl2_interrupt(
        &self,
        Parameters(a): Parameters<SessionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let book = self.book(&a.session_id)?;
        let ok = blocking!({
            let mut b = book.lock().map_err(|e| e.to_string())?;
            b.interrupt().map_err(|e| e.to_string())
        })
        .map_err(fail)?;
        json(wire::Ack {
            ok,
            detail: (!ok).then(|| "session did not recover; consider reopening".into()),
        })
    }

    #[tool(description = "Whether the session is answering: ready, busy, \
        unresponsive, or dead. Busy is normal; unresponsive and dead are not.")]
    async fn acl2_health(
        &self,
        Parameters(a): Parameters<SessionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let book = self.book(&a.session_id)?;
        let (h, unit, rebuilds) = blocking!({
            let mut b = book.lock().map_err(|e| e.to_string())?;
            let h = b.health(Duration::from_secs(10));
            Ok::<_, String>((h, b.running_unit(), b.rebuilds()))
        })
        .map_err(fail)?;
        json(wire::HealthReply {
            state: match h {
                Health::Ready => "ready",
                Health::Busy => "busy",
                Health::Unresponsive => "unresponsive",
                Health::Dead => "dead",
            }
            .to_string(),
            running_unit: unit,
            rebuilds,
        })
    }

    #[tool(description = "Read part of a form's captured output, by handle. The \
        structured results are a projection: the induction scheme the prover \
        chose, per-goal rewriting detail, and macro output exist only here.")]
    async fn acl2_output(
        &self,
        Parameters(a): Parameters<OutputArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let book = self.book(&a.session_id)?;
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
            let b = book.lock().map_err(|e| e.to_string())?;
            b.output(&r, slice).map_err(|e| e.to_string())
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
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
                "Drives a live ACL2 session anchored to a .lisp file.\n\n\
                 Workflow: open a session on the book, edit the file with ordinary \
                 tools, then sync. The server works out what changed. Sync stops at \
                 the first failure and reports checkpoints as terms with a typed \
                 reason.\n\n\
                 The structured results are a projection over a complete log. When \
                 they do not explain a failure, read the log with acl2_output \
                 rather than assuming the detail is unavailable."
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
    if trusted.is_empty() {
        if let Ok(v) = std::env::var("ACL2_TRUSTED") {
            trusted.extend(v.split(':').filter(|s| !s.is_empty()).map(String::from));
        }
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
    let service = Acl2Mcp::new(policy).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
