// Delivering `indent.ts` to VSCode.
//
// The editor offers exactly three ways to indent a language it does not
// build in, and this uses all three because they cover different moments:
//
//   on-type       Enter, and a closing paren. The moment that matters.
//   range         Format Selection, and format-on-paste.
//   document      Format Document, and format-on-save if it is on.
//
// The fourth way -- `indentationRules` in the language configuration -- is
// the one VSCode reaches for first and the one we cannot use; see the head
// of `indent.ts`. Because it is tried first, the language contribution turns
// it down to `keep` for `acl2`, or every Enter would be indented twice: once
// wrongly by the built-in rules, once correctly here, with both edits in the
// undo stack.
//
// Nothing in here decides anything. Every rule lives in `indent.ts` where it
// can be measured against the books; this is the glue that carries it.

import * as vscode from 'vscode';
import { IndentOptions, plannedIndent, reindent, reindentTrigger, topLevelStarts } from './indent';

/**
 * Where each top-level form starts, remembered per document version.
 *
 * Indentation only ever needs the enclosing top-level form, and finding it
 * means a scan. Doing that scan on every keystroke of an 800 KB book is the
 * kind of cost that is invisible until the file is
 * `kestrel/c/syntax/validator.lisp` and one form is five thousand lines
 * long. A document's version changes on every edit, so this is a cache that
 * cannot go stale -- and a `WeakMap` so a closed document takes its entry
 * with it.
 */
const startsCache = new WeakMap<vscode.TextDocument, { version: number; starts: number[] }>();

/**
 * Where to write a trace of what indentation saw and did.
 *
 * Off unless `acl2-session.indentTrace` is on. Indentation runs on the
 * keystroke path, and the interesting failures are ones where it does
 * nothing at all -- a change event that never matched, an edit that another
 * extension immediately overwrote. None of that is visible from the outside,
 * and reasoning about it from a screenshot of the result has already cost
 * two wrong diagnoses.
 */
let trace: vscode.OutputChannel | undefined;
function log(what: string): void {
  if (trace && vscode.workspace.getConfiguration('acl2-session').get<boolean>('indentTrace')) {
    trace.appendLine(`indent: ${what}`);
  }
}

function startsOf(doc: vscode.TextDocument): number[] {
  const hit = startsCache.get(doc);
  if (hit && hit.version === doc.version) {
    return hit.starts;
  }
  const starts = topLevelStarts(doc.getText());
  startsCache.set(doc, { version: doc.version, starts });
  return starts;
}

/**
 * The edit that fixes one line's indentation, or nothing.
 *
 * Nothing is the common answer, and returning it rather than an edit that
 * replaces whitespace with identical whitespace matters: an empty edit list
 * leaves the undo stack alone and cannot move the cursor.
 *
 * The decision is `plannedIndent`'s, in `indent.ts`, where it is measured.
 * What is left here is the editor's coordinates.
 */
function indentLineEdit(
  doc: vscode.TextDocument,
  lineNo: number,
  opts: IndentOptions = {}
): vscode.TextEdit | undefined {
  const line = doc.lineAt(lineNo);
  const want = plannedIndent(doc.getText(), doc.offsetAt(line.range.start), startsOf(doc), opts);
  if (want === undefined) {
    return undefined;
  }
  const have = line.firstNonWhitespaceCharacterIndex;
  if (have === want && !line.text.slice(0, have).includes('\t')) {
    return undefined;
  }
  return vscode.TextEdit.replace(
    new vscode.Range(line.range.start, line.range.start.translate(0, have)),
    // Spaces regardless of `insertSpaces`. Lisp indentation aligns under a
    // column a tab cannot reach -- `(append (foo x)` puts the next line
    // under column 8 -- so a tab-indented Lisp file is only accidentally
    // aligned. `acl2-indent.el` opens by setting `indent-tabs-mode` to nil
    // for the same reason.
    ' '.repeat(want)
  );
}

/** Re-indent a span of lines, top down, as one edit per changed line. */
function indentRangeEdits(doc: vscode.TextDocument, first: number, last: number): vscode.TextEdit[] {
  // Whole-document is the common case for both range and document
  // formatting, and `reindent` does it in one linear pass -- far cheaper
  // than asking line by line, each of which re-derives the paren stack.
  if (first === 0 && last === doc.lineCount - 1) {
    const text = doc.getText();
    const out = reindent(text);
    if (out === text) {
      return [];
    }
    const a = text.split('\n');
    const b = out.split('\n');
    const edits: vscode.TextEdit[] = [];
    for (let i = 0; i < a.length; i++) {
      if (a[i] === b[i]) {
        continue;
      }
      const lead = (s: string): number => s.length - s.trimStart().length;
      const line = doc.lineAt(i);
      edits.push(
        vscode.TextEdit.replace(
          new vscode.Range(line.range.start, line.range.start.translate(0, lead(a[i]))),
          b[i].slice(0, lead(b[i]))
        )
      );
    }
    return edits;
  }
  const edits: vscode.TextEdit[] = [];
  for (let i = first; i <= last; i++) {
    const e = indentLineEdit(doc, i);
    if (e) {
      edits.push(e);
    }
  }
  return edits;
}

const onType: vscode.OnTypeFormattingEditProvider = {
  provideOnTypeFormattingEdits(doc, position, ch) {
    // Enter puts the cursor on a line that so far holds only a copy of the
    // previous line's whitespace, so this is the one caller that must treat
    // an empty line as something to indent rather than something to leave
    // alone. A closing delimiter arrives on a line with content on it, and
    // only changes which form that content belongs to.
    const e = indentLineEdit(doc, position.line, { blankLines: 'indent' });
    log(`on-type ${JSON.stringify(ch)} line=${position.line} -> ${e ? JSON.stringify(e.newText.length) : 'no edit'}`);
    return e ? [e] : [];
  },
};

const range: vscode.DocumentRangeFormattingEditProvider = {
  provideDocumentRangeFormattingEdits(doc, r) {
    return indentRangeEdits(doc, r.start.line, r.end.line);
  },
};

const whole: vscode.DocumentFormattingEditProvider = {
  provideDocumentFormattingEdits(doc) {
    return indentRangeEdits(doc, 0, doc.lineCount - 1);
  },
};

/**
 * Re-indent the current line, or the selection.
 *
 * Bound to Tab for people arriving from Emacs, where it is the way one
 * indents. The binding is behind a setting because taking Tab away from
 * everyone else would be worse than not offering it: outside a selection,
 * VSCode's Tab inserts, and a reader who wanted a tab and got a re-indent
 * has lost something they cannot easily ask for again.
 */
export async function indentCommand(): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  if (!editor || editor.document.languageId !== 'acl2') {
    return;
  }
  const doc = editor.document;
  const edits = editor.selections.flatMap((s) =>
    s.isEmpty
      ? indentRangeEdits(doc, s.active.line, s.active.line)
      : indentRangeEdits(doc, s.start.line, s.end.line)
  );
  if (!edits.length) {
    return;
  }
  await editor.edit(
    (b) => {
      for (const e of edits) {
        b.replace(e.range, e.newText);
      }
    },
    { undoStopBefore: true, undoStopAfter: true }
  );
}

/**
 * Re-indent from the document rather than from the keystroke.
 *
 * The on-type providers below are the right mechanism and the one VSCode
 * documents, but they are only reached through the editor's own `type`
 * command -- and VSCodeVim replaces that command, so for anyone using it the
 * on-type path never runs at all. This watches the text instead, which no
 * extension can intercept.
 *
 * Both paths stay live. They cannot fight: whichever runs first leaves the
 * line correctly indented, and `indentLineEdit` returns nothing for a line
 * that is already right, so the second is a no-op rather than a second edit.
 */
function watchDocument(): vscode.Disposable {
  return vscode.workspace.onDidChangeTextDocument(async (e) => {
    if (e.document.languageId !== 'acl2') {
      return;
    }
    log(
      `change x${e.contentChanges.length} ` +
        e.contentChanges
          .map((c) => `[off=${c.rangeOffset} del=${c.rangeLength} ins=${JSON.stringify(c.text)}]`)
          .join(' ')
    );
    if (!vscode.workspace.getConfiguration('acl2-session').get<boolean>('indentOnType', true)) {
      log('  ignored: acl2-session.indentOnType is off');
      return;
    }
    const editor = vscode.window.activeTextEditor;
    if (!editor || editor.document !== e.document) {
      log('  ignored: the change is not in the active editor');
      return;
    }
    // Which line to re-indent, and why. Two ways in, and the order matters.
    //
    // The change itself is the reliable one. `editor.selection` is *not*
    // updated yet while this event fires -- the caret still sits where it
    // was before the edit, and VSCode reports its move separately
    // afterwards. So asking the caret whether it is in a line's leading
    // whitespace answers about the previous line, and answers no. That was
    // this bug: the trace showed `ins="\n           "` -- an Enter, plainly
    // -- rejected as "neither a fresh line nor a closing delimiter".
    //
    //   newline  a line break followed by nothing but whitespace, which is
    //            Enter however the whitespace got there. Not a paste, which
    //            carries content and belongs to the range formatter.
    //   close    a single closing delimiter, which has just decided what the
    //            line it finishes belongs to.
    //   fresh    the fallback, for an extension whose edits do not arrive as
    //            one recognisable change: a caret already sitting in leading
    //            whitespace on a line that just changed. Stale by one event,
    //            which is exactly why it cannot be the primary test.
    const single = e.contentChanges.length === 1 ? e.contentChanges[0] : undefined;
    const byText = single ? reindentTrigger(single.text) : undefined;
    const caret = editor.selection.active;
    let at: number;
    let kind: string;
    if (single && byText) {
      at = e.document.positionAt(single.rangeOffset + single.text.length).line;
      kind = byText;
    } else if (
      editor.selection.isEmpty &&
      e.document.lineAt(caret.line).text.slice(0, caret.character).trim() === '' &&
      e.contentChanges.some((c) => {
        const first = e.document.positionAt(c.rangeOffset).line;
        const last = e.document.positionAt(c.rangeOffset + c.text.length).line;
        return caret.line >= first && caret.line <= last;
      })
    ) {
      at = caret.line;
      kind = 'fresh';
    } else {
      log('  ignored: not an Enter, a closing delimiter, or a fresh line');
      return;
    }
    const edit = indentLineEdit(editor.document, at, { blankLines: 'indent' });
    log(
      `  ${kind} on line ${at} (${JSON.stringify(e.document.lineAt(at).text)}) -> ` +
        (edit ? `indent to ${edit.newText.length}` : 'already right, no edit')
    );
    if (!edit) {
      return;
    }
    const applied = await editor.edit((b) => b.replace(edit.range, edit.newText), {
      undoStopBefore: false,
      undoStopAfter: false,
    });
    log(`  edit applied=${applied}; line is now ${JSON.stringify(e.document.lineAt(at).text)}`);
  });
}

/** Register the three providers. Returns them for disposal. */
export function registerFormatting(channel?: vscode.OutputChannel): vscode.Disposable[] {
  trace = channel;
  const acl2 = { language: 'acl2' };
  return [
    // `\n` is the one that matters; the closing delimiters are what make a
    // clause snap into place as it is finished, which is the behaviour
    // anyone coming from Emacs will reach for without thinking.
    vscode.languages.registerOnTypeFormattingEditProvider(acl2, onType, '\n', ')', ']'),
    vscode.languages.registerDocumentRangeFormattingEditProvider(acl2, range),
    vscode.languages.registerDocumentFormattingEditProvider(acl2, whole),
    watchDocument(),
  ];
}
