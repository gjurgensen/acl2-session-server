// ACL2: entry point and UI wiring.
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
import { Envelope, InductionWire, RulesReply, RuneWire } from './protocol';
import { DecorationMode, Decorations } from './decorations';
import { EVAL_SCHEME, OutputView, SCHEME } from './outputView';
import { Highlighter } from './highlight';
import { Panel, PanelEnvelope, PanelInduction, PanelModel, PanelNameRules } from './panel';
import { Console, ConsoleResult, HISTORY } from './console';
import { DocView, plainSpans } from './docView';
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
  xdocText,
} from './names';

const UPDATE_DEBOUNCE_MS = 300;

interface BookSession {
  id: string;
  doc: vscode.TextDocument;
  /** Held target line (1-based); null checks the whole book; 0 holds all. */
  target: number | null;
  lastState?: StateEvent;
  /** Latest envelope per unit index, for diagnostics, panel and output view. */
  unitEvents: Map<number, UnitEvent>;
  /**
   * What each unit introduced, once asked for.
   *
   * Cached because the panel follows the *cursor*: `followPanel` runs on
   * every selection change, and fetching on each one put a reply -- and so a
   * re-render -- a moment after every keystroke and every click. A re-render
   * replaces the page, which resets every open dropdown, so the panel looked
   * frozen. Dropped for a unit when that unit is admitted again, which is
   * the only way what it introduced can change.
   */
  rulesByUnit: Map<number, PanelNameRules[]>;
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
  /** A certification is in flight; a second is refused rather than queued. */
  certifying?: boolean;
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
let panel: Panel;
let highlighter: Highlighter;
let status: vscode.StatusBarItem;
let channel: vscode.OutputChannel;
let windowIcon: { light: vscode.Uri; dark: vscode.Uri };

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
    // Prose, not a parenthesized aside: these two views are highlighted with
    // the output grammar, and `(nothing evaluated yet)` is shaped exactly
    // like an ACL2 term -- opens at column zero, closes at end of line -- so
    // it came out syntax-highlighted, `not` and all. Keep status messages out
    // of that shape.
    'No form under the cursor has been checked yet.'
  );
  evalView = new OutputView(EVAL_SCHEME, 'ACL2 Eval', 'Nothing evaluated yet.');
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
  // Webview tabs take an icon; the output views cannot, since they are text
  // documents and VSCode takes an editor tab's icon from the file icon theme.
  windowIcon = {
    light: vscode.Uri.joinPath(context.extensionUri, 'media', 'window-light.svg'),
    dark: vscode.Uri.joinPath(context.extensionUri, 'media', 'window-dark.svg'),
  };
  panel = new Panel();
  panel.iconPath = windowIcon;
  status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 10);
  // The progress readout is also the way back to the panel, for when it has
  // been closed and the gutter says something needs looking at.
  status.command = 'acl2-session.showPanel';
  status.tooltip = 'ACL2: show the panel';
  channel = vscode.window.createOutputChannel('ACL2');

  panel.onGoal = (m, goal, slot) => void fetchGoal(m, goal, slot);

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
      paintDocs();
    });
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
    panel,
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
    // `:` triggers so keyword families appear without a word first; the rest
    // is VSCode's own 24x7 word completion.
    vscode.languages.registerCompletionItemProvider({ language: 'acl2' }, completionProvider, ':'),
    // Registered through a holder: see `renewSymbols` on why it has to be
    // replaceable rather than registered once.
    new vscode.Disposable(() => symbolReg?.dispose()),
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
    vscode.commands.registerCommand('acl2-session.showDocs', () => forBook(showDocs)),
    vscode.commands.registerCommand('acl2-session.certify', () => forBook(certifyBook)),
    vscode.tasks.registerTaskProvider('acl2', certifyTaskProvider),
    vscode.commands.registerCommand('acl2-session.showPanel', () =>
      forBook((s) => {
        panel.open();
        void followPanel(s);
      })
    ),
    vscode.commands.registerCommand('acl2-session.gotoFrontier', () => forBook(gotoFrontier)),
    vscode.commands.registerCommand('acl2-session.gotoAttention', () => forBook(gotoAttention)),
    vscode.workspace.onDidChangeTextDocument((e) => {
      const s = bookFor(e.document);
      if (!s || e.contentChanges.length === 0) {
        return;
      }
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
      if (outputView.visible) {
        void followOutputView(s, false);
      }
      if (panel.visible) {
        void followPanel(s);
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
    vscode.window.onDidChangeActiveTextEditor(refreshContext),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration('acl2-session')) {
        for (const s of books.values()) {
          paint(s);
        }
        // termWidth changes how terms are laid out, and the panel holds the
        // model it would need to re-lay out, so a refresh is enough.
        panel.refresh();
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
  void vscode.window.showErrorMessage(`acl2-session: ${msg}`);
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
      reply = await client.request('describe', { book: s.id, name });
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
      const line = Math.max(0, (r.line ?? 1) - 1);
      return new vscode.Location(vscode.Uri.file(r.path), new vscode.Position(line, 0));
    }
    const line = r.line ?? (r.unit !== undefined ? unitLine(s, r.unit) : undefined);
    if (line === undefined) {
      return undefined;
    }
    return new vscode.Location(doc.uri, new vscode.Position(Math.max(0, line - 1), 0));
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
      const span = linkRangeOnLine(text, l.book);
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
 * What the outline last showed, so it is only renewed when it would differ.
 *
 * Without this the renewal feeds itself: `drive` settles on every command
 * pass, so a symbols query produces a `settled`, which renews, which makes
 * VSCode query again -- a loop that flickers the Outline view continuously
 * and is started by *any* request, not just this one.
 */
let outlineKey = '';

function currentOutlineKey(s: BookSession): string {
  const units = s.lastState?.units ?? [];
  return `${s.id}|${units.length}|${units.map((u) => u.status).join(',')}`;
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
      const last = Math.min(Math.max(b.line_end - 1, 0), doc.lineCount - 1);
      const range = new vscode.Range(
        Math.max(b.line_start - 1, 0),
        0,
        last,
        doc.lineAt(last).text.length
      );
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
    const span = linkRangeOnLine(doc.lineAt(position.line).text, l.book);
    // Anywhere in the form counts, not just inside the quotes: `gd` with the
    // cursor on `include-book` itself should go there too.
    if (span && position.character <= span[1] + 1) {
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
 * checking carries on and queries keep answering while it runs.
 */
async function certifyBook(s: BookSession): Promise<void> {
  if (s.certifying) {
    void vscode.window.showInformationMessage('acl2-session: already certifying this book');
    return;
  }
  if (s.doc.isDirty && !(await s.doc.save())) {
    void vscode.window.showWarningMessage('acl2-session: could not save before certifying');
    return;
  }
  s.certifying = true;
  const jobs = vscode.workspace
    .getConfiguration('acl2-session')
    .get<number>('certifyJobs', 4);
  const started = Date.now();
  const tick = setInterval(() => {
    const secs = Math.floor((Date.now() - started) / 1000);
    status.text = `$(sync~spin) ACL2: certifying ${Math.floor(secs / 60)}:${String(secs % 60).padStart(2, '0')}`;
    status.tooltip = 'ACL2: click to interrupt the certification';
    status.command = 'acl2-session.interrupt';
  }, 500);
  try {
    const r: CertifyReply = await client!.request('certify', { book: s.id, jobs });
    if (r.status === 'certified') {
      void vscode.window.showInformationMessage(
        `ACL2: certified in ${(r.elapsed_ms / 1000).toFixed(1)}s`
      );
    } else {
      // The blamed book is usually a dependency rather than this one, and
      // saying which is the difference between a report and "it broke".
      const blamed = (r.failed_books ?? [])[0];
      const where = blamed && !blamed.endsWith(basename(s.doc.fileName)) ? ` in ${basename(blamed)}` : '';
      const pick = await vscode.window.showErrorMessage(
        `ACL2: certification failed${where}${r.error_line ? `: ${r.error_line}` : ''}`,
        'Show Log'
      );
      if (pick === 'Show Log' && r.output_ref) {
        await showCertLog(s, r.output_ref, r.output_lines);
      }
    }
  } catch (e) {
    report(e);
  } finally {
    clearInterval(tick);
    s.certifying = false;
    status.command = 'acl2-session.showPanel';
    status.tooltip = 'ACL2: show the panel';
    updateStatusBar(s);
  }
}

function basename(p: string): string {
  return p.split(/[\\/]/).pop() ?? p;
}

async function showCertLog(s: BookSession, ref: string, lines: number): Promise<void> {
  try {
    const out = await client!.request('output', {
      book: s.id,
      output_ref: ref,
      mode: 'tail',
      lines: Math.min(lines, 4000),
    });
    outputView.set(s.id, null, out.text as string);
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
    void vscode.window.showInformationMessage('acl2-session: put the cursor on a name first');
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
      docs.show({ name, html: `<p>${safe}.</p>`, code: [], canGoBack: docHistory.length > 0 });
    } else {
      void vscode.window.showInformationMessage(`acl2-session: ${why}`);
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
 * that has not changed. The same reason the panel re-renders on reload.
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

/** The 1-based start line of a unit, from the last state event. */
function unitLine(s: BookSession, index: number): number | undefined {
  return s.lastState?.units.find((u) => u.index === index)?.line_start;
}

function forBook(f: (s: BookSession) => unknown): void {
  const editor = vscode.window.activeTextEditor;
  const s = editor && bookFor(editor.document);
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
    if (orphaned.length > 0) {
      void vscode.window.showErrorMessage(
        'acl2-session: the daemon exited and its sessions are gone (see the "ACL2" ' +
          'output channel). Run "ACL2: Start Live Checking" to begin again.'
      );
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
  // Through `bookFor`, because running Start on a book whose tab was closed
  // and reopened is exactly how a session ends up holding a document VSCode
  // has frozen -- and this path would otherwise answer by pushing its stale
  // text at the daemon.
  const existing = bookFor(doc);
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
      rulesByUnit: new Map(),
      evalQueue: [],
      evalReplies: new Map(),
    };
    books.set(key, s);
    byId.set(s.id, s);
    // The outline could not be answered before this session existed, and
    // VSCode will not ask again on its own.
    outlineKey = '';
    renewSymbols();
    status.show();
    status.text = '$(sync~spin) ACL2: starting';
    channel.appendLine(`opened ${doc.fileName} as ${s.id} under ${r.image}`);
    sendUpdate(s);
    refreshContext();
    // Starting and opening the panel are one action, because there is no
    // moment where you want the session but not the place its failures
    // explain themselves. It opens with preserveFocus, so it costs the
    // cursor nothing.
    panel.open();
    void followPanel(s);
  } catch (e) {
    report(e);
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
  const live = !!editor && books.has(editor.document.uri.toString());
  void vscode.commands.executeCommand('setContext', 'acl2Session.active', live);
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
  books.delete(s.doc.uri.toString());
  byId.delete(s.id);
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
  }
  refreshContext();
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
    void vscode.window.showInformationMessage('acl2-session: nothing to jump to yet');
    return;
  }
  const points: { line: number; kind: string }[] = units
    .filter((u) => u.status === 'skipped' || u.status === 'failed')
    .map((u) => ({ line: u.line_start, kind: u.status }));
  const running = units.find((u) => u.status === 'running');
  points.push(
    running
      ? { line: running.line_start, kind: 'running' }
      : { line: editor.document.lineCount, kind: 'eof' }
  );

  // By line, not by character: the cursor may sit anywhere along the form's
  // first line and still be standing on it.
  const line = editor.selection.active.line + 1;
  const at = points.findIndex((p) => p.line === line);
  const next = points[at >= 0 ? (at + 1) % points.length : 0];

  const pos = new vscode.Position(next.line - 1, 0);
  editor.selection = new vscode.Selection(pos, pos);
  editor.revealRange(new vscode.Range(pos, pos), vscode.TextEditorRevealType.InCenter);
  vscode.window.setStatusBarMessage(
    {
      skipped: 'ACL2: proofs skipped here \u2014 fix it to re-check below for real',
      failed: 'ACL2: this form failed',
      running: 'ACL2: the frontier, still working',
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
  } else if (!shown && panel.visible) {
    // Open but showing nothing yet -- which is exactly where `start` leaves
    // it, since the panel opens before any state has arrived. Adopt the
    // cursor's form as soon as there is something to render, or the one
    // button would open an empty panel and leave it that way until the
    // cursor happened to move.
    //
    // Falling back to the first form matters more than it looks: at start
    // the cursor is usually on line 1, which is the `in-package` or a
    // comment above it, so the honest answer to "which form is the cursor
    // in" is "none" and the panel would open on its placeholder. Only on
    // adoption, though -- once the panel is showing something, moving the
    // cursor onto a blank line leaves it where it is rather than jumping it
    // back to the top of the book.
    const editor = vscode.window.activeTextEditor;
    const at =
      editor?.document === s.doc
        ? unitAt(s, editor.selection.active.line + 1)?.index
        : undefined;
    void followPanel(s, at ?? 0);
  }
}

function onUnit(p: UnitEvent): void {
  const s = byId.get(p.book);
  if (!s) {
    return;
  }
  s.unitEvents.set(p.index, p);
  // Re-admitted: whatever it introduced may be different now.
  s.rulesByUnit.delete(p.index);
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
  // The outline carries each unit's status as its detail, and those are only
  // as fresh as the last time VSCode asked -- but renew only when they have
  // actually moved, or the renewal feeds itself. See `outlineKey`.
  const key = currentOutlineKey(s);
  if (key !== outlineKey) {
    outlineKey = key;
    renewSymbols();
  }
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
    outputView.set(s.id, index, 'Not checked yet.');
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
    outputView.set(s.id, index, `Output unavailable: ${err}`);
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
    void followInduction(s, at);
    void followRules(s, at);
  }
}

/**
 * Fill in what the form introduced.
 *
 * One request per name the form's events recorded, answered from the scrape
 * the daemon took as the form was admitted -- so unlike the log-reading
 * `induction` this needs no prover and returns during a proof, which is when
 * a reader is most likely to be looking at an earlier form.
 *
 * Names come from the unit's inner events, which is what "inside this form"
 * means for anything macro-generated: a `define` records the function and
 * every theorem it wrote, none of which appear in the buffer.
 */
async function followRules(s: BookSession, index: number): Promise<void> {
  const cached = s.rulesByUnit.get(index);
  if (cached) {
    // Already known: hand it over and let the panel decide whether anything
    // changed. Asking again would cost a round trip per name and, worse,
    // land a re-render on top of whatever the reader was doing.
    panel.rules(s.id, index, cached);
    return;
  }
  const e = s.unitEvents.get(index);
  const names = (e?.inner ?? []).map((i) => i.name).filter((n) => n);
  if (!names.length) {
    return;
  }
  try {
    const replies = await Promise.all(
      names.map((n) => client?.request('rules', { book: s.id, name: n }))
    );
    const entries: PanelNameRules[] = replies
      .filter((r): r is RulesReply => !!r && r.found)
      .map((r) => ({
        name: r.name,
        symbolClass: r.symbol_class,
        justification: r.justification,
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
    // Keyed by book and unit inside the panel: the cursor may have moved on.
    panel.rules(s.id, index, entries);
  } catch (err) {
    // Best effort, but said out loud -- a panel that silently shows less is
    // indistinguishable from a form that introduced less.
    channel.appendLine(`rules: form ${index} could not be read (${err})`);
  }
}

/**
 * Fill in why the form inducted, if it did.
 *
 * Gated on the envelope rather than always asked: ACL2 records the
 * `:induction` rune structurally in `rules`, so the cheap fact decides
 * whether to pay for the expensive one -- which reads the whole captured log.
 * Most forms never induct, and this keeps the panel free for them.
 */
async function followInduction(s: BookSession, index: number): Promise<void> {
  const e = s.unitEvents.get(index);
  // Both places, and the second is the one that matters. A macro that
  // expands to a `defthm` -- `defrule`, `define`, most of std -- records its
  // runes against the *inner* event, leaving the form's own `rules` empty.
  // Checking only the outer list missed every such form, which is to say
  // most real books.
  const runes = [...(e?.rules ?? []), ...(e?.inner ?? []).flatMap((i) => i.rules ?? [])];
  if (!e || !runes.some((r) => r.toLowerCase().includes(':induction'))) {
    return;
  }
  try {
    const r = await client?.request('induction', { book: s.id, output_ref: e.output_ref });
    const n = (r.inductions ?? []).length;
    if (n === 0) {
      // The form inducted -- a rune says so -- and the log yielded nothing.
      // That is either an ACL2 wording this does not know or a bug here, and
      // both are worth a line rather than silence.
      channel.appendLine(
        `induction: form ${index} has an :induction rune but its log parsed to nothing`
      );
    }
    const records: PanelInduction[] = ((r.inductions ?? []) as InductionWire[]).map((i) => ({
      goal: i.goal,
      suggestedBy: i.suggested_by,
      attribution: i.attribution,
      scheme: i.scheme,
      justifiedBy: i.justified_by,
      subgoals: i.subgoals,
    }));
    // Keyed by book and unit: the cursor may have moved on while the log was
    // being read, and a narrative for the form you have left is noise.
    panel.induction(s.id, index, records);
  } catch (err) {
    // Best effort in the panel, but not silent: swallowing this hid a real
    // fault once already.
    channel.appendLine(`induction: form ${index} could not be read (${err})`);
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
