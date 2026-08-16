// ACL2 Session: entry point and UI wiring.
//
// One daemon per window, one book per document. The extension is a thin
// renderer of the daemon's event stream: edits become `update` notifications
// (debounced, stamped with the document's own version as the revision), and
// `state` events become the status column, `unit` envelopes become
// diagnostics, panel content and output-view content, `settled` and `tail`
// drive the status bar. The target -- how far auto-admission goes -- is
// client state, sent with every update, so retraction holds exactly as
// prescribed.
//
// Rendering modes: `minimal` marks pending/running work in the
// gutter, admitted forms with a checkmark, reds as head-line diagnostics,
// and keeps only the conditional wash; `wash` draws whole-form washes,
// reds included, and no diagnostics. One treatment per fact, never two.

import * as vscode from 'vscode';
import {
  DaemonClient,
  EvalReply,
  EvalStateEvent,
  SettledEvent,
  StateEvent,
  TailEvent,
  UnitEvent,
  UnitState,
} from './protocol';
import { Envelope } from './protocol';
import { DecorationMode, Decorations } from './decorations';
import { EVAL_SCHEME, OutputView, SCHEME } from './outputView';
import { Highlighter } from './highlight';
import { Panel, PanelEnvelope, PanelModel } from './panel';
import { Console, ConsoleResult, HISTORY } from './console';

const UPDATE_DEBOUNCE_MS = 300;

interface BookSession {
  id: string;
  doc: vscode.TextDocument;
  /** Held target line (1-based); null checks the whole book; 0 holds all. */
  target: number | null;
  lastState?: StateEvent;
  /** Latest envelope per unit index, for diagnostics, panel and output view. */
  unitEvents: Map<number, UnitEvent>;
  debounce?: ReturnType<typeof setTimeout>;
  /**
   * The progress notification of the eval in flight, so an `eval_state`
   * event can say *why* it has not come back -- a form queued behind a long
   * proof and one that is grinding look the same from here otherwise.
   */
  evalProgress?: vscode.Progress<{ message?: string }>;
  /**
   * Console seqs sent to this book and not yet answered, oldest first. The
   * daemon runs evals in the order it received them and says only *that* one
   * has started, not which; the order is what turns that into a seq.
   */
  evalQueue: number[];
  /**
   * This book's console window, once anything has needed one. Per session,
   * not per window: an eval ran against the world *this* book built, and its
   * answer means nothing against another book's world.
   */
  console?: Console;
  /** Replies to this book's console evals, by seq, for the full-log button. */
  evalReplies: Map<number, EvalReply>;
}

let client: DaemonClient | undefined;
const books = new Map<string, BookSession>(); // key: document uri
const byId = new Map<string, BookSession>(); // key: daemon book id

let decorations: Decorations;
let outputView: OutputView;
let evalView: OutputView;
let panel: Panel;
let highlighter: Highlighter;
let status: vscode.StatusBarItem;
let channel: vscode.OutputChannel;

function mode(): DecorationMode {
  return vscode.workspace.getConfiguration('acl2-session').get<DecorationMode>('decorations', 'minimal');
}

function gutterAdmitted(): boolean {
  return vscode.workspace.getConfiguration('acl2-session').get<boolean>('gutterAdmitted', true);
}

export function activate(context: vscode.ExtensionContext): void {
  decorations = new Decorations(context);
  outputView = new OutputView(
    SCHEME,
    'ACL2 Output',
    '(no form under the cursor has been checked yet)'
  );
  evalView = new OutputView(EVAL_SCHEME, 'ACL2 Eval', '(nothing evaluated yet)');
  panel = new Panel();
  status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 10);
  channel = vscode.window.createOutputChannel('ACL2 Session');

  panel.onGoal = (m, goal, slot) => void fetchGoal(m, goal, slot);
  panel.onFullLog = (m) => {
    const s = byId.get(m.book);
    if (s) {
      void showUnitOutput(s, m.index).then(() => outputView.open());
    }
  };
  panel.onRetract = (m) => {
    const s = byId.get(m.book);
    if (s) {
      retractTo(s, m.index);
    }
  };
  panel.onTarget = (m) => {
    const s = byId.get(m.book);
    if (s) {
      s.target = m.lineEnd;
      sendUpdate(s);
    }
  };

  // The grammar and theme load off disk, so the first panel may render
  // before they arrive; refresh once they have, and again whenever the
  // colors the editor paints with change under us.
  highlighter = new Highlighter(context.extensionPath);
  const reload = () =>
    void highlighter.load().then(() => {
      const tokenizer = highlighter.ready ? (t: string) => highlighter.tokenize(t) : undefined;
      panel.tokenizer = tokenizer;
      panel.refresh();
      for (const s of books.values()) {
        if (s.console) {
          s.console.tokenizer = tokenizer;
          s.console.refresh();
        }
      }
    });
  reload();
  context.subscriptions.push(
    vscode.window.onDidChangeActiveColorTheme(reload),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (
        e.affectsConfiguration('workbench.colorTheme') ||
        e.affectsConfiguration('editor.tokenColorCustomizations')
      ) {
        reload();
      }
    })
  );

  context.subscriptions.push(
    decorations,
    panel,
    status,
    channel,
    vscode.workspace.registerTextDocumentContentProvider(SCHEME, outputView),
    vscode.workspace.registerTextDocumentContentProvider(EVAL_SCHEME, evalView),
    vscode.commands.registerCommand('acl2-session.start', startCommand),
    vscode.commands.registerCommand('acl2-session.evalSelection', () => forBook(evalCommand)),
    vscode.commands.registerCommand('acl2-session.showConsole', () =>
      forBook((s) => consoleFor(s).open())
    ),
    vscode.commands.registerCommand('acl2-session.stop', () => forBook(stopBook)),
    vscode.commands.registerCommand('acl2-session.interrupt', () =>
      forBook((s) => void client?.request('interrupt', { book: s.id }).catch(report))
    ),
    vscode.commands.registerCommand('acl2-session.retractToCursor', () =>
      forBook((s) => {
        const units = s.lastState?.units ?? [];
        const line = cursorLine();
        // Mid-form snaps forward to that form's end.
        let keep = 0;
        for (const u of units) {
          if (u.line_start <= line) {
            keep = u.index + 1;
          }
        }
        retractTo(s, keep);
      })
    ),
    vscode.commands.registerCommand('acl2-session.targetAtCursor', () =>
      forBook((s) => {
        s.target = cursorLine();
        sendUpdate(s);
      })
    ),
    vscode.commands.registerCommand('acl2-session.clearTarget', () =>
      forBook((s) => {
        s.target = null;
        sendUpdate(s);
      })
    ),
    vscode.commands.registerCommand('acl2-session.showOutput', () =>
      forBook(async (s) => {
        await followOutputView(s, true);
        await outputView.open();
      })
    ),
    vscode.commands.registerCommand('acl2-session.showPanel', () =>
      forBook((s) => {
        panel.open();
        void followPanel(s);
      })
    ),
    vscode.commands.registerCommand('acl2-session.gotoFrontier', () => forBook(gotoFrontier)),
    vscode.workspace.onDidChangeTextDocument((e) => {
      const s = books.get(e.document.uri.toString());
      if (!s || e.contentChanges.length === 0) {
        return;
      }
      if (s.debounce) {
        clearTimeout(s.debounce);
      }
      s.debounce = setTimeout(() => sendUpdate(s), UPDATE_DEBOUNCE_MS);
    }),
    vscode.window.onDidChangeTextEditorSelection((e) => {
      const s = books.get(e.textEditor.document.uri.toString());
      if (!s) {
        return;
      }
      if (outputView.visible) {
        void followOutputView(s, false);
      }
      if (panel.visible) {
        void followPanel(s);
      }
    }),
    vscode.window.onDidChangeVisibleTextEditors(() => {
      for (const s of books.values()) {
        paint(s);
      }
    }),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration('acl2-session')) {
        for (const s of books.values()) {
          paint(s);
        }
      }
    })
  );
}

export async function deactivate(): Promise<void> {
  // Close every book and wait for the answers: firing the requests and
  // exiting raced the daemon's own shutdown. EOF then ends the daemon,
  // which joins its drivers so every ACL2 dies with it.
  await Promise.all(
    [...books.values()].map((s) =>
      client?.request('close', { book: s.id }).catch(() => undefined)
    )
  );
  books.clear();
  byId.clear();
  client?.dispose();
  client = undefined;
}

function report(e: unknown): void {
  const msg = e instanceof Error ? e.message : String(e);
  channel.appendLine(`error: ${msg}`);
  void vscode.window.showErrorMessage(`acl2-session: ${msg}`);
}

function forBook(f: (s: BookSession) => unknown): void {
  const editor = vscode.window.activeTextEditor;
  const s = editor && books.get(editor.document.uri.toString());
  if (!s) {
    void vscode.window.showInformationMessage(
      'acl2-session: no live session for this file (run "ACL2: Start Live Checking")'
    );
    return;
  }
  void f(s);
}

function cursorLine(): number {
  const editor = vscode.window.activeTextEditor;
  return editor ? editor.selection.active.line + 1 : 1;
}

function ensureClient(): DaemonClient {
  if (client && client.alive) {
    return client;
  }
  const cfg = vscode.workspace.getConfiguration('acl2-session');
  const cmd = cfg.get<string>('daemonPath') || 'acl2-session-editor';
  const args: string[] = [];
  for (const t of cfg.get<string[]>('trusted') ?? []) {
    args.push('--trusted', t);
  }
  const def = cfg.get<string>('defaultImage');
  if (def) {
    args.push('--default', def);
  }
  const c = new DaemonClient(cmd, args);
  c.on('stderr', (s: string) => channel.append(s));
  c.on('daemon-error', (p: { message?: string }) =>
    channel.appendLine(`daemon: ${p?.message ?? 'unknown error'}`)
  );
  c.on('spawn-error', (e: Error) =>
    void vscode.window.showErrorMessage(
      `acl2-session: cannot start the daemon (${e.message}) -- set acl2-session.daemonPath`
    )
  );
  c.on('exit', () => {
    if (client === c && books.size > 0) {
      void vscode.window.showErrorMessage(
        'acl2-session: the daemon exited; see the "ACL2 Session" output channel'
      );
    }
    if (client === c) {
      client = undefined;
    }
  });
  c.on('state', onState);
  c.on('unit', onUnit);
  c.on('tail', onTail);
  c.on('settled', onSettled);
  c.on('eval_state', onEvalState);
  client = c;
  return c;
}

async function startCommand(): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  if (!editor || editor.document.uri.scheme !== 'file') {
    // By contract: an untitled buffer has no directory, hence no
    // session; save once so the book has a home, then live in the buffer.
    void vscode.window.showErrorMessage('acl2-session: save the book once, then start');
    return;
  }
  const doc = editor.document;
  const key = doc.uri.toString();
  const existing = books.get(key);
  if (existing) {
    sendUpdate(existing);
    return;
  }
  const c = ensureClient();
  try {
    const r = await c.request('open', { path: doc.fileName });
    const s: BookSession = {
      id: r.book as string,
      doc,
      target: null,
      unitEvents: new Map(),
      evalQueue: [],
      evalReplies: new Map(),
    };
    books.set(key, s);
    byId.set(s.id, s);
    status.show();
    status.text = '$(sync~spin) ACL2: starting';
    channel.appendLine(`opened ${doc.fileName} as ${s.id} under ${r.image}`);
    sendUpdate(s);
  } catch (e) {
    report(e);
  }
}

function stopBook(s: BookSession): void {
  void client?.request('close', { book: s.id }).catch(() => undefined);
  books.delete(s.doc.uri.toString());
  byId.delete(s.id);
  // The transcript is answers from a world that no longer exists.
  s.console?.dispose();
  s.console = undefined;
  for (const editor of vscode.window.visibleTextEditors) {
    if (editor.document === s.doc) {
      decorations.clear(editor);
    }
  }
  if (books.size === 0) {
    status.hide();
  }
}

function sendUpdate(s: BookSession): void {
  client?.notify('update', {
    book: s.id,
    text: s.doc.getText(),
    target_line: s.target,
    // The document's own monotone version is exactly the revision the
    // protocol wants stamped back on events.
    revision: s.doc.version,
  });
}

/** Retract so that `keep` units remain, and hold the target there. */
function retractTo(s: BookSession, keep: number): void {
  const units = s.lastState?.units ?? [];
  const kept = keep > 0 ? units[keep - 1] : undefined;
  s.target = kept ? kept.line_end : 0;
  client?.notify('retract', { book: s.id, keep });
}

// ---- Evaluating outside the file --------------------------------------------

/**
 * Run a form the file does not contain.
 *
 * This is not a REPL, and it is worth being precise about why: the session's
 * `eval` undoes every event the form introduces, so a `defun` typed here is
 * gone by the next form. What it is for is everything that is *not* part of
 * the book -- `pe`, `pl`, `pr`, `disabledp`, a throwaway `thm`, a
 * `set-gag-mode` -- where routing through the file would mean two edits per
 * question and would make the question part of the prefix `sync` must admit.
 * Settings stick where events do not, deliberately.
 */
function evalCommand(s: BookSession): void {
  const editor = vscode.window.activeTextEditor;
  const selected =
    editor && !editor.selection.isEmpty ? editor.document.getText(editor.selection) : undefined;
  const c = consoleFor(s);
  if (selected && selected.trim()) {
    // Straight into the console, rather than into a display of its own: the
    // answer belongs in the same transcript as one typed by hand, and the
    // console is where the envelope is rendered.
    c.submit(selected.trim());
    return;
  }
  // Nothing selected: the console *is* the input box, so open it and let the
  // user type there rather than inventing a second place to type.
  c.open();
}

/** How much of an eval's log to pull inline before deferring to the store. */
const EVAL_LOG_LINES = 2000;

async function showEvalResult(
  s: BookSession,
  r: EvalReply,
  opts: { announce: boolean } = { announce: true }
): Promise<void> {
  // The view is otherwise verbatim; a truncation is about the text itself,
  // not a banner over it, and silently showing the last 2000 lines of a
  // 40000-line grind would read as the whole story.
  const head =
    r.output_lines > EVAL_LOG_LINES
      ? `;; showing the last ${EVAL_LOG_LINES} of ${r.output_lines} lines\n\n`
      : '';
  evalView.set(s.id, null, head + (await evalLog(s, r, EVAL_LOG_LINES)));
  await evalView.open();
  if (!opts.announce) {
    return;
  }
  const secs = (r.elapsed_ms / 1000).toFixed(r.elapsed_ms < 10000 ? 1 : 0);
  const verdict = r.outcome === 'ok' ? 'ok' : (r.status ?? 'failed').replace(/_/g, ' ');
  vscode.window.setStatusBarMessage(`ACL2 eval: ${verdict} (${secs}s)`, 5000);
  if (r.world_change === 'dirty') {
    // The eval introduced events and undoing them did not verify, so the
    // prefix invariant is gone. The daemon already re-reconciles; saying so
    // is what keeps the rebuild from looking like a spontaneous re-prove.
    void vscode.window.showWarningMessage(
      'acl2-session: that form changed the world and the rollback did not verify — ' +
        'the session will rebuild by replay.'
    );
  }
}

function onEvalState(p: EvalStateEvent): void {
  const s = byId.get(p.book);
  if (!s) {
    return;
  }
  if (s.evalProgress) {
    const running = s.lastState?.units.find((u) => u.status === 'running');
    s.evalProgress.report({
      message:
        p.state === 'queued'
          ? running
            ? `queued behind form ${running.index + 1}`
            : 'queued'
          : 'running',
    });
  }
  // The console shows its own entry as queued the moment it is typed, so
  // only the transition to running is news -- and it is always the oldest
  // unanswered one, because the driver runs them in order.
  if (p.state === 'running' && s.evalQueue.length > 0) {
    s.console?.state(s.evalQueue[0], 'running', 0);
  }
}

// ---- The panel's console ----------------------------------------------------

/** How much of an eval's log to show inline, before the full-log button. */
const CONSOLE_LOG_LINES = 200;

/** This book's console, built on first use and wired to this book alone. */
function consoleFor(s: BookSession): Console {
  if (!s.console) {
    const c = new Console(s.id, vscode.workspace.asRelativePath(s.doc.uri));
    c.tokenizer = highlighter.ready ? (t) => highlighter.tokenize(t) : undefined;
    c.onEval = (seq, form) => void consoleEval(s, seq, form);
    c.onGoal = (seq, goal, slot) => void fetchEvalGoal(s, seq, goal, slot);
    c.onFullLog = (seq) => void showEvalLog(s, seq);
    s.console = c;
  }
  return s.console;
}

async function consoleEval(s: BookSession, seq: number, form: string): Promise<void> {
  const c = consoleFor(s);
  s.evalQueue.push(seq);
  try {
    const r: EvalReply = await client!.request('eval', { book: s.id, form });
    s.evalReplies.set(seq, r);
    for (const k of [...s.evalReplies.keys()].slice(0, -HISTORY)) {
      s.evalReplies.delete(k);
    }
    // The tail, not the head: for a query the answer is what ACL2 printed
    // last, and for a failure the error line is at the end too.
    const log = await evalLog(s, r, CONSOLE_LOG_LINES);
    c.done(seq, toConsoleResult(r), log);
  } catch (e) {
    c.failed(seq, e instanceof Error ? e.message : String(e));
  } finally {
    const at = s.evalQueue.indexOf(seq);
    if (at >= 0) {
      s.evalQueue.splice(at, 1);
    }
  }
}

async function evalLog(s: BookSession, r: EvalReply, lines: number): Promise<string> {
  try {
    const o = await client?.request('output', {
      book: s.id,
      output_ref: r.output_ref,
      mode: 'tail',
      lines,
    });
    return o.text as string;
  } catch (err) {
    return `(output unavailable: ${err})`;
  }
}

async function fetchEvalGoal(
  s: BookSession,
  seq: number,
  goal: string,
  slot: number
): Promise<void> {
  const c = consoleFor(s);
  const reply = s.evalReplies.get(seq);
  if (!reply) {
    c.goalResult(seq, slot, '(no captured output for this form)');
    return;
  }
  try {
    const r = await client?.request('output', {
      book: s.id,
      output_ref: reply.output_ref,
      mode: 'goal',
      text: goal,
    });
    c.goalResult(seq, slot, (r.text as string) || '(nothing captured under this goal)');
  } catch (err) {
    c.goalResult(seq, slot, `(output unavailable: ${err})`);
  }
}

/** The console's "full log" button: the whole thing, in the eval document. */
async function showEvalLog(s: BookSession, seq: number): Promise<void> {
  const reply = s.evalReplies.get(seq);
  if (reply) {
    await showEvalResult(s, reply, { announce: false });
  }
}

function gotoFrontier(s: BookSession): void {
  const editor = vscode.window.activeTextEditor;
  const units = s.lastState?.units ?? [];
  if (!editor || units.length === 0) {
    return;
  }
  const frontier =
    units.find((u) => u.status === 'running') ??
    units.find((u) => u.status === 'pending') ??
    units[units.length - 1];
  const pos = new vscode.Position(frontier.line_start - 1, 0);
  editor.selection = new vscode.Selection(pos, pos);
  editor.revealRange(new vscode.Range(pos, pos), vscode.TextEditorRevealType.InCenter);
}

// ---- Event rendering --------------------------------------------------------

function onState(p: StateEvent): void {
  const s = byId.get(p.book);
  if (!s) {
    return;
  }
  s.lastState = p;
  paint(s);
  updateStatusBar(s);
  const shown = panel.shown();
  if (shown && shown.book === s.id) {
    void followPanel(s, shown.index);
  }
}

function onUnit(p: UnitEvent): void {
  const s = byId.get(p.book);
  if (!s) {
    return;
  }
  s.unitEvents.set(p.index, p);
  if (outputView.shownBook === s.id && outputView.shownUnit === p.index) {
    void showUnitOutput(s, p.index);
  }
  const shown = panel.shown();
  if (shown && shown.book === s.id && shown.index === p.index) {
    void followPanel(s, p.index);
  }
}

function onTail(p: TailEvent): void {
  const s = byId.get(p.book);
  if (!s) {
    return;
  }
  // A null unit is an eval's tail: it belongs to this book's console, not to
  // any form of the file, and so to neither the panel nor a view that
  // follows the cursor.
  if (p.unit === null) {
    s.console?.tail(p.elapsed_ms, p.text);
  } else {
    if (outputView.shownBook === s.id && outputView.shownUnit === p.unit) {
      outputView.set(s.id, p.unit, p.text);
    }
    panel.tail(p.book, p.unit, p.elapsed_ms, p.text);
  }
  const secs = Math.round(p.elapsed_ms / 1000);
  status.text = `$(sync~spin) ACL2: proving ${p.unit === null ? '' : `${p.unit + 1}/${s.lastState?.units.length ?? '?'} `}(${secs}s)`;
}

function onSettled(p: SettledEvent): void {
  const s = byId.get(p.book);
  if (!s) {
    return;
  }
  updateStatusBar(s);
  if (p.reason === 'split_error' || p.reason === 'error') {
    status.text = `$(warning) ACL2: ${p.reason}`;
    if (p.message) {
      channel.appendLine(`${p.reason}: ${p.message}`);
    }
  }
}

function paint(s: BookSession): void {
  if (!s.lastState) {
    return;
  }
  const opts = { mode: mode(), gutterAdmitted: gutterAdmitted() };
  for (const editor of vscode.window.visibleTextEditors) {
    if (editor.document === s.doc) {
      decorations.apply(editor, s.lastState.units, opts);
      decorations.applyTarget(editor, s.target);
    }
  }
}

function updateStatusBar(s: BookSession): void {
  const units = s.lastState?.units ?? [];
  const total = units.length;
  const green = units.filter((u) => u.status === 'admitted' || u.status === 'conditional').length;
  const reds = units.filter((u) => u.status === 'skipped' || u.status === 'failed').length;
  const running = units.find((u) => u.status === 'running');
  if (running) {
    status.text = `$(sync~spin) ACL2: proving ${running.index + 1}/${total}`;
  } else if (reds > 0) {
    status.text = `$(error) ACL2: ${green}/${total} admitted, ${reds} red`;
  } else {
    status.text = `$(check) ACL2: ${green}/${total} admitted`;
  }
}

// ---- Output view and panel --------------------------------------------------

function unitAt(s: BookSession, line: number): UnitState | undefined {
  return s.lastState?.units.find((u) => u.line_start <= line && line <= u.line_end);
}

async function followOutputView(s: BookSession, force: boolean): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  if (!editor || editor.document !== s.doc) {
    return;
  }
  const unit = unitAt(s, editor.selection.active.line + 1);
  if (!unit) {
    return;
  }
  if (!force && outputView.shownBook === s.id && outputView.shownUnit === unit.index) {
    return;
  }
  await showUnitOutput(s, unit.index);
}

async function showUnitOutput(s: BookSession, index: number): Promise<void> {
  const e = s.unitEvents.get(index);
  if (!e) {
    outputView.set(s.id, index, '(not checked yet)');
    return;
  }
  try {
    const r = await client?.request('output', {
      book: s.id,
      output_ref: e.output_ref,
      mode: 'tail',
      lines: 400,
    });
    outputView.set(s.id, index, r.text as string);
  } catch (err) {
    outputView.set(s.id, index, `(output unavailable: ${err})`);
  }
}

/** Update the panel for the form at the cursor (or a pinned INDEX). */
async function followPanel(s: BookSession, index?: number): Promise<void> {
  let at = index;
  if (at === undefined) {
    const editor = vscode.window.activeTextEditor;
    if (!editor || editor.document !== s.doc) {
      return;
    }
    at = unitAt(s, editor.selection.active.line + 1)?.index;
  }
  if (at === undefined) {
    return;
  }
  const model = buildPanelModel(s, at);
  if (model) {
    panel.render(model);
  }
}

function buildPanelModel(s: BookSession, index: number): PanelModel | undefined {
  const st = s.lastState;
  const u = st?.units[index];
  if (!st || !u) {
    return undefined;
  }
  const model: PanelModel = {
    book: s.id,
    index,
    total: st.units.length,
    lineStart: u.line_start,
    lineEnd: u.line_end,
    status: u.status,
    checkpoints: [],
    rules: [],
    skipped: u.status === 'skipped',
  };
  // A pending or running unit's cached envelope describes an earlier life
  // (before a rollback re-queued it), so it is not shown.
  if (u.status === 'pending' || u.status === 'running') {
    return model;
  }
  const e = s.unitEvents.get(index);
  if (e) {
    Object.assign(model, toPanelEnvelope(e), { elapsedMs: e.elapsed_ms });
  }
  return model;
}

/**
 * The wire's envelope in the panel's spelling. One function, because the
 * daemon flattens a unit's envelope and an eval's identically -- and if that
 * ever stops being true, this is where it breaks loudly rather than in one
 * of two renderers that drifted apart.
 */
function toPanelEnvelope(e: Envelope): PanelEnvelope {
  const inner = (e.inner ?? []).map((i) => ({
    name: i.name,
    kind: i.kind,
    time: i.time,
    proverSteps: i.prover_steps,
    rules: i.rules ?? [],
    abortCauses: i.abort_causes,
  }));
  return {
    proverSteps: e.prover_steps,
    outputLines: e.output_lines,
    errorLine: e.error_line,
    abortCause: e.abort_cause,
    checkpoints: e.checkpoints ?? [],
    rules: e.rules ?? [],
    warnings: e.warnings,
    time: e.time,
    hintEvents: e.hint_events ?? [],
    splitterRules: e.splitter_rules ?? [],
    preRevert: e.pre_revert ?? false,
    forced: e.forced ?? false,
    inner,
    innerTotal: e.inner_total ?? inner.length,
    nTop: e.n_top ?? 0,
    nSub: e.n_sub ?? 0,
  };
}

function toConsoleResult(r: EvalReply): ConsoleResult {
  return {
    ...toPanelEnvelope(r),
    elapsedMs: r.elapsed_ms,
    outcome: r.outcome,
    worldChange: r.world_change,
    failStatus: r.status,
  };
}

async function fetchGoal(m: PanelModel, goal: string, slot: number): Promise<void> {
  const s = byId.get(m.book);
  const e = s?.unitEvents.get(m.index);
  if (!s || !e) {
    panel.goalResult(slot, '(no captured output for this form)');
    return;
  }
  try {
    const r = await client?.request('output', {
      book: s.id,
      output_ref: e.output_ref,
      mode: 'goal',
      text: goal,
    });
    panel.goalResult(slot, (r.text as string) || '(nothing captured under this goal)');
  } catch (err) {
    panel.goalResult(slot, `(output unavailable: ${err})`);
  }
}
