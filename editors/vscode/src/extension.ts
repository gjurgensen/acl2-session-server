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
  SettledEvent,
  StateEvent,
  TailEvent,
  UnitEvent,
  UnitState,
} from './protocol';
import { DecorationMode, Decorations } from './decorations';
import { OutputView, SCHEME } from './outputView';
import { Panel, PanelModel } from './panel';

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
}

let client: DaemonClient | undefined;
const books = new Map<string, BookSession>(); // key: document uri
const byId = new Map<string, BookSession>(); // key: daemon book id

let decorations: Decorations;
let outputView: OutputView;
let panel: Panel;
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
  outputView = new OutputView();
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

  context.subscriptions.push(
    decorations,
    panel,
    status,
    channel,
    vscode.workspace.registerTextDocumentContentProvider(SCHEME, outputView),
    vscode.commands.registerCommand('acl2-session.start', startCommand),
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

export function deactivate(): void {
  for (const s of books.values()) {
    void client?.request('close', { book: s.id }).catch(() => undefined);
  }
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
  if (outputView.shownBook === s.id && outputView.shownUnit === p.unit && p.unit !== null) {
    outputView.set(
      s.id,
      p.unit,
      `unit ${p.unit + 1} -- proving, ${Math.round(p.elapsed_ms / 1000)}s`,
      p.text
    );
  }
  panel.tail(p.book, p.unit, p.elapsed_ms, p.text);
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
  const total = s.lastState?.units.length ?? 0;
  const e = s.unitEvents.get(index);
  if (!e) {
    outputView.set(s.id, index, `unit ${index + 1}/${total} -- not checked yet`, '');
    return;
  }
  try {
    const r = await client?.request('output', {
      book: s.id,
      output_ref: e.output_ref,
      mode: 'tail',
      lines: 400,
    });
    const label = `unit ${index + 1}/${total} -- ${e.outcome}${e.status ? ` (${e.status})` : ''}, ${r.total_lines} lines`;
    outputView.set(s.id, index, label, r.text as string);
  } catch (err) {
    outputView.set(s.id, index, `unit ${index + 1}/${total}`, `(output unavailable: ${err})`);
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
    model.elapsedMs = e.elapsed_ms;
    model.proverSteps = e.prover_steps;
    model.outputLines = e.output_lines;
    model.errorLine = e.error_line;
    model.abortCause = e.abort_cause;
    model.checkpoints = e.checkpoints ?? [];
    model.rules = e.rules ?? [];
    model.warnings = e.warnings;
    model.time = e.time;
    model.hintEvents = e.hint_events ?? [];
    model.splitterRules = e.splitter_rules ?? [];
    model.preRevert = e.pre_revert ?? false;
    model.forced = e.forced ?? false;
    model.inner = (e.inner ?? []).map((i) => ({
      name: i.name,
      kind: i.kind,
      time: i.time,
      proverSteps: i.prover_steps,
      abortCauses: i.abort_causes,
    }));
    model.innerTotal = e.inner_total ?? model.inner.length;
  }
  return model;
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
