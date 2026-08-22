// Delivering `indent.ts` to VSCode.
//
// Two surfaces, and one rule between them:
//
//   as you type   a line break creates a line, or a closing delimiter
//                 finishes one. `onType`.
//   on request    Format Selection, Format Document, Tab, the command.
//
// The first is `DocumentOnTypeFormattingEditProvider`, the mechanism VSCode
// documents for exactly this. It had been ruled out here once, wrongly, and
// the record is worth keeping because the shape of the mistake is an easy
// one to repeat.
//
// The run it rested on could not have shown anything else. A probe provider
// was registered and a document watcher was left running beside it, and that
// watcher's edit lands on the trigger line every time. The editor cancels an
// in-flight on-type format the moment any change touches a line at or above
// the trigger:
//
//   let n = this._editor.onDidChangeModelContent(r => {
//     for (let a = 0; a < r.changes.length; a++)
//       if (r.changes[a].range.endLineNumber <= t.lineNumber) {
//         i.cancel(); n.dispose(); return } });
//
// So the workaround cancelled the mechanism it was being measured against. A
// probe that returns no edits is not thereby a control: what had to be inert
// was the thing beside it.
//
// The cause offered for the null result was VSCodeVim replacing the `type`
// command that on-type formatting hangs off. It does replace it -- and then
// hands single characters straight back (`out/extension.js`):
//
//   static async insert(e,t,r,n){ n ??= (1===t.length);
//     n ? await commands.executeCommand("default:type",{text:t})
//       : await e.edit(...) }
//
// `"\n"` is one character, insert-mode Enter arrives there as an
// `insertTextVSCode` transformation, and VSCodeVim binds no `enter` of its
// own, so Enter has no other route. `default:type` triggers with source
// `"keyboard"`, and `"keyboard"` is precisely the condition under which the
// type path fires the event format-on-type listens to:
//
//   runCommand(o,e){ let t=o.get(ri).getFocusedCodeEditor();
//     t && t.trigger("keyboard", this._handlerId, e) }
//   _type(e,t){ ... e==="keyboard" && this._onDidType.fire(t) }
//
// Measured with the watcher inert, same editor, same vim:
//
//   indent: on-type CALLED ch="\n" at 94:11 line="           "
//   indent:   -> indent to 18
//   indent: change x1 [off=3210 del=0 ins="       "]
//
// It fires. The eleven spaces it was handed are `autoIndent: keep` copying
// the line above; the seven that came back are our replace-eleven-with-
// eighteen after `computeMoreMinimalEdits` narrowed it. Minimal edits for
// free, on top of the caret handling described at `onType`.
//
// One thing the watcher did better, and it is not free to give up: it
// suppressed both undo stops, where the editor pushes one either side of a
// format edit (`t && o.pushUndoStop()`, and format-on-type passes true).
// Whether that reads as two Ctrl+Z after an Enter was not measured. It is
// not adjustable from a provider, so keeping it would have meant keeping the
// whole watcher -- a re-entrancy guard, a hand-placed caret, and a re-indent
// fired by any redo that reinserts a paren. Not worth that.
//
// The other way in -- `indentationRules` in the language configuration -- is
// what VSCode reaches for first and cannot express Lisp indentation; see the
// head of `indent.ts`. Because it is tried first, the language contribution
// turns `editor.autoIndent` down to `keep` for `acl2`, so the built-in
// bracket rules do not put a wrong indent on the line before we put the
// right one there. C indents by that route and is no evidence about this
// one: those are regexes the core applies during newline handling, no
// provider involved, which is why C survives whatever an extension does to
// `type`.
//
// A line made by a *command* rather than by typing is outside all of this, and
// cannot be brought in. `editor.action.insertLineAfter` -- which is what Vim's
// `o` runs -- reaches `TypeOperations._enter`, the same function Enter uses,
// and `_enter` returns at `autoIndent: keep` before it would consult a rule,
// let alone a provider. Nothing in the ecosystem solves this: of fourteen
// Lisp-family extensions surveyed, none carries indentation rules at all, and
// the two that indent as you type (Calva, and a Common Lisp extension that
// binds Enter and calls `textDocument/onTypeFormatting` on its server by hand)
// both leave `o` alone. Tab is the answer, which is why it is bound by
// default; for `o` itself, VSCodeVim runs a remap's `after` keys before its
// `commands`, so
//
//   { "before": ["o"], "after": ["o"], "commands": ["acl2-session.indent"] }
//
// opens the line, enters insert mode, and then indents it.
//
// Nothing in here decides anything. Every rule lives in `indent.ts` where it
// can be measured against the books; this is the glue that carries it.

import * as vscode from 'vscode';
import { IndentOptions, plannedIndent, reindent, topLevelStarts } from './indent';

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
  const edits = editor.selections.flatMap((s) => {
    if (!s.isEmpty) {
      return indentRangeEdits(doc, s.start.line, s.end.line);
    }
    // A blank line is where this is asked for most, and it is the one line a
    // whole-buffer pass must leave alone: there, blank means trailing
    // whitespace to be cleared; here it means an empty line inside a form,
    // waiting to be typed into at the column its body starts at. `blankLines`
    // is which of the two, and the cursor case is `indent`.
    //
    // Without this, Tab on an empty line did nothing at all -- and so did
    // every remap built on this command, which is how it was noticed.
    const one = indentLineEdit(doc, s.active.line, { blankLines: 'indent' });
    return one ? [one] : [];
  });
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
  // On a line that is now nothing but indentation, the caret belongs at the
  // end of it. Nowhere else on that line means anything, and the end of a
  // replaced range is the one position an edit does not carry along in any
  // dependable way -- which is knowable here, unlike in a document handler,
  // because this runs when a person asked it to and nothing else is in
  // flight. Reassigned only when it would actually move.
  const moved = editor.selections.map((s) => {
    if (!s.isEmpty) {
      return s;
    }
    const line = editor.document.lineAt(s.active.line);
    if (line.text.trim() || s.active.character === line.text.length) {
      return s;
    }
    const end = new vscode.Position(s.active.line, line.text.length);
    return new vscode.Selection(end, end);
  });
  if (moved.some((s, i) => !s.isEqual(editor.selections[i]))) {
    editor.selections = moved;
  }
}

/**
 * Re-indent as you type, through the mechanism VSCode documents for it.
 *
 * The editor calls this after it has inserted the character and applied its
 * own newline handling, with the caret already on the line to be fixed --
 * so `position.line` is the line to indent in both cases, the fresh line
 * after Enter and the finished line after a closing delimiter.
 *
 * `blankLines: 'indent'` because after Enter the line holds a copy of the
 * previous line's whitespace and nothing else; read as blank it would mean
 * every Enter had nothing to do.
 *
 * Nothing here touches the caret. The editor applies these edits with
 * `EditOperation.replaceMove`, which carries a marker sitting at the end of
 * a replaced range to the end of the new text -- which is the caret, at the
 * end of the indentation, which is where it belongs. Watching the document
 * instead means placing it by hand, and the end of a replaced range is the
 * one position an edit does not carry along in any dependable way.
 */
const onType: vscode.OnTypeFormattingEditProvider = {
  provideOnTypeFormattingEdits(doc, position, ch) {
    logEffective(doc);
    log(
      `on-type CALLED ch=${JSON.stringify(ch)} at ${position.line}:${position.character} ` +
        `line=${JSON.stringify(doc.lineAt(position.line).text)}`
    );
    const edit = indentLineEdit(doc, position.line, { blankLines: 'indent' });
    log(`  -> ${edit ? `indent to ${edit.newText.length}` : 'already right, no edit'}`);
    return edit ? [edit] : [];
  },
};

/**
 * The editor settings that decide whether any of this runs, logged once.
 *
 * Both halves of the scope, and it has to be both. A `ConfigurationScope`
 * of a bare `Uri` resolves the resource but drops the language override; a
 * bare `{ languageId }` applies the override but at workspace level. Either
 * one alone reports a value the editor is not using.
 *
 * That is not a hypothetical. This function first shipped with the `Uri`
 * form and reported `formatOnType=false autoIndent="full"` on the very run
 * where the on-type provider fired and `autoIndent: keep` had visibly
 * copied the previous line's eleven spaces -- both readings flatly contrary
 * to the behaviour in the same trace, because both were the values with the
 * `[acl2]` block stripped off. A line of instrumentation that disagrees
 * with the run it is instrumenting is worse than none; the earlier
 * `{ languageId }` probe was wrong the other way and no one noticed,
 * because that time it happened to agree.
 */
let loggedFor: string | undefined;
function logEffective(doc: vscode.TextDocument): void {
  if (loggedFor === doc.uri.toString()) {
    return;
  }
  loggedFor = doc.uri.toString();
  const scope = { uri: doc.uri, languageId: doc.languageId };
  const cfg = vscode.workspace.getConfiguration('editor', scope);
  // Alongside each language-scoped value, the value with the `[acl2]` block
  // taken away. When indentation is not running, the interesting question
  // is almost always whether the language defaults reached this document at
  // all, and one reading cannot answer it.
  const bare = vscode.workspace.getConfiguration('editor', doc.uri);
  const both = (k: string): string => {
    const lang = JSON.stringify(cfg.get(k));
    const plain = JSON.stringify(bare.get(k));
    return lang === plain ? lang : `${lang} (without [acl2]: ${plain})`;
  };
  log(
    `effective for ${doc.uri.fsPath} [${doc.languageId}]: ` +
      `formatOnType=${both('formatOnType')} autoIndent=${both('autoIndent')} ` +
      `insertSpaces=${both('insertSpaces')} tabSize=${both('tabSize')} ` +
      `detectIndentation=${both('detectIndentation')}`
  );
}

/** Register the formatting providers. Returns them for disposal. */
export function registerFormatting(channel?: vscode.OutputChannel): vscode.Disposable[] {
  trace = channel;
  const acl2 = { language: 'acl2' };
  log('registered: on-type provider for "\\n" "\\r" ) ] }, range, document');
  // The settings line wants an occasion of its own. `onType` logs it, but
  // only once it has been called, and the one run where it matters is the
  // run where it never is.
  const seeActive = (editor?: vscode.TextEditor): void => {
    if (editor && editor.document.languageId === 'acl2') {
      logEffective(editor.document);
    }
  };
  seeActive(vscode.window.activeTextEditor);
  return [
    vscode.languages.registerOnTypeFormattingEditProvider(acl2, onType, '\n', '\r', ')', ']', '}'),
    vscode.languages.registerDocumentRangeFormattingEditProvider(acl2, range),
    vscode.languages.registerDocumentFormattingEditProvider(acl2, whole),
    vscode.window.onDidChangeActiveTextEditor(seeActive),
  ];
}
