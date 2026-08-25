// ACL2: entry point and UI wiring.
//
// One daemon per window, one book per document. The extension is a thin
// renderer of the daemon's event stream: edits become `update` notifications
// (debounced, stamped with the document's own version as the revision), and
// `state` events become the status column, `unit` envelopes become
// diagnostics, form view content and output-view content, `settled` and `tail`
// drive the status bar. The target -- how far auto-admission goes -- is
// client state, sent with every update, so retraction holds exactly as
// prescribed.
//
// Rendering modes: `gutter` marks pending/running work in the
// gutter, admitted forms with a checkmark, reds as head-line diagnostics,
// and keeps only the conditional wash; `wash` draws whole-form washes,
// reds included, and no diagnostics. One treatment per fact, never two.

import * as vscode from 'vscode';
import {
  Anchor,
  Edit,
  attentionPoints,
  barrierIndex,
  diagnosticMessage,
  warningMessage,
  keepAnchor,
  makeSerializer,
  pinnedIndex,
  planTail,
  stepBarrier,
  warningKinds,
} from './policy';
import { readFileSync } from 'fs';
import {
  DaemonClient,
  EvalReply,
  EvalStateEvent,
  StaleDepsEvent,
  SettledEvent,
  StateEvent,
  TailEvent,
  UnitEvent,
  UnitState,
} from './protocol';
import { Envelope, RulesReply, RuneWire } from './protocol';
import { DecorationMode, Decorations } from './decorations';
import { byteColumn, charOffset } from './positions';
import { problemFor } from './bars';
import { EVAL_SCHEME, OutputView, SCHEME } from './outputView';
import { Highlighter } from './highlight';
import { FormEnvelope, FormModel, FormNameRules, FormView } from './formView';
import { setExtensionUri } from './webviewParts';
import { Console, ConsoleResult, HISTORY } from './console';
import { DocView, plainSpans } from './docView';
import { indentCommand, registerFormatting } from './formatting';
import { DocHtml as DocHtmlResult, esc, xdocHtml } from './xdoc';
import {
  BookLink,
  CertifyReply,
  CompleteReply,
  DefinitionReply,
  DescribeReply,
  hoverMarkdown,
  linkRangeOnLine,
  prefixAt,
  BookSymbol,
  DocReply,
} from './names';

/**
 * How long a command that starts a session waits for the first split.
 *
 * Long enough to cover starting ACL2 on a slow machine, short enough that a
 * daemon which never answers does not leave a keypress hanging. Overrunning it
 * is not an error: the session is up and held before the first form, which is
 * where the command would have left it anyway if the file had no forms.
 */
const START_SPLIT_WAIT_MS = 30_000;

/**
 * How long Start waits for the daemon to answer `open`.
 *
 * Generous, because an honest `open` genuinely can take a while: it holds the
 * daemon's protocol thread through the whole ACL2 startup handshake, up to
 * the customization-file timeout, and a second book's open waits behind the
 * first one's. Two minutes is well past all of that.
 *
 * It exists because the alternative is silence. Daemon exit rejects every
 * request in flight, but a daemon that is alive and wedged leaves this
 * promise pending for ever, and Start awaits it -- so the button did nothing,
 * said nothing, and left nothing in the channel to find.
 */
const OPEN_WAIT_MS = 120_000;

const UPDATE_DEBOUNCE_MS = 300;

/**
 * How far auto-admission goes: a position, not a line.
 *
 * A line names a form exactly until two of them share one --
 * `(defun foo (x) x) (defun bar (x) x)` is two forms -- and then only the
 * column says which. An editor always has a real cursor, so it always sends
 * one; the daemon takes a missing column to mean the whole line.
 */
interface Pos {
  line: number;
  column: number | null;
}

/**
 * The held barrier: check through the form anchored here, and no further.
 *
 * A *form*, not a position. The two coincided while the target was whatever
 * the cursor happened to point at, and came apart as soon as the barrier had
 * to survive an edit: a line number is invalidated by every insertion above
 * it, where a form is still the same form.
 *
 * `anchor` is that form's first character, tracked through edits exactly as a
 * pin's is. `null` holds before the first form -- a barrier above everything,
 * which no form can anchor. A session with no barrier at all has no `Held` to
 * begin with, and checks the whole book.
 */
interface Held {
  anchor: Anchor | null;
}

interface BookSession {
  id: string;
  doc: vscode.TextDocument;
  /** The held barrier; null checks the whole book. */
  target: Held | null;
  /**
   * Whether the reader has stopped this session admitting forms.
   *
   * Not a stall, which is what the daemon calls stopping because a form
   * failed or the buffer will not lex: those clear on the next edit and this
   * one does not, which is the whole of the difference and the reason a
   * reader can rely on it while restructuring a file.
   */
  paused?: boolean;
  /** Why the last run stopped, so an interrupt is not reported as an ending. */
  lastSettle?: string;
  lastState?: StateEvent;
  /**
   * Where the reader stopped, when it did: the line of the bracket that is
   * never closed. Held rather than painted and forgotten, because how it is
   * shown depends on the decoration mode, and that can change under it.
   */
  unreadableFrom?: number;
  unreadableWhy?: string;
  /** Latest envelope per unit index, for diagnostics, form view and output view. */
  unitEvents: Map<number, UnitEvent>;
  /**
   * The unit currently printing, and the handle its log is readable under.
   *
   * A running form has no entry in `unitEvents` -- that arrives when it
   * lands -- so this is the only way to read it, and it reads *live*. Both
   * are cleared the moment the form lands or the book settles, so a stale
   * handle can never be mistaken for a running one.
   */
  runningUnit?: number;
  runningRef?: string;
  /**
   * What each unit introduced, once asked for.
   *
   * Cached because the form view follows the *cursor*: `followFormView` runs on
   * every selection change, and fetching on each one put a reply -- and so a
   * re-render -- a moment after every keystroke and every click. A re-render
   * replaces the page, which resets every open dropdown, so the form view looked
   * frozen. Dropped for a unit when that unit is admitted again, which is
   * the only way what it introduced can change.
   */
  rulesByUnit: Map<number, FormNameRules[]>;
  debounce?: ReturnType<typeof setTimeout>;
  /**
   * Resolves when the daemon has split the book for the first time.
   *
   * Only the daemon splits, so until this settles there are no forms and any
   * verb that counts them has nothing to count. The commands that start a
   * session wait on it rather than guessing.
   */
  ready?: Promise<void>;
  markReady?: () => void;
  /** Said once per book: the editor could not decode this file's bytes. */
  warnedEncoding?: boolean;
  /**
   * Whether this document's text can carry this file's bytes.
   *
   * Measured against the file rather than assumed, and only while the two
   * are known to agree -- at start, and after each save. `undefined` means
   * not measured yet, which is treated as yes.
   */
  carriesFile?: boolean;
  /**
   * The progress notification of the eval in flight, so an `eval_state`
   * event can say *why* it has not come back -- a form queued behind a long
   * proof and one that is grinding look the same from here otherwise.
   */
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
  /** A certification is in flight; a second is refused rather than queued. */
  certifying?: boolean;
  /**
   * The stale dependencies last mentioned in the channel, joined.
   *
   * The daemon re-sends `stale_deps` on every tick until they stop being
   * stale, so without this one recertification of a widely-included book
   * writes a line a second for as long as it takes.
   */
  staleSaid?: string;
}

let client: DaemonClient | undefined;
const books = new Map<string, BookSession>(); // key: document uri
const byId = new Map<string, BookSession>(); // key: daemon book id

/**
 * The session for a document, re-anchored to the object VSCode is actually
 * synchronising.
 *
 * Sessions are keyed by uri so that they outlive a tab, but a session also
 * holds the `TextDocument` it started with -- and a closed document is never
 * reused when the file is opened again. Kept, that reference would go on
 * answering for an object VSCode has frozen: `getText` returns the text as of
 * the close and `version` stops advancing. Close a book's tab, reopen it, and
 * every edit would sync the daemon to pre-close content stamped with a
 * revision that never moves, while the `=== s.doc` checks that gate painting
 * stopped matching the moment the new object appeared.
 *
 * Every caller has the live document in hand already, so taking it back is
 * the whole fix. Where the objects are the same -- the ordinary case -- this
 * is an assignment of what is already there.
 */
function bookFor(doc: vscode.TextDocument): BookSession | undefined {
  const s = books.get(doc.uri.toString());
  if (s) {
    s.doc = doc;
  }
  return s;
}

let decorations: Decorations;
let outputView: OutputView;
let evalView: OutputView;
let docs: DocView;
let docHistory: { book: string; name: string }[] = [];
// The topic on screen, kept so a theme or grammar change can recolour its
// code without going back to the daemon for text that has not changed.
let lastDoc: { name: string; rendered: DocHtmlResult } | undefined;
let formView: FormView;
let highlighter: Highlighter;
let status: vscode.StatusBarItem;
let channel: vscode.OutputChannel;
let windowIcon: { light: vscode.Uri; dark: vscode.Uri };

function mode(): DecorationMode {
  return vscode.workspace.getConfiguration('acl2-session').get<DecorationMode>('decorations', 'gutter');
}

/**
 * How wide a hover's terms are laid out.
 *
 * A setting rather than a measurement, and it has to be: the extension API
 * exposes no geometry for a hover -- or for the editor -- and only a webview
 * can measure anything, which a tooltip is not. Defaults to the form view's own
 * ceiling so a term reads the same in both places.
 */
function hoverWidth(): number {
  return vscode.workspace.getConfiguration('acl2-session').get<number>('hoverWidth', 60);
}

export function activate(context: vscode.ExtensionContext): void {
  decorations = new Decorations(context);
  outputView = new OutputView(
    SCHEME,
    'ACL2 Output',
    // Prose, not a parenthesized aside: these two views are highlighted with
    // the output grammar, and `(nothing evaluated yet)` is shaped exactly
    // like an ACL2 term -- opens at column zero, closes at end of line -- so
    // it came out syntax-highlighted, `not` and all. Keep status messages out
    // of that shape.
    'No form under the cursor has been checked yet.'
  );
  evalView = new OutputView(EVAL_SCHEME, 'ACL2 Eval', 'Nothing evaluated yet.');
  // Webview tabs take an icon; the output views cannot, since they are text
  // documents and VSCode takes an editor tab's icon from the file icon theme.
  //
  // Before the views that read it. Assigned after `DocView` was constructed,
  // `docs.iconPath` took the value `windowIcon` had then -- `undefined` --
  // and kept it, so the Docs tab alone showed the generic webview icon.
  windowIcon = {
    light: vscode.Uri.joinPath(context.extensionUri, 'media', 'window-light.svg'),
    dark: vscode.Uri.joinPath(context.extensionUri, 'media', 'window-dark.svg'),
  };
  docs = new DocView();
  docs.iconPath = windowIcon;
  docs.onTopic = (name) => {
    const s = byId.get(docHistory[docHistory.length - 1]?.book ?? '');
    if (s) {
      void openTopic(s, name, true);
    }
  };
  docs.onBack = () => {
    // Drop the current topic, then re-open the one before it.
    docHistory.pop();
    const prev = docHistory.pop();
    const s = prev && byId.get(prev.book);
    if (s && prev) {
      void openTopic(s, prev.name, true);
    }
  };
  formView = new FormView();
  formView.iconPath = windowIcon;
  // Right, because this is a *contextual* item and VSCode's guidance puts
  // those there: it appears and disappears with the active editor and reports
  // that file's session, which makes it a sibling of the language-mode
  // indicator rather than of the global ones on the left. Priority 100 sits it
  // near the built-in editor status, since higher is further left on this side.
  status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
  // The progress readout is also the way back to the form view, for when it has
  // been closed and the gutter says something needs looking at.
  status.command = 'acl2-session.showFormView';
  status.tooltip = 'ACL2: show the form view';
  channel = vscode.window.createOutputChannel('ACL2');
  problems = vscode.languages.createDiagnosticCollection('acl2');
  context.subscriptions.push(problems);

  formView.onCols = (cols) => setLayout(cols);
  // Not the `showOutput` command: that resolves a book from the active text
  // editor, and clicking this focuses the webview, where there is none.
  formView.onShowOutput = (book, index) =>
    void (async () => {
      const s = byId.get(book);
      if (s) {
        await showUnitOutput(s, index);
        await outputView.open();
      }
    })();

  // The grammar and theme load off disk, so the first form view may render
  // before they arrive; refresh once they have, and again whenever the
  // colors the editor paints with change under us.
  setExtensionUri(context.extensionUri);
  here = context.extensionUri;
  highlighter = new Highlighter(context.extensionPath);
  const reload = () => {
    // The status colours are read from the theme too, and a decoration type
    // fixes its colours when it is created -- so a new theme means new types,
    // and a repaint, since disposing a type erases what it drew.
    decorations.reload();
    for (const s of books.values()) {
      paint(s);
    }
    void highlighter.load().then(() => {
      const tokenizer = highlighter.ready ? (t: string) => highlighter.tokenize(t) : undefined;
      formView.tokenizer = tokenizer;
      formView.refresh();
      for (const s of books.values()) {
        if (s.console) {
          s.console.tokenizer = tokenizer;
          s.console.refresh();
        }
      }
      paintDocs();
    });
  };
  reload();
  renewSymbols();
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
    formView,
    docs,
    status,
    channel,
    outputView,
    evalView,
    vscode.workspace.registerTextDocumentContentProvider(SCHEME, outputView),
    vscode.workspace.registerTextDocumentContentProvider(EVAL_SCHEME, evalView),
    vscode.languages.registerHoverProvider({ language: 'acl2' }, hoverProvider),
    vscode.languages.registerDefinitionProvider({ language: 'acl2' }, definitionProvider),
    vscode.languages.registerDocumentLinkProvider({ language: 'acl2' }, linkProvider),
    vscode.languages.registerCodeActionsProvider({ language: 'acl2' }, certifyCodeActions, {
      providedCodeActionKinds: [vscode.CodeActionKind.QuickFix],
    }),
    vscode.commands.registerCommand(
      'acl2-session.certifyIncluded',
      (uri?: vscode.Uri, line?: number) => void certifyIncluded(uri, line)
    ),
    // Indentation. Three providers, none of which needs a session: see
    // `formatting.ts`, and `indent.ts` for why the language configuration
    // cannot carry this.
    ...registerFormatting(),
    // Wrapped rather than passed bare, for the reason `webview.mjs` checks:
    // a bare registration receives whatever VSCode hands the command, and an
    // arrow that declares no parameters cannot quietly grow one.
    vscode.commands.registerCommand('acl2-session.indent', () => indentCommand()),
    // `:` triggers so keyword families appear without a word first; the rest
    // is VSCode's own 24x7 word completion.
    vscode.languages.registerCompletionItemProvider({ language: 'acl2' }, completionProvider, ':'),
    // Registered through a holder: see `renewSymbols` on why it has to be
    // replaceable rather than registered once.
    new vscode.Disposable(() => symbolReg?.dispose()),
    // Wrapped, not passed bare. VSCode hands an `editor/title` command the
    // editor's resource URI as its first argument, so a registered function
    // that takes a parameter receives one whether or not it wants one -- and
    // this one's parameter decides where the session starts.
    vscode.commands.registerCommand('acl2-session.start', () => void startCommand()),
    vscode.commands.registerCommand('acl2-session.evalSelection', () => forBook(evalCommand)),
    vscode.commands.registerCommand('acl2-session.showConsole', () =>
      forBook((s) => consoleFor(s).open())
    ),
    vscode.commands.registerCommand('acl2-session.stop', () => forBook(stopBook)),
    // Pause and resume are one button, so they are one command: a toolbar
    // with two of them would show whichever is inapplicable half the time.
    vscode.commands.registerCommand('acl2-session.pause', () =>
      forBook((s) => setPaused(s, true))
    ),
    vscode.commands.registerCommand('acl2-session.resume', () =>
      forBook((s) => setPaused(s, false))
    ),
    // The book id is optional and comes from the status bar during a
    // certification, where the bar is timing one book and the focus may be
    // anywhere. Everywhere else it is the active editor's, as every other
    // command resolves it.
    vscode.commands.registerCommand('acl2-session.interrupt', (book?: string) => {
      const named = typeof book === 'string' ? byId.get(book) : undefined;
      if (named) {
        void client?.request('interrupt', { book: named.id }).catch(report);
        return;
      }
      forBook((s) => void client?.request('interrupt', { book: s.id }).catch(report));
    }),
    vscode.commands.registerCommand('acl2-session.retractToCursor', () =>
      forBook((s) => {
        const units = s.lastState?.units ?? [];
        const at = cursorTarget();
        // Mid-form snaps forward to that form's end.
        let keep = 0;
        for (const u of units) {
          if (startsAtOrBefore(u, at)) {
            keep = u.index + 1;
          }
        }
        retractTo(s, keep);
      })
    ),
    // The four barrier commands all start the session when there is none.
    // Holding a barrier is a statement about a run, and refusing to hear it
    // until a run exists gets the order backwards: "check as far as here" is
    // most useful said *before* anything has been checked, which is the one
    // moment the old commands answered with "no live session for this file".
    vscode.commands.registerCommand('acl2-session.targetAtCursor', () =>
      onBook((s) => {
        const at = cursorTarget();
        // The form the cursor is in, or the last one starting above it: the
        // same reading the daemon gives a bare position, made explicit here
        // because from now on it is a form that gets held and not a point.
        const units = s.lastState?.units ?? [];
        let pick = -1;
        for (const u of units) {
          if (startsAtOrBefore(u, at)) {
            pick = u.index;
          }
        }
        holdAt(s, pick);
      })
    ),
    vscode.commands.registerCommand('acl2-session.targetTop', () =>
      onBook((s) => holdAt(s, -1))
    ),
    vscode.commands.registerCommand('acl2-session.targetForward', () =>
      onBook((s) => stepBarrierBy(s, 1))
    ),
    vscode.commands.registerCommand('acl2-session.targetBackward', () =>
      onBook((s) => stepBarrierBy(s, -1))
    ),
    vscode.commands.registerCommand('acl2-session.clearTarget', () =>
      forBook((s) => {
        s.target = null;
        sendUpdate(s);
      })
    ),
    vscode.commands.registerCommand('acl2-session.showOutput', () =>
      forBook(async (s) => {
        // Re-targets the pin rather than fighting it. Asking for a particular
        // form's output is as clear a statement of intent as asking to pin
        // one, so the pin follows -- the same way Target at Cursor moves the
        // barrier rather than refusing while one is held.
        if (pinned && pinned.book === s.id) {
          repin(s);
        }
        await followOutputView(s, true);
        await outputView.open();
      })
    ),
    vscode.commands.registerCommand('acl2-session.showDocs', () => forBook(showDocs)),
    vscode.commands.registerCommand('acl2-session.certify', () => forBook(certifyBook)),
    vscode.tasks.registerTaskProvider('acl2', certifyTaskProvider),
    // Neither of these goes through `forBook`: both are invoked from the
    // form view's own title bar, where the focused thing is a webview and there is
    // no active text editor to find a book from.
    vscode.commands.registerCommand('acl2-session.pinFormView', () => takePin()),
    vscode.commands.registerCommand('acl2-session.unpinFormView', () => dropPin()),
    // A third verb for the same pair, because the title bar and everywhere
    // else want different things. The bar shows one of two icons and needs a
    // command per state to swap them; a person reaching for this by name, or
    // one day by key, wants to say the thing they mean once and not care
    // which state it is in.
    vscode.commands.registerCommand('acl2-session.toggleFormViewPin', () =>
      pinned ? dropPin() : takePin()
    ),
    vscode.commands.registerCommand('acl2-session.showFormView', () =>
      forBook((s) => {
        formView.open();
        void followFormView(s);
      })
    ),
    vscode.commands.registerCommand('acl2-session.gotoFrontier', () => forBook(gotoFrontier)),
    vscode.commands.registerCommand('acl2-session.gotoAttention', () => forBook(gotoAttention)),
    // Saving is the moment the buffer becomes the file again, and for a book
    // whose bytes the buffer cannot carry it is the only moment the session
    // can be given the truth. Without this, editing such a file left it
    // failed until the book was closed and reopened -- the next keystroke
    // only re-sent the same replaced bytes.
    vscode.workspace.onDidSaveTextDocument((doc) => {
      const s = bookFor(doc);
      if (!s) {
        return;
      }
      if (s.debounce) {
        clearTimeout(s.debounce);
        s.debounce = undefined;
      }
      s.carriesFile = undefined; // re-measured against what was just written
      sendUpdate(s);
    }),
    vscode.workspace.onDidChangeTextDocument((e) => {
      const s = bookFor(e.document);
      if (!s || e.contentChanges.length === 0) {
        return;
      }
      const es: Edit[] = e.contentChanges.map((c) => ({
        startLine: c.range.start.line + 1,
        startChar: c.range.start.character,
        endLine: c.range.end.line + 1,
        endChar: c.range.end.character,
        newLines: c.text.split('\n').length,
      }));
      if (pinned && pinned.book === s.id) {
        const moved = keepAnchor(pinned.anchor, es);
        if (moved) {
          pinned = { book: pinned.book, anchor: moved };
        } else {
          releasePin('the form it was held on was edited away');
        }
      }
      keepBarrier(s, es);
      if (s.debounce) {
        clearTimeout(s.debounce);
      }
      s.debounce = setTimeout(() => sendUpdate(s), UPDATE_DEBOUNCE_MS);
    }),
    vscode.window.onDidChangeTextEditorSelection((e) => {
      const s = bookFor(e.textEditor.document);
      if (!s) {
        return;
      }
      // Both cursor-following views obey the same pin, because they are two
      // views of one form: the structured summary and the raw log. Held on
      // different forms they would show a form's checkpoints beside another
      // form's output, which is not a workflow anyone wants and was what this
      // did -- the pin existed and only the form view honoured it.
      if (outputView.visible && !(pinned && pinned.book === s.id)) {
        void followOutputView(s, false);
      }
      // A pinned form view is exactly the one that does not follow the cursor.
      if (formView.visible && !(pinned && pinned.book === s.id)) {
        void followFormView(s);
      }
    }),
    vscode.window.onDidChangeVisibleTextEditors((editors) => {
      // Reopening a book's tab brings a *new* document object for the same
      // file, and this fires before any edit or provider call would have
      // re-anchored the session. Do it here, or the identity check in `paint`
      // never matches again and the reopened book stays blank.
      for (const e of editors) {
        bookFor(e.document);
      }
      for (const s of books.values()) {
        paint(s);
      }
    }),
    vscode.window.onDidChangeActiveTextEditor(() => {
      refreshContext();
      // The bar describes the file in front, so a tab switch is a repaint.
      // Without this it went on describing the file you had left.
      refreshStatusBar();
    }),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (
        e.affectsConfiguration('acl2-session.colors') ||
        e.affectsConfiguration('acl2-session.washOpacity') ||
        e.affectsConfiguration('workbench.colorCustomizations')
      ) {
        // A decoration type carries its colours from creation, so a changed
        // palette means new types. The repaint below is not optional after
        // this: disposing a type erases what it was drawing.
        decorations.reload();
      }
      if (e.affectsConfiguration('acl2-session')) {
        for (const s of books.values()) {
          paint(s);
        }
        // termWidth caps the width terms are laid out at, and laying out
        // happens in the daemon now, so this has to go back over the wire.
        formView.reportCols();
      }
      if (e.affectsConfiguration('acl2-session.highlightOutput')) {
        void outputView.refreshLanguage();
        void evalView.refreshLanguage();
      }
    })
  );
  refreshContext();
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
  void vscode.window.showErrorMessage(`ACL2: ${msg}`);
}

/**
 * Hover: what does the world know about the name under the cursor?
 *
 * Only for documents with a live session -- the answer comes from an ACL2
 * that has actually admitted this file, which is the whole difference between
 * this and a static index. Without a session there is nothing honest to say,
 * so nothing is said.
 *
 * The token comes from VSCode's own word range, which uses the ACL2
 * `wordPattern` in language-configuration.json; positions never cross the
 * wire. A cancelled hover -- the cursor moved on -- resolves
 * to nothing rather than racing the reply into a tooltip nobody is reading.
 */
const hoverProvider: vscode.HoverProvider = {
  async provideHover(doc, position, token) {
    const s = bookFor(doc);
    if (!s || !client?.alive) {
      return undefined;
    }
    const range = doc.getWordRangeAtPosition(position);
    if (!range) {
      return undefined;
    }
    const name = doc.getText(range);
    let reply: DescribeReply;
    try {
      reply = await client.request('describe', { book: s.id, name, cols: hoverWidth() });
    } catch {
      // A daemon that went away is reported by the session machinery; a
      // tooltip is not the place to say so a second time.
      return undefined;
    }
    if (token.isCancellationRequested) {
      return undefined;
    }
    const md = hoverMarkdown(reply);
    return md ? new vscode.Hover(new vscode.MarkdownString(md), range) : undefined;
  },
};

/**
 * Go to definition.
 *
 * The daemon answers with a file and, when it could find the form, a line.
 * A missing line is not a failure: it means the world named the file but the
 * name is generated and absent from its text, so the file opens at the top
 * rather than pointing confidently at the wrong form (tier 3/4).
 *
 * A unit index rather than a path means "this file", and the client already
 * holds that unit's span -- so the jump uses the buffer's own coordinates and
 * stays right even if the document has moved on since the unit was admitted.
 */
const definitionProvider: vscode.DefinitionProvider = {
  async provideDefinition(doc, position, token) {
    const s = bookFor(doc);
    if (!s || !client?.alive) {
      return undefined;
    }
    // On an include-book, the definition *is* the book. Worth handling here
    // and not only as a DocumentLink: ctrl-click is not the only way people
    // navigate, and vim mode's `gf` grabs the raw word -- which for
    // `:dir :system` is neither a real path nor a `.lisp` -- so `gd` is the
    // gesture that has to work. The line is checked first so an ordinary
    // lookup never pays for this.
    if (/\(\s*include-book\b/i.test(doc.lineAt(position.line).text)) {
      const target = await includeTargetAt(s, doc, position);
      if (target) {
        return target;
      }
    }
    const range = doc.getWordRangeAtPosition(position);
    if (!range) {
      return undefined;
    }
    let r: DefinitionReply;
    try {
      r = await client.request('definition', { book: s.id, name: doc.getText(range) });
    } catch {
      return undefined;
    }
    if (token.isCancellationRequested || !r.available || !r.found) {
      return undefined;
    }
    if (r.path) {
      // Another file, so its text is not in hand to convert a byte column
      // against; the line is where the jump lands. Definitions written two
      // to a line are rare enough not to be worth opening the file to read.
      const line = Math.max(0, (r.line ?? 1) - 1);
      return new vscode.Location(vscode.Uri.file(r.path), new vscode.Position(line, 0));
    }
    const here = r.line !== undefined
      ? { line: r.line, column: r.column ?? null }
      : r.unit !== undefined
        ? unitStart(s, r.unit)
        : undefined;
    if (here === undefined) {
      return undefined;
    }
    return new vscode.Location(doc.uri, positionOf(doc, here));
  },
};

/**
 * Ctrl-click an `include-book` to open the book it includes.
 *
 * The daemon resolves the paths, because only it knows the session's books
 * directory and only ACL2 knows a project's own `:dir` aliases; the client
 * places the ranges, because only it has the document's UTF-16 coordinates.
 * An include that did not resolve -- an unknown alias, or a book that is not
 * built -- gets no link at all rather than one that opens nothing.
 */
const linkProvider: vscode.DocumentLinkProvider = {
  async provideDocumentLinks(doc, token) {
    const s = bookFor(doc);
    if (!s || !client?.alive) {
      return [];
    }
    let r: { links: BookLink[] };
    try {
      r = await client.request('links', { book: s.id });
    } catch {
      return [];
    }
    if (token.isCancellationRequested) {
      return [];
    }
    const out: vscode.DocumentLink[] = [];
    for (const l of r.links) {
      if (!l.path || l.line < 1 || l.line > doc.lineCount) {
        continue;
      }
      const text = doc.lineAt(l.line - 1).text;
      const span = linkRangeOnLine(text, l.book, charOffset(text, l.column));
      if (!span) {
        continue;
      }
      const range = new vscode.Range(l.line - 1, span[0], l.line - 1, span[1]);
      const link = new vscode.DocumentLink(range, vscode.Uri.file(l.path));
      link.tooltip = l.path;
      out.push(link);
    }
    return out;
  },
};

const SYMBOL_KIND: Record<string, vscode.SymbolKind> = {
  function: vscode.SymbolKind.Function,
  macro: vscode.SymbolKind.Method,
  theorem: vscode.SymbolKind.Event,
  constant: vscode.SymbolKind.Constant,
  include: vscode.SymbolKind.File,
  other: vscode.SymbolKind.Object,
};

/**
 * The outline, for the Outline view, breadcrumbs, ctrl+shift+O and sticky
 * scroll.
 *
 * The daemon builds it from the text, so it covers the whole file rather than
 * the admitted prefix -- an outline that stopped at the frontier would be
 * least useful exactly when a book is long and half-checked, which is when
 * anyone reaches for it. The *status* is added here, from the paint map this
 * side already holds: the daemon has no business in the painting.
 */
let symbolReg: vscode.Disposable | undefined;
/**
 * What each book's outline last showed, so it is only renewed when it would
 * differ.
 *
 * Without this the renewal feeds itself: `drive` settles on every command
 * pass, so a symbols query produces a `settled`, which renews, which makes
 * VSCode query again -- a loop that flickers the Outline view continuously
 * and is started by *any* request, not just this one.
 *
 * Per book, because renewing is not a per-book act: re-registering the
 * provider invalidates VSCode's symbol cache for *every* ACL2 document. One
 * global held whichever book settled last, so with two live books each settle
 * looked like a change to the other's outline and renewed on every pass --
 * exactly the flicker this exists to prevent, restored by the fix for it.
 */
const outlineKeys = new Map<string, string>();

function currentOutlineKey(s: BookSession): string {
  const units = s.lastState?.units ?? [];
  return `${units.length}|${units.map((u) => u.status).join(',')}`;
}

/**
 * Re-register the symbol provider, which is the only way to make VSCode ask
 * again.
 *
 * Document symbols are cached against the document's *version*, and there is
 * no change event a provider can fire -- unlike code lenses, which have one.
 * So the empty answer given before a session existed would stand until the
 * file was edited, and the outline of a book being checked stayed blank for
 * as long as nobody typed. Re-registering invalidates that cache.
 */
function renewSymbols(): void {
  symbolReg?.dispose();
  symbolReg = vscode.languages.registerDocumentSymbolProvider(
    { language: 'acl2' },
    symbolProvider
  );
}

const symbolProvider: vscode.DocumentSymbolProvider = {
  async provideDocumentSymbols(doc, token) {
    const s = bookFor(doc);
    if (!s || !client?.alive) {
      return undefined;
    }
    let r: { symbols: BookSymbol[] };
    try {
      r = await client.request('symbols', { book: s.id });
    } catch {
      return undefined;
    }
    if (token.isCancellationRequested) {
      return undefined;
    }
    const statusOf = new Map<number, string>();
    for (const u of s.lastState?.units ?? []) {
      statusOf.set(u.index, u.status);
    }
    const build = (b: BookSymbol): vscode.DocumentSymbol => {
      // The form's own extent, columns included: two entries on one line are
      // otherwise the same range, and clicking either selects both.
      const from = positionOf(doc, { line: b.line_start, column: b.col_start });
      const to = positionOf(doc, { line: b.line_end, column: b.col_end });
      const range = new vscode.Range(from, to);
      const sym = new vscode.DocumentSymbol(
        b.name,
        b.unit !== undefined ? (statusOf.get(b.unit) ?? '') : '',
        SYMBOL_KIND[b.kind] ?? vscode.SymbolKind.Object,
        range,
        range
      );
      sym.children = (b.children ?? []).map(build);
      return sym;
    };
    return r.symbols.map(build);
  },
};

const COMPLETION_KIND: Record<string, vscode.CompletionItemKind> = {
  function: vscode.CompletionItemKind.Function,
  macro: vscode.CompletionItemKind.Method,
  theorem: vscode.CompletionItemKind.Event,
  constant: vscode.CompletionItemKind.Constant,
  keyword: vscode.CompletionItemKind.Keyword,
  other: vscode.CompletionItemKind.Text,
};

/**
 * Completion from the live world.
 *
 * The daemon filters, because a world holds tens of thousands of names and
 * shipping them per keystroke is a megabyte per keystroke. `incomplete` is
 * passed straight through to VSCode's `isIncomplete`, which is what makes it
 * re-ask as the prefix narrows -- and what covers the first completion of a
 * session, answered from the buffer alone while the census is still being
 * built on an idle tick.
 *
 * A theorem is offered as an Event because VSCode has no icon for "fact";
 * it reads as distinct from functions and macros at a glance, which is the
 * job the icon actually has to do here.
 */
const completionProvider: vscode.CompletionItemProvider = {
  async provideCompletionItems(doc, position, token) {
    const s = bookFor(doc);
    if (!s || !client?.alive) {
      return undefined;
    }
    const prefix = prefixAt(doc.lineAt(position.line).text, position.character);
    let r: CompleteReply;
    try {
      r = await client.request('complete', { book: s.id, prefix });
    } catch {
      return undefined;
    }
    if (token.isCancellationRequested) {
      return undefined;
    }
    const items = r.items.map((i) => {
      const c = new vscode.CompletionItem(
        i.name,
        COMPLETION_KIND[i.kind] ?? vscode.CompletionItemKind.Text
      );
      if (i.book) {
        c.detail = i.book.replace(/^\(:system \. "|"\)$/g, '');
      }
      return c;
    });
    return new vscode.CompletionList(items, r.incomplete);
  },
};

/**
 * The book an `include-book` at this position refers to, if the cursor is on
 * one. Shares the daemon's resolution with the link provider rather than
 * re-deriving it, so the two can never disagree about where a book lives.
 */
async function includeTargetAt(
  s: BookSession,
  doc: vscode.TextDocument,
  position: vscode.Position
): Promise<vscode.Location | undefined> {
  let r: { links: BookLink[] };
  try {
    r = await client!.request('links', { book: s.id });
  } catch {
    return undefined;
  }
  for (const l of r.links) {
    if (!l.path || l.line !== position.line + 1) {
      continue;
    }
    const text = doc.lineAt(position.line).text;
    const from = charOffset(text, l.column);
    const span = linkRangeOnLine(text, l.book, from);
    // Anywhere in the form counts, not just inside the quotes: `gd` with the
    // cursor on `include-book` itself should go there too. Bounded below by
    // the form's own column, so a second include on the line does not claim
    // the cursor sitting in the first.
    if (span && position.character >= from && position.character <= span[1] + 1) {
      return new vscode.Location(vscode.Uri.file(l.path), new vscode.Position(0, 0));
    }
  }
  return undefined;
}

/**
 * Certify the book with cert.pl.
 *
 * Saves first, because certification runs against the file on **disk**: it is
 * what you would get from a shell, and certifying one thing while looking at
 * another is the silent lie this design refuses everywhere else. That is the
 * VSCode-conventional behaviour for a build command anyway.
 *
 * The session is untouched -- `cert.pl` is a separate process tree, so
 * checking carries on and queries keep answering while it runs. The build is
 * followed in a progress notification (`certifyWithProgress`), not the status
 * bar: a certification can take minutes and outlive the focus that started
 * it, and the notification survives a tab switch and a dismissal where the
 * status bar's single slot did not.
 */
async function certifyBook(s: BookSession): Promise<void> {
  if (s.certifying) {
    void vscode.window.showInformationMessage('ACL2: already certifying this book');
    return;
  }
  if (s.doc.isDirty && !(await s.doc.save())) {
    void vscode.window.showWarningMessage('ACL2: could not save before certifying');
    return;
  }
  s.certifying = true;
  const jobs = vscode.workspace
    .getConfiguration('acl2-session')
    .get<number>('certifyJobs', 4);
  try {
    await certifyWithProgress({
      label: basename(s.doc.fileName),
      start: () => client!.request('certify', { book: s.id, jobs }) as Promise<CertifyReply>,
      cancel: () => void client?.request('interrupt', { book: s.id }).catch(report),
      showLog: (ref, lines) => showCertLog(s.id, ref, lines),
    });
  } finally {
    s.certifying = false;
  }
}

/** Dependency paths with a certification queued or running, so a second
 *  request for the same book is refused rather than opening a second driver
 *  on it. The current book is guarded separately by `s.certifying`. */
const certifyingDeps = new Set<string>();

/**
 * Certify a book this editor does not have open -- a dependency named on an
 * `include-book` line whose `Uncertified`/`Skip-proofs` warning offers this
 * as its fix.
 *
 * The core certifies a file, not a session (it never consults the session),
 * so the daemon has no path-based verb; instead a driver is opened *scoped to
 * the dependency*, certified, and closed. The open is done inside `start`, so
 * it happens only when this reaches the front of the queue -- a request left
 * waiting behind other builds holds no ACL2 in the meantime. The close waits
 * until `certifyWithProgress` has fully resolved, so the log the failure path
 * reads is still there when "Show Log" asks for it.
 */
async function certifyDependency(path: string): Promise<void> {
  const label = basename(path);
  if (certifyingDeps.has(path)) {
    void vscode.window.showInformationMessage(`ACL2: ${label} is already being certified`);
    return;
  }
  certifyingDeps.add(path);
  const jobs = vscode.workspace
    .getConfiguration('acl2-session')
    .get<number>('certifyJobs', 4);
  let depId: string | undefined;
  try {
    await certifyWithProgress({
      label,
      start: async () => {
        const opened = (await client!.request('open', { path })) as { book: string };
        depId = opened.book;
        return client!.request('certify', { book: depId, jobs }) as Promise<CertifyReply>;
      },
      cancel: () => {
        if (depId) {
          void client?.request('interrupt', { book: depId }).catch(report);
        }
      },
      showLog: (ref, lines) => (depId ? showCertLog(depId, ref, lines) : Promise.resolve()),
    });
  } finally {
    if (depId) {
      await client?.request('close', { book: depId }).catch(report);
    }
    certifyingDeps.delete(path);
  }
}

/** The warnings whose fix is to certify the book named on their line. Both
 *  are about an included book being untrustworthy, which certifying repairs. */
const CERTIFIABLE = new Set([warningMessage('uncertified'), warningMessage('skip-proofs')]);

/**
 * Offer "Certify included book" on an `Uncertified`/`Skip-proofs` warning.
 *
 * The path is not resolved here -- resolving means asking the daemon, and this
 * runs on every cursor move over a diagnostic. The action carries only the
 * document and line; `certifyIncluded` does the resolving once, when invoked.
 */
const certifyCodeActions: vscode.CodeActionProvider = {
  provideCodeActions(doc, _range, context) {
    const actions: vscode.CodeAction[] = [];
    for (const d of context.diagnostics) {
      if (d.source !== 'ACL2' || !CERTIFIABLE.has(d.message)) {
        continue;
      }
      const a = new vscode.CodeAction('Certify included book', vscode.CodeActionKind.QuickFix);
      a.diagnostics = [d];
      a.command = {
        command: 'acl2-session.certifyIncluded',
        title: 'Certify included book',
        arguments: [doc.uri, d.range.start.line],
      };
      actions.push(a);
    }
    return actions;
  },
};

/**
 * Resolve the include-book on `line` to a file and certify it.
 *
 * The daemon's `links` already resolves each include-book to its path (and
 * leaves `path` absent for a `:dir` alias only ACL2 knows, or a file that is
 * not there) -- the same resolution ctrl-click uses -- so this reuses it
 * rather than re-deriving `:dir :system` and friends.
 */
async function certifyIncluded(uri?: vscode.Uri, line?: number): Promise<void> {
  if (!(uri instanceof vscode.Uri) || typeof line !== 'number') {
    return;
  }
  const s = books.get(uri.toString());
  if (!s || !client?.alive) {
    void vscode.window.showWarningMessage('ACL2: no live session for this file');
    return;
  }
  let reply: { links: BookLink[] };
  try {
    reply = await client.request('links', { book: s.id });
  } catch (e) {
    report(e);
    return;
  }
  const hit = reply.links.find((l) => l.line === line + 1 && l.path);
  if (!hit?.path) {
    void vscode.window.showWarningMessage(
      "ACL2: couldn't resolve the included book's path to certify it"
    );
    return;
  }
  await certifyDependency(hit.path);
}

/**
 * The one runner every certification serializes through -- current book and
 * dependency alike -- so no two builds overlap. See `makeSerializer`.
 */
const certSerialize = makeSerializer();

/**
 * Follow one certification in a cancellable progress notification.
 *
 * The whole certification UI, current book and dependency alike. The toast is
 * up from the moment it is asked for -- so its presence answers "is it still
 * going" -- and it passes through two phases:
 *
 *   - **queued**, while an earlier build runs: the message says so, and Cancel
 *     simply drops this one out of the queue, touching no build;
 *   - **running**: the message becomes an elapsed clock ticked once a second,
 *     and Cancel interrupts the build.
 *
 * Dismissing the toast never cancels; the notification stays in the bell with
 * its clock and Cancel intact.
 *
 * `start` fires the certify request and resolves with its reply; `cancel`
 * interrupts the build behind it. An interrupt makes `start` reject, so the
 * user's Cancel is told apart from a real fault -- reporting "interrupted" as
 * an error is the one wrong thing to do here.
 */
function certifyWithProgress(opts: {
  label: string;
  start: () => Promise<CertifyReply>;
  cancel: () => void;
  showLog: (ref: string, lines: number) => Promise<void>;
}): Thenable<void> {
  return vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      cancellable: true,
      title: `Certifying ${opts.label}`,
    },
    async (progress, token) => {
      progress.report({ message: 'queued…' });
      let cancelled = false;
      let running = false;
      token.onCancellationRequested(() => {
        cancelled = true;
        // Only a *running* build has something to interrupt; a queued one is
        // dropped by the guard at the top of the job below.
        if (running) {
          opts.cancel();
        }
      });
      await certSerialize(async () => {
        if (cancelled) {
          // Cancelled while still waiting its turn: never started, nothing to
          // interrupt, just fall through to the notice below.
          return;
        }
        running = true;
        const started = Date.now();
        const tick = setInterval(() => {
          const secs = Math.floor((Date.now() - started) / 1000);
          progress.report({
            message: `${Math.floor(secs / 60)}:${String(secs % 60).padStart(2, '0')}`,
          });
        }, 1000);
        try {
          const r = await opts.start();
          if (r.status === 'certified') {
            void vscode.window.showInformationMessage(
              `ACL2: certified ${opts.label} in ${(r.elapsed_ms / 1000).toFixed(1)}s`
            );
          } else {
            // The blamed book is usually a dependency rather than this one,
            // and saying which is the difference between a report and "it
            // broke".
            const blamed = (r.failed_books ?? [])[0];
            const where = blamed && basename(blamed) !== opts.label ? ` in ${basename(blamed)}` : '';
            const pick = await vscode.window.showErrorMessage(
              `ACL2: certification of ${opts.label} failed${where}` +
                (r.error_line ? `: ${r.error_line}` : ''),
              'Show Log'
            );
            if (pick === 'Show Log' && r.output_ref) {
              await opts.showLog(r.output_ref, r.output_lines);
            }
          }
        } catch (e) {
          if (cancelled) {
            void vscode.window.showInformationMessage(
              `ACL2: certification of ${opts.label} cancelled`
            );
          } else {
            report(e);
          }
        } finally {
          clearInterval(tick);
        }
      });
      if (cancelled && !running) {
        void vscode.window.showInformationMessage(
          `ACL2: certification of ${opts.label} cancelled`
        );
      }
    }
  );
}

function basename(p: string): string {
  return p.split(/[\\/]/).pop() ?? p;
}

async function showCertLog(book: string, ref: string, lines: number): Promise<void> {
  try {
    const out = await client!.request('output', {
      book,
      output_ref: ref,
      mode: 'tail',
      lines: Math.min(lines, 4000),
    });
    outputView.set(book, null, out.text as string);
    await outputView.open();
  } catch (e) {
    report(e);
  }
}

/**
 * The same thing as a build task, so Ctrl+Shift+B reaches it the way it
 * reaches `make` everywhere else.
 *
 * The task is a thin trigger for the command rather than a shell execution:
 * the structured completion -- which book was blamed, and its error line --
 * is exactly what a terminal would flatten.
 */
const certifyTaskProvider: vscode.TaskProvider = {
  provideTasks(): vscode.Task[] {
    const t = new vscode.Task(
      { type: 'acl2' },
      vscode.TaskScope.Workspace,
      'certify',
      'acl2',
      new vscode.CustomExecution(async () => new CertifyTerminal())
    );
    t.group = vscode.TaskGroup.Build;
    return [t];
  },
  resolveTask(): vscode.Task | undefined {
    return undefined;
  },
};

/** A task that just runs the command; the reporting lives there. */
class CertifyTerminal implements vscode.Pseudoterminal {
  private writeEmitter = new vscode.EventEmitter<string>();
  onDidWrite = this.writeEmitter.event;
  private closeEmitter = new vscode.EventEmitter<number>();
  onDidClose = this.closeEmitter.event;

  open(): void {
    this.writeEmitter.fire('Certifying via ACL2: Certify Book...\r\n');
    void vscode.commands.executeCommand('acl2-session.certify').then(() => {
      this.closeEmitter.fire(0);
    });
  }

  close(): void {
    /* the command owns the build; interrupting is a separate action */
  }
}

/**
 * Show the documentation for the name under the cursor.
 *
 * Rendered as text rather than HTML for now: the full renderer -- `<see>`
 * links you can follow, KaTeX, `@(def …)` resolved against the world -- wants
 * a webview of its own, and this is the part that is useful without one.
 */
async function showDocs(s: BookSession): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  const range = editor && editor.document.getWordRangeAtPosition(editor.selection.active);
  const name = range ? editor!.document.getText(range) : '';
  if (!name) {
    void vscode.window.showInformationMessage('ACL2: put the cursor on a name first');
    return;
  }
  await openTopic(s, name, false);
}

/**
 * Show one topic, following a cross-reference or opening a fresh one.
 *
 * The history lives here rather than in the view, because going back means
 * re-fetching: only the extension can talk to the daemon, and a topic body is
 * not worth keeping copies of once it has been read.
 */
async function openTopic(s: BookSession, name: string, keepOpen: boolean): Promise<void> {
  let r: DocReply;
  try {
    r = await client!.request('doc', { book: s.id, name });
  } catch (e) {
    report(e);
    return;
  }
  if (!r.found) {
    const why = r.available
      ? `no documentation for ${name}`
      : `the session is busy; try ${name} again in a moment`;
    if (keepOpen) {
      // Mid-navigation: say it in the window rather than in a toast the
      // reader has to dismiss to get back to what they were reading.
      //
      // Escaped, because `name` need not be a name: following a cross
      // reference hands us whatever the book wrote in `<see topic="...">`,
      // and this is the one path that puts it in the page as markup rather
      // than as a link's text.
      const safe = esc(why);
      // Pushed like any other topic, because `onBack` pops twice -- the
      // current entry, then the one to return to -- and that only works if
      // what is on screen is on the stack. An error page that showed itself
      // without pushing made Back pop the topic the reader came *from* as if
      // it were current, find nothing beneath it, and do nothing: stuck on
      // the error page with the previous topic gone from the history.
      //
      // `> 1` for the same reason `paintDocs` uses it: the entry on screen is
      // one of the entries counted.
      docHistory.push({ book: s.id, name });
      docs.show({ name, html: `<p>${safe}.</p>`, code: [], canGoBack: docHistory.length > 1 });
    } else {
      void vscode.window.showInformationMessage(`ACL2: ${why}`);
    }
    return;
  }
  docHistory.push({ book: s.id, name });
  const body = [r.short, r.long].filter(Boolean).join('\n');
  lastDoc = { name, rendered: xdocHtml(body) };
  paintDocs();
}

/**
 * Put the current topic on screen, coloured with whatever the tokenizer can
 * offer right now.
 *
 * Separate from fetching it, because the grammar and theme load off disk and
 * can arrive *after* a window is already open -- and because a theme change
 * has to recolour what is on screen without asking the daemon again for text
 * that has not changed. The same reason the form view re-renders on reload.
 */
function paintDocs(): void {
  if (!lastDoc) {
    return;
  }
  const code = lastDoc.rendered.code.map((c) =>
    highlighter.ready ? { slot: c.slot, lines: highlighter.tokenize(c.text) } : plainSpans(c)
  );
  docs.show({
    name: lastDoc.name,
    html: lastDoc.rendered.html,
    code,
    canGoBack: docHistory.length > 1,
  });
}

/** Where a unit starts, from the last state event. */
function unitStart(s: BookSession, index: number): Pos | undefined {
  const u = s.lastState?.units.find((x) => x.index === index);
  return u && { line: u.line_start, column: u.col_start };
}

/** A daemon position as an editor one, converting the column it counts in bytes. */
function positionOf(doc: vscode.TextDocument, at: Pos): vscode.Position {
  const line = Math.min(Math.max(at.line, 1), doc.lineCount) - 1;
  const character = at.column === null ? 0 : charOffset(doc.lineAt(line).text, at.column);
  return new vscode.Position(line, character);
}

/**
 * Like `forBook`, but starts the session first when there is not one.
 *
 * For the verbs that make sense on a book nobody has started yet -- every one
 * of them is about where a run should *stop*, which is worth saying before it
 * begins. The barrier is placed after the session exists, so the very first
 * `update` carries it and nothing is admitted only to be rolled back.
 */
function onBook(f: (s: BookSession) => unknown): void {
  const editor = vscode.window.activeTextEditor;
  const s = editor && bookFor(editor.document);
  if (s) {
    void f(s);
    return;
  }
  // Held before the first form from the very first `update`, so the session
  // comes up having checked nothing. Not politeness: `f` is about to name a
  // form by counting them, and a book left to run free would be admitting
  // forms while it waited to be told where to stop.
  void startCommand({ anchor: null }).then(async () => {
    const started = editor && bookFor(editor.document);
    if (!started) {
      return;
    }
    // The daemon does the splitting, so there are no forms to count until it
    // has. A book that never splits -- unreadable, or no daemon -- leaves this
    // pending, and the barrier stays where `start` put it.
    await Promise.race([
      started.ready ?? Promise.resolve(),
      new Promise<void>((res) => setTimeout(res, START_SPLIT_WAIT_MS)),
    ]);
    void f(started);
  });
}

/**
 * Re-seat the barrier on the split the daemon actually made.
 *
 * `keepBarrier` tracks the anchor through edits, but the anchor is a claim
 * about where a form starts and only the daemon splits the file. A change can
 * leave the anchor intact by the edit's own arithmetic and still dissolve the
 * form -- delete a closing paren and two forms become one, whose head is the
 * *earlier* of the two -- and then the anchor names a point inside a form
 * rather than the start of one. Left alone that resolves to whatever form
 * contains it, which is a form the reader never pointed at.
 *
 * `barrierIndex` rounds such an anchor up, and this writes the rounding back
 * so the next edit tracks the form that is really there.
 *
 * Only against a split of the buffer as it stands: mid-edit the daemon's
 * units describe an older text, and rounding up against those would walk the
 * barrier backwards a form at a time for as long as someone kept typing.
 */
function settleBarrier(s: BookSession): void {
  const units = s.lastState?.units ?? [];
  if (!s.target?.anchor || units.length === 0 || s.lastState?.revision !== s.doc.version) {
    return;
  }
  const i = barrierIndex(s.target.anchor, units);
  const u = i >= 0 ? units[i] : undefined;
  if (u && u.line_start === s.target.anchor.line && u.col_start === s.target.anchor.column) {
    return;
  }
  s.target = { anchor: u ? anchorOf(s.doc, u) : null };
}

/**
 * Carry the barrier across an edit.
 *
 * The same machinery the pin uses, and it has to be: a barrier that drifts
 * by a line admits or withholds the wrong form, and both are wrong silently.
 * What differs is the answer when the anchor does not survive. A pin lets go,
 * which is the conservative end of *that* feature -- showing nothing beats
 * showing the wrong form. Letting go here would mean no barrier, which is
 * "check the whole book": the least conservative thing available, and reached
 * by way of the reader deleting something.
 *
 * So it rounds up instead, to the nearest form above that does survive the
 * same edits, and to the top when none does. Each candidate is judged by
 * `keepAnchor` rather than assumed safe for being higher up -- one edit can
 * span both -- and each failure walks one form further from the change.
 */
function keepBarrier(s: BookSession, es: readonly Edit[]): void {
  if (!s.target?.anchor) {
    return;
  }
  const moved = keepAnchor(s.target.anchor, es);
  if (moved) {
    s.target = { anchor: moved };
    return;
  }
  const units = s.lastState?.units ?? [];
  // Where it stood before the edit, so the walk starts from the right place
  // even though the anchor itself is now unusable.
  const from = barrierIndex(s.target.anchor, units);
  for (let i = from - 1; i >= 0; i--) {
    // Built against the document as it is *now*: the pre-edit text is already
    // gone, and the UTF-16 offset is only consulted for an edit on the
    // anchor's own line. Getting it wrong there makes `keepAnchor` decline,
    // which walks one further up -- wrong in the safe direction.
    const up = keepAnchor(anchorOf(s.doc, units[i]), es);
    if (up) {
      s.target = { anchor: up };
      return;
    }
  }
  s.target = { anchor: null };
}

/** Move the barrier by whole forms, taking hold of a free-running book. */
function stepBarrierBy(s: BookSession, delta: number): void {
  const units = s.lastState?.units ?? [];
  const from = s.target ? barrierIndex(s.target.anchor, units) : frontierIndex(s);
  holdAt(s, stepBarrier(from, delta, units.length));
}

function forBook(f: (s: BookSession) => unknown): void {
  const editor = vscode.window.activeTextEditor;
  const s = editor && bookFor(editor.document);
  if (!s) {
    // A button rather than an instruction. The old message spelled out the
    // command to run, which is longer to read than the thing is to do, and
    // the notifications guidance asks for an action wherever there is one.
    void vscode.window
      .showInformationMessage('ACL2: no live session for this file', 'Start')
      .then((pick) => {
        if (pick === 'Start') {
          void startCommand();
        }
      });
    return;
  }
  void f(s);
}

/** A form's first character, in every coordinate the barrier needs. */
function anchorOf(doc: vscode.TextDocument, u: { line_start: number; col_start: number }): Anchor {
  const line = Math.min(Math.max(u.line_start, 1), doc.lineCount);
  return {
    line: u.line_start,
    column: u.col_start,
    character: charOffset(doc.lineAt(line - 1).text, u.col_start),
  };
}

/** The index the barrier stands after, `-1` for before the first form. */
function barrierOf(s: BookSession): number {
  const units = s.lastState?.units ?? [];
  // No barrier at all is the whole book, whose last form is the last index.
  // Not a special case anywhere else: every step and every redraw wants a
  // number, and "the end" is the honest one.
  return s.target ? barrierIndex(s.target.anchor, units) : units.length - 1;
}

/** Hold the barrier after form `i`; `-1` holds before the first. */
function holdAt(s: BookSession, i: number): void {
  const units = s.lastState?.units ?? [];
  const u = i >= 0 ? units[i] : undefined;
  s.target = { anchor: u ? anchorOf(s.doc, u) : null };
  sendUpdate(s);
}

/**
 * Where the barrier falls, for drawing: the end of the last form that will be
 * checked.
 *
 * After the last included form rather than before the first excluded one, and
 * that choice is free now in a way it was not before. While the mark lived in
 * the gutter it had to go beside the *beyond* form, because a gutter cell holds
 * one icon and marking an included form would have displaced its verdict. In
 * the text nothing is displaced, so the mark can sit where the statement
 * actually is -- everything up to here is in -- and it has somewhere to sit
 * when the target is the last form in the book, where "the first form beyond"
 * is nothing at all.
 *
 * Read from the *client's* barrier rather than from the daemon's statuses, so
 * the mark moves when the reader moves it rather than a round trip later.
 */
function targetMark(s: BookSession): Pos | null {
  const units = s.lastState?.units ?? [];
  if (!s.target) {
    return null;
  }
  const last = units[barrierOf(s)];
  // Held above everything: no form to sit after, so it leads the file.
  return last ? { line: last.line_end, column: last.col_end } : { line: 0, column: null };
}

/**
 * The last form in the world: where a step starts from when nothing is held.
 *
 * A session with no barrier is racing through the book, and "back one" said
 * against the *book* would move a barrier the reader cannot see and stop
 * nothing. Said against the frontier it does what stepping means -- take hold
 * where the run has actually got to, and move from there.
 */
function frontierIndex(s: BookSession): number {
  const units = s.lastState?.units ?? [];
  let last = -1;
  for (const u of units) {
    if (u.status === 'admitted' || u.status === 'conditional' || u.status === 'skipped') {
      last = u.index;
    }
  }
  return last;
}

/** Where the cursor is, in the daemon's coordinates. */
function cursorTarget(): Pos {
  const editor = vscode.window.activeTextEditor;
  if (!editor) {
    return { line: 1, column: null };
  }
  const pos = editor.selection.active;
  return {
    line: pos.line + 1,
    column: byteColumn(editor.document.lineAt(pos.line).text, pos.character),
  };
}

/** Whether a unit starts at or before a position. */
function startsAtOrBefore(u: UnitState, at: Pos): boolean {
  if (u.line_start !== at.line || at.column === null) {
    return u.line_start <= at.line;
  }
  return u.col_start <= at.column;
}

/**
 * A message with things to do about it.
 *
 * Every failure here is one the extension can name exactly, so every one can
 * offer the fix rather than the name of a setting to go and find. That is what
 * the notifications guidance asks for, and it is the same trade "no live
 * session" made when its instruction became a Start button.
 */
function act(message: string, actions: Record<string, () => void>): Thenable<void> {
  const names = Object.keys(actions);
  return vscode.window.showErrorMessage(message, ...names).then((pick) => {
    if (pick) {
      actions[pick]();
    }
  });
}

/** Where this extension is installed, for the copy of the README beside it. */
let here: vscode.Uri | undefined;

function openSetting(id: string): void {
  void vscode.commands.executeCommand('workbench.action.openSettings', id);
}

/**
 * The install steps, from the copy that shipped with *this* extension.
 *
 * Not a link to the repository: these are version-specific -- a `cargo install`
 * from a checkout -- so the right copy is the one beside the code actually
 * running, which also works with no network and cannot rot when files move.
 */
function showInstallSteps(): void {
  if (!here) {
    return;
  }
  const readme = vscode.Uri.joinPath(here, 'README.md');
  void vscode.commands.executeCommand('markdown.showPreview', readme).then(undefined, () =>
    vscode.window.showTextDocument(readme)
  );
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
  // Kept, not just logged. A daemon that refuses to start says why on its way
  // out -- "no ACL2 to run", with the usage -- and that is the one sentence
  // worth showing. Without it the reader is told `daemon exited`, which is the
  // symptom, while the cause sits in a channel nobody has been told to open.
  let dying = '';
  c.on('stderr', (s: string) => {
    channel.append(s);
    dying = (dying + s).split('\n').filter((l) => l.trim()).slice(-4).join('\n');
  });
  c.on('daemon-error', (p: { message?: string }) =>
    channel.appendLine(`daemon: ${p?.message ?? 'unknown error'}`)
  );
  // One notification per death: a failure to spawn also ends in `exit`, and
  // the reader who has just been told the binary is missing does not need to
  // be told again that it is gone.
  let told = false;
  c.on('spawn-error', (e: Error) => {
    told = true;
    void act(`ACL2: cannot start the daemon (${e.message})`, {
      'Set Path\u2026': () => openSetting('acl2-session.daemonPath'),
      'Install Steps': showInstallSteps,
    });
  });
  c.on('exit', () => {
    // A newer client may already have replaced this one; that death is its
    // predecessor's, and none of this belongs to it.
    if (client !== c) {
      return;
    }
    client = undefined;
    // The sessions died with the daemon. Their book ids mean nothing to a
    // fresh one, so there is nothing here to reconnect -- and leaving them
    // in the map is worse than useless: the gutter goes on asserting an
    // admitted prefix that no session backs any more, and Start finds the
    // stale entry and answers by sending it an update, which with no client
    // is `client?.notify` on undefined -- silently nothing at all. Dropping
    // them clears the paint and lets Start mean what it says.
    const orphaned = [...books.values()];
    for (const s of orphaned) {
      dropBook(s);
    }
    if (told) {
      return;
    }
    told = true;
    if (orphaned.length > 0) {
      void act('ACL2: the daemon exited; its sessions are gone', {
        'Show Log': () => channel.show(true),
        Start: () => void startCommand(),
      });
      return;
    }
    // It died before there was anything to lose, which is what a
    // misconfiguration looks like: the binary ran and refused. Its own words,
    // because "no ACL2 to run" is the fix and `daemon exited` is not.
    const why = dying.split('\n')[0] || 'the daemon exited';
    void act(`ACL2: ${why}`, {
      'Show Log': () => channel.show(true),
      // Offered only when the daemon is complaining about the image, since
      // that is the one it cannot find for itself and the one this setting
      // fixes.
      ...(/acl2|image/i.test(dying)
        ? { 'Set Image\u2026': () => openSetting('acl2-session.defaultImage') }
        : {}),
    });
  });
  c.on('state', onState);
  c.on('unit', onUnit);
  c.on('tail', onTail);
  c.on('settled', onSettled);
  c.on('eval_state', onEvalState);
  c.on('stale_deps', onStaleDeps);
  client = c;
  return c;
}

async function startCommand(initial?: Held): Promise<void> {
  // Belt and braces against the above: a barrier has an `anchor`, and nothing
  // VSCode passes a command does. Starting with a barrier nobody asked for is
  // the worst way to get this wrong, because it looks like the session simply
  // did nothing.
  const held = initial && 'anchor' in initial ? initial : undefined;
  const editor = vscode.window.activeTextEditor;
  if (!editor || editor.document.uri.scheme !== 'file') {
    // By contract: an untitled buffer has no directory, hence no
    // session; save once so the book has a home, then live in the buffer.
    void vscode.window.showErrorMessage('ACL2: save the book once, then start');
    return;
  }
  const doc = editor.document;
  const key = doc.uri.toString();
  // Through `bookFor`, because running Start on a book whose tab was closed
  // and reopened is exactly how a session ends up holding a document VSCode
  // has frozen -- and this path would otherwise answer by pushing its stale
  // text at the daemon.
  const existing = bookFor(doc);
  if (existing) {
    // Play means resume when there is something to resume. The button shows
    // the same triangle in both states and the key is the same key, because
    // from the reader's side it is the same gesture: get on with it.
    if (existing.paused) {
      setPaused(existing, false);
    } else {
      sendUpdate(existing);
    }
    return;
  }
  const c = ensureClient();
  try {
    // A timeout is not a cancellation: the daemon knows nothing about it and
    // goes on opening. If it then succeeds, the reply is the only word we will
    // ever get about a book that now exists -- registered, with a driver
    // thread and a live ACL2 behind it -- and dropping it left that session
    // orphaned until daemon exit, with a retried Start opening a second one
    // for the same file beside it. So the late reply closes what it names.
    const r = await c.request('open', { path: doc.fileName }, OPEN_WAIT_MS, (late) => {
      const id = (late as { book?: string })?.book;
      if (!id) {
        return;
      }
      channel.appendLine(
        `open of ${doc.fileName} finished after it was given up on; closing session ${id}`
      );
      void c.request('close', { book: id }).catch(() => undefined);
    });
    const s: BookSession = {
      id: r.book as string,
      doc,
      // Placed before the first `update`, which is the point: a barrier
      // arriving a moment later would arrive after the daemon had already
      // begun on a book it was told to check all of.
      target: held ?? null,
      ready: undefined,
      unitEvents: new Map(),
      rulesByUnit: new Map(),
      evalQueue: [],
      evalReplies: new Map(),
    };
    s.ready = new Promise<void>((res) => {
      s.markReady = res;
    });
    books.set(key, s);
    byId.set(s.id, s);
    // The outline could not be answered before this session existed, and
    // VSCode will not ask again on its own.
    outlineKeys.delete(s.id);
    renewSymbols();
    status.show();
    setStatus(s, '$(sync~spin) ACL2 starting');
    channel.appendLine(`opened ${doc.fileName} as ${s.id} under ${r.image}`);
    void noteProveOverride(r.prove_output_overridden as string | undefined);
    sendUpdate(s);
    refreshContext();
    // Starting and opening the form view are one action, because there is no
    // moment where you want the session but not the place its failures
    // explain themselves. It opens with preserveFocus, so it costs the
    // cursor nothing.
    formView.open();
    void followFormView(s);
  } catch (e) {
    report(e);
  }
}

// Shown at most once per window, so opening several books does not stack up
// several identical toasts before the reader gets to dismiss one. A window
// reload shows it again; "Don't show again" silences it for good. The channel
// note below is not gated by this -- the log keeps a line per book.
let proveOverrideNoticed = false;

/**
 * Tell the user, once, that the server re-enabled PROVE output.
 *
 * Their environment -- an `acl2-customization.lsp` that inhibits `prove`,
 * usually -- turned it off, and we turn it back on so proofs can be captured:
 * checkpoints, the induction narrative and per-goal output all vanish without
 * it. This changes nothing about what the file admits, and no flood reaches a
 * terminal, so it is a notice rather than a problem -- logged always, shown
 * once, and silenceable for good. `inherited` is the `inhibit-output-lst` we
 * found, or undefined when nothing was overridden, in which case we say
 * nothing at all.
 */
async function noteProveOverride(inherited: string | undefined): Promise<void> {
  if (!inherited) {
    return;
  }
  channel.appendLine(
    `  note: this environment inhibits PROVE output (inherited inhibit-output-lst ` +
    `${inherited}); re-enabled it so proofs can be captured. The file admits identically.`
  );
  const cfg = vscode.workspace.getConfiguration('acl2-session');
  if (proveOverrideNoticed || !cfg.get<boolean>('warnOnProveOutputOverride', true)) {
    return;
  }
  proveOverrideNoticed = true;
  const why = 'Why?';
  const hush = "Don't show again";
  const choice = await vscode.window.showWarningMessage(
    `ACL2 re-enabled PROVE output to capture proofs — your environment inhibits it ` +
    `(${inherited}). The file admits identically; only what the session captures differs.`,
    why,
    hush
  );
  if (choice === hush) {
    // Writes the same setting a user could edit by hand: one source of truth,
    // reversible, and it travels with Settings Sync.
    await cfg.update('warnOnProveOutputOverride', false, vscode.ConfigurationTarget.Global);
  } else if (choice === why) {
    channel.show();
  }
}

/**
 * Whether the active editor's file has a live session.
 *
 * Drives the editor title-bar button, which is start or stop depending. The
 * key is camelCase: a when-clause parser can read the hyphen in
 * `acl2-session.active` as a minus.
 */
function refreshContext(): void {
  const editor = vscode.window.activeTextEditor;
  const s = editor && books.get(editor.document.uri.toString());
  void vscode.commands.executeCommand('setContext', 'acl2Session.active', !!s);
  void vscode.commands.executeCommand('setContext', 'acl2Session.paused', !!s?.paused);
}

/**
 * Stop admitting forms, or start again.
 *
 * The flag is kept here as well as in the daemon, because the toolbar has to
 * know which button to show before any event comes back -- and a button that
 * changed a beat after the click would read as a click that missed.
 */
function setPaused(s: BookSession, on: boolean): void {
  s.paused = on;
  void client?.request(on ? 'pause' : 'resume', { book: s.id }).catch(report);
  refreshContext();
  updateStatusBar(s);
}

function stopBook(s: BookSession): void {
  void client?.request('close', { book: s.id }).catch(() => undefined);
  dropBook(s);
}

/**
 * Forget a session on this side.
 *
 * Split from `stopBook` because the daemon dying needs everything here and
 * none of the `close` above: there is nobody left to tell.
 */
function dropBook(s: BookSession): void {
  problems.delete(s.doc.uri);
  if (pinned && pinned.book === s.id) {
    releasePin('its book was closed');
  }
  books.delete(s.doc.uri.toString());
  byId.delete(s.id);
  outlineKeys.delete(s.id);
  // The bar cannot go on describing a book that no longer exists, and with
  // this cleared the next live book to ask for it can have it.
  if (statusBook === s.id) {
    statusBook = undefined;
    refreshStatusBar();
  }
  // The transcript is answers from a world that no longer exists.
  s.console?.dispose();
  s.console = undefined;
  // A pending debounce would otherwise fire an `update` for a book that is
  // gone -- typing and then stopping inside 300 ms was enough.
  if (s.debounce) {
    clearTimeout(s.debounce);
    s.debounce = undefined;
  }
  for (const editor of vscode.window.visibleTextEditors) {
    if (editor.document === s.doc) {
      decorations.clear(editor);
    }
  }
  if (books.size === 0) {
    status.hide();
    statusBook = undefined;
  }
  refreshContext();
}

/**
 * Send the buffer, or say that the buffer is the file.
 *
 * ACL2 reads bytes: its model is Latin-1, one byte one character, and
 * `books/kestrel/acl2pl/reader-tests.lisp` writes `#\<byte>` for all 256 of
 * them. VSCode hands us characters, not bytes -- it decoded the file at
 * `files.encoding`, and for a book that is not UTF-8 that decode already
 * replaced 1399 of them with U+FFFD. Nothing this end can undo that: the
 * bytes are gone before `getText()` is called.
 *
 * So for a saved buffer we send no text at all and let the daemon read the
 * file, whose bytes are the truth. Only unsaved edits need the buffer, and
 * only then is the loss unavoidable -- said out loud rather than synced as
 * if it were the book.
 */
function sendUpdate(s: BookSession): void {
  // Measured, not assumed, and only when the buffer and the file are known
  // to agree -- otherwise unsaved edits would be mistaken for an encoding
  // the buffer cannot represent. Almost every book answers yes and keeps the
  // behaviour it always had: the buffer is what gets sent.
  if (s.carriesFile === undefined && !s.doc.isDirty) {
    s.carriesFile = bufferCarriesFile(s.doc);
    if (!s.carriesFile && !s.warnedEncoding) {
      s.warnedEncoding = true;
      channel.appendLine(
        `${s.id}: this file is not valid UTF-8. ACL2 reads bytes, and the editor ` +
        `decoded them, so the buffer is not the file. The session will read the ` +
        `file itself; while there are unsaved edits it cannot, and what it checks ` +
        `will not be what the file holds until you save.`
      );
    }
  }
  // Omitting the text tells the daemon to read the file, whose bytes are the
  // truth. Only worth doing for a file the buffer cannot carry, and only
  // sound while there is nothing unsaved.
  const fromFile = s.carriesFile === false && !s.doc.isDirty;
  s.lastSettle = undefined;
  client?.notify('update', {
    book: s.id,
    text: fromFile ? undefined : s.doc.getText(),
    // The *head* of the target form, not the barrier itself. The daemon's
    // own `goal_unit` reads a target as "the last form covering or ending by
    // this position", so a form's first character resolves back to that form
    // -- and the head is the point that survives editing the form, which its
    // end does not.
    target_line: s.target ? (s.target.anchor?.line ?? 0) : null,
    // The column only means anything if the daemon is splitting the same
    // bytes this column was measured against.
    //
    // Normally it is, by construction: the text sent above *is* the buffer,
    // the daemon splits its UTF-8 encoding, and `byteColumn` measures the
    // UTF-8 encoding of the same buffer. That holds whatever the file's own
    // encoding is, because the file is not what either side is reading.
    //
    // `fromFile` is the case where it does not hold -- the buffer could not
    // carry the file's bytes, so the daemon reads the file instead, and its
    // columns are offsets into bytes this buffer no longer has. Sending one
    // then would point at the wrong place in the wrong units. Lines survive
    // that, since a newline is a newline in any encoding, so the target
    // falls back to naming a whole line: less precise, and not wrong.
    target_column: fromFile ? null : (s.target?.anchor?.column ?? null),
    // The document's own monotone version is exactly the revision the
    // protocol wants stamped back on events.
    revision: s.doc.version,
  });
}

/**
 * Whether this document's text, encoded as UTF-8, is this file's bytes.
 *
 * ACL2's model is Latin-1: it reads bytes, and each byte is one character.
 * VSCode hands an extension characters -- it decoded the file at
 * `files.encoding` first -- so for a book that is not UTF-8 the bytes are
 * already gone by the time `getText()` is called, and no string sent from
 * here can carry them back. One file in a community checkout is like that
 * (`books/kestrel/acl2pl/reader-tests.lisp`, which writes `#\<byte>` for all
 * 256), and it is unreadable through the buffer.
 *
 * Called only when the document is clean, so a difference means the encoding
 * and not an unsaved edit. A file that cannot be read is treated as carried:
 * the buffer is then the only thing there is.
 */
function bufferCarriesFile(doc: vscode.TextDocument): boolean {
  try {
    return readFileSync(doc.fileName).equals(Buffer.from(doc.getText(), 'utf8'));
  } catch {
    return true;
  }
}

/** Retract so that `keep` units remain, and hold the target there. */
function retractTo(s: BookSession, keep: number): void {
  const units = s.lastState?.units ?? [];
  const kept = keep > 0 ? units[keep - 1] : undefined;
  // A retraction leaves a barrier behind it, or the next drive would re-admit
  // what was just rolled back. The kept *form*, not its end position: it is
  // held from here exactly as any other barrier is, and survives editing the
  // form it names for the same reason.
  s.target = { anchor: kept ? anchorOf(s.doc, kept) : null };
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
    // prefix invariant is gone. The daemon re-reconciles on its own; what the
    // reader needs is the reason their session is about to churn, not the
    // mechanism, which nothing here asks them to act on.
    void vscode.window.showWarningMessage('ACL2: rollback did not verify; rebuilding');
  }
}

/**
 * A dependency changed under the session, and the session is about to
 * re-prove because of it.
 *
 * The one notification that explains a spontaneous multi-second re-check, and
 * nothing listened for it: a reader saw a green book go grey and start again
 * with no reason given. The daemon reports on the first sighting and acts on
 * the second, so this arrives *before* the churn.
 *
 * The channel rather than a toast. It is an explanation for something the
 * reader will notice on their own, not a decision they have to make, and
 * recertifying a library people work on can make it arrive repeatedly.
 *
 * Deduped on the names, because the daemon re-sends on every tick until the
 * dependency stops being stale -- the same comparison its own `stale_seen`
 * makes, for the same reason.
 */
function onStaleDeps(p: StaleDepsEvent): void {
  const s = byId.get(p.book);
  if (!s) {
    return;
  }
  const names = p.stale.map((d) => d.name).join(', ');
  if (names === s.staleSaid) {
    return;
  }
  s.staleSaid = names;
  if (!names) {
    return;
  }
  const missing = p.stale.filter((d) => d.missing).map((d) => d.name);
  channel.appendLine(
    `${basename(s.doc.fileName)}: ${
      missing.length === p.stale.length ? 'included book gone' : 'included book changed'
    } (${names}) -- the form that includes it will be re-checked`
  );
}

function onEvalState(p: EvalStateEvent): void {
  const s = byId.get(p.book);
  if (!s) {
    return;
  }
  // The console shows its own entry as queued the moment it is typed, so
  // only the transition to running is news -- and it is always the oldest
  // unanswered one, because the driver runs them in order.
  if (p.state === 'running' && s.evalQueue.length > 0) {
    s.console?.state(s.evalQueue[0], 'running', 0);
  }
}

// ---- The form view's console ----------------------------------------------------

/** How much of an eval's log to show inline, before the full-log button. */
const CONSOLE_LOG_LINES = 200;

/** This book's console, built on first use and wired to this book alone. */
function consoleFor(s: BookSession): Console {
  if (!s.console) {
    const c = new Console(s.id, vscode.workspace.asRelativePath(s.doc.uri));
    c.iconPath = windowIcon;
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
    return `Output unavailable: ${err}`;
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

/**
 * Cycle the check's attention points.
 *
 * The list, top to bottom: every red form in file order, then exactly ONE
 * machine target -- the running form if a check is live, otherwise the end of
 * the book. Reds first, and topmost first, because a skipped false claim can
 * poison every proof below it, so fixing downward is the wrong direction.
 *
 * Standing on an attention point advances to the next, wrapping. Standing
 * anywhere else goes to the *first*, not to the next one below the cursor:
 * "where should I look" has a top-down answer that does not depend on where
 * the cursor happens to be.
 *
 * `gotoFrontier` stays separate, for going straight to the frontier without
 * walking the reds to get there.
 *
 * Landing on the running form does not start *following* it, so that the
 * cursor tracks the frontier as it advances: that would be a mode, not a jump.
 */
function gotoAttention(s: BookSession): void {
  const editor = vscode.window.activeTextEditor;
  if (!editor) {
    return;
  }
  const units = s.lastState?.units;
  if (!units) {
    void vscode.window.showInformationMessage('ACL2: nothing to jump to yet');
    return;
  }
  const points = attentionPoints(units).map((p) => {
    const u = p.unit === null ? undefined : units.find((x) => x.index === p.unit);
    return {
      at: u
        ? { line: u.line_start, column: u.col_start }
        : { line: editor.document.lineCount, column: null },
      kind: p.kind,
    };
  });

  // By line, not by character: the cursor may sit anywhere along the form's
  // first line and still be standing on it.
  const here = cursorTarget();
  const on = unitAt(s, here);
  const standingOn = (p: { at: Pos }): boolean =>
    p.at.line === here.line &&
    // Where that line holds a second form, though, the cursor has to be on
    // *this* one; where it holds only this one, anywhere along it will do.
    (on === undefined || on.line_start !== p.at.line || on.col_start === p.at.column);
  const at = points.findIndex(standingOn);
  const next = points[at >= 0 ? (at + 1) % points.length : 0];

  const pos = positionOf(editor.document, next.at);
  editor.selection = new vscode.Selection(pos, pos);
  editor.revealRange(new vscode.Range(pos, pos), vscode.TextEditorRevealType.InCenter);
  vscode.window.setStatusBarMessage(
    {
      skipped: 'ACL2: proofs skipped here \u2014 fix it to re-check below for real',
      failed: 'ACL2: this form failed',
      running: 'ACL2: the frontier, still working',
      frontier: 'ACL2: the frontier \u2014 next to be checked',
      eof: 'ACL2: nothing red \u2014 the end of the book',
    }[next.kind] ?? '',
    4000
  );
}

function gotoFrontier(s: BookSession): void {
  const editor = vscode.window.activeTextEditor;
  const units = s.lastState?.units ?? [];
  if (!editor || units.length === 0) {
    return;
  }
  // `beyond` is deliberately not a frontier: the run stops before it and no
  // amount of waiting brings the session there. That also moves the last
  // resort, which used to be the end of the *book* -- for a targeted run the
  // place to land is the barrier, so it is the last form the run can reach.
  const reachable = units.filter((u) => u.status !== 'beyond');
  const frontier =
    units.find((u) => u.status === 'running') ??
    units.find((u) => u.status === 'pending') ??
    reachable[reachable.length - 1] ??
    units[units.length - 1];
  const pos = positionOf(editor.document, {
    line: frontier.line_start,
    column: frontier.col_start,
  });
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
  s.markReady?.();
  s.markReady = undefined;
  settleBarrier(s);
  paint(s);
  updateStatusBar(s);
  const shown = formView.shown();
  if (pinned && pinned.book === s.id) {
    // Re-resolved rather than re-shown: the split has just moved, and the
    // whole question is whether the form we are held on is still there.
    void followFormView(s);
  } else if (shown && shown.book === s.id) {
    void followFormView(s, shown.index);
  } else if (!shown && formView.visible) {
    // Open but showing nothing yet -- which is exactly where `start` leaves
    // it, since the form view opens before any state has arrived. Adopt the
    // cursor's form as soon as there is something to render, or the one
    // button would open an empty form view and leave it that way until the
    // cursor happened to move.
    //
    // Falling back to the first form matters more than it looks: at start
    // the cursor is usually on line 1, which is the `in-package` or a
    // comment above it, so the honest answer to "which form is the cursor
    // in" is "none" and the form view would open on its placeholder. Only on
    // adoption, though -- once the form view is showing something, moving the
    // cursor onto a blank line leaves it where it is rather than jumping it
    // back to the top of the book.
    const editor = vscode.window.activeTextEditor;
    const at =
      editor?.document === s.doc
        ? unitAt(s, cursorTarget())?.index
        : undefined;
    void followFormView(s, at ?? 0);
  }
}

function onUnit(p: UnitEvent): void {
  const s = byId.get(p.book);
  if (!s) {
    return;
  }
  s.unitEvents.set(p.index, p);
  if (s.runningUnit === p.index) {
    // It has landed: its log is on disk under the handle this event carries,
    // and nothing is in flight under the old one.
    s.runningUnit = undefined;
    s.runningRef = undefined;
  }
  // Re-admitted: whatever it introduced may be different now.
  s.rulesByUnit.delete(p.index);
  if (outputView.shownBook === s.id && outputView.shownUnit === p.index) {
    void showUnitOutput(s, p.index);
  }
  const shown = formView.shown();
  if (shown && shown.book === s.id && shown.index === p.index) {
    void followFormView(s, p.index);
  }
}



function onTail(p: TailEvent): void {
  const s = byId.get(p.book);
  if (!s) {
    return;
  }
  // A null unit is an eval's tail: it belongs to this book's console, not to
  // any form of the file, and so to neither the form view nor a view that
  // follows the cursor.
  // The rule lives in `policy.planTail`, where it can be tested without an
  // editor -- and where the reason it is a push rather than a poll is written
  // down next to the numbers that made the case.
  const plan = planTail({
    unit: p.unit,
    sameBook: outputView.shownBook === s.id,
    shownUnit: outputView.shownUnit,
  });
  if (plan.target === 'console') {
    s.console?.tail(p.elapsed_ms, p.text);
  } else {
    s.runningUnit = p.unit ?? undefined;
    s.runningRef = p.output_ref;
    if (plan.target === 'output-view') {
      outputView.set(s.id, p.unit as number, p.text);
    }
    formView.tail(p.book, p.unit, p.elapsed_ms, p.text);
  }
  const secs = Math.round(p.elapsed_ms / 1000);
  setStatus(
    s,
    `$(sync~spin) ACL2 ${p.unit === null ? '' : `${p.unit + 1}/${s.lastState?.units.length ?? '?'} `}(${secs}s)`
  );
}

function onSettled(p: SettledEvent): void {
  const s = byId.get(p.book);
  if (!s) {
    return;
  }
  s.runningUnit = undefined;
  s.runningRef = undefined;
  // An interrupted run is not a finished one, and the counts alone cannot
  // tell them apart -- both end with nothing running. Without this the status
  // bar answered an interrupt with a tick and a total, which reads as done.
  s.lastSettle = p.reason;
  updateStatusBar(s);
  // The outline carries each unit's status as its detail, and those are only
  // as fresh as the last time VSCode asked -- but renew only when they have
  // actually moved, or the renewal feeds itself. See `outlineKey`.
  const key = currentOutlineKey(s);
  if (key !== outlineKeys.get(s.id)) {
    outlineKeys.set(s.id, key);
    renewSymbols();
  }
  if (p.reason === 'split_error' || p.reason === 'error') {
    setStatus(s, `$(warning) ACL2 ${p.reason}`);
    if (p.message) {
      channel.appendLine(`${p.reason}: ${p.message}`);
    }
  }
  // A buffer that will not lex is the one settle with a position. Paint from
  // it to the end of the file, and take the paint off again on any settle
  // that is not a split error -- which is how it clears the moment the
  // bracket is typed, without waiting for anything to be admitted.
  const stopped = p.reason === 'split_error' ? p.line : undefined;
  s.unreadableFrom = stopped;
  s.unreadableWhy = stopped === undefined ? undefined : p.message;
  markUnreadable(s);
  updateProblems(s);
}

/**
 * Paint (or clear) the tail of a buffer the reader could not get through.
 *
 * Reads the mode rather than being told it, so that the one caller who cares
 * about a *changed* mode -- `paint` -- needs to know nothing about this.
 */
function markUnreadable(s: BookSession): void {
  for (const editor of vscode.window.visibleTextEditors) {
    if (editor.document === s.doc) {
      decorations.applyUnreadable(editor, s.unreadableFrom ?? null, mode(), s.unreadableWhy);
    }
  }
}

/**
 * Failures, as things VSCode can list.
 *
 * One collection for every book: a `DiagnosticCollection` is keyed by URI, so
 * the same one serves them all and closing one book clears only its own.
 */
let problems!: vscode.DiagnosticCollection;


/** Whether failures are published at all. */
function reportProblems(): boolean {
  return vscode.workspace.getConfiguration('acl2-session').get<boolean>('problems', true);
}

/** Whether a conditional form is worth a warning of its own. See `problemFor`. */
function warnConditional(): boolean {
  return vscode.workspace
    .getConfiguration('acl2-session')
    .get<boolean>('conditionalWarnings', false);
}

/** Whether ACL2's own warnings are reported. See the setting's description. */
function warnAcl2(): boolean {
  return vscode.workspace
    .getConfiguration('acl2-session')
    .get<boolean>('formWarnings', true);
}

/**
 * Publish this book's failures, or take them down.
 *
 * In every mode, deliberately. A diagnostic is the only thing here that
 * survives leaving the file -- it is a line in the Problems view, somewhere to
 * jump from, and a mark on the scrollbar -- and none of that competes with a
 * bar or a wash for the reader's attention the way a second mark on the text
 * would. VSCode draws a squiggle along with it whether we want one or not:
 * `Diagnostic` has no rendering control, `DiagnosticTag` offers only fading
 * and strikethrough, and `editor.renderValidationDecorations` -- which would
 * do exactly this -- is an internal editor option that no extension can set.
 * So the squiggle is the price of the Problems entry, and
 * `acl2-session.problems` is how someone declines to pay it.
 *
 * Turning it off in `minimal` leaves a failed form marked by nothing at all,
 * scrollbar included. That is the honest consequence of a mode whose only
 * mark for a failure *is* this, and the setting's description says so.
 */
function updateProblems(s: BookSession): void {
  if (!reportProblems()) {
    problems.delete(s.doc.uri);
    return;
  }
  const doc = s.doc;
  const out: vscode.Diagnostic[] = [];
  // A bracket that never closes, first: it is the reason nothing below it has
  // a status to report, so it belongs above whatever statuses survive. It is
  // reported in every mode, exactly as a failed form is -- and in `minimal`
  // it is the only mark there is.
  if (s.unreadableFrom !== undefined && s.unreadableFrom <= doc.lineCount) {
    const line = Math.max(0, s.unreadableFrom - 1);
    const text = doc.lineAt(line).text;
    const d = new vscode.Diagnostic(
      new vscode.Range(line, 0, line, Math.max(1, text.length)),
      s.unreadableWhy ?? 'Cannot be read from here',
      vscode.DiagnosticSeverity.Error
    );
    d.source = 'ACL2';
    out.push(d);
  }
  const withConditional = warnConditional();
  const withWarnings = warnAcl2();
  for (const u of s.lastState?.units ?? []) {
    const kind = problemFor(u.status, withConditional);
    // The range is wanted whether or not the status is a problem: a form that
    // went in cleanly is exactly the one that carries ACL2's warnings.
    const kinds = withWarnings ? warningKinds(s.unitEvents.get(u.index)?.warnings) : [];
    if ((kind === undefined && kinds.length === 0) || u.line_start > doc.lineCount) {
      continue;
    }
    const line = u.line_start - 1;
    const text = doc.lineAt(line).text;
    const from = charOffset(text, u.col_start);
    // To where the form ends when it ends on this line, and to the end of the
    // line when it runs on. Not simply to the end of the line either way: a
    // line can hold two forms, and marking the whole of it would blame the
    // one that is fine for the one that is not.
    const to =
      u.line_end === u.line_start ? charOffset(text, u.col_end) : text.length;
    const span = new vscode.Range(line, from, line, Math.max(from + 1, to));
    // ACL2's own warnings, one entry per kind. The kind is all we have -- what
    // the warning actually *says* lives only in the printed output -- so each
    // becomes a fixed phrase (see `warningMessage`), and the form's output is
    // where the full text is read.
    for (const k of kinds) {
      const w = new vscode.Diagnostic(
        span,
        warningMessage(k),
        vscode.DiagnosticSeverity.Warning
      );
      w.source = 'ACL2';
      out.push(w);
    }
    if (kind === undefined) {
      continue;
    }
    const d = new vscode.Diagnostic(
      span,
      diagnosticMessage(u.status),
      kind === 'warning' ? vscode.DiagnosticSeverity.Warning : vscode.DiagnosticSeverity.Error
    );
    d.source = 'ACL2';
    out.push(d);
  }
  problems.set(doc.uri, out);
}

function paint(s: BookSession): void {
  if (!s.lastState) {
    return;
  }
  // Not inside the editor loop below: a diagnostic belongs to the document,
  // and is worth publishing whether or not the file is on screen.
  updateProblems(s);
  const opts = { mode: mode() };
  const target = targetMark(s);
  for (const editor of vscode.window.visibleTextEditors) {
    if (editor.document === s.doc) {
      decorations.apply(editor, s.lastState.units, opts);
      decorations.applyTarget(editor, target);
      decorations.applyUnreadable(
        editor,
        s.unreadableFrom ?? null,
        opts.mode,
        s.unreadableWhy
      );
    }
  }
}

/**
 * Which book the status bar is currently describing.
 *
 * The bar is one widget and books are many, so it has to belong to one of
 * them; without this it belonged to whichever book's event arrived last.
 * `onTail`, `onSettled` and `onState` all repaint, so with two live books it
 * flip-flopped between them several times a second -- and switching tabs
 * repainted nothing at all, so it went on describing the file you had left
 * until the next event from it.
 *
 * The book the bar shows is the active editor's, and it is also what the
 * bar's *command* acts on: during a certification the bar becomes an
 * interrupt button, and `acl2-session.interrupt` resolves against the active
 * editor -- so a bar describing some other book was a button that interrupted
 * the wrong session, or none.
 */
let statusBook: string | undefined;

/**
 * The book of the editor in front, if it has one.
 *
 * A webview or an output panel taking focus leaves `activeTextEditor`
 * undefined, which must not be read as "no book": the bar would blank every
 * time you clicked the form view it links to.
 */
function activeBook(): BookSession | undefined {
  const editor = vscode.window.activeTextEditor;
  return editor ? bookFor(editor.document) : undefined;
}

/** Repaint the bar for whichever book is in front now. */
function refreshStatusBar(): void {
  const s = activeBook();
  if (s) {
    updateStatusBar(s);
  }
}

/**
 * Whether the bar is this book's to write to.
 *
 * The book in front owns it, and nothing outranks that -- a certification
 * included. It used to: its clock wrote the widget directly, so it was not so
 * much precedence as a race, and with two books live and one certifying the
 * bar flickered between the other's counts and the clock twice a second. A
 * reader looking at one file has no use for another file's activity, and the
 * one thing they might want -- the interrupt button -- is no use either if it
 * is only there half the time.
 *
 * With nothing in front that has a session -- a webview focused, a plain text
 * file open -- whichever book the bar already describes keeps it, rather than
 * the bar blanking or being taken by the next event to arrive from anywhere.
 */
function ownsStatus(s: BookSession): boolean {
  const active = activeBook();
  if (active) {
    return active.id === s.id;
  }
  return statusBook === undefined || statusBook === s.id;
}

/**
 * Write the bar on one book's behalf.
 *
 * Every write goes through here, because every write used to go straight to
 * the widget: `onTail`, `onSettled` and `onState` each set it for whichever
 * book's event had arrived, so two live books took turns several times a
 * second.
 *
 * A running certification is the one thing that outranks the book in front:
 * the bar is its clock and its interrupt button until it finishes.
 */
function setStatus(
  s: BookSession,
  text: string,
  tooltip?: string,
  command?: string | vscode.Command
): void {
  if (!ownsStatus(s)) {
    return;
  }
  statusBook = s.id;
  status.text = text;
  // The command belongs to whatever the bar is *showing*, so it travels with
  // the text. Left alone when absent, like the tooltip: the transient writes
  // change what the bar says without changing what clicking it does.
  if (command !== undefined) {
    status.command = command;
  }
  // Optional, and omitted by the transient writes -- `starting`, a tail's
  // clock, a settle's warning -- which have only ever set the text and leave
  // the standing tooltip to say the rest.
  if (tooltip !== undefined) {
    status.tooltip = tooltip;
  }
}

function updateStatusBar(s: BookSession): void {
  // Certification no longer appears here -- it has its own progress
  // notification (`certifyWithProgress`). The status bar is the book's proof
  // status and nothing else, which is why a running build leaves it alone.
  const units = s.lastState?.units ?? [];
  const total = units.length;
  const green = units.filter((u) => u.status === 'admitted' || u.status === 'conditional').length;
  const reds = units.filter((u) => u.status === 'skipped' || u.status === 'failed').length;
  const running = units.find((u) => u.status === 'running');
  // An icon and a count, and nothing the icon already says. The status bar is
  // shared with every other extension and its guidance asks for short labels;
  // ours were the longest thing in it, spending words on what the tick, the
  // spinner and the pause bars were each conveying anyway.
  //
  // What the icon cannot carry goes to the tooltip, which is where a reader
  // who wants the detail looks and where nobody else has to read it. Nothing
  // is lost, only moved off a shared strip.
  const icon = s.paused
    ? '$(debug-pause)'
    : running
      ? '$(sync~spin)'
      : s.lastSettle === 'interrupted'
        ? '$(circle-slash)'
        : reds > 0
          ? '$(error)'
          : '$(check)';
  const at = running ? running.index + 1 : green;
  const why = s.paused
    ? 'paused'
    : running
      ? 'proving'
      : s.lastSettle === 'interrupted'
        ? 'interrupted'
        : reds > 0
          ? `${reds} red`
          // Nothing to add: the count already says every form went in, and
          // "40/40 admitted -- all admitted" says it twice.
          : '';
  // The file is named because the bar is one widget over many books: with two
  // sessions live, `12/40` alone does not say whose. It goes in the tooltip
  // rather than the strip, where the detail belongs.
  setStatus(
    s,
    `${icon} ACL2 ${at}/${total}`,
    `ACL2: ${basename(s.doc.fileName)} \u2014 ${green}/${total} admitted` +
      (why ? ` \u2014 ${why}` : '') +
      `\nClick to show the form view`,
    // Named on every ordinary repaint, so a finished certification cannot
    // leave its interrupt behind: whichever book takes the bar back takes the
    // command with it.
    'acl2-session.showFormView'
  );
}

// ---- Output view and form view --------------------------------------------------

/**
 * The unit a position falls in.
 *
 * The **last** one it touches, so that a cursor resting between two forms on
 * one line belongs to the one it is inside; both ends are inclusive, since a
 * caret sits between characters and the one just past a closing paren
 * belongs to the form it closes. Matches the daemon's `Form::covers`.
 */
function unitAt(s: BookSession, at: Pos): UnitState | undefined {
  const units = s.lastState?.units;
  if (!units) {
    return undefined;
  }
  const covers = (u: UnitState): boolean => {
    if (at.column === null) {
      return u.line_start <= at.line && at.line <= u.line_end;
    }
    const after = u.line_start < at.line || (u.line_start === at.line && u.col_start <= at.column);
    const before = at.line < u.line_end || (at.line === u.line_end && at.column <= u.col_end);
    return after && before;
  };
  for (let i = units.length - 1; i >= 0; i--) {
    if (covers(units[i])) {
      return units[i];
    }
  }
  return undefined;
}

async function followOutputView(s: BookSession, force: boolean): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  if (!editor || editor.document !== s.doc) {
    return;
  }
  const unit = unitAt(s, cursorTarget());
  if (!unit) {
    return;
  }
  if (!force && outputView.shownBook === s.id && outputView.shownUnit === unit.index) {
    return;
  }
  await showUnitOutput(s, unit.index);
}

async function showUnitOutput(s: BookSession, index: number): Promise<void> {
  // A finished form has a unit event carrying its handle; one still running
  // has no event yet, but its handle arrives on the tail and is readable as
  // the proof goes. Prefer the running one -- if this form is the one in
  // flight, the live log is what the reader wants to see.
  const ref = (s.runningUnit === index ? s.runningRef : undefined) ?? s.unitEvents.get(index)?.output_ref;
  if (!ref) {
    outputView.set(s.id, index, s.runningUnit === index ? 'Running...' : 'Not checked yet.');
    return;
  }
  try {
    const r = await client?.request('output', {
      book: s.id,
      output_ref: ref,
      mode: 'tail',
      lines: 400,
    });
    outputView.set(s.id, index, r.text as string);
  } catch (err) {
    outputView.set(s.id, index, `Output unavailable: ${err}`);
  }
}

/**
 * Tell the daemon how wide to lay terms out, and get what is on screen again.
 *
 * Both halves are needed. The first is the point; the second is because every
 * term already delivered was laid out for the old width, and the caches are
 * full of them. Dropping the caches and re-following costs two requests for
 * the form on show, which is what a pane drag is worth.
 *
 * What this cannot repair is a *checkpoint*: those ride on `unit` events,
 * which are pushed when a form lands and never re-sent, so a failed form
 * keeps the wrapping it arrived with until it runs again. Worth a re-request
 * verb one day; not worth one for a drag.
 */
function setLayout(cols: number): void {
  if (cols === lastCols) {
    return;
  }
  lastCols = cols;
  client?.notify('layout', { cols });
  for (const s of books.values()) {
    s.rulesByUnit.clear();
  }
  const shown = formView.shown();
  const s = shown && byId.get(shown.book);
  if (s && shown) {
    void reflowCheckpoints(s, shown.index).then(() => followFormView(s, shown.index));
  }
}

/**
 * Re-flow the checkpoints of the form on show.
 *
 * Everything else the form view renders can be asked for again, so a width change
 * just drops the caches. Checkpoints cannot: they ride on `unit` events, which
 * are pushed when a form lands and never sent a second time. So the client
 * sends back the text it already has and the daemon lays it out again -- which
 * works because laying out is idempotent across widths, the reader having
 * discarded the whitespace before it ever sees the layout.
 *
 * Best effort and in place: on any trouble the form view keeps the wrapping it
 * had, which is the same thing it would have shown anyway.
 */
async function reflowCheckpoints(s: BookSession, index: number): Promise<void> {
  const e = s.unitEvents.get(index);
  const terms = (e?.checkpoints ?? []).map((c) => c.term);
  if (!e || !terms.length) {
    return;
  }
  try {
    const r = await client?.request('reflow', { terms });
    const laid = (r?.terms as string[] | undefined) ?? [];
    // Positional, so a reply that lost or gained one is not usable.
    if (laid.length !== terms.length) {
      return;
    }
    e.checkpoints = (e.checkpoints ?? []).map((c, i) => ({ ...c, term: laid[i] }));
  } catch (err) {
    channel.appendLine(`reflow: form ${index} kept its old wrapping (${err})`);
  }
}

/** The width last sent, so a re-measure that says the same thing costs nothing. */
let lastCols = 0;

/**
 * The form the form view is held on, if it is held on one.
 *
 * The anchor is where that form *starts*, which is the only position that can
 * be checked: `pinnedIndex` asks the daemon's own split whether a form still
 * begins exactly there, and lets go when it does not. Holding an index instead
 * would survive nothing -- indices renumber whenever a form is added above --
 * and holding a line alone cannot tell two forms on one line apart.
 */
let pinned: { book: string; anchor: Anchor } | undefined;

/** Which form the form view should be showing, and whether it is held there. */
function formViewTarget(s: BookSession): number | undefined {
  if (pinned && pinned.book === s.id) {
    const at = pinnedIndex(pinned.anchor, s.lastState?.units ?? []);
    if (at !== undefined) {
      return at;
    }
    // No form starts where we are pointing any more, so whatever we would
    // show is not what was held.
    releasePin('the form it was held on is no longer there');
  }
  // The cursor only speaks for this book when it is in this book.
  const editor = vscode.window.activeTextEditor;
  return editor?.document === s.doc ? unitAt(s, cursorTarget())?.index : undefined;
}

/**
 * Hold the form view on the form it is showing.
 *
 * On what the form view *shows* rather than on where the cursor is, and the reason
 * is where this gets invoked from: its own title bar. A webview having focus
 * means there is no active text editor at all, so anything reading the cursor
 * finds nothing and reports that there is no session -- which was the first
 * thing this did. Reading the form view instead is both correct there and a better
 * description of the act: the button says keep showing this.
 *
 * It costs nothing anywhere else, since an unheld form view is already showing
 * whatever the cursor is in.
 *
 * Anchored to the form's own start rather than to a position inside it: only a
 * start can be checked against the daemon's split, which is what every later
 * decision rests on.
 */
/**
 * The form a pin would go on, from whichever view is showing one.
 *
 * The form view first, then the output view, then the cursor. Three sources
 * because the pin governs two views and can be taken from either: pinning
 * from the log with the form view closed has to work, and the button now sits
 * on both. The cursor is the last resort rather than the first because a view
 * that is *showing* a form is a more deliberate statement about which form is
 * meant than wherever the caret happens to rest.
 */
function pinTarget(): { s: BookSession; u: UnitState } | undefined {
  const shown = formView.shown();
  const fromView =
    (shown && ([byId.get(shown.book), shown.index] as const)) ??
    (outputView.shownBook !== null && outputView.shownUnit !== null
      ? ([byId.get(outputView.shownBook), outputView.shownUnit] as const)
      : undefined);
  if (fromView?.[0]) {
    const u = fromView[0].lastState?.units[fromView[1]];
    if (u) {
      return { s: fromView[0], u };
    }
  }
  const editor = vscode.window.activeTextEditor;
  const s = editor && bookFor(editor.document);
  const u = s && unitAt(s, cursorTarget());
  return s && u ? { s, u } : undefined;
}

/** Move the pin to the cursor's form, for a command that names one. */
function repin(s: BookSession): void {
  const u = unitAt(s, cursorTarget());
  if (u && u.line_start <= s.doc.lineCount) {
    setPin(s, u);
  }
}

function takePin(): void {
  const found = pinTarget();
  const s = found?.s;
  const u = found?.u;
  // The document, not an editor: there may not be one in front, and a
  // `TextDocument` can be read either way.
  if (!s || !u || u.line_start > s.doc.lineCount) {
    void vscode.window.showInformationMessage(
      'ACL2: there is no form to pin to \u2014 put the cursor on one, or open a form'
    );
    return;
  }
  setPin(s, u);
}

/** Pin both views to this form. */
function setPin(s: BookSession, u: UnitState): void {
  pinned = {
    book: s.id,
    anchor: {
      line: u.line_start,
      column: u.col_start,
      character: charOffset(s.doc.lineAt(u.line_start - 1).text, u.col_start),
    },
  };
  void vscode.commands.executeCommand('setContext', 'acl2Session.formViewPinned', true);
  formView.setPinLabel('pinned');
  formView.note(undefined);
  // Both views, together. Pinning from the log has to bring the form view
  // along and the other way round, or the pin would mean "hold what I am
  // looking at and leave the other one wherever it was".
  void followFormView(s);
  if (outputView.visible) {
    void showUnitOutput(s, u.index);
  }
}

/**
 * Let go because the reader said so, and show them what the cursor is in.
 *
 * Separate from `releasePin`, which is also what a lost form calls: this is
 * the deliberate one, so it says nothing and hands the form view back to the
 * cursor rather than leaving it on a form nobody is pointing at.
 */
function dropPin(): void {
  const was = pinned?.book;
  releasePin();
  const s = was === undefined ? undefined : byId.get(was);
  if (s) {
    void followFormView(s);
    if (outputView.visible) {
      void followOutputView(s, false);
    }
  }
}

/**
 * Let go, and say so.
 *
 * Never silently: a pin that stopped working without a word is
 * indistinguishable from one that is still holding, which is the failure this
 * whole design is arranged to avoid. `why` is absent when the reader asked.
 */
function releasePin(why?: string): void {
  if (!pinned) {
    return;
  }
  pinned = undefined;
  void vscode.commands.executeCommand('setContext', 'acl2Session.formViewPinned', false);
  formView.setPinLabel(undefined);
  if (why) {
    formView.note(`The formView is no longer held: ${why}.`);
    channel.appendLine(`formView: released -- ${why}`);
  }
}

/** Update the form view for the form at the cursor (or a pinned INDEX). */
async function followFormView(s: BookSession, index?: number): Promise<void> {
  // `formViewTarget` decides between the held form and the cursor's, and is the
  // one that knows whether the cursor speaks for this book at all.
  const at = index ?? formViewTarget(s);
  if (at === undefined) {
    return;
  }
  const model = buildFormModel(s, at);
  if (model) {
    formView.render(model);
    void followRules(s, at);
  }
}

/**
 * Fill in what the form introduced.
 *
 * One request per name the form's events recorded, answered from the scrape
 * the daemon took as the form was admitted -- so this needs no prover and
 * returns during a proof, which is when a reader is most likely to be looking
 * at an earlier form.
 *
 * Names come from the unit's inner events, which is what "inside this form"
 * means for anything macro-generated: a `define` records the function and
 * every theorem it wrote, none of which appear in the buffer.
 */
async function followRules(s: BookSession, index: number): Promise<void> {
  const cached = s.rulesByUnit.get(index);
  if (cached) {
    // Already known: hand it over and let the form view decide whether anything
    // changed. Asking again would cost a round trip per name and, worse,
    // land a re-render on top of whatever the reader was doing.
    formView.rules(s.id, index, cached);
    return;
  }
  const e = s.unitEvents.get(index);
  // The names ACL2 recorded, and the names the form *writes*. Two questions,
  // and documentation is the case that tells them apart: a standalone
  // `defxdoc` records no event at all, so asking only about `inner` would
  // never ask about the one name a form whose purpose is documentation has.
  const all = [...(e?.inner ?? []).map((i) => i.name), ...(e?.names ?? [])]
    .filter((n) => n)
    .filter((n, i, xs) => xs.indexOf(n) === i);
  if (!all.length) {
    return;
  }
  // Every name, now that laying one out is proportional to its size: the
  // cap that used to sit here was aimed at the wrong dimension. It bounded
  // how *many* names were formatted, while a single `defines` statement was
  // large enough on its own to lay out to megabytes.
  const names = all;
  try {
    // One request for the whole list. Asking per name meant `Promise.all`
    // over everything a form recorded, which for a `defines` of 620 events
    // was 620 requests fired at once the instant it landed -- and 620 replies
    // back onto a socket the renderer was too busy to drain.
    const batch = await client?.request('rules', { book: s.id, names });
    const replies: RulesReply[] = (batch?.rules as RulesReply[]) ?? [];
    // Kept when it has rules *or* documentation. Filtering on `found` alone
    // dropped every name that is documented and introduces nothing, which is
    // the whole of the standalone case.
    const entries: FormNameRules[] = replies
      .filter((r): r is RulesReply => !!r && (r.found || !!r.short))
      .map((r) => ({
        name: r.name,
        found: r.found,
        short: r.short,
        symbolClass: r.symbol_class,
        measure: r.measure,
        relation: r.relation,
        formals: r.formals,
        guard: r.guard,
        statement: r.statement,
        runes: (r.runes ?? []).map((u: RuneWire) => ({
          rune: u.rune,
          class: u.class,
          enabled: u.enabled,
          fields: u.fields ?? [],
        })),
      }));
    s.rulesByUnit.set(index, entries);
    // Keyed by book and unit inside the form view: the cursor may have moved on.
    formView.rules(s.id, index, entries);
  } catch (err) {
    // Best effort, but said out loud -- a form view that silently shows less is
    // indistinguishable from a form that introduced less.
    channel.appendLine(`rules: form ${index} could not be read (${err})`);
  }
}

function buildFormModel(s: BookSession, index: number): FormModel | undefined {
  const st = s.lastState;
  const u = st?.units[index];
  if (!st || !u) {
    return undefined;
  }
  const model: FormModel = {
    book: s.id,
    index,
    total: st.units.length,
    lineStart: u.line_start,
    lineEnd: u.line_end,
    colStart: u.col_start,
    colEnd: u.col_end,
    status: u.status,
    checkpoints: [],
    rules: [],
  };
  // An unchecked or running unit's cached envelope describes an earlier life
  // (before a rollback re-queued it), so it is not shown.
  if (u.status === 'pending' || u.status === 'beyond' || u.status === 'running') {
    return model;
  }
  const e = s.unitEvents.get(index);
  if (e) {
    Object.assign(model, toPanelEnvelope(e), { elapsedMs: e.elapsed_ms });
  }
  return model;
}

/**
 * The wire's envelope in the form view's spelling. One function, because the
 * daemon flattens a unit's envelope and an eval's identically -- and if that
 * ever stops being true, this is where it breaks loudly rather than in one
 * of two renderers that drifted apart.
 */
function toPanelEnvelope(e: Envelope): FormEnvelope {
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
    value: e.value,
    valueTriple: e.value_triple,
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

