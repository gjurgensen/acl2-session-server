// Delivering `indent.ts` to VSCode.
//
// Two surfaces, and one rule between them:
//
//   as you type   a line break creates a line, or a closing delimiter
//                 finishes one. `watchDocument`.
//   on request    Format Selection, Format Document, Tab, the command.
//
// VSCode's own mechanism for the first is `DocumentOnTypeFormattingEditProvider`,
// and it is not used here, because in this editor it is never called. That
// was measured rather than assumed, after being assumed wrongly once:
//
//   indent: registered. effective for acl2: formatOnType=true autoIndent="keep"
//   indent: change x1 [off=3198 del=0 ins="\n           "]
//
// A provider registered for `\n`, with `editor.formatOnType` confirmed true
// for the language, and an Enter that plainly happened -- and no call. The
// cause is almost certainly VSCodeVim, which replaces the `type` command
// that on-type formatting hangs off (`overrideCommand(context, 'type', ...)`
// in its `extensionBase.js`); what is certain is the absence of the call.
//
// Note that the editor's *own* newline handling is untouched by this: the
// change above carries the line break and `autoIndent: keep`'s spaces
// together, which is the core doing its work. Only the provider is
// unreachable. C indents fine in the same editor for the same reason, one
// step further along -- `indentationRules` and `onEnterRules` in the
// built-in cpp language configuration are regexes the core applies during
// newline handling, no provider involved. That mechanism is closed to Lisp;
// see the head of `indent.ts`.
//
// So the rule hangs off the document instead, which sees every edit whoever
// made it. One mechanism, not two that must agree.
//
// The other way in -- `indentationRules` in the language configuration -- is
// what VSCode reaches for first and cannot express Lisp indentation; see the
// head of `indent.ts`. Because it is tried first, the language contribution
// turns `editor.autoIndent` down to `keep` for `acl2`, so the built-in
// bracket rules do not put a wrong indent on the line before we put the
// right one there.
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
  /**
   * True while our own indent edit is being applied.
   *
   * Applying it fires this very handler again, from inside the `await`. The
   * re-entry is harmless on its own -- the line is by then correctly
   * indented, so it finds nothing to do -- but it is work on the keystroke
   * path, and a rule that reacted to its own output is one edit away from a
   * loop.
   */
  let applying = false;
  return vscode.workspace.onDidChangeTextDocument(async (e) => {
    if (e.document.languageId !== 'acl2') {
      return;
    }
    if (applying) {
      log('  ignored: our own indent edit');
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
    // The whole rule, and there is no more to it than this: a line is
    // re-indented when a line break creates it, and when a closing delimiter
    // finishes it. That is Emacs' `electric-indent-mode`, which is where
    // every ACL2 author's expectations come from, and it is deliberately not
    // "re-indent whenever the line looks unfinished".
    //
    // A caret-based test was tried here and was wrong twice over. It cannot
    // be primary, because `editor.selection` has not moved yet while this
    // event fires -- it still points at the line you left. And it cannot be
    // a fallback either, because "the caret is in leading whitespace" is
    // true of every space and every backspace typed at the front of a line,
    // so it took the cursor away from anyone adjusting their own indent by
    // hand.
    const single = e.contentChanges.length === 1 ? e.contentChanges[0] : undefined;
    const kind = single && reindentTrigger(single.text);
    if (!single || !kind) {
      log('  ignored: not a line break or a closing delimiter');
      return;
    }
    const at = e.document.positionAt(single.rangeOffset + single.text.length).line;
    const edit = indentLineEdit(editor.document, at, { blankLines: 'indent' });
    log(
      `  ${kind} on line ${at} (${JSON.stringify(e.document.lineAt(at).text)}) -> ` +
        (edit ? `indent to ${edit.newText.length}` : 'already right, no edit')
    );
    if (!edit) {
      return;
    }
    applying = true;
    let applied: boolean;
    try {
      applied = await editor.edit((b) => b.replace(edit.range, edit.newText), {
        undoStopBefore: false,
        undoStopAfter: false,
      });
    } finally {
      applying = false;
    }
    // Say where the caret goes, rather than hoping.
    //
    // After Enter the caret sits at the end of the whitespace we just
    // replaced -- at the *end* of the replaced range, which is the one
    // position an edit does not carry along in any dependable way. And an
    // extension that manages the cursor itself will put it back where it had
    // it. Either way the text was right and the caret was not, which is the
    // whole of what a person notices.
    //
    // Only for a line that is nothing but indentation: if the line has code
    // on it -- a closing delimiter re-indenting what it finished -- the
    // caret is somewhere in that code and the editor's own mapping is the
    // right answer.
    const line = editor.document.lineAt(at);
    if (applied && !line.text.trim()) {
      const end = new vscode.Position(at, line.text.length);
      editor.selection = new vscode.Selection(end, end);
    }
    log(
      `  edit applied=${applied}; line is now ${JSON.stringify(line.text)}; ` +
        `caret at ${editor.selection.active.line}:${editor.selection.active.character}`
    );
  });
}

/** Register the three providers. Returns them for disposal. */
export function registerFormatting(channel?: vscode.OutputChannel): vscode.Disposable[] {
  trace = channel;
  const acl2 = { language: 'acl2' };
  return [
    vscode.languages.registerDocumentRangeFormattingEditProvider(acl2, range),
    vscode.languages.registerDocumentFormattingEditProvider(acl2, whole),
    watchDocument(),
  ];
}
