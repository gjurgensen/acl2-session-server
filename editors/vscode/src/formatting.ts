// Delivering `indent.ts` to VSCode.
//
// Two surfaces, and one rule between them:
//
//   as you type   a line break creates a line, or a closing delimiter
//                 finishes one. `watchDocument`.
//   on request    Format Selection, Format Document, Tab, the command.
//
// VSCode's own mechanism for the first is
// `DocumentOnTypeFormattingEditProvider`. It had been ruled out here, and
// this is a controlled re-test of that, because the run it was ruled out on
// could not have shown anything else. Its trace:
//
//   indent: registered. effective for acl2: formatOnType=true autoIndent="keep"
//   indent: change x1 [off=3198 del=0 ins="\n           "]
//   indent:   newline on line 94 ("           ") -> indent to 18
//   indent:   edit applied=true; ... caret at 94:18
//
// The probe was a provider that only reported being called, and it never
// reported. But `watchDocument` was live beside it and wrote to line 94 --
// the line the trigger sat on -- and the editor cancels an in-flight on-type
// format the moment any change lands at or above the trigger's line:
//
//   let n = this._editor.onDidChangeModelContent(r => {
//     for (let a = 0; a < r.changes.length; a++)
//       if (r.changes[a].range.endLineNumber <= t.lineNumber) {
//         i.cancel(); n.dispose(); return } });
//
// So the workaround cancelled the mechanism it was measuring, on every
// Enter, by construction. Whatever that run showed, it was not a measurement
// of an unencumbered provider.
//
// The reason given for the null result does not survive either. It was that
// VSCodeVim replaces the `type` command on-type formatting hangs off. It
// does replace it -- and then hands single characters straight back to the
// editor (`out/extension.js`):
//
//   static async insert(e,t,r,n){ n ??= (1===t.length);
//     n ? await commands.executeCommand("default:type",{text:t})
//       : await e.edit(...) }
//
// `"\n"` is one character; insert-mode Enter arrives there as an
// `insertTextVSCode` transformation, and VSCodeVim contributes no `enter`
// keybinding of its own, so there is no other route it could take. On the
// editor's side `default:type` triggers with source `"keyboard"` --
//
//   runCommand(o,e){ let t=o.get(ri).getFocusedCodeEditor();
//     t && t.trigger("keyboard", this._handlerId, e) }
//
// -- and `"keyboard"` is exactly the condition under which the type path
// fires the event format-on-type listens to:
//
//   _type(e,t){ ... e==="keyboard" && this._onDidType.fire(t) }
//
// Every link holds. So the provider does the real work here, and
// `watchDocument` stands down unless `acl2-session.indentOnType` is turned
// back on: one mechanism at a time, which is what the earlier run lacked.
//
// C is not evidence either way, and it is worth keeping the note.
// `indentationRules` and `onEnterRules` in the built-in cpp language
// configuration are regexes the core applies during newline handling, no
// provider involved -- which is why C survives whatever an extension does to
// `type`, and is a mechanism Lisp cannot use at all; see the head of
// `indent.ts`.
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
 * The fallback, and off unless `acl2-session.indentOnType` is turned on.
 * It watches the text rather than the keystroke, so it runs no matter who
 * inserted the character -- which is worth having if the on-type provider
 * turns out to be unreachable after all, and worth having switched off
 * while that is being decided, since its edit lands on the trigger line and
 * cancels the provider it would be measured against. See the head of this
 * file.
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
    logEffective(e.document);
    log(
      `change x${e.contentChanges.length} ` +
        e.contentChanges
          .map((c) => `[off=${c.rangeOffset} del=${c.rangeLength} ins=${JSON.stringify(c.text)}]`)
          .join(' ')
    );
    if (!vscode.workspace.getConfiguration('acl2-session').get<boolean>('indentOnType', false)) {
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
 * end of the indentation, which is where it belongs. That is the one piece
 * `watchDocument` has to do by hand and cannot do reliably.
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
  const ours = vscode.workspace.getConfiguration('acl2-session', scope);
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
      `detectIndentation=${both('detectIndentation')}; ` +
      `indentOnType(watcher)=${ours.get('indentOnType')}`
  );
}

/** Register the formatting providers. Returns them for disposal. */
export function registerFormatting(channel?: vscode.OutputChannel): vscode.Disposable[] {
  trace = channel;
  const acl2 = { language: 'acl2' };
  log('registered: on-type provider for "\\n" ) ] }, range, document, watcher');
  return [
    vscode.languages.registerOnTypeFormattingEditProvider(acl2, onType, '\n', ')', ']', '}'),
    vscode.languages.registerDocumentRangeFormattingEditProvider(acl2, range),
    vscode.languages.registerDocumentFormattingEditProvider(acl2, whole),
    watchDocument(),
  ];
}
